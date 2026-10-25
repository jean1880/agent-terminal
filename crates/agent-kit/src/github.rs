//! Opening a pull request: where the current branch would go on GitHub, whether it has been
//! pushed, and the push itself.
//!
//! The app makes no request to GitHub. [`PrState::compare_url`] is GitHub's own "compare" page
//! for the branch, built from the remote's URL and handed to the browser, where the pull request
//! is written. The one network operation here is [`push_branch`], a plain `git push` (never
//! forced) that runs only when the user confirms it.
//!
//! Ceilings:
//! - The pull request opens on the repository of the branch's push remote. A fork workflow
//!   (push to `origin`, open against `upstream`) gets the fork's own compare page, where GitHub
//!   offers to switch the base repository.
//! - The base is read from remote-tracking refs, never from the network. When a remote has no
//!   `HEAD` and no `main`/`master` ref, it stays unknown and only the destination rules (never a
//!   differently named branch) protect it. A repository reached by two different SSH host aliases
//!   reads as two repositories, so its push state is `Unknown` and a default-named branch is
//!   refused.
//! - Only `github.com`, GitHub Enterprise Cloud (`*.ghe.com`) and hosts named `github.*` are
//!   recognised. An SSH host alias (`github-work:o/r`) is not read through `~/.ssh/config`.

use std::path::Path;

use crate::git::{git_output, git_raw, QUERY_TIMEOUT_SECS};

/// How long a push may take before it is given up on.
pub const PUSH_TIMEOUT_SECS: u64 = 120;

/// A repository on a GitHub host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubRepo {
    pub host: String,
    pub owner: String,
    pub name: String,
}

impl GithubRepo {
    /// `owner/name`.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

/// Characters a GitHub owner or repository name may hold.
fn name_ok(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn host_ok(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "github.com"
        || host == "www.github.com"
        || host.ends_with(".ghe.com")
        || (host.starts_with("github.")
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.')))
}

/// Reads a remote URL as a GitHub repository: `https://github.com/o/r(.git)`,
/// `ssh://git@github.com[:port]/o/r.git`, `git@github.com:o/r.git` or `git://github.com/o/r`.
/// Any user, password or token in the URL is dropped, never carried into what is built from it.
pub fn parse_remote(url: &str) -> Option<GithubRepo> {
    let url = url.trim();
    let (authority, path) = if let Some((scheme, rest)) = url.split_once("://") {
        if !["https", "http", "ssh", "git", "git+ssh", "ssh+git"].contains(&scheme) {
            return None;
        }
        rest.split_once('/')?
    } else {
        // scp-like: `[user@]host:owner/repo`.
        let (authority, path) = url.split_once(':')?;
        if authority.contains('/') {
            return None;
        }
        (authority, path)
    };
    let host = authority.rsplit('@').next()?;
    let host = match host.rsplit_once(':') {
        Some((h, port)) if port.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => host,
    };
    if !host_ok(host) {
        return None;
    }
    let path = path.trim_start_matches('/').trim_end_matches('/');
    let (owner, name) = path.split_once('/')?;
    let name = name.strip_suffix(".git").unwrap_or(name);
    (name_ok(owner) && name_ok(name)).then(|| GithubRepo {
        host: host.to_ascii_lowercase(),
        owner: owner.to_owned(),
        name: name.to_owned(),
    })
}

/// Percent-encodes a branch name for a URL path: everything but unreserved characters and `/`.
fn encode_ref(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// GitHub's page for opening a pull request from `head` into `base` (the repository's default
/// branch when `None`).
pub fn compare_url(repo: &GithubRepo, base: Option<&str>, head: &str) -> String {
    let range = match base {
        Some(base) => format!("{}...{}", encode_ref(base), encode_ref(head)),
        None => encode_ref(head),
    };
    format!(
        "https://{}/{}/{}/compare/{range}?expand=1",
        repo.host, repo.owner, repo.name
    )
}

/// Whether the branch is on the remote as it is here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushState {
    /// It has no upstream: never pushed, or pushed without `-u`.
    NoUpstream,
    /// The upstream has every commit.
    UpToDate,
    /// This many commits are not on the upstream yet.
    Ahead(u32),
    /// What the push target holds cannot be read from here (a different push repository, or a
    /// target never fetched): it may need pushing.
    Unknown,
}

/// A remote setting shown to the user, without the user, password or token of a URL.
fn without_userinfo(value: &str) -> String {
    match value.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
            let host = authority.rsplit('@').next().unwrap_or(authority);
            if path.is_empty() {
                format!("{scheme}://{host}")
            } else {
                format!("{scheme}://{host}/{path}")
            }
        }
        None => value.rsplit('@').next().unwrap_or(value).to_owned(),
    }
}

/// What opening a pull request for a tab's repository would involve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrState {
    /// The branch checked out.
    pub branch: String,
    /// The remote it pushes to.
    pub remote: String,
    /// Its name on that remote (the upstream's, else the same as here).
    pub remote_branch: String,
    /// The branch a pull request would go into, when the remote names its default.
    pub base: Option<String>,
    /// The remote's repository, when it is on GitHub.
    pub github: Option<GithubRepo>,
    pub push: PushState,
    /// Changes not committed yet (they are not part of a pull request).
    pub uncommitted: bool,
}

