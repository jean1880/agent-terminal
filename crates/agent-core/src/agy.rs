//! Antigravity (`agy`) adapter: a pure state machine over `agy`'s own stream-json schema
//! (`event` = `init` | `step_update` | `result`). Verified against agy 1.3.0 (see
//! `tests/fixtures/README.md` and the Phase 0 table in the v3 plan).
//!
//! No process, file or socket I/O happens here. Approvals do NOT go through this adapter: agy
//! runs with `--dangerously-skip-permissions` and the app's PreToolUse hook
//! (`agent-terminal --approval-hook`, see [`crate::approval`]) is the gate, so `Approve` and
//! `Answer` are unsupported on purpose.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::adapter::{
    Action, Adapter, AdapterError, Command, Control, Driver, Mode, OpenSession, OpenSessionDelta,
};
use crate::caps::Capabilities;
use crate::event::{Envelope, Event, ItemKind, ItemStatus, StreamKind, TurnState, Usage};

/// Read-only slash commands the CLI answers itself. Inside a stream-json session they fail and
/// the process exits, so they are never forwarded: each runs as its own `agy -p /cmd` process.
const SIDE_COMMANDS: &[&str] = &[
    "/model",
    "/usage",
    "/effort",
    "/credits",
    "/config",
    "/skills",
    "/help",
    "/hooks",
    "/permissions",
    "/changelog",
];

/// Source of the per-adapter item-id prefix used until the conversation id is known.
static ADAPTER_SEQ: AtomicU64 = AtomicU64::new(0);

/// The flag the adapter alone decides on (it is the hook's fail-safe, see [`Adapter::argv`]).
const SKIP_PERMISSIONS: &str = "--dangerously-skip-permissions";

/// Fragment of the error agy puts on a step the PreToolUse hook refused.
const HOOK_DENIAL: &str = "denied by pre-tool hook";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SideKind {
    /// `-p /cmd --output-format json`: stdout is a JSON document.
    Json,
    /// `agy models`: plain TSV, `id<TAB>display`.
    Models,
    /// `-p /usage --output-format json`: a JSON document whose buckets also become a
    /// `QuotaUpdated`. agy reports no quota during a turn, so the HOST must request
    /// `Control::Usage` after each agy `TurnCompleted` to keep the usage indicator current.
    Usage,
}

pub struct AgyAdapter {
    program: String,
    caps: Capabilities,
    /// Native conversation id, learned from `init`; used to resume after a respawn.
    conversation: Option<String>,
    model: Option<String>,
    turn_open: bool,
    /// A prompt was written and its turn has not opened yet. agy prints `init` as soon as it
    /// starts, prompt or not, so `init` opens a turn only when one is waiting.
    prompt_pending: bool,
    /// Ids of items started and not yet completed, in start order.
    open_items: Vec<String>,
    side_seq: u64,
    side_kinds: HashMap<String, SideKind>,
    /// An interrupt or respawn was requested, so the coming exit is expected.
    exit_expected: bool,
    /// Item-id prefix used while the conversation id is unknown (unique per adapter).
    fallback_prefix: String,
    /// Accumulated text of each open response item, flushed as a snapshot on completion.
    resp_text: HashMap<String, String>,
}

impl Default for AgyAdapter {
    fn default() -> Self {
        Self::new("agy")
    }
}

