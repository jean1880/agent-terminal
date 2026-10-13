//! SQLite store for the app-owned transcript.
//!
//! The app owns the thread: native agent sessions are only references
//! (`provider_threads.native_id`). One app thread may span several provider
//! threads after a model switch or a hand-off. Events are an append-only log of
//! [`agent_core::event::Envelope`]s, scrubbed with [`agent_core::redact::redact`]
//! before they touch disk.
//!
//! `rusqlite` is built with `bundled`, so the single shipped binary carries its
//! own SQLite and needs no runtime library.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agent_core::event::{Envelope, Event, ItemKind, ItemStatus, StreamKind};
use agent_core::redact::{redact, redact_keyed};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::heal_tail::HealTails;

/// A store write (an append, a batch, a checkpoint) slower than this is logged as a warning with
/// its kind and duration: one frame at 60 Hz.
pub const SLOW_WRITE: Duration = Duration::from_millis(16);

/// Identifier of an app thread (a UUID string).
pub type ThreadId = String;
/// Identifier of a provider thread (a UUID string).
pub type ProviderThreadId = String;

/// How long a writer waits on another connection's lock before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Schema version this build writes. Bump it and add a step to [`migrate`].
const SCHEMA_VERSION: i64 = 3;
/// [`Store::meta`] key of the native sessions whose threads the user deleted (JSON list of
/// `driver:native_id`).
const DISMISSED_KEY: &str = "dismissed_natives";
const MAX_DISMISSED_NATIVES: usize = 500;

#[derive(Debug)]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    Json(serde_json::Error),
    /// The thread or provider thread does not exist.
    NotFound(String),
    /// The database was written by a newer build.
    SchemaTooNew {
        found: i64,
        supported: i64,
    },
    /// The store writer thread has stopped (or never started): the write was not queued.
    WriterGone,
    /// The store writer's queue is full: the write was refused (counted and shown).
    Overloaded,
    /// The store writer did not catch up in time: a read that needs it was not made.
    Behind,
    /// SQLite rolled the transaction back on its own (disk full, I/O error, …).
    TransactionLost,
    /// A store job failed (its own error, as text): what the writer's batch saw of it.
    Failed(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "database error: {e}"),
            Self::Io(e) => write!(f, "store i/o error: {e}"),
            Self::Json(e) => write!(f, "envelope encoding error: {e}"),
            Self::NotFound(what) => write!(f, "not found: {what}"),
            Self::SchemaTooNew { found, supported } => write!(
                f,
                "database schema v{found} is newer than this build supports (v{supported})"
            ),
            Self::WriterGone => write!(f, "the store writer has stopped"),
            Self::Overloaded => write!(f, "the store writer is too far behind to take more"),
            Self::Behind => write!(f, "the store writer has not caught up yet"),
            Self::Failed(why) => write!(f, "{why}"),
            Self::TransactionLost => {
                write!(
                    f,
                    "the database rolled the write back (disk full or I/O error?)"
                )
            }
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(e) => Some(e),
            Self::Io(e) => Some(e),
            Self::Json(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}
impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// One row of the thread list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummary {
    pub id: ThreadId,
    pub title: String,
    pub cwd: String,
    pub updated_at: i64,
    /// Driver of the active (else most recent) provider thread.
    pub driver: Option<String>,
    pub model: Option<String>,
    /// True when events arrived after the last [`Store::mark_read`].
    pub unread: bool,
    /// Hidden from the default list ([`Store::list_threads`] with `include_archived` false).
    pub archived: bool,
}

/// A provider thread: one native agent session under an app thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderThread {
    pub id: ProviderThreadId,
    pub thread_id: ThreadId,
    pub driver: String,
    pub model: String,
    pub native_id: Option<String>,
}

/// One stored event and the agent it came from ([`Store::events_by_agent`]).
#[derive(Debug, Clone, PartialEq)]
pub struct StoredEvent {
    pub seq: i64,
    /// The driver key (`claude`, `agy`, …) of the provider thread it was stored under, when known.
    pub driver: Option<String>,
    pub envelope: Envelope,
}

/// A transcript item with its final text, free of `agent_core` types so the
/// hand-off budget can consume it directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptMessage {
    /// `user`, `assistant` or `tool`.
    pub role: String,
    /// The `ItemKind` in snake case (`user_message`, `command`, …).
    pub kind: String,
    pub text: String,
    pub item_id: String,
    /// `open` until the item completes, then `completed`, `failed`,
    /// `declined` or `interrupted`.
    pub status: String,
}

/// `$XDG_STATE_HOME/agent-terminal/threads.db`, falling back to
/// `~/.local/state/agent-terminal/threads.db`. Takes the environment values so
/// tests never read the real home. Empty and relative `xdg_state_home` values
/// are ignored, as the XDG spec requires.
pub fn default_path(xdg_state_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let base = match xdg_state_home.filter(|v| Path::new(v).is_absolute()) {
        Some(xdg) => PathBuf::from(xdg),
        None => PathBuf::from(home.filter(|h| !h.is_empty())?)
            .join(".local")
            .join("state"),
    };
    Some(base.join("agent-terminal").join("threads.db"))
}

/// Masks secrets in every string of an envelope (`raw` included) before it is
/// persisted.
///
/// A string under a sensitive key (`api_key`, `password`, …) is masked whole,
/// as its key alone marks it secret; every other string goes through the
/// pattern redaction.
///
/// Ceiling: redaction is per envelope, so a token split across two streaming
/// deltas is not recognized here. [`Store::append_event`] closes that gap when
/// the item completes or snapshots (see `heal_split_deltas`); until then the
/// earlier delta rows hold the fragments.
pub fn scrub_envelope(env: Envelope) -> Result<Envelope> {
    let value = serde_json::to_value(&env)?;
    Ok(serde_json::from_value(scrub_value(None, value))?)
}

/// Tests inject panics on purpose; their messages would bury real failures. Silences only those
/// (by message), leaving every other panic report as it was.
#[cfg(test)]
pub(crate) fn quiet_injected_panics() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let text = info
                .payload()
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
                .unwrap_or("");
            if !(text.starts_with("injected") || text == "a job that panics") {
                default(info);
            }
        }));
    });
}

/// The event's wire name (`content_delta`, `item_completed`, …), for logs. Encodes the event, so
/// only call it off the hot path (a slow write already being reported).
pub fn event_kind(event: &Event) -> String {
    serde_json::to_value(event)
        .ok()
        .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

/// `key` is the name of the nearest enclosing object field; array elements
/// inherit it, so `{"tokens": ["..."]}` is judged by "tokens".
fn scrub_value(key: Option<&str>, v: Value) -> Value {
    match v {
        Value::String(s) => Value::String(match key {
            Some(k) => redact_keyed(k, &s),
            None => redact(&s),
        }),
        Value::Array(a) => Value::Array(a.into_iter().map(|v| scrub_value(key, v)).collect()),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .map(|(k, v)| {
                    let v = scrub_value(Some(&k), v);
                    (k, v)
                })
                .collect(),
        ),
        other => other,
    }
}

/// How far back, in global `seq` numbers from a thread's newest row, [`Store::heal_split_deltas`]
/// searches (rows of other threads in that range count against it).
const HEAL_WINDOW: i64 = 4000;

pub struct Store {
    conn: Connection,
    /// Last timestamp handed out, so `updated_at` strictly increases even
    /// within one millisecond (list ordering stays deterministic).
    last_ms: Cell<i64>,
    /// Streamed text of open items, so a heal need not scan the table (see `heal_tail`).
    tails: RefCell<HealTails>,
    /// Inside [`Store::batch`]: methods that open a transaction take a savepoint instead.
    in_batch: Cell<bool>,
    /// Threads appended to in the current batch: their tails are distrusted if it fails.
    batch_threads: RefCell<HashSet<String>>,
    /// Test hook: the next append fails after its insert and tail update (an injected error).
    #[cfg(test)]
    fail_next_append: Cell<bool>,
    /// Test hook: the next append panics after its insert and tail update.
    #[cfg(test)]
    panic_next_append: Cell<bool>,
    /// Test hook: the next batch COMMIT fails.
    #[cfg(test)]
    fail_next_commit: Cell<bool>,
    /// Test record of which heal ran: `[tail, sql]`.
    #[cfg(test)]
    heal_paths: Cell<[u32; 2]>,
}

/// A unit of work that commits or rolls back as one: a transaction, or inside a
/// [`Store::batch`] a savepoint of the batch's transaction. Rolled back when dropped uncommitted.
/// The heal tails follow it: a level opened with it, kept or taken back with it.
struct Scope<'a> {
    store: &'a Store,
    savepoint: bool,
    done: bool,
}

impl Scope<'_> {
    fn commit(mut self) -> Result<()> {
        let sql = if self.savepoint {
            "RELEASE op"
        } else {
            "COMMIT"
        };
        self.store.conn.execute_batch(sql)?;
        self.done = true;
        self.store.tails.borrow_mut().release();
        Ok(())
    }
}

impl Drop for Scope<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let sql = if self.savepoint {
            "ROLLBACK TO op; RELEASE op"
        } else {
            "ROLLBACK"
        };
        if !self.store.conn.is_autocommit() {
            if let Err(e) = self.store.conn.execute_batch(sql) {
                tracing::warn!(error = %e, "could not roll a store write back");
            }
        }
        self.store.tails.borrow_mut().rollback();
    }
}

/// An open batch transaction ([`Store::begin_batch`]).
pub struct Batch<'a> {
    store: &'a Store,
    committed: bool,
}

/// One job's savepoint inside a batch, with its level of the heal tails: kept only if the job
/// succeeded and its savepoint was released; else (an error, an `Err` result, a panic, a lost
/// transaction, a failed RELEASE) both are taken back.
struct JobLevel<'a> {
    store: &'a Store,
    done: bool,
}

impl Drop for JobLevel<'_> {
    fn drop(&mut self) {
        if self.done {
            self.store.tails.borrow_mut().release();
            return;
        }
        if !self.store.conn.is_autocommit() {
            let _ = self
                .store
                .conn
                .execute_batch("ROLLBACK TO job; RELEASE job");
        }
        self.store.tails.borrow_mut().rollback();
    }
}

impl Batch<'_> {
    /// Whether the transaction is still open. SQLite rolls a whole transaction back on its own
    /// after some errors (disk full, I/O, out of memory); what ran in it is then gone, and what
    /// runs next would autocommit. The writer checks this after every job.
    pub fn alive(&self) -> bool {
        !self.store.conn.is_autocommit()
    }

    /// Runs one job under a savepoint of its own: rolled back, leaving no partial rows and no
    /// tail change, when it fails, returns `Err` or panics (the panic then carries on).
    pub fn job<T>(&self, f: impl FnOnce(&Store) -> Result<T>) -> Result<T> {
        let conn = &self.store.conn;
        conn.execute_batch("SAVEPOINT job")?;
        self.store.tails.borrow_mut().begin();
        let mut level = JobLevel {
            store: self.store,
            done: false,
        };
        let result = f(self.store);
        if result.is_ok() && self.alive() {
            conn.execute_batch("RELEASE job")?;
            level.done = true;
        }
        drop(level);
        result
    }

    /// Commits the batch. On failure nothing of it is stored, the tails take back everything
    /// the batch changed, and its threads' tails are distrusted.
    pub fn commit(mut self) -> Result<()> {
        #[cfg(test)]
        if self.store.fail_next_commit.replace(false) {
            return Err(StoreError::Io(std::io::Error::other(
                "injected commit failure",
            )));
        }
        if !self.alive() {
            return Err(StoreError::TransactionLost);
        }
        self.store.conn.execute_batch("COMMIT")?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for Batch<'_> {
    fn drop(&mut self) {
        let store = self.store;
        store.in_batch.set(false);
        let threads: Vec<String> = store.batch_threads.borrow_mut().drain().collect();
        if self.committed {
            // Committed: so are the tail changes.
            store.tails.borrow_mut().release();
            return;
        }
        if !store.conn.is_autocommit() {
            let _ = store.conn.execute_batch("ROLLBACK");
        }
        let mut tails = store.tails.borrow_mut();
        tails.rollback();
        // Belt and braces: the batch's threads are not vouched for until they start afresh.
        for t in threads {
            tails.distrust_thread(&t);
        }
    }
}

/// Distrusts a thread's heal tails unless disarmed: held across an append, so an error or a
/// panic (unwinding past the append) never leaves a tail vouching for a row not stored.
struct DistrustUnlessDone<'a> {
    store: &'a Store,
    thread: &'a str,
    done: bool,
}

impl Drop for DistrustUnlessDone<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.store.tails.borrow_mut().distrust_thread(self.thread);
        }
    }
}

/// What a `PRAGMA wal_checkpoint` reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointStats {
    /// The checkpoint could not finish (a reader or writer was in the way).
    pub busy: bool,
    /// Frames in the WAL.
    pub wal_frames: i64,
    /// Of those, frames copied into the database.
    pub checkpointed: i64,
    /// Bytes the WAL's frames hold (`wal_frames` × page size).
    pub wal_bytes: i64,
}

