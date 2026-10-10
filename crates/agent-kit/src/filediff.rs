//! Per-file diffs against the checkpoint taken before a turn, and the temp files an external
//! diff tool is given.
//!
//! The pre-turn baseline ([`TurnBase`]) is a commit (a checkpoint, kept under
//! `refs/agent-terminal/`) or, when checkpoints are off, a bare tree that git keeps until its next
//! `gc`. The old side of a file is read from it; the new side is the working file. Both are
//! compared by [`crate::editdiff`], so a file created by the turn (untracked, so invisible to
//! `git diff <rev>`) shows like any other.
//!
//! Every path is confined to the repository: no `..`, no symlinked directory on the way, and the
//! file itself is never a symlink ([`resolve`]).
//!
//! Ceilings: a file git's snapshot skips (a secret-looking name, an untracked file over 5 MiB)
//! has no old side and reads as new. The working file is read after a symlink check, not opened
//! with `O_NOFOLLOW`, so a swap between the two is possible for a process that already owns the
//! working tree.

use std::ffi::OsStr;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::editdiff::{self, FileEdit};
use crate::git::{
    checkpoints_of, discover, git_raw, list_refs, snapshot_tree, take_checkpoint,
    CheckpointOutcome, RefEntry, SnapshotOutcome, QUERY_TIMEOUT_SECS,
};

/// Either side of a file bigger than this is not diffed.
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// How long a temp file for an external tool is kept if the app does not get to remove it.
pub const TEMP_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// A pre-turn baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnBase {
    /// The repository's working-tree root, canonical.
    pub toplevel: PathBuf,
    /// A commit or tree id: hex.
    pub rev: String,
}

/// The newest checkpoint commit of tab `key`, if it has one. The baseline when a checkpoint
/// attempt reports "unchanged since the last one": that checkpoint already is the state.
pub fn latest_commit(entries: &[RefEntry], key: u64) -> Option<String> {
    checkpoints_of(entries, key)
        .last()
        .map(|(_, entry)| entry.commit.clone())
}

/// Takes the baseline for a turn about to start, in the repository containing `dir`.
/// `Ok(None)`: not a repository. With `keep_ref` the state is recorded as the tab's next
/// checkpoint (so it survives, and the 7-day / 50-per-tab pruning applies to it); without, as a
/// tree only. Blocking.
pub fn take_turn_base(
    dir: &Path,
    key: u64,
    label: &str,
    keep_ref: bool,
) -> Result<Option<TurnBase>, String> {
    let Some(repo) = discover(dir)? else {
        return Ok(None);
    };
    let toplevel = repo
        .toplevel
        .canonicalize()
        .map_err(|e| format!("Could not resolve the repository: {e}"))?;
    // Git can be briefly busy (an `index.lock` held by the agent mid-commit): try a few times.
    for attempt in 0..4 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(200));
        }
        let rev = if keep_ref {
            match take_checkpoint(&repo, key, label)? {
                CheckpointOutcome::Busy => continue,
                CheckpointOutcome::Created(cp, _) => cp.commit,
                CheckpointOutcome::Unchanged => latest_commit(&list_refs(&repo)?, key)
                    .ok_or_else(|| "the unchanged checkpoint is gone".to_owned())?,
            }
        } else {
            match snapshot_tree(&repo, &format!("turn-{key}"))? {
                SnapshotOutcome::Busy => continue,
                SnapshotOutcome::Taken(s) => s.tree,
            }
        };
        return Ok(Some(TurnBase { toplevel, rev }));
    }
    Err("Git is busy in this repository".to_owned())
}

/// A file inside the repository, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoFile {
    /// Relative to the repository root, plain components only.
    pub rel: PathBuf,
    /// Absolute, under the canonical root.
    pub abs: PathBuf,
}