impl AgyAdapter {
    /// `program` is the binary used for side processes; [`Adapter::argv`] uses the session's own.
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            caps: Capabilities::agy(),
            conversation: None,
            model: None,
            turn_open: false,
            prompt_pending: false,
            open_items: Vec::new(),
            side_seq: 0,
            side_kinds: HashMap::new(),
            exit_expected: false,
            fallback_prefix: format!("a{}", ADAPTER_SEQ.fetch_add(1, Ordering::Relaxed)),
            resp_text: HashMap::new(),
        }
    }

    /// Item id for step `index`: `<conv8>:<kind>-<n>` (first 8 chars of the conversation id,
    /// so ids stay unique across conversations), else the adapter's own prefix. An item already
    /// open under an earlier prefix keeps it.
    fn item_id(&self, kind: &str, index: u64) -> String {
        let suffix = format!(":{kind}-{index}");
        if let Some(open) = self.open_items.iter().find(|i| i.ends_with(&suffix)) {
            return open.clone();
        }
        let prefix: String = match &self.conversation {
            Some(c) if !c.is_empty() => c.chars().take(8).collect(),
            _ => self.fallback_prefix.clone(),
        };
        format!("{prefix}{suffix}")
    }

    /// The whole text of a response item, as a snapshot so the store scrubs it in one piece (a
    /// secret split across deltas is only caught there).
    fn flush_text(&mut self, out: &mut Vec<Envelope>, id: &str) {
        if let Some(text) = self.resp_text.remove(id).filter(|t| !t.is_empty()) {
            out.push(
                Envelope::new(Event::ContentSnapshot {
                    stream: StreamKind::Assistant,
                    text,
                })
                .item(id),
            );
        }
    }

    fn side(&mut self, id: String, kind: SideKind, args: &[&str]) -> Action {
        self.side_kinds.insert(id.clone(), kind);
        let mut argv = vec![self.program.clone()];
        argv.extend(args.iter().map(|s| (*s).to_owned()));
        Action::SideProcess { id, argv }
    }

    fn respawn(&mut self, model: Option<String>, mode: Option<Mode>) -> Action {
        self.exit_expected = true;
        Action::Respawn(OpenSessionDelta {
            model,
            effort: None,
            mode,
            resume: self.conversation.clone(),
        })
    }

    fn start_item(&mut self, out: &mut Vec<Envelope>, id: &str, kind: ItemKind, title: String) {
        self.start_item_with(out, id, kind, title, None);
    }

    fn start_item_with(
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
        self.flush_text(out, id);
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
        let open = std::mem::take(&mut self.open_items);
        for id in open {
            self.flush_text(out, &id);
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

    fn ensure_turn(&mut self, out: &mut Vec<Envelope>) {
        self.prompt_pending = false;
        if !self.turn_open {
            self.turn_open = true;
            out.push(Envelope::new(Event::TurnStarted {
                model: self.model.clone(),
            }));
        }
    }

    fn on_init(&mut self, frame: &Frame, out: &mut Vec<Envelope>) {
        let init = frame.init.clone().unwrap_or_default();
        if let Some(id) = &frame.conversation_id {
            self.conversation = Some(id.clone());
        }
        if init.model.is_some() {
            self.model = init.model.clone();
        }
        out.push(env(Event::SessionStarted {
            native_id: frame.conversation_id.clone().unwrap_or_default(),
            model: init.model,
            cwd: init.cwd,
        }));
        // A prompt written with the spawn is already a turn; an idle spawn (a thread opened or
        // switched to agy before anything was asked) is not, and must not show as working.
        if self.prompt_pending {
            self.ensure_turn(out);
        }
    }

    fn on_step(&mut self, step: &Step, out: &mut Vec<Envelope>) {
        if let Some(id) = &step.conversation_id {
            self.conversation = Some(id.clone());
        }
        // A missing index collapses onto 0; agy always sets it in practice.
        let index = step.step_index.unwrap_or(0);
        match step.step_type.as_str() {
            "user_input" => {
                if step.state == "DONE" {
                    self.ensure_turn(out);
                }
            }
            "agent_response" => self.on_response(step, index, out),
            "tool" => self.on_tool(step, index, out),
            _ if step.tool_name.is_some() => self.on_tool(step, index, out),
            _ => out.push(env(Event::Unknown)),
        }
    }

    fn on_response(&mut self, step: &Step, index: u64, out: &mut Vec<Envelope>) {
        let id = self.item_id("resp", index);
        let text = step.text_delta.as_deref().filter(|t| !t.is_empty());
        if let Some(text) = text {
            self.start_item(out, &id, ItemKind::AssistantMessage, "assistant".to_owned());
            self.resp_text.entry(id.clone()).or_default().push_str(text);
            out.push(
                Envelope::new(Event::ContentDelta {
                    stream: StreamKind::Assistant,
                    text: text.to_owned(),
                })
                .item(&id),
            );
        }
        // A thinking-only response step has no text and never opened an item: nothing to close.
        if !self.open_items.contains(&id) {
            return;
        }
        match step.state.as_str() {
            "DONE" => self.complete_item(out, &id, ItemStatus::Completed, None, None),
            "ERROR" => self.complete_item(out, &id, ItemStatus::Failed, None, None),
            _ => {}
        }
    }

    fn on_tool(&mut self, step: &Step, index: u64, out: &mut Vec<Envelope>) {
        let id = self.item_id("step", index);
        let info = step.tool_info.clone().unwrap_or_default();
        let name = step
            .tool_name
            .clone()
            .or(info.name.clone())
            .unwrap_or_else(|| "tool".to_owned());
        let params = info.parameters.clone();
        self.start_item_with(
            out,
            &id,
            tool_kind(&name),
            tool_title(&name, params.as_ref()),
            params,
        );
        match step.state.as_str() {
            "DONE" => {
                let output = info.output.as_ref().map(value_text);
                self.complete_item(out, &id, ItemStatus::Completed, output, None);
            }
            "ERROR" => {
                let message = info
                    .error
                    .as_ref()
                    .and_then(error_message)
                    .unwrap_or_else(|| "tool error".to_owned());
                let status = if message.contains(HOOK_DENIAL) {
                    ItemStatus::Declined
                } else {
                    ItemStatus::Failed
                };
                self.complete_item(out, &id, status, None, Some(message));
            }
            _ => {}
        }
    }

    fn on_result(&mut self, result: &ResultFrame, out: &mut Vec<Envelope>) {
        if let Some(id) = &result.conversation_id {
            self.conversation = Some(id.clone());
        }
        // Anything still open when the turn ends was cut short.
        self.close_all(out);
        if !result.denied_actions.is_empty() {
            let names: Vec<String> = result.denied_actions.iter().map(denied_name).collect();
            out.push(env(Event::Notice {
                text: format!("Denied actions: {}", names.join(", ")),
            }));
        }
        let error = result.error.clone().filter(|e| !e.is_empty());
        let state = if error.as_deref() == Some("interrupted") {
            TurnState::Interrupted
        } else if result.status.eq_ignore_ascii_case("SUCCESS") && error.is_none() {
            TurnState::Completed
        } else {
            TurnState::Failed
        };
        let error = match state {
            TurnState::Failed => Some(error.unwrap_or_else(|| {
                format!(
                    "agy turn ended with status {}",
                    result.status.to_lowercase()
                )
            })),
            _ => None,
        };
        self.turn_open = false;
        out.push(env(Event::TurnCompleted {
            state,
            usage: result.usage.as_ref().map(Usage::from),
            cost_usd: None,
            error,
        }));
    }
}

impl Adapter for AgyAdapter {
    fn driver(&self) -> Driver {
        Driver::Agy
    }

    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    /// Fail-safe: `--dangerously-skip-permissions` makes the app's PreToolUse hook the ONLY gate,
    /// so it is passed solely when the hook is installed (`approval_hook`). Plan never needs it.
    /// For the same reason `session.extra_args` can never carry it
    /// (`--dangerously-skip-permissions` or `--dangerously-skip-permissions=…`): those are dropped
    /// silently (no I/O here) and the adapter alone decides.
    ///
    /// The mode is agy's own `--mode` flag. Without the hook, agy cannot ask before acting, so Ask
    /// is sent as `--mode plan` (the session normally never asks for it). Verified live: headless
    /// agy refuses shell commands without the skip flag in every mode, but applies file edits in
    /// every mode, Plan included (it plans, then edits).
    fn argv(&self, session: &OpenSession) -> Vec<String> {
        let mut argv = vec![session.program.clone()];
        argv.extend(
            session
                .extra_args
                .iter()
                .filter(|a| {
                    a.as_str() != SKIP_PERMISSIONS
                        && !a.starts_with(&format!("{SKIP_PERMISSIONS}="))
                })
                .cloned(),
        );
        argv.extend(
            [
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
            ]
            .map(str::to_owned),
        );
        if let Some(model) = &session.model {
            argv.push("--model".to_owned());
            argv.push(model.clone());
        }
        if let Some(id) = &session.resume {
            argv.push("--conversation".to_owned());
            argv.push(id.clone());
        }
        let plan = || ["--mode", "plan"].map(str::to_owned);
        let accept_edits = || ["--mode", "accept-edits"].map(str::to_owned);
        match (session.mode, session.approval_hook) {
            (Mode::Plan, _) | (Mode::Ask, false) => argv.extend(plan()),
            (Mode::AcceptEdits, false) => argv.extend(accept_edits()),
            (Mode::AcceptEdits, true) => {
                argv.extend(accept_edits());
                argv.push("--dangerously-skip-permissions".to_owned());
            }
            (Mode::Ask, true) => argv.push("--dangerously-skip-permissions".to_owned()),
        }
        argv
    }

    fn handshake(&mut self) -> Vec<String> {
        Vec::new()
    }

    fn encode(&mut self, command: Command) -> Result<Vec<Action>, AdapterError> {
        match command {
            Command::Prompt { text } => {
                // The first token is matched case-insensitively after leading whitespace. Any
                // other `/word` is forwarded as agent text (custom commands and skills).
                let trimmed = text.trim();
                let first = trimmed.split_whitespace().next().unwrap_or("");
                if SIDE_COMMANDS.contains(&first.to_ascii_lowercase().as_str()) {
                    self.side_seq += 1;
                    let id = format!("side-{}", self.side_seq);
                    let command =
                        format!("{}{}", first.to_ascii_lowercase(), &trimmed[first.len()..]);
                    let action = self.side(
                        id,
                        SideKind::Json,
                        &["-p", &command, "--output-format", "json"],
                    );
                    return Ok(vec![action]);
                }
                let line = json!({
                    "event": "user",
                    "message": {"role": "user", "content": text},
                });
                self.prompt_pending = true;
                Ok(vec![Action::Write(vec![line.to_string()])])
            }
            Command::Interrupt => {
                // SIGINT ends the process (caps.interrupt_keeps_process == false).
                self.exit_expected = true;
                Ok(vec![Action::Interrupt])
            }
            // `agy --help` lists `--effort`, but `agy models` only offers ids with the effort
            // baked in (`gemini-3.1-pro-high`) and a base id is not known to be accepted by
            // `--model`, so the caller passes the composed id (`CatalogModel::model_id_for`) and
            // `effort` is not sent separately. Upgrade path: pass `--effort` once a base id is
            // verified to work.
            Command::SetModel { model, .. } => Ok(vec![self.respawn(Some(model), None)]),
            Command::SetMode { mode } => Ok(vec![self.respawn(None, Some(mode))]),
            Command::Approve { .. } | Command::Answer { .. } => Err(AdapterError::Unsupported(
                "agy approvals go through the approval hook, not the adapter",
            )),
            Command::Control { id, control } => {
                let action = match control {
                    Control::ListModels => self.side(id, SideKind::Models, &["models"]),
                    Control::Usage => self.side(
                        id,
                        SideKind::Usage,
                        &["-p", "/usage", "--output-format", "json"],
                    ),
                    Control::GetSettings => self.side(
                        id,
                        SideKind::Json,
                        &["-p", "/config", "--output-format", "json"],
                    ),
                    _ => {
                        return Err(AdapterError::Unsupported(
                            "this control is not offered by agy",
                        ))
                    }
                };
                Ok(vec![action])
            }
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
        let Ok(frame) = serde_json::from_value::<Frame>(raw.clone()) else {
            return vec![unknown(raw)];
        };
        let mut out = Vec::new();
        match frame.event.as_str() {
            "init" => self.on_init(&frame, &mut out),
            "step_update" => match &frame.step_update {
                Some(step) => self.on_step(step, &mut out),
                None => out.push(env(Event::Unknown)),
            },
            "result" => match &frame.result {
                Some(result) => self.on_result(result, &mut out),
                None => out.push(env(Event::Unknown)),
            },
            _ => out.push(env(Event::Unknown)),
        }
        if let Some(first) = out.first_mut() {
            first.raw = Some(raw);
        }
        out
    }

    fn feed_stderr(&mut self, line: &str) -> Vec<Envelope> {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("error:") {
            vec![env(Event::Error {
                message: rest.trim().to_owned(),
            })]
        } else if let Some(rest) = line.strip_prefix("warning:") {
            vec![env(Event::Notice {
                text: rest.trim().to_owned(),
            })]
        } else {
            Vec::new()
        }
    }

    fn feed_side(&mut self, id: &str, stdout: &str, success: bool) -> Vec<Envelope> {
        let kind = self.side_kinds.remove(id).unwrap_or(SideKind::Json);
        let mut quota = None;
        let event = if !success {
            let detail = stdout.trim().lines().next().unwrap_or("").trim();
            Event::ControlResult {
                ok: None,
                error: Some(if detail.is_empty() {
                    "agy side command failed".to_owned()
                } else {
                    detail.to_owned()
                }),
            }
        } else {
            match kind {
                SideKind::Models => Event::ControlResult {
                    ok: Some(parse_models(stdout)),
                    error: None,
                },
                SideKind::Json => Event::ControlResult {
                    ok: Some(parse_side_json(stdout)),
                    error: None,
                },
                SideKind::Usage => {
                    let doc = parse_side_json(stdout);
                    let windows = crate::quota::agy_usage(&doc);
                    if !windows.is_empty() {
                        quota = Some(Event::QuotaUpdated {
                            account: None,
                            windows,
                        });
                    }
                    Event::ControlResult {
                        ok: Some(doc),
                        error: None,
                    }
                }
            }
        };
        let mut out = vec![Envelope::new(event).request(id)];
        out.extend(quota.map(Envelope::new));
        out
    }

    fn on_exit(&mut self, code: Option<i32>) -> Vec<Envelope> {
        let mut out = Vec::new();
        self.close_all(&mut out);
        let expected = std::mem::take(&mut self.exit_expected);
        // A prompt that never got its turn (agy died before `init`) failed all the same.
        let pending = std::mem::take(&mut self.prompt_pending);
        if std::mem::take(&mut self.turn_open) || pending {
            out.push(env(Event::TurnCompleted {
                state: if expected {
                    TurnState::Interrupted
                } else {
                    TurnState::Failed
                },
                usage: None,
                cost_usd: None,
                error: (!expected).then(|| format!("agy exited unexpectedly (code {code:?})")),
            }));
        }
        out.push(env(Event::SessionExited { code, expected }));
        out
    }
}

fn env(event: Event) -> Envelope {
    Envelope::new(event)
}

fn unknown(raw: Value) -> Envelope {
    Envelope::new(Event::Unknown).raw(raw)
}

fn tool_kind(name: &str) -> ItemKind {
    match name {
        "run_command" | "send_command_input" => ItemKind::Command,
        "replace_file_content"
        | "multi_replace_file_content"
        | "write_to_file"
        | "sed_file"
        | "notebook_edit" => ItemKind::FileChange,
        "view_file" | "list_dir" | "grep_search" | "find_by_name" => ItemKind::FileRead,
        "search_web" | "read_url_content" => ItemKind::WebSearch,
        "call_mcp_tool" => ItemKind::McpTool,
        "start_subagent" | "invoke_subagent" => ItemKind::Subagent,
        _ => ItemKind::Tool,
    }
}

/// The most telling parameter as the card title, else the tool name.
fn tool_title(name: &str, params: Option<&Value>) -> String {
    const KEYS: &[&str] = &[
        "CommandLine",
        "TargetFile",
        "AbsolutePath",
        "DirectoryPath",
        "SearchPath",
        "Query",
        "Url",
    ];
    params
        .and_then(|p| {
            KEYS.iter()
                .find_map(|k| p.get(*k).and_then(Value::as_str).filter(|s| !s.is_empty()))
        })
        .map_or_else(|| name.to_owned(), str::to_owned)
}

fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn error_message(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        other => other
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

fn denied_name(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => ["name", "tool_name", "tool"]
            .iter()
            .find_map(|k| other.get(*k).and_then(Value::as_str))
            .map_or_else(|| other.to_string(), str::to_owned),
    }
}

/// `agy models`: `id<TAB>display` per line. A line without a tab is an id with no display name.
fn parse_models(stdout: &str) -> Value {
    let models: Vec<Value> = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| match l.split_once('\t') {
            Some((id, display)) => json!({"id": id.trim(), "display": display.trim()}),
            None => json!({"id": l, "display": l}),
        })
        .collect();
    Value::Array(models)
}

/// The JSON document of a side command; the last JSON line wins, plain text is kept as a string.
fn parse_side_json(stdout: &str) -> Value {
    let text = stdout.trim();
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        return v;
    }
    text.lines()
        .rev()
        .find_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .unwrap_or_else(|| Value::String(text.to_owned()))
}

