//! The typed delegation store API (P3b): the write protocol of plan §5 over the P1 types.
//!
//! Every method here is **one transaction** ([`Store::atomically`]): all of it is stored, or none
//! of it. The control writes (accept, reserve, record, complete, revoke, ACK) are meant to run as
//! durable control writes on the one store writer (`StoreWriter::delegation_*`, which use
//! `durable_call`); publication and page reads run as ordinary writer calls (`call_result`).
//! Nothing here is ever fire-and-forget: a seq, a cursor or a receipt is returned only after the
//! transaction that holds it committed.
//!
//! Two error layers:
//! - `Err(StoreError)`: the store could not do it, and nothing of it is stored (the transaction
//!   rolled back). A durable write's `Durability` error whose text starts with "committed" is the
//!   exception: see [`DurableOutcome`].
//! - `Ok(Err(ToolError))` ([`Verdict`]): the request is refused under the contract. Whatever the
//!   refusal itself records (an operation invalidated by a stale epoch, say) is committed.
//!
//! Recovery never re-executes anything: [`Store::reconcile_delegation_after_restart`] turns every
//! reservation, submission and non-terminal run into `unknown`, and a replay of an `unknown`
//! operation is `delivery_unknown` (contract §2), never a second dispatch.

use std::collections::{BTreeSet, HashSet};

use agent_core::adapter::Driver;
use agent_core::delegation::authority::{revocation, RevocationEffect, RevocationEvent, TaskMode};
use agent_core::delegation::error::{ErrorCode, ToolError};
use agent_core::delegation::identity::Identity;
use agent_core::delegation::ids::{
    Cursor, IdempotencyKey, MessageId, OperationId, ProfileId, RunId, WorkerId,
};
use agent_core::delegation::lifecycle::RunState;
use agent_core::delegation::limits::{admit_spawn, Limits, Usage};
use agent_core::delegation::mailbox::{
    ack_eligible, delivery_step, kind_allowed, ContinuationBudget, DeliveryState, Direction,
    MessageKind,
};
use agent_core::delegation::operation::{
    canonical_request, idempotency, on_restart, operation_step, replay, CanonicalRequest,
    IdempotencyDecision, OperationKind, OperationState, ReplayOutcome, Resolution, StoredOperation,
};
use agent_core::delegation::page::{
    assemble, decode_cursor, encode_cursor, CursorPayload, PageFilter, PageParts, PageUnit,
    Reservation, UnitKind,
};
use agent_core::delegation::wire::{
    AckOutcome, AckRejection, CancelReceipt, CancelState, CloseReceipt, CloseState,
    DelegationStatusCode, Delivery, EventCategory, MutationReceipt, PageEvent, PageMessage,
    SendReceipt, SpawnAuthority, SpawnReceipt, SpawnState, StateRef, Target, ToolRequest, Warning,
    WorkspaceInfo, MAX_RECEIPT_BYTES, MAX_WARNINGS,
};
use agent_core::delegation::API_VERSION;
use agent_core::event::{Envelope, Event, ItemStatus, TurnState, WorkerState};
use agent_core::redact::redact;
use rusqlite::{params, OptionalExtension};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};

use super::delegation::{
    EventLinkRow, MessageRow, NativeAliasRow, OperationRow, RootRow, RunRow, WorkerRow,
};
use super::{Result, Store, StoreError};

/// A contract refusal: committed as far as it records anything, and returned to the caller.
pub type Verdict<T> = std::result::Result<T, ToolError>;

/// Longest `PageEvent.summary` (contract §2), in bytes.
pub const MAX_EVENT_SUMMARY_BYTES: usize = 2 * 1024;
/// Longest completion result carried into the parent's mailbox (a message's bound).
pub const MAX_RESULT_BYTES: usize = 16 * 1024;
/// [`Store::meta`] key set by a clean shutdown and consumed by the next reconciliation.
pub const CLEAN_SHUTDOWN_KEY: &str = "delegation_clean_shutdown";

// ---------------------------------------------------------------------------------------------
// Durable outcomes
// ---------------------------------------------------------------------------------------------

/// What a durable control write's answer means for dispatch.
#[derive(Debug)]
pub enum DurableOutcome<T> {
    /// Committed under `synchronous=FULL`: the caller may go on (dispatch, publish).
    Committed(T),
    /// Not committed: nothing of it is stored. The tool call gets `persistence_failed` (retryable
    /// with the same key) and nothing is dispatched.
    Refused(StoreError),
    /// The rows **are** stored, but the durable switch back failed (or the answer was lost): the
    /// operation is refused and is **never** dispatched by this call. Recovery and a same-key
    /// replay see it by its stored state; nothing re-dispatches it.
    CommittedUnconfirmed(StoreError),
}

impl<T> DurableOutcome<T> {
    /// Classifies a `durable_call` answer (the receiver's `recv()`).
    pub fn from_answer(answer: std::result::Result<Result<T>, std::sync::mpsc::RecvError>) -> Self {
        match answer {
            Ok(Ok(value)) => DurableOutcome::Committed(value),
            Ok(Err(StoreError::Durability(why))) if why.starts_with("committed") => {
                DurableOutcome::CommittedUnconfirmed(StoreError::Durability(why))
            }
            Ok(Err(e)) => DurableOutcome::Refused(e),
            // No answer at all: whether it committed cannot be known, so assume it did.
            Err(_) => DurableOutcome::CommittedUnconfirmed(StoreError::WriterGone),
        }
    }

    /// Whether the caller may dispatch on it.
    pub fn may_dispatch(&self) -> bool {
        matches!(self, DurableOutcome::Committed(_))
    }
}

// ---------------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------------

/// The serde (snake case) name of a unit enum value.
fn name_of<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(Value::String(s)) => s,
        _ => "unknown".to_owned(),
    }
}

/// A stored enum value read back; a value this build cannot read is an error, never a guess.
fn parse_name<T: DeserializeOwned>(what: &str, s: &str) -> Result<T> {
    serde_json::from_value(Value::String(s.to_owned()))
        .map_err(|_| StoreError::Failed(format!("stored {what} `{s}` is not one this build knows")))
}

fn parse_id<T>(
    what: &str,
    s: &str,
    parse: fn(&str) -> std::result::Result<T, agent_core::delegation::ids::IdError>,
) -> Result<T> {
    parse(s).map_err(|e| StoreError::Failed(format!("stored {what} `{s}` is not a handle: {e}")))
}

fn worker_id(s: &str) -> Result<WorkerId> {
    parse_id("worker", s, WorkerId::parse)
}

fn run_id(s: &str) -> Result<RunId> {
    parse_id("run", s, RunId::parse)
}

fn message_id(s: &str) -> Result<MessageId> {
    parse_id("message", s, MessageId::parse)
}

fn operation_id(s: &str) -> Result<OperationId> {
    parse_id("operation", s, OperationId::parse)
}

/// `s` cut to at most `max` bytes at a character boundary.
fn cut(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

fn illegal(what: &str) -> StoreError {
    StoreError::Failed(format!("illegal delegation transition: {what}"))
}

fn rfc3339_ms(ms: i64) -> String {
    agent_core::quota::epoch_to_rfc3339(ms.div_euclid(1000))
}

/// The idempotency key and expected epoch of a mutating request.
fn key_and_epoch(req: &ToolRequest) -> Option<(&IdempotencyKey, Option<u64>)> {
    match req {
        ToolRequest::Spawn(r) => Some((&r.idempotency_key, r.expected_authority_epoch)),
        ToolRequest::Send(r) => Some((&r.idempotency_key, r.expected_authority_epoch)),
        ToolRequest::Cancel(r) => Some((&r.idempotency_key, r.expected_authority_epoch)),
        ToolRequest::Close(r) => Some((&r.idempotency_key, r.expected_authority_epoch)),
        ToolRequest::Read(r) if r.acknowledge.is_some() => r
            .idempotency_key
            .as_ref()
            .map(|k| (k, r.expected_authority_epoch)),
        _ => None,
    }
}

fn i64_of(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn u64_of(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

/// An operation row as the P1 type.
fn stored_operation(row: &OperationRow) -> Result<StoredOperation> {
    Ok(StoredOperation {
        id: operation_id(&row.id)?,
        kind: parse_name("operation kind", &row.kind)?,
        epoch: agent_core::delegation::ids::AuthorityEpoch(u64_of(row.authority_epoch)),
        canonical: CanonicalRequest::from_stored(row.canonical_request.clone()),
        resolution: serde_json::from_str(&row.resolution_json)?,
        receipt: Some(serde_json::from_str(&row.receipt_json)?),
        dispatch: parse_name("dispatch state", &row.dispatch_state)?,
        error: match &row.error_json {
            Some(e) => Some(serde_json::from_str(e)?),
            None => None,
        },
    })
}

// ---------------------------------------------------------------------------------------------
// Request and result types
// ---------------------------------------------------------------------------------------------

/// A new root: the user's own session, made a delegation root.
#[derive(Debug, Clone)]
pub struct NewRoot {
    /// The root's (visible) thread.
    pub thread: String,
    pub driver: Driver,
    pub profile_id: ProfileId,
    pub name: String,
    /// Sanitised policy JSON: never a capability.
    pub policy_json: String,
}

/// A created root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootHandle {
    /// `delegation_roots.id`.
    pub root: String,
    /// The root's own worker (the principal of the user's session).
    pub worker: WorkerId,
}

/// What a spawn creates, decided by the supervisor before acceptance.
#[derive(Debug, Clone)]
pub struct SpawnPlan {
    pub driver: Driver,
    /// The requested identity (model/effort `launch_configured` or `unknown`).
    pub identity: Identity,
    pub workspace: WorkspaceInfo,
    /// The safe launch snapshot (JSON): never tokens or an environment snapshot.
    pub launch_json: String,
    /// The hidden backing thread's directory.
    pub cwd: String,
    pub state: SpawnState,
    pub process_generation: u64,
}

/// What a send records at acceptance (the plan from `mailbox::plan_delivery`).
#[derive(Debug, Clone)]
pub struct SendPlan {
    /// `queued` or `queued_next_turn`.
    pub initial: DeliveryState,
    /// The run this delivery is aimed at, frozen in the receipt.
    pub target_run: Option<RunId>,
}

/// The per-tool part of an acceptance.
#[derive(Debug, Clone)]
pub enum AcceptPlan {
    Spawn(Box<SpawnPlan>),
    Send(SendPlan),
    /// `cancel`: the bound set is `Resolution::bound_runs`; an empty set is `idle` and is never
    /// dispatched.
    Cancel,
    Close,
}

/// Write protocol step 1: one acceptance.
#[derive(Debug, Clone)]
pub struct Accept {
    /// The authenticated caller (never from arguments).
    pub principal: WorkerId,
    /// The parsed and validated request (`parse_request` + `validate`).
    pub request: ToolRequest,
    /// What the request resolved to, fixed now and never recomputed.
    pub resolution: Resolution,
    pub plan: AcceptPlan,
    pub warnings: Vec<Warning>,
    pub limits: Limits,
    /// Live supervised processes app-wide (in memory: only an observed exit frees one).
    pub live_processes: u32,
    /// Operations this app instance is dispatching right now (a replay of one waits).
    pub dispatching: HashSet<OperationId>,
}

/// An accepted operation.
#[derive(Debug, Clone, PartialEq)]
pub struct AcceptedOp {
    pub operation: OperationId,
    /// The receipt exactly as stored (`receipt_json` is its serialisation).
    pub receipt: Value,
    pub worker: Option<WorkerId>,
    pub run: Option<RunId>,
    pub message: Option<MessageId>,
    /// The audit event's seq (the message's unit for a send).
    pub seq: i64,
}

/// What an acceptance came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Accepted {
    New(AcceptedOp),
    /// The key was already used for this very request: the contract §2 replay table.
    Replay(ReplayOutcome),
}

/// A reserved dispatch (write protocol step 2): exactly what to dispatch, from the store.
#[derive(Debug, Clone, PartialEq)]
pub struct Reserved {
    pub operation: StoredOperation,
    pub target: Option<WorkerId>,
    pub run: Option<RunId>,
    pub message: Option<MessageId>,
}

/// An observed dispatch fact (write protocol step 4).
#[derive(Debug, Clone, PartialEq)]
pub enum DispatchFact {
    /// Written to the transport (never "consumed").
    Submitted,
    /// The provider confirmed it.
    Confirmed,
    /// It failed; the error is stored (scrubbed) and replayed.
    Failed(ToolError),
    /// The outcome cannot be known.
    Unknown,
}

/// One link of a published event: event `seq` concerns `worker`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSpec {
    pub worker: WorkerId,
    pub run: Option<RunId>,
    pub category: EventCategory,
    /// An attested invocation id, when the event comes from one.
    pub invocation: Option<String>,
}

/// A native worker id resolved to an app worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAlias {
    pub provider_thread: String,
    pub process_generation: u64,
    pub native_worker_id: String,
    pub worker: WorkerId,
}

/// One authoritative event, published with its links and state in one transaction.
#[derive(Debug, Clone)]
pub struct PublishEvent {
    pub thread: String,
    pub provider_thread: Option<String>,
    pub envelope: Envelope,
    /// At least one; every worker of the same root.
    pub links: Vec<LinkSpec>,
    pub worker_states: Vec<(WorkerId, WorkerState, Option<String>)>,
    pub run_states: Vec<(RunId, RunState)>,
    pub aliases: Vec<NewAlias>,
}

/// A committed publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub seq: i64,
    /// `delegation_roots.id`.
    pub root: String,
    pub root_worker: WorkerId,
    pub stream_epoch: u64,
}

/// Write protocol step 5: a run's end.
#[derive(Debug, Clone)]
pub struct Completion {
    pub run: RunId,
    /// Terminal: `completed`, `interrupted` or `failed`.
    pub state: RunState,
    pub worker_state: WorkerState,
    pub reason: Option<String>,
    pub usage_json: Option<String>,
    /// The result for the parent's mailbox (scrubbed and cut to [`MAX_RESULT_BYTES`]); `None`
    /// sends no notification.
    pub result: Option<String>,
    /// The terminal event and its links.
    pub event: PublishEvent,
}

/// A committed completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completed {
    pub event: Published,
    pub notification: Option<(MessageId, i64)>,
}

/// Write protocol step 6: a committed revocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revoked {
    pub authority_epoch: u64,
    /// The tokens to kill, processes to stop and workers marked restricted.
    pub effect: RevocationEffect,
    /// Not-yet-reserved operations failed with `stale_epoch`.
    pub invalidated: Vec<OperationId>,
    /// Queued sends now `cancelled`.
    pub cancelled: Vec<MessageId>,
    /// Workers now `closed` (a close).
    pub closed: Vec<WorkerId>,
    /// The root's continuation reservation was released.
    pub continuation_released: bool,
}

/// One read or wait page query (contract §3 page rules).
#[derive(Debug, Clone)]
pub struct PageQuery {
    pub root_worker: WorkerId,
    /// What the cursor binds to.
    pub filter: PageFilter,
    /// The workers whose events count (the caller resolves and authorises the subtree).
    pub scope: Vec<WorkerId>,
    pub after: Option<Cursor>,
    pub max_events: u32,
    pub max_bytes: u32,
    /// The serialised `WorkerSummary`, always included and counted first.
    pub snapshot: Option<Value>,
    /// The serialised ACK outcome (ACKs are applied before the page is built).
    pub ack: Option<Value>,
    pub budget: usize,
}

/// An assembled page.
#[derive(Debug, Clone, PartialEq)]
pub struct PageOut {
    pub page: Value,
    pub last_seq: Option<u64>,
    pub truncated: bool,
    /// Messages this page returns to their recipient for the first time (marked presented by
    /// [`Store::read_and_present_delegation_page`]; left for the caller to mark by
    /// [`Store::read_delegation_page`]).
    pub presented: Vec<MessageId>,
}

/// A mailbox ACK (contract §3 read `acknowledge`).
#[derive(Debug, Clone)]
pub struct AckRequest {
    pub principal: WorkerId,
    /// A `read` with `acknowledge`, its key and optional epoch.
    pub request: ToolRequest,
}

/// What an ACK came to.
#[derive(Debug, Clone, PartialEq)]
pub enum AckResult {
    /// The `AckOutcome` object, exactly as stored.
    New(Value),
    Replay(ReplayOutcome),
}

/// What startup reconciliation changed. Nothing in it is ever retried automatically.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// No clean-shutdown mark was found.
    pub abnormal: bool,
    pub runs: Vec<RunId>,
    pub workers: Vec<WorkerId>,
    pub operations: Vec<OperationId>,
    pub messages: Vec<MessageId>,
    /// Roots whose stream epoch was advanced (abnormal restarts only).
    pub roots: Vec<String>,
}

/// The result of the idempotency front of a mutation.
enum Front {
    New {
        kind: OperationKind,
        canonical: CanonicalRequest,
    },
    Done(Verdict<ReplayOutcome>),
}

// ---------------------------------------------------------------------------------------------
// The store API
// ---------------------------------------------------------------------------------------------

impl Store {
    /// A fresh random handle suffix (24 hex digits): no table, no lock.
    fn handle_suffix(&self) -> Result<String> {
        Ok(self
            .conn
            .query_row("SELECT lower(hex(randomblob(12)))", [], |r| r.get(0))?)
    }

    fn new_worker_id(&self) -> Result<WorkerId> {
        let s = self.handle_suffix()?;
        WorkerId::from_suffix(&s).map_err(|e| StoreError::Failed(e.to_string()))
    }

    fn new_run_id(&self) -> Result<RunId> {
        let s = self.handle_suffix()?;
        RunId::from_suffix(&s).map_err(|e| StoreError::Failed(e.to_string()))
    }

    fn new_message_id(&self) -> Result<MessageId> {
        let s = self.handle_suffix()?;
        MessageId::from_suffix(&s).map_err(|e| StoreError::Failed(e.to_string()))
    }

    fn new_operation_id(&self) -> Result<OperationId> {
        let s = self.handle_suffix()?;
        OperationId::from_suffix(&s).map_err(|e| StoreError::Failed(e.to_string()))
    }

    fn worker_row(&self, w: &WorkerId) -> Result<WorkerRow> {
        self.delegation_worker(w.as_str())?
            .ok_or_else(|| StoreError::NotFound(w.to_string()))
    }

    fn root_row(&self, root: &str) -> Result<RootRow> {
        self.delegation_root(root)?
            .ok_or_else(|| StoreError::NotFound(root.to_owned()))
    }

    /// The root whose root worker is `w`.
    pub fn delegation_root_of_worker(&self, w: &WorkerId) -> Result<Option<RootRow>> {
        let Some(row) = self.delegation_worker(w.as_str())? else {
            return Ok(None);
        };
        self.delegation_root(&row.root_id)
    }

    /// The thread a worker's events are stored in: its hidden backing thread, else the root's.
    fn event_thread(&self, w: &WorkerRow) -> Result<String> {
        match &w.backing_thread_id {
            Some(t) => Ok(t.clone()),
            None => Ok(self.root_row(&w.root_id)?.root_thread_id),
        }
    }

    /// `w` and every descendant, `w` first, then breadth first by creation.
    pub fn delegation_subtree(&self, w: &WorkerId) -> Result<Vec<WorkerId>> {
        let row = self.worker_row(w)?;
        let all = self.delegation_workers_of_root(&row.root_id)?;
        let mut out = vec![row.id.clone()];
        let mut i = 0;
        while i < out.len() {
            let parent = out[i].clone();
            for c in &all {
                if c.parent_id.as_deref() == Some(parent.as_str()) && !out.contains(&c.id) {
                    out.push(c.id.clone());
                }
            }
            i += 1;
        }
        out.iter().map(|s| worker_id(s)).collect()
    }

    // ---- roots ----

