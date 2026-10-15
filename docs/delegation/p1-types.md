# P1: pure delegation types in agent-core

Builds what `docs/delegation/contract-v1.md` (revision 6) freezes, in `crates/agent-core` only:
- no I/O, no GTK, no RNG, no clock;
- the caller supplies the bytes, times, ids, file listings and `/proc` text;
- everything is exported through `lib.rs` (`pub mod delegation;`);
- no tool is exposed yet, and no adapter behaviour changes.

## Module layout (`crates/agent-core/src/delegation/`)

| File | Contents |
| --- | --- |
| `mod.rs` | `pub const API_VERSION: u32 = 1; pub const SERVER_NAME: &str = "agent_terminal"; pub const BRIDGE_ARG: &str = "--cross-agent-mcp";` re-exports |
| `ids.rs` | handle newtypes + validation |
| `token.rs` | `CapabilityToken`, `InstanceNonce` |
| `env.rs` | env var names, strip list |
| `tools.rs` | `ToolName`, JSON Schemas (`input_schema`, `output_schema`) |
| `wire.rs` | tool request/response structs, `MutationReceipt`, `Warning`, `StateRef`, output budget and capabilities truncation |
| `error.rs` | `ErrorCode`, `ToolError` |
| `ipc.rs` | `Frame`, `FrameDecoder`, `encode_frame`, connection protocol checks |
| `authority.rs` | `TaskMode`, ceilings, `InheritedAuthority`, workspace rules, revocation table |
| `identity.rs` | `Provenance`, `ValueProvenance`, `Identity`, `observe` |
| `lifecycle.rs` | worker and run reducers, cancel completion |
| `operation.rs` | operation reducer, idempotency decision, canonical request, replay |
| `mailbox.rs` | message kinds/direction, delivery planning, message reducer, ACK eligibility, continuation budget |
| `page.rs` | cursor payload, filter digest, page assembly by bytes |
| `correlation.rs` | `InvocationKey`, `invocation_from_meta`, `ObservedCalls`, attestation |
| `route.rs` | `DelegationCaps`, `RouteStatus`, `RouteBlocker`, `RouteQuery`, `admit_route()` |
| `launch.rs` | `DelegationBinding`, Claude overlay/args/read-back, Codex config/read-back |
| `agy_guard.rs` | §7 pure parts: L2 directory list, source names, argv/env verdicts, `/proc/<pid>/stat` parsing |
| `limits.rs` | `Limits`, admission checks |
| `scope.rs` | `Scope`, `ScopedEnvelope` |

## Types