impl Store {
    /// Opens (creating if needed) the database at `path`: parent directory
    /// `0700`, file `0600`, WAL mode, schema migrated.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            create_private_dir(parent)?;
        }
        create_private_file(path)?;
        tighten_permissions(path)?;
        let conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        enable_wal(&conn)?;
        // NORMAL is durable enough under WAL (a power cut can lose the last commits, never
        // corrupt) and avoids an fsync per event. The WAL is synced at checkpoints, so the
        // commits at risk are those since the last one: on the store writer's connection that
        // is its checkpoint interval (`store_writer::Timing`).
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(conn)
    }

    /// An in-memory store for tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        migrate(&conn)?;
        Ok(Self {
            conn,
            last_ms: Cell::new(0),
            tails: RefCell::new(HealTails::default()),
            in_batch: Cell::new(false),
            batch_threads: RefCell::new(HashSet::new()),
            #[cfg(test)]
            fail_next_append: Cell::new(false),
            #[cfg(test)]
            panic_next_append: Cell::new(false),
            #[cfg(test)]
            fail_next_commit: Cell::new(false),
            #[cfg(test)]
            heal_paths: Cell::new([0, 0]),
        })
    }

    /// Whether heals may use the in-memory tails (on by default). Off, every heal scans the
    /// table, as before the tails existed: the reference the differential test compares with.
    pub fn set_heal_tails(&self, enabled: bool) {
        self.tails.borrow_mut().set_enabled(enabled);
    }

    /// The item's tail may no longer vouch for it (one of its deltas was refused): it heals by
    /// the scan.
    pub fn distrust_tail(&self, thread: &str, item: &str) {
        self.tails.borrow_mut().refuse(thread, item);
    }

    /// Forgets the thread's heal tails (its page closed). Its next heal scans the table.
    pub fn forget_tails(&self, thread: &str) {
        self.tails.borrow_mut().clear_thread(thread);
    }

    /// Sets this connection's automatic checkpoint threshold in pages (0: off; SQLite's default
    /// is 1000). The store writer turns it off and checkpoints on its own thread instead.
    pub fn set_autocheckpoint(&self, pages: i64) -> Result<()> {
        self.conn
            .pragma_update(None, "wal_autocheckpoint", pages)
            .map_err(Into::into)
    }

    /// This connection's automatic checkpoint threshold, in pages.
    pub fn autocheckpoint(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA wal_autocheckpoint", [], |r| r.get(0))?)
    }

    /// A PASSIVE checkpoint: copies what it can from the WAL into the database without waiting
    /// on anyone. Never TRUNCATE: that waits for readers and could stall its caller.
    pub fn checkpoint(&self) -> Result<CheckpointStats> {
        let (busy, frames, done): (i64, i64, i64) =
            self.conn
                .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?;
        let page: i64 = self.conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        Ok(CheckpointStats {
            busy: busy != 0,
            wal_frames: frames.max(0),
            checkpointed: done.max(0),
            wal_bytes: frames.max(0).saturating_mul(page),
        })
    }

    /// Runs `f` inside one batch ([`Store::begin_batch`]), committed at the end. `Err` (and
    /// nothing of `f` stored) when the batch could not begin, SQLite rolled it back on its own
    /// meanwhile, or the COMMIT failed. A batch inside a batch just runs `f`.
    pub fn batch<R>(&self, f: impl FnOnce(&Store) -> R) -> Result<R> {
        // (Tests and the writer use it; the tails follow the batch: see `Batch`.)
        if self.in_batch.get() {
            return Ok(f(self));
        }
        let batch = self.begin_batch()?;
        let out = f(self);
        if !batch.alive() {
            return Err(StoreError::TransactionLost);
        }
        batch.commit()?;
        Ok(out)
    }

    /// Begins one `BEGIN IMMEDIATE` transaction for many writes: the store writer's batches.
    /// Every method called meanwhile that opens a transaction of its own takes a savepoint
    /// instead, so one failing write rolls back alone. The batch is committed only by
    /// [`Batch::commit`]; any other end (an error, a panic, a lost transaction) rolls it back
    /// and distrusts the heal tails of every thread appended to in it.
    pub fn begin_batch(&self) -> Result<Batch<'_>> {
        if self.in_batch.get() {
            return Err(StoreError::Sqlite(rusqlite::Error::InvalidQuery));
        }
        // Another process holding the write lock past the busy timeout: try again before the
        // batch is given up (and reported).
        let mut attempt = 0;
        loop {
            match self.conn.execute_batch("BEGIN IMMEDIATE") {
                Ok(()) => break,
                Err(rusqlite::Error::SqliteFailure(e, _))
                    if e.code == rusqlite::ErrorCode::DatabaseBusy && attempt < 2 =>
                {
                    attempt += 1;
                    tracing::warn!(
                        attempt,
                        "the store is locked by another connection; retrying"
                    );
                }
                Err(e) => return Err(e.into()),
            }
        }
        self.in_batch.set(true);
        self.batch_threads.borrow_mut().clear();
        self.tails.borrow_mut().begin();
        Ok(Batch {
            store: self,
            committed: false,
        })
    }

    /// Test hook: SQLite rolling the open transaction back on its own (as after a disk-full or
    /// I/O error), behind the store's back.
    #[cfg(test)]
    pub(crate) fn lose_transaction(&self) {
        let _ = self.conn.execute_batch("ROLLBACK");
    }

    /// A transaction, or a savepoint of the open batch (see [`Store::batch`]).
    fn scope(&self, behavior: rusqlite::TransactionBehavior) -> Result<Scope<'_>> {
        // Inside any open transaction (a batch, `atomically`, another scope): a savepoint of it.
        let savepoint = self.in_batch.get() || !self.conn.is_autocommit();
        let sql = match (savepoint, behavior) {
            (true, _) => "SAVEPOINT op",
            (false, rusqlite::TransactionBehavior::Immediate) => "BEGIN IMMEDIATE",
            (false, _) => "BEGIN DEFERRED",
        };
        self.conn.execute_batch(sql)?;
        self.tails.borrow_mut().begin();
        Ok(Scope {
            store: self,
            savepoint,
            done: false,
        })
    }

    /// Runs `f` under the named savepoint, with a level of the heal tails: both kept if it
    /// succeeds, both taken back if it fails or panics.
    fn in_savepoint<T>(&self, name: &str, f: impl FnOnce(&Store) -> Result<T>) -> Result<T> {
        self.conn.execute_batch(&format!("SAVEPOINT {name}"))?;
        self.tails.borrow_mut().begin();
        struct Level<'a> {
            store: &'a Store,
            name: &'a str,
            done: bool,
        }
        impl Drop for Level<'_> {
            fn drop(&mut self) {
                if self.done {
                    return;
                }
                if !self.store.conn.is_autocommit() {
                    let _ = self
                        .store
                        .conn
                        .execute_batch(&format!("ROLLBACK TO {0}; RELEASE {0}", self.name));
                }
                self.store.tails.borrow_mut().rollback();
            }
        }
        let mut level = Level {
            store: self,
            name,
            done: false,
        };
        let out = f(self)?;
        self.conn.execute_batch(&format!("RELEASE {name}"))?;
        level.done = true;
        self.tails.borrow_mut().release();
        Ok(out)
    }

    /// Runs `f` as one unit: all of it stored, or (an `Err`, a panic) none of it, tails
    /// included. Inside a batch, a savepoint of it; else a transaction of its own.
    pub fn atomically<T>(&self, f: impl FnOnce(&Store) -> Result<T>) -> Result<T> {
        let scope = self.scope(rusqlite::TransactionBehavior::Immediate)?;
        let out = f(self)?;
        scope.commit()?;
        Ok(out)
    }

    fn now(&self) -> i64 {
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let next = wall.max(self.last_ms.get() + 1);
        self.last_ms.set(next);
        next
    }

    /// A fresh random v4 UUID string. Touches no table, so it takes no lock: callers that queue
    /// a write (the store writer) make the row's id here first.
    pub fn new_id(&self) -> Result<String> {
        let hex: String = self
            .conn
            .query_row("SELECT lower(hex(randomblob(16)))", [], |r| r.get(0))?;
        // Stamp the version (4) and variant (10xx) nibbles of a random UUID.
        let nibble = hex
            .chars()
            .nth(16)
            .and_then(|c| c.to_digit(16))
            .unwrap_or(0);
        let variant = (nibble & 0x3) | 0x8;
        Ok(format!(
            "{}-{}-4{}-{:x}{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[13..16],
            variant,
            &hex[17..20],
            &hex[20..32]
        ))
    }

    /// A write transaction that takes the write lock at BEGIN (`IMMEDIATE`). A deferred one that
    /// reads first fails at once with SQLITE_BUSY_SNAPSHOT when another connection commits in
    /// between (WAL), without waiting on the busy timeout; this one waits instead. Callers use
    /// plain statements only: SQLite has no nested transactions, so never call another
    /// transaction-opening method (`write_txn`, `append_event`, …) while one is open.
    fn write_txn(&self) -> Result<Scope<'_>> {
        self.scope(rusqlite::TransactionBehavior::Immediate)
    }

    fn touch(&self, thread: &str, at: i64) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE threads SET updated_at = ?2 WHERE id = ?1",
            params![thread, at],
        )?;
        require_row(n, thread)
    }

    // ---- threads ----

    pub fn create_thread(&self, cwd: &str, title: Option<&str>) -> Result<ThreadId> {
        let now = self.now();
        self.create_thread_at(cwd, title, now)
    }

    /// A thread for a session that already exists elsewhere (an agent's own history), dated
    /// `at_ms` (epoch milliseconds) so it sorts where that session does, not as brand new.
    pub fn create_thread_at(&self, cwd: &str, title: Option<&str>, at_ms: i64) -> Result<ThreadId> {
        let id = self.new_id()?;
        self.insert_thread(&id, cwd, title, at_ms)?;
        Ok(id)
    }

    /// [`Store::create_thread`] under an id the caller made ([`Store::new_id`]): a write queued
    /// to the store writer knows its thread's id before it runs.
    pub fn create_thread_with_id(&self, id: &str, cwd: &str, title: Option<&str>) -> Result<()> {
        let now = self.now();
        self.insert_thread(id, cwd, title, now)
    }

    fn insert_thread(&self, id: &str, cwd: &str, title: Option<&str>, at_ms: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO threads (id, title, cwd, created_at, updated_at, archived, read_seq)
             VALUES (?1, ?2, ?3, ?4, ?4, 0, 0)",
            params![id, redact(title.unwrap_or("")), cwd, at_ms],
        )?;
        Ok(())
    }

    /// A thread for an agent's existing session (`driver`, `native_id`), dated `at_ms` (epoch
    /// milliseconds) so it sorts where that session does, with its provider thread already
    /// active. All or nothing: a failure part-way leaves no orphan thread behind.
    pub fn link_native_thread(
        &self,
        cwd: &str,
        title: Option<&str>,
        at_ms: i64,
        driver: &str,
        native_id: &str,
    ) -> Result<ThreadId> {
        let tx = self.write_txn()?;
        let thread = self.create_thread_at(cwd, title, at_ms)?;
        let pt = self.add_provider_thread(&thread, driver, "default")?;
        self.set_native_id(&pt, native_id)?;
        self.set_active_provider_thread(&thread, &pt)?;
        // Linking bumped the time to now; the thread sorts by the session's own.
        self.touch(&thread, at_ms)?;
        tx.commit()?;
        Ok(thread)
    }

    /// The thread (archived or not) that already holds `driver`'s native session `native_id`,
    /// so a session is never listed twice.
    pub fn thread_with_native_id(&self, driver: &str, native_id: &str) -> Result<Option<ThreadId>> {
        Ok(self
            .conn
            .query_row(
                "SELECT thread_id FROM provider_threads WHERE driver = ?1 AND native_id = ?2
                 ORDER BY created_at LIMIT 1",
                params![driver, native_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Newest first (by `updated_at`).
    pub fn list_threads(&self, include_archived: bool) -> Result<Vec<ThreadSummary>> {
        let mut stmt = self.conn.prepare(&format!(
            "{SUMMARY_SQL} WHERE (?1 OR t.archived = 0) ORDER BY t.updated_at DESC, t.rowid DESC"
        ))?;
        let rows = stmt.query_map(params![include_archived], summary_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// One thread's summary, archived or not; `None` when it does not exist.
    pub fn thread_summary(&self, thread: &str) -> Result<Option<ThreadSummary>> {
        let mut stmt = self
            .conn
            .prepare(&format!("{SUMMARY_SQL} WHERE t.id = ?1"))?;
        Ok(stmt.query_row(params![thread], summary_row).optional()?)
    }

    /// Deletes a thread and, by foreign-key cascade, its events and provider threads. Only this
    /// store's rows go: the agents' own session files (`native_id` references) are never touched.
    ///
    /// The native sessions it held are remembered as dismissed ([`Store::native_dismissed`]), so
    /// listing the agents' recent sessions does not bring a deleted thread straight back.
    pub fn delete_thread(&self, thread: &str) -> Result<()> {
        let tx = self.write_txn()?;
        let natives: Vec<(String, String)> = self
            .provider_threads(thread)?
            .into_iter()
            .filter_map(|pt| pt.native_id.map(|n| (pt.driver, n)))
            .collect();
        self.add_dismissed(&natives)?;
        let n = self
            .conn
            .execute("DELETE FROM threads WHERE id = ?1", params![thread])?;
        require_row(n, thread)?;
        tx.commit()?;
        self.tails.borrow_mut().clear_thread(thread);
        Ok(())
    }

    /// Remembers a native session as no longer the thread's to list again (its thread moved on
    /// to another session, e.g. a resume that started a new one).
    pub fn dismiss_native(&self, driver: &str, native_id: &str) -> Result<()> {
        // A read-modify-write of one meta value: under the write lock, so two connections
        // cannot lose each other's entry.
        let tx = self.write_txn()?;
        self.add_dismissed(&[(driver.to_owned(), native_id.to_owned())])?;
        tx.commit()?;
        Ok(())
    }

    fn add_dismissed(&self, natives: &[(String, String)]) -> Result<()> {
        if natives.is_empty() {
            return Ok(());
        }
        let mut dismissed = self.dismissed_natives()?;
        for (driver, native) in natives {
            let key = format!("{driver}:{native}");
            if !dismissed.contains(&key) {
                dismissed.push(key);
            }
        }
        // Bounded: only the newest few hundred matter, the lister looks at the latest sessions.
        let excess = dismissed.len().saturating_sub(MAX_DISMISSED_NATIVES);
        dismissed.drain(..excess);
        self.set_meta(DISMISSED_KEY, &serde_json::to_string(&dismissed)?)
    }

    /// Whether the user deleted the thread that held `driver`'s native session `native_id`.
    pub fn native_dismissed(&self, driver: &str, native_id: &str) -> Result<bool> {
        Ok(self
            .dismissed_natives()?
            .contains(&format!("{driver}:{native_id}")))
    }

    fn dismissed_natives(&self) -> Result<Vec<String>> {
        let Some(value) = self.meta(DISMISSED_KEY)? else {
            return Ok(Vec::new());
        };
        match serde_json::from_str(&value) {
            Ok(list) => Ok(list),
            Err(e) => {
                // Starting over only means a deleted session may be listed once more.
                tracing::warn!(error = %e, "the dismissed-sessions list was unreadable; starting over");
                Ok(Vec::new())
            }
        }
    }

    // ---- app state ----

    /// A small value the app keeps beside its threads (e.g. which were open), or `None`.
    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// Sets (replacing) a [`Store::meta`] value. Not redacted: callers store ids, not text.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn rename_thread(&self, thread: &str, title: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE threads SET title = ?2, updated_at = ?3 WHERE id = ?1",
            params![thread, redact(title), self.now()],
        )?;
        require_row(n, thread)
    }

    pub fn set_archived(&self, thread: &str, archived: bool) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE threads SET archived = ?2 WHERE id = ?1",
            params![thread, archived],
        )?;
        require_row(n, thread)
    }

    /// Marks every event so far as seen.
    pub fn mark_read(&self, thread: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE threads SET read_seq =
                 COALESCE((SELECT MAX(seq) FROM events WHERE thread_id = ?1), 0)
             WHERE id = ?1",
            params![thread],
        )?;
        require_row(n, thread)
    }

    // ---- provider threads ----

    pub fn add_provider_thread(
        &self,
        thread: &str,
        driver: &str,
        model: &str,
    ) -> Result<ProviderThreadId> {
        let id = self.new_id()?;
        self.add_provider_thread_with_id(&id, thread, driver, model)?;
        Ok(id)
    }

    /// [`Store::add_provider_thread`] under an id the caller made ([`Store::new_id`]).
    pub fn add_provider_thread_with_id(
        &self,
        id: &str,
        thread: &str,
        driver: &str,
        model: &str,
    ) -> Result<()> {
        let now = self.now();
        self.conn.execute(
            "INSERT INTO provider_threads (id, thread_id, driver, model, native_id, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?5)",
            params![id, thread, driver, model, now],
        )
        .map_err(|e| fk_or(e, thread))?;
        Ok(())
    }

    /// Records the model a provider thread was last set to: what a reopened thread resumes on.
    /// Store an alias when the agent offers one (it follows the agent's newest model); the
    /// resolved id an event reports is for display only.
    pub fn set_provider_model(&self, provider_thread: &str, model: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE provider_threads SET model = ?2, updated_at = ?3 WHERE id = ?1",
            params![provider_thread, model, self.now()],
        )?;
        require_row(n, provider_thread)
    }

    pub fn provider_threads(&self, thread: &str) -> Result<Vec<ProviderThread>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, thread_id, driver, model, native_id FROM provider_threads
             WHERE thread_id = ?1 ORDER BY created_at, rowid",
        )?;
        let rows = stmt.query_map(params![thread], |r| {
            Ok(ProviderThread {
                id: r.get(0)?,
                thread_id: r.get(1)?,
                driver: r.get(2)?,
                model: r.get(3)?,
                native_id: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn set_native_id(&self, provider_thread: &str, native_id: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE provider_threads SET native_id = ?2, updated_at = ?3 WHERE id = ?1",
            params![provider_thread, native_id, self.now()],
        )?;
        require_row(n, provider_thread)
    }

    /// Points the thread at one of its own provider threads.
    pub fn set_active_provider_thread(&self, thread: &str, provider_thread: &str) -> Result<()> {
        let owned: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM provider_threads WHERE id = ?1 AND thread_id = ?2",
                params![provider_thread, thread],
                |r| r.get(0),
            )
            .optional()?;
        if owned.is_none() {
            return Err(StoreError::NotFound(format!(
                "provider thread {provider_thread} of {thread}"
            )));
        }
        let n = self.conn.execute(
            "UPDATE threads SET active_provider_thread = ?2, updated_at = ?3 WHERE id = ?1",
            params![thread, provider_thread, self.now()],
        )?;
        require_row(n, thread)
    }

    pub fn active_provider_thread(&self, thread: &str) -> Result<Option<ProviderThreadId>> {
        let row: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT active_provider_thread FROM threads WHERE id = ?1",
                params![thread],
                |r| r.get(0),
            )
            .optional()?;
        match row {
            Some(active) => Ok(active),
            None => Err(StoreError::NotFound(thread.to_string())),
        }
    }

    // ---- events ----

    /// Appends one scrubbed envelope; returns its sequence number.
    pub fn append_event(
        &self,
        thread: &str,
        provider_thread: Option<&str>,
        env: &Envelope,
    ) -> Result<i64> {
        let started = Instant::now();
        // Anything but success (an error, a panic unwinding through here): the tail may now
        // hold a row the database does not, or the reverse.
        let mut guard = DistrustUnlessDone {
            store: self,
            thread,
            done: false,
        };
        // The insert and the `updated_at` bump commit together or not at all.
        let result = self
            .scope(rusqlite::TransactionBehavior::Deferred)
            .and_then(|tx| {
                let seq = self.append_in_txn(thread, provider_thread, env)?;
                tx.commit()?;
                Ok(seq)
            });
        guard.done = result.is_ok();
        drop(guard);
        let took = started.elapsed();
        if took > SLOW_WRITE {
            tracing::warn!(
                kind = event_kind(&env.event),
                ms = took.as_secs_f64() * 1000.0,
                "slow store append"
            );
        }
        result
    }

    /// The body of [`Store::append_event`]; the caller owns the transaction.
    ///
    /// The heal tails are updated as each row is written, from the scrubbed envelope; a write
    /// that fails afterwards (this append's, or the batch's COMMIT) distrusts the thread's tails.
    fn append_in_txn(
        &self,
        thread: &str,
        provider_thread: Option<&str>,
        env: &Envelope,
    ) -> Result<i64> {
        let scrubbed = scrub_envelope(env.clone())?;
        let json = serde_json::to_string(&scrubbed)?;
        let now = self.now();
        if self.in_batch.get() {
            self.batch_threads.borrow_mut().insert(thread.to_owned());
        }
        self.conn
            .execute(
                "INSERT INTO events (thread_id, provider_thread_id, at, envelope_json)
                 VALUES (?1, ?2, ?3, ?4)",
                params![thread, provider_thread, now, json],
            )
            .map_err(|e| fk_or(e, thread))?;
        let seq = self.conn.last_insert_rowid();
        match (&scrubbed.event, &scrubbed.item) {
            (Event::ItemStarted { .. }, Some(item)) => {
                // A reused item id (a second start, a reopened thread, another process) already
                // has rows the scan would heal with; a tail started now would not hold them.
                if self.tails.borrow().would_arm(thread, item) {
                    if self.item_has_rows(thread, item, seq)? {
                        self.tails.borrow_mut().refuse(thread, item);
                    } else {
                        self.tails.borrow_mut().arm(thread, item);
                    }
                } else {
                    self.tails.borrow_mut().arm(thread, item);
                }
            }
            (Event::ContentDelta { stream, text }, Some(item)) => self
                .tails
                .borrow_mut()
                .record(thread, item, *stream, seq, text),
            (Event::ItemCompleted { .. } | Event::ContentSnapshot { .. }, Some(item)) => {
                // Under a savepoint: a heal that fails is undone on its own and the event is
                // still stored (each delta row is already scrubbed alone).
                if let Err(e) = self.in_savepoint("heal", |s| s.heal(thread, item)) {
                    tracing::warn!(error = %e, "could not re-scrub an item's streamed text");
                    self.tails.borrow_mut().distrust_thread(thread);
                }
                if matches!(scrubbed.event, Event::ItemCompleted { .. }) {
                    self.tails.borrow_mut().consume(thread, item);
                }
            }
            _ => {}
        }
        if matches!(
            scrubbed.event,
            Event::TurnCompleted { .. } | Event::SessionExited { .. }
        ) {
            self.tails.borrow_mut().clear_thread(thread);
        }
        // Injected after the tails moved, so a failing append really leaves them ahead of the
        // database: only the distrust on the way out keeps them honest.
        #[cfg(test)]
        if self.fail_next_append.replace(false) {
            return Err(StoreError::Io(std::io::Error::other(
                "injected append failure",
            )));
        }
        #[cfg(test)]
        if self.panic_next_append.replace(false) {
            panic!("injected append panic");
        }
        self.touch(thread, now)?;
        Ok(seq)
    }

    /// Whether `thread` already has a row naming `item`, other than row `seq`, within the rows
    /// [`Store::heal_split_deltas`] would search. A substring test on the stored JSON: a false
    /// "yes" (the id quoted in some text) only costs the SQL heal; a false "no" cannot happen,
    /// since every row of the item carries exactly this `"item":"…"` field.
    fn item_has_rows(&self, thread: &str, item: &str, seq: i64) -> Result<bool> {
        let needle = format!("\"item\":{}", serde_json::to_string(item)?);
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM events
                 WHERE thread_id = ?1
                   AND seq > (SELECT COALESCE(MAX(seq), 0) FROM events WHERE thread_id = ?1) - ?3
                   AND seq <> ?4
                   AND instr(envelope_json, ?2) > 0)",
            params![thread, needle, HEAL_WINDOW, seq],
            |r| r.get(0),
        )?)
    }

    /// Re-scrubs an item's streamed text as a whole: from its trusted in-memory tail when there
    /// is one, else by scanning the table ([`Store::heal_split_deltas`]). Both write the same
    /// rows; the tail only skips the scan (and, for an item longer than [`HEAL_WINDOW`], also
    /// reaches the rows the scan cannot).
    fn heal(&self, thread: &str, item: &str) -> Result<()> {
        let tail = self.tails.borrow().trusted(thread, item);
        #[cfg(test)]
        {
            let mut paths = self.heal_paths.get();
            paths[usize::from(tail.is_none())] += 1;
            self.heal_paths.set(paths);
        }
        if let Some(streams) = tail {
            tracing::debug!(path = "tail", "healing an item's streamed text");
            // Its own savepoint: a tail heal that fails is undone whole and the scan runs
            // instead, so a broken tail can never leave fragments the scan would have healed.
            match self.in_savepoint("tail_heal", |s| s.heal_from_tail(thread, item, streams)) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::warn!(error = %e, "a heal tail did not match the store; scanning");
                    self.tails.borrow_mut().distrust_thread(thread);
                    #[cfg(test)]
                    {
                        let mut paths = self.heal_paths.get();
                        paths[1] += 1;
                        self.heal_paths.set(paths);
                    }
                }
            }
        }
        tracing::debug!(path = "sql", "healing an item's streamed text");
        self.heal_split_deltas(thread, item)
    }

    /// [`Store::heal_split_deltas`] with the rows already known: only a stream whose joined text
    /// redaction changes is read back (by `seq`) and rewritten, exactly as the scan would.
    fn heal_from_tail(
        &self,
        thread: &str,
        item: &str,
        streams: Vec<(StreamKind, crate::heal_tail::StreamRows)>,
    ) -> Result<()> {
        for (stream, rows) in streams {
            if rows.len() < 2 {
                continue;
            }
            let joined: String = rows.iter().map(|(_, t)| t.as_str()).collect();
            let whole = redact(&joined);
            if whole == joined {
                continue;
            }
            for (i, (seq, _)) in rows.iter().enumerate() {
                let json: String = self.conn.query_row(
                    "SELECT envelope_json FROM events WHERE seq = ?1 AND thread_id = ?2",
                    params![seq, thread],
                    |r| r.get(0),
                )?;
                let mut env: Envelope = serde_json::from_str(&json)?;
                // The row must still be this item's delta on this stream: never rewrite a row
                // the tail only thinks it knows.
                let ours = env.item.as_deref() == Some(item)
                    && matches!(&env.event, Event::ContentDelta { stream: s, .. } if *s == stream);
                if !ours {
                    return Err(StoreError::NotFound(format!(
                        "delta row {seq} of a heal tail"
                    )));
                }
                if let Event::ContentDelta { text, .. } = &mut env.event {
                    *text = if i == 0 { whole.clone() } else { String::new() };
                }
                env.raw = None;
                self.conn.execute(
                    "UPDATE events SET envelope_json = ?1 WHERE seq = ?2",
                    params![serde_json::to_string(&env)?, seq],
                )?;
            }
            self.tails.borrow_mut().healed(thread, item, stream, &whole);
        }
        Ok(())
    }

    /// Re-scrubs an item's streamed text as a whole. Each delta is scrubbed
    /// alone, so a token split across two deltas survives in both rows. When
    /// the item completes (or snapshots), join each stream's delta text, redact
    /// it, and if anything changed put the redacted whole in the first row,
    /// empty the rest (their `seq` stays) and drop those rows' `raw` frames,
    /// which carry the fragments verbatim.
    ///
    /// Ceiling: only rows within [`HEAL_WINDOW`] sequence numbers of the
    /// thread's newest are searched (there is no item column to index;
    /// `seq` is global, so other threads' rows interleaved there shrink the
    /// window), and an item that never
    /// completes or snapshots keeps its fragments. This scan is the fallback: an item whose
    /// in-memory tail is trusted heals from it instead ([`Store::heal`], `heal_tail`), and
    /// writes the same rows.
    fn heal_split_deltas(&self, thread: &str, item: &str) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, envelope_json FROM events
             WHERE thread_id = ?1
               AND seq > (SELECT COALESCE(MAX(seq), 0) FROM events WHERE thread_id = ?1) - ?3
               AND json_extract(envelope_json, '$.item') = ?2
               AND json_extract(envelope_json, '$.event.type') = 'content_delta'
             ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![thread, item, HEAL_WINDOW], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut groups: Vec<(StreamKind, Vec<(i64, Envelope)>)> = Vec::new();
        for row in rows {
            let (seq, json) = row?;
            let env: Envelope = serde_json::from_str(&json)?;
            let Event::ContentDelta { stream, .. } = &env.event else {
                continue;
            };
            let stream = *stream;
            match groups.iter_mut().find(|(s, _)| *s == stream) {
                Some((_, v)) => v.push((seq, env)),
                None => groups.push((stream, vec![(seq, env)])),
            }
        }
        drop(stmt);
        for (_, rows) in groups {
            if rows.len() < 2 {
                continue;
            }
            let joined: String = rows
                .iter()
                .filter_map(|(_, e)| match &e.event {
                    Event::ContentDelta { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            let whole = redact(&joined);
            if whole == joined {
                continue;
            }
            for (i, (seq, mut env)) in rows.into_iter().enumerate() {
                if let Event::ContentDelta { text, .. } = &mut env.event {
                    *text = if i == 0 { whole.clone() } else { String::new() };
                }
                env.raw = None;
                self.conn.execute(
                    "UPDATE events SET envelope_json = ?1 WHERE seq = ?2",
                    params![serde_json::to_string(&env)?, seq],
                )?;
            }
        }
        Ok(())
    }

    /// Stores an imported conversation (an agent's own transcript) in one transaction: every
    /// envelope scrubbed like [`Store::append_event`]'s, but the thread's `updated_at` is left
    /// alone (opening an old session must not move it to the top of the list) and the imported
    /// events count as read. Only into a thread with no events yet (checked inside the
    /// transaction, so two openers cannot both import); returns how many were stored, 0 when the
    /// thread already had history. All or none.
    pub fn import_events(
        &self,
        thread: &str,
        provider_thread: Option<&str>,
        envs: &[Envelope],
    ) -> Result<usize> {
        // Scrubbed before the write lock is taken, so the main connection (live events) waits
        // only for the inserts.
        let rows: Vec<String> = envs
            .iter()
            .map(|env| Ok(serde_json::to_string(&scrub_envelope(env.clone())?)?))
            .collect::<Result<_>>()?;
        let tx = self.write_txn()?;
        let existing: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM events WHERE thread_id = ?1",
            params![thread],
            |r| r.get(0),
        )?;
        if existing > 0 {
            return Ok(0);
        }
        let at = self.now();
        for json in &rows {
            self.conn
                .execute(
                    "INSERT INTO events (thread_id, provider_thread_id, at, envelope_json)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![thread, provider_thread, at, json],
                )
                .map_err(|e| fk_or(e, thread))?;
        }
        self.conn.execute(
            "UPDATE threads SET read_seq =
               (SELECT COALESCE(MAX(seq), 0) FROM events WHERE thread_id = ?1) WHERE id = ?1",
            params![thread],
        )?;
        tx.commit()?;
        Ok(envs.len())
    }

    /// Stores a user prompt as `ItemStarted{UserMessage}` + `ContentSnapshot`;
    /// returns the new item id.
    pub fn append_user_message(
        &self,
        thread: &str,
        provider_thread: Option<&str>,
        text: &str,
    ) -> Result<String> {
        let item = self.new_id()?;
        self.append_user_message_with_id(thread, provider_thread, &item, text)?;
        Ok(item)
    }

    /// [`Store::append_user_message`] under an item id the caller made ([`Store::new_id`]).
    pub fn append_user_message_with_id(
        &self,
        thread: &str,
        provider_thread: Option<&str>,
        item: &str,
        text: &str,
    ) -> Result<()> {
        let started = Envelope::new(Event::ItemStarted {
            kind: ItemKind::UserMessage,
            title: String::new(),
            input: None,
            parent: None,
        })
        .item(item);
        let snapshot = Envelope::new(Event::ContentSnapshot {
            stream: StreamKind::Assistant,
            text: text.to_string(),
        })
        .item(item);
        let mut guard = DistrustUnlessDone {
            store: self,
            thread,
            done: false,
        };
        let result = self
            .scope(rusqlite::TransactionBehavior::Deferred)
            .and_then(|tx| {
                self.append_in_txn(thread, provider_thread, &started)?;
                self.append_in_txn(thread, provider_thread, &snapshot)?;
                tx.commit()
            });
        guard.done = result.is_ok();
        result
    }

    /// The `seq` just before the newest `n` events of `thread`, to pass as `after_seq` to
    /// [`Store::events`]; `None` when the thread has `n` or fewer events (so all of it fits).
    pub fn tail_after(&self, thread: &str, n: usize) -> Result<Option<i64>> {
        let offset = i64::try_from(n).unwrap_or(i64::MAX);
        Ok(self
            .conn
            .query_row(
                "SELECT seq FROM events WHERE thread_id = ?1 ORDER BY seq DESC LIMIT 1 OFFSET ?2",
                params![thread, offset],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Events after `after_seq` (exclusive), oldest first, at most `limit`.
    pub fn events(
        &self,
        thread: &str,
        after_seq: Option<i64>,
        limit: usize,
    ) -> Result<Vec<(i64, Envelope)>> {
        Ok(self
            .events_by_agent(thread, after_seq, limit)?
            .into_iter()
            .map(|e| (e.seq, e.envelope))
            .collect())
    }

    /// [`Store::events`], each with the agent that produced it: the driver key of the provider
    /// thread it was stored under. `None` for an event stored with no provider thread, or whose
    /// provider thread is gone (the column is `ON DELETE SET NULL`); the caller falls back to the
    /// thread's current agent. Reads only existing columns, so every database version has it.
    pub fn events_by_agent(
        &self,
        thread: &str,
        after_seq: Option<i64>,
        limit: usize,
    ) -> Result<Vec<StoredEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.seq, p.driver, e.envelope_json FROM events e
             LEFT JOIN provider_threads p ON p.id = e.provider_thread_id
             WHERE e.thread_id = ?1 AND e.seq > ?2 ORDER BY e.seq LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![
                thread,
                after_seq.unwrap_or(0),
                limit.min(i64::MAX as usize) as i64
            ],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, driver, json) = row?;
            out.push(StoredEvent {
                seq,
                driver,
                envelope: serde_json::from_str(&json)?,
            });
        }
        Ok(out)
    }

    /// Whether the user has sent this thread a message (so there is something to hand over).
    /// Does not decode the events: it walks this thread's rows through the `(thread_id, seq)`
    /// index and stops at the first one whose JSON mentions a user message. That is quick when
    /// the user has written (the first prompt is early); a thread with no user message at all is
    /// scanned in full (JSON text, no decoding). The kind is not a column, so this is a
    /// substring test; adding an indexed `kind` column would make it constant, and is the upgrade
    /// path if a thread's event count ever makes the empty case slow.
    pub fn has_messages(&self, thread: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM events
                            WHERE thread_id = ?1 AND envelope_json LIKE '%\"user_message\"%')",
            params![thread],
            |r| r.get(0),
        )?)
    }

    /// Rebuilds the thread's items with their final text: a snapshot replaces
    /// the deltas accumulated so far, later deltas append to it. Items appear
    /// in the order they first occur. `Reasoning` items are left out: a
    /// hand-off built from this must not replay the model's private thinking.
    pub fn transcript_messages(&self, thread: &str) -> Result<Vec<TranscriptMessage>> {
        let mut out: Vec<TranscriptMessage> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        let mut after = 0;
        loop {
            let batch = self.events(thread, Some(after), 1000)?;
            let Some((last, _)) = batch.last() else { break };
            after = *last;
            for (_, env) in batch {
                apply_to_transcript(&mut out, &mut index, env);
            }
        }
        // A hand-off replays this; the model's private thinking must not travel with it.
        out.retain(|m| m.kind != "reasoning");
        Ok(out)
    }
}