    /// Creates a root and its root worker through the checked insert
    /// ([`Store::insert_delegation_root`]): the pair commits together or fails alone.
    pub fn create_delegation_root(&self, r: &NewRoot) -> Result<RootHandle> {
        let now = self.now();
        let root = format!("dr_{}", self.handle_suffix()?);
        let worker = self.new_worker_id()?;
        let row = RootRow {
            id: root.clone(),
            root_thread_id: r.thread.clone(),
            root_worker_id: worker.to_string(),
            task_state: "active".into(),
            authority_epoch: 0,
            policy_revision: 0,
            stream_epoch: 0,
            policy_json: r.policy_json.clone(),
            continuation_budget: i64::from(ContinuationBudget::MAX),
            continuation_reserved: false,
            created_at: now,
            updated_at: now,
        };
        let w = WorkerRow {
            id: worker.to_string(),
            root_id: root.clone(),
            parent_id: None,
            origin: "root".into(),
            depth: 0,
            name: r.name.clone(),
            driver: name_of(&r.driver),
            profile_id: r.profile_id.to_string(),
            launch_json: "{}".into(),
            backing_thread_id: None,
            state: name_of(&WorkerState::Running),
            task_mode: None,
            workspace_json: "{}".into(),
            identity_json: None,
            delegation_status: name_of(&DelegationStatusCode::Available),
            delegation_reason: None,
            delegation_paths: "[]".into(),
            last_activity: None,
            created_at: now,
            updated_at: now,
        };
        self.insert_delegation_root(&row, &w)?;
        Ok(RootHandle { root, worker })
    }

    // ---- capacity ----

    /// The persisted run counters: active (`reserved`..`stopping`) and pending (`reserved`,
    /// `starting`), per root and app-wide.
    fn run_usage(&self, root: &str, live_processes: u32) -> Result<Usage> {
        let count = |sql: &str, p: &[&dyn rusqlite::ToSql]| -> Result<u32> {
            let n: i64 = self.conn.query_row(sql, p, |r| r.get(0))?;
            Ok(u32::try_from(n).unwrap_or(u32::MAX))
        };
        const ACTIVE: &str = "('reserved', 'starting', 'running', 'stopping')";
        const PENDING: &str = "('reserved', 'starting')";
        let in_root = |states: &str| {
            format!(
                "SELECT COUNT(*) FROM delegation_runs r JOIN delegation_workers w
                 ON w.id = r.worker_id WHERE w.root_id = ?1 AND r.state IN {states}"
            )
        };
        let app =
            |states: &str| format!("SELECT COUNT(*) FROM delegation_runs WHERE state IN {states}");
        Ok(Usage {
            active_runs_root: count(&in_root(ACTIVE), &[&root])?,
            active_runs_app: count(&app(ACTIVE), &[])?,
            live_processes,
            pending_root: count(&in_root(PENDING), &[&root])?,
            pending_app: count(&app(PENDING), &[])?,
            ..Usage::default()
        })
    }

    // ---- the idempotency front (contract §2 order) ----

    fn idempotency_front(
        &self,
        principal: &WorkerRow,
        root: &RootRow,
        request: &ToolRequest,
        dispatching: &HashSet<OperationId>,
    ) -> Result<Front> {
        let Some(kind) = OperationKind::of(request) else {
            return Ok(Front::Done(Err(ToolError::new(
                ErrorCode::InvalidArgument,
                "not a mutating request",
            ))));
        };
        let Some((key, expected)) = key_and_epoch(request) else {
            return Ok(Front::Done(Err(ToolError::invalid(
                "idempotency_key",
                "required",
            ))));
        };
        let canonical = canonical_request(request, redact);
        let existing = self
            .delegation_operation_by_key(&principal.id, &name_of(&kind), key.as_str())?
            .map(|row| stored_operation(&row))
            .transpose()?;
        let current = agent_core::delegation::ids::AuthorityEpoch(u64_of(root.authority_epoch));
        let decision = idempotency(
            existing.as_ref(),
            kind,
            &canonical,
            expected.map(agent_core::delegation::ids::AuthorityEpoch),
            current,
        );
        let done = |e: ToolError| Ok(Front::Done(Err(e)));
        match decision {
            Err(code) => done(
                ToolError::new(code, "the authority epoch changed; read capabilities again")
                    .with_detail("authority_epoch", current.0),
            ),
            Ok(IdempotencyDecision::StaleEpoch(op)) => done(
                ToolError::new(
                    ErrorCode::StaleEpoch,
                    "this key was used under an earlier authority epoch",
                )
                .with_detail("operation_id", op.as_str()),
            ),
            Ok(IdempotencyDecision::Conflict(op)) => done(
                ToolError::new(
                    ErrorCode::IdempotencyConflict,
                    "this key was used for a different request",
                )
                .with_detail("operation_id", op.as_str()),
            ),
            Ok(IdempotencyDecision::Replay(op)) => {
                let stored =
                    existing.ok_or_else(|| StoreError::Failed("replay without row".into()))?;
                Ok(Front::Done(Ok(replay(&stored, dispatching.contains(&op)))))
            }
            Ok(IdempotencyDecision::New) => Ok(Front::New { kind, canonical }),
        }
    }

    // ---- step 1: accept ----

    /// Write protocol step 1, in one transaction: the persisted epoch, the capacity counters and
    /// idempotency are checked; then the operation (canonical scrubbed request, frozen
    /// resolution, the receipt stored as its serialised JSON exactly as returned), the worker,
    /// message or run reservation, and the audit event with its links are inserted together.
    ///
    /// A replay follows the contract §2 table by the stored dispatch state.
    pub fn accept_delegation(&self, a: &Accept) -> Result<Verdict<Accepted>> {
        self.atomically(|s| s.accept_in_txn(a))
    }

    fn accept_in_txn(&self, a: &Accept) -> Result<Verdict<Accepted>> {
        let Some(principal) = self.delegation_worker(a.principal.as_str())? else {
            return Ok(Err(ToolError::new(
                ErrorCode::NotFound,
                "unknown principal",
            )));
        };
        let root = self.root_row(&principal.root_id)?;
        let (kind, canonical) =
            match self.idempotency_front(&principal, &root, &a.request, &a.dispatching)? {
                Front::Done(Ok(outcome)) => return Ok(Ok(Accepted::Replay(outcome))),
                Front::Done(Err(e)) => return Ok(Err(e)),
                Front::New { kind, canonical } => (kind, canonical),
            };
        if kind == OperationKind::Ack {
            return Ok(Err(ToolError::new(
                ErrorCode::InvalidArgument,
                "an acknowledgement goes through acknowledge_delegation_messages",
            )));
        }
        if principal.state == name_of(&WorkerState::Closed) || root.task_state == "closed" {
            return Ok(Err(ToolError::new(
                ErrorCode::WorkerClosed,
                "the caller is closed",
            )));
        }
        let key = key_and_epoch(&a.request)
            .map(|(k, _)| k.to_string())
            .unwrap_or_default();
        let now = self.now();
        let op = self.new_operation_id()?;
        let receipt = MutationReceipt {
            api_version: API_VERSION,
            operation_id: op.clone(),
            authority_epoch: u64_of(root.authority_epoch),
            replayed: false,
            dispatch: OperationState::Accepted,
            warnings: a.warnings.iter().take(MAX_WARNINGS).cloned().collect(),
        };
        let effect = match (&a.request, &a.plan) {
            (ToolRequest::Spawn(req), AcceptPlan::Spawn(plan)) => {
                self.accept_spawn(a, &principal, &root, req, plan, receipt, now)?
            }
            (ToolRequest::Send(req), AcceptPlan::Send(plan)) => {
                self.accept_send(a, &principal, &root, req, plan, receipt, now)?
            }
            (ToolRequest::Cancel(req), AcceptPlan::Cancel) => self.accept_cancel(
                a,
                &principal,
                &root,
                &req.worker_id,
                req.expected_run_id.as_ref(),
                receipt,
                now,
            )?,
            (ToolRequest::Close(req), AcceptPlan::Close) => {
                self.accept_close(&principal, &root, &req.worker_id, receipt, now)?
            }
            _ => {
                return Ok(Err(ToolError::new(
                    ErrorCode::InternalError,
                    "the acceptance plan does not match the request",
                )))
            }
        };
        let effect = match effect {
            Ok(e) => e,
            Err(refused) => return Ok(Err(refused)),
        };
        let receipt_json = serde_json::to_string(&effect.receipt)?;
        if receipt_json.len() > MAX_RECEIPT_BYTES {
            // Returning Err rolls the whole acceptance back: nothing is half accepted.
            return Err(StoreError::Failed(
                "a delegation receipt exceeds its bound".into(),
            ));
        }
        self.insert_delegation_operation(&OperationRow {
            id: op.to_string(),
            root_id: root.id.clone(),
            principal_worker_id: principal.id.clone(),
            kind: name_of(&kind),
            idempotency_key: key,
            authority_epoch: root.authority_epoch,
            canonical_request: canonical.as_str().to_owned(),
            resolution_json: serde_json::to_string(&a.resolution)?,
            receipt_json,
            dispatch_state: name_of(&OperationState::Accepted),
            error_json: None,
            target_worker_id: effect.target.as_ref().map(ToString::to_string),
            run_id: effect.run.as_ref().map(ToString::to_string),
            created_at: now,
            updated_at: now,
        })?;
        // A message references its operation (an immediate key, and frozen once written): it is
        // inserted after the operation, with its event and links, in this same transaction.
        let mut seq = effect.seq;
        if let Some(pending) = &effect.message_row {
            let mut row = pending.row.clone();
            row.operation_id = Some(op.to_string());
            seq = self.insert_message_and_event(&row, pending.direction, pending.kind)?;
        }
        if effect.complete_now {
            // Nothing to dispatch (an idle cancel): the operation is done at acceptance.
            self.step_operation(&op, OperationState::DispatchReserved, None, now)?;
            self.step_operation(&op, OperationState::Confirmed, None, now)?;
        }
        Ok(Ok(Accepted::New(AcceptedOp {
            operation: op,
            receipt: effect.receipt,
            worker: effect.worker,
            run: effect.run,
            message: effect.message_row.map(|p| p.id),
            seq,
        })))
    }

