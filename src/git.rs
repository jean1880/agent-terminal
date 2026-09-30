//! Git plumbing for turn checkpoints: repo discovery, working-tree snapshots,
//! and the hidden refs that record them.
//!
//! A snapshot is written through a private index file, never the user's, so
//! taking one leaves the index, HEAD, the working tree and the stash exactly as
//! they were. Checkpoints live under [`REF_ROOT`], which a default `git push`
//! does not send.
//!
//! Everything here shells out to `git` and blocks: call it off the main thread.
//! The parsers and filters are pure and unit-tested on their own.

use crate::utils::run_command;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Timeout for read-only queries.
pub const QUERY_TIMEOUT_SECS: u64 = 5;
/// Timeout for each step of a snapshot. `add -u` hashes every changed file.
pub const SNAPSHOT_TIMEOUT_SECS: u64 = 10;
/// Namespace for checkpoint refs: `refs/agent-terminal/<tab key>/<seq>`.
pub const REF_ROOT: &str = "refs/agent-terminal";
/// Untracked files larger than this are left out of a snapshot, so a stray
/// build artefact that is not gitignored cannot bloat `.git/objects`.
pub const MAX_UNTRACKED_BYTES: u64 = 5 * 1024 * 1024;
/// At most this many untracked files are captured per snapshot.
pub const MAX_UNTRACKED_FILES: usize = 2_000;
/// Checkpoints kept per tab; the oldest go first.
pub const MAX_CHECKPOINTS_PER_TAB: usize = 50;
/// Checkpoints older than this are deleted, whichever tab made them.
///
/// Ceiling: a tab open for longer than this loses its own oldest checkpoints,
/// and with them the "since this tab opened" base. Upgrade path: record live
/// tab keys somewhere shared and spare them.
pub const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

const AUTHOR_NAME: &str = "agent-terminal";
const AUTHOR_EMAIL: &str = "agent-terminal@localhost";
/// Paths per `update-index` call, to stay well inside `ARG_MAX`.
const UPDATE_INDEX_BATCH: usize = 500;
/// Skipped paths listed in a checkpoint's commit message.
const MAX_SKIPPED_IN_MESSAGE: usize = 50;

/// Inherited variables that would point git at some other repository or
/// index than the directory it is run in. A terminal launched from inside a
/// git hook, for one, would carry them.
const REDIRECTING_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_PREFIX",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_NAMESPACE",
];

/// Settings that make the private index a plain, self-contained file.
///
/// A split index would have git write shared-index files into the repository
/// for it, and an fsmonitor or untracked cache would record tokens meant for
/// the user's own index. Reading the real index (which may be split) still
/// works with these off.
const SNAPSHOT_CONFIG: &[&str] = &[
    "-c",
    "core.splitIndex=false",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.untrackedCache=false",
];

fn command<I, S>(dir: &Path, args: I, env: &[(&str, &OsStr)]) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(dir);
    for var in REDIRECTING_VARS {
        cmd.env_remove(var);
    }
    // No credential prompts, no optional index locks (so a refresh never
    // races the agent's own git commands for index.lock), and stable
    // messages to match on.
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C");
    for (name, value) in env {
        cmd.env(name, value);
    }
    cmd
}

/// Runs git in `dir` and returns its output whatever the exit status.
pub fn git_output<I, S>(
    dir: &Path,
    args: I,
    env: &[(&str, &OsStr)],
    timeout_secs: u64,
) -> Result<std::process::Output, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_command(command(dir, args, env), "git", timeout_secs)
}

/// Runs git in `dir` and returns its raw stdout. A non-zero exit is an `Err`
/// carrying git's own message.
pub fn git_raw<I, S>(
    dir: &Path,
    args: I,
    env: &[(&str, &OsStr)],
    timeout_secs: u64,
) -> Result<Vec<u8>, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = git_output(dir, args, env, timeout_secs)?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(failure_message(&output.stderr))
    }
}

fn failure_message(stderr: &[u8]) -> String {
    let message = String::from_utf8_lossy(stderr).trim().to_string();
    if message.is_empty() {
        "git failed without saying why".to_string()
    } else {
        message
    }
}

fn trimmed(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

/// Whether a `git` binary can be run at all. Checked once per process: without
/// git, every tab is simply "not a repository" rather than an error.
pub fn git_installed() -> bool {
    static INSTALLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *INSTALLED.get_or_init(|| {
        let mut cmd = Command::new("git");
        cmd.arg("--version");
        let found = run_command(cmd, "git", QUERY_TIMEOUT_SECS).is_ok_and(|o| o.status.success());
        if !found {
            info!("git is not available; checkpoints and diffs are off");
        }
        found
    })
}

/// A repository as seen from one directory inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInfo {
    /// Root of the working tree (of this linked worktree, if it is one).
    pub toplevel: PathBuf,
    /// This worktree's git directory: `.git`, or `.git/worktrees/<name>`.
    pub git_dir: PathBuf,
    /// This worktree's real index file.
    pub index: PathBuf,
    /// `None` before the first commit.
    pub head: Option<String>,
    /// `None` on a detached HEAD.
    pub branch: Option<String>,
}

/// Parses `rev-parse --show-toplevel --absolute-git-dir --git-path index`,
/// run in `dir`. `--git-path` may answer relative to `dir`.
fn parse_rev_parse(stdout: &[u8], dir: &Path) -> Option<(PathBuf, PathBuf, PathBuf)> {
    let mut lines = stdout
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| PathBuf::from(OsStr::from_bytes(line)));
    let toplevel = lines.next()?;
    let git_dir = lines.next()?;
    let index = lines.next()?;
    let index = if index.is_absolute() {
        index
    } else {
        dir.join(index)
    };
    Some((toplevel, git_dir, index))
}

