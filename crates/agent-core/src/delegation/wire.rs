//! The tool wire format (contract §2 and §3): request and response structs, the mutation
//! receipt envelope, and the output budget.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use super::authority::{workspace_allowed, TaskMode};
use super::error::{truncate_bytes, ErrorCode, ToolError};
use super::identity::Identity;
use super::ids::{
    Cursor, IdError, IdempotencyKey, MessageId, OperationId, ProfileId, RunId, WorkerId,
};
use super::ipc::Principal;
use super::lifecycle::RunState;
use super::limits::{
    Limits, DEFAULT_LIST_LIMIT, DEFAULT_PAGE_BYTES, DEFAULT_PAGE_EVENTS, DEFAULT_WAIT_TIMEOUT_MS,
};
use super::mailbox::{kind_allowed, DeliveryState, Direction, MessageKind};
use super::route::{ChildBridge, RouteStatus};
use super::tools::ToolName;
use crate::adapter::Driver;
use crate::event::{WorkerOrigin, WorkerState};

/// Contract §3 "Output budget": `structuredContent` plus the same JSON as an escaped text
/// block, measured on the real serialisation.
pub const OUTPUT_BUDGET: usize = 786_432;
/// A serialised `WorkerSummary` (contract §2 output bounds).
pub const MAX_SUMMARY_BYTES: usize = 48 * 1024;
/// Holds `delivery_unknown.details.receipt` of a maximal receipt.
pub const MAX_ERROR_DETAILS_BYTES: usize = 64 * 1024;
/// Any `MutationReceipt`-based output, worst-case escaping.
pub const MAX_RECEIPT_BYTES: usize = 48 * 1024;
/// Read's `ack` object.
pub const MAX_ACK_BYTES: usize = 64 * 1024;
/// An encoded JSON-RPC id (the bridge checks it before dispatch).
pub const MAX_REQUEST_ID_BYTES: usize = 256;

/// The workspace a spawn asks for (contract §2 `Workspace`), tagged by `kind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkspaceRequest {
    SharedReadOnly,
    IsolatedWorktree {
        /// A commit-ish; `HEAD` of the caller's tree by default. Resolved to a commit at
        /// acceptance and stored with the operation; the request keeps the syntax.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
    },
}

impl WorkspaceRequest {
    pub fn kind(&self) -> WorkspaceKind {
        match self {
            WorkspaceRequest::SharedReadOnly => WorkspaceKind::SharedReadOnly,
            WorkspaceRequest::IsolatedWorktree { .. } => WorkspaceKind::IsolatedWorktree,
        }
    }
}

/// The documented default of `workspace.base`.
pub const DEFAULT_WORKTREE_BASE: &str = "HEAD";

/// A workspace kind, as reported in outputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceKind {
    SharedReadOnly,
    IsolatedWorktree,
}

/// When a send is delivered (contract §3 `cross_agent_send`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Auto,
    ThisRun,
    NextTurn,
}

/// An operation's dispatch state (contract §2 `DispatchState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchState {
    Accepted,
    DispatchReserved,
    Submitted,
    Confirmed,
    Failed,
    #[serde(other)]
    Unknown,
}

impl DispatchState {
    pub const ALL: [DispatchState; 6] = [
        DispatchState::Accepted,
        DispatchState::DispatchReserved,
        DispatchState::Submitted,
        DispatchState::Confirmed,
        DispatchState::Failed,
        DispatchState::Unknown,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            DispatchState::Accepted => "accepted",
            DispatchState::DispatchReserved => "dispatch_reserved",
            DispatchState::Submitted => "submitted",
            DispatchState::Confirmed => "confirmed",
            DispatchState::Failed => "failed",
            DispatchState::Unknown => "unknown",
        }
    }
}

/// How a provider honours `delivery: this_run` (contract §2 `ThisRunSupport`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThisRunSupport {
    /// The provider fences the run (Codex `turn/steer` `expectedTurnId`).
    Strict,
    /// Written while the run is live; the record says `misdelivered` if it lands later (Claude).
    BestEffort,
    /// agy.
    Unsupported,
}

/// What a caller may do to a worker (contract §2 `Controls`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Controls {
    pub send: bool,
    pub this_run: ThisRunSupport,
    pub cancel: bool,
    pub close: bool,
}

/// A state of either domain (contract §2 `StateRef`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "domain", content = "value", rename_all = "snake_case")]
pub enum StateRef {
    Worker(WorkerState),
    Run(RunState),
}

/// Most warnings in one receipt (contract §2).
pub const MAX_WARNINGS: usize = 8;
/// Longest warning code and message, in bytes (contract §2).
pub const MAX_WARNING_CODE_BYTES: usize = 64;
pub const MAX_WARNING_MESSAGE_BYTES: usize = 512;

/// A non-fatal note on a receipt (contract §2 `Warning`). `message` is scrubbed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Warning {
    pub code: String,
    pub message: String,
}

impl Warning {
    /// Bounded to the contract's field limits.
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            code: super::error::truncate_bytes(code, MAX_WARNING_CODE_BYTES).to_owned(),
            message: super::error::truncate_bytes(message, MAX_WARNING_MESSAGE_BYTES).to_owned(),
        }
    }
}

/// The envelope every mutating tool's output starts with (contract §2 `MutationReceipt`),
/// flattened into the tool's output object. Frozen at acceptance: a replay refreshes only
/// `replayed` and `dispatch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationReceipt {
    pub api_version: u32,
    pub operation_id: OperationId,
    /// The epoch the operation was accepted under.
    pub authority_epoch: u64,
    pub replayed: bool,
    pub dispatch: DispatchState,
    #[serde(default)]
    pub warnings: Vec<Warning>,
}

/// A worker's delegation status (contract §6), the same 6 codes in the store, on the wire and
/// here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationStatusCode {
    Available,
    WorkspaceUntrusted,
    RebindRequired,
    Restricted,
    BridgeDisabled,
    Unavailable,
}

impl DelegationStatusCode {
    pub const ALL: [DelegationStatusCode; 6] = [
        DelegationStatusCode::Available,
        DelegationStatusCode::WorkspaceUntrusted,
        DelegationStatusCode::RebindRequired,
        DelegationStatusCode::Restricted,
        DelegationStatusCode::BridgeDisabled,
        DelegationStatusCode::Unavailable,
    ];

    /// Only `rebind_required` is left by an automatic rebind (contract §6).
    pub fn auto_rebind(self) -> bool {
        match self {
            DelegationStatusCode::RebindRequired => true,
            DelegationStatusCode::Available
            | DelegationStatusCode::WorkspaceUntrusted
            | DelegationStatusCode::Restricted
            | DelegationStatusCode::BridgeDisabled
            | DelegationStatusCode::Unavailable => false,
        }
    }
}

/// Longest `DelegationStatus.reason`, in bytes.
pub const MAX_STATUS_REASON_BYTES: usize = 256;
/// Most `DelegationStatus.paths`, and the longest one, in bytes.
pub const MAX_STATUS_PATHS: usize = 16;
pub const MAX_STATUS_PATH_BYTES: usize = 256;

/// `WorkerSummary.delegation` (contract §2 `DelegationStatus`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationStatus {
    pub code: DelegationStatusCode,
    /// Scrubbed, at most [`MAX_STATUS_REASON_BYTES`].
    #[serde(default)]
    pub reason: Option<String>,
    /// L2 offenders only: at most [`MAX_STATUS_PATHS`] relative paths of at most
    /// [`MAX_STATUS_PATH_BYTES`] each.
    #[serde(default)]
    pub paths: Vec<String>,
}

impl DelegationStatus {
    pub fn new(code: DelegationStatusCode) -> Self {
        Self {
            code,
            reason: None,
            paths: Vec::new(),
        }
    }

    /// Cuts `reason` and `paths` to their bounds.
    pub fn fit(&mut self) {
        if let Some(r) = &mut self.reason {
            let cut = super::error::truncate_bytes(r, MAX_STATUS_REASON_BYTES).len();
            r.truncate(cut);
        }
        self.paths.truncate(MAX_STATUS_PATHS);
        for p in &mut self.paths {
            let cut = super::error::truncate_bytes(p, MAX_STATUS_PATH_BYTES).len();
            p.truncate(cut);
        }
    }
}

/// One `task_mode`'s route admission in `capabilities.profiles[].modes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeAdmission {
    pub task_mode: TaskMode,
    pub route_status: RouteStatus,
    pub route_blockers: Vec<String>,
    /// What the route admits but cannot supervise (`route::RouteCaveat` names). Absent when
    /// empty, so a reader of the earlier shape sees nothing new.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub route_caveats: Vec<String>,
    pub child_bridge: ChildBridge,
}

/// `"parent"` or a worker handle (contract §3 `cross_agent_send.target`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    Parent,
    Worker(WorkerId),
}

impl Serialize for Target {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Target::Parent => s.serialize_str("parent"),
            Target::Worker(w) => s.serialize_str(w.as_str()),
        }
    }
}

impl<'de> Deserialize<'de> for Target {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Target::parse(&s).ok_or_else(|| serde::de::Error::custom("target is neither parent nor w_"))
    }
}

impl Target {
    pub fn parse(s: &str) -> Option<Self> {
        if s == "parent" {
            return Some(Target::Parent);
        }
        WorkerId::parse(s).ok().map(Target::Worker)
    }
}

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

/// `cross_agent_capabilities` input.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<ProfileId>,
}

/// `cross_agent_spawn` input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnRequest {
    pub profile_id: ProfileId,
    pub name: String,
    pub task: String,
    pub task_mode: TaskMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub workspace: WorkspaceRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_authority_epoch: Option<u64>,
    pub idempotency_key: IdempotencyKey,
}

/// `cross_agent_send` input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendRequest {
    pub target: Target,
    pub text: String,
    pub kind: MessageKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<MessageId>,
    pub delivery: Delivery,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_authority_epoch: Option<u64>,
    pub idempotency_key: IdempotencyKey,
}