    #[allow(clippy::too_many_arguments)]
    fn accept_spawn(
        &self,
        a: &Accept,
        principal: &WorkerRow,
        root: &RootRow,
        req: &agent_core::delegation::wire::SpawnRequest,
        plan: &SpawnPlan,
        receipt: MutationReceipt,
        now: i64,
    ) -> Result<Verdict<Effect>> {
        let depth = principal.depth + 1;
        if depth > i64::from(a.limits.max_depth) {
            return Ok(Err(ToolError::new(
                ErrorCode::AuthorityDenied,
                "the caller is at the maximum depth",
            )
            .with_detail("reason", "depth")));
        }
        let usage = self.run_usage(&root.id, a.live_processes)?;
        if let Err(e) = admit_spawn(&a.limits, &usage, true) {
            return Ok(Err(e));
        }
        let worker = self.new_worker_id()?;
        let run = self.new_run_id()?;
        let thread = self.new_id()?;
        self.insert_thread(&thread, &plan.cwd, Some(&req.name), now)?;
        self.insert_delegation_worker(&WorkerRow {
            id: worker.to_string(),
            root_id: root.id.clone(),
            parent_id: Some(principal.id.clone()),
            origin: "supervised".into(),
            depth,
            name: req.name.clone(),
            driver: name_of(&plan.driver),
            profile_id: req.profile_id.to_string(),
            launch_json: plan.launch_json.clone(),
            backing_thread_id: Some(thread.clone()),
            state: name_of(&WorkerState::Starting),
            task_mode: Some(name_of(&req.task_mode)),
            workspace_json: serde_json::to_string(&plan.workspace)?,
            identity_json: Some(serde_json::to_string(&plan.identity)?),
            delegation_status: name_of(&DelegationStatusCode::Available),
            delegation_reason: None,
            delegation_paths: "[]".into(),
            last_activity: None,
            created_at: now,
            updated_at: now,
        })?;
        self.insert_delegation_run(&RunRow {
            id: run.to_string(),
            worker_id: worker.to_string(),
            process_generation: i64_of(plan.process_generation),
            provider_thread_id: None,
            native_turn_id: None,
            authority_epoch: root.authority_epoch,
            state: name_of(&RunState::Reserved),
            terminal_reason: None,
            usage_json: None,
            created_at: now,
            updated_at: now,
        })?;
        let seq = self
            .publish_in_txn(&PublishEvent {
                thread,
                provider_thread: None,
                envelope: Envelope::new(Event::Notice {
                    text: format!("Delegated worker accepted: {}", redact(&req.name)),
                }),
                links: vec![
                    LinkSpec {
                        worker: worker.clone(),
                        run: Some(run.clone()),
                        category: EventCategory::Lifecycle,
                        invocation: None,
                    },
                    LinkSpec {
                        worker: worker_id(&principal.id)?,
                        run: None,
                        category: EventCategory::Lifecycle,
                        invocation: None,
                    },
                ],
                worker_states: Vec::new(),
                run_states: Vec::new(),
                aliases: Vec::new(),
            })?
            .seq;
        let depth = u8::try_from(depth).unwrap_or(u8::MAX);
        let full = SpawnReceipt {
            receipt,
            worker_id: worker.clone(),
            run_id: Some(run.clone()),
            state: plan.state,
            identity: plan.identity.clone(),
            authority: SpawnAuthority {
                task_mode: req.task_mode,
                workspace: plan.workspace.clone(),
                can_spawn: agent_core::delegation::authority::can_spawn(depth),
                depth,
            },
        };
        Ok(Ok(Effect {
            receipt: serde_json::to_value(&full)?,
            target: Some(worker.clone()),
            worker: Some(worker),
            run: Some(run),
            message_row: None,
            seq,
            complete_now: false,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn accept_send(
        &self,
        a: &Accept,
        principal: &WorkerRow,
        root: &RootRow,
        req: &agent_core::delegation::wire::SendRequest,
        plan: &SendPlan,
        receipt: MutationReceipt,
        now: i64,
    ) -> Result<Verdict<Effect>> {
        let target = match (&req.target, &a.resolution.target) {
            (_, Some(t)) => t.clone(),
            (Target::Worker(w), None) => w.clone(),
            (Target::Parent, None) => match &principal.parent_id {
                Some(p) => worker_id(p)?,
                None => {
                    return Ok(Err(ToolError::new(
                        ErrorCode::NotFound,
                        "a root has no parent",
                    )))
                }
            },
        };
        let recipient = match self.delegation_worker(target.as_str())? {
            Some(r) if r.root_id == root.id => r,
            _ => return Ok(Err(ToolError::new(ErrorCode::NotFound, "unknown worker"))),
        };
        let direction = if principal.parent_id.as_deref() == Some(recipient.id.as_str()) {
            Direction::ChildToParent
        } else if recipient.parent_id.as_deref() == Some(principal.id.as_str()) {
            Direction::ParentToChild
        } else {
            return Ok(Err(ToolError::new(
                ErrorCode::NotFound,
                "messages go between a worker and its parent",
            )));
        };
        if !kind_allowed(direction, req.kind) {
            return Ok(Err(ToolError::invalid("kind", "direction")));
        }
        if recipient.state == name_of(&WorkerState::Closed) {
            return Ok(Err(ToolError::new(
                ErrorCode::WorkerClosed,
                "the worker is closed",
            )));
        }
        if direction == Direction::ParentToChild
            && recipient.delegation_status == name_of(&DelegationStatusCode::Restricted)
        {
            return Ok(Err(ToolError::new(
                ErrorCode::AuthorityDenied,
                "the worker is restricted",
            )
            .with_detail("reason", "restricted")));
        }
        if !matches!(
            plan.initial,
            DeliveryState::Queued | DeliveryState::QueuedNextTurn
        ) {
            return Ok(Err(ToolError::new(
                ErrorCode::InternalError,
                "a send is accepted queued",
            )));
        }
        if let Some(reply) = &req.reply_to {
            let ok = self.delegation_message(reply.as_str())?.is_some_and(|m| {
                m.root_id == root.id
                    && (m.recipient_worker_id == principal.id || m.sender_worker_id == principal.id)
            });
            if !ok {
                return Ok(Err(ToolError::new(ErrorCode::NotFound, "unknown reply_to")));
            }
        }
        let message = self.new_message_id()?;
        let row = MessageRow {
            id: message.to_string(),
            root_id: root.id.clone(),
            operation_id: None,
            sender_worker_id: principal.id.clone(),
            recipient_worker_id: recipient.id.clone(),
            kind: name_of(&req.kind),
            reply_to: req.reply_to.as_ref().map(ToString::to_string),
            body: redact(&req.text),
            delivery_policy: name_of(&req.delivery),
            target_run_id: plan.target_run.as_ref().map(ToString::to_string),
            delivery_state: name_of(&plan.initial),
            recipient_run_id: None,
            queued_at: now,
            submitted_at: None,
            confirmed_at: None,
            presented_at: None,
            acknowledged_at: None,
            updated_at: now,
        };
        let full = SendReceipt {
            receipt,
            message_id: message.clone(),
            delivery: plan.initial,
            target_run_id: plan.target_run.clone(),
        };
        Ok(Ok(Effect {
            receipt: serde_json::to_value(&full)?,
            target: Some(target),
            worker: None,
            run: None,
            message_row: Some(PendingMessage {
                id: message,
                row,
                direction,
                kind: req.kind,
            }),
            seq: 0,
            complete_now: false,
        }))
    }

    /// `target` must be a strict descendant of `principal` in the same root.
    fn descendant_check(
        &self,
        principal: &WorkerRow,
        target: &WorkerId,
    ) -> Result<Verdict<WorkerRow>> {
        let row = match self.delegation_worker(target.as_str())? {
            Some(r) if r.root_id == principal.root_id => r,
            _ => return Ok(Err(ToolError::new(ErrorCode::NotFound, "unknown worker"))),
        };
        let subtree = self.delegation_subtree(&worker_id(&principal.id)?)?;
        if row.id == principal.id || !subtree.contains(target) {
            // The caller itself or one of its ancestors (or a sibling branch): never.
            let ancestor = self.is_ancestor(&row.id, &principal.id)?;
            if row.id == principal.id || ancestor {
                return Ok(Err(ToolError::new(
                    ErrorCode::AuthorityDenied,
                    "a worker cannot cancel or close itself or an ancestor",
                )
                .with_detail("reason", "ancestor")));
            }
            return Ok(Err(ToolError::new(ErrorCode::NotFound, "unknown worker")));
        }
        if row.state == name_of(&WorkerState::Closed) {
            return Ok(Err(ToolError::new(
                ErrorCode::WorkerClosed,
                "the worker is closed",
            )));
        }
        Ok(Ok(row))
    }

    /// Whether `ancestor` is above `of` in its tree.
    fn is_ancestor(&self, ancestor: &str, of: &str) -> Result<bool> {
        let mut cur = self.delegation_worker(of)?.and_then(|w| w.parent_id);
        while let Some(p) = cur {
            if p == ancestor {
                return Ok(true);
            }
            cur = self.delegation_worker(&p)?.and_then(|w| w.parent_id);
        }
        Ok(false)
    }

    fn audit_notice(&self, target: &WorkerRow, run: Option<RunId>, text: String) -> Result<i64> {
        let thread = self.event_thread(target)?;
        Ok(self
            .publish_in_txn(&PublishEvent {
                thread,
                provider_thread: None,
                envelope: Envelope::new(Event::Notice { text }),
                links: vec![LinkSpec {
                    worker: worker_id(&target.id)?,
                    run,
                    category: EventCategory::Lifecycle,
                    invocation: None,
                }],
                worker_states: Vec::new(),
                run_states: Vec::new(),
                aliases: Vec::new(),
            })?
            .seq)
    }

    #[allow(clippy::too_many_arguments)]
    fn accept_cancel(
        &self,
        a: &Accept,
        principal: &WorkerRow,
        _root: &RootRow,
        target: &WorkerId,
        expected_run: Option<&RunId>,
        receipt: MutationReceipt,
        _now: i64,
    ) -> Result<Verdict<Effect>> {
        let row = match self.descendant_check(principal, target)? {
            Ok(r) => r,
            Err(e) => return Ok(Err(e)),
        };
        let active = self
            .active_delegation_run(&row.id)?
            .map(|r| run_id(&r.id))
            .transpose()?;
        if let Some(expected) = expected_run {
            if active.as_ref() != Some(expected) {
                return Ok(Err(ToolError::new(
                    ErrorCode::StaleRun,
                    "the expected run is not the worker's current run",
                )
                .with_detail("run_id", expected.as_str())));
            }
        }
        let bound = &a.resolution.bound_runs;
        let run = active.filter(|r| bound.contains(r));
        let state = if bound.is_empty() {
            CancelState::Idle
        } else {
            CancelState::Stopping
        };
        let seq = self.audit_notice(&row, run.clone(), "Cancel requested".to_owned())?;
        let full = CancelReceipt {
            receipt,
            worker_id: target.clone(),
            run_id: run,
            state,
        };
        Ok(Ok(Effect {
            receipt: serde_json::to_value(&full)?,
            target: Some(target.clone()),
            worker: None,
            run: None,
            message_row: None,
            seq,
            complete_now: bound.is_empty(),
        }))
    }

    fn accept_close(
        &self,
        principal: &WorkerRow,
        _root: &RootRow,
        target: &WorkerId,
        receipt: MutationReceipt,
        _now: i64,
    ) -> Result<Verdict<Effect>> {
        let row = match self.descendant_check(principal, target)? {
            Ok(r) => r,
            Err(e) => return Ok(Err(e)),
        };
        let seq = self.audit_notice(&row, None, "Close requested".to_owned())?;
        let full = CloseReceipt {
            receipt,
            worker_id: target.clone(),
            state: CloseState::Closing,
        };
        Ok(Ok(Effect {
            receipt: serde_json::to_value(&full)?,
            target: Some(target.clone()),
            worker: None,
            run: None,
            message_row: None,
            seq,
            complete_now: false,
        }))
    }

    /// Inserts a message and its `DelegatedMessage` event (in the child's thread), linked to the
    /// sender and the recipient, in the caller's transaction.
    fn insert_message_and_event(
        &self,
        row: &MessageRow,
        direction: Direction,
        kind: MessageKind,
    ) -> Result<i64> {
        self.insert_delegation_message(row)?;
        let sender = self.worker_row(&worker_id(&row.sender_worker_id)?)?;
        let recipient = self.worker_row(&worker_id(&row.recipient_worker_id)?)?;
        let child = if direction == Direction::ParentToChild {
            &recipient
        } else {
            &sender
        };
        let thread = self.event_thread(child)?;
        let env = Envelope::new(Event::DelegatedMessage {
            message_id: message_id(&row.id)?,
            direction,
            kind,
            from: worker_id(&row.sender_worker_id)?,
            to: worker_id(&row.recipient_worker_id)?,
            reply_to: row.reply_to.as_deref().map(message_id).transpose()?,
            text: row.body.clone(),
            delivery: parse_name("delivery state", &row.delivery_state)?,
        });
        let links = vec![
            LinkSpec {
                worker: worker_id(&sender.id)?,
                run: None,
                category: EventCategory::Message,
                invocation: None,
            },
            LinkSpec {
                worker: worker_id(&recipient.id)?,
                run: row.target_run_id.as_deref().map(run_id).transpose()?,
                category: EventCategory::Message,
                invocation: None,
            },
        ];
        Ok(self
            .publish_in_txn(&PublishEvent {
                thread,
                provider_thread: None,
                envelope: env,
                links,
                worker_states: Vec::new(),
                run_states: Vec::new(),
                aliases: Vec::new(),
            })?
            .seq)
    }

    // ---- operation state ----

    /// One operation transition, through `operation_step`.
    fn step_operation(
        &self,
        op: &OperationId,
        to: OperationState,
        error: Option<&ToolError>,
        now: i64,
    ) -> Result<()> {
        let row = self
            .delegation_operation(op.as_str())?
            .ok_or_else(|| StoreError::NotFound(op.to_string()))?;
        let from: OperationState = parse_name("dispatch state", &row.dispatch_state)?;
        operation_step(from, to).map_err(|_| illegal("operation"))?;
        // Every stored error is scrubbed whole (message and every string of its nested details):
        // it is replayed to callers verbatim.
        let error_json = error
            .map(scrub_tool_error)
            .transpose()?
            .map(|e| serde_json::to_string(&e))
            .transpose()?;
        self.set_delegation_dispatch_state(
            op.as_str(),
            &name_of(&to),
            error_json.as_deref(),
            None,
            now,
        )
    }

    /// One message transition, through `delivery_step`; `false` (and nothing changed) when it is
    /// not a legal step from the message's current state.
    fn try_step_message(
        &self,
        m: &str,
        to: DeliveryState,
        recipient_run: Option<&str>,
        now: i64,
    ) -> Result<bool> {
        let Some(row) = self.delegation_message(m)? else {
            return Ok(false);
        };
        let from: DeliveryState = parse_name("delivery state", &row.delivery_state)?;
        if delivery_step(from, to).is_err() {
            return Ok(false);
        }
        self.set_delegation_message_delivery(m, &name_of(&to), recipient_run, now)?;
        Ok(true)
    }

    fn is_parent_to_child(&self, m: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM delegation_messages m
                 JOIN delegation_workers r ON r.id = m.recipient_worker_id
                 WHERE m.id = ?1 AND r.parent_id = m.sender_worker_id)",
            params![m],
            |r| r.get(0),
        )?)
    }

    fn messages_of_operation(&self, op: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM delegation_messages WHERE operation_id = ?1 ORDER BY rowid")?;
        let rows = stmt.query_map(params![op], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<String>>>()?)
    }

    /// Fails a not-yet-reserved operation with `error`: its queued message is cancelled, and a
    /// reserved run (and the starting worker of a spawn) fails with it.
    fn invalidate_operation(
        &self,
        row: &OperationRow,
        error: &ToolError,
        now: i64,
    ) -> Result<Vec<MessageId>> {
        let op = operation_id(&row.id)?;
        let stored = error.clone().with_detail("operation_id", row.id.as_str());
        self.step_operation(&op, OperationState::Failed, Some(&stored), now)?;
        let mut cancelled = Vec::new();
        for m in self.messages_of_operation(&row.id)? {
            // Only a delivery that would drive a child is cancelled; a child→parent entry is
            // mailbox data and stays as it is (contract §6).
            if !self.is_parent_to_child(&m)? {
                continue;
            }
            if self.try_step_message(&m, DeliveryState::Cancelled, None, now)? {
                cancelled.push(message_id(&m)?);
            }
        }
        if let Some(run) = &row.run_id {
            if let Some(r) = self.delegation_run(run)? {
                if r.state == name_of(&RunState::Reserved) {
                    self.set_delegation_run_state(
                        run,
                        &name_of(&RunState::Failed),
                        None,
                        Some(error.code.as_str()),
                        None,
                        now,
                    )?;
                    let w = self.worker_row(&worker_id(&r.worker_id)?)?;
                    if w.state == name_of(&WorkerState::Starting) {
                        self.set_delegation_worker_state(
                            &w.id,
                            &name_of(&WorkerState::Failed),
                            None,
                            now,
                        )?;
                    }
                }
            }
        }
        Ok(cancelled)
    }

    // ---- step 2: reserve dispatch ----

    /// Write protocol step 2: reserves the dispatch of an accepted operation, after rechecking
    /// the persisted epoch and the target. A stale epoch or a closed target fails the operation
    /// (committed) and is returned as the refusal. After this commits, recovery is conservative:
    /// a restart makes it `unknown`, never re-dispatched.
    pub fn reserve_delegation_dispatch(&self, op: &OperationId) -> Result<Verdict<Reserved>> {
        self.atomically(|s| s.reserve_in_txn(op))
    }

    fn reserve_in_txn(&self, op: &OperationId) -> Result<Verdict<Reserved>> {
        let Some(row) = self.delegation_operation(op.as_str())? else {
            return Ok(Err(ToolError::new(
                ErrorCode::NotFound,
                "unknown operation",
            )));
        };
        let stored = stored_operation(&row)?;
        if stored.dispatch != OperationState::Accepted {
            return Ok(Err(ToolError::invalid("operation", "not_accepted")
                .with_detail("operation_id", op.as_str())
                .with_detail("dispatch", stored.dispatch.as_str())));
        }
        let now = self.now();
        let root = self.root_row(&row.root_id)?;
        if root.authority_epoch != row.authority_epoch {
            let e = ToolError::new(ErrorCode::StaleEpoch, "the authority epoch changed");
            self.invalidate_operation(&row, &e, now)?;
            return Ok(Err(e.with_detail("operation_id", op.as_str())));
        }
        let target_closed = match &row.target_worker_id {
            Some(t) => self
                .delegation_worker(t)?
                .is_some_and(|w| w.state == name_of(&WorkerState::Closed)),
            None => false,
        };
        if target_closed && stored.kind != OperationKind::Close {
            let e = ToolError::new(ErrorCode::WorkerClosed, "the worker is closed");
            self.invalidate_operation(&row, &e, now)?;
            return Ok(Err(e.with_detail("operation_id", op.as_str())));
        }
        self.step_operation(op, OperationState::DispatchReserved, None, now)?;
        let messages = self.messages_of_operation(&row.id)?;
        for m in &messages {
            if !self.try_step_message(m, DeliveryState::DispatchReserved, None, now)? {
                return Err(illegal("a reserved send whose message is not queued"));
            }
        }
        let mut operation = stored;
        operation.dispatch = OperationState::DispatchReserved;
        Ok(Ok(Reserved {
            operation,
            target: row.target_worker_id.as_deref().map(worker_id).transpose()?,
            run: row.run_id.as_deref().map(run_id).transpose()?,
            message: messages.first().map(|m| message_id(m)).transpose()?,
        }))
    }

    // ---- step 4: record facts ----

    /// Write protocol step 4: records an observed dispatch fact. A send's message follows
    /// (`submitted`, `failed`, `unknown`) where `delivery_step` allows; a spawn's reserved run
    /// fails or becomes `unknown` with it. Never translates a pipe write into "consumed".
    pub fn record_delegation_dispatch(
        &self,
        op: &OperationId,
        fact: &DispatchFact,
    ) -> Result<OperationState> {
        self.atomically(|s| {
            let now = s.now();
            let row = s
                .delegation_operation(op.as_str())?
                .ok_or_else(|| StoreError::NotFound(op.to_string()))?;
            let (to, error, message) = match fact {
                DispatchFact::Submitted => (
                    OperationState::Submitted,
                    None,
                    Some(DeliveryState::Submitted),
                ),
                DispatchFact::Confirmed => (OperationState::Confirmed, None, None),
                DispatchFact::Failed(e) => {
                    // Scrubbed whole when stored (`step_operation`).
                    let scrubbed = e.clone().with_detail("operation_id", op.as_str());
                    (
                        OperationState::Failed,
                        Some(scrubbed),
                        Some(DeliveryState::Failed),
                    )
                }
                DispatchFact::Unknown => {
                    (OperationState::Unknown, None, Some(DeliveryState::Unknown))
                }
            };
            s.step_operation(op, to, error.as_ref(), now)?;
            if let Some(m) = message {
                for id in s.messages_of_operation(&row.id)? {
                    s.try_step_message(&id, m, None, now)?;
                }
            }
            if let Some(run) = &row.run_id {
                let r = s.delegation_run(run)?;
                let pending = r.as_ref().is_some_and(|r| {
                    r.state == name_of(&RunState::Reserved)
                        || r.state == name_of(&RunState::Starting)
                });
                match fact {
                    DispatchFact::Failed(e) if pending => s.set_delegation_run_state(
                        run,
                        &name_of(&RunState::Failed),
                        None,
                        Some(e.code.as_str()),
                        None,
                        now,
                    )?,
                    DispatchFact::Unknown if pending => s.set_delegation_run_state(
                        run,
                        &name_of(&RunState::Unknown),
                        None,
                        None,
                        None,
                        now,
                    )?,
                    _ => {}
                }
            }
            Ok(to)
        })
    }

    /// Records a delivery fact for a message through `delivery_step`. `misdelivered` needs the
    /// run it was observed in, which must differ from the run it targeted; `provider_confirmed`
    /// of a targeted message must be in that run (else it is `misdelivered`).
    pub fn record_delegation_delivery(
        &self,
        m: &MessageId,
        to: DeliveryState,
        recipient_run: Option<&RunId>,
    ) -> Result<DeliveryState> {
        self.atomically(|s| {
            let row = s
                .delegation_message(m.as_str())?
                .ok_or_else(|| StoreError::NotFound(m.to_string()))?;
            let from: DeliveryState = parse_name("delivery state", &row.delivery_state)?;
            delivery_step(from, to).map_err(|_| illegal("delivery"))?;
            let observed = recipient_run.map(RunId::as_str);
            match to {
                DeliveryState::Misdelivered => {
                    if observed.is_none() || observed == row.target_run_id.as_deref() {
                        return Err(illegal("misdelivered needs another run"));
                    }
                }
                DeliveryState::ProviderConfirmed => {
                    if let (Some(t), Some(o)) = (row.target_run_id.as_deref(), observed) {
                        if t != o {
                            return Err(illegal("confirmed in another run is misdelivered"));
                        }
                    }
                }
                _ => {}
            }
            let now = s.now();
            s.set_delegation_message_delivery(m.as_str(), &name_of(&to), observed, now)?;
            Ok(to)
        })
    }

    /// Moves a run on (never out of a terminal state, never back to `reserved`).
    pub fn advance_delegation_run(
        &self,
        run: &RunId,
        to: RunState,
        native_turn: Option<&str>,
    ) -> Result<()> {
        self.atomically(|s| {
            let row = s
                .delegation_run(run.as_str())?
                .ok_or_else(|| StoreError::NotFound(run.to_string()))?;
            let from: RunState = parse_name("run state", &row.state)?;
            if from.is_terminal() || to == RunState::Reserved {
                return Err(illegal("run"));
            }
            let now = s.now();
            s.set_delegation_run_state(run.as_str(), &name_of(&to), native_turn, None, None, now)
        })
    }

    // ---- step 5: complete ----

    /// Write protocol step 5, in one transaction: the run's terminal state, the worker's state,
    /// the terminal event with its links, and the parent's mailbox notification (a `report`
    /// carrying the bounded, scrubbed result) with its own event and links.
    pub fn complete_delegation_run(&self, c: &Completion) -> Result<Completed> {
        self.atomically(|s| {
            if !c.state.is_terminal() {
                return Err(illegal("a completion needs a terminal run state"));
            }
            let run = s
                .delegation_run(c.run.as_str())?
                .ok_or_else(|| StoreError::NotFound(c.run.to_string()))?;
            let from: RunState = parse_name("run state", &run.state)?;
            if from.is_terminal() {
                return Err(illegal("the run already ended"));
            }
            let now = s.now();
            s.set_delegation_run_state(
                c.run.as_str(),
                &name_of(&c.state),
                None,
                c.reason.as_deref(),
                c.usage_json.as_deref(),
                now,
            )?;
            let worker = s.worker_row(&worker_id(&run.worker_id)?)?;
            s.set_delegation_worker_state(&worker.id, &name_of(&c.worker_state), None, now)?;
            let event = s.publish_in_txn(&c.event)?;
            let mut notification = None;
            if let (Some(result), Some(parent)) = (&c.result, &worker.parent_id) {
                let message = s.new_message_id()?;
                let row = MessageRow {
                    id: message.to_string(),
                    root_id: worker.root_id.clone(),
                    operation_id: None,
                    sender_worker_id: worker.id.clone(),
                    recipient_worker_id: parent.clone(),
                    kind: name_of(&MessageKind::Report),
                    reply_to: None,
                    body: cut(&redact(result), MAX_RESULT_BYTES),
                    delivery_policy: name_of(&Delivery::Auto),
                    target_run_id: None,
                    delivery_state: name_of(&DeliveryState::Queued),
                    recipient_run_id: None,
                    queued_at: now,
                    submitted_at: None,
                    confirmed_at: None,
                    presented_at: None,
                    acknowledged_at: None,
                    updated_at: now,
                };
                let seq = s.insert_message_and_event(
                    &row,
                    Direction::ChildToParent,
                    MessageKind::Report,
                )?;
                notification = Some((message, seq));
            }
            Ok(Completed {
                event,
                notification,
            })
        })
    }

    // ---- step 6: revoke ----

    /// Write protocol step 6, in one transaction (contract §6): bumps the root's authority epoch
    /// when the event revokes, fails every not-yet-reserved operation of the tree with
    /// `stale_epoch`, cancels queued parent→child sends, releases the continuation reservation,
    /// marks out-of-ceiling workers `restricted` and closes a closed subtree. The returned effect
    /// names the tokens to kill and the processes to stop (the caller does that, in memory).
    pub fn revoke_delegation(
        &self,
        root_worker: &WorkerId,
        event: &RevocationEvent,
    ) -> Result<Revoked> {
        self.atomically(|s| s.revoke_in_txn(root_worker, event))
    }

    fn revoke_in_txn(&self, root_worker: &WorkerId, event: &RevocationEvent) -> Result<Revoked> {
        let root = self
            .delegation_root_of_worker(root_worker)?
            .filter(|r| r.root_worker_id == root_worker.as_str())
            .ok_or_else(|| StoreError::NotFound(root_worker.to_string()))?;
        let workers = self.delegation_workers_of_root(&root.id)?;
        // Every target the event names must belong to this root, checked before any mutation:
        // a foreign worker would otherwise be closed under another root's epoch bump.
        if let RevocationEvent::Close(w) | RevocationEvent::GenerationExited(w) = event {
            if !workers.iter().any(|r| r.id == w.as_str()) {
                return Err(StoreError::Failed(format!(
                    "worker {w} does not belong to root {}",
                    root.id
                )));
            }
        }
        let mut tree = Vec::with_capacity(workers.len());
        for w in &workers {
            tree.push((
                worker_id(&w.id)?,
                w.parent_id.as_deref().map(worker_id).transpose()?,
                w.task_mode
                    .as_deref()
                    .map(|m| parse_name::<TaskMode>("task mode", m))
                    .transpose()?,
            ));
        }
        let effect = revocation(event, &tree);
        let now = self.now();
        let mut out = Revoked {
            authority_epoch: u64_of(root.authority_epoch),
            effect: effect.clone(),
            invalidated: Vec::new(),
            cancelled: Vec::new(),
            closed: Vec::new(),
            continuation_released: false,
        };
        if effect.bump {
            let revision = i64::from(matches!(event, RevocationEvent::RootRestriction { .. }));
            self.conn.execute(
                "UPDATE delegation_roots SET authority_epoch = authority_epoch + 1,
                     policy_revision = policy_revision + ?2, continuation_reserved = 0,
                     updated_at = ?3
                 WHERE id = ?1",
                params![root.id, revision, now],
            )?;
            out.authority_epoch += 1;
            out.continuation_released = root.continuation_reserved;
            let accepted: Vec<OperationRow> = {
                let mut stmt = self.conn.prepare(
                    "SELECT id FROM delegation_operations WHERE root_id = ?1
                     AND dispatch_state = 'accepted' ORDER BY rowid",
                )?;
                let ids = stmt
                    .query_map(params![root.id], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut rows = Vec::new();
                for id in ids {
                    if let Some(r) = self.delegation_operation(&id)? {
                        rows.push(r);
                    }
                }
                rows
            };
            let e = ToolError::new(ErrorCode::StaleEpoch, "the authority epoch changed");
            for row in &accepted {
                out.cancelled
                    .extend(self.invalidate_operation(row, &e, now)?);
                out.invalidated.push(operation_id(&row.id)?);
            }
            // Queued parent→child deliveries of no operation (continuations) go too; child→parent
            // mailbox entries are data and stay as they are.
            let queued: Vec<String> = {
                let mut stmt = self.conn.prepare(
                    "SELECT m.id FROM delegation_messages m
                     JOIN delegation_workers r ON r.id = m.recipient_worker_id
                     WHERE m.root_id = ?1 AND m.delivery_state IN ('queued', 'queued_next_turn')
                       AND r.parent_id = m.sender_worker_id ORDER BY m.rowid",
                )?;
                let ids = stmt
                    .query_map(params![root.id], |r| r.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                ids
            };
            for m in queued {
                if self.try_step_message(&m, DeliveryState::Cancelled, None, now)? {
                    out.cancelled.push(message_id(&m)?);
                }
            }
        }
        for w in &effect.mark_restricted {
            self.set_delegation_worker_status(
                w.as_str(),
                &name_of(&DelegationStatusCode::Restricted),
                Some("outside the root's new mode ceiling"),
                "[]",
                now,
            )?;
        }
        if let RevocationEvent::Close(w) = event {
            for id in self.delegation_subtree(w)? {
                let row = self.worker_row(&id)?;
                if row.state != name_of(&WorkerState::Closed) {
                    self.set_delegation_worker_state(
                        &row.id,
                        &name_of(&WorkerState::Closed),
                        None,
                        now,
                    )?;
                    out.closed.push(id);
                }
            }
        }
        if matches!(event, RevocationEvent::RootStop) {
            self.conn.execute(
                "UPDATE delegation_roots SET task_state = 'stopped', updated_at = ?2
                 WHERE id = ?1 AND task_state = 'active'",
                params![root.id, now],
            )?;
        }
        Ok(out)
    }

    /// The user's next turn after a Stop: the root task is active again.
    pub fn resume_delegation_root(&self, root_worker: &WorkerId) -> Result<()> {
        let now = self.now();
        self.conn.execute(
            "UPDATE delegation_roots SET task_state = 'active', updated_at = ?2
             WHERE root_worker_id = ?1 AND task_state = 'stopped'",
            params![root_worker.as_str(), now],
        )?;
        Ok(())
    }

    // ---- atomic event + link publication ----

    /// Appends one authoritative event with **all** of its links, worker and run state updates
    /// and native aliases in one transaction, and returns its seq only after that commits. A
    /// root reader can therefore never advance past an event whose membership appears later.
    pub fn publish_delegation_event(&self, e: &PublishEvent) -> Result<Published> {
        self.atomically(|s| s.publish_in_txn(e))
    }

    /// Whether `thread` is this root's thread or the backing thread of one of its workers.
    fn thread_in_root(&self, thread: &str, root: &RootRow) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT ?2 = ?3 OR EXISTS (SELECT 1 FROM delegation_workers
                 WHERE root_id = ?1 AND backing_thread_id = ?2)",
            params![root.id, thread, root.root_thread_id],
            |r| r.get(0),
        )?)
    }

