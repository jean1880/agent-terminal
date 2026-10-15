//! Operations (contract §2 "Canonical request", "Idempotency and replay"; §8 item 5).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::error::{ErrorCode, ToolError};
use super::ids::{AuthorityEpoch, OperationId, RunId, WorkerId};
use super::lifecycle::LifecycleError;
use super::wire::{canonical_json, ToolRequest};
use crate::event::WorkerState;

pub use super::wire::DispatchState as OperationState;

/// What an operation does; idempotency is unique per principal, kind and key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Spawn,
    Send,
    Ack,
    Cancel,
    Close,
}

impl OperationKind {
    /// The kind of a mutating request; `None` for reads, waits, lists and capabilities.
    pub fn of(req: &ToolRequest) -> Option<Self> {
        match req {
            ToolRequest::Spawn(_) => Some(OperationKind::Spawn),
            ToolRequest::Send(_) => Some(OperationKind::Send),
            ToolRequest::Read(_) if req.is_mutating() => Some(OperationKind::Ack),
            ToolRequest::Cancel(_) => Some(OperationKind::Cancel),
            ToolRequest::Close(_) => Some(OperationKind::Close),
            ToolRequest::Read(_)
            | ToolRequest::Capabilities(_)
            | ToolRequest::Wait(_)
            | ToolRequest::List(_) => None,
        }
    }
}

/// Legal: `Accepted → {DispatchReserved, Failed}`; `DispatchReserved → {Submitted, Confirmed,
/// Failed, Unknown}`; `Submitted → {Confirmed, Failed, Unknown}`. `Accepted → Failed` is also
/// the revocation path (`stale_epoch`).
pub fn operation_step(
    from: OperationState,
    to: OperationState,
) -> Result<OperationState, LifecycleError> {
    use OperationState as O;
    let ok = match from {
        O::Accepted => matches!(to, O::DispatchReserved | O::Failed),
        O::DispatchReserved => matches!(to, O::Submitted | O::Confirmed | O::Failed | O::Unknown),
        O::Submitted => matches!(to, O::Confirmed | O::Failed | O::Unknown),
        O::Confirmed | O::Failed | O::Unknown => false,
    };
    if ok {
        Ok(to)
    } else {
        Err(LifecycleError::Illegal {
            from: WorkerState::Unknown,
            input: "operation_step",
        })
    }
}

/// After a restart, a reservation or submission whose outcome was not recorded is `Unknown`
/// (never re-dispatched).
pub fn on_restart(state: OperationState) -> OperationState {
    match state {
        OperationState::DispatchReserved | OperationState::Submitted => OperationState::Unknown,
        other => other,
    }
}

/// Sorted-key JSON of the normalised, scrubbed request without its idempotency key and epoch:
/// stored and compared byte for byte, never hashed. It holds the request's syntax, not its
/// resolution (`"parent"` and `"HEAD"` stay as written).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CanonicalRequest(String);

impl CanonicalRequest {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A stored value read back.
    pub fn from_stored(s: String) -> Self {
        Self(s)
    }
}

/// The free-text fields `canonical_request` scrubs.
const FREE_TEXT_FIELDS: [&str; 3] = ["name", "task", "text"];

/// Builds the canonical request with `scrub` (the app passes `redact::redact`).
pub fn canonical_request(req: &ToolRequest, scrub: impl Fn(&str) -> String) -> CanonicalRequest {
    let mut args = req.normalised().args_value();
    if let Value::Object(map) = &mut args {
        map.remove("idempotency_key");
        map.remove("expected_authority_epoch");
        for field in FREE_TEXT_FIELDS {
            if let Some(Value::String(s)) = map.get_mut(field) {
                *s = scrub(s);
            }
        }
    }
    let wrapped = serde_json::json!({"tool": req.tool().as_str(), "args": args});
    CanonicalRequest(canonical_json(&wrapped))
}

/// What a request resolved to, fixed at acceptance and never recomputed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolution {
    pub target: Option<WorkerId>,
    pub bound_runs: Vec<RunId>,
    pub base_commit: Option<String>,
}

/// One stored operation (contract §8 item 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredOperation {
    pub id: OperationId,
    pub kind: OperationKind,
    /// The epoch it was accepted under.
    pub epoch: AuthorityEpoch,
    pub canonical: CanonicalRequest,
    pub resolution: Resolution,
    /// The original success receipt, written once at acceptance.
    pub receipt: Option<Value>,
    /// Current; updated separately from the receipt.
    pub dispatch: OperationState,
    /// The scrubbed error of a `Failed` operation.
    pub error: Option<ToolError>,
}

