//! Hermetic git for test fixtures: a fixed identity, no signing, no hooks, `main` as the
//! default branch, and neither the user's global nor the system config. Compiled only for
//! tests (`cfg(test)`, or the `test-support` feature a dependent enables in its
//! `[dev-dependencies]`), so the shipped binary never carries it.

use std::path::Path;
use std::process::{Command, Output};

/// The `-c` options every fixture git call runs with.
const HERMETIC: [&str; 10] = [
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
];

/// Runs hermetic git in `dir` and returns its output, whatever the exit status.
///
/// Panics only if git cannot be started at all.
pub fn git_output(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(HERMETIC)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"))
}

/// Runs hermetic git in `dir`, asserts it succeeded, and returns its trimmed stdout
/// (decoded lossily).
pub fn git(dir: &Path, args: &[&str]) -> String {
    let output = git_output(dir, args);
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Initialises a repository in the existing directory `dir`, writes `files` (relative paths,
/// parents created) and commits them all as `init`.
pub fn init_repo_at(dir: &Path, files: &[(&str, &[u8])]) {
    git(dir, &["init", "-q"]);
    for (name, body) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap_or_else(|e| panic!("{}: {e}", parent.display()));
        }
        std::fs::write(&path, body).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

/// Whether git is installed; says so on stderr when it is not, so a repository test that
/// returns early shows why it was skipped.
pub fn git_or_skip() -> bool {
    let installed = crate::git::git_installed();
    if !installed {
        eprintln!("git not installed; skipping");
    }
    installed
}