/// `cross_agent_read` input.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<WorkerId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Cursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_events: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledge: Option<Vec<MessageId>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<IdempotencyKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_authority_epoch: Option<u64>,
}

/// `cross_agent_wait` input.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workers: Option<Vec<WorkerId>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mailbox: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Cursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_events: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u32>,
}

/// `cross_agent_list` input.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<WorkerId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// `cross_agent_cancel` input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelRequest {
    pub worker_id: WorkerId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_authority_epoch: Option<u64>,
    pub idempotency_key: IdempotencyKey,
}

/// `cross_agent_close` input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloseRequest {
    pub worker_id: WorkerId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_authority_epoch: Option<u64>,
    pub idempotency_key: IdempotencyKey,
}

/// A validated tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolRequest {
    Capabilities(CapabilitiesRequest),
    Spawn(SpawnRequest),
    Send(SendRequest),
    Read(ReadRequest),
    Wait(WaitRequest),
    List(ListRequest),
    Cancel(CancelRequest),
    Close(CloseRequest),
}

/// Reads one tool's argument object field by field, so every refusal names its field.
struct Args<'a> {
    map: &'a Map<String, Value>,
}

impl<'a> Args<'a> {
    fn only(&self, allowed: &[&str]) -> Result<(), ToolError> {
        match self.map.keys().find(|k| !allowed.contains(&k.as_str())) {
            Some(k) => Err(ToolError::invalid(truncate_bytes(k, 64), "unknown_field")),
            None => Ok(()),
        }
    }

    fn get(&self, field: &str) -> Result<Option<&'a Value>, ToolError> {
        match self.map.get(field) {
            None => Ok(None),
            Some(Value::Null) => Err(ToolError::invalid(field, "null")),
            Some(v) => Ok(Some(v)),
        }
    }

    fn required(&self, field: &str) -> Result<&'a Value, ToolError> {
        self.get(field)?
            .ok_or_else(|| ToolError::invalid(field, "required"))
    }

    fn opt_str(&self, field: &str) -> Result<Option<&'a str>, ToolError> {
        match self.get(field)? {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(_) => Err(ToolError::invalid(field, "not_a_string")),
        }
    }

    fn req_str(&self, field: &str) -> Result<&'a str, ToolError> {
        self.opt_str(field)?
            .ok_or_else(|| ToolError::invalid(field, "required"))
    }

    fn opt_u64(&self, field: &str) -> Result<Option<u64>, ToolError> {
        match self.get(field)? {
            None => Ok(None),
            Some(v) => v
                .as_u64()
                .map(Some)
                .ok_or_else(|| ToolError::invalid(field, "not_a_non_negative_integer")),
        }
    }

    fn opt_u32(&self, field: &str) -> Result<Option<u32>, ToolError> {
        match self.opt_u64(field)? {
            None => Ok(None),
            Some(n) => u32::try_from(n)
                .map(Some)
                .map_err(|_| ToolError::invalid(field, "out_of_range")),
        }
    }

    fn opt_bool(&self, field: &str) -> Result<Option<bool>, ToolError> {
        match self.get(field)? {
            None => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(ToolError::invalid(field, "not_a_boolean")),
        }
    }

    fn opt_parsed<T>(
        &self,
        field: &str,
        parse: fn(&str) -> Result<T, IdError>,
    ) -> Result<Option<T>, ToolError> {
        match self.opt_str(field)? {
            None => Ok(None),
            Some(s) => parse(s)
                .map(Some)
                .map_err(|_| ToolError::invalid(field, "invalid_format")),
        }
    }

    fn req_parsed<T>(
        &self,
        field: &str,
        parse: fn(&str) -> Result<T, IdError>,
    ) -> Result<T, ToolError> {
        self.opt_parsed(field, parse)?
            .ok_or_else(|| ToolError::invalid(field, "required"))
    }

    fn opt_list<T>(
        &self,
        field: &str,
        parse: fn(&str) -> Result<T, IdError>,
    ) -> Result<Option<Vec<T>>, ToolError> {
        match self.get(field)? {
            None => Ok(None),
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| match v {
                    Value::String(s) => {
                        parse(s).map_err(|_| ToolError::invalid(field, "invalid_format"))
                    }
                    _ => Err(ToolError::invalid(field, "not_a_string")),
                })
                .collect::<Result<Vec<T>, ToolError>>()
                .map(Some),
            Some(_) => Err(ToolError::invalid(field, "not_an_array")),
        }
    }

    fn req_enum<T: serde::de::DeserializeOwned>(&self, field: &str) -> Result<T, ToolError> {
        let v = self.required(field)?;
        if !v.is_string() {
            return Err(ToolError::invalid(field, "not_a_string"));
        }
        serde_json::from_value(v.clone()).map_err(|_| ToolError::invalid(field, "invalid_value"))
    }
}

fn parse_workspace(v: &Value) -> Result<WorkspaceRequest, ToolError> {
    let map = v
        .as_object()
        .ok_or_else(|| ToolError::invalid("workspace", "not_an_object"))?;
    if let Some(k) = map.keys().find(|k| !matches!(k.as_str(), "kind" | "base")) {
        return Err(ToolError::invalid(
            &format!("workspace.{}", truncate_bytes(k, 64)),
            "unknown_field",
        ));
    }
    let kind = match map.get("kind") {
        Some(Value::String(s)) => s.as_str(),
        Some(_) => return Err(ToolError::invalid("workspace.kind", "not_a_string")),
        None => return Err(ToolError::invalid("workspace.kind", "required")),
    };
    match kind {
        "shared_read_only" => {
            if map.contains_key("base") {
                return Err(ToolError::invalid("workspace.base", "unknown_field"));
            }
            Ok(WorkspaceRequest::SharedReadOnly)
        }
        "isolated_worktree" => {
            let base = match map.get("base") {
                None => None,
                Some(Value::String(s)) => Some(s.clone()),
                Some(_) => return Err(ToolError::invalid("workspace.base", "not_a_string")),
            };
            Ok(WorkspaceRequest::IsolatedWorktree { base })
        }
        _ => Err(ToolError::invalid("workspace.kind", "invalid_value")),
    }
}

/// Parses one tool's arguments: shape, types and formats here, then
/// [`ToolRequest::validate`] for bounds and cross-field rules. Every refusal is
/// `invalid_argument` with `details.field` (or the code `validate` names).
pub fn parse_request(
    tool: ToolName,
    args: &Value,
    limits: &Limits,
) -> Result<ToolRequest, ToolError> {
    let map = args
        .as_object()
        .ok_or_else(|| ToolError::invalid("arguments", "not_an_object"))?;
    let a = Args { map };
    let req = match tool {
        ToolName::CrossAgentCapabilities => {
            a.only(&["profile_id"])?;
            ToolRequest::Capabilities(CapabilitiesRequest {
                profile_id: a.opt_parsed("profile_id", ProfileId::parse)?,
            })
        }
        ToolName::CrossAgentSpawn => {
            a.only(&[
                "profile_id",
                "name",
                "task",
                "task_mode",
                "model",
                "effort",
                "workspace",
                "expected_authority_epoch",
                "idempotency_key",
            ])?;
            ToolRequest::Spawn(SpawnRequest {
                profile_id: a.req_parsed("profile_id", ProfileId::parse)?,
                name: a.req_str("name")?.to_owned(),
                task: a.req_str("task")?.to_owned(),
                task_mode: a.req_enum("task_mode")?,
                model: a.opt_str("model")?.map(str::to_owned),
                effort: a.opt_str("effort")?.map(str::to_owned),
                workspace: parse_workspace(a.required("workspace")?)?,
                expected_authority_epoch: a.opt_u64("expected_authority_epoch")?,
                idempotency_key: a.req_parsed("idempotency_key", IdempotencyKey::parse)?,
            })
        }
        ToolName::CrossAgentSend => {
            a.only(&[
                "target",
                "text",
                "kind",
                "reply_to",
                "delivery",
                "expected_run_id",
                "expected_authority_epoch",
                "idempotency_key",
            ])?;
            let target = Target::parse(a.req_str("target")?)
                .ok_or_else(|| ToolError::invalid("target", "invalid_format"))?;
            ToolRequest::Send(SendRequest {
                target,
                text: a.req_str("text")?.to_owned(),
                kind: a.req_enum("kind")?,
                reply_to: a.opt_parsed("reply_to", MessageId::parse)?,
                delivery: a.req_enum("delivery")?,
                expected_run_id: a.opt_parsed("expected_run_id", RunId::parse)?,
                expected_authority_epoch: a.opt_u64("expected_authority_epoch")?,
                idempotency_key: a.req_parsed("idempotency_key", IdempotencyKey::parse)?,
            })
        }
        ToolName::CrossAgentRead => {
            a.only(&[
                "target",
                "after",
                "max_events",
                "max_bytes",
                "acknowledge",
                "idempotency_key",
                "expected_authority_epoch",
            ])?;
            ToolRequest::Read(ReadRequest {
                target: a.opt_parsed("target", WorkerId::parse)?,
                after: a.opt_parsed("after", Cursor::parse)?,
                max_events: a.opt_u32("max_events")?,
                max_bytes: a.opt_u32("max_bytes")?,
                acknowledge: a.opt_list("acknowledge", MessageId::parse)?,
                idempotency_key: a.opt_parsed("idempotency_key", IdempotencyKey::parse)?,
                expected_authority_epoch: a.opt_u64("expected_authority_epoch")?,
            })
        }
        ToolName::CrossAgentWait => {
            a.only(&[
                "workers",
                "mailbox",
                "after",
                "timeout_ms",
                "max_events",
                "max_bytes",
            ])?;
            ToolRequest::Wait(WaitRequest {
                workers: a.opt_list("workers", WorkerId::parse)?,
                mailbox: a.opt_bool("mailbox")?,
                after: a.opt_parsed("after", Cursor::parse)?,
                timeout_ms: a.opt_u32("timeout_ms")?,
                max_events: a.opt_u32("max_events")?,
                max_bytes: a.opt_u32("max_bytes")?,
            })
        }
        ToolName::CrossAgentList => {
            a.only(&["target", "cursor", "limit"])?;
            ToolRequest::List(ListRequest {
                target: a.opt_parsed("target", WorkerId::parse)?,
                cursor: a.opt_parsed("cursor", Cursor::parse)?,
                limit: a.opt_u32("limit")?,
            })
        }
        ToolName::CrossAgentCancel => {
            a.only(&[
                "worker_id",
                "expected_run_id",
                "expected_authority_epoch",
                "idempotency_key",
            ])?;
            ToolRequest::Cancel(CancelRequest {
                worker_id: a.req_parsed("worker_id", WorkerId::parse)?,
                expected_run_id: a.opt_parsed("expected_run_id", RunId::parse)?,
                expected_authority_epoch: a.opt_u64("expected_authority_epoch")?,
                idempotency_key: a.req_parsed("idempotency_key", IdempotencyKey::parse)?,
            })
        }
        ToolName::CrossAgentClose => {
            a.only(&["worker_id", "expected_authority_epoch", "idempotency_key"])?;
            ToolRequest::Close(CloseRequest {
                worker_id: a.req_parsed("worker_id", WorkerId::parse)?,
                expected_authority_epoch: a.opt_u64("expected_authority_epoch")?,
                idempotency_key: a.req_parsed("idempotency_key", IdempotencyKey::parse)?,
            })
        }
    };
    req.validate(limits)?;
    Ok(req)
}

