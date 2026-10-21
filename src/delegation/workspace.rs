//! Worktree leases for `isolated_worktree` children. Every function here blocks
//! (git): run it in `gio::spawn_blocking`.
//!
//! The AGENTS.md worktree rules hold: names reaching git never start with `-` (agent-kit's
//! prechecks) and paths follow `--`; a worktree is removed only when clean, never forced, never
//! with a tab open in it, and its branch is never deleted.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use agent_core::delegation::authority::WorkspaceScope;
use agent_core::delegation::{ErrorCode, ToolError, WorkerId};
use agent_kit::git::{git_raw, QUERY_TIMEOUT_SECS};
use agent_kit::worktree;

/// What `plan_lease` resolved at acceptance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LeasePlan {
    pub source_repo: PathBuf,
    pub base_commit: String,
    pub branch: String,
    pub path: PathBuf,
    /// The caller's tree had uncommitted changes (receipt warning `parent_changes_absent`).
    pub dirty_parent: bool,
}

/// A created worktree, canonical and checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorktreeLease {
    pub source_repo: PathBuf,
    pub worktree: PathBuf,
    pub branch: String,
    pub base_commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Cleanup {
    Removed,
    Retained(String),
}

fn unsupported(message: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::UnsupportedWorkspace, message)
}

fn untrusted(message: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::WorkspaceUntrusted, message)
}

/// The branch a worker's lease is on.
fn lease_branch(worker: &WorkerId) -> String {
    format!("agent-terminal/{worker}")
}

/// Whether `path` (absolute, no `..`) reaches no symlink: the canonical form of its longest
/// existing ancestor is that ancestor itself.
fn no_symlink_component(path: &Path) -> bool {
    if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
        return false;
    }
    let existing = path
        .ancestors()
        .find(|a| std::fs::symlink_metadata(a).is_ok());
    match existing {
        Some(a) => std::fs::canonicalize(a).is_ok_and(|real| real == a),
        None => false,
    }
}

/// A worktree path that would put the child where the sensitive-file list applies (inside a
/// `.git`, an agent config directory, …).
fn sensitive_location(path: &Path) -> bool {
    crate::approval_server::sensitive_anywhere(path, None)
}

/// Physical containment: the worktree is neither inside the source tree nor holds it, has no
/// symlink on its path, and is in no sensitive location.
fn check_location(source: &Path, path: &Path) -> Result<(), ToolError> {
    if !no_symlink_component(path) {
        return Err(untrusted("the worktree path goes through a symlink"));
    }
    if path.starts_with(source) || source.starts_with(path) {
        return Err(untrusted(
            "the worktree path overlaps the source repository",
        ));
    }
    if sensitive_location(path) {
        return Err(untrusted("the worktree path is in a sensitive location"));
    }
    Ok(())
}

/// Whether `tree` has uncommitted or untracked changes (ignored files do not count: they never
/// reach the child either way). A tree git cannot read is reported as changed.
fn has_changes(tree: &Path) -> bool {
    git_raw(
        tree,
        ["status", "--porcelain", "--untracked-files=normal"],
        &[],
        QUERY_TIMEOUT_SECS,
    )
    .map_or(true, |out| !out.is_empty())
}

