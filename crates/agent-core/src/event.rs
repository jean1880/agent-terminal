//! Canonical, provider-neutral event stream.
//!
//! Every adapter turns its agent's native frames into [`Envelope`]s. The UI, the store and the
//! handoff logic only ever see these types, never a native frame (which is kept in `raw` for the
//! debug view and for re-mapping after an adapter fix).
//!
//! Shape follows T3 Code's `providerRuntime.ts` (MIT, Copyright (c) 2026 T3 Tools Inc.), cut down
//! to the events agent-terminal renders.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One canonical event plus the ids it belongs to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// Transcript item this event belongs to (assistant message, tool call, …), when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    /// Approval / question / control request this event belongs to, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<String>,
    pub event: Event,
    /// The native frame, verbatim. Must be scrubbed (`redact`) before it is persisted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value>,
}

impl Envelope {
    pub fn new(event: Event) -> Self {
        Self {
            item: None,
            request: None,
            event,
            raw: None,
        }
    }
    pub fn item(mut self, id: impl Into<String>) -> Self {
        self.item = Some(id.into());
        self
    }
    pub fn request(mut self, id: impl Into<String>) -> Self {
        self.request = Some(id.into());
        self
    }
    pub fn raw(mut self, raw: Value) -> Self {
        self.raw = Some(raw);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// The agent process is up and has a native session/conversation id.
    SessionStarted {
        native_id: String,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
    },
    /// The agent process exited. `expected` is true after a requested interrupt/close.
    SessionExited {
        code: Option<i32>,
        expected: bool,
    },
    /// Full replacement of the agent's slash commands / skills (typeahead source).
    CommandsChanged {
        commands: Vec<AgentCommand>,
    },
    /// A turn began (Claude re-emits init per turn: `model` is authoritative).
    TurnStarted {
        #[serde(default)]
        model: Option<String>,
    },
    TurnCompleted {
        state: TurnState,
        #[serde(default)]
        usage: Option<Usage>,
        #[serde(default)]
        cost_usd: Option<f64>,
        #[serde(default)]
        error: Option<String>,
    },
    /// A transcript item opened (`Envelope::item` is its id).
    ItemStarted {
        kind: ItemKind,
        title: String,
        #[serde(default)]
        input: Option<Value>,
        /// Parent tool-use id for subagent nesting.
        #[serde(default)]
        parent: Option<String>,
    },
    /// Streaming text appended to an item.
    ContentDelta {
        stream: StreamKind,
        text: String,
    },
    /// Authoritative full text of an item (replaces accumulated deltas; dedup by item id).
    ContentSnapshot {
        stream: StreamKind,
        text: String,
    },
    ItemCompleted {
        status: ItemStatus,
        #[serde(default)]
        output: Option<String>,
        #[serde(default)]
        error: Option<String>,
    },
    PlanUpdated {
        steps: Vec<PlanStep>,
    },
    /// The agent wants permission to run a tool (`Envelope::request` is the id to answer).
    ApprovalRequested {
        tool: String,
        #[serde(default)]
        title: Option<String>,
        input: Value,
        #[serde(default)]
        reason: Option<String>,
        options: Vec<Decision>,
        response: ResponseCapability,
        /// When "Always allow" is offered: exactly what it would save, in the agent's own terms
        /// (Claude: `Bash(git status:*)`; agy: the command line and its folder), so the user can
        /// judge how broad it is before choosing it.
        #[serde(default)]
        remembers: Option<String>,
    },
    ApprovalResolved {
        decision: Decision,
    },
    /// The approval can no longer be answered (the process that asked has exited).
    ApprovalExpired,
    /// The questions were answered or withdrawn; the UI closes the card.
    QuestionResolved {
        answered: bool,
    },
    /// The session's permission mode changed (reply to a mode switch, or agent-initiated).
    ModeChanged {
        mode: crate::adapter::Mode,
    },
    /// Structured questions (Claude AskUserQuestion).
    QuestionRequested {
        questions: Vec<Question>,
    },
    /// Context-window occupancy for the gauge.
    UsageUpdated {
        used: u64,
        #[serde(default)]
        max: Option<u64>,
        #[serde(default)]
        auto_compact_at: Option<u64>,
    },
    Compacted {
        manual: bool,
        before: u64,
        #[serde(default)]
        after: Option<u64>,
    },
    /// Account and plan-quota snapshot for the usage indicator. Claude sends one every turn
    /// (`rate_limit_event.unifiedWindows`); agy and Codex are refreshed after each turn.
    /// `account: None` means "unchanged", not "signed out".
    QuotaUpdated {
        #[serde(default)]
        account: Option<Account>,
        windows: Vec<QuotaWindow>,
    },
    RateLimited {
        #[serde(default)]
        resets_at: Option<String>,
        #[serde(default)]
        detail: Option<Value>,
    },
    ModelChanged {
        model: String,
    },
    /// Full replacement of the agent's running background tasks (sub-agents, shell commands);
    /// empty once the last one ends. A thread with any is still working after its turn ended.
    /// Claude only (`background_tasks_changed`).
    BackgroundTasks {
        tasks: Vec<BackgroundTask>,
    },
    /// Text the CLI produced itself (local slash command output, synthetic messages).
    Notice {
        text: String,
    },
    /// Reply to a control request (`Envelope::request` is the request id). Exactly one of
    /// `ok` / `error` is set; a successful reply with no payload is `ok: Some(Value::Null)`.
    ControlResult {
        #[serde(default)]
        ok: Option<Value>,
        #[serde(default)]
        error: Option<String>,
    },
    Error {
        message: String,
    },
    /// A native frame this adapter does not map (kept in `raw`; never aborts the stream).
    /// Also what an older build reads for an event type a newer build stored.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    Completed,
    Interrupted,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    UserMessage,
    AssistantMessage,
    Reasoning,
    Command,
    FileChange,
    FileRead,
    McpTool,
    WebSearch,
    Subagent,
    Tool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    Assistant,
    Reasoning,
    ToolInput,
    ToolOutput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    Completed,
    Failed,
    Declined,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    AllowForSession,
    /// Allow, and remember it beyond this session: Claude writes its own suggested rules to the
    /// project's `.claude/settings.local.json`; agy's exact-match rule is kept by the app per
    /// workspace. Offered only where such a rule exists; Codex never offers it.
    AllowAlways,
    Deny,
    Cancel,
}

/// Whether an approval can still be answered (approval callbacks die with the process).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseCapability {
    /// Answer over the live session (Claude `control_response`, agy approval hook).
    Live,
    /// The process that asked is gone; the request is shown as expired.
    Expired,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanStep {
    pub text: String,
    pub status: StepStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multi_select: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// Normalised turn usage: input includes cache reads and writes, output includes reasoning.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
}

/// Who an agent is signed in as. Shown in the UI only; never logged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Email or account name.
    pub label: String,
    /// Plan or tier, e.g. "Claude Pro".
    #[serde(default)]
    pub plan: Option<String>,
    /// Who serves the quota, e.g. "firstParty", "Google".
    #[serde(default)]
    pub provider: Option<String>,
}