fn apply_to_transcript(
    out: &mut Vec<TranscriptMessage>,
    index: &mut HashMap<String, usize>,
    env: Envelope,
) {
    let Some(item) = env.item else { return };
    let mut slot = |out: &mut Vec<TranscriptMessage>, kind: &str| -> usize {
        *index.entry(item.clone()).or_insert_with(|| {
            out.push(TranscriptMessage {
                role: role_of(kind).to_string(),
                kind: kind.to_string(),
                text: String::new(),
                item_id: item.clone(),
                status: "open".to_string(),
            });
            out.len() - 1
        })
    };
    match env.event {
        Event::ItemStarted { kind, .. } => {
            slot(out, &kind_name(kind));
        }
        Event::ContentDelta { stream, text } if counts_as_text(stream) => {
            let i = slot(out, "unknown");
            out[i].text.push_str(&text);
        }
        Event::ContentSnapshot { stream, text } if counts_as_text(stream) => {
            let i = slot(out, "unknown");
            out[i].text = text;
        }
        Event::ItemCompleted { status, output, .. } => {
            let i = slot(out, "unknown");
            out[i].status = status_name(status).to_string();
            if out[i].text.is_empty() {
                if let Some(o) = output {
                    out[i].text = o;
                }
            }
        }
        _ => {}
    }
}