/// `path` (absolute, or relative to `toplevel`) as a file inside `toplevel`. Refuses `..`, a path
/// outside the root, a symlinked directory on the way, and a symlink as the file itself. The file
/// need not exist (a deleted file is still a file of the repository), but its directory must.
pub fn resolve(toplevel: &Path, path: &str) -> Result<RepoFile, String> {
    let root = toplevel
        .canonicalize()
        .map_err(|e| format!("Could not resolve the repository: {e}"))?;
    let given = Path::new(path);
    let rel: PathBuf = if given.is_absolute() {
        // The agent may name the path through the root as the user typed it or as it resolves.
        if let Ok(r) = given.strip_prefix(toplevel) {
            r.to_path_buf()
        } else if let Ok(r) = given.strip_prefix(&root) {
            r.to_path_buf()
        } else if let Ok(c) = given.canonicalize() {
            c.strip_prefix(&root)
                .map(Path::to_path_buf)
                .map_err(|_| "The file is outside the repository".to_owned())?
        } else if let (Some(parent), Some(file_name)) = (given.parent(), given.file_name()) {
            let c_parent = parent
                .canonicalize()
                .map_err(|_| "The file is outside the repository".to_owned())?;
            c_parent
                .strip_prefix(&root)
                .map(|r| r.join(file_name))
                .map_err(|_| "The file is outside the repository".to_owned())?
        } else {
            return Err("The file is outside the repository".to_owned());
        }
    } else {
        given.to_path_buf()
    };
    let rel: PathBuf = rel
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect();
    let abs = crate::restore::contained(&root, &rel)
        .ok_or_else(|| "The file is outside the repository, or behind a symlink".to_owned())?;
    if std::fs::symlink_metadata(&abs).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("The file is a symlink".to_owned());
    }
    Ok(RepoFile { rel, abs })
}

/// One side of a file's diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Side {
    /// The path is not there on this side.
    Missing,
    Bytes(Vec<u8>),
    /// Over [`MAX_FILE_BYTES`]: not read.
    TooLarge,
}

impl Side {
    fn bytes(&self) -> Option<&[u8]> {
        match self {
            Side::Bytes(b) => Some(b),
            _ => None,
        }
    }
}

/// Whether `rev` is a plain object id (hex, non-empty). Anything else is refused before it can
/// reach git, where a leading `-` would read as an option.
pub fn is_object_id(rev: &str) -> bool {
    !rev.is_empty() && rev.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The content of `rel` in `rev`, which must be an object id ([`is_object_id`]). Blocking.
pub fn file_at_rev(toplevel: &Path, rev: &str, rel: &Path) -> Result<Side, String> {
    if !is_object_id(rev) {
        return Err("Not a commit id".to_owned());
    }
    // `ls-tree` answers for exactly this path (no pathspec magic: `--` and a literal).
    let listing = git_raw(
        toplevel,
        [
            OsStr::new("ls-tree"),
            OsStr::new("-z"),
            OsStr::new(rev),
            OsStr::new("--"),
            rel.as_os_str(),
        ],
        &[("GIT_LITERAL_PATHSPECS", OsStr::new("1"))],
        QUERY_TIMEOUT_SECS,
    )?;
    let text = String::from_utf8_lossy(&listing);
    let Some(entry) = text.split('\0').find(|e| !e.is_empty()) else {
        return Ok(Side::Missing);
    };
    let (meta, _name) = entry.split_once('\t').unwrap_or((entry, ""));
    let mut fields = meta.split(' ');
    let (mode, kind, sha) = (fields.next(), fields.next(), fields.next());
    match (mode, kind, sha) {
        (Some("120000"), _, _) => Err("The file was a symlink before the turn".to_owned()),
        (_, Some("blob"), Some(sha)) => {
            let size = git_raw(toplevel, ["cat-file", "-s", sha], &[], QUERY_TIMEOUT_SECS)?;
            let size: u64 = String::from_utf8_lossy(&size)
                .trim()
                .parse()
                .unwrap_or(u64::MAX);
            if size > MAX_FILE_BYTES {
                return Ok(Side::TooLarge);
            }
            git_raw(toplevel, ["cat-file", "blob", sha], &[], QUERY_TIMEOUT_SECS).map(Side::Bytes)
        }
        _ => Err("The path was not a file before the turn".to_owned()),
    }
}

/// What a file's diff for a turn came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileDiff {
    Text(FileEdit),
    /// Either side has a NUL byte.
    Binary,
    /// The file is as it was before the turn.
    Unchanged,
    /// Either side is over [`MAX_FILE_BYTES`], or too big for the built-in differ
    /// ([`editdiff::MAX_SIDE_BYTES`]).
    TooLarge,
}