fn check_bytes(field: &str, s: &str, min: usize, max: u32) -> Result<(), ToolError> {
    if s.len() < min {
        return Err(ToolError::invalid(field, "too_short"));
    }
    if s.len() > max as usize {
        return Err(ToolError::invalid(field, "too_long"));
    }
    Ok(())
}

fn check_range(field: &str, v: Option<u32>, min: u32, max: u32) -> Result<(), ToolError> {
    match v {
        Some(n) if n < min || n > max => Err(ToolError::invalid(field, "out_of_range")),
        _ => Ok(()),
    }
}

fn check_count<T>(field: &str, v: Option<&Vec<T>>, max: u32) -> Result<(), ToolError> {
    match v {
        Some(items) if items.len() > max as usize => {
            Err(ToolError::invalid(field, "too_many_items"))
        }
        _ => Ok(()),
    }
}

/// A commit-ish: no leading `-` (never an option to git), no whitespace or control characters.
fn check_base(base: &str, limits: &Limits) -> Result<(), ToolError> {
    check_bytes("workspace.base", base, 1, limits.max_base_bytes)?;
    if base.starts_with('-') || base.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(ToolError::invalid("workspace.base", "invalid_format"));
    }
    Ok(())
}

impl ToolRequest {
    pub fn tool(&self) -> ToolName {
        match self {
            ToolRequest::Capabilities(_) => ToolName::CrossAgentCapabilities,
            ToolRequest::Spawn(_) => ToolName::CrossAgentSpawn,
            ToolRequest::Send(_) => ToolName::CrossAgentSend,
            ToolRequest::Read(_) => ToolName::CrossAgentRead,
            ToolRequest::Wait(_) => ToolName::CrossAgentWait,
            ToolRequest::List(_) => ToolName::CrossAgentList,
            ToolRequest::Cancel(_) => ToolName::CrossAgentCancel,
            ToolRequest::Close(_) => ToolName::CrossAgentClose,
        }
    }

    /// Bounds (contract §3) and cross-field rules: a spawn's workspace for its mode, a send's
    /// kind for its direction, read's `idempotency_key` iff `acknowledge`.
    pub fn validate(&self, limits: &Limits) -> Result<(), ToolError> {
        match self {
            ToolRequest::Capabilities(_) => Ok(()),
            ToolRequest::Spawn(r) => {
                let chars = r.name.chars().count();
                if chars < 1 {
                    return Err(ToolError::invalid("name", "too_short"));
                }
                if chars > limits.max_name_chars as usize {
                    return Err(ToolError::invalid("name", "too_long"));
                }
                check_bytes("task", &r.task, 1, limits.max_task_bytes)?;
                if let Some(m) = &r.model {
                    check_bytes("model", m, 1, limits.max_model_bytes)?;
                }
                if let Some(e) = &r.effort {
                    check_bytes("effort", e, 1, limits.max_effort_bytes)?;
                }
                if let WorkspaceRequest::IsolatedWorktree { base: Some(b) } = &r.workspace {
                    check_base(b, limits)?;
                }
                workspace_allowed(r.task_mode, &r.workspace).map_err(|code| {
                    ToolError::new(
                        code,
                        format!(
                            "task_mode {} cannot use a {} workspace",
                            r.task_mode.as_str(),
                            match r.workspace.kind() {
                                WorkspaceKind::SharedReadOnly => "shared read-only",
                                WorkspaceKind::IsolatedWorktree => "worktree",
                            }
                        ),
                    )
                    .with_detail("field", "workspace")
                })
            }
            ToolRequest::Send(r) => {
                check_bytes("text", &r.text, 1, limits.max_message_bytes)?;
                let dir = match r.target {
                    Target::Parent => Direction::ChildToParent,
                    Target::Worker(_) => Direction::ParentToChild,
                };
                if !kind_allowed(dir, r.kind) {
                    return Err(ToolError::invalid("kind", "not_allowed_for_target"));
                }
                Ok(())
            }
            ToolRequest::Read(r) => {
                check_range("max_events", r.max_events, 1, limits.page_events_max)?;
                check_range(
                    "max_bytes",
                    r.max_bytes,
                    limits.page_bytes_min,
                    limits.page_bytes_max,
                )?;
                check_count("acknowledge", r.acknowledge.as_ref(), limits.ack_batch_max)?;
                // Present means 1..=100 entries: `idempotency_key` is required iff it is.
                if r.acknowledge.as_ref().is_some_and(Vec::is_empty) {
                    return Err(ToolError::invalid("acknowledge", "empty"));
                }
                match (r.acknowledge.is_some(), r.idempotency_key.is_some()) {
                    (true, false) => Err(ToolError::invalid(
                        "idempotency_key",
                        "required_with_acknowledge",
                    )),
                    (false, true) => Err(ToolError::invalid(
                        "idempotency_key",
                        "only_with_acknowledge",
                    )),
                    _ => Ok(()),
                }
            }
            ToolRequest::Wait(r) => {
                check_count("workers", r.workers.as_ref(), limits.wait_workers_max)?;
                check_range("timeout_ms", r.timeout_ms, 0, limits.wait_timeout_max_ms)?;
                check_range("max_events", r.max_events, 1, limits.page_events_max)?;
                check_range(
                    "max_bytes",
                    r.max_bytes,
                    limits.page_bytes_min,
                    limits.page_bytes_max,
                )
            }
            ToolRequest::List(r) => check_range("limit", r.limit, 1, limits.list_page_max),
            ToolRequest::Cancel(_) | ToolRequest::Close(_) => Ok(()),
        }
    }

    /// Mutations: spawn, send, cancel, close, and read only with a non-empty `acknowledge`.
    pub fn is_mutating(&self) -> bool {
        match self {
            ToolRequest::Spawn(_)
            | ToolRequest::Send(_)
            | ToolRequest::Cancel(_)
            | ToolRequest::Close(_) => true,
            ToolRequest::Read(r) => r.acknowledge.as_ref().is_some_and(|a| !a.is_empty()),
            ToolRequest::Capabilities(_) | ToolRequest::Wait(_) | ToolRequest::List(_) => false,
        }
    }

    /// Every omitted optional field with a documented default filled in (contract §2
    /// canonical request).
    pub fn normalised(&self) -> ToolRequest {
        let mut out = self.clone();
        match &mut out {
            ToolRequest::Spawn(r) => {
                if let WorkspaceRequest::IsolatedWorktree { base } = &mut r.workspace {
                    base.get_or_insert_with(|| DEFAULT_WORKTREE_BASE.to_owned());
                }
            }
            ToolRequest::Read(r) => {
                r.max_events.get_or_insert(DEFAULT_PAGE_EVENTS);
                r.max_bytes.get_or_insert(DEFAULT_PAGE_BYTES);
            }
            ToolRequest::Wait(r) => {
                r.mailbox.get_or_insert(true);
                r.timeout_ms.get_or_insert(DEFAULT_WAIT_TIMEOUT_MS);
                r.max_events.get_or_insert(DEFAULT_PAGE_EVENTS);
                r.max_bytes.get_or_insert(DEFAULT_PAGE_BYTES);
            }
            ToolRequest::List(r) => {
                r.limit.get_or_insert(DEFAULT_LIST_LIMIT);
            }
            ToolRequest::Capabilities(_)
            | ToolRequest::Send(_)
            | ToolRequest::Cancel(_)
            | ToolRequest::Close(_) => {}
        }
        out
    }

    /// The request's arguments as JSON, every field it holds included.
    pub fn args_value(&self) -> Value {
        let v = match self {
            ToolRequest::Capabilities(r) => serde_json::to_value(r),
            ToolRequest::Spawn(r) => serde_json::to_value(r),
            ToolRequest::Send(r) => serde_json::to_value(r),
            ToolRequest::Read(r) => serde_json::to_value(r),
            ToolRequest::Wait(r) => serde_json::to_value(r),
            ToolRequest::List(r) => serde_json::to_value(r),
            ToolRequest::Cancel(r) => serde_json::to_value(r),
            ToolRequest::Close(r) => serde_json::to_value(r),
        };
        // These structs hold only strings, numbers, booleans and string maps: serialising
        // them cannot fail. Null would only make attestation and idempotency comparisons
        // fail closed.
        v.unwrap_or(Value::Null)
    }
}

/// Sorted-key JSON of `v`, independent of the map implementation.
pub(crate) fn canonical_json(v: &Value) -> String {
    let mut out = String::new();
    write_canonical(v, &mut out);
    out
}

