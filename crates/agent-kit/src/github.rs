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

    /// Whether this is the branch pull requests go into.
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
    let configured = line(
        top,
        &["config", "--get", &format!("branch.{branch}.remote")],
    )
    .filter(|r| remotes.contains(r));
    let remote = configured
        .or_else(|| remotes.iter().find(|r| *r == "origin").cloned())
        .or_else(|| remotes.first().cloned());
    let Some(remote) = remote else {
        return Ok(PrOutcome::Unavailable(
            "This repository has no remote to open a pull request on".to_owned(),
        ));
    };
    let url = line(top, &["remote", "get-url", "--push", "--", &remote])
        .or_else(|| line(top, &["remote", "get-url", "--", &remote]))
        .unwrap_or_default();
    let github = parse_remote(&url);

    let upstream = line(
        top,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            &format!("{branch}@{{upstream}}"),
        ],
    );
    let remote_branch = upstream
        .as_deref()
        .and_then(|u| u.strip_prefix(&format!("{remote}/")))
        .map(str::to_owned)
        .unwrap_or_else(|| branch.clone());
    let push = match &upstream {
        None => PushState::NoUpstream,
        Some(up) => match line(top, &["rev-list", "--count", &format!("{up}..{branch}")])
            .and_then(|n| n.parse::<u32>().ok())
        {
            Some(0) => PushState::UpToDate,
            Some(n) => PushState::Ahead(n),
            None => PushState::NoUpstream,
        },
    };
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
    let refspec = format!(
        "refs/heads/{}:refs/heads/{}",
        state.branch, state.remote_branch
    );
    let mut args = vec!["push"];
    if state.push == PushState::NoUpstream {
        args.push("--set-upstream");
    }
    args.extend([state.remote.as_str(), refspec.as_str()]);
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
        use std::fs;
        use std::path::PathBuf;
        use std::process::Command;

        fn sh(dir: &Path, args: &[&str]) {
            let output = Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "init.defaultBranch=main",
                ])
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        /// A clone of a local bare "remote", on `main`, with one commit pushed.
        fn new_clone() -> Option<(tempfile::TempDir, PathBuf)> {
            if !crate::git::git_installed() {
                eprintln!("git not installed; skipping");
                return None;
            }
            let tmp = tempfile::tempdir().unwrap();
            let bare = tmp.path().join("remote.git");
            fs::create_dir_all(&bare).unwrap();
            sh(&bare, &["init", "-q", "--bare"]);
            let work = tmp.path().join("work");
            fs::create_dir_all(&work).unwrap();
            sh(&work, &["init", "-q"]);
            fs::write(work.join("a.txt"), "a\n").unwrap();
            sh(&work, &["add", "."]);
            sh(&work, &["commit", "-q", "-m", "init"]);
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
                Some("https://github.com/o/r/compare/main...feat/y?expand=1")
            );
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