fn head_of(tree: &Path) -> Result<String, String> {
    git_raw(
        tree,
        ["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
        &[],
        QUERY_TIMEOUT_SECS,
    )
    .map(|out| String::from_utf8_lossy(&out).trim().to_owned())
}

/// At acceptance: the source repository (the caller's main working tree), the explicit committed
/// base, the branch `agent-terminal/<worker>` (unused and valid) and the default worktree path.
pub(crate) fn plan_lease(
    caller_tree: &Path,
    base: &str,
    worker: &WorkerId,
    root: Option<&Path>,
) -> Result<LeasePlan, ToolError> {
    let caller = std::fs::canonicalize(caller_tree)
        .map_err(|e| unsupported(format!("cannot resolve the caller's directory: {e}")))?;
    let main = worktree::main_toplevel(&caller).map_err(unsupported)?;
    let source_repo = std::fs::canonicalize(&main)
        .map_err(|e| unsupported(format!("cannot resolve the repository: {e}")))?;
    // Resolved in the caller's own tree, so `HEAD` is the caller's HEAD.
    let base_commit = worktree::resolve_base(&caller, base)
        .map_err(|e| ToolError::invalid("base", "not_a_commit").with_detail("reason", e))?;
    let branch = lease_branch(worker);
    worktree::check_new_branch(&source_repo, &branch).map_err(unsupported)?;
    let path = worktree::default_path(&source_repo, &branch, root);
    check_location(&source_repo, &path)?;
    Ok(LeasePlan {
        dirty_parent: has_changes(&caller),
        source_repo,
        base_commit,
        branch,
        path,
    })
}

/// Creates the planned worktree on its new branch at the base commit, then proves it: canonical,
/// no symlink on the way, outside the source tree, `HEAD == base_commit`, on its branch. A
/// worktree that fails the proof after creation is never removed: the error carries its path
/// (`retained_worktree`) and branch so the UI can show it as retained work.
pub(crate) fn create_lease(plan: &LeasePlan) -> Result<WorktreeLease, ToolError> {
    check_location(&plan.source_repo, &plan.path)?;
    worktree::add(
        &plan.source_repo,
        &plan.branch,
        &plan.path,
        &plan.base_commit,
    )
    .map_err(unsupported)?;
    let lease = WorktreeLease {
        source_repo: plan.source_repo.clone(),
        worktree: plan.path.clone(),
        branch: plan.branch.clone(),
        base_commit: plan.base_commit.clone(),
    };
    let proven = verify_lease(&lease).and_then(|()| match head_of(&lease.worktree) {
        Ok(head) if head == plan.base_commit => Ok(()),
        Ok(_) => Err(untrusted("the new worktree is not at the base commit")),
        Err(e) => Err(untrusted(e)),
    });
    if let Err(e) = proven {
        // Never removed: an unproven worktree may hold output a checkout hook made (ignored files
        // included), and its identity is in doubt. It is kept and reported as retained work.
        tracing::warn!(
            reason = %e.message,
            path = %lease.worktree.display(),
            "a new worktree failed its checks and was kept"
        );
        return Err(e
            .with_detail(
                "retained_worktree",
                lease.worktree.to_string_lossy().into_owned(),
            )
            .with_detail("branch", lease.branch.clone()));
    }
    Ok(lease)
}

/// Before every relaunch: the worktree is still a real directory at its canonical path, still a
/// linked worktree of the source repository, still on its branch, and still where it may be.
pub(crate) fn verify_lease(lease: &WorktreeLease) -> Result<(), ToolError> {
    let meta = std::fs::symlink_metadata(&lease.worktree)
        .map_err(|e| untrusted(format!("the worktree is gone: {e}")))?;
    if !meta.is_dir() {
        return Err(untrusted("the worktree is not a directory"));
    }
    let real = std::fs::canonicalize(&lease.worktree)
        .map_err(|e| untrusted(format!("cannot resolve the worktree: {e}")))?;
    if real != lease.worktree {
        return Err(untrusted("the worktree path goes through a symlink"));
    }
    check_location(&lease.source_repo, &lease.worktree)?;
    // A linked worktree's `.git` is a file naming its git directory; a link there could point
    // the child's git anywhere.
    let dot_git = std::fs::symlink_metadata(lease.worktree.join(".git"))
        .map_err(|e| untrusted(format!("the worktree has no .git: {e}")))?;
    if !dot_git.is_file() {
        return Err(untrusted("the worktree's .git is not a plain file"));
    }
    let main = worktree::main_toplevel(&lease.worktree).map_err(untrusted)?;
    let main = std::fs::canonicalize(main).map_err(|e| untrusted(e.to_string()))?;
    if main != lease.source_repo {
        return Err(untrusted("the worktree belongs to another repository"));
    }
    if worktree::current_branch(&lease.worktree).as_deref() != Some(lease.branch.as_str()) {
        return Err(untrusted("the worktree is no longer on its branch"));
    }
    Ok(())
}

/// After close: removes the worktree only when it is clean (nothing uncommitted, untracked or
/// ignored) and no tab is open in it. Never forced; the branch is kept.
pub(crate) fn cleanup_if_clean(
    lease: &WorktreeLease,
    tab_open_in: bool,
) -> Result<Cleanup, String> {
    if tab_open_in {
        return Ok(Cleanup::Retained("a tab is open in it".to_owned()));
    }
    if lease.branch.starts_with('-') || lease.worktree.as_os_str() == OsStr::new("") {
        return Ok(Cleanup::Retained("the lease is malformed".to_owned()));
    }
    if let Err(e) = verify_lease(lease) {
        return Ok(Cleanup::Retained(e.message));
    }
    match worktree::is_clean(&lease.worktree) {
        Ok(true) => {}
        Ok(false) => {
            return Ok(Cleanup::Retained(
                "it has changes, untracked or ignored files".to_owned(),
            ))
        }
        Err(e) => return Ok(Cleanup::Retained(e)),
    }
    worktree::remove(&lease.source_repo, &lease.worktree)?;
    Ok(Cleanup::Removed)
}

/// The lease as the authority scope it grants.
pub(crate) fn scope(lease: &WorktreeLease) -> WorkspaceScope {
    WorkspaceScope::Lease {
        source_repo: lease.source_repo.to_string_lossy().into_owned(),
        worktree: lease.worktree.to_string_lossy().into_owned(),
        base_commit: lease.base_commit.clone(),
        allowed_relative: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
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
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// `<tmp>/work/main` with two commits; `None` without git.
    fn new_repo() -> Option<(tempfile::TempDir, PathBuf)> {
        if !agent_kit::git::git_installed() {
            return None;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let main = tmp
            .path()
            .canonicalize()
            .expect("canonical")
            .join("work/main");
        fs::create_dir_all(&main).expect("mkdir");
        git(&main, &["init", "-q"]);
        fs::write(main.join("a.txt"), "a\n").expect("write");
        fs::write(main.join(".gitignore"), "build/\n").expect("write");
        git(&main, &["add", "."]);
        git(&main, &["commit", "-q", "-m", "one"]);
        fs::write(main.join("a.txt"), "a2\n").expect("write");
        git(&main, &["commit", "-q", "-am", "two"]);
        Some((tmp, main))
    }

    fn worker() -> WorkerId {
        WorkerId::parse("w_lease001").expect("id")
    }

    #[test]
    fn a_lease_is_created_at_the_base_and_leaves_the_user_state_alone() {
        let Some((_tmp, main)) = new_repo() else {
            return;
        };
        let first = git(&main, &["rev-parse", "HEAD~1"]);
        let head_before = git(&main, &["rev-parse", "HEAD"]);
        // Uncommitted user work, staged and unstaged.
        fs::write(main.join("a.txt"), "user edit\n").expect("write");
        fs::write(main.join("new.txt"), "staged\n").expect("write");
        git(&main, &["add", "new.txt"]);
        let index_before = git(&main, &["diff", "--cached", "--name-status"]);
        let status_before = git(&main, &["status", "--porcelain"]);

        let plan = plan_lease(&main, "HEAD~1", &worker(), None).expect("plan");
        assert_eq!(plan.source_repo, main);
        assert_eq!(plan.base_commit, first);
        assert_eq!(plan.branch, "agent-terminal/w_lease001");
        assert!(plan.dirty_parent, "uncommitted work is warned about");
        assert!(!plan.path.starts_with(&main));

        let lease = create_lease(&plan).expect("lease");
        assert_eq!(lease.worktree, plan.path);
        assert_eq!(git(&lease.worktree, &["rev-parse", "HEAD"]), first);
        assert_eq!(
            fs::read_to_string(lease.worktree.join("a.txt"))
                .ok()
                .as_deref(),
            Some("a\n")
        );
        assert_eq!(verify_lease(&lease), Ok(()));
        assert!(matches!(
            scope(&lease),
            WorkspaceScope::Lease { ref worktree, ref base_commit, .. }
                if Path::new(worktree) == lease.worktree && *base_commit == first
        ));

        // The user's HEAD, index and working tree are exactly as they were.
        assert_eq!(git(&main, &["rev-parse", "HEAD"]), head_before);
        assert_eq!(git(&main, &["symbolic-ref", "HEAD"]), "refs/heads/main");
        assert_eq!(
            git(&main, &["diff", "--cached", "--name-status"]),
            index_before
        );
        assert_eq!(git(&main, &["status", "--porcelain"]), status_before);

        // The same worker cannot plan a second lease: its branch exists.
        assert!(plan_lease(&main, "HEAD", &worker(), None).is_err());
    }

    #[test]
    fn a_clean_parent_has_no_warning_and_a_bad_base_is_refused() {
        let Some((_tmp, main)) = new_repo() else {
            return;
        };
        let plan = plan_lease(&main, "main", &worker(), None).expect("plan");
        assert!(!plan.dirty_parent);
        for bad in ["nope", "--all", "-x", ""] {
            let err = plan_lease(&main, bad, &worker(), None).expect_err(bad);
            assert_eq!(err.code, ErrorCode::InvalidArgument, "{bad}");
        }
    }

    #[test]
    fn cleanup_keeps_a_dirty_worktree_and_always_the_branch() {
        let Some((_tmp, main)) = new_repo() else {
            return;
        };
        let plan = plan_lease(&main, "HEAD", &worker(), None).expect("plan");
        let lease = create_lease(&plan).expect("lease");

        fs::write(lease.worktree.join("work.txt"), "child work\n").expect("write");
        assert!(matches!(
            cleanup_if_clean(&lease, false),
            Ok(Cleanup::Retained(_))
        ));
        assert!(lease.worktree.join("work.txt").exists());
        fs::remove_file(lease.worktree.join("work.txt")).expect("rm");

        // Ignored files would be lost by a remove, so they keep it too.
        fs::create_dir_all(lease.worktree.join("build")).expect("mkdir");
        fs::write(lease.worktree.join("build/out"), "x").expect("write");
        assert!(matches!(
            cleanup_if_clean(&lease, false),
            Ok(Cleanup::Retained(_))
        ));
        fs::remove_dir_all(lease.worktree.join("build")).expect("rm");

        // A tab open in it keeps it.
        assert_eq!(
            cleanup_if_clean(&lease, true),
            Ok(Cleanup::Retained("a tab is open in it".to_owned()))
        );
        assert!(lease.worktree.exists());

        assert_eq!(cleanup_if_clean(&lease, false), Ok(Cleanup::Removed));
        assert!(!lease.worktree.exists());
        // The branch survives its worktree.
        assert_eq!(
            git(
                &main,
                &[
                    "rev-parse",
                    "--verify",
                    "refs/heads/agent-terminal/w_lease001"
                ]
            ),
            plan.base_commit
        );
    }

    #[test]
    fn a_symlinked_or_nested_location_is_refused() {
        let Some((tmp, main)) = new_repo() else {
            return;
        };
        let real = tmp
            .path()
            .canonicalize()
            .expect("canonical")
            .join("real-root");
        fs::create_dir_all(&real).expect("mkdir");
        let link = tmp
            .path()
            .canonicalize()
            .expect("canonical")
            .join("link-root");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let err = plan_lease(&main, "HEAD", &worker(), Some(&link)).expect_err("symlink");
        assert_eq!(err.code, ErrorCode::WorkspaceUntrusted);
        // A root inside the source tree would nest the worktree in it.
        let err = plan_lease(&main, "HEAD", &worker(), Some(&main)).expect_err("nested");
        assert_eq!(err.code, ErrorCode::WorkspaceUntrusted);
        // A root inside a sensitive directory.
        let sensitive = main.parent().expect("parent").join(".claude");
        let err = plan_lease(&main, "HEAD", &worker(), Some(&sensitive)).expect_err("sensitive");
        assert_eq!(err.code, ErrorCode::WorkspaceUntrusted);
        // A configured plain root is fine.
        let plan = plan_lease(&main, "HEAD", &worker(), Some(&real)).expect("plan");
        assert!(plan.path.starts_with(&real));
    }

    #[test]
    fn an_unproven_worktree_is_kept_with_its_ignored_output() {
        use std::os::unix::fs::PermissionsExt;
        let Some((tmp, main)) = new_repo() else {
            return;
        };
        // A checkout hook that makes ignored build output and moves the new worktree's branch,
        // so the proof fails after `worktree add`.
        let hooks = tmp.path().canonicalize().expect("canonical").join("hooks");
        fs::create_dir_all(&hooks).expect("mkdir");
        let hook = hooks.join("post-checkout");
        fs::write(
            &hook,
            "#!/bin/sh\nmkdir -p build\necho made > build/out\ngit symbolic-ref HEAD refs/heads/elsewhere\n",
        )
        .expect("write");
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod");
        git(
            &main,
            &["config", "core.hooksPath", hooks.to_str().expect("utf8")],
        );

        let plan = plan_lease(&main, "HEAD", &worker(), None).expect("plan");
        let err = create_lease(&plan).expect_err("the proof fails");
        assert_eq!(err.code, ErrorCode::WorkspaceUntrusted);
        let details = err.details.expect("details");
        assert_eq!(
            details.get("retained_worktree").and_then(|v| v.as_str()),
            plan.path.to_str()
        );
        // Kept, with the hook's ignored output intact.
        assert!(plan.path.is_dir());
        assert_eq!(
            fs::read_to_string(plan.path.join("build/out"))
                .ok()
                .as_deref(),
            Some("made\n")
        );
    }

    #[test]
    fn verify_notices_a_moved_or_replaced_worktree() {
        let Some((tmp, main)) = new_repo() else {
            return;
        };
        let plan = plan_lease(&main, "HEAD", &worker(), None).expect("plan");
        let lease = create_lease(&plan).expect("lease");
        // Switched to another branch.
        git(&lease.worktree, &["checkout", "-q", "-b", "other"]);
        assert!(verify_lease(&lease).is_err());
        git(
            &lease.worktree,
            &["checkout", "-q", "agent-terminal/w_lease001"],
        );
        assert_eq!(verify_lease(&lease), Ok(()));
        // Pointed at through a symlink instead of its real path.
        let link = tmp
            .path()
            .canonicalize()
            .expect("canonical")
            .join("wt-link");
        std::os::unix::fs::symlink(&lease.worktree, &link).expect("symlink");
        let linked = WorktreeLease {
            worktree: link,
            ..lease.clone()
        };
        assert!(verify_lease(&linked).is_err());
        assert!(matches!(
            cleanup_if_clean(&linked, false),
            Ok(Cleanup::Retained(_))
        ));
        assert!(lease.worktree.exists());
    }
}