```rust
// ids.rs: each a #[serde(transparent)] String newtype with `parse(&str) -> Result<Self, IdError>`,
// `as_str()`, Display; Debug prints the value (handles are not secrets).
pub struct WorkerId(String);      // "w_" + 8..40 [A-Za-z0-9]
pub struct RunId(String);         // "r_"
pub struct MessageId(String);     // "m_"
pub struct OperationId(String);   // "op_"
pub struct Cursor(String);        // 1..=256 bytes, opaque
pub struct IdempotencyKey(String);// ^[A-Za-z0-9._:-]{1,128}$
pub struct ProfileId(String);     // ^[a-z0-9-]{1,64}$
pub struct AuthorityEpoch(pub u64);
pub struct ProcessGeneration(pub u64);
pub enum IdError { Empty, TooLong, BadPrefix, BadChar }
impl WorkerId { pub fn from_suffix(suffix: &str) -> Result<Self, IdError> } // app supplies random suffix; same for others

// token.rs
pub struct CapabilityToken(String);   // "atdc_" + 43 base64url
impl CapabilityToken {
    pub const PREFIX: &'static str = "atdc_";
    pub fn from_random(bytes: [u8; 32]) -> Self;
    pub fn parse(s: &str) -> Result<Self, IdError>;
    pub fn expose_secret(&self) -> &str;          // the only accessor
    pub fn ct_eq(&self, other: &str) -> bool;     // constant time over equal-length input
}
// Debug prints "atdc_****"; NO Display; Serialize/Deserialize as string (used only by ipc::Hello).
pub struct InstanceNonce(String);     // 32 lowercase hex; from_bytes([u8;16]); ct_eq

// env.rs
pub const ENV_ENDPOINT: &str = "AGENT_TERMINAL_DELEGATION_ENDPOINT";
pub const ENV_INSTANCE: &str = "AGENT_TERMINAL_DELEGATION_INSTANCE";
pub const ENV_CAP: &str = "AGENT_TERMINAL_DELEGATION_CAP";
pub const ENV_APP_PID: &str = "AGENT_TERMINAL_DELEGATION_APP_PID";   // lineage self-check (contract §1)
pub const STRIP_ON_SPAWN: [&str; 5] = [ENV_ENDPOINT, ENV_INSTANCE, ENV_CAP, ENV_APP_PID, "AGENT_TERMINAL_APPROVAL_SOCKET"];

// tools.rs
#[serde(rename_all = "snake_case")]
pub enum ToolName { CrossAgentCapabilities, CrossAgentSpawn, CrossAgentSend, CrossAgentRead,
                    CrossAgentWait, CrossAgentList, CrossAgentCancel, CrossAgentClose }
impl ToolName {
    pub const ALL: [ToolName; 8];
    pub fn as_str(self) -> &'static str;          // "cross_agent_spawn"
    pub fn parse(s: &str) -> Option<Self>;
    pub fn is_mutating(self) -> bool;             // spawn, send, cancel, close (read only with acknowledge: see ToolRequest::is_mutating)
    // Attestation is NOT limited to mutating tools: for agy every tool is attested (contract §4).
    pub fn description(self) -> &'static str;     // self-explanatory (ToolSearch deferral)
    pub fn input_schema(self) -> serde_json::Value;   // additionalProperties false, bounds per contract §3
    pub fn output_schema(self) -> serde_json::Value;
}
pub const DEGRADED_TOOLS: [ToolName; 1] = [ToolName::CrossAgentCapabilities];
pub const DELEGATION_SCOPE_TEXT: &str = "Depth and mode limits apply to agents started through agent-terminal. \
    Delegated agents cannot start their own built-in sub-agents. Your own sessions keep their built-in sub-agents, \
    which agent-terminal shows but does not limit.";   // contract §6

// error.rs
#[serde(rename_all = "snake_case")]
pub enum ErrorCode { RuntimeUnavailable, AuthenticationFailed, WorkspaceUntrusted, ProtocolIncompatible,
    InvalidArgument, NotFound, ProviderUnavailable, RouteUnverified, UnsupportedDelivery, UnsupportedEffort,
    UnsupportedWorkspace, AuthorityDenied, ApprovalRequired, QuotaExhausted, CapacityReached,
    IdempotencyConflict, StaleEpoch, StaleRun, CursorExpired, WorkerClosed, DeadlineExceeded,
    PersistenceFailed, DeliveryUnknown, TransportLost, InternalError, #[serde(other)] Unknown }
impl ErrorCode { pub const ALL: [ErrorCode; 26]; pub fn retryable(self) -> bool; }  // 25 codes + Unknown; table in contract §3; InternalError not retryable
pub struct ToolError { pub code: ErrorCode, pub message: String, pub retryable: bool,
                       #[serde(default, skip_serializing_if = "Option::is_none")] pub details: Option<Value> }
impl ToolError { pub fn new(code: ErrorCode, message: impl Into<String>) -> Self; } // retryable from code

// wire.rs: one struct per tool input/output, serde deny_unknown_fields on inputs.
pub enum WorkspaceRequest { SharedReadOnly, IsolatedWorktree { base: Option<String> } } // tag "kind"
pub enum Target { Parent, Worker(WorkerId) }   // "parent" | "w_…" (custom serde)
pub enum Delivery { Auto, ThisRun, NextTurn }
pub struct SpawnRequest { profile_id: ProfileId, name: String, task: String, task_mode: TaskMode,
    model: Option<String>, effort: Option<String>, workspace: WorkspaceRequest,
    expected_authority_epoch: Option<u64>, idempotency_key: IdempotencyKey }
pub struct SendRequest { target: Target, text: String, kind: MessageKind, reply_to: Option<MessageId>,
    delivery: Delivery, expected_run_id: Option<RunId>, expected_authority_epoch: Option<u64>,
    idempotency_key: IdempotencyKey }
pub struct ReadRequest { target: Option<WorkerId>, after: Option<Cursor>, max_events: Option<u32>,
    max_bytes: Option<u32>, acknowledge: Option<Vec<MessageId>>, idempotency_key: Option<IdempotencyKey>,
    expected_authority_epoch: Option<u64> }
pub struct WaitRequest { workers: Option<Vec<WorkerId>>, mailbox: Option<bool>, after: Option<Cursor>,
    timeout_ms: Option<u32>, max_events: Option<u32>, max_bytes: Option<u32> }
pub struct ListRequest { target: Option<WorkerId>, cursor: Option<Cursor>, limit: Option<u32> }
pub struct CancelRequest { worker_id: WorkerId, expected_run_id: Option<RunId>,
    expected_authority_epoch: Option<u64>, idempotency_key: IdempotencyKey }
pub struct CloseRequest { worker_id: WorkerId, expected_authority_epoch: Option<u64>, idempotency_key: IdempotencyKey }
pub struct CapabilitiesRequest { profile_id: Option<ProfileId> }
#[serde(rename_all = "snake_case")]
pub enum DispatchState { Accepted, DispatchReserved, Submitted, Confirmed, Failed, #[serde(other)] Unknown }
pub struct Warning { pub code: String, pub message: String }
pub struct MutationReceipt { pub api_version: u32, pub operation_id: OperationId, pub authority_epoch: u64,
    pub replayed: bool, pub dispatch: DispatchState, #[serde(default)] pub warnings: Vec<Warning> }
// Each receipt embeds it with #[serde(flatten)]: SpawnReceipt, SendReceipt, CancelReceipt, CloseReceipt, AckOutcome.
#[serde(tag = "domain", content = "value", rename_all = "snake_case")]
pub enum StateRef { Worker(WorkerState), Run(RunState) }
#[serde(rename_all = "snake_case")]
pub enum ThisRunSupport { Strict, BestEffort, Unsupported }     // capabilities delivery.this_run, Controls.this_run
pub struct Controls { pub send: bool, pub this_run: ThisRunSupport, pub cancel: bool, pub close: bool }
pub struct ModeAdmission { pub task_mode: TaskMode, pub route_status: RouteStatus, pub route_blockers: Vec<String>,
                           pub child_bridge: ChildBridge }      // capabilities profiles[].modes, one per TaskMode
pub const MAX_SUMMARY_BYTES: usize = 48 * 1024;               // serialised WorkerSummary (contract §2 output bounds)
pub fn fit_summary(s: &mut WorkerSummary);                     // shortens path (middle "…"), then last_activity, to the field bounds
/// Contract §3 "Output budget": structuredContent + the same JSON as an escaped text block, measured on the real serialisation.
pub const OUTPUT_BUDGET: usize = 786_432;
pub const MAX_WARNINGS: usize = 8;
pub const MAX_ERROR_DETAILS_BYTES: usize = 64 * 1024;          // holds delivery_unknown.details.receipt of a maximal receipt
pub const MAX_RECEIPT_BYTES: usize = 48 * 1024;                // any MutationReceipt-based output, worst-case escaping
pub const MAX_ACK_BYTES: usize = 64 * 1024;                    // read's ack object
pub const MAX_REQUEST_ID_BYTES: usize = 256;                   // encoded JSON-RPC id (bridge checks before dispatch)
pub fn request_id_ok(encoded_id: &str) -> bool;               // false → -32600 with id null, never echoed
/// The final guard (contract §3): replaces an over-budget output with InternalError (details.reason "output_budget",
/// details.operation_id); never touches the stored operation.
pub fn guard_output(output: Value, operation: Option<&OperationId>) -> Result<Value, ToolError>;
pub fn transport_cost(output: &Value) -> usize;               // len(json) + len(json_string(json))
pub struct Omitted { pub profiles: u32, pub models: u32 }
// CapabilitiesResponse gains `truncated: bool` and `omitted: Omitted`; ListPage gains `truncated: bool`.
/// Deterministic: keeps Settings/catalogue order; drops the last model of the last profile that still lists one until it
/// fits, and only then whole profiles from the end; never the first profile; never shortens a profile's identity or
/// status fields. Sets `truncated` and `omitted`. Post-condition: transport_cost(resp) <= OUTPUT_BUDGET.
pub fn truncate_capabilities(resp: &mut CapabilitiesResponse);
// Outputs: CapabilitiesResponse (+ delegation_scope, profiles[].modes: Vec<ModeAdmission>, truncated, omitted),
// SpawnReceipt, SendReceipt { #[flatten] receipt, message_id, delivery, target_run_id: Option<RunId> /* frozen at acceptance */ },
// Page (read; WaitPage = Page + timed_out), ListPage, CancelReceipt, CloseReceipt,
// WorkerSummary (+ delegation: DelegationStatus), PageEvent { seq, …, state: Option<StateRef> },
// PageMessage { seq, …, delivery, recipient_run_id }, AckOutcome { #[flatten] receipt, acknowledged, rejected }:
// fields exactly as contract §2/§3 (all pub, snake_case).
#[serde(rename_all = "snake_case")]
pub enum DelegationStatusCode { Available, WorkspaceUntrusted, RebindRequired, Restricted, BridgeDisabled, Unavailable }
impl DelegationStatusCode { pub const ALL: [DelegationStatusCode; 6]; pub fn auto_rebind(self) -> bool; } // true only for RebindRequired
pub struct DelegationStatus { pub code: DelegationStatusCode, pub reason: Option<String> /* ≤256 B */,
                              pub paths: Vec<String> /* ≤16 relative, ≤256 B each */ }   // same enum in store, wire and P1
pub enum ToolRequest { Capabilities(CapabilitiesRequest), Spawn(SpawnRequest), Send(SendRequest), Read(ReadRequest),
    Wait(WaitRequest), List(ListRequest), Cancel(CancelRequest), Close(CloseRequest) }
pub fn parse_request(tool: ToolName, args: &Value, limits: &Limits) -> Result<ToolRequest, ToolError>; // bounds + schema
impl ToolRequest {
    pub fn validate(&self, limits: &Limits) -> Result<(), ToolError>;  // cross-field rules (§3)
    pub fn is_mutating(&self) -> bool;                                 // read counts only with acknowledge
    pub fn normalised(&self) -> ToolRequest;                           // every default filled (§2 canonical request)
}

// ipc.rs
pub const MAX_FRAME: usize = 1 << 20;
pub const WELCOME_DEADLINE_MS: u64 = 2_000;     // bridge side, from process start (§1)
pub const RECONNECT_DEADLINE_MS: u64 = 2_000;   // bridge side, fresh, from the detected loss (§4)
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Frame { Hello(Hello), Welcome(Welcome), Refused { error: ToolError }, Call(Call),
                 Result(CallResult), Cancel { id: u64 }, Revoked { error: ToolError } }
pub struct ApiRange { pub min: u32, pub max: u32 }
pub struct Hello { pub api: ApiRange, pub instance: InstanceNonce, pub cap: CapabilityToken, pub bridge: BridgeInfo } // Debug redacts via token
pub struct BridgeInfo { pub version: String, pub pid: u32 }
pub struct Principal { pub worker_id: WorkerId, pub root_id: WorkerId, pub driver: Driver, pub depth: u8,
                       pub can_spawn: bool, pub task_mode_ceiling: Vec<TaskMode> }  // informational; ceiling is live (§4)
pub struct Welcome { pub api: u32, pub connection: String, pub principal: Principal,
                     pub authority_epoch: u64, pub limits: IpcLimits }
pub struct IpcLimits { pub max_frame: u32, pub max_inflight: u32 }
pub struct Call { pub id: u64, pub tool: ToolName, pub args: Value, pub timeout_ms: u32,
                  pub invocation: Option<InvocationKey> }
pub struct CallResult { pub id: u64, #[serde(flatten)] pub outcome: CallOutcome }
pub enum CallOutcome { Ok(Value), Err(ToolError) }   // serialised as "ok" | "err" key
pub enum FrameError { Empty, TooLarge(u32), Utf8, Json(String) }
pub fn encode_frame(frame: &Frame) -> Result<Vec<u8>, FrameError>;   // refuses > MAX_FRAME
pub struct FrameDecoder { /* 4-byte header + at most one bounded body */ }
impl FrameDecoder {
    pub fn new() -> Self;
    pub fn wants_bytes(&self) -> usize;                 // 0 while a complete frame is pending: the caller stops reading
    pub fn push(&mut self, bytes: &[u8]) -> Result<usize, FrameError>;  // consumes at most up to the end of one frame; returns bytes taken
    pub fn next_frame(&mut self) -> Option<Result<Frame, FrameError>>;
}
// The length is checked before reserving; after an Err the decoder is poisoned (caller closes).
// Buffered bytes never exceed 4 + MAX_FRAME.
pub enum ConnPhase { PreAuth, Authenticated }
pub enum ProtocolViolation { FrameBeforeHello, SecondHello, DuplicateCallId(u64), UnexpectedFrame(&'static str) }
pub struct ConnState { /* phase + pending call ids */ }
impl ConnState {   // pure server-side protocol checks; the app owns the socket and timers
    pub fn on_frame(&mut self, f: &Frame, inflight_max: u32) -> Result<FrameAction, ProtocolViolation>;
    pub fn on_result_sent(&mut self, id: u64);
}
pub enum FrameAction { Authenticate, Admit(u64), RejectCapacity(u64), CancelCall(u64) }
pub fn negotiate(hello: &ApiRange) -> Result<u32, ErrorCode>;   // ProtocolIncompatible unless 1 in range

// authority.rs
#[serde(rename_all = "snake_case")]
pub enum TaskMode { Ask, AcceptEdits, Plan }
impl TaskMode { pub const ALL: [TaskMode; 3]; pub fn as_mode(self) -> Mode; pub fn writes(self) -> bool; }
pub fn ceiling(principal_mode: Mode) -> &'static [TaskMode];      // uses Mode::session_safe; contract §6 table
pub fn permits(principal_mode: Mode, requested: TaskMode) -> bool;
pub const MAX_DEPTH: u8 = 2;
pub fn can_spawn(depth: u8) -> bool;                               // depth < MAX_DEPTH
pub fn workspace_allowed(mode: TaskMode, ws: &WorkspaceRequest) -> Result<(), ErrorCode>; // writers need worktree; shared needs plan
/// Every parent→child delivery, any kind: recipient's task_mode must be within the sender's live ceiling.
/// Child→parent: kind rules only. Called at acceptance and again at dispatch (E7).
pub fn delivery_allowed(dir: Direction, kind: MessageKind, sender_mode: Mode, recipient_mode: Option<TaskMode>) -> Result<(), ErrorCode>;
pub struct InheritedAuthority { pub worker: WorkerId, pub root: WorkerId, pub parent: Option<WorkerId>,
    pub epoch: AuthorityEpoch, pub policy_revision: u64, pub mode_ceiling: Mode, pub depth: u8,
    pub workspace: WorkspaceScope }
pub enum WorkspaceScope { Shared { root: String }, Lease { source_repo: String, worktree: String,
                          base_commit: String, allowed_relative: Vec<String> } }
impl InheritedAuthority {
    /// The child's root = self.root, parent = Some(self.worker), depth = self.depth + 1 (AuthorityDenied past MAX_DEPTH),
    /// mode within ceiling(self.mode_ceiling), epoch = self.epoch.
    pub fn child(&self, child: WorkerId, mode: TaskMode, scope: WorkspaceScope) -> Result<InheritedAuthority, ErrorCode>;
    pub fn narrow(&self, mode: Mode) -> InheritedAuthority;          // never widens
}
pub enum RevocationEvent { RootStop, RootRestriction { new_mode: Mode }, RootWidening, Close(WorkerId),
                           GenerationExited(WorkerId), AppShutdown }
pub enum RevocationScope { Root, Subtree(WorkerId), None }
pub struct RevocationEffect { pub bump: bool, pub scope: RevocationScope,
    pub kill_tokens: Vec<WorkerId>, pub stop_processes: Vec<WorkerId>, pub mark_restricted: Vec<WorkerId>,
    pub keep_root_token: bool }
/// Contract §6 revocation table, over a caller-supplied tree snapshot: (worker, parent, task_mode).
pub fn revocation(event: &RevocationEvent, tree: &[(WorkerId, Option<WorkerId>, Option<TaskMode>)]) -> RevocationEffect;

// identity.rs
#[serde(rename_all = "snake_case")]
pub enum Provenance { ProviderReported, ProviderAccepted, LaunchConfigured, Unsupported, #[serde(other)] Unknown }
pub struct ValueProvenance { pub requested: Option<String>, pub effective: Option<String>, pub source: Provenance } // Eq
pub struct Identity { pub driver: Driver, pub profile_id: ProfileId, pub model: ValueProvenance, pub effort: ValueProvenance }
pub enum IdentityObservation {
    Launched { model: Option<String>, effort: Option<String> },
    ReportedModel(String),          // Claude assistant.message.model / modelUsage key, per turn
    TurnSucceeded,                  // Claude result ok / Codex turn completed / agy result ok
    TurnFailed { unsupported_model: bool, unsupported_effort: bool },
}
pub fn observe(id: &mut Identity, obs: IdentityObservation);     // rules = contract §5 provenance rows
pub fn model_rule(d: Driver) -> ProvenanceRule; pub fn effort_rule(d: Driver) -> ProvenanceRule;
pub enum ProvenanceRule { ReportedPerTurn, AcceptedOnSuccess, ConfiguredOnly }
// Claude: model ReportedPerTurn, effort ConfiguredOnly; Codex: both AcceptedOnSuccess; agy: model AcceptedOnSuccess, effort ConfiguredOnly.
// AcceptedOnSuccess sets source ProviderAccepted and leaves `effective` None: acceptance is not identity.

// lifecycle.rs (WorkerState is event::WorkerState, extended with Stopping)
pub enum WorkerInput { ProcessStarted, TurnStarted(RunId), InputRequired, InputResolved,
    TurnEnded { run: RunId, state: TurnState }, CancelRequested { bound: Option<RunId> },
    ProcessExited { expected: bool }, CloseRequested, Revoked, RestartReconciled }
pub enum LifecycleError { StaleRun, Closed, Illegal { from: WorkerState, input: &'static str } }
pub fn worker_step(state: WorkerState, active: Option<&RunId>, input: &WorkerInput) -> Result<WorkerState, LifecycleError>;
pub enum RunState { Reserved, Starting, Running, Stopping, Completed, Interrupted, Failed, #[serde(other)] Unknown }
impl RunState { pub fn is_terminal(self) -> bool; }
pub fn run_step(state: RunState, input: &WorkerInput) -> Result<RunState, LifecycleError>;
/// A cancel operation is Confirmed when every run bound at acceptance is terminal (contract §3).
pub fn cancel_complete(bound: &[(RunId, RunState)]) -> bool;
/// The bound set: the target's current run (if any) plus every active descendant run. Empty → `idle`, no dispatch;
/// an idle target with active descendants is non-empty and is dispatched.
pub fn cancel_bound_set(target_run: Option<&RunId>, active_descendants: &[RunId]) -> Vec<RunId>;
// Invariants: Closed is absorbing; worker Stopping→Stopped only on ProcessExited;
// Stopping + TurnEnded(Interrupted) with the process alive → Waiting (run → Interrupted);
// RestartReconciled maps every non-terminal run to Unknown and never to Running.

// operation.rs
pub enum OperationKind { Spawn, Send, Ack, Cancel, Close }
pub use wire::DispatchState as OperationState;
pub fn operation_step(from: OperationState, to: OperationState) -> Result<OperationState, LifecycleError>;
// legal: Accepted→{DispatchReserved,Failed}; DispatchReserved→{Submitted,Confirmed,Failed,Unknown};
//        Submitted→{Confirmed,Failed,Unknown}; Accepted→Failed also on revocation (stale_epoch)
pub fn on_restart(state: OperationState) -> OperationState;   // DispatchReserved|Submitted → Unknown
pub struct CanonicalRequest(String);  // sorted-key JSON of the normalised, scrubbed request minus key/epoch
pub fn canonical_request(req: &ToolRequest, scrub: impl Fn(&str) -> String) -> CanonicalRequest; // uses ToolRequest::normalised
pub struct Resolution { pub target: Option<WorkerId>, pub bound_runs: Vec<RunId>, pub base_commit: Option<String> }
pub struct StoredOperation { pub id: OperationId, pub kind: OperationKind, pub epoch: AuthorityEpoch,
    pub canonical: CanonicalRequest, pub resolution: Resolution,
    pub receipt: Option<Value>,          // original success receipt, written once at acceptance
    pub dispatch: OperationState,        // current, updated separately
    pub error: Option<ToolError> }       // for Failed
pub enum IdempotencyDecision { New, Replay(OperationId), Conflict(OperationId), StaleEpoch(OperationId) }
pub fn idempotency(existing: Option<&StoredOperation>, kind: OperationKind, canonical: &CanonicalRequest,
                   expected_epoch: Option<AuthorityEpoch>, current: AuthorityEpoch) -> Result<IdempotencyDecision, ErrorCode>;
// Order (contract §2): expected_epoch Some(e) != current → Err(StaleEpoch) before lookup; stored epoch older → StaleEpoch;
// different canonical → Conflict; identical → Replay.
pub enum ReplayOutcome { Receipt(Value) /* replayed=true, dispatch=current */, Error(ToolError) /* stored, details.replayed */,
                         DeliveryUnknown(ToolError) /* details.receipt = original */, WaitForDispatch }
pub fn replay(op: &StoredOperation, dispatching_now: bool) -> ReplayOutcome;   // the contract §2 table

// mailbox.rs
#[serde(rename_all = "snake_case")] pub enum MessageKind { Instruction, Report, Question, Reply }
pub enum Direction { ParentToChild, ChildToParent }
pub fn kind_allowed(dir: Direction, kind: MessageKind) -> bool;
pub enum DeliveryState { Queued, QueuedNextTurn, DispatchReserved, Submitted, ProviderConfirmed, Misdelivered, Failed, Cancelled, #[serde(other)] Unknown }
impl DeliveryState { pub const ALL: [DeliveryState; 9]; pub fn is_terminal(self) -> bool; } // terminal: ProviderConfirmed, Misdelivered, Failed, Cancelled
// Misdelivered: consumed in a run other than the one targeted at acceptance (Claude best-effort this_run). A state, never an error.
pub fn delivery_step(from: DeliveryState, to: DeliveryState) -> Result<DeliveryState, LifecycleError>;
pub struct TargetRuntime { pub driver: Driver, pub live_input: bool, pub active_run: Option<RunId>, pub process_alive: bool,
                           pub closed: bool, pub awaiting_result: bool /* agy: written, no result yet */ }
pub enum DeliveryPlan { SteerNow(RunId) /* Codex strict */, WriteLive { target: Option<RunId> } /* Claude: Some for best-effort this_run, None for auto */,
                        StartTurn, QueueNextTurn }
pub fn plan_delivery(t: &TargetRuntime, d: Delivery, expected: Option<&RunId>) -> Result<DeliveryPlan, ErrorCode>;
// ThisRun: per DelegationCaps::of(t.driver).this_run: Strict → SteerNow; BestEffort → WriteLive{Some(run)}; Unsupported → UnsupportedDelivery.
// No live run or a mismatching expected run → StaleRun. At dispatch the app rechecks liveness; a dead target run → message Failed (stale_run).
pub fn on_steer_refused(d: Delivery) -> Result<DeliveryState, ErrorCode>; // Auto → QueuedNextTurn; ThisRun → Err(StaleRun)
/// Claude echo correlation without uuids: FIFO of written (scrubbed text) vs isReplay echoes; mismatch → Unknown.
pub struct EchoQueue { /* bounded FIFO of (MessageId, target: Option<RunId>, text digest) */ }
impl EchoQueue { pub fn written(&mut self, m: MessageId, target: Option<RunId>, text: &str); pub fn echoed(&mut self, text: &str, run: &RunId) -> EchoMatch; }
pub enum EchoMatch { Confirmed { message: MessageId, run: RunId },          // target None, or target == run
                     Misdelivered { message: MessageId, target: RunId, actual: RunId },
                     Mismatch { message: MessageId }, Unexpected }
pub fn ack_eligible(recipient: &WorkerId, caller: &WorkerId, presented: bool, acknowledged: bool) -> Result<bool, ErrorCode>;
// Ok(false) = already acknowledged (idempotent); NotFound if caller is not the recipient; InvalidArgument if not presented.
pub struct ContinuationBudget { pub used: u8, pub reserved: bool }
impl ContinuationBudget { pub const MAX: u8 = 3; pub fn reserve(&mut self) -> bool; pub fn commit(&mut self); pub fn release(&mut self); }

// page.rs
pub struct PageFilter { pub caller: WorkerId, pub target: Option<WorkerId>, pub workers: Vec<WorkerId>, pub mailbox: bool }
pub struct CursorPayload { pub root: WorkerId, pub stream_epoch: u64, pub last_seq: u64, pub filter_digest: [u8; 16] }
pub fn filter_digest(f: &PageFilter) -> [u8; 16];          // order-independent over `workers`; hand-written, no new crate
pub fn encode_cursor(p: &CursorPayload) -> Cursor;        // ≤256 bytes
pub fn decode_cursor(c: &Cursor, f: &PageFilter, stream_epoch: u64) -> Result<CursorPayload, ErrorCode>;
// InvalidArgument (field "after") on a filter mismatch or a malformed cursor; CursorExpired on another stream epoch.
/// One unit per seq: a message seq is a message only, never also an event (contract §3).
pub enum UnitKind { Event, Message }
/// Contract §3 "Selection, then measurement". `Reservation` is an UPPER BOUND, never claimed to be exact.
pub struct Reservation { pub json: usize, pub transport: usize }
impl Reservation {
    /// Per unit: json = len(json(v)); transport = len(json(v)) + len(json_string(json(v))) + 4 (separator and slack).
    pub fn unit(v: &Value) -> Reservation;
    /// The page with empty unit arrays and every variable envelope field at its maximum (snapshot, ack, 256 B cursor, all flags).
    pub fn max_envelope(page_with_empty_units: &Value) -> Reservation;
}
/// Step 1: reservation. Units in seq order while json(snapshot + units) <= max_bytes and envelope + units <= budget.
/// Always takes the first unit if any. Never skips.
pub fn reserve(envelope: Reservation, snapshot_json: usize, units: &[(u64 /*seq*/, UnitKind, Reservation)],
               max_units: u32, max_bytes: u32, budget: usize) -> usize /*taken*/;
/// Steps 2–4: `build(n)` serialises the actual page with the first n units; its measured transport_cost decides.
/// Drops the last unit while over budget, never below 1. Err(InternalError) only if the one-unit page is over budget.
pub fn settle(taken: usize, budget: usize, build: impl Fn(usize) -> Value) -> Result<(Value, bool /*truncated*/), ToolError>;
// assemble_list: the same reserve + settle over WorkerSummary values (at least one worker).

// correlation.rs
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum InvocationKey { Claude { tool_use_id: String }, Codex { thread_id: String, call_id: String },
                         Agy { conversation_id: String, step_index: u64 } }
pub fn invocation_from_meta(meta: &serde_json::Map<String, Value>) -> Option<InvocationKey>; // whitelist only, ≤256 B each
pub struct ObservedCalls { /* ≤ Limits::observed_calls per generation; oldest dropped first */ }
impl ObservedCalls {
    /// `salt`: 32 random bytes from the app, one per principal generation (P1 has no RNG).
    pub fn new(salt: [u8; 32], cap: usize) -> Self;
    /// Both take the output of `attestation_arguments`; nothing else may be passed (one helper, both sides).
    pub fn record(&mut self, key: InvocationKey, tool: ToolName, attestation_args: &AttestationArgs);
    /// Consumes the match. Never grants authority: the caller only denies on Unattested.
    pub fn attest(&mut self, key: Option<&InvocationKey>, tool: ToolName, attestation_args: &AttestationArgs) -> Attestation;
}
pub enum Attestation { Attested, Unattested }
/// Contract §2 "Attestation arguments": the ONE helper for both the stream step (record) and the bridge call (attest).
/// `raw` may be an object or a JSON string holding one. Validated by the tool schema, syntax-normalised (defaults
/// filled), ALL input fields kept (idempotency_key and expected_authority_epoch included), NOT scrubbed, sorted-key JSON.
/// Distinct from operation::canonical_request (scrubbed, key/epoch removed), which is for idempotency only.
pub struct AttestationArgs(String);   // Debug prints only its length: it may hold un-redacted text
pub fn attestation_arguments(tool: ToolName, raw: &Value, limits: &Limits) -> Result<AttestationArgs, ToolError>;
// Kept only as a 128-bit digest: two std SipHash passes, each over (half the salt, the args), the same construction as
// redact::stream's key digest. No new dependency; never persisted or logged (contract §8 item 8).
pub fn attestation_required(driver: Driver, tool: ToolName, gate_passed: bool) -> bool; // agy: every tool; Claude/Codex: every tool once G-C5/G-X5 pass
// (matches_session is removed: Claude keys carry no session; attestation against observed calls replaces it.)

// route.rs
pub enum RouteStatus { Verified, Unverified, Unsupported }
pub enum RouteBlocker {
    ClaudeBashUnderRestricted,    // G-C1 (writer modes)
    ClaudeTokenPersistence,       // G-C3
    CodexAmbientMcp,              // G-X1
    CodexNativeDelegation,        // G-X2
    CodexTokenPersistence,        // G-X3
    CodexOverlayOnResume,
    AgyHookProofFix,              // PR #44 merged: workspace plugin hooks, config-dir flags dropped, fresh proof per start
    AgyHookProofInWorktree,
    AgyWorkspaceMcpCheck,         // §7 L2 in full (non-git walk, recursive scan, argv/env/home, allow rules)
    AgyPeerAndSingleUse,          // §7 L3 + census + L4 + hello window
    AgyAttestation,               // G-A5
    AgyHookPayloadUnrecorded,     // G-A1: also denies other servers and the sub-agent tools in children
    AgyTokenPersistence,          // G-A3
    AgyManagedEntry, AgyAllowRules, AgentSyncEntry, EvidenceBundle }
pub enum BlockerScope { AllModes, WriterModes }                 // G-C1 is WriterModes; every other blocker AllModes
pub fn blocker_scope(b: RouteBlocker) -> BlockerScope;
#[serde(rename_all = "snake_case")]
pub enum ChildBridge { WithBridge, WithoutBridge }               // WithoutBridge only meaningful for an agy child
pub struct RouteQuery { pub parent: Driver, pub child: Driver, pub task_mode: TaskMode, pub child_bridge: ChildBridge }
pub struct RouteSpec { pub status: RouteStatus, pub blockers: Vec<RouteBlocker> }
/// Contract §11 route admission: parent group + child group (variant-specific for agy), mode-conditional blockers
/// counted only for their modes. All Unverified in P1. There is no pair-only route() any more.
pub fn admit_route(q: &RouteQuery) -> RouteSpec;
/// The agy child variant the supervisor uses: WithBridge if admitted and L2 passed, else WithoutBridge if admitted, else None.
pub fn agy_child_variant(parent: Driver, mode: TaskMode, l2_passed: bool) -> Option<ChildBridge>;
pub enum ParentBinding { LaunchOverlay, PerThreadConfig, PersistentGlobal }
pub enum TokenPlacement { ServerEnv, ProcessEnv }
pub enum TokenReuse { SequentialPerGeneration, SingleUse }
pub enum BridgeRestart { RespawnedByProvider, DeadUntilResume, DeadUntilRelaunch }
pub enum BridgeGate { HostAutoAllowByServerName, ApproveMode, AllowRulesOrHook }
pub enum PlanEnforcement { ToolList, Sandbox, Hook }
pub enum ChildMcpIsolation { StrictConfig, DisabledAndReadBack, HookDeniesOthers }
pub enum NativeDelegationControl { ToolListReadBack, Unproven, HookDenies }
pub enum Rebind { ResumeWithFreshOverlay, ThreadResumeWithFreshConfig, RelaunchConversation }
pub enum Receipt { ReplayEchoFifo, SteerAndItem, None }
pub struct DelegationCaps { pub parent_binding: ParentBinding, pub token_placement: TokenPlacement,
    pub token_reuse: TokenReuse, pub peer_check_required: bool, pub attestation_required: bool,
    pub bridge_restart: BridgeRestart, pub rebind: Rebind,
    pub bridge_gate: BridgeGate, pub child_requires_restricted: bool, pub child_requires_proven_hook: bool,
    pub child_mcp_isolation: ChildMcpIsolation, pub native_delegation: NativeDelegationControl,
    pub plan_enforcement: PlanEnforcement, pub this_run: ThisRunSupport /* Codex Strict, Claude BestEffort, agy Unsupported */, pub receipt: Receipt,
    pub write_only_after_result: bool, pub mcp_cancel_on_interrupt: bool }
impl DelegationCaps { pub fn of(d: Driver) -> Self; }
// Separate from caps::Capabilities on purpose: those are UI Features guarded 1:1 by Feature::ALL;
// these are role facts no control renders. Same exhaustive-destructure guard test.

// launch.rs
pub struct DelegationBinding { pub command: String /*absolute*/, pub endpoint: String,
    pub instance: InstanceNonce, pub token: CapabilityToken }   // Clone, PartialEq, Debug (redacted); NOT Serialize
impl DelegationBinding {
    pub fn env(&self, app_pid: u32) -> [(&'static str, String); 4];   // ENDPOINT, INSTANCE, CAP (secret), APP_PID
    pub fn claude_overlay(&self) -> Value;  // {"mcpServers":{"agent_terminal":{"type":"stdio","command":…,"args":["--cross-agent-mcp"],"env":{…}}}}
    pub fn codex_thread_config(&self) -> serde_json::Map<String, Value>; // dotted keys mcp_servers.agent_terminal.{command,args,env,default_tools_approval_mode,tool_timeout_sec}
    /// Child: the above plus mcp_servers.<name>.enabled=false for every name in `disable` (G-X1 first mechanism).
    pub fn codex_child_config(&self, disable: &[String]) -> serde_json::Map<String, Value>;
}
pub const CODEX_BUILTIN_SERVERS: &[&str] = &["codex_apps"];     // seen in every Codex fixture
pub const CLAUDE_CHILD_TOOLS_PLAN: &[&str] = &["Read", "Glob", "Grep", "TodoWrite"];
pub const CLAUDE_CHILD_TOOLS_WRITE: &[&str] = &["Read", "Glob", "Grep", "TodoWrite", "Edit", "Write", "NotebookEdit", "Bash"];
pub fn claude_child_args(mode: TaskMode, overlay_path: &str) -> Vec<String>;
// ["--restricted","--strict-mcp-config","--permission-prompt-tool","stdio","--permission-mode","default",
//  "--mcp-config",overlay_path,"--tools",<comma list>]
pub fn claude_parent_args(overlay_path: &str) -> Vec<String>;   // ["--mcp-config", path]
pub fn is_own_bridge_call(server_name: &str, source: Option<&str>) -> bool; // == SERVER_NAME && source == Some("dynamic") (Claude)
/// Child read-backs (contract §5); false → stop the child before its first turn.
pub fn claude_child_init_ok(mode: TaskMode, init_tools: &[String], init_mcp_servers: &[String]) -> bool;
pub fn codex_child_servers_ok(started: &[String]) -> bool;     // exactly ["agent_terminal"]
pub fn overlay_file_name(app_pid: u32, worker: &WorkerId, generation: ProcessGeneration) -> String; // mcp-<pid>-<w>-<gen>.json
pub fn overlay_sweep_candidate(name: &str) -> Option<u32>;    // the app pid of an `mcp-*.json`, else None

// agy_guard.rs (contract §7; the app does the I/O and passes the results in)
pub const CONFIG_REDIRECT_VARS: [&str; 3] = ["ANTIGRAVITY_APP_DATA_DIR", "JETSKI_APP_DATA_DIR", "CASCADE_GLOBAL_CONFIG_OVERRIDE"];
pub const REDIRECT_VAR_PREFIX: &str = "CASCADE_";
pub const CONFIG_DIR_FLAGS: [&str; 2] = ["gemini_dir", "app_data_dir"];
pub const WORKSPACE_SOURCE_NAMES: [&str; 7] = ["mcp_config.json", "hooks.json", "sidecars.json", "sidecars",
                                              "plugin.json", "plugins.json", "plugins.txt"];   // agy d2-config-probes.txt
pub const ALLOW_RULES: [&str; 8];                                  // "mcp(agent_terminal/<tool>)" for ToolName::ALL
pub enum L2Refusal { ArgvRedirect(String), EnvRedirect(String), HomeMismatch, AllowRulesMissing(Vec<String>),
                     WorkspaceSource(String /*relative path*/), Unreadable(String), ScanLimit, DiscoveryFailed }
pub fn argv_redirect(argv: &[String]) -> Option<String>;     // same classification as agy.rs (PR #44)
pub fn env_redirect(env: &[(String, String)]) -> Option<String>;   // a name only, never a value
/// Git: cwd..=toplevel. Non-git (toplevel None): cwd and every ancestor to "/". Pure path math on physical paths.
pub fn l2_directories(physical_cwd: &Path, toplevel: Option<&Path>) -> Vec<PathBuf>;
pub fn allow_rules_missing(settings_json: &str) -> Result<Vec<&'static str>, L2Refusal>;
/// `/proc/<pid>/stat`: fields are parsed after the LAST ')' (comm may hold spaces and parentheses).
pub fn stat_ppid(stat: &str) -> Option<u32>;
/// `/proc/<pid>/cmdline` (NUL-separated): argv after argv[0] is exactly ["--cross-agent-mcp"]. The approval hook
/// (`--approval-hook`, same executable, also a direct child of agy) is never a bridge.
pub fn is_bridge_cmdline(cmdline: &[u8]) -> bool;
/// Contract §1 lineage self-check, bridge side: the parent's ppid (from its stat text) equals ENV_APP_PID.
pub fn lineage_ok(parent_stat: &str, app_pid: &str) -> bool;
pub enum HelloVerdict { Accept, RefuseNoConsume /* fails L3: not agy's direct child, or not a bridge argv; notice only */,
                        RefuseLostIpc /* same pid, earlier connection closed → rebind_required */,
                        Compromise /* a second bridge that passes L3 → revoke generation */ }
/// Pure decision from the peer facts (ppid, exe match, bridge argv), the token's state and any earlier peer pid.
pub fn hello_verdict(peer_is_direct_child: bool, exe_matches: bool, bridge_argv: bool,
                     token_consumed_by: Option<u32>, peer_pid: u32, earlier_live: bool) -> HelloVerdict;
/// Census: count of (direct child, exe match, bridge argv) processes must be 1 and equal the connected peer.
pub fn census_ok(bridges: &[u32], peer_pid: u32) -> bool;

// limits.rs
pub struct Limits { /* every row of contract §9 as a pub field, including ipc_preauth, write/body deadlines,
                       outbound queue frames/bytes, bridge stdio bytes, welcome and reconnect deadlines, agy hello
                       window, observed_calls, attestation wait, l2 scan depth/entries, summary/list/capabilities
                       output bounds */ }
impl Default for Limits { /* contract §9 values */ }
pub struct Usage { pub active_runs_root: u32, pub active_runs_app: u32, pub live_processes: u32,
                   pub pending_root: u32, pub pending_app: u32, pub waits_app: u32, pub waits_principal: u32,
                   pub connections: u32, pub preauth_connections: u32 }
pub fn admit_spawn(l: &Limits, u: &Usage, needs_process: bool) -> Result<(), ToolError>;   // CapacityReached + details.limit
pub fn admit_wait(l: &Limits, u: &Usage) -> Result<(), ToolError>;
pub fn admit_connection(l: &Limits, u: &Usage) -> bool;
pub fn clamp_page(l: &Limits, max_events: Option<u32>, max_bytes: Option<u32>) -> Result<(u32, u32), ToolError>;

// scope.rs
pub struct Scope { pub root: WorkerId, pub worker: WorkerId, pub generation: ProcessGeneration, pub run: Option<RunId> }
pub struct ScopedEnvelope { pub scope: Scope, pub envelope: Envelope }
impl Scope {
    /// The concrete P2 key (no generic K): StreamKey { worker: self.worker.as_str().to_owned(),
    /// generation: self.generation.0, item: item.to_owned(), stream: stream.as_snake_case().to_owned() }.
    pub fn stream_key(&self, item: &str, stream: StreamKind) -> crate::redact::stream::StreamKey;
}
```

