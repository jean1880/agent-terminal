//! The transcript model: a pure reducer from canonical [`Envelope`]s to transcript items.
//!
//! No GTK here. The widget layer applies an envelope, gets back the list of [`Change`]s, and
//! updates (or creates) only the rows those name. Expanded/collapsed state lives on the
//! [`Item`], so it survives a row being evicted from the materialised window and rebuilt.

use std::collections::HashMap;

use agent_core::adapter::{Driver, Mode};
use agent_core::event::{
    BackgroundTask, BackgroundTaskKind, Decision, Envelope, Event, ItemKind, ItemStatus, PlanStep,
    Question, QuestionSource, ReasoningLabel, ReasoningState, ResponseCapability, StepStatus,
    StreamKind, TurnState, WorkerSnapshot, WorkerState,
};
use agent_core::form::{self, Form, FormAction};
use serde_json::Value;

pub type ItemId = String;

/// One transcript row (or a row nested inside a subagent card).
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: ItemId,
    pub body: Body,
    /// Expanded/collapsed state of a card or the reasoning row.
    pub expanded: bool,
    pub parent: Option<ItemId>,
    pub children: Vec<ItemId>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    User {
        text: String,
        queued: bool,
    },
    Assistant {
        text: String,
        driver: Driver,
        streaming: bool,
    },
    Reasoning {
        text: String,
        streaming: bool,
        /// What the adapter last said of the block ([`Event::Reasoning`]), or `Replayed` once
        /// the view loaded it from history. `None`: the adapter said nothing (Codex, until it
        /// maps its summaries; older history), so it follows `streaming`.
        state: Option<ReasoningState>,
        label: ReasoningLabel,
    },
    Tool(Tool),
    Notice {
        text: String,
        tone: Tone,
    },
    Compaction {
        manual: bool,
        before: u64,
        after: Option<u64>,
    },
    TurnError {
        message: String,
    },
    /// A provider or model boundary: an inline banner in the new agent's accent colour.
    Switch {
        driver: Driver,
        model: Option<String>,
        agent_changed: bool,
        /// The thread was on this agent before: a new session from a summary, not a resume.
        returned: bool,
    },
    Approval(Approval),
    Question(QuestionCard),
    /// A form or link an agent's tool server asked the user for. Never holds an answer.
    Form(FormCard),
    /// Quick replies under the reply that ended with a question. Not an interruption: it is
    /// never a [`PendingInterruption`], and nothing waits for it.
    QuickReplies(QuickReplies),
    /// The provider's last step statuses, retained in history after the active tracker retires.
    Plan {
        steps: Vec<PlanStep>,
        outcome: Option<TurnState>,
    },
}

impl Body {
    /// A reasoning block before anything is known of it.
    pub(super) fn new_reasoning() -> Self {
        Self::Reasoning {
            text: String::new(),
            streaming: true,
            state: None,
            label: ReasoningLabel::Thinking,
        }
    }
}

/// How the one reasoning widget shows a block: the four states of the UX contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasoningView {
    pub state: ReasoningState,
    pub title: &'static str,
    /// A tooltip saying where the text came from, when the title does not.
    pub hint: Option<&'static str>,
    /// Shown at all. A block still waiting for its first words takes no space, so the
    /// transcript does not jump; the header's "Working…" says the agent is busy.
    pub visible: bool,
    /// It has text to open.
    pub expandable: bool,
}

/// The view of a reasoning block (pure; the widget only applies it).
pub fn reasoning_view(
    text: &str,
    streaming: bool,
    state: Option<ReasoningState>,
    label: ReasoningLabel,
) -> ReasoningView {
    let has_text = !text.trim().is_empty();
    let state = match state {
        // A live block that closed: the adapter's last word stands unless it was cut off.
        Some(ReasoningState::Streaming) if !streaming => ReasoningState::Complete,
        Some(ReasoningState::Unknown) | None if streaming => ReasoningState::Streaming,
        Some(ReasoningState::Unknown) | None => ReasoningState::Complete,
        Some(s) => s,
    };
    // Text that did arrive is shown, whatever was said before it.
    let state = if state == ReasoningState::Unavailable && has_text {
        ReasoningState::Complete
    } else {
        state
    };
    let summary = label == ReasoningLabel::Summary;
    let (title, hint) = match state {
        ReasoningState::Streaming if summary => ("Summarizing reasoning…", None),
        ReasoningState::Streaming => ("Thinking…", None),
        ReasoningState::Complete | ReasoningState::Unknown if summary => {
            ("Reasoning summary", None)
        }
        ReasoningState::Complete | ReasoningState::Unknown => ("Thought process", None),
        ReasoningState::Replayed if summary => (
            "Reasoning summary",
            Some("Replayed from this thread's history"),
        ),
        ReasoningState::Replayed => (
            "Thought process",
            Some("Replayed from this thread's history"),
        ),
        ReasoningState::Unavailable => (
            "Thinking not shared by this model",
            Some("The model reasoned before answering but did not share its reasoning"),
        ),
    };
    ReasoningView {
        state,
        title,
        hint,
        // An explicit "not shared" is worth a line; any other empty block is not.
        visible: has_text || state == ReasoningState::Unavailable,
        expandable: has_text,
    }
}

/// The agent a command card launches, read from its input (structured, else the streamed text
/// once it parses); `None` for anything else.
pub(super) fn shell_launch(tool: &Tool) -> Option<agent_core::agent_command::AgentCommand> {
    if tool.kind != ItemKind::Command {
        return None;
    }
    let input = tool
        .input
        .clone()
        .filter(|i| i.as_object().is_some_and(|o| !o.is_empty()))
        .or_else(|| serde_json::from_str(&tool.input_text).ok())?;
    agent_core::agent_command::from_command_input(&input)
}

/// Empty plans and wholly completed plans do not occupy the active-work shelf.
pub(super) fn plan_is_active(steps: &[PlanStep]) -> bool {
    steps
        .iter()
        .any(|step| step.status != StepStatus::Completed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Info,
    Warning,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub kind: ItemKind,
    pub title: String,
    /// Structured input when the agent gave one up front.
    pub input: Option<Value>,
    /// Streamed input text (tool arguments arriving as JSON fragments).
    pub input_text: String,
    pub output: String,
    pub error: Option<String>,
    pub status: ToolStatus,
    /// A sub-agent's model as its provider reported it ([`Event::SubagentModel`]).
    pub reported_model: Option<String>,
}

/// One sub-agent, as the explorer lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentSummary {
    pub id: ItemId,
    /// The kind of agent (`Explore`, `general-purpose`…), else the tool's title.
    pub name: String,
    /// What it was asked to do: the one-line description, else the prompt's first line.
    pub task: String,
    pub status: ToolStatus,
    /// How many steps (direct children) it has taken so far.
    pub steps: usize,
    /// Provider-confirmed worker lifecycle; legacy nested tool calls have no separate lifecycle.
    pub lifecycle: Option<WorkerState>,
    /// Latest provider-reported activity, which is not necessarily still running.
    pub activity: Option<String>,
    /// The provider is known independently of model/effort, which may remain unknown.
    pub driver: Option<Driver>,
    pub delegation: Option<agent_core::event::WorkerDelegation>,
    /// The model and effort the parent asked for when it started the sub-agent (Claude's
    /// `Agent` input, a shell launch's `--model`): requested, never confirmed by the provider.
    pub requested_model: Option<String>,
    pub requested_effort: Option<String>,
    /// The model the provider says the sub-agent runs on, once it has said so.
    pub reported_model: Option<String>,
}

impl SubagentSummary {
    /// What it was asked to do, or a line saying it did not say.
    pub fn task_text(&self) -> &str {
        if self.task.is_empty() {
            "No task reported"
        } else {
            &self.task
        }
    }

    pub fn active(&self) -> bool {
        match self.lifecycle {
            Some(state) => matches!(
                state,
                // Stopping is still active: only an observed exit ends a worker.
                WorkerState::Starting
                    | WorkerState::Running
                    | WorkerState::Waiting
                    | WorkerState::Stopping
            ),
            None => self.status == ToolStatus::Running,
        }
    }

    /// Whether it is at work right now. A delegated worker between runs (its last turn done or
    /// interrupted, waiting for its parent's next message) is active but not running: only a
    /// worker with a current run is.
    pub fn running(&self) -> bool {
        match &self.delegation {
            Some(delegation) if self.lifecycle.is_some() => {
                delegation.run.is_some() && self.active()
            }
            _ => self.active(),
        }
    }

    pub fn finished(&self) -> bool {
        match self.lifecycle {
            Some(WorkerState::Unknown) => false,
            Some(_) => !self.active(),
            None => self.status != ToolStatus::Running,
        }
    }

    pub fn state_label(&self) -> &'static str {
        match self.lifecycle {
            Some(WorkerState::Starting) => "starting",
            Some(WorkerState::Running) => "running",
            Some(WorkerState::Waiting) => "waiting",
            Some(WorkerState::Completed) => "completed",
            Some(WorkerState::Failed) => "failed",
            Some(WorkerState::Stopped) => "stopped",
            Some(WorkerState::Closed) => "closed",
            Some(WorkerState::Stopping) => "stopping",
            Some(WorkerState::Unknown) => "status unknown",
            None => match self.status {
                ToolStatus::Running => "running",
                ToolStatus::Completed => "completed",
                ToolStatus::Failed => "failed",
                ToolStatus::Declined => "declined",
                ToolStatus::Interrupted => "stopped",
            },
        }
    }

    /// A shell command that launches an agent (`cross-agent agy …`, `claude -p …`): a sub-agent
    /// no delegation tool announced, on the agent it launched. `None` for any other command.
    fn of_shell(item: &Item, tool: &Tool) -> Option<Self> {
        let launch = shell_launch(tool)?;
        let label = launch.driver.info().label;
        let name = match &launch.model {
            Some(model) => format!("{label} · {model} (shell)"),
            None => format!("{label} (shell)"),
        };
        Some(Self {
            id: item.id.clone(),
            name,
            task: launch
                .task
                .as_deref()
                .and_then(|task| task.lines().map(str::trim).find(|l| !l.is_empty()))
                .map_or_else(|| tool.title.clone(), str::to_owned),
            status: tool.status,
            steps: 0,
            lifecycle: None,
            activity: None,
            driver: Some(launch.driver),
            delegation: None,
            requested_model: launch.model,
            requested_effort: None,
            reported_model: tool.reported_model.clone(),
        })
    }

    fn of(item: &Item, tool: &Tool) -> Self {
        // A live Claude sub-agent's input is not known when its card starts: it streams in as
        // JSON text after. Read the structured input when there is one, else the streamed text,
        // whole once it parses and field by field while it is still arriving.
        let streamed: Option<Value> = tool
            .input
            .as_ref()
            .filter(|i| i.as_object().is_some_and(|o| !o.is_empty()))
            .cloned()
            .or_else(|| serde_json::from_str(&tool.input_text).ok());
        let field = |key: &str| {
            streamed
                .as_ref()
                .and_then(|i| i.get(key))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| partial_string_field(&tool.input_text, key))
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        };
        let name = field("subagent_type")
            .or_else(|| field("agent"))
            .unwrap_or_else(|| tool.title.clone());
        let task = field("description")
            .or_else(|| field("prompt").and_then(|p| p.lines().next().map(str::to_owned)))
            .unwrap_or_default();
        Self {
            id: item.id.clone(),
            name,
            task,
            status: tool.status,
            steps: item.children.len(),
            lifecycle: streamed
                .as_ref()
                .and_then(|i| i.get("worker_state"))
                .and_then(|v| serde_json::from_value(v.clone()).ok()),
            activity: field("activity"),
            driver: None,
            delegation: streamed
                .as_ref()
                .and_then(|input| input.get("delegation"))
                .and_then(|value| serde_json::from_value(value.clone()).ok()),
            requested_model: field("model"),
            requested_effort: field("effort"),
            reported_model: tool.reported_model.clone(),
        }
    }
}