/// Tool input lives in `ItemStarted::input`, not in the transcript text, and
/// reasoning is never part of it.
fn counts_as_text(stream: StreamKind) -> bool {
    !matches!(stream, StreamKind::ToolInput | StreamKind::Reasoning)
}

fn role_of(kind: &str) -> &'static str {
    match kind {
        "user_message" => "user",
        "assistant_message" | "reasoning" => "assistant",
        _ => "tool",
    }
}

/// The serde (snake case) name of an item kind, so it never drifts from the
/// wire format.
fn kind_name(kind: ItemKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

fn status_name(status: ItemStatus) -> &'static str {
    match status {
        ItemStatus::Completed => "completed",
        ItemStatus::Failed => "failed",
        ItemStatus::Declined => "declined",
        ItemStatus::Interrupted => "interrupted",
    }
}

/// The columns of a [`ThreadSummary`], before its `WHERE` / `ORDER BY`.
const SUMMARY_SQL: &str = "SELECT t.id, t.title, t.cwd, t.updated_at,
            p.driver, p.model,
            EXISTS (SELECT 1 FROM events e WHERE e.thread_id = t.id AND e.seq > t.read_seq),
            t.archived
     FROM threads t
     LEFT JOIN provider_threads p ON p.id = COALESCE(
         t.active_provider_thread,
         (SELECT id FROM provider_threads q WHERE q.thread_id = t.id
          ORDER BY q.created_at DESC, q.rowid DESC LIMIT 1))";

fn summary_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ThreadSummary> {
    Ok(ThreadSummary {
        id: r.get(0)?,
        title: r.get(1)?,
        cwd: r.get(2)?,
        updated_at: r.get(3)?,
        driver: r.get(4)?,
        model: r.get(5)?,
        unread: r.get(6)?,
        archived: r.get(7)?,
    })
}

fn require_row(changed: usize, what: &str) -> Result<()> {
    if changed == 0 {
        Err(StoreError::NotFound(what.to_string()))
    } else {
        Ok(())
    }
}

/// A foreign-key failure on insert means the thread does not exist.
fn fk_or(e: rusqlite::Error, thread: &str) -> StoreError {
    match &e {
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            StoreError::NotFound(thread.to_string())
        }
        _ => StoreError::Sqlite(e),
    }
}

// ---- filesystem ----

fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    // `mode` applies only to directories this call creates; an existing
    // directory (the user's own state dir) is left as it is.
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    Ok(())
}

/// Switches to WAL. Changing the journal mode needs an exclusive moment that
/// SQLite does not always wait for (the busy handler is skipped when another
/// connection is mid-open), so a busy answer is retried for the busy timeout.
fn enable_wal(conn: &Connection) -> Result<()> {
    let deadline = std::time::Instant::now() + BUSY_TIMEOUT;
    loop {
        match conn.pragma_update(None, "journal_mode", "WAL") {
            Err(rusqlite::Error::SqliteFailure(f, _))
                if f.code == rusqlite::ErrorCode::DatabaseBusy
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            other => return other.map_err(Into::into),
        }
    }
}

/// Brings an existing database (and its directory) down to `0600` / `0700`: files
/// created by an older build or a loose umask must not stay readable. The
/// directory is only changed when the same user owns it and the file, so a
/// shared directory such as `/tmp` is never locked down by pointing the store
/// into it.
fn tighten_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let file = std::fs::metadata(path)?;
    if file.permissions().mode() & 0o777 != 0o600 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        let dir = std::fs::metadata(parent)?;
        if dir.uid() == file.uid() && dir.permissions().mode() & 0o777 != 0o700 {
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

fn create_private_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    // Created `0600` before SQLite opens it; WAL and SHM files copy the main
    // database file's permissions.
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    Ok(())
}

// ---- schema ----

/// Brings the schema to [`SCHEMA_VERSION`]. Each step runs once, inside a
/// `BEGIN IMMEDIATE` transaction that re-reads the version after taking the
/// write lock, so two processes opening a fresh database cannot both create the
/// tables, and calling this on an up-to-date database changes nothing.
fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)")?;
    if schema_version(conn)? == SCHEMA_VERSION {
        return Ok(()); // the common case takes no write lock
    }
    // A table rebuild (v2) must not fire `ON DELETE` actions of the tables that point at the
    // dropped one, and the pragma is a no-op inside a transaction, so it is switched off around
    // the whole migration and restored whatever the outcome. `migrate_locked` checks
    // `foreign_key_check` before it commits.
    let fk_was_on: bool = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
    conn.execute_batch("PRAGMA foreign_keys = OFF")?;
    let result = migrate_in_txn(conn);
    if fk_was_on {
        conn.execute_batch("PRAGMA foreign_keys = ON")?;
    }
    result
}

fn migrate_in_txn(conn: &Connection) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    match migrate_locked(conn) {
        Ok(()) => conn.execute_batch("COMMIT").map_err(Into::into),
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

fn schema_version(conn: &Connection) -> Result<i64> {
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |r| r.get(0),
    )?;
    if current > SCHEMA_VERSION {
        return Err(StoreError::SchemaTooNew {
            found: current,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(current)
}

fn migrate_locked(conn: &Connection) -> Result<()> {
    // Another process may have migrated while this one waited for the lock.
    let current = schema_version(conn)?;
    if current < 1 {
        conn.execute_batch(SCHEMA_V1)?;
    }
    if current < 2 {
        // Small app state that belongs with the threads (the open-thread list).
        conn.execute_batch(
            "CREATE TABLE meta (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             INSERT INTO schema_version (version) VALUES (2);",
        )?;
    }
    if current < 3 {
        // SQLite cannot alter a CHECK constraint, so the table is rebuilt (the documented
        // create-new, copy, drop, rename sequence) to allow the 'codex' driver. Rows keep their
        // ids, so `events.provider_thread_id` and `threads.active_provider_thread` stay valid.
        conn.execute_batch(
            "CREATE TABLE provider_threads_v3 (
                 id TEXT PRIMARY KEY,
                 thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
                 driver TEXT NOT NULL CHECK (driver IN ('claude', 'agy', 'codex')),
                 model TEXT NOT NULL,
                 native_id TEXT,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL
             );
             INSERT INTO provider_threads_v3
                 (id, thread_id, driver, model, native_id, created_at, updated_at)
                 SELECT id, thread_id, driver, model, native_id, created_at, updated_at
                 FROM provider_threads;
             DROP TABLE provider_threads;
             ALTER TABLE provider_threads_v3 RENAME TO provider_threads;
             CREATE INDEX provider_threads_thread ON provider_threads(thread_id);
             INSERT INTO schema_version (version) VALUES (3);",
        )?;
        let broken: Option<String> = conn
            .query_row(
                "SELECT \"table\" FROM pragma_foreign_key_check LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(table) = broken {
            return Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY),
                Some(format!("schema v3 left a dangling foreign key in {table}")),
            )));
        }
    }
    Ok(())
}

