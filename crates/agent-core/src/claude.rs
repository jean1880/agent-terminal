//! Claude Code adapter: `claude --input-format stream-json` plus the control protocol.
//!
//! A pure, sans-I/O state machine. The wire facts come from the Phase 0 probes
//! (`plans/2026-10-06_v3-structured-agents.md`, "Phase 0 results") and are replayed against the
//! recordings in `tests/fixtures/claude-*.ndjson`.
//!
//! Frames are read as untyped [`serde_json::Value`]s, so a field the CLI adds or drops never
//! aborts the stream: anything this adapter does not map becomes [`Event::Unknown`] with the whole
//! frame in `raw`. The first envelope produced from a frame carries it as `raw` (once, not per
//! envelope, so the store keeps one copy).
//!
//! Known limits (each with its upgrade path):
//! - Assistant snapshots carry no block index. The adapter numbers them per message in arrival
//!   order, which matches the stream indices while the CLI sends one snapshot per block. If it
//!   ever batches blocks, switch to matching on block type and content.
//! - The `seen` dedup set is capped at [`SEEN_CAP`] entries and then cleared; a replay older than
//!   that window would be shown twice.
//! - `ApprovalResolved` is only emitted for approvals the CLI cancels or that die with the
//!   process. The CLI never acknowledges an answer, so the session resolves the card itself once
//!   the answer is written (`ChatSession::respond_approval`).
//! - A backgrounded sub-agent (`task_started` with `is_backgrounded`, or an `Agent` result whose
//!   metadata says `async_launched`) stays open past its launch result and its turn's `result`
//!   and completes on `task_notification`. The per-turn snapshot numbering is reset by the
//!   parent's `result`; a sub-agent message split across frames on either side of it would
//!   reuse a block index. Upgrade path: key `snap_counts` cleanup by parent.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Map, Value};

use crate::adapter::{
    Action, Adapter, AdapterError, Command, Control, Driver, Mode, OpenSession, OpenSessionDelta,
    Outbox,
};
use crate::caps::Capabilities;
use crate::event::{
    AgentCommand, AgentCommandKind, BackgroundTask, BackgroundTaskKind, Decision, Envelope, Event,
    ItemKind, ItemStatus, Question, QuestionOption, ResponseCapability, StreamKind, TurnState,
    Usage,
};

/// Dedup window for replayed frames (see the module notes).
const SEEN_CAP: usize = 20_000;

/// An unanswered `can_use_tool` request.
#[derive(Debug, Clone)]
struct PendingApproval {
    tool_use_id: String,
    input: Value,
    suggestions: Vec<Value>,
}

/// A content block opened by a `content_block_start` stream event.
#[derive(Debug, Clone)]
struct Block {
    item: String,
    /// Tool blocks finish on their `tool_result`; text and thinking blocks on `content_block_stop`.
    tool: bool,
}

/// The Claude Code stream-json adapter.
#[derive(Debug)]
pub struct ClaudeAdapter {
    caps: Capabilities,
    counter: u64,
    init_id: Option<String>,
    session_started: bool,
    model: Option<String>,
    turn_open: bool,
    interrupt_pending: bool,
    // Commands: the bare init names are replaced by the richer `commands_changed` list.
    commands: Vec<AgentCommand>,
    commands_seen: bool,
    init_seen: bool,
    skills: Vec<String>,
    terminal_only: Vec<String>,
    // Streaming bookkeeping, keyed by parent tool use (None = main thread).
    msg_ids: HashMap<Option<String>, String>,
    blocks: HashMap<(Option<String>, u64), Block>,
    snap_counts: HashMap<String, u64>,
    started_items: HashSet<String>,
    open_items: Vec<String>,
    /// Backgrounded sub-agents (`Agent` tool uses) still running: they outlive the turn that
    /// launched them and finish on their `task_notification`.
    background: HashSet<String>,
    /// Items opened inside a backgrounded sub-agent, mapped to that sub-agent's tool-use id.
    background_children: HashMap<String, String>,
    /// The running background tasks last reported (`background_tasks_changed`).
    tasks: Vec<BackgroundTask>,
    /// Background task id → the tool call that started it (`task_started`).
    task_tools: HashMap<String, String>,
    seen: HashSet<String>,
    // Requests in flight.
    pending: HashMap<String, PendingApproval>,
    pending_controls: HashSet<String>,
    context_requests: HashSet<String>,
    /// `get_usage` requests in flight, whose replies also become `QuotaUpdated`.
    usage_requests: HashSet<String>,
    /// `set_permission_mode` requests in flight: Claude's success reply is the only sign the mode
    /// changed, so it becomes `ModeChanged` (without it the picker snapped back on the next
    /// status refresh).
    mode_requests: HashMap<String, Mode>,
    /// Model controls await acknowledgement; request order prevents a delayed reply from
    /// replacing a newer successfully selected model.
    model_requests: HashMap<String, (u64, String)>,
    model_ack_order: u64,
    outbox: Outbox,
    /// The permission mode Claude last reported in `init` (it changes it itself too, as when
    /// a plan is accepted).
    mode: Option<Mode>,
    /// `--effort` of the running process (set by `argv`, which takes `&self`).
    current_effort: std::cell::RefCell<Option<String>>,
}

impl Default for ClaudeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeAdapter {
    pub fn new() -> Self {
        Self {
            caps: Capabilities::claude(),
            counter: 0,
            init_id: None,
            session_started: false,
            model: None,
            turn_open: false,
            interrupt_pending: false,
            commands: Vec::new(),
            commands_seen: false,
            init_seen: false,
            skills: Vec::new(),
            terminal_only: Vec::new(),
            msg_ids: HashMap::new(),
            blocks: HashMap::new(),
            snap_counts: HashMap::new(),
            started_items: HashSet::new(),
            open_items: Vec::new(),
            background: HashSet::new(),
            background_children: HashMap::new(),
            tasks: Vec::new(),
            task_tools: HashMap::new(),
            seen: HashSet::new(),
            pending: HashMap::new(),
            pending_controls: HashSet::new(),
            context_requests: HashSet::new(),
            usage_requests: HashSet::new(),
            mode_requests: HashMap::new(),
            model_requests: HashMap::new(),
            model_ack_order: 0,
            outbox: Outbox::default(),
            mode: None,
            current_effort: std::cell::RefCell::new(None),
        }
    }

    fn next_id(&mut self, prefix: &str) -> String {
        self.counter += 1;
        format!("{prefix}-{}", self.counter)
    }

    fn control_line(&mut self, id: &str, request: Value) -> String {
        self.pending_controls.insert(id.to_owned());
        json!({"type": "control_request", "request_id": id, "request": request}).to_string()
    }

    /// First sighting of `key` returns true; a replay returns false.
    fn first_time(&mut self, key: String) -> bool {
        if self.seen.len() >= SEEN_CAP {
            self.seen.clear();
        }
        self.seen.insert(key)
    }

    fn open_item(&mut self, id: &str) {
        self.started_items.insert(id.to_owned());
        self.open_items.push(id.to_owned());
    }

    /// [`open_item`](Self::open_item) for an item nested under `parent`: an item inside a
    /// backgrounded sub-agent (at any depth) is remembered as that sub-agent's, so the parent
    /// turn's end does not settle it.
    fn open_child(&mut self, id: &str, parent: Option<&str>) {
        self.open_item(id);
        let Some(parent) = parent else {
            return;
        };
        let root = if self.background.contains(parent) {
            Some(parent.to_owned())
        } else {
            self.background_children.get(parent).cloned()
        };
        if let Some(root) = root {
            self.background_children.insert(id.to_owned(), root);
        }
    }

    fn close_item(&mut self, id: &str) {
        self.open_items.retain(|i| i != id);
    }

    /// Whether `id` belongs to a backgrounded sub-agent (the `Agent` call itself or anything
    /// inside it).
    fn in_background(&self, id: &str) -> bool {
        self.background.contains(id) || self.background_children.contains_key(id)
    }

    // ----- background sub-agents -----

    /// `task_started`: a backgrounded `Agent` call stays open past its immediate "launched"
    /// tool result. A foreground one keeps the usual tool-result completion, and so does a
    /// backgrounded shell command (its "running in background" result is its answer; the
    /// task list says it is still running). The tool-use id also fills in the task list,
    /// which may have shown the task first.
    fn task_started(&mut self, v: &Value) -> Vec<Envelope> {
        let Some(tool) = str_of(v, "tool_use_id") else {
            return Vec::new();
        };
        let agent =
            str_of(v, "task_type").is_none_or(|t| task_kind(t) == BackgroundTaskKind::Agent);
        if agent && v.get("is_backgrounded").and_then(Value::as_bool) == Some(true) {
            self.background.insert(tool.to_owned());
        }
        let Some(task) = str_of(v, "task_id") else {
            return Vec::new();
        };
        self.task_tools.insert(task.to_owned(), tool.to_owned());
        match self.tasks.iter_mut().find(|t| t.id == task) {
            Some(t) if t.tool_use_id.is_none() => {
                t.tool_use_id = Some(tool.to_owned());
                vec![Envelope::new(Event::BackgroundTasks {
                    tasks: self.tasks.clone(),
                })]
            }
            _ => Vec::new(),
        }
    }

    /// `background_tasks_changed`: the full list of running background tasks.
    fn background_tasks(&mut self, v: &Value) -> Vec<Envelope> {
        let tasks: Vec<BackgroundTask> = array_of(v, "tasks")
            .iter()
            .filter_map(|t| {
                let id = str_of(t, "task_id")?.to_owned();
                Some(BackgroundTask {
                    kind: task_kind(str_of(t, "task_type").unwrap_or("")),
                    description: str_of(t, "description").map(str::to_owned),
                    tool_use_id: self.task_tools.get(&id).cloned(),
                    id,
                })
            })
            .collect();
        // Forget the tool ids of tasks that ended.
        self.task_tools.retain(|task, _| {
            tasks.iter().any(|t| t.id == *task) || !self.tasks.iter().any(|t| t.id == *task)
        });
        if tasks == self.tasks {
            return Vec::new();
        }
        self.tasks = tasks.clone();
        vec![Envelope::new(Event::BackgroundTasks { tasks })]
    }

    /// `task_notification`: a backgrounded sub-agent finished. Its item completes with the
    /// notification's summary; anything still open inside it never got a result.
    fn task_notification(&mut self, v: &Value) -> Vec<Envelope> {
        if let Some(task) = str_of(v, "task_id") {
            if !self.tasks.iter().any(|t| t.id == task) {
                self.task_tools.remove(task);
            }
        }
        let Some(id) = str_of(v, "tool_use_id") else {
            return Vec::new();
        };
        // A foreground sub-agent's own tool result follows and completes it.
        if !self.background.remove(id) {
            return Vec::new();
        }
        let mut out = Vec::new();
        let children: Vec<String> = self
            .open_items
            .iter()
            .filter(|i| self.background_children.get(*i).map(String::as_str) == Some(id))
            .cloned()
            .collect();
        for child in children {
            self.close_item(&child);
            out.push(
                Envelope::new(Event::ItemCompleted {
                    status: ItemStatus::Interrupted,
                    output: None,
                    error: None,
                })
                .item(child),
            );
        }
        self.background_children.retain(|_, root| root != id);
        self.close_item(id);
        let status = task_status(str_of(v, "status").unwrap_or(""));
        let summary = str_of(v, "summary").map(str::to_owned);
        out.push(
            Envelope::new(Event::ItemCompleted {
                status,
                error: (status == ItemStatus::Failed).then(|| {
                    summary
                        .clone()
                        .unwrap_or_else(|| "the sub-agent failed".to_owned())
                }),
                output: summary,
            })
            .item(id),
        );
        out
    }

    // ----- commands -----

    fn commands_event(&self) -> Event {
        let mut commands: Vec<AgentCommand> = self
            .commands
            .iter()
            .map(|c| AgentCommand {
                kind: if self.skills.contains(&c.name) {
                    AgentCommandKind::Skill
                } else {
                    AgentCommandKind::Command
                },
                ..c.clone()
            })
            .collect();
        commands.extend(self.terminal_only.iter().map(|name| AgentCommand {
            name: name.clone(),
            description: None,
            argument_hint: None,
            kind: AgentCommandKind::TerminalOnly,
        }));
        Event::CommandsChanged { commands }
    }

    /// Adopt the rich `{name, description, argumentHint}` list (replace, never merge).
    fn adopt_commands(&mut self, list: &[Value]) -> Event {
        self.commands = list
            .iter()
            .filter_map(|c| {
                let name = str_of(c, "name")?.trim_start_matches('/').to_owned();
                Some(AgentCommand {
                    name,
                    description: str_of(c, "description").map(str::to_owned),
                    argument_hint: str_of(c, "argumentHint")
                        .filter(|h| !h.is_empty())
                        .map(str::to_owned),
                    kind: AgentCommandKind::Command,
                })
            })
            .filter(|c| !self.terminal_only.contains(&c.name))
            .collect();
        self.commands_seen = true;
        self.commands_event()
    }

    // ----- frame mapping -----

    fn map_frame(&mut self, v: &Value) -> Vec<Envelope> {
        match str_of(v, "type").unwrap_or("") {
            "system" => self.system(v),
            "stream_event" => self.stream_event(v),
            "assistant" => self.assistant(v),
            "user" => self.user(v),
            "control_request" => self.control_request(v),
            "control_response" => self.control_response(v),
            "control_cancel_request" => self.control_cancel(v),
            "rate_limit_event" => self.rate_limit(v),
            "result" => self.result(v),
            "keep_alive" => Vec::new(),
            _ => vec![Envelope::new(Event::Unknown)],
        }
    }

