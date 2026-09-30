//! New Tab in Worktree: where a new worktree goes, whether a branch name will
//! do, and the git calls that create and remove one.
//!
//! Nothing here is ever forced: a worktree is removed only when it is clean,
//! with `git worktree remove` and no `--force`, and its branch is never
//! deleted. The git calls block; run them off the main thread.

use crate::git::{git_output, git_raw, QUERY_TIMEOUT_SECS};
use std::path::{Path, PathBuf};

/// A worktree a tab was opened in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: String,
    /// The main working tree, which owns the worktree.
    pub main_toplevel: PathBuf,
}

/// A branch name as a directory name: `feat/x` becomes `feat-x`.
pub fn branch_dir_name(branch: &str) -> String {
    branch.trim().replace('/', "-")
}

/// Where a worktree for `branch` goes.
///
/// By default it is a hidden sibling of the main working tree,
/// `<parent>/.<repo>.worktrees/<branch>`. Hidden, because anything that
/// indexes every project directory beside the repository would otherwise
/// index the worktree as a second copy of it. `root`, when configured
/// (already `~`-expanded), replaces that with `<root>/<repo>/<branch>`.
pub fn default_path(main_toplevel: &Path, branch: &str, root: Option<&Path>) -> PathBuf {
    let repo = main_toplevel
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());
    let branch = branch_dir_name(branch);
    match root {
        Some(root) => root.join(&repo).join(branch),
        None => main_toplevel
            .parent()
            .unwrap_or(main_toplevel)
            .join(format!(".{repo}.worktrees"))
            .join(branch),
    }
}

/// A problem with a name that can be told without asking git. A leading `-`
/// would reach git as an option.
pub fn precheck_name(name: &str, what: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(format!("Enter a {what}"));
    }
    if name.starts_with('-') {
        return Err(format!("A {what} cannot start with '-'"));
    }
    Ok(())
}

/// The main working tree of the repository `dir` is in: the directory that
/// holds the common `.git`, whichever worktree `dir` itself is in.
pub fn main_toplevel(dir: &Path) -> Result<PathBuf, String> {
    let out = git_raw(
        dir,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        &[],
        QUERY_TIMEOUT_SECS,
    )?;
    let common = PathBuf::from(String::from_utf8_lossy(&out).trim());
    // Only an ordinary `<tree>/.git` says where the main tree is. A
    // submodule's (`.git/modules/x`), a separate or a bare git directory
    // does not, and guessing would put worktrees inside another `.git`.
    if common.file_name() != Some(std::ffi::OsStr::new(".git")) {
        return Err(format!(
            "This repository keeps its git directory at {}, which New Tab in \
             Worktree does not support (submodules and separate git directories)",
            common.display()
        ));
    }
    common
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("Unexpected git directory {}", common.display()))
}

/// The branch `dir` has checked out, if any.
pub fn current_branch(dir: &Path) -> Option<String> {
    let out = git_output(
        dir,
        ["symbolic-ref", "-q", "--short", "HEAD"],
        &[],
        QUERY_TIMEOUT_SECS,
    )
    .ok()?;
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !branch.is_empty()).then_some(branch)
}