/// The first schema. Kept verbatim: databases created before v2 are upgraded from it, and the
/// migration test builds one.
const SCHEMA_V1: &str = "CREATE TABLE threads (
         id TEXT PRIMARY KEY,
         title TEXT NOT NULL,
         cwd TEXT NOT NULL,
         created_at INTEGER NOT NULL,
         updated_at INTEGER NOT NULL,
         archived INTEGER NOT NULL DEFAULT 0,
         active_provider_thread TEXT,
         read_seq INTEGER NOT NULL DEFAULT 0
     );
     CREATE TABLE provider_threads (
         id TEXT PRIMARY KEY,
         thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
         driver TEXT NOT NULL CHECK (driver IN ('claude', 'agy')),
         model TEXT NOT NULL,
         native_id TEXT,
         created_at INTEGER NOT NULL,
         updated_at INTEGER NOT NULL
     );
     CREATE INDEX provider_threads_thread ON provider_threads(thread_id);
     CREATE TABLE events (
         seq INTEGER PRIMARY KEY AUTOINCREMENT,
         thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
         provider_thread_id TEXT REFERENCES provider_threads(id) ON DELETE SET NULL,
         at INTEGER NOT NULL,
         envelope_json TEXT NOT NULL
     );
     CREATE INDEX events_thread_seq ON events(thread_id, seq);
     INSERT INTO schema_version (version) VALUES (1);";

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::event::{ItemStatus, TurnState};
    use std::os::unix::fs::PermissionsExt;

    fn store() -> Store {
        Store::open_in_memory().expect("open")
    }

    fn delta(item: &str, text: &str) -> Envelope {
        Envelope::new(Event::ContentDelta {
            stream: StreamKind::Assistant,
            text: text.into(),
        })
        .item(item)
    }

    fn started(item: &str, kind: ItemKind) -> Envelope {
        Envelope::new(Event::ItemStarted {
            kind,
            title: String::new(),
            input: None,
            parent: None,
        })
        .item(item)
    }

    #[test]
    fn thread_crud_orders_newest_first_and_hides_archived() {
        let s = store();
        let a = s.create_thread("/a", Some("first")).expect("a");
        let b = s.create_thread("/b", None).expect("b");
        let ids: Vec<_> = s
            .list_threads(false)
            .expect("list")
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, vec![b.clone(), a.clone()]);

        s.rename_thread(&a, "renamed").expect("rename");
        let list = s.list_threads(false).expect("list");
        assert_eq!(list[0].id, a, "a rename bumps updated_at");
        assert_eq!(list[0].title, "renamed");
        assert_eq!(list[0].cwd, "/a");

        s.set_archived(&a, true).expect("archive");
        assert_eq!(s.list_threads(false).expect("list").len(), 1);
        assert_eq!(s.list_threads(true).expect("list").len(), 2);
        s.set_archived(&a, false).expect("unarchive");
        assert_eq!(s.list_threads(false).expect("list").len(), 2);
    }

    #[test]
    fn an_imported_session_keeps_its_own_time_is_found_by_native_id_and_reads_as_seen() {
        let s = store();
        let fresh = s.create_thread("/new", None).expect("fresh");
        // A session from last week, imported now: it sorts under the new thread, not above.
        let old = s
            .create_thread_at("/old", Some("old session"), 1_000)
            .expect("old");
        let ids: Vec<_> = s
            .list_threads(false)
            .expect("list")
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, vec![fresh.clone(), old.clone()]);

        let pt = s
            .add_provider_thread(&old, "claude", "default")
            .expect("pt");
        s.set_native_id(&pt, "native-7").expect("native");
        assert_eq!(
            s.thread_with_native_id("claude", "native-7").expect("q"),
            Some(old.clone())
        );
        assert_eq!(s.thread_with_native_id("agy", "native-7").expect("q"), None);

        let envs = [
            started("u1", ItemKind::UserMessage),
            delta("u1", "hello"),
            started("a1", ItemKind::AssistantMessage),
        ];
        assert_eq!(s.import_events(&old, Some(&pt), &envs).expect("import"), 3);
        assert_eq!(s.events(&old, None, 10).expect("events").len(), 3);
        assert_eq!(
            s.import_events(&old, Some(&pt), &envs).expect("again"),
            0,
            "never into a thread that has history"
        );
        assert_eq!(s.events(&old, None, 10).expect("events").len(), 3);
        let summary = s.thread_summary(&old).expect("summary").expect("exists");
        assert_eq!(
            summary.updated_at, 1_000,
            "an import does not touch the thread"
        );
        assert!(!summary.unread, "imported history counts as seen");

        // Deleting it remembers the session, so the recent-sessions scan does not bring it back.
        assert!(!s.native_dismissed("claude", "native-7").expect("q"));
        s.delete_thread(&old).expect("delete");
        assert!(s.native_dismissed("claude", "native-7").expect("q"));
        assert_eq!(
            s.thread_with_native_id("claude", "native-7").expect("q"),
            None
        );
        assert!(
            s.delete_thread(&old).is_err(),
            "a missing thread is still an error"
        );
        assert_eq!(s.list_threads(true).expect("list").len(), 1);

        // Linking is one step: a thread with its active provider thread, at the session's time.
        let linked = s
            .link_native_thread("/w", Some("t"), 500, "agy", "conv-1")
            .expect("link");
        assert_eq!(
            s.thread_with_native_id("agy", "conv-1").expect("q"),
            Some(linked.clone())
        );
        assert_eq!(
            s.thread_summary(&linked)
                .expect("s")
                .expect("exists")
                .updated_at,
            500
        );
        s.dismiss_native("agy", "conv-0").expect("dismiss");
        assert!(s.native_dismissed("agy", "conv-0").expect("q"));
    }

    #[test]
    fn archived_threads_carry_their_flag_and_a_single_summary_can_be_fetched() {
        let s = store();
        let a = s.create_thread("/a", Some("first")).expect("a");
        s.set_archived(&a, true).expect("archive");
        let all = s.list_threads(true).expect("list");
        assert!(all.iter().find(|t| t.id == a).expect("a").archived);
        assert!(s.thread_summary(&a).expect("one").expect("some").archived);
        s.set_archived(&a, false).expect("unarchive");
        let one = s.thread_summary(&a).expect("one").expect("some");
        assert!(!one.archived);
        assert_eq!((one.title.as_str(), one.cwd.as_str()), ("first", "/a"));
        assert_eq!(s.thread_summary("nope").expect("none"), None);
    }

    #[test]
    fn a_thread_has_messages_once_the_user_has_sent_one() {
        let s = store();
        let t = s.create_thread("/w", Some("t")).expect("thread");
        assert!(!s.has_messages(&t).expect("empty"));
        s.append_event(&t, None, &Envelope::new(Event::Unknown))
            .expect("event");
        assert!(!s.has_messages(&t).expect("only a notice"));
        s.append_user_message(&t, None, "hello").expect("user");
        assert!(s.has_messages(&t).expect("one message"));
        let other = s.create_thread("/w", None).expect("other");
        assert!(!s.has_messages(&other).expect("other thread"));
    }

    #[test]
    fn delete_removes_a_thread_with_its_events_and_provider_threads_only() {
        let s = store();
        let keep = s.create_thread("/keep", Some("keep")).expect("keep");
        let gone = s.create_thread("/gone", Some("gone")).expect("gone");
        for t in [&keep, &gone] {
            let pt = s.add_provider_thread(t, "claude", "opus").expect("pt");
            s.set_active_provider_thread(t, &pt).expect("active");
            s.set_native_id(&pt, "native-session").expect("native");
            s.append_user_message(t, Some(&pt), "hello").expect("user");
            s.append_event(t, Some(&pt), &Envelope::new(Event::Unknown))
                .expect("event");
        }
        let count = |table: &str, thread: &str| -> i64 {
            s.conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE thread_id = ?1"),
                    params![thread],
                    |r| r.get(0),
                )
                .expect("count")
        };
        assert!(count("events", &gone) > 0 && count("provider_threads", &gone) == 1);

        s.delete_thread(&gone).expect("delete");
        assert_eq!(count("events", &gone), 0);
        assert_eq!(count("provider_threads", &gone), 0);
        assert_eq!(s.thread_summary(&gone).expect("query"), None);
        // The other thread is untouched.
        assert!(count("events", &keep) > 0);
        assert_eq!(count("provider_threads", &keep), 1);
        assert_eq!(s.list_threads(true).expect("list").len(), 1);
        assert!(matches!(
            s.delete_thread(&gone),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn unknown_ids_are_not_found() {
        let s = store();
        assert!(matches!(
            s.rename_thread("nope", "x"),
            Err(StoreError::NotFound(_))
        ));
        assert!(matches!(
            s.append_event("nope", None, &Envelope::new(Event::Unknown)),
            Err(StoreError::NotFound(_))
        ));
        assert!(matches!(
            s.add_provider_thread("nope", "claude", "m"),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn ids_look_like_v4_uuids() {
        let s = store();
        let id = s.create_thread("/", None).expect("t");
        let parts: Vec<_> = id.split('-').map(str::len).collect();
        assert_eq!(parts, vec![8, 4, 4, 4, 12], "{id}");
        assert_eq!(id.as_bytes()[14], b'4', "{id}");
        assert!(
            matches!(id.as_bytes()[19], b'8' | b'9' | b'a' | b'b'),
            "{id}"
        );
    }

    #[test]
    fn a_provider_threads_model_is_what_it_was_last_set_to() {
        let s = store();
        let t = s.create_thread("/p", Some("t")).expect("t");
        let p = s
            .add_provider_thread(&t, "claude", "claude-opus-4")
            .expect("p");
        s.set_active_provider_thread(&t, &p).expect("active");
        s.set_provider_model(&p, "opus").expect("set");
        assert_eq!(s.provider_threads(&t).expect("pts")[0].model, "opus");
        assert_eq!(
            s.list_threads(false).expect("l")[0].model.as_deref(),
            Some("opus")
        );
        assert!(matches!(
            s.set_provider_model("nope", "x"),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn provider_threads_span_a_thread_and_drive_the_summary() {
        let s = store();
        let t = s.create_thread("/p", Some("t")).expect("t");
        let p1 = s.add_provider_thread(&t, "claude", "opus").expect("p1");
        let p2 = s.add_provider_thread(&t, "agy", "gemini").expect("p2");
        assert_eq!(
            s.list_threads(false).expect("l")[0].driver.as_deref(),
            Some("agy")
        );

        s.set_native_id(&p1, "native-1").expect("native");
        s.set_active_provider_thread(&t, &p1).expect("active");
        assert_eq!(s.active_provider_thread(&t).expect("a"), Some(p1.clone()));
        let sum = &s.list_threads(false).expect("l")[0];
        assert_eq!(
            (sum.driver.as_deref(), sum.model.as_deref()),
            (Some("claude"), Some("opus"))
        );

        let all = s.provider_threads(&t).expect("pts");
        assert_eq!(
            all.iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
            vec![p1, p2.clone()]
        );
        assert_eq!(all[0].native_id.as_deref(), Some("native-1"));

        let other = s.create_thread("/o", None).expect("o");
        assert!(matches!(
            s.set_active_provider_thread(&other, &p2),
            Err(StoreError::NotFound(_))
        ));
        assert!(
            s.add_provider_thread(&t, "gpt", "x").is_err(),
            "driver is constrained"
        );
    }

    #[test]
    fn events_round_trip_in_order_with_paging() {
        let s = store();
        let t = s.create_thread("/", None).expect("t");
        let p = s.add_provider_thread(&t, "claude", "m").expect("p");
        let sent = vec![
            Envelope::new(Event::TurnStarted {
                model: Some("m".into()),
            }),
            delta("a1", "he").raw(serde_json::json!({"n": 1})),
            delta("a1", "llo"),
            Envelope::new(Event::TurnCompleted {
                state: TurnState::Completed,
                usage: None,
                cost_usd: Some(0.5),
                error: None,
            }),
        ];
        let seqs: Vec<i64> = sent
            .iter()
            .map(|e| s.append_event(&t, Some(&p), e).expect("append"))
            .collect();
        assert!(seqs.windows(2).all(|w| w[0] < w[1]));

        let got = s.events(&t, None, 100).expect("events");
        assert_eq!(got.iter().map(|(_, e)| e.clone()).collect::<Vec<_>>(), sent);
        let page = s.events(&t, Some(seqs[1]), 1).expect("page");
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].0, seqs[2]);
        assert!(s.events(&t, Some(seqs[3]), 10).expect("end").is_empty());
    }

    #[test]
    fn each_event_carries_the_agent_it_came_from() {
        let s = store();
        let t = s.create_thread("/", None).expect("t");
        let claude = s.add_provider_thread(&t, "claude", "opus").expect("p1");
        let agy = s.add_provider_thread(&t, "agy", "gemini").expect("p2");
        let note = |n: &str| Envelope::new(Event::Notice { text: n.into() });
        s.append_event(&t, Some(&claude), &note("a")).expect("a");
        s.append_event(&t, None, &note("b")).expect("b");
        s.append_event(&t, Some(&agy), &note("c")).expect("c");
        let got: Vec<_> = s
            .events_by_agent(&t, None, 10)
            .expect("events")
            .into_iter()
            .map(|e| e.driver)
            .collect();
        assert_eq!(
            got,
            [Some("claude".to_owned()), None, Some("agy".to_owned())],
            "an event stored without a provider thread has no agent"
        );
        // The plain reader returns the same events.
        assert_eq!(s.events(&t, None, 10).expect("events").len(), 3);
    }

    #[test]
    fn unread_tracks_events_since_mark_read() {
        let s = store();
        let t = s.create_thread("/", None).expect("t");
        assert!(!s.list_threads(false).expect("l")[0].unread);
        s.append_event(&t, None, &Envelope::new(Event::Unknown))
            .expect("a");
        assert!(s.list_threads(false).expect("l")[0].unread);
        s.mark_read(&t).expect("read");
        assert!(!s.list_threads(false).expect("l")[0].unread);
    }

    #[test]
    fn snapshot_replaces_deltas_and_later_deltas_append() {
        let s = store();
        let t = s.create_thread("/", None).expect("t");
        s.append_user_message(&t, None, "hello agent")
            .expect("user");
        for e in [
            started("a1", ItemKind::AssistantMessage),
            delta("a1", "par"),
            delta("a1", "tial"),
            Envelope::new(Event::ContentSnapshot {
                stream: StreamKind::Assistant,
                text: "Authoritative.".into(),
            })
            .item("a1"),
            delta("a1", " More."),
            Envelope::new(Event::ItemCompleted {
                status: ItemStatus::Completed,
                output: None,
                error: None,
            })
            .item("a1"),
            started("c1", ItemKind::Command),
            Envelope::new(Event::ItemCompleted {
                status: ItemStatus::Failed,
                output: Some("boom".into()),
                error: None,
            })
            .item("c1"),
            started("c2", ItemKind::Tool),
        ] {
            s.append_event(&t, None, &e).expect("append");
        }
        let msgs = s.transcript_messages(&t).expect("msgs");
        let view: Vec<_> = msgs
            .iter()
            .map(|m| {
                (
                    m.role.as_str(),
                    m.kind.as_str(),
                    m.text.as_str(),
                    m.status.as_str(),
                )
            })
            .collect();
        assert_eq!(
            view,
            vec![
                ("user", "user_message", "hello agent", "open"),
                (
                    "assistant",
                    "assistant_message",
                    "Authoritative. More.",
                    "completed"
                ),
                ("tool", "command", "boom", "failed"),
                ("tool", "tool", "", "open"),
            ]
        );
    }

    #[test]
    fn a_planted_token_is_masked_in_raw_and_text() {
        let s = store();
        let t = s.create_thread("/", None).expect("t");
        // A fake token in a real shape, the fixture the masking is tested on.
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123"; // gitleaks:allow
        s.append_user_message(&t, None, &format!("my token is {secret}"))
            .expect("user");
        s.append_event(
            &t,
            None,
            &delta("a1", &format!("echo {secret}"))
                .raw(serde_json::json!({"cmd": format!("export GH={secret}"), "n": [secret]})),
        )
        .expect("append");

        let stored: Vec<String> = {
            let mut stmt = s
                .conn
                .prepare("SELECT envelope_json FROM events")
                .expect("prep");
            let rows = stmt.query_map([], |r| r.get(0)).expect("q");
            rows.collect::<std::result::Result<_, _>>().expect("rows")
        };
        assert_eq!(stored.len(), 3);
        for json in &stored {
            assert!(!json.contains("ghp_abcdefghij"), "leaked: {json}");
        }
        assert!(stored[2].contains("****0123"), "{}", stored[2]);
        let msgs = s.transcript_messages(&t).expect("msgs");
        assert!(msgs[0].text.contains("****0123"), "{:?}", msgs[0]);
    }

    #[test]
    fn open_creates_private_dir_and_file() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp
            .path()
            .join("state")
            .join("agent-terminal")
            .join("threads.db");
        let s = Store::open(&path).expect("open");
        let t = s.create_thread("/", Some("kept")).expect("t");
        drop(s);
        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(path.parent().expect("parent")), 0o700);
        assert_eq!(mode(&path), 0o600);
        let wal = path.with_extension("db-wal");
        if wal.exists() {
            assert_eq!(mode(&wal), 0o600);
        }
        let again = Store::open(&path).expect("reopen");
        assert_eq!(again.list_threads(false).expect("l")[0].id, t);
    }

    #[test]
    fn open_tightens_an_existing_loose_db_and_dir() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path().join("agent-terminal");
        std::fs::create_dir(&dir).expect("dir");
        let path = dir.join("threads.db");
        drop(Store::open(&path).expect("first"));
        let chmod = |p: &Path, m| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).expect("chmod")
        };
        chmod(&path, 0o644);
        chmod(&dir, 0o755);
        drop(Store::open(&path).expect("reopen"));
        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn concurrent_first_opens_all_succeed_and_migrate_once() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("agent-terminal").join("threads.db");
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    Store::open(&path).map(|_| ()).map_err(|e| e.to_string())
                })
            })
            .collect();
        for h in handles {
            h.join().expect("thread").expect("open");
        }
        let s = Store::open(&path).expect("open");
        let rows: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))
            .expect("count");
        assert_eq!(rows, SCHEMA_VERSION, "one row per migration step");
        let busy: i64 = s
            .conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .expect("busy");
        assert_eq!(busy, 5000);
        let sync: i64 = s
            .conn
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .expect("sync");
        assert_eq!(sync, 1, "NORMAL");
    }

    #[test]
    fn a_failed_append_leaves_no_event_behind() {
        let s = store();
        assert!(s
            .append_event("nope", None, &Envelope::new(Event::Unknown))
            .is_err());
        let n: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .expect("count");
        assert_eq!(n, 0);
        // And a good append is atomic with the timestamp bump.
        let t = s.create_thread("/", None).expect("t");
        let before = s.list_threads(false).expect("l")[0].updated_at;
        s.append_event(&t, None, &Envelope::new(Event::Unknown))
            .expect("a");
        assert!(s.list_threads(false).expect("l")[0].updated_at > before);
    }

    #[test]
    fn thread_titles_are_redacted() {
        let s = store();
        // A fake token in a real shape, assembled so no literal sits in a command line.
        let secret = format!("ghp_{}", "abcdefghijklmnopqrstuvwxyz0123");
        let t = s
            .create_thread("/", Some(&format!("fix {secret}")))
            .expect("t");
        let title = |s: &Store| s.list_threads(false).expect("l")[0].title.clone();
        assert!(!title(&s).contains("ghp_abcdefghij"), "{}", title(&s));
        s.rename_thread(&t, &format!("again {secret}"))
            .expect("rename");
        assert!(!title(&s).contains("ghp_abcdefghij"), "{}", title(&s));
        assert!(title(&s).contains("****0123"));
    }

    #[test]
    fn sensitive_keys_are_masked_in_nested_json_and_raw_frames() {
        let s = store();
        let t = s.create_thread("/", None).expect("t");
        let mut env = Envelope::new(Event::ContentDelta {
            stream: StreamKind::Assistant,
            text: "hi".into(),
        })
        .item("a1");
        env.raw = Some(serde_json::json!({
            "api_key": "plain-secret-9981", // gitleaks:allow
            "nested": {"db": {"Password": "hunter2hunter2"}, "list": [{"client_secret": "abcdefgh5678"}]}, // gitleaks:allow
            "note": "fine"
        }));
        s.append_event(&t, None, &env).expect("append");
        let stored = serde_json::to_string(&s.events(&t, None, 10).expect("ev")).expect("json");
        for leaked in ["plain-secret-9981", "hunter2hunter2", "abcdefgh5678"] {
            assert!(!stored.contains(leaked), "{leaked} leaked: {stored}");
        }
        assert!(
            stored.contains("****9981") && stored.contains("\"fine\""),
            "{stored}"
        );
    }

    #[test]
    fn a_token_split_across_deltas_is_masked_once_the_item_completes() {
        let s = store();
        let t = s.create_thread("/", None).expect("t");
        let secret = format!("ghp_{}", "abcdefghijklmnopqrstuvwxyz0123");
        let (a, b) = secret.split_at(12);
        for e in [
            started("a1", ItemKind::AssistantMessage),
            delta("a1", &format!("token is {a}")),
            delta("a1", &format!("{b} ok")),
        ] {
            s.append_event(&t, None, &e).expect("append");
        }
        let all =
            |s: &Store| serde_json::to_string(&s.events(&t, None, 50).expect("ev")).expect("j");
        // Until completion the fragments are all there is.
        assert!(all(&s).contains(a));
        s.append_event(
            &t,
            None,
            &Envelope::new(Event::ItemCompleted {
                status: ItemStatus::Completed,
                output: None,
                error: None,
            })
            .item("a1"),
        )
        .expect("complete");
        let stored = all(&s);
        assert!(!stored.contains(a) && !stored.contains(b), "{stored}");
        let msgs = s.transcript_messages(&t).expect("msgs");
        assert_eq!(msgs[0].text, "token is ****0123 ok");
    }

    #[test]
    fn transcript_leaves_out_reasoning() {
        let s = store();
        let t = s.create_thread("/", None).expect("t");
        let think = |text: &str| {
            Envelope::new(Event::ContentDelta {
                stream: StreamKind::Reasoning,
                text: text.into(),
            })
            .item("r1")
        };
        for e in [
            started("r1", ItemKind::Reasoning),
            think("private thoughts"),
            Envelope::new(Event::ContentSnapshot {
                stream: StreamKind::Reasoning,
                text: "private thoughts".into(),
            })
            .item("r1"),
            Envelope::new(Event::ItemCompleted {
                status: ItemStatus::Completed,
                output: None,
                error: None,
            })
            .item("r1"),
            started("a1", ItemKind::AssistantMessage),
            delta("a1", "answer"),
        ] {
            s.append_event(&t, None, &e).expect("append");
        }
        let msgs = s.transcript_messages(&t).expect("msgs");
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert_eq!(msgs[0].text, "answer");
    }

    #[test]
    fn meta_values_are_set_replaced_and_survive_a_v1_upgrade() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("threads.db");
        // A database as the first schema left it: no meta table yet.
        {
            let conn = Connection::open(&path).expect("open");
            conn.execute_batch(
                "CREATE TABLE schema_version (version INTEGER NOT NULL);
                 CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT NOT NULL, cwd TEXT NOT NULL,
                     created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
                     archived INTEGER NOT NULL DEFAULT 0, active_provider_thread TEXT,
                     read_seq INTEGER NOT NULL DEFAULT 0);
                 CREATE TABLE provider_threads (id TEXT PRIMARY KEY, thread_id TEXT NOT NULL,
                     driver TEXT NOT NULL, model TEXT NOT NULL, native_id TEXT,
                     created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
                 CREATE TABLE events (seq INTEGER PRIMARY KEY AUTOINCREMENT, thread_id TEXT NOT NULL,
                     provider_thread_id TEXT, at INTEGER NOT NULL, envelope_json TEXT NOT NULL);
                 INSERT INTO threads VALUES ('t1', 'old', '/w', 1, 1, 0, NULL, 0);
                 INSERT INTO schema_version (version) VALUES (1);",
            )
            .expect("v1");
        }
        let s = Store::open(&path).expect("upgrade");
        assert_eq!(s.list_threads(false).expect("list")[0].title, "old");
        assert_eq!(s.meta("open_threads").expect("meta"), None);
        s.set_meta("open_threads", "[\"t1\"]").expect("set");
        s.set_meta("open_threads", "[]").expect("replace");
        assert_eq!(s.meta("open_threads").expect("meta").as_deref(), Some("[]"));
    }

    #[test]
    fn migration_is_idempotent_and_refuses_a_newer_schema() {
        let s = store();
        migrate(&s.conn).expect("again");
        migrate(&s.conn).expect("and again");
        let rows: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))
            .expect("count");
        assert_eq!(rows, SCHEMA_VERSION, "one row per migration step");

        s.conn
            .execute("INSERT INTO schema_version (version) VALUES (99)", [])
            .expect("bump");
        assert!(matches!(
            migrate(&s.conn),
            Err(StoreError::SchemaTooNew { found: 99, .. })
        ));
    }

    #[test]
    fn a_v1_database_migrates_to_v3_keeping_rows_and_allowing_codex() {
        let conn = Connection::open_in_memory().expect("open");
        conn.pragma_update(None, "foreign_keys", "ON").expect("fk");
        conn.execute_batch("CREATE TABLE schema_version (version INTEGER NOT NULL)")
            .expect("version table");
        conn.execute_batch(SCHEMA_V1).expect("v1 schema");
        conn.execute_batch(
            "INSERT INTO threads (id, title, cwd, created_at, updated_at, active_provider_thread)
                 VALUES ('t1', 'old', '/w', 1, 2, 'p1');
             INSERT INTO provider_threads (id, thread_id, driver, model, native_id, created_at, updated_at)
                 VALUES ('p1', 't1', 'claude', 'opus', 'n1', 1, 2),
                        ('p2', 't1', 'agy', 'gemini', NULL, 3, 4);
             INSERT INTO events (thread_id, provider_thread_id, at, envelope_json)
                 VALUES ('t1', 'p1', 5, '{}'), ('t1', 'p1', 6, '{}'), ('t1', NULL, 7, '{}');",
        )
        .expect("v1 rows");
        assert!(conn
            .execute(
                "INSERT INTO provider_threads (id, thread_id, driver, model, created_at, updated_at)
                 VALUES ('p3', 't1', 'codex', 'm', 0, 0)",
                []
            )
            .is_err());

        migrate(&conn).expect("migrate");

        let version: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .expect("version");
        assert_eq!(version, 3);
        let rows: Vec<(String, String, Option<String>)> = conn
            .prepare("SELECT id, driver, native_id FROM provider_threads ORDER BY id")
            .expect("prepare")
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .expect("query")
            .collect::<std::result::Result<_, _>>()
            .expect("rows");
        assert_eq!(
            rows,
            vec![
                ("p1".into(), "claude".into(), Some("n1".into())),
                ("p2".into(), "agy".into(), None)
            ]
        );
        let links: Vec<Option<String>> = conn
            .prepare("SELECT provider_thread_id FROM events ORDER BY seq")
            .expect("prepare")
            .query_map([], |r| r.get(0))
            .expect("query")
            .collect::<std::result::Result<_, _>>()
            .expect("links");
        assert_eq!(links, vec![Some("p1".into()), Some("p1".into()), None]);
        let active: String = conn
            .query_row(
                "SELECT active_provider_thread FROM threads WHERE id = 't1'",
                [],
                |r| r.get(0),
            )
            .expect("active");
        assert_eq!(active, "p1");

        // Foreign keys are back on, the new CHECK allows codex and still refuses others.
        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .expect("fk");
        assert_eq!(fk, 1);
        let insert = |id: &str, driver: &str| {
            conn.execute(
                "INSERT INTO provider_threads (id, thread_id, driver, model, created_at, updated_at)
                 VALUES (?1, 't1', ?2, 'm', 0, 0)",
                params![id, driver],
            )
        };
        insert("p3", "codex").expect("codex allowed");
        assert!(insert("p4", "gpt").is_err());
        let index: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'provider_threads_thread'",
                [],
                |r| r.get(0),
            )
            .expect("index");
        assert_eq!(index, 1);

        // The cascade from threads still reaches the rebuilt table, and a second run is a no-op.
        migrate(&conn).expect("again");
        conn.execute("DELETE FROM threads WHERE id = 't1'", [])
            .expect("delete");
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM provider_threads", [], |r| r.get(0))
            .expect("count");
        assert_eq!(left, 0);
    }

    #[test]
    fn default_path_follows_xdg_with_a_home_fallback() {
        assert_eq!(
            default_path(Some("/x/state"), Some("/h")),
            Some(PathBuf::from("/x/state/agent-terminal/threads.db"))
        );
        let fallback = Some(PathBuf::from("/h/.local/state/agent-terminal/threads.db"));
        assert_eq!(default_path(None, Some("/h")), fallback);
        assert_eq!(default_path(Some(""), Some("/h")), fallback);
        assert_eq!(default_path(Some("relative"), Some("/h")), fallback);
        assert_eq!(default_path(None, None), None);
    }

    // ---- heal tails, batches, checkpoints ----

    fn completed(item: &str, status: ItemStatus) -> Envelope {
        Envelope::new(Event::ItemCompleted {
            status,
            output: None,
            error: None,
        })
        .item(item)
    }

    fn stream_delta(item: &str, stream: StreamKind, text: &str) -> Envelope {
        Envelope::new(Event::ContentDelta {
            stream,
            text: text.into(),
        })
        .item(item)
        .raw(serde_json::json!({"frag": text}))
    }

    /// Every events row, byte for byte (`at` aside: it is the wall clock).
    fn event_rows(s: &Store) -> Vec<(i64, String, Option<String>, String)> {
        let mut stmt = s
            .conn
            .prepare(
                "SELECT seq, thread_id, provider_thread_id, envelope_json FROM events ORDER BY seq",
            )
            .expect("prep");
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .expect("q");
        rows.collect::<std::result::Result<_, _>>().expect("rows")
    }

    /// A small deterministic generator (xorshift), so a failing seed replays.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// One step of a randomised script, applied the same way to both stores.
    #[allow(clippy::large_enum_variant)] // a test script, built once
    enum Step {
        Append(String, Envelope),
        /// A user prompt (`append_user_message_with_id`: two rows in one transaction).
        Prompt(String, String),
        /// An imported history (`import_events`), into a thread that has none yet.
        Import(String, Vec<Envelope>),
        /// The next append fails after its insert and tail update (an injected write error).
        Fail,
        /// The next append panics after its insert and tail update.
        Panic,
        /// SQLite rolls the open transaction back on its own (inside a batch).
        LoseTxn,
        /// The next batch's COMMIT fails.
        FailCommit,
        Forget(String),
        /// One job doing these steps, then ending as `end` says: its savepoint (and its tail
        /// level) kept, or taken back after an `Err` or a panic.
        Job(Vec<Step>, End),
        /// These steps as one batch of the store writer (`store_writer::run_jobs_as_batch`).
        Batch(Vec<Step>),
    }

    #[derive(Clone, Copy)]
    enum End {
        Ok,
        Err,
        Panic,
    }

    fn split(rng: &mut Rng, secret: &str) -> Vec<String> {
        let cuts = 1 + rng.below(4);
        let mut at: Vec<usize> = (0..cuts).map(|_| 1 + rng.below(secret.len() - 1)).collect();
        at.sort_unstable();
        at.dedup();
        let mut frags = Vec::new();
        let mut last = 0;
        for c in at {
            frags.push(secret[last..c].to_owned());
            last = c;
        }
        frags.push(secret[last..].to_owned());
        frags
    }

    fn script(seed: u64) -> Vec<Step> {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let threads = ["t1", "t2", "t3"];
        let streams = [StreamKind::Assistant, StreamKind::Reasoning];
        let secret = format!("ghp_{}", "abcdefghijklmnopqrstuvwxyz0123");
        // Open items: (thread, item, pending fragments of a split secret).
        let mut open: Vec<(usize, String, Vec<String>)> = Vec::new();
        // Items completed (or whose turn ended): an id that may come back.
        let mut closed: Vec<(usize, String)> = Vec::new();
        let mut next_item = 0;
        let mut imported = false;
        let mut steps = Vec::new();
        // The next delta of an open item, consuming a fragment of its secret when it has one.
        let next_delta =
            |rng: &mut Rng, open: &mut Vec<(usize, String, Vec<String>)>| -> Option<Step> {
                if open.is_empty() {
                    return None;
                }
                let k = rng.below(open.len());
                let stream = streams[rng.below(2)];
                let (t, item, frags) = &mut open[k];
                let text = if !frags.is_empty() && rng.below(3) != 0 {
                    let f = frags.remove(0);
                    if rng.below(2) == 0 {
                        format!("token {f}")
                    } else {
                        f
                    }
                } else {
                    format!("word{} ", rng.below(100))
                };
                Some(Step::Append(
                    threads[*t].into(),
                    stream_delta(item, stream, &text),
                ))
            };
        for _ in 0..400 {
            let t = rng.below(2);
            let step = match rng.below(27) {
                0..=2 => {
                    // A new item; sometimes one open already (started twice), sometimes one
                    // completed before (a reused id).
                    let item = match rng.below(8) {
                        0 if !open.is_empty() => open[rng.below(open.len())].1.clone(),
                        1 if !closed.is_empty() => closed.remove(rng.below(closed.len())).1,
                        _ => {
                            next_item += 1;
                            format!("i{next_item}")
                        }
                    };
                    let frags = if rng.below(2) == 0 {
                        split(&mut rng, &secret)
                    } else {
                        Vec::new()
                    };
                    open.push((t, item.clone(), frags));
                    Step::Append(
                        threads[t].into(),
                        started(&item, ItemKind::AssistantMessage),
                    )
                }
                3..=11 => match next_delta(&mut rng, &mut open) {
                    Some(step) => step,
                    None => continue,
                },
                12 if !open.is_empty() => {
                    let (t, item, _) = &open[rng.below(open.len())];
                    let snap = Envelope::new(Event::ContentSnapshot {
                        stream: streams[rng.below(2)],
                        text: "snapshot".into(),
                    })
                    .item(item.as_str());
                    Step::Append(threads[*t].into(), snap)
                }
                13 | 14 if !open.is_empty() => {
                    let (t, item, _) = open.remove(rng.below(open.len()));
                    let status = if rng.below(4) == 0 {
                        ItemStatus::Interrupted
                    } else {
                        ItemStatus::Completed
                    };
                    closed.push((t, item.clone()));
                    Step::Append(threads[t].into(), completed(&item, status))
                }
                15 => {
                    // Deltas of an item that never started here (a restart mid-item).
                    next_item += 1;
                    let item = format!("i{next_item}");
                    open.push((t, item.clone(), Vec::new()));
                    Step::Append(
                        threads[t].into(),
                        stream_delta(&item, StreamKind::Assistant, &secret[..9]),
                    )
                }
                16 => Step::Append(
                    threads[t].into(),
                    Envelope::new(Event::TurnCompleted {
                        state: agent_core::event::TurnState::Completed,
                        usage: None,
                        cost_usd: None,
                        error: None,
                    }),
                ),
                17 => Step::Fail,
                18 => Step::Panic,
                19 => Step::Forget(threads[t].into()),
                20 => {
                    next_item += 1;
                    Step::Prompt(threads[t].into(), format!("u{next_item}"))
                }
                22 | 23 => {
                    // A secret split around a delta that does not survive (its write fails,
                    // panics, or its batch is rolled back or not committed): only a tail that
                    // forgets that row heals what the store really holds.
                    next_item += 1;
                    let item = format!("x{next_item}");
                    let at = 1 + rng.below(secret.len() - 1);
                    let delta = |text: &str| {
                        Step::Append(
                            threads[t].into(),
                            stream_delta(&item, StreamKind::Assistant, text),
                        )
                    };
                    steps.push(Step::Append(
                        threads[t].into(),
                        started(&item, ItemKind::AssistantMessage),
                    ));
                    steps.push(delta(&secret[..at]));
                    match rng.below(6) {
                        0 => steps.push(Step::Batch(vec![delta("lost "), Step::FailCommit])),
                        1 => {
                            steps.push(Step::Batch(vec![delta("lost "), Step::LoseTxn, delta("")]))
                        }
                        2 => steps.push(Step::Batch(vec![Step::Panic, delta("lost ")])),
                        3 => steps.push(Step::Batch(vec![Step::Fail, delta("lost ")])),
                        4 => steps.extend([Step::Panic, delta("lost ")]),
                        _ => steps.extend([Step::Fail, delta("lost ")]),
                    }
                    steps.push(delta(&secret[at..]));
                    closed.push((t, item.clone()));
                    Step::Append(threads[t].into(), completed(&item, ItemStatus::Completed))
                }
                25 => {
                    // Codex's case: a job appends the rest of a split secret and a snapshot
                    // that heals it, then fails, returns Err or panics. Its rows and its
                    // heal go; the tail must forget them too, or the next completion would
                    // trust healed text the store no longer holds.
                    next_item += 1;
                    let item = format!("j{next_item}");
                    let at = 1 + rng.below(secret.len() - 1);
                    let delta = |text: &str| {
                        Step::Append(
                            threads[t].into(),
                            stream_delta(&item, StreamKind::Assistant, text),
                        )
                    };
                    let snapshot = Step::Append(
                        threads[t].into(),
                        Envelope::new(Event::ContentSnapshot {
                            stream: StreamKind::Reasoning,
                            text: "snapshot".into(),
                        })
                        .item(item.as_str()),
                    );
                    steps.push(Step::Append(
                        threads[t].into(),
                        started(&item, ItemKind::AssistantMessage),
                    ));
                    steps.push(delta(&secret[..at]));
                    let end = match rng.below(3) {
                        0 => End::Err,
                        1 => End::Panic,
                        _ => End::Ok,
                    };
                    let job = Step::Job(vec![delta(&secret[at..]), snapshot], end);
                    if rng.below(2) == 0 {
                        steps.push(Step::Batch(vec![job]));
                    } else {
                        steps.push(job);
                    }
                    // The rest again (it was taken back, unless the job was Ok), then done.
                    steps.push(delta(&secret[at..]));
                    closed.push((t, item.clone()));
                    Step::Append(threads[t].into(), completed(&item, ItemStatus::Completed))
                }
                24 => {
                    // A reused id: half a secret in its first life, the rest after it starts
                    // again. The scan joins both lives; a tail started anew would not.
                    next_item += 1;
                    let item = format!("r{next_item}");
                    let at = 1 + rng.below(secret.len() - 1);
                    let mut life = |text: &str| {
                        steps.push(Step::Append(
                            threads[t].into(),
                            started(&item, ItemKind::AssistantMessage),
                        ));
                        steps.push(Step::Append(
                            threads[t].into(),
                            stream_delta(&item, StreamKind::Assistant, text),
                        ));
                        steps.push(Step::Append(
                            threads[t].into(),
                            completed(&item, ItemStatus::Completed),
                        ));
                    };
                    life(&secret[..at]);
                    life(&secret[at..]);
                    continue;
                }
                21 if !imported => {
                    // A history with half a secret in an item that never completed there: a
                    // later start of the same id must not trust a tail without that row.
                    imported = true;
                    let mut frags = split(&mut rng, &secret);
                    let first = frags.remove(0);
                    open.push((2, "imp".into(), frags));
                    Step::Import(
                        "t3".into(),
                        vec![
                            started("imp", ItemKind::AssistantMessage),
                            stream_delta("imp", StreamKind::Assistant, &first),
                        ],
                    )
                }
                _ => {
                    // A batch of the writer: deltas, with now and then a failure inside it.
                    let mut inner = Vec::new();
                    for _ in 0..1 + rng.below(5) {
                        match rng.below(12) {
                            0 => inner.push(Step::Fail),
                            1 => inner.push(Step::Panic),
                            2 => inner.push(Step::LoseTxn),
                            3 => inner.push(Step::FailCommit),
                            _ => inner.extend(next_delta(&mut rng, &mut open)),
                        }
                    }
                    Step::Batch(inner)
                }
            };
            steps.push(step);
        }
        // Everything still open completes, so every item gets its final heal.
        for (t, item, _) in open {
            steps.push(Step::Append(
                threads[t].into(),
                completed(&item, ItemStatus::Completed),
            ));
        }
        steps
    }

    /// One step as a job of the store writer.
    fn job_of(step: &Step) -> crate::store_writer::TestJob {
        match step {
            Step::Append(t, env) => {
                let (t, env) = (t.clone(), env.clone());
                Box::new(move |s: &Store| s.append_event(&t, None, &env).map(drop))
            }
            Step::Prompt(t, item) => {
                let (t, item) = (t.clone(), item.clone());
                Box::new(move |s: &Store| s.append_user_message_with_id(&t, None, &item, "hi"))
            }
            Step::Import(t, envs) => {
                let (t, envs) = (t.clone(), envs.clone());
                Box::new(move |s: &Store| s.import_events(&t, None, &envs).map(drop))
            }
            Step::Fail => Box::new(|s: &Store| {
                s.fail_next_append.set(true);
                Ok(())
            }),
            Step::Panic => Box::new(|s: &Store| {
                s.panic_next_append.set(true);
                Ok(())
            }),
            Step::LoseTxn => Box::new(|s: &Store| {
                s.lose_transaction();
                Ok(())
            }),
            Step::FailCommit => Box::new(|s: &Store| {
                s.fail_next_commit.set(true);
                Ok(())
            }),
            Step::Forget(t) => {
                let t = t.clone();
                Box::new(move |s: &Store| {
                    s.forget_tails(&t);
                    Ok(())
                })
            }
            Step::Job(inner, end) => {
                let jobs: Vec<_> = inner.iter().map(job_of).collect();
                let end = *end;
                // Plain, as the writer's jobs are: inside a batch only the job's own savepoint
                // (and its tail level) stands between a failed job and the store.
                Box::new(move |s: &Store| {
                    for job in jobs {
                        job(s)?;
                    }
                    match end {
                        End::Ok => Ok(()),
                        End::Err => Err(StoreError::Failed("injected job error".into())),
                        End::Panic => panic!("injected job panic"),
                    }
                })
            }
            Step::Batch(_) => Box::new(|_: &Store| Ok(())),
        }
    }

    /// The invariant the tails rest on: outside any transaction, every trusted tail holds
    /// exactly the item's committed delta rows, stream by stream, as a fresh scan reads them.
    fn assert_tails_match_committed(s: &Store, context: &str) {
        let tails = s.tails.borrow();
        assert!(!tails.in_level(), "{context}: a tail level left open");
        for ((thread, item), streams) in tails.all_trusted() {
            let mut stmt = s
                .conn
                .prepare(
                    "SELECT seq, envelope_json FROM events WHERE thread_id = ?1
                       AND json_extract(envelope_json, '$.item') = ?2
                       AND json_extract(envelope_json, '$.event.type') = 'content_delta'
                     ORDER BY seq",
                )
                .expect("prep");
            let rows = stmt
                .query_map(params![thread, item], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                })
                .expect("q");
            let mut scanned: Vec<(StreamKind, crate::heal_tail::StreamRows)> = Vec::new();
            for row in rows {
                let (seq, json) = row.expect("row");
                let env: Envelope = serde_json::from_str(&json).expect("json");
                let Event::ContentDelta { stream, text } = env.event else {
                    continue;
                };
                match scanned.iter_mut().find(|(st, _)| *st == stream) {
                    Some((_, v)) => v.push((seq, text)),
                    None => scanned.push((stream, vec![(seq, text)])),
                }
            }
            assert_eq!(
                streams, scanned,
                "{context}: the tail of {thread}/{item} is not what the store holds"
            );
        }
    }

    fn play(s: &Store, steps: &[Step]) {
        quiet_injected_panics();
        for (n, step) in steps.iter().enumerate() {
            match step {
                Step::Batch(inner) => {
                    crate::store_writer::run_jobs_as_batch(s, inner.iter().map(job_of).collect());
                }
                step @ Step::Job(..) => {
                    // On its own, a job runs as the inline store runs one: atomically.
                    let job = job_of(step);
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        s.atomically(job)
                    }));
                }
                step => {
                    let job = job_of(step);
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(s)));
                }
            }
            assert_tails_match_committed(s, &format!("after step {n}"));
        }
        // A hook left set by the last steps must not leak into the comparison.
        s.fail_next_append.set(false);
        s.panic_next_append.set(false);
        s.fail_next_commit.set(false);
    }

    fn diff_store(tails: bool, seed: u64) -> Store {
        let s = store();
        s.set_heal_tails(tails);
        if tails && seed.is_multiple_of(3) {
            s.tails.borrow_mut().set_caps(120, 360);
        }
        if tails && seed.is_multiple_of(5) {
            // Few tails at once: deltas of items past the limit leave no marker at all.
            s.tails.borrow_mut().set_max_items(2);
        }
        for t in ["t1", "t2", "t3"] {
            s.create_thread_with_id(t, "/", None).expect("thread");
        }
        s
    }

    /// §6.4: the same randomised script (split secrets across 2-5 deltas, two interleaved
    /// streams and threads, snapshots, interrupts, items started twice, never, or again after
    /// completing (reused ids), prompts, an import, overflow, too many items, injected append
    /// errors and panics, forgotten tails, and writer batches with failures, panics, a lost
    /// transaction or a failed COMMIT inside) into a store healing from its tails and one forced
    /// to the SQL scan leaves byte-identical events tables.
    #[test]
    fn healing_from_tails_writes_exactly_what_the_sql_scan_does() {
        let mut paths = [0u32; 2];
        let mut healed_rows = 0;
        for seed in 1..=200u64 {
            let steps = script(seed);
            let tail = diff_store(true, seed);
            let sql = diff_store(false, seed);
            play(&tail, &steps);
            play(&sql, &steps);
            let rows = event_rows(&tail);
            healed_rows += rows
                .iter()
                .filter(|(_, _, _, j)| j.contains("content_delta") && j.contains("****0123"))
                .count();
            assert_eq!(rows, event_rows(&sql), "seed {seed}");
            let p = tail.heal_paths.get();
            paths = [paths[0] + p[0], paths[1] + p[1]];
            assert_eq!(
                sql.heal_paths.get()[0],
                0,
                "the forced store never used a tail"
            );
        }
        // Both paths really ran: the comparison was not vacuous.
        assert!(paths[0] > 100 && paths[1] > 20, "{paths:?}");
        // And split secrets really were healed (masked whole in a delta row).
        assert!(healed_rows > 5, "{healed_rows}");
    }

    /// Past [`HEAL_WINDOW`] the scan no longer sees an item's first rows; the tail still does,
    /// so the tail store is at least as scrubbed (here: strictly more).
    #[test]
    fn an_item_longer_than_the_heal_window_is_at_least_as_scrubbed_from_its_tail() {
        let secret = format!("ghp_{}", "abcdefghijklmnopqrstuvwxyz0123");
        let (a, b) = secret.split_at(11);
        let mut steps = vec![
            Step::Append("t1".into(), started("long", ItemKind::AssistantMessage)),
            Step::Append(
                "t1".into(),
                stream_delta("long", StreamKind::Assistant, &format!("key {a}")),
            ),
            Step::Append(
                "t1".into(),
                stream_delta("long", StreamKind::Assistant, &format!("{b} ok")),
            ),
        ];
        for n in 0..HEAL_WINDOW + 10 {
            steps.push(Step::Append(
                "t1".into(),
                stream_delta("long", StreamKind::Assistant, &format!("w{n} ")),
            ));
        }
        steps.push(Step::Append(
            "t1".into(),
            completed("long", ItemStatus::Completed),
        ));
        let tail = diff_store(true, 1);
        let sql = diff_store(false, 1);
        play(&tail, &steps);
        play(&sql, &steps);
        let leaks = |s: &Store| {
            event_rows(s)
                .iter()
                .filter(|(_, _, _, json)| json.contains(a) || json.contains(b))
                .count()
        };
        assert_eq!(leaks(&tail), 0);
        assert!(leaks(&sql) > 0, "the scan's window really was exceeded");
        assert_eq!(tail.heal_paths.get(), [1, 0]);
    }

    #[test]
    fn a_tail_cut_short_by_an_append_error_falls_back_to_the_scan() {
        let s = store();
        let t = s.create_thread("/", None).expect("t");
        let secret = format!("ghp_{}", "abcdefghijklmnopqrstuvwxyz0123");
        let (a, b) = secret.split_at(12);
        s.append_event(&t, None, &started("a1", ItemKind::AssistantMessage))
            .expect("start");
        s.append_event(&t, None, &delta("a1", a)).expect("a");
        s.fail_next_append.set(true);
        assert!(s.append_event(&t, None, &delta("a1", "lost")).is_err());
        s.append_event(&t, None, &delta("a1", b)).expect("b");
        s.append_event(&t, None, &completed("a1", ItemStatus::Completed))
            .expect("done");
        assert_eq!(s.heal_paths.get(), [0, 1], "healed by the scan");
        let all = serde_json::to_string(&s.events(&t, None, 50).expect("ev")).expect("j");
        assert!(!all.contains(a) && !all.contains("lost"), "{all}");
    }

    fn leaks(s: &Store, parts: &[&str]) -> bool {
        event_rows(s)
            .iter()
            .any(|(_, _, _, json)| parts.iter().any(|p| json.contains(p)))
    }

    /// B1: an item id that comes back (after its completion cleared its tail) already has rows;
    /// a tail started anew would miss them, so it heals by the scan, which joins both lives.
    #[test]
    fn a_reused_item_id_heals_by_the_scan() {
        let s = diff_store(true, 1);
        let secret = format!("ghp_{}", "abcdefghijklmnopqrstuvwxyz0123");
        let (a, b) = secret.split_at(13);
        for env in [
            started("r", ItemKind::AssistantMessage),
            delta("r", a),
            completed("r", ItemStatus::Completed),
            started("r", ItemKind::AssistantMessage),
            delta("r", b),
            completed("r", ItemStatus::Completed),
        ] {
            s.append_event("t1", None, &env).expect("append");
        }
        assert!(!leaks(&s, &[a, b]));
        assert_eq!(
            s.heal_paths.get(),
            [1, 1],
            "the second life heals by the scan"
        );
    }

    /// A1: a delta that came before its item's start while the tails were full left no marker;
    /// the start that follows must still not trust a tail that lacks that row.
    #[test]
    fn a_delta_unseen_by_full_tails_is_not_vouched_for_later() {
        let s = diff_store(true, 1);
        s.tails.borrow_mut().set_max_items(1);
        let secret = format!("ghp_{}", "abcdefghijklmnopqrstuvwxyz0123");
        let (a, b) = secret.split_at(9);
        s.append_event("t1", None, &started("full", ItemKind::AssistantMessage))
            .expect("start");
        s.append_event("t1", None, &delta("w", a)).expect("early");
        s.append_event("t1", None, &completed("full", ItemStatus::Completed))
            .expect("done");
        for env in [
            started("w", ItemKind::AssistantMessage),
            delta("w", b),
            completed("w", ItemStatus::Completed),
        ] {
            s.append_event("t1", None, &env).expect("append");
        }
        assert!(!leaks(&s, &[a, b]));
    }

    /// A tail that no longer matches the store (here a row rewritten behind its back) is not
    /// healed from: the tail heal is undone and the scan runs instead.
    #[test]
    fn a_tail_that_does_not_match_the_store_falls_back_to_the_scan() {
        let s = diff_store(true, 1);
        let secret = format!("ghp_{}", "abcdefghijklmnopqrstuvwxyz0123");
        let (a, b) = secret.split_at(10);
        s.append_event("t1", None, &started("m", ItemKind::AssistantMessage))
            .expect("start");
        let first = s.append_event("t1", None, &delta("m", a)).expect("a");
        s.append_event("t1", None, &delta("m", b)).expect("b");
        // The row the tail knows as the item's first now belongs to another item.
        let other = serde_json::to_string(&delta("other", a)).expect("json");
        s.conn
            .execute(
                "UPDATE events SET envelope_json = ?1 WHERE seq = ?2",
                params![other, first],
            )
            .expect("tamper");
        s.append_event("t1", None, &completed("m", ItemStatus::Completed))
            .expect("done");
        assert_eq!(
            s.heal_paths.get(),
            [1, 1],
            "tried the tail, healed by the scan"
        );
        // The scan saw only `b` for this item: nothing to heal, and nothing half-rewritten.
        let rows = event_rows(&s);
        assert!(rows
            .iter()
            .any(|(seq, _, _, j)| *seq == first && j.contains("\"other\"")));
    }

    #[test]
    fn atomically_nests_transactions_and_takes_everything_back_on_err() {
        let s = store();
        let t = s.create_thread("/", None).expect("t");
        // Methods that open transactions of their own nest inside it.
        s.atomically(|s| {
            s.append_event(&t, None, &Envelope::new(Event::Unknown))?;
            s.append_user_message(&t, None, "hi")?;
            s.dismiss_native("claude", "n1")
        })
        .expect("ok");
        assert_eq!(s.events(&t, None, 10).expect("events").len(), 3);
        let failed = s.atomically(|s| -> Result<()> {
            s.append_event(&t, None, &Envelope::new(Event::Unknown))?;
            s.dismiss_native("claude", "n2")?;
            Err(StoreError::Failed("no".into()))
        });
        assert!(failed.is_err());
        assert_eq!(s.events(&t, None, 10).expect("events").len(), 3);
        assert!(!s.native_dismissed("claude", "n2").expect("q"));
        assert!(s.conn.is_autocommit());
    }

    #[test]
    fn a_batch_keeps_its_good_writes_when_one_fails() {
        let s = store();
        let t = s.create_thread("/", None).expect("t");
        let results = s
            .batch(|s| {
                [
                    s.append_event(&t, None, &Envelope::new(Event::Unknown))
                        .is_ok(),
                    s.append_event("missing", None, &Envelope::new(Event::Unknown))
                        .is_ok(),
                    s.append_user_message(&t, None, "hi").is_ok(),
                    s.delete_thread("missing").is_ok(),
                ]
            })
            .expect("commit");
        assert_eq!(results, [true, false, true, false]);
        assert_eq!(s.events(&t, None, 10).expect("events").len(), 3);
        assert!(s.conn.is_autocommit(), "no transaction left open");
    }

    #[test]
    fn checkpoints_report_the_wal_and_can_be_turned_off() {
        let tmp = tempfile::tempdir().expect("tmp");
        let s = Store::open(&tmp.path().join("threads.db")).expect("open");
        assert_eq!(s.autocheckpoint().expect("pragma"), 1000);
        s.set_autocheckpoint(0).expect("off");
        assert_eq!(s.autocheckpoint().expect("pragma"), 0);
        let t = s.create_thread("/", None).expect("t");
        for _ in 0..50 {
            s.append_event(&t, None, &delta("a", &"x".repeat(2000)))
                .expect("append");
        }
        let before = s.checkpoint().expect("checkpoint");
        assert!(before.wal_frames > 0 && before.wal_bytes >= before.wal_frames * 512);
        assert!(!before.busy);
        assert_eq!(before.checkpointed, before.wal_frames);
    }

    /// The append benchmark (§1 E5, §2 WP4a): run it on a read-only snapshot of a real store,
    /// release build:
    /// `sqlite3 -readonly ~/.local/state/agent-terminal/threads.db ".backup /tmp/x/threads.db"`,
    /// then `AGENT_TERMINAL_BENCH_DB=/tmp/x/threads.db cargo test --release -p agent-kit
    /// append_bench -- --ignored --nocapture`. It appends to the copy's busiest thread.
    #[test]
    #[ignore = "benchmark: needs AGENT_TERMINAL_BENCH_DB, a scratch copy of a real store"]
    fn append_bench() {
        let Some(path) = std::env::var_os("AGENT_TERMINAL_BENCH_DB") else {
            eprintln!("AGENT_TERMINAL_BENCH_DB is not set");
            return;
        };
        let path = PathBuf::from(path);
        for (label, tails) in [("sql scan (master's heal)", false), ("heal tails", true)] {
            let s = Store::open(&path).expect("open the copy");
            s.set_heal_tails(tails);
            let thread: String = s
                .conn
                .query_row(
                    "SELECT thread_id FROM events GROUP BY thread_id ORDER BY COUNT(*) DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .expect("a thread with events");
            let mut deltas = Vec::new();
            let mut done = Vec::new();
            let mut starts = Vec::new();
            for round in 0..200 {
                let item = format!("bench-{label}-{round}");
                let t = Instant::now();
                s.append_event(&thread, None, &started(&item, ItemKind::AssistantMessage))
                    .expect("start");
                starts.push(t.elapsed());
                for n in 0..20 {
                    let t = Instant::now();
                    s.append_event(&thread, None, &delta(&item, &format!("word {n} ")))
                        .expect("delta");
                    deltas.push(t.elapsed());
                }
                let t = Instant::now();
                s.append_event(&thread, None, &completed(&item, ItemStatus::Completed))
                    .expect("complete");
                done.push(t.elapsed());
            }
            done.sort();
            starts.sort();
            let us = |d: Duration| d.as_secs_f64() * 1e6;
            // The start checks the item has no rows yet (a reused id heals by the scan).
            eprintln!(
                "{label}: item_started p50 {:.1} µs, p99 {:.1} µs",
                us(starts[starts.len() / 2]),
                us(starts[starts.len() * 99 / 100]),
            );
            let mean = deltas.iter().map(|d| us(*d)).sum::<f64>() / deltas.len() as f64;
            eprintln!(
                "{label}: content_delta mean {mean:.1} µs; item_completed p50 {:.1} µs, p99 {:.1} µs",
                us(done[done.len() / 2]),
                us(done[done.len() * 99 / 100]),
            );
        }
        // What the main thread pays now: queueing an append for the writer.
        let reader = std::rc::Rc::new(Store::open(&path).expect("open"));
        let writer = crate::store_writer::StoreWriter::spawn(&path).expect("writer");
        let handle = crate::store_writer::StoreHandle::Queued {
            reader,
            writer: writer.clone(),
        };
        let thread = handle.new_id().expect("id");
        let t2 = thread.clone();
        handle
            .write("create", None, move |s| {
                s.create_thread_with_id(&t2, "/", None)
            })
            .expect("queued");
        let mut queued = Vec::new();
        for n in 0..4000 {
            let env = if n % 21 == 20 {
                completed("q", ItemStatus::Completed)
            } else {
                delta("q", "word ")
            };
            let t = Instant::now();
            handle.append_event(&thread, None, &env).expect("queued");
            queued.push(t.elapsed());
        }
        assert!(writer.flush(Duration::from_secs(60)));
        queued.sort();
        eprintln!(
            "queued append on the calling thread: p50 {:.1} µs, p99 {:.1} µs",
            queued[queued.len() / 2].as_secs_f64() * 1e6,
            queued[queued.len() * 99 / 100].as_secs_f64() * 1e6,
        );
        assert_eq!(writer.failures().0, 0);
    }
}