    fn provider_thread_in_root(&self, pt: &str, root: &RootRow) -> Result<bool> {
        let thread: Option<String> = self
            .conn
            .query_row(
                "SELECT thread_id FROM provider_threads WHERE id = ?1",
                params![pt],
                |r| r.get(0),
            )
            .optional()?;
        match thread {
            Some(t) => self.thread_in_root(&t, root),
            None => Ok(false),
        }
    }

    fn event_in_root(&self, seq: i64, root: &RootRow) -> Result<bool> {
        let thread: Option<String> = self
            .conn
            .query_row(
                "SELECT thread_id FROM events WHERE seq = ?1",
                params![seq],
                |r| r.get(0),
            )
            .optional()?;
        match thread {
            Some(t) => self.thread_in_root(&t, root),
            None => Ok(false),
        }
    }

    /// `run` exists, belongs to a worker of `root`, and (when given) to `worker` itself.
    fn check_run_of(&self, run: &RunId, worker: Option<&WorkerId>, root: &RootRow) -> Result<()> {
        let Some(row) = self.delegation_run(run.as_str())? else {
            return Err(StoreError::NotFound(run.to_string()));
        };
        let owner = self.worker_row(&worker_id(&row.worker_id)?)?;
        if owner.root_id != root.id {
            return Err(StoreError::Failed(format!(
                "run {run} does not belong to root {}",
                root.id
            )));
        }
        if let Some(w) = worker {
            if owner.id != w.as_str() {
                return Err(StoreError::Failed(format!(
                    "run {run} does not belong to worker {w}"
                )));
            }
        }
        Ok(())
    }

    /// An alias's worker and provider thread both belong to `root`.
    fn check_alias(&self, a: &NewAlias, root: &RootRow) -> Result<()> {
        if self.worker_row(&a.worker)?.root_id != root.id {
            return Err(StoreError::Failed(format!(
                "alias worker {} does not belong to root {}",
                a.worker, root.id
            )));
        }
        if !self.provider_thread_in_root(&a.provider_thread, root)? {
            return Err(StoreError::Failed(
                "an alias's provider thread is not this root's".into(),
            ));
        }
        Ok(())
    }

    fn publish_in_txn(&self, e: &PublishEvent) -> Result<Published> {
        let Some(first) = e.links.first() else {
            return Err(StoreError::Failed("a delegation event needs a link".into()));
        };
        let root_id = self.worker_row(&first.worker)?.root_id;
        let root = self.root_row(&root_id)?;
        let mut seen = HashSet::new();
        for l in &e.links {
            if self.worker_row(&l.worker)?.root_id != root.id {
                return Err(StoreError::Failed("an event's links span two roots".into()));
            }
            if !seen.insert(l.worker.clone()) {
                return Err(StoreError::Failed(
                    "a worker is linked twice to one event".into(),
                ));
            }
        }
        if !self.thread_in_root(&e.thread, &root)? {
            return Err(StoreError::Failed(
                "the event's thread is not this root's".into(),
            ));
        }
        // Ownership, not just existence (foreign keys prove only that): everything the event
        // names belongs to this root, checked before anything is written.
        if let Some(pt) = &e.provider_thread {
            if !self.provider_thread_in_root(pt, &root)? {
                return Err(StoreError::Failed(
                    "the event's provider thread is not this root's".into(),
                ));
            }
        }
        for l in &e.links {
            if let Some(r) = &l.run {
                self.check_run_of(r, Some(&l.worker), &root)?;
            }
        }
        for (w, _, _) in &e.worker_states {
            if self.worker_row(w)?.root_id != root.id {
                return Err(StoreError::Failed("a state update for another root".into()));
            }
        }
        for (r, _) in &e.run_states {
            self.check_run_of(r, None, &root)?;
        }
        for a in &e.aliases {
            self.check_alias(a, &root)?;
        }
        let seq = self.append_event(&e.thread, e.provider_thread.as_deref(), &e.envelope)?;
        #[cfg(test)]
        if let Some(hook) = BEFORE_LINKS.with(|h| h.borrow_mut().take()) {
            hook();
        }
        for l in &e.links {
            self.insert_delegation_event_link(&EventLinkRow {
                event_seq: seq,
                worker_id: l.worker.to_string(),
                root_id: root.id.clone(),
                run_id: l.run.as_ref().map(ToString::to_string),
                category: name_of(&l.category),
                invocation_id: l.invocation.clone(),
            })?;
        }
        let now = self.now();
        for (w, state, activity) in &e.worker_states {
            self.set_delegation_worker_state(
                w.as_str(),
                &name_of(state),
                activity.as_deref(),
                now,
            )?;
        }
        for (r, state) in &e.run_states {
            self.set_delegation_run_state(r.as_str(), &name_of(state), None, None, None, now)?;
        }
        for a in &e.aliases {
            self.insert_delegation_native_alias(&NativeAliasRow {
                provider_thread_id: a.provider_thread.clone(),
                process_generation: i64_of(a.process_generation),
                native_worker_id: a.native_worker_id.clone(),
                worker_id: a.worker.to_string(),
                created_at: now,
            })?;
        }
        Ok(Published {
            seq,
            root: root.id.clone(),
            root_worker: worker_id(&root.root_worker_id)?,
            stream_epoch: u64_of(root.stream_epoch),
        })
    }

    /// Historical relinking (plan §3 "Atomic visibility"): the backfilled links and aliases and a
    /// stream-epoch bump in one transaction, so every cursor issued before it expires
    /// (`cursor_expired`) instead of silently missing membership behind it. Returns the new
    /// stream epoch.
    pub fn relink_delegation_history(
        &self,
        root_worker: &WorkerId,
        links: &[(i64, LinkSpec)],
        aliases: &[NewAlias],
    ) -> Result<u64> {
        self.atomically(|s| {
            let root = s
                .delegation_root_of_worker(root_worker)?
                .filter(|r| r.root_worker_id == root_worker.as_str())
                .ok_or_else(|| StoreError::NotFound(root_worker.to_string()))?;
            let now = s.now();
            // Every link, event, run and alias is checked before anything is written.
            for (seq, l) in links {
                if s.worker_row(&l.worker)?.root_id != root.id {
                    return Err(StoreError::Failed("a relink for another root".into()));
                }
                if let Some(r) = &l.run {
                    s.check_run_of(r, Some(&l.worker), &root)?;
                }
                if !s.event_in_root(*seq, &root)? {
                    return Err(StoreError::Failed(
                        "a relinked event is not in this root's threads".into(),
                    ));
                }
            }
            for a in aliases {
                s.check_alias(a, &root)?;
            }
            for (seq, l) in links {
                s.conn.execute(
                    "INSERT OR IGNORE INTO delegation_event_links
                         (event_seq, worker_id, root_id, run_id, category, invocation_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        seq,
                        l.worker.as_str(),
                        root.id,
                        l.run.as_ref().map(RunId::as_str),
                        name_of(&l.category),
                        l.invocation
                    ],
                )?;
            }
            for a in aliases {
                s.insert_delegation_native_alias(&NativeAliasRow {
                    provider_thread_id: a.provider_thread.clone(),
                    process_generation: i64_of(a.process_generation),
                    native_worker_id: a.native_worker_id.clone(),
                    worker_id: a.worker.to_string(),
                    created_at: now,
                })?;
            }
            s.conn.execute(
                "UPDATE delegation_roots SET stream_epoch = stream_epoch + 1, updated_at = ?2
                 WHERE id = ?1",
                params![root.id, now],
            )?;
            Ok(u64_of(root.stream_epoch) + 1)
        })
    }

    // ---- pages ----

    /// One read or wait page (contract §3): units in committed seq order, one per seq (a
    /// message's seq is a message only), selected by agent_core's reservation and measured by its
    /// settle step, cursor bound to the filter and the stream epoch. Read-only, in one read
    /// snapshot, so it runs on any connection and never waits on the writer.
    ///
    /// `PageOut::presented` lists the messages this page returns to their recipient that were
    /// never presented before; ACK eligibility needs them marked
    /// ([`Store::mark_delegation_presented`]), or use
    /// [`Store::read_and_present_delegation_page`] on the writer to do both at once.
    pub fn read_delegation_page(&self, q: &PageQuery) -> Result<Verdict<PageOut>> {
        let tx = self.scope(rusqlite::TransactionBehavior::Deferred)?;
        let out = self.page_in_txn(q)?;
        tx.commit()?;
        Ok(out)
    }

    /// [`Store::read_delegation_page`] and the presentation marks of its messages, in one
    /// transaction: what the writer runs for a real read or wait.
    pub fn read_and_present_delegation_page(&self, q: &PageQuery) -> Result<Verdict<PageOut>> {
        self.atomically(|s| {
            let mut out = match s.page_in_txn(q)? {
                Ok(out) => out,
                Err(e) => return Ok(Err(e)),
            };
            out.presented = s.present_in_txn(&q.filter.caller, &out.presented)?;
            Ok(Ok(out))
        })
    }

    /// Marks messages presented to `caller` (only those it is the recipient of, the first time
    /// only); returns the ones this call marked.
    pub fn mark_delegation_presented(
        &self,
        caller: &WorkerId,
        ids: &[MessageId],
    ) -> Result<Vec<MessageId>> {
        self.atomically(|s| s.present_in_txn(caller, ids))
    }

    fn present_in_txn(&self, caller: &WorkerId, ids: &[MessageId]) -> Result<Vec<MessageId>> {
        let now = self.now();
        let mut out = Vec::new();
        for id in ids {
            let mine = self
                .delegation_message(id.as_str())?
                .is_some_and(|m| m.recipient_worker_id == caller.as_str());
            if mine && self.mark_delegation_message_presented(id.as_str(), now)? {
                out.push(id.clone());
            }
        }
        Ok(out)
    }