## Additive changes to existing types

| File | Change |
| --- | --- |
| `event.rs` `WorkerState` | add `Stopping`, before `Unknown`. Older builds read it as `Unknown` |
| `event.rs` `WorkerSnapshot` | add `#[serde(default, skip_serializing_if = "Option::is_none")] pub delegation: Option<WorkerDelegation>` |
| `event.rs` new struct | `WorkerDelegation { worker_id: WorkerId, origin: WorkerOrigin, driver: Driver, profile_id: Option<ProfileId>, parent: Option<WorkerId>, run: Option<RunId>, depth: u8, model: ValueProvenance, effort: ValueProvenance, controls: Controls, status: DelegationStatus }`, plus `enum WorkerOrigin { Root, Native, Supervised }`. All `Eq` |
| `event.rs` `Event` | add `DelegatedMessage { message_id, direction: Direction, kind: MessageKind, from: WorkerId, to: WorkerId, reply_to: Option<MessageId>, text: String, delivery: DeliveryState }`, which is never a `UserMessage` item; and `WorkerLinked { worker_id: WorkerId, operation_id: OperationId }`, where `Envelope.item` is the native call card id (emitted only for an attested call) |
| `adapter.rs` `OpenSession` | add `#[serde(skip)] pub delegation: Option<DelegationBinding>`. The app sets it per spawn; it is never serialised |
| `caps.rs` | **no change** (see `route.rs` note) |
| `redact/mod.rs` | add `"atdc_"` to `TOKEN_PREFIXES` |

