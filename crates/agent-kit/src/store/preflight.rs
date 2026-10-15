//! The pre-v4 upgrade preflight: the one way an existing store reaches schema 4.
//!
//! `Store::open` runs it before anything else touches an existing database file (contract §8.2).
//! The order is the one proven in `docs/qa/delegation-discovery/sdk-and-store.md`:
//!
//! 1. A missing file is a fresh install: no preflight, `Store::open` creates it as before.
//! 2. `flock(<db>.upgrade-lock)`; open `READ_WRITE` without `CREATE`; read `sqlite_master` with no
//!    write (no `CREATE TABLE IF NOT EXISTS`, no WAL switch).
//! 3. `foreign_keys = OFF`, `BEGIN IMMEDIATE`, then the version is read again under the reservation.
//! 4. A **separate** read-only connection backs the reserved, unmodified database up with one
//!    bounded `Backup::step(-1)` that must return `Done` (never `run_to_completion`, which loops on
//!    `Busy` forever). The copy is switched to `journal_mode=DELETE`, fsynced, and verified:
//!    `integrity_check`, its schema version, the row counts of the pinned snapshot, `0600` in `0700`.
//! 5. It is published with `hard_link` (never over an existing name).
//! 6. The migration runs in the same reserved transaction, which also records
//!    `meta('pre_v4_backup')`, so a concurrent opener (waiting on the lock) sees v4 and the record.
//! 7. A failure before the commit leaves the schema untouched. A backup already published then is
//!    an orphan: valid, and retained.
//!
//! A v3 app instance still running is not excluded by SQLite (an idle writer holds no lock); the
//! application's single instance is the real guard, and v4 is strictly additive so such a writer
//! cannot corrupt it.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use super::{migrate_locked, StoreError, BUSY_TIMEOUT, SCHEMA_VERSION};

type Result<T> = std::result::Result<T, StoreError>;

/// [`super::Store::meta`] key of the published pre-v4 backup (an absolute path).
pub const PRE_V4_BACKUP_KEY: &str = "pre_v4_backup";

/// Name of the private directory, beside the database, that holds pre-upgrade backups.
pub const BACKUP_DIR: &str = "backups";

/// What the preflight did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Nothing to upgrade: an empty file (a fresh install mid-creation), handed to `Store::open`.
    Fresh,
    /// Already at this build's schema; the backup recorded when it was upgraded, if any.
    Current { backup: Option<String> },
    /// Upgraded from `from` under the published, verified `backup`.
    Migrated { from: i64, backup: PathBuf },
}

/// Points in the sequence a test can make fail.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fault {
    AfterReserve,
    AfterBackup,
    CorruptBackup,
    AfterVerify,
    BeforeCommit,
}

/// Test hooks of the preflight; empty (and free) outside tests.
#[derive(Debug, Clone, Default)]
pub(crate) struct Hooks {
    #[cfg(test)]
    pub fault: Option<Fault>,
    /// How long to hold the reservation before the backup (concurrency tests).
    #[cfg(test)]
    pub hold: std::time::Duration,
    /// Records the durability-relevant steps in order (`sync parent`, `sync backups`, `migrate`).
    #[cfg(test)]
    pub trace: Option<std::sync::Arc<std::sync::Mutex<Vec<String>>>>,
    /// Called with the database path under the reservation, before the backup source is opened.
    #[cfg(test)]
    pub before_backup: Option<fn(&Path)>,
    /// Called with the backup directory just before publication.
    #[cfg(test)]
    pub before_publish: Option<fn(&Path)>,
    /// `max_page_count` of the backup destination: a full disk.
    #[cfg(test)]
    pub backup_max_pages: Option<i64>,
}

impl Hooks {
    #[cfg(test)]
    fn fail_at(&self, at: Fault) -> Result<()> {
        if self.fault == Some(at) {
            return Err(StoreError::Upgrade(format!("injected failure: {at:?}")));
        }
        Ok(())
    }

    /// Records one step for the ordering tests; nothing outside tests.
    fn note(&self, _step: &str) {
        #[cfg(test)]
        if let Some(trace) = &self.trace {
            if let Ok(mut t) = trace.lock() {
                t.push(_step.to_owned());
            }
        }
    }
}

/// A file's identity: (device, inode), stable whatever its name becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileId {
    dev: u64,
    ino: u64,
}

impl FileId {
    fn of(meta: &fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
        }
    }
}

/// The identity of what `path` names, refusing a symlink (never followed).
fn id_of_path(path: &Path, what: &str) -> Result<FileId> {
    let meta = fs::symlink_metadata(path)
        .map_err(|e| StoreError::Upgrade(format!("stat {what} {}: {e}", path.display())))?;
    if meta.file_type().is_symlink() {
        return Err(StoreError::Upgrade(format!(
            "{what} {} became a symlink; refusing to upgrade",
            path.display()
        )));
    }
    Ok(FileId::of(&meta))
}

/// `path` still names the object identified as `expected`; else the upgrade is refused.
fn expect_same(path: &Path, expected: FileId, what: &str) -> Result<()> {
    if id_of_path(path, what)? != expected {
        return Err(StoreError::Upgrade(format!(
            "{what} {} was replaced during the upgrade; refusing to continue",
            path.display()
        )));
    }
    Ok(())
}

/// The process's effective uid (the owner of `/proc/self`), when it can be read.
fn effective_uid() -> Option<u32> {
    fs::metadata("/proc/self").ok().map(|m| m.uid())
}

/// The process's effective (primary) gid, the group of `/proc/self`, when it can be read.
fn effective_gid() -> Option<u32> {
    fs::metadata("/proc/self").ok().map(|m| m.gid())
}

/// The app's own state directory, canonicalised: the directory of [`super::default_path`] for this
/// process's `XDG_STATE_HOME`/`HOME`. `None` when it cannot be worked out or does not exist.
fn app_state_dir() -> Option<PathBuf> {
    let xdg = std::env::var("XDG_STATE_HOME").ok();
    let home = std::env::var("HOME").ok();
    let db = super::default_path(xdg.as_deref(), home.as_deref())?;
    fs::canonicalize(db.parent()?).ok()
}

/// Directories tests declare to be the app's own (unique temporary paths, so tests stay hermetic
/// and never touch the real state directory).
#[cfg(test)]
static TEST_APP_DIRS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Test hook: `dir` counts as the app's own state directory.
#[cfg(test)]
pub(crate) fn treat_as_app_dir(dir: &Path) {
    if let (Ok(real), Ok(mut dirs)) = (fs::canonicalize(dir), TEST_APP_DIRS.lock()) {
        dirs.push(real);
    }
}

