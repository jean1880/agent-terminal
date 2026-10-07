//! The transcript model: a pure reducer from canonical [`Envelope`]s to transcript items.
//!
//! No GTK here. The widget layer applies an envelope, gets back the list of [`Change`]s, and
//! updates (or creates) only the rows those name. Expanded/collapsed state lives on the
//! [`Item`], so it survives a row being evicted from the materialised window and rebuilt.

use std::collections::HashMap;

use agent_core::adapter::{Driver, Mode};
use agent_core::event::{
    BackgroundTask, BackgroundTaskKind, Decision, Envelope, Event, ItemKind, ItemStatus, PlanStep,
    Question, ResponseCapability, StreamKind, TurnState,
};
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
    },
    Assistant {
        text: String,
        driver: Driver,
        streaming: bool,
    },
    Reasoning {
        text: String,
        streaming: bool,
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
    },
    Approval(Approval),
    Question(QuestionCard),
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
}

impl SubagentSummary {
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
    /// What "Always allow" would save, shown on the card.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionState {
    Pending,
    Sent,
    Answered,
    Withdrawn,
    /// Never answered, and the agent that asked is gone: it can no longer be answered.
    Expired,
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
    Working { background: usize },
    /// The main agent's turn is over, but it is still waiting on these background tasks.
    Waiting { tasks: Vec<String> },
    /// A turn ran, and neither it nor any background work is still going.
    Finished,
}

impl Activity {
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
            Self::Waiting { tasks } => {
                let count = match tasks.len() {
                    1 => "1 background task".to_owned(),
                    n => format!("{n} background tasks"),
                };
                Some(format!(
                    "Main agent done, waiting on {count}: {}",
                    tasks.join(", ")
                ))
            }
            Self::Finished => Some("Finished".to_owned()),
        }
    }

    /// Whether a spinner turns: something is still running.
    pub fn busy(&self) -> bool {
        matches!(self, Self::Working { .. } | Self::Waiting { .. })
    }
}

