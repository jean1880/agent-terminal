//! Schema v4: the cross-agent delegation tables, and thin row-level helpers over them.
//!
//! v4 is **strictly additive** (contract §8.1): it creates new tables, indexes and triggers on
//! those new tables, and adds a row to `schema_version`. It never alters a v3 table, so a v3
//! writer still running against the file cannot corrupt it.
//!
//! The helpers here are deliberately string-typed (plain `String` / `i64` columns): the typed ids
//! and enums live in `agent_core::delegation`, and the typed store API wraps these rows. The
//! schema itself enforces what SQL can: the enums' value sets, the deferred root ↔ root-worker
//! link, one root worker and at most one active run per worker, the idempotency key, the frozen
//! request/resolution/receipt of an operation, and the set-once facts of a message.
//!
//! Callers pass JSON columns already scrubbed (`canonical_request` is compared byte for byte, so it
//! is stored exactly as given). Free-text columns (names, bodies, reasons, activity) are masked
//! again here with [`redact`], which leaves already-masked text as it is.

use agent_core::delegation::operation::OperationKind;
use agent_core::delegation::wire::Delivery;
use agent_core::redact::redact;
use rusqlite::{params, OptionalExtension, Row};

use super::{Result, Store, StoreError};

/// Run states that hold a worker's single active slot (`delegation_runs_one_active`).
pub const ACTIVE_RUN_STATES: [&str; 4] = ["reserved", "starting", "running", "stopping"];

/// The v4 migration step. Run inside the migration's reserved transaction.
pub(crate) const SCHEMA_V4: &str = "
CREATE TABLE delegation_roots (
    id TEXT PRIMARY KEY,
    root_thread_id TEXT NOT NULL UNIQUE REFERENCES threads(id),
    root_worker_id TEXT NOT NULL,
    task_state TEXT NOT NULL CHECK (task_state IN ('active', 'stopped', 'closed')),
    authority_epoch INTEGER NOT NULL DEFAULT 0 CHECK (authority_epoch >= 0),
    policy_revision INTEGER NOT NULL DEFAULT 0 CHECK (policy_revision >= 0),
    stream_epoch INTEGER NOT NULL DEFAULT 0 CHECK (stream_epoch >= 0),
    policy_json TEXT NOT NULL,
    continuation_budget INTEGER NOT NULL CHECK (continuation_budget >= 0),
    continuation_reserved INTEGER NOT NULL DEFAULT 0 CHECK (continuation_reserved IN (0, 1)),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (id, root_worker_id) REFERENCES delegation_workers(root_id, id)
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE delegation_workers (
    id TEXT PRIMARY KEY,
    root_id TEXT NOT NULL REFERENCES delegation_roots(id),
    parent_id TEXT,
    origin TEXT NOT NULL CHECK (origin IN ('root', 'native', 'supervised')),
    depth INTEGER NOT NULL CHECK (depth BETWEEN 0 AND 2),
    name TEXT NOT NULL,
    driver TEXT NOT NULL CHECK (driver IN ('claude', 'agy', 'codex')),
    profile_id TEXT NOT NULL,
    launch_json TEXT NOT NULL,
    backing_thread_id TEXT UNIQUE REFERENCES threads(id),
    state TEXT NOT NULL CHECK (state IN ('starting', 'running', 'waiting', 'completed', 'failed',
                                         'stopping', 'stopped', 'closed', 'unknown')),
    task_mode TEXT CHECK (task_mode IN ('ask', 'accept_edits', 'plan')),
    workspace_json TEXT NOT NULL,
    identity_json TEXT,
    delegation_status TEXT NOT NULL CHECK (delegation_status IN ('available', 'workspace_untrusted',
        'rebind_required', 'restricted', 'bridge_disabled', 'unavailable')),
    delegation_reason TEXT CHECK (length(CAST(delegation_reason AS BLOB)) <= 256),
    delegation_paths TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(delegation_paths)
        AND json_type(delegation_paths) = 'array' AND json_array_length(delegation_paths) <= 16),
    last_activity TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (root_id, parent_id) REFERENCES delegation_workers(root_id, id),
    CHECK ((origin = 'root') = (parent_id IS NULL)),
    CHECK ((origin = 'root') = (depth = 0)),
    CHECK (origin = 'supervised' OR backing_thread_id IS NULL)
);
CREATE UNIQUE INDEX delegation_workers_root_id ON delegation_workers(root_id, id);
CREATE UNIQUE INDEX delegation_workers_one_root ON delegation_workers(root_id)
    WHERE origin = 'root';
CREATE INDEX delegation_workers_parent ON delegation_workers(root_id, parent_id);
CREATE TRIGGER delegation_workers_depth BEFORE INSERT ON delegation_workers
WHEN NEW.parent_id IS NOT NULL
 AND NEW.depth IS NOT (SELECT depth + 1 FROM delegation_workers WHERE id = NEW.parent_id)
BEGIN SELECT RAISE(ABORT, 'a delegation worker is one level below its parent'); END;
CREATE TRIGGER delegation_workers_lineage BEFORE UPDATE ON delegation_workers
WHEN NEW.id IS NOT OLD.id OR NEW.root_id IS NOT OLD.root_id
  OR NEW.parent_id IS NOT OLD.parent_id OR NEW.origin IS NOT OLD.origin
  OR NEW.depth IS NOT OLD.depth OR NEW.created_at IS NOT OLD.created_at
  OR (OLD.backing_thread_id IS NOT NULL AND NEW.backing_thread_id IS NOT OLD.backing_thread_id)
BEGIN SELECT RAISE(ABORT, 'a delegation worker''s lineage is immutable'); END;