/// Whether `bytes` look like binary data (git's own test: a NUL in the first 8000 bytes).
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8000)].contains(&0)
}

/// The working file's content.
pub fn read_working(file: &RepoFile) -> Result<Side, String> {
    match std::fs::symlink_metadata(&file.abs) {
        Ok(meta) if meta.file_type().is_symlink() => Err("The file is a symlink".to_owned()),
        Ok(meta) if !meta.is_file() => Err("The path is not a file".to_owned()),
        Ok(meta) if meta.len() > MAX_FILE_BYTES => Ok(Side::TooLarge),
        Ok(_) => std::fs::read(&file.abs)
            .map(Side::Bytes)
            .map_err(|e| format!("Could not read the file: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Side::Missing),
        Err(e) => Err(format!("Could not read the file: {e}")),
    }
}

/// The diff of `file` between the baseline and now. Blocking.
pub fn turn_file_diff(base: &TurnBase, file: &RepoFile) -> Result<FileDiff, String> {
    let old = file_at_rev(&base.toplevel, &base.rev, &file.rel)?;
    let new = read_working(file)?;
    // The built-in differ only sees the first `MAX_SIDE_BYTES` of a side; a bigger file would be
    // shown as a whole-looking diff of its head, so it is called too large here (the external
    // tool still opens it, up to `MAX_FILE_BYTES`).
    let over = |s: &Side| {
        s.bytes()
            .is_some_and(|b| b.len() > editdiff::MAX_SIDE_BYTES)
    };
    if old == Side::TooLarge || new == Side::TooLarge || over(&old) || over(&new) {
        return Ok(FileDiff::TooLarge);
    }
    if old.bytes().is_some_and(is_binary) || new.bytes().is_some_and(is_binary) {
        return Ok(FileDiff::Binary);
    }
    if old == new {
        return Ok(FileDiff::Unchanged);
    }
    let text = |s: &Side| s.bytes().map(|b| String::from_utf8_lossy(b).into_owned());
    let (old_text, new_text) = (text(&old), text(&new));
    let shown = file.rel.to_string_lossy();
    Ok(FileDiff::Text(editdiff::file_diff(
        &shown,
        old_text.as_deref(),
        new_text.as_deref().unwrap_or(""),
    )))
}

// ---------------------------------------------------------------------------------------------
// What a file-change card shows
// ---------------------------------------------------------------------------------------------

/// Where a shown diff came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The working files against the checkpoint taken before the turn.
    Checkpoint,
    /// The edit as the agent described it (its own `old_string`/`new_string` or diff): no
    /// checkpoint was available, or git could not answer.
    AgentEdit,
}

/// A diff ready for a card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shown {
    /// Capped at [`crate::diff::MAX_DIFF_BYTES`] / [`crate::diff::MAX_DIFF_LINES`].
    pub text: String,
    pub origin: Origin,
    /// Lines left out by the cap.
    pub omitted_lines: usize,
    /// The files the edit names, as the agent named them.
    pub files: Vec<String>,
}

/// The diff of a file-change item whose tool input is `input`. With a `base`, each named file is
/// compared against it; when there is none, or git cannot answer for one of the files, the whole
/// diff is the agent's own edit instead. `Err` when the input names no file edit at all.
/// Blocking when a base is given.
pub fn shown_for_item(base: Option<&TurnBase>, input: &serde_json::Value) -> Result<Shown, String> {
    shown_for_item_in(base, input, None)
}