/// Whether `branch` is a valid, unused name for a new branch.
pub fn check_new_branch(repo: &Path, branch: &str) -> Result<(), String> {
    precheck_name(branch, "branch name")?;
    let branch = branch.trim();
    let valid = git_output(
        repo,
        ["check-ref-format", "--branch", branch],
        &[],
        QUERY_TIMEOUT_SECS,
    )?;
    if !valid.status.success() {
        return Err(format!("'{branch}' is not a valid branch name"));
    }
    let exists = git_output(
        repo,
        [
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
        &[],
        QUERY_TIMEOUT_SECS,
    )?;
    if exists.status.success() {
        return Err(format!("A branch named '{branch}' already exists"));
    }
    Ok(())
}

/// The commit `base` names, so the worktree starts from exactly what was
/// checked.
pub fn resolve_base(repo: &Path, base: &str) -> Result<String, String> {
    precheck_name(base, "base")?;
    let out = git_output(
        repo,
        [
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{}^{{commit}}", base.trim()),
        ],
        &[],
        QUERY_TIMEOUT_SECS,
    )?;
    let commit = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || commit.is_empty() {
        return Err(format!(
            "'{}' is not a commit in this repository",
            base.trim()
        ));
    }
    Ok(commit)
}

/// Creates `branch` at `base` (a commit id from [`resolve_base`]) in a new
/// worktree at `path`, which must not exist yet.
pub fn add(main_toplevel: &Path, branch: &str, path: &Path, base: &str) -> Result<(), String> {
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Could not create {}: {err}", parent.display()))?;
    }
    let path_arg = path.as_os_str().to_os_string();
    let args: Vec<std::ffi::OsString> = ["worktree", "add", "-b", branch.trim(), "--"]
        .iter()
        .map(std::ffi::OsString::from)
        .chain([path_arg, base.into()])
        .collect();
    git_raw(main_toplevel, args, &[], WORKTREE_TIMEOUT_SECS)
        .map(|_| ())
        .map_err(with_prune_hint)
}

/// Creating or removing a worktree writes or deletes a whole tree and may run
/// a checkout hook, far beyond a query's budget. Killing git part-way would
/// leave a half-made worktree, so this is generous.
const WORKTREE_TIMEOUT_SECS: u64 = 120;

/// Adds what to do when git was stopped part-way, which can leave a
/// half-made worktree registered.
fn with_prune_hint(err: String) -> String {
    if err.contains("did not finish within") {
        format!(
            "{err}\n\nIt may have left a partial worktree behind. `git worktree list` \
             shows it, and `git worktree prune` clears a registration whose folder \
             is gone."
        )
    } else {
        err
    }
}

/// Whether the worktree at `path` holds nothing that removing it would lose:
/// nothing uncommitted, nothing untracked, and no ignored files either.
/// `git worktree remove` refuses the first two itself, but deletes ignored
/// files — a `node_modules`, a `.env`, build output — without asking, and a
/// fresh worktree has none, so any there were made in it. `Err` when git
/// cannot say, which is never taken as clean.
pub fn is_clean(path: &Path) -> Result<bool, String> {
    let status = git_raw(
        path,
        ["status", "--porcelain", "--untracked-files=normal"],
        &[],
        QUERY_TIMEOUT_SECS,
    )?;
    if !status.is_empty() {
        return Ok(false);
    }
    let ignored = git_raw(
        path,
        [
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ],
        &[],
        QUERY_TIMEOUT_SECS,
    )?;
    Ok(ignored.is_empty())
}