Store tables (with the contract §8 columns), the Codex/Claude adapter integration, the supervisor, L2's file I/O and the
config `ProfileId` migration are not P1 work. They belong to P3, P5, P4, P9 and the coordinator respectively. The app's
`hook_config::CONFIG_REDIRECT_VARS` should re-export `agy_guard::CONFIG_REDIRECT_VARS` once both land (coordinator).

## Tests (all in agent-core, no display)

1. Ids:
   - `parse` accepts the documented shapes and rejects an empty value, a too-long value, a wrong prefix and bad
     characters.
   - Serde is transparent.
2. Token:
   - `from_random` gives the `atdc_` prefix and 48 characters, and `parse` round-trips it.
   - `format!("{:?}")` never contains the secret.
   - `ct_eq` is true and false correctly, and handles a length mismatch.
   - `redact(text_with_token)` masks it.
   - `StreamSanitiser` masks it across every split position.
3. `ToolName::ALL` lists every variant once, through an exhaustive `next` chain (same pattern as `caps.rs`).
   `as_str` and `parse` round-trip.
4. Each tool's `input_schema` and `output_schema`:
   - is valid JSON with `additionalProperties:false` on inputs;
   - the contract's example requests parse;
   - every bound is rejected at bound+1 with `InvalidArgument` and `details.field`;
   - unknown fields are rejected;
   - every mutating output schema contains the `MutationReceipt` fields, including `warnings`, and read's `ack` does too.
