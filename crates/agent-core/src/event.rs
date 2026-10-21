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

use crate::delegation::identity::ValueProvenance;
use crate::delegation::ids::{MessageId, OperationId, ProfileId, RunId, WorkerId};
use crate::delegation::mailbox::{DeliveryState, Direction, MessageKind};
use crate::delegation::wire::{Controls, DelegationStatus};

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
    /// The state of a reasoning item (`Envelope::item` is its id; its text still arrives as
    /// [`StreamKind::Reasoning`] content). Every agent's reasoning is one block in one of
    /// [`ReasoningState`]'s states; `label` says what the model shares (Claude and agy: its
    /// thinking; Codex: a summary of it).
    Reasoning {
        state: ReasoningState,
        #[serde(default)]
        label: ReasoningLabel,
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
        /// The card's note on its remembering buttons, shown as written: what "Allow for
        /// session" keeps when it is wider than this call, and what "Always allow" would save
        /// (the command line and its folder, a path, an MCP tool), so the user can judge how
        /// broad each is before choosing it. Set by the app's gate; `None` from the adapters.
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
    /// The next turn's mode was selected, while the active turn retains its original policy.
    ModeChangeDeferred {
        requested: crate::adapter::Mode,
        effective: crate::adapter::Mode,
    },
    /// Structured questions (Claude AskUserQuestion, Codex `requestUserInput`), or questions the
    /// app found in a finished reply ([`QuestionSource::Reply`]).
    QuestionRequested {
        questions: Vec<Question>,
        /// Who asked. A reply question is answered with a new prompt, not over the protocol.
        #[serde(default)]
        source: QuestionSource,
    },
    /// A tool server asks the user to fill in a form or open a link (Codex
    /// `mcpServer/elicitation/request`). `Envelope::request` is the id to answer with
    /// [`crate::adapter::Command::SubmitForm`].
    FormRequested {
        /// The MCP server asking, as the agent names it.
        server: String,
        message: String,
        form: crate::form::Form,
    },
    /// The form was answered or withdrawn; the UI closes the card.
    FormResolved {
        action: crate::form::FormAction,
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
    /// Provider quota refusal. `resets_at` is epoch seconds (Claude) or RFC 3339 (agy).
    RateLimited {
        #[serde(default)]
        resets_at: Option<String>,
        #[serde(default)]
        detail: Option<Value>,
    },
    ModelChanged {
        model: String,
    },
    /// A selection is waiting for backend acceptance; it is not yet the running model.
    ModelChangeRequested {
        model: String,
    },
    /// The backend refused a requested selection. Keep the last accepted model.
    ModelChangeFailed {
        model: String,
        message: String,
    },
    /// Full replacement of the agent's running background tasks (sub-agents, shell commands);
    /// empty once the last one ends. A thread with any is still working after its turn ended.
    /// Claude only (`background_tasks_changed`).
    BackgroundTasks {
        tasks: Vec<BackgroundTask>,
    },
    /// Full registry of known provider workers, including finished and explicitly closed ones.
    /// `waiting_for` names workers an open, explicit wait operation currently targets; it is
    /// independent of whether the main turn remains open. Call completion is not worker closure.
    WorkersUpdated {
        #[serde(default)]
        workers: Vec<WorkerSnapshot>,
        #[serde(default)]
        waiting_for: Vec<String>,
    },
    /// Text the CLI produced itself (local slash command output, synthetic messages).
    Notice {
        text: String,
    },
    /// A message between delegated workers. Delegated data: never a `UserMessage` item.
    DelegatedMessage {
        message_id: MessageId,
        direction: Direction,
        kind: MessageKind,
        from: WorkerId,
        to: WorkerId,
        #[serde(default)]
        reply_to: Option<MessageId>,
        text: String,
        delivery: DeliveryState,
    },
    /// An attested native call card (`Envelope::item`) started this worker.
    WorkerLinked {
        worker_id: WorkerId,
        operation_id: OperationId,
    },
    /// The mode the user chose for the thread, which the running mode can differ from (agy without
    /// its hook cannot ask). Stored so a reopened thread, on whichever agent, starts from the
    /// user's choice rather than that agent's profile default. Never shown.
    ModeChosen {
        mode: crate::adapter::Mode,
    },
    /// The app handed this thread's history to a new agent session (a switch, a fork). Stored so
    /// the handoff survives a restart: it is pending until a turn completes after it. Never shown.
    HandoffSeeded {
        /// The fenced, redacted history (`handoff_budget::render_history`).
        summary: String,
        /// The fence the summary uses; the prompt's user marker must use it too.
        fence: String,
        carried: usize,
        total: usize,
        /// Delivery is stored explicitly: only a later `HandoffSeeded` carrying nothing (written
        /// once the turn that carried it completed, or when a switch is rolled back) spends it, never
        /// a completed turn alone (Claude's prompt-less init turn carries nothing). `false` for a
        /// history written before this existed, where a completed turn still counts. Older builds
        /// ignore the field.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        acknowledged: bool,
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

/// Where a reasoning block is in its life ([`Event::Reasoning`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningState {
    /// Text is arriving as the model thinks (Claude `thinking_delta`, Codex summary deltas).
    Streaming,
    /// The whole block is in: streamed and closed, or delivered in one piece (agy reads it from
    /// its transcript; it is never streamed for show).
    Complete,
    /// Shown from stored history, not from the live agent. Set by the view on replay; an
    /// adapter never emits it.
    Replayed,
    /// The model reasoned but did not share it (a signature-only or redacted block, or a CLI
    /// that refused the option that shares it).
    Unavailable,
    /// A state from a newer build.
    #[serde(other)]
    Unknown,
}

/// What a reasoning block holds, which sets its wording.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningLabel {
    /// A summary of its reasoning (Codex reasoning summaries).
    Summary,
    /// The model's own thinking (Claude, agy); also what a label from a newer build reads as.
    #[default]
    #[serde(other)]
    Thinking,
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

/// Where a question card came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionSource {
    /// The app found a question with choices at the end of the agent's finished reply; the
    /// chosen answer is sent as the next prompt.
    Reply,
    /// The agent asked through its question tool; the answer goes back over the protocol. Also
    /// what a source from a newer build reads as.
    #[default]
    #[serde(other)]
    Agent,
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

/// Provider-reported identity and last known state of a worker (not a collaboration tool call).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerSnapshot {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default)]
    pub state: WorkerState,
    /// Latest reported activity or result; absence is unknown, never inferred from silence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    /// A worker agent-terminal supervises or links (cross-agent delegation). `None` for a
    /// provider's own worker, and for every snapshot a v3 build stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<WorkerDelegation>,
}

/// What delegation knows about a worker (`WorkerSummary`, cut to what the thread
/// view shows).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerDelegation {
    pub worker_id: WorkerId,
    pub origin: WorkerOrigin,
    pub driver: crate::adapter::Driver,
    #[serde(default)]
    pub profile_id: Option<ProfileId>,
    #[serde(default)]
    pub parent: Option<WorkerId>,
    #[serde(default)]
    pub run: Option<RunId>,
    pub depth: u8,
    pub model: ValueProvenance,
    pub effort: ValueProvenance,
    pub controls: Controls,
    pub status: DelegationStatus,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    Starting,
    Running,
    Waiting,
    Completed,
    Failed,
    Stopped,
    /// Explicit shutdown/closed signal; a completed task leaves its worker open.
    Closed,
    /// A stop was requested and its outcome is not in yet (delegation: a cancel or revocation
    /// interrupted the run). Older builds read it as `Unknown`.
    Stopping,
    #[default]
    #[serde(other)]
    Unknown,
}

