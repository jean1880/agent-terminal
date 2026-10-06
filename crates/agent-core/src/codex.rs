//! OpenAI Codex adapter: `codex app-server`, a JSON-RPC-shaped protocol over stdio NDJSON.
//!
//! A pure, sans-I/O state machine, like [`crate::claude`] and [`crate::agy`]. Frames are
//! `{id,method,params}` requests, `{method,params}` notifications and `{id,result|error}`
//! responses, WITHOUT a `"jsonrpc"` field, one per line.
//!
//! # Provenance
//!
//! Nothing here is replayed from a real recording yet (`tests/fixtures/codex-synthetic-turn.ndjson`
//! is SYNTHETIC). The mapping targets **codex-cli 0.160.1**: the method registry, the enums
//! (`AskForApproval`, `ApprovalsReviewer`, `SandboxPolicy`, `ReasoningSummary`, the approval
//! decisions) and `Model` were cross-checked against the JSON Schema that binary generates
//! (`codex app-server generate-json-schema`, stable surface; it writes files only). The struct
//! detail comes from the upstream protocol source, `openai/codex`
//! `codex-rs/app-server-protocol/src/protocol/` at HEAD:
//!
//! - `common.rs`: the method registry (`client_request_definitions!`, `server_request_definitions!`,
//!   `server_notification_definitions!`): `initialize`, `thread/start`, `thread/resume`,
//!   `thread/compact/start`, `turn/start`, `turn/interrupt`, `model/list`; the server requests
//!   `item/commandExecution/requestApproval` and `item/fileChange/requestApproval`; the
//!   notifications used below.
//! - `v1.rs`: `InitializeParams` (`clientInfo{name,title,version}`, optional `capabilities`).
//! - `v2/thread.rs`: `ThreadStartParams`, `ThreadResumeParams` (`excludeTurns`),
//!   `ThreadStartResponse`, `ThreadTokenUsageUpdatedNotification`, `ThreadTokenUsage`.
//! - `v2/turn.rs`: `TurnStartParams`, `TurnInterruptParams`, `UserInput`, `TurnStatus`,
//!   `TurnPlanUpdatedNotification`.
//! - `v2/thread_data.rs`: `Thread`, `Turn`, `TurnError`.
//! - `v2/item.rs`: `ThreadItem` (internally tagged by `type`), the approval params and decision
//!   enums, the item delta notifications, `CommandExecutionStatus` / `PatchApplyStatus`.
//! - `v2/shared.rs`: `AskForApproval` (kebab-case: `untrusted`, `on-request`, `never`),
//!   `ApprovalsReviewer` (`user`), `SandboxMode`.
//! - `v2/permissions.rs`: `SandboxPolicy` (tag `type`: `readOnly`, `workspaceWrite`, ...).
//! - `v2/model.rs`: `ModelListParams`, `Model`, `ModelListResponse`.
//! - `v2/notification.rs`: `ErrorNotification` (`willRetry`), `ServerRequestResolvedNotification`.
//!
//! The one thing taken from T3 Code (MIT, Copyright (c) 2026 T3 Tools Inc.,
//! `CodexAdapterV2.ts`) rather than the protocol crate is the `tools.update_plan.enabled` thread
//! config (Codex 0.152 made the `update_plan` tool opt-in; without it `turn/plan/updated` never
//! fires), the runtime-mode table's starting point, and always sending `approvalsReviewer`.
//!
//! # Handshake
//!
//! `handshake()` returns three lines at once: the `initialize` request, the `initialized`
//! notification and `thread/start` (or `thread/resume` when [`OpenSession::resume`] is set). The
//! adapter cannot write after reading (a `feed` returns events, not writes), so the sequence is
//! pipelined; the server handles one connection's messages in order. No experimental API is
//! used, so `initialize` carries no `capabilities`.
//!
//! The session the adapter was given in [`Adapter::argv`] is remembered (behind a `Mutex`,
//! because `argv` takes `&self`) and read by `handshake()` and by every `turn/start`.
//!
//! # Prompts and the outbox
//!
//! `turn/start` needs the thread id, which arrives in the `thread/start` response. A prompt encoded
//! earlier is queued and written when the response is fed. Writes and events the adapter
//! produces outside a call's return value (that flush, replies to server requests the app cannot
//! serve, the answer to `Control::ContextUsage`, `ModelChanged` / `ModeChanged`) go to
//! [`Adapter::drain_outbox`], which the transport calls after every `encode` / `feed`.
//!
//! # Modes (`approvalPolicy` + `sandboxPolicy`, sent on every `turn/start`)
//!
//! | [`Mode`]      | approvalPolicy | sandboxPolicy       | Effect                                  |
//! |---------------|----------------|---------------------|-----------------------------------------|
//! | `Ask`         | `untrusted`    | `readOnly`          | only known-safe reads run unasked; every edit and other command asks |
//! | `AcceptEdits` | `on-request`   | `workspaceWrite`    | edits inside the workspace are automatic; the model asks to escalate |
//! | `Plan`        | `never`        | `readOnly`          | cannot write and never asks (T3 Code has no read-only row; this is the strictest the enums allow without the experimental `collaborationMode`) |
//!
//! `Ask` is T3's `approval-required` row, `AcceptEdits` its `auto-accept-edits` row.
//!
//! # Known limits (each with its upgrade path)
//!
//! - `model/list` reads one page of up to [`MODEL_PAGE`] models; follow `nextCursor` if a catalogue
//!   ever exceeds that.
//! - The legacy `execCommandApproval` / `applyPatchApproval` server requests (turns started over
//!   the pre-v2 API) are not served: turns here always use `turn/start`, which only raises the
//!   v2 requests. They get a JSON-RPC error reply so the server never waits on them.
//! - `item/permissions/requestApproval`, `item/tool/requestUserInput`,
//!   `mcpServer/elicitation/request` and `item/tool/call` are answered with a JSON-RPC error and a
//!   [`Event::Notice`]; map them once the UI has a place for them.
//! - Approval decisions that carry a payload (`acceptWithExecpolicyAmendment`,
//!   `applyNetworkPolicyAmendment`) are not offered; the four plain decisions are.
//! - stderr (Codex's tracing output) is ignored.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard};

use serde_json::{json, Map, Value};

use crate::adapter::{
    Action, Adapter, AdapterError, Command, Control, Driver, Mode, OpenSession, Outbox,
};
use crate::caps::Capabilities;
use crate::event::{
    Decision, Envelope, Event, ItemKind, ItemStatus, PlanStep, ResponseCapability, StepStatus,
    StreamKind, TurnState, Usage,
};

/// Page size asked of `model/list`.
const MODEL_PAGE: u32 = 200;

/// Notifications that carry nothing the UI renders (bookkeeping, or covered by another frame):
/// dropped without an [`Event::Unknown`], which would only add noise to every turn.
const QUIET: &[&str] = &[
    "thread/started",
    "thread/status/changed",
    "thread/name/updated",
    "thread/settings/updated",
    "thread/compacted",
    "skills/changed",
    "account/updated",
    "account/rateLimits/updated",
    "mcpServer/startupStatus/updated",
    "remoteControl/status/changed",
    "hook/started",
    "hook/completed",
    "turn/diff/updated",
    "item/reasoning/textDelta",
    "item/commandExecution/terminalInteraction",
    "item/mcpToolCall/progress",
    "item/autoApprovalReview/started",
    "item/autoApprovalReview/completed",
];

/// A request this adapter sent and has not seen answered.
#[derive(Debug, Clone)]
enum Pending {
    Initialize,
    /// `thread/start` or `thread/resume`.
    ThreadStart,
    TurnStart,
    Compact,
    Interrupt,
    /// `model/list`, answering the control with this id.
    Models(String),
}

/// An unanswered approval server request.
#[derive(Debug, Clone)]
struct PendingApproval {
    /// The JSON-RPC id to put in the response, verbatim (number or string).
    rpc_id: Value,
    item: String,
}

