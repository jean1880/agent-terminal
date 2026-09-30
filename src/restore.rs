//! Undo: put a tab's working tree back the way a checkpoint recorded it.
//!
//! This is the one destructive thing the terminal does to a repository, so it
//! is built to be undone and to touch only what checkpoints know about:
//!
//! 1. The working tree as it is now is pinned first, under a ref of its own
//!    (`refs/agent-terminal/<tab>/pre-restore-<nanos>`), even when nothing
//!    has changed since the last checkpoint. Restoring to it undoes the undo.
//! 2. Tracked files are put back with `git restore --worktree`. HEAD and the
//!    index are never touched, so commits and staged changes stand.
//! 3. Files the checkpoint lacks are deleted one by one, and only inside the
//!    working tree: never through a symlinked directory, never a directory.
//! 4. Files checkpoints never capture (ignored, too large, likely secrets,
//!    nested repositories) are left exactly as they are.
//! 5. The result is snapshotted again and compared with the target.
//!
//! Everything here blocks; run it off the main thread.

use crate::git::{
    commit_tree, create_ref, git_raw, snapshot_tree, RepoInfo, Skipped, SnapshotOutcome,
    QUERY_TIMEOUT_SECS, REF_ROOT, SNAPSHOT_TIMEOUT_SECS,
};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

/// The working tree pinned before a restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pinned {
    pub refname: String,
    pub commit: String,
    pub tree: String,
    /// What the pin could not capture, and so what a restore leaves alone.
    pub untouched: Vec<Skipped>,
}

/// What restoring to a target would do, for the confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePlan {
    pub pinned: Pinned,
    pub target: String,
    /// Paths whose content goes back to the target's.
    pub changed: Vec<String>,
    /// Paths present now and absent from the target, which are deleted.
    pub deleted: Vec<String>,
    /// Paths the target has but the pin skipped (grown past the size cap,
    /// renamed to look like a secret, ignored since, a nested repository).
    /// Restoring them would overwrite files the pin has no copy of, so
    /// they are excluded, and left as they are.
    pub protected: Vec<String>,
}

/// Of the paths a pin skipped, those the target has, a skipped directory (a
/// nested repository, `dir/`) protecting everything under it.
pub fn protected_paths(skipped: &[String], target_files: &[String]) -> Vec<String> {
    skipped
        .iter()
        .map(|s| s.trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .filter(|s| target_files.iter().any(|f| covers(s, f)))
        .collect()
}

/// Whether protected path `p` is, or contains, `path`.
fn covers(p: &str, path: &str) -> bool {
    path == p
        || path
            .strip_prefix(p)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Pathspecs that leave `protected` out of a restore or a diff. `top`: from
/// the toplevel; `literal`: a `*` in a name is not a glob.
fn exclusions(protected: &[String]) -> Vec<String> {
    protected
        .iter()
        .map(|p| format!(":(top,exclude,literal){p}"))
        .collect()
}

/// Failing once files may already have changed: says so, and how to get
/// back what was there.
pub fn partly_applied(err: &str, pinned: &str) -> String {
    format!(
        "{err}\n\nThe undo may have been partly applied. Your files as they were \
         are kept as commit {pinned}; put them back with:\n\
         git restore --source={pinned} --worktree -- :/"
    )
}

/// Deletes an undo point that turned out not to be needed: nothing to undo,
/// or the undo was cancelled.
pub fn discard(repo: &RepoInfo, pinned: &Pinned) {
    if let Err(err) = crate::git::delete_ref(repo, &pinned.refname) {
        tracing::warn!("Could not delete {}: {err}", pinned.refname);
    }
}

fn nul_paths(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).to_string())
        .collect()
}

fn nul_paths_raw(bytes: &[u8]) -> Vec<PathBuf> {
    bytes
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| PathBuf::from(OsStr::from_bytes(p)))
        .collect()
}

fn unix_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// Pins the working tree as it is now, under a ref of its own. Unlike a
/// checkpoint it is never skipped as unchanged: the undo must exist.
pub fn pin(repo: &RepoInfo, key: u64) -> Result<Pinned, String> {
    let snapshot = match snapshot_tree(repo, &format!("restore-{key}-{}", unix_nanos()))? {
        SnapshotOutcome::Taken(snapshot) => snapshot,
        SnapshotOutcome::Busy => {
            return Err("Git is busy in this repository; try again in a moment".to_string())
        }
    };
    let message = format!("pre-restore · tab {key}");
    let commit = commit_tree(repo, &snapshot.tree, repo.head.as_deref(), &message)?;
    let refname = format!("{REF_ROOT}/{key}/pre-restore-{}", unix_nanos());
    create_ref(repo, &refname, &commit)?;
    Ok(Pinned {
        refname,
        commit,
        tree: snapshot.tree,
        untouched: snapshot.skipped,
    })
}