    fn system(&mut self, v: &Value) -> Vec<Envelope> {
        match str_of(v, "subtype").unwrap_or("") {
            "init" => self.init(v),
            "commands_changed" => {
                let list = array_of(v, "commands");
                vec![Envelope::new(self.adopt_commands(&list))]
            }
            "compact_boundary" => {
                let meta = v.get("compact_metadata").unwrap_or(&Value::Null);
                vec![Envelope::new(Event::Compacted {
                    manual: str_of(meta, "trigger") == Some("manual"),
                    before: u64_of(meta, "pre_tokens").unwrap_or(0),
                    after: u64_of(meta, "post_tokens"),
                })]
            }
            "local_command_output" => {
                let text =
                    strip_local_tags(&content_text(v.get("content").unwrap_or(&Value::Null)));
                notice(text)
            }
            "task_started" => self.task_started(v),
            "task_notification" => self.task_notification(v),
            "background_tasks_changed" => self.background_tasks(v),
            // Progress chatter that carries nothing the UI renders. A sub-agent's
            // `task_updated` status patch is repeated by its `task_notification`.
            "status"
            | "thinking_tokens"
            | "hook_started"
            | "hook_progress"
            | "hook_response"
            | "session_state_changed"
            | "task_updated"
            | "task_progress" => Vec::new(),
            _ => vec![Envelope::new(Event::Unknown)],
        }
    }

    fn init(&mut self, v: &Value) -> Vec<Envelope> {
        let mut out = Vec::new();
        let model = str_of(v, "model").map(str::to_owned);

        if !self.session_started {
            self.session_started = true;
            self.model = model.clone();
            out.push(Envelope::new(Event::SessionStarted {
                native_id: str_of(v, "session_id").unwrap_or_default().to_owned(),
                model: model.clone(),
                cwd: str_of(v, "cwd").map(str::to_owned),
            }));
        } else if let Some(m) = &model {
            if self.model.as_ref() != Some(m) {
                self.model = Some(m.clone());
                out.push(Envelope::new(Event::ModelChanged { model: m.clone() }));
            }
        }

        // Claude reports its permission mode in every `init`; a change it made itself (leaving
        // plan mode once a plan is accepted) shows here. The first one is what it was started
        // with, which the host already knows.
        if let Some(mode) = str_of(v, "permissionMode").and_then(mode_from_name) {
            if self.mode.is_some_and(|m| m != mode) {
                out.push(Envelope::new(Event::ModeChanged { mode }));
            }
            self.mode = Some(mode);
        }

        // `init` is re-emitted at the start of every turn.
        self.turn_open = true;
        out.push(Envelope::new(Event::TurnStarted { model }));

        let skills = strings_of(v, "skills");
        let terminal = strings_of(v, "terminal_slash_commands");
        let mut changed =
            !self.init_seen || skills != self.skills || terminal != self.terminal_only;
        self.init_seen = true;
        self.skills = skills;
        self.terminal_only = terminal;
        if !self.commands_seen {
            let bare: Vec<AgentCommand> = strings_of(v, "slash_commands")
                .into_iter()
                .filter(|n| !self.terminal_only.contains(n))
                .map(|name| AgentCommand {
                    name,
                    description: None,
                    argument_hint: None,
                    kind: AgentCommandKind::Command,
                })
                .collect();
            changed |= bare != self.commands;
            self.commands = bare;
        } else {
            self.commands
                .retain(|c| !self.terminal_only.contains(&c.name));
        }
        if changed {
            out.push(Envelope::new(self.commands_event()));
        }
        out
    }

    fn stream_event(&mut self, v: &Value) -> Vec<Envelope> {
        let parent = str_of(v, "parent_tool_use_id").map(str::to_owned);
        let ev = v.get("event").unwrap_or(&Value::Null);
        let index = u64_of(ev, "index").unwrap_or(0);
        match str_of(ev, "type").unwrap_or("") {
            "message_start" => {
                if let Some(id) = ev.get("message").and_then(|m| str_of(m, "id")) {
                    self.msg_ids.insert(parent.clone(), id.to_owned());
                }
                self.blocks.retain(|(p, _), _| *p != parent);
                Vec::new()
            }
            "content_block_start" => {
                let cb = ev.get("content_block").unwrap_or(&Value::Null);
                self.block_start(parent, index, cb)
            }
            "content_block_delta" => {
                let Some(block) = self.blocks.get(&(parent, index)).cloned() else {
                    return Vec::new();
                };
                let delta = ev.get("delta").unwrap_or(&Value::Null);
                let (stream, text) = match str_of(delta, "type").unwrap_or("") {
                    "text_delta" => (StreamKind::Assistant, str_of(delta, "text")),
                    "thinking_delta" => (StreamKind::Reasoning, str_of(delta, "thinking")),
                    "input_json_delta" => (StreamKind::ToolInput, str_of(delta, "partial_json")),
                    // `signature_delta` and any future delta carry nothing to render.
                    _ => return Vec::new(),
                };
                match text {
                    Some(t) if !t.is_empty() => vec![Envelope::new(Event::ContentDelta {
                        stream,
                        text: t.to_owned(),
                    })
                    .item(block.item)],
                    _ => Vec::new(),
                }
            }
            "content_block_stop" => match self.blocks.get(&(parent, index)).cloned() {
                Some(block) if !block.tool => {
                    self.close_item(&block.item);
                    vec![Envelope::new(Event::ItemCompleted {
                        status: ItemStatus::Completed,
                        output: None,
                        error: None,
                    })
                    .item(block.item)]
                }
                _ => Vec::new(),
            },
            "message_delta" | "message_stop" | "ping" => Vec::new(),
            _ => vec![Envelope::new(Event::Unknown)],
        }
    }

    fn block_start(&mut self, parent: Option<String>, index: u64, cb: &Value) -> Vec<Envelope> {
        let msg = self.msg_ids.get(&parent).cloned().unwrap_or_default();
        let (item, kind, title, input, tool) = match str_of(cb, "type").unwrap_or("") {
            "text" => (
                format!("{msg}:{index}"),
                ItemKind::AssistantMessage,
                String::new(),
                None,
                false,
            ),
            "thinking" | "redacted_thinking" => (
                format!("{msg}:{index}"),
                ItemKind::Reasoning,
                "Thinking".to_owned(),
                None,
                false,
            ),
            "tool_use" | "server_tool_use" | "mcp_tool_use" => {
                let (Some(id), Some(name)) = (str_of(cb, "id"), str_of(cb, "name")) else {
                    return vec![Envelope::new(Event::Unknown)];
                };
                (
                    id.to_owned(),
                    tool_kind(name),
                    name.to_owned(),
                    non_empty_object(cb.get("input")),
                    true,
                )
            }
            _ => return vec![Envelope::new(Event::Unknown)],
        };
        self.blocks.insert(
            (parent.clone(), index),
            Block {
                item: item.clone(),
                tool,
            },
        );
        self.open_child(&item, parent.as_deref());
        vec![Envelope::new(Event::ItemStarted {
            kind,
            title,
            input,
            parent,
        })
        .item(item)]
    }

    fn assistant(&mut self, v: &Value) -> Vec<Envelope> {
        let msg = v.get("message").unwrap_or(&Value::Null);
        let uuid = str_of(v, "uuid").unwrap_or_default().to_owned();
        let content = array_of(msg, "content");

        // The CLI's own output (local commands) is a synthetic assistant message.
        if str_of(msg, "model") == Some("<synthetic>") {
            if !self.first_time(format!("{uuid}#synthetic")) {
                return Vec::new();
            }
            return notice(content_text(&Value::Array(content)));
        }

        let parent = str_of(v, "parent_tool_use_id").map(str::to_owned);
        let msg_id = str_of(msg, "id").unwrap_or_default().to_owned();
        let mut out = Vec::new();
        for (pos, block) in content.iter().enumerate() {
            let index = {
                let n = self.snap_counts.entry(msg_id.clone()).or_insert(0);
                let i = *n;
                *n += 1;
                i
            };
            if !self.first_time(format!("{uuid}#{pos}")) {
                continue;
            }
            match str_of(block, "type").unwrap_or("") {
                "text" | "thinking" => {
                    let (kind, stream, text, title) = if str_of(block, "type") == Some("text") {
                        (
                            ItemKind::AssistantMessage,
                            StreamKind::Assistant,
                            str_of(block, "text"),
                            "",
                        )
                    } else {
                        (
                            ItemKind::Reasoning,
                            StreamKind::Reasoning,
                            str_of(block, "thinking"),
                            "Thinking",
                        )
                    };
                    let text = text.unwrap_or_default();
                    if text.is_empty() {
                        continue; // redacted thinking: nothing to show
                    }
                    let item = format!("{msg_id}:{index}");
                    let fresh = !self.started_items.contains(&item);
                    if fresh {
                        self.open_item(&item);
                        out.push(
                            Envelope::new(Event::ItemStarted {
                                kind,
                                title: title.to_owned(),
                                input: None,
                                parent: parent.clone(),
                            })
                            .item(item.clone()),
                        );
                    }
                    out.push(
                        Envelope::new(Event::ContentSnapshot {
                            stream,
                            text: text.to_owned(),
                        })
                        .item(item.clone()),
                    );
                    if fresh {
                        // No stream preceded this snapshot, so nothing will close the item.
                        self.close_item(&item);
                        out.push(
                            Envelope::new(Event::ItemCompleted {
                                status: ItemStatus::Completed,
                                output: None,
                                error: None,
                            })
                            .item(item),
                        );
                    }
                }
                "tool_use" | "server_tool_use" | "mcp_tool_use" => {
                    let (Some(id), Some(name)) = (str_of(block, "id"), str_of(block, "name"))
                    else {
                        out.push(Envelope::new(Event::Unknown));
                        continue;
                    };
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    if self.started_items.contains(id) {
                        out.push(
                            Envelope::new(Event::ContentSnapshot {
                                stream: StreamKind::ToolInput,
                                text: input.to_string(),
                            })
                            .item(id),
                        );
                    } else {
                        self.open_child(id, parent.as_deref());
                        out.push(
                            Envelope::new(Event::ItemStarted {
                                kind: tool_kind(name),
                                title: name.to_owned(),
                                input: Some(input),
                                parent: parent.clone(),
                            })
                            .item(id),
                        );
                    }
                }
                _ => out.push(Envelope::new(Event::Unknown)),
            }
        }
        out
    }

    fn user(&mut self, v: &Value) -> Vec<Envelope> {
        let uuid = str_of(v, "uuid").unwrap_or_default().to_owned();
        let content = v
            .get("message")
            .and_then(|m| m.get("content"))
            .unwrap_or(&Value::Null);
        let mut out = Vec::new();

        // Tool results complete their item.
        if let Value::Array(blocks) = content {
            let results: Vec<&Value> = blocks
                .iter()
                .filter(|b| str_of(b, "type") == Some("tool_result"))
                .collect();
            // The frame-level result metadata describes a frame's single result.
            let async_launch = results.len() == 1 && is_async_launch(v);
            let had_results = !results.is_empty();
            for b in results {
                let Some(id) = str_of(b, "tool_use_id") else {
                    continue;
                };
                // A backgrounded sub-agent answers its `Agent` call at once with internal
                // "launched" metadata; the item stays open until its `task_notification`.
                // `task_started` says so first; the structured result is the fallback.
                if self.background.contains(id) || async_launch {
                    self.background.insert(id.to_owned());
                    continue;
                }
                let text = tool_result_text(b.get("content").unwrap_or(&Value::Null));
                let failed = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                self.close_item(id);
                out.push(
                    Envelope::new(Event::ItemCompleted {
                        status: if failed {
                            ItemStatus::Failed
                        } else {
                            ItemStatus::Completed
                        },
                        output: Some(text.clone()),
                        error: failed.then_some(text),
                    })
                    .item(id),
                );
            }
            if had_results {
                return out;
            }
        }

        let text = content_text(content);
        let synthetic = v.get("isSynthetic").and_then(Value::as_bool) == Some(true);
        let local =
            text.contains("<local-command-stdout>") || text.contains("<local-command-stderr>");
        let interrupted = text.starts_with("[Request interrupted");
        if (synthetic || local || interrupted) && self.first_time(format!("{uuid}#user")) {
            return notice(strip_local_tags(&text));
        }
        Vec::new()
    }

    fn control_request(&mut self, v: &Value) -> Vec<Envelope> {
        let req = v.get("request").unwrap_or(&Value::Null);
        let Some(request_id) = str_of(v, "request_id") else {
            return vec![Envelope::new(Event::Unknown)];
        };
        if str_of(req, "subtype") != Some("can_use_tool") {
            return vec![Envelope::new(Event::Unknown)];
        }
        let tool = str_of(req, "tool_name").unwrap_or("tool").to_owned();
        let tool_use_id = str_of(req, "tool_use_id").unwrap_or(request_id).to_owned();
        let input = req.get("input").cloned().unwrap_or(Value::Null);
        let suggestions = array_of(req, "permission_suggestions");
        // "Always allow" persists rules, so only when Claude suggested one (a `setMode` alone is
        // not a rule worth writing to a settings file). The card shows exactly what it saves:
        // Claude's rules are often prefixes (`git status:*`), broader than the call itself.
        let remembers = rules_text(&suggestions);
        let mut options = vec![Decision::Allow, Decision::AllowForSession];
        if remembers.is_some() {
            options.push(Decision::AllowAlways);
        }
        options.push(Decision::Deny);
        self.pending.insert(
            request_id.to_owned(),
            PendingApproval {
                tool_use_id: tool_use_id.clone(),
                input: input.clone(),
                suggestions,
            },
        );

        let event = if tool == "AskUserQuestion" {
            Event::QuestionRequested {
                questions: questions_of(&input),
            }
        } else {
            Event::ApprovalRequested {
                title: str_of(req, "title")
                    .or_else(|| str_of(req, "description"))
                    .map(str::to_owned),
                reason: str_of(req, "decision_reason").map(str::to_owned),
                tool,
                input,
                options,
                response: ResponseCapability::Live,
                remembers,
            }
        };
        vec![Envelope::new(event).item(tool_use_id).request(request_id)]
    }

    fn control_cancel(&mut self, v: &Value) -> Vec<Envelope> {
        let Some(id) = str_of(v, "request_id") else {
            return vec![Envelope::new(Event::Unknown)];
        };
        match self.pending.remove(id) {
            Some(p) => vec![Envelope::new(Event::ApprovalResolved {
                decision: Decision::Cancel,
            })
            .item(p.tool_use_id)
            .request(id)],
            None => Vec::new(),
        }
    }