/// The diff of a file-change item whose tool input is `input`. With a `base`, each named file is
/// compared against it; when there is none, or git cannot answer for one of the files, the whole
/// diff is the agent's own edit instead, falling back to on-disk inspection for newly created or
/// untracked files. `Err` when the input names no file edit at all.
/// Blocking when a base is given.
pub fn shown_for_item_in(
    base: Option<&TurnBase>,
    input: &serde_json::Value,
    cwd: Option<&Path>,
) -> Result<Shown, String> {
    let mut files = editdiff::paths_from_input(input);
    let preview = editdiff::preview_from_input(input);
    for p in preview.iter().map(|e| e.path.clone()) {
        if !files.contains(&p) {
            files.push(p);
        }
    }
    if files.is_empty() && preview.is_empty() {
        return Err("This edit carries no file diff to show.".to_owned());
    }
    let resolve_file_path = |p: &str| -> PathBuf {
        let p_path = Path::new(p);
        if p_path.is_absolute() {
            p_path.to_path_buf()
        } else if let Some(cwd) = cwd {
            cwd.join(p_path)
        } else {
            p_path.to_path_buf()
        }
    };
    let from_checkpoint = base.and_then(|base| {
        let mut text = String::new();
        for path in &files {
            let full = resolve_file_path(path);
            let full_str = full.to_string_lossy();
            let file = resolve(&base.toplevel, &full_str).ok()?;
            match turn_file_diff(base, &file).ok()? {
                FileDiff::Text(d) => text.push_str(&d.diff),
                FileDiff::Binary => {
                    text.push_str(&format!("# {path}: binary file, not shown\n"));
                }
                FileDiff::Unchanged => {
                    text.push_str(&format!("# {path}: no changes since the turn started\n"))
                }
                FileDiff::TooLarge => {
                    text.push_str(&format!("# {path}: too large to show\n"));
                }
            }
        }
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    });
    let (text, origin) = match from_checkpoint {
        Some(text) => (text, Origin::Checkpoint),
        None => {
            let agent_text = preview.iter().map(|e| e.diff.as_str()).collect::<String>();
            if !agent_text.is_empty() {
                (agent_text, Origin::AgentEdit)
            } else if !files.is_empty() {
                let mut fallback = String::new();
                for path in &files {
                    let full = resolve_file_path(path);
                    let mut file_handled = false;
                    if let Some(repo) = crate::git::discover(&full).ok().flatten() {
                        let full_str = full.to_string_lossy();
                        if let Ok(file) = resolve(&repo.toplevel, &full_str) {
                            if let Some(head) = repo.head {
                                let head_base = TurnBase {
                                    toplevel: repo.toplevel.clone(),
                                    rev: head,
                                };
                                if let Ok(FileDiff::Text(d)) = turn_file_diff(&head_base, &file) {
                                    fallback.push_str(&d.diff);
                                    file_handled = true;
                                }
                            }
                        }
                    }
                    if !file_handled && full.is_file() {
                        if let Ok(bytes) = std::fs::read(&full) {
                            if !bytes.is_empty()
                                && !is_binary(&bytes)
                                && bytes.len() <= editdiff::MAX_SIDE_BYTES
                            {
                                let content = String::from_utf8_lossy(&bytes);
                                let d = editdiff::file_diff(path, None, &content);
                                fallback.push_str(&d.diff);
                            }
                        }
                    }
                }
                if !fallback.is_empty() {
                    (fallback, Origin::Checkpoint)
                } else {
                    return Err(format!("No diff is available for {}", files.join(", ")));
                }
            } else {
                return Err("This edit carries no file diff to show.".to_owned());
            }
        }
    };
    let (kept, omitted_lines) = crate::diff::truncate(
        &text,
        crate::diff::MAX_DIFF_BYTES,
        crate::diff::MAX_DIFF_LINES,
    );
    Ok(Shown {
        text: kept.to_owned(),
        origin,
        omitted_lines,
        files,
    })
}

// ---------------------------------------------------------------------------------------------
// Temp files for an external tool
// ---------------------------------------------------------------------------------------------