fn names(
    repo: &RepoInfo,
    from: &str,
    to: &str,
    filter: Option<&str>,
    excluded: &[String],
) -> Result<Vec<u8>, String> {
    let mut args: Vec<String> = ["diff", "--name-only", "-z", "--no-renames", "--no-ext-diff"]
        .map(String::from)
        .to_vec();
    if let Some(filter) = filter {
        args.push(filter.to_string());
    }
    args.extend([from.to_string(), to.to_string()]);
    if !excluded.is_empty() {
        args.push("--".into());
        args.push(":/".into());
        args.extend(exclusions(excluded));
    }
    git_raw(&repo.toplevel, args, &[], SNAPSHOT_TIMEOUT_SECS)
}

/// Pins the working tree and works out what restoring to `target` would do.
/// With nothing to undo, the pin is discarded again and the plan is empty.
pub fn prepare(repo: &RepoInfo, key: u64, target: &str) -> Result<RestorePlan, String> {
    let pinned = pin(repo, key)?;
    let target_files = nul_paths(&git_raw(
        &repo.toplevel,
        ["ls-tree", "-r", "--name-only", "-z", target],
        &[],
        SNAPSHOT_TIMEOUT_SECS,
    )?);
    // Skipped paths are matched against the target by name to protect them,
    // and a name that is not valid UTF-8 reaches here only approximately
    // (lossily decoded). Such a file cannot be proved protected, so refuse
    // rather than risk overwriting it.
    if let Some(odd) = pinned
        .untouched
        .iter()
        .find(|s| s.path.contains(char::REPLACEMENT_CHARACTER))
    {
        discard(repo, &pinned);
        return Err(format!(
            "{} has a name that is not valid UTF-8 and is not captured by checkpoints, \
             so undo cannot guarantee to leave it alone. Rename or remove it, then try again.",
            odd.path
        ));
    }
    let skipped: Vec<String> = pinned.untouched.iter().map(|s| s.path.clone()).collect();
    let protected = protected_paths(&skipped, &target_files);
    let changed: Vec<String> = nul_paths(&names(repo, &pinned.commit, target, None, &[])?)
        .into_iter()
        .filter(|path| !protected.iter().any(|p| covers(p, path)))
        .collect();
    let deleted = nul_paths(&names(
        repo,
        target,
        &pinned.commit,
        Some("--diff-filter=A"),
        &[],
    )?);
    if changed.is_empty() && deleted.is_empty() {
        discard(repo, &pinned);
    }
    Ok(RestorePlan {
        pinned,
        target: target.to_string(),
        changed,
        deleted,
        protected,
    })
}

/// A restore writes a whole tree's worth of files; killing git part-way would
/// leave it half-done, so it gets far longer than a query.
const RESTORE_TIMEOUT_SECS: u64 = 120;

/// Paths named in the confirmation before the rest are counted.
const LISTED_PATHS: usize = 30;

/// The confirmation's text: what will change, what will be deleted, what is
/// left alone, and how to take it back. `to` finishes "Puts … back to", e.g.
/// "how they were before the last turn". `mid_turn` warns that the session
/// looks busy.
pub fn summary(plan: &RestorePlan, to: &str, mid_turn: bool) -> String {
    let deleted: std::collections::HashSet<&str> =
        plan.deleted.iter().map(String::as_str).collect();
    let restored = plan.changed.len() - plan.deleted.len().min(plan.changed.len());
    let mut text = String::new();
    if mid_turn {
        text.push_str(
            "The session in this tab looks busy. Anything it writes after this is \
             confirmed is not covered by the undo.\n\n",
        );
    }
    text.push_str(&format!(
        "Puts {restored} file(s) back to {to}, and deletes {} file(s) made since.\n\n",
        plan.deleted.len()
    ));
    for path in plan.changed.iter().take(LISTED_PATHS) {
        let mark = if deleted.contains(path.as_str()) {
            "−"
        } else {
            "~"
        };
        text.push_str(&format!("{mark} {path}\n"));
    }
    if plan.changed.len() > LISTED_PATHS {
        text.push_str(&format!(
            "… and {} more\n",
            plan.changed.len() - LISTED_PATHS
        ));
    }
    if !plan.pinned.untouched.is_empty() {
        let some: Vec<&str> = plan
            .pinned
            .untouched
            .iter()
            .take(5)
            .map(|s| s.path.as_str())
            .collect();
        text.push_str(&format!(
            "\nLeft exactly as they are, since checkpoints never capture them: {} \
             file(s), such as {}.\n",
            plan.pinned.untouched.len(),
            some.join(", ")
        ));
    }
    text.push_str(
        "\nCommits and staged changes are not touched. The files as they are now \
         are kept first, so Undo afterwards puts them back.",
    );
    text
}

