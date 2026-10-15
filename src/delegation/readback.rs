//! Lane A: child read-back (§3.2–§3.3). Wave 0 holds the Codex extraction and comparison, which
//! the launch mapping (Lane B) and the checker (Lane A) must share.

use serde_json::Value;

/// What a Codex child's `thread/start` or `thread/resume` result must read back as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexRequested {
    /// `approvalPolicy`, as sent.
    pub approval_policy: String,
    /// The sandbox policy's `type` (`readOnly`, `workspaceWrite`), as sent.
    pub sandbox_type: String,
}

/// Extracts the read-back expectation from a Codex `(approvalPolicy, sandboxPolicy)` pair, as
/// `agent_core::codex::mode_policy` (roots) or the child mapping (Lane B) returns it. `None` when
/// the sandbox policy has no string `type`: nothing could be checked, so the child must not start.
pub(crate) fn codex_requested(policy: &(&'static str, Value)) -> Option<CodexRequested> {
    let (approval, sandbox) = policy;
    let sandbox_type = sandbox.get("type")?.as_str()?;
    Some(CodexRequested {
        approval_policy: (*approval).to_owned(),
        sandbox_type: sandbox_type.to_owned(),
    })
}

/// Whether a `thread/start` or `thread/resume` `result` reads back exactly what was requested:
/// `approvalPolicy` equal, and `sandbox.type` equal. A missing field fails.
pub(crate) fn codex_policy_read_back(requested: &CodexRequested, result: &Value) -> bool {
    let policy = result.get("approvalPolicy").and_then(Value::as_str);
    let sandbox = result
        .get("sandbox")
        .and_then(|s| s.get("type"))
        .and_then(Value::as_str);
    policy == Some(requested.approval_policy.as_str())
        && sandbox == Some(requested.sandbox_type.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::adapter::Mode;
    use serde_json::json;

    #[test]
    fn plan_mapping_reads_back_from_the_recorded_result_shape() {
        let requested = codex_requested(&agent_core::codex::mode_policy(Mode::Plan)).unwrap();
        assert_eq!(requested.approval_policy, "never");
        assert_eq!(requested.sandbox_type, "readOnly");
        // The shape recorded in tests/fixtures/real/delegation/codex/d6-sandbox-policy.
        let result = json!({"approvalPolicy": "never",
                            "sandbox": {"type": "readOnly", "networkAccess": false}});
        assert!(codex_policy_read_back(&requested, &result));
    }

    #[test]
    fn a_mismatch_or_a_missing_field_fails() {
        let requested = CodexRequested {
            approval_policy: "untrusted".into(),
            sandbox_type: "workspaceWrite".into(),
        };
        let wrong_sandbox = json!({"approvalPolicy": "untrusted", "sandbox": {"type": "readOnly"}});
        let wrong_policy =
            json!({"approvalPolicy": "never", "sandbox": {"type": "workspaceWrite"}});
        let flat_sandbox = json!({"approvalPolicy": "untrusted", "sandbox": "workspace-write"});
        let missing = json!({"sandbox": {"type": "workspaceWrite"}});
        for result in [wrong_sandbox, wrong_policy, flat_sandbox, missing] {
            assert!(!codex_policy_read_back(&requested, &result), "{result}");
        }
    }

    #[test]
    fn a_sandbox_without_a_type_gives_no_expectation() {
        assert_eq!(codex_requested(&("never", json!({}))), None);
        assert_eq!(codex_requested(&("never", json!({"type": 1}))), None);
    }
}