/// What a mutating call's idempotency key means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdempotencyDecision {
    New,
    Replay(OperationId),
    Conflict(OperationId),
    StaleEpoch(OperationId),
}

/// Contract §2 order: an `expected_epoch` that is not current is `Err(StaleEpoch)` before any
/// lookup; a key stored under an older epoch is `StaleEpoch`; a different canonical request is
/// `Conflict`; an identical one is `Replay`.
pub fn idempotency(
    existing: Option<&StoredOperation>,
    kind: OperationKind,
    canonical: &CanonicalRequest,
    expected_epoch: Option<AuthorityEpoch>,
    current: AuthorityEpoch,
) -> Result<IdempotencyDecision, ErrorCode> {
    if let Some(e) = expected_epoch {
        if e != current {
            return Err(ErrorCode::StaleEpoch);
        }
    }
    let Some(op) = existing else {
        return Ok(IdempotencyDecision::New);
    };
    if op.epoch != current {
        return Ok(IdempotencyDecision::StaleEpoch(op.id.clone()));
    }
    if op.kind != kind || &op.canonical != canonical {
        return Ok(IdempotencyDecision::Conflict(op.id.clone()));
    }
    Ok(IdempotencyDecision::Replay(op.id.clone()))
}

/// What a replay returns (the contract §2 table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayOutcome {
    /// The original receipt with `replayed: true` and `dispatch` = the current state.
    Receipt(Value),
    /// The stored error with `details.operation_id` and `details.replayed: true`.
    Error(ToolError),
    /// `delivery_unknown` with `details: {operation_id, receipt: <original receipt>}`.
    DeliveryUnknown(ToolError),
    /// This instance is dispatching it: wait (bounded by the call timeout), then replay.
    WaitForDispatch,
}

fn missing_receipt(op: &StoredOperation) -> ReplayOutcome {
    ReplayOutcome::Error(
        ToolError::new(
            ErrorCode::InternalError,
            "the operation has no stored receipt",
        )
        .with_detail("operation_id", op.id.as_str()),
    )
}