impl PrState {
    /// The compare page to open, when the remote is on GitHub.
    pub fn compare_url(&self) -> Option<String> {
        self.github
            .as_ref()
            .map(|repo| compare_url(repo, self.base.as_deref(), &self.remote_branch))
    }

    /// Whether the branch must be pushed before its pull request shows every commit.
    pub fn needs_push(&self) -> bool {
        self.push != PushState::UpToDate
    }

    /// Whether the push destination is the branch pull requests go into. The base is read from
    /// the push remote's repository (and is unknown when that differs from the fetch one), so
    /// equal names are the same branch of the same repository.
    pub fn on_base(&self) -> bool {
        self.base.as_deref() == Some(self.remote_branch.as_str())
    }
}

/// Why no pull request can be opened from here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrOutcome {
    Ready(PrState),
    /// A sentence for the button's tooltip.
    Unavailable(String),
}

fn line(dir: &Path, args: &[&str]) -> Option<String> {
    let out = git_output(dir, args, &[], QUERY_TIMEOUT_SECS).ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !text.is_empty()).then_some(text)
}

/// Reads, in `dir`'s repository, what opening a pull request would involve. Blocking: run it off
/// the main thread. Reads only; never fetches.
pub fn pr_state(dir: &Path) -> Result<PrOutcome, String> {
    let Some(repo) = crate::git::discover(dir)? else {
        return Ok(PrOutcome::Unavailable(
            "This folder is not inside a git repository".to_owned(),
        ));
    };
    let top = repo.toplevel.as_path();
    let Some(branch) = repo.branch.clone() else {
        return Ok(PrOutcome::Unavailable(
            "No branch is checked out (detached HEAD)".to_owned(),
        ));
    };
    if repo.head.is_none() {
        return Ok(PrOutcome::Unavailable(
            "This branch has no commits yet".to_owned(),
        ));
    }
    let remotes: Vec<String> = line(top, &["remote"])
        .map(|r| r.lines().map(str::to_owned).collect())
        .unwrap_or_default();
    // git's own order: branch.<b>.pushRemote, remote.pushDefault, branch.<b>.remote, origin.
    // A configured push remote that is not a remote's name (a URL, say) is never replaced by
    // another repository.
    // Only the setting that wins is checked: an invalid default does not disable a valid branch one.
    let configured_push = [
        format!("branch.{branch}.pushRemote"),
        "remote.pushDefault".to_owned(),
    ]
    .into_iter()
    .find_map(|key| line(top, &["config", "--get", &key]).map(|name| (key, name)));
    if let Some((key, name)) = &configured_push {
        if !remotes.contains(name) {
            let shown = agent_core::redact::redact(&without_userinfo(name));
            return Ok(PrOutcome::Unavailable(format!(
                "{key} is set to \"{shown}\", which is not one of this repository's remotes"
            )));
        }
    }
    let in_remotes = |r: String| remotes.contains(&r).then_some(r);
    let tracked_remote = line(
        top,
        &["config", "--get", &format!("branch.{branch}.remote")],
    )
    .and_then(in_remotes);
    let remote = configured_push
        .map(|(_, name)| name)
        .or_else(|| tracked_remote.clone())
        .or_else(|| remotes.iter().find(|r| *r == "origin").cloned())
        .or_else(|| remotes.first().cloned());
    let Some(remote) = remote else {
        return Ok(PrOutcome::Unavailable(
            "This repository has no remote to open a pull request on".to_owned(),
        ));
    };
    let fetch_url = line(top, &["remote", "get-url", "--", &remote]);
    // A push goes to every push URL, so all of them are read.
    let push_urls: Vec<String> = line(
        top,
        &["remote", "get-url", "--push", "--all", "--", &remote],
    )
    .map(|u| u.lines().map(str::to_owned).collect())
    .unwrap_or_default();
    // Remote-tracking refs describe the fetch repository; they say nothing of a different push
    // one. The same repository over another scheme (https fetch, ssh push) is not a different one.
    let fetch_identity = fetch_url.as_deref().map(repo_identity);
    let push_elsewhere = push_urls.len() > 1
        || push_urls
            .first()
            .is_some_and(|p| Some(repo_identity(p)) != fetch_identity);
    let github = parse_remote(push_urls.first().or(fetch_url.as_ref()).map_or("", |s| s));

    // The upstream's branch, read from the config exactly (never by splitting `origin/a/b`).
    let upstream_branch = if tracked_remote.as_deref() == Some(remote.as_str()) {
        line(top, &["config", "--get", &format!("branch.{branch}.merge")])
            .and_then(|m| m.strip_prefix("refs/heads/").map(str::to_owned))
    } else {
        None
    };
    let follows_upstream = matches!(
        line(top, &["config", "--get", "push.default"]).as_deref(),
        Some("upstream" | "tracking")
    );
    let base = line(
        top,
        &[
            "symbolic-ref",
            "--short",
            &format!("refs/remotes/{remote}/HEAD"),
        ],
    )
    .and_then(|r| r.strip_prefix(&format!("{remote}/")).map(str::to_owned))
    .or_else(|| {
        ["main", "master"].into_iter().map(str::to_owned).find(|b| {
            line(
                top,
                &[
                    "rev-parse",
                    "--verify",
                    "-q",
                    &format!("refs/remotes/{remote}/{b}"),
                ],
            )
            .is_some()
        })
    });
    // The remote-tracking refs describe the fetch repository: when the push goes elsewhere, the
    // base read from them is not the push repository's, so it is unknown.
    let fetch_base = base.clone();
    let base = if push_elsewhere { None } else { base };
    // With the push repository's base unknown, a branch that is, or is named like, a default
    // branch could be pushed straight onto it. No request is made to find out.
    if push_elsewhere
        && (fetch_base.as_deref() == Some(branch.as_str())
            || ["main", "master", "trunk", "develop"].contains(&branch.as_str()))
    {
        return Ok(PrOutcome::Unavailable(
            "Can't tell this repository's base branch; push from a feature branch".to_owned(),
        ));
    }
    // The upstream names the remote branch only when git's push would follow it, and never when
    // that is the base branch of a feature branch: `checkout -b feat origin/main` tracks main.
    let remote_branch = upstream_branch
        .filter(|u| {
            // A differently named destination needs a known base that it is not.
            follows_upstream && (*u == branch || base.as_deref().is_some_and(|b| b != u))
        })
        .unwrap_or_else(|| branch.clone());
    // Whether the push target has every commit, judged by its own remote-tracking ref.
    // A different push repository, or a tracked branch whose push target was never fetched, cannot
    // be judged from here: `Unknown`. A branch with neither upstream nor target is `NoUpstream`.
    let tracked = line(top, &["config", "--get", &format!("branch.{branch}.merge")]).is_some();
    let push = if push_elsewhere {
        PushState::Unknown
    } else {
        match line(
            top,
            &[
                "rev-list",
                "--count",
                &format!("refs/remotes/{remote}/{remote_branch}..{branch}"),
            ],
        )
        .and_then(|n| n.parse::<u32>().ok())
        {
            Some(0) => PushState::UpToDate,
            Some(n) => PushState::Ahead(n),
            None if tracked => PushState::Unknown,
            None => PushState::NoUpstream,
        }
    };
    let uncommitted = line(top, &["status", "--porcelain", "--untracked-files=no"]).is_some();
    Ok(PrOutcome::Ready(PrState {
        branch,
        remote,
        remote_branch,
        base,
        github,
        push,
        uncommitted,
    }))
}

