//! Pure wire protocol for `agent-terminal --approval-hook`, agy's PreToolUse gate.
//!
//! agy runs with `--dangerously-skip-permissions`, so this hook is the only thing between the
//! model and the shell. The flow: agy writes a [`HookInput`] JSON document to the hook's stdin;
//! the hook turns it into an [`ApprovalQuery`], sends it as ONE NDJSON line over the unix socket
//! named by [`ENV_SOCKET`], reads ONE line back (an [`ApprovalReply`]) and prints
//! [`hook_output`] on stdout. Anything that goes wrong is a denial: fail closed.
//!
//! No socket, file or process I/O lives here; the binary and the app own that.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::Decision;

/// Environment variable carrying the approval socket path. Unset means the hook is a no-op.
pub const ENV_SOCKET: &str = "AGENT_TERMINAL_APPROVAL_SOCKET";

/// Whether the hook should gate this call. Unset (or blank) means agy was not started by the
/// app (interactive use, other callers): the hook prints nothing and exits 0, which agy reads
/// as "allow" so those sessions are unaffected.
pub fn should_gate(env_value: Option<&str>) -> bool {
    env_value.is_some_and(|v| !v.trim().is_empty())
}

/// The tool call agy is about to run (`toolCall` in the hook payload).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ToolCall {
    pub name: String,
    #[serde(default)]
    pub args: Value,
}

/// agy's PreToolUse stdin payload. Only `toolCall.name` is required: a payload without it is a
/// parse error, which the hook turns into a denial.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookInput {
    pub tool_call: ToolCall,
    #[serde(default)]
    pub conversation_id: String,
    #[serde(default)]
    pub model_name: Option<String>,
    #[serde(default)]
    pub step_idx: Option<u64>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub workspace_paths: Vec<String>,
    #[serde(default)]
    pub artifact_directory_path: Option<String>,
}

impl HookInput {
    pub fn parse(stdin: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(stdin)
    }

    /// The question for the app. `id` must be unique per call (the hook picks it). The working
    /// directory is the command's own `Cwd` when it has one, else the first workspace path.
    pub fn to_query(&self, id: impl Into<String>) -> ApprovalQuery {
        let cwd = self
            .tool_call
            .args
            .get("Cwd")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .map(str::to_owned)
            .or_else(|| self.workspace_paths.first().cloned());
        ApprovalQuery {
            id: id.into(),
            conversation_id: self.conversation_id.clone(),
            tool: self.tool_call.name.clone(),
            args: self.tool_call.args.clone(),
            cwd,
        }
    }
}

/// Hook to app: one NDJSON line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalQuery {
    pub id: String,
    pub conversation_id: String,
    pub tool: String,
    #[serde(default)]
    pub args: Value,
    #[serde(default)]
    pub cwd: Option<String>,
}

/// App to hook: one NDJSON line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalReply {
    pub decision: Decision,
    #[serde(default)]
    pub reason: Option<String>,
}

/// One wire line (no trailing newline) for a query.
pub fn encode_query(query: &ApprovalQuery) -> Result<String, serde_json::Error> {
    serde_json::to_string(query)
}

pub fn parse_query(line: &str) -> Result<ApprovalQuery, serde_json::Error> {
    serde_json::from_str(line.trim())
}

/// One wire line (no trailing newline) for a reply.
pub fn encode_reply(reply: &ApprovalReply) -> Result<String, serde_json::Error> {
    serde_json::to_string(reply)
}

pub fn parse_reply(line: &str) -> Result<ApprovalReply, serde_json::Error> {
    serde_json::from_str(line.trim())
}

