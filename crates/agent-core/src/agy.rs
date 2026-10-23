//! Antigravity (`agy`) adapter: a pure state machine over `agy`'s own stream-json schema
//! (`event` = `init` | `step_update` | `result`). Verified against agy 1.3.0 (see
//! `tests/fixtures/README.md` and the Phase 0 table in the v3 plan).
//!
//! No process, file or socket I/O happens here. Approvals do NOT go through this adapter: agy
//! runs with `--dangerously-skip-permissions` and the app's PreToolUse hook
//! (`agent-terminal --approval-hook`, see [`crate::approval`]) is the gate, so `Approve` and
//! `Answer` are unsupported on purpose.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::adapter::{
    Action, Adapter, AdapterError, Command, Control, Driver, Mode, OpenSession, OpenSessionDelta,
    Outbox,
};
use crate::caps::Capabilities;
use crate::delegation::agy_guard;
use crate::event::{
    Envelope, Event, ItemKind, ItemStatus, ReasoningLabel, ReasoningState, StreamKind, TurnState,
    Usage,
};

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

/// What a profile argument is to the adapter's launch decisions.
#[derive(Debug, PartialEq, Eq)]
enum ProfileArg {
    /// Passed through.
    Keep,
    /// Dropped on its own.
    Drop,
    /// Dropped together with the argument after it (its value).
    DropWithValue,
}

/// agy is a Go binary: its flag parser takes one or two leading dashes and an optional
/// `=value`. Names are compared with `-` and `_` treated alike, so near-spellings are caught too.
/// The undocumented config-directory flags ([`agy_guard::CONFIG_DIR_FLAGS`]) move agy's whole
/// configuration (hooks, MCP servers, settings) away from `$HOME/.gemini`, the one the app's
/// hook proof reads (verified live for `--gemini_dir=`, agy 1.3.5); they take a value, inline
/// or as the next argument.
fn classify_profile_arg(arg: &str) -> ProfileArg {
    let Some((name, inline_value)) = agy_guard::flag_name(arg) else {
        return ProfileArg::Keep;
    };
    if name == SKIP_PERMISSIONS.trim_start_matches('-').replace('-', "_") {
        // A boolean flag: it never consumes the next argument.
        return ProfileArg::Drop;
    }
    if agy_guard::CONFIG_DIR_FLAGS.contains(&name.as_str()) {
        return if inline_value {
            ProfileArg::Drop
        } else {
            ProfileArg::DropWithValue
        };
    }
    ProfileArg::Keep
}