    fn page_in_txn(&self, q: &PageQuery) -> Result<Verdict<PageOut>> {
        let Some(root) = self
            .delegation_root_of_worker(&q.root_worker)?
            .filter(|r| r.root_worker_id == q.root_worker.as_str())
        else {
            return Ok(Err(ToolError::new(ErrorCode::NotFound, "unknown root")));
        };
        let stream_epoch = u64_of(root.stream_epoch);
        let last = match &q.after {
            None => 0,
            Some(c) => match decode_cursor(c, &q.filter, stream_epoch) {
                Ok(p) if p.root == q.root_worker => p.last_seq,
                Ok(_) => return Ok(Err(ToolError::invalid("after", "root"))),
                Err(ErrorCode::CursorExpired) => {
                    return Ok(Err(ToolError::new(
                        ErrorCode::CursorExpired,
                        "the stream was reset; read again with no cursor",
                    )))
                }
                Err(_) => return Ok(Err(ToolError::invalid("after", "filter"))),
            },
        };
        let scope: Vec<&str> = q.scope.iter().map(WorkerId::as_str).collect();
        let scope_json = serde_json::to_string(&scope)?;
        let scope_set: HashSet<&str> = scope.iter().copied().collect();
        let caller = q.filter.caller.as_str();
        let mut units: Vec<PageUnit> = Vec::new();
        let mut json_sum = q.snapshot.as_ref().map_or(0, json_len);
        let mut transport_sum = 0usize;
        {
            // Membership is decided in SQL, so every row returned is a unit: the scope's links,
            // plus (mailbox) message links of the caller whose message it is the recipient of.
            // Both halves walk the `(worker_id, event_seq)` index, and the LIMIT bounds what one
            // page reads however long the root's unrelated history is.
            let mut stmt = self.conn.prepare(PAGE_SQL)?;
            let limit = i64::from(q.max_events).saturating_add(1);
            let mut rows = stmt.query(params![
                root.id,
                i64_of(last),
                scope_json,
                q.filter.mailbox,
                caller,
                limit
            ])?;
            while let Some(r) = rows.next()? {
                let seq: i64 = r.get(0)?;
                #[cfg(test)]
                PAGE_ROWS.with(|c| c.set(c.get() + 1));
                let Some(unit) = self.page_unit(seq, &scope_set, caller, q.filter.mailbox)? else {
                    continue;
                };
                let res = Reservation::unit(&unit.value);
                json_sum = json_sum.saturating_add(res.json);
                transport_sum = transport_sum.saturating_add(res.transport);
                units.push(unit);
                // One unit past what the page can take is enough to know it is truncated.
                if units.len() > q.max_events as usize
                    || json_sum > q.max_bytes as usize
                    || transport_sum > q.budget
                {
                    break;
                }
            }
        }
        let digest = agent_core::delegation::page::filter_digest(&q.filter);
        let cursor_at = |seq: u64| {
            encode_cursor(&CursorPayload {
                root: q.root_worker.clone(),
                stream_epoch,
                last_seq: seq,
                filter_digest: digest,
            })
        };
        let render = |parts: &PageParts<'_>| {
            json!({
                "api_version": API_VERSION,
                "snapshot": parts.snapshot,
                "events": parts.events,
                "messages": parts.messages,
                "next_cursor": cursor_at(parts.last_seq.unwrap_or(last)),
                "truncated": parts.truncated,
                "epoch_changed": false,
                "ack": q.ack,
            })
        };
        let envelope = Reservation::max_envelope(&json!({
            "api_version": API_VERSION,
            "snapshot": q.snapshot,
            "events": [],
            "messages": [],
            "next_cursor": "A".repeat(agent_core::delegation::ids::MAX_CURSOR_BYTES),
            "truncated": true,
            "epoch_changed": true,
            "ack": q.ack,
        }));
        let assembled = match assemble(
            envelope,
            q.snapshot.as_ref(),
            &units,
            q.max_events,
            q.max_bytes,
            q.budget,
            render,
        ) {
            Ok(a) => a,
            Err(e) => return Ok(Err(e)),
        };
        let mut presented = Vec::new();
        for u in units.iter().take(assembled.taken) {
            if u.kind != UnitKind::Message {
                continue;
            }
            let to = u.value.get("to").and_then(Value::as_str);
            let id = u.value.get("message_id").and_then(Value::as_str);
            if let (Some(to), Some(id)) = (to, id) {
                let fresh = self
                    .delegation_message(id)?
                    .is_some_and(|m| m.presented_at.is_none());
                if to == caller && fresh {
                    presented.push(message_id(id)?);
                }
            }
        }
        Ok(Ok(PageOut {
            page: assembled.page,
            last_seq: assembled.last_seq,
            truncated: assembled.truncated,
            presented,
        }))
    }

    /// The page unit of one seq for this filter, or `None` if it is not in it.
    fn page_unit(
        &self,
        seq: i64,
        scope: &HashSet<&str>,
        caller: &str,
        mailbox: bool,
    ) -> Result<Option<PageUnit>> {
        let (at, json): (i64, String) = self.conn.query_row(
            "SELECT at, envelope_json FROM events WHERE seq = ?1",
            params![seq],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let env: Envelope = serde_json::from_str(&json)?;
        let links = self.delegation_event_links(seq)?;
        let in_scope = links.iter().find(|l| scope.contains(l.worker_id.as_str()));
        if let Event::DelegatedMessage { message_id: m, .. } = &env.event {
            let Some(row) = self.delegation_message(m.as_str())? else {
                return Ok(None);
            };
            if in_scope.is_none() && !(mailbox && row.recipient_worker_id == caller) {
                return Ok(None);
            }
            let msg = PageMessage {
                seq: u64_of(seq),
                message_id: m.clone(),
                from: worker_id(&row.sender_worker_id)?,
                to: worker_id(&row.recipient_worker_id)?,
                kind: parse_name("message kind", &row.kind)?,
                reply_to: row.reply_to.as_deref().map(message_id).transpose()?,
                text: row.body.clone(),
                delivery: parse_name("delivery state", &row.delivery_state)?,
                recipient_run_id: row.recipient_run_id.as_deref().map(run_id).transpose()?,
                acknowledged: row.acknowledged_at.is_some(),
                at: rfc3339_ms(row.queued_at),
            };
            return Ok(Some(PageUnit {
                seq: u64_of(seq),
                kind: UnitKind::Message,
                value: serde_json::to_value(&msg)?,
            }));
        }
        let Some(link) = in_scope else {
            return Ok(None);
        };
        let (summary, state) = event_summary(&env);
        let event = PageEvent {
            seq: u64_of(seq),
            worker_id: worker_id(&link.worker_id)?,
            run_id: link.run_id.as_deref().map(run_id).transpose()?,
            at: rfc3339_ms(at),
            category: parse_name("event category", &link.category)?,
            state,
            summary,
        };
        Ok(Some(PageUnit {
            seq: u64_of(seq),
            kind: UnitKind::Event,
            value: serde_json::to_value(&event)?,
        }))
    }

    /// The cursor a reader holds after `seq` for `filter` (e.g. right after a publication).
    pub fn delegation_cursor(
        &self,
        root_worker: &WorkerId,
        filter: &PageFilter,
        seq: u64,
    ) -> Result<Cursor> {
        let root = self
            .delegation_root_of_worker(root_worker)?
            .ok_or_else(|| StoreError::NotFound(root_worker.to_string()))?;
        Ok(encode_cursor(&CursorPayload {
            root: root_worker.clone(),
            stream_epoch: u64_of(root.stream_epoch),
            last_seq: seq,
            filter_digest: agent_core::delegation::page::filter_digest(filter),
        }))
    }

    // ---- mailbox ACK ----

    /// A durable ACK (contract §3): idempotent on its key (a retry replays the stored outcome and
    /// never consumes newer messages). Each id is judged alone by `ack_eligible`: an invisible or
    /// unknown id is `not_found`, one not yet presented is `invalid_argument`
    /// (`not_presented`), and a rejected id consumes nothing else.
    pub fn acknowledge_delegation_messages(&self, a: &AckRequest) -> Result<Verdict<AckResult>> {
        self.atomically(|s| s.ack_in_txn(a))
    }

    fn ack_in_txn(&self, a: &AckRequest) -> Result<Verdict<AckResult>> {
        let Some(principal) = self.delegation_worker(a.principal.as_str())? else {
            return Ok(Err(ToolError::new(
                ErrorCode::NotFound,
                "unknown principal",
            )));
        };
        let root = self.root_row(&principal.root_id)?;
        let (kind, canonical) =
            match self.idempotency_front(&principal, &root, &a.request, &HashSet::new())? {
                Front::Done(Ok(outcome)) => return Ok(Ok(AckResult::Replay(outcome))),
                Front::Done(Err(e)) => return Ok(Err(e)),
                Front::New { kind, canonical } => (kind, canonical),
            };
        let ToolRequest::Read(read) = &a.request else {
            return Ok(Err(ToolError::new(
                ErrorCode::InvalidArgument,
                "not an acknowledgement",
            )));
        };
        if kind != OperationKind::Ack {
            return Ok(Err(ToolError::new(
                ErrorCode::InvalidArgument,
                "not an acknowledgement",
            )));
        }
        let ids = read.acknowledge.clone().unwrap_or_default();
        let now = self.now();
        let caller = worker_id(&principal.id)?;
        let mut acknowledged = Vec::new();
        let mut rejected = Vec::new();
        let mut done = BTreeSet::new();
        for id in ids {
            if !done.insert(id.clone()) {
                continue;
            }
            let row = self
                .delegation_message(id.as_str())?
                .filter(|m| m.root_id == root.id);
            let verdict = match &row {
                None => Err(ErrorCode::NotFound),
                Some(m) => ack_eligible(
                    &worker_id(&m.recipient_worker_id)?,
                    &caller,
                    m.presented_at.is_some(),
                    m.acknowledged_at.is_some(),
                ),
            };
            match verdict {
                Ok(true) => {
                    self.conn.execute(
                        "UPDATE delegation_messages SET acknowledged_at = ?2, updated_at = ?2
                         WHERE id = ?1 AND acknowledged_at IS NULL",
                        params![id.as_str(), now],
                    )?;
                    acknowledged.push(id);
                }
                Ok(false) => acknowledged.push(id),
                Err(code) => rejected.push(AckRejection {
                    message_id: id,
                    code,
                }),
            }
        }
        let op = self.new_operation_id()?;
        let outcome = AckOutcome {
            receipt: MutationReceipt {
                api_version: API_VERSION,
                operation_id: op.clone(),
                authority_epoch: u64_of(root.authority_epoch),
                replayed: false,
                dispatch: OperationState::Accepted,
                warnings: Vec::new(),
            },
            acknowledged,
            rejected,
        };
        let receipt = serde_json::to_value(&outcome)?;
        let key = read
            .idempotency_key
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default();
        self.insert_delegation_operation(&OperationRow {
            id: op.to_string(),
            root_id: root.id.clone(),
            principal_worker_id: principal.id.clone(),
            kind: name_of(&kind),
            idempotency_key: key,
            authority_epoch: root.authority_epoch,
            canonical_request: canonical.as_str().to_owned(),
            resolution_json: serde_json::to_string(&Resolution::default())?,
            receipt_json: serde_json::to_string(&receipt)?,
            dispatch_state: name_of(&OperationState::Accepted),
            error_json: None,
            target_worker_id: None,
            run_id: None,
            created_at: now,
            updated_at: now,
        })?;
        // Applied in this transaction: nothing is dispatched, so it is confirmed at once (and a
        // later revocation never turns it into a failure).
        self.step_operation(&op, OperationState::DispatchReserved, None, now)?;
        self.step_operation(&op, OperationState::Confirmed, None, now)?;
        Ok(Ok(AckResult::New(receipt)))
    }

    // ---- startup reconciliation ----

    /// Marks a clean shutdown (the last delegation write of a closing app).
    pub fn mark_delegation_clean_shutdown(&self) -> Result<()> {
        self.set_meta(CLEAN_SHUTDOWN_KEY, "1")
    }

    /// Startup reconciliation, in one transaction, before any delegation work: every
    /// non-terminal run and every live worker state becomes `unknown`, every reserved or
    /// submitted operation (`on_restart`) and message becomes `unknown`, continuation
    /// reservations are released, and after an abnormal restart (no clean-shutdown mark) every
    /// root gets a new stream epoch, so no old cursor skips a rolled-back tail. Nothing is ever
    /// retried.
    pub fn reconcile_delegation_after_restart(&self) -> Result<Reconciled> {
        self.atomically(|s| {
            let abnormal = s.meta(CLEAN_SHUTDOWN_KEY)?.is_none();
            s.delete_meta(CLEAN_SHUTDOWN_KEY)?;
            let now = s.now();
            let mut out = Reconciled {
                abnormal,
                ..Reconciled::default()
            };
            for id in s.ids(
                "SELECT id FROM delegation_runs
                 WHERE state IN ('reserved', 'starting', 'running', 'stopping') ORDER BY rowid",
            )? {
                s.set_delegation_run_state(&id, &name_of(&RunState::Unknown), None, Some("restart"), None, now)?;
                out.runs.push(run_id(&id)?);
            }
            for id in s.ids(
                "SELECT id FROM delegation_workers WHERE origin <> 'root'
                 AND state IN ('starting', 'running', 'waiting', 'stopping') ORDER BY rowid",
            )? {
                s.set_delegation_worker_state(&id, &name_of(&WorkerState::Unknown), None, now)?;
                out.workers.push(worker_id(&id)?);
            }
            for id in s.ids(
                "SELECT id FROM delegation_operations
                 WHERE dispatch_state IN ('dispatch_reserved', 'submitted') ORDER BY rowid",
            )? {
                let row = s
                    .delegation_operation(&id)?
                    .ok_or_else(|| StoreError::NotFound(id.clone()))?;
                let from: OperationState = parse_name("dispatch state", &row.dispatch_state)?;
                let to = on_restart(from);
                s.step_operation(&operation_id(&id)?, to, None, now)?;
                out.operations.push(operation_id(&id)?);
            }
            for id in s.ids(
                "SELECT id FROM delegation_messages
                 WHERE delivery_state IN ('dispatch_reserved', 'submitted') ORDER BY rowid",
            )? {
                if s.try_step_message(&id, DeliveryState::Unknown, None, now)? {
                    out.messages.push(message_id(&id)?);
                }
            }
            s.conn.execute(
                "UPDATE delegation_roots SET continuation_reserved = 0 WHERE continuation_reserved = 1",
                [],
            )?;
            if abnormal {
                for id in s.ids("SELECT id FROM delegation_roots ORDER BY rowid")? {
                    s.conn.execute(
                        "UPDATE delegation_roots SET stream_epoch = stream_epoch + 1, updated_at = ?2
                         WHERE id = ?1",
                        params![id, now],
                    )?;
                    out.roots.push(id);
                }
            }
            Ok(out)
        })
    }

    fn ids(&self, sql: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<String>>>()?)
    }

    fn ids_with(&self, sql: &str, p: &[&dyn rusqlite::ToSql]) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(p, |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<String>>>()?)
    }

    // ---- a child's native session ----

    /// Records a supervised child's native session id on its hidden backing thread's provider
    /// thread, so the native import (which skips a session some thread already holds) never
    /// lists the child's session as a visible thread. Returns the provider thread.
    pub fn record_child_native_session(
        &self,
        worker: &WorkerId,
        driver: Driver,
        model: &str,
        native_id: &str,
    ) -> Result<String> {
        self.atomically(|s| {
            let w = s.worker_row(worker)?;
            let Some(thread) = w.backing_thread_id.clone() else {
                return Err(StoreError::Failed(
                    "only a supervised child has a backing thread".into(),
                ));
            };
            let driver = name_of(&driver);
            let pts = s.provider_threads(&thread)?;
            if let Some(pt) = pts
                .iter()
                .find(|p| p.driver == driver && p.native_id.as_deref() == Some(native_id))
            {
                return Ok(pt.id.clone());
            }
            let pt = match pts
                .iter()
                .find(|p| p.driver == driver && p.native_id.is_none())
            {
                Some(p) => p.id.clone(),
                None => s.add_provider_thread(&thread, &driver, model)?,
            };
            s.set_native_id(&pt, native_id)?;
            s.set_active_provider_thread(&thread, &pt)?;
            Ok(pt)
        })
    }

    // ---- deletion ----

    /// Closes and revokes what deleting `thread` would take away, so the delete can proceed:
    /// for a root thread a Root Stop (epoch bump) and every descendant closed; for a child's
    /// backing thread a Close of its subtree. Active runs of those workers end `interrupted`.
    /// The caller stops their processes first (an observed exit); the store cannot.
    /// `None` when the thread has no delegation rows.
    pub fn close_and_revoke_for_delete(&self, thread: &str) -> Result<Option<Revoked>> {
        self.atomically(|s| {
            let now = s.now();
            let (mut revoked, workers) = if let Some(root) = s.delegation_root_of_thread(thread)? {
                let rw = worker_id(&root.root_worker_id)?;
                let mut r = s.revoke_in_txn(&rw, &RevocationEvent::RootStop)?;
                let all = s.delegation_subtree(&rw)?;
                for w in all.iter().skip(1) {
                    let row = s.worker_row(w)?;
                    if row.state != name_of(&WorkerState::Closed) {
                        s.set_delegation_worker_state(
                            &row.id,
                            &name_of(&WorkerState::Closed),
                            None,
                            now,
                        )?;
                        r.closed.push(w.clone());
                    }
                }
                (r, all.into_iter().skip(1).collect::<Vec<_>>())
            } else if let Some(w) = s.worker_of_backing_thread(thread)? {
                let root = s.root_row(&w.root_id)?;
                let id = worker_id(&w.id)?;
                let r = s.revoke_in_txn(
                    &worker_id(&root.root_worker_id)?,
                    &RevocationEvent::Close(id.clone()),
                )?;
                (r, s.delegation_subtree(&id)?)
            } else {
                return Ok(None);
            };
            for w in &workers {
                if let Some(run) = s.active_delegation_run(w.as_str())? {
                    s.set_delegation_run_state(
                        &run.id,
                        &name_of(&RunState::Interrupted),
                        None,
                        Some("closed for deletion"),
                        None,
                        now,
                    )?;
                }
            }
            revoked.closed.sort();
            revoked.closed.dedup();
            Ok(Some(revoked))
        })
    }

    fn worker_of_backing_thread(&self, thread: &str) -> Result<Option<WorkerRow>> {
        let id: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM delegation_workers WHERE backing_thread_id = ?1",
                params![thread],
                |r| r.get(0),
            )
            .optional()?;
        id.map(|id| self.worker_row(&worker_id(&id)?)).transpose()
    }

    /// Called by `delete_thread` inside its transaction: refuses while any delegated worker the
    /// delete would take is live (not closed, or with an active run), else removes that subtree's
    /// delegation rows (links, aliases, messages, operations, runs, workers, and for a root its
    /// root row). Returns the other hidden backing threads that go with it, for the caller to
    /// delete too (so none is left to surface in the sidebar).
    pub(crate) fn release_delegation_for_delete(&self, thread: &str) -> Result<Vec<String>> {
        let (workers, root_case, root) =
            if let Some(root) = self.delegation_root_of_thread(thread)? {
                let all = self.delegation_subtree(&worker_id(&root.root_worker_id)?)?;
                (all, true, root)
            } else if let Some(w) = self.worker_of_backing_thread(thread)? {
                let root = self.root_row(&w.root_id)?;
                (self.delegation_subtree(&worker_id(&w.id)?)?, false, root)
            } else {
                return Ok(Vec::new());
            };
        let live: Vec<&WorkerId> = workers
            .iter()
            .filter(|w| !root_case || w.as_str() != root.root_worker_id)
            .filter(|w| {
                let row = self.delegation_worker(w.as_str()).ok().flatten();
                let open = row.is_none_or(|r| r.state != "closed");
                let running = self
                    .active_delegation_run(w.as_str())
                    .ok()
                    .flatten()
                    .is_some();
                open || running
            })
            .collect();
        if !live.is_empty() {
            return Err(StoreError::Failed(format!(
                "the thread has {} live delegated worker(s); close and revoke them first",
                live.len()
            )));
        }
        let ws: Vec<&str> = workers.iter().map(WorkerId::as_str).collect();
        let w_json = serde_json::to_string(&ws)?;
        let mut threads = vec![thread.to_owned()];
        for w in &workers {
            if let Some(t) = self
                .delegation_worker(w.as_str())?
                .and_then(|r| r.backing_thread_id)
            {
                if !threads.contains(&t) {
                    threads.push(t);
                }
            }
        }
        let t_json = serde_json::to_string(&threads)?;
        const IN_W: &str = "(SELECT value FROM json_each(?1))";
        let runs = self.ids_with(
            &format!("SELECT id FROM delegation_runs WHERE worker_id IN {IN_W}"),
            &[&w_json],
        )?;
        let r_json = serde_json::to_string(&runs)?;
        let ops = self.ids_with(
            "SELECT id FROM delegation_operations
             WHERE principal_worker_id IN (SELECT value FROM json_each(?1))
                OR target_worker_id IN (SELECT value FROM json_each(?1))
                OR run_id IN (SELECT value FROM json_each(?2))",
            &[&w_json, &r_json],
        )?;
        let o_json = serde_json::to_string(&ops)?;
        let mut messages: BTreeSet<String> = self
            .ids_with(
                "SELECT id FROM delegation_messages
                 WHERE sender_worker_id IN (SELECT value FROM json_each(?1))
                    OR recipient_worker_id IN (SELECT value FROM json_each(?1))
                    OR target_run_id IN (SELECT value FROM json_each(?2))
                    OR recipient_run_id IN (SELECT value FROM json_each(?2))
                    OR operation_id IN (SELECT value FROM json_each(?3))",
                &[&w_json, &r_json, &o_json],
            )?
            .into_iter()
            .collect();
        loop {
            let m_json = serde_json::to_string(&messages)?;
            let replies = self.ids_with(
                "SELECT id FROM delegation_messages WHERE reply_to IN (SELECT value FROM json_each(?1))",
                &[&m_json],
            )?;
            let before = messages.len();
            messages.extend(replies);
            if messages.len() == before {
                break;
            }
        }
        let m_json = serde_json::to_string(&messages)?;
        self.conn.execute(
            "DELETE FROM delegation_event_links
             WHERE worker_id IN (SELECT value FROM json_each(?1))
                OR run_id IN (SELECT value FROM json_each(?2))
                OR event_seq IN (SELECT seq FROM events
                                 WHERE thread_id IN (SELECT value FROM json_each(?3)))",
            params![w_json, r_json, t_json],
        )?;
        self.conn.execute(
            "DELETE FROM delegation_native_aliases
             WHERE worker_id IN (SELECT value FROM json_each(?1))
                OR provider_thread_id IN (SELECT id FROM provider_threads
                                          WHERE thread_id IN (SELECT value FROM json_each(?2)))",
            params![w_json, t_json],
        )?;
        self.conn.execute(
            "DELETE FROM delegation_messages WHERE id IN (SELECT value FROM json_each(?1))",
            params![m_json],
        )?;
        self.conn.execute(
            "DELETE FROM delegation_operations WHERE id IN (SELECT value FROM json_each(?1))",
            params![o_json],
        )?;
        self.conn.execute(
            "DELETE FROM delegation_runs WHERE id IN (SELECT value FROM json_each(?1))",
            params![r_json],
        )?;
        self.conn.execute(
            "DELETE FROM delegation_workers WHERE id IN (SELECT value FROM json_each(?1))",
            params![w_json],
        )?;
        let now = self.now();
        if root_case {
            self.conn.execute(
                "DELETE FROM delegation_roots WHERE id = ?1",
                params![root.id],
            )?;
        } else {
            // Membership left the stream: older cursors must resynchronise.
            self.conn.execute(
                "UPDATE delegation_roots SET stream_epoch = stream_epoch + 1, updated_at = ?2
                 WHERE id = ?1",
                params![root.id, now],
            )?;
        }
        threads.remove(0);
        Ok(threads)
    }
}

/// A message waiting for its operation row.
#[derive(Debug, Clone)]
struct PendingMessage {
    id: MessageId,
    row: MessageRow,
    direction: Direction,
    kind: MessageKind,
}

/// What a per-tool acceptance produced.
struct Effect {
    receipt: Value,
    target: Option<WorkerId>,
    worker: Option<WorkerId>,
    run: Option<RunId>,
    /// A send's message, inserted once its operation exists.
    message_row: Option<PendingMessage>,
    /// The audit event's seq (a send's is its message event's).
    seq: i64,
    /// Nothing to dispatch: the operation completes at acceptance.
    complete_now: bool,
}

/// A page's candidate seqs: `?1` root, `?2` last seq, `?3` the scope (JSON array), `?4` mailbox,
/// `?5` the caller, `?6` the row limit.
const PAGE_SQL: &str = "SELECT event_seq FROM delegation_event_links
     WHERE root_id = ?1 AND event_seq > ?2
       AND worker_id IN (SELECT value FROM json_each(?3))
     UNION
     SELECT l.event_seq FROM delegation_event_links l
     JOIN events e ON e.seq = l.event_seq
     JOIN delegation_messages m ON m.id = json_extract(e.envelope_json, '$.event.message_id')
     WHERE ?4 AND l.worker_id = ?5 AND l.event_seq > ?2 AND l.root_id = ?1
       AND l.category = 'message' AND m.recipient_worker_id = ?5
     ORDER BY 1
     LIMIT ?6";

/// A `ToolError` with every string in it scrubbed, recursively (message and nested details,
/// sensitive keys masked whole), the way stored envelopes are.
fn scrub_tool_error(e: &ToolError) -> Result<ToolError> {
    let v = serde_json::to_value(e)?;
    let mut scrubbed: ToolError = serde_json::from_value(super::scrub_value(None, v))?;
    // The code and retryable flag are ours, never secret: kept exactly.
    scrubbed.code = e.code;
    scrubbed.retryable = e.retryable;
    Ok(scrubbed)
}

fn json_len(v: &Value) -> usize {
    serde_json::to_string(v).map_or(usize::MAX, |s| s.len())
}

/// A page event's summary and state: scrubbed, at most [`MAX_EVENT_SUMMARY_BYTES`], never a
/// token stream.
fn event_summary(env: &Envelope) -> (String, Option<StateRef>) {
    let (text, state) = match &env.event {
        Event::Notice { text } => (text.clone(), None),
        Event::SessionStarted { .. } => (
            "session started".to_owned(),
            Some(StateRef::Worker(WorkerState::Running)),
        ),
        Event::SessionExited { code, expected } => (
            format!(
                "session exited{}{}",
                code.map(|c| format!(" with code {c}")).unwrap_or_default(),
                if *expected { "" } else { " unexpectedly" }
            ),
            Some(StateRef::Worker(WorkerState::Stopped)),
        ),
        Event::TurnStarted { .. } => (
            "turn started".to_owned(),
            Some(StateRef::Run(RunState::Running)),
        ),
        Event::TurnCompleted { state, error, .. } => {
            let run = match state {
                TurnState::Completed => RunState::Completed,
                TurnState::Interrupted => RunState::Interrupted,
                TurnState::Failed => RunState::Failed,
            };
            let text = match error {
                Some(e) => format!("turn {}: {e}", name_of(&run)),
                None => format!("turn {}", name_of(&run)),
            };
            (text, Some(StateRef::Run(run)))
        }
        Event::ItemStarted { title, .. } => (title.clone(), None),
        Event::ItemCompleted {
            status,
            output,
            error,
        } => {
            let status = match status {
                ItemStatus::Completed => "completed",
                ItemStatus::Failed => "failed",
                ItemStatus::Declined => "declined",
                ItemStatus::Interrupted => "interrupted",
            };
            let detail = error.as_deref().or(output.as_deref()).unwrap_or("");
            (format!("item {status}: {detail}"), None)
        }
        Event::ApprovalRequested { tool, title, .. } => (
            format!("approval requested: {}", title.as_deref().unwrap_or(tool)),
            None,
        ),
        Event::DelegatedMessage { text, .. } => (text.clone(), None),
        // Never a token stream: content is summarised by its item, not its deltas.
        Event::ContentDelta { .. } | Event::ContentSnapshot { .. } => ("activity".to_owned(), None),
        other => (super::event_kind(other), None),
    };
    (cut(&redact(&text), MAX_EVENT_SUMMARY_BYTES), state)
}