/// A background task as the status names it: its description, else what kind of task it is.
fn task_name(task: &BackgroundTask) -> String {
    match task.description.as_deref().map(str::trim) {
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
    Commands,
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
    /// Approval/question request id → item id.
    requests: HashMap<String, ItemId>,
    pub plan: Vec<PlanStep>,
    pub gauge: Option<Gauge>,
    pub mode: Option<Mode>,
    pub running: bool,
    /// Background work still going (sub-agents, background commands), as the agent last listed
    /// it: the main turn may be over while these run.
    pub background: Vec<BackgroundTask>,
    /// A turn has run in this thread.
    ran: bool,
    last_driver: Option<Driver>,
    last_model: Option<String>,
    /// The assistant/reasoning item that item-less deltas belong to.
    open_text: Option<ItemId>,
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

    /// Every sub-agent the thread started, nested ones included, in the order they began.
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
                }
            }
            stack.extend(item.children.iter().rev());
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

    pub fn current_model(&self) -> Option<&str> {
        self.last_model.as_deref()
    }

    /// The thread's status. `main_running`: the main agent's turn runs (the model's own view of
    /// it, or the backend's).
    pub fn activity(&self, main_running: bool) -> Activity {
        Activity::of(main_running, &self.background, self.ran)
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

    /// Inserts an item (nested under `parent` when that item exists) and returns its id.
    fn insert(&mut self, id: ItemId, body: Body, parent: Option<ItemId>) -> ItemId {
        let expanded = matches!(&body, Body::Tool(t) if t.kind == ItemKind::Subagent)
            || matches!(&body, Body::Approval(_) | Body::Question(_));
        let parent = parent.filter(|p| self.items.contains_key(p));
        match &parent {
            Some(p) => {
                if let Some(parent_item) = self.items.get_mut(p) {
                    parent_item.children.push(id.clone());
                }
            }
            None => self.order.push(id.clone()),
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
        let Some(id) = self.requests.get(request).cloned() else {
            return Vec::new();
        };
        match self.items.get_mut(&id).map(|i| &mut i.body) {
            Some(Body::Approval(a)) if a.state == ApprovalState::Pending => {
                a.state = ApprovalState::Sent(decision);
                vec![Change::Updated(id)]
            }
            _ => Vec::new(),
        }
    }

    pub fn mark_questions_sent(&mut self, request: &str) -> Vec<Change> {
        let Some(id) = self.requests.get(request).cloned() else {
            return Vec::new();
        };
        match self.items.get_mut(&id).map(|i| &mut i.body) {
            Some(Body::Question(q)) if q.state == QuestionState::Pending => {
                q.state = QuestionState::Sent;
                vec![Change::Updated(id)]
            }
            _ => Vec::new(),
        }
    }

    /// Closes out what replayed history left open when no agent is alive to finish it: a thread
    /// stored by an older build (or cut off by a crash) can end mid-turn, with no
    /// `TurnCompleted`. The turn stops running, streaming text ends, Running tool and sub-agent
    /// items become Interrupted, unanswered approvals and questions expire (nothing could take
    /// the answer), and the background list empties. A session that starts later emits its own
    /// fresh events, so nothing here fights it. Returns whether anything changed.
    pub fn settle_stale(&mut self) -> bool {
        let mut changed = self.running || !self.background.is_empty();
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
                Body::Approval(a)
                    if matches!(a.state, ApprovalState::Pending | ApprovalState::Sent(_)) =>
                {
                    a.state = ApprovalState::Expired;
                    changed = true;
                }
                Body::Question(q)
                    if matches!(q.state, QuestionState::Pending | QuestionState::Sent) =>
                {
                    q.state = QuestionState::Expired;
                    changed = true;
                }
                _ => {}
            }
        }
        changed
    }

    pub fn set_expanded(&mut self, id: &str, expanded: bool) {
        if let Some(item) = self.items.get_mut(id) {
            item.expanded = expanded;
        }
    }

    /// Applies one envelope. `driver` is the agent the thread is on right now (the envelope
    /// does not carry it); it colours assistant rows and detects provider switches.
    pub fn apply(&mut self, env: &Envelope, driver: Driver) -> Vec<Change> {
        let mut out = Vec::new();
        match &env.event {
            Event::SessionStarted { model, .. } => {
                let agent_changed = self.last_driver.is_some_and(|d| d != driver);
                let model_changed = model.is_some()
                    && self.last_model.is_some()
                    && model.as_deref() != self.last_model.as_deref();
                if agent_changed || model_changed {
                    out.extend(self.push_switch(driver, model.clone(), agent_changed));
                }
                self.last_driver = Some(driver);
                if model.is_some() {
                    self.last_model.clone_from(model);
                }
            }
            Event::SessionExited { code, expected } => {
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
                if self.last_driver.is_none() {
                    self.last_driver = Some(driver);
                }
                if let Some(m) = model {
                    if self.last_model.as_deref().is_some_and(|last| last != m) {
                        out.extend(self.push_switch(driver, Some(m.clone()), false));
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
                if self.running {
                    self.running = false;
                    out.push(Change::Running);
                }
                if let Some(id) = self.open_text.take() {
                    out.extend(self.finish_text(&id));
                }
                match state {
                    TurnState::Completed => {}
                    TurnState::Interrupted => {
                        out.extend(self.push_notice("Interrupted", Tone::Info));
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
                let id = env.item.clone().unwrap_or_else(|| self.next_id("item"));
                if self.items.contains_key(&id) {
                    return out; // Replayed start (adapters dedup, but be safe).
                }
                let body = match kind {
                    ItemKind::UserMessage => Body::User {
                        text: String::new(),
                    },
                    ItemKind::AssistantMessage => Body::Assistant {
                        text: String::new(),
                        driver,
                        streaming: true,
                    },
                    ItemKind::Reasoning => Body::Reasoning {
                        text: String::new(),
                        streaming: true,
                    },
                    _ => Body::Tool(Tool {
                        kind: *kind,
                        title: title.clone(),
                        input: input.clone(),
                        input_text: String::new(),
                        output: String::new(),
                        error: None,
                        status: ToolStatus::Running,
                    }),
                };
                if matches!(kind, ItemKind::AssistantMessage | ItemKind::Reasoning) {
                    self.open_text = Some(id.clone());
                }
                out.push(Change::Added(self.insert(id, body, parent.clone())));
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
                    if self.write_stream(&id, *stream, text, true) {
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
                out.push(Change::Updated(id));
            }
            Event::PlanUpdated { steps } => {
                self.plan.clone_from(steps);
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
                out.push(Change::Added(self.insert(id, body, None)));
            }
            Event::ApprovalResolved { decision } => {
                out.extend(self.update_approval(env, |a| {
                    a.state = ApprovalState::Resolved(*decision);
                }));
            }
            Event::ApprovalExpired => {
                out.extend(self.update_approval(env, |a| {
                    if matches!(a.state, ApprovalState::Pending | ApprovalState::Sent(_)) {
                        a.state = ApprovalState::Expired;
                    }
                }));
            }
            Event::QuestionRequested { questions } => {
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
                let target = env
                    .request
                    .as_ref()
                    .and_then(|r| self.requests.get(r))
                    .cloned();
                if let Some(id) = target {
                    if let Some(Body::Question(q)) = self.items.get_mut(&id).map(|i| &mut i.body) {
                        q.state = if *answered {
                            QuestionState::Answered
                        } else {
                            QuestionState::Withdrawn
                        };
                        out.push(Change::Updated(id));
                    }
                }
            }
            Event::ModeChanged { mode } => {
                self.mode = Some(*mode);
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
                let text = match resets_at {
                    Some(at) => format!("Rate limited. The quota resets at {at}."),
                    None => "Rate limited by the provider.".to_owned(),
                };
                out.extend(self.push_notice(text, Tone::Warning));
            }
            Event::ModelChanged { model } => {
                if self.last_model.as_deref() != Some(model.as_str()) {
                    if self.last_model.is_some() {
                        out.extend(self.push_switch(driver, Some(model.clone()), false));
                    }
                    self.last_model = Some(model.clone());
                }
            }
            Event::Notice { text } => {
                out.extend(self.push_notice(text.clone(), Tone::Info));
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
            // Background tasks are thread state, not transcript rows: the full list each time.
            Event::BackgroundTasks { tasks } => {
                if self.background != *tasks {
                    self.background.clone_from(tasks);
                    out.push(Change::Running);
                }
            }
            // Quota feeds the usage indicator (app-wide service), not the transcript.
            Event::QuotaUpdated { .. } | Event::Unknown => {}
        }
        out
    }

    fn push_switch(
        &mut self,
        driver: Driver,
        model: Option<String>,
        agent_changed: bool,
    ) -> Vec<Change> {
        let id = self.next_id("switch");
        let id = self.insert(
            id,
            Body::Switch {
                driver,
                model,
                agent_changed,
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
            StreamKind::Reasoning => Body::Reasoning {
                text: String::new(),
                streaming: true,
            },
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
            (Body::User { text }, _) => text,
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

    fn update_approval(&mut self, env: &Envelope, f: impl FnOnce(&mut Approval)) -> Vec<Change> {
        let Some(id) = env
            .request
            .as_ref()
            .and_then(|r| self.requests.get(r))
            .cloned()
        else {
            return Vec::new();
        };
        match self.items.get_mut(&id).map(|i| &mut i.body) {
            Some(Body::Approval(a)) => {
                f(a);
                vec![Change::Updated(id)]
            }
            _ => Vec::new(),
        }
    }
}

/// Compact token count for the gauge and the compaction divider: `950`, `21k`, `1.2M`.
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

    fn started(id: &str, kind: ItemKind, parent: Option<&str>) -> Envelope {
        Envelope::new(Event::ItemStarted {
            kind,
            title: format!("title {id}"),
            input: None,
            parent: parent.map(str::to_owned),
        })
        .item(id)
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
        let snap = Envelope::new(Event::ContentSnapshot {
            stream: StreamKind::Assistant,
            text: "Hello, world".into(),
        })
        .item("m1");
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
            Envelope::new(Event::ItemStarted {
                kind: ItemKind::Subagent,
                title: "Task".into(),
                input: Some(input),
                parent: parent.map(str::to_owned),
            })
            .item(id)
        };
        t.apply(&started("u", ItemKind::UserMessage, None), Driver::Claude);
        t.apply(
            &task(
                "a",
                None,
                json!({"subagent_type": "Explore", "description": "Find the parser"}),
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

    #[test]
    fn tool_completion_sets_status_output_and_expands_failures() {
        let mut t = Transcript::new();
        t.apply(&started("c1", ItemKind::Command, None), Driver::Claude);
        t.apply(
            &delta(Some("c1"), StreamKind::ToolInput, "{\"command\":"),
            Driver::Claude,
        );
        let done = Envelope::new(Event::ItemCompleted {
            status: ItemStatus::Failed,
            output: Some("boom".into()),
            error: Some("exit 1".into()),
        })
        .item("c1");
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
        t.mark_approval_sent("r1", Decision::Allow);
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
            &ev(Event::QuestionRequested { questions: vec![] }).request("q"),
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
        assert_eq!(c.len(), 1);
        let c = t.apply(&start("gemini"), Driver::Agy);
        let Some(Change::Added(id)) = c.first() else {
            panic!("expected a divider")
        };
        match &t.get(id).expect("divider").body {
            Body::Switch {
                driver,
                model,
                agent_changed,
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
        ev(Event::TurnCompleted {
            state: TurnState::Completed,
            usage: None,
            cost_usd: None,
            error: None,
        })
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
            Envelope::new(Event::ItemCompleted {
                status: ItemStatus::Completed,
                output: None,
                error: None,
            })
            .item("done"),
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
            ev(Event::QuestionRequested { questions: vec![] }).request("q1"),
            tasks(&[("t1", BackgroundTaskKind::Agent, Some("Map the crate"))]),
        ]
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
        assert_eq!(t.activity(false), Activity::Finished);
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
            &Envelope::new(Event::ItemCompleted {
                status: ItemStatus::Completed,
                output: None,
                error: None,
            })
            .item("c"),
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
        assert!(started_at.elapsed() < std::time::Duration::from_secs(2));
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