5. `ErrorCode::ALL` is exhaustive (26 = 25 + `Unknown`, `InternalError` not retryable) and serialises in snake_case; an unknown code decodes as
   `Unknown`; the `retryable` table matches the contract.
6. IPC:
   - Frames round-trip.
   - `FrameDecoder` handles a byte-at-a-time split, several frames in one read (it takes only the first and reports
     `wants_bytes() == 0` until `next_frame`), and a length of 0.
   - A length of `MAX_FRAME+1` errors **before** any buffer grows (check capacity), and the decoder is poisoned
     afterwards. Buffered bytes never exceed `4 + MAX_FRAME`.
   - Invalid UTF-8 and invalid JSON are errors.
   - `Hello`'s Debug hides the token.
   - `negotiate` returns `{1,1}`→1 and `{2,3}`→`ProtocolIncompatible`.
   - `ConnState`: a frame before hello, a second hello, a duplicate pending call id and an unexpected frame are each a
     `ProtocolViolation`; a 5th concurrent call gives `RejectCapacity`; an id is reusable after its result was sent.
7. Authority:
   - The ceiling table covers every `Mode`, including Bypass→Ask.
   - `permits` covers every `Mode`×`TaskMode` pair.
   - `can_spawn` is false at depth 2.
   - `workspace_allowed` refuses a writer on a shared tree.
   - `delivery_allowed`: a Plan sender to an `accept_edits` child is refused for **both** `instruction` and `reply`;
     every direction×kind pair is covered.
   - `InheritedAuthority::child` never widens the mode, and `narrow` never widens it either (property loop over all
     modes).
   - Lineage: `root.child(a)` has parent = root, depth 1; `a.child(b)` has parent = a, root = root, depth 2;
     `b.child(c)` is `AuthorityDenied`.
   - `revocation`: each table row of contract §6. Root restriction keeps the root token, kills only out-of-ceiling
     descendants, and marks them restricted. Widening does not bump. Close kills exactly the subtree.