/// Where a worker came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerOrigin {
    /// The user's own session.
    Root,
    /// A provider's own sub-agent, shown but not limited by agent-terminal.
    Native,
    /// Started through `cross_agent_spawn`.
    Supervised,
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
    fn reasoning_events_keep_their_stored_shape_and_tolerate_newer_values() {
        let env = Envelope::new(Event::Reasoning {
            state: ReasoningState::Streaming,
            label: ReasoningLabel::Summary,
        })
        .item("r1");
        let json = serde_json::to_value(&env).expect("serialize");
        assert_eq!(
            json["event"],
            serde_json::json!({"type": "reasoning", "state": "streaming", "label": "summary"})
        );
        let back: Envelope = serde_json::from_value(json).expect("deserialize");
        assert_eq!(env, back);
        let event: Event =
            serde_json::from_str(r#"{"type":"reasoning","state":"pondering","label":"musings"}"#)
                .expect("decode");
        assert_eq!(
            event,
            Event::Reasoning {
                state: ReasoningState::Unknown,
                label: ReasoningLabel::Thinking
            }
        );
        let event: Event = serde_json::from_str(r#"{"type":"reasoning","state":"unavailable"}"#)
            .expect("label defaults");
        assert_eq!(
            event,
            Event::Reasoning {
                state: ReasoningState::Unavailable,
                label: ReasoningLabel::Thinking
            }
        );
    }

    #[test]
    fn event_tag_is_snake_case() {
        let json = serde_json::to_value(Event::Unknown).expect("serialize");
        assert_eq!(json["type"], "unknown");
    }

    #[test]
    fn stopping_and_worker_origin_round_trip() {
        let json = serde_json::to_value(WorkerState::Stopping).expect("ser");
        assert_eq!(json, serde_json::json!("stopping"));
        assert_eq!(
            serde_json::from_value::<WorkerState>(json).expect("de"),
            WorkerState::Stopping
        );
        for (origin, name) in [
            (WorkerOrigin::Root, "root"),
            (WorkerOrigin::Native, "native"),
            (WorkerOrigin::Supervised, "supervised"),
        ] {
            let json = serde_json::to_value(origin).expect("ser");
            assert_eq!(json, serde_json::json!(name));
            assert_eq!(
                serde_json::from_value::<WorkerOrigin>(json).expect("de"),
                origin
            );
        }
    }

    #[test]
    fn a_v3_worker_snapshot_without_delegation_still_decodes() {
        let snap: WorkerSnapshot =
            serde_json::from_str(r#"{"id":"a","name":"n","state":"running"}"#).expect("decode");
        assert_eq!(snap.delegation, None);
        assert_eq!(snap.state, WorkerState::Running);
        let json = serde_json::to_value(&snap).expect("ser");
        assert!(json.get("delegation").is_none(), "absent, as v3 wrote it");
    }

    #[test]
    fn worker_delegation_round_trips() {
        use crate::delegation::identity::{Provenance, ValueProvenance};
        use crate::delegation::wire::{DelegationStatusCode, ThisRunSupport};
        let w = |s: &str| WorkerId::parse(s).expect("valid");
        let snap = WorkerSnapshot {
            id: "w_childaaa".into(),
            name: Some("reviewer".into()),
            task: None,
            state: WorkerState::Stopping,
            activity: None,
            delegation: Some(WorkerDelegation {
                worker_id: w("w_childaaa"),
                origin: WorkerOrigin::Supervised,
                driver: crate::adapter::Driver::Codex,
                profile_id: Some(ProfileId::parse("codex").expect("valid")),
                parent: Some(w("w_rootroot")),
                run: Some(RunId::parse("r_runrunru").expect("valid")),
                depth: 1,
                model: ValueProvenance {
                    requested: Some("gpt-5".into()),
                    effective: None,
                    source: Provenance::ProviderAccepted,
                },
                effort: ValueProvenance::unknown(),
                controls: Controls {
                    send: true,
                    this_run: ThisRunSupport::Strict,
                    cancel: true,
                    close: true,
                },
                status: DelegationStatus::new(DelegationStatusCode::Available),
            }),
        };
        let json = serde_json::to_string(&snap).expect("ser");
        let back: WorkerSnapshot = serde_json::from_str(&json).expect("de");
        assert_eq!(back, snap);
    }

    /// The `Event` shape of a build that predates delegation: same tag, no delegation
    /// variants, `#[serde(other)]` catch-all.
    #[derive(Debug, PartialEq, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum OlderEvent {
        Notice {
            text: String,
        },
        #[serde(other)]
        Unknown,
    }

    #[test]
    fn delegation_events_round_trip_and_older_builds_read_them_as_unknown() {
        let w = |s: &str| WorkerId::parse(s).expect("valid");
        let message = Envelope::new(Event::DelegatedMessage {
            message_id: MessageId::parse("m_msgmsgms").expect("valid"),
            direction: Direction::ChildToParent,
            kind: MessageKind::Report,
            from: w("w_childaaa"),
            to: w("w_rootroot"),
            reply_to: None,
            text: "done".into(),
            delivery: DeliveryState::ProviderConfirmed,
        });
        let linked = Envelope::new(Event::WorkerLinked {
            worker_id: w("w_childaaa"),
            operation_id: OperationId::parse("op_opopopop").expect("valid"),
        })
        .item("call-card-1");
        for env in [message, linked] {
            let json = serde_json::to_string(&env).expect("ser");
            let back: Envelope = serde_json::from_str(&json).expect("de");
            assert_eq!(back, env);
            let event_json = serde_json::to_string(&env.event).expect("ser");
            let older: OlderEvent = serde_json::from_str(&event_json).expect("older build");
            assert_eq!(older, OlderEvent::Unknown);
        }
        let older: OlderEvent =
            serde_json::from_str(r#"{"type":"notice","text":"t"}"#).expect("older notice");
        assert_eq!(older, OlderEvent::Notice { text: "t".into() });
        let json = serde_json::to_value(Event::WorkerLinked {
            worker_id: w("w_childaaa"),
            operation_id: OperationId::parse("op_opopopop").expect("valid"),
        })
        .expect("ser");
        assert_eq!(json["type"], "worker_linked");
    }

    #[test]
    fn worker_snapshot_defaults_missing_or_future_state_to_unknown() {
        let event: Event = serde_json::from_str(
            r#"{"type":"workers_updated","workers":[{"id":"a"},{"id":"b","state":"future"}]}"#,
        )
        .expect("decode");
        let Event::WorkersUpdated {
            workers,
            waiting_for,
        } = event
        else {
            panic!("workers");
        };
        assert!(waiting_for.is_empty());
        assert!(workers.iter().all(|w| w.state == WorkerState::Unknown
            && w.name.is_none()
            && w.task.is_none()
            && w.activity.is_none()));
    }
}