fn write_canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).unwrap_or_default());
                out.push(':');
                if let Some(child) = map.get(k) {
                    write_canonical(child, out);
                }
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&serde_json::to_string(scalar).unwrap_or_default()),
    }
}

// ---------------------------------------------------------------------------------------------
// Outputs
// ---------------------------------------------------------------------------------------------

/// A model a profile offers (advisory).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub efforts: Vec<String>,
}

/// `capabilities.profiles[].delivery`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliverySupport {
    pub this_run: ThisRunSupport,
    pub next_turn: bool,
}

/// `capabilities.profiles[].controls`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileControls {
    pub cancel: bool,
    pub close: bool,
}

/// One profile in the capabilities catalogue. Its identity and status fields are never
/// dropped or shortened by truncation; only `models` is cut.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileCapabilities {
    pub profile_id: ProfileId,
    pub name: String,
    pub driver: Driver,
    pub available: bool,
    pub reason: Option<String>,
    pub modes: Vec<ModeAdmission>,
    pub models: Vec<ModelInfo>,
    pub workspaces: Vec<WorkspaceKind>,
    pub delivery: DeliverySupport,
    pub controls: ProfileControls,
}

/// `capabilities.limits`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityLimits {
    pub max_task_bytes: u32,
    pub max_message_bytes: u32,
    pub max_page_events: u32,
    pub max_page_bytes: u32,
    pub max_wait_ms: u32,
    pub max_depth: u8,
    pub active_runs_per_root: u32,
}

impl CapabilityLimits {
    pub fn from_limits(l: &Limits) -> Self {
        Self {
            max_task_bytes: l.max_task_bytes,
            max_message_bytes: l.max_message_bytes,
            max_page_events: l.page_events_max,
            max_page_bytes: l.page_bytes_max,
            max_wait_ms: l.wait_timeout_max_ms,
            max_depth: l.max_depth,
            active_runs_per_root: l.active_runs_per_root,
        }
    }
}

/// What truncation removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Omitted {
    pub profiles: u32,
    pub models: u32,
}

/// `capabilities.delegation_scope`: limits apply to agents started through agent-terminal
/// ([`super::tools::DELEGATION_SCOPE_TEXT`] explains it).
pub const DELEGATION_SCOPE_SUPERVISED: &str = "supervised";

/// `cross_agent_capabilities` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilitiesResponse {
    pub api_version: u32,
    /// Live: recomputed on every call.
    pub caller: Principal,
    pub authority_epoch: u64,
    pub delegation_scope: String,
    pub profiles: Vec<ProfileCapabilities>,
    pub limits: CapabilityLimits,
    pub truncated: bool,
    pub omitted: Omitted,
}

/// A spawn's state at acceptance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpawnState {
    Accepted,
    Queued,
    Starting,
}

/// A worker's workspace, as reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub kind: WorkspaceKind,
    /// Display path, home shown as `~`, at most 1 KiB (shortened in the middle).
    pub path: Option<String>,
    /// At most 64 bytes.
    pub base_commit: Option<String>,
}

/// `cross_agent_spawn` output's `authority`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnAuthority {
    pub task_mode: TaskMode,
    pub workspace: WorkspaceInfo,
    pub can_spawn: bool,
    pub depth: u8,
}

/// `cross_agent_spawn` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnReceipt {
    #[serde(flatten)]
    pub receipt: MutationReceipt,
    pub worker_id: WorkerId,
    pub run_id: Option<RunId>,
    pub state: SpawnState,
    pub identity: Identity,
    pub authority: SpawnAuthority,
}

/// `cross_agent_send` output, frozen at acceptance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendReceipt {
    #[serde(flatten)]
    pub receipt: MutationReceipt,
    pub message_id: MessageId,
    /// The state at acceptance.
    pub delivery: DeliveryState,
    /// The run this delivery was aimed at when accepted; `None` for `next_turn` and queued.
    pub target_run_id: Option<RunId>,
}

/// A cancel's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelState {
    Stopping,
    Idle,
    Stopped,
}

/// `cross_agent_cancel` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelReceipt {
    #[serde(flatten)]
    pub receipt: MutationReceipt,
    pub worker_id: WorkerId,
    pub run_id: Option<RunId>,
    pub state: CancelState,
}

/// A close's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseState {
    Closing,
    Closed,
}

/// `cross_agent_close` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseReceipt {
    #[serde(flatten)]
    pub receipt: MutationReceipt,
    pub worker_id: WorkerId,
    pub state: CloseState,
}

/// One refused acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AckRejection {
    pub message_id: MessageId,
    pub code: ErrorCode,
}

/// Read's `ack` object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AckOutcome {
    #[serde(flatten)]
    pub receipt: MutationReceipt,
    pub acknowledged: Vec<MessageId>,
    pub rejected: Vec<AckRejection>,
}

/// `PageEvent.category`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventCategory {
    Lifecycle,
    Activity,
    Result,
    Approval,
    Message,
    Error,
}

/// One event unit of a page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageEvent {
    pub seq: u64,
    pub worker_id: WorkerId,
    pub run_id: Option<RunId>,
    /// RFC 3339.
    pub at: String,
    pub category: EventCategory,
    pub state: Option<StateRef>,
    /// Scrubbed, at most 2 KiB, never token streams.
    pub summary: String,
}

/// One message unit of a page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageMessage {
    pub seq: u64,
    pub message_id: MessageId,
    pub from: WorkerId,
    pub to: WorkerId,
    pub kind: MessageKind,
    pub reply_to: Option<MessageId>,
    /// Scrubbed, at most 16 KiB.
    pub text: String,
    pub delivery: DeliveryState,
    /// The run it was delivered into; `None` while queued or when pulled from a mailbox.
    pub recipient_run_id: Option<RunId>,
    pub acknowledged: bool,
    pub at: String,
}

/// A worker as `list`, `read` and `wait` report it (contract §2 `WorkerSummary`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerSummary {
    pub worker_id: WorkerId,
    pub parent_id: Option<WorkerId>,
    pub name: String,
    pub origin: WorkerOrigin,
    pub state: WorkerState,
    pub run_id: Option<RunId>,
    pub run_state: Option<RunState>,
    pub identity: Option<Identity>,
    pub depth: u8,
    pub can_spawn: bool,
    pub controls: Controls,
    pub task_mode: Option<TaskMode>,
    pub workspace: WorkspaceInfo,
    pub delegation: DelegationStatus,
    /// Scrubbed, at most 512 bytes.
    pub last_activity: Option<String>,
    /// RFC 3339.
    pub updated_at: String,
}

/// `cross_agent_read` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub api_version: u32,
    pub snapshot: Option<WorkerSummary>,
    pub events: Vec<PageEvent>,
    pub messages: Vec<PageMessage>,
    pub next_cursor: Cursor,
    pub truncated: bool,
    pub epoch_changed: bool,
    pub ack: Option<AckOutcome>,
}

/// `cross_agent_wait` output: a page (with `ack: null`) and whether it timed out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitPage {
    #[serde(flatten)]
    pub page: Page,
    pub timed_out: bool,
}

/// `cross_agent_list` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListPage {
    pub api_version: u32,
    pub workers: Vec<WorkerSummary>,
    pub next_cursor: Option<Cursor>,
    pub truncated: bool,
}

// ---------------------------------------------------------------------------------------------
// Output bounds
// ---------------------------------------------------------------------------------------------

/// Field bounds of a `WorkerSummary` (contract §2 output bounds).
pub const MAX_NAME_CHARS: usize = 80;
pub const MAX_MODEL_BYTES: usize = 128;
pub const MAX_EFFORT_BYTES: usize = 32;
pub const MAX_PATH_BYTES: usize = 1024;
pub const MAX_BASE_COMMIT_BYTES: usize = 64;
pub const MAX_LAST_ACTIVITY_BYTES: usize = 512;

/// `s` cut to `max` bytes by removing its middle, marked with `…`.
pub(crate) fn shorten_middle(s: &mut String, max: usize) {
    const MARK: &str = "…";
    if s.len() <= max {
        return;
    }
    let keep = max.saturating_sub(MARK.len());
    let mut head = keep / 2;
    while !s.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail_start = s.len() - (keep - head);
    while !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let out = format!("{}{MARK}{}", &s[..head], &s[tail_start..]);
    *s = out;
}

fn cut(s: &mut String, max: usize) {
    let n = truncate_bytes(s, max).len();
    s.truncate(n);
}

fn cut_chars(s: &mut String, max: usize) {
    if let Some((i, _)) = s.char_indices().nth(max) {
        s.truncate(i);
    }
}

fn fit_value(v: &mut super::identity::ValueProvenance, max: usize) {
    if let Some(s) = &mut v.requested {
        cut(s, max);
    }
    if let Some(s) = &mut v.effective {
        cut(s, max);
    }
}

fn json_len<T: Serialize>(v: &T) -> usize {
    serde_json::to_string(v).map_or(usize::MAX, |s| s.len())
}

/// Brings a summary within its field bounds, shortening `workspace.path` in the middle; then,
/// should the whole still exceed [`MAX_SUMMARY_BYTES`], shortens the path, then
/// `last_activity`, then the status paths, until it fits.
pub fn fit_summary(s: &mut WorkerSummary) {
    cut_chars(&mut s.name, MAX_NAME_CHARS);
    if let Some(id) = &mut s.identity {
        fit_value(&mut id.model, MAX_MODEL_BYTES);
        fit_value(&mut id.effort, MAX_EFFORT_BYTES);
    }
    if let Some(p) = &mut s.workspace.path {
        shorten_middle(p, MAX_PATH_BYTES);
    }
    if let Some(b) = &mut s.workspace.base_commit {
        cut(b, MAX_BASE_COMMIT_BYTES);
    }
    s.delegation.fit();
    if let Some(a) = &mut s.last_activity {
        cut(a, MAX_LAST_ACTIVITY_BYTES);
    }
    while json_len(s) > MAX_SUMMARY_BYTES {
        if let Some(p) = s.workspace.path.as_mut().filter(|p| p.len() > 16) {
            let half = p.len() / 2;
            shorten_middle(p, half);
        } else if let Some(a) = s.last_activity.as_mut().filter(|a| !a.is_empty()) {
            let half = a.len() / 2;
            cut(a, half);
        } else if s.delegation.paths.pop().is_none() {
            break;
        }
    }
}