/// Finds the repository `dir` belongs to. `Ok(None)` means it is not in one,
/// or git is not installed; `Err` means git ran and failed.
pub fn discover(dir: &Path) -> Result<Option<RepoInfo>, String> {
    if !git_installed() {
        return Ok(None);
    }
    let output = git_output(
        dir,
        [
            "rev-parse",
            "--show-toplevel",
            "--absolute-git-dir",
            "--git-path",
            "index",
        ],
        &[],
        QUERY_TIMEOUT_SECS,
    )?;
    if !output.status.success() {
        let message = failure_message(&output.stderr);
        // Also covers being inside `.git` itself, where there is no work tree
        // to snapshot.
        if message.contains("not a git repository")
            || message.contains("must be run in a work tree")
        {
            return Ok(None);
        }
        return Err(message);
    }
    let (toplevel, git_dir, index) = parse_rev_parse(&output.stdout, dir)
        .ok_or_else(|| "git rev-parse gave an unexpected answer".to_string())?;

    let head = git_output(
        dir,
        ["rev-parse", "-q", "--verify", "HEAD^{commit}"],
        &[],
        QUERY_TIMEOUT_SECS,
    )?;
    let head = head
        .status
        .success()
        .then(|| trimmed(&head.stdout))
        .filter(|s| !s.is_empty());
    let branch = git_output(
        dir,
        ["symbolic-ref", "-q", "--short", "HEAD"],
        &[],
        QUERY_TIMEOUT_SECS,
    )?;
    let branch = branch
        .status
        .success()
        .then(|| trimmed(&branch.stdout))
        .filter(|s| !s.is_empty());

    Ok(Some(RepoInfo {
        toplevel,
        git_dir,
        index,
        head,
        branch,
    }))
}

/// Why an untracked path was left out of a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// A repository of its own; adding it would create a gitlink.
    NestedRepo,
    /// Its name marks it as likely to hold a secret.
    Secret,
    /// Larger than [`MAX_UNTRACKED_BYTES`]; carries the size.
    TooLarge(u64),
    /// Past [`MAX_UNTRACKED_FILES`].
    OverCount,
    /// Gone, or not a regular file or symlink, by the time it was checked.
    Unreadable,
}

/// An untracked path a snapshot did not capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub path: String,
    pub reason: SkipReason,
}

impl std::fmt::Display for Skipped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let why = match &self.reason {
            SkipReason::NestedRepo => "nested repository".to_string(),
            SkipReason::Secret => "may hold a secret".to_string(),
            SkipReason::TooLarge(bytes) => format!("{} KiB", bytes / 1024),
            SkipReason::OverCount => "over the file limit".to_string(),
            SkipReason::Unreadable => "unreadable".to_string(),
        };
        write!(f, "{} ({why})", self.path)
    }
}

/// Whether a path marks a file as likely to hold a secret, by its name or by a
/// directory it sits in. Deliberately broad: an `.env.example` left out of a
/// snapshot costs nothing, a real `.env` copied into `.git/objects` stays
/// there until gc. Not so broad as to match words inside ordinary source
/// names (`tokenizer.rs`, `secret_sharing.rs`), which would silently drop
/// code from every checkpoint.
pub fn is_secret_path(path: &Path) -> bool {
    const DIRS: &[&str] = &[
        ".ssh", ".aws", ".gnupg", ".kube", ".docker", ".azure", "gcloud",
    ];
    const NAMES: &[&str] = &[
        ".netrc",
        ".npmrc",
        ".pypirc",
        ".git-credentials",
        ".htpasswd",
        "token",
        "tokens.json",
    ];
    const PREFIXES: &[&str] = &[
        ".env",
        "id_rsa",
        "id_ed25519",
        "id_ecdsa",
        "id_dsa",
        "credentials",
        "secrets.",
        "client_secret",
        "service-account",
        "service_account",
    ];
    const SUFFIXES: &[&str] = &[
        ".pem",
        ".key",
        ".p12",
        ".pfx",
        ".p8",
        ".jks",
        ".keystore",
        ".kdbx",
        ".ovpn",
        ".gpg",
        ".asc",
        ".secret",
        ".token",
    ];
    let mut components: Vec<String> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect();
    let Some(name) = components.pop() else {
        return false;
    };
    components.iter().any(|dir| DIRS.contains(&dir.as_str()))
        || NAMES.contains(&name.as_str())
        || PREFIXES.iter().any(|p| name.starts_with(p))
        || SUFFIXES.iter().any(|s| name.ends_with(s))
        || name.contains(".tfstate")
}

/// Splits `ls-files -z --others` output into the paths to capture and the
/// ones to skip. `size_of` returns a path's size without following a final
/// symlink, or `None` when it is gone or not a regular file or symlink.
///
/// A nested repository is listed by git as one `dir/` entry — git does not
/// descend into it — so a trailing slash is what marks one.
pub fn filter_untracked(
    listing: &[u8],
    size_of: impl Fn(&Path) -> Option<u64>,
) -> (Vec<OsString>, Vec<Skipped>) {
    let mut kept = Vec::new();
    let mut skipped = Vec::new();
    for entry in listing.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let display = String::from_utf8_lossy(entry).to_string();
        let skip = |reason| Skipped {
            path: display.clone(),
            reason,
        };
        if entry.ends_with(b"/") {
            skipped.push(skip(SkipReason::NestedRepo));
            continue;
        }
        let path = Path::new(OsStr::from_bytes(entry));
        if is_secret_path(path) {
            skipped.push(skip(SkipReason::Secret));
            continue;
        }
        match size_of(path) {
            None => skipped.push(skip(SkipReason::Unreadable)),
            Some(size) if size > MAX_UNTRACKED_BYTES => {
                skipped.push(skip(SkipReason::TooLarge(size)))
            }
            Some(_) if kept.len() >= MAX_UNTRACKED_FILES => {
                skipped.push(skip(SkipReason::OverCount))
            }
            Some(_) => kept.push(path.as_os_str().to_os_string()),
        }
    }
    (kept, skipped)
}

