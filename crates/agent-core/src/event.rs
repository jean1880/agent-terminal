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
    RateLimited {
        #[serde(default)]
        resets_at: Option<String>,
        #[serde(default)]
        detail: Option<Value>,
    },
    ModelChanged {
        model: String,
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
    fn event_tag_is_snake_case() {
        let json = serde_json::to_value(Event::Unknown).expect("serialize");
        assert_eq!(json["type"], "unknown");
    }
}