/// Where temp files go: `$XDG_RUNTIME_DIR/agent-terminal/diff`. `None` without a runtime
/// directory (there is no private place to put them, and `/tmp` is not one).
pub fn temp_root(xdg_runtime_dir: Option<&str>) -> Option<PathBuf> {
    let base = xdg_runtime_dir.filter(|d| Path::new(d).is_absolute())?;
    Some(Path::new(base).join("agent-terminal").join("diff"))
}

fn private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

/// Writes `content` as `<root>/<unique>/<name>`: the directory `0700`, the file `0600`, the
/// original file name kept so the tool can pick syntax from it. Returns the file's path.
pub fn write_temp_old(root: &Path, name: &OsStr, content: &[u8]) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    // The name is a plain file name: a path component, never a path.
    let name = Path::new(name)
        .file_name()
        .filter(|n| *n == name)
        .ok_or_else(|| std::io::Error::other("not a plain file name"))?;
    private_dir(root)?;
    let dir = root.join(format!(
        "{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    private_dir(&dir)?;
    let file = dir.join(name);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&file)?;
    f.write_all(content)?;
    f.sync_all()?;
    Ok(file)
}

/// Removes temp directories under `root` not touched for `max_age` (`Duration::ZERO`: all of
/// them). Run at start-up and exit; best effort. Returns how many were removed.
pub fn sweep_temp(root: &Path, max_age: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        let old_enough = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| now.duration_since(t).unwrap_or_default() >= max_age)
            .unwrap_or(true);
        if old_enough && std::fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Removes the temp directories this process made (named `<pid>-<n>`). Run at exit: another
