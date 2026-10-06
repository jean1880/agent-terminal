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

use std::cell::Cell;
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_core::event::{Envelope, Event, ItemKind, ItemStatus, StreamKind};
use agent_core::redact::redact;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

/// Identifier of an app thread (a UUID string).
pub type ThreadId = String;
/// Identifier of a provider thread (a UUID string).
pub type ProviderThreadId = String;

/// How long a writer waits on another connection's lock before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Schema version this build writes. Bump it and add a step to [`migrate`].
const SCHEMA_VERSION: i64 = 1;

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
/// Ceiling: redaction is per string, so a token split across two streaming
/// deltas is not recognized in either. The adapters close each streamed item
/// with a `ContentSnapshot` of its whole text (agy emits one before every
/// response completes; Claude sends its own assistant snapshots), and that is
/// scrubbed whole, so the transcript, which a snapshot replaces, is clean; the
/// raw delta rows may keep the split fragments. Upgrade path: scrub the
/// accumulated text per item at append time.
pub fn scrub_envelope(env: Envelope) -> Result<Envelope> {
    let value = serde_json::to_value(&env)?;
    Ok(serde_json::from_value(scrub_value(value))?)
}

fn scrub_value(v: Value) -> Value {
    match v {
        Value::String(s) => Value::String(redact(&s)),
        Value::Array(a) => Value::Array(a.into_iter().map(scrub_value).collect()),
        Value::Object(o) => {
            Value::Object(o.into_iter().map(|(k, v)| (k, scrub_value(v))).collect())
        }
        other => other,
    }
}

pub struct Store {
    conn: Connection,
    /// Last timestamp handed out, so `updated_at` strictly increases even
    /// within one millisecond (list ordering stays deterministic).
    last_ms: Cell<i64>,
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
        // corrupt) and avoids an fsync per event.
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
        })
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

    fn new_id(&self) -> Result<String> {
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

    fn touch(&self, thread: &str, at: i64) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE threads SET updated_at = ?2 WHERE id = ?1",
            params![thread, at],
        )?;
        require_row(n, thread)
    }

    // ---- threads ----

    pub fn create_thread(&self, cwd: &str, title: Option<&str>) -> Result<ThreadId> {
        let id = self.new_id()?;
        let now = self.now();
        self.conn.execute(
            "INSERT INTO threads (id, title, cwd, created_at, updated_at, archived, read_seq)
             VALUES (?1, ?2, ?3, ?4, ?4, 0, 0)",
            params![id, redact(title.unwrap_or("")), cwd, now],
        )?;
        Ok(id)
    }

    /// Newest first (by `updated_at`).
    pub fn list_threads(&self, include_archived: bool) -> Result<Vec<ThreadSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.title, t.cwd, t.updated_at,
                    p.driver, p.model,
                    EXISTS (SELECT 1 FROM events e WHERE e.thread_id = t.id AND e.seq > t.read_seq)
             FROM threads t
             LEFT JOIN provider_threads p ON p.id = COALESCE(
                 t.active_provider_thread,
                 (SELECT id FROM provider_threads q WHERE q.thread_id = t.id
                  ORDER BY q.created_at DESC, q.rowid DESC LIMIT 1))
             WHERE (?1 OR t.archived = 0)
             ORDER BY t.updated_at DESC, t.rowid DESC",
        )?;
        let rows = stmt.query_map(params![include_archived], |r| {
            Ok(ThreadSummary {
                id: r.get(0)?,
                title: r.get(1)?,
                cwd: r.get(2)?,
                updated_at: r.get(3)?,
                driver: r.get(4)?,
                model: r.get(5)?,
                unread: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
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
        let now = self.now();
        self.conn.execute(
            "INSERT INTO provider_threads (id, thread_id, driver, model, native_id, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?5)",
            params![id, thread, driver, model, now],
        )
        .map_err(|e| fk_or(e, thread))?;
        Ok(id)
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
        // The insert and the `updated_at` bump commit together or not at all.
        let tx = self.conn.unchecked_transaction()?;
        let seq = self.append_in_txn(thread, provider_thread, env)?;
        tx.commit()?;
        Ok(seq)
    }

    /// The body of [`Store::append_event`]; the caller owns the transaction.
    fn append_in_txn(
        &self,
        thread: &str,
        provider_thread: Option<&str>,
        env: &Envelope,
    ) -> Result<i64> {
        let scrubbed = scrub_envelope(env.clone())?;
        let json = serde_json::to_string(&scrubbed)?;
        let now = self.now();
        self.conn
            .execute(
                "INSERT INTO events (thread_id, provider_thread_id, at, envelope_json)
                 VALUES (?1, ?2, ?3, ?4)",
                params![thread, provider_thread, now, json],
            )
            .map_err(|e| fk_or(e, thread))?;
        let seq = self.conn.last_insert_rowid();
        self.touch(thread, now)?;
        Ok(seq)
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
        let started = Envelope::new(Event::ItemStarted {
            kind: ItemKind::UserMessage,
            title: String::new(),
            input: None,
            parent: None,
        })
        .item(item.clone());
        let snapshot = Envelope::new(Event::ContentSnapshot {
            stream: StreamKind::Assistant,
            text: text.to_string(),
        })
        .item(item.clone());
        let tx = self.conn.unchecked_transaction()?;
        self.append_in_txn(thread, provider_thread, &started)?;
        self.append_in_txn(thread, provider_thread, &snapshot)?;
        tx.commit()?;
        Ok(item)
    }

    /// Events after `after_seq` (exclusive), oldest first, at most `limit`.
    pub fn events(
        &self,
        thread: &str,
        after_seq: Option<i64>,
        limit: usize,
    ) -> Result<Vec<(i64, Envelope)>> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, envelope_json FROM events
             WHERE thread_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![
                thread,
                after_seq.unwrap_or(0),
                limit.min(i64::MAX as usize) as i64
            ],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, json) = row?;
            out.push((seq, serde_json::from_str(&json)?));
        }
        Ok(out)
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
        conn.execute_batch(
            "CREATE TABLE threads (
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
             INSERT INTO schema_version (version) VALUES (1);",
        )?;
    }
    Ok(())
}

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
        assert_eq!(rows, 1);
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
    fn migration_is_idempotent_and_refuses_a_newer_schema() {
        let s = store();
        migrate(&s.conn).expect("again");
        migrate(&s.conn).expect("and again");
        let rows: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))
            .expect("count");
        assert_eq!(rows, 1);

        s.conn
            .execute("INSERT INTO schema_version (version) VALUES (99)", [])
            .expect("bump");
        assert!(matches!(
            migrate(&s.conn),
            Err(StoreError::SchemaTooNew { found: 99, .. })
        ));
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
}