#[cfg(test)]
thread_local! {
    /// Test hook: runs inside a publication, after its event row and before its links.
    static BEFORE_LINKS: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
    /// Test counter: candidate rows a page read examined.
    static PAGE_ROWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::delegation::identity::{Provenance, ValueProvenance};
    use agent_core::delegation::page::filter_digest;
    use agent_core::delegation::tools::ToolName;
    use agent_core::delegation::wire::{
        parse_request, transport_cost, WorkspaceKind, OUTPUT_BUDGET,
    };
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    struct Fx {
        _dir: tempfile::TempDir,
        path: PathBuf,
        s: Store,
        root: RootHandle,
        root_thread: String,
    }

    fn open(path: &Path) -> Store {
        Store::open(path).expect("open")
    }

    fn fx() -> Fx {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("threads.db");
        let s = open(&path);
        let root_thread = s.create_thread("/w", Some("root")).expect("thread");
        let root = s
            .create_delegation_root(&NewRoot {
                thread: root_thread.clone(),
                driver: Driver::Claude,
                profile_id: ProfileId::parse("claude-default").expect("profile"),
                name: "root".into(),
                policy_json: "{}".into(),
            })
            .expect("root");
        Fx {
            _dir: dir,
            path,
            s,
            root,
            root_thread,
        }
    }

    fn req(tool: ToolName, v: Value) -> ToolRequest {
        parse_request(tool, &v, &Limits::default()).expect("valid request")
    }

    fn spawn_req(key: &str, task: &str) -> ToolRequest {
        req(
            ToolName::CrossAgentSpawn,
            json!({"profile_id": "codex-default", "name": "reviewer", "task": task,
                   "task_mode": "plan", "workspace": {"kind": "shared_read_only"},
                   "idempotency_key": key}),
        )
    }

    fn unknown_value() -> ValueProvenance {
        ValueProvenance {
            requested: None,
            effective: None,
            source: Provenance::Unknown,
        }
    }

    fn spawn_plan() -> Box<SpawnPlan> {
        Box::new(SpawnPlan {
            driver: Driver::Codex,
            identity: Identity {
                driver: Driver::Codex,
                profile_id: ProfileId::parse("codex-default").expect("profile"),
                model: unknown_value(),
                effort: unknown_value(),
            },
            workspace: WorkspaceInfo {
                kind: WorkspaceKind::SharedReadOnly,
                path: None,
                base_commit: None,
            },
            launch_json: "{}".into(),
            cwd: "/w".into(),
            state: SpawnState::Accepted,
            process_generation: 1,
        })
    }

    fn accept(
        s: &Store,
        principal: &WorkerId,
        request: ToolRequest,
        resolution: Resolution,
        plan: AcceptPlan,
    ) -> Verdict<Accepted> {
        s.accept_delegation(&Accept {
            principal: principal.clone(),
            request,
            resolution,
            plan,
            warnings: Vec::new(),
            limits: Limits::default(),
            live_processes: 0,
            dispatching: HashSet::new(),
        })
        .expect("store")
    }

    fn new_op(v: Verdict<Accepted>) -> AcceptedOp {
        match v {
            Ok(Accepted::New(op)) => op,
            other => panic!("not a new operation: {other:?}"),
        }
    }

    /// A spawned child of `principal` (accepted only).
    fn spawn(s: &Store, principal: &WorkerId, key: &str) -> AcceptedOp {
        new_op(accept(
            s,
            principal,
            spawn_req(key, "review the parser"),
            Resolution::default(),
            AcceptPlan::Spawn(spawn_plan()),
        ))
    }

    fn send(
        s: &Store,
        principal: &WorkerId,
        target: &str,
        kind: &str,
        text: &str,
        key: &str,
    ) -> Verdict<Accepted> {
        accept(
            s,
            principal,
            req(
                ToolName::CrossAgentSend,
                json!({"target": target, "text": text, "kind": kind, "delivery": "next_turn",
                       "idempotency_key": key}),
            ),
            Resolution::default(),
            AcceptPlan::Send(SendPlan {
                initial: DeliveryState::Queued,
                target_run: None,
            }),
        )
    }

    fn ack_req(ids: &[&MessageId], key: &str) -> ToolRequest {
        let ids: Vec<&str> = ids.iter().map(|m| m.as_str()).collect();
        req(
            ToolName::CrossAgentRead,
            json!({"acknowledge": ids, "idempotency_key": key}),
        )
    }

    fn filter(caller: &WorkerId) -> PageFilter {
        PageFilter {
            caller: caller.clone(),
            target: None,
            workers: Vec::new(),
            mailbox: true,
        }
    }

    fn query(
        root: &WorkerId,
        caller: &WorkerId,
        scope: Vec<WorkerId>,
        after: Option<Cursor>,
    ) -> PageQuery {
        PageQuery {
            root_worker: root.clone(),
            filter: filter(caller),
            scope,
            after,
            max_events: 100,
            max_bytes: 64 * 1024,
            snapshot: None,
            ack: None,
            budget: OUTPUT_BUDGET,
        }
    }

    /// Every unit seq of a page, events and messages merged, ascending.
    fn seqs(page: &Value) -> Vec<u64> {
        let mut out: Vec<u64> = ["events", "messages"]
            .iter()
            .flat_map(|k| page[*k].as_array().cloned().unwrap_or_default())
            .filter_map(|u| u["seq"].as_u64())
            .collect();
        out.sort_unstable();
        out
    }

    fn next_cursor(page: &Value) -> Cursor {
        Cursor::parse(page["next_cursor"].as_str().expect("cursor")).expect("cursor")
    }

    fn op_state(s: &Store, op: &OperationId) -> String {
        s.delegation_operation(op.as_str())
            .expect("q")
            .expect("row")
            .dispatch_state
    }

    // ---- idempotency ----

    #[test]
    fn the_same_key_and_request_replays_the_frozen_receipt_byte_for_byte() {
        let f = fx();
        let first = spawn(&f.s, &f.root.worker, "k1");
        let stored =
            f.s.delegation_operation(first.operation.as_str())
                .expect("q")
                .expect("row");
        assert_eq!(
            stored.receipt_json,
            serde_json::to_string(&first.receipt).expect("json")
        );
        assert_eq!(first.receipt["replayed"], json!(false));
        // A retry: no second worker, the same receipt with only replayed/dispatch refreshed.
        let again = accept(
            &f.s,
            &f.root.worker,
            spawn_req("k1", "review the parser"),
            Resolution::default(),
            AcceptPlan::Spawn(spawn_plan()),
        );
        let Ok(Accepted::Replay(ReplayOutcome::Receipt(replayed))) = again else {
            panic!("{again:?}")
        };
        let mut expected = first.receipt.clone();
        expected["replayed"] = json!(true);
        assert_eq!(replayed, expected);
        assert_eq!(
            f.s.delegation_workers_of_root(&f.root.root)
                .expect("q")
                .len(),
            2
        );
        // The replay follows the stored dispatch state.
        let reserved =
            f.s.reserve_delegation_dispatch(&first.operation)
                .expect("s")
                .expect("ok");
        assert_eq!(
            reserved.operation.dispatch,
            OperationState::DispatchReserved
        );
        f.s.record_delegation_dispatch(&first.operation, &DispatchFact::Submitted)
            .expect("submitted");
        let again = accept(
            &f.s,
            &f.root.worker,
            spawn_req("k1", "review the parser"),
            Resolution::default(),
            AcceptPlan::Spawn(spawn_plan()),
        );
        let Ok(Accepted::Replay(ReplayOutcome::Receipt(r))) = again else {
            panic!("{again:?}")
        };
        assert_eq!(r["dispatch"], json!("submitted"));
        // A failed operation replays its stored error.
        f.s.record_delegation_dispatch(
            &first.operation,
            &DispatchFact::Failed(ToolError::new(ErrorCode::ProviderUnavailable, "no codex")),
        )
        .expect("failed");
        let again = accept(
            &f.s,
            &f.root.worker,
            spawn_req("k1", "review the parser"),
            Resolution::default(),
            AcceptPlan::Spawn(spawn_plan()),
        );
        let Ok(Accepted::Replay(ReplayOutcome::Error(e))) = again else {
            panic!("{again:?}")
        };
        assert_eq!(e.code, ErrorCode::ProviderUnavailable);
        assert_eq!(
            e.details.as_ref().expect("details")["replayed"],
            json!(true)
        );
    }

    #[test]
    fn the_same_key_with_a_changed_request_is_a_conflict_and_changes_nothing() {
        let f = fx();
        let first = spawn(&f.s, &f.root.worker, "k1");
        let changed = accept(
            &f.s,
            &f.root.worker,
            spawn_req("k1", "review the lexer instead"),
            Resolution::default(),
            AcceptPlan::Spawn(spawn_plan()),
        );
        let Err(e) = changed else {
            panic!("{changed:?}")
        };
        assert_eq!(e.code, ErrorCode::IdempotencyConflict);
        assert_eq!(
            e.details.as_ref().expect("details")["operation_id"],
            json!(first.operation.as_str())
        );
        assert_eq!(
            f.s.delegation_workers_of_root(&f.root.root)
                .expect("q")
                .len(),
            2
        );
        // Two secrets that scrub alike are the same request (contract §2).
        // (The mask keeps the last 4 characters, so these share them.)
        let a = format!("use ghp_{}zzzz", "a".repeat(32));
        let b = format!("use ghp_{}zzzz", "b".repeat(32));
        let one = new_op(accept(
            &f.s,
            &f.root.worker,
            spawn_req("k2", &a),
            Resolution::default(),
            AcceptPlan::Spawn(spawn_plan()),
        ));
        let two = accept(
            &f.s,
            &f.root.worker,
            spawn_req("k2", &b),
            Resolution::default(),
            AcceptPlan::Spawn(spawn_plan()),
        );
        assert!(matches!(two, Ok(Accepted::Replay(_))), "{two:?}");
        let canonical =
            f.s.delegation_operation(one.operation.as_str())
                .expect("q")
                .expect("row")
                .canonical_request;
        assert!(!canonical.contains("aaaaaaaa"), "{canonical}");
    }

    #[test]
    fn a_stale_epoch_is_refused_before_the_lookup_and_an_old_key_is_never_rerun() {
        let f = fx();
        let first = spawn(&f.s, &f.root.worker, "k1");
        // A fence that is not current is refused before anything is looked up.
        let fenced = req(
            ToolName::CrossAgentSpawn,
            json!({"profile_id": "codex-default", "name": "x", "task": "t", "task_mode": "plan",
                   "workspace": {"kind": "shared_read_only"}, "expected_authority_epoch": 3,
                   "idempotency_key": "k9"}),
        );
        let refused = accept(
            &f.s,
            &f.root.worker,
            fenced,
            Resolution::default(),
            AcceptPlan::Spawn(spawn_plan()),
        );
        assert_eq!(
            refused.as_ref().err().map(|e| e.code),
            Some(ErrorCode::StaleEpoch),
            "{refused:?}"
        );
        // A revocation bumps the epoch; the not-yet-reserved spawn is invalidated.
        let revoked =
            f.s.revoke_delegation(&f.root.worker, &RevocationEvent::RootStop)
                .expect("revoke");
        assert_eq!(revoked.authority_epoch, 1);
        assert_eq!(revoked.invalidated, vec![first.operation.clone()]);
        assert_eq!(op_state(&f.s, &first.operation), "failed");
        // The same identical request under the new epoch is stale, never a new run.
        let again = accept(
            &f.s,
            &f.root.worker,
            spawn_req("k1", "review the parser"),
            Resolution::default(),
            AcceptPlan::Spawn(spawn_plan()),
        );
        let Err(e) = again else { panic!("{again:?}") };
        assert_eq!(e.code, ErrorCode::StaleEpoch);
        assert_eq!(
            e.details.as_ref().expect("d")["operation_id"],
            json!(first.operation.as_str())
        );
        // Reserving the invalidated operation is refused: it is not accepted any more.
        let reserve =
            f.s.reserve_delegation_dispatch(&first.operation)
                .expect("s");
        assert!(reserve.is_err());
    }

    #[test]
    fn reserving_after_a_revocation_fails_the_operation_with_stale_epoch() {
        let f = fx();
        let child = spawn(&f.s, &f.root.worker, "k1");
        let child_id = child.worker.clone().expect("worker");
        let sent = new_op(send(
            &f.s,
            &f.root.worker,
            child_id.as_str(),
            "instruction",
            "go",
            "s1",
        ));
        // Bump the epoch behind the operation's back (as a racing revocation would).
        f.s.conn
            .execute(
                "UPDATE delegation_roots SET authority_epoch = authority_epoch + 1 WHERE id = ?1",
                params![f.root.root],
            )
            .expect("bump");
        let r = f.s.reserve_delegation_dispatch(&sent.operation).expect("s");
        assert_eq!(r.err().map(|e| e.code), Some(ErrorCode::StaleEpoch));
        assert_eq!(op_state(&f.s, &sent.operation), "failed");
        let m =
            f.s.delegation_message(sent.message.as_ref().expect("m").as_str())
                .expect("q")
                .expect("row");
        assert_eq!(m.delivery_state, "cancelled");
    }

    #[test]
    fn operation_kinds_and_delivery_policies_are_checked_on_write() {
        let f = fx();
        let mut row = OperationRow {
            id: "op_aaaaaaaa".into(),
            root_id: f.root.root.clone(),
            principal_worker_id: f.root.worker.to_string(),
            kind: "launch_missiles".into(),
            idempotency_key: "k".into(),
            authority_epoch: 0,
            canonical_request: "{}".into(),
            resolution_json: "{}".into(),
            receipt_json: "{}".into(),
            dispatch_state: "accepted".into(),
            error_json: None,
            target_worker_id: None,
            run_id: None,
            created_at: 1,
            updated_at: 1,
        };
        assert!(f.s.insert_delegation_operation(&row).is_err());
        row.kind = "spawn".into();
        f.s.insert_delegation_operation(&row).expect("a known kind");
        let child = spawn(&f.s, &f.root.worker, "k1").worker.expect("w");
        let mut m = MessageRow {
            id: "m_aaaaaaaa".into(),
            root_id: f.root.root.clone(),
            operation_id: None,
            sender_worker_id: f.root.worker.to_string(),
            recipient_worker_id: child.to_string(),
            kind: "instruction".into(),
            reply_to: None,
            body: "x".into(),
            delivery_policy: "whenever".into(),
            target_run_id: None,
            delivery_state: "queued".into(),
            recipient_run_id: None,
            queued_at: 1,
            submitted_at: None,
            confirmed_at: None,
            presented_at: None,
            acknowledged_at: None,
            updated_at: 1,
        };
        assert!(f.s.insert_delegation_message(&m).is_err());
        m.delivery_policy = "next_turn".into();
        f.s.insert_delegation_message(&m).expect("a known policy");
    }

    // ---- crashes: drop the store, reopen, reconcile ----

    #[test]
    fn a_crash_before_the_intent_commits_leaves_nothing_and_the_key_is_free() {
        let f = fx();
        let Fx {
            _dir,
            path,
            s,
            root,
            ..
        } = f;
        s.conn.execute_batch("BEGIN IMMEDIATE").expect("begin");
        let _ = spawn(&s, &root.worker, "k1");
        drop(s); // the process dies before COMMIT
        let s = open(&path);
        let rec = s.reconcile_delegation_after_restart().expect("reconcile");
        assert!(rec.abnormal);
        assert_eq!(
            s.delegation_workers_of_root(&root.root).expect("q").len(),
            1
        );
        assert!(s
            .delegation_operation_by_key(root.worker.as_str(), "spawn", "k1")
            .expect("q")
            .is_none());
        // Nothing was dispatched, so the same key is simply new.
        let again = spawn(&s, &root.worker, "k1");
        assert_eq!(op_state(&s, &again.operation), "accepted");
    }

    #[test]
    fn a_crash_after_the_reservation_is_unknown_and_never_dispatched_again() {
        let f = fx();
        let Fx {
            _dir,
            path,
            s,
            root,
            ..
        } = f;
        let op = spawn(&s, &root.worker, "k1");
        s.reserve_delegation_dispatch(&op.operation)
            .expect("s")
            .expect("reserved");
        drop(s);
        let s = open(&path);
        let rec = s.reconcile_delegation_after_restart().expect("reconcile");
        assert_eq!(rec.operations, vec![op.operation.clone()]);
        assert_eq!(rec.runs, vec![op.run.clone().expect("run")]);
        assert_eq!(op_state(&s, &op.operation), "unknown");
        // A retry with the same key is delivery_unknown carrying the original receipt.
        let again = accept(
            &s,
            &root.worker,
            spawn_req("k1", "review the parser"),
            Resolution::default(),
            AcceptPlan::Spawn(spawn_plan()),
        );
        let Ok(Accepted::Replay(ReplayOutcome::DeliveryUnknown(e))) = again else {
            panic!("{again:?}")
        };
        assert_eq!(e.code, ErrorCode::DeliveryUnknown);
        assert!(!e.retryable);
        assert_eq!(e.details.as_ref().expect("d")["receipt"], op.receipt);
        // It can never be reserved (dispatched) again.
        assert!(s
            .reserve_delegation_dispatch(&op.operation)
            .expect("s")
            .is_err());
        let run = s
            .delegation_run(op.run.expect("run").as_str())
            .expect("q")
            .expect("row");
        assert_eq!(run.state, "unknown");
    }

    #[test]
    fn a_crash_after_the_submission_is_unknown_and_never_dispatched_again() {
        let f = fx();
        let Fx {
            _dir,
            path,
            s,
            root,
            ..
        } = f;
        let child = spawn(&s, &root.worker, "k1").worker.expect("w");
        let sent = new_op(send(
            &s,
            &root.worker,
            child.as_str(),
            "instruction",
            "go",
            "s1",
        ));
        s.reserve_delegation_dispatch(&sent.operation)
            .expect("s")
            .expect("reserved");
        s.record_delegation_dispatch(&sent.operation, &DispatchFact::Submitted)
            .expect("submitted");
        drop(s);
        let s = open(&path);
        let rec = s.reconcile_delegation_after_restart().expect("reconcile");
        assert!(rec.operations.contains(&sent.operation));
        let message = sent.message.clone().expect("m");
        assert_eq!(rec.messages, vec![message.clone()]);
        let m = s
            .delegation_message(message.as_str())
            .expect("q")
            .expect("row");
        assert_eq!(m.delivery_state, "unknown");
        let again = send(&s, &root.worker, child.as_str(), "instruction", "go", "s1");
        assert!(
            matches!(
                again,
                Ok(Accepted::Replay(ReplayOutcome::DeliveryUnknown(_)))
            ),
            "{again:?}"
        );
        assert!(s
            .reserve_delegation_dispatch(&sent.operation)
            .expect("s")
            .is_err());
        // No second message was ever written.
        let n: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM delegation_messages", [], |r| r.get(0))
            .expect("count");
        assert_eq!(n, 1);
    }

    #[test]
    fn a_crash_before_the_ack_keeps_the_presentation_and_a_retried_ack_replays() {
        let f = fx();
        let Fx {
            _dir,
            path,
            s,
            root,
            ..
        } = f;
        let child = spawn(&s, &root.worker, "k1").worker.expect("w");
        let report = new_op(send(&s, &child, "parent", "report", "done", "r1"))
            .message
            .expect("m");
        let page = s
            .read_and_present_delegation_page(&query(
                &root.worker,
                &root.worker,
                vec![root.worker.clone()],
                None,
            ))
            .expect("s")
            .expect("page");
        assert_eq!(page.presented, vec![report.clone()]);
        drop(s); // dies before the ACK
        let s = open(&path);
        s.reconcile_delegation_after_restart().expect("reconcile");
        let first = s
            .acknowledge_delegation_messages(&AckRequest {
                principal: root.worker.clone(),
                request: ack_req(&[&report], "a1"),
            })
            .expect("s")
            .expect("ack");
        let AckResult::New(outcome) = first else {
            panic!("{first:?}")
        };
        assert_eq!(outcome["acknowledged"], json!([report.as_str()]));
        // A newer report arrives and is presented; the lost-answer retry consumes nothing new.
        let newer = new_op(send(&s, &child, "parent", "report", "more", "r2"))
            .message
            .expect("m");
        s.read_and_present_delegation_page(&query(
            &root.worker,
            &root.worker,
            vec![root.worker.clone()],
            None,
        ))
        .expect("s")
        .expect("page");
        drop(s);
        let s = open(&path);
        let retry = s
            .acknowledge_delegation_messages(&AckRequest {
                principal: root.worker.clone(),
                request: ack_req(&[&report], "a1"),
            })
            .expect("s")
            .expect("replay");
        let AckResult::Replay(ReplayOutcome::Receipt(r)) = retry else {
            panic!("{retry:?}")
        };
        assert_eq!(r["acknowledged"], json!([report.as_str()]));
        assert_eq!(r["replayed"], json!(true));
        let m = s
            .delegation_message(newer.as_str())
            .expect("q")
            .expect("row");
        assert!(
            m.acknowledged_at.is_none(),
            "the retry never consumed the newer report"
        );
    }

    #[test]
    fn a_crash_before_the_result_notification_leaves_the_run_unknown_and_no_report() {
        let f = fx();
        let Fx {
            _dir,
            path,
            s,
            root,
            ..
        } = f;
        let op = spawn(&s, &root.worker, "k1");
        let run = op.run.clone().expect("run");
        s.reserve_delegation_dispatch(&op.operation)
            .expect("s")
            .expect("reserved");
        s.record_delegation_dispatch(&op.operation, &DispatchFact::Submitted)
            .expect("sub");
        s.advance_delegation_run(&run, RunState::Running, Some("turn-1"))
            .expect("running");
        // The run ends in the provider, but the app dies before step 5 commits.
        s.conn.execute_batch("BEGIN IMMEDIATE").expect("begin");
        let child = op.worker.clone().expect("w");
        let thread = s
            .delegation_worker(child.as_str())
            .expect("q")
            .expect("row")
            .backing_thread_id
            .expect("t");
        s.complete_delegation_run(&Completion {
            run: run.clone(),
            state: RunState::Completed,
            worker_state: WorkerState::Waiting,
            reason: None,
            usage_json: None,
            result: Some("all good".into()),
            event: PublishEvent {
                thread,
                provider_thread: None,
                envelope: Envelope::new(Event::TurnCompleted {
                    state: TurnState::Completed,
                    usage: None,
                    cost_usd: None,
                    error: None,
                }),
                links: vec![LinkSpec {
                    worker: child.clone(),
                    run: Some(run.clone()),
                    category: EventCategory::Result,
                    invocation: None,
                }],
                worker_states: Vec::new(),
                run_states: Vec::new(),
                aliases: Vec::new(),
            },
        })
        .expect("complete in the open transaction");
        drop(s);
        let s = open(&path);
        let rec = s.reconcile_delegation_after_restart().expect("reconcile");
        assert_eq!(rec.runs, vec![run.clone()]);
        assert_eq!(rec.workers, vec![child.clone()]);
        let r = s.delegation_run(run.as_str()).expect("q").expect("row");
        assert_eq!(r.state, "unknown", "never re-run, never guessed completed");
        let n: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM delegation_messages", [], |r| r.get(0))
            .expect("count");
        assert_eq!(n, 0, "no half-written notification");
        let again = accept(
            &s,
            &root.worker,
            spawn_req("k1", "review the parser"),
            Resolution::default(),
            AcceptPlan::Spawn(spawn_plan()),
        );
        assert!(
            matches!(
                again,
                Ok(Accepted::Replay(ReplayOutcome::DeliveryUnknown(_)))
            ),
            "{again:?}"
        );
    }

    #[test]
    fn a_completion_commits_the_run_the_event_and_the_notification_together() {
        let f = fx();
        let op = spawn(&f.s, &f.root.worker, "k1");
        let (child, run) = (op.worker.expect("w"), op.run.expect("r"));
        f.s.advance_delegation_run(&run, RunState::Running, None)
            .expect("running");
        let thread =
            f.s.delegation_worker(child.as_str())
                .expect("q")
                .expect("row")
                .backing_thread_id
                .expect("t");
        let secret = format!("ghp_{}", "z".repeat(36));
        let done =
            f.s.complete_delegation_run(&Completion {
                run: run.clone(),
                state: RunState::Completed,
                worker_state: WorkerState::Waiting,
                reason: None,
                usage_json: None,
                result: Some(format!("found it; token {secret} {}", "x".repeat(40_000))),
                event: PublishEvent {
                    thread,
                    provider_thread: None,
                    envelope: Envelope::new(Event::Notice {
                        text: "finished".into(),
                    }),
                    links: vec![LinkSpec {
                        worker: child.clone(),
                        run: Some(run.clone()),
                        category: EventCategory::Result,
                        invocation: None,
                    }],
                    worker_states: Vec::new(),
                    run_states: Vec::new(),
                    aliases: Vec::new(),
                },
            })
            .expect("complete");
        let (m, seq) = done.notification.expect("notified");
        assert!(seq > done.event.seq);
        let row = f.s.delegation_message(m.as_str()).expect("q").expect("row");
        assert_eq!(row.recipient_worker_id, f.root.worker.as_str());
        assert_eq!(row.kind, "report");
        assert!(row.body.len() <= MAX_RESULT_BYTES);
        assert!(!row.body.contains(&secret));
        assert_eq!(
            f.s.delegation_run(run.as_str())
                .expect("q")
                .expect("r")
                .state,
            "completed"
        );
        // A second completion of the same run is refused and changes nothing.
        let c2 = f.s.complete_delegation_run(&Completion {
            run: run.clone(),
            state: RunState::Failed,
            worker_state: WorkerState::Failed,
            reason: None,
            usage_json: None,
            result: None,
            event: PublishEvent {
                thread: f.root_thread.clone(),
                provider_thread: None,
                envelope: Envelope::new(Event::Notice { text: "x".into() }),
                links: vec![LinkSpec {
                    worker: child.clone(),
                    run: None,
                    category: EventCategory::Result,
                    invocation: None,
                }],
                worker_states: Vec::new(),
                run_states: Vec::new(),
                aliases: Vec::new(),
            },
        });
        assert!(c2.is_err());
    }

    // ---- atomic visibility ----

    fn child_event(thread: &str, worker: &WorkerId, text: &str) -> PublishEvent {
        PublishEvent {
            thread: thread.to_owned(),
            provider_thread: None,
            envelope: Envelope::new(Event::Notice { text: text.into() }),
            links: vec![LinkSpec {
                worker: worker.clone(),
                run: None,
                category: EventCategory::Activity,
                invocation: None,
            }],
            worker_states: Vec::new(),
            run_states: Vec::new(),
            aliases: Vec::new(),
        }
    }

    #[test]
    fn a_delayed_child_link_never_appears_behind_a_readers_cursor() {
        let f = fx();
        let child = spawn(&f.s, &f.root.worker, "k1").worker.expect("w");
        let child_thread =
            f.s.delegation_worker(child.as_str())
                .expect("q")
                .expect("row")
                .backing_thread_id
                .expect("t");
        let scope = vec![f.root.worker.clone(), child.clone()];
        let reader = open(&f.path);
        let other_writer = open(&f.path);
        let mid_reader = open(&f.path);
        // The reader is up to date before the child event starts.
        let p0 = reader
            .read_delegation_page(&query(&f.root.worker, &f.root.worker, scope.clone(), None))
            .expect("s")
            .expect("page");
        let c0 = next_cursor(&p0.page);
        let before = seqs(&p0.page);

        // The child's event row is written, and its link is delayed (the hook runs in between).
        // Meanwhile another writer tries to publish a root event, and the reader reads.
        let (root_worker, root_thread, scope2, c0b) = (
            f.root.worker.clone(),
            f.root_thread.clone(),
            scope.clone(),
            c0.clone(),
        );
        let (tx, rx) = std::sync::mpsc::channel();
        BEFORE_LINKS.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || {
                let rw = root_worker.clone();
                let rt = root_thread.clone();
                let racer = std::thread::spawn(move || {
                    other_writer
                        .publish_delegation_event(&child_event(&rt, &rw, "root event"))
                        .expect("the racing event commits after the child's")
                });
                std::thread::sleep(Duration::from_millis(300));
                let mid = mid_reader
                    .read_delegation_page(&query(&root_worker, &root_worker, scope2, Some(c0b)))
                    .expect("s")
                    .expect("page");
                let _ = tx.send((mid.page, racer));
            }))
        });
        let e1 =
            f.s.publish_delegation_event(&child_event(&child_thread, &child, "child event"))
                .expect("publish");
        let (mid, racer) = rx.recv().expect("hook ran");
        let e2 = racer.join().expect("racer");
        assert!(
            seqs(&mid).is_empty(),
            "nothing uncommitted is visible: {mid}"
        );
        assert_eq!(next_cursor(&mid), c0, "the reader did not advance");
        assert!(e2.seq > e1.seq, "commit order is seq order");

        // After both commits, the reader sees the child's event before the racing one, from the
        // cursor it already held: nothing appeared behind it.
        let p1 = reader
            .read_delegation_page(&query(
                &f.root.worker,
                &f.root.worker,
                scope.clone(),
                Some(c0.clone()),
            ))
            .expect("s")
            .expect("page");
        assert_eq!(seqs(&p1.page), vec![u64_of(e1.seq), u64_of(e2.seq)]);
        let c1 = next_cursor(&p1.page);
        assert!(before.iter().all(|s| *s < u64_of(e1.seq)));

        // A historical relink (a late alias of an old event) never inserts silently behind c1:
        // it bumps the stream epoch, and c1 is expired.
        let epoch =
            f.s.relink_delegation_history(
                &f.root.worker,
                &[(
                    e2.seq,
                    LinkSpec {
                        worker: child.clone(),
                        run: None,
                        category: EventCategory::Activity,
                        invocation: None,
                    },
                )],
                &[],
            )
            .expect("relink");
        assert_eq!(epoch, 1);
        let expired = reader
            .read_delegation_page(&query(
                &f.root.worker,
                &f.root.worker,
                scope.clone(),
                Some(c1),
            ))
            .expect("s");
        assert_eq!(
            expired.err().map(|e| e.code),
            Some(ErrorCode::CursorExpired)
        );
        // Re-read with no cursor: the backfilled membership is there, once per seq.
        let fresh = reader
            .read_delegation_page(&query(&f.root.worker, &f.root.worker, scope, None))
            .expect("s")
            .expect("page");
        let all = seqs(&fresh.page);
        assert_eq!(all.iter().filter(|s| **s == u64_of(e2.seq)).count(), 1);
    }

    #[test]
    fn cursors_are_bound_to_their_filter_and_expire_after_an_abnormal_restart() {
        let f = fx();
        let Fx {
            _dir,
            path,
            s,
            root,
            root_thread,
        } = f;
        let e = s
            .publish_delegation_event(&child_event(&root_thread, &root.worker, "hello"))
            .expect("publish");
        let q = query(&root.worker, &root.worker, vec![root.worker.clone()], None);
        let page = s.read_delegation_page(&q).expect("s").expect("page");
        assert_eq!(seqs(&page.page), vec![u64_of(e.seq)]);
        let cursor = next_cursor(&page.page);
        // Another filter: invalid_argument on `after`.
        let mut other = query(
            &root.worker,
            &root.worker,
            vec![root.worker.clone()],
            Some(cursor.clone()),
        );
        other.filter.mailbox = false;
        let r = s.read_delegation_page(&other).expect("s");
        let err = r.expect_err("refused");
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert_eq!(err.details.expect("d")["field"], json!("after"));
        // A clean restart keeps the stream epoch; an abnormal one expires every cursor.
        s.mark_delegation_clean_shutdown().expect("mark");
        drop(s);
        let s = open(&path);
        assert!(!s.reconcile_delegation_after_restart().expect("r").abnormal);
        let q2 = query(
            &root.worker,
            &root.worker,
            vec![root.worker.clone()],
            Some(cursor.clone()),
        );
        assert!(s.read_delegation_page(&q2).expect("s").is_ok());
        drop(s);
        let s = open(&path);
        let rec = s.reconcile_delegation_after_restart().expect("r");
        assert!(rec.abnormal);
        assert_eq!(rec.roots, vec![root.root.clone()]);
        let r = s.read_delegation_page(&q2).expect("s");
        assert_eq!(r.err().map(|e| e.code), Some(ErrorCode::CursorExpired));
        let digest = filter_digest(&filter(&root.worker));
        assert_ne!(digest, [0; 16]);
    }

    // ---- mailbox ----

    #[test]
    fn an_ack_rejects_bad_entries_alone_and_consumes_the_rest() {
        let f = fx();
        let child = spawn(&f.s, &f.root.worker, "k1").worker.expect("w");
        let shown = new_op(send(&f.s, &child, "parent", "report", "one", "r1"))
            .message
            .expect("m");
        let page =
            f.s.read_and_present_delegation_page(&query(
                &f.root.worker,
                &f.root.worker,
                vec![f.root.worker.clone()],
                None,
            ))
            .expect("s")
            .expect("page");
        assert_eq!(page.presented, vec![shown.clone()]);
        // Sent after the page: not presented yet.
        let unseen = new_op(send(&f.s, &child, "parent", "question", "two?", "r2"))
            .message
            .expect("m");
        // A message to the child is not the root's to acknowledge.
        let to_child = new_op(send(
            &f.s,
            &f.root.worker,
            child.as_str(),
            "instruction",
            "go",
            "s1",
        ))
        .message
        .expect("m");
        let missing = MessageId::parse("m_doesnotexist").expect("m");
        let r =
            f.s.acknowledge_delegation_messages(&AckRequest {
                principal: f.root.worker.clone(),
                request: ack_req(&[&unseen, &shown, &to_child, &missing], "a1"),
            })
            .expect("s")
            .expect("ack");
        let AckResult::New(outcome) = r else {
            panic!("{r:?}")
        };
        assert_eq!(outcome["acknowledged"], json!([shown.as_str()]));
        let rejected: Vec<(String, String)> = outcome["rejected"]
            .as_array()
            .expect("array")
            .iter()
            .map(|r| {
                (
                    r["message_id"].as_str().unwrap_or("").to_owned(),
                    r["code"].as_str().unwrap_or("").to_owned(),
                )
            })
            .collect();
        assert_eq!(
            rejected,
            vec![
                (unseen.to_string(), "invalid_argument".to_owned()),
                (to_child.to_string(), "not_found".to_owned()),
                (missing.to_string(), "not_found".to_owned()),
            ]
        );
        for (id, acked) in [(&shown, true), (&unseen, false), (&to_child, false)] {
            let m =
                f.s.delegation_message(id.as_str())
                    .expect("q")
                    .expect("row");
            assert_eq!(m.acknowledged_at.is_some(), acked, "{id}");
        }
        // Acknowledging again (new key) is idempotent: listed as acknowledged.
        let again =
            f.s.acknowledge_delegation_messages(&AckRequest {
                principal: f.root.worker.clone(),
                request: ack_req(&[&shown], "a2"),
            })
            .expect("s")
            .expect("ack");
        let AckResult::New(o) = again else {
            panic!("{again:?}")
        };
        assert_eq!(o["acknowledged"], json!([shown.as_str()]));
        // A done ACK is confirmed at once, so a revocation never turns it into a failure.
        let ack_op = OperationId::parse(o["operation_id"].as_str().expect("id")).expect("op");
        assert_eq!(op_state(&f.s, &ack_op), "confirmed");
        f.s.revoke_delegation(&f.root.worker, &RevocationEvent::RootStop)
            .expect("revoke");
        assert_eq!(op_state(&f.s, &ack_op), "confirmed");
        // Its key is not re-run under the new epoch.
        let replay =
            f.s.acknowledge_delegation_messages(&AckRequest {
                principal: f.root.worker.clone(),
                request: ack_req(&[&shown], "a2"),
            })
            .expect("s");
        assert!(
            matches!(replay, Err(ref e) if e.code == ErrorCode::StaleEpoch),
            "{replay:?}"
        );
    }

    #[test]
    fn delivery_facts_follow_delivery_step_including_misdelivered() {
        let f = fx();
        let op = spawn(&f.s, &f.root.worker, "k1");
        let (child, run) = (op.worker.expect("w"), op.run.expect("r"));
        let sent = accept(
            &f.s,
            &f.root.worker,
            req(
                ToolName::CrossAgentSend,
                json!({"target": child.as_str(), "text": "steer", "kind": "instruction",
                       "delivery": "this_run", "idempotency_key": "s1"}),
            ),
            Resolution::default(),
            AcceptPlan::Send(SendPlan {
                initial: DeliveryState::Queued,
                target_run: Some(run.clone()),
            }),
        );
        let sent = new_op(sent);
        assert_eq!(sent.receipt["target_run_id"], json!(run.as_str()));
        let m = sent.message.clone().expect("m");
        // Out of order: submitted before reserved is refused.
        assert!(f
            .s
            .record_delegation_delivery(&m, DeliveryState::Submitted, None)
            .is_err());
        f.s.reserve_delegation_dispatch(&sent.operation)
            .expect("s")
            .expect("reserved");
        f.s.record_delegation_dispatch(&sent.operation, &DispatchFact::Submitted)
            .expect("sub");
        // Confirmed in another run is misdelivered, not confirmed.
        f.s.advance_delegation_run(&run, RunState::Running, None)
            .expect("running");
        let later = f.s.new_run_id().expect("id");
        assert!(f
            .s
            .record_delegation_delivery(&m, DeliveryState::ProviderConfirmed, Some(&later))
            .is_err());
        assert!(f
            .s
            .record_delegation_delivery(&m, DeliveryState::Misdelivered, Some(&run))
            .is_err());
        f.s.insert_delegation_run(&RunRow {
            id: later.to_string(),
            worker_id: f.root.worker.to_string(),
            process_generation: 1,
            provider_thread_id: None,
            native_turn_id: None,
            authority_epoch: 0,
            state: "running".into(),
            terminal_reason: None,
            usage_json: None,
            created_at: 1,
            updated_at: 1,
        })
        .expect("another run");
        f.s.record_delegation_delivery(&m, DeliveryState::Misdelivered, Some(&later))
            .expect("misdelivered");
        let row = f.s.delegation_message(m.as_str()).expect("q").expect("row");
        assert_eq!(row.delivery_state, "misdelivered");
        assert_eq!(row.recipient_run_id.as_deref(), Some(later.as_str()));
        // Terminal: nothing moves it again. The receipt stays frozen.
        assert!(f
            .s
            .record_delegation_delivery(&m, DeliveryState::Failed, None)
            .is_err());
        let replay = accept(
            &f.s,
            &f.root.worker,
            req(
                ToolName::CrossAgentSend,
                json!({"target": child.as_str(), "text": "steer", "kind": "instruction", "delivery": "this_run", "idempotency_key": "s1"}),
            ),
            Resolution::default(),
            AcceptPlan::Send(SendPlan {
                initial: DeliveryState::Queued,
                target_run: Some(run.clone()),
            }),
        );
        let Ok(Accepted::Replay(ReplayOutcome::Receipt(r))) = replay else {
            panic!("{replay:?}")
        };
        assert_eq!(r["delivery"], json!("queued"));
        assert_eq!(r["target_run_id"], json!(run.as_str()));
    }

    // ---- pages ----

    #[test]
    fn a_worst_case_page_stays_within_the_budget_and_never_skips_a_seq() {
        let f = fx();
        let child = spawn(&f.s, &f.root.worker, "k1").worker.expect("w");
        // 16 KiB messages of characters JSON escapes six-fold.
        let text = "\u{1}".repeat(16 * 1024);
        let mut sent = Vec::new();
        for i in 0..40 {
            let op = new_op(send(
                &f.s,
                &child,
                "parent",
                "report",
                &text,
                &format!("r{i}"),
            ));
            sent.push(u64_of(op.seq));
        }
        let snapshot = json!({"blob": "\u{1}".repeat(8 * 1024)});
        let ack = json!({"blob": "\u{1}".repeat(10 * 1024)});
        let mut q = query(
            &f.root.worker,
            &f.root.worker,
            vec![f.root.worker.clone(), child.clone()],
            None,
        );
        q.max_events = 500;
        q.max_bytes = 256 * 1024;
        q.snapshot = Some(snapshot.clone());
        q.ack = Some(ack);
        let mut seen = Vec::new();
        let mut pages = 0;
        loop {
            let out = f.s.read_delegation_page(&q).expect("s").expect("page");
            pages += 1;
            assert!(
                transport_cost(&out.page) <= OUTPUT_BUDGET,
                "page {pages} is over budget"
            );
            assert_eq!(out.page["snapshot"], snapshot, "the snapshot is always in");
            let got = seqs(&out.page);
            assert!(!got.is_empty() || !out.truncated);
            seen.extend(got);
            if !out.truncated {
                break;
            }
            q.after = Some(next_cursor(&out.page));
            assert!(pages < 100);
        }
        assert!(pages > 1, "the budget cut the stream into pages");
        // Every message seq, once, in order (the spawn's audit event is an event unit).
        let messages: Vec<u64> = seen.iter().copied().filter(|s| sent.contains(s)).collect();
        assert_eq!(messages, sent);
        let mut sorted = seen.clone();
        sorted.dedup();
        assert_eq!(sorted, seen, "no seq twice");
    }

    #[test]
    fn a_message_seq_is_one_message_unit_never_also_an_event() {
        let f = fx();
        let child = spawn(&f.s, &f.root.worker, "k1").worker.expect("w");
        let op = new_op(send(
            &f.s,
            &f.root.worker,
            child.as_str(),
            "instruction",
            "go",
            "s1",
        ));
        let page =
            f.s.read_delegation_page(&query(
                &f.root.worker,
                &f.root.worker,
                vec![f.root.worker.clone(), child],
                None,
            ))
            .expect("s")
            .expect("page")
            .page;
        let events: Vec<u64> = page["events"]
            .as_array()
            .expect("e")
            .iter()
            .filter_map(|e| e["seq"].as_u64())
            .collect();
        let messages: Vec<u64> = page["messages"]
            .as_array()
            .expect("m")
            .iter()
            .filter_map(|e| e["seq"].as_u64())
            .collect();
        assert_eq!(messages, vec![u64_of(op.seq)]);
        assert!(!events.contains(&u64_of(op.seq)));
        assert_eq!(page["messages"][0]["text"], json!("go"));
    }

    // ---- revocation, deletion, native import ----

    #[test]
    fn a_close_revokes_the_subtree_and_cancels_queued_sends() {
        let f = fx();
        let child = spawn(&f.s, &f.root.worker, "k1").worker.expect("w");
        let grandchild = spawn(&f.s, &child, "k2").worker.expect("w");
        let queued = new_op(send(
            &f.s,
            &f.root.worker,
            child.as_str(),
            "instruction",
            "later",
            "s1",
        ));
        let report = new_op(send(&f.s, &child, "parent", "report", "data", "r1"));
        let r =
            f.s.revoke_delegation(&f.root.worker, &RevocationEvent::Close(child.clone()))
                .expect("revoke");
        assert_eq!(r.authority_epoch, 1);
        assert_eq!(r.closed, vec![child.clone(), grandchild.clone()]);
        assert!(r.cancelled.contains(&queued.message.clone().expect("m")));
        assert!(
            !r.cancelled.contains(&report.message.clone().expect("m")),
            "child→parent data stays"
        );
        assert!(r.effect.kill_tokens.contains(&child));
        // Closed workers refuse sends; closing again is worker_closed.
        let s2 = send(
            &f.s,
            &f.root.worker,
            child.as_str(),
            "instruction",
            "x",
            "s2",
        );
        assert_eq!(s2.err().map(|e| e.code), Some(ErrorCode::WorkerClosed));
        // A child cannot close its ancestor.
        let up = accept(
            &f.s,
            &grandchild,
            req(
                ToolName::CrossAgentClose,
                json!({"worker_id": f.root.worker.as_str(), "idempotency_key": "c1"}),
            ),
            Resolution::default(),
            AcceptPlan::Close,
        );
        assert!(up.is_err());
    }

    #[test]
    fn deleting_a_thread_with_live_descendants_is_refused_until_closed_and_revoked() {
        let f = fx();
        let op = spawn(&f.s, &f.root.worker, "k1");
        let child = op.worker.clone().expect("w");
        f.s.reserve_delegation_dispatch(&op.operation)
            .expect("s")
            .expect("reserved");
        f.s.advance_delegation_run(op.run.as_ref().expect("r"), RunState::Running, None)
            .expect("running");
        let grandchild = spawn(&f.s, &child, "k2").worker.expect("w");
        new_op(send(&f.s, &child, "parent", "report", "data", "r1"));
        let child_thread =
            f.s.delegation_worker(child.as_str())
                .expect("q")
                .expect("row")
                .backing_thread_id
                .expect("t");
        let grand_thread =
            f.s.delegation_worker(grandchild.as_str())
                .expect("q")
                .expect("row")
                .backing_thread_id
                .expect("t");
        f.s.record_child_native_session(&grandchild, Driver::Codex, "gpt", "native-grand")
            .expect("native");
        // Live descendants: both deletions are refused and nothing changes.
        assert!(f.s.delete_thread(&f.root_thread).is_err());
        assert!(f.s.delete_thread(&child_thread).is_err());
        assert_eq!(
            f.s.delegation_workers_of_root(&f.root.root)
                .expect("q")
                .len(),
            3
        );
        // Delete the child's subtree first.
        let r =
            f.s.close_and_revoke_for_delete(&child_thread)
                .expect("s")
                .expect("revoked");
        assert_eq!(r.closed, {
            let mut v = vec![child.clone(), grandchild.clone()];
            v.sort();
            v
        });
        let run =
            f.s.delegation_run(op.run.expect("r").as_str())
                .expect("q")
                .expect("row");
        assert_eq!(run.state, "interrupted");
        f.s.delete_thread(&child_thread).expect("deleted");
        assert!(f.s.thread_summary(&child_thread).expect("q").is_none());
        assert!(
            f.s.thread_summary(&grand_thread).expect("q").is_none(),
            "its hidden subtree goes too"
        );
        assert!(
            f.s.native_dismissed("codex", "native-grand").expect("q"),
            "never imported back"
        );
        assert_eq!(
            f.s.delegation_workers_of_root(&f.root.root)
                .expect("q")
                .len(),
            1
        );
        // The root: no live descendant is left, so it deletes, with its root row.
        let other = spawn(&f.s, &f.root.worker, "k3").worker.expect("w");
        assert!(f.s.delete_thread(&f.root_thread).is_err());
        f.s.close_and_revoke_for_delete(&f.root_thread)
            .expect("s")
            .expect("revoked");
        let _ = other;
        f.s.delete_thread(&f.root_thread).expect("deleted");
        assert!(f.s.delegation_root(&f.root.root).expect("q").is_none());
        assert!(
            f.s.list_threads(true).expect("list").is_empty(),
            "no hidden thread surfaced"
        );
        let fk: i64 =
            f.s.conn
                .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                    r.get(0)
                })
                .expect("fk check");
        assert_eq!(fk, 0);
    }

    #[test]
    fn the_native_import_skips_a_childs_session() {
        let f = fx();
        let child = spawn(&f.s, &f.root.worker, "k1").worker.expect("w");
        let thread =
            f.s.delegation_worker(child.as_str())
                .expect("q")
                .expect("row")
                .backing_thread_id
                .expect("t");
        let pt =
            f.s.record_child_native_session(&child, Driver::Codex, "gpt", "child-native")
                .expect("native");
        // Idempotent.
        assert_eq!(
            f.s.record_child_native_session(&child, Driver::Codex, "gpt", "child-native")
                .expect("again"),
            pt
        );
        // The import's own test: a session some thread holds is skipped (link_recent_sessions).
        assert_eq!(
            f.s.thread_with_native_id("codex", "child-native")
                .expect("q"),
            Some(thread.clone())
        );
        assert_eq!(f.s.active_provider_thread(&thread).expect("q"), Some(pt));
        let ids: Vec<String> =
            f.s.list_threads(true)
                .expect("l")
                .into_iter()
                .map(|t| t.id)
                .collect();
        assert_eq!(ids, vec![f.root_thread.clone()]);
        // A root worker has no backing thread to hold one.
        assert!(f
            .s
            .record_child_native_session(&f.root.worker, Driver::Claude, "opus", "x")
            .is_err());
    }

    #[test]
    fn capacity_and_depth_are_checked_from_persisted_counters() {
        let f = fx();
        let limits = Limits {
            active_runs_per_root: 2,
            ..Limits::default()
        };
        let try_spawn = |key: &str, principal: &WorkerId| {
            f.s.accept_delegation(&Accept {
                principal: principal.clone(),
                request: spawn_req(key, "t"),
                resolution: Resolution::default(),
                plan: AcceptPlan::Spawn(spawn_plan()),
                warnings: Vec::new(),
                limits,
                live_processes: 0,
                dispatching: HashSet::new(),
            })
            .expect("store")
        };
        let a = new_op(try_spawn("k1", &f.root.worker)).worker.expect("w");
        let b = new_op(try_spawn("k2", &a)).worker.expect("w");
        let full = try_spawn("k3", &f.root.worker);
        let e = full.expect_err("capacity");
        assert_eq!(e.code, ErrorCode::CapacityReached);
        assert!(e.retryable);
        let deep = try_spawn("k4", &b);
        assert!(deep.is_err());
    }

    #[test]
    fn a_committed_but_unconfirmed_durable_accept_is_never_dispatched() {
        let f = fx();
        let Fx {
            _dir,
            path,
            s,
            root,
            ..
        } = f;
        drop(s);
        let writer = crate::store_writer::StoreWriter::spawn(&path).expect("writer");
        writer
            .call("inject", |s| s.fail_sync_switch_after(1))
            .recv()
            .expect("armed");
        let answer = writer
            .delegation_accept(Accept {
                principal: root.worker.clone(),
                request: spawn_req("k1", "t"),
                resolution: Resolution::default(),
                plan: AcceptPlan::Spawn(spawn_plan()),
                warnings: Vec::new(),
                limits: Limits::default(),
                live_processes: 0,
                dispatching: HashSet::new(),
            })
            .recv();
        let outcome = DurableOutcome::from_answer(answer);
        assert!(
            matches!(outcome, DurableOutcome::CommittedUnconfirmed(_)),
            "{outcome:?}"
        );
        assert!(!outcome.may_dispatch());
        // The rows are stored; a same-key retry replays it, and the operation is still only
        // accepted (nothing dispatched it).
        let reader = open(&path);
        let row = reader
            .delegation_operation_by_key(root.worker.as_str(), "spawn", "k1")
            .expect("q")
            .expect("stored");
        assert_eq!(row.dispatch_state, "accepted");
        let replay = writer
            .delegation_accept(Accept {
                principal: root.worker.clone(),
                request: spawn_req("k1", "t"),
                resolution: Resolution::default(),
                plan: AcceptPlan::Spawn(spawn_plan()),
                warnings: Vec::new(),
                limits: Limits::default(),
                live_processes: 0,
                dispatching: HashSet::new(),
            })
            .recv();
        assert!(matches!(
            DurableOutcome::from_answer(replay),
            DurableOutcome::Committed(Ok(Accepted::Replay(_)))
        ));
        // A refused switch to FULL: nothing stored.
        writer
            .call("inject", |s| s.fail_sync_switch_after(0))
            .recv()
            .expect("armed");
        let refused = writer
            .delegation_accept(Accept {
                principal: root.worker.clone(),
                request: spawn_req("k2", "t"),
                resolution: Resolution::default(),
                plan: AcceptPlan::Spawn(spawn_plan()),
                warnings: Vec::new(),
                limits: Limits::default(),
                live_processes: 0,
                dispatching: HashSet::new(),
            })
            .recv();
        assert!(matches!(
            DurableOutcome::from_answer(refused),
            DurableOutcome::Refused(_)
        ));
        assert!(reader
            .delegation_operation_by_key(root.worker.as_str(), "spawn", "k2")
            .expect("q")
            .is_none());
        let _ = writer.shutdown(Duration::from_secs(10));
    }

    // ---- review round 1: cross-root ownership, scrubbed errors, bounded mailbox reads ----

    /// A second root in the same store, with one spawned child.
    fn second_root(s: &Store) -> (RootHandle, String, AcceptedOp) {
        let thread = s.create_thread("/v", Some("other root")).expect("thread");
        let root = s
            .create_delegation_root(&NewRoot {
                thread: thread.clone(),
                driver: Driver::Codex,
                profile_id: ProfileId::parse("codex-default").expect("profile"),
                name: "other".into(),
                policy_json: "{}".into(),
            })
            .expect("root");
        let child = spawn(s, &root.worker, "other-k1");
        (root, thread, child)
    }

    fn epochs(s: &Store, root: &str) -> (i64, i64) {
        let r = s.delegation_root(root).expect("q").expect("row");
        (r.authority_epoch, r.stream_epoch)
    }

    #[test]
    fn a_revocation_never_touches_a_worker_of_another_root() {
        let f = fx();
        spawn(&f.s, &f.root.worker, "k1");
        let (b, _, b_child) = second_root(&f.s);
        let foreign = b_child.worker.clone().expect("w");
        for event in [
            RevocationEvent::Close(foreign.clone()),
            RevocationEvent::GenerationExited(foreign.clone()),
        ] {
            let r = f.s.revoke_delegation(&f.root.worker, &event);
            assert!(r.is_err(), "{event:?}: {r:?}");
        }
        // Nothing moved in either root.
        assert_eq!(epochs(&f.s, &f.root.root), (0, 0));
        assert_eq!(epochs(&f.s, &b.root), (0, 0));
        let w =
            f.s.delegation_worker(foreign.as_str())
                .expect("q")
                .expect("row");
        assert_eq!(w.state, "starting");
        assert_eq!(op_state(&f.s, &b_child.operation), "accepted");
        // Its own root may close it.
        let r =
            f.s.revoke_delegation(&b.worker, &RevocationEvent::Close(foreign.clone()))
                .expect("own root");
        assert_eq!(r.closed, vec![foreign]);
    }

    #[test]
    fn a_publication_or_relink_never_reaches_another_roots_runs_workers_or_threads() {
        let f = fx();
        let a_child = spawn(&f.s, &f.root.worker, "k1");
        let a_worker = a_child.worker.clone().expect("w");
        let a_run = a_child.run.clone().expect("r");
        let (b, b_thread, b_child) = second_root(&f.s);
        let b_worker = b_child.worker.clone().expect("w");
        let b_run = b_child.run.clone().expect("r");
        let b_pt =
            f.s.add_provider_thread(&b_thread, "codex", "gpt")
                .expect("pt");
        let a_pt =
            f.s.add_provider_thread(&f.root_thread, "claude", "opus")
                .expect("pt");
        let events_before: i64 =
            f.s.conn
                .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
                .expect("count");
        let base = || child_event(&f.root_thread, &f.root.worker, "x");
        let alias = |pt: &str, worker: &WorkerId| NewAlias {
            provider_thread: pt.to_owned(),
            process_generation: 1,
            native_worker_id: "agent-1".into(),
            worker: worker.clone(),
        };
        let mut cases: Vec<(&str, PublishEvent)> = Vec::new();
        let mut e = base();
        e.run_states = vec![(b_run.clone(), RunState::Running)];
        cases.push(("a run of another root", e));
        let mut e = base();
        e.aliases = vec![alias(&a_pt, &b_worker)];
        cases.push(("an alias to another root's worker", e));
        let mut e = base();
        e.aliases = vec![alias(&b_pt, &a_worker)];
        cases.push(("an alias on another root's provider thread", e));
        let mut e = base();
        e.links[0].run = Some(b_run.clone());
        cases.push(("a link to another root's run", e));
        let mut e = base();
        e.links[0].run = Some(a_run.clone());
        cases.push(("a link to another worker's run", e));
        let mut e = base();
        e.provider_thread = Some(b_pt.clone());
        cases.push(("another root's provider thread", e));
        let mut e = base();
        e.worker_states = vec![(b_worker.clone(), WorkerState::Closed, None)];
        cases.push(("another root's worker state", e));
        for (what, e) in cases {
            assert!(f.s.publish_delegation_event(&e).is_err(), "{what}");
        }
        let events_after: i64 =
            f.s.conn
                .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
                .expect("count");
        assert_eq!(events_after, events_before, "no refused event was stored");
        let run = f.s.delegation_run(b_run.as_str()).expect("q").expect("row");
        assert_eq!(run.state, "reserved");
        assert_eq!(
            f.s.delegation_native_alias(&b_pt, 1, "agent-1").expect("q"),
            None
        );
        assert_eq!(
            f.s.delegation_native_alias(&a_pt, 1, "agent-1").expect("q"),
            None
        );

        // Relink: a foreign alias, a foreign run or another root's event is refused, and the
        // stream epoch does not move.
        let b_event =
            f.s.publish_delegation_event(&child_event(&b_thread, &b.worker, "b"))
                .expect("b event");
        let link = |run: Option<RunId>| LinkSpec {
            worker: a_worker.clone(),
            run,
            category: EventCategory::Activity,
            invocation: None,
        };
        let a_event =
            f.s.publish_delegation_event(&child_event(&f.root_thread, &f.root.worker, "a"))
                .expect("a event");
        let refused = [
            f.s.relink_delegation_history(&f.root.worker, &[], &[alias(&a_pt, &b_worker)]),
            f.s.relink_delegation_history(&f.root.worker, &[], &[alias(&b_pt, &a_worker)]),
            f.s.relink_delegation_history(
                &f.root.worker,
                &[(a_event.seq, link(Some(b_run.clone())))],
                &[],
            ),
            f.s.relink_delegation_history(&f.root.worker, &[(b_event.seq, link(None))], &[]),
        ];
        for (i, r) in refused.iter().enumerate() {
            assert!(r.is_err(), "relink case {i}: {r:?}");
        }
        assert_eq!(epochs(&f.s, &f.root.root).1, 0);
        assert!(f.s.delegation_event_links(b_event.seq).expect("q").len() == 1);
        // The legitimate relink still works.
        f.s.relink_delegation_history(
            &f.root.worker,
            &[(a_event.seq, link(Some(a_run)))],
            &[alias(&a_pt, &a_worker)],
        )
        .expect("own relink");
    }

    #[test]
    fn a_stored_failure_is_scrubbed_through_its_nested_details() {
        let f = fx();
        let op = spawn(&f.s, &f.root.worker, "k1");
        f.s.reserve_delegation_dispatch(&op.operation)
            .expect("s")
            .expect("reserved");
        let token = format!("ghp_{}", "q".repeat(36));
        let failure = ToolError::new(
            ErrorCode::ProviderUnavailable,
            format!("refused with {token}"),
        )
        .with_detail(
            "nested",
            json!({"deep": [format!("Bearer {token}"), {"password": "hunter2-hunter2"}],
                   "note": format!("key={token}")}),
        );
        f.s.record_delegation_dispatch(&op.operation, &DispatchFact::Failed(failure))
            .expect("failed");
        let stored =
            f.s.delegation_operation(op.operation.as_str())
                .expect("q")
                .expect("row")
                .error_json
                .expect("error");
        for secret in [token.as_str(), "qqqqqqqqqqqq", "hunter2-hunter2"] {
            assert!(!stored.contains(secret), "{secret} in {stored}");
        }
        let again = accept(
            &f.s,
            &f.root.worker,
            spawn_req("k1", "review the parser"),
            Resolution::default(),
            AcceptPlan::Spawn(spawn_plan()),
        );
        let Ok(Accepted::Replay(ReplayOutcome::Error(e))) = again else {
            panic!("{again:?}")
        };
        assert_eq!(
            e.code,
            ErrorCode::ProviderUnavailable,
            "the code is kept exactly"
        );
        let replayed = serde_json::to_string(&e).expect("json");
        assert!(!replayed.contains(&token), "{replayed}");
        assert!(!replayed.contains("hunter2-hunter2"), "{replayed}");
    }

    #[test]
    fn a_mailbox_read_never_scans_unrelated_message_history() {
        let f = fx();
        let a = spawn(&f.s, &f.root.worker, "k1").worker.expect("w");
        let b = spawn(&f.s, &f.root.worker, "k2").worker.expect("w");
        // A long history between the root and B that is none of A's business.
        for i in 0..300 {
            new_op(send(
                &f.s,
                &f.root.worker,
                b.as_str(),
                "instruction",
                "work",
                &format!("s{i}"),
            ));
            new_op(send(&f.s, &b, "parent", "report", "done", &format!("r{i}")));
        }
        let mut q = query(&f.root.worker, &a, vec![a.clone()], None);
        q.max_events = 10;
        PAGE_ROWS.with(|c| c.set(0));
        let page = f.s.read_delegation_page(&q).expect("s").expect("page");
        let rows = PAGE_ROWS.with(std::cell::Cell::get);
        assert!(rows <= 11, "examined {rows} rows");
        assert!(seqs(&page.page).len() <= 1, "only A's own spawn event");
        // The root's own mailbox is still bounded by the page, not the history.
        let mut q = query(&f.root.worker, &f.root.worker, vec![], None);
        q.max_events = 10;
        PAGE_ROWS.with(|c| c.set(0));
        let page = f.s.read_delegation_page(&q).expect("s").expect("page");
        assert_eq!(PAGE_ROWS.with(std::cell::Cell::get), 11);
        assert!(page.truncated);
        assert_eq!(page.page["messages"].as_array().expect("m").len(), 10);
        assert!(page.page["messages"]
            .as_array()
            .expect("m")
            .iter()
            .all(|m| m["to"] == json!(f.root.worker.as_str())));
        // Both halves of the query walk the (worker_id, event_seq) index, never the whole table.
        let mut stmt =
            f.s.conn
                .prepare(&format!("EXPLAIN QUERY PLAN {PAGE_SQL}"))
                .expect("plan");
        let plan: Vec<String> = stmt
            .query_map(params![f.root.root, 0, "[]", true, a.as_str(), 11], |r| {
                r.get::<_, String>(3)
            })
            .expect("q")
            .collect::<rusqlite::Result<_>>()
            .expect("rows");
        let links: Vec<&String> = plan
            .iter()
            .filter(|l| l.contains("delegation_event_links") || l.starts_with("SEARCH l"))
            .collect();
        assert!(!links.is_empty(), "{plan:?}");
        for l in &links {
            assert!(l.contains("INDEX"), "a full scan of the links: {plan:?}");
        }
    }
}