/// The profile arguments with every launch decision the adapter owns removed, after a `--` too:
/// Go's `--` ends only the FlagSet parsing it, and a subcommand's FlagSet parses what follows.
fn profile_args(extra_args: &[String]) -> Vec<String> {
    let mut kept = Vec::with_capacity(extra_args.len());
    let mut args = extra_args.iter();
    while let Some(arg) = args.next() {
        match classify_profile_arg(arg) {
            ProfileArg::Keep => kept.push(arg.clone()),
            ProfileArg::Drop => {}
            ProfileArg::DropWithValue => {
                args.next();
            }
        }
    }
    kept
}

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
    /// Thinking item IDs that have already completed, preventing re-opening on redundant step updates.
    completed_thinks: HashSet<String>,
    side_seq: u64,
    side_kinds: HashMap<String, SideKind>,
    /// An interrupt or respawn was requested, so the coming exit is expected.
    exit_expected: bool,
    /// Item-id prefix used while the conversation id is unknown (unique per adapter).
    fallback_prefix: String,
    /// Accumulated text of each open response item, flushed as a snapshot on completion.
    resp_text: HashMap<String, String>,
    /// Context before the last model call: its input, without adding output or cache reads again.
    context_tokens: u64,
    /// Latest model call's input this turn; result usage sums all calls in the recorded stream.
    step_input_tokens: Option<u64>,
    /// The actions agy denied in its last turn ([`denied_name`]), until the next prompt: an
    /// unexpected exit right after names them.
    last_denied: Vec<String>,
    outbox: Outbox,
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
            completed_thinks: HashSet::new(),
            side_seq: 0,
            side_kinds: HashMap::new(),
            exit_expected: false,
            fallback_prefix: format!("a{}", ADAPTER_SEQ.fetch_add(1, Ordering::Relaxed)),
            resp_text: HashMap::new(),
            context_tokens: 0,
            step_input_tokens: None,
            last_denied: Vec::new(),
            outbox: Outbox::default(),
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
        crate::adapter::start_open_item(&mut self.open_items, out, id, kind, title, input);
    }

    /// Opens a thinking item. agy hands over each thinking block whole (its transcript has it,
    /// stdout does not stream it), so the block is Complete from the start: never streamed for
    /// show.
    fn start_thinking(&mut self, out: &mut Vec<Envelope>, id: &str) {
        self.start_item(out, id, ItemKind::Reasoning, "Thinking".to_owned());
        out.push(
            Envelope::new(Event::Reasoning {
                state: ReasoningState::Complete,
                label: ReasoningLabel::Thinking,
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
        if let Some(usage) = step.usage.as_ref().filter(|u| u.input_tokens > 0) {
            self.step_input_tokens = Some(usage.input_tokens);
        }
        // A missing index collapses onto 0; agy always sets it in practice.
        let index = step.step_index.unwrap_or(0);
        match step.step_type.as_str() {
            "user_input" => {
                if step.state == "DONE" {
                    self.ensure_turn(out);
                }
            }
            "agent_response" | "thinking" | "thought" => self.on_response(step, index, out),
            "tool" => self.on_tool(step, index, out),
            _ if step.tool_name.is_some() => self.on_tool(step, index, out),
            _ => out.push(env(Event::Unknown)),
        }
    }

    fn on_response(&mut self, step: &Step, index: u64, out: &mut Vec<Envelope>) {
        let think = step
            .thinking
            .as_deref()
            .or(step.thought.as_deref())
            .or(step.thoughts.as_deref())
            .or(step.thinking_delta.as_deref())
            .filter(|t| !t.trim().is_empty());
        if let Some(think_text) = think {
            let think_id = self.item_id("think", index);
            if !self.completed_thinks.contains(&think_id) {
                if !self.open_items.contains(&think_id) {
                    self.start_thinking(out, &think_id);
                }
                out.push(thinking_text(&think_id, think_text));
                if step.state == "DONE" {
                    self.completed_thinks.insert(think_id.clone());
                    self.complete_item(out, &think_id, ItemStatus::Completed, None, None);
                }
            }
        }

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
        let info = step.tool_info.clone().unwrap_or_default();
        let think = step
            .thinking
            .as_deref()
            .or(step.thought.as_deref())
            .or(step.thoughts.as_deref())
            .or(step.thinking_delta.as_deref())
            .or(info.thinking.as_deref())
            .or(info.thought.as_deref())
            .filter(|t| !t.trim().is_empty());
        if let Some(think_text) = think {
            let think_id = self.item_id("think", index);
            if !self.completed_thinks.contains(&think_id) {
                self.completed_thinks.insert(think_id.clone());
                if !self.open_items.contains(&think_id) {
                    self.start_thinking(out, &think_id);
                    out.push(thinking_text(&think_id, think_text));
                    self.complete_item(out, &think_id, ItemStatus::Completed, None, None);
                }
            }
        }

        let id = self.item_id("step", index);
        let name = step
            .tool_name
            .clone()
            .or(info.name.clone())
            .unwrap_or_else(|| "tool".to_owned());
        let kind = tool_kind(&name);
        // Stdout's parameters, else the transcript's record of the same call (decoded, so
        // the card title reads `/etc/hostname`, not `"/etc/hostname"`).
        let mut params = info
            .parameters
            .clone()
            .filter(|p| p.as_object().is_some_and(|o| !o.is_empty()))
            .or_else(|| {
                step.transcript_calls
                    .iter()
                    .find(|c| c.get("name").and_then(Value::as_str) == Some(name.as_str()))
                    .and_then(|c| c.get("args"))
                    .map(crate::tool_title::decode_agy_args)
            })
            .or_else(|| info.parameters.clone());
        if let Some(p) = params.as_mut().and_then(Value::as_object_mut) {
            if !p.contains_key("tool_name") {
                p.insert("tool_name".to_string(), Value::String(name.clone()));
            }
        }
        let title = match kind {
            ItemKind::FileChange => name.clone(),
            _ => tool_title(&name, params.as_ref()),
        };
        self.start_item_with(out, &id, kind, title, params);
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
                if crate::quota::is_rate_limit_or_quota(&message) {
                    out.push(env(Event::RateLimited {
                        resets_at: None,
                        detail: Some(Value::String(message.clone())),
                    }));
                }
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
        // Kept until the next prompt: an exit right after this turn is reported with them.
        self.last_denied = result.denied_actions.iter().map(denied_name).collect();
        if !self.last_denied.is_empty() {
            out.push(env(Event::Notice {
                text: format!("agy denied these actions: {}", self.last_denied.join(", ")),
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
        if let Some(err) = &error {
            if crate::quota::is_rate_limit_or_quota(err)
                || crate::quota::is_rate_limit_or_quota(&result.status)
            {
                out.push(env(Event::RateLimited {
                    resets_at: None,
                    detail: Some(Value::String(err.clone())),
                }));
            }
        } else if crate::quota::is_rate_limit_or_quota(&result.status) {
            out.push(env(Event::RateLimited {
                resets_at: None,
                detail: Some(Value::String(result.status.clone())),
            }));
        }
        if let Some(input) = self.step_input_tokens.take().or_else(|| {
            result
                .usage
                .as_ref()
                .filter(|u| u.input_tokens > 0)
                .map(|u| u.input_tokens)
        }) {
            self.context_tokens = input;
        }
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
    /// (`--dangerously-skip-permissions` or `--dangerously-skip-permissions=…`, one dash or two):
    /// those are dropped silently (no I/O here) and the adapter alone decides.
    ///
    /// The hook proof reads `$HOME/.gemini`, so `extra_args` cannot move agy's configuration
    /// either: `--gemini_dir` and `--app_data_dir` (with their value, inline or next) are dropped
    /// the same way. Dropping rather than refusing the skip flag keeps the launch what the session
    /// believes it is: with the flag kept, agy would load other hook files while the session
    /// treats the app's hook as the gate, and its Ask and Accept-edits modes would no longer mean
    /// what they say.
    ///
    /// The mode is agy's own `--mode` flag. Without the hook, agy cannot ask before acting, so Ask
    /// is sent as `--mode plan` (the session normally never asks for it). Verified live: headless
    /// agy refuses shell commands without the skip flag in every mode, but applies file edits in
    /// every mode, Plan included (it plans, then edits).
    fn argv(&self, session: &OpenSession) -> Vec<String> {
        let mut argv = vec![session.program.clone()];
        argv.extend(profile_args(&session.extra_args));
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
            // Bypass is the hook allowing everything: with no hook to do it, the session never
            // asks for it, and the strictest launch is the fallback.
            (Mode::Plan, _) | (Mode::Ask | Mode::Bypass, false) => argv.extend(plan()),
            (Mode::AcceptEdits, false) => argv.extend(accept_edits()),
            (Mode::AcceptEdits, true) => {
                argv.extend(accept_edits());
                argv.push("--dangerously-skip-permissions".to_owned());
            }
            (Mode::Ask | Mode::Bypass, true) => {
                argv.push("--dangerously-skip-permissions".to_owned())
            }
        }
        argv
    }

    fn handshake(&mut self) -> Vec<String> {
        Vec::new()
    }

    fn prompt_starts_turn(&self, text: &str) -> bool {
        let first = text.split_whitespace().next().unwrap_or_default();
        !SIDE_COMMANDS.contains(&first.to_ascii_lowercase().as_str())
    }

    fn encode(&mut self, command: Command) -> Result<Vec<Action>, AdapterError> {
        match command {
            Command::Prompt { text } => {
                // The first token is matched case-insensitively after leading whitespace. Any
                // other `/word` is forwarded as agent text (custom commands and skills).
                let trimmed = text.trim();
                let first = trimmed.split_whitespace().next().unwrap_or("");
                if !self.prompt_starts_turn(&text) {
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
                self.step_input_tokens = None;
                // A new turn: the last one's denials no longer explain an exit.
                self.last_denied.clear();
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
            Command::SubmitForm { .. } => {
                Err(AdapterError::Unsupported("agy never asks for a form"))
            }
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
                    Control::ContextUsage => {
                        let max = 1_048_576;
                        let event = if self.context_tokens > 0 {
                            self.outbox.events.push(Envelope::new(Event::UsageUpdated {
                                used: self.context_tokens,
                                max: Some(max),
                                auto_compact_at: None,
                            }));
                            Event::ControlResult {
                                ok: Some(json!({"used": self.context_tokens, "max": max})),
                                error: None,
                            }
                        } else {
                            Event::ControlResult {
                                ok: None,
                                error: Some(
                                    "Antigravity has not reported token usage yet".to_owned(),
                                ),
                            }
                        };
                        self.outbox.events.push(Envelope::new(event).request(id));
                        return Ok(Vec::new());
                    }
                    _ => {
                        return Err(AdapterError::Unsupported(
                            "this control is not offered by agy",
                        ))
                    }
                };
                Ok(vec![action])
            }
            // agy runs a message written mid-turn as the next turn (probe, 2026-10-11): the host
            // keeps its own queue for it (`Capabilities::live_input` is false).
            Command::Steer { .. } | Command::DelegatedSteer { .. } => Err(
                AdapterError::Unsupported("agy cannot take a message while a turn runs"),
            ),
        }
    }

    fn feed(&mut self, line: &str) -> Vec<Envelope> {
        crate::adapter::map_json_frame(line.trim(), |raw| {
            // A frame that is JSON but not agy's shape is Unknown, keeping the parsed JSON.
            let Ok(frame) = serde_json::from_value::<Frame>(raw.clone()) else {
                return vec![env(Event::Unknown)];
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
            out
        })
    }

    fn feed_stderr(&mut self, line: &str) -> Vec<Envelope> {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("error:") {
            // An interrupt is the user's own stop, not a failure: the turn ends `Interrupted`
            // (result frame or exit), like the other agents', and no error card is raised.
            if rest.trim().eq_ignore_ascii_case("interrupted") {
                return Vec::new();
            }
            let mut res = Vec::new();
            if crate::quota::is_rate_limit_or_quota(rest) {
                res.push(env(Event::RateLimited {
                    resets_at: None,
                    detail: Some(Value::String(rest.trim().to_owned())),
                }));
            }
            res.push(env(Event::Error {
                message: rest.trim().to_owned(),
            }));
            res
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
        let mut rate_limit = None;
        let event = if !success {
            let detail = stdout.trim().lines().next().unwrap_or("").trim();
            if crate::quota::is_rate_limit_or_quota(detail) {
                rate_limit = Some(Event::RateLimited {
                    resets_at: None,
                    detail: Some(Value::String(detail.to_owned())),
                });
            }
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
                        if let Some(w) = windows.iter().find(|w| w.used >= 1.0) {
                            rate_limit = Some(Event::RateLimited {
                                resets_at: w.resets_at.clone(),
                                detail: Some(Value::String("Quota exhausted".to_owned())),
                            });
                        }
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
        out.extend(rate_limit.map(Envelope::new));
        out.extend(quota.map(Envelope::new));
        out
    }

    fn on_exit(&mut self, code: Option<i32>) -> Vec<Envelope> {
        let mut out = Vec::new();
        self.step_input_tokens = None;
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
        // An unexpected exit right after a turn in which agy denied actions: say which, and that
        // the conversation goes on. No cause is claimed: in the stored threads, denials came on
        // turns that completed and agy kept running; this only puts the two facts together.
        let denied = std::mem::take(&mut self.last_denied);
        if !expected && !denied.is_empty() {
            out.push(env(Event::Notice {
                text: format!(
                    "agy exited right after denying {}. Send a message to continue: the \
                     conversation resumes in a new agy process.",
                    denied.join(", ")
                ),
            }));
        }
        out.push(env(Event::SessionExited { code, expected }));
        out
    }

    fn drain_outbox(&mut self) -> Outbox {
        std::mem::take(&mut self.outbox)
    }
}

fn env(event: Event) -> Envelope {
    Envelope::new(event)
}

/// A thinking block's whole text: a snapshot, so a repeated step update replaces it rather than
/// appending it again.
fn thinking_text(id: &str, text: &str) -> Envelope {
    Envelope::new(Event::ContentSnapshot {
        stream: StreamKind::Reasoning,
        text: text.to_owned(),
    })
    .item(id)
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

/// One `denied_actions` entry as words. agy writes either a bare name or an object; its policy
/// writes `{"action":"command","display_name":"RunCommand"}` (seen in stored threads,
/// 2026-10-10), shown as `RunCommand (command)`. Older or other shapes name the tool under
/// `name`/`tool_name`/`tool`. Anything unrecognised falls back to its JSON rather than nothing.
fn denied_name(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => {
            let field = |k: &str| {
                other
                    .get(k)
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
            };
            let tool = ["name", "tool_name", "tool"].iter().find_map(|k| field(k));
            match (field("display_name"), field("action"), tool) {
                (Some(display), Some(action), _) if display != action => {
                    format!("{display} ({action})")
                }
                (Some(display), _, _) => display.to_owned(),
                (None, _, Some(tool)) => tool.to_owned(),
                (None, Some(action), None) => action.to_owned(),
                (None, None, None) => other.to_string(),
            }
        }
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

/// Reads an explicit JSON `null` as the type's default, so a frame carrying
/// `"state": null` still parses (`#[serde(default)]` only covers a missing key).
fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Default, Deserialize)]
struct Frame {
    #[serde(default, deserialize_with = "null_default")]
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
    #[serde(default, deserialize_with = "null_default")]
    state: String,
    #[serde(default)]
    step_index: Option<u64>,
    #[serde(default, deserialize_with = "null_default")]
    step_type: String,
    #[serde(default)]
    text_delta: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    thought: Option<String>,
    #[serde(default)]
    thinking_delta: Option<String>,
    #[serde(default)]
    thoughts: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_info: Option<ToolInfo>,
    #[serde(default)]
    usage: Option<AgyUsage>,
    /// The step's tool calls as agy's transcript recorded them, args decoded (`{name, args}`):
    /// put on the step by the session's transcript reader.
    #[serde(default)]
    transcript_calls: Vec<Value>,
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
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    thought: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ResultFrame {
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    status: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    usage: Option<AgyUsage>,
    #[serde(default, deserialize_with = "null_default")]
    denied_actions: Vec<Value>,
}

#[derive(Debug, Default, Deserialize)]
struct AgyUsage {
    #[serde(default, deserialize_with = "null_default")]
    input_tokens: u64,
    #[serde(default, deserialize_with = "null_default")]
    output_tokens: u64,
    #[serde(default, deserialize_with = "null_default")]
    thinking_tokens: u64,
    #[serde(default, deserialize_with = "null_default")]
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

    #[test]
    fn frames_with_explicit_nulls_still_parse() {
        let mut adapter = AgyAdapter::default();
        let result = r#"{"event":"result","result":{"status":"SUCCESS","denied_actions":null,"error":null,"usage":{"input_tokens":null,"output_tokens":3}}}"#;
        let evs = adapter.feed(result);
        assert!(
            evs.iter().any(|e| matches!(
                e.event,
                Event::TurnCompleted {
                    state: TurnState::Completed,
                    ..
                }
            )),
            "{evs:?}"
        );
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
            delegation: None,
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
    fn context_usage_control_reports_input_tokens() {
        let mut a = AgyAdapter::default();
        a.encode(Command::Control {
            id: "cu1".into(),
            control: Control::ContextUsage,
        })
        .expect("control");
        let outbox = a.drain_outbox();
        assert_eq!(outbox.events.len(), 1);
        assert!(matches!(
            &outbox.events[0].event,
            Event::ControlResult { ok: None, error: Some(err) } if err.contains("not reported")
        ));

        a.feed(
            r#"{"event":"result","result":{"status":"SUCCESS","usage":{"input_tokens":1000,"output_tokens":200}}}"#,
        );

        a.encode(Command::Control {
            id: "cu2".into(),
            control: Control::ContextUsage,
        })
        .expect("control");
        let outbox = a.drain_outbox();
        assert_eq!(outbox.events.len(), 2);
        assert!(matches!(
            &outbox.events[0].event,
            Event::UsageUpdated {
                used: 1000,
                max: Some(1_048_576),
                ..
            }
        ));
        match &outbox.events[1].event {
            Event::ControlResult {
                ok: Some(val),
                error: None,
            } => {
                assert_eq!(val["used"], 1000);
                assert_eq!(val["max"], 1_048_576);
            }
            other => panic!("expected ControlResult ok, got {other:?}"),
        }
    }

    #[test]
    fn agy_context_follows_the_latest_turn() {
        let mut a = AgyAdapter::default();
        for (usage, expected) in [
            (json!({"input_tokens": 2000, "output_tokens": 300}), 2000),
            (json!({"input_tokens": 500, "output_tokens": 5000}), 500),
            (json!({"input_tokens": 0, "output_tokens": 100}), 500),
            (json!({"input_tokens": null, "output_tokens": 100}), 500),
            (Value::Null, 500),
        ] {
            a.feed(
                &json!({"event": "result", "result": {"status": "SUCCESS", "usage": usage}})
                    .to_string(),
            );
            a.encode(Command::Control {
                id: "context".into(),
                control: Control::ContextUsage,
            })
            .expect("control");
            let out = a.drain_outbox();
            assert!(
                matches!(&out.events[0].event, Event::UsageUpdated { used, .. } if *used == expected)
            );
            assert!(
                matches!(&out.events[1].event, Event::ControlResult { ok: Some(value), .. }
                if value["used"] == expected)
            );
        }
    }

    #[test]
    fn agy_context_uses_the_last_model_call_instead_of_the_turn_total() {
        let mut a = AgyAdapter::default();
        for (fixture, expected) in [(EDIT, 14035), (HOOK, 12771)] {
            for line in fixture.lines() {
                let rec: Value = serde_json::from_str(line).expect("fixture");
                if rec["dir"] == "out" {
                    a.feed(&rec["frame"].to_string());
                }
            }
            a.encode(Command::Control {
                id: "context".into(),
                control: Control::ContextUsage,
            })
            .expect("control");
            let out = a.drain_outbox();
            assert!(
                matches!(&out.events[0].event, Event::UsageUpdated { used, .. } if *used == expected)
            );
        }

        // A thinking-only call still reports context; missing/zero step usage cannot erase it.
        for usage in [
            json!({"input_tokens": 500}),
            json!({"input_tokens": 0}),
            Value::Null,
        ] {
            a.feed(
                &json!({"event": "step_update", "step_update": {
                    "step_type": "agent_response", "state": "DONE", "step_index": 8, "usage": usage
                }})
                .to_string(),
            );
        }
        a.feed(r#"{"event":"result","result":{"status":"SUCCESS","usage":{"input_tokens":20000,"output_tokens":5000}}}"#);
        a.encode(Command::Control {
            id: "smaller".into(),
            control: Control::ContextUsage,
        })
        .expect("control");
        assert!(matches!(
            &a.drain_outbox().events[0].event,
            Event::UsageUpdated { used: 500, .. }
        ));

        // A later result-only turn uses its own input, rather than the previous turn's step.
        a.feed(r#"{"event":"result","result":{"status":"SUCCESS","usage":{"input_tokens":800,"output_tokens":100}}}"#);
        a.encode(Command::Control {
            id: "fallback".into(),
            control: Control::ContextUsage,
        })
        .expect("control");
        assert!(matches!(
            &a.drain_outbox().events[0].event,
            Event::UsageUpdated { used: 800, .. }
        ));
    }

    #[test]
    fn agy_context_does_not_keep_an_unfinished_turns_step_usage() {
        for exit in [false, true] {
            let mut a = AgyAdapter::default();
            a.feed(
                r#"{"event":"result","result":{"status":"SUCCESS","usage":{"input_tokens":1000}}}"#,
            );
            a.feed(r#"{"event":"step_update","step_update":{"step_type":"agent_response","state":"DONE","step_index":1,"usage":{"input_tokens":500}}}"#);
            if exit {
                a.on_exit(None);
            } else {
                a.encode(Command::Prompt {
                    text: "next turn".into(),
                })
                .expect("prompt");
            }
            a.encode(Command::Control {
                id: "retained".into(),
                control: Control::ContextUsage,
            })
            .expect("control");
            assert!(matches!(
                &a.drain_outbox().events[0].event,
                Event::UsageUpdated { used: 1000, .. }
            ));
            a.feed(
                r#"{"event":"result","result":{"status":"SUCCESS","usage":{"input_tokens":800}}}"#,
            );
            a.encode(Command::Control {
                id: "fallback".into(),
                control: Control::ContextUsage,
            })
            .expect("control");
            assert!(matches!(
                &a.drain_outbox().events[0].event,
                Event::UsageUpdated { used: 800, .. }
            ));
        }
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
        // agy's own "error: interrupted" is the user's stop, never an error card ("Turn failed").
        assert!(!ev.iter().any(|e| matches!(&e.event, Event::Error { .. })));
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

    /// agy's policy writes `{"action","display_name"}` objects (the frame below is a real one,
    /// from a stored thread): the notice names them, never their JSON.
    #[test]
    fn policy_denials_are_named_not_dumped_as_json() {
        let mut a = AgyAdapter::default();
        let ev = a.feed(
            r#"{"event":"result","result":{"conversation_id":"54bedcf5-6f77-4cf4-8c29-88d0be861976","status":"SUCCESS","denied_actions":[{"action":"command","display_name":"RunCommand"},{"action":"read_file","display_name":"ViewFile"},{"action":"write_file","display_name":"ReplaceFileContent"}]}}"#,
        );
        let Event::Notice { text } = &ev[0].event else {
            panic!("{ev:?}")
        };
        assert_eq!(
            text,
            "agy denied these actions: RunCommand (command), ViewFile (read_file), \
             ReplaceFileContent (write_file)"
        );
        assert!(!text.contains('{'), "{text}");
        assert!(matches!(
            ev[1].event,
            Event::TurnCompleted {
                state: TurnState::Completed,
                ..
            }
        ));
    }

    #[test]
    fn every_denied_shape_reads_as_words() {
        let name = |v: Value| denied_name(&v);
        assert_eq!(
            name(json!({"action": "command", "display_name": "RunCommand"})),
            "RunCommand (command)"
        );
        assert_eq!(name(json!({"display_name": "ViewFile"})), "ViewFile");
        assert_eq!(name(json!({"action": "read_file"})), "read_file");
        assert_eq!(name(json!({"tool_name": "Bash"})), "Bash");
        assert_eq!(name(json!({"name": "write_to_file"})), "write_to_file");
        assert_eq!(name(json!("run_command")), "run_command");
        // The same word twice is said once; blanks do not count.
        assert_eq!(name(json!({"action": "x", "display_name": "x"})), "x");
        assert_eq!(
            name(json!({"action": "command", "display_name": " "})),
            "command"
        );
        // Unknown: its JSON, rather than nothing.
        assert_eq!(name(json!({"what": 1})), r#"{"what":1}"#);
    }

    /// An unexpected exit right after a turn with denials says which, and that a message
    /// resumes; an expected exit, or one after a new prompt, adds nothing.
    #[test]
    fn an_exit_right_after_denials_names_them() {
        let denial = r#"{"event":"result","result":{"status":"SUCCESS","denied_actions":[{"action":"command","display_name":"RunCommand"}]}}"#;
        let notice = |ev: &[Envelope]| {
            ev.iter().find_map(|e| match &e.event {
                Event::Notice { text } if text.starts_with("agy exited") => Some(text.clone()),
                _ => None,
            })
        };

        let mut a = AgyAdapter::default();
        a.feed(denial);
        let ev = a.on_exit(Some(1));
        let text = notice(&ev).expect("a notice");
        assert!(text.contains("RunCommand (command)"), "{text}");
        assert!(text.contains("Send a message to continue"), "{text}");
        assert!(matches!(
            ev.last().expect("exit").event,
            Event::SessionExited {
                code: Some(1),
                expected: false
            }
        ));

        let mut a = AgyAdapter::default();
        a.feed(denial);
        a.encode(Command::Interrupt).expect("interrupt");
        assert_eq!(notice(&a.on_exit(Some(1))), None, "expected exit");

        let mut a = AgyAdapter::default();
        a.feed(denial);
        a.encode(Command::Prompt {
            text: "go on".into(),
        })
        .expect("prompt");
        assert_eq!(notice(&a.on_exit(Some(1))), None, "a new turn began");

        let mut a = AgyAdapter::default();
        assert_eq!(notice(&a.on_exit(Some(1))), None, "no denials");
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
        // Dangerous permissive: launched as Ask is; the hook is what allows everything.
        assert_eq!(a.argv(&session(Mode::Bypass, true)), argv);
    }

    #[test]
    fn argv_without_hook_maps_modes_to_flags_and_never_skips_permissions() {
        let a = AgyAdapter::default();
        for (mode, flag) in [
            (Mode::Ask, "plan"),
            (Mode::AcceptEdits, "accept-edits"),
            (Mode::Plan, "plan"),
            (Mode::Bypass, "plan"),
        ] {
            let argv = a.argv(&session(mode, false));
            assert!(argv.windows(2).any(|w| w == ["--mode", flag]), "{mode:?}");
            assert!(!argv.contains(&"--dangerously-skip-permissions".to_owned()));
        }
    }

    #[test]
    fn a_steer_is_refused_so_the_host_keeps_its_queue() {
        let mut a = AgyAdapter::new("/usr/bin/agy");
        assert!(matches!(
            a.encode(Command::Steer {
                text: "also".into()
            }),
            Err(AdapterError::Unsupported(_))
        ));
        assert!(matches!(
            a.encode(Command::DelegatedSteer {
                text: "also".into(),
                turn: "t".into(),
                tag: "op".into(),
            }),
            Err(AdapterError::Unsupported(_))
        ));
        assert!(!a.capabilities().live_input);
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

    /// `(profile args, what reaches agy)`. agy is a Go binary on the standard `flag` package
    /// (its strings carry "flag provided but not defined", "bad flag syntax", "flag needs an
    /// argument"): one or two dashes, `=value` or the next argument as the value of a string flag
    /// (even one starting with `-`). Owned flags are dropped after `--` too: Go's `--` ends only
    /// the FlagSet parsing it, and a subcommand's FlagSet parses what follows again, so a flag
    /// there is not provably inert. Spellings with `-` and `_` swapped are dropped as well.
    const PROFILE_ARGS: &[(&[&str], &[&str])] = &[
        // The skip flag: a boolean, so it never takes the next argument.
        (&["--dangerously-skip-permissions"], &[]),
        (&["--dangerously-skip-permissions=true"], &[]),
        (&["-dangerously-skip-permissions"], &[]),
        (&["-dangerously-skip-permissions=1"], &[]),
        (&["--dangerously_skip_permissions=true"], &[]),
        (&["--dangerously-skip-permissions", "kept"], &["kept"]),
        // The config-dir flags, inline or with the next argument as their value.
        (&["--gemini_dir=/tmp/alt"], &[]),
        (&["--gemini_dir", "/tmp/alt"], &[]),
        (&["-gemini_dir=/tmp/alt"], &[]),
        (&["-gemini_dir", "/tmp/alt"], &[]),
        (&["--gemini-dir=/tmp/alt"], &[]),
        (&["--gemini_dir", "--keep"], &[]),
        (&["--app_data_dir=/tmp/alt"], &[]),
        (&["--app_data_dir", "/tmp/alt"], &[]),
        (&["-app-data-dir", "/tmp/alt"], &[]),
        (&["--app-data_dir=/tmp/alt"], &[]),
        // Last: dropped alone, so it cannot take the adapter's own flags as its value.
        (&["--keep", "--gemini_dir"], &["--keep"]),
        // After `--`: still dropped (see above); `--` itself and plain words are kept.
        (
            &["--", "models", "--gemini_dir=/tmp/alt", "word"],
            &["--", "models", "word"],
        ),
        (
            &[
                "--",
                "--dangerously-skip-permissions",
                "--app_data_dir",
                "/tmp/alt",
            ],
            &["--"],
        ),
        // Everything the adapter does not own is passed through untouched.
        (
            &[
                "--x",
                "-",
                "--",
                "value",
                "--gemini_dirt",
                "--sandbox=gemini_dir",
            ],
            &[
                "--x",
                "-",
                "--",
                "value",
                "--gemini_dirt",
                "--sandbox=gemini_dir",
            ],
        ),
        (
            &["---gemini_dir=/tmp/alt"],
            &["---gemini_dir=/tmp/alt"], // agy refuses it: "bad flag syntax"
        ),
    ];

    #[test]
    fn profile_args_drop_exactly_the_flags_the_adapter_owns() {
        for (given, kept) in PROFILE_ARGS {
            let given: Vec<String> = given.iter().map(|a| (*a).to_owned()).collect();
            assert_eq!(profile_args(&given), *kept, "{given:?}");
        }
    }

    /// The hook proof reads `$HOME/.gemini`; a profile flag that moves agy's configuration would
    /// make agy load hooks the proof never saw while the skip flag is passed. Whatever the
    /// profile says, the skip flag appears exactly when the adapter decides it.
    #[test]
    fn extra_args_cannot_move_the_config_dir_or_set_the_skip_flag() {
        let a = AgyAdapter::default();
        for (given, kept) in PROFILE_ARGS {
            for hook in [false, true] {
                let mut s = session(Mode::Ask, hook);
                s.extra_args = given.iter().map(|a| (*a).to_owned()).collect();
                let argv = a.argv(&s);
                assert_eq!(&argv[1..=kept.len()], *kept, "{given:?}");
                assert_eq!(
                    argv[kept.len() + 1..]
                        .iter()
                        .filter(|x| x.as_str() == SKIP_PERMISSIONS)
                        .count(),
                    usize::from(hook),
                    "{given:?}: the skip flag stays the adapter's decision alone"
                );
            }
        }
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

    #[test]
    fn thinking_attached_to_actions_and_responses_emits_reasoning_items() {
        let mut a = AgyAdapter::default();
        a.encode(Command::Prompt { text: "hi".into() }).unwrap();
        a.feed(r#"{"event":"init","conversation_id":"c1","init":{}}"#);

        // Tool step with thinking
        let events = a.feed(r#"{"event":"step_update","conversation_id":"c1","step_update":{"step_type":"tool","step_index":1,"state":"DONE","thinking":"Let me inspect the file","tool_name":"view_file","tool_info":{"parameters":{"path":"src/lib.rs"},"output":"ok"}}}"#);
        assert!(events.iter().any(|e| matches!(
            &e.event,
            Event::ItemStarted {
                kind: ItemKind::Reasoning,
                ..
            }
        )));
        assert!(events.iter().any(|e| matches!(&e.event, Event::ContentSnapshot { stream: StreamKind::Reasoning, text } if text == "Let me inspect the file")));
        assert!(events.iter().any(|e| matches!(
            &e.event,
            Event::ItemStarted {
                kind: ItemKind::FileRead,
                ..
            }
        )));

        // Response step with thought
        let events = a.feed(r#"{"event":"step_update","conversation_id":"c1","step_update":{"step_type":"agent_response","step_index":2,"state":"DONE","thought":"Now explain what was found","text_delta":"The file looks good."}}"#);
        assert!(events.iter().any(|e| matches!(
            &e.event,
            Event::ItemStarted {
                kind: ItemKind::Reasoning,
                ..
            }
        )));
        assert!(events.iter().any(|e| matches!(&e.event, Event::ContentSnapshot { stream: StreamKind::Reasoning, text } if text == "Now explain what was found")));
        assert!(events.iter().any(|e| matches!(&e.event, Event::ContentDelta { stream: StreamKind::Assistant, text } if text == "The file looks good.")));
    }

    /// A step whose stdout carries no parameters takes them from the transcript's record of
    /// the call, decoded, and titles
    /// `Read /etc/hostname`.
    #[test]
    fn w0_transcript_calls_fill_a_bare_step_and_title_it() {
        let transcript: Value = serde_json::from_str(
            include_str!("../../../tests/fixtures/real/agy/transcript-view-file.jsonl").trim(),
        )
        .expect("transcript line");
        let calls = transcript["tool_calls"].clone();
        let mut a = AgyAdapter::default();
        a.encode(Command::Prompt { text: "hi".into() }).unwrap();
        a.feed(r#"{"event":"init","conversation_id":"c1","init":{}}"#);
        let step = json!({"event": "step_update", "conversation_id": "c1", "step_update": {
            "step_type": "tool", "step_index": 2, "state": "ACTIVE", "tool_name": "view_file",
            "tool_info": {"name": "view_file"}, "transcript_calls": calls}});
        let out = a.feed(&step.to_string());
        let (kind, title, input) = out
            .iter()
            .find_map(|e| match &e.event {
                Event::ItemStarted {
                    kind: kind @ ItemKind::FileRead,
                    title,
                    input,
                    ..
                } => Some((*kind, title.clone(), input.clone())),
                _ => None,
            })
            .expect("tool item");
        let input = input.expect("input from the transcript");
        assert_eq!(input["AbsolutePath"], "/etc/hostname");
        assert_eq!(
            crate::tool_title::classify(kind, &title, Some(&input)).text(),
            "Read /etc/hostname"
        );
    }

    /// Recorded agy frames (`read-outside-project.jsonl`) with the thinking the
    /// session reads from agy's transcript put on the step, as `enrich_agy_step` does: one
    /// reasoning block, Complete at once (never Streaming), its text whole, delivered once even
    /// though the step is reported twice (ACTIVE, then DONE).
    #[test]
    fn w0_agy_thinking_is_one_whole_block_never_streamed() {
        let fixture = include_str!("../../../tests/fixtures/real/agy/read-outside-project.jsonl");
        let mut a = AgyAdapter::default();
        a.encode(Command::Prompt { text: "hi".into() }).unwrap();
        let mut out = Vec::new();
        for line in fixture.lines() {
            let mut frame: Value = serde_json::from_str(line).expect("fixture line");
            if let Some(step) = frame.get_mut("step_update").and_then(Value::as_object_mut) {
                step.insert("thinking".into(), json!("Reading the hostname file."));
            }
            out.extend(a.feed(&frame.to_string()));
        }
        let states: Vec<ReasoningState> = out
            .iter()
            .filter_map(|e| match e.event {
                Event::Reasoning { state, label } => {
                    assert_eq!(label, ReasoningLabel::Thinking);
                    Some(state)
                }
                _ => None,
            })
            .collect();
        assert_eq!(states, [ReasoningState::Complete]);
        assert!(!out.iter().any(|e| matches!(
            e.event,
            Event::ContentDelta {
                stream: StreamKind::Reasoning,
                ..
            }
        )));
        let texts: Vec<&str> = out
            .iter()
            .filter_map(|e| match &e.event {
                Event::ContentSnapshot {
                    stream: StreamKind::Reasoning,
                    text,
                } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["Reading the hostname file."]);
    }

    /// One `step_update` for an adapter mid-turn in conversation `c1`.
    fn step_of(a: &mut AgyAdapter, step: Value) -> Vec<Envelope> {
        let mut frame = json!({"event": "step_update", "conversation_id": "c1"});
        frame["step_update"] = step;
        a.feed(&frame.to_string())
    }

    fn turn_adapter() -> AgyAdapter {
        let mut a = AgyAdapter::default();
        a.encode(Command::Prompt { text: "hi".into() }).unwrap();
        a.feed(r#"{"event":"init","conversation_id":"c1","init":{}}"#);
        a
    }

    fn started_items(out: &[Envelope]) -> Vec<(ItemKind, String)> {
        out.iter()
            .filter_map(|e| match &e.event {
                Event::ItemStarted { kind, title, .. } => Some((*kind, title.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn each_side_command_gets_its_own_id() {
        let mut a = AgyAdapter::new("agy");
        let ids: Vec<String> = ["/model", "/usage", "/model"]
            .into_iter()
            .map(
                |text| match a.encode(Command::Prompt { text: text.into() }) {
                    Ok(actions) => match &actions[0] {
                        Action::SideProcess { id, .. } => id.clone(),
                        other => panic!("{text}: {other:?}"),
                    },
                    Err(e) => panic!("{text}: {e}"),
                },
            )
            .collect();
        assert_eq!(ids, ["side-1", "side-2", "side-3"]);
    }

    #[test]
    fn agy_needs_no_handshake() {
        // Nothing may be written before the first prompt: agy reads stdin as the user turn.
        assert!(AgyAdapter::default().handshake().is_empty());
    }

    #[test]
    fn web_mcp_and_subagent_tools_have_their_own_kinds() {
        for (name, kind) in [
            ("search_web", ItemKind::WebSearch),
            ("read_url_content", ItemKind::WebSearch),
            ("call_mcp_tool", ItemKind::McpTool),
            ("start_subagent", ItemKind::Subagent),
            ("invoke_subagent", ItemKind::Subagent),
            ("something_new", ItemKind::Tool),
        ] {
            assert_eq!(tool_kind(name), kind, "{name}");
        }
    }

    #[test]
    fn a_tool_step_is_a_tool_whether_typed_or_only_named() {
        let mut a = turn_adapter();
        // Typed "tool", named only in its info.
        let out = step_of(
            &mut a,
            json!({"step_type": "tool", "step_index": 1, "state": "ACTIVE",
                   "tool_info": {"name": "run_command", "parameters": {"CommandLine": "ls"}}}),
        );
        assert_eq!(started_items(&out), [(ItemKind::Command, "ls".to_owned())]);
        // A step type this build does not know, but carrying a tool name, is still that tool.
        let out = step_of(
            &mut a,
            json!({"step_type": "tool_call_v2", "step_index": 2, "state": "ACTIVE",
                   "tool_name": "view_file", "tool_info": {"parameters": {"AbsolutePath": "/a"}}}),
        );
        assert_eq!(started_items(&out), [(ItemKind::FileRead, "/a".to_owned())]);
        // Neither: unknown, never a made-up item.
        let out = step_of(&mut a, json!({"step_type": "telemetry", "step_index": 3}));
        assert!(started_items(&out).is_empty());
        assert!(out.iter().any(|e| matches!(e.event, Event::Unknown)));
    }

    #[test]
    fn a_failed_response_step_fails_its_item() {
        let mut a = turn_adapter();
        let step = |state: &str| {
            json!({"step_type": "agent_response", "step_index": 4, "state": state,
                   "text_delta": "partial"})
        };
        step_of(&mut a, step("ACTIVE"));
        let out = step_of(&mut a, step("ERROR"));
        assert!(
            out.iter().any(|e| matches!(
                e.event,
                Event::ItemCompleted {
                    status: ItemStatus::Failed,
                    ..
                }
            )),
            "{out:?}"
        );
    }

    #[test]
    fn thinking_completes_only_when_its_step_is_done() {
        let mut a = turn_adapter();
        let step = |state: &str| {
            json!({"step_type": "thinking", "step_index": 6, "state": state,
                   "thinking": "weighing it"})
        };
        let done = |out: &[Envelope]| {
            out.iter()
                .any(|e| matches!(e.event, Event::ItemCompleted { .. }))
        };
        assert!(!done(&step_of(&mut a, step("ACTIVE"))), "still thinking");
        assert!(done(&step_of(&mut a, step("DONE"))), "done thinking");
    }

    #[test]
    fn tool_parameters_fall_back_to_the_transcript_and_always_name_the_tool() {
        let mut a = turn_adapter();
        let input_of = |out: &[Envelope]| {
            out.iter()
                .find_map(|e| match &e.event {
                    Event::ItemStarted { input, .. } => input.clone(),
                    _ => None,
                })
                .expect("an input")
        };
        // Empty stdout parameters lose to the transcript's record of the call.
        let out = step_of(
            &mut a,
            json!({"step_type": "tool", "step_index": 1, "state": "ACTIVE",
                   "tool_name": "view_file", "tool_info": {"parameters": {}},
                   "transcript_calls": [{"name": "view_file",
                                         "args": {"AbsolutePath": "\"/etc/hosts\""}}]}),
        );
        let input = input_of(&out);
        assert_eq!(input["AbsolutePath"], "/etc/hosts");
        assert_eq!(
            input["tool_name"], "view_file",
            "the tool is named in its input"
        );
        // A tool_name the parameters already carry is kept, not overwritten.
        let out = step_of(
            &mut a,
            json!({"step_type": "tool", "step_index": 2, "state": "ACTIVE",
                   "tool_name": "call_mcp_tool",
                   "tool_info": {"parameters": {"tool_name": "github.search"}}}),
        );
        assert_eq!(input_of(&out)["tool_name"], "github.search");
    }

    #[test]
    fn a_file_change_card_is_titled_by_its_tool() {
        let mut a = turn_adapter();
        let out = step_of(
            &mut a,
            json!({"step_type": "tool", "step_index": 3, "state": "ACTIVE",
                   "tool_name": "write_to_file",
                   "tool_info": {"parameters": {"TargetFile": "/w/a.rs"}}}),
        );
        assert_eq!(
            started_items(&out),
            [(ItemKind::FileChange, "write_to_file".to_owned())]
        );
    }

    #[test]
    fn an_empty_conversation_id_does_not_make_an_empty_item_prefix() {
        let mut a = AgyAdapter::default();
        a.encode(Command::Prompt { text: "hi".into() }).unwrap();
        let out = a.feed(
            &json!({"event": "step_update", "step_update": {"conversation_id": "",
                     "step_type": "tool", "step_index": 1, "state": "ACTIVE",
                     "tool_name": "run_command"}})
            .to_string(),
        );
        let id = out.iter().find_map(|e| e.item.clone()).expect("an item id");
        assert!(!id.starts_with(':'), "{id}");
        assert!(id.ends_with(":step-1"), "{id}");
    }

    #[test]
    fn a_turn_with_an_error_fails_even_when_the_status_says_success() {
        let mut a = turn_adapter();
        let out =
            a.feed(r#"{"event":"result","result":{"status":"SUCCESS","error":"tool crashed"}}"#);
        assert!(
            out.iter().any(|e| matches!(
                &e.event,
                Event::TurnCompleted { state: TurnState::Failed, error: Some(m), .. } if m == "tool crashed"
            )),
            "{out:?}"
        );
    }

    #[test]
    fn a_quota_error_raises_a_rate_limit_whatever_the_status() {
        let mut a = turn_adapter();
        let out = a.feed(
            r#"{"event":"result","result":{"status":"FAILED","error":"You have exhausted your quota"}}"#,
        );
        assert!(
            out.iter()
                .any(|e| matches!(e.event, Event::RateLimited { .. })),
            "{out:?}"
        );
    }
}