/// The contract §2 replay table. The stored receipt is refreshed in exactly two fields,
/// `replayed` and `dispatch`; nothing else of it ever changes.
pub fn replay(op: &StoredOperation, dispatching_now: bool) -> ReplayOutcome {
    match op.dispatch {
        OperationState::Accepted | OperationState::Submitted | OperationState::Confirmed => {
            match &op.receipt {
                Some(Value::Object(original)) => {
                    let mut r = original.clone();
                    r.insert("replayed".into(), Value::Bool(true));
                    r.insert(
                        "dispatch".into(),
                        Value::String(op.dispatch.as_str().into()),
                    );
                    ReplayOutcome::Receipt(Value::Object(r))
                }
                _ => missing_receipt(op),
            }
        }
        OperationState::DispatchReserved if dispatching_now => ReplayOutcome::WaitForDispatch,
        OperationState::DispatchReserved | OperationState::Unknown => {
            let receipt = op.receipt.clone().unwrap_or(Value::Null);
            ReplayOutcome::DeliveryUnknown(
                ToolError::new(
                    ErrorCode::DeliveryUnknown,
                    "the operation was dispatched and its outcome is unknown; it is never re-sent",
                )
                .with_detail("operation_id", op.id.as_str())
                .with_detail("receipt", receipt),
            )
        }
        OperationState::Failed => match &op.error {
            Some(e) => ReplayOutcome::Error(
                e.clone()
                    .with_detail("operation_id", op.id.as_str())
                    .with_detail("replayed", true),
            ),
            None => missing_receipt(op),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delegation::limits::Limits;
    use crate::delegation::tools::ToolName;
    use crate::delegation::wire::parse_request;
    use crate::redact::redact;
    use serde_json::json;

    fn req(tool: ToolName, v: Value) -> ToolRequest {
        parse_request(tool, &v, &Limits::default()).expect("valid")
    }

    fn op_id() -> OperationId {
        OperationId::parse("op_aaaaaaaa").expect("valid")
    }

    fn stored(dispatch: OperationState) -> StoredOperation {
        StoredOperation {
            id: op_id(),
            kind: OperationKind::Send,
            epoch: AuthorityEpoch(2),
            canonical: CanonicalRequest("c".into()),
            resolution: Resolution::default(),
            receipt: Some(json!({
                "api_version": 1, "operation_id": "op_aaaaaaaa", "authority_epoch": 2,
                "replayed": false, "dispatch": "accepted", "warnings": [],
                "message_id": "m_aaaaaaaa", "delivery": "submitted", "target_run_id": "r_aaaaaaaa"
            })),
            dispatch,
            error: None,
        }
    }

    #[test]
    fn only_legal_operation_transitions_pass() {
        use OperationState as O;
        let legal = [
            (O::Accepted, O::DispatchReserved),
            (O::Accepted, O::Failed),
            (O::DispatchReserved, O::Submitted),
            (O::DispatchReserved, O::Confirmed),
            (O::DispatchReserved, O::Failed),
            (O::DispatchReserved, O::Unknown),
            (O::Submitted, O::Confirmed),
            (O::Submitted, O::Failed),
            (O::Submitted, O::Unknown),
        ];
        for from in O::ALL {
            for to in O::ALL {
                assert_eq!(
                    operation_step(from, to).is_ok(),
                    legal.contains(&(from, to)),
                    "{from:?} -> {to:?}"
                );
            }
        }
        assert_eq!(on_restart(O::DispatchReserved), O::Unknown);
        assert_eq!(on_restart(O::Submitted), O::Unknown);
        assert_eq!(on_restart(O::Accepted), O::Accepted);
        assert_eq!(on_restart(O::Confirmed), O::Confirmed);
    }

    #[test]
    fn idempotency_decisions() {
        let current = AuthorityEpoch(2);
        let op = stored(OperationState::Accepted);
        let same = op.canonical.clone();
        let other = CanonicalRequest("d".into());
        assert_eq!(
            idempotency(None, OperationKind::Send, &same, None, current),
            Ok(IdempotencyDecision::New)
        );
        assert_eq!(
            idempotency(Some(&op), OperationKind::Send, &same, None, current),
            Ok(IdempotencyDecision::Replay(op_id()))
        );
        assert_eq!(
            idempotency(
                Some(&op),
                OperationKind::Send,
                &other,
                Some(current),
                current
            ),
            Ok(IdempotencyDecision::Conflict(op_id()))
        );
        // Stored under an older epoch, request with no epoch.
        assert_eq!(
            idempotency(
                Some(&op),
                OperationKind::Send,
                &same,
                None,
                AuthorityEpoch(3)
            ),
            Ok(IdempotencyDecision::StaleEpoch(op_id()))
        );
        // Both epoch and request differ: StaleEpoch wins.
        assert_eq!(
            idempotency(
                Some(&op),
                OperationKind::Send,
                &other,
                None,
                AuthorityEpoch(3)
            ),
            Ok(IdempotencyDecision::StaleEpoch(op_id()))
        );
        // A stale expected epoch is refused before any lookup.
        assert_eq!(
            idempotency(
                None,
                OperationKind::Send,
                &same,
                Some(AuthorityEpoch(1)),
                current
            ),
            Err(ErrorCode::StaleEpoch)
        );
        assert_eq!(
            idempotency(
                Some(&op),
                OperationKind::Send,
                &same,
                Some(AuthorityEpoch(1)),
                current
            ),
            Err(ErrorCode::StaleEpoch)
        );
    }

    #[test]
    fn canonical_request_is_syntax_without_key_or_epoch() {
        let a = req(
            ToolName::CrossAgentSend,
            json!({"target": "parent", "text": "hi", "kind": "report", "delivery": "auto",
                   "idempotency_key": "k1", "expected_authority_epoch": 3}),
        );
        let b = req(
            ToolName::CrossAgentSend,
            json!({"idempotency_key": "k2", "delivery": "auto", "kind": "report", "text": "hi",
                   "target": "parent"}),
        );
        let ca = canonical_request(&a, redact);
        assert_eq!(ca, canonical_request(&b, redact));
        assert!(!ca.as_str().contains("k1"));
        assert!(!ca.as_str().contains("idempotency_key"));
        assert!(!ca.as_str().contains("expected_authority_epoch"));
        assert!(ca.as_str().contains("\"parent\""));
        let omitted = req(
            ToolName::CrossAgentSpawn,
            json!({"profile_id": "codex", "name": "n", "task": "t", "task_mode": "plan",
                   "workspace": {"kind": "isolated_worktree"}, "idempotency_key": "k"}),
        );
        let explicit = req(
            ToolName::CrossAgentSpawn,
            json!({"profile_id": "codex", "name": "n", "task": "t", "task_mode": "plan",
                   "workspace": {"kind": "isolated_worktree", "base": "HEAD"}, "idempotency_key": "k"}),
        );
        let c = canonical_request(&omitted, redact);
        assert_eq!(c, canonical_request(&explicit, redact));
        assert!(c.as_str().contains("\"HEAD\""));
        // Scrubbed: two secrets that redact identically are the same request.
        // Built at run time so no credential-shaped literal sits in the source.
        let secret = |c: &str| format!("key {}_{}WXYZ", "ghp", c.repeat(32));
        let s1 = req(
            ToolName::CrossAgentSend,
            json!({"target": "parent", "text": secret("a"),
                   "kind": "report", "delivery": "auto", "idempotency_key": "k"}),
        );
        let s2 = req(
            ToolName::CrossAgentSend,
            json!({"target": "parent", "text": secret("b"),
                   "kind": "report", "delivery": "auto", "idempotency_key": "k"}),
        );
        let c1 = canonical_request(&s1, redact);
        assert_eq!(c1, canonical_request(&s2, redact));
        assert!(!c1.as_str().contains("aaaaaaaa"));
        // Sorted keys.
        let idx = |k: &str| c.as_str().find(k).expect(k);
        assert!(idx("\"name\"") < idx("\"profile_id\""));
        assert!(idx("\"profile_id\"") < idx("\"task\""));
    }

    #[test]
    fn replay_follows_the_contract_table() {
        // accepted / submitted / confirmed: the original receipt, replayed, current dispatch.
        for state in [
            OperationState::Accepted,
            OperationState::Submitted,
            OperationState::Confirmed,
        ] {
            let op = stored(state);
            let ReplayOutcome::Receipt(r) = replay(&op, false) else {
                panic!("receipt for {state:?}");
            };
            assert_eq!(r["replayed"], true);
            assert_eq!(r["dispatch"], state.as_str());
            let mut expected = op.receipt.clone().expect("receipt");
            expected["replayed"] = json!(true);
            expected["dispatch"] = json!(state.as_str());
            assert_eq!(r, expected, "only replayed and dispatch change");
        }
        // dispatch_reserved while this instance dispatches it: wait.
        assert_eq!(
            replay(&stored(OperationState::DispatchReserved), true),
            ReplayOutcome::WaitForDispatch
        );
        // unknown, and a reservation found after a restart: delivery_unknown with the receipt.
        for (state, dispatching) in [
            (OperationState::Unknown, false),
            (OperationState::Unknown, true),
            (OperationState::DispatchReserved, false),
        ] {
            let op = stored(state);
            let ReplayOutcome::DeliveryUnknown(e) = replay(&op, dispatching) else {
                panic!("delivery_unknown for {state:?}");
            };
            assert_eq!(e.code, ErrorCode::DeliveryUnknown);
            assert!(!e.retryable);
            let d = e.details.expect("details");
            assert_eq!(d["operation_id"], "op_aaaaaaaa");
            assert_eq!(Some(&d["receipt"]), op.receipt.as_ref());
        }
        // failed: the stored error, with operation_id and replayed.
        let mut failed = stored(OperationState::Failed);
        failed.error = Some(ToolError::new(ErrorCode::StaleRun, "run ended"));
        let ReplayOutcome::Error(e) = replay(&failed, false) else {
            panic!("error");
        };
        assert_eq!(e.code, ErrorCode::StaleRun);
        let d = e.details.expect("details");
        assert_eq!(d["operation_id"], "op_aaaaaaaa");
        assert_eq!(d["replayed"], true);
    }

    #[test]
    fn kinds_of_requests() {
        let read_ack = req(
            ToolName::CrossAgentRead,
            json!({"acknowledge": ["m_aaaaaaaa"], "idempotency_key": "k"}),
        );
        assert_eq!(OperationKind::of(&read_ack), Some(OperationKind::Ack));
        let read = req(ToolName::CrossAgentRead, json!({}));
        assert_eq!(OperationKind::of(&read), None);
    }
}