/// `rel` inside `toplevel`, or `None` if it could lead anywhere else: an
/// absolute path, a `..`, or a directory on the way that resolves outside
/// (a symlink). `toplevel` must already be canonical. The last component is
/// not resolved, so a symlink there is itself what gets deleted.
pub fn contained(toplevel: &Path, rel: &Path) -> Option<PathBuf> {
    if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    let name = rel.file_name()?;
    let parent = toplevel.join(rel.parent().unwrap_or(Path::new("")));
    let parent = parent.canonicalize().ok()?;
    parent.starts_with(toplevel).then(|| parent.join(name))
}

/// What a restore did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restored {
    /// Paths that still differ from the target afterwards. Empty when the
    /// working tree now matches it.
    pub mismatched: Vec<String>,
    /// Paths that were not deleted because they lie outside the working tree.
    pub refused: Vec<String>,
}

/// Puts the working tree back to `plan.target`. HEAD and the index are left
/// alone, and so are `plan.protected`. Undo by restoring to
/// `plan.pinned.commit`. An error after files may have changed says so and
/// names the pin.
///
/// Ceiling: between the check that the tree still matches the pin and the
/// restore itself there is a window of one snapshot's `write-tree`; anything
/// written in it is overwritten without being pinned. The confirmation warns
/// when the session looks busy, which is when that happens. Upgrade path:
/// restore from the pinned index state rather than the live tree.
pub fn apply(repo: &RepoInfo, plan: &RestorePlan) -> Result<Restored, String> {
    let toplevel = repo
        .toplevel
        .canonicalize()
        .map_err(|err| format!("Could not resolve {}: {err}", repo.toplevel.display()))?;

    // The pin is the undo, so it must still be the whole truth: anything
    // written since it was taken (the agent, still working) would be
    // overwritten with nothing to bring it back.
    match snapshot_tree(repo, &format!("recheck-{}", unix_nanos()))? {
        SnapshotOutcome::Taken(now) if now.tree == plan.pinned.tree => {}
        SnapshotOutcome::Taken(_) => {
            return Err(
                "Files changed after this undo was prepared, so nothing was restored. \
                 Wait for the session to finish, then try again."
                    .to_string(),
            )
        }
        SnapshotOutcome::Busy => {
            return Err("Git is busy in this repository; nothing was restored".to_string())
        }
    }

    // From here on files may change, so every failure says how to get back.
    restore_checked(repo, plan, &toplevel).map_err(|err| partly_applied(&err, &plan.pinned.commit))
}

