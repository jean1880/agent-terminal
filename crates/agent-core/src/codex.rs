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
//! - `item/permissions/requestApproval`, `mcpServer/elicitation/request` and `item/tool/call` are
//!   answered with a JSON-RPC error and a [`Event::Notice`]; map them once the UI has a place for
//!   them. `item/tool/requestUserInput` is mapped to the native question card.
//! - Approval decisions that carry a payload (`acceptWithExecpolicyAmendment`,
//!   `applyNetworkPolicyAmendment`) are not offered; the four plain decisions are.
//! - stderr (Codex's tracing output) is ignored.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Mutex, MutexGuard};

use serde_json::{json, Map, Value};

use crate::adapter::{
    Action, Adapter, AdapterError, Command, Control, Driver, Mode, OpenSession, Outbox,
};
use crate::caps::Capabilities;
use crate::event::{
    Decision, Envelope, Event, ItemKind, ItemStatus, PlanStep, Question, QuestionOption,
    ResponseCapability, StepStatus, StreamKind, TurnState, Usage, WorkerSnapshot, WorkerState,
};

/// Page size asked of `model/list`.
const MODEL_PAGE: u32 = 200;

// Bound retired-turn history on long-lived connections; settled active-turn requests retire
// with their scope, so delayed frames do not require keeping the entire conversation here.
const TURN_TOMBSTONE_LIMIT: usize = 128;

/// Notifications that carry nothing the UI renders (bookkeeping, or covered by another frame):
/// dropped without an [`Event::Unknown`], which would only add noise to every turn.
const QUIET: &[&str] = &[
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
    TurnStart(SubmittedModel),
    Compact,
    Interrupt,
    /// `model/list`, answering the control with this id.
    Models(String),
}

/// The selection actually written to one `turn/start`, independent of later picker changes.
#[derive(Debug, Clone)]
struct SubmittedModel {
    model: Option<String>,
    effort: Option<String>,
    revision: u64,
}

impl SubmittedModel {
    fn selection_key(&self) -> Option<String> {
        (self.revision != 0).then(|| format!("codex-model-{}", self.revision))
    }
}

/// An unanswered approval server request.
#[derive(Debug, Clone)]
struct PendingApproval {
    /// The JSON-RPC id to put in the response, verbatim (number or string).
    rpc_id: Value,
    item: String,
    scope: RequestScope,
    /// The decisions the server offered (`availableDecisions`, or all of ours when it named
    /// none we know). A decision outside this is refused rather than sent.
    options: Vec<Decision>,
}

/// An unanswered `request_user_input` server request.
#[derive(Debug, Clone)]
struct PendingQuestion {
    /// The JSON-RPC id to put in the response, verbatim (number or string).
    rpc_id: Value,
    item: String,
    scope: RequestScope,
    /// The app-server question ids, paired with the question text used by the shared card UI.
    keys: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RequestScope {
    thread: Option<String>,
    turn: Option<String>,
}

pub struct CodexAdapter {
    caps: Capabilities,
    client_version: String,
    /// The session given to [`Adapter::argv`].
    session: Mutex<Option<OpenSession>>,
    next_id: u64,
    pending: HashMap<u64, Pending>,
    startup_failed: bool,
    thread_id: Option<String>,
    turn_id: Option<String>,
    turn_open: bool,
    /// An interrupt was sent for the open turn, so an exit before it ends is expected.
    interrupt_sent: bool,
    model: Option<String>,
    /// An explicit launch model or later choice must survive the initial thread response.
    /// That response reports startup state separately from the selected turn default.
    model_selected_during_start: bool,
    selection_revision: u64,
    confirmed_revision: u64,
    accepted_revision: Option<u64>,
    confirmed_model: Option<String>,
    confirmed_effort: Option<String>,
    effective_selection: Option<SubmittedModel>,
    /// Reasoning effort sent on every `turn/start` (`low`/`medium`/`high`/...; a model-advertised
    /// string). Seeded from `OpenSession::effort`, changed by `Command::SetModel { effort }`
    /// (per turn: no respawn).
    effort: Option<String>,
    mode: Mode,
    /// Policy sent for the active or pending turn. Selecting a new default cannot mutate it.
    effective_mode: Option<Mode>,
    cwd: Option<String>,
    /// Prompts encoded before the thread id arrived.
    queued: VecDeque<String>,
    outbox: Outbox,
    approvals: HashMap<String, PendingApproval>,
    questions: HashMap<String, PendingQuestion>,
    /// Keep every settled identity until its turn ends, then retire that scope. Retention
    /// grows with active work, not connection lifetime; evicting an active identity would
    /// make an old answer valid again. Child scopes retire only with child terminal evidence.
    settled_requests: HashMap<String, RequestScope>,
    ended_turns: HashSet<RequestScope>,
    ended_turn_order: VecDeque<RequestScope>,
    /// Ids of items started and not yet completed, in start order.
    open_items: Vec<String>,
    /// Accumulated streamed text per open message item (snapshot fallback).
    texts: HashMap<String, String>,
    /// Accumulated reasoning summary per open reasoning item.
    reasoning: HashMap<String, String>,
    /// Worker identity is a thread id, independent of collaboration operation/item ids.
    workers: BTreeMap<String, WorkerSnapshot>,
    /// Current child turn ids: unscoped child requests must never borrow the parent's id.
    worker_turns: HashMap<String, String>,
    /// Open explicit wait operations, keyed by item id (several can target one worker).
    worker_waits: BTreeMap<String, Vec<String>>,
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
            startup_failed: false,
            thread_id: None,
            turn_id: None,
            turn_open: false,
            interrupt_sent: false,
            model: None,
            model_selected_during_start: false,
            selection_revision: 0,
            confirmed_revision: 0,
            accepted_revision: None,
            confirmed_model: None,
            confirmed_effort: None,
            effective_selection: None,
            effort: None,
            mode: Mode::default(),
            effective_mode: None,
            cwd: None,
            queued: VecDeque::new(),
            outbox: Outbox::default(),
            approvals: HashMap::new(),
            questions: HashMap::new(),
            settled_requests: HashMap::new(),
            ended_turns: HashSet::new(),
            ended_turn_order: VecDeque::new(),
            open_items: Vec::new(),
            texts: HashMap::new(),
            reasoning: HashMap::new(),
            workers: BTreeMap::new(),
            worker_turns: HashMap::new(),
            worker_waits: BTreeMap::new(),
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

    fn worker(&mut self, id: &str) -> &mut WorkerSnapshot {
        self.workers
            .entry(id.to_owned())
            .or_insert_with(|| WorkerSnapshot {
                id: id.to_owned(),
                name: None,
                task: None,
                state: WorkerState::Unknown,
                activity: None,
            })
    }

    fn waiting_for(&self) -> Vec<String> {
        let mut ids: Vec<_> = self.worker_waits.values().flatten().cloned().collect();
        ids.sort();
        ids.dedup();
        ids
    }

    fn emit_workers(&self, out: &mut Vec<Envelope>) {
        out.push(Envelope::new(Event::WorkersUpdated {
            workers: self.workers.values().cloned().collect(),
            waiting_for: self.waiting_for(),
        }));
    }

    fn clear_worker_waits(&mut self, out: &mut Vec<Envelope>) {
        if !self.worker_waits.is_empty() {
            self.worker_waits.clear();
            self.emit_workers(out);
        }
    }

    fn finish_effective_mode(&mut self, out: &mut Vec<Envelope>) {
        self.effective_selection = None;
        if self
            .effective_mode
            .take()
            .is_some_and(|mode| mode != self.mode)
        {
            out.push(Envelope::new(Event::ModeChanged { mode: self.mode }));
        }
    }

    fn confirm_submitted_model(&mut self, submitted: &SubmittedModel, out: &mut Vec<Envelope>) {
        if submitted.revision < self.confirmed_revision
            || self.accepted_revision == Some(submitted.revision)
        {
            return;
        }
        self.accepted_revision = Some(submitted.revision);
        if submitted.revision == self.confirmed_revision
            && submitted.model == self.confirmed_model
            && submitted.effort == self.confirmed_effort
        {
            return;
        }
        self.confirmed_revision = submitted.revision;
        self.confirmed_model = submitted.model.clone();
        self.confirmed_effort = submitted.effort.clone();
        if let Some(model) = &submitted.model {
            let mut event = Envelope::new(Event::ModelChanged {
                model: model.clone(),
            });
            event.request = submitted.selection_key();
            out.push(event);
        }
    }

    fn selection_is_unconfirmed(&self, submitted: &SubmittedModel) -> bool {
        submitted.revision >= self.confirmed_revision
            && self.accepted_revision != Some(submitted.revision)
            && (submitted.revision > self.confirmed_revision
                || submitted.model != self.confirmed_model
                || submitted.effort != self.confirmed_effort)
    }

    fn fail_startup(&mut self, message: String, out: &mut Vec<Envelope>) {
        if self.startup_failed {
            return;
        }
        self.startup_failed = true;
        let submitted =
            !self.queued.is_empty() || self.effective_selection.is_some() || self.turn_open;
        self.queued.clear();
        self.outbox.actions.clear();
        self.thread_id = None;
        self.turn_id = None;
        self.turn_open = false;
        if let Some(model) = &self.model {
            let mut event = Envelope::new(Event::ModelChangeFailed {
                model: model.clone(),
                message: message.clone(),
            });
            if self.selection_revision != 0 {
                event.request = Some(format!("codex-model-{}", self.selection_revision));
            }
            out.push(event);
        }
        out.push(Envelope::new(Event::Error {
            message: message.clone(),
        }));
        if submitted {
            out.push(Envelope::new(Event::TurnCompleted {
                state: TurnState::Failed,
                usage: None,
                cost_usd: None,
                error: Some(message),
            }));
        }
        self.finish_effective_mode(out);
    }

    /// Verified against the installed app-server schema: operation statuses and agent statuses
    /// are different enums. Preserve worker ids/statuses even when no nickname was reported.
    fn track_collaboration(&mut self, item: &Value, completed: bool, out: &mut Vec<Envelope>) {
        if s(item, "type") != Some("collabAgentToolCall") {
            return;
        }
        let before = (self.workers.clone(), self.worker_waits.clone());
        let tool = s(item, "tool").unwrap_or_default();
        let receivers: Vec<String> = strings(item, "receiverThreadIds")
            .into_iter()
            .filter(|id| !id.is_empty())
            .collect();
        let prompt = s(item, "prompt").filter(|p| !p.trim().is_empty());
        for id in &receivers {
            let worker = self.worker(id);
            // A message is not necessarily a new task. These operations actually assign work.
            if matches!(tool, "spawnAgent" | "sendInput" | "followupTask") {
                if let Some(prompt) = prompt {
                    if worker.task.as_deref() != Some(prompt) {
                        worker.activity = None;
                    }
                    worker.task = Some(prompt.to_owned());
                }
            }
        }
        if let Some(states) = item.get("agentsStates").and_then(Value::as_object) {
            for (id, state) in states {
                if id.is_empty() {
                    continue;
                }
                let worker = self.worker(id);
                set_worker_state(worker, collab_worker_state(s(state, "status")));
                if let Some(message) = s(state, "message").filter(|m| !m.trim().is_empty()) {
                    worker.activity = Some(message.to_owned());
                }
            }
        }
        if let Some(id) = s(item, "id") {
            if tool == "wait" && !completed && s(item, "status") == Some("inProgress") {
                self.worker_waits.insert(id.to_owned(), receivers);
            } else {
                self.worker_waits.remove(id);
            }
        }
        if before != (self.workers.clone(), self.worker_waits.clone()) {
            self.emit_workers(out);
        }
    }