/// `len(json(output)) + len(json_string(json(output)))`: the `structuredContent` plus the same
/// JSON as an escaped text block.
pub fn transport_cost(output: &Value) -> usize {
    let Ok(json) = serde_json::to_string(output) else {
        return usize::MAX;
    };
    json.len() + escaped_len(&json)
}

/// Length of `s` as a JSON string literal, quotes included.
pub(crate) fn escaped_len(s: &str) -> usize {
    serde_json::to_string(s).map_or(usize::MAX, |e| e.len())
}

/// The encoded JSON-RPC id is at most [`MAX_REQUEST_ID_BYTES`]. `false` → `-32600` with id
/// null, the id never echoed.
pub fn request_id_ok(encoded_id: &str) -> bool {
    encoded_id.len() <= MAX_REQUEST_ID_BYTES
}

/// The final guard (contract §3): an over-budget output becomes `internal_error` with
/// `details.reason: "output_budget"` (and `details.operation_id` when one exists). It only
/// changes what is sent, never the stored operation.
pub fn guard_output(output: Value, operation: Option<&OperationId>) -> Result<Value, ToolError> {
    if transport_cost(&output) <= OUTPUT_BUDGET {
        return Ok(output);
    }
    let mut err = ToolError::new(
        ErrorCode::InternalError,
        "the response exceeded the output budget",
    )
    .with_detail("reason", "output_budget");
    if let Some(op) = operation {
        err = err.with_detail("operation_id", op.as_str());
    }
    Err(err)
}

fn cost_of(resp: &CapabilitiesResponse) -> usize {
    serde_json::to_value(resp).map_or(usize::MAX, |v| transport_cost(&v))
}

/// The transport cost of removing `v` (and one separating comma) from the serialisation.
fn removal_cost<T: Serialize>(v: &T, with_comma: bool) -> usize {
    let Ok(json) = serde_json::to_string(v) else {
        return 0;
    };
    // Escaping is per character, so a piece costs its own escaped length (less the quotes).
    let mut c = json.len() + escaped_len(&json).saturating_sub(2);
    if with_comma {
        c += 2;
    }
    c
}

/// One deterministic truncation step; returns the cost it removed, or `None` when nothing
/// more may go (only the first profile, with no models, is left).
fn truncation_step(resp: &mut CapabilitiesResponse) -> Option<usize> {
    if let Some(pi) = resp.profiles.iter().rposition(|p| !p.models.is_empty()) {
        let models = &mut resp.profiles[pi].models;
        let comma = models.len() > 1;
        let removed = models.pop()?;
        resp.omitted.models += 1;
        return Some(removal_cost(&removed, comma));
    }
    if resp.profiles.len() > 1 {
        let removed = resp.profiles.pop()?;
        resp.omitted.profiles += 1;
        return Some(removal_cost(&removed, true));
    }
    None
}

/// Deterministic: keeps Settings and catalogue order; drops the last model of the last
/// profile that still lists one until it fits, and only then whole profiles from the end;
/// never the first profile; never shortens a profile's identity or status fields. Sets
/// `truncated` and `omitted`. Post-condition: the response's `transport_cost` is at most
/// [`OUTPUT_BUDGET`].
pub fn truncate_capabilities(resp: &mut CapabilitiesResponse) {
    if cost_of(resp) <= OUTPUT_BUDGET {
        return;
    }
    resp.truncated = true;
    // Exact up to the digits of the `omitted` counters, which the measured pass below settles.
    let mut estimate = cost_of(resp);
    while estimate > OUTPUT_BUDGET {
        match truncation_step(resp) {
            Some(removed) => estimate = estimate.saturating_sub(removed),
            None => break,
        }
    }
    while cost_of(resp) > OUTPUT_BUDGET {
        if truncation_step(resp).is_none() {
            break;
        }
    }
}