CREATE TABLE delegation_runs (
    id TEXT PRIMARY KEY,
    worker_id TEXT NOT NULL REFERENCES delegation_workers(id),
    process_generation INTEGER NOT NULL CHECK (process_generation >= 0),
    provider_thread_id TEXT REFERENCES provider_threads(id) ON DELETE SET NULL,
    native_turn_id TEXT,
    authority_epoch INTEGER NOT NULL CHECK (authority_epoch >= 0),
    state TEXT NOT NULL CHECK (state IN ('reserved', 'starting', 'running', 'stopping',
                                         'completed', 'interrupted', 'failed', 'unknown')),
    terminal_reason TEXT,
    usage_json TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX delegation_runs_one_active ON delegation_runs(worker_id)
    WHERE state IN ('reserved', 'starting', 'running', 'stopping');
CREATE INDEX delegation_runs_worker ON delegation_runs(worker_id, created_at);
CREATE TRIGGER delegation_runs_frozen BEFORE UPDATE ON delegation_runs
WHEN NEW.id IS NOT OLD.id OR NEW.worker_id IS NOT OLD.worker_id
  OR NEW.process_generation IS NOT OLD.process_generation
  OR NEW.authority_epoch IS NOT OLD.authority_epoch OR NEW.created_at IS NOT OLD.created_at
  OR (OLD.state IN ('completed', 'interrupted', 'failed') AND NEW.state IS NOT OLD.state)
BEGIN SELECT RAISE(ABORT, 'a delegation run''s identity and terminal state are final'); END;

CREATE TABLE delegation_operations (
    id TEXT PRIMARY KEY,
    root_id TEXT NOT NULL REFERENCES delegation_roots(id),
    principal_worker_id TEXT NOT NULL REFERENCES delegation_workers(id),
    kind TEXT NOT NULL CHECK (length(kind) BETWEEN 1 AND 32),
    idempotency_key TEXT NOT NULL CHECK (length(idempotency_key) BETWEEN 1 AND 128),
    authority_epoch INTEGER NOT NULL CHECK (authority_epoch >= 0),
    canonical_request TEXT NOT NULL,
    resolution_json TEXT NOT NULL,
    receipt_json TEXT NOT NULL,
    dispatch_state TEXT NOT NULL CHECK (dispatch_state IN ('accepted', 'dispatch_reserved',
        'submitted', 'confirmed', 'failed', 'unknown')),
    error_json TEXT,
    target_worker_id TEXT REFERENCES delegation_workers(id),
    run_id TEXT REFERENCES delegation_runs(id),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (principal_worker_id, kind, idempotency_key),
    CHECK (dispatch_state <> 'failed' OR error_json IS NOT NULL)
);
CREATE INDEX delegation_operations_root ON delegation_operations(root_id, created_at);
CREATE TRIGGER delegation_operations_frozen BEFORE UPDATE ON delegation_operations
WHEN NEW.id IS NOT OLD.id OR NEW.root_id IS NOT OLD.root_id
  OR NEW.principal_worker_id IS NOT OLD.principal_worker_id OR NEW.kind IS NOT OLD.kind
  OR NEW.idempotency_key IS NOT OLD.idempotency_key
  OR NEW.authority_epoch IS NOT OLD.authority_epoch
  OR NEW.canonical_request IS NOT OLD.canonical_request
  OR NEW.resolution_json IS NOT OLD.resolution_json
  OR NEW.receipt_json IS NOT OLD.receipt_json
  OR NEW.target_worker_id IS NOT OLD.target_worker_id OR NEW.created_at IS NOT OLD.created_at
  OR (OLD.run_id IS NOT NULL AND NEW.run_id IS NOT OLD.run_id)
BEGIN SELECT RAISE(ABORT, 'a delegation operation''s request, resolution and receipt are frozen');
END;

CREATE TABLE delegation_messages (
    id TEXT PRIMARY KEY,
    root_id TEXT NOT NULL REFERENCES delegation_roots(id),
    operation_id TEXT REFERENCES delegation_operations(id),
    sender_worker_id TEXT NOT NULL REFERENCES delegation_workers(id),
    recipient_worker_id TEXT NOT NULL REFERENCES delegation_workers(id),
    kind TEXT NOT NULL CHECK (kind IN ('instruction', 'report', 'question', 'reply')),
    reply_to TEXT REFERENCES delegation_messages(id),
    body TEXT NOT NULL,
    delivery_policy TEXT NOT NULL CHECK (length(delivery_policy) BETWEEN 1 AND 32),
    target_run_id TEXT REFERENCES delegation_runs(id),
    delivery_state TEXT NOT NULL CHECK (delivery_state IN ('queued', 'queued_next_turn',
        'dispatch_reserved', 'submitted', 'provider_confirmed', 'misdelivered', 'failed',
        'cancelled', 'unknown')),
    recipient_run_id TEXT REFERENCES delegation_runs(id),
    queued_at INTEGER NOT NULL,
    submitted_at INTEGER,
    confirmed_at INTEGER,
    presented_at INTEGER,
    acknowledged_at INTEGER,
    updated_at INTEGER NOT NULL,
    event_seq INTEGER UNIQUE REFERENCES events(seq)
);
CREATE INDEX delegation_messages_recipient ON delegation_messages(recipient_worker_id, queued_at);
CREATE INDEX delegation_messages_mailbox
    ON delegation_messages(recipient_worker_id, event_seq, root_id);
CREATE INDEX delegation_messages_root ON delegation_messages(root_id, queued_at);
CREATE INDEX delegation_messages_operation ON delegation_messages(operation_id);
CREATE TRIGGER delegation_messages_frozen BEFORE UPDATE ON delegation_messages
WHEN NEW.id IS NOT OLD.id OR NEW.root_id IS NOT OLD.root_id
  OR NEW.operation_id IS NOT OLD.operation_id
  OR NEW.sender_worker_id IS NOT OLD.sender_worker_id
  OR NEW.recipient_worker_id IS NOT OLD.recipient_worker_id OR NEW.kind IS NOT OLD.kind
  OR NEW.reply_to IS NOT OLD.reply_to OR NEW.body IS NOT OLD.body
  OR NEW.delivery_policy IS NOT OLD.delivery_policy
  OR NEW.target_run_id IS NOT OLD.target_run_id OR NEW.queued_at IS NOT OLD.queued_at
  OR NEW.event_seq IS NOT OLD.event_seq
  OR (OLD.recipient_run_id IS NOT NULL AND NEW.recipient_run_id IS NOT OLD.recipient_run_id)
  OR (OLD.presented_at IS NOT NULL AND NEW.presented_at IS NOT OLD.presented_at)
  OR (OLD.delivery_state IN ('provider_confirmed', 'misdelivered', 'failed', 'cancelled')
      AND NEW.delivery_state IS NOT OLD.delivery_state)
BEGIN SELECT RAISE(ABORT, 'a delegation message''s accepted fields and set-once facts are final');
END;

CREATE TABLE delegation_event_links (
    event_seq INTEGER NOT NULL REFERENCES events(seq),
    worker_id TEXT NOT NULL REFERENCES delegation_workers(id),
    root_id TEXT NOT NULL REFERENCES delegation_roots(id),
    run_id TEXT REFERENCES delegation_runs(id),
    category TEXT NOT NULL CHECK (category IN ('lifecycle', 'activity', 'result', 'approval',
                                               'message', 'error')),
    invocation_id TEXT,
    PRIMARY KEY (event_seq, worker_id)
) WITHOUT ROWID;
CREATE INDEX delegation_event_links_root ON delegation_event_links(root_id, event_seq);
CREATE INDEX delegation_event_links_worker ON delegation_event_links(worker_id, event_seq);

CREATE TABLE delegation_native_aliases (
    provider_thread_id TEXT NOT NULL REFERENCES provider_threads(id),
    process_generation INTEGER NOT NULL CHECK (process_generation >= 0),
    native_worker_id TEXT NOT NULL,
    worker_id TEXT NOT NULL REFERENCES delegation_workers(id),
    created_at INTEGER NOT NULL,
    PRIMARY KEY (provider_thread_id, process_generation, native_worker_id)
) WITHOUT ROWID;
CREATE INDEX delegation_native_aliases_worker ON delegation_native_aliases(worker_id);

INSERT INTO schema_version (version) VALUES (4);
";

/// The tables v4 adds, in creation order.
pub const V4_TABLES: [&str; 7] = [
    "delegation_roots",
    "delegation_workers",
    "delegation_runs",
    "delegation_operations",
    "delegation_messages",
    "delegation_event_links",
    "delegation_native_aliases",
];

/// One `delegation_roots` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootRow {
    pub id: String,
    pub root_thread_id: String,
    pub root_worker_id: String,
    /// `active`, `stopped` or `closed`.
    pub task_state: String,
    pub authority_epoch: i64,
    pub policy_revision: i64,
    pub stream_epoch: i64,
    /// Sanitised policy JSON. No bearer capability is ever stored.
    pub policy_json: String,
    pub continuation_budget: i64,
    pub continuation_reserved: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One `delegation_workers` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerRow {
    pub id: String,
    pub root_id: String,
    /// `None` exactly for the root worker.
    pub parent_id: Option<String>,
    /// `root`, `native` or `supervised`.
    pub origin: String,
    pub depth: i64,
    pub name: String,
    pub driver: String,
    pub profile_id: String,
    /// The safe launch snapshot (JSON): never tokens or an environment snapshot.
    pub launch_json: String,
    /// The hidden thread of a supervised child (absent from the sidebar, open-thread restoration
    /// and native import). Only `supervised` workers have one.
    pub backing_thread_id: Option<String>,
    pub state: String,
    pub task_mode: Option<String>,
    pub workspace_json: String,
    pub identity_json: Option<String>,
    pub delegation_status: String,
    pub delegation_reason: Option<String>,
    /// JSON array of at most 16 relative paths.
    pub delegation_paths: String,
    pub last_activity: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One `delegation_runs` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRow {
    pub id: String,
    pub worker_id: String,
    pub process_generation: i64,
    pub provider_thread_id: Option<String>,
    pub native_turn_id: Option<String>,
    pub authority_epoch: i64,
    pub state: String,
    pub terminal_reason: Option<String>,
    pub usage_json: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One `delegation_operations` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationRow {
    pub id: String,
    pub root_id: String,
    pub principal_worker_id: String,
    pub kind: String,
    pub idempotency_key: String,
    /// The epoch the operation was accepted under.
    pub authority_epoch: i64,
    /// Scrubbed, syntax-normalised sorted-key JSON, compared byte for byte (contract §2).
    pub canonical_request: String,
    /// Target worker, bound runs and base commit, fixed at acceptance.
    pub resolution_json: String,
    /// The original success receipt: written once at acceptance, never updated.
    pub receipt_json: String,
    pub dispatch_state: String,
    /// The scrubbed `ToolError` of a `failed` operation.
    pub error_json: Option<String>,
    pub target_worker_id: Option<String>,
    pub run_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One `delegation_messages` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRow {
    pub id: String,
    pub root_id: String,
    pub operation_id: Option<String>,
    pub sender_worker_id: String,
    pub recipient_worker_id: String,
    pub kind: String,
    pub reply_to: Option<String>,
    pub body: String,
    pub delivery_policy: String,
    /// Frozen at acceptance.
    pub target_run_id: Option<String>,
    pub delivery_state: String,
    /// The run in which consumption was observed; set once.
    pub recipient_run_id: Option<String>,
    pub queued_at: i64,
    pub submitted_at: Option<i64>,
    pub confirmed_at: Option<i64>,
    /// The first page that returned it to its recipient (ACK eligibility); set once.
    pub presented_at: Option<i64>,
    pub acknowledged_at: Option<i64>,
    pub updated_at: i64,
    /// The seq of its `DelegatedMessage` event (its page unit), frozen at insert. Indexed with
    /// the recipient, so a mailbox read walks only the caller's own incoming messages.
    pub event_seq: Option<i64>,
}

/// One `delegation_event_links` row: event `event_seq` concerns `worker_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventLinkRow {
    pub event_seq: i64,
    pub worker_id: String,
    pub root_id: String,
    pub run_id: Option<String>,
    pub category: String,
    pub invocation_id: Option<String>,
    /// Canonical process generation of this event; 0 is unknown for legacy links.
    pub process_generation: i64,
}

/// One `delegation_native_aliases` row: a provider's native worker id → an app worker handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeAliasRow {
    pub provider_thread_id: String,
    pub process_generation: i64,
    pub native_worker_id: String,
    pub worker_id: String,
    pub created_at: i64,
}

const ROOT_COLS: &str = "id, root_thread_id, root_worker_id, task_state, authority_epoch,
    policy_revision, stream_epoch, policy_json, continuation_budget, continuation_reserved,
    created_at, updated_at";
const WORKER_COLS: &str = "id, root_id, parent_id, origin, depth, name, driver, profile_id,
    launch_json, backing_thread_id, state, task_mode, workspace_json, identity_json,
    delegation_status, delegation_reason, delegation_paths, last_activity, created_at, updated_at";
const RUN_COLS: &str = "id, worker_id, process_generation, provider_thread_id, native_turn_id,
    authority_epoch, state, terminal_reason, usage_json, created_at, updated_at";
const OPERATION_COLS: &str = "id, root_id, principal_worker_id, kind, idempotency_key,
    authority_epoch, canonical_request, resolution_json, receipt_json, dispatch_state, error_json,
    target_worker_id, run_id, created_at, updated_at";
const MESSAGE_COLS: &str = "id, root_id, operation_id, sender_worker_id, recipient_worker_id,
    kind, reply_to, body, delivery_policy, target_run_id, delivery_state, recipient_run_id,
    queued_at, submitted_at, confirmed_at, presented_at, acknowledged_at, updated_at, event_seq";
const LINK_COLS: &str =
    "event_seq, worker_id, root_id, run_id, category, invocation_id, process_generation";

fn root_row(r: &Row<'_>) -> rusqlite::Result<RootRow> {
    Ok(RootRow {
        id: r.get(0)?,
        root_thread_id: r.get(1)?,
        root_worker_id: r.get(2)?,
        task_state: r.get(3)?,
        authority_epoch: r.get(4)?,
        policy_revision: r.get(5)?,
        stream_epoch: r.get(6)?,
        policy_json: r.get(7)?,
        continuation_budget: r.get(8)?,
        continuation_reserved: r.get(9)?,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
    })
}

fn worker_row(r: &Row<'_>) -> rusqlite::Result<WorkerRow> {
    Ok(WorkerRow {
        id: r.get(0)?,
        root_id: r.get(1)?,
        parent_id: r.get(2)?,
        origin: r.get(3)?,
        depth: r.get(4)?,
        name: r.get(5)?,
        driver: r.get(6)?,
        profile_id: r.get(7)?,
        launch_json: r.get(8)?,
        backing_thread_id: r.get(9)?,
        state: r.get(10)?,
        task_mode: r.get(11)?,
        workspace_json: r.get(12)?,
        identity_json: r.get(13)?,
        delegation_status: r.get(14)?,
        delegation_reason: r.get(15)?,
        delegation_paths: r.get(16)?,
        last_activity: r.get(17)?,
        created_at: r.get(18)?,
        updated_at: r.get(19)?,
    })
}

fn run_row(r: &Row<'_>) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        id: r.get(0)?,
        worker_id: r.get(1)?,
        process_generation: r.get(2)?,
        provider_thread_id: r.get(3)?,
        native_turn_id: r.get(4)?,
        authority_epoch: r.get(5)?,
        state: r.get(6)?,
        terminal_reason: r.get(7)?,
        usage_json: r.get(8)?,
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
    })
}

