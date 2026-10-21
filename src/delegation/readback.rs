//! Lane A: child read-back (§3.2–§3.3). Wave 0 holds the Codex extraction and comparison, which
//! the launch mapping (Lane B) and the checker (Lane A) must share.

use agent_core::adapter::Driver;
use agent_core::delegation::authority::TaskMode;
use agent_core::delegation::{ErrorCode, ToolError};
use agent_core::event::TurnState;
use serde_json::Value;

#[derive(Default)]
pub(super) struct Readback {
    driver: Option<Driver>,
    mode: Option<TaskMode>,
    pub ready: bool,
    pub bootstrap: bool,
    init: bool,
    model: bool,
    violated: bool,
    pub(super) failed: bool,
    policy: bool,
    server: bool,
}
pub(super) enum Step {
    Wait,
    Task,
    Stop(ToolError),
}
impl Readback {
    pub(super) fn child(driver: Driver, mode: TaskMode) -> Self {
        Self {
            driver: Some(driver),
            mode: Some(mode),
            bootstrap: driver == Driver::Claude,
            ..Self::default()
        }
    }
    /// The child's process has spawned. agy has no read-back of its own (its launch was proven
    /// before the spawn), but its session takes input only once that spawn is done: before it,
    /// a task is refused as `runtime_unavailable`. So agy becomes ready here, and only here.
    pub(super) fn started(&mut self) -> Step {
        if self.driver != Some(Driver::Agy) || self.failed || self.ready {
            return Step::Wait;
        }
        self.ready = true;
        Step::Task
    }
    pub(super) fn line(&mut self, v: &Value) -> Step {
        if self.failed {
            return Step::Wait;
        }
        match self.driver {
            Some(Driver::Claude) if self.bootstrap => {
                if contains_tool(v) {
                    self.violated = true;
                    return self.stop(
                        ErrorCode::RouteUnverified,
                        "tool use during Claude startup check",
                    );
                }
                if v.get("type").and_then(Value::as_str) == Some("system")
                    && v.get("subtype").and_then(Value::as_str) == Some("init")
                {
                    let tools = strings(v.get("tools"));
                    let servers = v.get("mcp_servers").and_then(Value::as_array).map(|a| {
                        a.iter()
                            .filter_map(|x| {
                                x.as_str().or_else(|| x.get("name").and_then(Value::as_str))
                            })
                            .map(str::to_owned)
                            .collect::<Vec<_>>()
                    });
                    self.init = self
                        .mode
                        .zip(tools)
                        .zip(servers)
                        .is_some_and(|((m, t), s)| {
                            agent_core::delegation::launch::claude_child_init_ok(m, &t, &s)
                        });
                    if !self.init {
                        return self.stop(
                            ErrorCode::RouteUnverified,
                            "Claude startup tools or MCP servers differ from the child policy",
                        );
                    }
                }
                if v.pointer("/message/model")
                    .and_then(Value::as_str)
                    .is_some_and(|m| !m.is_empty() && m != "unrecognized_model")
                {
                    self.model = true;
                }
                if v.pointer("/message/model").and_then(Value::as_str) == Some("unrecognized_model")
                {
                    return self.stop(
                        ErrorCode::ProviderUnavailable,
                        "Claude did not recognise the model",
                    );
                }
            }
            Some(Driver::Codex) => {
                if v.get("method").and_then(Value::as_str)
                    == Some("mcpServer/startupStatus/updated")
                {
                    let params = &v["params"];
                    let name = params
                        .get("name")
                        .or_else(|| params.get("serverName"))
                        .and_then(Value::as_str);
                    if name != Some("agent_terminal") {
                        return self.stop(
                            ErrorCode::ProviderUnavailable,
                            "an MCP server other than agent_terminal started",
                        );
                    }
                    self.server |= params.get("status").and_then(Value::as_str) == Some("ready");
                }
                if let Some(result) = v.get("result").filter(|r| r.get("thread").is_some()) {
                    let mode = self.mode.unwrap_or(TaskMode::Plan);
                    let sandbox = if mode == TaskMode::Plan {
                        "readOnly"
                    } else {
                        "workspaceWrite"
                    };
                    let approval = if mode == TaskMode::Plan {
                        "never"
                    } else {
                        "untrusted"
                    };
                    self.policy = codex_policy_read_back(
                        &CodexRequested {
                            approval_policy: approval.into(),
                            sandbox_type: sandbox.into(),
                        },
                        result,
                    );
                    if !self.policy {
                        return self.stop(
                            ErrorCode::RouteUnverified,
                            "Codex sandbox or approval policy failed read-back",
                        );
                    }
                }
                if self.policy && self.server && !self.ready {
                    self.ready = true;
                    return Step::Task;
                }
            }
            _ => {}
        }
        Step::Wait
    }
    pub(super) fn ended(&mut self, state: TurnState) -> Step {
        if self.failed || !self.bootstrap {
            return Step::Wait;
        }
        if state == TurnState::Completed && self.init && self.model && !self.violated {
            self.bootstrap = false;
            self.ready = true;
            Step::Task
        } else {
            self.stop(
                ErrorCode::RouteUnverified,
                "Claude startup check did not complete successfully",
            )
        }
    }
    pub(super) fn hello_timeout(&mut self, hello: bool) -> Step {
        if self.failed || self.ready {
            return Step::Wait;
        }
        if self.driver == Some(Driver::Codex) && self.policy && hello {
            self.ready = true;
            Step::Task
        } else if self.driver == Some(Driver::Agy) {
            self.stop(
                ErrorCode::ProviderUnavailable,
                "the agy process did not start within the startup deadline",
            )
        } else {
            self.stop(
                ErrorCode::RouteUnverified,
                "child startup read-back deadline elapsed",
            )
        }
    }
    fn stop(&mut self, code: ErrorCode, text: &str) -> Step {
        self.failed = true;
        self.ready = false;
        Step::Stop(ToolError::new(code, text))
    }
}
fn strings(v: Option<&Value>) -> Option<Vec<String>> {
    v?.as_array()?
        .iter()
        .map(|x| x.as_str().map(str::to_owned))
        .collect()
}
fn contains_tool(v: &Value) -> bool {
    match v {
        Value::Object(o) => {
            o.get("type").and_then(Value::as_str) == Some("tool_use")
                || o.values().any(contains_tool)
        }
        Value::Array(a) => a.iter().any(contains_tool),
        _ => false,
    }
}

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
#[cfg(test)]
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
    #[test]
    fn bootstrap_failure_and_interruption_never_unlock_the_real_task() {
        for state in [TurnState::Failed, TurnState::Interrupted] {
            let mut guard = Readback::child(Driver::Claude, TaskMode::Plan);
            let tools = agent_core::delegation::launch::CLAUDE_CHILD_TOOLS_PLAN;
            let init = json!({"type":"system","subtype":"init","tools":tools,"mcp_servers":[{"name":"agent_terminal"}]});
            assert!(matches!(guard.line(&init), Step::Wait));
            guard.line(&json!({"type":"assistant","message":{"model":"reported-model"}}));
            assert!(matches!(guard.ended(state), Step::Stop(_)));
            assert!(!guard.ready);
            assert!(matches!(guard.ended(TurnState::Completed), Step::Wait));
        }
    }
    #[test]
    fn a_tool_use_after_valid_init_is_latched() {
        let mut guard = Readback::child(Driver::Claude, TaskMode::Plan);
        guard.line(&json!({"type":"system","subtype":"init","tools":agent_core::delegation::launch::CLAUDE_CHILD_TOOLS_PLAN,"mcp_servers":[{"name":"agent_terminal"}]}));
        guard.line(&json!({"type":"assistant","message":{"model":"model","content":[{"type":"tool_use","name":"Read"}]}}));
        assert!(!guard.ready);
        assert!(matches!(guard.ended(TurnState::Completed), Step::Wait));
    }
    #[test]
    fn codex_reads_back_writer_sandbox_and_keeps_monitoring_servers() {
        let mut guard = Readback::child(Driver::Codex, TaskMode::Ask);
        assert!(matches!(guard.line(&json!({"result":{"thread":{"id":"t"},"approvalPolicy":"untrusted","sandbox":{"type":"workspaceWrite"}}})),Step::Wait));
        assert!(matches!(guard.line(&json!({"method":"mcpServer/startupStatus/updated","params":{"name":"agent_terminal","status":"ready"}})),Step::Task));
        assert!(guard.ready);
        assert!(matches!(guard.line(&json!({"method":"mcpServer/startupStatus/updated","params":{"name":"foreign","status":"ready"}})),Step::Stop(_)));
        assert!(!guard.ready);
    }
    #[test]
    fn agy_is_ready_only_once_its_process_started_and_the_deadline_bites_before() {
        let mut guard = Readback::child(Driver::Agy, TaskMode::Plan);
        assert!(
            !guard.ready,
            "agy must not take its task before its process exists"
        );
        assert!(matches!(guard.started(), Step::Task));
        assert!(guard.ready);
        assert!(
            matches!(guard.started(), Step::Wait),
            "the task is released once"
        );
        assert!(matches!(guard.hello_timeout(false), Step::Wait));
        let mut never = Readback::child(Driver::Agy, TaskMode::Plan);
        assert!(matches!(never.hello_timeout(true), Step::Stop(_)));
        assert!(
            matches!(never.started(), Step::Wait),
            "a stopped start stays stopped"
        );
        let mut claude = Readback::child(Driver::Claude, TaskMode::Plan);
        assert!(matches!(claude.started(), Step::Wait));
    }
    #[test]
    fn codex_startup_deadline_requires_policy_and_hello() {
        for policy in [false, true] {
            for hello in [false, true] {
                let mut guard = Readback::child(Driver::Codex, TaskMode::Plan);
                if policy {
                    guard.line(&json!({"result":{"thread":{"id":"t"},"approvalPolicy":"never","sandbox":{"type":"readOnly"}}}));
                }
                assert_eq!(
                    matches!(guard.hello_timeout(hello), Step::Task),
                    policy && hello
                );
            }
        }
    }
}