/// Whether `dir` is exactly the app's own state directory (by canonical path, never by name): the
/// only directory the app may make private. Any other directory is the user's and never changed.
pub(crate) fn is_app_dir(dir: &Path) -> bool {
    let Ok(real) = fs::canonicalize(dir) else {
        return false;
    };
    #[cfg(test)]
    if TEST_APP_DIRS.lock().is_ok_and(|dirs| dirs.contains(&real)) {
        return true;
    }
    app_state_dir().is_some_and(|app| app == real)
}

/// Makes the directory that actually holds the store trustworthy before anything in it is opened.
/// `dir` is resolved (no symlink) and is never the directory a symlink to the store lives in.
///
/// - Another user's directory is refused.
/// - The app's own state directory ([`is_app_dir`]: by exact path, never by name), writable by
///   group or others, is tightened to `0700`
///   (a umask of 002 leaves it `0775` with the user's private group, and refusing would break
///   upgrades).
/// - Any other directory is the user's layout and is never changed: it is refused when others may
///   write to it, or a group other than the user's own primary group. Group-writable by the
///   user's own group (the umask-002 default) is accepted as it is.
///
/// The checks and the `chmod` go through one open descriptor (`fstat`/`fchmod`), whose identity is
/// compared with the path's before and after. agent-kit has no `openat`/`O_NOFOLLOW` binding
/// (std only, no libc dependency), hence the identity re-checks rather than descriptor-relative
/// operations.
///
/// Threat model: a same-UID desktop app. Another local user racing the user's own state directory
/// through a writable ancestor is out of scope; these checks keep what the app itself does correct
/// and stop it from opening, or changing, anything it did not mean to.
pub(crate) fn trust_dir(dir: &Path) -> Result<()> {
    let named = id_of_path(dir, "the store directory")?;
    let handle = fs::File::open(dir)
        .map_err(|e| StoreError::Upgrade(format!("open the store directory: {e}")))?;
    let meta = handle
        .metadata()
        .map_err(|e| StoreError::Upgrade(format!("stat the store directory: {e}")))?;
    if FileId::of(&meta) != named {
        return Err(StoreError::Upgrade(format!(
            "the store directory {} was replaced while it was opened",
            dir.display()
        )));
    }
    if !meta.is_dir() {
        return Err(StoreError::Upgrade(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    if effective_uid().is_some_and(|uid| uid != meta.uid()) {
        return Err(StoreError::Upgrade(format!(
            "the store directory {} belongs to another user; move the store into a directory of \
             your own",
            dir.display()
        )));
    }
    // Group-writable by the user's own (primary, usually private) group is the umask-002 default
    // and, in a same-UID threat model, not a foreign writer.
    let own_group = effective_gid().is_some_and(|gid| gid == meta.gid());
    let foreign_writers = meta.mode() & 0o002 != 0 || (meta.mode() & 0o020 != 0 && !own_group);
    if foreign_writers && !is_app_dir(dir) {
        return Err(StoreError::Upgrade(format!(
            "the store directory {} is writable by other users; make it private (chmod go-w) or \
             move the store into the app's own state directory",
            dir.display()
        )));
    }
    if meta.mode() & 0o022 != 0 && is_app_dir(dir) {
        handle
            .set_permissions(fs::Permissions::from_mode(0o700))
            .map_err(|e| StoreError::Upgrade(format!("make the store directory private: {e}")))?;
    }
    expect_same(dir, named, "the store directory")
}

/// Resolves an existing database path once, to an absolute path free of symlinks, and makes the
/// directory that really holds it trustworthy ([`trust_dir`]); the directory a symlink to it lives
/// in is never touched. Everything after uses only this path.
pub(crate) fn resolve(db: &Path) -> Result<PathBuf> {
    let real = fs::canonicalize(db)
        .map_err(|e| StoreError::Upgrade(format!("resolve {}: {e}", db.display())))?;
    let meta = fs::symlink_metadata(&real)
        .map_err(|e| StoreError::Upgrade(format!("stat {}: {e}", real.display())))?;
    if !meta.is_file() {
        return Err(StoreError::Upgrade(format!(
            "{} is not a regular file",
            real.display()
        )));
    }
    let parent = real
        .parent()
        .ok_or_else(|| StoreError::Upgrade("the store has no directory".into()))?;
    trust_dir(parent)?;
    Ok(real)
}

/// `fail_at` outside tests: nothing.
#[cfg(not(test))]
macro_rules! fail_at {
    ($hooks:expr, $at:ident) => {
        let _ = &$hooks;
    };
}
#[cfg(test)]
macro_rules! fail_at {
    ($hooks:expr, $at:ident) => {
        $hooks.fail_at(Fault::$at)?
    };
}

fn upgrade_err(what: &str) -> impl FnOnce(std::io::Error) -> StoreError + '_ {
    move |e| StoreError::Upgrade(format!("{what}: {e}"))
}

/// The lock file serialising upgrade openers: `<db>.upgrade-lock`, `0600`, never removed (removing
/// a lock file lets two openers lock two different files).
fn lock_path(db: &Path) -> PathBuf {
    let mut name = db.as_os_str().to_owned();
    name.push(".upgrade-lock");
    PathBuf::from(name)
}

/// The schema version, read without writing: a missing `schema_version` table is version 0.
pub(crate) fn read_version(conn: &Connection) -> Result<i64> {
    let has: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_version')",
        [],
        |r| r.get(0),
    )?;
    if !has {
        return Ok(0);
    }
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |r| r.get(0),
    )?)
}

fn recorded_backup(conn: &Connection) -> Result<Option<String>> {
    let has_meta: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta')",
        [],
        |r| r.get(0),
    )?;
    if !has_meta {
        return Ok(None);
    }
    Ok(conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            [PRE_V4_BACKUP_KEY],
            |r| r.get(0),
        )
        .optional()?)
}