/// One plan-quota window, e.g. "5-hour" at 7 % or Antigravity's "Gemini Models / Weekly".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    /// Model group sharing the window (agy: "Gemini Models"); `None` when the plan has one.
    #[serde(default)]
    pub group: Option<String>,
    /// Short window name: "5-hour", "Weekly", "Weekly (Fable)".
    pub label: String,
    /// Used share, 0.0–1.0 (agy reports remaining; adapters convert).
    pub used: f64,
    /// RFC 3339 reset time, when known.
    #[serde(default)]
    pub resets_at: Option<String>,
}

/// One running background task ([`Event::BackgroundTasks`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundTask {
    /// The agent's task id (Claude `task_id`).
    pub id: String,
    #[serde(default)]
    pub kind: BackgroundTaskKind,
    #[serde(default)]
    pub description: Option<String>,
    /// The tool call that started it, once known (Claude names it in `task_started`, which may
    /// arrive after the list first shows the task).
    #[serde(default)]
    pub tool_use_id: Option<String>,
}

/// What a background task runs. A kind this build does not know decodes as `Other`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundTaskKind {
    /// A sub-agent (Claude `local_agent`).
    Agent,
    /// A shell command (Claude `local_bash`, `bash`).
    Shell,
    #[default]
    #[serde(other)]
    Other,
}

/// A command or skill the agent itself offers (typeahead "agent" and "skill" providers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCommand {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub argument_hint: Option<String>,
    pub kind: AgentCommandKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentCommandKind {
    /// Expanded by the agent only at the start of a message.
    Command,
    /// A skill, mentionable anywhere.
    Skill,
    /// Bound to the agent's own terminal UI: never offered (Claude `terminal_slash_commands`).
    TerminalOnly,
    /// Answered by the CLI outside a session (agy `-p /model`); the adapter must not forward it.
    SideCommand,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trips_through_json() {
        let env = Envelope::new(Event::ContentDelta {
            stream: StreamKind::Assistant,
            text: "hi".into(),
        })
        .item("m1");
        let json = serde_json::to_string(&env).expect("serialize");
        let back: Envelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(env, back);
    }

    #[test]
    fn unknown_event_types_from_newer_builds_decode_as_unknown() {
        let event: Event = serde_json::from_str(r#"{"type":"from_the_future"}"#).expect("decode");
        assert_eq!(event, Event::Unknown);
    }

    #[test]
    fn background_tasks_keep_their_stored_shape() {
        let env = Envelope::new(Event::BackgroundTasks {
            tasks: vec![BackgroundTask {
                id: "t1".into(),
                kind: BackgroundTaskKind::Agent,
                description: Some("Test agent".into()),
                tool_use_id: Some("toolu_1".into()),
            }],
        });
        let json = serde_json::to_value(&env).expect("serialize");
        assert_eq!(
            json["event"],
            serde_json::json!({"type": "background_tasks", "tasks": [{
                "id": "t1", "kind": "agent", "description": "Test agent", "tool_use_id": "toolu_1"
            }]})
        );
        let back: Envelope = serde_json::from_value(json).expect("deserialize");
        assert_eq!(env, back);
        // A minimal stored task and a kind from a newer build still decode.
        let event: Event = serde_json::from_str(
            r#"{"type":"background_tasks","tasks":[{"id":"t2"},{"id":"t3","kind":"remote_thing"}]}"#,
        )
        .expect("decode");
        let Event::BackgroundTasks { tasks } = event else {
            panic!("not background tasks: {event:?}");
        };
        assert_eq!(tasks[0].kind, BackgroundTaskKind::Other);
        assert_eq!(tasks[0].tool_use_id, None);
        assert_eq!(tasks[1].kind, BackgroundTaskKind::Other);
    }

    #[test]
    fn event_tag_is_snake_case() {
        let json = serde_json::to_value(Event::Unknown).expect("serialize");
        assert_eq!(json["type"], "unknown");
    }
}
