//! The tool wire format (contract §2 and §3): request and response structs, the mutation
//! receipt envelope, and the output budget.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::authority::TaskMode;
use super::ids::{OperationId, WorkerId};
use super::lifecycle::RunState;
use super::route::{ChildBridge, RouteStatus};
use crate::event::WorkerState;

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
}