/// Rows the backup must hold: the tables every schema since v1 has.
fn counts(conn: &Connection) -> Result<(i64, i64, i64)> {
    Ok(conn.query_row(
        "SELECT (SELECT COUNT(*) FROM threads), (SELECT COUNT(*) FROM provider_threads),
                (SELECT COUNT(*) FROM events)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?)
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// fsyncs a directory, so the entries created in it survive a power loss.
fn sync_dir(dir: &Path, hooks: &Hooks, step: &str) -> Result<()> {
    hooks.note(step);
    fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| StoreError::Upgrade(format!("sync {}: {e}", dir.display())))
}

/// The backup directory beside the (resolved) database: created `0700`, or an existing one of ours
/// made `0700`. A symlink, a non-directory or another owner's directory is refused, never followed.
/// The parent is fsynced whether or not the directory was just created, so its entry is durable
/// before any migration relies on a backup inside it. Returns the directory and its identity.
fn private_backup_dir(db: &Path, hooks: &Hooks) -> Result<(PathBuf, FileId)> {
    let parent = db
        .parent()
        .ok_or_else(|| StoreError::Upgrade("the store has no directory".into()))?;
    let dir = parent.join(BACKUP_DIR);
    match fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(upgrade_err("create the backup directory")(e)),
    }
    let meta = fs::symlink_metadata(&dir).map_err(upgrade_err("stat the backup directory"))?;
    let owner = effective_uid().unwrap_or_else(|| meta.uid());
    if meta.file_type().is_symlink() || !meta.is_dir() || meta.uid() != owner {
        return Err(StoreError::Upgrade(format!(
            "{} is not a private directory of yours (a symlink, a file or another user's); move \
             it aside and start the app again",
            dir.display()
        )));
    }
    if meta.mode() & 0o777 != 0o700 {
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .map_err(upgrade_err("make the backup directory private"))?;
    }
    sync_dir(parent, hooks, "sync parent")?;
    Ok((dir, FileId::of(&meta)))
}

/// Backs the reserved, unmodified database up from a SEPARATE read-only connection into a private
/// temporary file, and verifies it. The source connection must open the very file the reservation
/// holds (`db_id`). Returns the verified temporary path; on any failure the temporary file is
/// removed.
fn backup_and_verify(
    db: &Path,
    db_id: FileId,
    dir: &Path,
    version: i64,
    hooks: &Hooks,
) -> Result<PathBuf> {
    let tmp = dir.join(format!(".tmp-{}-{}.db", std::process::id(), nanos()));
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(upgrade_err("create the backup file"))?;
    let result = (|| -> Result<()> {
        let src = Connection::open_with_flags(
            db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        src.busy_timeout(BUSY_TIMEOUT)?;
        // The source is the reserved file, not whatever the name points at now.
        expect_same(db, db_id, "the database")?;
        let mut dst = Connection::open(&tmp)?;
        #[cfg(test)]
        if let Some(pages) = hooks.backup_max_pages {
            let _: i64 = dst.query_row(&format!("PRAGMA max_page_count = {pages}"), [], |r| {
                r.get(0)
            })?;
        }
        // One read transaction pins the snapshot both the counts and the copy come from.
        src.execute_batch("BEGIN")?;
        let expected = counts(&src)?;
        {
            let backup = Backup::new(&src, &mut dst)?;
            match backup.step(-1)? {
                StepResult::Done => {}
                other => {
                    return Err(StoreError::Upgrade(format!(
                        "the backup did not finish in one step ({other:?})"
                    )))
                }
            }
        }
        src.execute_batch("COMMIT")?;
        drop(src);
        // A WAL source copies its WAL header bytes: make the copy a self-contained file.
        let mode: String = dst.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("delete") {
            return Err(StoreError::Upgrade(format!(
                "the backup stayed in journal mode {mode}"
            )));
        }
        dst.close().map_err(|(_, e)| StoreError::Sqlite(e))?;
        fs::File::open(&tmp)
            .and_then(|f| f.sync_all())
            .map_err(upgrade_err("sync the backup file"))?;
        fail_at!(hooks, AfterBackup);
        #[cfg(test)]
        if hooks.fault == Some(Fault::CorruptBackup) {
            let len = fs::metadata(&tmp)?.len();
            fs::OpenOptions::new()
                .write(true)
                .open(&tmp)?
                .set_len(len / 2)?;
        }
        verify(&tmp, version, expected)
    })();
    match result {
        Ok(()) => Ok(tmp),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Verifies a backup on a fresh read-only connection: intact, at `version`, holding `expected`
/// rows, and `0600`.
fn verify(path: &Path, version: i64, expected: (i64, i64, i64)) -> Result<()> {
    let check = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let integrity: Vec<String> = check
        .prepare("PRAGMA integrity_check")?
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    if integrity != ["ok"] {
        return Err(StoreError::Upgrade(format!(
            "the backup failed its integrity check: {}",
            integrity.join("; ")
        )));
    }
    let found = read_version(&check)?;
    if found != version {
        return Err(StoreError::Upgrade(format!(
            "the backup is at schema v{found}, not v{version}"
        )));
    }
    let got = counts(&check)?;
    if got != expected {
        return Err(StoreError::Upgrade(format!(
            "the backup holds {got:?} rows, the store {expected:?}"
        )));
    }
    let mode = fs::metadata(path)?.mode() & 0o777;
    if mode != 0o600 {
        return Err(StoreError::Upgrade(format!("the backup is mode {mode:o}")));
    }
    Ok(())
}

/// Publishes the verified temporary backup under `published` without ever replacing an existing
/// file (`link(2)` fails with `EEXIST`), then drops the temporary name and fsyncs the directory.
fn publish(tmp: &Path, published: &Path, hooks: &Hooks) -> Result<()> {
    fs::hard_link(tmp, published).map_err(upgrade_err("publish the backup"))?;
    fs::remove_file(tmp).map_err(upgrade_err("remove the temporary backup name"))?;
    if let Some(dir) = published.parent() {
        sync_dir(dir, hooks, "sync backups")?;
    }
    Ok(())
}

/// Whether a database without a schema version holds nothing at all: no table, index, view or
/// trigger but an empty `schema_version` (which a fresh open creates before its migration).
fn holds_nothing(conn: &Connection) -> Result<bool> {
    let others: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE name <> 'schema_version' AND substr(name, 1, 7) <> 'sqlite_'",
        [],
        |r| r.get(0),
    )?;
    if others > 0 {
        return Ok(false);
    }
    let has_version_table: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_version')",
        [],
        |r| r.get(0),
    )?;
    if !has_version_table {
        return Ok(true);
    }
    let rows: i64 = conn.query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))?;
    Ok(rows == 0)
}