/// The arguments of the `git` command [`push_branch`] runs, and the one a confirmation shows:
/// never forced, with the fully qualified refspec. Only a branch with no upstream gets one
/// (`pr_state` reports a tracked branch as `Unknown`).
pub fn push_args(state: &PrState) -> Vec<String> {
    let mut args = vec!["push".to_owned()];
    if state.push == PushState::NoUpstream {
        args.push("--set-upstream".to_owned());
    }
    args.push(state.remote.clone());
    args.push(format!(
        "refs/heads/{}:refs/heads/{}",
        state.branch, state.remote_branch
    ));
    args
}

/// A repository's identity from its URL: `host/owner/name` for a GitHub one, whatever the scheme
/// (https, ssh, scp-style) or `.git` suffix; the trimmed URL otherwise.
fn repo_identity(url: &str) -> String {
    match parse_remote(url) {
        Some(r) => format!("{}/{}", r.host, r.slug()).to_ascii_lowercase(),
        None => url.trim().to_owned(),
    }
}

/// Pushes `state`'s branch to its remote, never forced. A branch with no upstream gets one
/// (`--set-upstream`); one with an upstream pushes to it. Blocking. Git's own message on failure,
/// with anything secret-shaped in it masked.
pub fn push_branch(dir: &Path, state: &PrState) -> Result<(), String> {
    for (name, what) in [
        (&state.remote, "remote name"),
        (&state.branch, "branch name"),
        (&state.remote_branch, "branch name"),
    ] {
        crate::worktree::precheck_name(name, what)?;
    }
    let args = push_args(state);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    git_raw(dir, &args, &[], PUSH_TIMEOUT_SECS)
        .map(|_| ())
        .map_err(|why| agent_core::redact::redact(&why))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(owner: &str, name: &str) -> GithubRepo {
        GithubRepo {
            host: "github.com".into(),
            owner: owner.into(),
            name: name.into(),
        }
    }

    #[test]
    fn every_github_remote_form_is_read_and_credentials_are_dropped() {
        for url in [
            "https://github.com/jean1880/agent-terminal.git",
            "https://github.com/jean1880/agent-terminal",
            "https://github.com/jean1880/agent-terminal/",
            "https://x-access-token:secret@github.com/jean1880/agent-terminal.git",
            "git@github.com:jean1880/agent-terminal.git",
            "github.com:jean1880/agent-terminal",
            "ssh://git@github.com/jean1880/agent-terminal.git",
            "ssh://git@github.com:22/jean1880/agent-terminal.git",
            "git://github.com/jean1880/agent-terminal",
            "  https://GitHub.com/jean1880/agent-terminal.git\n",
        ] {
            assert_eq!(
                parse_remote(url),
                Some(repo("jean1880", "agent-terminal")),
                "{url}"
            );
        }
        let ghe = parse_remote("git@acme.ghe.com:team/app.git").expect("GHE cloud");
        assert_eq!(ghe.host, "acme.ghe.com");
        assert!(parse_remote("https://github.acme.io/team/app").is_some());
    }

    #[test]
    fn other_hosts_and_odd_paths_are_not_github() {
        for url in [
            "",
            "/srv/git/repo.git",
            "../repo",
            "file:///srv/repo.git",
            "https://gitlab.com/o/r.git",
            "git@bitbucket.org:o/r.git",
            "https://notgithub.com/o/r",
            "https://github.com/onlyowner",
            "https://github.com/o/r/extra",
            "https://github.com/o/../r",
            "https://github.com/o/r?x=1",
            "github-work:o/r.git",
            "ftp://github.com/o/r",
        ] {
            assert_eq!(parse_remote(url), None, "{url}");
        }
    }

    #[test]
    fn the_compare_url_encodes_branch_names() {
        let r = repo("o", "r");
        assert_eq!(
            compare_url(&r, Some("master"), "feat/github-pr"),
            "https://github.com/o/r/compare/master...feat/github-pr?expand=1"
        );
        assert_eq!(
            compare_url(&r, None, "fix#12?x&y"),
            "https://github.com/o/r/compare/fix%2312%3Fx%26y?expand=1"
        );
        assert_eq!(
            compare_url(&r, Some("main"), "café"),
            "https://github.com/o/r/compare/main...caf%C3%A9?expand=1"
        );
    }

    fn state(push: PushState) -> PrState {
        PrState {
            branch: "feat/x".into(),
            remote: "origin".into(),
            remote_branch: "feat/x".into(),
            base: Some("main".into()),
            github: Some(repo("o", "r")),
            push,
            uncommitted: false,
        }
    }

    #[test]
    fn push_needs_and_the_base_branch() {
        assert!(state(PushState::NoUpstream).needs_push());
        assert!(state(PushState::Ahead(2)).needs_push());
        assert!(!state(PushState::UpToDate).needs_push());
        let mut on_main = state(PushState::UpToDate);
        on_main.remote_branch = "main".into();
        assert!(on_main.on_base());
        assert!(!state(PushState::UpToDate).on_base());
        let mut local = state(PushState::UpToDate);
        local.github = None;
        assert_eq!(local.compare_url(), None);
    }

    #[test]
    fn a_push_refuses_names_that_read_as_options() {
        let mut bad = state(PushState::NoUpstream);
        bad.remote = "--receive-pack=evil".into();
        assert!(push_branch(Path::new("/nonexistent"), &bad).is_err());
        let mut bad = state(PushState::NoUpstream);
        bad.remote_branch = "-f".into();
        assert!(push_branch(Path::new("/nonexistent"), &bad).is_err());
    }

    mod repo {
        use super::super::*;
        use crate::test_support::{git as sh, git_or_skip, init_repo_at};
        use std::fs;
        use std::path::PathBuf;

        /// A clone of a local bare "remote", on `main`, with one commit pushed.
        fn new_clone() -> Option<(tempfile::TempDir, PathBuf)> {
            if !git_or_skip() {
                return None;
            }
            let tmp = tempfile::tempdir().unwrap();
            let bare = tmp.path().join("remote.git");
            fs::create_dir_all(&bare).unwrap();
            sh(&bare, &["init", "-q", "--bare"]);
            let work = tmp.path().join("work");
            fs::create_dir_all(&work).unwrap();
            init_repo_at(&work, &[("a.txt", b"a\n")]);
            sh(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
            sh(&work, &["push", "-q", "-u", "origin", "main"]);
            sh(&work, &["remote", "set-head", "origin", "main"]);
            Some((tmp, work))
        }

        fn ready(dir: &Path) -> PrState {
            match pr_state(dir).unwrap() {
                PrOutcome::Ready(s) => s,
                PrOutcome::Unavailable(why) => panic!("unavailable: {why}"),
            }
        }

        #[test]
        fn a_new_branch_is_pushed_with_an_upstream_and_then_up_to_date() {
            let Some((_tmp, work)) = new_clone() else {
                return;
            };
            let s = ready(&work);
            assert_eq!(s.branch, "main");
            assert!(s.on_base());
            assert_eq!(s.push, PushState::UpToDate);
            assert_eq!(s.github, None, "a local path is not GitHub");

            sh(&work, &["switch", "-q", "-c", "feat/x"]);
            fs::write(work.join("b.txt"), "b\n").unwrap();
            sh(&work, &["add", "."]);
            sh(&work, &["commit", "-q", "-m", "b"]);
            fs::write(work.join("a.txt"), "changed\n").unwrap();
            let s = ready(&work);
            assert_eq!(s.push, PushState::NoUpstream);
            assert_eq!(s.base.as_deref(), Some("main"));
            assert!(!s.on_base());
            assert!(s.uncommitted);

            push_branch(&work, &s).expect("push");
            let s = ready(&work);
            assert_eq!(s.push, PushState::UpToDate);
            assert_eq!(s.remote_branch, "feat/x");

            sh(&work, &["commit", "-q", "-am", "c"]);
            let s = ready(&work);
            assert_eq!(s.push, PushState::Ahead(1));
            assert!(!s.uncommitted);
            push_branch(&work, &s).expect("second push");
            assert_eq!(ready(&work).push, PushState::UpToDate);
        }

        #[test]
        fn the_github_url_comes_from_the_push_remote() {
            let Some((_tmp, work)) = new_clone() else {
                return;
            };
            sh(
                &work,
                &[
                    "remote",
                    "set-url",
                    "--push",
                    "origin",
                    "git@github.com:o/r.git",
                ],
            );
            sh(&work, &["switch", "-q", "-c", "feat/y"]);
            let s = ready(&work);
            assert_eq!(
                s.compare_url().as_deref(),
                // The fetch repository's base is not this repository's: GitHub picks its default.
                Some("https://github.com/o/r/compare/feat/y?expand=1")
            );
        }

        /// Commits a file on the current branch.
        fn commit_file(work: &Path, name: &str) {
            fs::write(work.join(name), "x\n").unwrap();
            sh(work, &["add", "."]);
            sh(work, &["commit", "-q", "-m", name]);
        }

        #[test]
        fn a_branch_cut_from_origin_main_pushes_to_its_own_name() {
            let Some((_tmp, work)) = new_clone() else {
                return;
            };
            sh(&work, &["checkout", "-q", "-b", "feat", "origin/main"]);
            commit_file(&work, "b.txt");
            let s = ready(&work);
            assert_eq!(s.remote_branch, "feat");
            assert!(!s.on_base());
            assert!(s.needs_push());
            push_branch(&work, &s).expect("push");
            let bare = work.parent().unwrap().join("remote.git");
            sh(&bare, &["rev-parse", "--verify", "refs/heads/feat"]);
            let main = sh(&bare, &["rev-parse", "main"]);
            let again = ready(&work);
            assert_eq!(again.push, PushState::UpToDate);
            assert_eq!(sh(&bare, &["rev-parse", "main"]), main, "main is untouched");
        }

        #[test]
        fn the_push_remote_settings_pick_the_remote() {
            let Some((tmp, work)) = new_clone() else {
                return;
            };
            let fork = tmp.path().join("fork.git");
            fs::create_dir_all(&fork).unwrap();
            sh(&fork, &["init", "-q", "--bare"]);
            sh(&work, &["remote", "add", "fork", fork.to_str().unwrap()]);
            sh(&work, &["switch", "-q", "-c", "feat/z"]);
            commit_file(&work, "z.txt");
            sh(&work, &["config", "branch.feat/z.pushRemote", "fork"]);
            let s = ready(&work);
            assert_eq!(s.remote, "fork");
            push_branch(&work, &s).expect("push");
            sh(&fork, &["rev-parse", "--verify", "refs/heads/feat/z"]);

            sh(&work, &["config", "--unset", "branch.feat/z.pushRemote"]);
            sh(&work, &["config", "branch.feat/z.remote", "origin"]);
            assert_eq!(ready(&work).remote, "origin");
            sh(&work, &["config", "remote.pushDefault", "fork"]);
            assert_eq!(ready(&work).remote, "fork");
        }

        #[test]
        fn without_origin_head_the_push_never_targets_the_base() {
            let Some((_tmp, work)) = new_clone() else {
                return;
            };
            sh(&work, &["remote", "set-head", "origin", "-d"]);
            sh(&work, &["checkout", "-q", "-b", "feat", "origin/main"]);
            commit_file(&work, "b.txt");
            sh(&work, &["config", "push.default", "upstream"]);
            let s = ready(&work);
            assert_ne!(s.remote_branch, "main");
            assert_eq!(s.remote_branch, "feat");
            assert!(!s.on_base());
        }

        #[test]
        fn an_unknown_base_never_lets_the_upstream_name_the_destination() {
            let Some((_tmp, work)) = new_clone() else {
                return;
            };
            sh(&work, &["remote", "set-head", "origin", "-d"]);
            sh(
                &work,
                &["update-ref", "refs/remotes/origin/develop", "HEAD"],
            );
            sh(&work, &["update-ref", "-d", "refs/remotes/origin/main"]);
            sh(&work, &["checkout", "-q", "-b", "feat", "origin/develop"]);
            commit_file(&work, "b.txt");
            sh(&work, &["config", "push.default", "upstream"]);
            let s = ready(&work);
            assert_eq!(s.base, None);
            assert_eq!(s.remote_branch, "feat");
        }

        #[test]
        fn a_push_remote_that_is_a_url_is_unavailable() {
            let Some((_tmp, work)) = new_clone() else {
                return;
            };
            sh(&work, &["switch", "-q", "-c", "feat"]);
            sh(&work, &["config", "branch.feat.remote", "origin"]);
            sh(
                &work,
                &[
                    "config",
                    "branch.feat.pushRemote",
                    "https://github.com/me/fork.git",
                ],
            );
            assert!(matches!(pr_state(&work), Ok(PrOutcome::Unavailable(_))));
            sh(&work, &["config", "--unset", "branch.feat.pushRemote"]);
            sh(&work, &["config", "remote.pushDefault", "nowhere"]);
            assert!(matches!(pr_state(&work), Ok(PrOutcome::Unavailable(_))));
        }

        #[test]
        fn the_upstream_is_read_from_config_not_split_from_its_name() {
            let Some((tmp, work)) = new_clone() else {
                return;
            };
            let other = tmp.path().join("release.git");
            fs::create_dir_all(&other).unwrap();
            sh(&other, &["init", "-q", "--bare"]);
            // `git remote add` refuses the name, but a hand-edited config can hold it.
            sh(
                &work,
                &[
                    "config",
                    "remote.origin/release.url",
                    other.to_str().unwrap(),
                ],
            );
            sh(&work, &["switch", "-q", "-c", "feat"]);
            commit_file(&work, "b.txt");
            sh(
                &work,
                &[
                    "config",
                    "remote.origin/release.fetch",
                    "+refs/heads/*:refs/remotes/origin/release/*",
                ],
            );
            sh(
                &work,
                &["update-ref", "refs/remotes/origin/release/3.4", "HEAD"],
            );
            sh(&work, &["config", "branch.feat.remote", "origin/release"]);
            sh(&work, &["config", "branch.feat.merge", "refs/heads/3.4"]);
            sh(&work, &["config", "branch.feat.pushRemote", "origin"]);
            sh(&work, &["config", "push.default", "upstream"]);
            let s = ready(&work);
            assert_eq!(s.remote, "origin");
            assert_eq!(s.remote_branch, "feat", "the upstream is on another remote");

            sh(&work, &["config", "--unset", "branch.feat.pushRemote"]);
            let s = ready(&work);
            assert_eq!(s.remote, "origin/release");
            assert_eq!(s.remote_branch, "feat", "no base is known on that remote");
        }

        #[test]
        fn a_different_push_url_is_never_up_to_date() {
            let Some((tmp, work)) = new_clone() else {
                return;
            };
            sh(&work, &["switch", "-q", "-c", "feat"]);
            commit_file(&work, "b.txt");
            sh(&work, &["update-ref", "refs/remotes/origin/feat", "HEAD"]);
            assert_eq!(ready(&work).push, PushState::UpToDate);
            let fork = tmp.path().join("fork.git");
            fs::create_dir_all(&fork).unwrap();
            sh(&fork, &["init", "-q", "--bare"]);
            sh(
                &work,
                &[
                    "remote",
                    "set-url",
                    "--push",
                    "origin",
                    fork.to_str().unwrap(),
                ],
            );
            assert_eq!(ready(&work).push, PushState::Unknown);
        }

        #[test]
        fn a_different_push_repository_does_not_lend_its_base_to_the_destination() {
            let Some((tmp, work)) = new_clone() else {
                return;
            };
            sh(
                &work,
                &["update-ref", "refs/remotes/origin/develop", "HEAD"],
            );
            sh(&work, &["checkout", "-q", "-b", "feat", "origin/develop"]);
            commit_file(&work, "b.txt");
            sh(&work, &["config", "push.default", "upstream"]);
            let fork = tmp.path().join("fork.git");
            fs::create_dir_all(&fork).unwrap();
            sh(&fork, &["init", "-q", "--bare"]);
            sh(
                &work,
                &[
                    "remote",
                    "set-url",
                    "--push",
                    "origin",
                    fork.to_str().unwrap(),
                ],
            );
            let s = ready(&work);
            assert_eq!(s.base, None);
            assert_eq!(s.remote_branch, "feat");
        }

        #[test]
        fn an_invalid_default_does_not_disable_a_valid_branch_push_remote() {
            let Some((_tmp, work)) = new_clone() else {
                return;
            };
            sh(&work, &["switch", "-q", "-c", "feat"]);
            sh(&work, &["config", "branch.feat.pushRemote", "origin"]);
            sh(&work, &["config", "remote.pushDefault", "nowhere"]);
            assert_eq!(ready(&work).remote, "origin");
        }

        #[test]
        fn on_base_reads_the_destination_not_the_local_name() {
            let Some((tmp, work)) = new_clone() else {
                return;
            };
            let fork = tmp.path().join("fork.git");
            fs::create_dir_all(&fork).unwrap();
            sh(&fork, &["init", "-q", "--bare"]);
            sh(&work, &["remote", "add", "fork", fork.to_str().unwrap()]);
            sh(&work, &["update-ref", "refs/remotes/fork/main", "HEAD"]);
            sh(&work, &["config", "branch.main.remote", "fork"]);
            sh(&work, &["config", "branch.main.merge", "refs/heads/topic"]);
            sh(&work, &["config", "push.default", "upstream"]);
            let s = ready(&work);
            assert_eq!(s.branch, "main");
            assert_eq!(s.remote_branch, "topic");
            assert!(!s.on_base());
        }

        /// Points `origin` at GitHub: fetched over https, pushed over each of `pushes`.
        fn github_origin(work: &Path, pushes: &[&str]) {
            sh(
                work,
                &["remote", "set-url", "origin", "https://github.com/o/r.git"],
            );
            for (i, p) in pushes.iter().enumerate() {
                let add: &[&str] = if i == 0 {
                    &["remote", "set-url", "--push", "origin", p]
                } else {
                    &["remote", "set-url", "--add", "--push", "origin", p]
                };
                sh(work, add);
            }
        }

        #[test]
        fn https_fetch_and_ssh_push_of_one_repository_keep_the_base() {
            let Some((_tmp, work)) = new_clone() else {
                return;
            };
            github_origin(&work, &["git@github.com:o/r.git"]);
            let s = ready(&work);
            assert_eq!(s.base.as_deref(), Some("main"));
            assert!(s.on_base(), "main is the base, so the action is off");
        }

        #[test]
        fn a_different_push_repository_refuses_a_default_named_branch_only() {
            let Some((tmp, work)) = new_clone() else {
                return;
            };
            let fork = tmp.path().join("fork.git");
            fs::create_dir_all(&fork).unwrap();
            sh(&fork, &["init", "-q", "--bare"]);
            sh(
                &work,
                &[
                    "remote",
                    "set-url",
                    "--push",
                    "origin",
                    fork.to_str().unwrap(),
                ],
            );
            assert!(matches!(pr_state(&work), Ok(PrOutcome::Unavailable(_))));
            sh(&work, &["switch", "-q", "-c", "feat"]);
            assert_eq!(ready(&work).remote_branch, "feat");
        }

        #[test]
        fn every_push_url_counts() {
            let Some((_tmp, work)) = new_clone() else {
                return;
            };
            sh(&work, &["switch", "-q", "-c", "feat"]);
            commit_file(&work, "b.txt");
            sh(&work, &["update-ref", "refs/remotes/origin/feat", "HEAD"]);
            github_origin(
                &work,
                &["https://github.com/o/r.git", "git@github.com:o/r.git"],
            );
            let s = ready(&work);
            assert_eq!(s.push, PushState::Unknown);
            assert_eq!(s.base, None);
        }

        #[test]
        fn the_confirmation_and_the_push_share_one_command() {
            let mut s = super::state(PushState::NoUpstream);
            s.branch = "feat".into();
            s.remote_branch = "topic".into();
            assert_eq!(
                push_args(&s),
                [
                    "push",
                    "--set-upstream",
                    "origin",
                    "refs/heads/feat:refs/heads/topic"
                ]
            );
            s.push = PushState::Unknown;
            assert_eq!(
                push_args(&s),
                ["push", "origin", "refs/heads/feat:refs/heads/topic"]
            );
        }

        #[test]
        fn a_rejected_push_remote_url_is_shown_without_its_credentials() {
            let Some((_tmp, work)) = new_clone() else {
                return;
            };
            sh(&work, &["switch", "-q", "-c", "feat"]);
            sh(
                &work,
                &[
                    "config",
                    "branch.feat.pushRemote",
                    "https://user:tok@host/x.git",
                ],
            );
            let Ok(PrOutcome::Unavailable(why)) = pr_state(&work) else {
                panic!("expected Unavailable");
            };
            assert!(!why.contains("tok") && !why.contains("user"), "{why}");
            assert!(why.contains("host/x.git"), "{why}");
        }

        #[test]
        fn no_remote_or_no_branch_is_unavailable() {
            let Some((_tmp, work)) = new_clone() else {
                return;
            };
            sh(&work, &["switch", "-q", "--detach"]);
            assert!(matches!(pr_state(&work), Ok(PrOutcome::Unavailable(_))));
            sh(&work, &["switch", "-q", "main"]);
            sh(&work, &["remote", "remove", "origin"]);
            assert!(matches!(pr_state(&work), Ok(PrOutcome::Unavailable(_))));
        }
    }
}