/// A snapshot of the working tree, as a tree object in the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub tree: String,
    pub skipped: Vec<Skipped>,
}

/// What an attempt to snapshot produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotOutcome {
    /// Someone holds `index.lock` — most likely the agent, mid-commit. Not an
    /// error: the next trigger tries again.
    Busy,
    Taken(Snapshot),
}

/// The private index, deleted however the snapshot ends.
struct ScratchIndex(PathBuf);

impl Drop for ScratchIndex {
    fn drop(&mut self) {
        let mut lock = self.0.clone().into_os_string();
        lock.push(".lock");
        for path in [self.0.as_path(), Path::new(&lock)] {
            if let Err(err) = std::fs::remove_file(path) {
                if err.kind() != std::io::ErrorKind::NotFound {
                    warn!("Could not remove {}: {err}", path.display());
                }
            }
        }
    }
}

/// Size of `path` without following a final symlink; `None` if it is gone or
/// is neither a regular file nor a symlink.
fn untracked_size(path: &Path) -> Option<u64> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    let kind = meta.file_type();
    (kind.is_file() || kind.is_symlink()).then_some(meta.len())
}

/// Snapshots `repo`'s working tree — tracked changes and untracked,
/// non-ignored files, less anything [`filter_untracked`] skips — into a tree
/// object, without touching the user's index. `tag` keeps concurrent
/// snapshots (one per tab) from sharing a private index.
pub fn snapshot_tree(repo: &RepoInfo, tag: &str) -> Result<SnapshotOutcome, String> {
    let mut lock = repo.index.clone().into_os_string();
    lock.push(".lock");
    if Path::new(&lock).exists() {
        debug!(
            "index.lock present in {}; skipping snapshot",
            repo.toplevel.display()
        );
        return Ok(SnapshotOutcome::Busy);
    }

    let work = repo.git_dir.join("agent-terminal");
    std::fs::create_dir_all(&work)
        .map_err(|err| format!("Could not create {}: {err}", work.display()))?;
    let scratch = ScratchIndex(work.join(format!("index-{tag}")));
    let env = [("GIT_INDEX_FILE", scratch.0.as_os_str())];
    let dir = repo.toplevel.as_path();

    let seed_from_head = || -> Result<(), String> {
        match std::fs::remove_file(&scratch.0) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(format!("Could not reset the snapshot index: {err}")),
        }
        // No HEAD: a missing index file is an empty one.
        if repo.head.is_some() {
            git_raw(dir, ["read-tree", "HEAD"], &env, SNAPSHOT_TIMEOUT_SECS)?;
        }
        Ok(())
    };

    // Starting from a copy of the real index keeps its stat cache, so `add -u`
    // only re-hashes what changed. Git replaces the index by rename, so the
    // copy is of one whole version or another.
    match std::fs::copy(&repo.index, &scratch.0) {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => seed_from_head()?,
        Err(err) => return Err(format!("Could not copy the index: {err}")),
    }

    let add_tracked = || {
        let args = SNAPSHOT_CONFIG.iter().copied().chain(["add", "-u"]);
        git_raw(dir, args, &env, SNAPSHOT_TIMEOUT_SECS)
    };
    if let Err(first) = add_tracked() {
        // An index the copy could not use: start over from HEAD, once.
        warn!(
            "Snapshot of {} failed ({first}); retrying from HEAD",
            dir.display()
        );
        seed_from_head()?;
        add_tracked()?;
    }

    let listing = git_raw(
        dir,
        SNAPSHOT_CONFIG
            .iter()
            .copied()
            .chain(["ls-files", "-z", "--others", "--exclude-standard"]),
        &env,
        SNAPSHOT_TIMEOUT_SECS,
    )?;
    let (kept, skipped) = filter_untracked(&listing, |p| untracked_size(&dir.join(p)));
    if !skipped.is_empty() {
        info!(
            "Snapshot of {} left out {} untracked path(s)",
            dir.display(),
            skipped.len()
        );
    }
    // update-index, not `add`: it takes paths literally (a `*` in a name is
    // not a glob), and with --remove a file deleted since the listing is
    // skipped instead of failing the whole snapshot.
    for batch in kept.chunks(UPDATE_INDEX_BATCH) {
        let args = SNAPSHOT_CONFIG
            .iter()
            .map(OsString::from)
            .chain(["update-index", "--add", "--remove", "--"].map(OsString::from))
            .chain(batch.iter().cloned());
        git_raw(dir, args, &env, SNAPSHOT_TIMEOUT_SECS)?;
    }

    let tree = trimmed(&git_raw(
        dir,
        SNAPSHOT_CONFIG.iter().copied().chain(["write-tree"]),
        &env,
        SNAPSHOT_TIMEOUT_SECS,
    )?);
    if tree.is_empty() {
        return Err("git write-tree returned nothing".to_string());
    }
    Ok(SnapshotOutcome::Taken(Snapshot { tree, skipped }))
}

/// The ref for checkpoint `seq` of tab `key`.
pub fn checkpoint_ref(key: u64, seq: u32) -> String {
    format!("{REF_ROOT}/{key}/{seq:04}")
}