    fn control_response(&mut self, v: &Value) -> Vec<Envelope> {
        let resp = v.get("response").unwrap_or(&Value::Null);
        let Some(id) = str_of(resp, "request_id") else {
            return vec![Envelope::new(Event::Unknown)];
        };
        self.pending_controls.remove(id);
        let body = resp.get("response").cloned();
        let mut out = Vec::new();

        let mode_set = self.mode_requests.remove(id);
        let model_set = self.model_requests.remove(id);
        if str_of(resp, "subtype") == Some("error") {
            self.usage_requests.remove(id);
            self.context_requests.remove(id);
            let error = str_of(resp, "error")
                .unwrap_or("control request failed")
                .to_owned();
            out.push(
                Envelope::new(Event::ControlResult {
                    ok: None,
                    error: Some(error.clone()),
                })
                .request(id),
            );
            if let Some((_, model)) = model_set {
                out.push(
                    Envelope::new(Event::ModelChangeFailed {
                        message: format!("Claude set_model to {model} failed: {error}"),
                        model,
                    })
                    .request(id),
                );
            }
            return out;
        }

        out.push(
            Envelope::new(Event::ControlResult {
                ok: Some(body.clone().unwrap_or_else(|| json!({}))),
                error: None,
            })
            .request(id),
        );
        if let Some(mode) = mode_set {
            self.mode = Some(mode);
            out.push(Envelope::new(Event::ModeChanged { mode }));
        }
        if let Some((order, model)) = model_set {
            if str_of(resp, "subtype") == Some("success") && order > self.model_ack_order {
                self.model_ack_order = order;
                self.model = Some(model.clone());
                out.push(Envelope::new(Event::ModelChanged { model }).request(id));
            }
        }
        let body = body.unwrap_or(Value::Null);
        if self.init_id.as_deref() == Some(id) {
            if let Some(account) = crate::quota::claude_account(&body) {
                out.push(Envelope::new(Event::QuotaUpdated {
                    account: Some(account),
                    windows: Vec::new(),
                }));
            }
            if let Value::Array(list) = body.get("commands").unwrap_or(&Value::Null) {
                out.push(Envelope::new(self.adopt_commands(list)));
            }
        }
        if self.usage_requests.remove(id) {
            let windows = crate::quota::claude_usage(&body);
            if !windows.is_empty() {
                out.push(Envelope::new(Event::QuotaUpdated {
                    account: None,
                    windows,
                }));
            }
        }
        if self.context_requests.remove(id) {
            if let Some(used) = u64_of(&body, "totalTokens") {
                out.push(Envelope::new(Event::UsageUpdated {
                    used,
                    max: u64_of(&body, "maxTokens"),
                    auto_compact_at: u64_of(&body, "autoCompactThreshold"),
                }));
            }
        }
        out
    }

    /// The CLI sends `rate_limit_event` every turn, mostly with `status: "allowed"`. Only a refusal
    /// is a rate limit (it drives "Continue in …"); a warning is a notice; anything else is quiet.
    fn rate_limit(&mut self, v: &Value) -> Vec<Envelope> {
        let info = v.get("rate_limit_info").cloned();
        let status = info
            .as_ref()
            .and_then(|i| str_of(i, "status"))
            .unwrap_or("")
            .to_owned();
        let resets_at = info
            .as_ref()
            .and_then(|i| i.get("resetsAt"))
            .and_then(Value::as_u64)
            .map(|s| s.to_string()); // epoch seconds; the UI formats it
                                     // Every event carries the plan windows, whatever its status: the usage indicator
                                     // updates on every turn. `account: None` leaves the known account unchanged.
        let windows = info
            .as_ref()
            .map(crate::quota::claude_turn_windows)
            .unwrap_or_default();
        let mut out = Vec::new();
        if !windows.is_empty() {
            out.push(Envelope::new(Event::QuotaUpdated {
                account: None,
                windows,
            }));
        }
        match status.as_str() {
            "rejected" => out.push(Envelope::new(Event::RateLimited {
                resets_at,
                detail: info,
            })),
            "allowed_warning" => out.extend(notice("Approaching the usage limit.".to_owned())),
            _ => {}
        }
        out
    }

    fn result(&mut self, v: &Value) -> Vec<Envelope> {
        // A turn Claude ran itself to report a finished background task ends with a `result`
        // whose `origin.kind` is "task-notification". It normally follows its own `init`; one
        // with no turn open (print mode flushes both results at the end) ends nothing new.
        let from_task =
            v.get("origin").and_then(|o| str_of(o, "kind")) == Some("task-notification");
        if from_task && !self.turn_open {
            return Vec::new();
        }
        self.turn_open = false;
        let interrupted_by_us = std::mem::take(&mut self.interrupt_pending);
        self.blocks.clear();
        self.msg_ids.clear();
        self.snap_counts.clear();
        self.started_items.clear();

        let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
        let success = str_of(v, "subtype") == Some("success");
        // An abort, or an error result whose `errors` say the user stopped the run. Only those
        // phrases: a bare "cancel" or "interrupt" ("connection cancelled by server",
        // "interrupted system call") is a real failure and keeps its error text.
        let said_cancelled = (is_error || !success)
            && strings_of(v, "errors").iter().any(|e| {
                let e = e.to_ascii_lowercase();
                [
                    "interrupted by user",
                    "cancelled by user",
                    "canceled by user",
                    "request was aborted",
                ]
                .iter()
                .any(|p| e.contains(p))
            });
        let aborted = matches!(
            str_of(v, "terminal_reason"),
            Some("aborted_streaming" | "aborted_tools")
        ) || said_cancelled;
        let (state, error) = if aborted || (interrupted_by_us && (is_error || !success)) {
            (TurnState::Interrupted, None)
        } else if success && !is_error {
            (TurnState::Completed, None)
        } else {
            (TurnState::Failed, Some(failure_text(v)))
        };
        let usage = v.get("usage").map(usage_of);
        // Anything still open never got its own close: the turn is over, so settle it. A
        // backgrounded sub-agent and everything inside it outlive the turn: they finish on
        // their `task_notification`.
        let (keep, settle): (Vec<String>, Vec<String>) = std::mem::take(&mut self.open_items)
            .into_iter()
            .partition(|i| self.in_background(i));
        self.open_items = keep;
        let mut out: Vec<Envelope> = settle
            .into_iter()
            .map(|item| {
                let interrupted = state == TurnState::Interrupted;
                Envelope::new(Event::ItemCompleted {
                    status: if interrupted {
                        ItemStatus::Interrupted
                    } else {
                        ItemStatus::Failed
                    },
                    output: None,
                    error: (!interrupted).then(|| "no result for this item".to_owned()),
                })
                .item(item)
            })
            .collect();
        out.push(Envelope::new(Event::TurnCompleted {
            state,
            usage,
            cost_usd: v.get("total_cost_usd").and_then(Value::as_f64),
            error,
        }));
        out
    }
}

impl Adapter for ClaudeAdapter {
    fn driver(&self) -> Driver {
        Driver::Claude
    }

    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    fn argv(&self, session: &OpenSession) -> Vec<String> {
        let mut argv = vec![session.program.clone()];
        argv.extend(session.extra_args.iter().cloned());
        argv.extend(
            [
                "--output-format",
                "stream-json",
                "--verbose",
                "--input-format",
                "stream-json",
                "--permission-prompt-tool",
                "stdio",
                "--include-partial-messages",
            ]
            .map(str::to_owned),
        );
        if let Some(model) = &session.model {
            argv.push("--model".to_owned());
            argv.push(model.clone());
        }
        if let Some(effort) = &session.effort {
            argv.push("--effort".to_owned());
            argv.push(effort.clone());
        }
        // The effort this process runs with, for `SetModel`'s respawn decision.
        *self.current_effort.borrow_mut() = session.effort.clone();
        argv.push("--permission-mode".to_owned());
        argv.push(mode_name(session.mode).to_owned());
        // Session flags take `=`; resuming wins over naming a new session.
        if let Some(id) = &session.resume {
            argv.push(format!("--resume={id}"));
        } else if let Some(id) = &session.new_session_id {
            argv.push(format!("--session-id={id}"));
        }
        argv
    }

    fn handshake(&mut self) -> Vec<String> {
        // The host reuses this adapter when effort changes respawn the process. Its first
        // init must confirm the restarted session even if only effort changed.
        self.session_started = false;
        self.init_seen = false;
        self.commands_seen = false;
        self.mode = None;
        self.interrupt_pending = false;
        let id = self.next_id("init");
        self.init_id = Some(id.clone());
        vec![self.control_line(&id, json!({"subtype": "initialize"}))]
    }

    fn encode(&mut self, command: Command) -> Result<Vec<Action>, AdapterError> {
        let line = match command {
            Command::Prompt { text } => json!({
                "type": "user",
                "message": {"role": "user", "content": text},
                "parent_tool_use_id": null,
            })
            .to_string(),
            Command::Interrupt => {
                // Only an open turn can be interrupted; a stale flag would mislabel the next
                // result or exit.
                self.interrupt_pending = self.turn_open;
                let id = self.next_id("int");
                self.control_line(&id, json!({"subtype": "interrupt"}))
            }
            Command::Approve {
                request,
                decision,
                updated_input,
                message,
            } => self.answer_line(&request, decision, updated_input, message)?,
            Command::Answer { request, answers } => {
                let input = self
                    .pending
                    .get(&request)
                    .map(|p| p.input.clone())
                    .ok_or_else(|| unknown_request(&request))?;
                let mut merged = match input {
                    Value::Object(m) => m,
                    _ => Map::new(),
                };
                merged.insert("answers".to_owned(), answers);
                self.answer_line(&request, Decision::Allow, Some(Value::Object(merged)), None)?
            }
            Command::SetModel { model, effort } => {
                // There is no in-session effort control: a different effort means a new process
                // (`--effort`) resumed on the same session. The same effort (or none given)
                // stays a live `set_model`.
                if effort.is_some() && effort != *self.current_effort.borrow() {
                    return Ok(vec![Action::Respawn(OpenSessionDelta {
                        model: Some(model),
                        effort,
                        mode: None,
                        // The host fills in the native session id it already holds.
                        resume: None,
                    })]);
                }
                let id = self.next_id("model");
                self.model_requests
                    .insert(id.clone(), (self.counter, model.clone()));
                self.outbox.events.push(
                    Envelope::new(Event::ModelChangeRequested {
                        model: model.clone(),
                    })
                    .request(&id),
                );
                self.control_line(&id, json!({"subtype": "set_model", "model": model}))
            }
            Command::SetMode { mode } => {
                let id = self.next_id("mode");
                self.mode_requests.insert(id.clone(), mode);
                self.control_line(
                    &id,
                    json!({"subtype": "set_permission_mode", "mode": mode_name(mode)}),
                )
            }
            Command::Control { id, control } => {
                if matches!(control, Control::ContextUsage) {
                    self.context_requests.insert(id.clone());
                }
                if matches!(control, Control::Usage) {
                    self.usage_requests.insert(id.clone());
                }
                let request = control_request_body(&control);
                self.control_line(&id, request)
            }
        };
        Ok(vec![Action::Write(vec![line])])
    }

    fn feed(&mut self, line: &str) -> Vec<Envelope> {
        if line.trim().is_empty() {
            return Vec::new();
        }
        let Ok(frame) = serde_json::from_str::<Value>(line) else {
            return vec![Envelope::new(Event::Unknown).raw(Value::String(line.to_owned()))];
        };
        let mut out = self.map_frame(&frame);
        // One copy of the frame per line, on the first envelope only.
        if let Some(first) = out.first_mut() {
            first.raw = Some(frame);
        }
        out
    }

    fn drain_outbox(&mut self) -> Outbox {
        std::mem::take(&mut self.outbox)
    }

    fn feed_stderr(&mut self, line: &str) -> Vec<Envelope> {
        let lower = line.to_ascii_lowercase();
        let looks_bad = ["error", "fatal", "panic", "exception", "failed"]
            .iter()
            .any(|w| lower.contains(w));
        if !looks_bad {
            return Vec::new();
        }
        vec![Envelope::new(Event::Error {
            message: line.trim().to_owned(),
        })
        .raw(Value::String(line.to_owned()))]
    }

    fn feed_side(&mut self, _id: &str, stdout: &str, _success: bool) -> Vec<Envelope> {
        // Claude has no side processes; keep whatever arrives rather than lose it.
        vec![Envelope::new(Event::Unknown).raw(Value::String(stdout.to_owned()))]
    }

    fn on_exit(&mut self, code: Option<i32>) -> Vec<Envelope> {
        let expected = self.interrupt_pending;
        let mut out = Vec::new();
        // Background tasks die with the process: sub-agents are settled with everything else,
        // and the task list empties.
        self.background.clear();
        self.background_children.clear();
        self.task_tools.clear();
        if !std::mem::take(&mut self.tasks).is_empty() {
            out.push(Envelope::new(Event::BackgroundTasks { tasks: Vec::new() }));
        }
        for item in std::mem::take(&mut self.open_items) {
            out.push(
                Envelope::new(Event::ItemCompleted {
                    status: ItemStatus::Interrupted,
                    output: None,
                    error: None,
                })
                .item(item),
            );
        }
        let mut pending: Vec<_> = std::mem::take(&mut self.pending).into_iter().collect();
        pending.sort_by(|a, b| a.0.cmp(&b.0));
        for (request, p) in pending {
            out.push(
                Envelope::new(Event::ApprovalResolved {
                    decision: Decision::Cancel,
                })
                .item(p.tool_use_id)
                .request(request),
            );
        }
        let mut controls: Vec<_> = std::mem::take(&mut self.pending_controls)
            .into_iter()
            .collect();
        controls.sort();
        for id in controls {
            out.push(
                Envelope::new(Event::ControlResult {
                    ok: None,
                    error: Some("agent exited".to_owned()),
                })
                .request(&id),
            );
            if let Some((_, model)) = self.model_requests.remove(&id) {
                out.push(Envelope::new(Event::ModelChangeFailed {
                    message: format!("Claude set_model to {model} failed: agent exited before acknowledgement"),
                    model,
                }).request(id));
            }
        }
        self.model_requests.clear();
        self.mode_requests.clear();
        self.context_requests.clear();
        self.usage_requests.clear();
        self.outbox = Outbox::default();
        if std::mem::take(&mut self.turn_open) {
            out.push(Envelope::new(Event::TurnCompleted {
                state: if expected {
                    TurnState::Interrupted
                } else {
                    TurnState::Failed
                },
                usage: None,
                cost_usd: None,
                error: (!expected).then(|| format!("agent exited unexpectedly (code {code:?})")),
            }));
        }
        out.push(Envelope::new(Event::SessionExited { code, expected }));
        self.blocks.clear();
        self.started_items.clear();
        out
    }
}