fn operation_row(r: &Row<'_>) -> rusqlite::Result<OperationRow> {
    Ok(OperationRow {
        id: r.get(0)?,
        root_id: r.get(1)?,
        principal_worker_id: r.get(2)?,
        kind: r.get(3)?,
        idempotency_key: r.get(4)?,
        authority_epoch: r.get(5)?,
        canonical_request: r.get(6)?,
        resolution_json: r.get(7)?,
        receipt_json: r.get(8)?,
        dispatch_state: r.get(9)?,
        error_json: r.get(10)?,
        target_worker_id: r.get(11)?,
        run_id: r.get(12)?,
        created_at: r.get(13)?,
        updated_at: r.get(14)?,
    })
}

fn message_row(r: &Row<'_>) -> rusqlite::Result<MessageRow> {
    Ok(MessageRow {
        id: r.get(0)?,
        root_id: r.get(1)?,
        operation_id: r.get(2)?,
        sender_worker_id: r.get(3)?,
        recipient_worker_id: r.get(4)?,
        kind: r.get(5)?,
        reply_to: r.get(6)?,
        body: r.get(7)?,
        delivery_policy: r.get(8)?,
        target_run_id: r.get(9)?,
        delivery_state: r.get(10)?,
        recipient_run_id: r.get(11)?,
        queued_at: r.get(12)?,
        submitted_at: r.get(13)?,
        confirmed_at: r.get(14)?,
        presented_at: r.get(15)?,
        acknowledged_at: r.get(16)?,
        updated_at: r.get(17)?,
        event_seq: r.get(18)?,
    })
}