8. Identity, the `observe` table per driver:
   - Claude: reported per turn and can change between turns; effort stays `LaunchConfigured` after success.
   - Codex: both values become `ProviderAccepted` with `effective == None` after success, and a failure with an
     unsupported effort maps accordingly.
   - agy: the model becomes `ProviderAccepted` with `effective == None`, and the effort stays configured.
   - No `Launched` with `None` values gives `Unknown`.
9. Lifecycle:
   - Every legal transition is covered, and a selection of illegal ones are refused.
   - `CancelRequested` with a stale bound run gives `StaleRun`.
   - `Closed` is absorbing.
   - `RestartReconciled` never yields `Running`.
   - `Stopping` reaches `Stopped` only on an observed exit; an interrupted turn with the process alive gives `Waiting`.
   - `cancel_complete` is false while any bound run is non-terminal.
   - `cancel_bound_set`: an idle target with two active descendants gives both descendants (dispatched); no target
     run and no descendants gives an empty set (`idle`).
10. Operations:
    - Only the legal transitions in `operation_step` are accepted.
    - `on_restart` maps reserved and submitted operations to `Unknown`.
    - `idempotency` gives `New`, `Replay`, `Conflict` and `StaleEpoch` (stored under an older epoch, request with no
      epoch), and a stale expected epoch is refused before lookup. When both epoch and request differ, the result is
      `StaleEpoch`.
    - `canonical_request` is independent of key order, excludes the key and the epoch, equals itself for an omitted
      default and an explicit default, and keeps `"parent"` and `"HEAD"` literally.
    - `replay`: every row of the contract §2 table, including `Unknown` → `DeliveryUnknown` carrying the original
      receipt, and `DispatchReserved` while dispatching → `WaitForDispatch`.