impl ClaudeAdapter {
    /// Build the `control_response` for a pending `can_use_tool` request.
    fn answer_line(
        &mut self,
        request: &str,
        decision: Decision,
        updated_input: Option<Value>,
        message: Option<String>,
    ) -> Result<String, AdapterError> {
        // Checked before the request is taken, so a refused answer leaves it answerable.
        let offered_always = self
            .pending
            .get(request)
            .is_some_and(|p| p.suggestions.iter().any(is_rule_suggestion));
        if decision == Decision::AllowAlways && !offered_always {
            return Err(AdapterError::Invalid(format!(
                "no rule to remember for approval {request}"
            )));
        }
        let p = self
            .pending
            .remove(request)
            .ok_or_else(|| unknown_request(request))?;
        let mut body = Map::new();
        match decision {
            Decision::Allow | Decision::AllowForSession | Decision::AllowAlways => {
                body.insert("behavior".to_owned(), json!("allow"));
                body.insert(
                    "updatedInput".to_owned(),
                    updated_input.unwrap_or_else(|| p.input.clone()),
                );
                // The scope is ours to set, never whatever Claude suggested: "for session" stays
                // in memory, "always" writes only rules, to the project's untracked
                // `.claude/settings.local.json` (`localSettings`).
                let scoped: Vec<Value> = match decision {
                    Decision::AllowForSession => p
                        .suggestions
                        .iter()
                        .map(|s| with_destination(s, "session"))
                        .collect(),
                    Decision::AllowAlways => p
                        .suggestions
                        .iter()
                        .filter(|s| is_rule_suggestion(s))
                        .map(|s| with_destination(s, "localSettings"))
                        .collect(),
                    _ => Vec::new(),
                };
                if !scoped.is_empty() {
                    body.insert("updatedPermissions".to_owned(), Value::Array(scoped));
                }
            }
            Decision::Deny | Decision::Cancel => {
                body.insert("behavior".to_owned(), json!("deny"));
                body.insert(
                    "message".to_owned(),
                    json!(message.unwrap_or_else(|| "The user denied this request".to_owned())),
                );
                if decision == Decision::Cancel {
                    body.insert("interrupt".to_owned(), json!(true));
                }
            }
        }
        body.insert("toolUseID".to_owned(), json!(p.tool_use_id));
        Ok(json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request,
                "response": Value::Object(body),
            },
        })
        .to_string())
    }
}

// ----- free helpers -----

/// A permission suggestion that adds allow rules (`{"type":"addRules","behavior":"allow",…}`),
/// the only kind "Always allow" persists. The behaviour must say "allow": anything else (or
/// nothing) is never saved as an allow.
fn is_rule_suggestion(s: &Value) -> bool {
    str_of(s, "type") == Some("addRules") && str_of(s, "behavior") == Some("allow")
}

/// What "Always allow" would save, as Claude writes rules (`Bash(git status:*)`, `WebFetch`),
/// for the project; `None` when there is no rule to save.
fn rules_text(suggestions: &[Value]) -> Option<String> {
    let rules: Vec<String> = suggestions
        .iter()
        .filter(|s| is_rule_suggestion(s))
        .flat_map(|s| array_of(s, "rules"))
        .filter_map(|r| {
            let tool = str_of(&r, "toolName")?;
            Some(match str_of(&r, "ruleContent").filter(|c| !c.is_empty()) {
                Some(content) => format!("{tool}({content})"),
                None => tool.to_owned(),
            })
        })
        .collect();
    (!rules.is_empty()).then(|| format!("{} in this project", rules.join(", ")))
}

/// `s` with its `destination` (where Claude applies or saves it) replaced.
fn with_destination(s: &Value, destination: &str) -> Value {
    let mut s = s.clone();
    if let Some(obj) = s.as_object_mut() {
        obj.insert("destination".to_owned(), json!(destination));
    }
    s
}

fn unknown_request(id: &str) -> AdapterError {
    AdapterError::Invalid(format!(
        "no pending approval {id} (answered, cancelled or expired)"
    ))
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Ask => "default",
        Mode::AcceptEdits => "acceptEdits",
        Mode::Plan => "plan",
    }
}

/// [`mode_name`] backwards; `None` for Claude's modes the picker has no entry for
/// (`bypassPermissions`, `dontAsk`).
fn mode_from_name(name: &str) -> Option<Mode> {
    match name {
        "default" => Some(Mode::Ask),
        "acceptEdits" => Some(Mode::AcceptEdits),
        "plan" => Some(Mode::Plan),
        _ => None,
    }
}

fn control_request_body(control: &Control) -> Value {
    match control {
        Control::McpStatus => json!({"subtype": "mcp_status"}),
        Control::McpToggle { server, enabled } => {
            json!({"subtype": "mcp_toggle", "serverName": server, "enabled": enabled})
        }
        Control::McpReconnect { server } => {
            json!({"subtype": "mcp_reconnect", "serverName": server})
        }
        Control::GetSettings => json!({"subtype": "get_settings"}),
        Control::ListModels => json!({"subtype": "list_models"}),
        Control::FileSuggestions { query } => {
            json!({"subtype": "file_suggestions", "query": query})
        }
        Control::ContextUsage => json!({"subtype": "get_context_usage"}),
        Control::Usage => json!({"subtype": "get_usage"}),
    }
}

fn tool_kind(name: &str) -> ItemKind {
    match name {
        "Bash" => ItemKind::Command,
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => ItemKind::FileChange,
        "Read" | "Glob" | "Grep" => ItemKind::FileRead,
        "WebSearch" | "WebFetch" => ItemKind::WebSearch,
        "Task" | "Agent" => ItemKind::Subagent,
        n if n.starts_with("mcp__") => ItemKind::McpTool,
        _ => ItemKind::Tool,
    }
}

/// Whether a `user` record holding a tool result is a backgrounded sub-agent's immediate
/// "launched" answer. Stream-json names the metadata `tool_use_result`, the transcript
/// `toolUseResult`; both carry `{"isAsync": true, "status": "async_launched", …}`.
pub(crate) fn is_async_launch(record: &Value) -> bool {
    ["tool_use_result", "toolUseResult"]
        .iter()
        .filter_map(|k| record.get(*k))
        .any(|r| {
            r.get("isAsync").and_then(Value::as_bool) == Some(true)
                || str_of(r, "status") == Some("async_launched")
        })
}

/// A background task's `task_type`. Seen: `local_agent`, `local_bash`, `bash`.
fn task_kind(task_type: &str) -> BackgroundTaskKind {
    match task_type {
        t if t.ends_with("agent") => BackgroundTaskKind::Agent,
        t if t.ends_with("bash") || t.ends_with("shell") => BackgroundTaskKind::Shell,
        _ => BackgroundTaskKind::Other,
    }
}