fn link_row(r: &Row<'_>) -> rusqlite::Result<EventLinkRow> {
    Ok(EventLinkRow {
        event_seq: r.get(0)?,
        worker_id: r.get(1)?,
        root_id: r.get(2)?,
        run_id: r.get(3)?,
        category: r.get(4)?,
        invocation_id: r.get(5)?,
        process_generation: r.get(6)?,
    })
}

fn masked(text: Option<&str>) -> Option<String> {
    text.map(redact)
}

fn scrubbed_json(text: &str) -> Result<String> {
    let value = serde_json::from_str(text)?;
    Ok(serde_json::to_string(&super::scrub_value(None, value))?)
}

/// Refuses a value that is not one of the P1 enum's (`operations.kind` and
/// `messages.delivery_policy` carry only length CHECKs in SQL, so a later rename never forces a
/// table rebuild; the value set is enforced here, on write).
fn known<T: serde::de::DeserializeOwned>(what: &str, value: &str) -> Result<()> {
    serde_json::from_value::<T>(serde_json::Value::String(value.to_owned()))
        .map(drop)
        .map_err(|_| StoreError::Failed(format!("`{value}` is not a known {what}")))
}

impl Store {
    // ---- roots and workers ----

    /// Creates a root and its root worker as one unit (contract: the root row first, then its
    /// worker, both links checked before the unit is released). The root's deferred link to its
    /// worker is checked here, not left to the outer COMMIT, so inside a writer batch a broken
    /// pair fails alone instead of taking the whole batch down.
    pub fn insert_delegation_root(&self, root: &RootRow, root_worker: &WorkerRow) -> Result<()> {
        if root_worker.origin != "root" || root_worker.id != root.root_worker_id {
            return Err(StoreError::Failed(
                "a root's worker must be its own root-origin worker".into(),
            ));
        }
        self.atomically(|s| {
            s.conn.execute(
                &format!(
                    "INSERT INTO delegation_roots ({ROOT_COLS})
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
                ),
                params![
                    root.id,
                    root.root_thread_id,
                    root.root_worker_id,
                    root.task_state,
                    root.authority_epoch,
                    root.policy_revision,
                    root.stream_epoch,
                    root.policy_json,
                    root.continuation_budget,
                    root.continuation_reserved,
                    root.created_at,
                    root.updated_at
                ],
            )?;
            s.insert_delegation_worker(root_worker)?;
            s.check_root_links(&root.id)
        })
    }