    /// Child notifications cannot change the main thread's turn id, reasoning or quota. Scope
    /// child metadata by a verified parent id (including nested workers) or a known receiver.
    fn worker_notification(
        &mut self,
        method: &str,
        params: &Value,
        out: &mut Vec<Envelope>,
    ) -> bool {
        if method == "thread/started" {
            let thread = params.get("thread").unwrap_or(&Value::Null);
            let Some(id) = s(thread, "id") else {
                return true;
            };
            let spawn = thread
                .get("source")
                .and_then(|s| s.get("subAgent"))
                .and_then(|s| s.get("thread_spawn"));
            let parent = s(thread, "parentThreadId").or_else(|| {
                spawn
                    .and_then(|s| s.get("parent_thread_id"))
                    .and_then(Value::as_str)
            });
            let ours = self.workers.contains_key(id)
                || parent.is_some_and(|p| {
                    self.thread_id.as_deref() == Some(p) || self.workers.contains_key(p)
                });
            if ours {
                let before = self.workers.clone();
                let worker = self.worker(id);
                let nickname = s(thread, "agentNickname").or_else(|| {
                    spawn
                        .and_then(|s| s.get("agent_nickname"))
                        .and_then(Value::as_str)
                });
                let role = s(thread, "agentRole").or_else(|| {
                    spawn
                        .and_then(|s| s.get("agent_role"))
                        .and_then(Value::as_str)
                });
                if let Some(name) = nickname
                    .filter(|n| !n.trim().is_empty())
                    .or_else(|| role.filter(|r| worker.name.is_none() && !r.trim().is_empty()))
                {
                    worker.name = Some(name.to_owned());
                }
                set_worker_state(worker, thread_worker_state(thread.get("status")));
                if before != self.workers {
                    self.emit_workers(out);
                }
            }
            return true;
        }
        let Some(id) = s(params, "threadId") else {
            return false;
        };
        if self.thread_id.as_deref() == Some(id) {
            if method == "thread/closed" {
                self.clear_worker_waits(out);
                return true;
            }
            return method == "thread/status/changed";
        }
        if !self.workers.contains_key(id) {
            // Notifications about unrelated threads share this transport on newer servers.
            return self.thread_id.is_some();
        }
        let before = (self.workers.clone(), self.worker_waits.clone());
        match method {
            "thread/status/changed" => {
                set_worker_state(self.worker(id), thread_worker_state(params.get("status")));
            }
            "thread/closed" => {
                self.retire_thread_requests(id, out);
                self.worker_turns.remove(id);
                self.worker(id).state = WorkerState::Closed;
            }
            "turn/started" => {
                let scope = self.notification_turn_scope(params);
                if self.ended_turns.contains(&scope) {
                    return true;
                }
                if let Some(turn) = scope.turn {
                    self.worker_turns.insert(id.to_owned(), turn);
                }
                self.worker(id).state = WorkerState::Running;
            }
            "turn/completed" => {
                let turn = params.get("turn").unwrap_or(&Value::Null);
                let scope = self.notification_turn_scope(params);
                if self.ended_turns.contains(&scope) {
                    return true;
                }
                if self
                    .worker_turns
                    .get(id)
                    .zip(scope.turn.as_ref())
                    .is_some_and(|(active, ended)| active != ended)
                {
                    return true;
                }
                self.retire_turn_requests(scope, out);
                self.worker_turns.remove(id);
                let state = match s(turn, "status") {
                    Some("completed") => WorkerState::Completed,
                    Some("interrupted") => WorkerState::Stopped,
                    Some("failed") => WorkerState::Failed,
                    _ => WorkerState::Unknown,
                };
                set_worker_state(self.worker(id), state);
            }
            "item/started" | "item/completed" => {
                let item = params.get("item").unwrap_or(&Value::Null);
                self.track_collaboration(item, method == "item/completed", out);
                let (kind, title, _) = item_info(item);
                if !matches!(kind, ItemKind::AssistantMessage | ItemKind::Reasoning) {
                    self.worker(id).activity = Some(title);
                }
            }
            _ => {}
        }
        if before != (self.workers.clone(), self.worker_waits.clone()) {
            self.emit_workers(out);
        }
        true
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
        // Capture before writing: the user can change the picker before turn/started arrives.
        if self.effective_mode.is_none() {
            self.effective_mode = Some(self.mode);
        }
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
        let submitted = SubmittedModel {
            model: self.model.clone(),
            effort: self.effort.clone(),
            revision: self.selection_revision,
        };
        if self.effective_selection.is_none() {
            self.effective_selection = Some(submitted.clone());
        }
        self.request(
            Pending::TurnStart(submitted),
            "turn/start",
            Value::Object(params),
        )
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
        self.track_collaboration(item, false, out);
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
        self.track_collaboration(item, true, out);
        let (kind, title, input) = item_info(item);
        if matches!(kind, ItemKind::WebSearch | ItemKind::FileChange)
            && self.open_items.iter().any(|open| open == id)
        {
            // Completed metadata is authoritative: file changes can start with no patch.
            // The reducer refines a matching ItemStarted without creating another card.
            out.push(
                Envelope::new(Event::ItemStarted {
                    kind,
                    title,
                    input,
                    parent: None,
                })
                .item(id),
            );
        } else {
            self.start_item(out, id, kind, title, input);
        }
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
        if self.startup_failed {
            return;
        }
        if self.worker_notification(method, params, out) {
            return;
        }
        match method {
            "turn/started" => {
                let scope = self.notification_turn_scope(params);
                if self.ended_turns.contains(&scope) {
                    return;
                }
                self.clear_worker_waits(out);
                if self.effective_mode.is_none() {
                    self.effective_mode = Some(self.mode);
                }
                self.turn_open = true;
                self.interrupt_sent = false;
                if let Some(turn) = s(params.get("turn").unwrap_or(&Value::Null), "id") {
                    self.turn_id = Some(turn.to_owned());
                }
                let submitted = self.effective_selection.clone();
                if let Some(selection) = &submitted {
                    self.confirm_submitted_model(selection, out);
                }
                out.push(Envelope::new(Event::TurnStarted {
                    model: submitted.map_or_else(|| self.confirmed_model.clone(), |s| s.model),
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
                if self.stale_explicit_turn(params) {
                    return;
                }
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
                let approval = self
                    .approvals
                    .iter()
                    .find(|(_, pending)| Some(&pending.rpc_id) == params.get("requestId"))
                    .map(|(key, _)| key.clone());
                if let Some((key, p)) = approval
                    .and_then(|key| self.approvals.remove(&key).map(|pending| (key, pending)))
                {
                    self.remember_settled_request(key.clone(), p.scope.clone());
                    out.push(
                        Envelope::new(Event::ApprovalExpired)
                            .item(p.item)
                            .request(key),
                    );
                }
                let question = self
                    .questions
                    .iter()
                    .find(|(_, pending)| Some(&pending.rpc_id) == params.get("requestId"))
                    .map(|(key, _)| key.clone());
                if let Some((key, p)) = question
                    .and_then(|key| self.questions.remove(&key).map(|pending| (key, pending)))
                {
                    self.remember_settled_request(key.clone(), p.scope.clone());
                    out.push(
                        Envelope::new(Event::QuestionResolved { answered: false })
                            .item(p.item)
                            .request(key),
                    );
                }
            }
            "model/rerouted" => match s(params, "toModel") {
                Some(model) => {
                    let selection_key = self
                        .effective_selection
                        .as_ref()
                        .and_then(SubmittedModel::selection_key);
                    self.confirmed_model = Some(model.to_owned());
                    if let Some(selection) = &mut self.effective_selection {
                        self.confirmed_revision = self.confirmed_revision.max(selection.revision);
                        self.accepted_revision = Some(selection.revision);
                        self.confirmed_effort = selection.effort.clone();
                        if selection.revision == self.selection_revision {
                            self.model = Some(model.to_owned());
                        }
                        selection.model = Some(model.to_owned());
                    } else {
                        self.model = Some(model.to_owned());
                    }
                    let mut event = Envelope::new(Event::ModelChanged {
                        model: model.to_owned(),
                    });
                    event.request = selection_key;
                    out.push(event);
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
                    self.clear_worker_waits(out);
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
        let scope = self.notification_turn_scope(params);
        if self.ended_turns.contains(&scope) {
            return; // also protects the gap before the next turn/started supplies its id
        }
        if self
            .turn_id
            .as_deref()
            .zip(scope.turn.as_deref())
            .is_some_and(|(active, ended)| active != ended)
        {
            return; // a late terminal notification cannot close the active turn
        }
        self.retire_turn_requests(scope, out);
        self.clear_worker_waits(out);
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
        self.finish_effective_mode(out);
    }

    /// Expire only this turn's unanswered requests, including requests with no item/started.
    fn retire_turn_requests(&mut self, scope: RequestScope, out: &mut Vec<Envelope>) {
        remember_bounded(
            &mut self.ended_turns,
            &mut self.ended_turn_order,
            scope.clone(),
            TURN_TOMBSTONE_LIMIT,
        );
        let mut approvals: Vec<String> = self
            .approvals
            .iter()
            .filter(|(_, p)| p.scope == scope)
            .map(|(key, _)| key.clone())
            .collect();
        approvals.sort();
        for key in approvals {
            if let Some(pending) = self.approvals.remove(&key) {
                out.push(
                    Envelope::new(Event::ApprovalExpired)
                        .item(pending.item)
                        .request(key),
                );
            }
        }
        let mut questions: Vec<String> = self
            .questions
            .iter()
            .filter(|(_, p)| p.scope == scope)
            .map(|(key, _)| key.clone())
            .collect();
        questions.sort();
        for key in questions {
            if let Some(pending) = self.questions.remove(&key) {
                out.push(
                    Envelope::new(Event::QuestionResolved { answered: false })
                        .item(pending.item)
                        .request(key),
                );
            }
        }
        self.settled_requests.retain(|_, settled| *settled != scope);
    }

    fn retire_thread_requests(&mut self, thread: &str, out: &mut Vec<Envelope>) {
        let scopes: HashSet<RequestScope> = self
            .approvals
            .values()
            .map(|p| p.scope.clone())
            .chain(self.questions.values().map(|p| p.scope.clone()))
            .chain(self.settled_requests.values().cloned())
            .filter(|scope| scope.thread.as_deref() == Some(thread))
            .collect();
        let mut scopes: Vec<_> = scopes.into_iter().collect();
        scopes.sort_by(|a, b| a.turn.cmp(&b.turn));
        for scope in scopes {
            self.retire_turn_requests(scope, out);
        }
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

    fn remember_settled_request(&mut self, key: String, scope: RequestScope) {
        self.settled_requests.insert(key, scope);
    }

    fn request_scope(&self, params: &Value) -> RequestScope {
        let thread = s(params, "threadId").or(self.thread_id.as_deref());
        let turn = s(params, "turnId").or_else(|| {
            if thread == self.thread_id.as_deref() {
                self.turn_id.as_deref()
            } else {
                thread.and_then(|thread| self.worker_turns.get(thread).map(String::as_str))
            }
        });
        RequestScope {
            thread: thread.map(str::to_owned),
            turn: turn.map(str::to_owned),
        }
    }

    fn notification_turn_scope(&self, params: &Value) -> RequestScope {
        let mut scope = self.request_scope(params);
        scope.turn = s(params.get("turn").unwrap_or(&Value::Null), "id")
            .map(str::to_owned)
            .or(scope.turn);
        scope
    }

    fn stale_explicit_turn(&self, params: &Value) -> bool {
        let scope = self.request_scope(params);
        let active = if scope.thread == self.thread_id {
            self.turn_id.as_deref()
        } else {
            scope
                .thread
                .as_deref()
                .and_then(|thread| self.worker_turns.get(thread).map(String::as_str))
        };
        self.ended_turns.contains(&scope)
            || scope.thread.as_deref().is_some_and(|thread| {
                self.workers
                    .get(thread)
                    .is_some_and(|worker| worker.state == WorkerState::Closed)
            })
            || s(params, "turnId")
                .zip(active)
                .is_some_and(|(requested, active)| requested != active)
    }

    /// RPC ids belong to one app-server connection and restart at zero. The transcript
    /// persists across connections, so its request identity must also name the native
    /// turn/item. Keep the original typed RPC id separately for responses and resolution.
    fn server_request_key(&self, id: &Value, method: &str, params: &Value) -> String {
        let scope = self.request_scope(params);
        format!(
            "codex:{method}:{}",
            json!([scope.thread, scope.turn, s(params, "itemId"), id])
        )
    }

    fn on_server_request(
        &mut self,
        id: &Value,
        method: &str,
        params: &Value,
        out: &mut Vec<Envelope>,
    ) {
        if self.stale_explicit_turn(params) {
            return; // an unseen old request must not create a live interruption
        }
        if method == "item/tool/requestUserInput" {
            let key = self.server_request_key(id, method, params);
            if self.settled_requests.contains_key(&key) {
                return;
            }
            let item = s(params, "itemId").unwrap_or_default().to_owned();
            let questions: Vec<Question> = params
                .get("questions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|question| Question {
                    id: s(question, "id").unwrap_or_default().to_owned(),
                    header: s(question, "header").unwrap_or_default().to_owned(),
                    question: s(question, "question").unwrap_or_default().to_owned(),
                    options: question
                        .get("options")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|option| {
                            let label = s(option, "label")?.to_owned();
                            if label.trim().is_empty() {
                                return None;
                            }
                            Some(QuestionOption {
                                label,
                                description: s(option, "description").map(str::to_owned),
                            })
                        })
                        .collect(),
                    multi_select: false,
                })
                .filter(|question| !question.id.is_empty() && !question.question.is_empty())
                .collect();
            if questions.is_empty() || questions.iter().any(|q| q.options.is_empty()) {
                self.write(
                    json!({"id": id, "error": {
                        "code": -32602,
                        "message": "requestUserInput must contain usable questions with selectable options; free-text-only questions are not supported",
                    }})
                    .to_string(),
                );
                out.push(Envelope::new(Event::Notice {
                    text: "Codex requested user input without usable questions and selectable options; it was refused because free-text-only questions cannot be answered here."
                        .to_owned(),
                }));
                return;
            }
            self.questions.insert(
                key.clone(),
                PendingQuestion {
                    rpc_id: id.clone(),
                    item: item.clone(),
                    scope: self.request_scope(params),
                    keys: questions
                        .iter()
                        .map(|question| (question.id.clone(), question.question.clone()))
                        .collect(),
                },
            );
            out.push(
                Envelope::new(Event::QuestionRequested { questions })
                    .item(item)
                    .request(key),
            );
            return;
        }
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
        let key = self.server_request_key(id, method, params);
        if self.settled_requests.contains_key(&key) {
            return;
        }
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
                scope: self.request_scope(params),
                options: options.clone(),
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
                // Codex has no persistent allow.
                remembers: None,
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
        if self.startup_failed
            && matches!(
                pending,
                Pending::Initialize | Pending::ThreadStart | Pending::TurnStart(_)
            )
        {
            return;
        }
        let result = frame.get("result");
        let error = frame
            .get("error")
            .map(|e| s(e, "message").map_or_else(|| e.to_string(), str::to_owned));
        match (pending, result, error) {
            (Pending::Initialize, Some(_), _) => {}
            (Pending::ThreadStart, Some(result), _) => self.on_thread_ready(result, out),
            (Pending::TurnStart(submitted), Some(result), _) => {
                self.confirm_submitted_model(&submitted, out);
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
            (Pending::TurnStart(submitted), None, error) => {
                let message = error.unwrap_or_else(|| "turn/start failed".to_owned());
                if self.selection_is_unconfirmed(&submitted) {
                    if let Some(model) = &submitted.model {
                        let mut event = Envelope::new(Event::ModelChangeFailed {
                            model: model.clone(),
                            message: message.clone(),
                        });
                        event.request = submitted.selection_key();
                        out.push(event);
                    }
                    if submitted.revision == self.selection_revision {
                        self.model = self.confirmed_model.clone();
                        self.effort = self.confirmed_effort.clone();
                    }
                }
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
                    self.finish_effective_mode(out);
                }
            }
            (Pending::Initialize | Pending::ThreadStart, None, error) => {
                self.fail_startup(
                    error.unwrap_or_else(|| "Codex thread startup failed".to_owned()),
                    out,
                );
            }
            (Pending::Compact, None, error) => {
                out.push(Envelope::new(Event::Error {
                    message: error.unwrap_or_else(|| "request failed".to_owned()),
                }));
            }
            // An interrupt with no turn to stop, or an acknowledged compact: nothing to show.
            (Pending::Interrupt | Pending::Compact, _, _) => {}
        }
    }

    fn on_thread_ready(&mut self, result: &Value, out: &mut Vec<Envelope>) {
        if self.startup_failed {
            return;
        }
        let Some(thread) = result
            .get("thread")
            .and_then(|t| s(t, "id"))
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
        else {
            self.fail_startup("thread/start returned no thread id".to_owned(), out);
            return;
        };
        self.thread_id = Some(thread.clone());
        let startup_model = s(result, "model").map(str::to_owned);
        self.confirmed_model = startup_model.clone();
        let startup_effort = self.session().as_ref().and_then(|s| s.effort.clone());
        self.confirmed_effort = startup_effort;
        if !self.model_selected_during_start {
            if let Some(model) = &startup_model {
                self.model = Some(model.clone());
            }
        }
        self.model_selected_during_start = false;
        out.push(Envelope::new(Event::SessionStarted {
            native_id: thread.clone(),
            model: startup_model,
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
        self.startup_failed = false;
        self.thread_id = None;
        self.model_selected_during_start = false;
        self.selection_revision = 0;
        self.confirmed_revision = 0;
        self.accepted_revision = None;
        self.confirmed_model = None;
        self.confirmed_effort = None;
        self.effective_selection = None;
        self.turn_id = None;
        self.turn_open = false;
        self.effective_mode = None;
        self.interrupt_sent = false;
        self.approvals.clear();
        self.questions.clear();
        self.settled_requests.clear();
        self.ended_turns.clear();
        self.ended_turn_order.clear();
        self.open_items.clear();
        self.texts.clear();
        self.reasoning.clear();
        // Reconnecting the same thread forgets live certainty, not its worker history. A newly
        // constructed adapter knows only workers observed on this connection; replaying UI
        // consumers preserve previously stored identities absent from these snapshots.
        for worker in self.workers.values_mut() {
            if matches!(
                worker.state,
                WorkerState::Starting | WorkerState::Running | WorkerState::Waiting
            ) {
                worker.state = WorkerState::Unknown;
            }
        }
        self.worker_waits.clear();
        self.worker_turns.clear();
        self.usage_total = None;
        self.usage_base = Usage::default();
        self.context = None;
        self.compact_pending = false;
        self.outbox = Outbox::default();
        if let Some(s) = &session {
            self.model = s.model.clone();
            self.model_selected_during_start = s.model.is_some();
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

    fn prompt_starts_turn(&self, text: &str) -> bool {
        !text.trim().eq_ignore_ascii_case("/compact")
    }

    fn discard_queued_prompts(&mut self) {
        self.queued.clear();
    }

    fn encode(&mut self, command: Command) -> Result<Vec<Action>, AdapterError> {
        if self.startup_failed
            && matches!(&command, Command::Prompt { .. } | Command::SetModel { .. })
        {
            return Err(AdapterError::Invalid(
                "Codex thread startup failed; restart the session before sending another prompt or model choice".to_owned(),
            ));
        }
        match command {
            Command::Prompt { text } => {
                if !self.prompt_starts_turn(&text) {
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
                let mut updates = Vec::new();
                self.clear_worker_waits(&mut updates);
                self.outbox.events.extend(updates);
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
                // Checked before it is removed: a refused decision leaves the request answerable.
                match self.approvals.get(&request) {
                    None => {
                        return Err(AdapterError::Invalid(format!(
                            "no pending approval {request}"
                        )));
                    }
                    Some(pending) if !pending.options.contains(&decision) => {
                        return Err(AdapterError::Invalid(format!(
                            "Codex did not offer {} for approval {request}",
                            decision_wire(decision)
                        )));
                    }
                    Some(_) => {}
                }
                let Some(approval) = self.approvals.remove(&request) else {
                    return Err(AdapterError::Invalid(format!(
                        "no pending approval {request}"
                    )));
                };
                self.remember_settled_request(request, approval.scope.clone());
                let line = json!({
                    "id": approval.rpc_id,
                    "result": {"decision": decision_wire(decision)},
                })
                .to_string();
                Ok(vec![Action::Write(vec![line])])
            }
            Command::Answer { request, answers } => {
                let Some(question) = self.questions.get(&request).cloned() else {
                    return Err(AdapterError::Invalid(format!(
                        "no pending Codex question {request}"
                    )));
                };
                let Some(selected) = answers.as_object() else {
                    return Err(AdapterError::Invalid(
                        "question answers must be an object".to_owned(),
                    ));
                };
                let mut mapped = Map::new();
                for (id, text) in question.keys {
                    let Some(answer) = selected.get(&text).and_then(Value::as_str) else {
                        return Err(AdapterError::Invalid(format!(
                            "missing answer for Codex question {text:?}"
                        )));
                    };
                    mapped.insert(id, json!({"answers": [answer]}));
                }
                let line = json!({
                    "id": question.rpc_id,
                    "result": {"answers": mapped},
                })
                .to_string();
                self.questions.remove(&request);
                self.remember_settled_request(request, question.scope);
                Ok(vec![Action::Write(vec![line])])
            }
            Command::SetModel { model, effort } => {
                // Applied by the next `turn/start`; no restart. `None` keeps the effort.
                self.model_selected_during_start = self.thread_id.is_none();
                self.selection_revision = self.selection_revision.saturating_add(1);
                if effort.is_some() {
                    self.effort = effort;
                }
                self.model = Some(model.clone());
                self.outbox.events.push(
                    Envelope::new(Event::ModelChangeRequested { model })
                        .request(format!("codex-model-{}", self.selection_revision)),
                );
                Ok(Vec::new())
            }
            Command::SetMode { mode } => {
                if self.mode == mode {
                    return Ok(Vec::new());
                }
                self.mode = mode;
                if let Some(effective) = self.effective_mode.filter(|effective| *effective != mode)
                {
                    self.emit(Event::ModeChangeDeferred {
                        requested: mode,
                        effective,
                    });
                    self.emit(Event::Notice { text: format!(
                        "{} applies to the next turn. This turn keeps {}; its pending approvals still need an answer.",
                        codex_mode_label(mode), codex_mode_label(effective)
                    ) });
                } else {
                    // Selecting the effective mode cancels any deferred selection.
                    self.emit(Event::ModeChanged { mode });
                }
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
        self.settled_requests.clear();
        self.worker_turns.clear();
        let expected = self.interrupt_sent;
        let mut out = Vec::new();
        self.clear_worker_waits(&mut out);
        let mut changed = false;
        for worker in self.workers.values_mut() {
            if matches!(
                worker.state,
                WorkerState::Starting | WorkerState::Running | WorkerState::Waiting
            ) {
                // Losing this connection is not evidence that the child was closed.
                worker.state = WorkerState::Unknown;
                changed = true;
            }
        }
        if changed {
            self.emit_workers(&mut out);
        }
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
        let mut questions: Vec<_> = std::mem::take(&mut self.questions).into_iter().collect();
        questions.sort_by(|a, b| a.0.cmp(&b.0));
        for (request, question) in questions {
            out.push(
                Envelope::new(Event::QuestionResolved { answered: false })
                    .item(question.item)
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
        self.finish_effective_mode(&mut out);
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

fn remember_bounded<T: Clone + Eq + std::hash::Hash>(
    keys: &mut HashSet<T>,
    order: &mut VecDeque<T>,
    key: T,
    limit: usize,
) {
    if keys.insert(key.clone()) {
        order.push_back(key);
        if order.len() > limit {
            if let Some(oldest) = order.pop_front() {
                keys.remove(&oldest);
            }
        }
    }
}

/// `approvalPolicy` and `sandboxPolicy` for a mode (see the module's mode table).
fn mode_policy(mode: Mode) -> (&'static str, Value) {
    match mode {
        Mode::Ask => ("untrusted", json!({"type": "readOnly"})),
        Mode::AcceptEdits => ("on-request", json!({"type": "workspaceWrite"})),
        Mode::Plan => ("never", json!({"type": "readOnly"})),
    }
}

fn codex_mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Ask => "Ask before edits",
        Mode::AcceptEdits => "Accept edits",
        Mode::Plan => "Plan",
    }
}

/// The wire name of a decision (`CommandExecutionApprovalDecision` / `FileChangeApprovalDecision`).
fn decision_wire(decision: Decision) -> &'static str {
    match decision {
        Decision::Allow => "accept",
        Decision::AllowForSession => "acceptForSession",
        // Codex has no persistent allow and never offers it, so `encode` refuses it before any
        // line is written; the name only appears in that refusal.
        Decision::AllowAlways => "acceptAlways",
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
        "webSearch" => {
            let (title, input) = web_search_details(item);
            (ItemKind::WebSearch, title, input)
        }
        "collabAgentToolCall" => (
            ItemKind::Tool,
            s(item, "tool").unwrap_or("subagent").to_owned(),
            Some(
                json!({"prompt": item.get("prompt"), "receiverThreadIds": item.get("receiverThreadIds")}),
            ),
        ),
        // Legacy activity cards lack verified worker identity/lifecycle; never count the card
        // as a worker. Current workers are tracked from thread ids and agentsStates instead.
        "subAgentActivity" => (ItemKind::Tool, "subagent activity".to_owned(), None),
        "imageView" => (
            ItemKind::FileRead,
            s(item, "path").unwrap_or("image").to_owned(),
            None,
        ),
        other => (ItemKind::Tool, other.to_owned(), None),
    }
}

/// A web-search item's action moved from the legacy top-level `query` to `action` in newer
/// Codex app-server schemas. Normalise either form so the card names the actual operation.
fn web_search_details(item: &Value) -> (String, Option<Value>) {
    let action = item.get("action").filter(|a| a.is_object());
    let source = action.unwrap_or(item);
    match action.and_then(|action| s(action, "type")) {
        Some("search") | None => {
            let queries: Vec<String> = source
                .get("queries")
                .and_then(Value::as_array)
                .map(|queries| {
                    queries
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let query = s(source, "query")
                .map(str::to_owned)
                .or_else(|| queries.first().cloned());
            let title = match (&query, queries.len()) {
                (Some(query), 0 | 1) => query.clone(),
                (Some(query), count) => format!("{query} (+{} more)", count - 1),
                (None, _) => "web search".to_owned(),
            };
            let input = query.map(|query| json!({"query": query, "queries": queries}));
            (title, input)
        }
        Some("open_page") => {
            let url = s(source, "url").map(str::to_owned);
            let title = url
                .as_deref()
                .map(|url| format!("Open {url}"))
                .unwrap_or_else(|| "open web page".to_owned());
            (title, url.map(|url| json!({"url": url})))
        }
        Some("find_in_page") => {
            let url = s(source, "url").map(str::to_owned);
            let pattern = s(source, "pattern").map(str::to_owned);
            let title = match (&pattern, &url) {
                (Some(pattern), Some(url)) => format!("Find {pattern:?} in {url}"),
                (Some(pattern), None) => format!("Find {pattern:?}"),
                (None, Some(url)) => format!("Find in {url}"),
                (None, None) => "find in web page".to_owned(),
            };
            let mut input = Map::new();
            if let Some(url) = url {
                input.insert("url".to_owned(), Value::String(url));
            }
            if let Some(pattern) = pattern {
                input.insert("pattern".to_owned(), Value::String(pattern));
            }
            let input = (!input.is_empty()).then_some(Value::Object(input));
            (title, input)
        }
        Some(_) => ("web search".to_owned(), None),
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
        Some("collabAgentToolCall") => (status, None, None),
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

fn collab_worker_state(status: Option<&str>) -> WorkerState {
    match status {
        Some("pendingInit") => WorkerState::Starting,
        Some("running") => WorkerState::Running,
        Some("interrupted") => WorkerState::Stopped,
        Some("completed") => WorkerState::Completed,
        Some("errored") => WorkerState::Failed,
        Some("shutdown") => WorkerState::Closed,
        _ => WorkerState::Unknown,
    }
}

fn thread_worker_state(status: Option<&Value>) -> WorkerState {
    let Some(status) = status else {
        return WorkerState::Unknown;
    };
    match s(status, "type") {
        Some("active") => {
            if strings(status, "activeFlags")
                .iter()
                .any(|flag| matches!(flag.as_str(), "waitingOnApproval" | "waitingOnUserInput"))
            {
                WorkerState::Waiting
            } else {
                WorkerState::Running
            }
        }
        Some("systemError") => WorkerState::Failed,
        // idle/notLoaded neither prove task success nor explicit worker closure.
        _ => WorkerState::Unknown,
    }
}

fn set_worker_state(worker: &mut WorkerSnapshot, state: WorkerState) {
    if state != WorkerState::Unknown
        || !matches!(
            worker.state,
            WorkerState::Completed
                | WorkerState::Failed
                | WorkerState::Stopped
                | WorkerState::Closed
        )
    {
        worker.state = state;
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

    /// Frames follow the installed app-server JSON schema; no provider prompt is needed.
    #[test]
    fn collaboration_frames_publish_workers_and_explicit_waits() {
        let mut a = started(Mode::Ask);
        let out = a.feed(&json!({"method":"item/started", "params":{
            "threadId":"th-1", "turnId":"tu-1", "item":{
                "type":"collabAgentToolCall", "id":"spawn-1", "tool":"spawnAgent",
                "status":"inProgress", "senderThreadId":"th-1", "receiverThreadIds":["worker-1"],
                "prompt":"Review configuration", "agentsStates":{"worker-1":{"status":"pendingInit"}}
            }
        }}).to_string());
        let workers = out
            .iter()
            .map(|e| serde_json::to_value(&e.event).expect("json"))
            .find(|e| e["type"] == "workers_updated")
            .expect("worker snapshot");
        assert_eq!(workers["workers"][0]["id"], "worker-1");
        assert_eq!(workers["workers"][0]["task"], "Review configuration");
        assert_eq!(workers["workers"][0]["state"], "starting");
    }

    #[test]
    fn completed_file_change_refines_its_initial_empty_changes() {
        let mut a = started(Mode::Ask);
        a.feed(r#"{"method":"item/started","params":{"threadId":"th-1","turnId":"tu","item":{"type":"fileChange","id":"edit","status":"inProgress","changes":[]}}}"#);
        let changes = json!([{"path":"src/a.rs","kind":{"type":"update"},"diff":"@@ -1 +1 @@\n-old\n+new\n"}]);
        let out = a.feed(&json!({"method":"item/completed","params":{"threadId":"th-1","turnId":"tu","item":{"type":"fileChange","id":"edit","status":"completed","changes":changes}}}).to_string());
        assert!(out.iter().any(|e| matches!(&e.event, Event::ItemStarted {kind: ItemKind::FileChange, input: Some(input), ..} if *input == changes)), "completed changes must replace the empty start");
    }

    #[test]
    fn a_question_without_options_is_refused_instead_of_stranding_the_user() {
        let mut a = started(Mode::Ask);
        let out = a.feed(r#"{"id":"question-1","method":"item/tool/requestUserInput","params":{"threadId":"th-1","turnId":"tu","itemId":"q","questions":[{"id":"q1","header":"Choice","question":"What should change?","options":[]}]}}"#);
        assert!(!out
            .iter()
            .any(|e| matches!(e.event, Event::QuestionRequested { .. })));
        let wire = writes(&a.drain_outbox().actions);
        assert_eq!(wire[0]["error"]["code"], -32602);
        assert!(wire[0]["error"]["message"]
            .as_str()
            .expect("message")
            .contains("options"));
    }

    const WORKERS: &str = include_str!("../tests/fixtures/codex-schema-workers.ndjson");

    #[test]
    fn schema_workers_keep_identity_across_operations_and_close_only_explicitly() {
        let mut a = started(Mode::Ask);
        a.feed(
            r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"parent-turn"}}}"#,
        );
        let mut snapshots = Vec::new();
        for (i, line) in WORKERS.lines().enumerate() {
            let out = a.feed(line);
            snapshots.extend(out.iter().filter_map(|e| match &e.event {
                Event::WorkersUpdated {
                    workers,
                    waiting_for,
                } => Some((workers.clone(), waiting_for.clone())),
                _ => None,
            }));
            assert!(
                !out.iter().any(|e| matches!(
                    e.event,
                    Event::ItemStarted {
                        kind: ItemKind::Subagent,
                        ..
                    }
                )),
                "operations must not count as workers"
            );
            let worker = a.workers.get("worker-1").expect("worker");
            match i {
                1 => assert_eq!(worker.name.as_deref(), Some("Mira")),
                2 => assert_eq!(
                    worker.state,
                    WorkerState::Running,
                    "spawn call complete is not worker complete"
                ),
                3 => assert_eq!(a.waiting_for(), ["worker-1"]),
                4 => assert!(
                    out.is_empty(),
                    "child reasoning must not enter the parent's transcript"
                ),
                5 => assert_eq!(worker.state, WorkerState::Waiting),
                6 => {
                    assert_eq!(worker.state, WorkerState::Completed);
                    assert_eq!(a.turn_id.as_deref(), Some("parent-turn"));
                    assert!(a.turn_open, "worker turn must not end the parent turn");
                }
                7 => {
                    assert_eq!(worker.state, WorkerState::Completed);
                    assert!(a.waiting_for().is_empty());
                }
                8 => {
                    assert_eq!(worker.task.as_deref(), Some("Review settings"));
                    assert_eq!(
                        worker.activity, None,
                        "new assignment clears the old result"
                    );
                }
                9 | 11 => assert_eq!(
                    worker.state,
                    WorkerState::Running,
                    "operation completion is not closure"
                ),
                12 | 13 => assert_eq!(worker.state, WorkerState::Closed),
                14 => assert_eq!(a.turn_id.as_deref(), Some("parent-turn")),
                _ => {}
            }
        }
        assert_eq!(
            a.workers.len(),
            2,
            "one entry per worker thread, not per operation"
        );
        assert_eq!(a.workers["worker-2"].state, WorkerState::Unknown);
        // Canonical snapshots replay by replacement; no native frames are needed to recover
        // identities/history, and future builds can add metadata without breaking the store.
        let mut replay = BTreeMap::new();
        for (workers, _) in snapshots {
            let env = Envelope::new(Event::WorkersUpdated {
                workers,
                waiting_for: Vec::new(),
            });
            let encoded = serde_json::to_string(&env).expect("serialize");
            let decoded: Envelope = serde_json::from_str(&encoded).expect("decode");
            if let Event::WorkersUpdated { workers, .. } = decoded.event {
                replay = workers.into_iter().map(|w| (w.id.clone(), w)).collect();
            }
        }
        assert_eq!(replay, a.workers);
    }

    fn waiting_adapter() -> CodexAdapter {
        let mut a = started(Mode::Ask);
        a.feed(r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"tu-1"}}}"#);
        for line in WORKERS.lines().take(4) {
            a.feed(line);
        }
        assert_eq!(a.waiting_for(), ["worker-1"]);
        a
    }

    #[test]
    fn explicit_waits_clear_on_cancellation_failure_interrupt_and_exit() {
        for status in ["completed", "failed", "interrupted"] {
            let mut a = waiting_adapter();
            let out = a.feed(&json!({"method":"item/completed","params":{"threadId":"th-1","turnId":"tu-1","item":{
                "type":"collabAgentToolCall","id":"wait-1","tool":"wait","status":status,
                "senderThreadId":"th-1","receiverThreadIds":["worker-1"],"prompt":null,"agentsStates":{}
            }}}).to_string());
            assert!(a.waiting_for().is_empty());
            assert!(out.iter().any(|e| matches!(&e.event, Event::WorkersUpdated { waiting_for, .. } if waiting_for.is_empty())));
            assert_eq!(a.workers["worker-1"].state, WorkerState::Running);
        }
        let mut a = waiting_adapter();
        a.encode(Command::Interrupt).expect("interrupt");
        assert!(a.waiting_for().is_empty());
        assert!(a.drain_outbox().events.iter().any(|e| matches!(&e.event, Event::WorkersUpdated { waiting_for, .. } if waiting_for.is_empty())));
        let mut a = waiting_adapter();
        a.feed(r#"{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"tu-1","status":"failed","error":{"message":"quota"}}}}"#);
        assert!(a.waiting_for().is_empty());
        let mut a = waiting_adapter();
        a.on_exit(Some(1));
        assert!(a.waiting_for().is_empty());
        assert_eq!(
            a.workers["worker-1"].state,
            WorkerState::Unknown,
            "connection loss does not close the child"
        );
    }

    #[test]
    fn worker_schema_states_and_nested_metadata_have_safe_fallbacks() {
        for (wire, state) in [
            ("pendingInit", WorkerState::Starting),
            ("running", WorkerState::Running),
            ("interrupted", WorkerState::Stopped),
            ("completed", WorkerState::Completed),
            ("errored", WorkerState::Failed),
            ("shutdown", WorkerState::Closed),
            ("notFound", WorkerState::Unknown),
            ("future-state", WorkerState::Unknown),
        ] {
            assert_eq!(collab_worker_state(Some(wire)), state);
        }
        let mut a = waiting_adapter();
        a.feed(r#"{"method":"thread/started","params":{"thread":{"id":"nested","source":{"subAgent":{"thread_spawn":{"parent_thread_id":"worker-1","depth":2,"agent_nickname":null,"agent_role":"reviewer"}}},"status":{"type":"notLoaded"}}}}"#);
        assert_eq!(a.workers["nested"].name.as_deref(), Some("reviewer"));
        assert_eq!(a.workers["nested"].state, WorkerState::Unknown);
        a.feed(r#"{"method":"thread/closed","params":{"threadId":"th-1"}}"#);
        assert!(a.waiting_for().is_empty());
        assert_eq!(
            a.workers["worker-1"].state,
            WorkerState::Running,
            "closing parent isn't proof child closed"
        );
    }

    #[test]
    fn partially_unanswerable_question_batch_is_refused_whole() {
        let mut a = started(Mode::Ask);
        let out = a.feed(&json!({"id":55,"method":"item/tool/requestUserInput","params":{
            "threadId":"th-1","turnId":"tu","itemId":"q","questions":[
                {"id":"a","header":"A","question":"One?","options":[{"label":"Yes","description":"Continue"}]},
                {"id":"b","header":"B","question":"Two?","options":[{"label":" "},{"description":"No label"}]}
            ]}}).to_string());
        assert!(!out
            .iter()
            .any(|e| matches!(e.event, Event::QuestionRequested { .. })));
        assert!(a.questions.is_empty());
        assert_eq!(writes(&a.drain_outbox().actions)[0]["id"], 55);
    }

    #[test]
    fn changing_mode_mid_turn_does_not_claim_the_current_sandbox_changed() {
        let mut a = started(Mode::Ask);
        let first = one_write(
            a.encode(Command::Prompt {
                text: "Review".into(),
            })
            .expect("prompt"),
        );
        assert_eq!(first["params"]["sandboxPolicy"]["type"], "readOnly");
        a.feed(r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"tu"}}}"#);
        a.encode(Command::SetMode {
            mode: Mode::AcceptEdits,
        })
        .expect("mode");
        let out = a.drain_outbox().events;
        assert!(
            !out.iter()
                .any(|e| matches!(e.event, Event::ModeChanged { .. })),
            "running mode cannot change without a policy write"
        );
        let deferred = out
            .iter()
            .map(|e| serde_json::to_value(&e.event).expect("json"))
            .find(|e| e["type"] == "mode_change_deferred")
            .expect("deferred");
        assert_eq!(deferred["effective"], "ask");
        assert_eq!(deferred["requested"], "accept_edits");
        let ended = a.feed(r#"{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"tu","status":"completed"}}}"#);
        assert!(ended.iter().any(|e| matches!(
            e.event,
            Event::ModeChanged {
                mode: Mode::AcceptEdits
            }
        )));
        let next = one_write(
            a.encode(Command::Prompt {
                text: "Edit".into(),
            })
            .expect("prompt"),
        );
        assert_eq!(next["params"]["sandboxPolicy"]["type"], "workspaceWrite");
        assert_eq!(next["params"]["approvalPolicy"], "on-request");
    }

    #[test]
    fn deferred_modes_preserve_accept_edits_cancel_and_apply_only_to_the_next_turn() {
        let mut a = started(Mode::AcceptEdits);
        a.encode(Command::Prompt {
            text: "Edit".into(),
        })
        .expect("prompt");
        a.feed(r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"tu"}}}"#);
        a.encode(Command::SetMode { mode: Mode::Plan })
            .expect("mode");
        assert_eq!(a.effective_mode, Some(Mode::AcceptEdits));
        assert!(a.drain_outbox().events.iter().any(|e| matches!(
            e.event,
            Event::ModeChangeDeferred {
                requested: Mode::Plan,
                effective: Mode::AcceptEdits
            }
        )));
        a.encode(Command::SetMode { mode: Mode::Plan })
            .expect("repeat");
        assert!(
            a.drain_outbox().events.is_empty(),
            "no repeated notice for unchanged request"
        );
        a.encode(Command::SetMode {
            mode: Mode::AcceptEdits,
        })
        .expect("cancel");
        assert_eq!(
            a.drain_outbox().events,
            [Envelope::new(Event::ModeChanged {
                mode: Mode::AcceptEdits
            })]
        );
        a.encode(Command::SetMode { mode: Mode::Plan })
            .expect("plan");
        a.drain_outbox();
        a.feed(r#"{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"tu","status":"interrupted"}}}"#);
        assert_eq!(a.effective_mode, None);
        let next = one_write(
            a.encode(Command::Prompt {
                text: "Plan".into(),
            })
            .expect("prompt"),
        );
        assert_eq!(next["params"]["sandboxPolicy"]["type"], "readOnly");
        assert_eq!(next["params"]["approvalPolicy"], "never");
    }

    #[test]
    fn mode_change_during_pending_start_is_deferred_and_failed_start_clears_it() {
        let mut a = started(Mode::Ask);
        let start = one_write(
            a.encode(Command::Prompt {
                text: "Review".into(),
            })
            .expect("prompt"),
        );
        a.encode(Command::SetMode {
            mode: Mode::AcceptEdits,
        })
        .expect("mode");
        assert_eq!(a.effective_mode, Some(Mode::Ask));
        assert!(a.drain_outbox().events.iter().any(|e| matches!(
            e.event,
            Event::ModeChangeDeferred {
                effective: Mode::Ask,
                ..
            }
        )));
        let out = a.feed(&json!({"id":start["id"],"error":{"message":"start failed"}}).to_string());
        assert_eq!(a.effective_mode, None);
        assert!(out.iter().any(|e| matches!(
            e.event,
            Event::TurnCompleted {
                state: TurnState::Failed,
                ..
            }
        )));
        assert!(
            out.iter().any(|e| matches!(
                e.event,
                Event::ModeChanged {
                    mode: Mode::AcceptEdits
                }
            )),
            "now idle: selected next-turn default is available"
        );
    }

    #[test]
    fn reconnect_preserves_worker_history_without_claiming_live_activity() {
        let mut a = waiting_adapter();
        let closed = WorkerSnapshot {
            id: "closed".into(),
            name: Some("Finished reviewer".into()),
            task: None,
            state: WorkerState::Closed,
            activity: None,
        };
        a.workers.insert(closed.id.clone(), closed.clone());
        a.handshake();
        assert!(a.waiting_for().is_empty());
        assert_eq!(a.workers["worker-1"].state, WorkerState::Unknown);
        assert_eq!(a.workers["closed"], closed);
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
        assert!(approval.request.as_deref().is_some_and(|request| {
            request.starts_with("codex:item/commandExecution/requestApproval:")
        }));
        assert!(matches!(&approval.event, Event::ApprovalRequested {
            tool, title: Some(t), reason: Some(r), options, response: ResponseCapability::Live, input,
            remembers: None
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
    fn a_stopped_queued_prompt_is_replaced_by_one_continuation_on_the_resumed_model() {
        let mut a = CodexAdapter::new();
        a.argv(&session(Mode::Ask, Some("th-1")));
        a.handshake();
        assert!(a
            .encode(Command::Prompt {
                text: "original work".into()
            })
            .expect("queue original")
            .is_empty());
        a.discard_queued_prompts();
        a.on_exit(None);
        let mut resumed = session(Mode::Ask, Some("th-1"));
        resumed.model = Some("selected".into());
        resumed.effort = Some("high".into());
        a.argv(&resumed);
        let handshake = a.handshake();
        let resume: Value = serde_json::from_str(&handshake[2]).expect("resume request");
        assert_eq!(resume["method"], "thread/resume");
        assert_eq!(resume["params"]["threadId"], "th-1");
        assert_eq!(resume["params"]["model"], "selected");
        assert!(a
            .encode(Command::Prompt {
                text: "continue the interrupted work".into()
            })
            .expect("queue continuation")
            .is_empty());
        a.feed(r#"{"id":2,"result":{"thread":{"id":"th-1"},"model":"selected"}}"#);
        let flushed = writes(&a.drain_outbox().actions);
        assert_eq!(
            flushed.len(),
            1,
            "stopped work must not be replayed alongside continuation"
        );
        assert_eq!(flushed[0]["params"]["model"], "selected");
        assert_eq!(flushed[0]["params"]["effort"], "high");
        assert_eq!(
            flushed[0]["params"]["input"][0]["text"],
            "continue the interrupted work"
        );
        assert!(!a.prompt_starts_turn(" /COMPACT "));
        assert!(a.prompt_starts_turn("/review"));
    }

    #[test]
    fn an_initial_turn_confirms_or_rejects_its_model_when_startup_omits_it() {
        for success in [false, true] {
            let mut a = CodexAdapter::new();
            a.argv(&session(Mode::Ask, Some("th-1")));
            a.handshake();
            a.feed(r#"{"id":2,"result":{"thread":{"id":"th-1"}}}"#);
            let sent = one_write(
                a.encode(Command::Prompt {
                    text: "continue".into(),
                })
                .expect("send"),
            );
            let response = if success {
                json!({"id":sent["id"],"result":{"turn":{"id":"tu"}}})
            } else {
                json!({"id":sent["id"],"error":{"message":"model unavailable"}})
            };
            let events = a.feed(&response.to_string());
            if success {
                assert!(events.iter().any(
                    |e| matches!(&e.event, Event::ModelChanged { model } if model == "gpt-5-codex")
                        && e.request.is_none()
                ));
                let started = a.feed(
                    r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"tu"}}}"#,
                );
                assert!(!started
                    .iter()
                    .any(|e| matches!(e.event, Event::ModelChanged { .. })));
            } else {
                assert!(events.iter().any(|e| matches!(&e.event, Event::ModelChangeFailed { model, .. } if model == "gpt-5-codex") && e.request.is_none()));
                assert!(events.iter().any(|e| matches!(
                    e.event,
                    Event::TurnCompleted {
                        state: TurnState::Failed,
                        ..
                    }
                )));
            }
        }
    }

    #[test]
    fn startup_failure_discards_queued_work_and_late_responses_cannot_restart_it() {
        for resume in [None, Some("th-1")] {
            for failure in [
                r#"{"id":1,"error":{"message":"initialize denied"}}"#,
                r#"{"id":2,"error":{"message":"thread denied"}}"#,
                r#"{"id":2,"result":{"thread":{}}}"#,
            ] {
                let mut a = CodexAdapter::new();
                a.argv(&session(Mode::Ask, resume));
                a.handshake();
                a.encode(Command::Prompt {
                    text: "continue".into(),
                })
                .expect("queue");
                let events = a.feed(failure);
                assert!(events
                    .iter()
                    .any(|e| matches!(e.event, Event::Error { .. })));
                assert!(events.iter().any(|e| matches!(&e.event, Event::ModelChangeFailed { model, .. } if model == "gpt-5-codex")));
                assert!(events.iter().any(|e| matches!(
                    e.event,
                    Event::TurnCompleted {
                        state: TurnState::Failed,
                        ..
                    }
                )));
                let late =
                    a.feed(r#"{"id":2,"result":{"thread":{"id":"th-1"},"model":"gpt-5-codex"}}"#);
                assert!(!late
                    .iter()
                    .any(|e| matches!(e.event, Event::SessionStarted { .. })));
                assert!(a.drain_outbox().actions.is_empty());
                assert!(a
                    .encode(Command::Prompt {
                        text: "stale retry".into()
                    })
                    .is_err());
                a.argv(&session(Mode::Ask, resume));
                a.handshake();
                a.encode(Command::Prompt {
                    text: "fresh retry".into(),
                })
                .expect("queue fresh retry");
                a.feed(r#"{"id":2,"result":{"thread":{"id":"th-1"},"model":"gpt-5-codex"}}"#);
                let sent = writes(&a.drain_outbox().actions);
                assert_eq!(sent.len(), 1);
                assert_eq!(sent[0]["params"]["input"][0]["text"], "fresh retry");
            }
        }
    }

    #[test]
    fn a_late_initialize_failure_invalidates_a_submitted_turn() {
        let mut a = CodexAdapter::new();
        a.argv(&session(Mode::Ask, Some("th-1")));
        a.handshake();
        a.feed(r#"{"id":2,"result":{"thread":{"id":"th-1"}}}"#);
        let sent = one_write(
            a.encode(Command::Prompt {
                text: "continue".into(),
            })
            .expect("send"),
        );
        let failed = a.feed(r#"{"id":1,"error":{"message":"initialize denied"}}"#);
        assert!(failed.iter().any(|e| matches!(
            e.event,
            Event::TurnCompleted {
                state: TurnState::Failed,
                ..
            }
        )));
        let late = a.feed(&json!({"id":sent["id"],"result":{"turn":{"id":"tu"}}}).to_string());
        assert!(late.is_empty());
        let started =
            a.feed(r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"tu"}}}"#);
        assert!(started.is_empty());
    }

    #[test]
    fn an_initial_turn_reroute_is_not_overwritten_by_its_original_acceptance() {
        let mut a = CodexAdapter::new();
        a.argv(&session(Mode::Ask, Some("th-1")));
        a.handshake();
        a.feed(r#"{"id":2,"result":{"thread":{"id":"th-1"}}}"#);
        let sent = one_write(
            a.encode(Command::Prompt {
                text: "continue".into(),
            })
            .expect("send"),
        );
        let reroute = a.feed(r#"{"method":"model/rerouted","params":{"threadId":"th-1","turnId":"tu","toModel":"fallback"}}"#);
        assert!(matches!(&reroute[0].event, Event::ModelChanged { model } if model == "fallback"));
        assert!(reroute[0].request.is_none());
        let ack = a.feed(&json!({"id":sent["id"],"result":{"turn":{"id":"tu"}}}).to_string());
        assert!(!ack
            .iter()
            .any(|e| matches!(e.event, Event::ModelChanged { .. })));
        assert_eq!(a.confirmed_model.as_deref(), Some("fallback"));
    }

    #[test]
    fn an_explicit_launch_model_is_used_for_continuation_when_startup_reports_the_old_model() {
        for resume in [None, Some("th-1")] {
            let mut a = CodexAdapter::new();
            let mut open = session(Mode::Ask, resume);
            open.model = Some("selected".into());
            open.effort = Some("high".into());
            a.argv(&open);
            a.handshake();
            a.encode(Command::Prompt {
                text: "continue".into(),
            })
            .expect("queue continuation");
            let events = a.feed(r#"{"id":2,"result":{"thread":{"id":"th-1"},"model":"old"}}"#);
            assert!(
                matches!(&events[0].event, Event::SessionStarted { model: Some(model), .. } if model == "old")
            );
            let flushed = writes(&a.drain_outbox().actions);
            assert_eq!(flushed.len(), 1);
            assert_eq!(flushed[0]["params"]["model"], "selected");
            assert_eq!(flushed[0]["params"]["effort"], "high");
            let accepted =
                a.feed(&json!({"id":flushed[0]["id"],"result":{"turn":{"id":"tu"}}}).to_string());
            assert!(accepted.iter().any(
                |e| matches!(&e.event, Event::ModelChanged { model } if model == "selected")
                    && e.request.is_none()
            ));
            let started = a.feed(
                r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"tu"}}}"#,
            );
            assert!(!started
                .iter()
                .any(|e| matches!(e.event, Event::ModelChanged { .. })));
        }
        let mut a = CodexAdapter::new();
        let mut open = session(Mode::Ask, None);
        open.model = None;
        a.argv(&open);
        a.handshake();
        a.encode(Command::Prompt {
            text: "use default".into(),
        })
        .expect("queue default");
        a.feed(r#"{"id":2,"result":{"thread":{"id":"th-1"},"model":"reported-default"}}"#);
        let flushed = writes(&a.drain_outbox().actions);
        assert_eq!(flushed[0]["params"]["model"], "reported-default");
    }

    #[test]
    fn a_model_selected_during_thread_start_survives_the_original_model_response() {
        for resume in [None, Some("th-1")] {
            let mut a = CodexAdapter::new();
            a.argv(&session(Mode::Ask, resume));
            a.handshake();
            a.encode(Command::SetModel {
                model: "gpt-5.5".into(),
                effort: Some("high".into()),
            })
            .expect("select");
            a.drain_outbox();
            assert!(a
                .encode(Command::Prompt {
                    text: "queued".into()
                })
                .expect("queue")
                .is_empty());
            let events =
                a.feed(r#"{"id":2,"result":{"thread":{"id":"th-1"},"model":"gpt-5-codex"}}"#);
            assert!(matches!(&events[0].event,
                Event::SessionStarted { model: Some(model), .. } if model == "gpt-5-codex"));
            let flushed = writes(&a.drain_outbox().actions);
            assert_eq!(flushed[0]["params"]["model"], "gpt-5.5");
            assert_eq!(flushed[0]["params"]["effort"], "high");
            let next = one_write(
                a.encode(Command::Prompt {
                    text: "next".into(),
                })
                .expect("send"),
            );
            assert_eq!(next["params"]["model"], "gpt-5.5");
        }
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
                Envelope::new(Event::ModelChangeRequested {
                    model: "gpt-5.5".into()
                })
                .request("codex-model-1"),
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
    fn model_confirmation_uses_the_submitted_choice_and_requires_backend_acceptance() {
        for notification_first in [false, true] {
            let mut a = started(Mode::Ask);
            a.encode(Command::SetModel {
                model: "selected".into(),
                effort: Some("high".into()),
            })
            .expect("select");
            let requested = a.drain_outbox();
            let selection_key = requested.events[0].request.clone();
            assert_eq!(selection_key.as_deref(), Some("codex-model-1"));
            assert!(
                matches!(&requested.events[0].event, Event::ModelChangeRequested { model } if model == "selected")
            );
            assert!(!requested
                .events
                .iter()
                .any(|e| matches!(e.event, Event::ModelChanged { .. })));
            let sent = one_write(
                a.encode(Command::Prompt {
                    text: "first".into(),
                })
                .expect("send"),
            );
            a.encode(Command::SetModel {
                model: "newer".into(),
                effort: Some("low".into()),
            })
            .expect("select newer");
            a.drain_outbox();
            let response = json!({"id":sent["id"],"result":{"turn":{"id":"tu"}}}).to_string();
            let notification =
                r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"tu"}}}"#;
            let frames = if notification_first {
                [notification, response.as_str()]
            } else {
                [response.as_str(), notification]
            };
            let events: Vec<_> = frames.into_iter().flat_map(|frame| a.feed(frame)).collect();
            assert!(events
                .iter()
                .filter(|e| matches!(e.event, Event::ModelChanged { .. }))
                .all(|e| e.request == selection_key));
            assert_eq!(
                events
                    .iter()
                    .filter(
                        |e| matches!(&e.event, Event::ModelChanged { model } if model == "selected")
                    )
                    .count(),
                1
            );
            assert!(events.iter().any(|e| matches!(&e.event, Event::TurnStarted { model: Some(model) } if model == "selected")));
            assert!(!events
                .iter()
                .any(|e| matches!(&e.event, Event::ModelChanged { model } if model == "newer")));
            let next = one_write(
                a.encode(Command::Prompt {
                    text: "next".into(),
                })
                .expect("next"),
            );
            assert_eq!(next["params"]["model"], "newer");
            assert_eq!(next["params"]["effort"], "low");
        }
    }

    #[test]
    fn a_rejected_model_restores_confirmed_settings_without_overwriting_a_newer_choice() {
        for newer in [false, true] {
            let mut a = started(Mode::Ask);
            a.encode(Command::SetModel {
                model: "invalid".into(),
                effort: Some("high".into()),
            })
            .expect("select");
            a.drain_outbox();
            let sent = one_write(
                a.encode(Command::Prompt {
                    text: "first".into(),
                })
                .expect("send"),
            );
            if newer {
                a.encode(Command::SetModel {
                    model: "newer".into(),
                    effort: Some("low".into()),
                })
                .expect("select newer");
                a.drain_outbox();
            }
            let events = a.feed(
                &json!({"id":sent["id"],"error":{"code":-32600,"message":"no such model"}})
                    .to_string(),
            );
            assert!(events.iter().any(|e| matches!(&e.event, Event::ModelChangeFailed { model, message } if model == "invalid" && message == "no such model")));
            assert!(!events
                .iter()
                .any(|e| matches!(e.event, Event::ModelChanged { .. })));
            assert!(events.iter().any(|e| matches!(
                e.event,
                Event::TurnCompleted {
                    state: TurnState::Failed,
                    ..
                }
            )));
            let next = one_write(
                a.encode(Command::Prompt {
                    text: "next".into(),
                })
                .expect("next"),
            );
            assert_eq!(
                next["params"]["model"],
                if newer { "newer" } else { "gpt-5-codex" }
            );
            if newer {
                assert_eq!(next["params"]["effort"], "low");
            } else {
                assert!(next["params"].get("effort").is_none());
            }
        }
    }

    #[test]
    fn rejection_restores_the_last_accepted_model_and_effort() {
        let mut a = started(Mode::Ask);
        a.encode(Command::SetModel {
            model: "accepted".into(),
            effort: Some("high".into()),
        })
        .expect("select");
        a.drain_outbox();
        let sent = one_write(
            a.encode(Command::Prompt {
                text: "first".into(),
            })
            .expect("send"),
        );
        a.feed(&json!({"id":sent["id"],"result":{"turn":{"id":"tu"}}}).to_string());
        a.feed(r#"{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"tu","status":"completed"}}}"#);
        a.encode(Command::SetModel {
            model: "invalid".into(),
            effort: Some("low".into()),
        })
        .expect("select invalid");
        a.drain_outbox();
        let sent = one_write(
            a.encode(Command::Prompt {
                text: "second".into(),
            })
            .expect("send"),
        );
        a.feed(&json!({"id":sent["id"],"error":{"message":"no such model"}}).to_string());
        let next = one_write(
            a.encode(Command::Prompt {
                text: "retry".into(),
            })
            .expect("send"),
        );
        assert_eq!(next["params"]["model"], "accepted");
        assert_eq!(next["params"]["effort"], "high");
    }

    #[test]
    fn a_reroute_confirms_the_active_turn_without_overwriting_a_later_choice() {
        let mut a = started(Mode::Ask);
        a.encode(Command::SetModel {
            model: "selected".into(),
            effort: Some("high".into()),
        })
        .expect("select");
        a.drain_outbox();
        let sent = one_write(
            a.encode(Command::Prompt {
                text: "first".into(),
            })
            .expect("send"),
        );
        a.encode(Command::SetModel {
            model: "newer".into(),
            effort: Some("low".into()),
        })
        .expect("select newer");
        a.drain_outbox();
        let events = a.feed(r#"{"method":"model/rerouted","params":{"threadId":"th-1","turnId":"tu","fromModel":"selected","toModel":"fallback","reason":"x"}}"#);
        assert_eq!(events[0].request.as_deref(), Some("codex-model-1"));
        assert!(matches!(&events[0].event, Event::ModelChanged { model } if model == "fallback"));
        let ack = a.feed(&json!({"id":sent["id"],"result":{"turn":{"id":"tu"}}}).to_string());
        assert!(!ack
            .iter()
            .any(|e| matches!(e.event, Event::ModelChanged { .. })));
        let started =
            a.feed(r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"tu"}}}"#);
        assert!(
            matches!(&started[0].event, Event::TurnStarted { model: Some(model) } if model == "fallback")
        );
        let next = one_write(
            a.encode(Command::Prompt {
                text: "next".into(),
            })
            .expect("next"),
        );
        assert_eq!(next["params"]["model"], "newer");
        assert_eq!(next["params"]["effort"], "low");
    }

    #[test]
    fn repeated_model_choices_have_distinct_acceptance_and_rejection_keys() {
        for success in [false, true] {
            let mut a = started(Mode::Ask);
            a.encode(Command::SetModel {
                model: "same".into(),
                effort: Some("high".into()),
            })
            .expect("first choice");
            let first_key = a.drain_outbox().events[0].request.clone();
            let sent = one_write(
                a.encode(Command::Prompt {
                    text: "first".into(),
                })
                .expect("send"),
            );
            for (model, effort) in [("other", "high"), ("same", "low")] {
                a.encode(Command::SetModel {
                    model: model.into(),
                    effort: Some(effort.into()),
                })
                .expect("later choice");
            }
            let latest_key = a
                .drain_outbox()
                .events
                .last()
                .expect("latest request")
                .request
                .clone();
            assert_eq!(first_key.as_deref(), Some("codex-model-1"));
            assert_eq!(latest_key.as_deref(), Some("codex-model-3"));
            let response = if success {
                json!({"id":sent["id"],"result":{"turn":{"id":"tu"}}})
            } else {
                json!({"id":sent["id"],"error":{"message":"refused"}})
            };
            let events = a.feed(&response.to_string());
            let ack = events
                .iter()
                .find(|e| {
                    matches!(
                        e.event,
                        Event::ModelChanged { .. } | Event::ModelChangeFailed { .. }
                    )
                })
                .expect("selection acknowledgement");
            assert_eq!(ack.request, first_key);
            assert_ne!(ack.request, latest_key);
            let next = one_write(
                a.encode(Command::Prompt {
                    text: "next".into(),
                })
                .expect("next"),
            );
            assert_eq!(next["params"]["model"], "same");
            assert_eq!(next["params"]["effort"], "low");
        }
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
            let request = ev[0].request.clone().expect("canonical request");
            assert_ne!(request, "17");
            let w = one_write(
                a.encode(Command::Approve {
                    request: request.clone(),
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
                    request,
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
    fn reused_rpc_ids_without_items_are_scoped_to_the_active_turn() {
        for rpc_id in 0..4 {
            let frame = json!({
                "id": rpc_id,
                "method": "item/commandExecution/requestApproval",
                "params": {"command": "git fetch", "reason": "Network access"},
            })
            .to_string();
            let mut old = started(Mode::Ask);
            old.turn_id = Some("old-turn".into());
            let old_request = old.feed(&frame)[0].request.clone().expect("old request");
            let mut resumed = started(Mode::Ask);
            resumed.turn_id = Some("new-turn".into());
            let first = resumed.feed(&frame);
            let request = first[0].request.clone().expect("new request");
            assert_ne!(
                request,
                rpc_id.to_string(),
                "historical unscoped ids differ"
            );
            assert_ne!(
                request, old_request,
                "new connections cannot hide a new turn"
            );
            assert_eq!(
                resumed.feed(&frame)[0].request,
                Some(request.clone()),
                "a retransmitted pending request keeps its identity"
            );
            assert_eq!(resumed.approvals.len(), 1);
            assert_eq!(
                one_write(
                    resumed
                        .encode(Command::Approve {
                            request,
                            decision: Decision::Allow,
                            updated_input: None,
                            message: None,
                        })
                        .expect("approve resumed request")
                ),
                json!({"id": rpc_id, "result": {"decision": "accept"}}),
            );
        }
    }

    #[test]
    fn request_identity_preserves_rpc_type_and_native_question_scope() {
        let mut a = started(Mode::Ask);
        let command = "item/commandExecution/requestApproval";
        let params = json!({"threadId": "thread", "turnId": "turn", "itemId": "item"});
        let numeric = a.server_request_key(&json!(0), command, &params);
        let string = a.server_request_key(&json!("0"), command, &params);
        assert_ne!(numeric, string);
        assert_ne!(
            numeric,
            a.server_request_key(&json!(0), "item/fileChange/requestApproval", &params)
        );

        let question = |turn: &str| {
            json!({"id": 0, "method": "item/tool/requestUserInput", "params": {
                "threadId": "thread", "turnId": turn,
                "questions": [{"id": "choice", "question": "Continue?",
                    "options": [{"label": "Yes"}, {"label": "No"}]}],
            }})
            .to_string()
        };
        let old = a.feed(&question("old"))[0]
            .request
            .clone()
            .expect("old question");
        a.handshake();
        let new = a.feed(&question("new"))[0]
            .request
            .clone()
            .expect("new question");
        assert_ne!(old, new);
        let resolved = a.feed(
            r#"{"method":"serverRequest/resolved","params":{"threadId":"thread","requestId":0}}"#,
        );
        assert_eq!(resolved[0].request.as_deref(), Some(new.as_str()));
        assert_eq!(
            resolved[0].event,
            Event::QuestionResolved { answered: false }
        );
        assert!(a.questions.is_empty());
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
        let request = ev[0].request.clone().expect("canonical request");
        assert_ne!(request, "req-a");
        let w = one_write(
            a.encode(Command::Approve {
                request,
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
    fn a_decision_the_server_did_not_offer_is_refused_and_the_request_stays_open() {
        let mut a = started(Mode::Ask);
        let ev = a.feed(
            r#"{"id":5,"method":"item/commandExecution/requestApproval","params":{"itemId":"i","availableDecisions":["accept","cancel"]}}"#,
        );
        let request = ev[0].request.clone().expect("canonical request");
        let approve = |decision| Command::Approve {
            request: request.clone(),
            decision,
            updated_input: None,
            message: None,
        };
        assert!(matches!(
            a.encode(approve(Decision::AllowForSession)),
            Err(AdapterError::Invalid(m)) if m.contains("acceptForSession")
        ));
        // Still pending: an offered decision goes through.
        let w = one_write(a.encode(approve(Decision::Allow)).expect("offered"));
        assert_eq!(w, json!({"id": 5, "result": {"decision": "accept"}}));
    }

    #[test]
    fn a_request_resolved_elsewhere_expires_the_approval() {
        let mut a = started(Mode::Ask);
        let requested = a.feed(&approval_request(
            "3",
            "item/commandExecution/requestApproval",
        ));
        let ev = a.feed(
            r#"{"method":"serverRequest/resolved","params":{"threadId":"th-1","requestId":3}}"#,
        );
        assert_eq!(ev[0].event, Event::ApprovalExpired);
        assert_eq!(ev[0].request, requested[0].request);
        assert_eq!(ev[0].item.as_deref(), Some("it-1"));
    }

    #[test]
    fn a_duplicate_after_answer_cannot_restore_an_approval_but_a_new_turn_can_reuse_its_id() {
        let mut a = started(Mode::Ask);
        a.feed(r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"tu-1"}}}"#);
        let frame = approval_request("0", "item/commandExecution/requestApproval");
        let request = a.feed(&frame)[0].request.clone().expect("request");
        let approve = |request| Command::Approve {
            request,
            decision: Decision::Allow,
            updated_input: None,
            message: None,
        };
        assert_eq!(
            one_write(a.encode(approve(request.clone())).expect("answer")),
            json!({"id": 0, "result": {"decision": "accept"}})
        );
        assert!(a.feed(&frame).is_empty(), "a late retransmission is inert");
        assert!(
            a.encode(approve(request)).is_err(),
            "no second wire response"
        );
        a.feed(r#"{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"tu-1","status":"completed"}}}"#);
        a.feed(r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"tu-2"}}}"#);
        let next = a.feed(&frame.replace("tu-1", "tu-2"))[0]
            .request
            .clone()
            .expect("next request");
        assert_eq!(
            one_write(a.encode(approve(next)).expect("new turn answer")),
            json!({"id": 0, "result": {"decision": "accept"}})
        );
    }

    #[test]
    fn a_terminal_turn_expires_its_requests_and_late_frames_cannot_touch_the_next_turn() {
        let mut a = started(Mode::Ask);
        a.feed(r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"tu-1"}}}"#);
        let approval = approval_request("0", "item/commandExecution/requestApproval");
        let question = r#"{"id":"0","method":"item/tool/requestUserInput","params":{"threadId":"th-1","turnId":"tu-1","itemId":"q","questions":[{"id":"choice","question":"Continue?","options":[{"label":"Yes"},{"label":"No"}]}]}}"#;
        let approval_key = a.feed(&approval)[0].request.clone().expect("approval");
        let question_key = a.feed(question)[0].request.clone().expect("question");
        let completed = r#"{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"tu-1","status":"interrupted"}}}"#;
        let ended = a.feed(completed);
        assert!(ended.iter().any(
            |e| e.request.as_ref() == Some(&approval_key) && e.event == Event::ApprovalExpired
        ));
        assert!(ended
            .iter()
            .any(|e| e.request.as_ref() == Some(&question_key)
                && e.event == Event::QuestionResolved { answered: false }));
        assert!(a.feed(&approval).is_empty());
        assert!(a.feed(question).is_empty());
        a.feed(r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"tu-2"}}}"#);
        let next_frame = approval.replace("tu-1", "tu-2");
        let next = a.feed(&next_frame)[0].request.clone().expect("new request");
        assert!(
            a.feed(completed).is_empty(),
            "old completion cannot finish the new turn"
        );
        assert!(a.approvals.contains_key(&next));
        let resolved =
            r#"{"method":"serverRequest/resolved","params":{"threadId":"th-1","requestId":0}}"#;
        assert_eq!(a.feed(resolved)[0].request.as_ref(), Some(&next));
        assert!(a.feed(resolved).is_empty(), "resolution is idempotent");
        assert!(
            a.feed(&next_frame).is_empty(),
            "expired duplicate cannot restore the request"
        );
    }

    #[test]
    fn late_unseen_requests_for_a_completed_turn_are_inert_before_and_during_the_next_turn() {
        let mut a = started(Mode::Ask);
        a.feed(r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"tu-1"}}}"#);
        a.feed(r#"{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"tu-1","status":"completed"}}}"#);
        let old_approval = approval_request("0", "item/commandExecution/requestApproval");
        let old_question = json!({"id": "0", "method": "item/tool/requestUserInput", "params": {
            "threadId": "th-1", "turnId": "tu-1", "itemId": "late-question",
            "questions": [{"id": "choice", "question": "Continue?", "options": [{"label": "Yes"}]}],
        }})
        .to_string();
        for late in [&old_approval, &old_question] {
            assert!(
                a.feed(late).is_empty(),
                "an unseen request cannot revive a completed turn"
            );
        }
        a.encode(Command::Prompt {
            text: "next".into(),
        })
        .expect("prompt");
        for late in [&old_approval, &old_question] {
            assert!(a.feed(late).is_empty(), "pending start is protected too");
        }
        a.feed(r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"tu-2"}}}"#);
        for late in [&old_approval, &old_question] {
            assert!(
                a.feed(late).is_empty(),
                "an old request cannot interrupt the new turn"
            );
        }
        let next = a.feed(&old_approval.replace("tu-1", "tu-2"));
        assert!(
            matches!(next[0].event, Event::ApprovalRequested { .. }),
            "raw id reuse remains valid"
        );
        assert_eq!(
            one_write(
                a.encode(Command::Approve {
                    request: next[0].request.clone().expect("current request"),
                    decision: Decision::Allow,
                    updated_input: None,
                    message: None,
                })
                .expect("approve current")
            ),
            json!({"id": 0, "result": {"decision": "accept"}})
        );
    }

    #[test]
    fn late_duplicate_completion_during_a_pending_start_keeps_the_next_turn_effective_policy() {
        let mut a = started(Mode::Ask);
        a.feed(r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"tu-1"}}}"#);
        let completed = r#"{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"tu-1","status":"completed"}}}"#;
        a.feed(completed);
        a.encode(Command::Prompt {
            text: "next".into(),
        })
        .expect("next prompt");
        a.encode(Command::SetMode {
            mode: Mode::AcceptEdits,
        })
        .expect("defer mode");
        a.drain_outbox();
        assert!(
            a.feed(completed).is_empty(),
            "a duplicate cannot finish the pending turn"
        );
        a.feed(r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"tu-2"}}}"#);
        a.encode(Command::SetMode { mode: Mode::Plan })
            .expect("defer again");
        assert!(
            a.drain_outbox().events.iter().any(|e| e.event
                == Event::ModeChangeDeferred {
                    requested: Mode::Plan,
                    effective: Mode::Ask,
                }),
            "the pending turn retains the policy actually sent on its wire"
        );
    }

    #[test]
    fn request_and_turn_duplicate_protection_is_bounded_on_a_long_lived_connection() {
        let mut a = started(Mode::Ask);
        a.feed(r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"many-requests"}}}"#);
        let mut last = String::new();
        let mut first = None;
        for item in 0..1100 {
            last = json!({"id": 0, "method": "item/commandExecution/requestApproval", "params": {
                "threadId": "th-1", "turnId": "many-requests", "itemId": format!("item-{item}"), "command": "printf synthetic",
            }}).to_string();
            let request = a.feed(&last)[0]
                .request
                .clone()
                .expect("new item uses the same raw id");
            if item == 0 {
                first = Some((last.clone(), request.clone()));
            }
            a.encode(Command::Approve {
                request,
                decision: Decision::Allow,
                updated_input: None,
                message: None,
            })
            .expect("approve");
        }
        assert_eq!(
            a.settled_requests.len(),
            1100,
            "active identities cannot be evicted"
        );
        assert!(
            a.feed(&last).is_empty(),
            "settled requests stay protected throughout their active turn"
        );
        let (first_frame, first_key) = first.expect("first request");
        let current = a.feed(&last.replace("item-1099", "current-item"))[0]
            .request
            .clone()
            .expect("pending current item");
        assert!(
            a.feed(&first_frame).is_empty(),
            "the first answered request cannot revive after 1,024 answers"
        );
        assert!(
            a.encode(Command::Approve {
                request: first_key,
                decision: Decision::Allow,
                updated_input: None,
                message: None
            })
            .is_err(),
            "an old answer cannot approve current wire id 0"
        );
        assert!(a
            .encode(Command::Approve {
                request: current,
                decision: Decision::Allow,
                updated_input: None,
                message: None
            })
            .is_ok());
        a.feed(r#"{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"many-requests","status":"completed"}}}"#);
        assert!(
            a.settled_requests.is_empty(),
            "terminal scope releases active-turn identities"
        );
        for turn in 0..TURN_TOMBSTONE_LIMIT + 10 {
            let id = format!("turn-{turn}");
            a.feed(&json!({"method": "turn/started", "params": {"threadId": "th-1", "turn": {"id": id}}}).to_string());
            a.feed(&json!({"method": "turn/completed", "params": {"threadId": "th-1", "turn": {"id": id, "status": "completed"}}}).to_string());
        }
        assert_eq!(a.ended_turns.len(), TURN_TOMBSTONE_LIMIT);
        assert_eq!(a.ended_turn_order.len(), TURN_TOMBSTONE_LIMIT);
        a.encode(Command::Prompt {
            text: "next".into(),
        })
        .expect("next prompt");
        let last_completed =
            json!({"method": "turn/completed", "params": {"threadId": "th-1", "turn": {
                "id": format!("turn-{}", TURN_TOMBSTONE_LIMIT + 9), "status": "completed",
            }}})
            .to_string();
        assert!(
            a.feed(&last_completed).is_empty(),
            "recent terminal turns stay protected at the cap"
        );
        a.feed(
            r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"new-turn"}}}"#,
        );
        assert!(
            matches!(
                a.feed(&last.replace("many-requests", "new-turn"))[0].event,
                Event::ApprovalRequested { .. }
            ),
            "the cap never turns a new scoped request into a duplicate"
        );
    }

    #[test]
    fn a_child_request_survives_parent_completion_and_retires_only_its_own_scope() {
        let mut a = started(Mode::Ask);
        a.feed(r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"tu-1"}}}"#);
        a.feed(r#"{"method":"thread/started","params":{"thread":{"id":"child","parentThreadId":"th-1","status":{"type":"active"}}}}"#);
        a.feed(r#"{"method":"turn/started","params":{"threadId":"child","turn":{"id":"tu-1"}}}"#);
        let parent = approval_request("0", "item/commandExecution/requestApproval");
        let child =
            approval_request("1", "item/commandExecution/requestApproval").replace("th-1", "child");
        let parent_key = a.feed(&parent)[0].request.clone().expect("parent");
        let child_key = a.feed(&child)[0].request.clone().expect("child");
        assert!(
            a.feed(&child.replace("tu-1", "unseen-old-child-turn"))
                .is_empty(),
            "known child active scope rejects explicit old turns too"
        );
        a.encode(Command::Approve {
            request: child_key,
            decision: Decision::Allow,
            updated_input: None,
            message: None,
        })
        .expect("answer child");
        let completed = r#"{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"tu-1","status":"completed"}}}"#;
        assert!(a
            .feed(completed)
            .iter()
            .any(|e| e.event == Event::ApprovalExpired && e.request.as_ref() == Some(&parent_key)));
        assert_eq!(
            a.settled_requests.len(),
            1,
            "parent terminal must retain child's settled request"
        );
        assert!(
            a.feed(&child).is_empty(),
            "child duplicate is still protected"
        );
        let question = json!({"id": "q", "method": "item/tool/requestUserInput", "params": {
            "threadId": "child", "turnId": "tu-1", "itemId": "child-q",
            "questions": [{"id": "choice", "question": "Continue?", "options": [{"label": "Yes"}]}],
        }})
        .to_string();
        let key = a.feed(&question)[0]
            .request
            .clone()
            .expect("child question remains live");
        assert!(a
            .encode(Command::Answer {
                request: key,
                answers: json!({"Continue?": "Yes"})
            })
            .is_ok());
        a.feed(&completed.replace("th-1", "child"));
        assert!(
            a.settled_requests.is_empty(),
            "child terminal releases only child scope"
        );
        assert!(
            a.feed(&question.replace("child-q", "unseen-child-q"))
                .is_empty(),
            "late child request cannot revive after terminal"
        );
    }

    #[test]
    fn late_plan_notifications_cannot_revive_an_ended_turn_or_change_the_next_plan() {
        let mut a = started(Mode::Ask);
        a.feed(r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"tu-1"}}}"#);
        let plan = json!({"method": "turn/plan/updated", "params": {
            "threadId": "th-1", "turnId": "tu-1", "plan": [{"step": "Current work", "status": "pending"}],
        }}).to_string();
        assert!(matches!(a.feed(&plan)[0].event, Event::PlanUpdated { .. }));
        a.feed(r#"{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"tu-1","status":"completed"}}}"#);
        assert!(a.feed(&plan).is_empty(), "ended plan remains history");
        a.encode(Command::Prompt {
            text: "next".into(),
        })
        .expect("next prompt");
        assert!(
            a.feed(&plan).is_empty(),
            "pending next turn cannot revive old plan"
        );
        a.feed(r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"tu-2"}}}"#);
        assert!(a.feed(&plan).is_empty(), "new turn cannot receive old plan");
        assert!(
            a.feed(&plan.replace("tu-1", "unknown-turn")).is_empty(),
            "explicit non-current plan is refused"
        );
        assert!(
            matches!(
                a.feed(&plan.replace("tu-1", "tu-2"))[0].event,
                Event::PlanUpdated { .. }
            ),
            "current plan remains usable"
        );
    }

    #[test]
    fn closing_a_child_expires_its_requests_and_unscoped_ids_use_the_child_turn() {
        let mut a = started(Mode::Ask);
        a.feed(
            r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"parent-turn"}}}"#,
        );
        a.feed(r#"{"method":"thread/started","params":{"thread":{"id":"child","parentThreadId":"th-1","status":{"type":"active"}}}}"#);
        a.feed(
            r#"{"method":"turn/started","params":{"threadId":"child","turn":{"id":"child-turn"}}}"#,
        );
        let parent = json!({"id": 0, "method": "item/commandExecution/requestApproval", "params": {
            "threadId": "th-1", "command": "printf parent",
        }})
        .to_string();
        let parent_key = a.feed(&parent)[0].request.clone().expect("parent request");
        let child = json!({"id": 1, "method": "item/commandExecution/requestApproval", "params": {
            "threadId": "child", "itemId": "child-item", "command": "printf child",
        }})
        .to_string();
        let child_key = a.feed(&child)[0].request.clone().expect("child request");
        assert!(child_key.contains("child-turn") && !child_key.contains("parent-turn"));
        a.encode(Command::Approve {
            request: child_key,
            decision: Decision::Allow,
            updated_input: None,
            message: None,
        })
        .expect("settled child request");
        let pending = a.feed(&child.replace("child-item", "child-pending"))[0]
            .request
            .clone()
            .expect("pending child");
        let question = json!({"id": "q", "method": "item/tool/requestUserInput", "params": {
            "threadId": "child", "itemId": "child-q", "questions": [{"id": "choice", "question": "Continue?", "options": [{"label": "Yes"}]}],
        }}).to_string();
        let question_key = a.feed(&question)[0]
            .request
            .clone()
            .expect("child question");
        let closed = a.feed(r#"{"method":"thread/closed","params":{"threadId":"child"}}"#);
        assert!(closed
            .iter()
            .any(|e| e.request.as_ref() == Some(&pending) && e.event == Event::ApprovalExpired));
        assert!(closed
            .iter()
            .any(|e| e.request.as_ref() == Some(&question_key)
                && e.event == Event::QuestionResolved { answered: false }));
        assert!(
            a.settled_requests.is_empty(),
            "closed child releases its settled scopes"
        );
        assert!(a
            .feed(&child.replace("child-item", "unseen-after-close"))
            .is_empty());
        assert!(a.feed(&question).is_empty());
        assert!(
            a.encode(Command::Approve {
                request: parent_key.clone(),
                decision: Decision::Allow,
                updated_input: None,
                message: None
            })
            .is_ok(),
            "parent remains answerable"
        );
        assert!(
            a.feed(&parent).is_empty(),
            "unscoped parent duplicate uses its active turn"
        );
        a.feed(r#"{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"parent-turn","status":"completed"}}}"#);
        assert!(a.settled_requests.is_empty());
        a.feed(r#"{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"next-parent-turn"}}}"#);
        assert_ne!(
            a.feed(&parent)[0].request.as_ref(),
            Some(&parent_key),
            "unscoped raw id reuse follows the next native turn"
        );
    }

    #[test]
    fn request_user_input_becomes_a_question_card_and_answers_with_question_ids() {
        let mut a = started(Mode::Ask);
        let ev = a.feed(
            r#"{"id":9,"method":"item/tool/requestUserInput","params":{"itemId":"ask-1","questions":[{"id":"confirm","header":"Preview","question":"Start the preview?","options":[{"label":"Allow","description":"Launch it"},{"label":"Cancel","description":"Do not launch it"}]}]}}"#,
        );
        assert!(matches!(
            &ev[0].event,
            Event::QuestionRequested { questions }
                if questions[0].id == "confirm" && questions[0].options[0].label == "Allow"
        ));
        let request = ev[0].request.clone().expect("canonical request");
        assert_ne!(request, "9");
        let w = one_write(
            a.encode(Command::Answer {
                request: request.clone(),
                answers: json!({"Start the preview?": "Allow"}),
            })
            .expect("answer"),
        );
        assert_eq!(
            w,
            json!({"id": 9, "result": {"answers": {"confirm": {"answers": ["Allow"]}}}})
        );
        assert!(matches!(
            a.encode(Command::Answer {
                request,
                answers: json!({"Start the preview?": "Allow"}),
            }),
            Err(AdapterError::Invalid(_))
        ));
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
            Err(AdapterError::Invalid(_))
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
            matches!(&ev[0].event, Event::ItemStarted { kind: ItemKind::WebSearch, title, input: Some(i), .. }
            if title == "rust" && i["query"] == "rust")
        );

        let ev = a.feed(&item(
            "webSearch",
            r#","action":{"type":"search","queries":["Rust 1.91","Rust release notes"]}"#,
        ));
        assert!(
            matches!(&ev[0].event, Event::ItemStarted { kind: ItemKind::WebSearch, title, input: Some(i), .. }
            if title == "Rust 1.91 (+1 more)" && i["query"] == "Rust 1.91" && i["queries"][1] == "Rust release notes")
        );

        let ev = a.feed(&item("somethingNew", ""));
        assert!(
            matches!(&ev[0].event, Event::ItemStarted { kind: ItemKind::Tool, title, .. } if title == "somethingNew")
        );

        // The app shows its own prompt; Codex's echo of it is dropped.
        assert!(a.feed(&item("userMessage", r#","content":[]"#)).is_empty());
    }

    #[test]
    fn completed_web_search_refines_a_started_card_with_its_action() {
        let mut a = started(Mode::Ask);
        let started = a.feed(
            r#"{"method":"item/started","params":{"threadId":"th-1","turnId":"tu","startedAtMs":1,"item":{"type":"webSearch","id":"ws-1","status":"inProgress"}}}"#,
        );
        assert!(
            matches!(&started[0].event, Event::ItemStarted { title, input: None, .. } if title == "web search")
        );

        let done = a.feed(
            r#"{"method":"item/completed","params":{"threadId":"th-1","turnId":"tu","completedAtMs":2,"item":{"type":"webSearch","id":"ws-1","status":"completed","action":{"type":"open_page","url":"https://doc.rust-lang.org"}}}}"#,
        );
        assert!(
            matches!(&done[0].event, Event::ItemStarted { title, input: Some(i), .. }
            if title == "Open https://doc.rust-lang.org" && i["url"] == "https://doc.rust-lang.org")
        );
        assert!(matches!(
            done[1].event,
            Event::ItemCompleted {
                status: ItemStatus::Completed,
                ..
            }
        ));
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
        let second = a.feed(&done.replace("\"tu\"", "\"tu-2\""));
        assert!(
            matches!(&second[0].event, Event::TurnCompleted { usage: Some(u), .. }
            if u.input_tokens == 30 && u.output_tokens == 15)
        );
        // No usage reported in a turn: none claimed.
        let third = a.feed(&done.replace("\"tu\"", "\"tu-3\""));
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
        a.feed(
            &approval_request("4", "item/commandExecution/requestApproval").replace("tu-1", "tu"),
        );
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
        assert!(c.questions);
        assert!(!c.file_suggestions && !c.mcp_panel && !c.settings_panel && !c.usage);
        assert_eq!(c.compact_command.as_deref(), Some("/compact"));
    }
}