/// The tab key and sequence number of a checkpoint ref; `None` for anything
/// else under [`REF_ROOT`].
pub fn parse_checkpoint_ref(refname: &str) -> Option<(u64, u32)> {
    let rest = refname.strip_prefix(REF_ROOT)?.strip_prefix('/')?;
    let (key, seq) = rest.split_once('/')?;
    if !seq.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((key.parse().ok()?, seq.parse().ok()?))
}

/// One ref under [`REF_ROOT`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefEntry {
    pub refname: String,
    pub commit: String,
    pub tree: String,
    /// Committer time, seconds since the epoch.
    pub time: u64,
}

const REF_FORMAT: &str = "--format=%(refname)%00%(objectname)%00%(tree)%00%(committerdate:unix)";

/// Parses `for-each-ref` output in [`REF_FORMAT`]. Malformed lines are dropped.
pub fn parse_ref_listing(stdout: &[u8]) -> Vec<RefEntry> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\0');
            let refname = fields.next()?.to_string();
            let commit = fields.next()?.to_string();
            let tree = fields.next()?.to_string();
            let time = fields.next()?.trim().parse().ok()?;
            (!refname.is_empty() && !commit.is_empty()).then_some(RefEntry {
                refname,
                commit,
                tree,
                time,
            })
        })
        .collect()
}

/// Every ref under [`REF_ROOT`].
pub fn list_refs(repo: &RepoInfo) -> Result<Vec<RefEntry>, String> {
    let stdout = git_raw(
        &repo.toplevel,
        ["for-each-ref", REF_FORMAT, REF_ROOT],
        &[],
        QUERY_TIMEOUT_SECS,
    )?;
    Ok(parse_ref_listing(&stdout))
}

/// Tab `key`'s checkpoints, oldest first, with their sequence numbers.
pub fn checkpoints_of(entries: &[RefEntry], key: u64) -> Vec<(u32, &RefEntry)> {
    let mut found: Vec<(u32, &RefEntry)> = entries
        .iter()
        .filter_map(|e| match parse_checkpoint_ref(&e.refname)? {
            (k, seq) if k == key => Some((seq, e)),
            _ => None,
        })
        .collect();
    found.sort_by_key(|(seq, _)| *seq);
    found
}

/// Refs to delete: any older than `cutoff` (seconds since the epoch), plus
/// tab `key`'s oldest checkpoints beyond `max_per_tab`.
pub fn refs_to_prune(
    entries: &[RefEntry],
    key: u64,
    max_per_tab: usize,
    cutoff: u64,
) -> Vec<String> {
    let mut doomed: Vec<String> = entries
        .iter()
        .filter(|e| e.time < cutoff)
        .map(|e| e.refname.clone())
        .collect();
    let own = checkpoints_of(entries, key);
    let excess = own.len().saturating_sub(max_per_tab);
    for (_, entry) in own.into_iter().take(excess) {
        if !doomed.contains(&entry.refname) {
            doomed.push(entry.refname.clone());
        }
    }
    doomed
}

/// Records `tree` as a commit on top of `parent` (none for a first commit),
/// under a fixed identity: an unset `user.name` must not stop a checkpoint,
/// and signing must not prompt.
pub fn commit_tree(
    repo: &RepoInfo,
    tree: &str,
    parent: Option<&str>,
    message: &str,
) -> Result<String, String> {
    let mut args = vec!["-c", "commit.gpgsign=false", "commit-tree", tree];
    if let Some(parent) = parent {
        args.extend(["-p", parent]);
    }
    args.extend(["-m", message]);
    let env = [
        ("GIT_AUTHOR_NAME", OsStr::new(AUTHOR_NAME)),
        ("GIT_AUTHOR_EMAIL", OsStr::new(AUTHOR_EMAIL)),
        ("GIT_COMMITTER_NAME", OsStr::new(AUTHOR_NAME)),
        ("GIT_COMMITTER_EMAIL", OsStr::new(AUTHOR_EMAIL)),
    ];
    let commit = trimmed(&git_raw(&repo.toplevel, args, &env, SNAPSHOT_TIMEOUT_SECS)?);
    if commit.is_empty() {
        return Err("git commit-tree returned nothing".to_string());
    }
    Ok(commit)
}

/// Points `refname` at `commit`, refusing if the ref already exists — an
/// empty old value means "must not exist" — so nothing is ever overwritten.
pub fn create_ref(repo: &RepoInfo, refname: &str, commit: &str) -> Result<(), String> {
    git_raw(
        &repo.toplevel,
        ["update-ref", refname, commit, ""],
        &[],
        QUERY_TIMEOUT_SECS,
    )
    .map(|_| ())
}

/// Deletes `refname`.
pub fn delete_ref(repo: &RepoInfo, refname: &str) -> Result<(), String> {
    git_raw(
        &repo.toplevel,
        ["update-ref", "-d", refname],
        &[],
        QUERY_TIMEOUT_SECS,
    )
    .map(|_| ())
}

/// A checkpoint that was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub seq: u32,
    pub refname: String,
    pub commit: String,
    pub tree: String,
}

/// What [`take_checkpoint`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointOutcome {
    /// The index was locked; try again at the next trigger.
    Busy,
    /// Nothing changed since the tab's last checkpoint.
    Unchanged,
    Created(Checkpoint, Vec<Skipped>),
}