    /// The deferred root ↔ root-worker link of `root`, checked now.
    fn check_root_links(&self, root: &str) -> Result<()> {
        let dangling: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM delegation_roots r WHERE r.id = ?1 AND NOT EXISTS (
                 SELECT 1 FROM delegation_workers w
                 WHERE w.root_id = r.id AND w.id = r.root_worker_id AND w.origin = 'root'))",
            params![root],
            |r| r.get(0),
        )?;
        if dangling {
            return Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY),
                Some(format!("delegation root {root} has no root worker")),
            )));
        }
        Ok(())
    }

    /// Inserts one worker. A supervised child's backing thread must already exist.
    pub fn insert_delegation_worker(&self, w: &WorkerRow) -> Result<()> {
        self.conn.execute(
            &format!(
                "INSERT INTO delegation_workers ({WORKER_COLS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                         ?17, ?18, ?19, ?20)"
            ),
            params![
                w.id,
                w.root_id,
                w.parent_id,
                w.origin,
                w.depth,
                redact(&w.name),
                w.driver,
                w.profile_id,
                scrubbed_json(&w.launch_json)?,
                w.backing_thread_id,
                w.state,
                w.task_mode,
                scrubbed_json(&w.workspace_json)?,
                w.identity_json.as_deref().map(scrubbed_json).transpose()?,
                w.delegation_status,
                masked(w.delegation_reason.as_deref()),
                scrubbed_json(&w.delegation_paths)?,
                masked(w.last_activity.as_deref()),
                w.created_at,
                w.updated_at
            ],
        )?;
        Ok(())
    }

    pub fn delegation_root(&self, id: &str) -> Result<Option<RootRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {ROOT_COLS} FROM delegation_roots WHERE id = ?1"),
                params![id],
                root_row,
            )
            .optional()?)
    }

    /// The root whose root thread is `thread`, if any.
    pub fn delegation_root_of_thread(&self, thread: &str) -> Result<Option<RootRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {ROOT_COLS} FROM delegation_roots WHERE root_thread_id = ?1"),
                params![thread],
                root_row,
            )
            .optional()?)
    }

    pub fn delegation_worker(&self, id: &str) -> Result<Option<WorkerRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {WORKER_COLS} FROM delegation_workers WHERE id = ?1"),
                params![id],
                worker_row,
            )
            .optional()?)
    }

    /// Every worker of a root, oldest first.
    pub fn delegation_workers_of_root(&self, root: &str) -> Result<Vec<WorkerRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {WORKER_COLS} FROM delegation_workers WHERE root_id = ?1
             ORDER BY created_at, rowid"
        ))?;
        let rows = stmt.query_map(params![root], worker_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Whether `thread` is the hidden backing thread of a supervised child.
    pub fn is_hidden_child_thread(&self, thread: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM delegation_workers WHERE backing_thread_id = ?1)",
            params![thread],
            |r| r.get(0),
        )?)
    }

    /// Updates a worker's session state and last activity.
    pub fn set_delegation_worker_state(
        &self,
        id: &str,
        state: &str,
        last_activity: Option<&str>,
        at: i64,
    ) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE delegation_workers SET state = ?2,
                 last_activity = COALESCE(?3, last_activity), updated_at = ?4
             WHERE id = ?1",
            params![id, state, masked(last_activity), at],
        )?;
        super::require_row(n, id)
    }

    /// Records identity observations without changing launch configuration or authority.
    /// Caller uses the store writer; every string is scrubbed before persistence.
    pub fn set_delegation_worker_identity(
        &self,
        id: &str,
        identity: &agent_core::delegation::identity::Identity,
    ) -> Result<()> {
        let value = super::scrub_value(None, serde_json::to_value(identity)?);
        let json = serde_json::to_string(&value)?;
        let n = self.conn.execute(
            "UPDATE delegation_workers SET identity_json = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, json, self.now()],
        )?;
        super::require_row(n, id)
    }

    /// Rebinds a principal to its selected profile. Identity observations from the previous
    /// provider are invalidated; native IDs and credentials are never supplied here.
    pub fn set_delegation_worker_profile(
        &self,
        id: &str,
        driver: agent_core::adapter::Driver,
        profile_id: &str,
    ) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE delegation_workers SET driver = ?2, profile_id = ?3,
             identity_json = NULL, updated_at = ?4 WHERE id = ?1",
            params![id, driver.info().key, profile_id, self.now()],
        )?;
        super::require_row(n, id)
    }

    /// Updates a worker's delegation status (one of the 6 codes), its reason and its paths (a
    /// JSON array of at most 16 relative paths).
    pub fn set_delegation_worker_status(
        &self,
        id: &str,
        status: &str,
        reason: Option<&str>,
        paths: &str,
        at: i64,
    ) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE delegation_workers SET delegation_status = ?2, delegation_reason = ?3,
                 delegation_paths = ?4, updated_at = ?5
             WHERE id = ?1",
            params![id, status, masked(reason), paths, at],
        )?;
        super::require_row(n, id)
    }

    // ---- runs ----

    /// Inserts a run. A second active run of the same worker is refused by
    /// `delegation_runs_one_active` (a constraint error).
    pub fn insert_delegation_run(&self, run: &RunRow) -> Result<()> {
        self.conn.execute(
            &format!(
                "INSERT INTO delegation_runs ({RUN_COLS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"
            ),
            params![
                run.id,
                run.worker_id,
                run.process_generation,
                run.provider_thread_id,
                run.native_turn_id,
                run.authority_epoch,
                run.state,
                masked(run.terminal_reason.as_deref()),
                run.usage_json,
                run.created_at,
                run.updated_at
            ],
        )?;
        Ok(())
    }

    pub fn delegation_run(&self, id: &str) -> Result<Option<RunRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {RUN_COLS} FROM delegation_runs WHERE id = ?1"),
                params![id],
                run_row,
            )
            .optional()?)
    }

    /// The worker's active run ([`ACTIVE_RUN_STATES`]), if any.
    pub fn active_delegation_run(&self, worker: &str) -> Result<Option<RunRow>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {RUN_COLS} FROM delegation_runs WHERE worker_id = ?1
                     AND state IN ('reserved', 'starting', 'running', 'stopping')"
                ),
                params![worker],
                run_row,
            )
            .optional()?)
    }

    /// Moves a run to `state`. A terminal run never changes again (trigger).
    pub fn set_delegation_run_state(
        &self,
        id: &str,
        state: &str,
        native_turn_id: Option<&str>,
        terminal_reason: Option<&str>,
        usage_json: Option<&str>,
        at: i64,
    ) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE delegation_runs SET state = ?2,
                 native_turn_id = COALESCE(?3, native_turn_id),
                 terminal_reason = COALESCE(?4, terminal_reason),
                 usage_json = COALESCE(?5, usage_json), updated_at = ?6
             WHERE id = ?1",
            params![
                id,
                state,
                native_turn_id,
                masked(terminal_reason),
                usage_json,
                at
            ],
        )?;
        super::require_row(n, id)
    }

    // ---- operations ----

    /// Inserts an accepted operation. The same (principal, kind, key) twice is a constraint error:
    /// look it up first with [`Store::delegation_operation_by_key`].
    pub fn insert_delegation_operation(&self, op: &OperationRow) -> Result<()> {
        known::<OperationKind>("operation kind", &op.kind)?;
        self.conn.execute(
            &format!(
                "INSERT INTO delegation_operations ({OPERATION_COLS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)"
            ),
            params![
                op.id,
                op.root_id,
                op.principal_worker_id,
                op.kind,
                op.idempotency_key,
                op.authority_epoch,
                op.canonical_request,
                op.resolution_json,
                op.receipt_json,
                op.dispatch_state,
                op.error_json,
                op.target_worker_id,
                op.run_id,
                op.created_at,
                op.updated_at
            ],
        )?;
        Ok(())
    }

    pub fn delegation_operation(&self, id: &str) -> Result<Option<OperationRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {OPERATION_COLS} FROM delegation_operations WHERE id = ?1"),
                params![id],
                operation_row,
            )
            .optional()?)
    }

    /// The operation stored under an idempotency key, across all epochs (contract §2).
    pub fn delegation_operation_by_key(
        &self,
        principal_worker: &str,
        kind: &str,
        idempotency_key: &str,
    ) -> Result<Option<OperationRow>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {OPERATION_COLS} FROM delegation_operations
                     WHERE principal_worker_id = ?1 AND kind = ?2 AND idempotency_key = ?3"
                ),
                params![principal_worker, kind, idempotency_key],
                operation_row,
            )
            .optional()?)
    }

    /// Records a dispatch fact. The receipt and request are frozen (trigger); only the dispatch
    /// state, the error of a `failed` operation and a not-yet-set run may change.
    pub fn set_delegation_dispatch_state(
        &self,
        id: &str,
        dispatch_state: &str,
        error_json: Option<&str>,
        run_id: Option<&str>,
        at: i64,
    ) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE delegation_operations SET dispatch_state = ?2,
                 error_json = COALESCE(?3, error_json), run_id = COALESCE(run_id, ?4),
                 updated_at = ?5
             WHERE id = ?1",
            params![id, dispatch_state, error_json, run_id, at],
        )?;
        super::require_row(n, id)
    }

    // ---- messages ----

    pub fn insert_delegation_message(&self, m: &MessageRow) -> Result<()> {
        known::<Delivery>("delivery policy", &m.delivery_policy)?;
        self.conn.execute(
            &format!(
                "INSERT INTO delegation_messages ({MESSAGE_COLS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                         ?17, ?18, ?19)"
            ),
            params![
                m.id,
                m.root_id,
                m.operation_id,
                m.sender_worker_id,
                m.recipient_worker_id,
                m.kind,
                m.reply_to,
                redact(&m.body),
                m.delivery_policy,
                m.target_run_id,
                m.delivery_state,
                m.recipient_run_id,
                m.queued_at,
                m.submitted_at,
                m.confirmed_at,
                m.presented_at,
                m.acknowledged_at,
                m.updated_at,
                m.event_seq
            ],
        )?;
        Ok(())
    }

    pub fn delegation_message(&self, id: &str) -> Result<Option<MessageRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {MESSAGE_COLS} FROM delegation_messages WHERE id = ?1"),
                params![id],
                message_row,
            )
            .optional()?)
    }

    /// Records a delivery fact: the new state, and the run consumption was observed in (set once;
    /// a different run later is refused by the trigger).
    pub fn set_delegation_message_delivery(
        &self,
        id: &str,
        delivery_state: &str,
        recipient_run_id: Option<&str>,
        at: i64,
    ) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE delegation_messages SET delivery_state = ?2,
                 recipient_run_id = COALESCE(?3, recipient_run_id),
                 submitted_at = CASE WHEN ?2 = 'submitted' THEN COALESCE(submitted_at, ?4)
                                     ELSE submitted_at END,
                 confirmed_at = CASE WHEN ?2 IN ('provider_confirmed', 'misdelivered')
                                     THEN COALESCE(confirmed_at, ?4) ELSE confirmed_at END,
                 updated_at = ?4
             WHERE id = ?1",
            params![id, delivery_state, recipient_run_id, at],
        )?;
        super::require_row(n, id)
    }

    /// Marks a message presented to its recipient (the first time only); whether this call did.
    pub fn mark_delegation_message_presented(&self, id: &str, at: i64) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE delegation_messages SET presented_at = ?2, updated_at = ?2
             WHERE id = ?1 AND presented_at IS NULL",
            params![id, at],
        )?;
        Ok(n == 1)
    }

    // ---- event links and native aliases ----

    /// Last committed transcript event, for a replay/live subscription fence. The caller must
    /// read behind the writer's barrier; new events have strictly greater sequence numbers.
    pub fn delegation_event_high_water(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))?)
    }

    pub fn insert_delegation_event_link(&self, link: &EventLinkRow) -> Result<()> {
        self.conn.execute(
            &format!(
                "INSERT INTO delegation_event_links ({LINK_COLS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
            ),
            params![
                link.event_seq,
                link.worker_id,
                link.root_id,
                link.run_id,
                link.category,
                link.invocation_id,
                link.process_generation
            ],
        )?;
        Ok(())
    }

    /// The workers one event concerns.
    pub fn delegation_event_links(&self, event_seq: i64) -> Result<Vec<EventLinkRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {LINK_COLS} FROM delegation_event_links WHERE event_seq = ?1 ORDER BY worker_id"
        ))?;
        let rows = stmt.query_map(params![event_seq], link_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn insert_delegation_native_alias(&self, alias: &NativeAliasRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO delegation_native_aliases
                 (provider_thread_id, process_generation, native_worker_id, worker_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                alias.provider_thread_id,
                alias.process_generation,
                alias.native_worker_id,
                alias.worker_id,
                alias.created_at
            ],
        )?;
        Ok(())
    }

    /// The app worker a native worker id stands for, within one provider thread and generation.
    pub fn delegation_native_alias(
        &self,
        provider_thread: &str,
        process_generation: i64,
        native_worker_id: &str,
    ) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT worker_id FROM delegation_native_aliases
                 WHERE provider_thread_id = ?1 AND process_generation = ?2
                   AND native_worker_id = ?3",
                params![provider_thread, process_generation, native_worker_id],
                |r| r.get(0),
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::event::{Envelope, Event};

    fn root(id: &str, thread: &str) -> RootRow {
        RootRow {
            id: id.into(),
            root_thread_id: thread.into(),
            root_worker_id: format!("w_{id}root"),
            task_state: "active".into(),
            authority_epoch: 0,
            policy_revision: 0,
            stream_epoch: 0,
            policy_json: "{}".into(),
            continuation_budget: 3,
            continuation_reserved: false,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn worker(id: &str, root: &str, parent: Option<&str>, origin: &str, depth: i64) -> WorkerRow {
        WorkerRow {
            id: id.into(),
            root_id: root.into(),
            parent_id: parent.map(str::to_owned),
            origin: origin.into(),
            depth,
            name: format!("worker {id}"),
            driver: "codex".into(),
            profile_id: "codex-default".into(),
            launch_json: "{}".into(),
            backing_thread_id: None,
            state: "running".into(),
            task_mode: Some("plan".into()),
            workspace_json: "{\"kind\":\"shared_read_only\"}".into(),
            identity_json: None,
            delegation_status: "available".into(),
            delegation_reason: None,
            delegation_paths: "[]".into(),
            last_activity: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn run(id: &str, worker: &str, state: &str) -> RunRow {
        RunRow {
            id: id.into(),
            worker_id: worker.into(),
            process_generation: 1,
            provider_thread_id: None,
            native_turn_id: None,
            authority_epoch: 0,
            state: state.into(),
            terminal_reason: None,
            usage_json: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    /// A store with a root (`ro`, thread `root_thread`) and one supervised child `w_child` backed
    /// by `child_thread`.
    struct Tree {
        s: Store,
        root_thread: String,
        child_thread: String,
    }

    fn tree() -> Tree {
        let s = Store::open_in_memory().expect("store");
        let root_thread = s.create_thread("/w", Some("root")).expect("t");
        let child_thread = s.create_thread("/w", Some("child")).expect("t");
        let r = root("ro", &root_thread);
        let rw = worker(&r.root_worker_id, "ro", None, "root", 0);
        s.insert_delegation_root(&r, &rw).expect("root");
        let mut child = worker("w_child", "ro", Some(&r.root_worker_id), "supervised", 1);
        child.backing_thread_id = Some(child_thread.clone());
        s.insert_delegation_worker(&child).expect("child");
        Tree {
            s,
            root_thread,
            child_thread,
        }
    }

    fn is_constraint(e: &StoreError) -> bool {
        matches!(e, StoreError::Sqlite(rusqlite::Error::SqliteFailure(f, _))
            if f.code == rusqlite::ErrorCode::ConstraintViolation)
    }

    #[test]
    fn identity_observations_are_scrubbed_and_require_an_existing_worker() {
        use agent_core::adapter::Driver;
        use agent_core::delegation::identity::{Identity, Provenance, ValueProvenance};
        use agent_core::delegation::ProfileId;
        let tree = tree();
        let mut identity = Identity::new(Driver::Codex, ProfileId::parse("codex-default").unwrap());
        let canary = format!("{}{}", "ghp_", "x".repeat(36));
        identity.model = ValueProvenance {
            requested: Some(canary.clone()),
            effective: None,
            source: Provenance::LaunchConfigured,
        };
        tree.s
            .set_delegation_worker_identity("w_child", &identity)
            .unwrap();
        let stored = tree
            .s
            .delegation_worker("w_child")
            .unwrap()
            .unwrap()
            .identity_json
            .unwrap();
        assert!(
            !stored.contains(&canary),
            "identity must not persist unmasked credentials"
        );
        let back: Identity = serde_json::from_str(&stored).unwrap();
        assert_eq!(back.model.source, Provenance::LaunchConfigured);
        assert_eq!(back.model.effective, None);
        assert!(tree
            .s
            .set_delegation_worker_identity("missing", &identity)
            .is_err());
        tree.s
            .set_delegation_worker_profile("w_child", Driver::Claude, "claude-default")
            .unwrap();
        let rebound = tree.s.delegation_worker("w_child").unwrap().unwrap();
        assert_eq!(rebound.driver, "claude");
        assert_eq!(rebound.profile_id, "claude-default");
        assert_eq!(rebound.identity_json, None);
    }

    #[test]
    fn worker_launch_and_workspace_metadata_are_scrubbed_at_the_write_boundary() {
        let tree = tree();
        let canary = format!("{}{}", "ghp_", "y".repeat(36));
        let mut row = worker("w_metadata", "ro", Some("w_roroot"), "supervised", 1);
        row.backing_thread_id = Some(tree.s.create_thread("/w", Some("metadata")).unwrap());
        row.launch_json = serde_json::json!({"model":canary,"effort":canary}).to_string();
        row.workspace_json =
            serde_json::json!({"kind":"shared_read_only","path":format!("/tmp/{canary}")})
                .to_string();
        row.identity_json = Some(serde_json::json!({"requested":canary}).to_string());
        row.delegation_paths = serde_json::json!([format!("/tmp/{canary}")]).to_string();
        tree.s.insert_delegation_worker(&row).unwrap();
        let stored = tree.s.delegation_worker(&row.id).unwrap().unwrap();
        for json in [
            &stored.launch_json,
            &stored.workspace_json,
            stored.identity_json.as_ref().unwrap(),
            &stored.delegation_paths,
        ] {
            assert!(!json.contains(&canary));
            let _: serde_json::Value = serde_json::from_str(json).unwrap();
        }
    }

    #[test]
    fn a_partial_root_is_rejected_by_its_deferred_link() {
        let s = Store::open_in_memory().expect("store");
        let thread = s.create_thread("/w", None).expect("t");
        let r = root("ro", &thread);
        // Plain SQL: the root alone is accepted until COMMIT, which the deferred key refuses.
        s.conn.execute_batch("BEGIN").expect("begin");
        s.conn
            .execute(
                "INSERT INTO delegation_roots (id, root_thread_id, root_worker_id, task_state,
                     policy_json, continuation_budget, created_at, updated_at)
                 VALUES ('ro', ?1, 'w_missing', 'active', '{}', 3, 1, 1)",
                params![thread],
            )
            .expect("deferred");
        let commit = s.conn.execute_batch("COMMIT");
        assert!(commit.is_err(), "the deferred key refuses a partial root");
        s.conn.execute_batch("ROLLBACK").expect("rollback");
        assert_eq!(s.delegation_root("ro").expect("q"), None);

        // The helper checks the link itself, so the partial root fails alone, as one unit.
        let partial = s.atomically(|s| {
            s.conn.execute(
                "INSERT INTO delegation_roots (id, root_thread_id, root_worker_id, task_state,
                     policy_json, continuation_budget, created_at, updated_at)
                 VALUES ('ro', ?1, 'w_missing', 'active', '{}', 3, 1, 1)",
                params![thread],
            )?;
            s.check_root_links("ro")
        });
        assert!(partial.as_ref().is_err_and(is_constraint), "{partial:?}");
        assert_eq!(s.delegation_root("ro").expect("q"), None);
        // A worker of the wrong origin, or named differently, is refused before any row.
        let wrong = worker(&r.root_worker_id, "ro", None, "native", 0);
        assert!(s.insert_delegation_root(&r, &wrong).is_err());
        // The whole pair commits together.
        let rw = worker(&r.root_worker_id, "ro", None, "root", 0);
        s.insert_delegation_root(&r, &rw).expect("root");
        assert_eq!(s.delegation_root("ro").expect("q"), Some(r.clone()));
        assert_eq!(
            s.delegation_root_of_thread(&thread)
                .expect("q")
                .map(|r| r.id),
            Some("ro".into())
        );
        // One root worker per root; a root worker never has a parent or a backing thread.
        let second = worker("w_other", "ro", None, "root", 0);
        assert!(s.insert_delegation_worker(&second).is_err());
    }

    #[test]
    fn lineage_depth_and_origin_rules_hold() {
        let Tree { s, .. } = tree();
        // Depth follows the parent; a parent must be in the same root.
        let mut deep = worker("w_deep", "ro", Some("w_child"), "supervised", 1);
        assert!(
            s.insert_delegation_worker(&deep).is_err(),
            "depth 1 under 1"
        );
        deep.depth = 2;
        s.insert_delegation_worker(&deep).expect("depth 2");
        let too_deep = worker("w_3", "ro", Some("w_deep"), "supervised", 3);
        assert!(s.insert_delegation_worker(&too_deep).is_err());
        // Only supervised workers have a backing thread.
        let mut native = worker("w_native", "ro", Some("w_roroot"), "native", 1);
        native.backing_thread_id = Some("t".into());
        assert!(s.insert_delegation_worker(&native).is_err());
        // Lineage never changes.
        let moved = s.conn.execute(
            "UPDATE delegation_workers SET parent_id = 'w_roroot', depth = 1 WHERE id = 'w_deep'",
            [],
        );
        assert!(moved.is_err());
        s.set_delegation_worker_state("w_deep", "completed", Some("done"), 5)
            .expect("state");
        s.set_delegation_worker_status("w_deep", "restricted", Some("ceiling"), "[\"a\"]", 6)
            .expect("status");
        let w = s.delegation_worker("w_deep").expect("q").expect("row");
        assert_eq!(
            (w.state.as_str(), w.delegation_status.as_str()),
            ("completed", "restricted")
        );
        assert!(s
            .set_delegation_worker_status("w_deep", "bogus", None, "[]", 7)
            .is_err());
        assert_eq!(s.delegation_workers_of_root("ro").expect("q").len(), 3);
    }

    #[test]
    fn a_worker_has_at_most_one_active_run() {
        let Tree { s, .. } = tree();
        s.insert_delegation_run(&run("r_1", "w_child", "reserved"))
            .expect("first");
        let second = s.insert_delegation_run(&run("r_2", "w_child", "starting"));
        assert!(second.as_ref().is_err_and(is_constraint), "{second:?}");
        // Another worker's run is independent.
        s.insert_delegation_run(&run("r_root", "w_roroot", "running"))
            .expect("root run");
        assert_eq!(
            s.active_delegation_run("w_child").expect("q").map(|r| r.id),
            Some("r_1".into())
        );
        s.set_delegation_run_state("r_1", "completed", Some("turn-1"), Some("ok"), None, 9)
            .expect("complete");
        s.insert_delegation_run(&run("r_2", "w_child", "starting"))
            .expect("the slot is free");
        // A terminal run never moves again, and its identity is fixed.
        assert!(s
            .set_delegation_run_state("r_1", "running", None, None, None, 10)
            .is_err());
        let done = s.delegation_run("r_1").expect("q").expect("row");
        assert_eq!(done.native_turn_id.as_deref(), Some("turn-1"));
        assert_eq!(done.state, "completed");
    }

    fn op(id: &str, kind: &str, key: &str) -> OperationRow {
        OperationRow {
            id: id.into(),
            root_id: "ro".into(),
            principal_worker_id: "w_roroot".into(),
            kind: kind.into(),
            idempotency_key: key.into(),
            authority_epoch: 0,
            canonical_request: "{\"task\":\"x\"}".into(),
            resolution_json: "{\"target\":\"w_child\"}".into(),
            receipt_json: "{\"api_version\":1}".into(),
            dispatch_state: "accepted".into(),
            error_json: None,
            target_worker_id: Some("w_child".into()),
            run_id: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn an_idempotency_key_is_unique_per_principal_and_kind_and_the_receipt_is_frozen() {
        let Tree { s, .. } = tree();
        s.insert_delegation_operation(&op("op_1", "send", "k1"))
            .expect("op");
        let dup = s.insert_delegation_operation(&op("op_2", "send", "k1"));
        assert!(dup.as_ref().is_err_and(is_constraint), "{dup:?}");
        // Under a later epoch too: the key is unique across epochs.
        let mut later = op("op_3", "send", "k1");
        later.authority_epoch = 5;
        assert!(s.insert_delegation_operation(&later).is_err());
        s.insert_delegation_operation(&op("op_4", "cancel", "k1"))
            .expect("another kind");
        assert_eq!(
            s.delegation_operation_by_key("w_roroot", "send", "k1")
                .expect("q")
                .map(|o| o.id),
            Some("op_1".into())
        );
        // The dispatch state moves; the receipt, request and resolution never do.
        s.insert_delegation_run(&run("r_1", "w_child", "reserved"))
            .expect("run");
        s.set_delegation_dispatch_state("op_1", "dispatch_reserved", None, Some("r_1"), 2)
            .expect("reserve");
        for column in ["receipt_json", "canonical_request", "resolution_json"] {
            let changed = s.conn.execute(
                &format!("UPDATE delegation_operations SET {column} = '{{}}' WHERE id = 'op_1'"),
                [],
            );
            assert!(changed.is_err(), "{column} is frozen");
        }
        // A bound run is not rebound.
        s.set_delegation_dispatch_state("op_1", "submitted", None, Some("r_other"), 3)
            .expect("COALESCE keeps the first run");
        let stored = s.delegation_operation("op_1").expect("q").expect("row");
        assert_eq!(stored.run_id.as_deref(), Some("r_1"));
        assert_eq!(stored.receipt_json, "{\"api_version\":1}");
        assert_eq!(stored.dispatch_state, "submitted");
        // A failed operation carries its error.
        assert!(s
            .set_delegation_dispatch_state("op_4", "failed", None, None, 4)
            .is_err());
        s.set_delegation_dispatch_state("op_4", "failed", Some("{\"code\":\"x\"}"), None, 4)
            .expect("failed with its error");
        assert!(s
            .set_delegation_dispatch_state("op_1", "bogus", None, None, 5)
            .is_err());
    }

    #[test]
    fn a_messages_delivery_facts_are_set_once_and_its_body_is_masked() {
        let Tree { s, .. } = tree();
        s.insert_delegation_run(&run("r_1", "w_child", "running"))
            .expect("run");
        s.insert_delegation_run(&run("r_root", "w_roroot", "running"))
            .expect("run");
        s.insert_delegation_operation(&op("op_1", "send", "k1"))
            .expect("op");
        let secret = format!("ghp_{}", "abcdefghijklmnopqrstuvwxyz0123");
        let m = MessageRow {
            id: "m_1".into(),
            root_id: "ro".into(),
            operation_id: Some("op_1".into()),
            sender_worker_id: "w_roroot".into(),
            recipient_worker_id: "w_child".into(),
            kind: "instruction".into(),
            reply_to: None,
            body: format!("use {secret}"),
            delivery_policy: "this_run".into(),
            target_run_id: Some("r_1".into()),
            delivery_state: "queued".into(),
            recipient_run_id: None,
            queued_at: 1,
            submitted_at: None,
            confirmed_at: None,
            presented_at: None,
            acknowledged_at: None,
            updated_at: 1,
            event_seq: None,
        };
        s.insert_delegation_message(&m).expect("message");
        let stored = s.delegation_message("m_1").expect("q").expect("row");
        assert!(!stored.body.contains("ghp_abcdefghij"), "{}", stored.body);
        s.set_delegation_message_delivery("m_1", "submitted", None, 2)
            .expect("submitted");
        s.set_delegation_message_delivery("m_1", "misdelivered", Some("r_root"), 3)
            .expect("observed in another run");
        // The observed run is set once, and a terminal state is final.
        assert!(s
            .set_delegation_message_delivery("m_1", "misdelivered", Some("r_1"), 4)
            .is_err());
        assert!(s
            .set_delegation_message_delivery("m_1", "provider_confirmed", None, 4)
            .is_err());
        let changed = s.conn.execute(
            "UPDATE delegation_messages SET target_run_id = 'r_root' WHERE id = 'm_1'",
            [],
        );
        assert!(changed.is_err(), "the target run is frozen at acceptance");
        assert!(s
            .mark_delegation_message_presented("m_1", 5)
            .expect("first"));
        assert!(!s
            .mark_delegation_message_presented("m_1", 6)
            .expect("again"));
        let stored = s.delegation_message("m_1").expect("q").expect("row");
        assert_eq!(
            (
                stored.recipient_run_id.as_deref(),
                stored.presented_at,
                stored.submitted_at,
                stored.confirmed_at
            ),
            (Some("r_root"), Some(5), Some(2), Some(3))
        );
    }

    #[test]
    fn v4_links_upgrade_with_unknown_generation_and_keep_their_rows() {
        let Tree { s, root_thread, .. } = tree();
        let seq = s
            .append_event(
                &root_thread,
                None,
                &Envelope::new(Event::Notice { text: "old".into() }),
            )
            .expect("event");
        s.conn
            .execute_batch(
                "ALTER TABLE delegation_event_links DROP COLUMN process_generation;
            DELETE FROM schema_version WHERE version = 5;",
            )
            .expect("old v4 schema");
        s.conn.execute("INSERT INTO delegation_event_links (event_seq,worker_id,root_id,run_id,category,invocation_id)
            VALUES (?1,'w_child','ro',NULL,'lifecycle',NULL)", params![seq]).expect("old link");
        super::super::migrate(&s.conn).expect("upgrade");
        super::super::migrate(&s.conn).expect("idempotent");
        let links = s.delegation_event_links(seq).expect("links");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].worker_id, "w_child");
        assert_eq!(links[0].process_generation, 0);
        let mut next = links[0].clone();
        next.worker_id = "w_roroot".into();
        next.process_generation = 8;
        s.insert_delegation_event_link(&next)
            .expect("new generation");
        assert!(s
            .delegation_event_links(seq)
            .expect("read")
            .iter()
            .any(|l| l.process_generation == 8));
    }

    #[test]
    fn event_links_are_keyed_by_event_and_worker_and_aliases_map_native_ids() {
        let Tree { s, root_thread, .. } = tree();
        let before = s.delegation_event_high_water().expect("water mark");
        let seq = s
            .append_event(
                &root_thread,
                None,
                &Envelope::new(Event::Notice {
                    text: "registry".into(),
                }),
            )
            .expect("event");
        assert!(before < seq);
        assert_eq!(s.delegation_event_high_water().expect("water mark"), seq);
        let link = |worker: &str| EventLinkRow {
            process_generation: 0,
            event_seq: seq,
            worker_id: worker.into(),
            root_id: "ro".into(),
            run_id: None,
            category: "lifecycle".into(),
            invocation_id: None,
        };
        s.insert_delegation_event_link(&link("w_child")).expect("a");
        s.insert_delegation_event_link(&link("w_roroot"))
            .expect("one event, several workers");
        assert!(s.insert_delegation_event_link(&link("w_child")).is_err());
        assert_eq!(s.delegation_event_links(seq).expect("q").len(), 2);
        let mut bad = link("w_x");
        bad.event_seq = seq + 100;
        assert!(
            s.insert_delegation_event_link(&bad).is_err(),
            "no dangling event"
        );

        let pt = s
            .add_provider_thread(&root_thread, "claude", "opus")
            .expect("pt");
        let alias = NativeAliasRow {
            provider_thread_id: pt.clone(),
            process_generation: 1,
            native_worker_id: "agent-7".into(),
            worker_id: "w_child".into(),
            created_at: 1,
        };
        s.insert_delegation_native_alias(&alias).expect("alias");
        assert!(s.insert_delegation_native_alias(&alias).is_err());
        assert_eq!(
            s.delegation_native_alias(&pt, 1, "agent-7").expect("q"),
            Some("w_child".into())
        );
        assert_eq!(
            s.delegation_native_alias(&pt, 2, "agent-7").expect("q"),
            None
        );
    }

    #[test]
    fn a_hidden_child_thread_is_absent_from_the_sidebar_and_restoration_and_never_imported() {
        let Tree {
            s,
            root_thread,
            child_thread,
        } = tree();
        let pt = s
            .add_provider_thread(&child_thread, "codex", "gpt")
            .expect("pt");
        s.set_native_id(&pt, "child-native").expect("native");
        // The sidebar and the open-thread restoration's known threads (list_threads(false)).
        for archived in [false, true] {
            let ids: Vec<String> = s
                .list_threads(archived)
                .expect("list")
                .into_iter()
                .map(|t| t.id)
                .collect();
            assert_eq!(ids, vec![root_thread.clone()], "archived={archived}");
        }
        assert!(s.is_hidden_child_thread(&child_thread).expect("q"));
        assert!(!s.is_hidden_child_thread(&root_thread).expect("q"));
        // The native import skips a held session: the child's is held by its hidden thread, so
        // it is never imported as a visible thread.
        assert_eq!(
            s.thread_with_native_id("codex", "child-native").expect("q"),
            Some(child_thread.clone())
        );
        // A hidden or root thread cannot be deleted out from under its delegation rows.
        assert!(s.delete_thread(&child_thread).is_err());
        assert!(s.delete_thread(&root_thread).is_err());
    }
}