/// The string value of `"key":` in JSON that may still be arriving (cut anywhere). Only a value
/// whose closing quote has arrived counts, so a half-streamed description is never shown.
fn partial_string_field(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let after = &text[text.find(&needle)? + needle.len()..];
    let after = after.trim_start().strip_prefix(':')?.trim_start();
    let body = after.strip_prefix('"')?;
    // Re-parse the quoted string with serde, up to its closing (unescaped) quote.
    let mut escaped = false;
    for (i, c) in body.char_indices() {
        match c {
            '\\' if !escaped => escaped = true,
            '"' if !escaped => {
                return serde_json::from_str::<String>(&format!("\"{}\"", &body[..i])).ok();
            }
            _ => escaped = false,
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Completed,
    Failed,
    Declined,
    Interrupted,
}

impl From<ItemStatus> for ToolStatus {
    fn from(s: ItemStatus) -> Self {
        match s {
            ItemStatus::Completed => Self::Completed,
            ItemStatus::Failed => Self::Failed,
            ItemStatus::Declined => Self::Declined,
            ItemStatus::Interrupted => Self::Interrupted,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Approval {
    pub request: String,
    pub tool: String,
    pub title: Option<String>,
    pub input: Value,
    pub reason: Option<String>,
    pub options: Vec<Decision>,
    pub state: ApprovalState,
    /// What "Allow for session" and "Always allow" keep, shown on the card as written.
    pub remembers: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalState {
    Pending,
    /// The user chose; the agent has not confirmed yet.
    Sent(Decision),
    Resolved(Decision),
    Expired,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuestionCard {
    pub request: String,
    pub questions: Vec<Question>,
    pub state: QuestionState,
}

/// The choices a reply ended with ([`agent_core::event::QuestionSource::Reply`]).
#[derive(Debug, Clone, PartialEq)]
pub struct QuickReplies {
    /// `reply:<assistant item id>`.
    pub request: String,
    /// Each option as rendered plain text: exactly what a chip puts in the composer.
    pub options: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionState {
    Pending,
    Sent,
    Answered,
    Withdrawn,
    /// Never answered, and the agent that asked is gone: it can no longer be answered.
    Expired,
}

/// A form (or link) a tool server asked for. What the user typed is never kept here: the card
/// sends it and only the outcome is stored.
#[derive(Debug, Clone, PartialEq)]
pub struct FormCard {
    pub request: String,
    /// The tool server asking, as the agent names it.
    pub server: String,
    pub message: String,
    pub form: Form,
    pub state: FormState,
    /// The agent refused the last answer: the card is open again, and says so.
    pub refused: bool,
}

/// Whether a form can be shown and answered: fields that pass the checks, or a link that may be
/// opened. A form that cannot be is closed on arrival and raises no badge or notification.
pub fn form_answerable(form: &Form) -> bool {
    match form {
        Form::Fields { fields } => form::check_form(fields).is_ok(),
        Form::Link { url } => form::openable_link(url),
        Form::Unknown => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormState {
    Pending,
    /// The user answered; the agent has not confirmed yet.
    Sent,
    Resolved(FormAction),
    /// Never answered, and the agent that asked is gone.
    Expired,
    /// A form this build cannot show or answer (a newer kind, or one that fails the checks).
    Unanswerable,
}

/// An unhandled approval or question requiring operator attention.
#[derive(Debug, Clone, PartialEq)]
pub enum PendingInterruption {
    Approval {
        request: String,
        item_id: ItemId,
        tool: String,
        title: Option<String>,
        input: Value,
        options: Vec<Decision>,
    },
    Question {
        request: String,
        item_id: ItemId,
        questions: Vec<Question>,
    },
    Form {
        request: String,
        item_id: ItemId,
        server: String,
        message: String,
        /// A link to open rather than fields to fill in.
        link: bool,
    },
}

/// Context-window occupancy for the header gauge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gauge {
    pub used: u64,
    pub max: Option<u64>,
    pub auto_compact_at: Option<u64>,
}

/// What the thread is doing, counting its background work as well as the main agent's turn: the
/// one status the view shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activity {
    /// Nothing has run yet (a new thread): nothing to say.
    Idle,
    /// The main agent's turn runs; `background` more tasks run alongside it.
    Working {
        background: usize,
    },
    Thinking,
    Tool {
        title: String,
        background: usize,
    },
    WaitingForWorkers {
        names: Vec<String>,
    },
    NeedsApproval,
    NeedsAnswer,
    Failed,
    Stopped,
    /// The main agent's turn is over, but it is still waiting on these background tasks.
    Waiting {
        tasks: Vec<String>,
    },
    /// A turn ran, and neither it nor any background work is still going.
    Finished,
    /// The agent process is starting and has not answered yet.
    Starting,
    /// The agent, paused to save memory, is starting again for the message just sent.
    Resuming,
    /// The agent is paused to save memory; the next message resumes it.
    Paused,
}

impl Activity {
    /// What the header shows given where the agent process is in its life: a start (or a
    /// resume) in progress says so over anything else, since nothing the thread does can show
    /// until the agent is up; a paused agent says so unless something still runs or waits.
    pub fn with_lifecycle(self, lifecycle: crate::chat::Lifecycle) -> Self {
        if lifecycle.starting {
            return if lifecycle.resuming {
                Self::Resuming
            } else {
                Self::Starting
            };
        }
        let waits = matches!(self, Self::NeedsApproval | Self::NeedsAnswer);
        if lifecycle.suspended && !self.busy() && !waits {
            return Self::Paused;
        }
        self
    }

    /// The thread's status from the main turn and the background task list. `ran`: a turn has
    /// run in this thread (so an idle one has finished rather than never started).
    pub fn of(main_running: bool, background: &[BackgroundTask], ran: bool) -> Self {
        if main_running {
            Self::Working {
                background: background.len(),
            }
        } else if !background.is_empty() {
            Self::Waiting {
                tasks: background.iter().map(task_name).collect(),
            }
        } else if ran {
            Self::Finished
        } else {
            Self::Idle
        }
    }

    /// What the header says; `None` shows nothing.
    pub fn text(&self) -> Option<String> {
        match self {
            Self::Idle => None,
            Self::Working { background: 0 } => Some("Working…".to_owned()),
            Self::Working { background: n } => {
                Some(format!("Working…, plus {n} in the background"))
            }
            Self::Thinking => Some("Thinking…".to_owned()),
            Self::Tool { title, background } => Some(if *background == 0 {
                format!("Running {title}…")
            } else {
                format!("Running {title} · {background} background tasks active")
            }),
            Self::WaitingForWorkers { names } => Some(format!(
                "Waiting for {}…",
                names
                    .iter()
                    .take(STATUS_TASKS)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            Self::NeedsApproval => Some("Needs approval".to_owned()),
            Self::NeedsAnswer => Some("Needs your answer".to_owned()),
            Self::Failed => Some("Failed".to_owned()),
            Self::Stopped => Some("Stopped".to_owned()),
            Self::Waiting { tasks } => {
                let count = match tasks.len() {
                    1 => "1 background task".to_owned(),
                    n => format!("{n} background tasks"),
                };
                // The first few names; the header is one line.
                let mut names = tasks
                    .iter()
                    .take(STATUS_TASKS)
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                if tasks.len() > STATUS_TASKS {
                    names.push_str(&format!(" and {} more", tasks.len() - STATUS_TASKS));
                }
                Some(format!("Main agent done, waiting on {count}: {names}"))
            }
            Self::Finished => Some("Completed".to_owned()),
            Self::Starting => Some("Starting…".to_owned()),
            Self::Resuming => Some("Resuming…".to_owned()),
            Self::Paused => Some("Paused: resumes when you send".to_owned()),
        }
    }

    /// Whether a spinner turns: something is still running.
    pub fn busy(&self) -> bool {
        matches!(
            self,
            Self::Working { .. }
                | Self::Thinking
                | Self::Tool { .. }
                | Self::WaitingForWorkers { .. }
                | Self::Waiting { .. }
                | Self::Starting
                | Self::Resuming
        )
    }
}

/// Most background task names the status line lists before "and N more".
const STATUS_TASKS: usize = 3;

/// A background task as the status names it: its description's first line, else what kind of
/// task it is.
pub fn task_name(task: &BackgroundTask) -> String {
    let first_line = |d: &str| {
        d.lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or_default()
            .to_owned()
    };
    match task.description.as_deref().map(first_line).as_deref() {
        Some(d) if !d.is_empty() => d.to_owned(),
        _ => match task.kind {
            BackgroundTaskKind::Agent => "a sub-agent",
            BackgroundTaskKind::Shell => "a shell command",
            BackgroundTaskKind::Other => "a task",
        }
        .to_owned(),
    }
}

/// What an [`Transcript::apply`] call changed, for the widget layer.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// A new item; nested when `Item::parent` is set.
    Added(ItemId),
    /// An existing item's content or state changed.
    Updated(ItemId),
    Plan,
    Gauge,
    Mode,
    /// What the thread is doing changed: its turn, or its background work.
    Running,
    Workers,
    Commands,
    /// An existing item was removed.
    Removed(ItemId),
    /// A control reply, routed by request id to whoever asked.
    Control {
        request: String,
        result: Result<Value, String>,
    },
}

#[derive(Debug, Default)]
pub struct Transcript {
    items: HashMap<ItemId, Item>,
    /// Top-level items in display order (nested items live in their parent's `children`).
    order: Vec<ItemId>,
    /// Number of queued user items pinned at the tail of `order`.
    queued_count: usize,
    /// Approval/question request id → item id.
    requests: HashMap<String, ItemId>,
    pub plan: Vec<PlanStep>,
    /// The history row currently being refined by provider plan updates.
    plan_item: Option<ItemId>,
    pub gauge: Option<Gauge>,
    pub mode: Option<Mode>,
    pub pending_mode: Option<Mode>,
    pub running: bool,
    /// Background work still going (sub-agents, background commands), as the agent last listed
    /// it: the main turn may be over while these run.
    pub background: Vec<BackgroundTask>,
    /// Synthetic worker items live in `items` for detail panels, outside transcript order.
    worker_ids: Vec<ItemId>,
    waiting_for: Vec<ItemId>,
    last_outcome: Option<TurnState>,
    /// A turn has run in this thread.
    ran: bool,
    last_driver: Option<Driver>,
    /// Every agent the thread has been on, so a switch back can say it is not a resume.
    seen_drivers: Vec<Driver>,
    last_model: Option<String>,
    /// An agent-switch divider drawn before the new agent named its model (its switch notices
    /// come first): the next model it reports goes in it.
    unnamed_switch: Option<ItemId>,
    /// The assistant/reasoning item that item-less deltas belong to.
    open_text: Option<ItemId>,
    /// What the user allowed Codex for the rest of its session (permission requests answered
    /// "Allow for this session"), one line each: the header's "Extra access" chip. Cleared when
    /// the session restarts or exits, which is when Codex forgets the grant.
    extra_access: Vec<String>,
    seq: u64,
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, id: &str) -> Option<&Item> {
        self.items.get(id)
    }

    #[allow(dead_code)] // kept as API; exercised by tests
    pub fn get_mut(&mut self, id: &str) -> Option<&mut Item> {
        self.items.get_mut(id)
    }

    /// Top-level items in display order.
    pub fn order(&self) -> &[ItemId] {
        &self.order
    }

    pub(super) fn all_item_ids(&self) -> Vec<String> {
        self.items.keys().cloned().collect()
    }

    /// User prompts in display order, for the composer's per-thread recall history. The prompt a
    /// busy switch sent for the user is not theirs, so it is left out.
    pub fn user_messages(&self) -> Vec<String> {
        self.order
            .iter()
            .filter_map(|id| self.items.get(id))
            .filter_map(|item| match &item.body {
                Body::User { text, .. } if !text.is_empty() && !is_switch_continuation(text) => {
                    Some(text.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// Shows the choices a reply ended with, replacing any chips still showing. Nothing pends on
    /// them: they are no [`PendingInterruption`], no request is registered, and a question
    /// without options offers nothing.
    fn offer_quick_replies(
        &mut self,
        request: Option<&str>,
        questions: &[Question],
    ) -> Vec<Change> {
        let mut out = self.dismiss_quick_replies();
        let options: Vec<String> = questions
            .first()
            .map(|q| q.options.iter().map(|o| o.label.clone()).collect())
            .unwrap_or_default();
        if options.is_empty() {
            return out;
        }
        let request = request.map_or_else(|| self.next_id("reply"), str::to_owned);
        let id = format!("replies:{request}");
        let body = Body::QuickReplies(QuickReplies { request, options });
        out.push(Change::Added(self.insert(id, body, None)));
        out
    }

    /// Takes the quick-reply chips away (the user sent a prompt, a turn began, or they chose
    /// Dismiss).
    pub fn dismiss_quick_replies(&mut self) -> Vec<Change> {
        let ids: Vec<ItemId> = self
            .order
            .iter()
            .filter(|id| {
                self.items
                    .get(*id)
                    .is_some_and(|i| matches!(i.body, Body::QuickReplies(_)))
            })
            .cloned()
            .collect();
        ids.iter().flat_map(|id| self.remove_item(id)).collect()
    }

    /// Whether quick-reply chips are showing.
    #[cfg(test)]
    fn has_quick_replies(&self) -> bool {
        self.items
            .values()
            .any(|i| matches!(i.body, Body::QuickReplies(_)))
    }

    /// Removes an item from the transcript (e.g. popping a queued user message).
    pub fn remove_item(&mut self, id: &str) -> Vec<Change> {
        if let Some(item) = self.items.remove(id) {
            if matches!(&item.body, Body::User { queued: true, .. }) {
                self.queued_count = self.queued_count.saturating_sub(1);
            }
            self.order.retain(|i| i != id);
            vec![Change::Removed(id.to_owned())]
        } else {
            Vec::new()
        }
    }

    /// Returns the most recent unhandled approval or question requiring operator action, if any.
    pub fn pending_interruption(&self) -> Option<PendingInterruption> {
        for id in self.order.iter().rev() {
            if let Some(item) = self.items.get(id) {
                match &item.body {
                    Body::Approval(a) if a.state == ApprovalState::Pending => {
                        return Some(PendingInterruption::Approval {
                            request: a.request.clone(),
                            item_id: id.clone(),
                            tool: a.tool.clone(),
                            title: a.title.clone(),
                            input: a.input.clone(),
                            options: a.options.clone(),
                        });
                    }
                    Body::Question(q) if q.state == QuestionState::Pending => {
                        return Some(PendingInterruption::Question {
                            request: q.request.clone(),
                            item_id: id.clone(),
                            questions: q.questions.clone(),
                        });
                    }
                    Body::Form(f) if f.state == FormState::Pending => {
                        return Some(PendingInterruption::Form {
                            request: f.request.clone(),
                            item_id: id.clone(),
                            server: f.server.clone(),
                            message: f.message.clone(),
                            link: matches!(f.form, Form::Link { .. }),
                        });
                    }
                    _ => {}
                }
            }
        }
        None
    }

    /// How many approvals, questions and forms wait on the user (the shelf shows the newest).
    pub fn pending_count(&self) -> usize {
        self.order
            .iter()
            .filter_map(|id| self.items.get(id))
            .filter(|item| match &item.body {
                Body::Approval(a) => a.state == ApprovalState::Pending,
                Body::Question(q) => q.state == QuestionState::Pending,
                Body::Form(f) => f.state == FormState::Pending,
                _ => false,
            })
            .count()
    }

    /// Whether `id` is a sub-agent or one of its steps (at any depth): what the explorer shows.
    pub fn in_subagent(&self, id: &str) -> bool {
        let mut next = Some(id);
        while let Some(id) = next {
            let Some(item) = self.items.get(id) else {
                return false;
            };
            if matches!(&item.body, Body::Tool(t) if t.kind == ItemKind::Subagent) {
                return true;
            }
            next = item.parent.as_deref();
        }
        false
    }

    /// Every sub-agent the thread started, nested ones and agents its shell commands launched
    /// included, in the order they began.
    pub fn subagents(&self) -> Vec<SubagentSummary> {
        let mut out = Vec::new();
        let mut stack: Vec<&ItemId> = self.order.iter().rev().collect();
        while let Some(id) = stack.pop() {
            let Some(item) = self.items.get(id) else {
                continue;
            };
            if let Body::Tool(t) = &item.body {
                if t.kind == ItemKind::Subagent {
                    out.push(SubagentSummary::of(item, t));
                } else if let Some(shell) = SubagentSummary::of_shell(item, t) {
                    out.push(shell);
                }
            }
            stack.extend(item.children.iter().rev());
        }
        for id in &self.worker_ids {
            if let Some(item) = self.items.get(id) {
                if let Body::Tool(tool) = &item.body {
                    out.push(SubagentSummary::of(item, tool));
                }
            }
        }
        for worker in &mut out {
            worker.driver = worker
                .delegation
                .as_ref()
                .map(|delegation| delegation.driver)
                .or(worker.driver)
                .or(self.last_driver);
        }
        out
    }

    #[allow(dead_code)] // kept as API; exercised by tests
    pub fn len(&self) -> usize {
        self.order.len()
    }

    #[allow(dead_code)] // kept as API; exercised by tests
    pub fn item_for_request(&self, request: &str) -> Option<&ItemId> {
        self.requests.get(request)
    }

    /// Extra access the user granted Codex for its session, one line each (empty: none).
    pub fn extra_access(&self) -> &[String] {
        &self.extra_access
    }

    pub fn current_model(&self) -> Option<&str> {
        self.last_model.as_deref()
    }

    /// Every sub-agent of the thread whatever started it: [`Self::subagents`], plus `supervised`,
    /// the delegation registry's workers. Registry handles are canonical; a linked snapshot of a
    /// supervised worker in this stream must not make a second row for it.
    #[cfg(test)]
    pub fn subagents_with(&self, supervised: Vec<SubagentSummary>) -> Vec<SubagentSummary> {
        Self::with_supervised(self.subagents(), supervised)
    }

    /// `own` ([`Self::subagents`], computed once by a caller that needs it too) with
    /// `supervised` merged in as [`Self::subagents_with`] does.
    pub fn with_supervised(
        mut agents: Vec<SubagentSummary>,
        supervised: Vec<SubagentSummary>,
    ) -> Vec<SubagentSummary> {
        agents.retain(|agent| {
            !agent.delegation.as_ref().is_some_and(|delegation| {
                supervised
                    .iter()
                    .any(|worker| worker.id == delegation.worker_id.as_str())
            })
        });
        agents.extend(supervised);
        agents
    }

    /// The agents the thread's running sub-agents are on (any route: the agent's own, the
    /// provider's workers, the delegation registry's `supervised`), each once, in slot order.
    #[cfg(test)]
    pub fn running_drivers(&self, supervised: Vec<SubagentSummary>) -> Vec<Driver> {
        Self::running_drivers_among(&self.subagents_with(supervised))
    }

    /// [`Self::running_drivers`] of `agents`, a [`Self::subagents_with`] the caller already has.
    pub fn running_drivers_among(agents: &[SubagentSummary]) -> Vec<Driver> {
        Driver::ALL
            .into_iter()
            .filter(|&driver| {
                agents
                    .iter()
                    .any(|agent| agent.running() && agent.driver == Some(driver))
            })
            .collect()
    }

    /// The thread's status. `main_running`: the main agent's turn runs (the model's own view of
    /// it, or the backend's). The view counts delegated workers too ([`Self::activity_with`]).
    #[cfg(test)]
    pub fn activity(&self, main_running: bool) -> Activity {
        self.activity_with(main_running, Vec::new())
    }

    /// [`Self::activity`], counting `supervised` (the delegation registry's workers) as the
    /// thread's sub-agents: a parent whose turn is over while a delegated worker runs is waiting
    /// on it, not finished.
    #[cfg(test)]
    pub fn activity_with(&self, main_running: bool, supervised: Vec<SubagentSummary>) -> Activity {
        self.activity_among(main_running, &self.subagents_with(supervised))
    }

    /// [`Self::activity_with`], given `agents`: a [`Self::subagents_with`] the caller already
    /// has, so a status refresh walks the items once.
    pub fn activity_among(&self, main_running: bool, agents: &[SubagentSummary]) -> Activity {
        if let Some(pending) = self.pending_interruption() {
            return match pending {
                PendingInterruption::Approval { .. } => Activity::NeedsApproval,
                PendingInterruption::Question { .. } | PendingInterruption::Form { .. } => {
                    Activity::NeedsAnswer
                }
            };
        }
        if main_running && !self.waiting_for.is_empty() {
            return Activity::WaitingForWorkers {
                names: self
                    .waiting_for
                    .iter()
                    .map(|id| {
                        agents
                            .iter()
                            .find(|a| &a.id == id)
                            .map(|a| a.name.clone())
                            .unwrap_or_else(|| "a sub-agent".to_owned())
                    })
                    .collect(),
            };
        }
        let background = self.background_with(agents);
        if main_running {
            for id in self.order.iter().rev() {
                match self.items.get(id).map(|item| &item.body) {
                    Some(Body::Tool(tool))
                        if tool.status == ToolStatus::Running
                            && tool.kind != ItemKind::Subagent =>
                    {
                        return Activity::Tool {
                            title: tool.title.clone(),
                            background: background.len(),
                        };
                    }
                    Some(Body::Reasoning {
                        streaming: true, ..
                    }) => return Activity::Thinking,
                    _ => {}
                }
            }
        }
        let activity = Activity::of(main_running, &background, self.ran);
        if activity == Activity::Finished {
            return match self.last_outcome {
                Some(TurnState::Failed) => Activity::Failed,
                Some(TurnState::Interrupted) => Activity::Stopped,
                _ => activity,
            };
        }
        activity
    }

    /// Everything running in the background: the agent's task list, then the provider's workers
    /// still at work (a worker's id is its sub-agent item's).
    pub fn background_tasks(&self) -> Vec<BackgroundTask> {
        self.background_with(&self.subagents())
    }

    /// [`Self::background_tasks`], with the delegation registry's workers.
    /// [`Self::background_tasks`], counting `agents` (a [`Self::subagents_with`] the caller
    /// already has, the delegation registry's workers included).
    pub fn background_with(&self, agents: &[SubagentSummary]) -> Vec<BackgroundTask> {
        let mut background = self.background.clone();
        for worker in agents
            .iter()
            .filter(|a| a.lifecycle.is_some() && a.running())
        {
            background.push(BackgroundTask {
                id: worker.id.clone(),
                kind: BackgroundTaskKind::Agent,
                description: Some(worker.name.clone()),
                tool_use_id: None,
            });
        }
        background
    }

    fn worker_item_id(driver: Driver, id: &str) -> String {
        format!("worker:{}:{id}", driver.info().key)
    }

    fn update_workers(
        &mut self,
        driver: Driver,
        workers: &[WorkerSnapshot],
        waiting_for: &[String],
    ) {
        let prefix = format!("worker:{}:", driver.info().key);
        for id in self.worker_ids.iter().filter(|id| id.starts_with(&prefix)) {
            if !workers
                .iter()
                .any(|w| Self::worker_item_id(driver, &w.id) == *id)
            {
                if let Some(Item {
                    body: Body::Tool(tool),
                    ..
                }) = self.items.get_mut(id)
                {
                    if tool.status == ToolStatus::Running {
                        tool.status = ToolStatus::Interrupted;
                        if let Some(input) = tool.input.as_mut().and_then(Value::as_object_mut) {
                            input.insert(
                                "worker_state".to_owned(),
                                serde_json::json!(WorkerState::Unknown),
                            );
                        }
                    }
                }
            }
        }
        for worker in workers {
            let id = Self::worker_item_id(driver, &worker.id);
            if !self.worker_ids.contains(&id) {
                self.worker_ids.push(id.clone());
            }
            let previous = self.items.get(&id).and_then(|item| match &item.body {
                Body::Tool(tool) => tool.input.as_ref(),
                _ => None,
            });
            let known_field = |new: &Option<String>, key: &str| {
                new.as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .or_else(|| {
                        previous
                            .and_then(|input| input.get(key))
                            .and_then(Value::as_str)
                            .filter(|value| !value.trim().is_empty())
                    })
                    .map(str::to_owned)
            };
            let task = known_field(&worker.task, "prompt");
            let activity = known_field(&worker.activity, "activity");
            let previous_state = previous
                .and_then(|input| input.get("worker_state"))
                .and_then(|state| serde_json::from_value::<WorkerState>(state.clone()).ok());
            let state = match (worker.state, previous_state) {
                (
                    WorkerState::Unknown,
                    Some(
                        known @ (WorkerState::Completed
                        | WorkerState::Failed
                        | WorkerState::Stopped
                        | WorkerState::Closed),
                    ),
                ) => known,
                (state, _) => state,
            };
            let name = known_field(&worker.name, "agent").unwrap_or_else(|| {
                format!(
                    "{} worker {}",
                    driver.info().label,
                    worker.id.chars().take(8).collect::<String>()
                )
            });
            let status = match state {
                WorkerState::Starting
                | WorkerState::Running
                | WorkerState::Waiting
                | WorkerState::Stopping => ToolStatus::Running,
                WorkerState::Completed | WorkerState::Closed => ToolStatus::Completed,
                WorkerState::Failed => ToolStatus::Failed,
                WorkerState::Stopped | WorkerState::Unknown => ToolStatus::Interrupted,
            };
            let tool = Tool {
                kind: ItemKind::Subagent,
                title: name.clone(),
                input: Some(
                    serde_json::json!({ "agent": name, "prompt": task, "worker_state": state, "activity": activity, "delegation": worker.delegation }),
                ),
                input_text: String::new(),
                output: if matches!(state, WorkerState::Completed | WorkerState::Failed) {
                    activity.unwrap_or_default()
                } else {
                    String::new()
                },
                error: None,
                status,
                reported_model: None,
            };
            if let Some(item) = self.items.get_mut(&id) {
                item.body = Body::Tool(tool);
            } else {
                self.items.insert(
                    id.clone(),
                    Item {
                        id,
                        body: Body::Tool(tool),
                        expanded: true,
                        parent: None,
                        children: Vec::new(),
                    },
                );
            }
        }
        self.waiting_for = waiting_for
            .iter()
            .map(|id| Self::worker_item_id(driver, id))
            .collect();
    }

    /// Disconnected/replayed active workers have unknown live state, never an invented closure.
    fn settle_workers(&mut self) -> bool {
        let mut changed = !self.waiting_for.is_empty();
        self.waiting_for.clear();
        for id in &self.worker_ids {
            if let Some(Item {
                body: Body::Tool(tool),
                ..
            }) = self.items.get_mut(id)
            {
                if tool.status == ToolStatus::Running {
                    tool.status = ToolStatus::Interrupted;
                    if let Some(input) = tool.input.as_mut().and_then(Value::as_object_mut) {
                        input.insert(
                            "worker_state".to_owned(),
                            serde_json::json!(WorkerState::Unknown),
                        );
                    }
                    changed = true;
                }
            }
        }
        changed
    }

    /// The input an approval card shows. A Codex file-change request names the item it is about
    /// but carries no diff itself; the diff is in that item's `changes`, which is merged in so the
    /// card can render it. Everything else is the request's own input.
    fn approval_input(&self, input: &Value, item: Option<&str>) -> Value {
        if !agent_kit::editdiff::preview_from_input(input).is_empty() {
            return input.clone();
        }
        let changes = item
            .and_then(|i| self.items.get(i))
            .and_then(|i| match &i.body {
                Body::Tool(t) if t.kind == ItemKind::FileChange => t.input.clone(),
                _ => None,
            })
            .filter(|c| !agent_kit::editdiff::preview_from_input(c).is_empty());
        match changes {
            Some(changes) => {
                let mut merged = input.as_object().cloned().unwrap_or_default();
                merged.insert("changes".to_owned(), changes);
                Value::Object(merged)
            }
            None => input.clone(),
        }
    }

    fn next_id(&mut self, prefix: &str) -> ItemId {
        self.seq += 1;
        format!("{prefix}:{}", self.seq)
    }

    /// Inserts a top-level item just before `anchor` when that is a known top-level item; else at
    /// the end, like [`Self::insert`]. An approval goes before the step it asks about: that step's
    /// card usually started first, so its result (a failure, say) read above the approval that
    /// let it run. A step inside a sub-agent is not an anchor: approvals stay top-level, where
    /// the shelf above the composer finds the pending one.
    fn insert_before(&mut self, id: ItemId, body: Body, anchor: Option<&str>) -> ItemId {
        let anchor = anchor
            .and_then(|a| self.items.get(a))
            .filter(|a| a.parent.is_none())
            .map(|a| a.id.clone());
        let id = self.insert(id, body, None);
        if let Some(anchor) = anchor {
            self.order.retain(|s| *s != id);
            let at = self
                .order
                .iter()
                .position(|s| *s == anchor)
                .unwrap_or(self.order.len());
            self.order.insert(at, id.clone());
        }
        id
    }

    /// Inserts an item (nested under `parent` when that item exists) and returns its id.
    fn insert(&mut self, id: ItemId, body: Body, parent: Option<ItemId>) -> ItemId {
        let expanded = matches!(&body, Body::Tool(t) if t.kind == ItemKind::Subagent)
            || matches!(&body, Body::Approval(_) | Body::Question(_) | Body::Form(_));
        let parent = parent.filter(|p| self.items.contains_key(p));
        match &parent {
            Some(p) => {
                if let Some(parent_item) = self.items.get_mut(p) {
                    parent_item.children.push(id.clone());
                }
            }
            None => {
                let is_queued = matches!(&body, Body::User { queued: true, .. });
                if is_queued {
                    self.queued_count += 1;
                    self.order.push(id.clone());
                } else if self.queued_count > 0 {
                    let insert_pos = self.order.len().saturating_sub(self.queued_count);
                    self.order.insert(insert_pos, id.clone());
                } else {
                    self.order.push(id.clone());
                }
            }
        }
        self.items.insert(
            id.clone(),
            Item {
                id: id.clone(),
                body,
                expanded,
                parent,
                children: Vec::new(),
            },
        );
        id
    }

    /// The newest top-level row, not counting prompts still queued behind it.
    fn newest_row(&self) -> Option<&Item> {
        let end = self.order.len().saturating_sub(self.queued_count);
        self.order[..end]
            .last()
            .and_then(|id| self.items.get(id.as_str()))
    }

    /// Whether the newest row already says `text` as a notice.
    fn newest_notice_is(&self, text: &str) -> bool {
        matches!(self.newest_row().map(|i| &i.body), Some(Body::Notice { text: t, .. }) if t == text)
    }

    /// A row the view adds itself (built-in command output such as `/help`).
    pub fn push_notice(&mut self, text: impl Into<String>, tone: Tone) -> Vec<Change> {
        let id = self.next_id("notice");
        let id = self.insert(
            id,
            Body::Notice {
                text: text.into(),
                tone,
            },
            None,
        );
        vec![Change::Added(id)]
    }

    /// The user answered an approval card; it shows as sent until the agent confirms.
    pub fn mark_approval_sent(&mut self, request: &str, decision: Decision) -> Vec<Change> {
        self.update_requested(Some(request), |body| match body {
            Body::Approval(a) if a.state == ApprovalState::Pending => {
                a.state = ApprovalState::Sent(decision);
                true
            }
            _ => false,
        })
    }

    pub fn mark_questions_sent(&mut self, request: &str) -> Vec<Change> {
        self.update_requested(Some(request), |body| match body {
            Body::Question(q) if q.state == QuestionState::Pending => {
                q.state = QuestionState::Sent;
                true
            }
            _ => false,
        })
    }

    /// The user submitted or declined a form card; it shows as sent until the agent confirms.
    pub fn mark_form_sent(&mut self, request: &str) -> Vec<Change> {
        self.update_requested(Some(request), |body| match body {
            Body::Form(f) if f.state == FormState::Pending => {
                f.state = FormState::Sent;
                f.refused = false;
                true
            }
            _ => false,
        })
    }

    /// The agent did not accept the answer just sent: the card is answerable again, with a line
    /// saying so. Does nothing once the form was resolved some other way meanwhile.
    pub fn mark_form_refused(&mut self, request: &str) -> Vec<Change> {
        self.update_requested(Some(request), |body| match body {
            Body::Form(f) if f.state == FormState::Sent => {
                f.state = FormState::Pending;
                f.refused = true;
                true
            }
            _ => false,
        })
    }

    /// Applies `f` to the body of the item `request` belongs to: `Updated` when `f` says it
    /// changed it, nothing for an unknown request.
    fn update_requested(
        &mut self,
        request: Option<&str>,
        f: impl FnOnce(&mut Body) -> bool,
    ) -> Vec<Change> {
        let Some(id) = request.and_then(|r| self.requests.get(r)).cloned() else {
            return Vec::new();
        };
        let changed = self
            .items
            .get_mut(&id)
            .is_some_and(|item| f(&mut item.body));
        if changed {
            vec![Change::Updated(id)]
        } else {
            Vec::new()
        }
    }

    /// Dangerous permissive mode ends with the process that ran it (no new process starts in
    /// it): once that is gone, the header falls back to the session's own mode. True when it
    /// was shown.
    fn forget_bypass(&mut self) -> bool {
        if self.mode == Some(Mode::Bypass) {
            self.mode = None;
            return true;
        }
        false
    }

    /// Closes out what replayed history left open when no agent is alive to finish it: a thread
    /// stored by an older build (or cut off by a crash) can end mid-turn, with no
    /// `TurnCompleted`. The turn stops running, streaming text ends, Running tool and sub-agent
    /// items become Interrupted, unanswered approvals and questions expire (nothing could take
    /// the answer), and the background list empties. A session that starts later emits its own
    /// fresh events, so nothing here fights it. Returns whether anything changed.
    pub fn settle_stale(&mut self) -> bool {
        let mut changed = self.running || !self.background.is_empty();
        changed |= !self.retire_plan(Some(TurnState::Interrupted)).is_empty();
        if let Some(mode) = self.pending_mode.take() {
            self.mode = Some(mode);
            changed = true;
        }
        changed |= self.forget_bypass();
        if self.running {
            self.last_outcome = Some(TurnState::Interrupted);
        }
        changed |= self.settle_workers();
        self.running = false;
        self.background.clear();
        self.open_text = None;
        for item in self.items.values_mut() {
            match &mut item.body {
                Body::Assistant { streaming, .. } | Body::Reasoning { streaming, .. }
                    if *streaming =>
                {
                    *streaming = false;
                    changed = true;
                }
                Body::Tool(t) if t.status == ToolStatus::Running => {
                    t.status = ToolStatus::Interrupted;
                    changed = true;
                }
                _ => {}
            }
        }
        changed |= !self.expire_requests().is_empty();
        // No Codex session is alive to remember a grant.
        changed |= !std::mem::take(&mut self.extra_access).is_empty();
        changed
    }

    /// A sub-agent that finished cannot finish its own steps any more: one still Running (its
    /// completion was lost) becomes Interrupted, as [`Self::settle_stale`] settles them, so its
    /// spinner stops. Only its direct tool steps: a nested sub-agent still running keeps its own
    /// steps. A background sub-agent (its call returns while it works on) and a worker with its
    /// own lifecycle are left alone. The changed steps.
    fn settle_finished_subagent(&mut self, id: &str) -> Vec<Change> {
        let Some(item) = self.items.get(id) else {
            return Vec::new();
        };
        let Body::Tool(tool) = &item.body else {
            return Vec::new();
        };
        if tool.kind != ItemKind::Subagent || tool.status == ToolStatus::Running {
            return Vec::new();
        }
        let input: Option<Value> = tool
            .input
            .clone()
            .or_else(|| serde_json::from_str(&tool.input_text).ok());
        let field = |key: &str| input.as_ref().and_then(|i| i.get(key));
        let in_background = field("run_in_background").and_then(Value::as_bool) == Some(true)
            || self
                .background
                .iter()
                .any(|task| task.id == id || task.tool_use_id.as_deref() == Some(id));
        if in_background || field("worker_state").is_some() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for child in item.children.clone() {
            if let Some(Body::Tool(step)) = self.items.get_mut(&child).map(|c| &mut c.body) {
                if step.status == ToolStatus::Running && step.kind != ItemKind::Subagent {
                    step.status = ToolStatus::Interrupted;
                    out.push(Change::Updated(child));
                }
            }
        }
        out
    }

    /// Expires every approval, question and form still waiting on the user (or sent and not yet
    /// acknowledged): nothing can take the answer any more, because the turn that asked ended
    /// (stopped or failed) or no agent is alive. Returns the items changed.
    fn expire_requests(&mut self) -> Vec<ItemId> {
        let mut changed = Vec::new();
        for (id, item) in &mut self.items {
            let expired = match &mut item.body {
                Body::Approval(a)
                    if matches!(a.state, ApprovalState::Pending | ApprovalState::Sent(_)) =>
                {
                    a.state = ApprovalState::Expired;
                    true
                }
                Body::Question(q)
                    if matches!(q.state, QuestionState::Pending | QuestionState::Sent) =>
                {
                    q.state = QuestionState::Expired;
                    true
                }
                Body::Form(f) if matches!(f.state, FormState::Pending | FormState::Sent) => {
                    f.state = FormState::Expired;
                    true
                }
                _ => false,
            };
            if expired {
                changed.push(id.clone());
            }
        }
        changed.sort();
        changed
    }

    /// A question the agent withdrew without an answer (its request cancelled or expired, as
    /// Claude reports any request it drops: `ApprovalResolved { Cancel }`). `expired`: nothing
    /// can answer it now. Questions already answered keep their state.
    fn withdraw_question(&mut self, env: &Envelope, expired: bool) -> Vec<Change> {
        self.update_requested(env.request.as_deref(), |body| match body {
            Body::Question(q)
                if matches!(q.state, QuestionState::Pending | QuestionState::Sent) =>
            {
                q.state = if expired {
                    QuestionState::Expired
                } else {
                    QuestionState::Withdrawn
                };
                true
            }
            _ => false,
        })
    }

    /// Marks every finished reasoning block loaded from history as Replayed (it did not come
    /// from the live agent). A block the model did not share stays Unavailable, and one still
    /// streaming (a live session resumed mid-block) is left to its agent.
    pub fn mark_replayed(&mut self) {
        for item in self.items.values_mut() {
            if let Body::Reasoning {
                streaming: false,
                state,
                ..
            } = &mut item.body
            {
                if *state != Some(ReasoningState::Unavailable) {
                    *state = Some(ReasoningState::Replayed);
                }
            }
        }
    }

    pub fn set_expanded(&mut self, id: &str, expanded: bool) {
        if let Some(item) = self.items.get_mut(id) {
            item.expanded = expanded;
        }
    }

    /// Retire the shelf without inferring that pending work was completed.
    fn retire_plan(&mut self, outcome: Option<TurnState>) -> Vec<Change> {
        let mut changes = Vec::new();
        if let Some(id) = self.plan_item.take() {
            if let Some(Body::Plan {
                outcome: previous, ..
            }) = self.items.get_mut(&id).map(|item| &mut item.body)
            {
                if outcome.is_some() && *previous != outcome {
                    *previous = outcome;
                    changes.push(Change::Updated(id));
                }
            }
        }
        if !self.plan.is_empty() {
            self.plan.clear();
            changes.push(Change::Plan);
        }
        changes
    }

    /// Applies one envelope. `driver` is the agent the thread is on right now (the envelope
    /// does not carry it); it colours assistant rows and detects provider switches.
    pub fn apply(&mut self, env: &Envelope, driver: Driver) -> Vec<Change> {
        let mut out = Vec::new();
        // A switch says what it carried (and what ended) as the new agent, before that agent's
        // session starts. The divider goes first, so those notices read under it; the session's
        // start then names the model in it.
        let entering = self.last_driver.is_some_and(|d| d != driver);
        if entering && !matches!(env.event, Event::SessionStarted { .. }) {
            out.extend(self.enter_agent(driver, None));
            self.unnamed_switch = out.iter().find_map(|c| match c {
                Change::Added(id) => Some(id.clone()),
                _ => None,
            });
            self.last_model = None;
        }
        match &env.event {
            Event::SessionStarted { model, .. } => {
                // A new process knows nothing of what the last one was allowed.
                self.extra_access.clear();
                if entering {
                    out.extend(self.enter_agent(driver, model.clone()));
                } else if let Some(m) = model {
                    if self.unnamed_switch.is_some() {
                        out.extend(self.name_switch(m));
                    } else if self.last_model.as_deref().is_some_and(|last| last != m) {
                        self.gauge = None;
                        out.extend(self.push_switch(driver, Some(m.clone()), false, false));
                        out.push(Change::Gauge);
                    }
                }
                self.on_driver(driver);
                if model.is_some() {
                    self.last_model.clone_from(model);
                }
            }
            Event::SessionExited { code, expected } => {
                self.extra_access.clear();
                out.extend(self.retire_plan(Some(if *expected {
                    TurnState::Interrupted
                } else {
                    TurnState::Failed
                })));
                if let Some(mode) = self.pending_mode.take() {
                    self.mode = Some(mode);
                    out.push(Change::Mode);
                }
                if self.forget_bypass() {
                    out.push(Change::Mode);
                }
                if self.settle_workers() {
                    out.push(Change::Workers);
                    out.push(Change::Running);
                }
                // A dead session runs nothing in the background either.
                if self.running || !self.background.is_empty() {
                    self.running = false;
                    self.background.clear();
                    out.push(Change::Running);
                }
                self.open_text = None;
                if !expected {
                    let message = match code {
                        Some(c) => format!("The agent exited unexpectedly (code {c})."),
                        None => "The agent exited unexpectedly.".to_owned(),
                    };
                    let id = self.next_id("error");
                    out.push(Change::Added(self.insert(
                        id,
                        Body::TurnError { message },
                        None,
                    )));
                }
            }
            Event::CommandsChanged { .. } => out.push(Change::Commands),
            Event::TurnStarted { model } => {
                // A later turn never resurrects the preceding turn's active shelf.
                out.extend(self.retire_plan(None));
                self.last_outcome = None;
                // The quick replies answered the last turn's reply; a new turn replaces them.
                out.extend(self.dismiss_quick_replies());
                if self.last_driver.is_none() {
                    self.on_driver(driver);
                }
                if let Some(m) = model {
                    if self.unnamed_switch.is_some() {
                        out.extend(self.name_switch(m));
                    } else if self.last_model.as_deref().is_some_and(|last| last != m) {
                        out.extend(self.push_switch(driver, Some(m.clone()), false, false));
                    }
                    self.last_model = Some(m.clone());
                }
                self.ran = true;
                if !self.running {
                    self.running = true;
                    out.push(Change::Running);
                }
            }
            Event::TurnCompleted { state, error, .. } => {
                out.extend(self.retire_plan(Some(*state)));
                let outcome_changed =
                    self.last_outcome != Some(*state) || !self.waiting_for.is_empty();
                self.last_outcome = Some(*state);
                self.waiting_for.clear();
                if self.running {
                    self.running = false;
                    out.push(Change::Running);
                } else if outcome_changed {
                    out.push(Change::Running);
                }
                if let Some(id) = self.open_text.take() {
                    out.extend(self.finish_text(&id));
                }
                // A stopped turn can no longer take an answer: whatever it was still asking (a
                // question, an approval, a form) expires, so nothing waits forever. Only a stop:
                // background work (Codex workers, Claude background sub-agents) can outlive a
                // turn that completed or failed and still ask, and those requests stay live.
                if *state == TurnState::Interrupted {
                    out.extend(self.expire_requests().into_iter().map(Change::Updated));
                }
                match state {
                    TurnState::Completed => {}
                    TurnState::Interrupted => {
                        // Said once: a second "Interrupted" in a row adds nothing.
                        if !self.newest_notice_is("Interrupted") {
                            out.extend(self.push_notice("Interrupted", Tone::Info));
                        }
                    }
                    TurnState::Failed => {
                        let message = error
                            .clone()
                            .filter(|e| !e.trim().is_empty())
                            .unwrap_or_else(|| "The turn failed.".to_owned());
                        let id = self.next_id("error");
                        out.push(Change::Added(self.insert(
                            id,
                            Body::TurnError { message },
                            None,
                        )));
                    }
                }
            }
            Event::ItemStarted {
                kind,
                title,
                input,
                parent,
            } => {
                // Older Codex history labelled collaboration calls as subagents.
                // Those calls do not establish a worker identity or lifecycle.
                let restored_kind = if driver == Driver::Codex && *kind == ItemKind::Subagent {
                    ItemKind::Tool
                } else {
                    *kind
                };
                let kind = &restored_kind;
                let id = env.item.clone().unwrap_or_else(|| self.next_id("item"));
                if let Some(item) = self.items.get_mut(&id) {
                    // A finished sub-agent started again (Claude continuing it) runs again on
                    // the same card, keeping what it was asked. Only a Claude sub-agent: any
                    // other repeated start is a refinement or a replay.
                    if let Body::Tool(tool) = &mut item.body {
                        if driver == Driver::Claude
                            && tool.kind == ItemKind::Subagent
                            && *kind == ItemKind::Subagent
                            && tool.status != ToolStatus::Running
                        {
                            tool.status = ToolStatus::Running;
                            tool.error = None;
                            out.push(Change::Updated(id));
                            return out;
                        }
                    }
                    // Codex can start a tool before it knows the full action, then repeat the
                    // item in its completed snapshot. Accept a matching tool start as a metadata
                    // refinement; identical replayed starts remain no-ops.
                    if let Body::Tool(tool) = &mut item.body {
                        if tool.kind == *kind && (tool.title != *title || tool.input != *input) {
                            tool.title.clone_from(title);
                            tool.input.clone_from(input);
                            out.push(Change::Updated(id.clone()));
                        }
                    }
                    if let Body::User { queued, .. } = &mut item.body {
                        let is_queued = title == "queued"
                            || input
                                .as_ref()
                                .and_then(|v| v.get("queued"))
                                .and_then(Value::as_bool)
                                == Some(true);
                        if *queued != is_queued {
                            if *queued {
                                self.queued_count = self.queued_count.saturating_sub(1);
                            } else {
                                self.queued_count += 1;
                            }
                            *queued = is_queued;
                            out.push(Change::Updated(id));
                        }
                    }
                    return out;
                }
                if *kind == ItemKind::UserMessage {
                    // Whatever the user sends supersedes the suggestions, typed or not.
                    out.extend(self.dismiss_quick_replies());
                }
                let body = match kind {
                    ItemKind::UserMessage => {
                        let queued = title == "queued"
                            || input
                                .as_ref()
                                .and_then(|v| v.get("queued"))
                                .and_then(Value::as_bool)
                                == Some(true);
                        Body::User {
                            text: String::new(),
                            queued,
                        }
                    }
                    ItemKind::AssistantMessage => Body::Assistant {
                        text: String::new(),
                        driver,
                        streaming: true,
                    },
                    ItemKind::Reasoning => Body::new_reasoning(),
                    _ => Body::Tool(Tool {
                        kind: *kind,
                        title: title.clone(),
                        input: input.clone(),
                        input_text: String::new(),
                        output: String::new(),
                        error: None,
                        status: ToolStatus::Running,
                        reported_model: None,
                    }),
                };
                if matches!(kind, ItemKind::AssistantMessage | ItemKind::Reasoning) {
                    self.open_text = Some(id.clone());
                }
                out.push(Change::Added(self.insert(id, body, parent.clone())));
            }
            Event::SubagentModel { model } => {
                let Some(id) = env.item.clone() else {
                    return out;
                };
                if let Some(Body::Tool(tool)) = self.items.get_mut(&id).map(|item| &mut item.body) {
                    if tool.kind == ItemKind::Subagent
                        && tool.reported_model.as_deref() != Some(model.as_str())
                    {
                        tool.reported_model = Some(model.clone());
                        out.push(Change::Updated(id));
                    }
                }
            }
            Event::ContentDelta { stream, text } => {
                let id = self.text_target(env, *stream, driver, &mut out);
                if let Some(id) = id {
                    if self.write_stream(&id, *stream, text, false) {
                        out.push(Change::Updated(id));
                    }
                }
            }
            Event::ContentSnapshot { stream, text } => {
                let id = self.text_target(env, *stream, driver, &mut out);
                if let Some(id) = id {
                    let mut updated = self.write_stream(&id, *stream, text, true);
                    if *stream == StreamKind::ToolInput {
                        if let Some(item) = self.items.get_mut(&id) {
                            if let Body::Tool(t) = &mut item.body {
                                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text)
                                {
                                    if !parsed.is_null() && t.input.as_ref() != Some(&parsed) {
                                        t.input = Some(parsed);
                                        updated = true;
                                    }
                                }
                            }
                        }
                    }
                    if updated {
                        out.push(Change::Updated(id));
                    }
                }
            }
            Event::ItemCompleted {
                status,
                output,
                error,
            } => {
                let Some(id) = env.item.clone() else {
                    return out;
                };
                if self.open_text.as_deref() == Some(id.as_str()) {
                    self.open_text = None;
                }
                let Some(item) = self.items.get_mut(&id) else {
                    return out;
                };
                match &mut item.body {
                    Body::Tool(t) => {
                        t.status = (*status).into();
                        if let Some(o) = output {
                            t.output.clone_from(o);
                        }
                        if error.is_some() {
                            t.error.clone_from(error);
                        }
                        // A failure is what the user needs to see first.
                        if t.status == ToolStatus::Failed && t.kind != ItemKind::Subagent {
                            item.expanded = true;
                        }
                    }
                    Body::Assistant { streaming, .. } | Body::Reasoning { streaming, .. } => {
                        *streaming = false;
                    }
                    _ => {}
                }
                out.push(Change::Updated(id.clone()));
                out.extend(self.settle_finished_subagent(&id));
            }
            Event::Reasoning { state, label } => {
                let Some(id) = env.item.clone() else {
                    return out;
                };
                if let Some(Body::Reasoning {
                    state: current,
                    label: current_label,
                    ..
                }) = self.items.get_mut(&id).map(|i| &mut i.body)
                {
                    if *current != Some(*state) || current_label != label {
                        *current = Some(*state);
                        *current_label = *label;
                        out.push(Change::Updated(id));
                    }
                }
            }
            Event::PlanUpdated { steps } => {
                // A terminal turn owns no active plan. Delayed provider updates must not
                // recreate its shelf or rewrite the retained historical snapshot.
                if !self.running {
                    return out;
                }
                // Some providers start another plan in the same turn after finishing one.
                // Keep the completed row instead of overwriting it with the next task.
                if !self.plan.is_empty() && !plan_is_active(&self.plan) && self.plan != *steps {
                    self.plan_item = None;
                }
                self.plan.clone_from(steps);
                if !steps.is_empty() {
                    let body = Body::Plan {
                        steps: steps.clone(),
                        outcome: None,
                    };
                    if let Some(id) = self.plan_item.as_ref() {
                        if let Some(item) = self.items.get_mut(id) {
                            if item.body != body {
                                item.body = body;
                                out.push(Change::Updated(id.clone()));
                            }
                        }
                    } else {
                        let id = self.next_id("plan");
                        self.plan_item = Some(id.clone());
                        out.push(Change::Added(self.insert(id, body, None)));
                    }
                }
                out.push(Change::Plan);
            }
            Event::ApprovalRequested {
                tool,
                title,
                input,
                reason,
                options,
                response,
                remembers,
            } => {
                let request = env.request.clone().unwrap_or_else(|| self.next_id("req"));
                if self.requests.contains_key(&request) {
                    return out;
                }
                let id = format!("approval:{request}");
                let state = match response {
                    ResponseCapability::Live => ApprovalState::Pending,
                    ResponseCapability::Expired => ApprovalState::Expired,
                };
                let input = self.approval_input(input, env.item.as_deref());
                let body = Body::Approval(Approval {
                    request: request.clone(),
                    tool: tool.clone(),
                    title: title.clone(),
                    input,
                    reason: reason.clone(),
                    options: options.clone(),
                    state,
                    remembers: remembers.clone(),
                });
                self.requests.insert(request, id.clone());
                let step = env.item.clone();
                out.push(Change::Added(self.insert_before(id, body, step.as_deref())));
            }
            Event::ApprovalResolved { decision } => {
                if *decision == Decision::AllowForSession {
                    self.note_session_grant(env);
                }
                out.extend(self.update_approval(env, |a| {
                    a.state = ApprovalState::Resolved(*decision);
                }));
                // Claude asks its questions through the same permission request and drops one
                // (a stop, a cancel, the process ending) as a cancelled approval.
                if matches!(decision, Decision::Cancel | Decision::Deny) {
                    out.extend(self.withdraw_question(env, false));
                }
            }
            Event::ApprovalExpired => {
                out.extend(self.update_approval(env, |a| {
                    if matches!(a.state, ApprovalState::Pending | ApprovalState::Sent(_)) {
                        a.state = ApprovalState::Expired;
                    }
                }));
                out.extend(self.withdraw_question(env, true));
            }
            Event::QuestionRequested {
                questions,
                source: QuestionSource::Reply,
            } => out.extend(self.offer_quick_replies(env.request.as_deref(), questions)),
            Event::QuestionRequested { questions, .. } => {
                let request = env.request.clone().unwrap_or_else(|| self.next_id("req"));
                if self.requests.contains_key(&request) {
                    return out;
                }
                let id = format!("question:{request}");
                let body = Body::Question(QuestionCard {
                    request: request.clone(),
                    questions: questions.clone(),
                    state: QuestionState::Pending,
                });
                self.requests.insert(request, id.clone());
                out.push(Change::Added(self.insert(id, body, None)));
            }
            Event::QuestionResolved { answered } => {
                out.extend(
                    self.update_requested(env.request.as_deref(), |body| match body {
                        Body::Question(q) => {
                            q.state = if *answered {
                                QuestionState::Answered
                            } else {
                                QuestionState::Withdrawn
                            };
                            true
                        }
                        _ => false,
                    }),
                );
            }
            Event::ModeChanged { mode } => {
                self.mode = Some(*mode);
                self.pending_mode = None;
                out.push(Change::Mode);
            }
            Event::ModeChangeDeferred {
                requested,
                effective,
            } => {
                self.mode = Some(*effective);
                self.pending_mode = Some(*requested);
                out.push(Change::Mode);
            }
            Event::UsageUpdated {
                used,
                max,
                auto_compact_at,
            } => {
                self.gauge = Some(Gauge {
                    used: *used,
                    max: *max,
                    auto_compact_at: *auto_compact_at,
                });
                out.push(Change::Gauge);
            }
            Event::Compacted {
                manual,
                before,
                after,
            } => {
                let id = self.next_id("compact");
                out.push(Change::Added(self.insert(
                    id,
                    Body::Compaction {
                        manual: *manual,
                        before: *before,
                        after: *after,
                    },
                    None,
                )));
                if let (Some(g), Some(after)) = (self.gauge.as_mut(), after) {
                    g.used = *after;
                    out.push(Change::Gauge);
                }
            }
            Event::RateLimited { resets_at, .. } => {
                let text = rate_limit_notice(resets_at.as_deref(), crate::freshness::now_epoch());
                out.extend(self.push_notice(text, Tone::Warning));
            }
            Event::ModelChanged { model } => {
                if self.unnamed_switch.is_some() {
                    out.extend(self.name_switch(model));
                    self.last_model = Some(model.clone());
                } else if self.last_model.as_deref() != Some(model.as_str()) {
                    if self.last_model.is_some() {
                        out.extend(self.push_switch(driver, Some(model.clone()), false, false));
                        self.gauge = None;
                        out.push(Change::Gauge);
                    }
                    self.last_model = Some(model.clone());
                }
            }
            // Pending selection is process-local backend state, never restored from history.
            Event::ModelChangeRequested { .. } => {}
            Event::ModelChangeFailed { model, message } => {
                out.extend(self.push_notice(
                    format!("Could not switch to {model}: {message}. The previous model is kept."),
                    Tone::Warning,
                ));
            }
            Event::Notice { text } => {
                // The agent's own "Set model to X" right under the "Model switched to X" divider
                // says the same thing twice, and so does any notice repeated in a row.
                let says_switch = matches!(
                    self.newest_row().map(|i| &i.body),
                    Some(Body::Switch { model: Some(m), .. })
                        if text.starts_with("Set model to") && text.contains(m.as_str())
                );
                if !says_switch && !self.newest_notice_is(text) {
                    out.extend(self.push_notice(text.clone(), Tone::Info));
                }
            }
            Event::ControlResult { ok, error } => {
                if let Some(request) = env.request.clone() {
                    let result = match (ok, error) {
                        (_, Some(e)) => Err(e.clone()),
                        (Some(v), None) => Ok(v.clone()),
                        (None, None) => Ok(Value::Null),
                    };
                    out.push(Change::Control { request, result });
                }
            }
            Event::Error { message } => {
                let id = self.next_id("error");
                out.push(Change::Added(self.insert(
                    id,
                    Body::TurnError {
                        message: message.clone(),
                    },
                    None,
                )));
            }
            // The session's own bookkeeping: the notice beside it is what the user sees.
            Event::HandoffSeeded { .. } | Event::ModeChosen { .. } => {}
            Event::FormRequested {
                server,
                message,
                form,
            } => {
                let request = env.request.clone().unwrap_or_else(|| self.next_id("req"));
                if self.requests.contains_key(&request) {
                    return out;
                }
                let answerable = form_answerable(form);
                let id = format!("form:{request}");
                let body = Body::Form(FormCard {
                    request: request.clone(),
                    server: server.clone(),
                    message: message.clone(),
                    form: form.clone(),
                    refused: false,
                    state: if answerable {
                        FormState::Pending
                    } else {
                        FormState::Unanswerable
                    },
                });
                self.requests.insert(request, id.clone());
                out.push(Change::Added(self.insert(id, body, None)));
            }
            Event::FormResolved { action } => {
                let target = env
                    .request
                    .as_ref()
                    .and_then(|r| self.requests.get(r))
                    .cloned();
                if let Some(id) = target {
                    if let Some(Body::Form(f)) = self.items.get_mut(&id).map(|i| &mut i.body) {
                        // A form this build never offered stays as it was shown.
                        if f.state == FormState::Unanswerable {
                            return out;
                        }
                        f.refused = false;
                        // A cancel the user never made (the server settled it elsewhere, the
                        // turn ended, the process exited) is an expiry, not a dismissal.
                        f.state = if f.state == FormState::Pending && *action == FormAction::Cancel
                        {
                            FormState::Expired
                        } else {
                            FormState::Resolved(*action)
                        };
                        out.push(Change::Updated(id));
                    }
                }
            }
            // Background tasks are thread state, not transcript rows: the full list each time.
            Event::BackgroundTasks { tasks } => {
                if self.background != *tasks {
                    self.background.clone_from(tasks);
                    out.push(Change::Running);
                }
            }
            Event::WorkersUpdated {
                workers,
                waiting_for,
            } => {
                self.update_workers(driver, workers, waiting_for);
                out.push(Change::Workers);
                out.push(Change::Running);
            }
            // Quota feeds the usage indicator (app-wide service), not the transcript.
            Event::QuotaUpdated { .. } | Event::Unknown => {}
            // Delegation events are not shown yet.
            Event::DelegatedMessage { .. } | Event::WorkerLinked { .. } => {}
        }
        out
    }

    /// The thread is on `driver` now.
    fn on_driver(&mut self, driver: Driver) {
        self.last_driver = Some(driver);
        if !self.seen_drivers.contains(&driver) {
            self.seen_drivers.push(driver);
        }
    }

    /// The thread moved to `driver`: the old agent's workers are done, its gauge means nothing,
    /// and a divider says where the thread went (and whether it was there before).
    fn enter_agent(&mut self, driver: Driver, model: Option<String>) -> Vec<Change> {
        let mut out = Vec::new();
        if self.settle_workers() {
            out.push(Change::Workers);
            out.push(Change::Running);
        }
        self.gauge = None;
        let returned = self.seen_drivers.contains(&driver);
        out.extend(self.push_switch(driver, model, true, returned));
        out.push(Change::Gauge);
        self.on_driver(driver);
        out
    }

    /// Puts the model the new agent reported into the divider drawn before it named one.
    fn name_switch(&mut self, reported: &str) -> Vec<Change> {
        let Some(id) = self.unnamed_switch.take() else {
            return Vec::new();
        };
        match self.items.get_mut(&id).map(|i| &mut i.body) {
            Some(Body::Switch { model, .. }) => {
                *model = Some(reported.to_owned());
                vec![Change::Updated(id)]
            }
            _ => Vec::new(),
        }
    }

    fn push_switch(
        &mut self,
        driver: Driver,
        model: Option<String>,
        agent_changed: bool,
        returned: bool,
    ) -> Vec<Change> {
        let id = self.next_id("switch");
        let id = self.insert(
            id,
            Body::Switch {
                driver,
                model,
                agent_changed,
                returned,
            },
            None,
        );
        vec![Change::Added(id)]
    }

    fn finish_text(&mut self, id: &str) -> Vec<Change> {
        match self.items.get_mut(id).map(|i| &mut i.body) {
            Some(Body::Assistant { streaming, .. } | Body::Reasoning { streaming, .. })
                if *streaming =>
            {
                *streaming = false;
                vec![Change::Updated(id.to_owned())]
            }
            _ => Vec::new(),
        }
    }

    /// The item a content event writes to, creating an implicit assistant/reasoning item when
    /// the adapter sent text without opening one.
    fn text_target(
        &mut self,
        env: &Envelope,
        stream: StreamKind,
        driver: Driver,
        out: &mut Vec<Change>,
    ) -> Option<ItemId> {
        if let Some(id) = &env.item {
            if self.items.contains_key(id) {
                return Some(id.clone());
            }
        }
        if env.item.is_none() {
            if let Some(open) = &self.open_text {
                let matches = matches!(
                    (stream, self.items.get(open).map(|i| &i.body)),
                    (StreamKind::Assistant, Some(Body::Assistant { .. }))
                        | (StreamKind::Reasoning, Some(Body::Reasoning { .. }))
                );
                if matches {
                    return Some(open.clone());
                }
            }
        }
        let body = match stream {
            StreamKind::Assistant => Body::Assistant {
                text: String::new(),
                driver,
                streaming: true,
            },
            StreamKind::Reasoning => Body::new_reasoning(),
            // Tool text for a tool we never saw start: nothing to attach it to.
            StreamKind::ToolInput | StreamKind::ToolOutput => return None,
        };
        let id = env.item.clone().unwrap_or_else(|| self.next_id("text"));
        let id = self.insert(id, body, None);
        self.open_text = Some(id.clone());
        out.push(Change::Added(id.clone()));
        Some(id)
    }

    /// Appends (or with `replace`, sets) a stream's text. Returns whether anything changed.
    fn write_stream(&mut self, id: &str, stream: StreamKind, text: &str, replace: bool) -> bool {
        let Some(item) = self.items.get_mut(id) else {
            return false;
        };
        let target: &mut String = match (&mut item.body, stream) {
            (Body::User { text, .. }, _) => text,
            (Body::Assistant { text, .. }, StreamKind::Assistant | StreamKind::Reasoning) => text,
            (Body::Reasoning { text, .. }, StreamKind::Reasoning | StreamKind::Assistant) => text,
            (Body::Tool(t), StreamKind::ToolInput) => &mut t.input_text,
            (Body::Tool(t), StreamKind::ToolOutput | StreamKind::Assistant) => &mut t.output,
            _ => return false,
        };
        if replace {
            if target == text {
                return false;
            }
            text.clone_into(target);
        } else {
            if text.is_empty() {
                return false;
            }
            target.push_str(text);
        }
        true
    }

    /// A permission request answered "Allow for this session": what it granted joins the
    /// header's "Extra access" list (each line once).
    fn note_session_grant(&mut self, env: &Envelope) {
        let Some(Body::Approval(a)) = env
            .request
            .as_ref()
            .and_then(|r| self.requests.get(r))
            .and_then(|id| self.items.get(id))
            .map(|i| &i.body)
        else {
            return;
        };
        if a.tool != super::permission::TOOL {
            return;
        }
        let granted = super::permission::Request::parse(&a.input);
        // An unreadable request was never grantable, whatever was sent.
        if !granted.renderable {
            return;
        }
        for line in granted.grant_lines() {
            if !self.extra_access.contains(&line) {
                self.extra_access.push(line);
            }
        }
    }

    fn update_approval(&mut self, env: &Envelope, f: impl FnOnce(&mut Approval)) -> Vec<Change> {
        self.update_requested(env.request.as_deref(), |body| match body {
            Body::Approval(a) => {
                f(a);
                true
            }
            _ => false,
        })
    }
}

fn rate_limit_notice(resets_at: Option<&str>, now: i64) -> String {
    let local = resets_at.and_then(|at| match at.parse::<i64>() {
        Ok(epoch) => {
            let seconds = if at.len() == 13 && at.bytes().all(|b| b.is_ascii_digit()) {
                epoch / 1000
            } else {
                epoch
            };
            gtk4::glib::DateTime::from_unix_local(seconds).ok()
        }
        Err(_) => gtk4::glib::DateTime::from_iso8601(at, None)
            .and_then(|at| at.to_local())
            .ok(),
    });
    let today = gtk4::glib::DateTime::from_unix_local(now).ok();
    let formatted = local.and_then(|at| {
        let same_day = today.is_some_and(|today| {
            (at.year(), at.month(), at.day_of_month())
                == (today.year(), today.month(), today.day_of_month())
        });
        at.format(if same_day { "%H:%M" } else { "%Y-%m-%d %H:%M" })
            .ok()
    });
    match formatted {
        Some(at) => format!("Rate limited. The quota resets at {at}."),
        None => "Rate limited by the provider.".to_owned(),
    }
}

/// Compact token count for the gauge and the compaction divider: `950`, `21k`, `1.2M`.
/// Whether a user message is the prompt a busy switch sent for the user.
pub fn is_switch_continuation(text: &str) -> bool {
    text == crate::chat::SWITCH_CONTINUATION
}

pub fn format_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        let k = n as f64 / 1_000.0;
        if k < 10.0 {
            format!("{k:.1}k").replace(".0k", "k")
        } else {
            format!("{}k", (k).round() as u64)
        }
    } else {
        let m = n as f64 / 1_000_000.0;
        format!("{m:.1}M").replace(".0M", "M")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(e: Event) -> Envelope {
        Envelope::new(e)
    }

    #[test]
    fn usage_before_the_first_model_report_survives_it() {
        let mut t = Transcript::new();
        t.apply(
            &ev(Event::UsageUpdated {
                used: 42_000,
                max: Some(200_000),
                auto_compact_at: Some(160_000),
            }),
            Driver::Codex,
        );
        let before = t.gauge;
        let changes = t.apply(
            &ev(Event::ModelChanged { model: "m1".into() }),
            Driver::Codex,
        );
        assert_eq!(t.gauge, before);
        assert!(!changes.contains(&Change::Gauge));
        assert_eq!(t.last_model.as_deref(), Some("m1"));
    }

    fn tool_start(kind: ItemKind, title: &str, input: Option<Value>) -> Event {
        Event::ItemStarted {
            kind,
            title: title.to_owned(),
            input,
            parent: None,
        }
    }

    fn done(status: ItemStatus, output: &str) -> Event {
        Event::ItemCompleted {
            status,
            output: Some(output.to_owned()),
            error: (status == ItemStatus::Failed).then(|| output.to_owned()),
        }
    }

    #[test]
    fn a_finished_claude_sub_agent_started_again_runs_again() {
        let mut t = Transcript::new();
        let input = json!({"description": "Survey", "prompt": "Look around."});
        t.apply(
            &ev(tool_start(ItemKind::Subagent, "Agent", Some(input.clone()))).item("toolu_a"),
            Driver::Claude,
        );
        t.apply(
            &ev(done(ItemStatus::Failed, "boom")).item("toolu_a"),
            Driver::Claude,
        );
        assert!(!t.subagents()[0].active());
        // The adapter restarts it with no input: the card keeps what it was asked.
        let changes = t.apply(
            &ev(tool_start(ItemKind::Subagent, "Agent", None)).item("toolu_a"),
            Driver::Claude,
        );
        assert_eq!(changes, [Change::Updated("toolu_a".to_owned())]);
        let agents = t.subagents();
        assert!(agents[0].active());
        let Some(Body::Tool(tool)) = t.get("toolu_a").map(|i| &i.body) else {
            panic!("no tool card");
        };
        assert_eq!(tool.error, None);
        assert_eq!(tool.input.as_ref(), Some(&input));
        t.apply(
            &ev(Event::BackgroundTasks {
                tasks: vec![BackgroundTask {
                    id: "ag1".into(),
                    kind: BackgroundTaskKind::Agent,
                    description: Some("Survey".into()),
                    tool_use_id: Some("toolu_a".into()),
                }],
            }),
            Driver::Claude,
        );
        let tasks = t.background_tasks();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].tool_use_id.as_deref(), Some(agents[0].id.as_str()));
        // Its next completion finishes it.
        t.apply(
            &ev(done(ItemStatus::Completed, "second")).item("toolu_a"),
            Driver::Claude,
        );
        assert!(!t.subagents()[0].active());
    }

    #[test]
    fn a_repeated_start_of_a_running_or_codex_item_changes_nothing() {
        let mut t = Transcript::new();
        let start = ev(tool_start(
            ItemKind::Subagent,
            "Agent",
            Some(json!({"prompt": "go"})),
        ))
        .item("toolu_a");
        t.apply(&start, Driver::Claude);
        assert!(t.apply(&start, Driver::Claude).is_empty());
        assert!(t.subagents()[0].active());

        // A Codex sub-agent start is read as a tool: a finished one is not restarted, and an
        // identical start of a finished tool stays a no-op.
        let mut t = Transcript::new();
        let start = ev(tool_start(ItemKind::Subagent, "spawn", None)).item("c1");
        t.apply(&start, Driver::Codex);
        t.apply(
            &ev(done(ItemStatus::Completed, "ok")).item("c1"),
            Driver::Codex,
        );
        assert!(t.apply(&start, Driver::Codex).is_empty());
        // A refinement updates the card's metadata and leaves it finished.
        let refined = ev(tool_start(ItemKind::Subagent, "spawn worker", None)).item("c1");
        assert_eq!(
            t.apply(&refined, Driver::Codex),
            [Change::Updated("c1".to_owned())]
        );
        let Some(Body::Tool(tool)) = t.get("c1").map(|i| &i.body) else {
            panic!("no tool card");
        };
        assert_eq!(tool.status, ToolStatus::Completed);
        assert_eq!(tool.title, "spawn worker");
    }

    #[test]
    fn the_prompt_a_switch_sent_is_not_recalled_as_the_users() {
        let mut t = Transcript::new();
        for (id, text) in [
            ("u1", "fix the build"),
            ("u2", crate::chat::SWITCH_CONTINUATION),
        ] {
            t.apply(
                &ev(Event::ItemStarted {
                    kind: ItemKind::UserMessage,
                    title: String::new(),
                    input: None,
                    parent: None,
                })
                .item(id),
                Driver::Claude,
            );
            t.apply(
                &ev(Event::ContentSnapshot {
                    stream: StreamKind::Assistant,
                    text: agent_core::redact::redact(text),
                })
                .item(id),
                Driver::Claude,
            );
        }
        assert_eq!(t.user_messages(), ["fix the build"]);
        assert!(
            is_switch_continuation(&agent_core::redact::redact(
                crate::chat::SWITCH_CONTINUATION
            )),
            "redaction leaves the marker's text as sent"
        );
    }

    #[test]
    fn a_rate_limit_reset_in_milliseconds_matches_epoch_seconds() {
        let today =
            gtk4::glib::DateTime::new(&gtk4::glib::TimeZone::local(), 2026, 10, 14, 13, 30, 0.0)
                .unwrap();
        for at in [today.add_hours(1).unwrap(), today.add_days(1).unwrap()] {
            let seconds = at.to_unix();
            let expected = rate_limit_notice(Some(&seconds.to_string()), today.to_unix());
            for milliseconds in [seconds * 1000, seconds * 1000 + 999] {
                assert_eq!(
                    rate_limit_notice(Some(&milliseconds.to_string()), today.to_unix()),
                    expected
                );
            }
        }
    }

    #[test]
    fn a_switch_clears_the_context_gauge() {
        for (driver, model) in [(Driver::Codex, "m1"), (Driver::Claude, "m2")] {
            let mut t = Transcript::new();
            let started = |model: &str| {
                ev(Event::SessionStarted {
                    native_id: "n".into(),
                    model: Some(model.into()),
                    cwd: None,
                })
            };
            t.apply(&started("m1"), Driver::Claude);
            t.apply(
                &ev(Event::UsageUpdated {
                    used: 42_000,
                    max: Some(200_000),
                    auto_compact_at: None,
                }),
                Driver::Claude,
            );
            let unchanged = t.apply(&started("m1"), Driver::Claude);
            assert!(t.gauge.is_some());
            assert!(!unchanged.contains(&Change::Gauge));
            let changes = t.apply(&started(model), driver);
            assert!(
                t.gauge.is_none(),
                "old context must leave with its agent or model"
            );
            assert!(changes.contains(&Change::Gauge));
        }
    }

    #[test]
    fn the_rate_limit_notice_shows_a_local_time() {
        let today =
            gtk4::glib::DateTime::new(&gtk4::glib::TimeZone::local(), 2026, 10, 14, 13, 30, 0.0)
                .unwrap();
        let now = today.to_unix();
        for (at, format) in [
            (today.add_hours(1).unwrap(), "%H:%M"),
            (today.add_days(1).unwrap(), "%Y-%m-%d %H:%M"),
        ] {
            let expected = format!(
                "Rate limited. The quota resets at {}.",
                at.format(format).unwrap()
            );
            for input in [
                at.to_unix().to_string(),
                agent_core::quota::epoch_to_rfc3339(at.to_unix()),
            ] {
                assert_eq!(rate_limit_notice(Some(&input), now), expected);
            }
        }
        for input in [
            None,
            Some("invalid"),
            Some("9223372036854775807"),
            Some("99999999999999999999-01-01T00:00:00Z"),
        ] {
            assert_eq!(
                rate_limit_notice(input, now),
                "Rate limited by the provider."
            );
        }
    }

    #[test]
    fn a_live_model_change_clears_the_context_gauge() {
        let mut t = Transcript::new();
        t.apply(
            &ev(Event::ModelChanged { model: "m1".into() }),
            Driver::Codex,
        );
        t.apply(
            &ev(Event::UsageUpdated {
                used: 42_000,
                max: Some(200_000),
                auto_compact_at: None,
            }),
            Driver::Codex,
        );
        let same = t.apply(
            &ev(Event::ModelChanged { model: "m1".into() }),
            Driver::Codex,
        );
        assert!(t.gauge.is_some());
        assert!(!same.contains(&Change::Gauge));
        let changed = t.apply(
            &ev(Event::ModelChanged { model: "m2".into() }),
            Driver::Codex,
        );
        assert!(t.gauge.is_none());
        assert!(changed.contains(&Change::Gauge));
    }

    /// The reasoning items' views, in transcript order.
    fn reasoning_views(t: &Transcript) -> Vec<(ReasoningView, String)> {
        let mut items: Vec<&Item> = t.items.values().collect();
        items.sort_by_key(|i| t.order.iter().position(|id| *id == i.id));
        items
            .into_iter()
            .filter_map(|i| match &i.body {
                Body::Reasoning {
                    text,
                    streaming,
                    state,
                    label,
                } => Some((
                    reasoning_view(text, *streaming, *state, *label),
                    text.clone(),
                )),
                _ => None,
            })
            .collect()
    }

    /// The recorded Claude turn (`--thinking-display summarized` on 2.1.293),
    /// through the adapter into the model, renders each thinking block as a non-empty Complete
    /// "Thought process"; the same events loaded from the store render Replayed.
    #[test]
    fn w0_claude_thinking_renders_complete_live_and_replayed_from_history() {
        use agent_core::adapter::Adapter;
        let fixture = include_str!("../../../tests/fixtures/real/claude/thinking-and-touch.jsonl");
        let mut adapter = agent_core::claude::ClaudeAdapter::new();
        let envs: Vec<Envelope> = fixture
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|rec| rec["dir"] == "out")
            .flat_map(|rec| adapter.feed(&rec["frame"].to_string()))
            .collect();
        let mut live = Transcript::new();
        for env in &envs {
            live.apply(env, Driver::Claude);
        }
        let views = reasoning_views(&live);
        assert_eq!(views.len(), 2);
        for (view, text) in &views {
            assert!(!text.trim().is_empty());
            assert_eq!(view.state, ReasoningState::Complete);
            assert_eq!(view.title, "Thought process");
            assert!(view.visible && view.expandable);
        }
        let mut replayed = Transcript::new();
        for env in &envs {
            replayed.apply(env, Driver::Claude);
        }
        replayed.mark_replayed();
        for (view, text) in reasoning_views(&replayed) {
            assert!(!text.trim().is_empty());
            assert_eq!(view.state, ReasoningState::Replayed);
            assert!(view.hint.is_some() && view.expandable);
        }
    }

    /// The one widget's four states, and its fallbacks when the adapter says nothing.
    #[test]
    fn reasoning_view_covers_the_four_states() {
        use ReasoningLabel::{Summary, Thinking};
        use ReasoningState::*;
        let v = |text: &str, streaming, state, label| reasoning_view(text, streaming, state, label);
        let streaming = v("hm", true, Some(Streaming), Thinking);
        assert_eq!((streaming.state, streaming.title), (Streaming, "Thinking…"));
        let complete = v("hm", false, Some(Complete), Thinking);
        assert_eq!(
            (complete.state, complete.title),
            (Complete, "Thought process")
        );
        let replayed = v("hm", false, Some(Replayed), Thinking);
        assert_eq!(replayed.state, Replayed);
        assert!(replayed.hint.is_some() && replayed.expandable);
        let unavailable = v("", false, Some(Unavailable), Thinking);
        assert_eq!(unavailable.state, Unavailable);
        assert_eq!(unavailable.title, "Thinking not shared by this model");
        assert!(unavailable.visible && !unavailable.expandable);
        // Codex's label.
        assert_eq!(v("s", true, None, Summary).title, "Summarizing reasoning…");
        assert_eq!(v("s", false, None, Summary).title, "Reasoning summary");
        // Nothing said: follows `streaming`; an empty block takes no space.
        assert_eq!(v("x", true, None, Thinking).state, Streaming);
        assert_eq!(v("x", false, None, Thinking).state, Complete);
        assert!(!v("", false, None, Thinking).visible);
        assert!(!v("", true, Some(Streaming), Thinking).visible);
        // Cut off mid-stream: no longer "Thinking…".
        assert_eq!(v("x", false, Some(Streaming), Thinking).state, Complete);
        // Text that arrived wins over "not shared".
        assert_eq!(v("x", false, Some(Unavailable), Thinking).state, Complete);
    }

    #[test]
    fn reasoning_events_set_the_blocks_state_and_replay_keeps_not_shared() {
        let mut t = Transcript::new();
        for (id, state) in [
            ("r1", ReasoningState::Complete),
            ("r2", ReasoningState::Unavailable),
        ] {
            t.apply(&started(id, ItemKind::Reasoning, None), Driver::Agy);
            let changes = t.apply(
                &ev(Event::Reasoning {
                    state,
                    label: ReasoningLabel::Thinking,
                })
                .item(id),
                Driver::Agy,
            );
            assert_eq!(changes, [Change::Updated(id.into())]);
            t.apply(
                &ev(Event::ContentSnapshot {
                    stream: StreamKind::Reasoning,
                    text: if id == "r1" {
                        "whole".into()
                    } else {
                        String::new()
                    },
                })
                .item(id),
                Driver::Agy,
            );
            t.apply(
                &ev(Event::ItemCompleted {
                    status: ItemStatus::Completed,
                    output: None,
                    error: None,
                })
                .item(id),
                Driver::Agy,
            );
        }
        t.mark_replayed();
        let states: Vec<ReasoningState> = reasoning_views(&t)
            .into_iter()
            .map(|(v, _)| v.state)
            .collect();
        assert_eq!(
            states,
            [ReasoningState::Replayed, ReasoningState::Unavailable]
        );
    }

    fn plan_steps(status: StepStatus) -> Vec<PlanStep> {
        vec![PlanStep {
            text: "Verify the synthetic change".into(),
            status,
        }]
    }

    fn finish_turn(state: TurnState) -> Envelope {
        Envelope::turn_completed(state, None)
    }

    fn plans(t: &Transcript) -> Vec<(&[PlanStep], Option<TurnState>)> {
        t.order()
            .iter()
            .filter_map(|id| match &t.get(id)?.body {
                Body::Plan { steps, outcome } => Some((steps.as_slice(), *outcome)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn completed_plans_retire_immediately_and_survive_replay_and_a_new_plan() {
        let events = [
            ev(Event::TurnStarted { model: None }),
            ev(Event::PlanUpdated {
                steps: plan_steps(StepStatus::Pending),
            }),
            ev(Event::PlanUpdated {
                steps: plan_steps(StepStatus::InProgress),
            }),
            ev(Event::PlanUpdated {
                steps: plan_steps(StepStatus::Completed),
            }),
            finish_turn(TurnState::Completed),
        ];
        let mut t = Transcript::new();
        for event in &events[..3] {
            t.apply(event, Driver::Codex);
            if matches!(&event.event, Event::PlanUpdated { .. }) {
                assert!(plan_is_active(&t.plan));
            }
        }
        t.apply(&events[3], Driver::Codex);
        assert!(t.running, "completion occurs before the turn ends");
        assert!(!plan_is_active(&t.plan));
        assert_eq!(
            plans(&t),
            [(plan_steps(StepStatus::Completed).as_slice(), None)]
        );
        // A provider can repeat its final snapshot without making another history row.
        t.apply(&events[3], Driver::Codex);
        assert_eq!(plans(&t).len(), 1);
        t.apply(&events[4], Driver::Codex);
        assert!(t.plan.is_empty());
        let mut replayed = Transcript::new();
        for event in &events {
            replayed.apply(event, Driver::Codex);
        }
        assert!(!replayed.settle_stale());
        assert_eq!(plans(&t), plans(&replayed));
        replayed.apply(&ev(Event::TurnStarted { model: None }), Driver::Codex);
        assert!(!plan_is_active(&replayed.plan));
        replayed.apply(
            &ev(Event::PlanUpdated {
                steps: plan_steps(StepStatus::Pending),
            }),
            Driver::Codex,
        );
        assert!(plan_is_active(&replayed.plan));
        assert_eq!(plans(&replayed).len(), 2);
        assert_eq!(plans(&replayed)[0].0[0].status, StepStatus::Completed);
        assert_eq!(plans(&replayed)[1].0[0].status, StepStatus::Pending);
    }

    #[test]
    fn failed_interrupted_and_disconnected_plans_preserve_unfinished_steps() {
        for state in [
            TurnState::Failed,
            TurnState::Interrupted,
            TurnState::Completed,
        ] {
            let mut t = Transcript::new();
            t.apply(&ev(Event::TurnStarted { model: None }), Driver::Codex);
            t.apply(
                &ev(Event::PlanUpdated {
                    steps: plan_steps(StepStatus::InProgress),
                }),
                Driver::Codex,
            );
            t.apply(&finish_turn(state), Driver::Codex);
            assert!(t.plan.is_empty());
            assert_eq!(plans(&t)[0].0[0].status, StepStatus::InProgress);
            assert_eq!(plans(&t)[0].1, Some(state));
        }
        for stale in [false, true] {
            let mut t = Transcript::new();
            t.apply(&ev(Event::TurnStarted { model: None }), Driver::Codex);
            t.apply(
                &ev(Event::PlanUpdated {
                    steps: plan_steps(StepStatus::Pending),
                }),
                Driver::Codex,
            );
            if stale {
                assert!(t.settle_stale());
                assert!(!t.settle_stale());
            } else {
                t.apply(
                    &ev(Event::SessionExited {
                        code: Some(1),
                        expected: false,
                    }),
                    Driver::Codex,
                );
            }
            assert!(t.plan.is_empty());
            assert_eq!(plans(&t)[0].0[0].status, StepStatus::Pending);
            assert_eq!(
                plans(&t)[0].1,
                Some(if stale {
                    TurnState::Interrupted
                } else {
                    TurnState::Failed
                })
            );
        }
    }

    #[test]
    fn a_replacement_plan_in_the_same_turn_keeps_completed_history() {
        let mut t = Transcript::new();
        t.apply(&ev(Event::TurnStarted { model: None }), Driver::Codex);
        for status in [
            StepStatus::InProgress,
            StepStatus::Completed,
            StepStatus::Pending,
        ] {
            t.apply(
                &ev(Event::PlanUpdated {
                    steps: plan_steps(status),
                }),
                Driver::Codex,
            );
        }
        assert_eq!(plans(&t).len(), 2);
        assert_eq!(plans(&t)[0].0[0].status, StepStatus::Completed);
        assert_eq!(plans(&t)[1].0[0].status, StepStatus::Pending);
    }

    #[test]
    fn a_late_plan_update_cannot_reopen_a_terminal_or_stale_turn() {
        for state in [
            TurnState::Completed,
            TurnState::Interrupted,
            TurnState::Failed,
        ] {
            let mut t = Transcript::new();
            t.apply(&ev(Event::TurnStarted { model: None }), Driver::Codex);
            t.apply(
                &ev(Event::PlanUpdated {
                    steps: plan_steps(StepStatus::InProgress),
                }),
                Driver::Codex,
            );
            t.apply(&finish_turn(state), Driver::Codex);
            let history = plans(&t)
                .into_iter()
                .map(|(steps, outcome)| (steps.to_vec(), outcome))
                .collect::<Vec<_>>();
            assert!(t
                .apply(
                    &ev(Event::PlanUpdated {
                        steps: plan_steps(StepStatus::Pending)
                    }),
                    Driver::Codex,
                )
                .is_empty());
            assert!(t.plan.is_empty());
            assert_eq!(
                plans(&t),
                history
                    .iter()
                    .map(|(steps, outcome)| (steps.as_slice(), *outcome))
                    .collect::<Vec<_>>()
            );
        }
        let mut t = Transcript::new();
        t.apply(&ev(Event::TurnStarted { model: None }), Driver::Codex);
        t.apply(
            &ev(Event::PlanUpdated {
                steps: plan_steps(StepStatus::InProgress),
            }),
            Driver::Codex,
        );
        t.settle_stale();
        assert!(t
            .apply(
                &ev(Event::PlanUpdated {
                    steps: plan_steps(StepStatus::Completed)
                }),
                Driver::Codex
            )
            .is_empty());
        assert!(t.plan.is_empty());
        assert_eq!(plans(&t)[0].0[0].status, StepStatus::InProgress);
        assert_eq!(plans(&t)[0].1, Some(TurnState::Interrupted));
    }

    fn worker_state(state: WorkerState, waiting: bool) -> Envelope {
        ev(Event::WorkersUpdated {
            workers: vec![WorkerSnapshot {
                id: "child-1".into(),
                name: Some("Mira".into()),
                task: Some("Review the composer".into()),
                state,
                activity: Some("Inspecting controls".into()),
                delegation: None,
            }],
            waiting_for: if waiting {
                vec!["child-1".into()]
            } else {
                Vec::new()
            },
        })
    }

    #[test]
    fn deferred_mode_keeps_current_policy_until_adoption() {
        let mut t = Transcript::new();
        t.apply(&ev(Event::ModeChanged { mode: Mode::Ask }), Driver::Codex);
        t.apply(
            &ev(Event::ModeChangeDeferred {
                requested: Mode::AcceptEdits,
                effective: Mode::Ask,
            }),
            Driver::Codex,
        );
        assert_eq!(t.mode, Some(Mode::Ask));
        assert_eq!(t.pending_mode, Some(Mode::AcceptEdits));
        t.apply(
            &ev(Event::ModeChanged {
                mode: Mode::AcceptEdits,
            }),
            Driver::Codex,
        );
        assert_eq!(t.mode, Some(Mode::AcceptEdits));
        assert_eq!(t.pending_mode, None);
    }

    #[test]
    fn stale_or_exited_session_adopts_deferred_mode_as_the_idle_default() {
        let mut stale = Transcript::new();
        let deferred = ev(Event::ModeChangeDeferred {
            requested: Mode::AcceptEdits,
            effective: Mode::Ask,
        });
        stale.apply(&deferred, Driver::Codex);
        let mut exited = Transcript::new();
        exited.apply(&deferred, Driver::Codex);
        assert!(stale.settle_stale());
        assert_eq!(stale.mode, Some(Mode::AcceptEdits));
        assert_eq!(stale.pending_mode, None);
        assert!(!stale.settle_stale(), "settlement is idempotent");
        let changes = exited.apply(
            &ev(Event::SessionExited {
                code: Some(0),
                expected: true,
            }),
            Driver::Codex,
        );
        assert_eq!(exited.mode, Some(Mode::AcceptEdits));
        assert_eq!(exited.pending_mode, None);
        assert!(changes.iter().any(|change| matches!(change, Change::Mode)));
    }

    /// A replayed or exited dangerous permissive mode is not shown as running: no process
    /// starts in it, so the header falls back to the session's own mode.
    #[test]
    fn dangerous_permissive_mode_is_forgotten_with_its_process() {
        let bypass = ev(Event::ModeChanged { mode: Mode::Bypass });
        let mut stale = Transcript::new();
        stale.apply(&bypass, Driver::Claude);
        assert_eq!(stale.mode, Some(Mode::Bypass));
        assert!(stale.settle_stale());
        assert_eq!(stale.mode, None);
        let mut exited = Transcript::new();
        exited.apply(&bypass, Driver::Claude);
        let changes = exited.apply(
            &ev(Event::SessionExited {
                code: Some(0),
                expected: true,
            }),
            Driver::Claude,
        );
        assert_eq!(exited.mode, None);
        assert!(changes.iter().any(|change| matches!(change, Change::Mode)));
        // A safe mode stays as it was.
        let mut ask = Transcript::new();
        ask.apply(&ev(Event::ModeChanged { mode: Mode::Ask }), Driver::Claude);
        ask.settle_stale();
        assert_eq!(ask.mode, Some(Mode::Ask));
    }

    #[test]
    fn old_codex_collaboration_calls_are_not_counted_as_workers() {
        let mut t = Transcript::new();
        t.apply(
            &started("old-spawn", ItemKind::Subagent, None),
            Driver::Codex,
        );
        assert!(t.subagents().is_empty());
        assert!(
            matches!(t.get("old-spawn").map(|i| &i.body), Some(Body::Tool(tool)) if tool.kind == ItemKind::Tool)
        );
        t.apply(&worker_state(WorkerState::Running, false), Driver::Codex);
        assert_eq!(t.subagents().len(), 1);
    }

    #[test]
    fn worker_wait_is_visible_during_a_turn_and_results_do_not_close_workers() {
        let mut t = Transcript::new();
        t.apply(&ev(Event::TurnStarted { model: None }), Driver::Codex);
        assert_eq!(t.activity(true), Activity::Working { background: 0 });
        t.apply(&worker_state(WorkerState::Running, true), Driver::Codex);
        assert_eq!(
            t.activity(true),
            Activity::WaitingForWorkers {
                names: vec!["Mira".into()]
            }
        );
        let first_id = t.subagents()[0].id.clone();
        assert!(t.subagents()[0].active());
        t.apply(&worker_state(WorkerState::Completed, false), Driver::Codex);
        let done = t.subagents();
        assert_eq!(done.len(), 1, "follow-up operations update the same worker");
        assert_eq!(done[0].id, first_id);
        assert_eq!(done[0].task, "Review the composer");
        assert_eq!(done[0].state_label(), "completed");
        assert!(done[0].finished());
        assert!(!done[0].active());
        t.apply(&worker_state(WorkerState::Closed, false), Driver::Codex);
        assert_eq!(t.subagents()[0].state_label(), "closed");
        assert_eq!(
            t.order().len(),
            0,
            "worker details are not duplicate operation transcript cards"
        );
    }

    #[test]
    fn sparse_worker_snapshots_preserve_known_metadata_and_terminal_state() {
        for state in [
            WorkerState::Completed,
            WorkerState::Failed,
            WorkerState::Stopped,
            WorkerState::Closed,
        ] {
            let mut t = Transcript::new();
            t.apply(&worker_state(state, false), Driver::Codex);
            t.apply(
                &ev(Event::WorkersUpdated {
                    workers: vec![WorkerSnapshot {
                        id: "child-1".into(),
                        name: Some("  ".into()),
                        task: None,
                        state: WorkerState::Unknown,
                        activity: Some(String::new()),
                        delegation: None,
                    }],
                    waiting_for: Vec::new(),
                }),
                Driver::Codex,
            );
            let workers = t.subagents();
            assert_eq!(workers.len(), 1);
            assert_eq!(workers[0].name, "Mira");
            assert_eq!(workers[0].task, "Review the composer");
            assert_eq!(workers[0].activity.as_deref(), Some("Inspecting controls"));
            assert_eq!(workers[0].lifecycle, Some(state));
        }
    }

    #[test]
    fn explicit_worker_restart_replaces_terminal_state_and_retains_last_metadata() {
        let mut t = Transcript::new();
        t.apply(&worker_state(WorkerState::Closed, false), Driver::Codex);
        t.apply(
            &ev(Event::WorkersUpdated {
                workers: vec![WorkerSnapshot {
                    id: "child-1".into(),
                    name: None,
                    task: Some("Review the new task".into()),
                    state: WorkerState::Running,
                    activity: None,
                    delegation: None,
                }],
                waiting_for: Vec::new(),
            }),
            Driver::Codex,
        );
        let workers = t.subagents();
        assert_eq!(workers[0].name, "Mira");
        assert_eq!(workers[0].task, "Review the new task");
        assert_eq!(workers[0].activity.as_deref(), Some("Inspecting controls"));
        assert_eq!(workers[0].lifecycle, Some(WorkerState::Running));
        assert!(workers[0].active());
    }

    #[test]
    fn active_workers_survive_main_completion_but_replay_settles_their_live_state() {
        let mut t = Transcript::new();
        t.apply(&ev(Event::TurnStarted { model: None }), Driver::Codex);
        t.apply(&worker_state(WorkerState::Running, false), Driver::Codex);
        t.apply(
            &ev(Event::TurnCompleted {
                state: TurnState::Completed,
                usage: None,
                cost_usd: None,
                error: None,
            }),
            Driver::Codex,
        );
        assert_eq!(
            t.activity(false),
            Activity::Waiting {
                tasks: vec!["Mira".into()]
            }
        );
        assert!(t.settle_stale());
        assert!(!t.subagents()[0].active());
        assert_eq!(t.subagents()[0].state_label(), "status unknown");
        assert_eq!(t.activity(false), Activity::Finished);
    }

    #[test]
    fn a_stopping_worker_stays_active_until_its_exit_is_observed() {
        let mut t = Transcript::new();
        t.apply(&ev(Event::TurnStarted { model: None }), Driver::Codex);
        t.apply(&worker_state(WorkerState::Stopping, false), Driver::Codex);
        t.apply(
            &ev(Event::TurnCompleted {
                state: TurnState::Completed,
                usage: None,
                cost_usd: None,
                error: None,
            }),
            Driver::Codex,
        );
        let worker = &t.subagents()[0];
        assert_eq!(worker.lifecycle, Some(WorkerState::Stopping));
        assert!(worker.active());
        assert!(!worker.finished());
        assert_eq!(worker.state_label(), "stopping");
        assert_eq!(
            t.activity(false),
            Activity::Waiting {
                tasks: vec!["Mira".into()]
            }
        );
        t.apply(&worker_state(WorkerState::Stopped, false), Driver::Codex);
        assert!(t.subagents()[0].finished());
    }

    #[test]
    fn failed_turn_is_not_reported_as_success_and_reasoning_is_explicit() {
        let mut t = Transcript::new();
        t.apply(&ev(Event::TurnStarted { model: None }), Driver::Codex);
        assert_eq!(t.activity(true), Activity::Working { background: 0 });
        t.apply(
            &ev(Event::ItemStarted {
                kind: ItemKind::Reasoning,
                title: "reasoning".into(),
                input: None,
                parent: None,
            })
            .item("r1"),
            Driver::Codex,
        );
        assert_eq!(t.activity(true), Activity::Thinking);
        t.apply(
            &ev(Event::TurnCompleted {
                state: TurnState::Failed,
                usage: None,
                cost_usd: None,
                error: Some("test failure".into()),
            }),
            Driver::Codex,
        );
        assert_eq!(t.activity(false), Activity::Failed);
    }

    fn approval_for(step: &str, request: &str) -> Envelope {
        Envelope::new(Event::ApprovalRequested {
            tool: "Bash".into(),
            title: None,
            input: json!({"command": "cargo test"}),
            reason: None,
            options: vec![Decision::Allow, Decision::Deny],
            response: ResponseCapability::Live,
            remembers: None,
        })
        .item(step)
        .request(request)
    }

    /// The approval reads before the step it let run, so a failure lands below "Allowed", not
    /// above it. A step inside a sub-agent keeps its approval top-level, where the shelf finds it.
    #[test]
    fn approvals_read_before_the_step_they_ask_about() {
        let mut t = Transcript::new();
        t.apply(&started("u1", ItemKind::UserMessage, None), Driver::Claude);
        t.apply(&started("t1", ItemKind::Command, None), Driver::Claude);
        t.apply(&approval_for("t1", "r1"), Driver::Claude);
        assert_eq!(t.order(), ["u1", "approval:r1", "t1"]);

        t.apply(&started("s1", ItemKind::Subagent, None), Driver::Claude);
        t.apply(
            &started("c1", ItemKind::Command, Some("s1")),
            Driver::Claude,
        );
        t.apply(&approval_for("c1", "r2"), Driver::Claude);
        assert_eq!(
            t.order(),
            ["u1", "approval:r1", "t1", "s1", "approval:r2"],
            "a sub-agent's step is not an anchor"
        );
        assert!(matches!(
            t.pending_interruption(),
            Some(PendingInterruption::Approval { request, .. }) if request == "r2"
        ));

        t.apply(&approval_for("nope", "r3"), Driver::Claude);
        assert_eq!(t.order().last().map(String::as_str), Some("approval:r3"));
    }

    fn started(id: &str, kind: ItemKind, parent: Option<&str>) -> Envelope {
        Envelope::item_started(id, kind, format!("title {id}"), None, parent)
    }

    fn delta(id: Option<&str>, stream: StreamKind, text: &str) -> Envelope {
        let e = Envelope::new(Event::ContentDelta {
            stream,
            text: text.into(),
        });
        match id {
            Some(id) => e.item(id),
            None => e,
        }
    }

    /// A finished sub-agent's steps still running stop (their completions were lost); those of
    /// a sub-agent still at work, of a nested sub-agent still running, and of a background
    /// sub-agent keep running.
    #[test]
    fn a_finished_subagent_settles_its_steps_left_running() {
        let status = |t: &Transcript, id: &str| match t.get(id).map(|i| &i.body) {
            Some(Body::Tool(tool)) => tool.status,
            other => panic!("{id}: {other:?}"),
        };
        let done = |id: &str| Envelope::item_completed(id, ItemStatus::Completed, None, None);
        let mut t = Transcript::new();
        for env in [
            started("agent", ItemKind::Subagent, None),
            started("lost", ItemKind::Command, Some("agent")),
            started("ok", ItemKind::Command, Some("agent")),
            done("ok"),
            started("nested", ItemKind::Subagent, Some("agent")),
            started("deep", ItemKind::Command, Some("nested")),
        ] {
            t.apply(&env, Driver::Claude);
        }
        assert_eq!(
            status(&t, "lost"),
            ToolStatus::Running,
            "its agent still works"
        );
        let changes = t.apply(&done("agent"), Driver::Claude);
        assert!(changes.contains(&Change::Updated("lost".into())));
        assert_eq!(status(&t, "lost"), ToolStatus::Interrupted);
        assert_eq!(status(&t, "ok"), ToolStatus::Completed);
        assert_eq!(status(&t, "nested"), ToolStatus::Running);
        assert_eq!(
            status(&t, "deep"),
            ToolStatus::Running,
            "its own agent runs"
        );

        let mut t = Transcript::new();
        let background = Envelope::item_started(
            "bg",
            ItemKind::Subagent,
            "Agent",
            Some(serde_json::json!({ "run_in_background": true })),
            None,
        );
        t.apply(&background, Driver::Claude);
        t.apply(
            &started("step", ItemKind::Command, Some("bg")),
            Driver::Claude,
        );
        t.apply(&done("bg"), Driver::Claude);
        assert_eq!(
            status(&t, "step"),
            ToolStatus::Running,
            "a background sub-agent works on after its call returns"
        );
    }

    #[test]
    fn streaming_text_accumulates_and_snapshot_replaces() {
        let mut t = Transcript::new();
        let c = t.apply(
            &started("m1", ItemKind::AssistantMessage, None),
            Driver::Claude,
        );
        assert_eq!(c, vec![Change::Added("m1".into())]);
        t.apply(
            &delta(Some("m1"), StreamKind::Assistant, "Hel"),
            Driver::Claude,
        );
        let c = t.apply(
            &delta(Some("m1"), StreamKind::Assistant, "lo"),
            Driver::Claude,
        );
        assert_eq!(c, vec![Change::Updated("m1".into())]);
        let text = |t: &Transcript| match &t.get("m1").map(|i| i.body.clone()) {
            Some(Body::Assistant { text, .. }) => text.clone(),
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(text(&t), "Hello");
        let snap = Envelope::snapshot("m1", StreamKind::Assistant, "Hello, world");
        t.apply(&snap, Driver::Claude);
        assert_eq!(text(&t), "Hello, world");
        // An identical snapshot is not a change.
        assert!(t.apply(&snap, Driver::Claude).is_empty());
    }

    #[test]
    fn itemless_deltas_open_an_implicit_assistant_item() {
        let mut t = Transcript::new();
        let c = t.apply(&delta(None, StreamKind::Assistant, "a"), Driver::Agy);
        assert_eq!(c.len(), 2);
        let Change::Added(id) = &c[0] else {
            panic!("expected Added, got {c:?}")
        };
        let id = id.clone();
        t.apply(&delta(None, StreamKind::Assistant, "b"), Driver::Agy);
        assert_eq!(t.len(), 1);
        match &t.get(&id).map(|i| i.body.clone()) {
            Some(Body::Assistant { text, driver, .. }) => {
                assert_eq!(text, "ab");
                assert_eq!(*driver, Driver::Agy);
            }
            other => panic!("unexpected {other:?}"),
        }
        // Tool text for an unknown tool is dropped, not turned into a row.
        assert!(t
            .apply(&delta(None, StreamKind::ToolOutput, "x"), Driver::Agy)
            .is_empty());
    }

    #[test]
    fn the_explorer_lists_every_subagent_in_order_with_its_task_and_steps() {
        let mut t = Transcript::new();
        let task = |id: &str, parent: Option<&str>, input: serde_json::Value| {
            Envelope::item_started(id, ItemKind::Subagent, "Task", Some(input), parent)
        };
        t.apply(&started("u", ItemKind::UserMessage, None), Driver::Claude);
        t.apply(
            &task(
                "a",
                None,
                json!({"subagent_type": "Explore", "description": "Find the parser",
                       "model": "sonnet", "effort": "high"}),
            ),
            Driver::Claude,
        );
        t.apply(
            &started("a1", ItemKind::FileRead, Some("a")),
            Driver::Claude,
        );
        // A sub-agent of a sub-agent, named only by its prompt.
        t.apply(
            &task(
                "b",
                Some("a"),
                json!({"prompt": "Check the tests\nthen report"}),
            ),
            Driver::Claude,
        );
        t.apply(&started("c", ItemKind::Command, None), Driver::Claude);
        let list = t.subagents();
        let ids: Vec<_> = list.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(list[0].name, "Explore");
        assert_eq!(list[0].task, "Find the parser");
        assert_eq!(list[0].steps, 2, "the read and the nested agent");
        assert_eq!(list[0].status, ToolStatus::Running);
        assert_eq!(list[1].name, "Task", "no type: the tool's title");
        assert_eq!(list[1].task, "Check the tests");
        assert_eq!(list[0].requested_model.as_deref(), Some("sonnet"));
        assert_eq!(list[0].requested_effort.as_deref(), Some("high"));
        assert_eq!(list[1].requested_model, None, "none asked for: none shown");
        assert_eq!(list[1].requested_effort, None);
        assert_eq!(list[0].reported_model, None, "nothing reported yet");
    }

    #[test]
    fn a_subagent_keeps_the_model_its_provider_reported() {
        let mut t = Transcript::new();
        t.apply(&started("a", ItemKind::Subagent, None), Driver::Claude);
        t.apply(&started("b", ItemKind::Subagent, Some("a")), Driver::Claude);
        t.apply(&started("c", ItemKind::Command, None), Driver::Claude);
        let report = |id: &str, model: &str| {
            Envelope::new(Event::SubagentModel {
                model: model.into(),
            })
            .item(id)
        };
        assert_eq!(
            t.apply(&report("b", "claude-haiku-5-5"), Driver::Claude),
            [Change::Updated("b".into())]
        );
        assert!(
            t.apply(&report("b", "claude-haiku-5-5"), Driver::Claude)
                .is_empty(),
            "the same model again changes nothing"
        );
        // Not a sub-agent, or no such item: ignored.
        assert!(t.apply(&report("c", "m"), Driver::Claude).is_empty());
        assert!(t.apply(&report("gone", "m"), Driver::Claude).is_empty());
        let list = t.subagents();
        assert_eq!(list[0].reported_model, None);
        assert_eq!(list[1].reported_model.as_deref(), Some("claude-haiku-5-5"));
        assert_eq!(
            t.apply(&report("b", "claude-sonnet-5-5"), Driver::Claude),
            [Change::Updated("b".into())]
        );
        assert_eq!(
            t.subagents()[1].reported_model.as_deref(),
            Some("claude-sonnet-5-5")
        );
    }

    #[test]
    fn a_live_subagent_is_named_from_its_streamed_input_as_soon_as_each_field_arrives() {
        // Live Claude: the card starts with no input; the JSON streams in after.
        let mut t = Transcript::new();
        t.apply(&started("agent", ItemKind::Subagent, None), Driver::Claude);
        let stream = |t: &mut Transcript, text: &str| {
            t.apply(
                &delta(Some("agent"), StreamKind::ToolInput, text),
                Driver::Claude,
            );
        };
        let first = |t: &Transcript| t.subagents().into_iter().next().expect("listed");
        stream(&mut t, r#"{"description": "Find the par"#);
        assert_eq!(first(&t).task, "", "never a half-streamed description");
        stream(&mut t, r#"ser \"fast\"", "subagent_type": "Expl"#);
        assert_eq!(first(&t).task, r#"Find the parser "fast""#);
        assert_eq!(
            first(&t).name,
            "title agent",
            "type not complete yet: the title"
        );
        stream(&mut t, r#"ore", "prompt": "Look at src/"}"#);
        let s = first(&t);
        assert_eq!(
            (s.name.as_str(), s.task.as_str()),
            ("Explore", r#"Find the parser "fast""#)
        );
    }

    #[test]
    fn subagent_children_nest_under_their_parent() {
        let mut t = Transcript::new();
        t.apply(&started("task", ItemKind::Subagent, None), Driver::Claude);
        t.apply(
            &started("read", ItemKind::FileRead, Some("task")),
            Driver::Claude,
        );
        t.apply(
            &started("orphan", ItemKind::Command, Some("gone")),
            Driver::Claude,
        );
        assert_eq!(t.order(), ["task", "orphan"]);
        let task = t.get("task").expect("task");
        assert_eq!(task.children, ["read"]);
        assert!(task.expanded, "subagent cards start expanded");
        assert_eq!(
            t.get("read").and_then(|i| i.parent.as_deref()),
            Some("task")
        );
        assert_eq!(t.get("orphan").and_then(|i| i.parent.as_deref()), None);
    }

    /// The agent's own sub-agent runs on its agent: the dots light that agent while it runs, and
    /// no other.
    #[test]
    fn running_drivers_follow_the_agents_own_sub_agents() {
        let mut t = Transcript::new();
        t.apply(&ev(Event::TurnStarted { model: None }), Driver::Claude);
        assert!(t.running_drivers(Vec::new()).is_empty());
        t.apply(&started("task", ItemKind::Subagent, None), Driver::Claude);
        assert_eq!(t.running_drivers(Vec::new()), [Driver::Claude]);
        let done = Envelope::item_completed("task", ItemStatus::Completed, None, None);
        t.apply(&done, Driver::Claude);
        assert!(t.running_drivers(Vec::new()).is_empty());
    }

    /// An agent the thread's agent launches from its shell is its sub-agent, on the agent it
    /// launched; any other command is not.
    #[test]
    fn a_shell_launched_agent_is_a_sub_agent() {
        let mut t = Transcript::new();
        t.apply(&ev(Event::TurnStarted { model: None }), Driver::Claude);
        let command = |id: &str, line: &str| {
            Envelope::item_started(
                id,
                ItemKind::Command,
                "Bash",
                Some(serde_json::json!({ "command": line })),
                None,
            )
        };
        t.apply(&command("ls", "ls -la"), Driver::Claude);
        assert!(t.subagents().is_empty());
        t.apply(
            &command("x", "cross-agent agy -m gemini-4-argon \"Review the diff\""),
            Driver::Claude,
        );
        let agents = t.subagents();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].driver, Some(Driver::Agy));
        assert_eq!(agents[0].name, "Antigravity · gemini-4-argon (shell)");
        assert_eq!(agents[0].task, "Review the diff");
        assert_eq!(t.running_drivers(Vec::new()), [Driver::Agy]);
        let done = Envelope::item_completed("x", ItemStatus::Completed, Some("Looks good."), None);
        t.apply(&done, Driver::Claude);
        assert!(t.running_drivers(Vec::new()).is_empty());
        assert_eq!(t.subagents()[0].status, ToolStatus::Completed);
    }

    #[test]
    fn tool_completion_sets_status_output_and_expands_failures() {
        let mut t = Transcript::new();
        t.apply(&started("c1", ItemKind::Command, None), Driver::Claude);
        t.apply(
            &delta(Some("c1"), StreamKind::ToolInput, "{\"command\":"),
            Driver::Claude,
        );
        let done = Envelope::item_completed("c1", ItemStatus::Failed, Some("boom"), Some("exit 1"));
        t.apply(&done, Driver::Claude);
        let item = t.get("c1").expect("c1");
        assert!(item.expanded);
        let Body::Tool(tool) = &item.body else {
            panic!("not a tool")
        };
        assert_eq!(tool.status, ToolStatus::Failed);
        assert_eq!(tool.output, "boom");
        assert_eq!(tool.error.as_deref(), Some("exit 1"));
        assert_eq!(tool.input_text, "{\"command\":");
    }

    #[test]
    fn repeated_tool_start_refines_metadata_without_adding_another_card() {
        let mut t = Transcript::new();
        let pending = Envelope::item_started("ws", ItemKind::WebSearch, "web search", None, None);
        t.apply(&pending, Driver::Codex);
        let refined = Envelope::item_started(
            "ws",
            ItemKind::WebSearch,
            "Rust 1.91",
            Some(serde_json::json!({"query": "Rust 1.91"})),
            None,
        );
        assert_eq!(
            t.apply(&refined, Driver::Codex),
            [Change::Updated("ws".into())]
        );
        assert_eq!(t.order(), ["ws"]);
        let Body::Tool(tool) = &t.get("ws").expect("search").body else {
            panic!("not a tool")
        };
        assert_eq!(tool.title, "Rust 1.91");
        assert_eq!(tool.input, Some(serde_json::json!({"query": "Rust 1.91"})));
    }

    #[test]
    fn expand_state_is_kept_on_the_item() {
        let mut t = Transcript::new();
        t.apply(&started("c1", ItemKind::Command, None), Driver::Claude);
        assert!(!t.get("c1").expect("c1").expanded);
        t.set_expanded("c1", true);
        t.apply(
            &delta(Some("c1"), StreamKind::ToolOutput, "more"),
            Driver::Claude,
        );
        assert!(t.get("c1").expect("c1").expanded);
    }

    #[test]
    fn codex_reconnected_approval_remains_visible_after_history_replay() {
        use agent_core::adapter::{Action, Adapter, Command};
        use agent_core::codex::CodexAdapter;

        let mut t = Transcript::new();
        let historical = approval_for("old-command", "0");
        t.apply(&historical, Driver::Codex);
        t.apply(
            &ev(Event::ApprovalResolved {
                decision: Decision::Allow,
            })
            .request("0"),
            Driver::Codex,
        );
        t.settle_stale();

        for item in ["new-command", "reconnected-command"] {
            let mut adapter = CodexAdapter::new();
            let events = adapter.feed(
                &json!({
                    "id": 0,
                    "method": "item/commandExecution/requestApproval",
                    "params": {
                        "threadId": "thread",
                        "turnId": item,
                        "itemId": item,
                        "command": "git fetch origin --tags",
                        "availableDecisions": ["accept", "cancel"]
                    }
                })
                .to_string(),
            );
            let approval = events
                .iter()
                .find(|env| matches!(env.event, Event::ApprovalRequested { .. }))
                .expect("approval");
            let request = approval.request.as_ref().expect("request");
            assert!(
                t.apply(approval, Driver::Codex)
                    .iter()
                    .any(|change| matches!(change, Change::Added(_))),
                "reused wire ID must create a fresh visible card"
            );
            assert!(
                matches!(t.pending_interruption(), Some(PendingInterruption::Approval { request: pending, .. }) if pending == *request)
            );
            let actions = adapter
                .encode(Command::Approve {
                    request: request.clone(),
                    decision: Decision::Allow,
                    updated_input: None,
                    message: None,
                })
                .expect("answer fresh approval");
            let Action::Write(lines) = &actions[0] else {
                panic!("wire response");
            };
            let response: Value = serde_json::from_str(&lines[0]).expect("JSON");
            assert_eq!(response["id"], 0, "reply retains the original wire ID");
            assert_eq!(response["result"]["decision"], "accept");
            t.apply(
                &ev(Event::ApprovalResolved {
                    decision: Decision::Allow,
                })
                .request(request),
                Driver::Codex,
            );
            assert_eq!(t.pending_interruption(), None);
        }
        assert!(
            matches!(&t.get("approval:0").expect("history retained").body, Body::Approval(a) if a.state == ApprovalState::Resolved(Decision::Allow))
        );
    }

    #[test]
    fn approval_lifecycle() {
        let mut t = Transcript::new();
        let req = Envelope::new(Event::ApprovalRequested {
            tool: "Bash".into(),
            title: None,
            input: json!({"command": "ls"}),
            reason: None,
            options: vec![Decision::Allow, Decision::Deny],
            response: ResponseCapability::Live,
            remembers: None,
        })
        .request("r1");
        let c = t.apply(&req, Driver::Claude);
        assert_eq!(c, vec![Change::Added("approval:r1".into())]);
        // Duplicate requests are ignored.
        assert!(t.apply(&req, Driver::Claude).is_empty());
        assert_eq!(
            t.item_for_request("r1").map(String::as_str),
            Some("approval:r1")
        );
        assert!(matches!(
            t.pending_interruption(),
            Some(PendingInterruption::Approval { ref request, .. }) if request == "r1"
        ));
        t.mark_approval_sent("r1", Decision::Allow);
        assert_eq!(t.pending_interruption(), None);
        let state = |t: &Transcript| match &t.get("approval:r1").expect("card").body {
            Body::Approval(a) => a.state,
            _ => panic!("not an approval"),
        };
        assert_eq!(state(&t), ApprovalState::Sent(Decision::Allow));
        t.apply(
            &ev(Event::ApprovalResolved {
                decision: Decision::Allow,
            })
            .request("r1"),
            Driver::Claude,
        );
        assert_eq!(state(&t), ApprovalState::Resolved(Decision::Allow));
        // A late expiry does not overwrite a resolution.
        t.apply(&ev(Event::ApprovalExpired).request("r1"), Driver::Claude);
        assert_eq!(state(&t), ApprovalState::Resolved(Decision::Allow));
    }

    #[test]
    fn expired_capability_starts_expired_and_pending_expires() {
        let mut t = Transcript::new();
        let mk = |id: &str, response| {
            Envelope::new(Event::ApprovalRequested {
                tool: "Edit".into(),
                title: None,
                input: Value::Null,
                reason: None,
                options: vec![Decision::Allow],
                response,
                remembers: None,
            })
            .request(id)
        };
        t.apply(&mk("a", ResponseCapability::Expired), Driver::Claude);
        t.apply(&mk("b", ResponseCapability::Live), Driver::Claude);
        t.apply(&ev(Event::ApprovalExpired).request("b"), Driver::Claude);
        for id in ["approval:a", "approval:b"] {
            match &t.get(id).expect("card").body {
                Body::Approval(a) => assert_eq!(a.state, ApprovalState::Expired),
                _ => panic!("not an approval"),
            }
        }
    }

    #[test]
    fn questions_resolve() {
        let mut t = Transcript::new();
        t.apply(
            &ev(Event::QuestionRequested {
                questions: vec![],
                source: Default::default(),
            })
            .request("q"),
            Driver::Claude,
        );
        t.mark_questions_sent("q");
        t.apply(
            &ev(Event::QuestionResolved { answered: true }).request("q"),
            Driver::Claude,
        );
        match &t.get("question:q").expect("card").body {
            Body::Question(q) => assert_eq!(q.state, QuestionState::Answered),
            _ => panic!("not a question"),
        }
    }

    fn form_event(form: Form) -> Envelope {
        ev(Event::FormRequested {
            server: "github".into(),
            message: "Which repo?".into(),
            form,
        })
        .request("f")
    }

    fn one_field_form() -> Form {
        Form::Fields {
            fields: vec![form::FormField {
                name: "repo".into(),
                title: None,
                description: None,
                required: true,
                input: form::FieldInput::Text {
                    format: None,
                    min_length: None,
                    max_length: None,
                    default: None,
                },
            }],
        }
    }

    fn form_state(t: &Transcript) -> FormState {
        match &t.get("form:f").expect("form card").body {
            Body::Form(f) => f.state,
            _ => panic!("not a form"),
        }
    }

    /// A form blocks the turn like a question: it is the pending interruption (shelf, sidebar
    /// badge, notification, composer hint) and the thread says it needs an answer.
    #[test]
    fn a_form_is_a_pending_interruption_and_needs_an_answer() {
        let mut t = Transcript::new();
        t.apply(&form_event(one_field_form()), Driver::Codex);
        assert_eq!(
            t.pending_interruption(),
            Some(PendingInterruption::Form {
                request: "f".into(),
                item_id: "form:f".into(),
                server: "github".into(),
                message: "Which repo?".into(),
                link: false,
            })
        );
        assert_eq!(t.activity(true), Activity::NeedsAnswer);
        assert!(t.get("form:f").expect("card").expanded);

        t.mark_form_sent("f");
        assert_eq!(form_state(&t), FormState::Sent);
        assert!(
            t.pending_interruption().is_none(),
            "answered: nothing left to ask"
        );
        t.apply(
            &ev(Event::FormResolved {
                action: FormAction::Accept,
            })
            .request("f"),
            Driver::Codex,
        );
        assert_eq!(form_state(&t), FormState::Resolved(FormAction::Accept));
        assert_ne!(t.activity(true), Activity::NeedsAnswer);
    }

    /// A cancel the view did not start expires the card; the user's own is a dismissal.
    #[test]
    fn a_cancel_the_user_did_not_make_is_an_expiry() {
        let cancel = ev(Event::FormResolved {
            action: FormAction::Cancel,
        })
        .request("f");
        let mut t = Transcript::new();
        t.apply(&form_event(one_field_form()), Driver::Codex);
        t.apply(&cancel, Driver::Codex);
        assert_eq!(form_state(&t), FormState::Expired);
        assert!(t.pending_interruption().is_none());

        let mut own = Transcript::new();
        own.apply(&form_event(one_field_form()), Driver::Codex);
        own.mark_form_sent("f");
        own.apply(&cancel, Driver::Codex);
        assert_eq!(form_state(&own), FormState::Resolved(FormAction::Cancel));
    }

    /// A refused answer reopens the card (pending again, with the refusal noted); a form this
    /// build never offered keeps its own wording whatever later resolves it.
    #[test]
    fn a_refused_answer_reopens_the_card_and_an_unanswerable_one_is_not_relabelled() {
        let mut t = Transcript::new();
        t.apply(&form_event(one_field_form()), Driver::Codex);
        t.mark_form_sent("f");
        t.mark_form_refused("f");
        assert_eq!(form_state(&t), FormState::Pending);
        assert!(matches!(&t.get("form:f").expect("card").body, Body::Form(f) if f.refused));
        assert!(t.pending_interruption().is_some());
        t.mark_form_sent("f");
        assert!(matches!(&t.get("form:f").expect("card").body, Body::Form(f) if !f.refused));
        assert!(
            t.mark_form_refused("nope").is_empty(),
            "an unknown request changes nothing"
        );

        let mut closed = Transcript::new();
        closed.apply(&form_event(Form::Unknown), Driver::Codex);
        closed.apply(
            &ev(Event::FormResolved {
                action: FormAction::Decline,
            })
            .request("f"),
            Driver::Codex,
        );
        assert_eq!(form_state(&closed), FormState::Unanswerable);
        assert!(!form_answerable(&Form::Unknown));
    }

    /// The shelf counts what waits: approvals, questions and answerable forms.
    #[test]
    fn pending_count_counts_every_open_interruption() {
        let mut t = Transcript::new();
        assert_eq!(t.pending_count(), 0);
        t.apply(&form_event(one_field_form()), Driver::Codex);
        t.apply(&permission_request("p", json!([]), true), Driver::Codex);
        t.apply(
            &ev(Event::FormRequested {
                server: "x".into(),
                message: String::new(),
                form: Form::Unknown,
            })
            .request("u"),
            Driver::Codex,
        );
        assert_eq!(
            t.pending_count(),
            2,
            "an unanswerable form waits for nothing"
        );
        t.mark_form_sent("f");
        assert_eq!(t.pending_count(), 1);
    }

    /// A link form reports itself as a link, and a repeated request id is not a second card.
    #[test]
    fn a_link_form_is_pending_once() {
        let mut t = Transcript::new();
        let link = Form::Link {
            url: "https://example.com/login".into(),
        };
        t.apply(&form_event(link.clone()), Driver::Codex);
        t.apply(&form_event(link), Driver::Codex);
        assert_eq!(t.len(), 1);
        assert!(matches!(
            t.pending_interruption(),
            Some(PendingInterruption::Form { link: true, .. })
        ));
    }

    /// Fail closed: a form kind this build does not know, a link that is not a plain web address
    /// and fields that fail the checks are never answerable, so never an interruption.
    #[test]
    fn forms_it_cannot_answer_are_closed_not_pending() {
        for form in [
            Form::Unknown,
            Form::Link {
                url: "javascript:alert(1)".into(),
            },
            Form::Link {
                url: "https://user:pw@example.com/".into(),
            },
            Form::Fields {
                fields: vec![
                    form::FormField {
                        name: "a".into(),
                        title: None,
                        description: None,
                        required: false,
                        input: form::FieldInput::Unknown,
                    },
                    form::FormField {
                        name: "a".into(),
                        title: None,
                        description: None,
                        required: false,
                        input: form::FieldInput::Unknown,
                    },
                ],
            },
        ] {
            let mut t = Transcript::new();
            t.apply(&form_event(form.clone()), Driver::Codex);
            assert_eq!(form_state(&t), FormState::Unanswerable, "{form:?}");
            assert!(t.pending_interruption().is_none(), "{form:?}");
            assert_ne!(t.activity(false), Activity::NeedsAnswer);
        }
    }

    /// History replay: an answered form replays as resolved; one left open with no live agent
    /// expires and is never answerable; no typed answer exists anywhere in the model.
    #[test]
    fn replayed_forms_are_resolved_or_expired_never_answerable() {
        let mut answered = Transcript::new();
        answered.apply(&form_event(one_field_form()), Driver::Codex);
        answered.apply(
            &ev(Event::FormResolved {
                action: FormAction::Decline,
            })
            .request("f"),
            Driver::Codex,
        );
        assert!(!answered.settle_stale(), "already settled");
        assert_eq!(
            form_state(&answered),
            FormState::Resolved(FormAction::Decline)
        );

        let mut open = Transcript::new();
        open.apply(&form_event(one_field_form()), Driver::Codex);
        assert!(open.settle_stale());
        assert_eq!(form_state(&open), FormState::Expired);
        assert!(open.pending_interruption().is_none());
        assert_ne!(open.activity(false), Activity::NeedsAnswer);
    }

    fn permission_request(request: &str, entries: Value, renderable: bool) -> Envelope {
        ev(Event::ApprovalRequested {
            tool: "codex_permissions".into(),
            title: None,
            input: json!({
                "cwd": "/w", "reason": null, "renderable": renderable, "entries": entries,
                "network": true, "session_allowed": true, "warning": null, "requested": {}
            }),
            reason: None,
            options: vec![
                Decision::Allow,
                Decision::AllowForSession,
                Decision::Deny,
                Decision::Cancel,
            ],
            response: ResponseCapability::Live,
            remembers: None,
        })
        .request(request)
    }

    fn reply_chips(request: &str, options: &[&str]) -> Envelope {
        ev(Event::QuestionRequested {
            questions: vec![Question {
                id: "reply".into(),
                header: "Quick replies".into(),
                question: "Which?".into(),
                options: options
                    .iter()
                    .map(|o| agent_core::event::QuestionOption {
                        label: (*o).into(),
                        description: None,
                    })
                    .collect(),
                multi_select: false,
            }],
            source: QuestionSource::Reply,
        })
        .request(request)
    }

    /// "Extra access" follows what the user allowed for the session, and only that: a turn-long
    /// grant, a refusal or an unreadable request add nothing, and a new or ended Codex session
    /// forgets the lot.
    #[test]
    fn extra_access_follows_session_grants_until_the_session_ends() {
        let entries = json!([{"access": "write", "target": {"kind": "path", "path": "/w/out"}}]);
        let resolve = |t: &mut Transcript, request: &str, decision| {
            t.apply(
                &ev(Event::ApprovalResolved { decision }).request(request),
                Driver::Codex,
            );
        };
        let mut t = Transcript::new();
        t.apply(
            &permission_request("a", entries.clone(), true),
            Driver::Codex,
        );
        t.apply(
            &permission_request("b", entries.clone(), true),
            Driver::Codex,
        );
        t.apply(
            &permission_request("c", entries.clone(), false),
            Driver::Codex,
        );
        t.apply(&permission_request("d", entries, true), Driver::Codex);
        resolve(&mut t, "a", Decision::Allow);
        resolve(&mut t, "b", Decision::Deny);
        resolve(&mut t, "c", Decision::AllowForSession);
        assert!(
            t.extra_access().is_empty(),
            "turn grants, refusals and unreadable requests are not session access"
        );
        resolve(&mut t, "d", Decision::AllowForSession);
        assert_eq!(t.extra_access(), ["Write: /w/out", "Network access"]);
        resolve(&mut t, "d", Decision::AllowForSession);
        assert_eq!(t.extra_access().len(), 2, "listed once");

        t.apply(
            &ev(Event::SessionStarted {
                native_id: "n".into(),
                model: None,
                cwd: None,
            }),
            Driver::Codex,
        );
        assert!(t.extra_access().is_empty(), "a new process forgets it");

        t.apply(&permission_request("e", json!([]), true), Driver::Codex);
        let more = json!([{"access": "read", "target": {"kind": "path", "path": "/r"}}]);
        t.apply(&permission_request("f", more, true), Driver::Codex);
        resolve(&mut t, "f", Decision::AllowForSession);
        assert_eq!(t.extra_access(), ["Read: /r", "Network access"]);
        t.apply(
            &ev(Event::SessionExited {
                code: Some(0),
                expected: true,
            }),
            Driver::Codex,
        );
        assert!(t.extra_access().is_empty(), "an ended session forgets it");
    }

    /// A transcript with one finished reply and quick replies under it.
    fn with_chips() -> Transcript {
        let mut t = Transcript::new();
        t.apply(&ev(Event::TurnStarted { model: None }), Driver::Codex);
        t.apply(
            &started("a1", ItemKind::AssistantMessage, None),
            Driver::Codex,
        );
        t.apply(
            &ev(Event::TurnCompleted {
                state: TurnState::Completed,
                usage: None,
                cost_usd: None,
                error: None,
            }),
            Driver::Codex,
        );
        t.apply(
            &reply_chips("reply:a1", &["Keep it", "Drop it"]),
            Driver::Codex,
        );
        t
    }

    /// A reply question is not an interruption: no shelf entry, no NeedsAnswer, and the thread
    /// reads as idle. (The old behaviour rendered every `QuestionRequested` as a pending card.)
    #[test]
    fn a_reply_question_is_not_an_interruption() {
        let t = with_chips();
        assert!(t.has_quick_replies());
        assert_eq!(t.pending_interruption(), None);
        assert!(!matches!(t.activity(false), Activity::NeedsAnswer));
        assert!(t
            .order()
            .iter()
            .all(|id| !matches!(t.get(id).map(|i| &i.body), Some(Body::Question(_)))));
        // An agent's own question still is one.
        let mut t = t;
        t.apply(
            &ev(Event::QuestionRequested {
                questions: vec![],
                source: QuestionSource::Agent,
            })
            .request("q"),
            Driver::Codex,
        );
        assert!(matches!(
            t.pending_interruption(),
            Some(PendingInterruption::Question { .. })
        ));
    }

    #[test]
    fn the_chips_hold_the_option_text_exactly_and_replace_earlier_ones() {
        let mut t = with_chips();
        t.apply(
            &reply_chips("reply:a2", &["One", "Two", "Three"]),
            Driver::Codex,
        );
        let chips: Vec<_> = t
            .order()
            .iter()
            .filter_map(|id| match &t.get(id)?.body {
                Body::QuickReplies(q) => Some((q.request.clone(), q.options.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            chips,
            vec![(
                "reply:a2".to_owned(),
                vec!["One".to_owned(), "Two".to_owned(), "Three".to_owned()]
            )]
        );
    }

    #[test]
    fn a_question_with_no_options_offers_nothing() {
        let mut t = Transcript::new();
        t.apply(&reply_chips("reply:a1", &[]), Driver::Codex);
        assert!(!t.has_quick_replies());
    }

    #[test]
    fn the_chips_go_when_the_user_sends_a_prompt() {
        let mut t = with_chips();
        let changes = t.apply(&started("u1", ItemKind::UserMessage, None), Driver::Codex);
        assert!(!t.has_quick_replies());
        assert!(changes.iter().any(|c| matches!(c, Change::Removed(_))));
    }

    #[test]
    fn the_chips_go_when_a_new_turn_starts() {
        let mut t = with_chips();
        let changes = t.apply(&ev(Event::TurnStarted { model: None }), Driver::Codex);
        assert!(!t.has_quick_replies());
        assert!(changes.iter().any(|c| matches!(c, Change::Removed(_))));
    }

    #[test]
    fn dismiss_takes_the_chips_away_and_a_second_dismiss_is_a_no_op() {
        let mut t = with_chips();
        assert_eq!(t.dismiss_quick_replies().len(), 1);
        assert!(!t.has_quick_replies());
        assert!(t.dismiss_quick_replies().is_empty());
    }

    #[test]
    fn a_replayed_start_of_an_existing_prompt_leaves_the_chips() {
        // History replays events the transcript already holds; only a new prompt supersedes.
        let mut t = Transcript::new();
        t.apply(&started("u1", ItemKind::UserMessage, None), Driver::Codex);
        t.apply(&reply_chips("reply:a1", &["A", "B"]), Driver::Codex);
        t.apply(&started("u1", ItemKind::UserMessage, None), Driver::Codex);
        assert!(t.has_quick_replies());
    }

    /// What a reduced thread shows of its agents: who each reply is credited to, and each
    /// divider's agent and whether it says the agent changed.
    fn credits(t: &Transcript) -> Vec<(String, Driver, bool)> {
        t.order()
            .iter()
            .filter_map(|id| match &t.get(id)?.body {
                Body::Assistant { driver, .. } => Some((id.clone(), *driver, false)),
                Body::Switch {
                    driver,
                    agent_changed,
                    ..
                } => Some((id.clone(), *driver, *agent_changed)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_replay_with_each_events_agent_credits_replies_as_live_did() {
        let session = |model: &str| {
            ev(Event::SessionStarted {
                native_id: "n".into(),
                model: Some(model.into()),
                cwd: None,
            })
        };
        // A thread on Claude that switched to agy, each event with the agent it came from (as
        // the store records it by provider thread).
        let stream = [
            (Driver::Claude, session("opus")),
            (
                Driver::Claude,
                started("c1", ItemKind::AssistantMessage, None),
            ),
            (Driver::Agy, session("gemini")),
            (Driver::Agy, started("a1", ItemKind::AssistantMessage, None)),
        ];
        let mut live = Transcript::new();
        for (driver, env) in &stream {
            live.apply(env, *driver);
        }
        let mut replayed = Transcript::new();
        for (driver, env) in &stream {
            replayed.apply(env, *driver);
        }
        assert_eq!(credits(&replayed), credits(&live));
        assert_eq!(
            credits(&live),
            [
                ("c1".to_owned(), Driver::Claude, false),
                ("switch:1".to_owned(), Driver::Agy, true),
                ("a1".to_owned(), Driver::Agy, false),
            ]
        );
        // The old replay, every event credited to the current agent, got this wrong.
        let mut flat = Transcript::new();
        for (_, env) in &stream {
            flat.apply(env, Driver::Agy);
        }
        assert_ne!(credits(&flat), credits(&live));
    }

    #[test]
    fn a_switchs_notices_read_under_its_divider_which_then_names_the_model() {
        let start = |m: &str| {
            ev(Event::SessionStarted {
                native_id: "n".into(),
                model: Some(m.into()),
                cwd: None,
            })
        };
        let mut t = Transcript::new();
        t.apply(&start("opus"), Driver::Claude);
        t.apply(
            &started("c1", ItemKind::AssistantMessage, None),
            Driver::Claude,
        );
        // What the session says as it switches, as the new agent, before that agent starts.
        let notice = ev(Event::Notice {
            text: "Continuing in Antigravity with all 2 earlier messages.".into(),
        });
        let changes = t.apply(&notice, Driver::Agy);
        assert!(matches!(changes.first(), Some(Change::Added(id)) if id.starts_with("switch")));
        let kinds: Vec<&str> = t
            .order()
            .iter()
            .filter_map(|id| t.get(id))
            .map(|i| match &i.body {
                Body::Switch { .. } => "switch",
                Body::Notice { .. } => "notice",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            ["other", "switch", "notice"],
            "the divider comes first"
        );

        let changes = t.apply(&start("gemini"), Driver::Agy);
        let Some(Change::Updated(id)) = changes.first() else {
            panic!("the start names the model in the same divider: {changes:?}")
        };
        match &t.get(id).expect("divider").body {
            Body::Switch {
                driver,
                model,
                agent_changed,
                ..
            } => {
                assert_eq!(*driver, Driver::Agy);
                assert_eq!(model.as_deref(), Some("gemini"));
                assert!(agent_changed);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            t.order()
                .iter()
                .filter(|id| id.starts_with("switch"))
                .count(),
            1,
            "one divider, not a second for the model"
        );
        assert_eq!(t.current_model(), Some("gemini"));
        // A later model change on the new agent is a model switch as before.
        let changes = t.apply(
            &ev(Event::ModelChanged {
                model: "gemini-flash".into(),
            }),
            Driver::Agy,
        );
        assert!(matches!(changes.first(), Some(Change::Added(_))));
    }

    #[test]
    fn provider_and_model_switches_add_dividers() {
        let mut t = Transcript::new();
        let start = |m: &str| {
            ev(Event::SessionStarted {
                native_id: "n".into(),
                model: Some(m.into()),
                cwd: None,
            })
        };
        assert!(t.apply(&start("opus"), Driver::Claude).is_empty());
        // Same model re-announced by the per-turn init: nothing.
        assert!(t
            .apply(
                &ev(Event::TurnStarted {
                    model: Some("opus".into())
                }),
                Driver::Claude
            )
            .iter()
            .all(|c| !matches!(c, Change::Added(_))));
        let c = t.apply(
            &ev(Event::ModelChanged {
                model: "sonnet".into(),
            }),
            Driver::Claude,
        );
        assert!(matches!(c.as_slice(), [Change::Added(_), Change::Gauge]));
        let c = t.apply(&start("gemini"), Driver::Agy);
        let Some(Change::Added(id)) = c.first() else {
            panic!("expected a divider")
        };
        match &t.get(id).expect("divider").body {
            Body::Switch {
                driver,
                model,
                agent_changed,
                ..
            } => {
                assert_eq!(*driver, Driver::Agy);
                assert_eq!(model.as_deref(), Some("gemini"));
                assert!(agent_changed);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(t.current_model(), Some("gemini"));
    }

    #[test]
    fn a_model_switch_and_an_interrupt_are_each_said_once() {
        let notices = |t: &Transcript| {
            t.order()
                .iter()
                .filter_map(|id| t.get(id))
                .filter(|i| matches!(i.body, Body::Notice { .. }))
                .count()
        };
        let mut t = Transcript::new();
        t.apply(
            &ev(Event::ModelChanged {
                model: "opus".into(),
            }),
            Driver::Claude,
        );
        let c = t.apply(
            &ev(Event::ModelChanged {
                model: "claude-sonnet-5-5".into(),
            }),
            Driver::Claude,
        );
        assert!(matches!(c.first(), Some(Change::Added(_))), "the divider");
        // The agent's own words for the same switch add nothing under the divider.
        let said = Event::Notice {
            text: "Set model to `sonnet (claude-sonnet-5-5)`".into(),
        };
        assert!(t.apply(&ev(said), Driver::Claude).is_empty());
        assert_eq!(notices(&t), 0);
        // Another notice still shows, once.
        let other = || Event::Notice {
            text: "Cleared".into(),
        };
        assert_eq!(t.apply(&ev(other()), Driver::Claude).len(), 1);
        assert!(t.apply(&ev(other()), Driver::Claude).is_empty());
        assert_eq!(notices(&t), 1);
        // An interrupt after an interrupt is not a second row.
        let interrupted = || {
            ev(Event::TurnCompleted {
                state: TurnState::Interrupted,
                usage: None,
                cost_usd: None,
                error: None,
            })
        };
        t.apply(&interrupted(), Driver::Claude);
        t.apply(&interrupted(), Driver::Claude);
        assert_eq!(notices(&t), 2, "Cleared and one Interrupted");
    }

    #[test]
    fn turn_lifecycle_tracks_running_and_errors() {
        let mut t = Transcript::new();
        let c = t.apply(&ev(Event::TurnStarted { model: None }), Driver::Claude);
        assert_eq!(c, vec![Change::Running]);
        assert!(t.running);
        t.apply(
            &started("m", ItemKind::AssistantMessage, None),
            Driver::Claude,
        );
        let c = t.apply(
            &ev(Event::TurnCompleted {
                state: TurnState::Failed,
                usage: None,
                cost_usd: None,
                error: Some("quota".into()),
            }),
            Driver::Claude,
        );
        assert!(!t.running);
        assert!(c.contains(&Change::Running));
        assert!(
            c.contains(&Change::Updated("m".into())),
            "open text is closed: {c:?}"
        );
        let last = t.order().last().expect("error row");
        assert_eq!(
            t.get(last).map(|i| i.body.clone()),
            Some(Body::TurnError {
                message: "quota".into()
            })
        );
    }

    fn tasks(list: &[(&str, BackgroundTaskKind, Option<&str>)]) -> Envelope {
        ev(Event::BackgroundTasks {
            tasks: list
                .iter()
                .map(|(id, kind, description)| BackgroundTask {
                    id: (*id).to_owned(),
                    kind: *kind,
                    description: description.map(str::to_owned),
                    tool_use_id: None,
                })
                .collect(),
        })
    }

    fn turn_done() -> Envelope {
        Envelope::turn_completed(TurnState::Completed, None)
    }

    #[test]
    fn the_waiting_status_stays_one_short_line() {
        let many = Activity::Waiting {
            tasks: (1..=5).map(|i| format!("t{i}")).collect(),
        };
        assert_eq!(
            many.text().as_deref(),
            Some("Main agent done, waiting on 5 background tasks: t1, t2, t3 and 2 more")
        );
        // A multi-line description is named by its first line.
        let task = BackgroundTask {
            id: "t".into(),
            kind: BackgroundTaskKind::Agent,
            description: Some("Review the diff\nthen report back".into()),
            tool_use_id: None,
        };
        assert_eq!(task_name(&task), "Review the diff");
        let leading = BackgroundTask {
            description: Some("\n  \nMap the crate".into()),
            ..task
        };
        assert_eq!(task_name(&leading), "Map the crate");
    }

    #[test]
    fn the_header_says_when_the_agent_starts_resumes_or_is_paused() {
        use crate::chat::Lifecycle;
        let starting = Lifecycle {
            alive: true,
            starting: true,
            ..Lifecycle::default()
        };
        let resuming = Lifecycle {
            resuming: true,
            ..starting
        };
        let paused = Lifecycle {
            suspended: true,
            ..Lifecycle::default()
        };
        let up = Lifecycle {
            alive: true,
            ..Lifecycle::default()
        };
        let working = Activity::Working { background: 0 };
        // A start or a resume says so over anything, even the turn the message just started.
        assert_eq!(Activity::Idle.with_lifecycle(starting), Activity::Starting);
        assert_eq!(working.clone().with_lifecycle(resuming), Activity::Resuming);
        // Paused only says so when nothing runs or waits.
        for quiet in [
            Activity::Idle,
            Activity::Finished,
            Activity::Stopped,
            Activity::Failed,
        ] {
            assert_eq!(quiet.with_lifecycle(paused), Activity::Paused);
        }
        assert_eq!(
            Activity::NeedsApproval.with_lifecycle(paused),
            Activity::NeedsApproval
        );
        assert_eq!(working.clone().with_lifecycle(paused), working);
        // An agent that is up changes nothing.
        assert_eq!(Activity::Finished.with_lifecycle(up), Activity::Finished);
        assert_eq!(
            Activity::Paused.text().as_deref(),
            Some("Paused: resumes when you send")
        );
        assert_eq!(Activity::Starting.text().as_deref(), Some("Starting…"));
        assert_eq!(Activity::Resuming.text().as_deref(), Some("Resuming…"));
        assert!(Activity::Starting.busy() && Activity::Resuming.busy());
        assert!(!Activity::Paused.busy(), "a paused agent does not spin");
    }

    #[test]
    fn the_thread_status_counts_background_work_until_it_finishes() {
        let mut t = Transcript::new();
        let now = |t: &Transcript| t.activity(t.running);
        assert_eq!(now(&t), Activity::Idle, "a new thread says nothing");
        assert_eq!(now(&t).text(), None);

        t.apply(&ev(Event::TurnStarted { model: None }), Driver::Claude);
        assert_eq!(now(&t), Activity::Working { background: 0 });
        assert_eq!(now(&t).text().as_deref(), Some("Working…"));

        // A sub-agent and a shell command go to the background while the turn runs.
        let c = t.apply(
            &tasks(&[
                ("t1", BackgroundTaskKind::Agent, Some("Map the crate")),
                ("t2", BackgroundTaskKind::Shell, None),
            ]),
            Driver::Claude,
        );
        assert_eq!(c, vec![Change::Running]);
        assert_eq!(
            now(&t).text().as_deref(),
            Some("Working…, plus 2 in the background")
        );
        // The same list again is not a change.
        assert!(t
            .apply(
                &tasks(&[
                    ("t1", BackgroundTaskKind::Agent, Some("Map the crate")),
                    ("t2", BackgroundTaskKind::Shell, None),
                ]),
                Driver::Claude
            )
            .is_empty());

        // The main turn ends: the thread is not finished, it waits on its background work.
        t.apply(&turn_done(), Driver::Claude);
        assert!(!t.running, "the main turn (and its stop button) is over");
        let waiting = now(&t);
        assert!(waiting.busy());
        assert_eq!(
            waiting.text().as_deref(),
            Some("Main agent done, waiting on 2 background tasks: Map the crate, a shell command")
        );

        t.apply(
            &tasks(&[("t1", BackgroundTaskKind::Agent, Some("Map the crate"))]),
            Driver::Claude,
        );
        assert_eq!(
            now(&t).text().as_deref(),
            Some("Main agent done, waiting on 1 background task: Map the crate")
        );

        // The last task ends: finished.
        t.apply(&tasks(&[]), Driver::Claude);
        assert_eq!(now(&t), Activity::Finished);
        assert!(!now(&t).busy());

        // Claude's follow-up turn to read the results works as any turn.
        t.apply(&ev(Event::TurnStarted { model: None }), Driver::Claude);
        assert_eq!(now(&t), Activity::Working { background: 0 });
        t.apply(&turn_done(), Driver::Claude);
        assert_eq!(now(&t), Activity::Finished);
    }

    /// A thread stored mid-turn by an older build (no `TurnCompleted`): an approval and a question
    /// still waiting, a sub-agent and a reply still running, background work listed.
    fn stale_history() -> Vec<Envelope> {
        vec![
            ev(Event::TurnStarted { model: None }),
            started("m", ItemKind::AssistantMessage, None),
            delta(Some("m"), StreamKind::Assistant, "Let me check"),
            started("agent", ItemKind::Subagent, None),
            started("agent-read", ItemKind::FileRead, Some("agent")),
            started("done", ItemKind::Command, None),
            Envelope::item_completed("done", ItemStatus::Completed, None, None),
            Envelope::new(Event::ApprovalRequested {
                tool: "Bash".into(),
                title: None,
                input: json!({"command": "ls"}),
                reason: None,
                options: vec![Decision::Allow, Decision::Deny],
                response: ResponseCapability::Live,
                remembers: None,
            })
            .request("r1"),
            ev(Event::QuestionRequested {
                questions: vec![],
                source: Default::default(),
            })
            .request("q1"),
            tasks(&[("t1", BackgroundTaskKind::Agent, Some("Map the crate"))]),
        ]
    }

    /// The reported bug: stopping the agent while it asked a question left the question
    /// "unanswered" for good (the shelf kept pointing at it). Claude asks through a permission
    /// request and drops one as a cancelled approval, which only updated approval cards.
    #[test]
    fn a_question_the_agent_drops_or_a_stop_cuts_off_stops_waiting() {
        let asked = |t: &mut Transcript, request: &str| {
            t.apply(&ev(Event::TurnStarted { model: None }), Driver::Claude);
            t.apply(
                &ev(Event::QuestionRequested {
                    questions: vec![],
                    source: QuestionSource::Agent,
                })
                .request(request),
                Driver::Claude,
            );
            assert!(matches!(
                t.pending_interruption(),
                Some(PendingInterruption::Question { .. })
            ));
        };
        let state = |t: &Transcript, request: &str| match t
            .get(&format!("question:{request}"))
            .map(|i| &i.body)
        {
            Some(Body::Question(q)) => q.state,
            other => panic!("{other:?}"),
        };

        // Claude cancels the request (control_cancel_request, or its process ending).
        let mut t = Transcript::new();
        asked(&mut t, "q1");
        t.apply(
            &ev(Event::ApprovalResolved {
                decision: Decision::Cancel,
            })
            .request("q1"),
            Driver::Claude,
        );
        assert_eq!(state(&t, "q1"), QuestionState::Withdrawn);
        assert!(t.pending_interruption().is_none());
        assert_eq!(t.pending_count(), 0);

        // An expiry names it the same way.
        let mut t = Transcript::new();
        asked(&mut t, "q2");
        t.apply(&ev(Event::ApprovalExpired).request("q2"), Driver::Claude);
        assert_eq!(state(&t, "q2"), QuestionState::Expired);

        // The turn is stopped with no word on the request: it expires with the turn.
        let mut t = Transcript::new();
        asked(&mut t, "q3");
        t.apply(
            &ev(Event::TurnCompleted {
                state: TurnState::Interrupted,
                usage: None,
                cost_usd: None,
                error: None,
            }),
            Driver::Claude,
        );
        assert_eq!(state(&t, "q3"), QuestionState::Expired);
        assert!(t.pending_interruption().is_none());

        // A turn that completes leaves a live request alone (background work can still ask).
        let mut t = Transcript::new();
        asked(&mut t, "q4");
        t.apply(
            &ev(Event::TurnCompleted {
                state: TurnState::Completed,
                usage: None,
                cost_usd: None,
                error: None,
            }),
            Driver::Claude,
        );
        assert_eq!(state(&t, "q4"), QuestionState::Pending);
        // An answered question keeps its state whatever follows.
        t.apply(
            &ev(Event::QuestionResolved { answered: true }).request("q4"),
            Driver::Claude,
        );
        t.apply(
            &ev(Event::ApprovalResolved {
                decision: Decision::Cancel,
            })
            .request("q4"),
            Driver::Claude,
        );
        assert_eq!(state(&t, "q4"), QuestionState::Answered);
    }

    /// End to end through the Claude adapter's own frames: AskUserQuestion arrives as a
    /// `can_use_tool` request; a `control_cancel_request` (what a stop brings) clears it.
    #[test]
    fn a_cancelled_claude_question_clears_through_the_adapter() {
        use agent_core::adapter::Adapter;
        let mut a = agent_core::claude::ClaudeAdapter::new();
        let mut t = Transcript::new();
        let mut feed = |a: &mut agent_core::claude::ClaudeAdapter, line: serde_json::Value| {
            for env in a.feed(&line.to_string()) {
                t.apply(&env, Driver::Claude);
            }
            t.pending_interruption().is_some()
        };
        assert!(feed(
            &mut a,
            serde_json::json!({"type": "control_request", "request_id": "r1", "request": {
                "subtype": "can_use_tool", "tool_name": "AskUserQuestion", "tool_use_id": "tu1",
                "input": {"questions": [{"question": "Which?", "header": "Pick",
                    "options": [{"label": "A"}, {"label": "B"}], "multiSelect": false}]}}})
        ));
        assert!(!feed(
            &mut a,
            serde_json::json!({"type": "control_cancel_request", "request_id": "r1"})
        ));
    }

    #[test]
    fn a_stale_stored_turn_settles_when_no_agent_is_alive() {
        let mut t = Transcript::new();
        for env in stale_history() {
            t.apply(&env, Driver::Claude);
        }
        assert!(t.running && !t.background.is_empty(), "as stored");
        assert!(t.settle_stale());
        assert!(!t.running, "no phantom running turn");
        assert!(t.background.is_empty());
        assert_eq!(t.activity(false), Activity::Stopped);
        let tool = |id: &str| match &t.get(id).expect(id).body {
            Body::Tool(tool) => tool.status,
            other => panic!("not a tool: {other:?}"),
        };
        assert_eq!(tool("agent"), ToolStatus::Interrupted);
        assert_eq!(tool("agent-read"), ToolStatus::Interrupted);
        assert_eq!(
            tool("done"),
            ToolStatus::Completed,
            "finished items keep theirs"
        );
        assert!(
            t.subagents()
                .iter()
                .all(|s| s.status != ToolStatus::Running),
            "no sub-agent listed as running forever"
        );
        match &t.get("approval:r1").expect("card").body {
            Body::Approval(a) => assert_eq!(a.state, ApprovalState::Expired),
            other => panic!("not an approval: {other:?}"),
        }
        match &t.get("question:q1").expect("card").body {
            Body::Question(q) => assert_eq!(q.state, QuestionState::Expired),
            other => panic!("not a question: {other:?}"),
        }
        assert!(matches!(
            t.get("m").map(|i| &i.body),
            Some(Body::Assistant {
                streaming: false,
                ..
            })
        ));
        // A card that expired cannot be answered any more.
        assert!(t.mark_approval_sent("r1", Decision::Allow).is_empty());
        assert!(t.mark_questions_sent("q1").is_empty());
        // Settled once, there is nothing left to settle.
        assert!(!t.settle_stale());
    }

    #[test]
    fn a_complete_history_has_nothing_to_settle() {
        let mut t = Transcript::new();
        t.apply(&ev(Event::TurnStarted { model: None }), Driver::Claude);
        t.apply(&started("c", ItemKind::Command, None), Driver::Claude);
        t.apply(
            &Envelope::item_completed("c", ItemStatus::Completed, None, None),
            Driver::Claude,
        );
        t.apply(&turn_done(), Driver::Claude);
        assert!(!t.settle_stale());
    }

    #[test]
    fn a_session_that_exits_leaves_no_background_work() {
        let mut t = Transcript::new();
        t.apply(&ev(Event::TurnStarted { model: None }), Driver::Claude);
        t.apply(
            &tasks(&[("t1", BackgroundTaskKind::Agent, None)]),
            Driver::Claude,
        );
        t.apply(&turn_done(), Driver::Claude);
        assert_eq!(
            t.activity(false).text().as_deref(),
            Some("Main agent done, waiting on 1 background task: a sub-agent")
        );
        let c = t.apply(
            &ev(Event::SessionExited {
                code: Some(0),
                expected: true,
            }),
            Driver::Claude,
        );
        assert_eq!(c, vec![Change::Running]);
        assert!(t.background.is_empty());
        assert_eq!(t.activity(false), Activity::Finished);
    }

    #[test]
    fn unexpected_exit_is_an_error_and_expected_exit_is_quiet() {
        let mut t = Transcript::new();
        let c = t.apply(
            &ev(Event::SessionExited {
                code: Some(0),
                expected: true,
            }),
            Driver::Claude,
        );
        assert!(c.is_empty());
        let c = t.apply(
            &ev(Event::SessionExited {
                code: Some(3),
                expected: false,
            }),
            Driver::Claude,
        );
        assert!(matches!(c.as_slice(), [Change::Added(_)]));
    }

    #[test]
    fn compaction_updates_the_gauge_and_adds_a_divider() {
        let mut t = Transcript::new();
        t.apply(
            &ev(Event::UsageUpdated {
                used: 21_478,
                max: Some(200_000),
                auto_compact_at: Some(160_000),
            }),
            Driver::Claude,
        );
        let c = t.apply(
            &ev(Event::Compacted {
                manual: true,
                before: 21_478,
                after: Some(2_082),
            }),
            Driver::Claude,
        );
        assert!(c.contains(&Change::Gauge));
        assert_eq!(t.gauge.map(|g| g.used), Some(2_082));
    }

    #[test]
    fn control_results_route_by_request() {
        let mut t = Transcript::new();
        let c = t.apply(
            &ev(Event::ControlResult {
                ok: None,
                error: Some("nope".into()),
            })
            .request("k1"),
            Driver::Claude,
        );
        assert_eq!(
            c,
            vec![Change::Control {
                request: "k1".into(),
                result: Err("nope".into())
            }]
        );
        let c = t.apply(
            &ev(Event::ControlResult {
                ok: None,
                error: None,
            })
            .request("k2"),
            Driver::Claude,
        );
        assert_eq!(
            c,
            vec![Change::Control {
                request: "k2".into(),
                result: Ok(Value::Null)
            }]
        );
    }

    #[test]
    fn five_thousand_mixed_items_reduce_quickly() {
        let mut t = Transcript::new();
        let started_at = std::time::Instant::now();
        for i in 0..5_000 {
            let id = format!("i{i}");
            let kind = match i % 4 {
                0 => ItemKind::UserMessage,
                1 => ItemKind::AssistantMessage,
                2 => ItemKind::Command,
                _ => ItemKind::Reasoning,
            };
            t.apply(&started(&id, kind, None), Driver::Claude);
            t.apply(
                &delta(Some(&id), StreamKind::Assistant, "text "),
                Driver::Claude,
            );
        }
        assert_eq!(t.len(), 5_000);
        assert!(started_at.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn queued_user_prompts_stay_pinned_at_the_bottom_of_transcript_order() {
        let mut t = Transcript::new();
        // Turn 1 starts with a user message
        t.apply(&started("u1", ItemKind::UserMessage, None), Driver::Agy);
        // Turn 1 runs a tool
        t.apply(&started("t1", ItemKind::FileRead, None), Driver::Agy);

        // While turn 1 is running, the operator queues a follow-up prompt
        let q_envelope = Envelope::item_started(
            "q1",
            ItemKind::UserMessage,
            "queued",
            Some(serde_json::json!({"queued": true})),
            None,
        );
        t.apply(&q_envelope, Driver::Agy);

        assert_eq!(t.order(), ["u1", "t1", "q1"]);

        // Turn 1 continues emitting events: another tool, reasoning, assistant message
        t.apply(&started("t2", ItemKind::Command, None), Driver::Agy);
        t.apply(&started("r1", ItemKind::Reasoning, None), Driver::Agy);
        t.apply(
            &started("a1", ItemKind::AssistantMessage, None),
            Driver::Agy,
        );

        // Queued prompt q1 must remain at the very end
        assert_eq!(t.order(), ["u1", "t1", "t2", "r1", "a1", "q1"]);

        // Operator queues a second prompt
        let q2_envelope = Envelope::item_started(
            "q2",
            ItemKind::UserMessage,
            "queued",
            Some(serde_json::json!({"queued": true})),
            None,
        );
        t.apply(&q2_envelope, Driver::Agy);

        assert_eq!(t.order(), ["u1", "t1", "t2", "r1", "a1", "q1", "q2"]);

        // Turn 1 completes and q1 is unqueued
        let unqueue_q1 = Envelope::item_started("q1", ItemKind::UserMessage, "", None, None);
        t.apply(&unqueue_q1, Driver::Agy);

        // q1 is now active, q2 is still queued at the end
        assert_eq!(t.order(), ["u1", "t1", "t2", "r1", "a1", "q1", "q2"]);

        // q1's turn emits a tool
        t.apply(&started("t3", ItemKind::FileRead, None), Driver::Agy);
        assert_eq!(t.order(), ["u1", "t1", "t2", "r1", "a1", "q1", "t3", "q2"]);
    }

    #[test]
    fn token_formatting() {
        assert_eq!(format_tokens(950), "950");
        assert_eq!(format_tokens(2_082), "2.1k");
        assert_eq!(format_tokens(2_000), "2k");
        assert_eq!(format_tokens(21_478), "21k");
        assert_eq!(format_tokens(200_000), "200k");
        assert_eq!(format_tokens(1_200_000), "1.2M");
        assert_eq!(format_tokens(1_000_000), "1M");
    }
}