11. Mailbox:
    - `plan_delivery` covers live versus queued delivery, `this_run` against a missing or stale run, a closed target,
      agy `awaiting_result` giving `QueueNextTurn`, Codex `ThisRun` → `SteerNow`, Claude `ThisRun` →
      `WriteLive{Some(run)}`, Claude `Auto` → `WriteLive{None}`, and agy `ThisRun` → `UnsupportedDelivery`.
    - `DeliveryState::ALL` has 9 entries (exhaustive `next` chain); `misdelivered` round-trips and is terminal;
      `delivery_step` allows `Submitted → Misdelivered` and refuses any move out of a terminal state.
    - `on_steer_refused` is covered.
    - `EchoQueue`: an in-order echo in the target run gives `Confirmed`; in a later run `Misdelivered { target, actual }`;
      with no target (`auto`) always `Confirmed`; an out-of-order or altered text gives `Mismatch`.
    - A `SendReceipt` built at acceptance never changes: replaying it after `Misdelivered` returns the same
      `target_run_id` and `delivery`, with only `replayed` and `dispatch` refreshed.
    - `ack_eligible`: not the recipient, not presented, already acknowledged, and eligible.
    - The continuation budget allows one reservation at a time, at most 3, and a release does not consume budget.
12. Pages:
    - A cursor round-trips; another filter gives `InvalidArgument`; another stream epoch gives `CursorExpired`;
      `workers` order does not change the digest.
    - `assemble`: the snapshot is always in and counts first; units are whole; a message seq is one unit (never also an
      event), and `max_units` counts it once; a single oversized unit is still returned; `truncated` is set when units
      remain; a boundary where the next unit is a message is not skipped by `next_cursor`.
    - `assemble_list`: stops at the first worker that would exceed `OUTPUT_BUDGET`, keeps at least one, never skips;
      `fit_summary` brings a worst-case summary under `MAX_SUMMARY_BYTES`.
    - **Worst-case output regression (contract §3 "Output budget"):**
      - Build the maximal capabilities catalogue: 32 profiles, 64 models each with 8 efforts, every string at its
        byte cap. Run it once with ASCII and once with characters that escape six-fold (`\u0001`).
      - Assert that `transport_cost` exceeds `OUTPUT_BUDGET` before truncation.
      - After `truncate_capabilities`, assert:
        - `transport_cost` is at most `OUTPUT_BUDGET`, and the real MCP serialisation (`CallToolResult` with
          `structuredContent` and its text block) is under 1 MiB;
        - `truncated` is true;
        - `omitted.models` equals the models actually removed, and `omitted.profiles` the profiles removed;
        - the surviving profiles are a prefix, in order, with all identity fields intact;
        - models were removed from the end before any profile.
      - With `profile_id`, a single maximal profile is truncated the same way.
      - A maximal page (48 KiB snapshot, then 16 KiB control-character messages) and a maximal list (100 worst-case
        summaries) also stay within `OUTPUT_BUDGET`, with `truncated` set and no `seq` skipped.
      - The maximal spawn receipt (8 warnings with six-fold escaping, maximal identity and path) is at most
        `MAX_RECEIPT_BYTES`. Replaying it as `delivery_unknown` puts `{operation_id, receipt}` in `details`, which is at
        most `MAX_ERROR_DETAILS_BYTES`. The whole error output fits `OUTPUT_BUDGET`, and the receipt round-trips byte for
        byte.
      - `reserve` and `settle` with the maximal envelope (64 KiB `ack`, 48 KiB snapshot, 256 B cursor) and a 100 KiB
        first unit keep that unit. The measured page is at most `OUTPUT_BUDGET`, for ASCII, six-fold-escaped and
        non-ASCII text.
      - **Conservative-stop test.** Choose units so that the reservation stops early: it takes fewer units than would
        actually fit, because the real cursor is short and the flags are absent. Assert that the page still advances (at
        least one unit, and `next_cursor` moves), and that the measured size never exceeds `OUTPUT_BUDGET`.
      - **Correction test.** Feed reservations that underestimate on purpose, so that `settle` must drop units. Assert
        that it drops only from the end, keeps the first unit, sets `truncated` and ends within the budget.
      - A one-unit page that is still over the budget (a forced bug) gives `InternalError`.
      - `request_id_ok`: 256 B encoded passes, 257 B fails, for a string id, an escaped-character id and a number id.
      - `guard_output` on an oversized value gives `InternalError` carrying the operation id. The stored operation is
        unchanged, and a same-key `replay` still returns its receipt.