// ---- native frame models: lenient, every field optional ----

#[derive(Debug, Default, Deserialize)]
struct Frame {
    #[serde(default)]
    event: String,
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    init: Option<Init>,
    #[serde(default)]
    step_update: Option<Step>,
    #[serde(default)]
    result: Option<ResultFrame>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Init {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Step {
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    state: String,
    #[serde(default)]
    step_index: Option<u64>,
    #[serde(default)]
    step_type: String,
    #[serde(default)]
    text_delta: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_info: Option<ToolInfo>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ToolInfo {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    output: Option<Value>,
    #[serde(default)]
    parameters: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
struct ResultFrame {
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    status: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    usage: Option<AgyUsage>,
    #[serde(default)]
    denied_actions: Vec<Value>,
}

#[derive(Debug, Default, Deserialize)]
struct AgyUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    thinking_tokens: u64,
    #[serde(default)]
    cache_read_tokens: u64,
}

impl From<&AgyUsage> for Usage {
    /// agy's `total_tokens` equals input + output, so thinking is already inside `output_tokens`
    /// and cache reads inside `input_tokens` (matching the canonical convention).
    fn from(u: &AgyUsage) -> Self {
        Usage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cached_input_tokens: u.cache_read_tokens,
            reasoning_tokens: u.thinking_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EDIT: &str = include_str!("../tests/fixtures/agy-edit.ndjson");
    const HOOK: &str = include_str!("../tests/fixtures/agy-hook-approval.ndjson");
    const INTERRUPT: &str = include_str!("../tests/fixtures/agy-interrupt.ndjson");
    const RESUME: &str = include_str!("../tests/fixtures/agy-resume.ndjson");
    const LOCAL_ERR: &str = include_str!("../tests/fixtures/agy-local-command-error.ndjson");

    /// Feed every recorded `out`/`err` frame and collect the canonical events.
    fn replay(fixture: &str) -> Vec<Envelope> {
        let mut adapter = AgyAdapter::default();
        let mut events = Vec::new();
        for line in fixture.lines() {
            let rec: Value = serde_json::from_str(line).expect("fixture line");
            let frame = &rec["frame"];
            match rec["dir"].as_str() {
                Some("out") => events.extend(adapter.feed(&frame.to_string())),
                Some("err") => {
                    let text = frame["raw"].as_str().unwrap_or("");
                    events.extend(adapter.feed_stderr(text));
                }
                _ => {}
            }
        }
        events
    }

    /// True when the envelope belongs to the item `<conv8>:<tail>`.
    fn is_item(e: &Envelope, tail: &str) -> bool {
        e.item
            .as_deref()
            .is_some_and(|i| i.ends_with(&format!(":{tail}")))
    }

    fn completed(events: &[Envelope]) -> Vec<(&str, &Event)> {
        events
            .iter()
            .filter(|e| matches!(e.event, Event::ItemCompleted { .. }))
            .map(|e| (e.item.as_deref().unwrap_or(""), &e.event))
            .collect()
    }

    fn session(mode: Mode, hook: bool) -> OpenSession {
        OpenSession {
            program: "/usr/bin/agy".into(),
            extra_args: vec!["--x".into()],
            cwd: "/work/repo".into(),
            model: Some("m1".into()),
            effort: None,
            mode,
            resume: Some("conv-1".into()),
            new_session_id: None,
            approval_hook: hook,
        }
    }

    #[test]
    fn edit_turn_maps_tools_text_and_usage() {
        let ev = replay(EDIT);
        assert!(
            matches!(&ev[0].event, Event::SessionStarted { native_id, .. }
            if native_id == "3af90996-e4fe-44be-8d6c-ca20da039f6f")
        );
        assert!(matches!(ev[1].event, Event::TurnStarted { .. }));
        let view = ev
            .iter()
            .find(|e| is_item(e, "step-2"))
            .expect("view_file item");
        assert!(
            matches!(&view.event, Event::ItemStarted { kind: ItemKind::FileRead, title, .. }
            if title == "/work/repo/calc.py")
        );
        let edit = ev
            .iter()
            .find(|e| is_item(e, "step-4") && matches!(e.event, Event::ItemStarted { .. }))
            .expect("edit item");
        assert!(matches!(
            &edit.event,
            Event::ItemStarted {
                kind: ItemKind::FileChange,
                ..
            }
        ));
        let text: String = ev
            .iter()
            .filter(|e| is_item(e, "resp-7"))
            .filter_map(|e| match &e.event {
                Event::ContentDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.starts_with("Added [`multiply`]"));
        assert!(text.ends_with("```\n"));
        let done = completed(&ev);
        assert!(done.iter().any(|(id, _)| id.ends_with(":resp-7")));
        // user_input and thinking-only response steps produce no items.
        assert!(!ev.iter().any(|e| is_item(e, "resp-1")));
        let last = ev.last().expect("events");
        match &last.event {
            Event::TurnCompleted { state, usage, .. } => {
                assert_eq!(*state, TurnState::Completed);
                let u = usage.as_ref().expect("usage");
                assert_eq!(u.input_tokens, 52386);
                assert_eq!(u.output_tokens, 1681);
                assert_eq!(u.reasoning_tokens, 1048);
            }
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
        assert!(last.raw.is_some());
    }

    #[test]
    fn hook_approval_one_completed_one_declined() {
        let ev = replay(HOOK);
        let done = completed(&ev);
        let ok = done
            .iter()
            .find(|(id, _)| id.ends_with(":step-2"))
            .expect("allowed command");
        assert!(
            matches!(ok.1, Event::ItemCompleted { status: ItemStatus::Completed, output: Some(o), .. }
            if o.trim() == "allow-me")
        );
        let denied = done
            .iter()
            .find(|(id, _)| id.ends_with(":step-4"))
            .expect("denied command");
        assert!(
            matches!(denied.1, Event::ItemCompleted { status: ItemStatus::Declined, error: Some(e), .. }
            if e.contains("denied by pre-tool hook"))
        );
        let started = ev.iter().find(|e| is_item(e, "step-2")).expect("start");
        assert!(
            matches!(&started.event, Event::ItemStarted { kind: ItemKind::Command, title, .. }
            if title == "echo allow-me")
        );
    }

    #[test]
    fn interrupt_keeps_partial_text_and_reports_interrupted() {
        let ev = replay(INTERRUPT);
        let text: String = ev
            .iter()
            .filter_map(|e| match &e.event {
                Event::ContentDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.starts_with("Rivers are the vascular system"));
        let text_all = text.clone();
        assert!(ev
            .iter()
            .any(|e| matches!(&e.event, Event::Error { message } if message == "interrupted")));
        // The open response is cut short (its whole text snapshotted first), then the turn is
        // Interrupted.
        let n = ev.len();
        assert!(matches!(
            &ev[n - 3].event,
            Event::ContentSnapshot { stream: StreamKind::Assistant, text } if text == &text_all
        ));
        assert!(matches!(
            ev[n - 2].event,
            Event::ItemCompleted {
                status: ItemStatus::Interrupted,
                ..
            }
        ));
        assert!(matches!(
            ev[n - 1].event,
            Event::TurnCompleted {
                state: TurnState::Interrupted,
                error: None,
                ..
            }
        ));
    }

    #[test]
    fn resume_reports_the_new_model() {
        let ev = replay(RESUME);
        assert!(
            matches!(&ev[0].event, Event::SessionStarted { model: Some(m), .. }
            if m == "gemini-3.1-pro-low")
        );
        // system_message step is an unmapped frame, kept raw.
        assert!(ev
            .iter()
            .any(|e| matches!(e.event, Event::Unknown) && e.raw.is_some()));
        assert!(matches!(
            ev.last().map(|e| &e.event),
            Some(Event::TurnCompleted {
                state: TurnState::Completed,
                ..
            })
        ));
    }

    #[test]
    fn local_command_error_fails_the_turn() {
        let ev = replay(LOCAL_ERR);
        assert!(ev
            .iter()
            .any(|e| matches!(&e.event, Event::Error { message } if message.starts_with("/model is answered"))));
        assert!(matches!(
            &ev.last().expect("events").event,
            Event::TurnCompleted { state: TurnState::Failed, error: Some(e), .. }
                if e.contains("/model")
        ));
    }

    #[test]
    fn denied_actions_add_a_notice() {
        let mut a = AgyAdapter::default();
        let ev = a.feed(
            r#"{"event":"result","result":{"status":"SUCCESS","denied_actions":["run_command"]}}"#,
        );
        assert!(matches!(&ev[0].event, Event::Notice { text } if text.contains("run_command")));
        assert!(matches!(ev[1].event, Event::TurnCompleted { .. }));
    }

    #[test]
    fn bad_input_is_unknown_never_a_panic() {
        let mut a = AgyAdapter::default();
        let ev = a.feed("not json");
        assert!(matches!(ev[0].event, Event::Unknown));
        assert_eq!(ev[0].raw, Some(Value::String("not json".into())));
        assert!(matches!(
            a.feed(r#"{"event":"mystery"}"#)[0].event,
            Event::Unknown
        ));
        assert!(a.feed("  ").is_empty());
        assert!(a.feed_stderr("noise").is_empty());
        assert!(
            matches!(&a.feed_stderr("warning: slow")[0].event, Event::Notice { text } if text == "slow")
        );
    }

    #[test]
    fn on_exit_closes_open_items_and_flags_expected() {
        let mut a = AgyAdapter::default();
        a.feed(
            r#"{"event":"step_update","step_update":{"state":"ACTIVE","step_index":3,"step_type":"tool","tool_name":"run_command","tool_info":{"parameters":{"CommandLine":"sleep 9"}}}}"#,
        );
        a.encode(Command::Interrupt).expect("interrupt");
        let ev = a.on_exit(Some(130));
        assert!(matches!(
            ev[0].event,
            Event::ItemCompleted {
                status: ItemStatus::Interrupted,
                ..
            }
        ));
        assert!(matches!(
            ev[1].event,
            Event::SessionExited {
                code: Some(130),
                expected: true
            }
        ));
    }

    #[test]
    fn argv_with_hook_uses_skip_permissions() {
        let a = AgyAdapter::default();
        let argv = a.argv(&session(Mode::Ask, true));
        assert_eq!(
            argv,
            [
                "/usr/bin/agy",
                "--x",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--model",
                "m1",
                "--conversation",
                "conv-1",
                "--dangerously-skip-permissions",
            ]
        );
        let edits = a.argv(&session(Mode::AcceptEdits, true));
        assert!(edits.windows(2).any(|w| w == ["--mode", "accept-edits"]));
        assert!(edits.contains(&"--dangerously-skip-permissions".to_owned()));
        let plan = a.argv(&session(Mode::Plan, true));
        assert!(plan.windows(2).any(|w| w == ["--mode", "plan"]));
        assert!(!plan.contains(&"--dangerously-skip-permissions".to_owned()));
    }

    #[test]
    fn argv_without_hook_maps_modes_to_flags_and_never_skips_permissions() {
        let a = AgyAdapter::default();
        for (mode, flag) in [
            (Mode::Ask, "plan"),
            (Mode::AcceptEdits, "accept-edits"),
            (Mode::Plan, "plan"),
        ] {
            let argv = a.argv(&session(mode, false));
            assert!(argv.windows(2).any(|w| w == ["--mode", flag]), "{mode:?}");
            assert!(!argv.contains(&"--dangerously-skip-permissions".to_owned()));
        }
    }

    #[test]
    fn prompt_encodes_a_user_event_and_intercepts_side_commands() {
        let mut a = AgyAdapter::new("/usr/bin/agy");
        let w = a.encode(Command::Prompt { text: "hi".into() }).expect("ok");
        let Action::Write(lines) = &w[0] else {
            panic!("expected write")
        };
        let v: Value = serde_json::from_str(&lines[0]).expect("json");
        assert_eq!(v["event"], "user");
        assert_eq!(v["message"]["role"], "user");
        assert_eq!(v["message"]["content"], "hi");
        for cmd in SIDE_COMMANDS {
            let r = a
                .encode(Command::Prompt {
                    text: (*cmd).to_owned(),
                })
                .expect("side");
            assert!(matches!(&r[0], Action::SideProcess { argv, .. }
                if argv[..] == ["/usr/bin/agy", "-p", *cmd, "--output-format", "json"]));
        }
        // Only the first token counts, and a path-like slash text is a normal prompt.
        let r = a
            .encode(Command::Prompt {
                text: "/modelx".into(),
            })
            .expect("ok");
        assert!(matches!(r[0], Action::Write(_)));
    }

    #[test]
    fn other_commands_map_as_specified() {
        let mut a = AgyAdapter::new("agy");
        a.feed(r#"{"event":"init","conversation_id":"c9","init":{"model":"m"}}"#);
        assert_eq!(
            a.encode(Command::Interrupt).expect("ok"),
            vec![Action::Interrupt]
        );
        assert_eq!(
            // The effort rides in the composed model id; the separate field is not sent.
            a.encode(Command::SetModel {
                model: "x-high".into(),
                effort: Some("high".into())
            })
            .expect("ok"),
            vec![Action::Respawn(OpenSessionDelta {
                model: Some("x-high".into()),
                effort: None,
                mode: None,
                resume: Some("c9".into())
            })]
        );
        assert_eq!(
            a.encode(Command::SetMode { mode: Mode::Plan }).expect("ok"),
            vec![Action::Respawn(OpenSessionDelta {
                model: None,
                effort: None,
                mode: Some(Mode::Plan),
                resume: Some("c9".into())
            })]
        );
        assert!(matches!(
            a.encode(Command::Approve {
                request: "r".into(),
                decision: crate::event::Decision::Allow,
                updated_input: None,
                message: None
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
        assert!(matches!(
            a.encode(Command::Control {
                id: "k".into(),
                control: Control::McpStatus
            }),
            Err(AdapterError::Unsupported(_))
        ));
        let r = a
            .encode(Command::Control {
                id: "m".into(),
                control: Control::ListModels,
            })
            .expect("ok");
        assert_eq!(
            r,
            vec![Action::SideProcess {
                id: "m".into(),
                argv: vec!["agy".into(), "models".into()]
            }]
        );
        for (control, cmd) in [
            (Control::Usage, "/usage"),
            (Control::GetSettings, "/config"),
        ] {
            let r = a
                .encode(Command::Control {
                    id: "u".into(),
                    control,
                })
                .expect("ok");
            assert!(matches!(&r[0], Action::SideProcess { argv, .. }
                if argv[..] == ["agy", "-p", cmd, "--output-format", "json"]));
        }
    }

    #[test]
    fn side_results_parse_json_tsv_and_failures() {
        let mut a = AgyAdapter::default();
        a.encode(Command::Control {
            id: "u".into(),
            control: Control::Usage,
        })
        .expect("ok");
        let ev = a.feed_side("u", r#"{"credits": 5}"#, true);
        assert_eq!(ev[0].request.as_deref(), Some("u"));
        assert!(
            matches!(&ev[0].event, Event::ControlResult { ok: Some(v), error: None }
            if v["credits"] == 5)
        );

        a.encode(Command::Control {
            id: "m".into(),
            control: Control::ListModels,
        })
        .expect("ok");
        let ev = a.feed_side("m", "gemini-a\tGemini A\ngemini-b\tGemini B\n", true);
        assert!(
            matches!(&ev[0].event, Event::ControlResult { ok: Some(v), .. }
            if v[1]["id"] == "gemini-b" && v[0]["display"] == "Gemini A")
        );

        // A real /usage document also yields a QuotaUpdated; the ControlResult stays first.
        a.encode(Command::Control {
            id: "q".into(),
            control: Control::Usage,
        })
        .expect("ok");
        let doc = r#"{"command":{"name":"usage","data":{"groups":[{"name":"Gemini Models","buckets":[{"window":"5h","remaining_fraction":0.75,"reset_time":"2026-10-06T16:59:24Z"}]}]}}}"#;
        let ev = a.feed_side("q", doc, true);
        assert_eq!(ev.len(), 2);
        assert!(matches!(
            &ev[0].event,
            Event::ControlResult { ok: Some(_), .. }
        ));
        assert!(
            matches!(&ev[1].event, Event::QuotaUpdated { account: None, windows }
            if windows.len() == 1 && windows[0].used == 0.25
                && windows[0].group.as_deref() == Some("Gemini Models"))
        );

        let ev = a.feed_side("u", "boom\nmore", false);
        assert!(
            matches!(&ev[0].event, Event::ControlResult { ok: None, error: Some(e) }
            if e == "boom")
        );
    }

    fn active_tool(index: u64, conv: &str) -> String {
        format!(
            r#"{{"event":"step_update","conversation_id":"{conv}","step_update":{{"conversation_id":"{conv}","state":"ACTIVE","step_index":{index},"step_type":"tool","tool_name":"run_command"}}}}"#
        )
    }

    #[test]
    fn extra_args_cannot_smuggle_skip_permissions() {
        let a = AgyAdapter::default();
        let mut s = session(Mode::Ask, false);
        s.extra_args = vec![
            "--dangerously-skip-permissions".into(),
            "--dangerously-skip-permissions=true".into(),
            "--keep".into(),
        ];
        let argv = a.argv(&s);
        assert!(argv.contains(&"--keep".to_owned()));
        assert!(
            !argv
                .iter()
                .any(|x| x.starts_with("--dangerously-skip-permissions")),
            "{argv:?}"
        );
        // With the hook the adapter adds exactly one itself.
        s.approval_hook = true;
        let n = a
            .argv(&s)
            .iter()
            .filter(|x| x.starts_with("--dangerously-skip-permissions"))
            .count();
        assert_eq!(n, 1);
    }

    #[test]
    fn side_commands_match_case_insensitively_and_other_slashes_forward() {
        let mut a = AgyAdapter::new("agy");
        for text in ["/Model", "  /MODEL  ", "\n/model"] {
            let r = a.encode(Command::Prompt { text: text.into() }).expect("ok");
            assert!(
                matches!(&r[0], Action::SideProcess { argv, .. }
                    if argv[..] == ["agy", "-p", "/model", "--output-format", "json"]),
                "{text:?}"
            );
        }
        let r = a
            .encode(Command::Prompt {
                text: "/Usage now".into(),
            })
            .expect("ok");
        assert!(matches!(&r[0], Action::SideProcess { argv, .. } if argv[2] == "/usage now"));
        let r = a
            .encode(Command::Prompt {
                text: "/my-skill go".into(),
            })
            .expect("ok");
        assert!(matches!(r[0], Action::Write(_)));
    }

    #[test]
    fn response_completion_snapshots_the_whole_text() {
        let mut a = AgyAdapter::default();
        let step = |state: &str, delta: &str| {
            format!(
                r#"{{"event":"step_update","step_update":{{"conversation_id":"abcdef123456","state":"{state}","step_index":5,"step_type":"agent_response","text_delta":"{delta}"}}}}"#
            )
        };
        a.feed(&step("ACTIVE", "ghp_abcdefghij"));
        let ev = a.feed(&step("DONE", "klmnop"));
        let n = ev.len();
        assert!(matches!(
            &ev[n - 2].event,
            Event::ContentSnapshot { stream: StreamKind::Assistant, text } if text == "ghp_abcdefghijklmnop"
        ));
        assert_eq!(ev[n - 2].item.as_deref(), Some("abcdef12:resp-5"));
        assert!(matches!(
            ev[n - 1].event,
            Event::ItemCompleted {
                status: ItemStatus::Completed,
                ..
            }
        ));
    }

    #[test]
    fn item_ids_are_unique_across_conversations() {
        let ids = |conv: &str| {
            let mut a = AgyAdapter::default();
            a.feed(&active_tool(2, conv));
            a.open_items.clone()
        };
        assert_eq!(ids("11111111-aaaa"), vec!["11111111:step-2".to_owned()]);
        assert_ne!(ids("11111111-aaaa"), ids("22222222-bbbb"));
        // Before any conversation id is known, two adapters still differ.
        let bare = |_: ()| {
            let mut a = AgyAdapter::default();
            a.feed(
                r#"{"event":"step_update","step_update":{"state":"ACTIVE","step_index":2,"step_type":"tool","tool_name":"run_command"}}"#,
            );
            a.open_items.clone()
        };
        assert_ne!(bare(()), bare(()));
    }

    #[test]
    fn an_idle_spawn_prints_init_without_starting_a_turn() {
        // Verified live: agy prints `init` straight after spawn, before any input.
        let mut a = AgyAdapter::default();
        let ev = a.feed(r#"{"event":"init","conversation_id":"c1","init":{}}"#);
        assert!(ev
            .iter()
            .any(|e| matches!(e.event, Event::SessionStarted { .. })));
        assert!(
            !ev.iter()
                .any(|e| matches!(e.event, Event::TurnStarted { .. })),
            "an idle agent must not show as working"
        );
        // Stopping it then is no failed turn.
        let ev = a.on_exit(Some(0));
        assert!(!ev
            .iter()
            .any(|e| matches!(e.event, Event::TurnCompleted { .. })));
        // The prompt's own `user_input` step opens the turn.
        let mut a = AgyAdapter::default();
        a.feed(r#"{"event":"init","conversation_id":"c1","init":{}}"#);
        a.encode(Command::Prompt { text: "hi".into() })
            .expect("prompt");
        let ev = a.feed(
            r#"{"event":"step_update","step_update":{"state":"DONE","step_index":0,"step_type":"user_input"}}"#,
        );
        assert!(ev
            .iter()
            .any(|e| matches!(e.event, Event::TurnStarted { .. })));
    }

    #[test]
    fn on_exit_with_an_open_turn_completes_it() {
        let mut a = AgyAdapter::default();
        a.encode(Command::Prompt { text: "hi".into() })
            .expect("prompt");
        a.feed(r#"{"event":"init","conversation_id":"c1","init":{}}"#);
        let ev = a.on_exit(Some(1));
        assert!(matches!(
            &ev[0].event,
            Event::TurnCompleted { state: TurnState::Failed, error: Some(e), .. } if e.contains("unexpectedly")
        ));
        assert!(matches!(
            ev[1].event,
            Event::SessionExited {
                expected: false,
                ..
            }
        ));

        let mut a = AgyAdapter::default();
        a.encode(Command::Prompt { text: "hi".into() })
            .expect("prompt");
        a.feed(r#"{"event":"init","conversation_id":"c1","init":{}}"#);
        a.encode(Command::Interrupt).expect("interrupt");
        let ev = a.on_exit(Some(130));
        assert!(matches!(
            ev[0].event,
            Event::TurnCompleted {
                state: TurnState::Interrupted,
                error: None,
                ..
            }
        ));
        // No open turn: no TurnCompleted.
        let mut a = AgyAdapter::default();
        assert_eq!(a.on_exit(Some(0)).len(), 1);
    }
}