pub struct CodexAdapter {
    caps: Capabilities,
    client_version: String,
    /// The session given to [`Adapter::argv`].
    session: Mutex<Option<OpenSession>>,
    next_id: u64,
    pending: HashMap<u64, Pending>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    turn_open: bool,
    /// An interrupt was sent for the open turn, so an exit before it ends is expected.
    interrupt_sent: bool,
    model: Option<String>,
    /// Reasoning effort sent on every `turn/start` (`low`/`medium`/`high`/...; a model-advertised
    /// string). Seeded from `OpenSession::effort`, changed by `Command::SetModel { effort }`
    /// (per turn: no respawn).
    effort: Option<String>,
    mode: Mode,
    cwd: Option<String>,
    /// Prompts encoded before the thread id arrived.
    queued: VecDeque<String>,
    outbox: Outbox,
    approvals: HashMap<String, PendingApproval>,
    /// Ids of items started and not yet completed, in start order.
    open_items: Vec<String>,
    /// Accumulated streamed text per open message item (snapshot fallback).
    texts: HashMap<String, String>,
    /// Accumulated reasoning summary per open reasoning item.
    reasoning: HashMap<String, String>,
    /// Latest cumulative thread usage, and the cumulative usage when the last turn ended.
    usage_total: Option<Usage>,
    usage_base: Usage,
    /// Context occupancy and window from the last `thread/tokenUsage/updated`.
    context: Option<(u64, Option<u64>)>,
    /// A `/compact` was requested, so the coming `contextCompaction` item is manual.
    compact_pending: bool,
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexAdapter {
    pub fn new() -> Self {
        Self {
            caps: Capabilities::codex(),
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
            session: Mutex::new(None),
            next_id: 0,
            pending: HashMap::new(),
            thread_id: None,
            turn_id: None,
            turn_open: false,
            interrupt_sent: false,
            model: None,
            effort: None,
            mode: Mode::default(),
            cwd: None,
            queued: VecDeque::new(),
            outbox: Outbox::default(),
            approvals: HashMap::new(),
            open_items: Vec::new(),
            texts: HashMap::new(),
            reasoning: HashMap::new(),
            usage_total: None,
            usage_base: Usage::default(),
            context: None,
            compact_pending: false,
        }
    }

    /// The version reported in `initialize`'s `clientInfo` (default: this crate's version; the
    /// app passes its own).
    pub fn client_version(mut self, version: impl Into<String>) -> Self {
        self.client_version = version.into();
        self
    }

    fn session(&self) -> MutexGuard<'_, Option<OpenSession>> {
        // A poisoned lock only means another thread panicked while holding it; the data is
        // plain and still valid.
        self.session.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records a request and returns its line.
    fn request(&mut self, pending: Pending, method: &str, params: Value) -> String {
        self.next_id += 1;
        let id = self.next_id;
        self.pending.insert(id, pending);
        json!({"id": id, "method": method, "params": params}).to_string()
    }

    fn write(&mut self, line: String) {
        self.outbox.actions.push(Action::Write(vec![line]));
    }

    fn emit(&mut self, event: Event) {
        self.outbox.events.push(Envelope::new(event));
    }

    fn turn_start_line(&mut self, thread: &str, text: &str) -> String {
        let (approval, sandbox) = mode_policy(self.mode);
        let mut params = Map::new();
        params.insert("threadId".into(), json!(thread));
        params.insert("input".into(), json!([{"type": "text", "text": text}]));
        if let Some(cwd) = &self.cwd {
            params.insert("cwd".into(), json!(cwd));
        }
        if let Some(model) = &self.model {
            params.insert("model".into(), json!(model));
        }
        if let Some(effort) = &self.effort {
            params.insert("effort".into(), json!(effort));
        }
        params.insert("approvalPolicy".into(), json!(approval));
        // Always explicit: omitting it leaves a previously set reviewer in effect.
        params.insert("approvalsReviewer".into(), json!("user"));
        params.insert("sandboxPolicy".into(), sandbox);
        // The model catalogue can default summaries to "none"; ask for them every turn.
        params.insert("summary".into(), json!("detailed"));
        self.request(Pending::TurnStart, "turn/start", Value::Object(params))
    }

    // ---- items ----

    fn start_item(
        &mut self,
        out: &mut Vec<Envelope>,
        id: &str,
        kind: ItemKind,
        title: String,
        input: Option<Value>,
    ) {
        if self.open_items.iter().any(|i| i == id) {
            return;
        }
        self.open_items.push(id.to_owned());
        out.push(
            Envelope::new(Event::ItemStarted {
                kind,
                title,
                input,
                parent: None,
            })
            .item(id),
        );
    }

    fn complete_item(
        &mut self,
        out: &mut Vec<Envelope>,
        id: &str,
        status: ItemStatus,
        output: Option<String>,
        error: Option<String>,
    ) {
        self.open_items.retain(|i| i != id);
        self.texts.remove(id);
        self.reasoning.remove(id);
        out.push(
            Envelope::new(Event::ItemCompleted {
                status,
                output,
                error,
            })
            .item(id),
        );
    }

    fn close_all(&mut self, out: &mut Vec<Envelope>) {
        for id in std::mem::take(&mut self.open_items) {
            for (stream, text) in [
                (StreamKind::Assistant, self.texts.remove(&id)),
                (StreamKind::Reasoning, self.reasoning.remove(&id)),
            ] {
                if let Some(text) = text.filter(|t| !t.is_empty()) {
                    out.push(Envelope::new(Event::ContentSnapshot { stream, text }).item(&id));
                }
            }
            out.push(
                Envelope::new(Event::ItemCompleted {
                    status: ItemStatus::Interrupted,
                    output: None,
                    error: None,
                })
                .item(id),
            );
        }
    }

    fn on_item_started(&mut self, params: &Value, out: &mut Vec<Envelope>) {
        let Some(item) = params.get("item") else {
            out.push(Envelope::new(Event::Unknown));
            return;
        };
        let Some(id) = s(item, "id") else {
            out.push(Envelope::new(Event::Unknown));
            return;
        };
        if is_silent_item(item) {
            return;
        }
        let (kind, title, input) = item_info(item);
        self.start_item(out, id, kind, title, input);
    }

    fn on_item_completed(&mut self, params: &Value, out: &mut Vec<Envelope>) {
        let Some(item) = params.get("item") else {
            out.push(Envelope::new(Event::Unknown));
            return;
        };
        let Some(id) = s(item, "id") else {
            out.push(Envelope::new(Event::Unknown));
            return;
        };
        if s(item, "type") == Some("contextCompaction") {
            out.push(Envelope::new(Event::Compacted {
                manual: std::mem::take(&mut self.compact_pending),
                before: self.context.map_or(0, |(used, _)| used),
                after: None,
            }));
            return;
        }
        if is_silent_item(item) {
            return;
        }
        let (kind, title, input) = item_info(item);
        self.start_item(out, id, kind, title, input);
        // The completed item is authoritative: its text replaces whatever streamed.
        let streamed_text = self.texts.get(id).cloned();
        let streamed_reasoning = self.reasoning.get(id).cloned();
        let snapshot = match s(item, "type") {
            Some("agentMessage" | "plan") => s(item, "text")
                .map(str::to_owned)
                .or(streamed_text)
                .map(|t| (StreamKind::Assistant, t)),
            Some("reasoning") => {
                let summary = strings(item, "summary");
                let content = strings(item, "content");
                let text = if !summary.is_empty() {
                    summary.join("\n\n")
                } else if !content.is_empty() {
                    content.join("\n\n")
                } else {
                    streamed_reasoning.unwrap_or_default()
                };
                Some((StreamKind::Reasoning, text))
            }
            _ => None,
        };
        if let Some((stream, text)) = snapshot.filter(|(_, t)| !t.is_empty()) {
            out.push(Envelope::new(Event::ContentSnapshot { stream, text }).item(id));
        }
        let (status, output, error) = item_result(item);
        self.complete_item(out, id, status, output, error);
    }

    fn on_text_delta(
        &mut self,
        params: &Value,
        stream: StreamKind,
        out: &mut Vec<Envelope>,
    ) -> bool {
        let (Some(id), Some(delta)) = (s(params, "itemId"), s(params, "delta")) else {
            return false;
        };
        if self.thread_id.is_none() {
            return false;
        }
        let (kind, title) = match stream {
            StreamKind::Reasoning => (ItemKind::Reasoning, "reasoning"),
            _ => (ItemKind::AssistantMessage, "assistant"),
        };
        self.start_item(out, id, kind, title.to_owned(), None);
        let acc = match stream {
            StreamKind::Reasoning => &mut self.reasoning,
            _ => &mut self.texts,
        };
        acc.entry(id.to_owned()).or_default().push_str(delta);
        out.push(
            Envelope::new(Event::ContentDelta {
                stream,
                text: delta.to_owned(),
            })
            .item(id),
        );
        true
    }

    // ---- notifications ----

    fn on_notification(&mut self, method: &str, params: &Value, out: &mut Vec<Envelope>) {
        match method {
            "turn/started" => {
                self.turn_open = true;
                self.interrupt_sent = false;
                if let Some(turn) = s(params.get("turn").unwrap_or(&Value::Null), "id") {
                    self.turn_id = Some(turn.to_owned());
                }
                out.push(Envelope::new(Event::TurnStarted {
                    model: self.model.clone(),
                }));
            }
            "turn/completed" => self.on_turn_completed(params, out),
            "item/started" => self.on_item_started(params, out),
            "item/completed" => self.on_item_completed(params, out),
            "item/agentMessage/delta" | "item/plan/delta" => {
                if !self.on_text_delta(params, StreamKind::Assistant, out) {
                    out.push(Envelope::new(Event::Unknown));
                }
            }
            "item/reasoning/summaryTextDelta" => {
                if !self.on_text_delta(params, StreamKind::Reasoning, out) {
                    out.push(Envelope::new(Event::Unknown));
                }
            }
            "item/reasoning/summaryPartAdded" => {
                // A new summary part: separate it from the text already shown.
                if let Some(id) = s(params, "itemId") {
                    let has_text = self.reasoning.get(id).is_some_and(|t| !t.is_empty());
                    if has_text {
                        self.reasoning
                            .entry(id.to_owned())
                            .or_default()
                            .push_str("\n\n");
                        out.push(
                            Envelope::new(Event::ContentDelta {
                                stream: StreamKind::Reasoning,
                                text: "\n\n".to_owned(),
                            })
                            .item(id),
                        );
                    }
                }
            }
            "item/commandExecution/outputDelta" => {
                match (s(params, "itemId"), s(params, "delta")) {
                    (Some(id), Some(delta)) => out.push(
                        Envelope::new(Event::ContentDelta {
                            stream: StreamKind::ToolOutput,
                            text: delta.to_owned(),
                        })
                        .item(id),
                    ),
                    _ => out.push(Envelope::new(Event::Unknown)),
                }
            }
            "item/fileChange/patchUpdated" => {
                if let Some(id) = s(params, "itemId") {
                    let diff = params
                        .get("changes")
                        .and_then(Value::as_array)
                        .map(|c| {
                            c.iter()
                                .filter_map(|ch| s(ch, "diff"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default();
                    if !diff.is_empty() {
                        out.push(
                            Envelope::new(Event::ContentSnapshot {
                                stream: StreamKind::ToolInput,
                                text: diff,
                            })
                            .item(id),
                        );
                    }
                }
            }
            "turn/plan/updated" => {
                let steps = params
                    .get("plan")
                    .and_then(Value::as_array)
                    .map(|plan| {
                        plan.iter()
                            .map(|p| PlanStep {
                                text: s(p, "step").unwrap_or_default().to_owned(),
                                status: match s(p, "status") {
                                    Some("completed") => StepStatus::Completed,
                                    Some("inProgress") => StepStatus::InProgress,
                                    _ => StepStatus::Pending,
                                },
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                out.push(Envelope::new(Event::PlanUpdated { steps }));
            }
            "thread/tokenUsage/updated" => self.on_token_usage(params, out),
            "account/rateLimits/updated" => {
                // A sparse rolling update; the app merges windows by (group, label).
                let windows = crate::quota::codex_rate_limits(params);
                if !windows.is_empty() {
                    out.push(Envelope::new(Event::QuotaUpdated {
                        account: None,
                        windows,
                    }));
                }
            }
            "serverRequest/resolved" => {
                // Resolved by something other than our answer (the turn was interrupted).
                let key = params.get("requestId").map(id_key).unwrap_or_default();
                if let Some(p) = self.approvals.remove(&key) {
                    out.push(
                        Envelope::new(Event::ApprovalExpired)
                            .item(p.item)
                            .request(key),
                    );
                }
            }
            "model/rerouted" => match s(params, "toModel") {
                Some(model) => {
                    self.model = Some(model.to_owned());
                    out.push(Envelope::new(Event::ModelChanged {
                        model: model.to_owned(),
                    }));
                }
                None => out.push(Envelope::new(Event::Unknown)),
            },
            "error" => {
                let message = params
                    .get("error")
                    .and_then(|e| s(e, "message"))
                    .unwrap_or("codex reported an error")
                    .to_owned();
                if params.get("willRetry").and_then(Value::as_bool) == Some(true) {
                    out.push(Envelope::new(Event::Notice {
                        text: format!("Codex is retrying: {message}"),
                    }));
                } else {
                    out.push(Envelope::new(Event::Error { message }));
                }
            }
            "warning" => match s(params, "message") {
                Some(text) => out.push(Envelope::new(Event::Notice {
                    text: text.to_owned(),
                })),
                None => out.push(Envelope::new(Event::Unknown)),
            },
            m if QUIET.contains(&m) => {}
            _ => out.push(Envelope::new(Event::Unknown)),
        }
    }

    fn on_turn_completed(&mut self, params: &Value, out: &mut Vec<Envelope>) {
        let turn = params.get("turn").unwrap_or(&Value::Null);
        // Anything still open when the turn ends was cut short.
        self.close_all(out);
        let message = turn
            .get("error")
            .and_then(|e| s(e, "message"))
            .map(str::to_owned);
        let state = match s(turn, "status") {
            Some("completed") => TurnState::Completed,
            Some("interrupted") => TurnState::Interrupted,
            _ => TurnState::Failed,
        };
        let error = match state {
            TurnState::Failed => Some(message.unwrap_or_else(|| "codex turn failed".to_owned())),
            _ => None,
        };
        let usage = self.usage_total.clone().and_then(|total| {
            let delta = Usage {
                input_tokens: total
                    .input_tokens
                    .saturating_sub(self.usage_base.input_tokens),
                output_tokens: total
                    .output_tokens
                    .saturating_sub(self.usage_base.output_tokens),
                cached_input_tokens: total
                    .cached_input_tokens
                    .saturating_sub(self.usage_base.cached_input_tokens),
                reasoning_tokens: total
                    .reasoning_tokens
                    .saturating_sub(self.usage_base.reasoning_tokens),
            };
            self.usage_base = total;
            (delta != Usage::default()).then_some(delta)
        });
        self.turn_open = false;
        self.turn_id = None;
        self.interrupt_sent = false;
        out.push(Envelope::new(Event::TurnCompleted {
            state,
            usage,
            cost_usd: None,
            error,
        }));
    }

    fn on_token_usage(&mut self, params: &Value, out: &mut Vec<Envelope>) {
        let Some(usage) = params.get("tokenUsage") else {
            out.push(Envelope::new(Event::Unknown));
            return;
        };
        if let Some(total) = usage.get("total") {
            self.usage_total = Some(usage_of(total));
        }
        let used = usage
            .get("last")
            .and_then(|l| num(l, "totalTokens"))
            .unwrap_or(0);
        let max = num(usage, "modelContextWindow");
        self.context = Some((used, max));
        out.push(Envelope::new(Event::UsageUpdated {
            used,
            max,
            auto_compact_at: None,
        }));
    }

    // ---- server requests ----

    fn on_server_request(
        &mut self,
        id: &Value,
        method: &str,
        params: &Value,
        out: &mut Vec<Envelope>,
    ) {
        let (tool, title, all_decisions) = match method {
            "item/commandExecution/requestApproval" => (
                "command_execution",
                s(params, "command").map(str::to_owned),
                true,
            ),
            "item/fileChange/requestApproval" => (
                "file_change",
                Some(
                    s(params, "reason")
                        .unwrap_or("Apply file changes")
                        .to_owned(),
                ),
                true,
            ),
            _ => {
                // Not served: reply with an error so the server does not wait on us forever.
                self.write(
                    json!({"id": id, "error": {
                        "code": -32601,
                        "message": format!("agent-terminal does not handle {method}"),
                    }})
                    .to_string(),
                );
                out.push(Envelope::new(Event::Notice {
                    text: format!("Codex asked for {method}, which agent-terminal does not support yet; it was refused."),
                }));
                return;
            }
        };
        let key = id_key(id);
        let item = s(params, "itemId").unwrap_or_default().to_owned();
        let options = match params.get("availableDecisions").and_then(Value::as_array) {
            Some(list) if all_decisions => {
                let mut options: Vec<Decision> = Vec::new();
                for d in list
                    .iter()
                    .filter_map(Value::as_str)
                    .filter_map(decision_of)
                {
                    if !options.contains(&d) {
                        options.push(d);
                    }
                }
                if options.is_empty() {
                    ALL_DECISIONS.to_vec()
                } else {
                    options
                }
            }
            _ => ALL_DECISIONS.to_vec(),
        };
        self.approvals.insert(
            key.clone(),
            PendingApproval {
                rpc_id: id.clone(),
                item: item.clone(),
            },
        );
        out.push(
            Envelope::new(Event::ApprovalRequested {
                tool: tool.to_owned(),
                title,
                input: params.clone(),
                reason: s(params, "reason").map(str::to_owned),
                options,
                response: ResponseCapability::Live,
            })
            .item(item)
            .request(key),
        );
    }

    // ---- responses ----

    fn on_response(&mut self, id: &Value, frame: &Value, out: &mut Vec<Envelope>) {
        let Some(pending) = id.as_u64().and_then(|n| self.pending.remove(&n)) else {
            out.push(Envelope::new(Event::Unknown));
            return;
        };
        let result = frame.get("result");
        let error = frame
            .get("error")
            .map(|e| s(e, "message").map_or_else(|| e.to_string(), str::to_owned));
        match (pending, result, error) {
            (Pending::Initialize, Some(_), _) => {}
            (Pending::ThreadStart, Some(result), _) => self.on_thread_ready(result, out),
            (Pending::TurnStart, Some(result), _) => {
                if self.turn_id.is_none() {
                    self.turn_id = result
                        .get("turn")
                        .and_then(|t| s(t, "id"))
                        .map(str::to_owned);
                }
            }
            (Pending::Models(control), Some(result), _) => out.push(
                Envelope::new(Event::ControlResult {
                    ok: Some(models_of(result)),
                    error: None,
                })
                .request(control),
            ),
            (Pending::Models(control), None, error) => out.push(
                Envelope::new(Event::ControlResult {
                    ok: None,
                    error: Some(error.unwrap_or_else(|| "model/list failed".to_owned())),
                })
                .request(control),
            ),
            (Pending::TurnStart, None, error) => {
                let message = error.unwrap_or_else(|| "turn/start failed".to_owned());
                out.push(Envelope::new(Event::Error {
                    message: message.clone(),
                }));
                // No `turn/started` will follow; end the turn the UI began when it sent this.
                if !self.turn_open {
                    out.push(Envelope::new(Event::TurnCompleted {
                        state: TurnState::Failed,
                        usage: None,
                        cost_usd: None,
                        error: Some(message),
                    }));
                }
            }
            (Pending::Initialize | Pending::ThreadStart | Pending::Compact, None, error) => {
                out.push(Envelope::new(Event::Error {
                    message: error.unwrap_or_else(|| "request failed".to_owned()),
                }));
            }
            // An interrupt with no turn to stop, or an acknowledged compact: nothing to show.
            (Pending::Interrupt | Pending::Compact, _, _) => {}
        }
    }

    fn on_thread_ready(&mut self, result: &Value, out: &mut Vec<Envelope>) {
        let Some(thread) = result
            .get("thread")
            .and_then(|t| s(t, "id"))
            .map(str::to_owned)
        else {
            out.push(Envelope::new(Event::Error {
                message: "thread/start returned no thread id".to_owned(),
            }));
            return;
        };
        self.thread_id = Some(thread.clone());
        if let Some(model) = s(result, "model") {
            self.model = Some(model.to_owned());
        }
        out.push(Envelope::new(Event::SessionStarted {
            native_id: thread.clone(),
            model: self.model.clone(),
            cwd: s(result, "cwd").map(str::to_owned),
        }));
        while let Some(text) = self.queued.pop_front() {
            let line = self.turn_start_line(&thread, &text);
            self.write(line);
        }
    }
}

impl Adapter for CodexAdapter {
    fn driver(&self) -> Driver {
        Driver::Codex
    }

    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    /// `<program> <extra_args> app-server`. The session is remembered for [`Adapter::handshake`]
    /// and every later `turn/start` (cwd, model, mode, resume id).
    fn argv(&self, session: &OpenSession) -> Vec<String> {
        *self.session() = Some(session.clone());
        let mut argv = vec![session.program.clone()];
        argv.extend(session.extra_args.iter().cloned());
        argv.push("app-server".to_owned());
        argv
    }

    fn handshake(&mut self) -> Vec<String> {
        let session = self.session().clone();
        // A new process: forget the last one's connection state. Queued prompts survive.
        self.next_id = 0;
        self.pending.clear();
        self.thread_id = None;
        self.turn_id = None;
        self.turn_open = false;
        self.interrupt_sent = false;
        self.approvals.clear();
        self.open_items.clear();
        self.texts.clear();
        self.reasoning.clear();
        self.usage_total = None;
        self.usage_base = Usage::default();
        self.context = None;
        self.compact_pending = false;
        self.outbox = Outbox::default();
        if let Some(s) = &session {
            self.model = s.model.clone();
            self.effort = s.effort.clone();
            self.mode = s.mode;
            self.cwd = Some(s.cwd.clone()).filter(|c| !c.is_empty());
        }

        let version = self.client_version.clone();
        let init = self.request(
            Pending::Initialize,
            "initialize",
            json!({"clientInfo": {
                "name": "agent-terminal",
                "title": "Agent Terminal",
                "version": version,
            }}),
        );
        let initialized = json!({"method": "initialized"}).to_string();

        let mut params = Map::new();
        if let Some(cwd) = &self.cwd {
            params.insert("cwd".into(), json!(cwd));
        }
        if let Some(model) = &self.model {
            params.insert("model".into(), json!(model));
        }
        params.insert("approvalsReviewer".into(), json!("user"));
        params.insert("config".into(), json!({"tools.update_plan.enabled": true}));
        let resume = session.and_then(|s| s.resume);
        let thread = match resume {
            Some(id) => {
                params.insert("threadId".into(), json!(id));
                // The app keeps its own transcript; do not have the server send the history.
                params.insert("excludeTurns".into(), json!(true));
                self.request(Pending::ThreadStart, "thread/resume", Value::Object(params))
            }
            None => self.request(Pending::ThreadStart, "thread/start", Value::Object(params)),
        };
        vec![init, initialized, thread]
    }

    fn encode(&mut self, command: Command) -> Result<Vec<Action>, AdapterError> {
        match command {
            Command::Prompt { text } => {
                if text.trim().eq_ignore_ascii_case("/compact") {
                    let Some(thread) = self.thread_id.clone() else {
                        return Err(AdapterError::Invalid(
                            "the Codex thread is not started yet".to_owned(),
                        ));
                    };
                    self.compact_pending = true;
                    let line = self.request(
                        Pending::Compact,
                        "thread/compact/start",
                        json!({"threadId": thread}),
                    );
                    return Ok(vec![Action::Write(vec![line])]);
                }
                match self.thread_id.clone() {
                    Some(thread) => {
                        let line = self.turn_start_line(&thread, &text);
                        Ok(vec![Action::Write(vec![line])])
                    }
                    None => {
                        self.queued.push_back(text);
                        Ok(Vec::new())
                    }
                }
            }
            Command::Interrupt => {
                let (Some(thread), Some(turn)) = (self.thread_id.clone(), self.turn_id.clone())
                else {
                    return Ok(Vec::new()); // nothing running: a no-op, like Esc at an idle prompt
                };
                self.interrupt_sent = true;
                let line = self.request(
                    Pending::Interrupt,
                    "turn/interrupt",
                    json!({"threadId": thread, "turnId": turn}),
                );
                Ok(vec![Action::Write(vec![line])])
            }
            Command::Approve {
                request, decision, ..
            } => {
                let Some(approval) = self.approvals.remove(&request) else {
                    return Err(AdapterError::Invalid(format!(
                        "no pending approval {request}"
                    )));
                };
                let line = json!({
                    "id": approval.rpc_id,
                    "result": {"decision": decision_wire(decision)},
                })
                .to_string();
                Ok(vec![Action::Write(vec![line])])
            }
            Command::Answer { .. } => Err(AdapterError::Unsupported(
                "Codex questions are not supported yet",
            )),
            Command::SetModel { model, effort } => {
                // Applied by the next `turn/start`; no restart. `None` keeps the effort.
                if effort.is_some() {
                    self.effort = effort;
                }
                self.model = Some(model.clone());
                self.emit(Event::ModelChanged { model });
                Ok(Vec::new())
            }
            Command::SetMode { mode } => {
                self.mode = mode;
                self.emit(Event::ModeChanged { mode });
                Ok(Vec::new())
            }
            Command::Control { id, control } => match control {
                Control::ListModels => {
                    let line = self.request(
                        Pending::Models(id),
                        "model/list",
                        json!({"limit": MODEL_PAGE}),
                    );
                    Ok(vec![Action::Write(vec![line])])
                }
                Control::ContextUsage => {
                    let event = match self.context {
                        Some((used, max)) => Event::ControlResult {
                            ok: Some(json!({"used": used, "max": max})),
                            error: None,
                        },
                        None => Event::ControlResult {
                            ok: None,
                            error: Some("Codex has not reported token usage yet".to_owned()),
                        },
                    };
                    self.outbox.events.push(Envelope::new(event).request(id));
                    Ok(Vec::new())
                }
                _ => Err(AdapterError::Unsupported(
                    "this control is not offered by Codex",
                )),
            },
        }
    }

    fn feed(&mut self, line: &str) -> Vec<Envelope> {
        let line = line.trim();
        if line.is_empty() {
            return Vec::new();
        }
        let raw: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return vec![unknown(Value::String(line.to_owned()))],
        };
        let id = raw.get("id").filter(|v| !v.is_null()).cloned();
        let method = raw.get("method").and_then(Value::as_str).map(str::to_owned);
        let params = raw.get("params").cloned().unwrap_or(Value::Null);
        let mut out = Vec::new();
        match (method, id) {
            (Some(method), Some(id)) => self.on_server_request(&id, &method, &params, &mut out),
            (Some(method), None) => self.on_notification(&method, &params, &mut out),
            (None, Some(id)) => self.on_response(&id, &raw, &mut out),
            (None, None) => out.push(Envelope::new(Event::Unknown)),
        }
        if let Some(first) = out.first_mut() {
            first.raw = Some(raw);
        }
        out
    }

    fn feed_stderr(&mut self, _line: &str) -> Vec<Envelope> {
        Vec::new()
    }

    fn feed_side(&mut self, _id: &str, _stdout: &str, _success: bool) -> Vec<Envelope> {
        Vec::new() // Codex runs no side processes
    }

    fn on_exit(&mut self, code: Option<i32>) -> Vec<Envelope> {
        let expected = self.interrupt_sent;
        let mut out = Vec::new();
        self.close_all(&mut out);
        let mut approvals: Vec<_> = std::mem::take(&mut self.approvals).into_iter().collect();
        approvals.sort_by(|a, b| a.0.cmp(&b.0));
        for (request, approval) in approvals {
            out.push(
                Envelope::new(Event::ApprovalExpired)
                    .item(approval.item)
                    .request(request),
            );
        }
        let mut controls: Vec<String> = self
            .pending
            .drain()
            .filter_map(|(_, p)| match p {
                Pending::Models(id) => Some(id),
                _ => None,
            })
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
                error: (!expected).then(|| format!("codex exited unexpectedly (code {code:?})")),
            }));
        }
        self.thread_id = None;
        self.turn_id = None;
        self.interrupt_sent = false;
        self.outbox.actions.clear(); // writes to a dead process
        out.push(Envelope::new(Event::SessionExited { code, expected }));
        out
    }

    fn drain_outbox(&mut self) -> Outbox {
        std::mem::take(&mut self.outbox)
    }
}

const ALL_DECISIONS: [Decision; 4] = [
    Decision::Allow,
    Decision::AllowForSession,
    Decision::Deny,
    Decision::Cancel,
];

/// `approvalPolicy` and `sandboxPolicy` for a mode (see the module's mode table).
fn mode_policy(mode: Mode) -> (&'static str, Value) {
    match mode {
        Mode::Ask => ("untrusted", json!({"type": "readOnly"})),
        Mode::AcceptEdits => ("on-request", json!({"type": "workspaceWrite"})),
        Mode::Plan => ("never", json!({"type": "readOnly"})),
    }
}

/// The wire name of a decision (`CommandExecutionApprovalDecision` / `FileChangeApprovalDecision`).
fn decision_wire(decision: Decision) -> &'static str {
    match decision {
        Decision::Allow => "accept",
        Decision::AllowForSession => "acceptForSession",
        Decision::Deny => "decline",
        Decision::Cancel => "cancel",
    }
}

fn decision_of(wire: &str) -> Option<Decision> {
    match wire {
        "accept" => Some(Decision::Allow),
        "acceptForSession" => Some(Decision::AllowForSession),
        "decline" => Some(Decision::Deny),
        "cancel" => Some(Decision::Cancel),
        _ => None,
    }
}

/// Items the app already shows itself (its own prompt) or that have no card.
fn is_silent_item(item: &Value) -> bool {
    matches!(s(item, "type"), Some("userMessage" | "contextCompaction"))
}

/// Kind, card title and input of a started item.
fn item_info(item: &Value) -> (ItemKind, String, Option<Value>) {
    let ty = s(item, "type").unwrap_or("item");
    match ty {
        "agentMessage" => (ItemKind::AssistantMessage, "assistant".to_owned(), None),
        "plan" => (ItemKind::AssistantMessage, "plan".to_owned(), None),
        "reasoning" => (ItemKind::Reasoning, "reasoning".to_owned(), None),
        "commandExecution" => {
            let command = s(item, "command").unwrap_or("command").to_owned();
            let input = json!({"command": command, "cwd": item.get("cwd")});
            (ItemKind::Command, command, Some(input))
        }
        "fileChange" => {
            let changes = item.get("changes").cloned().unwrap_or(Value::Null);
            let paths: Vec<&str> = changes
                .as_array()
                .map(|c| c.iter().filter_map(|ch| s(ch, "path")).collect())
                .unwrap_or_default();
            let title = match paths.as_slice() {
                [] => "file change".to_owned(),
                [one] => (*one).to_owned(),
                [first, rest @ ..] => format!("{first} (+{} more)", rest.len()),
            };
            (ItemKind::FileChange, title, Some(changes))
        }
        "mcpToolCall" => (
            ItemKind::McpTool,
            format!(
                "{}.{}",
                s(item, "server").unwrap_or("mcp"),
                s(item, "tool").unwrap_or("tool")
            ),
            item.get("arguments").cloned(),
        ),
        "dynamicToolCall" => (
            ItemKind::Tool,
            s(item, "tool").unwrap_or("tool").to_owned(),
            item.get("arguments").cloned(),
        ),
        "webSearch" => (
            ItemKind::WebSearch,
            s(item, "query").unwrap_or("web search").to_owned(),
            None,
        ),
        "collabAgentToolCall" => (
            ItemKind::Subagent,
            s(item, "tool").unwrap_or("subagent").to_owned(),
            item.get("prompt").cloned(),
        ),
        "subAgentActivity" => (ItemKind::Subagent, "subagent".to_owned(), None),
        "imageView" => (
            ItemKind::FileRead,
            s(item, "path").unwrap_or("image").to_owned(),
            None,
        ),
        other => (ItemKind::Tool, other.to_owned(), None),
    }
}

/// Status, output and error of a completed item.
fn item_result(item: &Value) -> (ItemStatus, Option<String>, Option<String>) {
    let status = match s(item, "status") {
        Some("completed") => ItemStatus::Completed,
        Some("failed") => ItemStatus::Failed,
        Some("declined") => ItemStatus::Declined,
        Some(_) => ItemStatus::Interrupted,
        None => ItemStatus::Completed,
    };
    match s(item, "type") {
        Some("commandExecution") => {
            let output = s(item, "aggregatedOutput").map(str::to_owned);
            let error = (status == ItemStatus::Failed).then(|| {
                item.get("exitCode")
                    .and_then(Value::as_i64)
                    .map_or_else(|| "command failed".to_owned(), |c| format!("exit code {c}"))
            });
            (status, output, error)
        }
        Some("fileChange") => {
            let error = (status == ItemStatus::Failed).then(|| "patch failed to apply".to_owned());
            (status, None, error)
        }
        Some("mcpToolCall") => {
            let output = item
                .get("result")
                .filter(|r| !r.is_null())
                .map(|r| content_text(r.get("content")).unwrap_or_else(|| r.to_string()));
            let error = item
                .get("error")
                .filter(|e| !e.is_null())
                .map(|e| s(e, "message").map_or_else(|| e.to_string(), str::to_owned));
            (status, output, error)
        }
        Some("dynamicToolCall") => {
            let failed = item.get("success").and_then(Value::as_bool) == Some(false);
            (
                if failed { ItemStatus::Failed } else { status },
                content_text(item.get("contentItems")),
                None,
            )
        }
        _ => (
            if status == ItemStatus::Interrupted {
                ItemStatus::Completed
            } else {
                status
            },
            None,
            None,
        ),
    }
}

/// The `text` of each element of a content array, joined; `None` when there is none.
fn content_text(content: Option<&Value>) -> Option<String> {
    let text: Vec<&str> = content?
        .as_array()?
        .iter()
        .filter_map(|c| s(c, "text"))
        .collect();
    (!text.is_empty()).then(|| text.join("\n"))
}

/// One selectable model from `model/list` (`Model` in `v2/model.rs`). Shaped for the catalogue
/// (id, display, description, efforts); the coordinator maps it onto `catalog::CatalogModel`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CodexModel {
    /// The `model` slug to send in `turn/start` (not the catalogue row's own `id`).
    pub id: String,
    pub display: String,
    pub description: String,
    /// `supportedReasoningEfforts[].reasoningEffort`, in the server's order.
    pub efforts: Vec<String>,
    pub default_effort: Option<String>,
    pub is_default: bool,
}

/// `model/list` result → the visible models (`hidden: true` rows are left out).
pub fn parse_codex_models(result: &Value) -> Vec<CodexModel> {
    result
        .get("data")
        .and_then(Value::as_array)
        .map(|data| {
            data.iter()
                .filter(|m| m.get("hidden").and_then(Value::as_bool) != Some(true))
                .filter_map(|m| {
                    let id = s(m, "model").or_else(|| s(m, "id"))?;
                    Some(CodexModel {
                        id: id.to_owned(),
                        display: s(m, "displayName").unwrap_or(id).to_owned(),
                        description: s(m, "description").unwrap_or_default().to_owned(),
                        efforts: m
                            .get("supportedReasoningEfforts")
                            .and_then(Value::as_array)
                            .map(|e| {
                                e.iter()
                                    .filter_map(|o| s(o, "reasoningEffort"))
                                    .map(str::to_owned)
                                    .collect()
                            })
                            .unwrap_or_default(),
                        default_effort: s(m, "defaultReasoningEffort").map(str::to_owned),
                        is_default: m.get("isDefault").and_then(Value::as_bool) == Some(true),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn models_of(result: &Value) -> Value {
    serde_json::to_value(parse_codex_models(result)).unwrap_or(Value::Null)
}

/// A `TokenUsageBreakdown`: `inputTokens` includes cached input and `outputTokens` includes
/// reasoning output (OpenAI convention), which is the canonical convention too.
fn usage_of(breakdown: &Value) -> Usage {
    Usage {
        input_tokens: num(breakdown, "inputTokens").unwrap_or(0),
        output_tokens: num(breakdown, "outputTokens").unwrap_or(0),
        cached_input_tokens: num(breakdown, "cachedInputTokens").unwrap_or(0),
        reasoning_tokens: num(breakdown, "reasoningOutputTokens").unwrap_or(0),
    }
}

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn num(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}

fn strings(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .filter(|t| !t.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// A JSON-RPC id (number or string) as the request id the UI answers with.
fn id_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn unknown(raw: Value) -> Envelope {
    Envelope::new(Event::Unknown).raw(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TURN: &str = include_str!("../tests/fixtures/codex-synthetic-turn.ndjson");

    fn session(mode: Mode, resume: Option<&str>) -> OpenSession {
        OpenSession {
            program: "/usr/bin/codex".into(),
            extra_args: vec!["-c".into(), "model_reasoning_effort=high".into()],
            cwd: "/work/repo".into(),
            model: Some("gpt-5-codex".into()),
            effort: None,
            mode,
            resume: resume.map(str::to_owned),
            new_session_id: None,
            approval_hook: false,
        }
    }

    /// A started adapter: argv and handshake done, the thread/start response fed.
    fn started(mode: Mode) -> CodexAdapter {
        let mut a = CodexAdapter::new().client_version("0.1.0");
        a.argv(&session(mode, None));
        a.handshake();
        let ev = a.feed(
            r#"{"id":2,"result":{"thread":{"id":"th-1"},"model":"gpt-5-codex","cwd":"/work/repo"}}"#,
        );
        assert!(matches!(ev[0].event, Event::SessionStarted { .. }));
        a
    }

    fn writes(actions: &[Action]) -> Vec<Value> {
        actions
            .iter()
            .flat_map(|a| match a {
                Action::Write(lines) => lines.clone(),
                other => panic!("expected a write, got {other:?}"),
            })
            .map(|l| serde_json::from_str(&l).expect("json line"))
            .collect()
    }

    fn one_write(actions: Vec<Action>) -> Value {
        let mut w = writes(&actions);
        assert_eq!(w.len(), 1, "{w:?}");
        w.remove(0)
    }

    /// Short, comparable name of an envelope.
    fn tag(e: &Envelope) -> String {
        let item = e.item.as_deref().unwrap_or("");
        match &e.event {
            Event::SessionStarted { .. } => "session_started".into(),
            Event::SessionExited { .. } => "session_exited".into(),
            Event::TurnStarted { .. } => "turn_started".into(),
            Event::TurnCompleted { state, .. } => format!("turn_completed:{state:?}"),
            Event::ItemStarted { kind, .. } => format!("item_started:{item}:{kind:?}"),
            Event::ItemCompleted { status, .. } => format!("item_completed:{item}:{status:?}"),
            Event::ContentDelta { stream, .. } => format!("delta:{item}:{stream:?}"),
            Event::ContentSnapshot { stream, .. } => format!("snapshot:{item}:{stream:?}"),
            Event::ApprovalRequested { .. } => format!("approval:{item}"),
            Event::ApprovalExpired => format!("approval_expired:{item}"),
            Event::UsageUpdated { .. } => "usage".into(),
            Event::PlanUpdated { .. } => "plan".into(),
            Event::Unknown => "unknown".into(),
            other => format!("{other:?}"),
        }
    }

    // ---- synthetic replay ----

    /// Replays the fixture: `out` frames are fed; each `in` frame must be exactly what the adapter
    /// wrote at that point (handshake, the queued prompt, the approval answer).
    fn replay(fixture: &str) -> Vec<Envelope> {
        let mut a = CodexAdapter::new().client_version("0.1.0");
        a.argv(&session(Mode::Ask, None));
        let mut wrote: VecDeque<Value> = a
            .handshake()
            .iter()
            .map(|l| serde_json::from_str(l).expect("handshake line"))
            .collect();
        // The user types before the thread exists: queued, flushed by the thread/start response.
        assert!(a
            .encode(Command::Prompt {
                text: "list the files".into()
            })
            .expect("queue")
            .is_empty());
        let mut events = Vec::new();
        let mut last_request: Option<String> = None;
        for line in fixture.lines() {
            let rec: Value = serde_json::from_str(line).expect("fixture line");
            let frame = &rec["frame"];
            match rec["dir"].as_str() {
                Some("out") => {
                    let ev = a.feed(&frame.to_string());
                    if let Some(req) = ev.iter().find_map(|e| {
                        matches!(e.event, Event::ApprovalRequested { .. })
                            .then(|| e.request.clone())
                            .flatten()
                    }) {
                        last_request = Some(req);
                    }
                    events.extend(ev);
                    let outbox = a.drain_outbox();
                    wrote.extend(writes(&outbox.actions));
                    events.extend(outbox.events);
                }
                Some("in") => {
                    if frame.get("method").is_none() {
                        // The app answers the approval.
                        let decision = match frame["result"]["decision"].as_str() {
                            Some("accept") => Decision::Allow,
                            Some("acceptForSession") => Decision::AllowForSession,
                            Some("decline") => Decision::Deny,
                            _ => Decision::Cancel,
                        };
                        let actions = a
                            .encode(Command::Approve {
                                request: last_request.clone().expect("approval seen"),
                                decision,
                                updated_input: None,
                                message: None,
                            })
                            .expect("approve");
                        wrote.extend(writes(&actions));
                    }
                    let expected = wrote.pop_front().expect("adapter wrote nothing");
                    assert_eq!(&expected, frame, "t_ms {}", rec["t_ms"]);
                }
                _ => {}
            }
        }
        assert!(wrote.is_empty(), "unmatched writes: {wrote:?}");
        events
    }

    #[test]
    fn synthetic_turn_maps_to_the_canonical_sequence() {
        let ev = replay(TURN);
        let tags: Vec<String> = ev.iter().map(tag).collect();
        assert_eq!(
            tags,
            [
                "session_started",
                "turn_started",
                "item_started:rs_1:Reasoning",
                "delta:rs_1:Reasoning",
                "delta:rs_1:Reasoning",
                "snapshot:rs_1:Reasoning",
                "item_completed:rs_1:Completed",
                "item_started:msg_1:AssistantMessage",
                "delta:msg_1:Assistant",
                "delta:msg_1:Assistant",
                "snapshot:msg_1:Assistant",
                "item_completed:msg_1:Completed",
                "item_started:cmd_1:Command",
                "approval:cmd_1",
                "delta:cmd_1:ToolOutput",
                "item_completed:cmd_1:Completed",
                "usage",
                "item_started:msg_2:AssistantMessage",
                "delta:msg_2:Assistant",
                "snapshot:msg_2:Assistant",
                "item_completed:msg_2:Completed",
                "turn_completed:Completed",
            ]
        );
        assert!(
            matches!(&ev[0].event, Event::SessionStarted { native_id, model: Some(m), cwd: Some(c) }
            if native_id == "019a0000-0000-7000-8000-000000000001" && m == "gpt-5-codex" && c == "/work/repo")
        );
        assert!(
            matches!(&ev[1].event, Event::TurnStarted { model: Some(m) } if m == "gpt-5-codex")
        );
        // The reasoning summary is the snapshot, not the sum of its deltas' framing.
        assert!(
            matches!(&ev[5].event, Event::ContentSnapshot { text, .. } if text == "**Planning** the listing")
        );
        let approval = &ev[13];
        assert_eq!(approval.request.as_deref(), Some("0"));
        assert!(matches!(&approval.event, Event::ApprovalRequested {
            tool, title: Some(t), reason: Some(r), options, response: ResponseCapability::Live, input
        } if tool == "command_execution" && t == "ls -la" && r.contains("outside")
            && options == &ALL_DECISIONS.to_vec() && input["command"] == "ls -la"));
        assert!(
            matches!(&ev[15].event, Event::ItemCompleted { output: Some(o), error: None, .. }
            if o.starts_with("total 8"))
        );
        assert!(matches!(
            &ev[16].event,
            Event::UsageUpdated {
                used: 1500,
                max: Some(272000),
                ..
            }
        ));
        match &ev[21].event {
            Event::TurnCompleted {
                state: TurnState::Completed,
                usage: Some(u),
                error: None,
                ..
            } => assert_eq!(
                u,
                &Usage {
                    input_tokens: 1200,
                    output_tokens: 300,
                    cached_input_tokens: 1000,
                    reasoning_tokens: 120
                }
            ),
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
        // `raw` rides on the first envelope of a frame only.
        assert!(ev[0].raw.is_some() && ev[1].raw.is_some());
    }

    // ---- argv, handshake, modes ----

    #[test]
    fn argv_is_program_extra_args_then_app_server() {
        let a = CodexAdapter::new();
        assert_eq!(
            a.argv(&session(Mode::Ask, None)),
            [
                "/usr/bin/codex",
                "-c",
                "model_reasoning_effort=high",
                "app-server"
            ]
        );
        assert_eq!(a.driver(), Driver::Codex);
        assert!(a.capabilities().model_switch_in_session);
    }

    #[test]
    fn handshake_is_initialize_initialized_and_thread_start() {
        let mut a = CodexAdapter::new().client_version("3.0.0");
        a.argv(&session(Mode::Ask, None));
        let lines: Vec<Value> = a
            .handshake()
            .iter()
            .map(|l| serde_json::from_str(l).expect("json"))
            .collect();
        assert_eq!(
            lines,
            vec![
                json!({"id": 1, "method": "initialize", "params": {"clientInfo": {
                    "name": "agent-terminal", "title": "Agent Terminal", "version": "3.0.0"}}}),
                json!({"method": "initialized"}),
                json!({"id": 2, "method": "thread/start", "params": {
                    "cwd": "/work/repo",
                    "model": "gpt-5-codex",
                    "approvalsReviewer": "user",
                    "config": {"tools.update_plan.enabled": true}}}),
            ]
        );
        // No frame carries a "jsonrpc" field.
        assert!(lines.iter().all(|l| l.get("jsonrpc").is_none()));
    }

    #[test]
    fn handshake_resumes_when_asked() {
        let mut a = CodexAdapter::new();
        a.argv(&session(Mode::Ask, Some("th-9")));
        let lines = a.handshake();
        let thread: Value = serde_json::from_str(&lines[2]).expect("json");
        assert_eq!(
            thread,
            json!({"id": 2, "method": "thread/resume", "params": {
                "threadId": "th-9",
                "excludeTurns": true,
                "cwd": "/work/repo",
                "model": "gpt-5-codex",
                "approvalsReviewer": "user",
                "config": {"tools.update_plan.enabled": true}}})
        );
    }

    #[test]
    fn a_prompt_before_the_thread_exists_is_queued_then_flushed_in_order() {
        let mut a = CodexAdapter::new();
        a.argv(&session(Mode::Ask, None));
        a.handshake();
        for text in ["one", "two"] {
            assert!(a
                .encode(Command::Prompt { text: text.into() })
                .expect("queue")
                .is_empty());
        }
        assert!(a.drain_outbox().is_empty());
        a.feed(r#"{"id":2,"result":{"thread":{"id":"th-1"},"model":"m"}}"#);
        let flushed = writes(&a.drain_outbox().actions);
        let texts: Vec<&str> = flushed
            .iter()
            .map(|w| w["params"]["input"][0]["text"].as_str().expect("text"))
            .collect();
        assert_eq!(texts, ["one", "two"]);
        assert_eq!(flushed[0]["params"]["threadId"], "th-1");
        assert!(a.drain_outbox().is_empty(), "drained once");
    }

    #[test]
    fn prompt_encodes_turn_start_with_the_mode_policy() {
        let table = [
            (Mode::Ask, "untrusted", json!({"type": "readOnly"})),
            (
                Mode::AcceptEdits,
                "on-request",
                json!({"type": "workspaceWrite"}),
            ),
            (Mode::Plan, "never", json!({"type": "readOnly"})),
        ];
        for (mode, approval, sandbox) in table {
            let mut a = started(mode);
            let w = one_write(a.encode(Command::Prompt { text: "hi".into() }).expect("ok"));
            assert_eq!(
                w,
                json!({"id": 3, "method": "turn/start", "params": {
                    "threadId": "th-1",
                    "input": [{"type": "text", "text": "hi"}],
                    "cwd": "/work/repo",
                    "model": "gpt-5-codex",
                    "approvalPolicy": approval,
                    "approvalsReviewer": "user",
                    "sandboxPolicy": sandbox,
                    "summary": "detailed"}}),
                "{mode:?}"
            );
        }
    }

    #[test]
    fn model_and_mode_apply_on_the_next_turn_and_emit_events() {
        let mut a = started(Mode::Ask);
        assert!(a
            .encode(Command::SetModel {
                model: "gpt-5.5".into(),
                effort: None
            })
            .expect("ok")
            .is_empty());
        assert!(a
            .encode(Command::SetMode {
                mode: Mode::AcceptEdits
            })
            .expect("ok")
            .is_empty());
        let outbox = a.drain_outbox();
        assert!(outbox.actions.is_empty());
        assert_eq!(
            outbox.events,
            vec![
                Envelope::new(Event::ModelChanged {
                    model: "gpt-5.5".into()
                }),
                Envelope::new(Event::ModeChanged {
                    mode: Mode::AcceptEdits
                }),
            ]
        );
        let w = one_write(a.encode(Command::Prompt { text: "x".into() }).expect("ok"));
        assert_eq!(w["params"]["model"], "gpt-5.5");
        assert_eq!(w["params"]["approvalPolicy"], "on-request");
        assert_eq!(
            w["params"]["sandboxPolicy"],
            json!({"type": "workspaceWrite"})
        );
    }

    #[test]
    fn effort_rides_on_every_turn_start() {
        let mut a = started(Mode::Ask);
        let w = one_write(a.encode(Command::Prompt { text: "a".into() }).expect("ok"));
        assert!(w["params"].get("effort").is_none());
        a.encode(Command::SetModel {
            model: "gpt-5-codex".into(),
            effort: Some("high".into()),
        })
        .expect("set");
        let w = one_write(a.encode(Command::Prompt { text: "b".into() }).expect("ok"));
        assert_eq!(w["params"]["effort"], "high");
        // `None` keeps the effort; no respawn is asked for.
        let actions = a
            .encode(Command::SetModel {
                model: "gpt-5.5".into(),
                effort: None,
            })
            .expect("set");
        assert!(actions.is_empty());
        let w = one_write(a.encode(Command::Prompt { text: "c".into() }).expect("ok"));
        assert_eq!(w["params"]["effort"], "high");
    }

    #[test]
    fn a_rate_limit_notification_becomes_a_quota_update() {
        let mut a = started(Mode::Ask);
        let ev = a.feed(
            r#"{"method":"account/rateLimits/updated","params":{"rateLimits":{"primary":{"usedPercent":42,"windowDurationMins":300,"resetsAt":1790000000}}}}"#,
        );
        match &ev[0].event {
            Event::QuotaUpdated { account, windows } => {
                assert!(account.is_none());
                assert_eq!(windows.len(), 1);
                assert_eq!(
                    (windows[0].label.as_str(), windows[0].used),
                    ("5-hour", 0.42)
                );
            }
            other => panic!("{other:?}"),
        }
        // Nothing usable in it: no event, not an Unknown.
        assert!(a
            .feed(r#"{"method":"account/rateLimits/updated","params":{"rateLimits":{}}}"#)
            .is_empty());
    }

    #[test]
    fn the_session_effort_seeds_turn_start() {
        let mut a = CodexAdapter::new();
        let mut s = session(Mode::Ask, None);
        s.effort = Some("low".into());
        a.argv(&s);
        a.handshake();
        a.feed(r#"{"id":2,"result":{"thread":{"id":"th-1"}}}"#);
        let w = one_write(a.encode(Command::Prompt { text: "a".into() }).expect("ok"));
        assert_eq!(w["params"]["effort"], "low");
    }

    // ---- interrupt, approvals, controls ----

    #[test]
    fn interrupt_needs_a_running_turn() {
        let mut a = started(Mode::Ask);
        assert!(a.encode(Command::Interrupt).expect("no-op").is_empty());
        a.feed(r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"tu-1"}}}"#);
        let w = one_write(a.encode(Command::Interrupt).expect("ok"));
        assert_eq!(
            w,
            json!({"id": 3, "method": "turn/interrupt", "params": {"threadId": "th-1", "turnId": "tu-1"}})
        );
        // The process survives an interrupt; the turn ends with status "interrupted".
        let ev = a.feed(
            r#"{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"tu-1","status":"interrupted","error":null}}}"#,
        );
        assert!(matches!(
            ev.last().expect("events").event,
            Event::TurnCompleted {
                state: TurnState::Interrupted,
                error: None,
                ..
            }
        ));
        assert!(a.encode(Command::Interrupt).expect("idle again").is_empty());
    }

    fn approval_request(id: &str, method: &str) -> String {
        format!(
            r#"{{"id":{id},"method":"{method}","params":{{"threadId":"th-1","turnId":"tu-1","itemId":"it-1","startedAtMs":1,"reason":"why","command":"rm x"}}}}"#
        )
    }

    #[test]
    fn approve_answers_the_server_request_with_the_mapped_decision() {
        for (decision, wire) in [
            (Decision::Allow, "accept"),
            (Decision::AllowForSession, "acceptForSession"),
            (Decision::Deny, "decline"),
            (Decision::Cancel, "cancel"),
        ] {
            let mut a = started(Mode::Ask);
            let ev = a.feed(&approval_request(
                "17",
                "item/commandExecution/requestApproval",
            ));
            assert_eq!(ev[0].request.as_deref(), Some("17"));
            let w = one_write(
                a.encode(Command::Approve {
                    request: "17".into(),
                    decision,
                    updated_input: None,
                    message: None,
                })
                .expect("approve"),
            );
            assert_eq!(w, json!({"id": 17, "result": {"decision": wire}}));
            // Answered once: a second answer is invalid, and nothing expires at exit.
            assert!(matches!(
                a.encode(Command::Approve {
                    request: "17".into(),
                    decision,
                    updated_input: None,
                    message: None
                }),
                Err(AdapterError::Invalid(_))
            ));
            assert!(!a
                .on_exit(Some(0))
                .iter()
                .any(|e| matches!(e.event, Event::ApprovalExpired)));
        }
    }

    #[test]
    fn a_string_request_id_is_echoed_verbatim_and_file_changes_are_approvals_too() {
        let mut a = started(Mode::Ask);
        let ev = a.feed(&approval_request(
            "\"req-a\"",
            "item/fileChange/requestApproval",
        ));
        assert!(
            matches!(&ev[0].event, Event::ApprovalRequested { tool, title: Some(t), .. }
            if tool == "file_change" && t == "why")
        );
        assert_eq!(ev[0].request.as_deref(), Some("req-a"));
        let w = one_write(
            a.encode(Command::Approve {
                request: "req-a".into(),
                decision: Decision::Allow,
                updated_input: None,
                message: None,
            })
            .expect("approve"),
        );
        assert_eq!(w, json!({"id": "req-a", "result": {"decision": "accept"}}));
    }

    #[test]
    fn available_decisions_limit_the_options() {
        let mut a = started(Mode::Ask);
        let ev = a.feed(
            r#"{"id":5,"method":"item/commandExecution/requestApproval","params":{"itemId":"i","availableDecisions":["accept",{"acceptWithExecpolicyAmendment":{"execpolicyAmendment":["ls"]}},"cancel"]}}"#,
        );
        assert!(
            matches!(&ev[0].event, Event::ApprovalRequested { options, .. }
            if options == &vec![Decision::Allow, Decision::Cancel])
        );
    }

    #[test]
    fn a_request_resolved_elsewhere_expires_the_approval() {
        let mut a = started(Mode::Ask);
        a.feed(&approval_request(
            "3",
            "item/commandExecution/requestApproval",
        ));
        let ev = a.feed(
            r#"{"method":"serverRequest/resolved","params":{"threadId":"th-1","requestId":3}}"#,
        );
        assert_eq!(ev[0].event, Event::ApprovalExpired);
        assert_eq!(ev[0].request.as_deref(), Some("3"));
        assert_eq!(ev[0].item.as_deref(), Some("it-1"));
    }

    #[test]
    fn requests_the_app_cannot_serve_are_refused_so_the_server_never_waits() {
        let mut a = started(Mode::Ask);
        let ev = a.feed(r#"{"id":9,"method":"item/tool/requestUserInput","params":{}}"#);
        assert!(
            matches!(&ev[0].event, Event::Notice { text } if text.contains("requestUserInput"))
        );
        let outbox = a.drain_outbox();
        let w = one_write(outbox.actions);
        assert_eq!(w["id"], 9);
        assert_eq!(w["error"]["code"], -32601);
    }

    #[test]
    fn list_models_and_context_usage_controls() {
        let mut a = started(Mode::Ask);
        let w = one_write(
            a.encode(Command::Control {
                id: "m1".into(),
                control: Control::ListModels,
            })
            .expect("ok"),
        );
        assert_eq!(
            w,
            json!({"id": 3, "method": "model/list", "params": {"limit": 200}})
        );
        let ev = a.feed(
            r#"{"id":3,"result":{"data":[{"id":"a","model":"gpt-5-codex","displayName":"GPT-5 Codex","description":"d","supportedReasoningEfforts":[{"reasoningEffort":"low","description":"x"},{"reasoningEffort":"high","description":"y"}],"defaultReasoningEffort":"low","hidden":false,"isDefault":true},{"id":"b","model":"old","displayName":"Old","hidden":true,"isDefault":false}],"nextCursor":null}}"#,
        );
        assert_eq!(ev[0].request.as_deref(), Some("m1"));
        assert!(
            matches!(&ev[0].event, Event::ControlResult { ok: Some(v), error: None }
            if v == &json!([{"id": "gpt-5-codex", "display": "GPT-5 Codex", "description": "d",
                "efforts": ["low", "high"], "default_effort": "low", "is_default": true}]))
        );

        // Context usage: unknown first, then from the last tokenUsage.
        let ask = |a: &mut CodexAdapter| {
            assert!(a
                .encode(Command::Control {
                    id: "c".into(),
                    control: Control::ContextUsage
                })
                .expect("ok")
                .is_empty());
            a.drain_outbox().events
        };
        let first = ask(&mut a);
        assert!(matches!(
            &first[0].event,
            Event::ControlResult {
                ok: None,
                error: Some(_)
            }
        ));
        a.feed(
            r#"{"method":"thread/tokenUsage/updated","params":{"threadId":"th-1","turnId":"tu","tokenUsage":{"total":{"totalTokens":9,"inputTokens":5,"cachedInputTokens":0,"outputTokens":4,"reasoningOutputTokens":0},"last":{"totalTokens":7,"inputTokens":4,"cachedInputTokens":0,"outputTokens":3,"reasoningOutputTokens":0},"modelContextWindow":1000}}}"#,
        );
        let second = ask(&mut a);
        assert_eq!(second[0].request.as_deref(), Some("c"));
        assert!(
            matches!(&second[0].event, Event::ControlResult { ok: Some(v), .. }
            if v == &json!({"used": 7, "max": 1000}))
        );

        assert!(matches!(
            a.encode(Command::Control {
                id: "x".into(),
                control: Control::McpStatus
            }),
            Err(AdapterError::Unsupported(_))
        ));
        assert!(matches!(
            a.encode(Command::Answer {
                request: "r".into(),
                answers: Value::Null
            }),
            Err(AdapterError::Unsupported(_))
        ));
    }

    #[test]
    fn compact_prompt_becomes_thread_compact_start() {
        let mut a = started(Mode::Ask);
        let w = one_write(
            a.encode(Command::Prompt {
                text: " /Compact ".into(),
            })
            .expect("ok"),
        );
        assert_eq!(
            w,
            json!({"id": 3, "method": "thread/compact/start", "params": {"threadId": "th-1"}})
        );
        let ev = a.feed(
            r#"{"method":"item/completed","params":{"threadId":"th-1","turnId":"tu","completedAtMs":1,"item":{"type":"contextCompaction","id":"cc1"}}}"#,
        );
        assert!(matches!(
            ev[0].event,
            Event::Compacted {
                manual: true,
                after: None,
                ..
            }
        ));
        // Not yet started: nothing to compact.
        let mut b = CodexAdapter::new();
        assert!(matches!(
            b.encode(Command::Prompt {
                text: "/compact".into()
            }),
            Err(AdapterError::Invalid(_))
        ));
        // A longer text that merely starts with it is a normal prompt.
        let w = one_write(
            a.encode(Command::Prompt {
                text: "/compact please".into(),
            })
            .expect("ok"),
        );
        assert_eq!(w["method"], "turn/start");
    }

    // ---- notifications ----

    #[test]
    fn items_map_to_kinds_titles_and_results() {
        let mut a = started(Mode::Ask);
        let item = |ty: &str, extra: &str| {
            format!(
                r#"{{"method":"item/completed","params":{{"threadId":"th-1","turnId":"tu","completedAtMs":1,"item":{{"type":"{ty}","id":"{ty}-1"{extra}}}}}}}"#
            )
        };
        // Never started: the completion opens the card first.
        let ev = a.feed(&item(
            "commandExecution",
            r#","command":"false","cwd":"/w","status":"failed","exitCode":1,"aggregatedOutput":"nope""#,
        ));
        assert!(
            matches!(&ev[0].event, Event::ItemStarted { kind: ItemKind::Command, title, .. } if title == "false")
        );
        assert!(
            matches!(&ev[1].event, Event::ItemCompleted { status: ItemStatus::Failed, output: Some(o), error: Some(e) }
            if o == "nope" && e == "exit code 1")
        );

        let ev = a.feed(&item(
            "commandExecution",
            r#","command":"rm x","cwd":"/w","status":"declined""#,
        ));
        assert!(matches!(
            ev[1].event,
            Event::ItemCompleted {
                status: ItemStatus::Declined,
                ..
            }
        ));

        let ev = a.feed(&item(
            "fileChange",
            r#","status":"completed","changes":[{"path":"a.rs","kind":{"type":"update"},"diff":"@@"},{"path":"b.rs","kind":{"type":"add"},"diff":"+x"}]"#,
        ));
        assert!(
            matches!(&ev[0].event, Event::ItemStarted { kind: ItemKind::FileChange, title, .. }
            if title == "a.rs (+1 more)")
        );

        let ev = a.feed(&item(
            "mcpToolCall",
            r#","server":"fs","tool":"read","status":"completed","arguments":{"p":1},"result":{"content":[{"type":"text","text":"hello"}]},"error":null"#,
        ));
        assert!(
            matches!(&ev[0].event, Event::ItemStarted { kind: ItemKind::McpTool, title, input: Some(i), .. }
            if title == "fs.read" && i["p"] == 1)
        );
        assert!(
            matches!(&ev[1].event, Event::ItemCompleted { status: ItemStatus::Completed, output: Some(o), .. } if o == "hello")
        );

        let ev = a.feed(&item("webSearch", r#","query":"rust""#));
        assert!(
            matches!(&ev[0].event, Event::ItemStarted { kind: ItemKind::WebSearch, title, .. } if title == "rust")
        );

        let ev = a.feed(&item("somethingNew", ""));
        assert!(
            matches!(&ev[0].event, Event::ItemStarted { kind: ItemKind::Tool, title, .. } if title == "somethingNew")
        );

        // The app shows its own prompt; Codex's echo of it is dropped.
        assert!(a.feed(&item("userMessage", r#","content":[]"#)).is_empty());
    }

    #[test]
    fn reasoning_parts_are_separated_and_the_completed_summary_wins() {
        let mut a = started(Mode::Ask);
        let delta = |idx: u32, text: &str| {
            format!(
                r#"{{"method":"item/reasoning/summaryTextDelta","params":{{"threadId":"th-1","turnId":"tu","itemId":"rs","delta":"{text}","summaryIndex":{idx}}}}}"#
            )
        };
        a.feed(&delta(0, "first"));
        let part = a.feed(
            r#"{"method":"item/reasoning/summaryPartAdded","params":{"threadId":"th-1","turnId":"tu","itemId":"rs","summaryIndex":1}}"#,
        );
        assert!(
            matches!(&part[0].event, Event::ContentDelta { stream: StreamKind::Reasoning, text } if text == "\n\n")
        );
        a.feed(&delta(1, "second"));
        let done = a.feed(
            r#"{"method":"item/completed","params":{"threadId":"th-1","turnId":"tu","completedAtMs":1,"item":{"type":"reasoning","id":"rs","summary":["first","second"],"content":[]}}}"#,
        );
        assert!(
            matches!(&done[0].event, Event::ContentSnapshot { stream: StreamKind::Reasoning, text } if text == "first\n\nsecond")
        );
    }

    #[test]
    fn plan_updates_errors_and_reroutes() {
        let mut a = started(Mode::Ask);
        let ev = a.feed(
            r#"{"method":"turn/plan/updated","params":{"threadId":"th-1","turnId":"tu","explanation":null,"plan":[{"step":"a","status":"completed"},{"step":"b","status":"inProgress"},{"step":"c","status":"pending"}]}}"#,
        );
        assert!(matches!(&ev[0].event, Event::PlanUpdated { steps }
            if steps.iter().map(|s| s.status).collect::<Vec<_>>()
                == [StepStatus::Completed, StepStatus::InProgress, StepStatus::Pending]));
        let ev = a.feed(
            r#"{"method":"error","params":{"error":{"message":"rate limited"},"willRetry":true,"threadId":"th-1","turnId":"tu"}}"#,
        );
        assert!(matches!(&ev[0].event, Event::Notice { text } if text.contains("rate limited")));
        let ev = a.feed(
            r#"{"method":"error","params":{"error":{"message":"boom"},"willRetry":false,"threadId":"th-1","turnId":"tu"}}"#,
        );
        assert!(matches!(&ev[0].event, Event::Error { message } if message == "boom"));
        let ev = a.feed(
            r#"{"method":"model/rerouted","params":{"threadId":"th-1","turnId":"tu","fromModel":"a","toModel":"b","reason":"x"}}"#,
        );
        assert!(matches!(&ev[0].event, Event::ModelChanged { model } if model == "b"));
    }

    #[test]
    fn turn_usage_is_the_delta_of_the_cumulative_total() {
        let mut a = started(Mode::Ask);
        let usage = |total_in: u64, total_out: u64| {
            format!(
                r#"{{"method":"thread/tokenUsage/updated","params":{{"threadId":"th-1","turnId":"tu","tokenUsage":{{"total":{{"totalTokens":0,"inputTokens":{total_in},"cachedInputTokens":0,"outputTokens":{total_out},"reasoningOutputTokens":0}},"last":{{"totalTokens":1,"inputTokens":1,"cachedInputTokens":0,"outputTokens":0,"reasoningOutputTokens":0}},"modelContextWindow":null}}}}}}"#
            )
        };
        let done = r#"{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"tu","status":"completed","error":null}}}"#;
        a.feed(&usage(100, 10));
        let first = a.feed(done);
        assert!(
            matches!(&first[0].event, Event::TurnCompleted { usage: Some(u), .. }
            if u.input_tokens == 100 && u.output_tokens == 10)
        );
        a.feed(&usage(130, 25));
        let second = a.feed(done);
        assert!(
            matches!(&second[0].event, Event::TurnCompleted { usage: Some(u), .. }
            if u.input_tokens == 30 && u.output_tokens == 15)
        );
        // No usage reported in a turn: none claimed.
        let third = a.feed(done);
        assert!(matches!(
            &third[0].event,
            Event::TurnCompleted { usage: None, .. }
        ));
    }

    #[test]
    fn a_failed_turn_carries_its_error_and_cuts_open_items_short() {
        let mut a = started(Mode::Ask);
        a.feed(r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"tu"}}}"#);
        a.feed(
            r#"{"method":"item/agentMessage/delta","params":{"threadId":"th-1","turnId":"tu","itemId":"m","delta":"partial"}}"#,
        );
        let ev = a.feed(
            r#"{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"tu","status":"failed","error":{"message":"quota","codexErrorInfo":null}}}}"#,
        );
        let tags: Vec<String> = ev.iter().map(tag).collect();
        assert_eq!(
            tags,
            [
                "snapshot:m:Assistant",
                "item_completed:m:Interrupted",
                "turn_completed:Failed"
            ]
        );
        assert!(
            matches!(&ev[2].event, Event::TurnCompleted { error: Some(e), .. } if e == "quota")
        );
    }

    #[test]
    fn a_rejected_turn_start_fails_the_turn_the_ui_began() {
        let mut a = started(Mode::Ask);
        a.encode(Command::Prompt { text: "x".into() }).expect("ok");
        let ev = a.feed(r#"{"id":3,"error":{"code":-32600,"message":"no such model"}}"#);
        assert!(matches!(&ev[0].event, Event::Error { message } if message == "no such model"));
        assert!(matches!(
            &ev[1].event,
            Event::TurnCompleted {
                state: TurnState::Failed,
                ..
            }
        ));
    }

    // ---- exit and bad input ----

    #[test]
    fn on_exit_closes_items_expires_approvals_and_ends_the_turn() {
        let mut a = started(Mode::Ask);
        a.feed(r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"tu"}}}"#);
        a.feed(
            r#"{"method":"item/started","params":{"threadId":"th-1","turnId":"tu","startedAtMs":1,"item":{"type":"commandExecution","id":"c1","command":"sleep 9","cwd":"/w","status":"inProgress","commandActions":[]}}}"#,
        );
        a.feed(&approval_request(
            "4",
            "item/commandExecution/requestApproval",
        ));
        a.encode(Command::Control {
            id: "m".into(),
            control: Control::ListModels,
        })
        .expect("ok");
        let ev = a.on_exit(Some(1));
        let tags: Vec<String> = ev.iter().map(tag).collect();
        assert_eq!(
            tags,
            [
                "item_completed:c1:Interrupted",
                "approval_expired:it-1",
                "ControlResult { ok: None, error: Some(\"agent exited\") }",
                "turn_completed:Failed",
                "session_exited"
            ]
        );
        assert!(
            matches!(&ev[3].event, Event::TurnCompleted { error: Some(e), .. } if e.contains("unexpectedly"))
        );
        assert!(matches!(
            ev[4].event,
            Event::SessionExited {
                code: Some(1),
                expected: false
            }
        ));

        // After an interrupt the exit is expected and the turn is Interrupted.
        let mut a = started(Mode::Ask);
        a.feed(r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"tu"}}}"#);
        a.encode(Command::Interrupt).expect("ok");
        let ev = a.on_exit(Some(130));
        assert!(matches!(
            ev[0].event,
            Event::TurnCompleted {
                state: TurnState::Interrupted,
                error: None,
                ..
            }
        ));
        assert!(matches!(
            ev[1].event,
            Event::SessionExited { expected: true, .. }
        ));
    }

    #[test]
    fn bad_input_is_unknown_never_a_panic() {
        let mut a = CodexAdapter::new();
        let ev = a.feed("not json");
        assert_eq!(ev[0].event, Event::Unknown);
        assert_eq!(ev[0].raw, Some(Value::String("not json".into())));
        assert_eq!(a.feed("{}")[0].event, Event::Unknown);
        assert_eq!(
            a.feed(r#"{"method":"from/thefuture","params":{}}"#)[0].event,
            Event::Unknown
        );
        assert!(a.feed(r#"{"method":"from/thefuture","params":{}}"#)[0]
            .raw
            .is_some());
        // A response nobody asked for.
        assert_eq!(a.feed(r#"{"id":99,"result":{}}"#)[0].event, Event::Unknown);
        // Bookkeeping notifications are quiet.
        assert!(a
            .feed(r#"{"method":"thread/started","params":{}}"#)
            .is_empty());
        assert!(a.feed("  ").is_empty());
        assert!(a.feed_stderr("INFO noise").is_empty());
        assert!(a.feed_side("x", "", true).is_empty());
        // Malformed params of a known method.
        assert_eq!(
            a.feed(r#"{"method":"item/started","params":{}}"#)[0].event,
            Event::Unknown
        );
    }

    #[test]
    fn capabilities_match_the_protocol() {
        let c = Capabilities::codex();
        assert!(c.model_switch_in_session && c.live_approvals && c.interrupt_keeps_process);
        assert!(c.streams_text && c.streams_reasoning && c.plan_mode);
        assert!(c.model_list && c.context_usage);
        assert!(
            !c.questions && !c.file_suggestions && !c.mcp_panel && !c.settings_panel && !c.usage
        );
        assert_eq!(c.compact_command.as_deref(), Some("/compact"));
    }
}