/// A background task's final status (`task_notification.status`) as an item status. Claude
/// reports `completed`, `failed`, `killed` and `stopped`; an unknown one counts as a failure.
pub(crate) fn task_status(status: &str) -> ItemStatus {
    match status {
        "completed" => ItemStatus::Completed,
        "killed" | "stopped" | "cancelled" | "canceled" | "interrupted" => ItemStatus::Interrupted,
        _ => ItemStatus::Failed,
    }
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn u64_of(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}

fn array_of(v: &Value, key: &str) -> Vec<Value> {
    v.get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn strings_of(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(|s| s.trim_start_matches('/').to_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn non_empty_object(v: Option<&Value>) -> Option<Value> {
    match v {
        Some(Value::Object(m)) if !m.is_empty() => v.cloned(),
        _ => None,
    }
}

/// Text of a message `content`: a plain string, or the `text` of each block.
fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| str_of(b, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Text of a `tool_result`'s `content`: like [`content_text`], but a block with
/// no text (an image, a `tool_reference`) shows as a `[type]` placeholder, so a
/// result made only of those is not an empty output.
fn tool_result_text(content: &Value) -> String {
    let Value::Array(blocks) = content else {
        return content_text(content);
    };
    blocks
        .iter()
        .filter_map(|b| match (str_of(b, "text"), str_of(b, "type")) {
            (Some(t), _) => Some(t.to_owned()),
            (None, Some(kind)) => Some(format!("[{kind}]")),
            (None, None) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn strip_local_tags(text: &str) -> String {
    let mut s = text.to_owned();
    for tag in [
        "<local-command-stdout>",
        "</local-command-stdout>",
        "<local-command-stderr>",
        "</local-command-stderr>",
    ] {
        s = s.replace(tag, "");
    }
    s.trim().to_owned()
}

fn notice(text: String) -> Vec<Envelope> {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![Envelope::new(Event::Notice { text })]
    }
}

/// Input includes cache reads and writes; output already includes thinking tokens.
fn usage_of(u: &Value) -> Usage {
    let n = |k: &str| u64_of(u, k).unwrap_or(0);
    Usage {
        input_tokens: n("input_tokens")
            + n("cache_read_input_tokens")
            + n("cache_creation_input_tokens"),
        output_tokens: n("output_tokens"),
        cached_input_tokens: n("cache_read_input_tokens"),
        reasoning_tokens: u
            .get("output_tokens_details")
            .and_then(|d| u64_of(d, "thinking_tokens"))
            .unwrap_or(0),
    }
}

/// Why a turn failed. `subtype: "success"` with `is_error` is a real failure (401, 429, 529).
fn failure_text(v: &Value) -> String {
    if let Some(r) = str_of(v, "result").filter(|r| !r.is_empty()) {
        return r.to_owned();
    }
    let errors: Vec<&str> = v
        .get("errors")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !errors.is_empty() {
        return errors.join("; ");
    }
    if let Some(status) = v.get("api_error_status").filter(|s| !s.is_null()) {
        return format!("API error {status}");
    }
    str_of(v, "subtype").unwrap_or("error").to_owned()
}

fn questions_of(input: &Value) -> Vec<Question> {
    array_of(input, "questions")
        .iter()
        .enumerate()
        .map(|(i, q)| Question {
            id: format!("q{i}"),
            header: str_of(q, "header").unwrap_or_default().to_owned(),
            question: str_of(q, "question").unwrap_or_default().to_owned(),
            options: array_of(q, "options")
                .iter()
                .map(|o| QuestionOption {
                    label: str_of(o, "label").unwrap_or_default().to_owned(),
                    description: str_of(o, "description").map(str::to_owned),
                })
                .collect(),
            multi_select: q
                .get("multiSelect")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_refused_rate_limit_is_reported() {
        let frame = |status: &str| {
            json!({"type": "rate_limit_event", "rate_limit_info": {"status": status, "resetsAt": 1_800_000_000u64}})
                .to_string()
        };
        let mut a = ClaudeAdapter::new();
        assert!(a.feed(&frame("allowed")).is_empty());
        let warn = a.feed(&frame("allowed_warning"));
        assert!(matches!(warn.as_slice(), [e] if matches!(e.event, Event::Notice { .. })));
        let refused = a.feed(&frame("rejected"));
        assert!(matches!(
            refused.as_slice(),
            [e] if matches!(&e.event, Event::RateLimited { resets_at: Some(r), .. } if r == "1800000000")
        ));
    }

    const APPROVAL: &str = include_str!("../tests/fixtures/claude-turn-approval.ndjson");
    const CONTROLS: &str = include_str!("../tests/fixtures/claude-controls-slash.ndjson");
    const INTERRUPT: &str = include_str!("../tests/fixtures/claude-interrupt.ndjson");

    /// Feed every stdout frame of a recording.
    fn replay(fixture: &str) -> (ClaudeAdapter, Vec<Envelope>) {
        let mut a = ClaudeAdapter::new();
        let mut out = Vec::new();
        for line in fixture.lines().filter(|l| !l.trim().is_empty()) {
            let rec: Value = serde_json::from_str(line).expect("fixture line is JSON");
            if str_of(&rec, "dir") == Some("out") {
                out.extend(a.feed(&rec["frame"].to_string()));
            }
        }
        (a, out)
    }

    fn written(a: &mut ClaudeAdapter, c: Command) -> Value {
        let actions = a.encode(c).expect("encodes");
        match actions.as_slice() {
            [Action::Write(lines)] if lines.len() == 1 => {
                serde_json::from_str(&lines[0]).expect("line is JSON")
            }
            other => panic!("expected one Write, got {other:?}"),
        }
    }

    fn events(out: &[Envelope]) -> Vec<&Event> {
        out.iter().map(|e| &e.event).collect()
    }

    #[test]
    fn a_mode_claude_accepts_or_reports_becomes_mode_changed() {
        let mut a = ClaudeAdapter::new();
        let init = |mode: &str| {
            json!({"type": "system", "subtype": "init", "model": "m", "session_id": "s",
                   "permissionMode": mode})
            .to_string()
        };
        let changed = |out: &[Envelope]| -> Vec<Mode> {
            out.iter()
                .filter_map(|e| match e.event {
                    Event::ModeChanged { mode } => Some(mode),
                    _ => None,
                })
                .collect()
        };
        // The first init is the mode it was started with: nothing new.
        assert!(changed(&a.feed(&init("default"))).is_empty());
        // The picker asks; Claude's success reply is the change.
        let req = written(
            &mut a,
            Command::SetMode {
                mode: Mode::AcceptEdits,
            },
        );
        let id = req["request_id"].as_str().expect("id").to_owned();
        let ok = json!({"type": "control_response",
                        "response": {"subtype": "success", "request_id": id}});
        assert_eq!(changed(&a.feed(&ok.to_string())), [Mode::AcceptEdits]);
        // Reported again as it already is: nothing.
        assert!(changed(&a.feed(&init("acceptEdits"))).is_empty());
        // A refused request changes nothing.
        let req = written(&mut a, Command::SetMode { mode: Mode::Plan });
        let id = req["request_id"].as_str().expect("id").to_owned();
        let err = json!({"type": "control_response",
                         "response": {"subtype": "error", "request_id": id, "error": "no"}});
        assert!(changed(&a.feed(&err.to_string())).is_empty());
        // Claude changing it itself (a plan accepted) shows at the next turn.
        assert_eq!(changed(&a.feed(&init("default"))), [Mode::Ask]);
        // A mode the picker has no entry for is not forced onto it.
        assert!(changed(&a.feed(&init("bypassPermissions"))).is_empty());
    }

    #[test]
    fn every_turn_updates_the_quota_and_initialize_brings_the_account() {
        // The per-turn rate_limit_event carries both plan windows, with the account unchanged.
        let (_, out) = replay(APPROVAL);
        let quotas: Vec<_> = events(&out)
            .into_iter()
            .filter_map(|e| match e {
                Event::QuotaUpdated { account, windows } => Some((account, windows)),
                _ => None,
            })
            .collect();
        assert_eq!(quotas.len(), 1, "one rate_limit_event in the recording");
        assert!(quotas[0].0.is_none());
        assert_eq!(quotas[0].1.len(), 2);
        assert!(
            events(&out)
                .iter()
                .all(|e| !matches!(e, Event::RateLimited { .. })),
            "an allowed event is not a rate limit"
        );

        // The initialize reply carries the account and no windows.
        let mut a = ClaudeAdapter::new();
        a.handshake(); // the recording starts with the same request id
        let mut out = Vec::new();
        for line in CONTROLS.lines().filter(|l| !l.trim().is_empty()) {
            let rec: Value = serde_json::from_str(line).expect("fixture line is JSON");
            if str_of(&rec, "dir") == Some("out") {
                out.extend(a.feed(&rec["frame"].to_string()));
            }
        }
        let account = events(&out).into_iter().find_map(|e| match e {
            Event::QuotaUpdated {
                account: Some(a),
                windows,
            } if windows.is_empty() => Some(a),
            _ => None,
        });
        assert_eq!(account.expect("account").label, "user@example.com");
    }

    #[test]
    fn a_get_usage_reply_becomes_a_quota_update() {
        let mut a = ClaudeAdapter::default();
        a.encode(Command::Control {
            id: "usage-1".into(),
            control: Control::Usage,
        })
        .expect("encode");
        let reply = json!({"type": "control_response", "response": {
            "subtype": "success", "request_id": "usage-1",
            "response": {"rate_limits": {"limits": [
                {"kind": "session", "percent": 50, "resets_at": "2026-10-06T16:30:00+00:00"}]}}}});
        let out = a.feed(&reply.to_string());
        assert!(matches!(
            &out[0].event,
            Event::ControlResult { ok: Some(_), .. }
        ));
        assert!(
            matches!(&out[1].event, Event::QuotaUpdated { account: None, windows }
            if windows.len() == 1 && windows[0].used == 0.5)
        );
        // Other control replies do not.
        let out = a.feed(&reply.to_string());
        assert!(out
            .iter()
            .all(|e| !matches!(e.event, Event::QuotaUpdated { .. })));
    }

    #[test]
    fn approval_fixture_sequence() {
        let (_, out) = replay(APPROVAL);
        let evs = events(&out);
        let pos = |f: &dyn Fn(&Event) -> bool| evs.iter().position(|e| f(e));
        let started = pos(&|e| matches!(e, Event::SessionStarted { .. })).expect("SessionStarted");
        let approval =
            pos(&|e| matches!(e, Event::ApprovalRequested { tool, .. } if tool == "Edit"))
                .expect("ApprovalRequested for Edit");
        let done = evs
            .iter()
            .rposition(|e| {
                matches!(
                    e,
                    Event::TurnCompleted {
                        state: TurnState::Completed,
                        ..
                    }
                )
            })
            .expect("TurnCompleted Completed");
        assert!(started < approval && approval < done);

        // The edit's tool_result completes its item after the approval.
        let edit_done = out
            .iter()
            .position(|e| {
                e.item.as_deref() == Some("toolu_01FtAUGzxPrHFJZCNmbyLX9W")
                    && matches!(
                        e.event,
                        Event::ItemCompleted {
                            status: ItemStatus::Completed,
                            ..
                        }
                    )
            })
            .expect("ItemCompleted for the edit");
        assert!(approval < edit_done && edit_done < done);

        // The approval is addressable, tied to its tool item, and live.
        let req = &out[approval];
        assert_eq!(
            req.request.as_deref(),
            Some("db5b803e-b2b5-4786-b647-34c4702d90f0")
        );
        assert_eq!(req.item.as_deref(), Some("toolu_01FtAUGzxPrHFJZCNmbyLX9W"));
        assert!(matches!(
            req.event,
            Event::ApprovalRequested {
                response: ResponseCapability::Live,
                ..
            }
        ));

        // The edit is a file change; the read before it is a file read.
        assert!(out.iter().any(|e| matches!(
            &e.event,
            Event::ItemStarted { kind: ItemKind::FileChange, title, .. } if title == "Edit"
        )));
        assert!(out.iter().any(|e| matches!(
            &e.event,
            Event::ItemStarted {
                kind: ItemKind::FileRead,
                ..
            }
        )));
        // Tool arguments stream in, and text streams out.
        assert!(out.iter().any(|e| matches!(
            e.event,
            Event::ContentDelta {
                stream: StreamKind::ToolInput,
                ..
            }
        )));
        assert!(out.iter().any(|e| matches!(
            &e.event,
            Event::ContentDelta { stream: StreamKind::Assistant, text } if text == "Done"
        )));
        // The final text snapshot lands on the streamed item, with no second ItemStarted.
        let snapshot = out
            .iter()
            .find(|e| {
                matches!(
                    &e.event,
                    Event::ContentSnapshot {
                        stream: StreamKind::Assistant,
                        ..
                    }
                )
            })
            .expect("assistant snapshot");
        let item = snapshot.item.clone().expect("snapshot item");
        assert_eq!(
            out.iter()
                .filter(|e| e.item.as_deref() == Some(item.as_str())
                    && matches!(e.event, Event::ItemStarted { .. }))
                .count(),
            1
        );
        // Init is a turn start per turn.
        assert!(
            evs.iter()
                .filter(|e| matches!(e, Event::TurnStarted { .. }))
                .count()
                >= 1,
        );
        // Frames are kept raw (on the first envelope each produced).
        assert!(out.iter().any(|e| e.raw.is_some()));
        // Usage is normalised: input includes the cache.
        let Some(Event::TurnCompleted {
            usage: Some(u),
            cost_usd,
            ..
        }) = evs
            .iter()
            .rev()
            .find_map(|e| matches!(e, Event::TurnCompleted { .. }).then_some((*e).clone()))
        else {
            panic!("no TurnCompleted with usage");
        };
        assert_eq!(u.input_tokens, 26 + 57_925 + 10_633);
        assert_eq!(u.cached_input_tokens, 57_925);
        assert!(cost_usd.is_some());
    }

    #[test]
    fn controls_fixture_sequence() {
        let (_, out) = replay(CONTROLS);
        let results: Vec<&str> = out
            .iter()
            .filter(|e| matches!(e.event, Event::ControlResult { .. }))
            .filter_map(|e| e.request.as_deref())
            .collect();
        for id in [
            "init-1", "mcp-1", "set-1", "models-1", "files-1", "ctx-1", "model-1",
        ] {
            assert!(
                results.contains(&id),
                "missing ControlResult for {id}: {results:?}"
            );
        }
        assert!(out
            .iter()
            .all(|e| !matches!(e.event, Event::ControlResult { error: Some(_), .. })));

        // /cost: a synthetic assistant message becomes one Notice (the replay is deduped).
        let cost_notices = out
            .iter()
            .filter(
                |e| matches!(&e.event, Event::Notice { text } if text.contains("Current session")),
            )
            .count();
        assert_eq!(cost_notices, 1);

        assert!(out.iter().any(|e| matches!(
            e.event,
            Event::Compacted {
                manual: true,
                before: 21_478,
                after: Some(2_082)
            }
        )));
        // The synthetic summary and the "Compacted" stdout are notices too.
        assert!(out
            .iter()
            .any(|e| matches!(&e.event, Event::Notice { text } if text == "Compacted")));

        // set_model: the next init reports the new model.
        let changes: Vec<&str> = out
            .iter()
            .filter_map(|e| match &e.event {
                Event::ModelChanged { model } => Some(model.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(changes, ["claude-sonnet-5-5"]);
        assert!(out.iter().any(|e| matches!(
            &e.event,
            Event::Notice { text } if text.starts_with("Set model to")
        )));

        // The context gauge comes from get_context_usage only when the request was ours, so a
        // bare replay (no encode) must not invent one.
        assert!(!out
            .iter()
            .any(|e| matches!(e.event, Event::UsageUpdated { .. })));
        // Local commands close their turn.
        assert!(
            out.iter()
                .filter(|e| matches!(
                    e.event,
                    Event::TurnCompleted {
                        state: TurnState::Completed,
                        ..
                    }
                ))
                .count()
                >= 4
        );
    }

    #[test]
    fn context_usage_reply_updates_the_gauge() {
        let mut a = ClaudeAdapter::new();
        written(
            &mut a,
            Command::Control {
                id: "ctx-1".into(),
                control: Control::ContextUsage,
            },
        );
        let mut out = Vec::new();
        for line in CONTROLS.lines() {
            let rec: Value = serde_json::from_str(line).expect("json");
            if str_of(&rec, "dir") == Some("out")
                && rec["frame"]["response"]["request_id"] == "ctx-1"
            {
                out.extend(a.feed(&rec["frame"].to_string()));
            }
        }
        assert!(out.iter().any(|e| matches!(
            e.event,
            Event::UsageUpdated {
                used: 17_859,
                max: Some(200_000),
                auto_compact_at: Some(167_000)
            }
        )));
    }

    #[test]
    fn commands_come_from_commands_changed_with_terminal_entries_marked() {
        let (_, out) = replay(APPROVAL);
        let last = out
            .iter()
            .rev()
            .find_map(|e| match &e.event {
                Event::CommandsChanged { commands } => Some(commands),
                _ => None,
            })
            .expect("CommandsChanged");
        let find = |n: &str| last.iter().find(|c| c.name == n);
        // Rich entries (description + hint) win over init's bare names.
        let design = find("design").expect("design");
        assert!(design.description.is_some());
        assert_eq!(design.argument_hint.as_deref(), Some("consent | revoke"));
        // Terminal-only commands are present but tagged for hiding.
        for n in ["doctor", "color", "focus", "reload-plugins"] {
            assert_eq!(find(n).expect(n).kind, AgentCommandKind::TerminalOnly);
        }
        // Skills are tagged as skills.
        assert!(last.iter().any(|c| c.kind == AgentCommandKind::Skill));
        assert!(last.iter().any(|c| c.kind == AgentCommandKind::Command));
    }

    #[test]
    fn interrupt_fixture_ends_interrupted() {
        let (_, out) = replay(INTERRUPT);
        let last = out
            .iter()
            .rev()
            .find_map(|e| match &e.event {
                Event::TurnCompleted { state, error, .. } => Some((*state, error.clone())),
                _ => None,
            })
            .expect("TurnCompleted");
        assert_eq!(last, (TurnState::Interrupted, None));
        // The interrupt's own ack is a control result.
        assert!(out.iter().any(|e| e.request.as_deref() == Some("int-1")
            && matches!(e.event, Event::ControlResult { error: None, .. })));
    }

    #[test]
    fn result_classification() {
        let mut a = ClaudeAdapter::new();
        let turn = |a: &mut ClaudeAdapter, frame: Value| -> (TurnState, Option<String>) {
            match a.feed(&frame.to_string()).remove(0).event {
                Event::TurnCompleted { state, error, .. } => (state, error),
                other => panic!("not a TurnCompleted: {other:?}"),
            }
        };
        // success + is_error is a failure, with the API status explained.
        let (s, e) = turn(
            &mut a,
            json!({"type": "result", "subtype": "success", "is_error": true, "result": "", "api_error_status": 529}),
        );
        assert_eq!(s, TurnState::Failed);
        assert_eq!(e.as_deref(), Some("API error 529"));
        // A pending interrupt (of an open turn) turns an error result into Interrupted.
        a.feed(&json!({"type": "system", "subtype": "init", "model": "m"}).to_string());
        written(&mut a, Command::Interrupt);
        let (s, _) = turn(
            &mut a,
            json!({"type": "result", "subtype": "error_during_execution", "is_error": true}),
        );
        assert_eq!(s, TurnState::Interrupted);
        // ... once; the flag does not leak into the next turn.
        let (s, _) = turn(
            &mut a,
            json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "errors": ["boom"]}),
        );
        assert_eq!(s, TurnState::Failed);
    }

    #[test]
    fn aborted_tools_and_cancel_errors_are_interrupted() {
        let mut a = ClaudeAdapter::new();
        let state = |a: &mut ClaudeAdapter, frame: Value| match a
            .feed(&frame.to_string())
            .remove(0)
            .event
        {
            Event::TurnCompleted { state, .. } => state,
            other => panic!("not a TurnCompleted: {other:?}"),
        };
        assert_eq!(
            state(
                &mut a,
                json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "terminal_reason": "aborted_tools"})
            ),
            TurnState::Interrupted
        );
        assert_eq!(
            state(
                &mut a,
                json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "errors": ["Request was Cancelled by user"]})
            ),
            TurnState::Interrupted
        );
        // A genuine failure stays one, even when its text mentions cancelling.
        for error in [
            "boom",
            "connection cancelled by server",
            "interrupted system call",
        ] {
            assert_eq!(
                state(
                    &mut a,
                    json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "errors": [error]})
                ),
                TurnState::Failed,
                "{error}"
            );
        }
    }

    #[test]
    fn a_tool_result_of_only_non_text_blocks_is_not_empty() {
        let mut a = ClaudeAdapter::new();
        let frame = json!({"type": "user", "uuid": "u1", "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "image", "source": {"type": "base64", "data": "AAAA"}},
                {"type": "tool_reference", "tool_name": "Grep"}
            ]}
        ]}});
        let out = a.feed(&frame.to_string());
        match &out[0].event {
            Event::ItemCompleted { output, .. } => {
                assert_eq!(output.as_deref(), Some("[image]\n[tool_reference]"));
            }
            other => panic!("not an ItemCompleted: {other:?}"),
        }
    }

    #[test]
    fn a_ping_stream_event_is_not_unknown() {
        let mut a = ClaudeAdapter::new();
        let out = a.feed(
            &json!({"type": "stream_event", "parent_tool_use_id": null, "event": {"type": "ping"}})
                .to_string(),
        );
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn bad_and_unknown_lines_become_unknown_with_raw() {
        let mut a = ClaudeAdapter::new();
        let out = a.feed("not json {");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].event, Event::Unknown);
        assert_eq!(out[0].raw, Some(json!("not json {")));
        let out = a.feed(r#"{"type":"brand_new_frame","x":1}"#);
        assert_eq!(out[0].event, Event::Unknown);
        assert_eq!(out[0].raw, Some(json!({"type": "brand_new_frame", "x": 1})));
        assert!(a.feed("   ").is_empty());
    }

    #[test]
    fn tool_kinds() {
        assert_eq!(tool_kind("Bash"), ItemKind::Command);
        for n in ["Edit", "Write", "MultiEdit", "NotebookEdit"] {
            assert_eq!(tool_kind(n), ItemKind::FileChange);
        }
        for n in ["Read", "Glob", "Grep"] {
            assert_eq!(tool_kind(n), ItemKind::FileRead);
        }
        assert_eq!(tool_kind("WebFetch"), ItemKind::WebSearch);
        assert_eq!(tool_kind("Task"), ItemKind::Subagent);
        assert_eq!(tool_kind("Agent"), ItemKind::Subagent);
        assert_eq!(tool_kind("mcp__x__y"), ItemKind::McpTool);
        assert_eq!(tool_kind("TodoWrite"), ItemKind::Tool);
    }

    #[test]
    fn snapshot_without_stream_opens_and_closes_its_item_once() {
        let mut a = ClaudeAdapter::new();
        let frame = json!({
            "type": "assistant", "uuid": "u1", "parent_tool_use_id": "toolu_parent",
            "message": {"id": "msg_1", "model": "m", "content": [
                {"type": "text", "text": "hi"},
                {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}},
            ]},
        })
        .to_string();
        let out = a.feed(&frame);
        let kinds: Vec<&str> = out
            .iter()
            .map(|e| match &e.event {
                Event::ItemStarted { .. } => "start",
                Event::ContentSnapshot { .. } => "snap",
                Event::ItemCompleted { .. } => "done",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["start", "snap", "done", "start"]);
        assert!(matches!(
            &out[3].event,
            Event::ItemStarted { kind: ItemKind::Command, parent: Some(p), .. } if p == "toolu_parent"
        ));
        // A replay of the same frame is ignored.
        assert!(a.feed(&frame).is_empty());
    }

    #[test]
    fn ask_user_question_becomes_a_question() {
        let mut a = ClaudeAdapter::new();
        let frame = json!({
            "type": "control_request", "request_id": "r1",
            "request": {"subtype": "can_use_tool", "tool_name": "AskUserQuestion", "tool_use_id": "t1",
                "input": {"questions": [{"question": "Which?", "header": "Pick", "multiSelect": true,
                    "options": [{"label": "A", "description": "first"}, {"label": "B"}]}]}},
        })
        .to_string();
        let out = a.feed(&frame);
        let Event::QuestionRequested { questions } = &out[0].event else {
            panic!("expected a question, got {:?}", out[0].event);
        };
        assert_eq!(questions.len(), 1);
        assert!(questions[0].multi_select);
        assert_eq!(
            questions[0].options[0].description.as_deref(),
            Some("first")
        );
        assert_eq!(out[0].request.as_deref(), Some("r1"));

        let sent = written(
            &mut a,
            Command::Answer {
                request: "r1".into(),
                answers: json!({"Which?": "A"}),
            },
        );
        assert_eq!(sent["response"]["response"]["behavior"], "allow");
        assert_eq!(
            sent["response"]["response"]["updatedInput"]["answers"],
            json!({"Which?": "A"})
        );
        assert!(sent["response"]["response"]["updatedInput"]["questions"].is_array());
    }

    #[test]
    fn cancel_request_resolves_the_approval() {
        let mut a = ClaudeAdapter::new();
        a.feed(
            &json!({"type": "control_request", "request_id": "r1",
                "request": {"subtype": "can_use_tool", "tool_name": "Bash", "tool_use_id": "t1", "input": {}}})
            .to_string(),
        );
        let out =
            a.feed(&json!({"type": "control_cancel_request", "request_id": "r1"}).to_string());
        assert!(matches!(
            out[0].event,
            Event::ApprovalResolved {
                decision: Decision::Cancel
            }
        ));
        assert!(a
            .encode(Command::Approve {
                request: "r1".into(),
                decision: Decision::Allow,
                updated_input: None,
                message: None,
            })
            .is_err());
    }

    #[test]
    fn on_exit_closes_everything_open() {
        let mut a = ClaudeAdapter::new();
        a.feed(
            &json!({"type": "system", "subtype": "init", "model": "m", "session_id": "s"})
                .to_string(),
        );
        a.feed(
            &json!({"type": "stream_event", "parent_tool_use_id": null, "event": {"type": "message_start", "message": {"id": "m1"}}})
                .to_string(),
        );
        a.feed(
            &json!({"type": "stream_event", "parent_tool_use_id": null, "event": {"type": "content_block_start", "index": 0,
                "content_block": {"type": "tool_use", "id": "t1", "name": "Bash", "input": {}}}})
            .to_string(),
        );
        a.feed(
            &json!({"type": "control_request", "request_id": "r1",
                "request": {"subtype": "can_use_tool", "tool_name": "Bash", "tool_use_id": "t1", "input": {}}})
            .to_string(),
        );
        written(&mut a, Command::Interrupt);
        let out = a.on_exit(Some(130));
        let evs = events(&out);
        assert!(matches!(
            evs[0],
            Event::ItemCompleted {
                status: ItemStatus::Interrupted,
                ..
            }
        ));
        assert!(evs.iter().any(|e| matches!(
            e,
            Event::ApprovalResolved {
                decision: Decision::Cancel
            }
        )));
        assert!(evs.iter().any(|e| matches!(
            e,
            Event::TurnCompleted {
                state: TurnState::Interrupted,
                ..
            }
        )));
        assert!(matches!(
            evs.last(),
            Some(Event::SessionExited {
                code: Some(130),
                expected: true
            })
        ));
        // An exit nobody asked for is not expected.
        let mut b = ClaudeAdapter::new();
        assert!(matches!(
            b.on_exit(Some(1)).last().map(|e| &e.event),
            Some(Event::SessionExited {
                code: Some(1),
                expected: false
            })
        ));
    }

    fn init_frame() -> String {
        json!({"type": "system", "subtype": "init", "model": "m", "session_id": "s"}).to_string()
    }

    fn open_tool_frame() -> String {
        json!({"type": "stream_event", "parent_tool_use_id": null, "event": {"type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "t1", "name": "Bash", "input": {}}}})
        .to_string()
    }

    #[test]
    fn result_closes_items_still_open() {
        // Failed turn: the dangling tool fails with a reason.
        let mut a = ClaudeAdapter::new();
        a.feed(&init_frame());
        a.feed(&open_tool_frame());
        let out = a.feed(&json!({"type": "result", "subtype": "success"}).to_string());
        assert!(matches!(
            &out[0].event,
            Event::ItemCompleted { status: ItemStatus::Failed, error: Some(e), .. }
                if e == "no result for this item"
        ));
        assert_eq!(out[0].item.as_deref(), Some("t1"));
        assert!(matches!(out[1].event, Event::TurnCompleted { .. }));
        // An exit afterwards has nothing left to close.
        assert!(!a
            .on_exit(Some(0))
            .iter()
            .any(|e| matches!(e.event, Event::ItemCompleted { .. })));

        // Interrupted turn: the item is Interrupted.
        let mut a = ClaudeAdapter::new();
        a.feed(&init_frame());
        a.feed(&open_tool_frame());
        written(&mut a, Command::Interrupt);
        let out = a.feed(
            &json!({"type": "result", "subtype": "error_during_execution", "is_error": true})
                .to_string(),
        );
        assert!(matches!(
            out[0].event,
            Event::ItemCompleted {
                status: ItemStatus::Interrupted,
                error: None,
                ..
            }
        ));
        assert!(matches!(
            out[1].event,
            Event::TurnCompleted {
                state: TurnState::Interrupted,
                ..
            }
        ));
    }

    // ----- sub-agents (shapes from claude 2.1.29x stream-json captures, ids shortened) -----

    fn agent_call(id: &str, background: bool) -> String {
        json!({"type": "assistant", "uuid": format!("u-{id}"), "parent_tool_use_id": null,
            "message": {"id": format!("m-{id}"), "role": "assistant", "content": [
                {"type": "tool_use", "id": id, "name": "Agent", "input": {
                    "description": "Test agent", "prompt": "Reply with the single word OK.",
                    "subagent_type": "general-purpose", "run_in_background": background}}]}})
        .to_string()
    }

    fn task_started(task: &str, tool: &str, background: bool) -> String {
        json!({"type": "system", "subtype": "task_started", "task_id": task, "run_id": "r1",
            "tool_use_id": tool, "description": "Test agent", "subagent_type": "general-purpose",
            "is_backgrounded": background, "spawn_depth": 1, "task_type": "local_agent"})
        .to_string()
    }

    fn tasks_changed(tasks: &[&str]) -> String {
        let tasks: Vec<Value> = tasks
            .iter()
            .map(|t| {
                json!({"task_id": t, "run_id": "r1", "task_type": "local_agent",
                            "description": "Test agent"})
            })
            .collect();
        json!({"type": "system", "subtype": "background_tasks_changed", "tasks": tasks}).to_string()
    }

    fn async_launched(tool: &str) -> String {
        json!({"type": "user", "uuid": format!("r-{tool}"), "parent_tool_use_id": null,
            "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tool,
                "content": [{"type": "text", "text": "Async agent launched successfully. (This tool result is internal metadata.)"}]}]},
            "tool_use_result": {"isAsync": true, "status": "async_launched", "agentId": "ag1",
                "description": "Test agent"}})
        .to_string()
    }

    fn text_frame(uuid: &str, parent: Option<&str>, text: &str) -> String {
        json!({"type": "assistant", "uuid": uuid, "parent_tool_use_id": parent,
            "message": {"id": format!("m-{uuid}"), "role": "assistant",
                "content": [{"type": "text", "text": text}]}})
        .to_string()
    }

    fn task_notification(task: &str, tool: &str, status: &str, summary: &str) -> String {
        json!({"type": "system", "subtype": "task_notification", "task_id": task, "run_id": "r1",
            "tool_use_id": tool, "status": status, "output_file": "/tmp/t/tasks/x.output",
            "summary": summary})
        .to_string()
    }

    fn task_updated(task: &str) -> String {
        json!({"type": "system", "subtype": "task_updated", "task_id": task, "run_id": "r1",
            "patch": {"status": "completed", "end_time": 1}})
        .to_string()
    }

    fn completions<'a>(out: &'a [Envelope], id: &str) -> Vec<&'a Event> {
        out.iter()
            .filter(|e| {
                e.item.as_deref() == Some(id) && matches!(e.event, Event::ItemCompleted { .. })
            })
            .map(|e| &e.event)
            .collect()
    }

    fn count(out: &[Envelope], f: impl Fn(&Event) -> bool) -> usize {
        out.iter().filter(|e| f(&e.event)).count()
    }

    fn task_lists(out: &[Envelope]) -> Vec<Vec<BackgroundTask>> {
        out.iter()
            .filter_map(|e| match &e.event {
                Event::BackgroundTasks { tasks } => Some(tasks.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_background_sub_agent_runs_past_its_launch_result_and_the_turn_end() {
        let mut a = ClaudeAdapter::new();
        let mut out = Vec::new();
        let mut feed = |a: &mut ClaudeAdapter, line: String| {
            let envs = a.feed(&line);
            out.extend(envs);
            out.clone()
        };
        feed(&mut a, init_frame());
        feed(&mut a, agent_call("toolu_bg", true));
        feed(&mut a, tasks_changed(&["task1"]));
        feed(&mut a, task_started("task1", "toolu_bg", true));
        // The immediate "launched" result does not complete it.
        let so_far = feed(&mut a, async_launched("toolu_bg"));
        assert!(completions(&so_far, "toolu_bg").is_empty(), "{so_far:?}");
        feed(
            &mut a,
            text_frame("main1", None, "Agent launched in the background."),
        );
        // The sub-agent's own tool call nests under it.
        let child = json!({"type": "assistant", "uuid": "sub1", "parent_tool_use_id": "toolu_bg",
            "message": {"id": "m-sub1", "role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_child", "name": "Read", "input": {"file_path": "/x"}}]}});
        let so_far = feed(&mut a, child.to_string());
        assert!(so_far.iter().any(|e| e.item.as_deref() == Some("toolu_child")
            && matches!(&e.event, Event::ItemStarted { parent: Some(p), .. } if p == "toolu_bg")));
        // The parent turn ends: neither the sub-agent nor its open child is settled.
        let so_far = feed(
            &mut a,
            json!({"type": "result", "subtype": "success"}).to_string(),
        );
        assert!(completions(&so_far, "toolu_bg").is_empty(), "{so_far:?}");
        assert!(completions(&so_far, "toolu_child").is_empty(), "{so_far:?}");
        let child_result = json!({"type": "user", "uuid": "sub2", "parent_tool_use_id": "toolu_bg",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_child", "content": "x"}]}});
        feed(&mut a, child_result.to_string());
        feed(&mut a, text_frame("sub3", Some("toolu_bg"), "OK"));
        feed(&mut a, task_updated("task1"));
        let so_far = feed(
            &mut a,
            task_notification("task1", "toolu_bg", "completed", "OK"),
        );
        assert!(matches!(
            completions(&so_far, "toolu_bg").as_slice(),
            [Event::ItemCompleted { status: ItemStatus::Completed, output: Some(o), error: None }]
                if o == "OK"
        ));
        feed(&mut a, tasks_changed(&[]));
        // Claude then runs a turn of its own about the notification.
        feed(&mut a, init_frame());
        feed(&mut a, text_frame("main2", None, "DONE"));
        let out = feed(
            &mut a,
            json!({"type": "result", "subtype": "success",
                   "origin": {"kind": "task-notification", "producer": "session-task"}})
            .to_string(),
        );

        assert_eq!(count(&out, |e| *e == Event::Unknown), 0, "{out:?}");
        assert_eq!(
            count(&out, |e| matches!(e, Event::SessionStarted { .. })),
            1
        );
        assert_eq!(count(&out, |e| matches!(e, Event::TurnStarted { .. })), 2);
        assert_eq!(count(&out, |e| matches!(e, Event::TurnCompleted { .. })), 2);
        assert_eq!(completions(&out, "toolu_bg").len(), 1);
        assert!(matches!(
            completions(&out, "toolu_child").as_slice(),
            [Event::ItemCompleted {
                status: ItemStatus::Completed,
                ..
            }]
        ));
        // The task list: shown, then given its tool call by task_started, then empty.
        let lists = task_lists(&out);
        assert_eq!(lists.len(), 3, "{lists:?}");
        assert_eq!(lists[0].len(), 1);
        assert_eq!(lists[0][0].id, "task1");
        assert_eq!(lists[0][0].kind, BackgroundTaskKind::Agent);
        assert_eq!(lists[0][0].description.as_deref(), Some("Test agent"));
        assert_eq!(lists[1][0].tool_use_id.as_deref(), Some("toolu_bg"));
        assert!(lists[2].is_empty());
    }

    #[test]
    fn print_mode_flushing_both_results_at_the_end_ends_one_turn() {
        // The recorded `-p` order: the second turn's init arrives before the first result, and
        // both results come last.
        let mut a = ClaudeAdapter::new();
        let mut out = Vec::new();
        for line in [
            init_frame(),
            agent_call("toolu_bg", true),
            tasks_changed(&["task1"]),
            task_started("task1", "toolu_bg", true),
            async_launched("toolu_bg"),
            text_frame("sub1", Some("toolu_bg"), "OK"),
            task_notification("task1", "toolu_bg", "completed", "OK"),
            tasks_changed(&[]),
            init_frame(),
            text_frame("main2", None, "DONE"),
            json!({"type": "result", "subtype": "success"}).to_string(),
            json!({"type": "result", "subtype": "success",
                   "origin": {"kind": "task-notification"}})
            .to_string(),
        ] {
            out.extend(a.feed(&line));
        }
        assert_eq!(count(&out, |e| *e == Event::Unknown), 0);
        assert_eq!(
            count(&out, |e| matches!(e, Event::SessionStarted { .. })),
            1
        );
        assert_eq!(count(&out, |e| matches!(e, Event::TurnCompleted { .. })), 1);
        assert!(matches!(
            out.last().map(|e| &e.event),
            Some(Event::TurnCompleted {
                state: TurnState::Completed,
                ..
            })
        ));
        assert!(matches!(
            completions(&out, "toolu_bg").as_slice(),
            [Event::ItemCompleted { status: ItemStatus::Completed, output: Some(o), .. }] if o == "OK"
        ));
    }

    #[test]
    fn a_foreground_sub_agent_completes_on_its_tool_result_with_no_unknowns() {
        let mut a = ClaudeAdapter::new();
        let mut out = Vec::new();
        let prompt = json!({"type": "user", "uuid": "p1", "parent_tool_use_id": "toolu_fg",
            "agent_id": "ag1", "subagent_type": "general-purpose", "task_description": "Test agent",
            "message": {"role": "user", "content": [{"type": "text", "text": "Reply with the single word OK."}]}});
        let result = json!({"type": "user", "uuid": "r1", "parent_tool_use_id": null,
            "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_fg",
                "content": [{"type": "text", "text": "[Subagent hand-back] OK"}]}]},
            "tool_use_result": {"status": "completed", "agentId": "ag1"}});
        let mut notified_at = 0;
        for line in [
            init_frame(),
            agent_call("toolu_fg", false),
            task_started("task1", "toolu_fg", false),
            prompt.to_string(),
            task_updated("task1"),
            task_notification("task1", "toolu_fg", "completed", "OK"),
            result.to_string(),
            json!({"type": "result", "subtype": "success"}).to_string(),
        ] {
            if line.contains("task_notification") {
                notified_at = out.len();
            }
            out.extend(a.feed(&line));
        }
        assert_eq!(count(&out, |e| *e == Event::Unknown), 0, "{out:?}");
        assert!(task_lists(&out).is_empty());
        let done: Vec<usize> = out
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                e.item.as_deref() == Some("toolu_fg")
                    && matches!(e.event, Event::ItemCompleted { .. })
            })
            .map(|(i, _)| i)
            .collect();
        assert_eq!(done.len(), 1);
        // The notification itself adds nothing; the tool result after it completes the card.
        assert!(done[0] >= notified_at, "completed by its tool result");
        assert!(matches!(
            &out[done[0]].event,
            Event::ItemCompleted { status: ItemStatus::Completed, output: Some(o), .. }
                if o.contains("OK")
        ));
    }

    #[test]
    fn background_task_statuses_and_exit() {
        assert_eq!(task_status("completed"), ItemStatus::Completed);
        assert_eq!(task_status("failed"), ItemStatus::Failed);
        assert_eq!(task_status("killed"), ItemStatus::Interrupted);
        assert_eq!(task_status("stopped"), ItemStatus::Interrupted);

        let mut a = ClaudeAdapter::new();
        a.feed(&init_frame());
        a.feed(&agent_call("toolu_bg", true));
        a.feed(&task_started("task1", "toolu_bg", true));
        a.feed(&async_launched("toolu_bg"));
        let out = a.feed(&task_notification("task1", "toolu_bg", "failed", "boom"));
        assert!(matches!(
            out.as_slice(),
            [e] if matches!(&e.event, Event::ItemCompleted { status: ItemStatus::Failed, error: Some(m), .. } if m == "boom")
        ));

        // Still running when the process exits: interrupted, and the task list empties.
        let mut a = ClaudeAdapter::new();
        a.feed(&init_frame());
        a.feed(&agent_call("toolu_bg", true));
        a.feed(&tasks_changed(&["task1"]));
        a.feed(&task_started("task1", "toolu_bg", true));
        a.feed(&async_launched("toolu_bg"));
        a.feed(&json!({"type": "result", "subtype": "success"}).to_string());
        let out = a.on_exit(Some(0));
        assert!(matches!(
            completions(&out, "toolu_bg").as_slice(),
            [Event::ItemCompleted {
                status: ItemStatus::Interrupted,
                ..
            }]
        ));
        assert_eq!(task_lists(&out), vec![Vec::new()]);
    }

    #[test]
    fn the_launch_result_alone_keeps_a_sub_agent_open() {
        // No task_started before the result: the structured result metadata is enough.
        let mut a = ClaudeAdapter::new();
        a.feed(&init_frame());
        a.feed(&agent_call("toolu_bg", true));
        assert!(a.feed(&async_launched("toolu_bg")).is_empty());
        let out = a.feed(&json!({"type": "result", "subtype": "success"}).to_string());
        assert!(completions(&out, "toolu_bg").is_empty());
        let out = a.feed(&task_notification("task1", "toolu_bg", "completed", "OK"));
        assert_eq!(completions(&out, "toolu_bg").len(), 1);
    }

    #[test]
    fn interrupt_without_an_open_turn_is_not_remembered() {
        let mut a = ClaudeAdapter::new();
        written(&mut a, Command::Interrupt);
        a.feed(&init_frame());
        let out = a.on_exit(Some(1));
        assert!(matches!(
            out.last().map(|e| &e.event),
            Some(Event::SessionExited {
                expected: false,
                ..
            })
        ));
    }

    #[test]
    fn raw_is_attached_to_the_first_envelope_only() {
        let mut a = ClaudeAdapter::new();
        // init yields SessionStarted, TurnStarted and CommandsChanged.
        let out = a.feed(&init_frame());
        assert!(out.len() > 1);
        assert!(out[0].raw.is_some());
        assert!(out[1..].iter().all(|e| e.raw.is_none()));
    }

    #[test]
    fn stderr_only_reports_errors() {
        let mut a = ClaudeAdapter::new();
        assert!(a.feed_stderr("starting up").is_empty());
        let out = a.feed_stderr("Error: boom");
        assert!(matches!(&out[0].event, Event::Error { message } if message == "Error: boom"));
        assert_eq!(a.feed_side("x", "out", true)[0].event, Event::Unknown);
    }

    // ----- encode: exact JSON -----

    #[test]
    fn argv_flags() {
        let a = ClaudeAdapter::new();
        let mut s = OpenSession {
            program: "/usr/bin/claude".into(),
            extra_args: vec!["--foo".into()],
            cwd: "/w".into(),
            model: Some("sonnet".into()),
            effort: None,
            mode: Mode::AcceptEdits,
            resume: Some("abc".into()),
            new_session_id: Some("zzz".into()),
            approval_hook: false,
        };
        assert_eq!(
            a.argv(&s),
            [
                "/usr/bin/claude",
                "--foo",
                "--output-format",
                "stream-json",
                "--verbose",
                "--input-format",
                "stream-json",
                "--permission-prompt-tool",
                "stdio",
                "--include-partial-messages",
                "--model",
                "sonnet",
                "--permission-mode",
                "acceptEdits",
                "--resume=abc",
            ]
        );
        s.resume = None;
        s.model = None;
        s.mode = Mode::Plan;
        let argv = a.argv(&s);
        assert!(argv.ends_with(&["plan".to_owned(), "--session-id=zzz".to_owned()]));
        assert!(!argv.iter().any(|x| x == "--model"));
        s.mode = Mode::Ask;
        s.new_session_id = None;
        let argv = a.argv(&s);
        assert!(argv.ends_with(&["--permission-mode".to_owned(), "default".to_owned()]));
    }

    #[test]
    fn handshake_is_initialize() {
        let mut a = ClaudeAdapter::new();
        let lines = a.handshake();
        assert_eq!(lines.len(), 1);
        let v: Value = serde_json::from_str(&lines[0]).expect("json");
        assert_eq!(
            v,
            json!({"type": "control_request", "request_id": "init-1", "request": {"subtype": "initialize"}})
        );
    }

    #[test]
    fn encode_prompt_and_interrupt() {
        let mut a = ClaudeAdapter::new();
        assert_eq!(
            written(
                &mut a,
                Command::Prompt {
                    text: "hello".into()
                }
            ),
            json!({"type": "user", "message": {"role": "user", "content": "hello"}, "parent_tool_use_id": null})
        );
        assert_eq!(
            written(&mut a, Command::Interrupt),
            json!({"type": "control_request", "request_id": "int-1", "request": {"subtype": "interrupt"}})
        );
    }

    fn pending_edit(a: &mut ClaudeAdapter) {
        a.feed(
            &json!({"type": "control_request", "request_id": "r1", "request": {
                "subtype": "can_use_tool", "tool_name": "Edit", "tool_use_id": "tu1",
                "input": {"file_path": "/x"},
                "permission_suggestions": [{"type": "setMode", "mode": "acceptEdits", "destination": "session"}]}})
            .to_string(),
        );
    }

    fn pending_bash(a: &mut ClaudeAdapter) -> Vec<Envelope> {
        a.feed(
            &json!({"type": "control_request", "request_id": "r2", "request": {
                "subtype": "can_use_tool", "tool_name": "Bash", "tool_use_id": "tu2",
                "input": {"command": "git status"},
                "permission_suggestions": [
                    {"type": "addRules", "behavior": "allow", "destination": "localSettings",
                     "rules": [{"toolName": "Bash", "ruleContent": "git status:*"}]},
                    {"type": "setMode", "mode": "acceptEdits", "destination": "localSettings"}]}})
            .to_string(),
        )
    }

    fn approve(decision: Decision, request: &str) -> Command {
        Command::Approve {
            request: request.into(),
            decision,
            updated_input: None,
            message: None,
        }
    }

    #[test]
    fn always_allow_writes_only_rules_to_local_settings_and_session_stays_in_memory() {
        let mut a = ClaudeAdapter::new();
        // Offered only when Claude suggested a rule.
        let options = |out: &[Envelope]| match &out[0].event {
            Event::ApprovalRequested { options, .. } => options.clone(),
            other => panic!("{other:?}"),
        };
        let bash = pending_bash(&mut a);
        assert!(options(&bash).contains(&Decision::AllowAlways));
        // The card is told exactly what would be saved (a prefix rule, broader than the call).
        assert!(
            matches!(&bash[0].event, Event::ApprovalRequested { remembers: Some(r), .. }
            if r == "Bash(git status:*) in this project")
        );
        // A rule without an explicit "allow" is never one to save.
        let vague = a.feed(
            &json!({"type": "control_request", "request_id": "r4", "request": {
                "subtype": "can_use_tool", "tool_name": "Bash", "tool_use_id": "tu4",
                "input": {"command": "ls"},
                "permission_suggestions": [{"type": "addRules", "destination": "localSettings",
                    "rules": [{"toolName": "Bash", "ruleContent": "ls:*"}]}]}})
            .to_string(),
        );
        assert!(!options(&vague).contains(&Decision::AllowAlways));
        let edit = a.feed(
            &json!({"type": "control_request", "request_id": "r3", "request": {
                "subtype": "can_use_tool", "tool_name": "Edit", "tool_use_id": "tu3",
                "input": {"file_path": "/x"},
                "permission_suggestions": [{"type": "setMode", "mode": "acceptEdits", "destination": "session"}]}})
            .to_string(),
        );
        assert!(
            !options(&edit).contains(&Decision::AllowAlways),
            "a mode is not a rule"
        );
        // Refused where not offered, and the request stays answerable.
        assert!(matches!(
            a.encode(approve(Decision::AllowAlways, "r3")),
            Err(AdapterError::Invalid(_))
        ));
        assert!(a.encode(approve(Decision::Allow, "r3")).is_ok());

        let line = written(&mut a, approve(Decision::AllowAlways, "r2"));
        assert_eq!(
            line["response"]["response"]["updatedPermissions"],
            json!([{"type": "addRules", "behavior": "allow", "destination": "localSettings",
                    "rules": [{"toolName": "Bash", "ruleContent": "git status:*"}]}]),
            "only the rule, never the mode change"
        );

        // "For session" never persists, whatever Claude suggested.
        pending_bash(&mut a);
        let line = written(&mut a, approve(Decision::AllowForSession, "r2"));
        let scoped = line["response"]["response"]["updatedPermissions"]
            .as_array()
            .expect("permissions")
            .clone();
        assert_eq!(scoped.len(), 2);
        assert!(
            scoped.iter().all(|s| s["destination"] == "session"),
            "{scoped:?}"
        );
    }

    #[test]
    fn encode_approve_allow_deny_cancel_session() {
        let mut a = ClaudeAdapter::new();
        pending_edit(&mut a);
        assert_eq!(
            written(
                &mut a,
                Command::Approve {
                    request: "r1".into(),
                    decision: Decision::Allow,
                    updated_input: None,
                    message: None,
                }
            ),
            json!({"type": "control_response", "response": {"subtype": "success", "request_id": "r1",
                "response": {"behavior": "allow", "updatedInput": {"file_path": "/x"}, "toolUseID": "tu1"}}})
        );
        pending_edit(&mut a);
        assert_eq!(
            written(
                &mut a,
                Command::Approve {
                    request: "r1".into(),
                    decision: Decision::AllowForSession,
                    updated_input: Some(json!({"file_path": "/y"})),
                    message: None,
                }
            ),
            json!({"type": "control_response", "response": {"subtype": "success", "request_id": "r1",
                "response": {"behavior": "allow", "updatedInput": {"file_path": "/y"}, "toolUseID": "tu1",
                    "updatedPermissions": [{"type": "setMode", "mode": "acceptEdits", "destination": "session"}]}}})
        );
        pending_edit(&mut a);
        assert_eq!(
            written(
                &mut a,
                Command::Approve {
                    request: "r1".into(),
                    decision: Decision::Deny,
                    updated_input: None,
                    message: Some("no".into()),
                }
            ),
            json!({"type": "control_response", "response": {"subtype": "success", "request_id": "r1",
                "response": {"behavior": "deny", "message": "no", "toolUseID": "tu1"}}})
        );
        pending_edit(&mut a);
        assert_eq!(
            written(
                &mut a,
                Command::Approve {
                    request: "r1".into(),
                    decision: Decision::Cancel,
                    updated_input: None,
                    message: None,
                }
            ),
            json!({"type": "control_response", "response": {"subtype": "success", "request_id": "r1",
                "response": {"behavior": "deny", "message": "The user denied this request",
                    "interrupt": true, "toolUseID": "tu1"}}})
        );
        // Answered requests are gone.
        assert!(matches!(
            a.encode(Command::Approve {
                request: "r1".into(),
                decision: Decision::Allow,
                updated_input: None,
                message: None,
            }),
            Err(AdapterError::Invalid(_))
        ));
    }

    #[test]
    fn effort_is_a_flag_and_a_changed_effort_respawns() {
        let mut a = ClaudeAdapter::new();
        let session = OpenSession {
            program: "claude".into(),
            extra_args: vec![],
            cwd: "/w".into(),
            model: Some("sonnet".into()),
            effort: Some("medium".into()),
            mode: Mode::Ask,
            resume: None,
            new_session_id: None,
            approval_hook: false,
        };
        let argv = a.argv(&session);
        assert!(argv.windows(2).any(|w| w == ["--effort", "medium"]));
        // Same effort, or none given: a live set_model.
        for effort in [Some("medium".to_owned()), None] {
            let actions = a
                .encode(Command::SetModel {
                    model: "opus".into(),
                    effort,
                })
                .expect("encode");
            assert!(
                matches!(actions.as_slice(), [Action::Write(_)]),
                "{actions:?}"
            );
        }
        // A different effort restarts the process on the same session.
        assert_eq!(
            a.encode(Command::SetModel {
                model: "opus".into(),
                effort: Some("high".into()),
            })
            .expect("encode"),
            vec![Action::Respawn(OpenSessionDelta {
                model: Some("opus".into()),
                effort: Some("high".into()),
                mode: None,
                resume: None,
            })]
        );
        // A session started without an effort treats any explicit effort as a change.
        let bare = OpenSession {
            effort: None,
            ..session
        };
        let mut fresh = ClaudeAdapter::new();
        assert!(!fresh.argv(&bare).iter().any(|x| x == "--effort"));
        assert!(matches!(
            fresh
                .encode(Command::SetModel {
                    model: "opus".into(),
                    effort: Some("low".into())
                })
                .expect("encode")
                .as_slice(),
            [Action::Respawn(_)]
        ));
    }

    #[test]
    fn restarted_process_reports_session_started_for_changed_or_unchanged_model() {
        for model in ["claude-opus-4-6", "m"] {
            let mut a = ClaudeAdapter::new();
            let mut session = OpenSession {
                program: "claude".into(),
                extra_args: vec![],
                cwd: "/w".into(),
                model: Some("m".into()),
                effort: Some("medium".into()),
                mode: Mode::Ask,
                resume: None,
                new_session_id: None,
                approval_hook: false,
            };
            a.argv(&session);
            a.handshake();
            a.feed(&init_frame());
            assert!(matches!(
                a.encode(Command::SetModel {
                    model: model.into(),
                    effort: Some("high".into())
                })
                .expect("encode")
                .as_slice(),
                [Action::Respawn(_)]
            ));
            a.on_exit(Some(0));
            session.model = Some(model.into());
            session.effort = Some("high".into());
            session.resume = Some("s".into());
            a.argv(&session);
            a.handshake();
            let init =
                json!({"type": "system", "subtype": "init", "model": model, "session_id": "s"})
                    .to_string();
            let out = a.feed(&init);
            assert_eq!(out.iter().filter(|e| matches!(&e.event, Event::SessionStarted { model: Some(reported), .. } if reported == model)).count(), 1, "first init of restarted process must confirm model, including an effort-only restart");
            assert!(out
                .iter()
                .all(|e| !matches!(e.event, Event::ModelChanged { .. })));
            let later = a.feed(&init);
            assert!(later
                .iter()
                .all(|e| !matches!(e.event, Event::SessionStarted { .. })));
            assert!(later
                .iter()
                .any(|e| matches!(e.event, Event::TurnStarted { .. })));
        }
    }

    #[test]
    fn model_control_acknowledges_only_successful_requests() {
        let mut a = ClaudeAdapter::new();
        a.feed(&init_frame());
        let request = written(
            &mut a,
            Command::SetModel {
                model: "sonnet".into(),
                effort: None,
            },
        );
        let id = request["request_id"].as_str().expect("wire request id");
        assert_eq!(request["request"]["model"], "sonnet");
        assert_eq!(
            a.model.as_deref(),
            Some("m"),
            "encode must not adopt the model"
        );
        let requested = a.drain_outbox().events;
        assert!(
            matches!(requested.as_slice(), [e] if e.event == Event::ModelChangeRequested { model: "sonnet".into() } && e.request.as_deref() == Some(id))
        );
        let ack = json!({"type": "control_response", "response": {"subtype": "success", "request_id": id}});
        let out = a.feed(&ack.to_string());
        assert!(out
            .iter()
            .any(|e| matches!(&e.event, Event::ModelChanged { model } if model == "sonnet")));
        assert_eq!(a.model.as_deref(), Some("sonnet"));
        assert!(a
            .feed(&ack.to_string())
            .iter()
            .all(|e| !matches!(e.event, Event::ModelChanged { .. })));

        let request = written(
            &mut a,
            Command::SetModel {
                model: "rejected".into(),
                effort: None,
            },
        );
        let ack = json!({"type": "control_response", "response": {"subtype": "error", "request_id": request["request_id"], "error": "not available"}});
        let out = a.feed(&ack.to_string());
        assert_eq!(a.model.as_deref(), Some("sonnet"));
        assert!(out
            .iter()
            .all(|e| !matches!(e.event, Event::ModelChanged { .. })));
        assert!(out.iter().any(|e| matches!(&e.event, Event::ModelChangeFailed { model, message } if model == "rejected" && message.contains("set_model") && message.contains("not available"))));
        assert!(out.iter().any(|e| matches!(&e.event, Event::ControlResult { error: Some(error), .. } if error == "not available") && e.request.as_deref() == request["request_id"].as_str()));
    }

    #[test]
    fn model_control_late_acknowledgements_cannot_revert_newer_success() {
        let mut a = ClaudeAdapter::new();
        a.feed(&init_frame());
        let first = written(
            &mut a,
            Command::SetModel {
                model: "sonnet".into(),
                effort: None,
            },
        );
        let second = written(
            &mut a,
            Command::SetModel {
                model: "opus".into(),
                effort: None,
            },
        );
        for (request, expected) in [(second, "opus"), (first, "opus")] {
            a.feed(&json!({"type": "control_response", "response": {"subtype": "success", "request_id": request["request_id"]}}).to_string());
            assert_eq!(a.model.as_deref(), Some(expected));
        }
    }

    #[test]
    fn model_control_exit_clears_pending_acknowledgements() {
        let mut a = ClaudeAdapter::new();
        a.feed(&init_frame());
        let request = written(
            &mut a,
            Command::SetModel {
                model: "sonnet".into(),
                effort: None,
            },
        );
        let out = a.on_exit(Some(1));
        assert!(out.iter().any(|e| matches!(&e.event, Event::ModelChangeFailed { model, message } if model == "sonnet" && message.contains("exited"))));
        assert!(a.model_requests.is_empty());
        assert!(a.pending_controls.is_empty());
        assert!(a.drain_outbox().is_empty());
        let late = a.feed(&json!({"type": "control_response", "response": {"subtype": "success", "request_id": request["request_id"]}}).to_string());
        assert!(late
            .iter()
            .all(|e| !matches!(e.event, Event::ModelChanged { .. })));
        assert_eq!(a.model.as_deref(), Some("m"));
    }

    #[test]
    fn model_control_keeps_earlier_success_when_newer_selection_is_rejected() {
        let mut a = ClaudeAdapter::new();
        a.feed(&init_frame());
        let first = written(
            &mut a,
            Command::SetModel {
                model: "sonnet".into(),
                effort: None,
            },
        );
        let second = written(
            &mut a,
            Command::SetModel {
                model: "opus".into(),
                effort: None,
            },
        );
        let out = a.feed(&json!({"type": "control_response", "response": {"subtype": "success", "request_id": first["request_id"]}}).to_string());
        assert!(out
            .iter()
            .any(|e| matches!(&e.event, Event::ModelChanged { model } if model == "sonnet")));
        let out = a.feed(&json!({"type": "control_response", "response": {"subtype": "error", "request_id": second["request_id"], "error": "not available"}}).to_string());
        assert!(out.iter().any(
            |e| matches!(&e.event, Event::ModelChangeFailed { model, .. } if model == "opus")
        ));
        assert_eq!(a.model.as_deref(), Some("sonnet"));
    }

    #[test]
    fn encode_model_mode_and_controls() {
        let mut a = ClaudeAdapter::new();
        assert_eq!(
            written(
                &mut a,
                Command::SetModel {
                    model: "sonnet".into(),
                    effort: None
                }
            ),
            json!({"type": "control_request", "request_id": "model-1", "request": {"subtype": "set_model", "model": "sonnet"}})
        );
        assert_eq!(
            written(&mut a, Command::SetMode { mode: Mode::Plan }),
            json!({"type": "control_request", "request_id": "mode-2", "request": {"subtype": "set_permission_mode", "mode": "plan"}})
        );
        let cases = [
            (Control::McpStatus, json!({"subtype": "mcp_status"})),
            (
                Control::McpToggle {
                    server: "s".into(),
                    enabled: false,
                },
                json!({"subtype": "mcp_toggle", "serverName": "s", "enabled": false}),
            ),
            (
                Control::McpReconnect { server: "s".into() },
                json!({"subtype": "mcp_reconnect", "serverName": "s"}),
            ),
            (Control::GetSettings, json!({"subtype": "get_settings"})),
            (Control::ListModels, json!({"subtype": "list_models"})),
            (
                Control::FileSuggestions {
                    query: "calc".into(),
                },
                json!({"subtype": "file_suggestions", "query": "calc"}),
            ),
            (
                Control::ContextUsage,
                json!({"subtype": "get_context_usage"}),
            ),
            (Control::Usage, json!({"subtype": "get_usage"})),
        ];
        for (control, body) in cases {
            assert_eq!(
                written(
                    &mut a,
                    Command::Control {
                        id: "c9".into(),
                        control
                    }
                ),
                json!({"type": "control_request", "request_id": "c9", "request": body})
            );
        }
    }
}