fn restore_checked(
    repo: &RepoInfo,
    plan: &RestorePlan,
    toplevel: &Path,
) -> Result<Restored, String> {
    let toplevel = toplevel.to_path_buf();
    let mut args = vec![
        "restore".to_string(),
        format!("--source={}", plan.target),
        "--worktree".into(),
        "--".into(),
        ":/".into(),
    ];
    args.extend(exclusions(&plan.protected));
    git_raw(&toplevel, args, &[], RESTORE_TIMEOUT_SECS)?;

    // `restore` removes files that are tracked but absent from the target;
    // what it leaves are the untracked ones added since. Asked again rather
    // than taken from the plan, so the list is exactly the files that exist.
    let added = git_raw(
        &toplevel,
        [
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            "--diff-filter=A",
            plan.target.as_str(),
            plan.pinned.commit.as_str(),
        ],
        &[],
        QUERY_TIMEOUT_SECS,
    )?;
    let mut refused = Vec::new();
    let mut emptied = Vec::new();
    for rel in nul_paths_raw(&added) {
        let Some(path) = contained(&toplevel, &rel) else {
            refused.push(rel.display().to_string());
            continue;
        };
        match std::fs::symlink_metadata(&path) {
            // Never a directory: a checkpoint records files, so a directory
            // here is something it does not know about.
            Ok(meta) if meta.is_dir() => continue,
            Ok(_) => {}
            Err(_) => continue,
        }
        std::fs::remove_file(&path)
            .map_err(|err| format!("Could not delete {}: {err}", rel.display()))?;
        if let Some(parent) = path.parent() {
            emptied.push(parent.to_path_buf());
        }
    }
    // Directories the deletions left empty go too, deepest first, and only
    // while empty: remove_dir refuses anything else.
    emptied.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for mut dir in emptied {
        while dir != toplevel && dir.starts_with(&toplevel) && std::fs::remove_dir(&dir).is_ok() {
            if !dir.pop() {
                break;
            }
        }
    }

    // The check: a fresh snapshot must match the target, skipped and
    // protected files aside (the snapshot leaves those out, and protected
    // ones were deliberately not restored).
    let after = match snapshot_tree(repo, &format!("verify-{}", unix_nanos()))? {
        SnapshotOutcome::Taken(snapshot) => snapshot.tree,
        SnapshotOutcome::Busy => {
            return Err("Git became busy before the result could be checked".to_string())
        }
    };
    let target_tree = format!("{}^{{tree}}", plan.target);
    let mismatched = nul_paths(&names(repo, &target_tree, &after, None, &plan.protected)?);
    Ok(Restored {
        mismatched,
        refused,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_summary_says_what_changes_what_goes_and_what_stays() {
        use crate::git::SkipReason;
        let plan = RestorePlan {
            pinned: Pinned {
                refname: "r".into(),
                commit: "c".into(),
                tree: "t".into(),
                untouched: vec![Skipped {
                    path: ".env".into(),
                    reason: SkipReason::Secret,
                }],
            },
            target: "x".into(),
            changed: vec!["a.rs".into(), "new.rs".into()],
            deleted: vec!["new.rs".into()],
            protected: Vec::new(),
        };
        let text = summary(&plan, "how they were before the last turn", false);
        assert!(text.starts_with(
            "Puts 1 file(s) back to how they were before the last turn, and deletes 1 file(s)"
        ));
        assert!(text.contains("~ a.rs\n"));
        assert!(text.contains("− new.rs\n"));
        assert!(text.contains("never capture them: 1 file(s), such as .env."));
        assert!(!text.contains("looks busy"));
        assert!(summary(&plan, "x", true).starts_with("The session in this tab looks busy"));

        let many = RestorePlan {
            changed: (0..LISTED_PATHS + 4).map(|i| format!("f{i}")).collect(),
            deleted: Vec::new(),
            ..plan
        };
        assert!(summary(&many, "x", false).contains("… and 4 more\n"));
    }

    #[test]
    fn skipped_paths_the_target_has_are_protected() {
        let target: Vec<String> = ["a.rs", "big.bin", "vendor/x/lib.rs", "vendorish.rs"]
            .map(String::from)
            .to_vec();
        let skipped: Vec<String> = [".env", "big.bin", "vendor/x/"].map(String::from).to_vec();
        assert_eq!(
            protected_paths(&skipped, &target),
            vec!["big.bin".to_string(), "vendor/x".to_string()]
        );
        assert!(covers("vendor/x", "vendor/x/lib.rs"));
        assert!(!covers("vendor", "vendorish.rs"));
    }

    #[test]
    fn a_failure_after_files_changed_names_the_way_back() {
        let text = partly_applied("git restore did not finish within 120s", "abc123");
        assert!(text.contains("partly applied"));
        assert!(text.contains("git restore --source=abc123 --worktree -- :/"));
    }

    #[test]
    fn only_plain_relative_paths_are_contained() {
        let tmp = tempfile::tempdir().unwrap();
        let top = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(top.join("src")).unwrap();
        assert_eq!(
            contained(&top, Path::new("src/a.rs")),
            Some(top.join("src/a.rs"))
        );
        assert_eq!(contained(&top, Path::new("a.rs")), Some(top.join("a.rs")));
        assert_eq!(contained(&top, Path::new("../escape")), None);
        assert_eq!(contained(&top, Path::new("src/../../escape")), None);
        assert_eq!(contained(&top, Path::new("/etc/passwd")), None);
        // A parent that does not exist cannot be proved inside.
        assert_eq!(contained(&top, Path::new("gone/a.rs")), None);
    }

    #[test]
    fn a_symlinked_directory_out_of_the_tree_is_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let top = tmp.path().canonicalize().unwrap();
        std::os::unix::fs::symlink(outside.path(), top.join("link")).unwrap();
        assert_eq!(contained(&top, Path::new("link/victim.txt")), None);
        // The link itself, as the last component, is inside and deletable.
        assert_eq!(contained(&top, Path::new("link")), Some(top.join("link")));
    }

    mod repo {
        use super::super::*;
        use crate::git::{discover, git_installed};
        use std::fs;
        use std::process::Command;

        fn sh(dir: &Path, args: &[&str]) -> String {
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
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }

        fn new_repo() -> Option<tempfile::TempDir> {
            if !git_installed() {
                eprintln!("git not installed; skipping");
                return None;
            }
            let dir = tempfile::tempdir().unwrap();
            sh(dir.path(), &["init", "-q"]);
            fs::write(dir.path().join("keep.txt"), "v1\n").unwrap();
            fs::write(dir.path().join("doomed.txt"), "tracked\n").unwrap();
            sh(dir.path(), &["add", "."]);
            sh(dir.path(), &["commit", "-q", "-m", "init"]);
            Some(dir)
        }

        #[test]
        fn a_restore_round_trips_and_its_pin_undoes_it() {
            let Some(tmp) = new_repo() else { return };
            let dir = tmp.path();
            let repo = discover(dir).unwrap().unwrap();
            let head = sh(dir, &["rev-parse", "HEAD"]);
            let index_before = fs::read(&repo.index).unwrap();

            // The turn to undo: an edit, a deletion, a new file in a new dir,
            // and a secret the checkpoints never capture.
            fs::write(dir.join("keep.txt"), "v2\n").unwrap();
            fs::remove_file(dir.join("doomed.txt")).unwrap();
            fs::create_dir_all(dir.join("new/deep")).unwrap();
            fs::write(dir.join("new/deep/file.txt"), "added\n").unwrap();
            fs::write(dir.join(".env"), "TOKEN=x\n").unwrap();

            let plan = prepare(&repo, 4, &head).unwrap();
            assert!(plan
                .pinned
                .refname
                .starts_with("refs/agent-terminal/4/pre-restore-"));
            assert!(plan.changed.contains(&"keep.txt".to_string()));
            assert_eq!(plan.deleted, vec!["new/deep/file.txt".to_string()]);
            assert!(plan.pinned.untouched.iter().any(|s| s.path == ".env"));

            let done = apply(&repo, &plan).unwrap();
            assert!(done.mismatched.is_empty(), "{:?}", done.mismatched);
            assert_eq!(fs::read_to_string(dir.join("keep.txt")).unwrap(), "v1\n");
            assert_eq!(
                fs::read_to_string(dir.join("doomed.txt")).unwrap(),
                "tracked\n"
            );
            assert!(!dir.join("new").exists(), "emptied directories go too");
            assert_eq!(fs::read_to_string(dir.join(".env")).unwrap(), "TOKEN=x\n");
            // HEAD and the index are the user's, untouched.
            assert_eq!(sh(dir, &["rev-parse", "HEAD"]), head);
            assert_eq!(fs::read(&repo.index).unwrap(), index_before);

            // Undo the undo: back to the pinned state.
            let back = prepare(&repo, 4, &plan.pinned.commit).unwrap();
            let done = apply(&repo, &back).unwrap();
            assert!(done.mismatched.is_empty(), "{:?}", done.mismatched);
            assert_eq!(fs::read_to_string(dir.join("keep.txt")).unwrap(), "v2\n");
            assert!(!dir.join("doomed.txt").exists());
            assert_eq!(
                fs::read_to_string(dir.join("new/deep/file.txt")).unwrap(),
                "added\n"
            );
        }

        #[test]
        fn a_change_after_preparing_stops_the_restore() {
            let Some(tmp) = new_repo() else { return };
            let dir = tmp.path();
            let repo = discover(dir).unwrap().unwrap();
            let head = sh(dir, &["rev-parse", "HEAD"]);
            fs::write(dir.join("keep.txt"), "v2\n").unwrap();
            let plan = prepare(&repo, 3, &head).unwrap();
            // The agent keeps writing while the confirmation is up.
            fs::write(dir.join("keep.txt"), "v3, not pinned\n").unwrap();
            let err = apply(&repo, &plan).unwrap_err();
            assert!(err.contains("nothing was restored"), "{err}");
            assert_eq!(
                fs::read_to_string(dir.join("keep.txt")).unwrap(),
                "v3, not pinned\n"
            );
        }

        #[test]
        fn the_pin_is_kept_when_there_is_something_to_undo_and_dropped_when_not() {
            let Some(tmp) = new_repo() else { return };
            let repo = discover(tmp.path()).unwrap().unwrap();
            let head = sh(tmp.path(), &["rev-parse", "HEAD"]);
            let plan = prepare(&repo, 2, &head).unwrap();
            assert!(plan.changed.is_empty());
            let listed = sh(tmp.path(), &["for-each-ref", "refs/agent-terminal/2/"]);
            assert!(!listed.contains("pre-restore-"), "{listed}");

            fs::write(tmp.path().join("keep.txt"), "v2\n").unwrap();
            let plan = prepare(&repo, 2, &head).unwrap();
            assert!(!plan.changed.is_empty());
            let listed = sh(tmp.path(), &["for-each-ref", "refs/agent-terminal/2/"]);
            assert!(listed.contains("pre-restore-"), "{listed}");
            // A cancelled undo drops it again.
            discard(&repo, &plan.pinned);
            let listed = sh(tmp.path(), &["for-each-ref", "refs/agent-terminal/2/"]);
            assert!(!listed.contains("pre-restore-"), "{listed}");
        }

        #[test]
        fn a_skipped_file_with_an_undecodable_name_refuses_the_undo() {
            use std::os::unix::ffi::OsStrExt;
            let Some(tmp) = new_repo() else { return };
            let dir = tmp.path();
            let repo = discover(dir).unwrap().unwrap();
            let head = sh(dir, &["rev-parse", "HEAD"]);
            fs::write(dir.join("keep.txt"), "v2\n").unwrap();
            // Skipped (secret-looking suffix) and not valid UTF-8.
            let odd = dir.join(std::ffi::OsStr::from_bytes(b"odd\xff.key"));
            fs::write(&odd, "k\n").unwrap();
            let err = prepare(&repo, 6, &head).unwrap_err();
            assert!(err.contains("not valid UTF-8"), "{err}");
            let listed = sh(dir, &["for-each-ref", "refs/agent-terminal/6/"]);
            assert!(
                !listed.contains("pre-restore-"),
                "pin left behind: {listed}"
            );
            assert_eq!(fs::read_to_string(&odd).unwrap(), "k\n");
        }

        #[test]
        fn a_file_the_pin_skipped_is_never_overwritten() {
            use crate::git::{take_checkpoint, CheckpointOutcome, MAX_UNTRACKED_BYTES};
            let Some(tmp) = new_repo() else { return };
            let dir = tmp.path();
            let repo = discover(dir).unwrap().unwrap();
            // Captured small in a checkpoint...
            fs::write(dir.join("data.bin"), "small\n").unwrap();
            let CheckpointOutcome::Created(cp, _) = take_checkpoint(&repo, 8, "t").unwrap() else {
                panic!("expected a checkpoint");
            };
            // ...then grown past the cap, so no pin can hold it.
            let big = vec![b'x'; (MAX_UNTRACKED_BYTES + 1) as usize];
            fs::write(dir.join("data.bin"), &big).unwrap();
            fs::write(dir.join("keep.txt"), "later\n").unwrap();

            let plan = prepare(&repo, 8, &cp.commit).unwrap();
            assert_eq!(plan.protected, vec!["data.bin".to_string()]);
            assert!(!plan.changed.contains(&"data.bin".to_string()));
            assert!(plan.changed.contains(&"keep.txt".to_string()));

            let done = apply(&repo, &plan).unwrap();
            assert!(done.mismatched.is_empty(), "{:?}", done.mismatched);
            // Compared by size: printing 5 MiB on a failure helps no one.
            let kept = fs::read(dir.join("data.bin")).unwrap();
            assert_eq!(kept.len(), big.len(), "the skipped file was overwritten");
            assert!(kept == big, "the skipped file was overwritten");
            assert_eq!(fs::read_to_string(dir.join("keep.txt")).unwrap(), "v1\n");
        }

        #[test]
        fn a_directory_where_the_target_has_none_is_left_alone() {
            let Some(tmp) = new_repo() else { return };
            let dir = tmp.path();
            let repo = discover(dir).unwrap().unwrap();
            let head = sh(dir, &["rev-parse", "HEAD"]);
            // A nested repository: never captured, so never deleted.
            let nested = dir.join("vendor/lib");
            fs::create_dir_all(&nested).unwrap();
            sh(&nested, &["init", "-q"]);
            fs::write(nested.join("x.txt"), "x\n").unwrap();
            let plan = prepare(&repo, 1, &head).unwrap();
            apply(&repo, &plan).unwrap();
            assert!(nested.join("x.txt").exists());
        }
    }
}