/// Runs the preflight on an existing database file, `db` already [`resolve`]d (absolute, no
/// symlink, in a trusted directory). See the module docs for the sequence.
pub(crate) fn run(db: &Path, hooks: &Hooks) -> Result<Outcome> {
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(lock_path(db))
        .map_err(upgrade_err("open the upgrade lock"))?;
    lock.lock().map_err(upgrade_err("take the upgrade lock"))?;
    // The lock is released when `lock` is dropped (closing the descriptor releases the flock).
    // The database's identity, bound by a held descriptor: every later open must reach this file.
    let db_id = id_of_path(db, "the database")?;
    let held = fs::File::open(db).map_err(upgrade_err("open the database"))?;
    if FileId::of(&held.metadata()?) != db_id {
        return Err(StoreError::Upgrade(
            "the database was replaced while it was opened; refusing to upgrade".into(),
        ));
    }
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    expect_same(db, db_id, "the database")?;
    let version = read_version(&conn)?;
    if version == 0 {
        if holds_nothing(&conn)? {
            return Ok(Outcome::Fresh);
        }
        return Err(StoreError::Upgrade(format!(
            "{} holds data but no schema version this app recognises; it was left untouched. \
             Move it aside to start a new store, or restore a backup",
            db.display()
        )));
    }
    if version > SCHEMA_VERSION {
        return Err(StoreError::SchemaTooNew {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    if version == SCHEMA_VERSION {
        return Ok(Outcome::Current {
            backup: recorded_backup(&conn)?,
        });
    }
    // A table rebuild (v3) must not fire `ON DELETE` actions, and the pragma is a no-op inside a
    // transaction: off before the reservation. This connection is closed afterwards.
    conn.execute_batch("PRAGMA foreign_keys = OFF")?;
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = reserved(&conn, db, db_id, hooks);
    let out = match result {
        Ok(outcome) => match conn.execute_batch("COMMIT") {
            Ok(()) => Ok(outcome),
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e.into())
            }
        },
        Err(e) => {
            if !conn.is_autocommit() {
                let _ = conn.execute_batch("ROLLBACK");
            }
            Err(e)
        }
    };
    drop(conn);
    drop(held);
    drop(lock);
    out
}

/// The body of [`run`] under the reservation: recheck, back up, publish, migrate.
fn reserved(conn: &Connection, db: &Path, db_id: FileId, hooks: &Hooks) -> Result<Outcome> {
    // Another opener may have upgraded while this one waited for the lock.
    let version = read_version(conn)?;
    if version == SCHEMA_VERSION {
        return Ok(Outcome::Current {
            backup: recorded_backup(conn)?,
        });
    }
    if !(1..SCHEMA_VERSION).contains(&version) {
        return Err(StoreError::Upgrade(format!(
            "unexpected schema v{version} under the reservation"
        )));
    }
    fail_at!(hooks, AfterReserve);
    #[cfg(test)]
    std::thread::sleep(hooks.hold);
    #[cfg(test)]
    if let Some(swap) = hooks.before_backup {
        swap(db);
    }
    let (dir, dir_id) = private_backup_dir(db, hooks)?;
    let tmp = backup_and_verify(db, db_id, &dir, version, hooks)?;
    #[cfg(test)]
    if hooks.fault == Some(Fault::AfterVerify) {
        let _ = fs::remove_file(&tmp);
        return Err(StoreError::Upgrade("injected failure: AfterVerify".into()));
    }
    let stem = db
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "store".into());
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let published = dir.join(format!(
        "{stem}-v{version}-{secs}-{}-{}.db",
        std::process::id(),
        nanos()
    ));
    #[cfg(test)]
    if let Some(swap) = hooks.before_publish {
        swap(&dir);
    }
    // Publish only into the directory that was checked, never through a swapped-in one.
    let published_ok = expect_same(&dir, dir_id, "the backup directory")
        .and_then(|()| publish(&tmp, &published, hooks));
    if let Err(e) = published_ok {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    // From here a failure leaves a valid, published orphan backup: retained, never deleted.
    // Never migrate unless the backup's directory is still the checked one and the database is
    // still the reserved file.
    expect_same(&dir, dir_id, "the backup directory")?;
    expect_same(db, db_id, "the database")?;
    hooks.note("migrate");
    migrate_locked(conn, true)?;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![PRE_V4_BACKUP_KEY, published.display().to_string()],
    )?;
    fail_at!(hooks, BeforeCommit);
    tracing::info!(
        from = version,
        to = SCHEMA_VERSION,
        "store upgraded after a verified backup"
    );
    Ok(Outcome::Migrated {
        from: version,
        backup: published,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{migrate_locked_to, migrate_with, Store};
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    /// Events every fixture holds.
    const EVENTS: i64 = 60;

    /// A real store at schema `version` (built by this build's own migration steps, stopped
    /// early), in WAL mode like the app's, with threads, provider threads, events, an active
    /// provider thread and (from v2) a meta row. Everything checkpointed.
    fn fixture(path: &Path, version: i64) {
        let dir = path.parent().expect("parent");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .expect("dir");
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .expect("db file");
        let conn = Connection::open(path).expect("open");
        let _: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .expect("wal");
        conn.execute_batch("CREATE TABLE schema_version (version INTEGER NOT NULL)")
            .expect("version table");
        conn.execute_batch("BEGIN IMMEDIATE").expect("begin");
        migrate_locked_to(&conn, 0, version).expect("old schema");
        conn.execute_batch(
            "INSERT INTO threads (id, title, cwd, created_at, updated_at, active_provider_thread)
                 VALUES ('t1', 'first', '/w', 1, 2, 'p1'), ('t2', 'second', '/w', 3, 4, NULL);
             INSERT INTO provider_threads (id, thread_id, driver, model, native_id, created_at, updated_at)
                 VALUES ('p1', 't1', 'claude', 'opus', 'native-1', 1, 2),
                        ('p2', 't2', 'agy', 'gemini', NULL, 3, 4);",
        )
        .expect("rows");
        for i in 0..EVENTS {
            let pt = if i % 3 == 0 { None } else { Some("p1") };
            conn.execute(
                "INSERT INTO events (thread_id, provider_thread_id, at, envelope_json)
                 VALUES (?1, ?2, ?3, '{}')",
                rusqlite::params![if i % 2 == 0 { "t1" } else { "t2" }, pt, i],
            )
            .expect("event");
        }
        if version >= 2 {
            conn.execute_batch("INSERT INTO meta (key, value) VALUES ('open_threads', '[\"t1\"]')")
                .expect("meta");
        }
        conn.execute_batch("COMMIT").expect("commit");
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .expect("checkpoint");
    }

    fn db_in(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("agent-terminal").join("threads.db")
    }

    fn read_only(path: &Path) -> Connection {
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).expect("read-only")
    }

    fn has_table(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [name],
            |r| r.get(0),
        )
        .expect("sqlite_master")
    }

    /// (published backups, temporary files) in the backup directory.
    fn backups(db: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let dir = db.parent().expect("parent").join(BACKUP_DIR);
        let (mut done, mut tmp) = (Vec::new(), Vec::new());
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(".tmp-"))
            {
                tmp.push(p);
            } else {
                done.push(p);
            }
        }
        done.sort();
        (done, tmp)
    }

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).expect("stat").mode() & 0o777
    }

    /// Everything the fixture held is still there and linked, and the foreign keys hold.
    fn assert_preserved(path: &Path) {
        let s = Store::open(path).expect("open");
        let titles: Vec<String> = s
            .list_threads(true)
            .expect("list")
            .into_iter()
            .map(|t| t.title)
            .collect();
        assert_eq!(titles, vec!["second", "first"]);
        let pts = s.provider_threads("t1").expect("pts");
        assert_eq!(pts.len(), 1);
        assert_eq!(pts[0].native_id.as_deref(), Some("native-1"));
        assert_eq!(
            s.active_provider_thread("t1").expect("active").as_deref(),
            Some("p1")
        );
        assert_eq!(
            s.thread_with_native_id("claude", "native-1").expect("q"),
            Some("t1".to_owned())
        );
        let (events, linked): (i64, i64) = s
            .conn
            .query_row(
                "SELECT COUNT(*), COUNT(provider_thread_id) FROM events",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("events");
        assert_eq!((events, linked), (EVENTS, EVENTS - EVENTS / 3));
        let broken: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .expect("fk check");
        assert_eq!(broken, 0, "foreign keys hold");
        let ok: String = s
            .conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .expect("integrity");
        assert_eq!(ok, "ok");
        assert_eq!(read_version(&s.conn).expect("version"), SCHEMA_VERSION);
        for table in crate::store::delegation::V4_TABLES {
            assert!(has_table(&s.conn, table), "{table} exists");
        }
    }

    #[test]
    fn a_fresh_install_is_created_at_v4_without_a_backup() {
        let dir = tempfile::tempdir().expect("tmp");
        let db = db_in(&dir);
        let s = Store::open(&db).expect("open");
        assert_eq!(read_version(&s.conn).expect("version"), SCHEMA_VERSION);
        assert_eq!(s.meta(PRE_V4_BACKUP_KEY).expect("meta"), None);
        assert!(!db.parent().expect("p").join(BACKUP_DIR).exists());
        drop(s);
        // Reopening an existing v4 store takes the preflight's no-op path.
        assert_eq!(
            run(&db, &Hooks::default()).expect("preflight"),
            Outcome::Current { backup: None }
        );
    }

    #[test]
    fn v1_v2_and_v3_stores_migrate_to_v4_keeping_threads_events_and_refs() {
        for version in 1..SCHEMA_VERSION {
            let dir = tempfile::tempdir().expect("tmp");
            let db = db_in(&dir);
            fixture(&db, version);
            // A loose backup directory left by someone else's umask is made private.
            let bdir = db.parent().expect("p").join(BACKUP_DIR);
            fs::create_dir(&bdir).expect("backups");
            fs::set_permissions(&bdir, fs::Permissions::from_mode(0o755)).expect("chmod");

            assert_preserved(&db);
            let s = Store::open(&db).expect("open");
            if version >= 2 {
                assert_eq!(
                    s.meta("open_threads").expect("meta").as_deref(),
                    Some("[\"t1\"]")
                );
            }
            let recorded = s
                .meta(PRE_V4_BACKUP_KEY)
                .expect("meta")
                .expect("the backup is recorded");
            let (done, tmp) = backups(&db);
            assert_eq!(done, vec![PathBuf::from(&recorded)], "v{version}");
            assert!(tmp.is_empty());
            let backup = PathBuf::from(recorded);
            assert_eq!(mode(&backup), 0o600);
            assert_eq!(mode(&bdir), 0o700);
            let b = read_only(&backup);
            assert_eq!(read_version(&b).expect("v"), version, "the preimage");
            assert_eq!(counts(&b).expect("counts"), (2, 2, EVENTS));
            let journal: String = b
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .expect("journal");
            assert_eq!(journal, "delete", "a self-contained file");
            assert!(!has_table(&b, "delegation_roots"));
        }
    }

    #[test]
    fn v4_is_strictly_additive_over_v3() {
        let dir = tempfile::tempdir().expect("tmp");
        let db = db_in(&dir);
        fixture(&db, 3);
        let objects = |c: &Connection| -> Vec<(String, String, Option<String>)> {
            c.prepare("SELECT type, name, sql FROM sqlite_master ORDER BY type, name")
                .expect("prepare")
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .expect("query")
                .collect::<rusqlite::Result<_>>()
                .expect("rows")
        };
        let before = objects(&read_only(&db));
        drop(Store::open(&db).expect("open"));
        let after = objects(&read_only(&db));
        let added: Vec<_> = after.iter().filter(|o| !before.contains(o)).collect();
        assert!(
            before.iter().all(|o| after.contains(o)),
            "no v3 object changed or went"
        );
        assert!(
            added
                .iter()
                .all(|(_, name, _)| name.starts_with("delegation_")
                    || name.starts_with("sqlite_autoindex_delegation_")),
            "only delegation objects are added: {added:?}"
        );
        assert!(added
            .iter()
            .filter(|(kind, ..)| kind == "trigger")
            .all(|(_, _, sql)| sql.as_deref().is_some_and(|s| s.contains("ON delegation_"))));
    }

    #[test]
    fn each_injected_failure_leaves_the_schema_untouched_and_a_retry_migrates() {
        for fault in [
            Fault::AfterReserve,
            Fault::AfterBackup,
            Fault::CorruptBackup,
            Fault::AfterVerify,
            Fault::BeforeCommit,
        ] {
            let dir = tempfile::tempdir().expect("tmp");
            let db = db_in(&dir);
            fixture(&db, 3);
            let hooks = Hooks {
                fault: Some(fault),
                ..Hooks::default()
            };
            let failed = Store::open_with(&db, &hooks);
            assert!(failed.is_err(), "{fault:?}");
            {
                let c = read_only(&db);
                assert_eq!(read_version(&c).expect("v"), 3, "{fault:?}");
                assert!(!has_table(&c, "delegation_roots"), "{fault:?}");
                assert_eq!(counts(&c).expect("counts"), (2, 2, EVENTS), "{fault:?}");
                let ok: String = c
                    .query_row("PRAGMA integrity_check", [], |r| r.get(0))
                    .expect("ic");
                assert_eq!(ok, "ok");
            }
            let (done, tmp) = backups(&db);
            assert!(tmp.is_empty(), "{fault:?}: no temporary file is left");
            let orphan = if fault == Fault::BeforeCommit {
                assert_eq!(done.len(), 1, "a published backup is retained");
                Some(done[0].clone())
            } else {
                assert!(done.is_empty(), "{fault:?}: nothing published");
                None
            };
            // A retry through the real open path migrates.
            assert_preserved(&db);
            let s = Store::open(&db).expect("open");
            let recorded = PathBuf::from(s.meta(PRE_V4_BACKUP_KEY).expect("m").expect("set"));
            if let Some(orphan) = orphan {
                assert_ne!(recorded, orphan, "the orphan is not reused");
                assert!(orphan.exists(), "the orphan is retained");
                assert_eq!(read_version(&read_only(&orphan)).expect("v"), 3);
                assert_eq!(backups(&db).0.len(), 2);
            }
        }
    }

    #[test]
    fn an_uncheckpointed_wal_writer_ends_up_in_the_backup() {
        let dir = tempfile::tempdir().expect("tmp");
        let db = db_in(&dir);
        fixture(&db, 3);
        // Another connection commits rows that stay in the WAL, and stays open.
        let writer = Connection::open(&db).expect("writer");
        writer
            .pragma_update(None, "wal_autocheckpoint", 0)
            .expect("no autocheckpoint");
        writer.execute_batch("BEGIN").expect("begin");
        for i in 0..200 {
            writer
                .execute(
                    "INSERT INTO events (thread_id, at, envelope_json) VALUES ('t1', ?1, '{}')",
                    [i],
                )
                .expect("insert");
        }
        writer.execute_batch("COMMIT").expect("commit");
        let wal = PathBuf::from(format!("{}-wal", db.display()));
        assert!(
            fs::metadata(&wal).expect("wal").len() > 0,
            "frames in the WAL"
        );

        let s = Store::open(&db).expect("open");
        let backup = PathBuf::from(s.meta(PRE_V4_BACKUP_KEY).expect("m").expect("set"));
        let (_, _, events) = counts(&read_only(&backup)).expect("counts");
        assert_eq!(events, EVENTS + 200, "the WAL-only rows are in the backup");
        drop(writer);
    }

    #[test]
    fn publication_never_overwrites_an_existing_file() {
        let dir = tempfile::tempdir().expect("tmp");
        let tmp = dir.path().join(".tmp-1.db");
        let target = dir.path().join("threads-v3-1.db");
        fs::write(&tmp, b"new").expect("tmp");
        fs::write(&target, b"kept").expect("target");
        let hooks = Hooks::default();
        assert!(matches!(
            publish(&tmp, &target, &hooks),
            Err(StoreError::Upgrade(_))
        ));
        assert_eq!(fs::read(&target).expect("read"), b"kept");
        assert!(
            tmp.exists(),
            "the source is left for the caller to clean up"
        );
        let fresh = dir.path().join("threads-v3-2.db");
        publish(&tmp, &fresh, &hooks).expect("publish");
        assert_eq!(fs::read(&fresh).expect("read"), b"new");
        assert!(!tmp.exists());
    }

    #[test]
    fn a_restart_reuses_the_recorded_backup_and_makes_no_new_one() {
        let dir = tempfile::tempdir().expect("tmp");
        let db = db_in(&dir);
        fixture(&db, 3);
        drop(Store::open(&db).expect("upgrade"));
        let (first, _) = backups(&db);
        assert_eq!(first.len(), 1);
        let again = run(&db, &Hooks::default()).expect("preflight");
        assert_eq!(
            again,
            Outcome::Current {
                backup: Some(first[0].display().to_string())
            }
        );
        drop(Store::open(&db).expect("reopen"));
        assert_eq!(backups(&db).0, first);
    }

    #[test]
    fn a_newer_schema_is_refused_and_an_older_one_is_never_upgraded_unbacked() {
        let dir = tempfile::tempdir().expect("tmp");
        let db = db_in(&dir);
        fixture(&db, 3);
        drop(Store::open(&db).expect("upgrade"));
        Connection::open(&db)
            .expect("open")
            .execute("INSERT INTO schema_version (version) VALUES (99)", [])
            .expect("bump");
        assert!(matches!(
            Store::open(&db),
            Err(StoreError::SchemaTooNew { found: 99, .. })
        ));

        // The ordinary migration path refuses an older schema on its own.
        let conn = Connection::open_in_memory().expect("mem");
        conn.execute_batch("CREATE TABLE schema_version (version INTEGER NOT NULL)")
            .expect("t");
        conn.execute_batch("BEGIN").expect("b");
        migrate_locked_to(&conn, 0, 3).expect("v3");
        conn.execute_batch("COMMIT").expect("c");
        assert!(matches!(
            migrate_with(&conn, false),
            Err(StoreError::Upgrade(_))
        ));
        assert_eq!(read_version(&conn).expect("v"), 3);
    }

    #[test]
    fn two_concurrent_openers_migrate_once_and_share_the_backup() {
        let dir = tempfile::tempdir().expect("tmp");
        let db = db_in(&dir);
        fixture(&db, 3);
        let gate = Arc::new(Barrier::new(2));
        let openers: Vec<_> = (0..2)
            .map(|_| {
                let (db, gate) = (db.clone(), gate.clone());
                std::thread::spawn(move || {
                    gate.wait();
                    let hooks = Hooks {
                        hold: Duration::from_millis(300),
                        ..Hooks::default()
                    };
                    let s = Store::open_with(&db, &hooks).map_err(|e| e.to_string())?;
                    s.meta(PRE_V4_BACKUP_KEY).map_err(|e| e.to_string())
                })
            })
            .collect();
        let recorded: Vec<Option<String>> = openers
            .into_iter()
            .map(|h| h.join().expect("thread").expect("open"))
            .collect();
        assert!(recorded[0].is_some());
        assert_eq!(recorded[0], recorded[1], "both see the one backup");
        let (done, tmp) = backups(&db);
        assert_eq!((done.len(), tmp.len()), (1, 0));
        assert_preserved(&db);
    }

    const CHILD_DB: &str = "AGENT_KIT_PREFLIGHT_CHILD_DB";

    /// The child half of the cross-process test: does nothing unless started by it.
    #[test]
    fn preflight_child_opener() {
        let Some(db) = std::env::var_os(CHILD_DB) else {
            return;
        };
        let hooks = Hooks {
            hold: Duration::from_millis(400),
            ..Hooks::default()
        };
        let s = Store::open_with(Path::new(&db), &hooks).expect("child open");
        let recorded = s.meta(PRE_V4_BACKUP_KEY).expect("meta").unwrap_or_default();
        println!("PREFLIGHT-BACKUP={recorded}");
    }

    #[test]
    fn two_opener_processes_migrate_once_and_share_the_backup() {
        let dir = tempfile::tempdir().expect("tmp");
        let db = db_in(&dir);
        fixture(&db, 3);
        let exe = std::env::current_exe().expect("test binary");
        let children: Vec<_> = (0..2)
            .map(|_| {
                Command::new(&exe)
                    .args([
                        "--exact",
                        "store::preflight::tests::preflight_child_opener",
                        "--nocapture",
                        "--test-threads=1",
                    ])
                    .env(CHILD_DB, &db)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawn")
            })
            .collect();
        let recorded: Vec<String> = children
            .into_iter()
            .map(|c| {
                let out = c.wait_with_output().expect("wait");
                assert!(out.status.success(), "child failed");
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    // libtest prints the test's name on the same line first.
                    .find_map(|l| {
                        l.split_once("PREFLIGHT-BACKUP=")
                            .map(|(_, path)| path.to_owned())
                    })
                    .expect("the child reports its backup")
            })
            .collect();
        assert!(!recorded[0].is_empty());
        assert_eq!(recorded[0], recorded[1]);
        let (done, tmp) = backups(&db);
        assert_eq!(done, vec![PathBuf::from(&recorded[0])]);
        assert!(tmp.is_empty());
        assert_preserved(&db);
    }

    fn traced() -> (Hooks, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let trace = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let hooks = Hooks {
            trace: Some(trace.clone()),
            ..Hooks::default()
        };
        (hooks, trace)
    }

    /// The schema is still the fixture's v3 and nothing was published.
    fn assert_untouched(db: &Path) {
        let c = read_only(db);
        assert_eq!(read_version(&c).expect("v"), 3);
        assert!(!has_table(&c, "delegation_roots"));
        assert_eq!(counts(&c).expect("counts"), (2, 2, EVENTS));
        let (done, tmp) = backups(db);
        assert!(done.is_empty() && tmp.is_empty(), "{done:?} {tmp:?}");
    }

    #[test]
    fn the_backup_directory_and_its_parent_are_synced_before_the_migration() {
        for precreated in [false, true] {
            let dir = tempfile::tempdir().expect("tmp");
            let db = db_in(&dir);
            fixture(&db, 3);
            if precreated {
                fs::create_dir(db.parent().expect("p").join(BACKUP_DIR)).expect("backups");
            }
            let (hooks, trace) = traced();
            Store::open_with(&db, &hooks).expect("open");
            let steps = trace.lock().expect("trace").clone();
            assert_eq!(
                steps,
                vec!["sync parent", "sync backups", "migrate"],
                "precreated={precreated}"
            );
        }
    }

    #[test]
    fn a_symlinked_database_is_resolved_once_and_its_real_directory_holds_the_backup() {
        let dir = tempfile::tempdir().expect("tmp");
        let real = db_in(&dir);
        fixture(&real, 3);
        let link_dir = dir.path().join("elsewhere");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&link_dir)
            .expect("dir");
        let link = link_dir.join("threads.db");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let s = Store::open(&link).expect("open through the link");
        let recorded = PathBuf::from(s.meta(PRE_V4_BACKUP_KEY).expect("m").expect("set"));
        assert!(recorded.is_absolute());
        assert_eq!(
            recorded.parent(),
            Some(
                fs::canonicalize(real.parent().expect("p"))
                    .expect("c")
                    .join(BACKUP_DIR)
                    .as_path()
            )
        );
        assert!(!link_dir.join(BACKUP_DIR).exists());
        assert!(fs::symlink_metadata(&link)
            .expect("l")
            .file_type()
            .is_symlink());
        assert_preserved(&real);
    }

    #[test]
    fn a_group_writable_store_directory_is_made_private_before_the_preflight() {
        let dir = tempfile::tempdir().expect("tmp");
        let db = db_in(&dir);
        fixture(&db, 3);
        let parent = db.parent().expect("p");
        treat_as_app_dir(parent);
        fs::set_permissions(parent, fs::Permissions::from_mode(0o770)).expect("chmod");
        let (hooks, trace) = traced();
        Store::open_with(&db, &hooks).expect("open");
        assert_eq!(mode(parent), 0o700);
        assert!(trace.lock().expect("t").contains(&"migrate".to_owned()));
    }

    #[test]
    fn an_unrelated_directory_named_agent_terminal_keeps_its_mode() {
        let dir = tempfile::tempdir().expect("tmp");
        let db = dir
            .path()
            .join("projects")
            .join("agent-terminal")
            .join("threads.db");
        fixture(&db, 3);
        let parent = db.parent().expect("p");
        assert!(
            !is_app_dir(parent),
            "a name is not the app's state directory"
        );
        fs::set_permissions(parent, fs::Permissions::from_mode(0o775)).expect("chmod");
        drop(Store::open(&db).expect("open"));
        assert_eq!(mode(parent), 0o775, "never changed");
        fs::set_permissions(parent, fs::Permissions::from_mode(0o755)).expect("chmod");
        drop(Store::open(&db).expect("reopen"));
        assert_eq!(mode(parent), 0o755, "not tightened after the open either");
        assert_preserved(&db);
    }

    #[test]
    fn a_group_writable_directory_the_user_chose_is_refused_and_left_as_it_was() {
        let dir = tempfile::tempdir().expect("tmp");
        let db = dir.path().join("shared").join("threads.db");
        fixture(&db, 3);
        let parent = db.parent().expect("p");
        fs::set_permissions(parent, fs::Permissions::from_mode(0o777)).expect("chmod");
        let refused = Store::open(&db);
        assert!(
            matches!(&refused, Err(StoreError::Upgrade(why)) if why.contains("writable by other")),
            "{:?}",
            refused.err()
        );
        assert_eq!(mode(parent), 0o777, "never changed");
        assert_untouched(&db);
        // Writable only by the user's own group (umask 002): accepted, and still never changed.
        fs::set_permissions(parent, fs::Permissions::from_mode(0o775)).expect("chmod");
        drop(Store::open(&db).expect("open"));
        assert_eq!(mode(parent), 0o775);
        assert_preserved(&db);
    }

    #[test]
    fn a_symlink_in_another_directory_of_the_users_leaves_that_directory_alone() {
        let dir = tempfile::tempdir().expect("tmp");
        let real = db_in(&dir);
        fixture(&real, 3);
        let link_dir = dir.path().join("projects");
        fs::create_dir(&link_dir).expect("dir");
        fs::set_permissions(&link_dir, fs::Permissions::from_mode(0o775)).expect("chmod");
        let link = link_dir.join("threads.db");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        drop(Store::open(&link).expect("open through the link"));
        assert_eq!(
            mode(&link_dir),
            0o775,
            "the symlink's directory is untouched"
        );
        assert!(!link_dir.join(BACKUP_DIR).exists());
        assert!(!link_dir.join("threads.db.upgrade-lock").exists());
        assert_preserved(&real);
    }

    #[test]
    fn a_table_named_like_sqlite_but_not_reserved_is_data_not_an_empty_store() {
        let dir = tempfile::tempdir().expect("tmp");
        let db = db_in(&dir);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(db.parent().expect("p"))
            .expect("dir");
        {
            let c = Connection::open(&db).expect("open");
            c.execute_batch(
                "CREATE TABLE sqliteArchive (name TEXT); INSERT INTO sqliteArchive VALUES ('x');",
            )
            .expect("data");
        }
        let before = fs::read(&db).expect("bytes");
        let refused = Store::open(&db);
        assert!(
            matches!(&refused, Err(StoreError::Upgrade(why)) if why.contains("Move it aside")),
            "{:?}",
            refused.err()
        );
        assert_eq!(fs::read(&db).expect("bytes"), before);
    }

    #[test]
    fn a_database_replaced_during_the_reservation_is_never_backed_up_or_migrated() {
        fn swap(db: &Path) {
            let aside = db.with_extension("reserved");
            fs::rename(db, &aside).expect("move the reserved file aside");
            fs::copy(&aside, db).expect("another file under the name");
        }
        let dir = tempfile::tempdir().expect("tmp");
        let db = db_in(&dir);
        fixture(&db, 3);
        let hooks = Hooks {
            before_backup: Some(swap),
            ..Hooks::default()
        };
        let refused = Store::open_with(&db, &hooks);
        assert!(
            matches!(&refused, Err(StoreError::Upgrade(why)) if why.contains("replaced")),
            "{:?}",
            refused.err()
        );
        assert_untouched(&db.with_extension("reserved"));
        assert_untouched(&db);
    }

    #[test]
    fn a_swapped_backup_directory_is_refused_before_publication() {
        fn swap(dir: &Path) {
            let aside = dir.with_extension("checked");
            fs::rename(dir, &aside).expect("move backups aside");
            std::os::unix::fs::symlink(&aside, dir).expect("symlink in its place");
        }
        let dir = tempfile::tempdir().expect("tmp");
        let db = db_in(&dir);
        fixture(&db, 3);
        let hooks = Hooks {
            before_publish: Some(swap),
            ..Hooks::default()
        };
        let refused = Store::open_with(&db, &hooks);
        assert!(
            matches!(&refused, Err(StoreError::Upgrade(why)) if why.contains("symlink")),
            "{:?}",
            refused.err()
        );
        let c = read_only(&db);
        assert_eq!(read_version(&c).expect("v"), 3);
        assert!(!has_table(&c, "delegation_roots"));
        let aside = db.parent().expect("p").join("backups.checked");
        let left: Vec<_> = fs::read_dir(aside).expect("dir").flatten().collect();
        assert!(left.is_empty(), "nothing published: {left:?}");
        // A symlinked backups/ stays refused on the next open, never followed.
        assert!(matches!(Store::open(&db), Err(StoreError::Upgrade(_))));
    }

    #[test]
    fn a_database_with_data_but_no_schema_version_is_refused_without_a_write() {
        for with_empty_version_table in [false, true] {
            let dir = tempfile::tempdir().expect("tmp");
            let db = db_in(&dir);
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(db.parent().expect("p"))
                .expect("dir");
            {
                let c = Connection::open(&db).expect("open");
                c.execute_batch(
                    "CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT);
                     INSERT INTO threads VALUES ('t1', 'someone else''s data');",
                )
                .expect("data");
                if with_empty_version_table {
                    c.execute_batch("CREATE TABLE schema_version (version INTEGER NOT NULL)")
                        .expect("empty version table");
                }
            }
            let before = fs::read(&db).expect("bytes");
            let refused = Store::open(&db);
            assert!(
                matches!(&refused, Err(StoreError::Upgrade(why)) if why.contains("Move it aside")),
                "{:?}",
                refused.err()
            );
            assert_eq!(fs::read(&db).expect("bytes"), before, "not a byte written");
            let c = read_only(&db);
            let journal: String = c
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .expect("journal");
            assert_eq!(journal, "delete", "no WAL switch");
            assert!(!has_table(&c, "provider_threads"));
        }
    }

    #[test]
    fn a_read_only_store_directory_refuses_the_upgrade_untouched() {
        if effective_uid() == Some(0) {
            return; // root writes through any mode
        }
        let dir = tempfile::tempdir().expect("tmp");
        let db = db_in(&dir);
        fixture(&db, 3);
        let parent = db.parent().expect("p").to_owned();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).expect("chmod");
        let refused = Store::open(&db);
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).expect("chmod back");
        assert!(refused.is_err());
        assert_untouched(&db);
        Store::open(&db).expect("a writable directory upgrades");
    }

    #[test]
    fn a_full_disk_during_the_backup_refuses_the_upgrade_untouched() {
        let dir = tempfile::tempdir().expect("tmp");
        let db = db_in(&dir);
        fixture(&db, 3);
        let hooks = Hooks {
            backup_max_pages: Some(1),
            ..Hooks::default()
        };
        let refused = Store::open_with(&db, &hooks);
        assert!(
            matches!(&refused, Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(f, _)))
                if f.code == rusqlite::ErrorCode::DiskFull),
            "{:?}",
            refused.err()
        );
        assert_untouched(&db);
        assert_preserved(&db);
    }

    #[test]
    fn a_relative_path_records_an_absolute_backup_path() {
        let dir = tempfile::tempdir().expect("tmp");
        let db = db_in(&dir);
        fixture(&db, 3);
        let cwd = std::env::current_dir().expect("cwd");
        let mut relative = PathBuf::new();
        for _ in cwd.components().skip(1) {
            relative.push("..");
        }
        relative.push(db.strip_prefix("/").expect("absolute"));
        assert!(relative.is_relative());
        let s = Store::open(&relative).expect("open");
        let recorded = PathBuf::from(s.meta(PRE_V4_BACKUP_KEY).expect("m").expect("set"));
        assert!(recorded.is_absolute(), "{}", recorded.display());
        assert!(recorded.exists());
    }
}