fn checkpoint_message(seq: u32, key: u64, label: &str, skipped: &[Skipped]) -> String {
    let mut message = format!("checkpoint {seq} · tab {key} · {label}");
    if !skipped.is_empty() {
        message.push_str("\n\nNot captured:\n");
        for entry in skipped.iter().take(MAX_SKIPPED_IN_MESSAGE) {
            message.push_str(&format!("- {entry}\n"));
        }
        if skipped.len() > MAX_SKIPPED_IN_MESSAGE {
            message.push_str(&format!(
                "- … {} more\n",
                skipped.len() - MAX_SKIPPED_IN_MESSAGE
            ));
        }
    }
    message
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Snapshots `repo` and, if anything changed since tab `key`'s last
/// checkpoint, records the next one. `label` names the tab's profile in the
/// commit message. Pruning failures are logged, not returned: a checkpoint
/// that was written succeeded.
pub fn take_checkpoint(
    repo: &RepoInfo,
    key: u64,
    label: &str,
) -> Result<CheckpointOutcome, String> {
    take_checkpoint_capped(repo, key, label, MAX_CHECKPOINTS_PER_TAB)
}

fn take_checkpoint_capped(
    repo: &RepoInfo,
    key: u64,
    label: &str,
    max_per_tab: usize,
) -> Result<CheckpointOutcome, String> {
    let snapshot = match snapshot_tree(repo, &key.to_string())? {
        SnapshotOutcome::Busy => return Ok(CheckpointOutcome::Busy),
        SnapshotOutcome::Taken(snapshot) => snapshot,
    };
    let entries = list_refs(repo)?;
    let own = checkpoints_of(&entries, key);
    let last = own.last().map(|(seq, entry)| (*seq, *entry));
    if last.is_some_and(|(_, entry)| entry.tree == snapshot.tree) {
        return Ok(CheckpointOutcome::Unchanged);
    }

    let seq = last.map_or(1, |(seq, _)| seq.saturating_add(1));
    let parent = last
        .map(|(_, entry)| entry.commit.as_str())
        .or(repo.head.as_deref());
    let message = checkpoint_message(seq, key, label, &snapshot.skipped);
    let commit = commit_tree(repo, &snapshot.tree, parent, &message)?;
    let refname = checkpoint_ref(key, seq);
    create_ref(repo, &refname, &commit)?;
    debug!("Checkpoint {refname} -> {commit}");

    let mut entries = entries;
    entries.push(RefEntry {
        refname: refname.clone(),
        commit: commit.clone(),
        tree: snapshot.tree.clone(),
        time: now_unix(),
    });
    let cutoff = now_unix().saturating_sub(RETENTION.as_secs());
    for doomed in refs_to_prune(&entries, key, max_per_tab, cutoff) {
        if let Err(err) = delete_ref(repo, &doomed) {
            warn!("Could not prune {doomed}: {err}");
        }
    }

    Ok(CheckpointOutcome::Created(
        Checkpoint {
            seq,
            refname,
            commit,
            tree: snapshot.tree,
        },
        snapshot.skipped,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(refname: &str, tree: &str, time: u64) -> RefEntry {
        RefEntry {
            refname: refname.to_string(),
            commit: format!("c-{refname}"),
            tree: tree.to_string(),
            time,
        }
    }

    #[test]
    fn rev_parse_answer_resolves_a_relative_index_against_the_dir() {
        let out = b"/repo\n/repo/.git\n.git/index\n";
        let (top, git_dir, index) = parse_rev_parse(out, Path::new("/repo/sub")).unwrap();
        assert_eq!(top, PathBuf::from("/repo"));
        assert_eq!(git_dir, PathBuf::from("/repo/.git"));
        assert_eq!(index, PathBuf::from("/repo/sub/.git/index"));

        let out = b"/wt\n/repo/.git/worktrees/wt\n/repo/.git/worktrees/wt/index\n";
        let (_, _, index) = parse_rev_parse(out, Path::new("/wt")).unwrap();
        assert_eq!(index, PathBuf::from("/repo/.git/worktrees/wt/index"));

        assert!(parse_rev_parse(b"/repo\n", Path::new("/repo")).is_none());
    }

    #[test]
    fn secret_paths_are_recognised_case_insensitively() {
        for path in [
            ".env",
            "app/.env.local",
            "server.PEM",
            "tls.key",
            "id_ed25519",
            "id_rsa.pub",
            "credentials.json",
            "vault.kdbx",
            "terraform.tfstate.backup",
            ".netrc",
            ".npmrc",
            ".pypirc",
            ".git-credentials",
            ".htpasswd",
            "token",
            "tokens.json",
            "gh.token",
            "api.secret",
            "secrets.yaml",
            "client_secret_123.json",
            "service-account-prod.json",
            "AuthKey_ABC.p8",
            "release.jks",
            "upload.keystore",
            "vpn.ovpn",
            "backup.gpg",
            "signing.asc",
            "home/.ssh/config",
            ".aws/config",
            "x/.gnupg/pubring.kbx",
            ".kube/config",
            ".docker/config.json",
        ] {
            assert!(is_secret_path(Path::new(path)), "{path}");
        }
        for path in [
            "main.rs",
            "keyboard.rs",
            "environment.md",
            "README.md",
            "src/tokenizer.rs",
            "src/secret_sharing.rs",
            "docs/ssh.md",
            "identity.rs",
        ] {
            assert!(!is_secret_path(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn inherited_repository_redirects_are_stripped() {
        // A terminal started from inside a git hook inherits these; honouring
        // them would snapshot, or write refs into, some other repository.
        let cmd = command(Path::new("/"), ["status"], &[]);
        let envs: Vec<_> = cmd.get_envs().collect();
        for var in REDIRECTING_VARS {
            assert!(
                envs.contains(&(OsStr::new(var), None)),
                "{var} is not removed"
            );
        }
        assert!(envs.contains(&(OsStr::new("GIT_OPTIONAL_LOCKS"), Some(OsStr::new("0")))));
        // A caller's own GIT_INDEX_FILE is applied after the strip.
        let cmd = command(
            Path::new("/"),
            ["status"],
            &[("GIT_INDEX_FILE", OsStr::new("/tmp/i"))],
        );
        let index = cmd
            .get_envs()
            .filter(|(k, _)| *k == OsStr::new("GIT_INDEX_FILE"))
            .last()
            .and_then(|(_, v)| v);
        assert_eq!(index, Some(OsStr::new("/tmp/i")));
    }

    #[test]
    fn untracked_filter_skips_nested_repos_secrets_and_large_files() {
        let listing = b"src/new.rs\0vendor/x/\0newdir/.env\0big.bin\0gone.txt\0";
        let size = |p: &Path| match p.to_str()? {
            "big.bin" => Some(MAX_UNTRACKED_BYTES + 1),
            "gone.txt" => None,
            _ => Some(10),
        };
        let (kept, skipped) = filter_untracked(listing, size);
        assert_eq!(kept, vec![OsString::from("src/new.rs")]);
        let reasons: Vec<_> = skipped
            .iter()
            .map(|s| (s.path.as_str(), &s.reason))
            .collect();
        assert_eq!(
            reasons,
            vec![
                ("vendor/x/", &SkipReason::NestedRepo),
                ("newdir/.env", &SkipReason::Secret),
                ("big.bin", &SkipReason::TooLarge(MAX_UNTRACKED_BYTES + 1)),
                ("gone.txt", &SkipReason::Unreadable),
            ]
        );
    }

    #[test]
    fn untracked_filter_caps_the_file_count() {
        let listing: Vec<u8> = (0..MAX_UNTRACKED_FILES + 3)
            .flat_map(|i| format!("f{i}\0").into_bytes())
            .collect();
        let (kept, skipped) = filter_untracked(&listing, |_| Some(1));
        assert_eq!(kept.len(), MAX_UNTRACKED_FILES);
        assert_eq!(skipped.len(), 3);
        assert!(skipped.iter().all(|s| s.reason == SkipReason::OverCount));
    }

    #[test]
    fn checkpoint_refs_round_trip_and_reject_other_names() {
        let name = checkpoint_ref(42, 7);
        assert_eq!(name, "refs/agent-terminal/42/0007");
        assert_eq!(parse_checkpoint_ref(&name), Some((42, 7)));
        assert_eq!(
            parse_checkpoint_ref("refs/agent-terminal/42/12345"),
            Some((42, 12345))
        );
        assert_eq!(
            parse_checkpoint_ref("refs/agent-terminal/42/pre-restore-1"),
            None
        );
        assert_eq!(parse_checkpoint_ref("refs/heads/main"), None);
        assert_eq!(parse_checkpoint_ref("refs/agent-terminal/x/0001"), None);
    }

    #[test]
    fn ref_listing_parses_and_drops_malformed_lines() {
        let out = b"refs/agent-terminal/1/0001\x00abc\x00def\x001700000000\ngarbage\n";
        assert_eq!(
            parse_ref_listing(out),
            vec![RefEntry {
                refname: "refs/agent-terminal/1/0001".into(),
                commit: "abc".into(),
                tree: "def".into(),
                time: 1_700_000_000,
            }]
        );
    }

    #[test]
    fn pruning_takes_expired_refs_and_the_tabs_oldest_past_the_cap() {
        let entries = vec![
            entry("refs/agent-terminal/1/0003", "t3", 300),
            entry("refs/agent-terminal/1/0001", "t1", 100),
            entry("refs/agent-terminal/1/0002", "t2", 200),
            entry("refs/agent-terminal/2/0001", "u1", 50),
            entry("refs/agent-terminal/2/0002", "u2", 400),
        ];
        let doomed = refs_to_prune(&entries, 1, 2, 60);
        assert_eq!(
            doomed,
            vec![
                "refs/agent-terminal/2/0001".to_string(),
                "refs/agent-terminal/1/0001".to_string(),
            ]
        );
        // A ref both expired and over the cap is listed once.
        assert_eq!(refs_to_prune(&entries, 1, 2, 150).len(), 2);
    }

    #[test]
    fn checkpoint_message_caps_the_skipped_list() {
        let skipped: Vec<Skipped> = (0..MAX_SKIPPED_IN_MESSAGE + 2)
            .map(|i| Skipped {
                path: format!("f{i}"),
                reason: SkipReason::Secret,
            })
            .collect();
        let message = checkpoint_message(3, 9, "Claude", &skipped);
        assert!(message.starts_with("checkpoint 3 · tab 9 · Claude\n\nNot captured:\n"));
        assert!(message.ends_with("- … 2 more\n"));
        assert_eq!(
            checkpoint_message(1, 9, "Claude", &[]),
            "checkpoint 1 · tab 9 · Claude"
        );
    }

    /// Tests against a real repository. Skipped when git is not installed.
    mod repo {
        use super::super::*;
        use std::fs;

        /// Fixture-side git: hermetic config, a fixed identity, no hooks.
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
            fs::write(dir.path().join("a.txt"), "one\n").unwrap();
            fs::write(dir.path().join("gone.txt"), "bye\n").unwrap();
            fs::write(dir.path().join(".gitignore"), "ignored/\n").unwrap();
            sh(dir.path(), &["add", "."]);
            sh(dir.path(), &["commit", "-q", "-m", "init"]);
            Some(dir)
        }

        fn tree_files(dir: &Path, tree: &str) -> Vec<String> {
            let listing = sh(dir, &["ls-tree", "-r", "--name-only", tree]);
            listing.lines().map(str::to_string).collect()
        }

        fn blob(dir: &Path, tree: &str, path: &str) -> String {
            sh(dir, &["cat-file", "-p", &format!("{tree}:{path}")])
        }

        /// Everything a snapshot must leave alone.
        fn user_state(dir: &Path, repo: &RepoInfo) -> (Vec<u8>, String, String, String) {
            let mut files = Vec::new();
            for entry in walk(dir) {
                files.push(entry);
            }
            files.sort();
            (
                fs::read(&repo.index).unwrap_or_default(),
                sh(dir, &["rev-parse", "HEAD"]),
                sh(dir, &["stash", "list"]),
                files.join("\n"),
            )
        }

        fn walk(dir: &Path) -> Vec<String> {
            let mut out = Vec::new();
            let mut stack = vec![dir.to_path_buf()];
            while let Some(d) = stack.pop() {
                for e in fs::read_dir(&d).unwrap() {
                    let e = e.unwrap();
                    let p = e.path();
                    if p.file_name().is_some_and(|n| n == ".git") {
                        continue;
                    }
                    if p.is_dir() {
                        stack.push(p);
                    } else {
                        let body = fs::read(&p).unwrap_or_default();
                        out.push(format!("{}:{}", p.display(), body.len()));
                    }
                }
            }
            out
        }

        fn taken(outcome: SnapshotOutcome) -> Snapshot {
            match outcome {
                SnapshotOutcome::Taken(s) => s,
                SnapshotOutcome::Busy => panic!("unexpected Busy"),
            }
        }

        fn make_changes(dir: &Path) {
            fs::write(dir.join("a.txt"), "two\n").unwrap();
            fs::remove_file(dir.join("gone.txt")).unwrap();
            fs::create_dir_all(dir.join("newdir")).unwrap();
            fs::write(dir.join("newdir/ok.txt"), "ok\n").unwrap();
            fs::write(dir.join("newdir/.env"), "TOKEN=x\n").unwrap();
            fs::write(
                dir.join("newdir/big.bin"),
                vec![0u8; (MAX_UNTRACKED_BYTES + 1) as usize],
            )
            .unwrap();
            // Names that break naive argument or pathspec handling: a glob
            // character, a leading dash, and bytes that are not UTF-8.
            fs::write(dir.join("odd *name.txt"), "glob\n").unwrap();
            fs::write(dir.join("-dash.txt"), "dash\n").unwrap();
            fs::write(dir.join(OsStr::from_bytes(b"bad\xff.txt")), "bytes\n").unwrap();
            fs::create_dir_all(dir.join("ignored")).unwrap();
            fs::write(dir.join("ignored/x.txt"), "x\n").unwrap();
        }

        fn check_snapshot(dir: &Path, repo: &RepoInfo) {
            make_changes(dir);
            let before = user_state(dir, repo);
            let snap = taken(snapshot_tree(repo, "t").unwrap());
            assert_eq!(user_state(dir, repo), before, "snapshot touched user state");

            let files = tree_files(dir, &snap.tree);
            assert!(files.contains(&"newdir/ok.txt".to_string()), "{files:?}");
            assert!(files.contains(&"odd *name.txt".to_string()), "{files:?}");
            assert!(files.contains(&"-dash.txt".to_string()), "{files:?}");
            // ls-tree quotes a non-UTF-8 name as an octal escape.
            assert!(files.iter().any(|f| f.contains("bad")), "{files:?}");
            assert!(!files.contains(&"gone.txt".to_string()), "{files:?}");
            assert!(!files.contains(&"newdir/.env".to_string()), "{files:?}");
            assert!(!files.contains(&"newdir/big.bin".to_string()), "{files:?}");
            assert!(
                !files.iter().any(|f| f.starts_with("ignored/")),
                "{files:?}"
            );
            assert_eq!(blob(dir, &snap.tree, "a.txt"), "two");
            let reasons: Vec<_> = snap.skipped.iter().map(|s| &s.reason).collect();
            assert!(reasons.contains(&&SkipReason::Secret));
            assert!(reasons.iter().any(|r| matches!(r, SkipReason::TooLarge(_))));
            // The private index is cleaned up.
            assert!(!repo.git_dir.join("agent-terminal/index-t").exists());
        }

        #[test]
        fn snapshot_captures_changes_and_leaves_user_state_alone() {
            let Some(tmp) = new_repo() else { return };
            let repo = discover(tmp.path()).unwrap().unwrap();
            check_snapshot(tmp.path(), &repo);
        }

        #[test]
        fn snapshot_works_with_a_split_index() {
            let Some(tmp) = new_repo() else { return };
            sh(tmp.path(), &["config", "core.splitIndex", "true"]);
            sh(tmp.path(), &["update-index", "--split-index"]);
            let shared = fs::read_dir(tmp.path().join(".git"))
                .unwrap()
                .filter_map(Result::ok)
                .any(|e| e.file_name().to_string_lossy().starts_with("sharedindex."));
            assert!(shared, "fixture did not produce a split index");
            let repo = discover(tmp.path()).unwrap().unwrap();
            check_snapshot(tmp.path(), &repo);
        }

        #[test]
        fn snapshot_works_in_a_linked_worktree() {
            let Some(tmp) = new_repo() else { return };
            let wt_parent = tempfile::tempdir().unwrap();
            let wt = wt_parent.path().join("wt");
            sh(
                tmp.path(),
                &["worktree", "add", "-q", "-b", "side", wt.to_str().unwrap()],
            );
            let repo = discover(&wt).unwrap().unwrap();
            assert!(repo.git_dir.ends_with("worktrees/wt"), "{:?}", repo.git_dir);
            assert_eq!(repo.branch.as_deref(), Some("side"));
            check_snapshot(&wt, &repo);
        }

        #[test]
        fn a_subdirectory_tab_snapshots_the_whole_repo() {
            let Some(tmp) = new_repo() else { return };
            fs::create_dir_all(tmp.path().join("sub")).unwrap();
            fs::write(tmp.path().join("a.txt"), "changed\n").unwrap();
            let repo = discover(&tmp.path().join("sub")).unwrap().unwrap();
            let snap = taken(snapshot_tree(&repo, "t").unwrap());
            assert_eq!(blob(tmp.path(), &snap.tree, "a.txt"), "changed");
        }

        #[test]
        fn nested_repos_are_skipped_not_added_as_gitlinks() {
            let Some(tmp) = new_repo() else { return };
            let nested = tmp.path().join("vendor/x");
            fs::create_dir_all(&nested).unwrap();
            sh(&nested, &["init", "-q"]);
            fs::write(nested.join("n.txt"), "n\n").unwrap();
            let repo = discover(tmp.path()).unwrap().unwrap();
            let snap = taken(snapshot_tree(&repo, "t").unwrap());
            assert!(!tree_files(tmp.path(), &snap.tree)
                .iter()
                .any(|f| f.starts_with("vendor")));
            assert!(snap
                .skipped
                .iter()
                .any(|s| s.reason == SkipReason::NestedRepo));
        }

        #[test]
        fn a_held_index_lock_is_busy_not_an_error() {
            let Some(tmp) = new_repo() else { return };
            let repo = discover(tmp.path()).unwrap().unwrap();
            let mut lock = repo.index.clone().into_os_string();
            lock.push(".lock");
            fs::write(&lock, "").unwrap();
            assert_eq!(snapshot_tree(&repo, "t").unwrap(), SnapshotOutcome::Busy);
        }

        #[test]
        fn a_corrupt_index_copy_is_retried_from_head() {
            let Some(tmp) = new_repo() else { return };
            let repo = discover(tmp.path()).unwrap().unwrap();
            fs::write(tmp.path().join("a.txt"), "changed\n").unwrap();
            fs::write(&repo.index, b"not an index").unwrap();
            let snap = taken(snapshot_tree(&repo, "t").unwrap());
            assert_eq!(blob(tmp.path(), &snap.tree, "a.txt"), "changed");
            // The user's (broken) index is still theirs to fix.
            assert_eq!(fs::read(&repo.index).unwrap(), b"not an index");
        }

        #[test]
        fn not_a_repository_is_none() {
            if !git_installed() {
                return;
            }
            let dir = tempfile::tempdir().unwrap();
            assert_eq!(discover(dir.path()).unwrap(), None);
        }

        #[test]
        fn checkpoints_dedupe_chain_and_never_overwrite() {
            let Some(tmp) = new_repo() else { return };
            let repo = discover(tmp.path()).unwrap().unwrap();
            let head = repo.head.clone().unwrap();

            let CheckpointOutcome::Created(first, _) = take_checkpoint(&repo, 7, "Claude").unwrap()
            else {
                panic!("expected a checkpoint");
            };
            assert_eq!(first.seq, 1);
            assert_eq!(first.refname, "refs/agent-terminal/7/0001");
            assert_eq!(
                sh(tmp.path(), &["rev-parse", &format!("{}^", first.commit)]),
                head
            );

            assert_eq!(
                take_checkpoint(&repo, 7, "Claude").unwrap(),
                CheckpointOutcome::Unchanged
            );

            fs::write(tmp.path().join("a.txt"), "three\n").unwrap();
            let CheckpointOutcome::Created(second, _) =
                take_checkpoint(&repo, 7, "Claude").unwrap()
            else {
                panic!("expected a checkpoint");
            };
            assert_eq!(second.seq, 2);
            assert_eq!(
                sh(tmp.path(), &["rev-parse", &format!("{}^", second.commit)]),
                first.commit
            );
            assert!(create_ref(&repo, &first.refname, &second.commit).is_err());
            // HEAD never moves.
            assert_eq!(sh(tmp.path(), &["rev-parse", "HEAD"]), head);
        }

        #[test]
        fn an_unborn_head_gets_a_parentless_checkpoint() {
            if !git_installed() {
                return;
            }
            let tmp = tempfile::tempdir().unwrap();
            sh(tmp.path(), &["init", "-q"]);
            fs::write(tmp.path().join("first.txt"), "1\n").unwrap();
            let repo = discover(tmp.path()).unwrap().unwrap();
            assert_eq!(repo.head, None);
            let CheckpointOutcome::Created(cp, _) = take_checkpoint(&repo, 1, "Claude").unwrap()
            else {
                panic!("expected a checkpoint");
            };
            assert_eq!(sh(tmp.path(), &["rev-list", "--count", &cp.commit]), "1");
            assert_eq!(tree_files(tmp.path(), &cp.tree), vec!["first.txt"]);
        }

        #[test]
        fn a_tab_keeps_at_most_the_cap() {
            let Some(tmp) = new_repo() else { return };
            let repo = discover(tmp.path()).unwrap().unwrap();
            for i in 0..4 {
                fs::write(tmp.path().join("a.txt"), format!("{i}\n")).unwrap();
                take_checkpoint_capped(&repo, 3, "Claude", 2).unwrap();
            }
            let entries = list_refs(&repo).unwrap();
            let own = checkpoints_of(&entries, 3);
            let seqs: Vec<u32> = own.iter().map(|(seq, _)| *seq).collect();
            assert_eq!(seqs, vec![3, 4]);
        }
    }
}