/// What the hook prints for agy. `None` stands for every failure to obtain an answer (socket
/// unreachable, timeout, unparseable reply or payload) and denies: fail closed. Only an explicit
/// `Allow`, `AllowForSession` or `AllowAlways` lets the call run.
pub fn hook_output(reply: Option<&ApprovalReply>) -> String {
    let (decision, reason) = match reply {
        Some(r)
            if matches!(
                r.decision,
                Decision::Allow | Decision::AllowForSession | Decision::AllowAlways
            ) =>
        {
            return serde_json::json!({"decision": "allow"}).to_string();
        }
        Some(r) => (
            "deny",
            r.reason
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "denied in agent-terminal".to_owned()),
        ),
        None => (
            "deny",
            "agent-terminal approval unavailable (no answer); denied to stay safe".to_owned(),
        ),
    };
    serde_json::json!({"decision": decision, "reason": reason}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAYLOADS: &str = include_str!("../tests/fixtures/agy-hook-payloads.ndjson");

    fn fixture_payloads() -> Vec<String> {
        PAYLOADS
            .lines()
            .map(|l| {
                let v: Value = serde_json::from_str(l).expect("fixture line");
                v["payload"].to_string()
            })
            .collect()
    }

    fn out(reply: Option<&ApprovalReply>) -> Value {
        serde_json::from_str(&hook_output(reply)).expect("hook output is JSON")
    }

    #[test]
    fn parses_every_recorded_payload() {
        let payloads = fixture_payloads();
        assert_eq!(payloads.len(), 3);
        let input = HookInput::parse(&payloads[2]).expect("parse");
        assert_eq!(input.tool_call.name, "run_command");
        assert_eq!(input.tool_call.args["CommandLine"], "echo deny-me");
        assert_eq!(
            input.conversation_id,
            "3b87c9bb-5433-4305-8665-63c442e61e35"
        );
        assert_eq!(input.model_name.as_deref(), Some("gemini-3.8-flash-high"));
        assert_eq!(input.step_idx, Some(4));
        assert_eq!(input.workspace_paths, ["/work/repo"]);
        assert!(input.transcript_path.is_some());
        assert!(input.artifact_directory_path.is_some());
        for p in &payloads {
            assert!(HookInput::parse(p).is_ok());
        }
    }

    #[test]
    fn query_carries_tool_args_and_cwd() {
        let input = HookInput::parse(&fixture_payloads()[0]).expect("parse");
        let q = input.to_query("q1");
        assert_eq!(q.id, "q1");
        assert_eq!(q.tool, "run_command");
        assert_eq!(q.args["CommandLine"], "echo allow-me");
        assert_eq!(q.cwd.as_deref(), Some("/work/repo"));

        // Without a Cwd arg the first workspace path is used; without either, none.
        let bare =
            HookInput::parse(r#"{"toolCall":{"name":"write_to_file","args":{}}}"#).expect("parse");
        assert_eq!(bare.to_query("q2").cwd, None);
        let ws = HookInput::parse(
            r#"{"toolCall":{"name":"x"},"workspacePaths":["/a","/b"],"conversationId":"c"}"#,
        )
        .expect("parse");
        assert_eq!(ws.to_query("q3").cwd.as_deref(), Some("/a"));
    }

    #[test]
    fn unusable_payloads_are_errors() {
        assert!(HookInput::parse("").is_err());
        assert!(HookInput::parse("not json").is_err());
        assert!(HookInput::parse(r#"{"conversationId":"c"}"#).is_err());
        assert!(HookInput::parse(r#"{"toolCall":{"args":{}}}"#).is_err());
    }

    #[test]
    fn wire_lines_round_trip_and_are_single_lines() {
        let input = HookInput::parse(&fixture_payloads()[1]).expect("parse");
        let q = input.to_query("q9");
        let line = encode_query(&q).expect("encode");
        assert!(!line.contains('\n'));
        assert_eq!(parse_query(&format!("{line}\n")).expect("parse"), q);

        let reply = ApprovalReply {
            decision: Decision::Deny,
            reason: Some("no".into()),
        };
        let line = encode_reply(&reply).expect("encode");
        assert!(!line.contains('\n'));
        assert_eq!(parse_reply(&line).expect("parse"), reply);
        assert!(parse_reply("garbage").is_err());
        assert_eq!(
            parse_reply(r#"{"decision":"allow"}"#)
                .expect("parse")
                .reason,
            None
        );
    }

    #[test]
    fn allow_prints_allow() {
        for d in [
            Decision::Allow,
            Decision::AllowForSession,
            Decision::AllowAlways,
        ] {
            let r = ApprovalReply {
                decision: d,
                reason: None,
            };
            assert_eq!(out(Some(&r)), serde_json::json!({"decision": "allow"}));
        }
    }

    #[test]
    fn deny_and_cancel_print_deny_with_reason() {
        let denied = ApprovalReply {
            decision: Decision::Deny,
            reason: Some("too risky".into()),
        };
        let v = out(Some(&denied));
        assert_eq!(v["decision"], "deny");
        assert_eq!(v["reason"], "too risky");

        let cancelled = ApprovalReply {
            decision: Decision::Cancel,
            reason: Some("  ".into()),
        };
        let v = out(Some(&cancelled));
        assert_eq!(v["decision"], "deny");
        assert_eq!(v["reason"], "denied in agent-terminal");
    }

    #[test]
    fn no_answer_fails_closed() {
        // Unreachable socket, timeout and parse errors all reach the hook as `None`.
        let v = out(None);
        assert_eq!(v["decision"], "deny");
        assert!(v["reason"].as_str().is_some_and(|r| !r.is_empty()));
    }

    #[test]
    fn gate_is_off_only_when_the_variable_is_unset_or_blank() {
        assert_eq!(ENV_SOCKET, "AGENT_TERMINAL_APPROVAL_SOCKET");
        assert!(!should_gate(None));
        assert!(!should_gate(Some("")));
        assert!(!should_gate(Some("  ")));
        assert!(should_gate(Some(
            "/run/user/1000/agent-terminal/approval.sock"
        )));
    }
}