/// Removes the worktree at `path`. Never forced: git refuses a dirty one.
/// The branch is kept.
pub fn remove(main_toplevel: &Path, path: &Path) -> Result<(), String> {
    let args: Vec<std::ffi::OsString> = vec![
        "worktree".into(),
        "remove".into(),
        "--".into(),
        path.as_os_str().to_os_string(),
    ];
    git_raw(main_toplevel, args, &[], WORKTREE_TIMEOUT_SECS)
        .map(|_| ())
        .map_err(with_prune_hint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_slashes_become_dashes() {
        assert_eq!(branch_dir_name("feat/diff-panel"), "feat-diff-panel");
        assert_eq!(branch_dir_name(" fix "), "fix");
    }

    #[test]
    fn the_default_is_a_hidden_sibling_and_a_root_overrides_it() {
        let top = Path::new("/home/u/git/agent-terminal");
        assert_eq!(
            default_path(top, "feat/x", None),
            PathBuf::from("/home/u/git/.agent-terminal.worktrees/feat-x")
        );
        assert_eq!(
            default_path(top, "feat/x", Some(Path::new("/srv/wt"))),
            PathBuf::from("/srv/wt/agent-terminal/feat-x")
        );
    }

    #[test]
    fn names_that_would_read_as_options_are_refused() {
        assert!(precheck_name("-f", "branch name").is_err());
        assert!(precheck_name("  ", "branch name").is_err());
        assert!(precheck_name("feat/x", "branch name").is_ok());
    }

    mod repo {
        use super::super::*;
        use std::fs;
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

        /// `<tmp>/work/main` with one commit, so worktrees land in `<tmp>/work`.
        fn new_repo() -> Option<(tempfile::TempDir, PathBuf)> {
            if !crate::git::git_installed() {
                eprintln!("git not installed; skipping");
                return None;
            }
            let tmp = tempfile::tempdir().unwrap();
            let main = tmp.path().join("work/main");
            fs::create_dir_all(&main).unwrap();
            sh(&main, &["init", "-q"]);
            fs::write(main.join("a.txt"), "a\n").unwrap();
            fs::write(main.join(".gitignore"), "build/\n").unwrap();
            sh(&main, &["add", "."]);
            sh(&main, &["commit", "-q", "-m", "init"]);
            Some((tmp, main))
        }

        #[test]
        fn a_worktree_is_created_on_a_new_branch_and_only_removed_when_clean() {
            let Some((_tmp, main)) = new_repo() else {
                return;
            };
            // The main tree's canonical path, as git reports it.
            let main = main.canonicalize().unwrap();
            assert!(check_new_branch(&main, "feat/x").is_ok());
            assert!(check_new_branch(&main, "main").is_err(), "existing branch");
            assert!(check_new_branch(&main, "bad..name").is_err());
            assert!(check_new_branch(&main, "-b").is_err(), "reads as an option");
            assert!(resolve_base(&main, "nope").is_err());
            assert!(resolve_base(&main, "--all").is_err(), "reads as an option");

            let base = resolve_base(&main, "main").unwrap();
            let path = default_path(&main, "feat/x", None);
            add(&main, "feat/x", &path, &base).unwrap();
            assert_eq!(current_branch(&path).as_deref(), Some("feat/x"));
            // Seen from inside the worktree, the main tree is still found.
            assert_eq!(main_toplevel(&path).unwrap(), main);
            assert!(add(&main, "feat/y", &path, &base).is_err(), "path exists");

            fs::write(path.join("dirty.txt"), "x\n").unwrap();
            assert_eq!(is_clean(&path), Ok(false));
            assert!(remove(&main, &path).is_err(), "a dirty worktree is kept");
            assert!(path.exists());

            fs::remove_file(path.join("dirty.txt")).unwrap();
            // Ignored files are not dirty to git, but removing the worktree
            // would delete them, so they keep it.
            fs::create_dir_all(path.join("build")).unwrap();
            fs::write(path.join("build/out.bin"), "made here\n").unwrap();
            assert_eq!(is_clean(&path), Ok(false), "ignored files keep it");
            fs::remove_dir_all(path.join("build")).unwrap();

            assert_eq!(is_clean(&path), Ok(true));
            remove(&main, &path).unwrap();
            assert!(!path.exists());
            // The branch survives its worktree.
            assert!(check_new_branch(&main, "feat/x").is_err());
        }

        #[test]
        fn a_separate_git_dir_is_refused_rather_than_guessed() {
            if !crate::git::git_installed() {
                return;
            }
            let tmp = tempfile::tempdir().unwrap();
            let tree = tmp.path().join("tree");
            let git_dir = tmp.path().join("elsewhere.git");
            fs::create_dir_all(&tree).unwrap();
            sh(
                &tree,
                &[
                    "init",
                    "-q",
                    "--separate-git-dir",
                    git_dir.to_str().unwrap(),
                ],
            );
            let err = main_toplevel(&tree).unwrap_err();
            assert!(err.contains("does not support"), "{err}");
        }

        #[test]
        fn a_timeout_explains_how_to_clear_a_partial_worktree() {
            assert!(with_prune_hint("git did not finish within 120s".into())
                .contains("git worktree prune"));
            assert_eq!(with_prune_hint("fatal: other".into()), "fatal: other");
        }
    }
}