13. Correlation: `invocation_from_meta` is tested on the recorded `_meta` from the three providers' fixtures (copy the
    scrubbed objects into the test):
    - Codex `x-codex-turn-metadata` is ignored.
    - A malformed agy `progressToken` gives `None`.
    - Over-long values give `None`.
    - `ObservedCalls`: a key attests once; another tool, other arguments, a missing key and an evicted entry are
      `Unattested`.
    - `attestation_arguments`: the same call from the stream (object or JSON string, defaults omitted, other key order)
      and from the bridge (defaults explicit) gives equal args; a changed or added `idempotency_key`, a removed or
      changed `expected_authority_epoch`, and two secrets that `redact` scrubs identically each give different args;
      its Debug never prints the content.
    - `attestation_required(Agy, t, _)` is true for all 8 tools, `read`/`list`/`wait`/`capabilities` included.
14. Route and caps:
    - The `DelegationCaps` exhaustive-destructure guard.
    - `admit_route` covers every parent × child × `TaskMode` × `ChildBridge`, and all are `Unverified` in P1.
    - Blockers equal the contract §11 groups: Claude child `plan` excludes `ClaudeBashUnderRestricted`, `ask` and
      `accept_edits` include it; Claude→Codex includes `CodexAmbientMcp` and `CodexNativeDelegation`; an agy child
      `WithBridge` includes `AgyAttestation`, `AgyPeerAndSingleUse`, `AgyWorkspaceMcpCheck` and `AgyTokenPersistence`,
      and `WithoutBridge` includes none of them but does include `AgyHookPayloadUnrecorded`.
    - `agy_child_variant` falls back to `WithoutBridge` when L2 fails and that variant is admitted, else `None`.
    - `DelegationCaps::of`: Codex `Strict`, Claude `BestEffort`, agy `Unsupported`; agy has `attestation_required` and
      `TokenReuse::SingleUse`.
    - `DelegationStatusCode::ALL` has 6; only `RebindRequired` has `auto_rebind`.
15. Launch:
    - `claude_child_args(Plan)` contains no Edit, Write or Bash and does contain `--restricted` and
      `--strict-mcp-config`.
    - `codex_thread_config` has exactly the 5 documented keys, with `default_tools_approval_mode == "approve"`, and the
      token appears only under `env`. `codex_child_config(&["x".into(), "codex_apps".into()])` adds exactly the two
      `enabled=false` keys.
    - `claude_overlay` puts the token only under `env`.
    - `DelegationBinding` Debug is redacted.
    - `OpenSession` serialisation never contains `delegation` (serialise one that has a binding set and assert the
      token prefix is absent).
    - `claude_child_init_ok` refuses an init list containing `Task` or `Agent`, an extra MCP server, or a missing
      tool; `codex_child_servers_ok` refuses `codex_apps`.
    - `overlay_file_name` and `overlay_sweep_candidate` round-trip, and a foreign name gives `None`.
16. agy guard:
    - `argv_redirect` matches the PR #44 table (one or two dashes, `-`/`_` alike, inline or next argument, after
      `--`), and passes `--gemini_dirt` and `--sandbox=gemini_dir`.
    - `env_redirect` reports each `CONFIG_REDIRECT_VARS` name, even when blank, and any `CASCADE_*`, by name only.
    - `l2_directories`: a git case stops at the toplevel; a non-git case reaches `/`.
    - `allow_rules_missing` lists absent rules and refuses unparsable JSON.
    - `stat_ppid` parses `1234 (a) b) c) S 42 …` as 42, and refuses a line with no `)`.
    - `WORKSPACE_SOURCE_NAMES` contains `sidecars.json` and `sidecars`.
    - `is_bridge_cmdline`: `agent-terminal\0--cross-agent-mcp\0` and `agent-terminal\0--cross-agent-mcp` (no trailing
      NUL) are both bridges; `agent-terminal\0--approval-hook\0`,
      an extra argument and an empty cmdline are not.
    - `census_ok`: one bridge equal to the peer passes; zero, two, or one that is not the peer fails; approval-hook
      processes never enter the list.
    - `hello_verdict`: a peer that is not agy's direct child or has a non-bridge argv → `RefuseNoConsume` (token stays
      unconsumed); same pid after a closed connection → `RefuseLostIpc`; a different direct-child bridge → `Compromise`.
    - `lineage_ok`: grandparent equal to the app pid passes; a nested parent (sub-agent) fails.
17. Limits: every `admit_*` boundary at the limit and at limit+1 (including pre-auth connections), `clamp_page`
    defaults and maximums, and `RECONNECT_DEADLINE_MS` is separate from `WELCOME_DEADLINE_MS`.
18. Events:
    - `WorkerState::Stopping` round-trips.
    - A stored v3 `WorkerSnapshot` without `delegation` decodes.
    - `DelegatedMessage` and `WorkerLinked` round-trip, and an older-shaped decoder sees them as `Event::Unknown`
      (already covered by `#[serde(other)]`; assert it).
    - `StateRef` serialises as `{"domain":"run","value":"stopping"}`.
    - `ThisRunSupport` serialises as `"strict"`, `"best_effort"`, `"unsupported"`; `Controls.this_run` uses it.
    - `env(app_pid)` returns 4 pairs and `STRIP_ON_SPAWN` has 5 names, `ENV_APP_PID` included.
19. Scope: `stream_key` builds the concrete `StreamKey` with the four documented fields, and two scopes that differ only
    in generation give unequal keys.

Gate: `cargo test -p agent-core`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all --check`.
P1 adds no dependencies. If `base64url` encoding is needed, write it by hand in about 20 lines rather than add a crate.