/// Worst-case values at every field bound, for the output-budget tests.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use crate::delegation::identity::{Provenance, ValueProvenance};

    /// `bytes` bytes of `c` (rounded down to whole characters).
    pub(crate) fn text(c: char, bytes: usize) -> String {
        std::iter::repeat_n(c, bytes / c.len_utf8()).collect()
    }

    fn vp(c: char, max: usize) -> ValueProvenance {
        ValueProvenance {
            requested: Some(text(c, max)),
            effective: Some(text(c, max)),
            source: Provenance::ProviderReported,
        }
    }

    fn id<T>(prefix: &str, fill: char, parse: fn(&str) -> Result<T, IdError>) -> T {
        match parse(&format!("{prefix}{}", fill.to_string().repeat(40))) {
            Ok(v) => v,
            Err(e) => panic!("fixture id: {e}"),
        }
    }

    pub(crate) fn worker(fill: char) -> WorkerId {
        id("w_", fill, WorkerId::parse)
    }

    pub(crate) fn run(fill: char) -> RunId {
        id("r_", fill, RunId::parse)
    }

    pub(crate) fn message(fill: char) -> MessageId {
        id("m_", fill, MessageId::parse)
    }

    pub(crate) fn operation() -> OperationId {
        id("op_", 'O', OperationId::parse)
    }

    pub(crate) fn profile(n: usize) -> ProfileId {
        match ProfileId::parse(&format!("{n:0>64}")) {
            Ok(p) => p,
            Err(e) => panic!("fixture profile: {e}"),
        }
    }

    pub(crate) fn worst_identity(c: char) -> Identity {
        Identity {
            driver: Driver::Codex,
            profile_id: profile(0),
            model: vp(c, MAX_MODEL_BYTES),
            effort: vp(c, MAX_EFFORT_BYTES),
        }
    }

    pub(crate) fn worst_workspace(c: char) -> WorkspaceInfo {
        WorkspaceInfo {
            kind: WorkspaceKind::IsolatedWorktree,
            path: Some(text(c, MAX_PATH_BYTES)),
            base_commit: Some(text(c, MAX_BASE_COMMIT_BYTES)),
        }
    }

    pub(crate) fn worst_summary(c: char) -> WorkerSummary {
        WorkerSummary {
            worker_id: worker('W'),
            parent_id: Some(worker('P')),
            name: std::iter::repeat_n(c, MAX_NAME_CHARS).collect(),
            origin: WorkerOrigin::Supervised,
            state: WorkerState::Stopping,
            run_id: Some(run('R')),
            run_state: Some(RunState::Interrupted),
            identity: Some(worst_identity(c)),
            depth: 2,
            can_spawn: false,
            controls: Controls {
                send: true,
                this_run: ThisRunSupport::BestEffort,
                cancel: true,
                close: true,
            },
            task_mode: Some(TaskMode::AcceptEdits),
            workspace: worst_workspace(c),
            delegation: DelegationStatus {
                code: DelegationStatusCode::WorkspaceUntrusted,
                reason: Some(text(c, MAX_STATUS_REASON_BYTES)),
                paths: vec![text(c, MAX_STATUS_PATH_BYTES); MAX_STATUS_PATHS],
            },
            last_activity: Some(text(c, MAX_LAST_ACTIVITY_BYTES)),
            updated_at: "2026-10-15T00:00:00.000000000+00:00".into(),
        }
    }

    pub(crate) fn worst_receipt(c: char) -> MutationReceipt {
        MutationReceipt {
            api_version: 1,
            operation_id: operation(),
            authority_epoch: u64::MAX,
            replayed: false,
            dispatch: DispatchState::DispatchReserved,
            warnings: vec![
                Warning::new(
                    &"w".repeat(MAX_WARNING_CODE_BYTES),
                    &text(c, MAX_WARNING_MESSAGE_BYTES)
                );
                MAX_WARNINGS
            ],
        }
    }

    pub(crate) fn worst_ack(c: char) -> AckOutcome {
        AckOutcome {
            receipt: worst_receipt(c),
            acknowledged: vec![message('A'); 100],
            rejected: vec![
                AckRejection {
                    message_id: message('B'),
                    code: ErrorCode::IdempotencyConflict,
                };
                100
            ],
        }
    }

    pub(crate) fn worst_message(seq: u64, c: char) -> PageMessage {
        PageMessage {
            seq,
            message_id: message('M'),
            from: worker('F'),
            to: worker('T'),
            kind: MessageKind::Question,
            reply_to: Some(message('Q')),
            text: text(c, 16 * 1024),
            delivery: DeliveryState::QueuedNextTurn,
            recipient_run_id: Some(run('X')),
            acknowledged: false,
            at: "2026-10-15T00:00:00.000000000+00:00".into(),
        }
    }

    pub(crate) fn worst_spawn_receipt(c: char) -> SpawnReceipt {
        SpawnReceipt {
            receipt: worst_receipt(c),
            worker_id: worker('W'),
            run_id: Some(run('R')),
            state: SpawnState::Starting,
            identity: worst_identity(c),
            authority: SpawnAuthority {
                task_mode: TaskMode::AcceptEdits,
                workspace: worst_workspace(c),
                can_spawn: true,
                depth: 2,
            },
        }
    }

    /// 32 profiles, 64 models each with 8 efforts, every string at its byte cap.
    pub(crate) fn maximal_catalogue(c: char) -> CapabilitiesResponse {
        let modes: Vec<ModeAdmission> = TaskMode::ALL
            .into_iter()
            .map(|task_mode| ModeAdmission {
                task_mode,
                route_status: RouteStatus::Unverified,
                route_blockers: vec![text(c, 64); 16],
                // Every caveat at once: an enum, so its real names are its largest values.
                route_caveats: super::super::route::RouteCaveat::ALL
                    .map(|caveat| caveat.as_str().to_owned())
                    .to_vec(),
                child_bridge: ChildBridge::WithoutBridge,
            })
            .collect();
        let models: Vec<ModelInfo> = (0..64)
            .map(|_| ModelInfo {
                id: text(c, MAX_MODEL_BYTES),
                efforts: vec![text(c, MAX_EFFORT_BYTES); 8],
            })
            .collect();
        CapabilitiesResponse {
            api_version: 1,
            caller: Principal {
                worker_id: worker('W'),
                root_id: worker('R'),
                driver: Driver::Claude,
                depth: 0,
                can_spawn: true,
                task_mode_ceiling: TaskMode::ALL.to_vec(),
            },
            authority_epoch: u64::MAX,
            delegation_scope: DELEGATION_SCOPE_SUPERVISED.into(),
            profiles: (0..32)
                .map(|i| ProfileCapabilities {
                    profile_id: profile(i),
                    name: std::iter::repeat_n(c, MAX_NAME_CHARS).collect(),
                    driver: Driver::Codex,
                    available: true,
                    reason: Some(text(c, MAX_STATUS_REASON_BYTES)),
                    modes: modes.clone(),
                    models: models.clone(),
                    workspaces: vec![
                        WorkspaceKind::SharedReadOnly,
                        WorkspaceKind::IsolatedWorktree,
                    ],
                    delivery: DeliverySupport {
                        this_run: ThisRunSupport::Strict,
                        next_turn: true,
                    },
                    controls: ProfileControls {
                        cancel: true,
                        close: true,
                    },
                })
                .collect(),
            limits: CapabilityLimits::from_limits(&Limits::default()),
            truncated: false,
            omitted: Omitted::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn state_ref_is_domain_and_value() {
        assert_eq!(
            serde_json::to_value(StateRef::Run(RunState::Stopping)).expect("ser"),
            json!({"domain": "run", "value": "stopping"})
        );
        assert_eq!(
            serde_json::to_value(StateRef::Worker(WorkerState::Waiting)).expect("ser"),
            json!({"domain": "worker", "value": "waiting"})
        );
        let back: StateRef =
            serde_json::from_value(json!({"domain": "run", "value": "completed"})).expect("de");
        assert_eq!(back, StateRef::Run(RunState::Completed));
    }

    #[test]
    fn this_run_support_and_controls_serialise_snake_case() {
        for (v, name) in [
            (ThisRunSupport::Strict, "strict"),
            (ThisRunSupport::BestEffort, "best_effort"),
            (ThisRunSupport::Unsupported, "unsupported"),
        ] {
            assert_eq!(serde_json::to_value(v).expect("ser"), json!(name));
        }
        let c = Controls {
            send: true,
            this_run: ThisRunSupport::BestEffort,
            cancel: true,
            close: false,
        };
        assert_eq!(
            serde_json::to_value(c).expect("ser"),
            json!({"send": true, "this_run": "best_effort", "cancel": true, "close": false})
        );
    }

    #[test]
    fn dispatch_state_all_is_exhaustive_and_unknown_decodes() {
        use DispatchState::*;
        fn next(s: DispatchState) -> Option<DispatchState> {
            match s {
                Accepted => Some(DispatchReserved),
                DispatchReserved => Some(Submitted),
                Submitted => Some(Confirmed),
                Confirmed => Some(Failed),
                Failed => Some(Unknown),
                Unknown => None,
            }
        }
        let mut walked = vec![Accepted];
        while let Some(s) = next(*walked.last().expect("non-empty")) {
            walked.push(s);
        }
        assert_eq!(walked, DispatchState::ALL);
        for s in DispatchState::ALL {
            assert_eq!(serde_json::to_value(s).expect("ser"), json!(s.as_str()));
        }
        assert_eq!(
            serde_json::from_str::<DispatchState>("\"later\"").expect("de"),
            Unknown
        );
    }

    #[test]
    fn delegation_status_all_has_six_and_only_rebind_required_auto_rebinds() {
        use DelegationStatusCode::*;
        fn next(c: DelegationStatusCode) -> Option<DelegationStatusCode> {
            match c {
                Available => Some(WorkspaceUntrusted),
                WorkspaceUntrusted => Some(RebindRequired),
                RebindRequired => Some(Restricted),
                Restricted => Some(BridgeDisabled),
                BridgeDisabled => Some(Unavailable),
                Unavailable => None,
            }
        }
        let mut walked = vec![Available];
        while let Some(c) = next(*walked.last().expect("non-empty")) {
            walked.push(c);
        }
        assert_eq!(walked, DelegationStatusCode::ALL);
        assert_eq!(DelegationStatusCode::ALL.len(), 6);
        for c in DelegationStatusCode::ALL {
            assert_eq!(c.auto_rebind(), c == RebindRequired);
        }
        let mut s = DelegationStatus {
            code: WorkspaceUntrusted,
            reason: Some("é".repeat(300)),
            paths: vec!["p".repeat(300); 20],
        };
        s.fit();
        assert!(s.reason.as_deref().is_some_and(|r| r.len() <= 256));
        assert_eq!(s.paths.len(), 16);
        assert!(s.paths.iter().all(|p| p.len() == 256));
    }

    #[test]
    fn target_and_workspace_keep_their_wire_shapes() {
        assert_eq!(
            serde_json::to_value(Target::Parent).expect("ser"),
            json!("parent")
        );
        let w = WorkerId::parse("w_abcdefgh").expect("valid");
        assert_eq!(
            serde_json::from_value::<Target>(json!("w_abcdefgh")).expect("de"),
            Target::Worker(w)
        );
        assert!(serde_json::from_value::<Target>(json!("Parent")).is_err());
        assert_eq!(
            serde_json::to_value(WorkspaceRequest::IsolatedWorktree {
                base: Some("HEAD".into())
            })
            .expect("ser"),
            json!({"kind": "isolated_worktree", "base": "HEAD"})
        );
        assert_eq!(
            serde_json::to_value(WorkspaceRequest::SharedReadOnly).expect("ser"),
            json!({"kind": "shared_read_only"})
        );
    }

    fn parse(tool: ToolName, v: Value) -> Result<ToolRequest, ToolError> {
        parse_request(tool, &v, &Limits::default())
    }

    fn field_of(e: &ToolError) -> Option<String> {
        e.details
            .as_ref()
            .and_then(|d| d.get("field"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    fn assert_invalid(tool: ToolName, v: Value, field: &str) {
        let e = parse(tool, v.clone()).expect_err(&format!("{tool:?} {v}"));
        assert_eq!(e.code, ErrorCode::InvalidArgument, "{v}");
        assert_eq!(field_of(&e).as_deref(), Some(field), "{v}");
    }

    fn with(mut base: Value, field: &str, value: Value) -> Value {
        base[field] = value;
        base
    }

    fn spawn_example() -> Value {
        json!({"profile_id": "codex", "name": "reviewer", "task": "Review the diff", "task_mode": "plan",
               "model": "gpt-5", "effort": "high", "workspace": {"kind": "shared_read_only"},
               "expected_authority_epoch": 4, "idempotency_key": "spawn-1"})
    }

    fn send_example() -> Value {
        json!({"target": "w_abcdefgh", "text": "Continue", "kind": "instruction", "reply_to": "m_abcdefgh",
               "delivery": "this_run", "expected_run_id": "r_abcdefgh", "idempotency_key": "send-1"})
    }

    #[test]
    fn the_contract_example_requests_parse() {
        let cases = [
            (ToolName::CrossAgentCapabilities, json!({})),
            (
                ToolName::CrossAgentCapabilities,
                json!({"profile_id": "codex"}),
            ),
            (ToolName::CrossAgentSpawn, spawn_example()),
            (
                ToolName::CrossAgentSpawn,
                json!({"profile_id": "claude", "name": "fixer", "task": "Fix it", "task_mode": "accept_edits",
                       "workspace": {"kind": "isolated_worktree", "base": "main~1"}, "idempotency_key": "s2"}),
            ),
            (ToolName::CrossAgentSend, send_example()),
            (
                ToolName::CrossAgentSend,
                json!({"target": "parent", "text": "Done", "kind": "report", "delivery": "auto",
                       "idempotency_key": "r1"}),
            ),
            (
                ToolName::CrossAgentRead,
                json!({"target": "w_abcdefgh", "after": "abc", "max_events": 500, "max_bytes": 262144,
                       "acknowledge": ["m_abcdefgh"], "idempotency_key": "a1", "expected_authority_epoch": 0}),
            ),
            (
                ToolName::CrossAgentWait,
                json!({"workers": ["w_abcdefgh"], "mailbox": false, "after": "abc", "timeout_ms": 30000,
                       "max_events": 1, "max_bytes": 1024}),
            ),
            (
                ToolName::CrossAgentList,
                json!({"target": "w_abcdefgh", "cursor": "abc", "limit": 100}),
            ),
            (
                ToolName::CrossAgentCancel,
                json!({"worker_id": "w_abcdefgh", "expected_run_id": "r_abcdefgh", "idempotency_key": "c1"}),
            ),
            (
                ToolName::CrossAgentClose,
                json!({"worker_id": "w_abcdefgh", "idempotency_key": "x1"}),
            ),
        ];
        for (tool, v) in cases {
            let req = parse(tool, v.clone()).unwrap_or_else(|e| panic!("{tool:?} {v}: {e}"));
            assert_eq!(req.tool(), tool);
            // The typed request serialises back to exactly the arguments given.
            assert_eq!(req.args_value(), v, "{tool:?}");
        }
    }

    #[test]
    fn every_bound_is_refused_one_past_it_with_its_field() {
        let l = Limits::default();
        let s = ToolName::CrossAgentSpawn;
        let at = |n: usize| "a".repeat(n);
        assert!(parse(s, with(spawn_example(), "name", json!("é".repeat(80)))).is_ok());
        assert_invalid(
            s,
            with(spawn_example(), "name", json!("é".repeat(81))),
            "name",
        );
        assert_invalid(s, with(spawn_example(), "name", json!("")), "name");
        assert!(parse(s, with(spawn_example(), "task", json!(at(65_536)))).is_ok());
        assert_invalid(s, with(spawn_example(), "task", json!(at(65_537))), "task");
        assert_invalid(s, with(spawn_example(), "task", json!("")), "task");
        assert!(parse(s, with(spawn_example(), "model", json!(at(128)))).is_ok());
        assert_invalid(s, with(spawn_example(), "model", json!(at(129))), "model");
        assert!(parse(s, with(spawn_example(), "effort", json!(at(32)))).is_ok());
        assert_invalid(s, with(spawn_example(), "effort", json!(at(33))), "effort");
        assert!(parse(s, with(spawn_example(), "profile_id", json!(at(64)))).is_ok());
        assert_invalid(
            s,
            with(spawn_example(), "profile_id", json!(at(65))),
            "profile_id",
        );
        assert!(parse(s, with(spawn_example(), "idempotency_key", json!(at(128)))).is_ok());
        assert_invalid(
            s,
            with(spawn_example(), "idempotency_key", json!(at(129))),
            "idempotency_key",
        );
        assert_invalid(
            s,
            with(spawn_example(), "expected_authority_epoch", json!(-1)),
            "expected_authority_epoch",
        );
        assert_invalid(
            s,
            with(
                spawn_example(),
                "workspace",
                json!({"kind": "isolated_worktree", "base": at(257)}),
            ),
            "workspace.base",
        );
        assert_invalid(
            s,
            with(
                spawn_example(),
                "workspace",
                json!({"kind": "isolated_worktree", "base": "--upload-pack=x"}),
            ),
            "workspace.base",
        );
        let t = ToolName::CrossAgentSend;
        assert!(parse(t, with(send_example(), "text", json!(at(16_384)))).is_ok());
        assert_invalid(t, with(send_example(), "text", json!(at(16_385))), "text");
        assert_invalid(
            t,
            with(send_example(), "target", json!("w_short")),
            "target",
        );
        let r = ToolName::CrossAgentRead;
        for (field, ok, bad) in [
            ("max_events", 500, 501),
            ("max_events", 1, 0),
            ("max_bytes", 262_144, 262_145),
            ("max_bytes", 1024, 1023),
        ] {
            assert!(parse(r, json!({ field: ok })).is_ok());
            assert_invalid(r, json!({ field: bad }), field);
        }
        let ids: Vec<String> = (0..=l.ack_batch_max).map(|i| format!("m_{i:08}")).collect();
        assert!(parse(
            r,
            json!({"acknowledge": ids[..100].to_vec(), "idempotency_key": "k"})
        )
        .is_ok());
        assert_invalid(
            r,
            json!({"acknowledge": ids, "idempotency_key": "k"}),
            "acknowledge",
        );
        assert_invalid(r, json!({"after": at(257)}), "after");
        let w = ToolName::CrossAgentWait;
        let workers: Vec<String> = (0..=32).map(|i| format!("w_{i:08}")).collect();
        assert!(parse(w, json!({"workers": workers[..32].to_vec()})).is_ok());
        assert_invalid(w, json!({ "workers": workers }), "workers");
        assert!(parse(w, json!({"timeout_ms": 30_000})).is_ok());
        assert_invalid(w, json!({"timeout_ms": 30_001}), "timeout_ms");
        assert_invalid(w, json!({"max_events": 501}), "max_events");
        let li = ToolName::CrossAgentList;
        assert!(parse(li, json!({"limit": 100})).is_ok());
        assert_invalid(li, json!({"limit": 101}), "limit");
        assert_invalid(li, json!({"limit": 0}), "limit");
    }

    #[test]
    fn unknown_fields_and_wrong_types_are_refused() {
        for tool in ToolName::ALL {
            assert_invalid(tool, json!({"surprise": 1}), "surprise");
            let e = parse(tool, json!([])).expect_err("array");
            assert_eq!(field_of(&e).as_deref(), Some("arguments"));
        }
        assert_invalid(
            ToolName::CrossAgentSpawn,
            with(spawn_example(), "extra", json!(true)),
            "extra",
        );
        assert_invalid(
            ToolName::CrossAgentSpawn,
            with(
                spawn_example(),
                "workspace",
                json!({"kind": "shared_read_only", "base": "HEAD"}),
            ),
            "workspace.base",
        );
        assert_invalid(
            ToolName::CrossAgentSpawn,
            with(spawn_example(), "workspace", json!({"kind": "tmp", "x": 1})),
            "workspace.x",
        );
        assert_invalid(
            ToolName::CrossAgentSpawn,
            with(spawn_example(), "task_mode", json!("bypass")),
            "task_mode",
        );
        assert_invalid(
            ToolName::CrossAgentSpawn,
            with(spawn_example(), "model", Value::Null),
            "model",
        );
        assert_invalid(
            ToolName::CrossAgentSend,
            with(send_example(), "delivery", json!(1)),
            "delivery",
        );
        let mut missing = send_example();
        if let Some(m) = missing.as_object_mut() {
            m.remove("idempotency_key");
        }
        assert_invalid(ToolName::CrossAgentSend, missing, "idempotency_key");
    }

    #[test]
    fn cross_field_rules() {
        // A writer on the shared tree.
        let e = parse(
            ToolName::CrossAgentSpawn,
            with(spawn_example(), "task_mode", json!("ask")),
        )
        .expect_err("writer on shared");
        assert_eq!(e.code, ErrorCode::UnsupportedWorkspace);
        assert_eq!(field_of(&e).as_deref(), Some("workspace"));
        // Kinds by direction.
        assert_invalid(
            ToolName::CrossAgentSend,
            with(send_example(), "kind", json!("report")),
            "kind",
        );
        assert_invalid(
            ToolName::CrossAgentSend,
            with(
                with(send_example(), "target", json!("parent")),
                "kind",
                json!("instruction"),
            ),
            "kind",
        );
        // idempotency_key iff acknowledge.
        assert_invalid(
            ToolName::CrossAgentRead,
            json!({"acknowledge": ["m_abcdefgh"]}),
            "idempotency_key",
        );
        assert_invalid(
            ToolName::CrossAgentRead,
            json!({"idempotency_key": "k"}),
            "idempotency_key",
        );
        // Present means 1..=100 entries, with or without a key.
        assert_invalid(
            ToolName::CrossAgentRead,
            json!({"acknowledge": []}),
            "acknowledge",
        );
        assert_invalid(
            ToolName::CrossAgentRead,
            json!({"acknowledge": [], "idempotency_key": "k"}),
            "acknowledge",
        );
    }

    #[test]
    fn normalisation_and_mutation() {
        let read = parse(ToolName::CrossAgentRead, json!({})).expect("read");
        assert!(!read.is_mutating());
        assert_eq!(
            read.normalised().args_value(),
            json!({"max_events": 100, "max_bytes": 65536})
        );
        let ack = parse(
            ToolName::CrossAgentRead,
            json!({"acknowledge": ["m_abcdefgh"], "idempotency_key": "k"}),
        )
        .expect("ack");
        assert!(ack.is_mutating());
        let wait = parse(ToolName::CrossAgentWait, json!({})).expect("wait");
        assert_eq!(
            wait.normalised().args_value(),
            json!({"mailbox": true, "timeout_ms": 10000, "max_events": 100, "max_bytes": 65536})
        );
        let list = parse(ToolName::CrossAgentList, json!({})).expect("list");
        assert_eq!(list.normalised().args_value(), json!({"limit": 50}));
        let spawn = parse(
            ToolName::CrossAgentSpawn,
            with(
                with(spawn_example(), "task_mode", json!("ask")),
                "workspace",
                json!({"kind": "isolated_worktree"}),
            ),
        )
        .expect("spawn");
        assert!(spawn.is_mutating());
        assert_eq!(
            spawn.normalised().args_value()["workspace"],
            json!({"kind": "isolated_worktree", "base": "HEAD"})
        );
        for tool in ToolName::ALL {
            let mutating_tool = tool.is_mutating();
            if let Ok(req) = parse(tool, json!({})) {
                assert_eq!(req.is_mutating(), mutating_tool, "{tool:?}");
            }
        }
    }

    fn models(r: &CapabilitiesResponse) -> usize {
        r.profiles.iter().map(|p| p.models.len()).sum()
    }

    /// The real MCP response: a JSON-RPC result carrying `structuredContent` and the same JSON
    /// as a text block, under the largest allowed request id.
    fn mcp_response_len(output: &Value) -> usize {
        let text = serde_json::to_string(output).expect("ser");
        let id = "i".repeat(MAX_REQUEST_ID_BYTES - 2);
        let response = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"content": [{"type": "text", "text": text}], "structuredContent": output, "isError": false}
        });
        serde_json::to_string(&response).expect("ser").len()
    }

    fn check_truncation(original: &CapabilitiesResponse, resp: &CapabilitiesResponse) {
        let v = serde_json::to_value(resp).expect("ser");
        assert!(transport_cost(&v) <= OUTPUT_BUDGET);
        assert!(mcp_response_len(&v) < 1 << 20);
        assert_eq!(
            resp.omitted.profiles as usize,
            original.profiles.len() - resp.profiles.len()
        );
        assert_eq!(
            resp.omitted.models as usize,
            models(original) - models(resp)
        );
        assert!(
            !resp.profiles.is_empty(),
            "the first profile is never removed"
        );
        let mut cut_seen = false;
        for (i, p) in resp.profiles.iter().enumerate() {
            let o = &original.profiles[i];
            let mut same_identity = p.clone();
            same_identity.models = o.models.clone();
            assert_eq!(&same_identity, o, "identity and status fields intact");
            assert_eq!(
                p.models[..],
                o.models[..p.models.len()],
                "models are a prefix"
            );
            if cut_seen {
                assert!(p.models.is_empty(), "models go from the last profile first");
            }
            if p.models.len() < o.models.len() {
                cut_seen = true;
            }
        }
        if resp.omitted.profiles > 0 {
            assert_eq!(models(resp), 0, "models are removed before any profile");
        }
    }

    #[test]
    fn maximal_catalogue_is_truncated_to_the_budget() {
        for c in ['a', '\u{1}'] {
            let original = fixtures::maximal_catalogue(c);
            let before = serde_json::to_value(&original).expect("ser");
            assert!(transport_cost(&before) > OUTPUT_BUDGET, "{c:?}");
            let mut resp = original.clone();
            truncate_capabilities(&mut resp);
            assert!(resp.truncated);
            check_truncation(&original, &resp);
            if c == '\u{1}' {
                assert!(
                    resp.omitted.profiles > 0,
                    "six-fold escaping drops whole profiles"
                );
            }
            // With profile_id: a single maximal profile, the same rule.
            let mut single = original.clone();
            single.profiles.truncate(1);
            let single_original = single.clone();
            truncate_capabilities(&mut single);
            check_truncation(&single_original, &single);
            assert_eq!(single.profiles.len(), 1);
        }
        // Under the budget: untouched.
        let mut small = fixtures::maximal_catalogue('a');
        small.profiles.truncate(2);
        let copy = small.clone();
        truncate_capabilities(&mut small);
        assert_eq!(small, copy);
        assert!(!small.truncated);
    }

    #[test]
    fn worst_case_summaries_fit_their_bound() {
        for c in ['a', '\u{1}', 'é', '😀'] {
            let s = fixtures::worst_summary(c);
            assert!(
                serde_json::to_string(&s).expect("ser").len() <= MAX_SUMMARY_BYTES,
                "{c:?}"
            );
        }
        let mut over = fixtures::worst_summary('\u{1}');
        over.name = "n".repeat(500);
        over.workspace.path = Some(fixtures::text('\u{1}', 50_000));
        over.last_activity = Some(fixtures::text('\u{1}', 50_000));
        over.delegation.reason = Some("r".repeat(5_000));
        over.delegation.paths = vec!["p".repeat(5_000); 40];
        if let Some(id) = &mut over.identity {
            id.model.requested = Some("m".repeat(5_000));
        }
        fit_summary(&mut over);
        assert!(serde_json::to_string(&over).expect("ser").len() <= MAX_SUMMARY_BYTES);
        assert_eq!(over.name.chars().count(), MAX_NAME_CHARS);
        let path = over.workspace.path.clone().unwrap_or_default();
        assert!(path.len() <= MAX_PATH_BYTES);
        assert!(path.contains('…'));
        assert!(over
            .last_activity
            .as_deref()
            .is_some_and(|a| a.len() <= 512));
        assert_eq!(over.delegation.paths.len(), MAX_STATUS_PATHS);
        assert!(over
            .identity
            .as_ref()
            .and_then(|i| i.model.requested.as_deref())
            .is_some_and(|m| m.len() <= MAX_MODEL_BYTES));
        let mut short = "a/b".to_owned();
        shorten_middle(&mut short, 1024);
        assert_eq!(short, "a/b");
        let mut long = format!("/home/{}/end", "x".repeat(2000));
        shorten_middle(&mut long, 64);
        assert!(long.len() <= 64 && long.starts_with("/home/") && long.ends_with("/end"));
    }

    #[test]
    fn maximal_spawn_receipt_replays_as_delivery_unknown_within_bounds() {
        use crate::delegation::ids::AuthorityEpoch;
        use crate::delegation::operation::{
            replay, CanonicalRequest, OperationKind, ReplayOutcome, Resolution, StoredOperation,
        };
        let receipt = fixtures::worst_spawn_receipt('\u{1}');
        let receipt_json = serde_json::to_string(&receipt).expect("ser");
        assert!(
            receipt_json.len() <= MAX_RECEIPT_BYTES,
            "{}",
            receipt_json.len()
        );
        let ack = serde_json::to_string(&fixtures::worst_ack('\u{1}')).expect("ser");
        assert!(ack.len() <= MAX_ACK_BYTES);
        let op = StoredOperation {
            id: fixtures::operation(),
            kind: OperationKind::Spawn,
            epoch: AuthorityEpoch(1),
            canonical: CanonicalRequest::from_stored("{}".into()),
            resolution: Resolution::default(),
            receipt: Some(serde_json::from_str(&receipt_json).expect("value")),
            dispatch: DispatchState::Unknown,
            error: None,
        };
        let ReplayOutcome::DeliveryUnknown(err) = replay(&op, false) else {
            panic!("delivery_unknown");
        };
        let details = err.details.clone().expect("details");
        assert!(serde_json::to_string(&details).expect("ser").len() <= MAX_ERROR_DETAILS_BYTES);
        assert_eq!(
            details["operation_id"],
            json!(fixtures::operation().as_str())
        );
        // Byte for byte against the stored receipt (`receipt_json` as the store holds it).
        let stored = serde_json::to_string(&op.receipt).expect("ser");
        assert!(
            serde_json::to_string(&details["receipt"]).expect("ser") == stored,
            "the original receipt, byte for byte"
        );
        let back: SpawnReceipt = serde_json::from_value(details["receipt"].clone()).expect("de");
        assert_eq!(back, receipt);
        let output = json!({ "error": err });
        assert!(transport_cost(&output) <= OUTPUT_BUDGET);
    }

    #[test]
    fn request_ids_are_bounded_when_encoded() {
        let encoded = |v: Value| serde_json::to_string(&v).expect("ser");
        let s256 = encoded(json!("s".repeat(254)));
        assert_eq!(s256.len(), 256);
        assert!(request_id_ok(&s256));
        assert!(!request_id_ok(&encoded(json!("s".repeat(255)))));
        // Escaped characters count as encoded: 42 × 6 + 2 quotes + 2 = 256.
        let esc = encoded(json!(format!("{}ab", "\u{1}".repeat(42))));
        assert_eq!(esc.len(), 256);
        assert!(request_id_ok(&esc));
        let esc257 = encoded(json!(format!("{}abc", "\u{1}".repeat(42))));
        assert_eq!(esc257.len(), 257);
        assert!(!request_id_ok(&esc257));
        // A number id as received (any length of digits).
        assert!(request_id_ok(&"9".repeat(256)));
        assert!(!request_id_ok(&"9".repeat(257)));
        assert!(request_id_ok(&encoded(json!(7))));
    }

    #[test]
    fn guard_output_replaces_an_oversized_output_and_leaves_the_operation_alone() {
        use crate::delegation::ids::AuthorityEpoch;
        use crate::delegation::operation::{
            replay, CanonicalRequest, OperationKind, ReplayOutcome, Resolution, StoredOperation,
        };
        let small = json!({"ok": true});
        assert_eq!(guard_output(small.clone(), None), Ok(small));
        let receipt = serde_json::to_value(fixtures::worst_spawn_receipt('a')).expect("ser");
        let op = StoredOperation {
            id: fixtures::operation(),
            kind: OperationKind::Spawn,
            epoch: AuthorityEpoch(1),
            canonical: CanonicalRequest::from_stored("{}".into()),
            resolution: Resolution::default(),
            receipt: Some(receipt.clone()),
            dispatch: DispatchState::Accepted,
            error: None,
        };
        let stored_before = op.clone();
        let huge = json!({"x": "y".repeat(OUTPUT_BUDGET)});
        let err = guard_output(huge, Some(&op.id)).expect_err("over budget");
        assert_eq!(err.code, ErrorCode::InternalError);
        assert!(!err.retryable);
        let d = err.details.expect("details");
        assert_eq!(d["reason"], "output_budget");
        assert_eq!(d["operation_id"], json!(op.id.as_str()));
        assert_eq!(op, stored_before);
        let ReplayOutcome::Receipt(r) = replay(&op, false) else {
            panic!("replay");
        };
        assert_eq!(r["worker_id"], receipt["worker_id"]);
        assert_eq!(r["replayed"], true);
    }

    #[test]
    fn a_send_receipt_is_frozen_at_acceptance() {
        use crate::delegation::ids::AuthorityEpoch;
        use crate::delegation::mailbox::delivery_step;
        use crate::delegation::operation::{
            replay, CanonicalRequest, OperationKind, ReplayOutcome, Resolution, StoredOperation,
        };
        let target = fixtures::run('A');
        let accepted = SendReceipt {
            receipt: MutationReceipt {
                api_version: 1,
                operation_id: fixtures::operation(),
                authority_epoch: 3,
                replayed: false,
                dispatch: DispatchState::Accepted,
                warnings: vec![],
            },
            message_id: fixtures::message('M'),
            delivery: DeliveryState::Submitted,
            target_run_id: Some(target.clone()),
        };
        let mut op = StoredOperation {
            id: fixtures::operation(),
            kind: OperationKind::Send,
            epoch: AuthorityEpoch(3),
            canonical: CanonicalRequest::from_stored("{}".into()),
            resolution: Resolution::default(),
            receipt: Some(serde_json::to_value(&accepted).expect("ser")),
            dispatch: DispatchState::Accepted,
            error: None,
        };
        // The message later lands in another run: a fact on the message record only.
        let record = delivery_step(DeliveryState::Submitted, DeliveryState::Misdelivered);
        assert_eq!(record, Ok(DeliveryState::Misdelivered));
        op.dispatch = DispatchState::Confirmed;
        let ReplayOutcome::Receipt(r) = replay(&op, false) else {
            panic!("replay");
        };
        let replayed: SendReceipt = serde_json::from_value(r).expect("de");
        assert_eq!(replayed.target_run_id, Some(target));
        assert_eq!(replayed.delivery, DeliveryState::Submitted);
        assert!(replayed.receipt.replayed);
        assert_eq!(replayed.receipt.dispatch, DispatchState::Confirmed);
        let mut expected = accepted.clone();
        expected.receipt.replayed = true;
        expected.receipt.dispatch = DispatchState::Confirmed;
        assert_eq!(replayed, expected);
    }

    #[test]
    fn canonical_json_sorts_keys_at_every_level() {
        let v = json!({"b": {"z": 1, "a": [{"y": 2, "x": 1}]}, "a": "\u{1}"});
        assert_eq!(
            canonical_json(&v),
            r#"{"a":"\u0001","b":{"a":[{"x":1,"y":2}],"z":1}}"#
        );
    }
}