/// instance's files, which a tool of its own may have open, are left to the age sweep.
pub fn remove_own_temp(root: &Path) -> usize {
    let prefix = format!("{}-", std::process::id());
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        .filter(|e| std::fs::remove_dir_all(e.path()).is_ok())
        .count()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    use super::*;
    use crate::git::{git_installed, RefEntry};

    fn entry(refname: &str, commit: &str) -> RefEntry {
        RefEntry {
            refname: refname.into(),
            commit: commit.into(),
            tree: "t".into(),
            time: 1,
            parent: None,
        }
    }

    #[test]
    fn the_baseline_is_the_tabs_newest_checkpoint() {
        let entries = [
            entry("refs/agent-terminal/7/0001", "c1"),
            entry("refs/agent-terminal/9/0001", "other"),
            entry("refs/agent-terminal/7/0002", "c2"),
        ];
        assert_eq!(latest_commit(&entries, 7).as_deref(), Some("c2"));
        assert_eq!(latest_commit(&entries, 9).as_deref(), Some("other"));
        assert_eq!(latest_commit(&entries, 1), None);
    }

    /// Fixture-side git: hermetic config, a fixed identity.
    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args([
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
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn repo() -> Option<tempfile::TempDir> {
        if !git_installed() {
            eprintln!("git not installed; skipping");
            return None;
        }
        let dir = tempfile::tempdir().expect("tmp");
        sh(dir.path(), &["init", "-q"]);
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").expect("a");
        std::fs::write(dir.path().join("my file.txt"), "x\n").expect("spaced");
        std::fs::write(dir.path().join("bin.dat"), [0u8, 1, 2]).expect("bin");
        sh(dir.path(), &["add", "."]);
        sh(dir.path(), &["commit", "-q", "-m", "init"]);
        Some(dir)
    }

    #[test]
    fn a_turns_edits_diff_against_the_pre_turn_checkpoint_including_new_and_odd_files() {
        let Some(dir) = repo() else { return };
        let p = dir.path();
        // The user already had an uncommitted change when the turn started.
        std::fs::write(p.join("a.txt"), "one\nTWO\nthree\n").expect("pre-turn edit");
        let base = take_turn_base(p, 4242, "test", true)
            .expect("base")
            .expect("repo");
        assert_eq!(base.rev.len(), 40, "a checkpoint commit");

        // The turn: edits a tracked file, creates an untracked one, edits a spaced name, touches a binary.
        std::fs::write(p.join("a.txt"), "one\nTWO\nthree\nfour\n").expect("edit");
        std::fs::write(p.join("new.rs"), "fn main() {}\n").expect("new");
        std::fs::write(p.join("my file.txt"), "y\n").expect("spaced edit");
        std::fs::write(p.join("bin.dat"), [9u8, 0, 0]).expect("bin edit");

        let diff = |path: &str| {
            let file = resolve(&base.toplevel, path).expect("resolve");
            turn_file_diff(&base, &file).expect("diff")
        };
        // Against the PRE-TURN state (the user's TWO is context, not part of the turn).
        let FileDiff::Text(a) = diff("a.txt") else {
            panic!("text")
        };
        assert!(a.diff.contains(" TWO\n"), "{}", a.diff);
        assert!(a.diff.contains("+four\n"), "{}", a.diff);
        assert!(!a.diff.contains("-two"), "{}", a.diff);
        assert_eq!((a.added, a.removed), (1, 0));
        // A file the turn created is new, though git does not track it.
        let FileDiff::Text(n) = diff("new.rs") else {
            panic!("text")
        };
        assert!(n.diff.starts_with("--- /dev/null\n"), "{}", n.diff);
        // An absolute path through the root resolves to the same file.
        let abs = base.toplevel.join("my file.txt");
        let FileDiff::Text(s) = diff(abs.to_str().expect("utf8")) else {
            panic!("text")
        };
        assert!(s.diff.contains("-x\n") && s.diff.contains("+y\n"));
        assert_eq!(diff("bin.dat"), FileDiff::Binary);
        // A file the turn did not touch is unchanged.
        std::fs::write(p.join("quiet.txt"), "q\n").expect("quiet");
        let quiet_base = take_turn_base(p, 4242, "test", true)
            .expect("base")
            .expect("repo");
        let file = resolve(&quiet_base.toplevel, "quiet.txt").expect("resolve");
        assert_eq!(
            turn_file_diff(&quiet_base, &file).expect("diff"),
            FileDiff::Unchanged
        );
        // Nothing changed since: the same checkpoint is the baseline again.
        let again = take_turn_base(p, 4242, "test", true)
            .expect("base")
            .expect("repo");
        assert_eq!(again.rev, quiet_base.rev);
    }

    #[test]
    fn a_rev_that_is_not_an_object_id_never_reaches_git() {
        let Some(dir) = repo() else { return };
        for rev in ["", "-h", "--output=/tmp/x", "HEAD", "abc def"] {
            let err = file_at_rev(dir.path(), rev, Path::new("a.txt")).expect_err(rev);
            assert_eq!(err, "Not a commit id", "{rev:?}");
        }
        let head = sh(dir.path(), &["rev-parse", "HEAD"]);
        assert!(matches!(
            file_at_rev(dir.path(), &head, Path::new("a.txt")),
            Ok(Side::Bytes(_))
        ));
    }

    #[test]
    fn a_text_file_past_the_differs_window_is_too_large_not_a_partial_diff() {
        let Some(dir) = repo() else { return };
        let p = dir.path();
        let base = take_turn_base(p, 4243, "test", true)
            .expect("base")
            .expect("repo");
        // Over the built-in differ's window, under the external tool's cap.
        let big = "line\n".repeat(editdiff::MAX_SIDE_BYTES / 5 + 10);
        assert!(big.len() as u64 <= MAX_FILE_BYTES);
        std::fs::write(p.join("a.txt"), big).expect("big");
        let file = resolve(&base.toplevel, "a.txt").expect("resolve");
        assert_eq!(
            turn_file_diff(&base, &file).expect("diff"),
            FileDiff::TooLarge
        );
    }

    #[test]
    fn without_checkpoints_the_baseline_is_a_bare_tree_and_leaves_no_ref() {
        let Some(dir) = repo() else { return };
        let p = dir.path();
        let base = take_turn_base(p, 5151, "test", false)
            .expect("base")
            .expect("repo");
        assert_eq!(sh(p, &["for-each-ref", "refs/agent-terminal"]), "");
        std::fs::write(p.join("a.txt"), "changed\n").expect("edit");
        let file = resolve(&base.toplevel, "a.txt").expect("resolve");
        assert!(matches!(
            turn_file_diff(&base, &file),
            Ok(FileDiff::Text(_))
        ));
    }

    #[test]
    fn a_card_shows_the_checkpoint_diff_and_falls_back_to_the_agents_own_edit() {
        use serde_json::json;
        let Some(dir) = repo() else { return };
        let p = dir.path();
        let base = take_turn_base(p, 6001, "test", true)
            .expect("base")
            .expect("repo");
        std::fs::write(p.join("a.txt"), "one\nTWO\nthree\n").expect("edit");
        let input = json!({"file_path": p.join("a.txt"), "old_string": "two", "new_string": "TWO"});

        let shown = shown_for_item(Some(&base), &input).expect("shown");
        assert_eq!(shown.origin, Origin::Checkpoint);
        assert!(
            shown
                .text
                .starts_with("--- a/a.txt\n+++ b/a.txt\n@@ -1,3 +1,3 @@\n"),
            "{}",
            shown.text
        );
        assert!(shown.text.contains("-two\n+TWO\n"));
        assert_eq!(shown.omitted_lines, 0);

        // No baseline: the agent's own old/new strings, as an `@@ edit @@` fragment.
        let fallback = shown_for_item(None, &input).expect("fallback");
        assert_eq!(fallback.origin, Origin::AgentEdit);
        assert!(
            fallback.text.contains("@@ edit @@\n-two\n+TWO\n"),
            "{}",
            fallback.text
        );

        // A path outside the repository cannot be compared: the agent's edit is shown instead.
        let outside = json!({"file_path": "/etc/hostname", "old_string": "a", "new_string": "b"});
        assert_eq!(
            shown_for_item(Some(&base), &outside).expect("shown").origin,
            Origin::AgentEdit
        );

        // A binary file says so; an untouched one says so.
        std::fs::write(p.join("bin.dat"), [7u8, 0]).expect("bin");
        let bin = json!({"file_path": "bin.dat", "content": "ignored"});
        assert!(shown_for_item(Some(&base), &bin)
            .expect("shown")
            .text
            .contains("bin.dat: binary file"));
        let quiet = json!({"file_path": "my file.txt", "old_string": "x", "new_string": "x"});
        assert!(shown_for_item(Some(&base), &quiet)
            .expect("shown")
            .text
            .contains("no changes since the turn"));

        // Not an edit at all.
        assert!(shown_for_item(None, &json!({"command": "ls"})).is_err());
    }

    #[test]
    fn an_agy_edit_with_target_file_is_shown_as_agent_edit() {
        use serde_json::json;
        let input = json!({
            "TargetFile": "calc.py",
            "TargetContent": "def add(a, b):\n    return a + b\n",
            "ReplacementContent": "def add(a, b):\n    return a + b\n\ndef multiply(a, b):\n    return a * b\n",
        });
        let shown = shown_for_item(None, &input).expect("shown");
        assert_eq!(shown.origin, Origin::AgentEdit);
        assert!(shown.text.contains("+def multiply(a, b):"));

        let write_input = json!({
            "TargetFile": "calc.py",
            "CodeContent": "def multiply(a, b):\n    return a * b\n",
        });
        let write_shown = shown_for_item(None, &write_input).expect("shown");
        assert_eq!(write_shown.origin, Origin::AgentEdit);
        assert!(write_shown.text.contains("+def multiply(a, b):"));
    }

    #[test]
    fn a_codex_unified_diff_is_shown_as_given_and_a_huge_one_is_cut() {
        use serde_json::json;
        let input =
            json!([{"path": "a.rs", "kind": {"type": "update"}, "diff": "@@ -1 +1 @@\n-o\n+n\n"}]);
        let shown = shown_for_item(None, &input).expect("shown");
        assert_eq!(shown.origin, Origin::AgentEdit);
        assert_eq!(shown.text, "--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-o\n+n\n");

        let big: String = (0..crate::diff::MAX_DIFF_LINES + 50)
            .map(|i| format!("+line {i}\n"))
            .collect();
        let input = json!({"file_path": "big.txt", "content": big});
        let shown = shown_for_item(None, &input).expect("shown");
        assert!(shown.omitted_lines > 0);
        assert!(shown.text.lines().count() <= crate::diff::MAX_DIFF_LINES);
    }

    #[test]
    fn a_directory_that_is_not_a_repository_has_no_baseline() {
        if !git_installed() {
            return;
        }
        let dir = tempfile::tempdir().expect("tmp");
        assert_eq!(take_turn_base(dir.path(), 1, "t", true), Ok(None));
    }

    #[test]
    fn only_files_inside_the_repository_resolve() {
        let dir = tempfile::tempdir().expect("tmp");
        let top = dir.path().canonicalize().expect("canon");
        std::fs::create_dir_all(top.join("src")).expect("src");
        std::fs::write(top.join("src/a.rs"), "x").expect("file");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret"), "s").expect("secret");
        std::os::unix::fs::symlink(outside.path(), top.join("link")).expect("dir link");
        std::os::unix::fs::symlink(top.join("src/a.rs"), top.join("src/alias.rs"))
            .expect("file link");

        assert_eq!(
            resolve(&top, "src/a.rs").expect("ok").rel,
            PathBuf::from("src/a.rs")
        );
        assert_eq!(
            resolve(&top, "./src/a.rs").expect("dot").rel,
            PathBuf::from("src/a.rs")
        );
        let abs = top.join("src/a.rs");
        assert_eq!(
            resolve(&top, abs.to_str().expect("utf8")).expect("abs").abs,
            abs
        );
        // A file that does not exist yet (or any more) is still a file of the repository.
        assert!(resolve(&top, "src/deleted.rs").is_ok());
        for bad in [
            "../x",
            "src/../../x",
            "/etc/passwd",
            "link/secret",
            "src/alias.rs",
            "nodir/x",
        ] {
            assert!(resolve(&top, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn temp_files_are_private_keep_their_name_and_are_swept() {
        let dir = tempfile::tempdir().expect("tmp");
        let root = temp_root(dir.path().to_str()).expect("root");
        assert!(root.ends_with("agent-terminal/diff"));
        assert_eq!(temp_root(None), None);
        assert_eq!(temp_root(Some("relative")), None);

        let file = write_temp_old(&root, OsStr::new("my file.rs"), b"old\n").expect("write");
        assert_eq!(file.file_name(), Some(OsStr::new("my file.rs")));
        assert_eq!(std::fs::read(&file).expect("read"), b"old\n");
        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(file.parent().expect("dir")), 0o700);
        assert_eq!(mode(&root), 0o700);
        // The same name twice does not collide.
        let second = write_temp_old(&root, OsStr::new("my file.rs"), b"").expect("again");
        assert_ne!(second, file);
        // A path is not a file name.
        assert!(write_temp_old(&root, OsStr::new("../x"), b"").is_err());
        assert!(write_temp_old(&root, OsStr::new("a/b"), b"").is_err());

        // Younger than a day: kept. Aged: removed.
        assert_eq!(sweep_temp(&root, TEMP_MAX_AGE), 0);
        let old = file.parent().expect("dir");
        let two_days = SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60);
        std::fs::File::open(old)
            .expect("open")
            .set_modified(two_days)
            .expect("age");
        assert_eq!(sweep_temp(&root, TEMP_MAX_AGE), 1);
        assert!(!file.exists() && second.exists());
        // At exit this process's own directories go; another instance's stay.
        let foreign = root.join("1-0");
        std::fs::create_dir(&foreign).expect("foreign");
        assert_eq!(remove_own_temp(&root), 1);
        assert!(!second.exists() && foreign.exists());
        assert_eq!(sweep_temp(&root, Duration::ZERO), 1);
    }
}
