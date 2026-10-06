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
//!   process; the UI already knows the decision it sent.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Map, Value};

use crate::adapter::{
    Action, Adapter, AdapterError, Command, Control, Driver, Mode, OpenSession, OpenSessionDelta,
};
use crate::caps::Capabilities;
use crate::event::{
    AgentCommand, AgentCommandKind, Decision, Envelope, Event, ItemKind, ItemStatus, Question,
    QuestionOption, ResponseCapability, StreamKind, TurnState, Usage,
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
    seen: HashSet<String>,
    // Requests in flight.
    pending: HashMap<String, PendingApproval>,
    pending_controls: HashSet<String>,
    context_requests: HashSet<String>,
    /// `get_usage` requests in flight, whose replies also become `QuotaUpdated`.
    usage_requests: HashSet<String>,
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
            seen: HashSet::new(),
            pending: HashMap::new(),
            pending_controls: HashSet::new(),
            context_requests: HashSet::new(),
            usage_requests: HashSet::new(),
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

    fn close_item(&mut self, id: &str) {
        self.open_items.retain(|i| i != id);
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
            // Progress chatter that carries nothing the UI renders.
            "status"
            | "thinking_tokens"
            | "hook_started"
            | "hook_progress"
            | "hook_response"
            | "session_state_changed" => Vec::new(),
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
            "message_delta" | "message_stop" => Vec::new(),
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
        self.open_item(&item);
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
                        self.open_item(id);
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
            for b in blocks
                .iter()
                .filter(|b| str_of(b, "type") == Some("tool_result"))
            {
                let Some(id) = str_of(b, "tool_use_id") else {
                    continue;
                };
                let text = content_text(b.get("content").unwrap_or(&Value::Null));
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
        }
        if !out.is_empty() {
            return out;
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
        self.pending.insert(
            request_id.to_owned(),
            PendingApproval {
                tool_use_id: tool_use_id.clone(),
                input: input.clone(),
                suggestions: array_of(req, "permission_suggestions"),
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
                options: vec![Decision::Allow, Decision::AllowForSession, Decision::Deny],
                response: ResponseCapability::Live,
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

        if str_of(resp, "subtype") == Some("error") {
            self.usage_requests.remove(id);
            let error = str_of(resp, "error")
                .unwrap_or("control request failed")
                .to_owned();
            return vec![Envelope::new(Event::ControlResult {
                ok: None,
                error: Some(error),
            })
            .request(id)];
        }

        out.push(
            Envelope::new(Event::ControlResult {
                ok: Some(body.clone().unwrap_or_else(|| json!({}))),
                error: None,
            })
            .request(id),
        );
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
        self.turn_open = false;
        let interrupted_by_us = std::mem::take(&mut self.interrupt_pending);
        self.blocks.clear();
        self.msg_ids.clear();
        self.snap_counts.clear();
        self.started_items.clear();

        let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
        let success = str_of(v, "subtype") == Some("success");
        let aborted = str_of(v, "terminal_reason") == Some("aborted_streaming");
        let (state, error) = if aborted || (interrupted_by_us && (is_error || !success)) {
            (TurnState::Interrupted, None)
        } else if success && !is_error {
            (TurnState::Completed, None)
        } else {
            (TurnState::Failed, Some(failure_text(v)))
        };
        let usage = v.get("usage").map(usage_of);
        // Anything still open never got its own close: the turn is over, so settle it.
        let mut out: Vec<Envelope> = std::mem::take(&mut self.open_items)
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
                self.control_line(&id, json!({"subtype": "set_model", "model": model}))
            }
            Command::SetMode { mode } => {
                let id = self.next_id("mode");
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
                .request(id),
            );
        }
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
        let p = self
            .pending
            .remove(request)
            .ok_or_else(|| unknown_request(request))?;
        let mut body = Map::new();
        match decision {
            Decision::Allow | Decision::AllowForSession => {
                body.insert("behavior".to_owned(), json!("allow"));
                body.insert(
                    "updatedInput".to_owned(),
                    updated_input.unwrap_or_else(|| p.input.clone()),
                );
                if decision == Decision::AllowForSession && !p.suggestions.is_empty() {
                    body.insert("updatedPermissions".to_owned(), Value::Array(p.suggestions));
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
        assert!(matches!(&out[0].event, Event::ControlResult { ok: Some(_), .. }));
        assert!(matches!(&out[1].event, Event::QuotaUpdated { account: None, windows }
            if windows.len() == 1 && windows[0].used == 0.5));
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
            assert!(matches!(actions.as_slice(), [Action::Write(_)]), "{actions:?}");
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
