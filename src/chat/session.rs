//! One chat thread's backend: adapter + agent process + store + switching and handoff.
//! Implements [`super::ChatBackend`].
//!
//! Everything runs on the GTK main thread. The session is shared as an `Rc`; every callback it
//! hands out (process lines, process exit, approval envelopes, side-process results) holds a
//! `Weak`, so dropping the session ends the process and removes the approval socket.
//!
//! No `RefCell` borrow is ever held across a call that can re-enter (the sink, an adapter call
//! that emits, the process): the view may call straight back into the backend from the sink.
//!
//! Logging never includes prompt or frame bodies.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use agent_core::adapter::{
    Action, Adapter, AdapterError, Command, Control, Driver, Mode, OpenSession, OpenSessionDelta,
};
use agent_core::catalog::Replacement;
use agent_core::event::{
    AgentCommand, Decision, Envelope, Event, ItemKind, ItemStatus, StreamKind, TurnState,
};
use agent_core::handoff_budget::{
    handoff_budget, handoff_coverage, provider_message_with_handoff, render_history,
    select_history, HistoricalMessage, Role, DEFAULT_HANDOFF_TOKEN_CAP,
};
use agent_core::transition::{
    decide_transition, plan_selection, ModelSelection, SessionState, Transition,
};
use agent_kit::store::{ProviderThreadId, Store, ThreadId, TranscriptMessage};
use agent_kit::store_writer::StoreHandle;
use gtk4::glib;
use tracing::{debug, info, warn};

mod agy_transcript;
mod codex_policy;
mod delegated;
mod reply_questions;

pub(crate) use delegated::{HeldNews, TakenNews, WakeOutcome};

use super::{ChatBackend, EnvelopeSink, SessionStatus};
use crate::agent_proc::{
    run_side, AgentEnv, AgentProcess, ExitWatch, SpawnSpec, StopPhase, AGY_TIMEOUT,
};
use crate::approval_server::ApprovalHandle;
use crate::delegation::seam::{
    ChildPolicy, DelegationHooks, DelegationLaunchFacts, GateVerdict, SessionOptions, TurnBoundary,
};
use agent_core::delegation::ProcessGeneration;

/// Process environment the adapter does not own: what the agent's profile adds (its env file)
/// and what the app removes (`clear_env`, a launching agent session's markers).
pub type LaunchEnv = AgentEnv;

/// agy's approval socket, or why it has none: `Err(None)` when the hook is not installed (and
/// for Claude and Codex, which never use one); `Err(Some(reason))` when it is installed but the
/// socket could not be bound (say, the package was upgraded under the running app).
pub type Approval = Result<ApprovalHandle, Option<String>>;

/// Everything a switch to another agent starts with. The window builds it from that agent's
/// profile; this module does not know profiles.
pub struct AgentLaunch {
    pub adapter: Box<dyn Adapter>,
    /// The binary, resolved from the profile (absolute when detection found it).
    pub program: String,
    /// The profile's own arguments.
    pub extra_args: Vec<String>,
    /// The agent's configured default model, used when the switch names none.
    pub default_model: Option<String>,
    /// The agent's configured default effort, used when the switch names none.
    pub default_effort: Option<String>,
    pub env: LaunchEnv,
    /// agy: the approval socket for the new process. Without one agy cannot ask (Ask runs as
    /// Plan; see [`effective_mode`]).
    pub approval: Approval,
}

/// `Continued on <new model> (<old> was retired)`.
pub fn retired_notice(retired: &RetiredModel) -> String {
    format!(
        "Continued on {} ({} was retired)",
        retired.replacement.display, retired.model
    )
}

/// A model its agent no longer offers, and where the thread goes instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredModel {
    pub model: String,
    pub replacement: Replacement,
}

/// Builds what another driver starts with (the window knows how; this module does not). An `Err`
/// is the reason the driver cannot be started (missing or switched off): the switch is refused
/// with it and nothing changes.
pub type AdapterFactory = Rc<dyn Fn(Driver) -> Result<AgentLaunch, String>>;

/// How long a state-changing tool step may run before its hook query must have arrived.
const CANARY_GRACE: Duration = Duration::from_secs(8);

/// Proves agy's approval hook for one launch ([`crate::hook_config::check_target`]); run off the
/// main thread before every agy process that would get `--dangerously-skip-permissions`.
pub(crate) type HookCheck =
    std::sync::Arc<dyn Fn(&crate::hook_config::HookTarget) -> Result<(), String> + Send + Sync>;

/// The real launch proof. Under test it accepts everything unless the test installed its own
/// (as [`Inner::home`] reads no real home): a test session must never read this machine's
/// `~/.gemini`. Tests of the proof itself install the real check with [`set_test_hook_check`].
fn default_hook_check() -> HookCheck {
    #[cfg(test)]
    {
        TEST_HOOK_CHECK
            .with(|check| check.borrow().clone())
            .unwrap_or_else(|| std::sync::Arc::new(|_| Ok(())))
    }
    #[cfg(not(test))]
    {
        std::sync::Arc::new(crate::hook_config::check_target)
    }
}

#[cfg(test)]
thread_local! {
    /// Each test runs on its own thread, so this never leaks between tests.
    static TEST_HOOK_CHECK: RefCell<Option<HookCheck>> = const { RefCell::new(None) };
}

/// The check every session this test thread creates from now on runs before a hooked agy start.
#[cfg(test)]
fn set_test_hook_check(check: HookCheck) {
    TEST_HOOK_CHECK.with(|slot| *slot.borrow_mut() = Some(check));
}

/// An agy process waiting on its fresh hook proof: what was written to it meanwhile, in order,
/// and the user messages behind those writes, as the user wrote them: a start that ends without
/// a process (refused, failed to spawn, torn down) queues them again rather than dropping them.
struct PendingStart {
    generation: u64,
    writes: Vec<String>,
    prompts: Vec<QueuedPrompt>,
}

fn delegation_redirect_names(spec: &SpawnSpec) -> Vec<String> {
    use agent_core::delegation::agy_guard::{CONFIG_REDIRECT_VARS, REDIRECT_VAR_PREFIX};
    let relevant =
        |name: &str| CONFIG_REDIRECT_VARS.contains(&name) || name.starts_with(REDIRECT_VAR_PREFIX);
    let mut names = std::env::vars_os()
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .filter(|name| relevant(name) && !spec.unset.contains(name))
        .collect::<std::collections::BTreeSet<_>>();
    for (name, _) in &spec.env {
        if relevant(name) {
            names.insert(name.clone());
        }
    }
    names.into_iter().collect()
}

/// What a refused fresh hook proof changed, to be said once its hookless process has spawned.
struct RefusedHook {
    /// The socket taken from the session, dropped (closed) with the announcement: closing it
    /// may expire cards through the sink.
    socket: Option<ApprovalHandle>,
    mode_changed: Option<Mode>,
    text: String,
}

/// The hooked tool names a started item of this kind can be. `None`: not watched (see
/// [`Inner::watch_tool_step`]).
fn canary_tools(kind: ItemKind) -> Option<&'static [&'static str]> {
    match kind {
        ItemKind::Command => Some(&["run_command", "send_command_input"]),
        ItemKind::FileChange => Some(&[
            "replace_file_content",
            "multi_replace_file_content",
            "write_to_file",
            "sed_file",
            "notebook_edit",
        ]),
        ItemKind::McpTool => Some(&["call_mcp_tool"]),
        ItemKind::Subagent => Some(&["start_subagent", "invoke_subagent"]),
        ItemKind::WebSearch => Some(&["search_web", "read_url_content"]),
        ItemKind::FileRead
        | ItemKind::Tool
        | ItemKind::UserMessage
        | ItemKind::AssistantMessage
        | ItemKind::Reasoning => None,
    }
}

/// Room assumed for the user's next prompt when budgeting a handoff, in budget units.
const HANDOFF_PROMPT_ALLOWANCE: usize = 2_048;

struct State {
    model: Option<String>,
    /// Reasoning effort of the running session (Claude `--effort`); `None` is the agent's own.
    effort: Option<String>,
    mode: Mode,
    running_turn: bool,
    alive: bool,
    commands: Vec<AgentCommand>,
    native_id: Option<String>,
}

/// An unacknowledged selection, kept separate from the model reported by the agent.
struct PendingModel {
    model: String,
    request: Option<String>,
    effort: Option<String>,
    previous_model: Option<String>,
    previous_effort: Option<String>,
    restarting: bool,
}

fn accepted_model_choice(driver: Driver, requested: Option<&str>, reported: &str) -> String {
    let alias = requested.filter(|choice| {
        driver == Driver::Claude
            && (*choice == "default"
                || matches!(*choice, "opus" | "sonnet" | "haiku")
                    && agent_core::catalog::family_of(reported) == *choice)
    });
    alias.unwrap_or(reported).to_owned()
}

/// An open approval a rule can describe: the call, the working directory its rule is judged in,
/// and the folder its card offered for "Allow for session" (pinned against a later re-resolve).
type PendingRule = (crate::always_allow::Action, Option<String>, Option<PathBuf>);

struct Inner {
    adapter: RefCell<Box<dyn Adapter>>,
    open: RefCell<OpenSession>,
    /// Writes are queued to the store writer (never run on this thread); the reads that need
    /// this session's own writes go through its barrier ([`StoreHandle::read`]).
    store: StoreHandle,
    thread: ThreadId,
    provider_thread: RefCell<ProviderThreadId>,
    sink: EnvelopeSink,
    /// Cleared (never re-set) when the canary proves the hook is not gating agy.
    approval: RefCell<Option<ApprovalHandle>>,
    /// Why agy has no socket although its hook is installed (see [`Approval`]); said in the
    /// notice instead of "install the hook".
    unbound: RefCell<Option<String>>,
    /// How long a tool step may wait for its hook query to show up.
    canary_grace: Cell<Duration>,
    /// Tool item ids that were cancelled, declined or failed before completion (not tripped by canary).
    canary_cancelled: RefCell<HashSet<String>>,
    /// The fresh proof every hooked agy start waits on (see [`Inner::start_process`]).
    hook_check: RefCell<HookCheck>,
    /// The agy start waiting on that proof, if any.
    starting: RefCell<Option<PendingStart>>,
    factory: RefCell<Option<AdapterFactory>>,
    /// Whether a switch to agy now would get its approval hook (the window's cached proof). Only
    /// says so before the switch; the launch itself proves the hook as always.
    agy_can_ask: RefCell<Option<Rc<dyn Fn() -> bool>>>,
    launch_env: RefCell<LaunchEnv>,
    proc: RefCell<Option<AgentProcess>>,
    state: RefCell<State>,
    /// A rendered, redacted handoff the new agent has not confirmed yet. Stored as
    /// `Event::HandoffSeeded` too, so a restart re-arms it; spent only by a completed turn.
    pending_handoff: RefCell<Option<Handoff>>,
    /// The pending handoff went out with the prompt now running: not sent again unless that turn
    /// ends without completing.
    handoff_in_flight: Cell<bool>,
    /// Which turn the prompt carrying the handoff is in, while it is in flight
    /// ([`handoff_turn_end`]).
    handoff_turn: Cell<HandoffTurn>,
    /// The `result` being read ends a turn Claude ran itself for a finished background task
    /// (`origin.kind` "task-notification"), not one a prompt of ours started.
    turn_from_task: Cell<bool>,
    /// The agent a switch left, until the new one reports a session (see [`Previous`]).
    rollback: RefCell<Option<Previous>>,
    /// The thread's model was retired by its agent: the next prompt first moves to this one.
    retired: RefCell<Option<RetiredModel>>,
    /// The model the thread was last SET to (an alias where the agent has one), as opposed to the
    /// resolved id its events report: what is stored and what retirement is judged on.
    selected: RefCell<Option<String>>,
    pending_model: RefCell<Option<PendingModel>>,
    /// The hook already enforces this mode; only agy's process restart waits for the turn.
    deferred_mode: Cell<Option<Mode>>,
    ctl_seq: Cell<u64>,
    /// Bumped on every (re)start and stop: callbacks of an older process are ignored.
    generation: Cell<u64>,
    /// Deliberate teardown must not dispatch queued work from its closing envelopes.
    stopping: Cell<bool>,
    /// Retirement can switch agents while dispatching; that switch must not pop another prompt.
    dispatching_queue: Cell<bool>,
    /// Nested envelopes finish reaching the view before a queued prompt can start.
    delivering: Cell<bool>,
    queued_dispatch_pending: Cell<bool>,
    /// Bumped when the adapter is replaced (a switch to another agent, a rollback); unlike
    /// `generation`, a restart of the same agent keeps it. A side process (agy's usage or model
    /// list) that finishes after a switch is answered with an error, never fed to the new agent.
    adapter_epoch: Cell<u64>,
    /// The mode the user last chose. The running mode can differ (agy without its hook cannot
    /// ask), and a switch to an agent that can honour it goes back to it.
    wanted_mode: Cell<Mode>,
    /// The user's last choice other than [`Mode::Bypass`]: what is stored, and what a switch to
    /// another agent goes back to, since dangerous permissive mode never outlives its agent.
    safe_mode: Cell<Mode>,
    /// Open Claude and Codex approvals a rule can describe: the call and the working directory
    /// its rule is judged in (`None`: relative paths never resolve), by request id, so "Allow for
    /// session" / "Always allow" can record it.
    pending_rules: RefCell<HashMap<String, PendingRule>>,
    /// This thread's session rules, each tagged with its agent; shared with agy's approval
    /// server, kept through a same-agent restart, cleared by a cross-agent switch.
    session_rules: crate::approval_server::SessionRules,
    /// The Always rules ([`crate::always_allow::path`]), cached and re-read when the file
    /// changes; `None` offers no "Always".
    saved_rules: RefCell<Option<crate::always_allow::StoreCache>>,
    /// The unusable-rules-file notice was already shown in this thread.
    rules_blocked_noticed: Cell<bool>,
    /// Queued user prompts submitted while a turn is already running.
    queued_prompts: RefCell<std::collections::VecDeque<QueuedPrompt>>,
    /// Already published input held when a startup retry waits for root rebinding. Its text
    /// already includes any handoff, so it must not enter the ordinary prompt builder again.
    held_retries: RefCell<std::collections::VecDeque<Sent>>,
    launch_facts: RefCell<Option<DelegationLaunchFacts>>,
    weak_self: RefCell<Option<std::rc::Weak<Inner>>>,
    prompt_in_flight: Cell<bool>,
    /// Last step index enriched with thinking from Antigravity transcript, avoiding repeated disk reads.
    last_enriched_step: Cell<Option<u64>>,
    /// A new Codex thread id and its provider thread, not stored until a turn on it produces an
    /// item or completes: Codex has no rollout before that, so a stored id could not be resumed
    /// (plan 2.5).
    provisional_native: RefCell<Option<(ProviderThreadId, String)>>,
    /// The process generation whose agy transcript read is in flight (off the main thread).
    /// Lines that arrive meanwhile wait in `agy_hold`, so the adapter still sees them in order.
    agy_reading: Cell<Option<u64>>,
    agy_hold: RefCell<std::collections::VecDeque<(String, bool)>>,
    /// An exit that arrived during that read; handled once the held lines are in.
    agy_deferred_exit: Cell<Option<(u64, Option<i32>)>>,
    /// How long a Claude permission request may wait for the user before it is denied (agy's
    /// socket has the same bound). Injectable for tests.
    approval_deadline: Cell<Duration>,
    /// Claude's `--thinking-display summarized` for the running process.
    thinking: ThinkingFlag,
    /// User input written before Claude's init, retained only for the thinking-flag retry.
    pre_init_prompts: RefCell<Vec<Sent>>,
    /// Open Codex permission requests, by request id: what the answer may be (see
    /// [`Inner::gate_codex_permissions`]). Never a rule.
    codex_permissions: RefCell<HashMap<String, PermissionCard>>,
    /// What Codex was allowed for its session (`scope: "session"`), said plainly; held by the
    /// app-server process, so forgotten when it ends.
    codex_grants: RefCell<Vec<String>>,
    /// Codex permission cards whose targets are being checked on disk (`hold_codex_permissions`).
    held_permissions: RefCell<HashSet<String>>,
    /// A Codex permission card shown again under a new id: adapter id → the id shown.
    codex_shown: RefCell<HashMap<String, String>>,
    /// Where session grants let Codex write (resolved), for later rename checks.
    codex_grant_paths: RefCell<Vec<PathBuf>>,
    /// Where this turn's grants let Codex write (resolved); cleared when the turn ends.
    codex_turn_grant_paths: RefCell<Vec<PathBuf>>,
    /// What the current turn said, for the quick replies offered when it completes.
    reply_turn: RefCell<reply_questions::ReplyTurn>,
    // ---- delegation ----
    /// A bound root's or a supervised child's delegation hooks.
    hooks: Option<Rc<dyn DelegationHooks>>,
    /// A supervised child's gate, publication boundary and fixed task mode.
    child: Option<ChildPolicy>,
    /// The contract's process generation: bumped only when a process starts (unlike
    /// `generation`, which a stop bumps too).
    process_generation: Cell<u64>,
    /// The last process generation closed out (`process_stopping` runs once per generation).
    stopped_generation: Cell<u64>,
    /// Every started generation's exit watch, dropped only once it reports `Exited`.
    watches: RefCell<Vec<(u64, ExitWatch)>>,
    /// A rebind stopped the process: nothing starts one until `rebind_start`.
    rebinding: Cell<bool>,
    /// The native id of the running turn (Codex), which a delegated steer is pinned to.
    native_turn: RefCell<Option<String>>,
    /// Open approval requests by the id the view knows them by, so revocation can end them.
    open_approvals: RefCell<Vec<String>>,
    /// Provider workers (Codex sub-agents) the agent last listed as still active: they run in
    /// the agent's process, so stopping it to save memory would end them.
    active_workers: Cell<usize>,
    /// Original tool/input for a final authority check immediately before answering a card.
    approval_inputs: RefCell<HashMap<String, (String, serde_json::Value)>>,
    /// Requests a child's gate let through only as a card with no grant (`AskOnly`).
    ask_only: RefCell<HashSet<String>>,
    /// The worker this session is (a root principal or a child), named by the supervisor; the
    /// recipient of every delegated message it is given.
    worker: RefCell<Option<agent_core::delegation::WorkerId>>,
    /// This launch consumed a prepared binding; same-name user MCP servers confer no trust.
    bound_generation: Cell<bool>,
    /// The process generation a binding was taken for whose process has not spawned yet. A
    /// start that ends without one releases the binding (`DelegationHooks::launch_abandoned`).
    unspawned_binding: Cell<Option<u64>>,
    /// Set while a busy switch or restart stops the process and carries its unfinished task to
    /// the next one itself (the handoff and the continuation): a message still waiting in a
    /// start that never spawned is part of that task, so it is not also queued again.
    carrying_task: Cell<bool>,
    /// The stored messages such a switch took over, held until its continuation is sent (they
    /// are in the transcript its handoff carries). A switch that sends none (it rolled back, or
    /// the replacement is not running) queues them again.
    carried_unsent: RefCell<Vec<QueuedPrompt>>,
    /// Set while [`Inner::suspend`] stops the process: its exit is not the thread's news, so the
    /// adapter's exit envelopes reach neither the view nor the store.
    quiet_exit: Cell<bool>,
    /// The process was suspended and nothing has started since: the next start is a resume the
    /// user did not ask for, so a delegation rebind around it says nothing.
    suspended: Cell<bool>,
    /// The current process generation is up: its adapter accepted the handshake's answer
    /// ([`Adapter::handshake_answered`]), or it reported its session or started a turn.
    /// Presentation only (reset by every start).
    reported: Cell<bool>,
    /// The current start resumes a suspended process: the header says "Resuming…" until it
    /// is up. Presentation only.
    resuming: Cell<bool>,
    /// The [`crate::chat::Lifecycle`] last given to `lifecycle_listener`.
    lifecycle_told: Cell<crate::chat::Lifecycle>,
    /// Told whenever the lifecycle changes, with or without an envelope (a silent handshake
    /// answer, a suspension). Set by the window; it must not call back into the session
    /// synchronously.
    lifecycle_listener: RefCell<Option<crate::chat::LifecycleListener>>,
    /// The native session has a conversation an agent can resume: this process was started
    /// resuming one, or a turn completed on it after a prompt was sent to it. A session id alone
    /// (a fresh start, Claude's prompt-less init turn) may name no transcript yet.
    has_conversation: Cell<bool>,
    /// The running process has taken a prompt (reset by every start).
    prompted: Cell<bool>,
    /// Turns started by sub-agent news since the user last spoke (see
    /// [`agent_core::delegation::wake::MAX_CONSECUTIVE_WAKES`]).
    auto_wakes: Cell<u32>,
    /// The user stopped this root: no sub-agent news wakes it until the user speaks again.
    wakes_held: Cell<bool>,
    /// Sub-agent news the supervisor holds for the user while this root is in dangerous
    /// permissive mode ([`delegated::HeldNews`]).
    held_news: RefCell<Option<Rc<dyn delegated::HeldNews>>>,
}

/// What the session decided about one Codex permission request, keyed by the id the view knows
/// it by.
#[derive(Debug, Clone)]
struct PermissionCard {
    /// The adapter's request id (the card's own id, unless it was shown again).
    wire: String,
    /// The card as the adapter sent it, before any narrowing: what is checked again.
    original: Envelope,
    /// What the disk check found when the card was shown; an answer is sent only if a fresh
    /// check finds the same.
    verdict: codex_policy::Verdict,
    /// An answer is being checked again.
    checking: bool,
    /// How many times the card was shown again.
    round: u32,
    /// Allow (this turn) may be sent: the card showed the request exactly.
    allow: bool,
    /// Allow for session may be sent: nothing it reaches is too broad or sensitive.
    session_allowed: bool,
    /// Why Allow for session is not offered, when it is not.
    warning: Option<String>,
    /// What the request grants, for the session-grant notice.
    summary: String,
}

/// Whether the running Claude process got `--thinking-display summarized`, and how it took it.
/// The flag is hidden, so a CLI that refuses it at start is retried once without it.
#[derive(Default)]
struct ThinkingFlag {
    /// The running process was started with the flag.
    passed: Cell<bool>,
    /// It said on stderr that it does not take the flag.
    refused: Cell<bool>,
    /// The one retry without the flag was spent: this thread never passes it again.
    retried: Cell<bool>,
    /// The running process sent its `init`: it started, so a later exit is not the CLI refusing
    /// the flag at start.
    init_seen: Cell<bool>,
}

struct QueuedPrompt {
    item_id: String,
    text: String,
    stored: bool,
}

struct Sent {
    text: String,
    handoff: Option<Handoff>,
}

struct ScopedFlag<'a> {
    flag: &'a Cell<bool>,
    previous: bool,
}

impl<'a> ScopedFlag<'a> {
    fn set(flag: &'a Cell<bool>) -> Self {
        Self {
            flag,
            previous: flag.replace(true),
        }
    }
}

impl Drop for ScopedFlag<'_> {
    fn drop(&mut self) {
        self.flag.set(self.previous);
    }
}

/// The mode a session can actually run in. agy without its approval hook cannot ask before
/// acting (headless, it has nobody to ask), so Ask becomes Plan there. Verified live: agy's own
/// plan mode is not read-only either (it plans, then applies edits); only shell commands are
/// refused without `--dangerously-skip-permissions`. The notice says so.
fn effective_mode(hookless_agy: bool, wanted: Mode) -> Mode {
    if hookless_agy && wanted == Mode::Ask {
        Mode::Plan
    } else {
        wanted
    }
}

/// Whether `mode` is no wider than a supervised child's fixed task mode `ceiling`: Plan, then
/// Ask, then Accept edits. Dangerous permissive mode is never within any child's.
fn mode_within(mode: Mode, ceiling: Mode) -> bool {
    fn rank(mode: Mode) -> u8 {
        match mode {
            Mode::Plan => 0,
            Mode::Ask => 1,
            Mode::AcceptEdits => 2,
            Mode::Bypass => 3,
        }
    }
    mode != Mode::Bypass && rank(mode) <= rank(ceiling)
}

/// Why agy has no approval socket when its hooks file has not been checked yet (a switch to agy
/// before the first check finished). Not "not installed": that is not known yet.
pub(crate) const HOOK_NOT_CHECKED: &str = "the approval hook has not been checked yet";

/// What hookless agy still does, said in every notice about it.
const HOOKLESS_STILL: &str = "It refuses shell commands, but applies file edits on its own in \
                              every mode (in Plan it writes a plan first, then edits). Ask before \
                              edits is not available.";

/// The notice for agy running without its approval socket: why (`unbound`, when the hook is
/// installed but the socket could not be used, or [`HOOK_NOT_CHECKED`]; else the hook is
/// missing), and what it still does.
fn hookless_text(unbound: Option<&str>) -> String {
    const STILL: &str = HOOKLESS_STILL;
    match unbound {
        Some(HOOK_NOT_CHECKED) => format!(
            "Antigravity cannot ask you before acting yet: its approval hook has not been checked \
             yet. {STILL} Once the check has run, switch to Antigravity again (or open a new \
             thread) to have it ask first."
        ),
        Some(reason) if crate::hook_config::verification_refused(reason) => {
            format!("Antigravity cannot ask you before acting in this thread: {reason}. {STILL}")
        }
        Some(reason) => format!(
            "Antigravity cannot ask you before acting in this thread: its approval hook is \
             installed, but {reason}. {STILL}"
        ),
        None => format!(
            "Antigravity cannot ask you before acting: the approval hook is not installed. \
             {STILL} To have it ask you first, add this top-level entry to \
             ~/.gemini/config/hooks.json:\n{}",
            crate::hook_config::install_entry_json()
        ),
    }
}

/// A transcript read for one agy `step_update` line: the parsed line and what to look up.
struct AgyEnrichJob {
    val: serde_json::Value,
    conv_id: String,
    step_index: u64,
}

/// One thread's chat backend. See the module docs.
pub struct ChatSession {
    inner: Rc<Inner>,
}

fn driver_name(driver: Driver) -> &'static str {
    driver.info().key
}

fn driver_label(driver: Driver) -> &'static str {
    driver.info().long_label
}

/// The store a session writes through, and the thread's active provider thread when its caller
/// has already read it (off the main thread, after the store writer's barrier).
pub struct SessionStore {
    pub handle: StoreHandle,
    /// `Some(None)`: the thread has none yet (one is created). `None`: not read; the session
    /// reads it itself, behind the barrier.
    pub provider: Option<Option<ProviderThreadId>>,
}

impl From<StoreHandle> for SessionStore {
    fn from(handle: StoreHandle) -> Self {
        Self {
            handle,
            provider: None,
        }
    }
}

impl From<Rc<Store>> for SessionStore {
    fn from(store: Rc<Store>) -> Self {
        StoreHandle::from(store).into()
    }
}

impl ChatSession {
    /// Starts the agent process and returns the live session.
    ///
    /// - `open` describes the process; its `approval_hook` field is overwritten from `approval`
    ///   (hook on only when there is a socket to export, one source for both).
    /// - The store's active provider thread for `thread` is used, or created from the adapter's
    ///   driver and `open.model` when there is none.
    /// - `approval` (agy): the socket exported to the process; envelopes for its requests flow
    ///   through this session.
    #[cfg_attr(not(test), allow(dead_code))] // exercised by tests; kept as API
    pub fn new(
        adapter: Box<dyn Adapter>,
        open: OpenSession,
        store: impl Into<SessionStore>,
        thread: ThreadId,
        sink: EnvelopeSink,
        approval: Option<ApprovalHandle>,
    ) -> Rc<Self> {
        Self::with_env(
            adapter,
            open,
            store,
            thread,
            sink,
            approval.ok_or(None),
            LaunchEnv::default(),
        )
    }

    /// [`Self::new`] with the profile's environment applied to every process it starts, and
    /// agy's approval as an [`Approval`] (so a socket that could not be bound says why).
    pub fn with_env(
        adapter: Box<dyn Adapter>,
        open: OpenSession,
        store: impl Into<SessionStore>,
        thread: ThreadId,
        sink: EnvelopeSink,
        approval: Approval,
        env: LaunchEnv,
    ) -> Rc<Self> {
        Self::build(
            adapter,
            open,
            store.into(),
            thread,
            sink,
            approval,
            env,
            SessionOptions::default(),
        )
    }

    /// [`Self::with_env`] with delegation options installed before the first process starts.
    #[allow(clippy::too_many_arguments)] // `with_env`'s seven, plus the options
    fn build(
        mut adapter: Box<dyn Adapter>,
        mut open: OpenSession,
        store: SessionStore,
        thread: ThreadId,
        sink: EnvelopeSink,
        approval: Approval,
        env: LaunchEnv,
        opts: SessionOptions,
    ) -> Rc<Self> {
        let SessionStore {
            handle: store,
            provider: known_provider,
        } = store;
        let driver = adapter.driver();
        let (approval, unbound) = match approval {
            Ok(handle) => (Some(handle), None),
            Err(reason) => (None, reason),
        };
        if let Some(child) = &opts.child {
            // A child requests a child's policies, starts in no mode wider than its task's, and
            // its approval socket judges every call by its gate first.
            adapter.supervise_as_child();
            if !mode_within(open.mode, child.mode) {
                open.mode = child.mode;
            }
            if let Some(handle) = &approval {
                handle.set_gate(child.gate.clone());
            }
        }
        open.approval_hook = approval.is_some();
        let hookless_agy = driver == Driver::Agy && approval.is_none();
        // No process starts in dangerous permissive mode, whatever asked for it (a stored
        // choice, a fork, a profile default): it is only ever entered by the user, in-session.
        let wanted_mode = open.mode.session_safe();
        open.mode = effective_mode(hookless_agy, wanted_mode);
        // Read by the caller off the main thread, after the writer's barrier, when it could;
        // else here (behind the same barrier).
        let active = match known_provider {
            Some(known) => Ok(known),
            None => store
                .read(|s| s.active_provider_thread(&thread))
                .and_then(|r| r),
        };
        let provider_thread = match active {
            Ok(Some(p)) => p,
            // Never a second provider thread for a thread whose own could not be read.
            Err(agent_kit::store::StoreError::Behind) => {
                warn!("the store is behind; this session's events are stored without its agent");
                String::new()
            }
            _ => match store.create_provider_thread(
                &thread,
                driver_name(driver),
                open.model.as_deref().unwrap_or("default"),
            ) {
                Ok(p) => p,
                Err(e) => {
                    warn!(error = %e, "could not create a provider thread");
                    String::new()
                }
            },
        };
        let selected = open.model.clone();
        let state = State {
            model: open.model.clone(),
            effort: open.effort.clone(),
            mode: open.mode,
            running_turn: false,
            alive: false,
            commands: Vec::new(),
            native_id: open.resume.clone(),
        };
        let inner = Rc::new(Inner {
            adapter: RefCell::new(adapter),
            open: RefCell::new(open),
            store,
            thread,
            provider_thread: RefCell::new(provider_thread),
            sink,
            approval: RefCell::new(approval),
            unbound: RefCell::new(unbound),
            canary_grace: Cell::new(CANARY_GRACE),
            canary_cancelled: RefCell::new(HashSet::new()),
            hook_check: RefCell::new(default_hook_check()),
            starting: RefCell::new(None),
            factory: RefCell::new(None),
            agy_can_ask: RefCell::new(None),
            launch_env: RefCell::new(env),
            proc: RefCell::new(None),
            state: RefCell::new(state),
            pending_handoff: RefCell::new(None),
            handoff_in_flight: Cell::new(false),
            handoff_turn: Cell::new(HandoffTurn::Waiting),
            turn_from_task: Cell::new(false),
            rollback: RefCell::new(None),
            retired: RefCell::new(None),
            selected: RefCell::new(selected),
            pending_model: RefCell::new(None),
            deferred_mode: Cell::new(None),
            ctl_seq: Cell::new(0),
            generation: Cell::new(0),
            stopping: Cell::new(false),
            dispatching_queue: Cell::new(false),
            delivering: Cell::new(false),
            queued_dispatch_pending: Cell::new(false),
            adapter_epoch: Cell::new(0),
            wanted_mode: Cell::new(wanted_mode),
            safe_mode: Cell::new(wanted_mode),
            pending_rules: RefCell::new(HashMap::new()),
            session_rules: crate::approval_server::SessionRules::default(),
            saved_rules: RefCell::new(
                crate::always_allow::path().map(crate::always_allow::StoreCache::new),
            ),
            rules_blocked_noticed: Cell::new(false),
            queued_prompts: RefCell::new(std::collections::VecDeque::new()),
            held_retries: RefCell::new(std::collections::VecDeque::new()),
            launch_facts: RefCell::new(None),
            weak_self: RefCell::new(None),
            prompt_in_flight: Cell::new(false),
            last_enriched_step: Cell::new(None),
            provisional_native: RefCell::new(None),
            agy_reading: Cell::new(None),
            agy_deferred_exit: Cell::new(None),
            agy_hold: RefCell::new(std::collections::VecDeque::new()),
            approval_deadline: Cell::new(crate::approval_server::DEFAULT_DEADLINE),
            thinking: ThinkingFlag::default(),
            pre_init_prompts: RefCell::new(Vec::new()),
            codex_permissions: RefCell::new(HashMap::new()),
            codex_grants: RefCell::new(Vec::new()),
            held_permissions: RefCell::new(HashSet::new()),
            codex_shown: RefCell::new(HashMap::new()),
            codex_grant_paths: RefCell::new(Vec::new()),
            codex_turn_grant_paths: RefCell::new(Vec::new()),
            reply_turn: RefCell::default(),
            hooks: opts.hooks,
            child: opts.child,
            process_generation: Cell::new(opts.generation_base.0),
            stopped_generation: Cell::new(opts.generation_base.0),
            watches: RefCell::new(Vec::new()),
            rebinding: Cell::new(false),
            native_turn: RefCell::new(None),
            open_approvals: RefCell::new(Vec::new()),
            active_workers: Cell::new(0),
            approval_inputs: RefCell::new(HashMap::new()),
            ask_only: RefCell::new(HashSet::new()),
            worker: RefCell::new(None),
            bound_generation: Cell::new(false),
            unspawned_binding: Cell::new(None),
            carrying_task: Cell::new(false),
            carried_unsent: RefCell::new(Vec::new()),
            quiet_exit: Cell::new(false),
            suspended: Cell::new(false),
            reported: Cell::new(false),
            resuming: Cell::new(false),
            lifecycle_told: Cell::new(crate::chat::Lifecycle::default()),
            lifecycle_listener: RefCell::new(None),
            has_conversation: Cell::new(false),
            prompted: Cell::new(false),
            auto_wakes: Cell::new(0),
            wakes_held: Cell::new(false),
            held_news: RefCell::new(None),
        });
        *inner.weak_self.borrow_mut() = Some(Rc::downgrade(&inner));
        inner.attach_approval();
        // Said when the user's Ask could not be honoured; a thread already in Plan or Accept
        // edits does not repeat it on every reopen. An installed hook that could not be used is
        // always said (it is fixed by a restart, not by the user's mode).
        let unbound_agy = hookless_agy && inner.unbound.borrow().is_some();
        if inner.state.borrow().mode != wanted_mode || unbound_agy {
            inner.hookless_notice();
        }
        inner.start_process();
        Rc::new(Self { inner })
    }

    /// Lets `switch` build an adapter for the other driver (needed for `CreateWithHandoff`).
    pub fn set_adapter_factory(&self, factory: AdapterFactory) {
        *self.inner.factory.borrow_mut() = Some(factory);
    }

    /// Lets the picker say, before a switch to agy, whether it could ask first
    /// ([`ChatBackend::switch_caveats`]). Without it, agy is assumed unable to.
    pub fn set_agy_can_ask(&self, can_ask: Rc<dyn Fn() -> bool>) {
        *self.inner.agy_can_ask.borrow_mut() = Some(can_ask);
    }

    /// The current launch environment, including any cross-agent switch since the thread opened.
    pub(crate) fn launch_environment(&self) -> LaunchEnv {
        self.inner.launch_env.borrow().clone()
    }

    /// The mode the user chose, which the running one can differ from (agy without its hook
    /// cannot ask), as a new process would get it (a fork, a handoff): never dangerous
    /// permissive mode, which ends with its agent; the last safe choice instead.
    pub fn wanted_mode(&self) -> Mode {
        self.inner.safe_mode.get()
    }

    /// Re-arms a handoff found stored and undelivered in the thread's history (see
    /// [`undelivered_handoff`]), as the thread opens: a new thread's own handoff too (stored with
    /// it by [`seeded_handoff_events`]), which rides on the next real prompt exactly like a
    /// cross-agent switch's. Already stored and announced, so neither is done again. A stored fence that is not one this app draws is not trusted: dropped.
    pub fn restore_handoff(&self, handoff: Handoff) {
        if !agent_core::handoff_budget::is_valid_fence(&handoff.fence) {
            warn!("ignoring a stored handoff with an invalid fence");
            return;
        }
        self.inner.handoff_in_flight.set(false);
        *self.inner.pending_handoff.borrow_mut() = Some(handoff);
    }

    /// Marks the thread's model as retired (or clears it with `None`): the next prompt moves the
    /// thread to the replacement first, through the ordinary same-agent switch, and says so. A
    /// switch the user makes meanwhile replaces the whole question.
    pub fn set_retired(&self, retired: Option<RetiredModel>) {
        *self.inner.retired.borrow_mut() = retired;
    }

    /// The model the thread was last set to (an alias where the agent has one); `None` is the
    /// agent's own default.
    pub fn selected_model(&self) -> Option<String> {
        self.inner.selected.borrow().clone()
    }

    /// The reasoning effort the session runs at; `None` is the agent's own default.
    pub fn selected_effort(&self) -> Option<String> {
        self.inner.state.borrow().effort.clone()
    }

    /// Adds a notice to the thread (stored and shown like any event).
    pub fn note(&self, text: impl Into<String>) {
        self.inner
            .emit(Envelope::new(Event::Notice { text: text.into() }));
    }

    /// Stops the agent on purpose and closes the thread's history out: open items end
    /// Interrupted, pending approvals are cancelled, and a turn in flight is completed as
    /// Interrupted, followed by the session's exit. Every envelope is persisted before it reaches
    /// the sink (`Inner::emit`), so it lands in the store even when the view and window that
    /// would show it are already gone. Call when the thread's page closes or the app quits;
    /// dropping the session alone only kills the process and leaves the history open-ended.
    /// A session with no process (already exited, or stopped before) is left alone.
    ///
    /// The closing events land after the thread's read mark, which would show every closed
    /// thread as unread. `keep_read`: the thread was read before, so it is marked read again,
    /// synchronously (a write queued for later may not finish before the process exits). A
    /// thread that really was unread stays unread.
    pub fn shutdown(&self, keep_read: bool) {
        if !self.inner.state.borrow().alive
            && self.inner.proc.borrow().is_none()
            && self.inner.starting.borrow().is_none()
        {
            return;
        }
        self.inner.stop_current();
        if keep_read {
            if let Err(e) = self.inner.store.mark_read(&self.inner.thread) {
                warn!(error = %e, "could not keep the closed thread read");
            }
        }
        // Queued after the closing events: the store's heal tails of this thread can go.
        self.inner.store.forget_thread(&self.inner.thread);
    }

    /// Stops the agent process if the thread is idle in every way that matters, keeping the
    /// conversation: the next prompt starts it again, resuming the same native session. Nothing
    /// is said to the view or stored. Whether it stopped.
    pub fn suspend_if_idle(&self) -> bool {
        self.inner.suspend()
    }

    /// Where the agent process is in its life, for the header and the sidebar (read-only).
    pub fn lifecycle(&self) -> crate::chat::Lifecycle {
        self.inner.lifecycle()
    }

    /// Calls `listener` whenever [`Self::lifecycle`] changes, whether or not an envelope says
    /// so (Codex's handshake answer carries none; a suspension is silent). It is called from
    /// inside the session, so it must defer anything that calls back into it.
    pub fn set_lifecycle_listener(&self, listener: crate::chat::LifecycleListener) {
        self.inner.lifecycle_told.set(self.inner.lifecycle());
        *self.inner.lifecycle_listener.borrow_mut() = Some(listener);
    }

    /// A watch on the running agent process, if one is alive: taken before [`Self::shutdown`],
    /// it follows the process out (closing the window waits on it).
    pub fn exit_watch(&self) -> Option<crate::agent_proc::ExitWatch> {
        self.inner
            .proc
            .borrow()
            .as_ref()
            .filter(|p| p.is_alive())
            .map(AgentProcess::exit_watch)
    }

    /// Safe facts of the last successfully spawned generation; never rebuilds a launch or
    /// consumes its staged binding. No other environment values are retained here.
    pub(crate) fn delegation_launch_facts(&self) -> Option<DelegationLaunchFacts> {
        self.inner.launch_facts.borrow().clone()
    }

    #[cfg(test)]
    fn set_canary_grace(&self, grace: Duration) {
        self.inner.canary_grace.set(grace);
    }

    /// The command line, working directory and environment the next start would use.
    #[cfg(test)]
    fn launch_spec(&self) -> SpawnSpec {
        self.inner.launch_spec()
    }
}

#[cfg(test)]
fn create_provider_thread(
    store: &Store,
    thread: &str,
    driver: Driver,
    model: Option<&str>,
) -> Result<ProviderThreadId, agent_kit::store::StoreError> {
    let id = store.add_provider_thread(thread, driver_name(driver), model.unwrap_or("default"))?;
    store.set_active_provider_thread(thread, &id)?;
    Ok(id)
}

impl Inner {
    /// Routes the current approval handle's envelopes through this session.
    fn attach_approval(self: &Rc<Self>) {
        if let Some(handle) = self.approval() {
            handle.share_session_rules(self.session_rules.clone());
            let weak = Rc::downgrade(self);
            handle.attach(move |env| {
                if let Some(inner) = weak.upgrade() {
                    inner.emit(env);
                }
            });
            handle.set_mode(self.state.borrow().mode);
        }
    }

    fn hookless_notice(&self) {
        let text = hookless_text(self.unbound.borrow().as_deref());
        self.emit(Envelope::new(Event::Notice { text }));
    }

    // ---- events ----

    /// Persists (under the current provider thread), updates the status, then delivers.
    fn emit(&self, env: Envelope) {
        let delivery = ScopedFlag::set(&self.delivering);
        // A prompted turn ran on this native session: there is a conversation to resume.
        if self.prompted.get() && matches!(env.event, Event::TurnCompleted { .. }) {
            self.has_conversation.set(true);
        }
        let env = self.route_codex_permission(env);
        // A supervised child's events cross its publication boundary first: what it
        // withholds goes nowhere, and what it passes is the sanitised envelope.
        let env = match &self.child {
            Some(child) => {
                let generation = ProcessGeneration(self.process_generation.get());
                match child.publish.publish(generation, env) {
                    Some(env) => env,
                    None => return,
                }
            }
            None => env,
        };
        let pt = self.provider_thread.borrow().clone();
        let provider = (!pt.is_empty()).then_some(pt.as_str());
        // A control reply answers a live request and replay drops it; storing it would also
        // write a polled background task's output to the store once a second.
        let transient = matches!(env.event, Event::ControlResult { .. });
        if !transient {
            match &self.child {
                Some(child) => child.publish.persist(&self.thread, provider, &env),
                None => {
                    if let Err(e) = self.store.append_event(&self.thread, provider, &env) {
                        warn!(error = %e, "could not persist an event");
                    }
                }
            }
        }
        self.apply(&env);
        self.reply_turn.borrow_mut().observe(&env);
        // Every event is masked on the way to the view (not only approvals: tool input and
        // output, errors, notices and questions carry secrets too); the rules judged the real
        // call. A secret split across two streaming deltas is only caught at persistence.
        let turn_ended = matches!(env.event, Event::TurnCompleted { .. }).then(|| env.clone());
        (self.sink)(&crate::approval_server::masked_for_view(env));
        if let Some(ended) = turn_ended {
            self.offer_reply_question(&ended);
        }
        drop(delivery);
        // An exit, a failed spawn: whatever the envelope changed of the lifecycle.
        self.notify_lifecycle();
        self.flush_queued_dispatch();
    }

    /// Delivers without persisting, including transient queued-item state.
    fn deliver(&self, env: &Envelope) {
        let delivery = ScopedFlag::set(&self.delivering);
        self.apply(env);
        (self.sink)(&crate::approval_server::masked_for_view(env.clone()));
        drop(delivery);
        // An exit, a failed spawn: whatever the envelope changed of the lifecycle.
        self.notify_lifecycle();
        self.flush_queued_dispatch();
    }

    fn error(&self, message: impl Into<String>) {
        self.emit(Envelope::new(Event::Error {
            message: message.into(),
        }));
    }

    fn report(&self, e: &AdapterError) {
        self.error(e.to_string());
    }

    fn apply(&self, env: &Envelope) {
        match &env.event {
            Event::SessionStarted {
                native_id, model, ..
            } => {
                // The agent is up: a switch to it can no longer roll back.
                *self.rollback.borrow_mut() = None;
                self.confirm_carried();
                self.mark_up();
                if !native_id.is_empty() {
                    let pt = self.provider_thread.borrow().clone();
                    // A resume that started a new native session leaves the old one behind:
                    // it is this thread's history now, never a separate session to list.
                    let previous = self.state.borrow().native_id.clone();
                    let resumed = previous.as_deref() == Some(native_id.as_str());
                    if let Some(old) = previous.filter(|old| old != native_id) {
                        let driver = driver_name(self.adapter.borrow().driver());
                        if let Err(e) = self.store.dismiss_native(driver, &old) {
                            warn!(error = %e, "could not record the replaced native session");
                        }
                    }
                    // A new Codex thread stays provisional until a turn completes (2.5).
                    if self.adapter.borrow().driver() == Driver::Codex && !resumed {
                        *self.provisional_native.borrow_mut() = Some((pt, native_id.clone()));
                    } else if pt.is_empty() {
                        // No provider thread to record it on (the store could not be read).
                    } else if let Err(e) = self.store.set_native_id(&pt, native_id) {
                        warn!(error = %e, "could not record the native session id");
                    }
                    self.state.borrow_mut().native_id = Some(native_id.clone());
                }
                if let Some(m) = model {
                    self.set_model(m);
                    if self.pending_model.borrow().as_ref().is_some_and(|p| {
                        p.restarting
                            && (self.adapter.borrow().driver() != Driver::Codex || p.model == *m)
                    }) {
                        self.confirm_model_change(m);
                    }
                }
            }
            Event::TurnStarted { model } => {
                // An agent that runs turns has started, whether or not it reported a session:
                // an exit from here on is an ordinary one, not a failed switch.
                *self.rollback.borrow_mut() = None;
                self.confirm_carried();
                self.mark_up();
                if let Some(m) = model {
                    self.set_model(m);
                }
                self.state.borrow_mut().running_turn = true;
                // The first turn to start after the prompt carrying the handoff went out is the
                // one that may carry it ([`handoff_turn_end`] judges it as it ends).
                if self.handoff_in_flight.get() && self.handoff_turn.get() == HandoffTurn::Waiting {
                    self.handoff_turn.set(HandoffTurn::Open { answered: false });
                }
                self.last_enriched_step.set(None);
                let native = self.adapter.borrow().active_turn();
                *self.native_turn.borrow_mut() = native.clone();
                if let Some(hooks) = &self.hooks {
                    hooks.turn(
                        ProcessGeneration(self.process_generation.get()),
                        TurnBoundary::Started {
                            native_turn: native.as_deref(),
                        },
                    );
                }
            }
            Event::TurnCompleted { state, .. } => {
                self.prompt_in_flight.set(false);
                self.state.borrow_mut().running_turn = false;
                self.last_enriched_step.set(None);
                *self.native_turn.borrow_mut() = None;
                if let Some(hooks) = &self.hooks {
                    hooks.turn(
                        ProcessGeneration(self.process_generation.get()),
                        TurnBoundary::Ended { state: *state },
                    );
                }
                // Codex's turn-scoped permission grants ended with the turn.
                self.codex_turn_grant_paths.borrow_mut().clear();
                // Only the turn of the prompt that carried the handoff spends it, and only when
                // it completed with a top-level answer: see `handoff_turn_end`.
                let end = handoff_turn_end(
                    self.handoff_in_flight.get(),
                    self.handoff_turn.get(),
                    *state,
                    self.turn_from_task.replace(false),
                );
                match end {
                    HandoffTurnEnd::NotCarried => {}
                    HandoffTurnEnd::StillInFlight => self.handoff_turn.set(HandoffTurn::Waiting),
                    HandoffTurnEnd::Undelivered => self.handoff_in_flight.set(false),
                    HandoffTurnEnd::Delivered => {
                        self.handoff_in_flight.set(false);
                        // Stored as delivered (a handoff carrying nothing withdraws it), so a
                        // reopen never re-arms what this turn carried.
                        if self.pending_handoff.borrow_mut().take().is_some() {
                            self.emit(handoff_spent());
                        }
                    }
                }
                if *state == TurnState::Completed {
                    self.confirm_provisional_native();
                }
                if !self.stopping.get() {
                    self.queued_dispatch_pending.set(true);
                }
            }
            // The agent produced an item: Codex has recorded the turn, so the thread can be
            // resumed even if this turn never completes (2.5).
            Event::ItemStarted { kind, parent, .. } if *kind != ItemKind::UserMessage => {
                // A top-level answer in the turn that may carry the handoff. A background
                // sub-agent's item (it has a parent, and outlives the turn that launched it) says
                // nothing about our prompt.
                if parent.is_none() {
                    if let HandoffTurn::Open { .. } = self.handoff_turn.get() {
                        self.handoff_turn.set(HandoffTurn::Open { answered: true });
                    }
                }
                self.confirm_provisional_native();
            }
            Event::ModelChanged { model } => {
                self.set_model(model);
                let accepted = self.pending_model.borrow().as_ref().is_some_and(|p| {
                    match (&p.request, &env.request) {
                        (Some(expected), Some(actual)) => expected == actual,
                        (None, _) => p.model == *model || (p.restarting && env.request.is_none()),
                        _ => false,
                    }
                });
                if accepted {
                    self.confirm_model_change(model);
                } else {
                    // An earlier request may succeed while a later one is pending. Preserve
                    // that later request, but rollback/reopen must follow this actual success.
                    let requested = env
                        .request
                        .is_none()
                        .then(|| self.selected.borrow().clone())
                        .flatten();
                    let stored = accepted_model_choice(
                        self.adapter.borrow().driver(),
                        requested.as_deref(),
                        model,
                    );
                    self.persist_model(&stored);
                    self.open.borrow_mut().model = Some(stored.clone());
                    if let Some(pending) = self.pending_model.borrow_mut().as_mut() {
                        pending.previous_model = Some(stored);
                        pending.previous_effort = self.state.borrow().effort.clone();
                    }
                }
            }
            Event::ModelChangeRequested { model } => {
                if !self
                    .pending_model
                    .borrow()
                    .as_ref()
                    .is_some_and(|p| p.model == *model)
                {
                    self.prepare_model_change(
                        model.clone(),
                        self.state.borrow().effort.clone(),
                        false,
                    );
                }
                if let Some(pending) = self.pending_model.borrow_mut().as_mut() {
                    pending.request.clone_from(&env.request);
                }
            }
            Event::ModelChangeFailed { model, .. } => {
                if self.pending_model.borrow().as_ref().is_some_and(|p| {
                    match (&p.request, &env.request) {
                        (Some(expected), Some(actual)) => expected == actual,
                        (None, _) => p.model == *model,
                        _ => false,
                    }
                }) {
                    self.reject_model_change();
                }
            }
            Event::ModeChanged { mode } => {
                self.state.borrow_mut().mode = *mode;
                self.open.borrow_mut().mode = *mode;
                if let Some(a) = self.approval() {
                    a.set_mode(*mode);
                }
                if let Some(hooks) = &self.hooks {
                    hooks.mode_changed(*mode);
                }
                self.wake_mode_changed(*mode);
            }
            Event::CommandsChanged { commands } => {
                self.state.borrow_mut().commands = commands.clone();
            }
            Event::ApprovalRequested { tool, input, .. } => {
                if let Some(req) = &env.request {
                    self.approval_inputs
                        .borrow_mut()
                        .insert(req.clone(), (tool.clone(), input.clone()));
                    let mut open = self.open_approvals.borrow_mut();
                    if !open.contains(req) {
                        open.push(req.clone());
                    }
                }
            }
            Event::WorkersUpdated { workers, .. } => {
                self.active_workers
                    .set(workers.iter().filter(|w| w.state.is_active()).count());
            }
            Event::ApprovalResolved { .. } | Event::ApprovalExpired => {
                if let Some(req) = &env.request {
                    self.pending_rules.borrow_mut().remove(req);
                    self.codex_permissions.borrow_mut().remove(req);
                    self.held_permissions.borrow_mut().remove(req);
                    self.open_approvals.borrow_mut().retain(|r| r != req);
                    self.approval_inputs.borrow_mut().remove(req);
                    self.ask_only.borrow_mut().remove(req);
                }
            }
            _ => {}
        }
    }

    fn set_model(&self, model: &str) {
        self.state.borrow_mut().model = Some(model.to_owned());
        self.open.borrow_mut().model = Some(model.to_owned());
    }

    /// Stores a provisional Codex thread id once a turn on it produced an item or completed, if
    /// its provider thread is still this thread's agent.
    fn confirm_provisional_native(&self) {
        let Some((pt, native)) = self.provisional_native.borrow_mut().take() else {
            return;
        };
        if *self.provider_thread.borrow() != pt {
            return;
        }
        if pt.is_empty() {
            return;
        }
        if let Err(e) = self.store.set_native_id(&pt, &native) {
            warn!(error = %e, "could not record the native session id");
        }
    }

    /// Whether `env` starts a Codex thread other than the one this session asked to resume: the
    /// adapter fell back to a new thread because the old one had no rollout (plan 2.5).
    fn is_lost_codex_resume(&self, env: &Envelope) -> bool {
        let Event::SessionStarted { native_id, .. } = &env.event else {
            return false;
        };
        self.adapter.borrow().driver() == Driver::Codex
            && !native_id.is_empty()
            && self
                .state
                .borrow()
                .native_id
                .as_ref()
                .is_some_and(|old| old != native_id)
    }

    /// The new Codex thread knows nothing of this one: its history rides on the next prompt,
    /// like a switch's handoff, unless a handoff is already waiting.
    fn hand_off_to_fresh_codex_thread(&self) {
        if self.pending_handoff.borrow().is_some() {
            return;
        }
        let messages = match self
            .store
            .read(|s| s.transcript_messages(&self.thread))
            .and_then(|r| r)
        {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "could not read the thread to carry it into a new Codex thread");
                return;
            }
        };
        let handoff = build_handoff(&messages, &format!("Codex thread {}", self.thread));
        if handoff.carried > 0 {
            self.arm_handoff(handoff);
        }
    }

    fn prepare_model_change(&self, model: String, effort: Option<String>, restarting: bool) {
        let state = self.state.borrow();
        let previous_model = self
            .selected
            .borrow()
            .clone()
            .or_else(|| state.model.clone());
        *self.pending_model.borrow_mut() = Some(PendingModel {
            model,
            request: None,
            effort,
            previous_model,
            previous_effort: state.effort.clone(),
            restarting,
        });
    }

    fn confirm_model_change(&self, reported: &str) {
        let Some(pending) = self.pending_model.borrow_mut().take() else {
            return;
        };
        self.state.borrow_mut().effort = pending.effort.clone();
        self.open.borrow_mut().effort = pending.effort;
        // Known Claude aliases legitimately resolve to versioned ids. A different model
        // reported by a resumed process or reroute must instead be what reopens.
        let stored = accepted_model_choice(
            self.adapter.borrow().driver(),
            Some(&pending.model),
            reported,
        );
        self.persist_model(&stored);
        self.open.borrow_mut().model = Some(stored);
    }

    fn reject_model_change(&self) {
        let Some(pending) = self.pending_model.borrow_mut().take() else {
            return;
        };
        self.open.borrow_mut().model = pending.previous_model;
        self.open.borrow_mut().effort = pending.previous_effort.clone();
        self.state.borrow_mut().effort = pending.previous_effort;
    }

    // ---- process ----

    fn launch_spec(&self) -> SpawnSpec {
        // One source for the hook flag and the socket env var: both come from `approval`.
        let approval = self.approval();
        self.open.borrow_mut().approval_hook = approval.is_some();
        // The delegation binding staged for exactly the next process generation; a start the
        // supervisor did not stage takes none, so it gets no token (fail closed).
        let driver = self.adapter.borrow().driver();
        let next = ProcessGeneration(self.process_generation.get() + 1);
        // A binding an earlier launch took and never spawned is released before another is
        // taken.
        self.abandon_unspawned_binding();
        let binding = self.hooks.as_ref().and_then(|h| h.binding(driver, next));
        self.bound_generation.set(binding.is_some());
        if binding.is_some() {
            self.unspawned_binding.set(Some(next.0));
        }
        self.open.borrow_mut().delegation = binding.as_ref().and_then(|b| b.open.clone());
        let open = self.open.borrow();
        let mut argv = self.adapter.borrow().argv(&open);
        // Claude shares its thinking only with this hidden flag, so it goes only to a
        // version known to take it (`claude_probe`), and never after the one retry without it.
        let thinking = self.adapter.borrow().driver() == Driver::Claude
            && !self.thinking.retried.get()
            && crate::claude_probe::thinking_display_allowed(&open.program);
        if thinking {
            argv.extend(agent_core::claude::THINKING_DISPLAY_ARGS.map(str::to_owned));
        }
        self.thinking.passed.set(thinking);
        self.thinking.refused.set(false);
        self.thinking.init_seen.set(false);
        if let Some(binding) = &binding {
            if driver == Driver::Claude {
                argv.push("--replay-user-messages".to_owned());
            }
            argv.extend(binding.argv_tail.iter().cloned());
        }
        let launch = self.launch_env.borrow();
        // The profile's environment first, so it can never override the approval socket or the
        // delegation binding.
        let mut env = launch.env.clone();
        env.extend(approval.as_ref().map(|a| a.env()).unwrap_or_default());
        if let Some(binding) = binding {
            env.extend(binding.env);
        }
        let mut unset = launch.unset.clone();
        // No inherited delegation credential or approval socket ever reaches an agent; its own
        // values (above) are set after these are removed.
        for name in agent_core::delegation::env::STRIP_ON_SPAWN {
            if !unset.iter().any(|u| u == name) {
                unset.push(name.to_owned());
            }
        }
        if approval.is_none() {
            if !unset.iter().any(|u| u == agent_core::approval::ENV_SOCKET) {
                unset.push(agent_core::approval::ENV_SOCKET.to_owned());
            }
            if !unset
                .iter()
                .any(|u| u == crate::approval_server::ENV_HOOK_BIN)
            {
                unset.push(crate::approval_server::ENV_HOOK_BIN.to_owned());
            }
        }
        SpawnSpec {
            argv,
            cwd: (!open.cwd.is_empty()).then(|| open.cwd.clone()),
            env,
            unset,
        }
    }

    /// Starts the next process generation. An agy process that would get
    /// `--dangerously-skip-permissions` first waits for a fresh proof of its hook sources, read
    /// off the main thread just before the spawn: a proof cached earlier (which built the socket
    /// and drives the UI) is never what passes the flag, since a hook file may have appeared
    /// since. Meanwhile the session is starting (alive, no process) and writes to it are held.
    /// A failed proof starts it hookless instead.
    fn start_process(self: &Rc<Self>) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.resuming.set(self.suspended.replace(false));
        self.reported.set(false);
        // A fresh native session (no resume: a new thread, a switch) has nothing to resume yet,
        // whatever the previous one had.
        self.has_conversation
            .set(self.open.borrow().resume.is_some());
        self.prompted.set(false);
        self.clear_agy_read();
        self.canary_cancelled.borrow_mut().clear();
        *self.starting.borrow_mut() = None;
        if self.child.is_some()
            && self.adapter.borrow().driver() == Driver::Agy
            && self.approval().is_none()
        {
            self.error("A delegated Antigravity agent requires a proven approval hook; it was not started.");
            self.finish_process(Some(1), false);
            return;
        }
        if self.adapter.borrow().driver() != Driver::Agy || self.approval().is_none() {
            self.spawn_generation(generation);
            self.notify_lifecycle();
            return;
        }
        let target = {
            let open = self.open.borrow();
            crate::hook_config::HookTarget::new(
                &self.launch_env.borrow(),
                Some(PathBuf::from(&open.cwd)).filter(|cwd| !cwd.as_os_str().is_empty()),
            )
        };
        let check = self.hook_check.borrow().clone();
        *self.starting.borrow_mut() = Some(PendingStart {
            generation,
            writes: Vec::new(),
            prompts: Vec::new(),
        });
        self.state.borrow_mut().alive = true;
        self.notify_lifecycle();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let verdict = gtk4::gio::spawn_blocking(move || check(&target))
                .await
                .unwrap_or_else(|_| Err("the hook check panicked".to_owned()));
            if let Some(inner) = weak.upgrade() {
                inner.finish_proven_start(generation, verdict);
            }
        });
    }

    /// The fresh proof for start `generation` is in. A start that was stopped or replaced
    /// meanwhile is dropped; otherwise the process spawns, with the skip flag only on `Ok`.
    fn finish_proven_start(self: &Rc<Self>, generation: u64, verdict: Result<(), String>) {
        if self.generation.get() != generation {
            return;
        }
        let Some(pending) = self
            .starting
            .borrow_mut()
            .take_if(|p| p.generation == generation)
        else {
            return;
        };
        // A sink can re-enter while the refusal is said (stop the session, or replace it with a
        // new start whose own proof is still out). So the hookless launch is fixed before
        // anything is emitted, and spawned only if this start is still the session's afterwards.
        let spec = match verdict {
            Ok(()) => self.launch_spec(),
            Err(reason) if self.child.is_some() => {
                self.error(format!("The delegated Antigravity agent's approval hook could not be proven: {reason}. It was not started."));
                self.finish_process(Some(1), false);
                return;
            }
            Err(reason) => {
                let refused = self.refuse_unproven_hook(reason);
                let spec = self.launch_spec();
                self.announce_refused_hook(generation, refused);
                spec
            }
        };
        if self.generation.get() != generation || self.stopping.get() {
            return;
        }
        self.spawn_spec_with_writes(generation, spec, pending.writes, pending.prompts);
    }

    /// The hook could not be proven right before a start: agy runs without the skip flag and
    /// without its socket from now on, exactly as after a canary trip. Only changes state, and
    /// never reaches the sink; [`Self::announce_refused_hook`] says it.
    fn refuse_unproven_hook(&self, reason: String) -> RefusedHook {
        warn!(%reason, "agy's approval hook could not be proven for this start; it runs hookless");
        // Taken, not dropped here: closing the socket may expire cards through the sink.
        let socket = self.approval.borrow_mut().take();
        if self.wanted_mode.get() == Mode::Bypass {
            self.wanted_mode.set(self.safe_mode.get());
        }
        let mode = effective_mode(true, self.open.borrow().mode.session_safe());
        let changed = self.state.borrow().mode != mode;
        self.open.borrow_mut().mode = mode;
        self.state.borrow_mut().mode = mode;
        let text = format!(
            "Antigravity's approval hook was checked again just before it started, and could not \
             be confirmed: {reason}. It runs without asking you first. {HOOKLESS_STILL}"
        );
        *self.unbound.borrow_mut() = Some(reason);
        RefusedHook {
            socket,
            mode_changed: changed.then_some(mode),
            text,
        }
    }

    /// Says what [`Self::refuse_unproven_hook`] did, each event only while start `generation` is
    /// still the session's (a sink handling one may stop or replace it).
    fn announce_refused_hook(&self, generation: u64, refused: RefusedHook) {
        let RefusedHook {
            socket,
            mode_changed,
            text,
        } = refused;
        drop(socket); // closes it
        if let Some(mode) = mode_changed {
            if self.generation.get() != generation {
                return;
            }
            self.emit(Envelope::new(Event::ModeChanged { mode }));
        }
        if self.generation.get() == generation {
            self.emit(Envelope::new(Event::Notice { text }));
        }
    }

    fn spawn_generation(self: &Rc<Self>, generation: u64) {
        let spec = self.launch_spec();
        self.spawn_spec(generation, spec);
    }

    fn spawn_spec(self: &Rc<Self>, generation: u64, spec: SpawnSpec) {
        self.spawn_spec_with_writes(generation, spec, Vec::new(), Vec::new());
    }

    fn spawn_spec_with_writes(
        self: &Rc<Self>,
        generation: u64,
        mut spec: SpawnSpec,
        writes: Vec<String>,
        prompts: Vec<QueuedPrompt>,
    ) {
        if self.adapter.borrow().driver() != Driver::Agy || self.hooks.is_none() {
            self.spawn_spec_now(generation, spec, None);
            self.flush_or_keep(generation, writes, prompts);
            return;
        }
        let Some(hooks) = self.hooks.clone() else {
            return;
        };
        use agent_core::delegation::agy_guard::CONFIG_REDIRECT_VARS;
        let home = spec
            .env
            .iter()
            .rev()
            .find(|(name, _)| name == "HOME")
            .map(|(_, value)| PathBuf::from(value))
            .or_else(|| {
                (!spec.unset.iter().any(|name| name == "HOME"))
                    .then(|| std::env::var_os("HOME").map(PathBuf::from))
                    .flatten()
            });
        let redirects = delegation_redirect_names(&spec);
        // Freeze inherited safety inputs across the asynchronous guard. Redirects are never
        // allowed by a delegated launch, so absence is fixed explicitly in the final spec.
        match &home {
            Some(home) => spec
                .env
                .push(("HOME".into(), home.to_string_lossy().into_owned())),
            None => spec.unset.push("HOME".into()),
        }
        for name in CONFIG_REDIRECT_VARS {
            if !redirects.iter().any(|n| n == name) {
                spec.unset.push(name.into());
            }
        }
        let mut facts = DelegationLaunchFacts {
            generation: ProcessGeneration(self.process_generation.get() + 1),
            argv: spec.argv.clone(),
            physical_cwd: PathBuf::new(),
            effective_home: home,
            config_redirect_vars: redirects,
            hook_proven: self.approval().is_some()
                && spec
                    .argv
                    .iter()
                    .any(|arg| arg == "--dangerously-skip-permissions"),
        };
        *self.starting.borrow_mut() = Some(PendingStart {
            generation,
            writes,
            prompts,
        });
        self.state.borrow_mut().alive = true;
        let cwd = spec.cwd.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let physical = gtk4::gio::spawn_blocking(move || {
                let cwd = cwd
                    .map(PathBuf::from)
                    .map(Ok)
                    .unwrap_or_else(std::env::current_dir)?;
                std::fs::canonicalize(cwd)
            })
            .await;
            let Some(inner) = weak.upgrade() else { return };
            if inner.generation.get() != generation
                || inner
                    .starting
                    .borrow()
                    .as_ref()
                    .is_none_or(|p| p.generation != generation)
            {
                return;
            }
            let verdict = match physical {
                Ok(Ok(cwd)) => {
                    facts.physical_cwd = cwd.clone();
                    match (
                        cwd.to_str(),
                        facts
                            .effective_home
                            .as_ref()
                            .is_none_or(|h| h.to_str().is_some()),
                    ) {
                        (Some(cwd), true) => {
                            spec.cwd = Some(cwd.to_owned());
                            hooks.before_spawn(facts.clone()).await
                        }
                        _ => Err("the checked launch paths are not valid UTF-8".into()),
                    }
                }
                _ => Err("the agent's physical working directory could not be checked".into()),
            };
            if inner.generation.get() != generation || inner.stopping.get() {
                return;
            }
            let Some(pending) = inner
                .starting
                .borrow_mut()
                .take_if(|p| p.generation == generation)
            else {
                return;
            };
            if let Err(reason) = verdict {
                inner.error(format!(
                    "The agent's final delegation launch was refused: {reason}"
                ));
                if inner.generation.get() == generation {
                    inner.finish_process(Some(1), false);
                    inner.keep_unsent_prompts(pending.prompts);
                }
                return;
            }
            if delegation_redirect_names(&spec) != facts.config_redirect_vars {
                inner.error("The agent's configuration redirect environment changed during its launch check; it was not started.");
                if inner.generation.get() == generation {
                    inner.finish_process(Some(1), false);
                    inner.keep_unsent_prompts(pending.prompts);
                }
                return;
            }
            inner.spawn_spec_now(generation, spec, Some(facts));
            inner.flush_or_keep(generation, pending.writes, pending.prompts);
        });
    }

    /// After a held start's spawn: its writes go to the process, or, when it did not spawn (a
    /// failed spawn may already have rolled back to the previous agent), the messages behind
    /// them are queued again.
    fn flush_or_keep(&self, generation: u64, writes: Vec<String>, prompts: Vec<QueuedPrompt>) {
        let spawned = self.generation.get() == generation
            && self.proc.borrow().as_ref().is_some_and(|p| p.is_alive());
        if spawned {
            self.flush_start_writes(generation, writes);
        } else {
            self.keep_unsent_prompts(prompts);
        }
    }

    /// User messages that never reached an agent (its start was refused, did not spawn, or was
    /// torn down by anything but the user's own stop): queued again, in order and ahead of
    /// anything newer, as the user wrote them. Sent the ordinary way when the agent next starts,
    /// so whichever agent and handoff are current then apply. The notice is said, not stored:
    /// the queue lives in memory only, and a stored promise could outlast it.
    fn keep_unsent_prompts(&self, prompts: Vec<QueuedPrompt>) {
        if prompts.is_empty() {
            return;
        }
        let kept = prompts.len();
        // Their turn never opened: nothing is running, and the next message must queue behind.
        self.state.borrow_mut().running_turn = false;
        for queued in prompts.into_iter().rev() {
            self.hold_queued_prompt(queued);
        }
        self.deliver(&Envelope::new(Event::Notice {
            text: if kept == 1 {
                "Your message did not reach the agent. It is kept, and goes first when the agent \
                 next starts."
                    .to_owned()
            } else {
                format!(
                    "Your {kept} messages did not reach the agent. They are kept, and go first \
                     when the agent next starts."
                )
            },
        }));
    }

    /// A binding taken for a start that ended without a process (refused, failed to spawn, or
    /// stopped first) is released: no exit will ever be observed for it to end it otherwise.
    fn abandon_unspawned_binding(&self) {
        if let Some(generation) = self.unspawned_binding.take() {
            if let Some(hooks) = &self.hooks {
                hooks.launch_abandoned(ProcessGeneration(generation));
            }
        }
    }

    fn flush_start_writes(&self, generation: u64, writes: Vec<String>) {
        if self.generation.get() == generation {
            if let Some(proc) = self.proc.borrow().as_ref().filter(|p| p.is_alive()) {
                for line in &writes {
                    proc.write_line(line);
                }
            }
        }
    }

    fn spawn_spec_now(
        self: &Rc<Self>,
        generation: u64,
        spec: SpawnSpec,
        facts: Option<DelegationLaunchFacts>,
    ) {
        let (w_out, w_err, w_exit) = (
            Rc::downgrade(self),
            Rc::downgrade(self),
            Rc::downgrade(self),
        );
        let spawned = AgentProcess::spawn(
            &spec,
            move |line| {
                if let Some(s) = w_out.upgrade() {
                    s.on_line(generation, line, false);
                }
            },
            move |line| {
                if let Some(s) = w_err.upgrade() {
                    s.on_line(generation, line, true);
                }
            },
            move |code| {
                if let Some(s) = w_exit.upgrade() {
                    s.on_exit(generation, code);
                }
            },
        );
        match spawned {
            Ok(proc) => {
                let process_generation = self.process_generation.get() + 1;
                self.process_generation.set(process_generation);
                // Its binding reached a process, whose exit ends it.
                self.unspawned_binding.set(None);
                *self.launch_facts.borrow_mut() = facts;
                let (pid, watch) = (proc.pid(), proc.exit_watch());
                {
                    let mut watches = self.watches.borrow_mut();
                    watches.retain(|(_, w)| w.phase() != StopPhase::Exited);
                    watches.push((process_generation, watch.clone()));
                }
                let handshake = self.adapter.borrow_mut().handshake();
                for line in &handshake {
                    proc.write_line(line);
                }
                *self.proc.borrow_mut() = Some(proc);
                self.state.borrow_mut().alive = true;
                if let Some(hooks) = &self.hooks {
                    hooks.process_started(ProcessGeneration(process_generation), pid, watch);
                }
            }
            Err(e) => {
                {
                    // No process, so no turn: a prompt written into the start opened none.
                    let mut state = self.state.borrow_mut();
                    state.alive = false;
                    state.running_turn = false;
                }
                self.abandon_unspawned_binding();
                // Nothing will complete a turn: a pending handoff goes out with the next prompt.
                self.handoff_in_flight.set(false);
                // A switch's new agent that cannot even be spawned: back to the old one. Taken
                // first: roll_back starts a process, whose own failure lands back here.
                let previous = self.rollback.borrow_mut().take();
                if let Some(previous) = previous {
                    self.reject_model_change();
                    self.roll_back(previous, &e.to_string());
                    return;
                }
                let pending = self
                    .pending_model
                    .borrow()
                    .as_ref()
                    .map(|p| p.model.clone());
                if let Some(model) = pending {
                    self.emit(Envelope::new(Event::ModelChangeFailed {
                        model,
                        message: e,
                    }));
                } else {
                    self.error(e);
                }
            }
        }
    }

    fn approval(&self) -> Option<ApprovalHandle> {
        self.approval.borrow().clone()
    }

    /// Whether an Antigravity `step_update` line needs its thinking (and tool-call args) read
    /// from the native transcript. `agy` streams token usage over stdout but omits the raw
    /// thinking block, persisting it to disk. Pure: the read itself is the caller's job, on a
    /// worker thread.
    fn agy_enrich_job(&self, line: &str) -> Option<AgyEnrichJob> {
        if self.adapter.borrow().driver() != Driver::Agy || !line.contains("\"step_update\"") {
            return None;
        }
        let val: serde_json::Value = serde_json::from_str(line).ok()?;
        if val.get("event").and_then(serde_json::Value::as_str) != Some("step_update") {
            return None;
        }
        let step = val.get("step_update")?.as_object()?;
        let has_thinking = step
            .get("thinking")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|t| !t.trim().is_empty());
        if has_thinking {
            return None;
        }
        let step_index = step.get("step_index")?.as_u64()?;
        if self.last_enriched_step.get() == Some(step_index) {
            return None;
        }
        let conv_id = step
            .get("conversation_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .or_else(|| self.state.borrow().native_id.clone())?;
        Some(AgyEnrichJob {
            val,
            conv_id,
            step_index,
        })
    }

    fn on_line(self: &Rc<Self>, generation: u64, line: &str, stderr: bool) {
        if generation != self.generation.get() {
            return;
        }
        // Every raw line, in arrival order, before agy's hold (read-back; memory only).
        if let Some(hooks) = &self.hooks {
            hooks.line(
                ProcessGeneration(self.process_generation.get()),
                line,
                stderr,
            );
            if generation != self.generation.get() {
                return; // the hook stopped this process
            }
        }
        self.on_line_in_order(generation, line, stderr);
    }

    /// [`Self::on_line`] after the hooks saw it: a line held behind an agy transcript read
    /// comes back here, so the hooks see each line once.
    fn on_line_in_order(self: &Rc<Self>, generation: u64, line: &str, stderr: bool) {
        if generation != self.generation.get() {
            return;
        }
        match self.agy_reading.get() {
            Some(reading) if reading == generation => {
                // A transcript read is in flight: keep the order the adapter must see.
                self.agy_hold
                    .borrow_mut()
                    .push_back((line.to_owned(), stderr));
                return;
            }
            Some(_) => {
                // Left over from a replaced process.
                self.agy_reading.set(None);
                self.agy_hold.borrow_mut().clear();
            }
            None => {}
        }
        if !stderr {
            if let Some(job) = self.agy_enrich_job(line) {
                if let Ok(home) = std::env::var("HOME") {
                    self.agy_reading.set(Some(generation));
                    let me = Rc::clone(self);
                    glib::spawn_future_local(async move {
                        let (conv_id, step_index) = (job.conv_id.clone(), job.step_index);
                        let record = gtk4::gio::spawn_blocking(move || {
                            agy_transcript::read_step(Path::new(&home), &conv_id, step_index)
                        })
                        .await
                        .unwrap_or_default();
                        if me.agy_reading.get() != Some(generation) {
                            return; // the process was replaced while the file was read
                        }
                        me.agy_reading.set(None);
                        let enriched = me.apply_agy_record(job, record);
                        me.dispatch_line(generation, &enriched, false);
                        loop {
                            if me.agy_reading.get().is_some() {
                                break; // a later line started its own read; it drains the rest
                            }
                            let next = me.agy_hold.borrow_mut().pop_front();
                            let Some((held, held_stderr)) = next else {
                                break;
                            };
                            me.on_line_in_order(generation, &held, held_stderr);
                        }
                        if me.agy_reading.get().is_none() {
                            if let Some((exit_gen, code)) = me.agy_deferred_exit.take() {
                                me.on_exit(exit_gen, code);
                            }
                        }
                    });
                    return;
                }
            }
        }
        self.dispatch_line(generation, line, stderr);
    }

    /// Merges a transcript record into the step: its thinking, and its tool calls with args
    /// decoded (`transcript_calls`), exposed for the card-title builder in agent-core.
    fn apply_agy_record(&self, job: AgyEnrichJob, record: agy_transcript::StepRecord) -> String {
        let AgyEnrichJob {
            mut val,
            step_index,
            ..
        } = job;
        let original = val.to_string();
        let Some(step) = val
            .get_mut("step_update")
            .and_then(serde_json::Value::as_object_mut)
        else {
            return original;
        };
        if record.is_empty() {
            return original;
        }
        self.last_enriched_step.set(Some(step_index));
        if let Some(thinking) = record.thinking {
            if let Some(info) = step
                .get_mut("tool_info")
                .and_then(serde_json::Value::as_object_mut)
            {
                info.insert(
                    "thinking".to_owned(),
                    serde_json::Value::String(thinking.clone()),
                );
            }
            step.insert("thinking".to_owned(), serde_json::Value::String(thinking));
        }
        if !record.calls.is_empty() {
            let calls = record
                .calls
                .into_iter()
                .map(|c| serde_json::json!({ "name": c.name, "args": c.args }))
                .collect();
            step.insert(
                "transcript_calls".to_owned(),
                serde_json::Value::Array(calls),
            );
        }
        val.to_string()
    }

    /// Feeds one line to the adapter and handles what comes out.
    fn dispatch_line(self: &Rc<Self>, generation: u64, line: &str, stderr: bool) {
        if generation != self.generation.get() {
            return;
        }
        if stderr
            && self.thinking.passed.get()
            && agent_core::claude::refuses_thinking_display(line)
        {
            // Not the user's error: the exit that follows is retried without the flag.
            warn!("claude refused --thinking-display; retrying without it");
            self.thinking.refused.set(true);
            return;
        }
        if !stderr && line.contains("\"init\"") {
            // What a Claude `init` says it is: the next launch's thinking-flag gate.
            if let Ok(frame) = serde_json::from_str::<serde_json::Value>(line) {
                if frame["type"] == "system" && frame["subtype"] == "init" {
                    self.thinking.init_seen.set(true);
                    self.pre_init_prompts.borrow_mut().clear();
                }
                if let Some(version) = agent_core::claude::session_version(&frame) {
                    crate::claude_probe::note_version(&self.open.borrow().program, version);
                }
            }
        }
        // A turn Claude ran itself for a finished background task is not one our prompt
        // started: its end never spends a handoff (`handoff_turn_end`). Read from the frame,
        // which says where the turn came from; the TurnCompleted it becomes does not.
        self.turn_from_task.set(
            !stderr
                && line.contains("task-notification")
                && serde_json::from_str::<serde_json::Value>(line).is_ok_and(|frame| {
                    frame["type"] == "result" && frame["origin"]["kind"] == "task-notification"
                }),
        );
        let envelopes = if stderr {
            self.adapter.borrow_mut().feed_stderr(line)
        } else {
            self.adapter.borrow_mut().feed(line)
        };
        // The adapter accepted the agent's answer to its handshake: it is up. Claude reports its
        // session only with the first prompt's turn, and Codex's reply carries no event, so
        // neither `SessionStarted` nor any envelope can be waited for.
        if !self.reported.get() && self.adapter.borrow().handshake_answered() {
            self.mark_up();
        }
        for env in envelopes {
            self.observe(&env);
            if generation != self.generation.get() {
                return; // a hook stopped the process
            }
            let Some(env) = self.gate_form(env) else {
                continue; // declined before it was shown; the notice is out
            };
            let Some(env) = self.hold_codex_permissions(generation, env) else {
                continue; // shown once its targets are checked on disk
            };
            let Some(env) = self.gate_child_approval(env) else {
                continue; // denied by the child's inherited authority; the card is resolved
            };
            let (env, auto) = self.gate_approval(env);
            let lost_resume = self.is_lost_codex_resume(&env);
            self.emit(env.clone());
            if lost_resume {
                self.hand_off_to_fresh_codex_thread();
            }
            self.watch_tool_step(&env);
            self.arm_approval_deadline(&env);
            if auto {
                self.auto_approve(&env);
            }
            if generation != self.generation.get() {
                return; // the process was replaced while handling this envelope
            }
        }
        self.drain_outbox();
    }

    // ---- hook canary ----

    /// agy runs with `--dangerously-skip-permissions`, so the hook is its only gate. If the hook
    /// entry is not really firing (edited away, a narrower matcher, a different agy build), every
    /// tool would run unasked. So each state-changing tool step must be matched by a query the
    /// hook sent us. The query and the step travel on different channels, hence the grace.
    ///
    /// Scope: command, file edit, MCP, subagent and web steps. Unclassified (`Tool`) steps are not
    /// watched: agy's internal steps (finish, wait, task bookkeeping) are not known to be hooked,
    /// and killing a healthy session over one would be worse than missing it.
    fn watch_tool_step(self: &Rc<Self>, env: &Envelope) {
        match &env.event {
            Event::ItemStarted { kind, .. } => {
                let Some(tools) = canary_tools(*kind) else {
                    return;
                };
                let Some(approval) = self.approval() else {
                    return;
                };
                if self.adapter.borrow().driver() != Driver::Agy || approval.consume_query(tools) {
                    return;
                }
                let item_id = env.item.clone();
                if let Some(id) = &item_id {
                    if self.canary_cancelled.borrow().contains(id) {
                        return;
                    }
                }
                let weak = Rc::downgrade(self);
                let generation = self.generation.get();
                let process_generation = self.process_generation.get();
                let grace = self.canary_grace.get();
                glib::spawn_future_local(async move {
                    glib::timeout_future(grace).await;
                    let Some(inner) = weak.upgrade() else { return };
                    // That process is already gone: replaced, or stopped or exited, which leaves
                    // `generation` as it was. A dead agy runs no more tools to gate.
                    if inner.generation.get() != generation
                        || inner.process_generation.get() != process_generation
                        || !inner.state.borrow().alive
                    {
                        return;
                    }
                    if inner.approval().is_some_and(|a| a.consume_query(tools)) {
                        return;
                    }
                    if let Some(id) = &item_id {
                        if inner.canary_cancelled.borrow().contains(id) {
                            return;
                        }
                    }
                    inner.trip_canary();
                });
            }
            Event::ItemCompleted { status, error, .. } => {
                if let Some(id) = &env.item {
                    if *status != ItemStatus::Completed
                        || error.as_deref().is_some_and(|e| {
                            e.contains("denied") || e.contains("hook") || e.contains("refused")
                        })
                    {
                        self.canary_cancelled.borrow_mut().insert(id.clone());
                    }
                }
            }
            _ => {}
        }
    }

    /// The user's home, for the dotfile rule of the path check; none under test.
    fn home() -> Option<PathBuf> {
        if cfg!(test) {
            return None;
        }
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|h| h.is_absolute())
    }

    /// The thread's directory, and the working directory the call's rule is judged in: `cwd` in
    /// its input, else the thread's; none for a Claude file call (Claude's own directory moves
    /// with `cd`, so a relative path is never offered or matched).
    fn rule_dirs(
        &self,
        input: &serde_json::Value,
        action: Option<&crate::always_allow::Action>,
    ) -> (PathBuf, Option<String>) {
        let open_cwd = self.open.borrow().cwd.clone();
        let file_call = action.is_some_and(|a| {
            matches!(
                a.kind(),
                crate::always_allow::ActionKind::Edit | crate::always_allow::ActionKind::Read
            )
        });
        let cwd = if file_call && self.adapter.borrow().driver() == Driver::Claude {
            None
        } else {
            Some(
                input
                    .get("cwd")
                    .and_then(serde_json::Value::as_str)
                    .filter(|c| !c.is_empty())
                    .map_or_else(|| open_cwd.clone(), str::to_owned),
            )
        };
        (PathBuf::from(open_cwd), cwd)
    }

    /// Whether an Always rule allows `action`, from the cached store. An unusable file allows
    /// nothing: saved rules are off, the notice names the file once per thread, and the file is
    /// never touched.
    fn saved_allow(
        &self,
        action: &crate::always_allow::Action,
        ctx: &crate::always_allow::Context,
    ) -> bool {
        // The cache borrow ends inside the helper, before any notice goes out.
        match crate::always_allow::saved_allow(&self.saved_rules, action, ctx) {
            Ok(allowed) => allowed,
            Err(notice) => {
                warn!("approval rules file unusable; saved rules are off");
                if !self.rules_blocked_noticed.replace(true) {
                    self.emit(Envelope::new(Event::Notice { text: notice }));
                }
                false
            }
        }
    }

    /// Codex fields an approval request is known to carry. Any other field with a
    /// value (an escalation, extra permissions, a grant root) means the request is not the plain
    /// call a rule was made for, so it is never auto-accepted.
    fn codex_plain_request(tool: &str, input: &serde_json::Value) -> bool {
        const COMMAND: &[&str] = &[
            "kind",
            "threadId",
            "turnId",
            "itemId",
            "startedAtMs",
            "environmentId",
            "command",
            "cwd",
            "commandActions",
            "proposedExecpolicyAmendment",
            "availableDecisions",
            "reason",
        ];
        // `changes` is the adapter's own: the diff it takes from the item that started the patch.
        const FILE: &[&str] = &[
            "kind",
            "threadId",
            "turnId",
            "itemId",
            "startedAtMs",
            "reason",
            "changes",
        ];
        let (known, kind) = match tool {
            "command_execution" => (COMMAND, "command"),
            "file_change" => (FILE, "fileChange"),
            // An MCP tool-call approval carries the call's own arguments, which an MCP rule does
            // not judge (it names the tool, whatever its arguments). The adapter names `mcp__`
            // tools only for a request it tied to one open call (`on_mcp_tool_approval`).
            t if t.starts_with("mcp__") => return true,
            _ => return false,
        };
        let Some(fields) = input.as_object() else {
            return false;
        };
        let kind_ok = match fields.get("kind") {
            None | Some(serde_json::Value::Null) => true,
            Some(k) => k.as_str() == Some(kind),
        };
        kind_ok
            && fields
                .iter()
                .all(|(k, v)| known.contains(&k.as_str()) || v.is_null())
    }

    /// The app's own approval gate for Claude and Codex (agy's is its socket). For an
    /// `ApprovalRequested` envelope it sets the buttons that mean the same everywhere ("Allow for
    /// session" only where a rule could match the call; "Always allow" also needs a rules file and
    /// a kind it applies to: shell, a contained edit, an MCP tool), says what Always keeps,
    /// records the call for the answer, and returns
    /// whether a session rule of this agent or an Always rule already allows it. With saved rules
    /// off ([`crate::always_allow::saved_rules_enabled`]) Always neither applies nor is offered.
    fn gate_approval(&self, mut env: Envelope) -> (Envelope, bool) {
        let driver = self.adapter.borrow().driver();
        if driver == Driver::Agy {
            return (env, false);
        }
        // A Codex permission request is never matched, answered or remembered by a rule.
        if matches!(&env.event, Event::ApprovalRequested { tool, .. }
            if tool == agent_core::codex::PERMISSIONS_TOOL)
        {
            return (env, false); // narrowed by `hold_codex_permissions`
        }
        let Some(request) = env.request.clone() else {
            return (env, false);
        };
        let Event::ApprovalRequested {
            tool,
            input,
            options,
            remembers,
            ..
        } = &mut env.event
        else {
            return (env, false);
        };
        let action = crate::always_allow::Action::of(tool, input);
        let (workspace, cwd) = self.rule_dirs(input, action.as_ref());
        let home = Self::home();
        let ctx = crate::always_allow::Context {
            driver: driver_name(driver),
            workspace: &workspace,
            cwd: cwd.as_deref(),
            home: home.as_deref(),
        };
        // In a supervised child, a rule applies, and a card offers to keep one, only where
        // its gate says (inside its own lease, a writing mode within the parent's ceiling); a
        // card the gate forced (`AskOnly`) offers none.
        let forced = self.ask_only.borrow().contains(&request);
        let rule_ok = !forced
            && action
                .as_ref()
                .is_some_and(|a| self.rule_may_apply(a, cwd.as_deref()));
        let offer = rule_ok
            && action
                .as_ref()
                .is_some_and(|a| crate::always_allow::offerable(a, &ctx));
        // Automatic Pattern Approvals off: saved rules neither approve nor are offered.
        let saved_on = crate::always_allow::saved_rules_enabled();
        let always = offer
            && saved_on
            && self.saved_rules.borrow().is_some()
            && action
                .as_ref()
                .is_some_and(crate::always_allow::always_offerable);
        options.retain(|d| match d {
            Decision::AllowForSession => offer,
            Decision::AllowAlways => always,
            _ => true,
        });
        *remembers = action.as_ref().and_then(|a| {
            crate::approval_server::card_note(
                a,
                &ctx,
                options.contains(&Decision::AllowForSession),
                options.contains(&Decision::AllowAlways),
            )
        });
        // A Codex request with a field a plain call does not carry (an escalation) is never
        // answered by the app, by a rule or by Accept edits.
        let plain = driver != Driver::Codex || Self::codex_plain_request(tool, input);
        // Codex Accept edits runs on `untrusted`: the app accepts edits whose every path is
        // in the workspace and not sensitive; anything else, and every shell command, asks.
        // A child's Accept edits auto also needs every target to be one its rules may cover.
        let accept_edit = driver == Driver::Codex
            && tool == "file_change"
            && self.state.borrow().mode == Mode::AcceptEdits
            && !forced
            && codex_edits_contained(input, &workspace, home.as_deref())
            && (self.child.is_none()
                || crate::always_allow::codex_change_targets(input).is_some_and(|targets| {
                    targets.iter().all(|path| {
                        let edit = crate::always_allow::Action::Edit {
                            path: (*path).to_owned(),
                        };
                        self.rule_may_apply(&edit, cwd.as_deref())
                    })
                }));
        // Dangerous permissive mode: the agent's own bypass should leave nothing to ask, and
        // whatever still arrives (escalations included) is approved, as the user agreed to. A
        // supervised child never runs it.
        let bypass = self.state.borrow().mode == Mode::Bypass && self.child.is_none();
        let bridge = self.bound_generation.get()
            && !forced
            && self.adapter.borrow().is_own_bridge_approval(&request);
        let auto = options.contains(&Decision::Allow)
            && (bridge
                || bypass
                || plain
                    && (accept_edit
                        || rule_ok
                            && action.as_ref().is_some_and(|a| {
                                let in_session =
                                    self.session_rules.borrow().matches_session(a, &ctx);
                                in_session || saved_on && self.saved_allow(a, &ctx)
                            })));
        // The folder the card offered (pinned: the answer keeps it only if it is still the same).
        let shown = action
            .as_ref()
            .filter(|_| options.contains(&Decision::AllowForSession))
            .and_then(|a| crate::always_allow::session_folder(a, &ctx));
        if let Some(action) = action {
            self.pending_rules
                .borrow_mut()
                .insert(request, (action, cwd, shown));
        }
        (env, auto)
    }

    /// Claude blocks on a `can_use_tool` request for as long as the app stays silent. A request
    /// still unanswered at the deadline is denied with a notice (fail closed), as agy's socket
    /// does. A request answered, cancelled or expired by then is no longer pending: nothing is
    /// sent.
    fn arm_approval_deadline(self: &Rc<Self>, env: &Envelope) {
        // AskUserQuestion arrives as `QuestionRequested`, which is intentionally not timed.
        if self.adapter.borrow().driver() != Driver::Claude
            || !matches!(env.event, Event::ApprovalRequested { .. })
        {
            return;
        }
        let Some(request) = env.request.clone() else {
            return;
        };
        let weak = Rc::downgrade(self);
        let deadline = self.approval_deadline.get();
        glib::spawn_future_local(async move {
            glib::timeout_future(deadline).await;
            if let Some(inner) = weak.upgrade() {
                inner.expire_approval(&request);
            }
        });
    }

    fn expire_approval(self: &Rc<Self>, request: &str) {
        if !self
            .adapter
            .borrow()
            .pending_request_ids()
            .iter()
            .any(|id| id == request)
        {
            return;
        }
        self.pending_rules.borrow_mut().remove(request);
        let denied = self.command(Command::Approve {
            request: request.to_owned(),
            decision: Decision::Deny,
            updated_input: None,
            message: Some("No answer in agent-terminal before the deadline; denied.".to_owned()),
        });
        if denied {
            self.emit(
                Envelope::new(Event::ApprovalResolved {
                    decision: Decision::Deny,
                })
                .request(request),
            );
            self.emit(Envelope::new(Event::Notice {
                text: "A permission request got no answer in time and was denied.".to_owned(),
            }));
        }
    }

    /// Answers `env`'s request Allow once its card is out (the card then shows it resolved).
    fn auto_approve(self: &Rc<Self>, env: &Envelope) {
        let Some(req) = env.request.clone() else {
            return;
        };
        info!("auto-approving a call allowed by the session's policy");
        let weak = Rc::downgrade(self);
        glib::MainContext::ref_thread_default().invoke_local(move || {
            if let Some(inner) = weak.upgrade() {
                inner.respond_approval(&req, Decision::Allow);
            }
        });
    }

    /// The rule "Allow for session" / "Always allow" would keep for a Claude or Codex call,
    /// judged afresh by [`crate::always_allow::decide`] (the card's buttons are not trusted), with
    /// what the user should know about it ([`crate::always_allow::Rule::warnings`]). Pure:
    /// nothing is kept until the agent has taken the answer ([`Self::keep_rule`]).
    fn plan_rule(
        &self,
        pending: Option<&PendingRule>,
        decision: Decision,
        custom: Option<crate::always_allow::Rule>,
    ) -> Result<(crate::always_allow::Rule, Vec<String>), String> {
        let driver = self.adapter.borrow().driver();
        let workspace = PathBuf::from(self.open.borrow().cwd.clone());
        let home = Self::home();
        let ctx = crate::always_allow::Context {
            driver: driver_name(driver),
            workspace: &workspace,
            cwd: pending.and_then(|(_, cwd, _)| cwd.as_deref()),
            home: home.as_deref(),
        };
        let scope = if decision == Decision::AllowAlways {
            crate::always_allow::Scope::Always
        } else {
            crate::always_allow::Scope::Session
        };
        crate::always_allow::decide_as_shown(
            pending.map(|(a, _, _)| a),
            custom,
            scope,
            &ctx,
            pending.and_then(|(_, _, shown)| shown.as_deref()),
        )
        .map(|rule| {
            let warnings = rule.warnings(scope, Some(&ctx));
            (rule, warnings)
        })
    }

    /// Keeps `rule`: for the session at once, and for an Always answer also in the rules file,
    /// written off the main thread (a failure is said in a notice; the session rule stands).
    /// Nothing reaches the file while saved rules are off.
    fn keep_rule(&self, rule: crate::always_allow::Rule, decision: Decision) {
        let path = self
            .saved_rules
            .borrow()
            .as_ref()
            .filter(|_| crate::always_allow::saved_rules_enabled())
            .map(|cache| cache.path().to_owned());
        if let (Decision::AllowAlways, Some(path)) = (decision, path) {
            let weak = self.weak_self.borrow().clone();
            let saved = rule.clone();
            glib::spawn_future_local(async move {
                let target = path.clone();
                // Checked again at the write: the switch may have turned off since the answer.
                let result = gtk4::gio::spawn_blocking(move || {
                    crate::always_allow::AlwaysRules::remember_while(
                        &target,
                        saved,
                        &crate::always_allow::saved_rules_enabled,
                    )
                })
                .await;
                let Some(inner) = weak.and_then(|w| w.upgrade()) else {
                    return;
                };
                match result {
                    Ok(Ok(_)) => info!("remembered an always-allow rule"),
                    Ok(Err(e)) => {
                        warn!(error = %e, "always-allow rule not saved");
                        inner.rules_blocked_noticed.set(true);
                        inner.emit(Envelope::new(Event::Notice {
                            text: crate::always_allow::blocked_notice(&path, &e),
                        }));
                    }
                    Err(_) => warn!("the always-allow save worker panicked"),
                }
            });
        }
        self.session_rules.borrow_mut().add(rule);
    }

    /// Ends this thread's session approvals (a cross-agent switch); returns how many.
    fn end_session_rules(&self) -> usize {
        self.pending_rules.borrow_mut().clear();
        let mut rules = self.session_rules.borrow_mut();
        let n = rules.rules.len();
        rules.rules.clear();
        n
    }

    /// The hook is not gating agy: stop it now and carry on without the skip flag (plan mode, no
    /// socket), so commands are refused again.
    fn trip_canary(self: &Rc<Self>) {
        if self.child.is_some() {
            self.error("The delegated Antigravity agent ran a tool without its approval hook; it was stopped.");
            self.stop_current();
            *self.approval.borrow_mut() = None;
            return;
        }
        warn!("agy ran a tool without a hook query; restarting it without the skip flag");
        self.error(
            "agy ran a tool without asking agent-terminal; the approval hook is not active \u{2014} restarting it in plan mode without --dangerously-skip-permissions (it refuses commands, but may still edit files)",
        );
        self.stop_current();
        *self.approval.borrow_mut() = None; // closes the socket
        let native = self.state.borrow().native_id.clone();
        {
            let mut open = self.open.borrow_mut();
            open.mode = Mode::Plan;
            open.resume = native.or_else(|| open.resume.take());
            open.new_session_id = None;
        }
        self.state.borrow_mut().mode = Mode::Plan;
        self.emit(Envelope::new(Event::ModeChanged { mode: Mode::Plan }));
        self.start_process();
        self.dispatch_queue_after_restart();
    }

    fn on_exit(self: &Rc<Self>, generation: u64, code: Option<i32>) {
        if generation != self.generation.get() {
            return;
        }
        if self.agy_reading.get() == Some(generation) {
            // Lines are still waiting on the transcript read; they come before the exit.
            self.agy_deferred_exit.set(Some((generation, code)));
            return;
        }
        info!(code = ?code, "agent exited");
        if self.retry_without_thinking(code) {
            return;
        }
        self.finish_process(code, false);
        // A switch's new agent that died before it ever started: back to the old one. Taken
        // first, as in start_process.
        let previous = self.rollback.borrow_mut().take();
        if let Some(previous) = previous {
            let reason = match code {
                Some(code) => format!("it exited (code {code}) before starting"),
                None => "it stopped before starting".to_owned(),
            };
            self.roll_back(previous, &reason);
        }
    }

    /// Claude exited refusing `--thinking-display`. Restarts it ONCE without the flag (the
    /// session never fails to start over it) and shows the thread's reasoning as not shared.
    /// False when this exit is anything else.
    fn retry_without_thinking(self: &Rc<Self>, code: Option<i32>) -> bool {
        let flag = &self.thinking;
        // Only a failed exit before the CLI ever started (no `init`) is the start-up refusal.
        let failed_at_start = code.is_some_and(|c| c != 0) && !flag.init_seen.get();
        if !(flag.passed.get() && flag.refused.get() && failed_at_start) || flag.retried.get() {
            return false;
        }
        flag.retried.set(true);
        let prompts = self.pre_init_prompts.take();
        let _stopping = ScopedFlag::set(&self.stopping);
        let program = self.open.borrow().program.clone();
        crate::claude_probe::note_thinking_refused(&program);
        // The app restarts it, so the exit is not reported as a failure.
        self.finish_process(code, true);
        let id = format!("thinking-unavailable-{}", self.generation.get());
        for env in [
            Envelope::new(Event::ItemStarted {
                kind: ItemKind::Reasoning,
                title: "Thinking".to_owned(),
                input: None,
                parent: None,
            }),
            Envelope::new(Event::Reasoning {
                state: agent_core::event::ReasoningState::Unavailable,
                label: agent_core::event::ReasoningLabel::Thinking,
            }),
            Envelope::new(Event::ItemCompleted {
                status: ItemStatus::Completed,
                output: None,
                error: None,
            }),
        ] {
            self.emit(env.item(id.clone()));
        }
        self.start_process();
        for sent in prompts {
            self.prompt_in_flight.set(true);
            if !self.dispatch_sent_prompt(&sent.text, sent.handoff.clone()) {
                self.prompt_in_flight.set(false);
                self.held_retries.borrow_mut().push_back(sent);
            }
        }
        true
    }

    /// Closes out the current process: adapter exit envelopes, status, pending approvals.
    /// `deliberate`: the app stopped it (a switch or a close), so its exit is expected and an
    /// open turn was interrupted, never failed.
    fn finish_process(&self, code: Option<i32>, deliberate: bool) {
        self.deferred_mode.set(None);
        // A start still waiting on its checks never spawns: its writes and delegation binding go,
        // and its user messages queue again (unless the user's own stop cleared them first).
        let unsent = self.starting.borrow_mut().take().map(|p| {
            if !self.carrying_task.get() {
                return p.prompts;
            }
            // A busy switch is stopping this start: its stored messages are in the transcript
            // the switch's handoff carries, so the switch holds them until its continuation is
            // sent. One that could not be stored is not in it, and queues again.
            let (carried, rest): (Vec<_>, Vec<_>) = p.prompts.into_iter().partition(|q| q.stored);
            self.carried_unsent.borrow_mut().extend(carried);
            rest
        });
        self.abandon_unspawned_binding();
        let proc = self.proc.borrow_mut().take();
        drop(proc); // terminates it when it is still running
                    // Synthetic: `stop_current` gets here before the process is gone. The supervisor kills
                    // the generation's token and ends its runs; only the exit watch reports the real exit.
        let process_generation = self.process_generation.get();
        let closing = self.stopped_generation.replace(process_generation) < process_generation;
        if closing {
            if let Some(hooks) = &self.hooks {
                hooks.process_stopping(ProcessGeneration(process_generation));
            }
        }
        *self.native_turn.borrow_mut() = None;
        let mut envelopes = self.adapter.borrow_mut().on_exit(code);
        if deliberate {
            envelopes.iter_mut().for_each(as_deliberate_stop);
        }
        {
            let mut state = self.state.borrow_mut();
            state.alive = false;
            state.running_turn = false;
        }
        self.prompt_in_flight.set(false);
        self.pre_init_prompts.borrow_mut().clear();
        // A dead process completed nothing: a pending handoff goes out with the next prompt,
        // even when the exit raised no turn of its own (it died before one opened).
        self.handoff_in_flight.set(false);
        // The final exit envelope repaints the header. Clear pending state first so a dead
        // process cannot leave the chip claiming its model change is still in progress.
        self.reject_model_change();
        self.pending_rules.borrow_mut().clear();
        // The app-server process held Codex's session grants; they ended with it.
        self.codex_permissions.borrow_mut().clear();
        self.held_permissions.borrow_mut().clear();
        self.codex_grants.borrow_mut().clear();
        self.codex_grant_paths.borrow_mut().clear();
        self.codex_turn_grant_paths.borrow_mut().clear();
        if !self.quiet_exit.get() {
            for env in envelopes {
                self.emit(env);
            }
        }
        // After the expiries above, which still need the ids cards were shown again under.
        self.codex_shown.borrow_mut().clear();
        if let Some(a) = self.approval() {
            a.expire_all("the agent exited");
        }
        self.open_approvals.borrow_mut().clear();
        self.active_workers.set(0);
        self.approval_inputs.borrow_mut().clear();
        self.ask_only.borrow_mut().clear();
        self.bound_generation.set(false);
        if closing {
            if let Some(child) = &self.child {
                child
                    .publish
                    .cancel_generation(ProcessGeneration(process_generation));
            }
        }
        // Last, once the exit has settled the turn: the start's messages wait for the next one.
        if let Some(unsent) = unsent {
            self.keep_unsent_prompts(unsent);
        }
    }

    /// Forgets an in-flight agy transcript read, its held lines and a deferred exit: they belong
    /// to a process that is gone, and its read's continuation then finds nothing to do.
    fn clear_agy_read(&self) {
        self.agy_reading.set(None);
        self.agy_hold.borrow_mut().clear();
        self.agy_deferred_exit.set(None);
    }

    /// Stops the process on purpose; its own exit callback is then ignored.
    fn stop_current(&self) {
        let _stopping = ScopedFlag::set(&self.stopping);
        self.queued_dispatch_pending.set(false);
        self.generation.set(self.generation.get() + 1);
        self.clear_agy_read();
        self.adapter.borrow_mut().discard_queued_prompts();
        self.finish_process(None, true);
    }

    /// What stops [`Self::suspend`] now, if anything (the first reason found). The process can
    /// be stopped and resumed later with nothing lost only when it is alive, has a conversation
    /// to resume (a native id, and a turn on it or a resumed start), and holds no work,
    /// question or state that lives only in the process or in flight. Never a delegated worker,
    /// never dangerous permissive mode (session-only: a resume would not re-enter it), and
    /// never with session grants Codex's own process holds (they would end unannounced).
    fn suspend_blocker(&self) -> Option<&'static str> {
        let state = self.state.borrow();
        let checks = [
            (!state.alive, "not running"),
            (state.running_turn, "a turn is running"),
            (state.mode == Mode::Bypass, "dangerous permissive mode"),
            (state.native_id.is_none(), "no session id to resume"),
            (
                !self.has_conversation.get(),
                "no conversation yet to resume",
            ),
            (
                !self.codex_grants.borrow().is_empty(),
                "Codex session grants",
            ),
            (self.child.is_some(), "a delegated worker"),
            (self.active_workers.get() > 0, "active provider workers"),
            (
                !self.proc.borrow().as_ref().is_some_and(|p| p.is_alive()),
                "no live process",
            ),
            (self.starting.borrow().is_some(), "starting"),
            (self.stopping.get(), "stopping"),
            (self.rebinding.get(), "rebinding"),
            (self.delivering.get(), "delivering"),
            (self.dispatching_queue.get(), "dispatching the queue"),
            (self.queued_dispatch_pending.get(), "a queued dispatch"),
            (self.prompt_in_flight.get(), "a prompt in flight"),
            (self.handoff_in_flight.get(), "a handoff in flight"),
            (!self.queued_prompts.borrow().is_empty(), "queued prompts"),
            (!self.held_retries.borrow().is_empty(), "held retries"),
            (
                !self.pre_init_prompts.borrow().is_empty(),
                "input before init",
            ),
            (self.pending_model.borrow().is_some(), "a model change"),
            (self.deferred_mode.get().is_some(), "a deferred mode"),
            (self.rollback.borrow().is_some(), "a switch not confirmed"),
            (
                self.provisional_native.borrow().is_some(),
                "a Codex thread not yet resumable",
            ),
            (!self.open_approvals.borrow().is_empty(), "an open approval"),
            (!self.pending_rules.borrow().is_empty(), "a pending rule"),
            (
                !self.codex_permissions.borrow().is_empty()
                    || !self.held_permissions.borrow().is_empty(),
                "an open permission request",
            ),
            (self.agy_reading.get().is_some(), "a transcript read"),
        ];
        checks
            .into_iter()
            .find_map(|(blocked, why)| blocked.then_some(why))
    }

    /// Stops an idle agent process to free its memory, keeping its native session so the next
    /// prompt resumes it ([`Self::ensure_alive`]). Silent: no exit reaches the view or the
    /// store, since nothing ended for the user. A delegation root's stop is a planned rotation
    /// its hooks agree to (nothing revoked; the next prompt rebinds quietly). False, doing
    /// nothing, while [`Self::suspend_blocker`] names a reason or the hooks refuse.
    fn suspend(&self) -> bool {
        if let Some(why) = self.suspend_blocker() {
            tracing::debug!(thread = %self.thread, why, "idle agent not suspended");
            return false;
        }
        let generation = ProcessGeneration(self.process_generation.get());
        if let Some(hooks) = &self.hooks {
            if !hooks.suspending(generation) {
                return false;
            }
        }
        let _quiet = ScopedFlag::set(&self.quiet_exit);
        self.stop_current();
        self.suspended.set(true);
        // Silent for the view and the store, but not for the lifecycle.
        self.notify_lifecycle();
        true
    }

    /// Where the agent process is in its life ([`ChatSession::lifecycle`]).
    fn lifecycle(&self) -> crate::chat::Lifecycle {
        let alive = self.state.borrow().alive;
        let starting = alive && !self.reported.get();
        crate::chat::Lifecycle {
            alive,
            starting,
            resuming: starting && self.resuming.get(),
            // A stopped process with nothing started since: the next prompt resumes it.
            suspended: !alive && self.suspended.get(),
        }
    }

    /// The current process is up (see `reported`).
    fn mark_up(&self) {
        self.reported.set(true);
        self.resuming.set(false);
        self.notify_lifecycle();
    }

    /// Tells the lifecycle listener when the lifecycle changed since it was last told.
    fn notify_lifecycle(&self) {
        let now = self.lifecycle();
        if self.lifecycle_told.replace(now) == now {
            return;
        }
        let listener = self.lifecycle_listener.borrow().clone();
        if let Some(listener) = listener {
            listener(now);
        }
    }

    /// A dead process (agy after an interrupt) is restarted resuming its native session.
    fn ensure_alive(self: &Rc<Self>) {
        // A rebind owns the next start: it waits for the old generation's exit and a staged
        // binding, then calls `rebind_start`.
        if self.state.borrow().alive || self.rebinding.get() {
            return;
        }
        let native = self.state.borrow().native_id.clone();
        if let Some(id) = native {
            let mut open = self.open.borrow_mut();
            open.resume = Some(id);
            open.new_session_id = None;
        }
        self.start_process();
    }

    /// Starts a fresh agent process for this thread without creating a new provider
    /// conversation. This lets process-start configuration, such as MCP registrations, take
    /// effect without losing the thread.
    fn reload_session(self: &Rc<Self>) {
        if self.state.borrow().alive {
            self.stop_current();
        }
        self.ensure_alive();
        self.dispatch_queue_after_restart();
    }

    fn dispatch_queue_after_restart(self: &Rc<Self>) {
        let ready = {
            let state = self.state.borrow();
            state.alive && !state.running_turn
        };
        if !ready {
            return;
        }
        self.queued_dispatch_pending.set(true);
        if self.dispatching_queue.get() {
            // A restart from the sink must let the interrupted dispatch requeue its item first.
            let weak = Rc::downgrade(self);
            let generation = self.generation.get();
            glib::spawn_future_local(async move {
                if let Some(inner) = weak.upgrade() {
                    if inner.generation.get() == generation {
                        inner.flush_queued_dispatch();
                    }
                }
            });
        } else {
            self.flush_queued_dispatch();
        }
    }

    // ---- commands ----

    /// Encodes and carries out one command. False when the adapter refused it.
    fn command(self: &Rc<Self>, command: Command) -> bool {
        let model_change = matches!(command, Command::SetModel { .. });
        let result = self.adapter.borrow_mut().encode(command);
        match result {
            Ok(actions) => {
                self.execute(actions);
                self.drain_outbox();
                true
            }
            Err(e) => {
                if model_change {
                    self.reject_model_change();
                }
                self.report(&e);
                false
            }
        }
    }

    /// Carries out what the adapter produced on its own initiative (Codex: prompts queued until
    /// its thread exists, replies to server requests, answers it can give itself): its actions
    /// like an `encode`'s, its events like a `feed`'s. Called after every `encode`, `feed` and
    /// `feed_side`. Bounded, because executing an action may make the adapter say more.
    fn drain_outbox(self: &Rc<Self>) {
        for _ in 0..8 {
            let outbox = self.adapter.borrow_mut().drain_outbox();
            if outbox.is_empty() {
                return;
            }
            for env in outbox.events {
                self.observe(&env);
                self.emit(env.clone());
                self.watch_tool_step(&env);
            }
            self.execute(outbox.actions);
        }
    }

    fn execute(self: &Rc<Self>, actions: Vec<Action>) {
        self.execute_with_control(actions, None);
    }

    fn execute_with_control(self: &Rc<Self>, actions: Vec<Action>, control: Option<&str>) {
        for action in actions {
            match action {
                Action::Write(lines) => {
                    let written = {
                        let proc = self.proc.borrow();
                        let mut starting = self.starting.borrow_mut();
                        match (proc.as_ref().filter(|p| p.is_alive()), starting.as_mut()) {
                            (Some(p), _) => {
                                for line in &lines {
                                    p.write_line(line);
                                }
                                true
                            }
                            // Held until the start's hook proof is in, then written in order.
                            (None, Some(pending)) => {
                                pending.writes.extend(lines);
                                true
                            }
                            (None, None) => false,
                        }
                    };
                    if !written {
                        // Suspension is not a failure: say what it is, and how to resume.
                        let paused = self.suspended.get();
                        if let Some(id) = control {
                            let error = if paused {
                                "The agent is paused to save memory; send a message to resume it."
                            } else {
                                "The agent is not running; send a message to start it again."
                            };
                            self.emit(
                                Envelope::new(Event::ControlResult {
                                    ok: None,
                                    error: Some(error.to_owned()),
                                })
                                .request(id),
                            );
                        } else if paused {
                            // Said, not stored: a pause is not part of the thread's history.
                            self.deliver(&Envelope::new(Event::Notice {
                                text: "The agent is paused to save memory; send a message to \
                                       resume it."
                                    .to_owned(),
                            }));
                        } else {
                            self.error("The agent is not running.");
                        }
                    }
                }
                Action::Interrupt => {
                    let interrupted = match self.proc.borrow().as_ref() {
                        Some(p) => {
                            p.interrupt();
                            true
                        }
                        None => false,
                    };
                    // A start still waiting on its hook proof ends the way an interrupted agy
                    // does (it exits): what it held is dropped, its messages included (the user
                    // stopped them), and the next prompt restarts it.
                    if !interrupted && self.starting.borrow().is_some() {
                        if let Some(pending) = self.starting.borrow_mut().as_mut() {
                            pending.prompts.clear();
                        }
                        self.stop_current();
                    }
                }
                Action::SideProcess { id, argv } => self.spawn_side(id, argv),
                Action::Respawn(delta) => self.respawn(&delta),
            }
        }
    }

    fn spawn_side(self: &Rc<Self>, id: String, argv: Vec<String>) {
        let cwd = Some(self.open.borrow().cwd.clone()).filter(|c| !c.is_empty());
        let env = self.launch_env.borrow().clone();
        let weak = Rc::downgrade(self);
        let epoch = self.adapter_epoch.get();
        glib::spawn_future_local(async move {
            let (stdout, ok) = run_side(argv, cwd, &env, AGY_TIMEOUT).await;
            let Some(inner) = weak.upgrade() else { return };
            inner.finish_side(epoch, &id, &stdout, ok);
        });
    }

    /// A side process's result, for the adapter that started it (`epoch`). The thread switched
    /// agent meanwhile: its request is answered with an error (so nothing waits on it forever)
    /// and the output is dropped, rather than fed to the new agent's adapter.
    fn finish_side(self: &Rc<Self>, epoch: u64, id: &str, stdout: &str, ok: bool) {
        if epoch != self.adapter_epoch.get() {
            self.emit(
                Envelope::new(Event::ControlResult {
                    ok: None,
                    error: Some("the thread switched agent before this finished".to_owned()),
                })
                .request(id),
            );
            return;
        }
        let envelopes = self.adapter.borrow_mut().feed_side(id, stdout, ok);
        for env in envelopes {
            self.emit(env);
        }
        self.drain_outbox();
    }

    /// Stops the process and starts a new one with `delta` applied (the adapter set
    /// `resume` to the native id so history carries over).
    fn respawn(self: &Rc<Self>, delta: &OpenSessionDelta) {
        self.stop_current();
        if let Some(model) = &delta.model {
            let effort = delta
                .effort
                .clone()
                .or_else(|| self.state.borrow().effort.clone());
            self.prepare_model_change(model.clone(), effort, true);
        }
        // Without the hook agy cannot ask, so Ask is never put on its command line.
        let mode = delta
            .mode
            .map(|m| effective_mode(self.ask_unavailable(), m));
        let native = self.state.borrow().native_id.clone();
        {
            let mut open = self.open.borrow_mut();
            if let Some(m) = &delta.model {
                open.model = Some(m.clone());
            }
            if let Some(e) = &delta.effort {
                open.effort = Some(e.clone());
            }
            if let Some(mode) = mode {
                open.mode = mode;
            }
            open.resume = delta.resume.clone().or(native);
            open.new_session_id = None;
        }
        if let Some(m) = &delta.model {
            self.emit(Envelope::new(Event::ModelChangeRequested {
                model: m.clone(),
            }));
        }
        if let Some(mode) = mode {
            self.emit(Envelope::new(Event::ModeChanged { mode }));
        }
        self.start_process();
    }

    /// agy with no approval handle: Plan and Accept edits go to its `--mode` flag, but it cannot
    /// ask before acting.
    fn ask_unavailable(&self) -> bool {
        self.approval().is_none() && self.adapter.borrow().driver() == Driver::Agy
    }

    /// A first word like `/model` or `/compact`: a command for the agent, which it only
    /// recognises at the start of the message.
    fn is_slash_command(text: &str) -> bool {
        text.split_whitespace().next().is_some_and(|w| {
            w.len() > 1
                && w.starts_with('/')
                && w[1..]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':')
        })
    }

    /// A prompt must not start a turn on a retired model: move to its replacement first.
    /// False if the notice's sink stopped or switched the replacement process.
    fn apply_retired(self: &Rc<Self>) -> bool {
        let Some(retired) = self.retired.borrow_mut().take() else {
            return true;
        };
        let driver = self.adapter.borrow().driver();
        self.switch(
            driver,
            Some(retired.replacement.model.clone()),
            retired.replacement.effort.clone(),
        );
        let generation = self.generation.get();
        self.emit(Envelope::new(Event::Notice {
            text: retired_notice(&retired),
        }));
        generation == self.generation.get()
    }

    /// Remembers the model the thread was set to, for the next time it is opened.
    fn persist_model(&self, model: &str) {
        *self.selected.borrow_mut() = Some(model.to_owned());
        let pt = self.provider_thread.borrow().clone();
        if pt.is_empty() {
            return;
        }
        if let Err(e) = self.store.set_provider_model(&pt, model) {
            warn!(error = %e, "could not record the thread's model");
        }
    }

    fn queue_prompt(self: &Rc<Self>, text: &str) {
        let item = glib::uuid_string_random().to_string();

        self.deliver(
            &Envelope::new(Event::ItemStarted {
                kind: ItemKind::UserMessage,
                title: "queued".to_string(),
                input: Some(serde_json::json!({"queued": true})),
                parent: None,
            })
            .item(item.clone()),
        );
        self.deliver(
            &Envelope::new(Event::ContentSnapshot {
                stream: StreamKind::Assistant,
                text: agent_core::redact::redact(text),
            })
            .item(item.clone()),
        );
        self.queued_prompts.borrow_mut().push_back(QueuedPrompt {
            item_id: item,
            text: text.to_owned(),
            stored: false,
        });
    }

    /// A message written while a turn runs, sent at once to an agent that takes it then
    /// (`Capabilities::live_input`: Claude folds it into the running turn, Codex steers it). It
    /// shows as an ordinary message, not a queued one. Everything else goes the way it always
    /// did: sent as a prompt if the turn ended meanwhile, else queued — an agent without live
    /// input, a slash command (mid-turn it would stop being one), or a handoff still waiting to
    /// ride on a real prompt. A write the adapter refuses turns the shown message into a queued
    /// one, so it is never lost.
    fn steer_prompt(self: &Rc<Self>, text: &str) {
        if !self.root_prompt_allowed() {
            return self.queue_prompt(text);
        }
        if !self.state.borrow().running_turn {
            return self.send_prompt(text);
        }
        let live = self.adapter.borrow().capabilities().live_input;
        let handoff_waiting =
            self.pending_handoff.borrow().is_some() && !self.handoff_in_flight.get();
        if !live || Self::is_slash_command(text) || handoff_waiting {
            return self.queue_prompt(text);
        }
        let pt = self.provider_thread.borrow().clone();
        let provider = (!pt.is_empty()).then_some(pt.as_str());
        let (item, stored) = match self.store.append_user_message(&self.thread, provider, text) {
            Ok(item) => (item, true),
            Err(e) => {
                warn!(error = %e, "could not persist the user message");
                (glib::uuid_string_random().to_string(), false)
            }
        };
        self.deliver(
            &Envelope::new(Event::ItemStarted {
                kind: ItemKind::UserMessage,
                title: String::new(),
                input: None,
                parent: None,
            })
            .item(item.clone()),
        );
        self.deliver(
            &Envelope::new(Event::ContentSnapshot {
                stream: StreamKind::Assistant,
                text: agent_core::redact::redact(text),
            })
            .item(item.clone()),
        );
        if self.root_prompt_allowed()
            && self.command(Command::Steer {
                text: text.to_owned(),
            })
        {
            self.remember_pre_init_prompt(text, None);
            return;
        }
        // Refused: it waits for the turn to end, as a message sent mid-turn always did.
        self.deliver(
            &Envelope::new(Event::ItemStarted {
                kind: ItemKind::UserMessage,
                title: "queued".to_string(),
                input: Some(serde_json::json!({"queued": true})),
                parent: None,
            })
            .item(item.clone()),
        );
        self.queued_prompts.borrow_mut().push_back(QueuedPrompt {
            item_id: item,
            text: text.to_owned(),
            stored,
        });
    }

    fn send_prompt(self: &Rc<Self>, text: &str) {
        if self.rebinding.get() {
            // Nothing runs until the rebind starts the next generation, which sends the queue.
            self.queue_prompt(text);
            return;
        }
        // Anything already waiting goes first: queued messages, and prompts a refused or failed
        // start kept (`held_retries`).
        let waiting =
            !self.queued_prompts.borrow().is_empty() || !self.held_retries.borrow().is_empty();
        if self.dispatching_queue.get()
            || self.delivering.get()
            || !self.state.borrow().running_turn && waiting
        {
            self.queue_prompt(text);
            self.dispatch_queued_prompt();
            return;
        }
        self.send_prompt_now(text);
    }

    /// A confirmed switch's continuation opens its own turn before queued user input.
    fn send_prompt_now(self: &Rc<Self>, text: &str) {
        if !self.root_prompt_allowed() {
            self.queue_prompt(text);
            return;
        }
        if !self.state.borrow().running_turn && !self.apply_retired() {
            self.queue_prompt(text);
            return;
        }
        let pt = self.provider_thread.borrow().clone();
        let provider = (!pt.is_empty()).then_some(pt.as_str());
        let (item, stored) = match self.store.append_user_message(&self.thread, provider, text) {
            Ok(item) => (item, true),
            Err(e) => {
                warn!(error = %e, "could not persist the user message");
                (glib::uuid_string_random().to_string(), false)
            }
        };

        // The handoff rides on the next real prompt: never on a slash command (it would stop
        // being one), nor twice while the turn that carries it runs. It is spent only when that
        // turn completes (`apply`): a prompt that never reached the agent, or a turn the agent
        // died in, leaves it pending for the next one.
        let handoff = if Self::is_slash_command(text) || self.handoff_in_flight.get() {
            None
        } else {
            self.pending_handoff.borrow().clone()
        };

        self.deliver(
            &Envelope::new(Event::ItemStarted {
                kind: ItemKind::UserMessage,
                title: String::new(),
                input: None,
                parent: None,
            })
            .item(item.clone()),
        );
        self.deliver(
            &Envelope::new(Event::ContentSnapshot {
                stream: StreamKind::Assistant,
                text: agent_core::redact::redact(text),
            })
            .item(item.clone()),
        );

        let news = self.news_for_prompt(text, handoff.is_some());
        let sent = match (&handoff, &news) {
            (Some(h), _) => provider_message_with_handoff(&h.summary, &h.fence, text),
            (None, Some(news)) => agent_core::delegation::wake::with_user_prompt(&news.text, text),
            (None, None) => text.to_owned(),
        };
        self.prompt_in_flight.set(true);
        let queued = QueuedPrompt {
            item_id: item,
            text: text.to_owned(),
            stored,
        };
        let accepted = self.dispatch_sent_prompt(&sent, handoff);
        self.settle_news(news, accepted);
        if accepted {
            self.note_unspawned_prompt(queued);
        } else {
            self.hold_queued_prompt(queued);
        }
    }

    /// A user message just written into a start that has not spawned yet: kept with that start,
    /// so one that ends without a process queues it again instead of dropping it.
    fn note_unspawned_prompt(&self, queued: QueuedPrompt) {
        if self.proc.borrow().as_ref().is_some_and(|p| p.is_alive()) {
            return;
        }
        if let Some(pending) = self.starting.borrow_mut().as_mut() {
            pending.prompts.push(queued);
        }
    }

    fn root_prompt_allowed(&self) -> bool {
        self.child.is_some()
            || self
                .hooks
                .as_ref()
                .is_none_or(|hooks| hooks.before_root_prompt())
    }

    fn hold_queued_prompt(&self, queued: QueuedPrompt) {
        let item = queued.item_id.clone();
        self.queued_prompts.borrow_mut().push_front(queued);
        self.prompt_in_flight.set(false);
        self.deliver(
            &Envelope::new(Event::ItemStarted {
                kind: ItemKind::UserMessage,
                title: "queued".to_owned(),
                input: Some(serde_json::json!({"queued": true})),
                parent: None,
            })
            .item(item),
        );
    }

    /// Sends `sent` as a prompt; true when the adapter took it.
    fn dispatch_sent_prompt(self: &Rc<Self>, sent: &str, handoff: Option<Handoff>) -> bool {
        if !self.root_prompt_allowed() {
            return false;
        }
        self.ensure_alive();
        if !self.root_prompt_allowed() {
            return false;
        }
        // A submitted/queued inference is already work: the user must be able to stop it
        // while waiting for the backend's turn/started, not only after that reply arrives.
        let starts_turn =
            self.state.borrow().alive && self.adapter.borrow().prompt_starts_turn(sent);
        let was_running = self.state.borrow().running_turn;
        if starts_turn {
            self.state.borrow_mut().running_turn = true;
        }
        let accepted = self.command(Command::Prompt {
            text: sent.to_owned(),
        });
        if accepted {
            self.prompted.set(true);
            self.remember_pre_init_prompt(sent, handoff.clone());
        }
        if !accepted && starts_turn {
            self.state.borrow_mut().running_turn = was_running;
        }
        if accepted && handoff.is_some() {
            self.handoff_in_flight.set(true);
            // The turn that carries it has not started yet. One already open is not taken as its
            // own, even if it may fold the line in: Claude delivers a line written near a turn's
            // end to the next turn (best effort), and nothing in its stream says which. Its end
            // leaves the handoff in flight; the next turn to start can still spend it.
            self.handoff_turn.set(HandoffTurn::Waiting);
        }
        accepted
    }

    fn remember_pre_init_prompt(&self, text: &str, handoff: Option<Handoff>) {
        if self.adapter.borrow().driver() == Driver::Claude && !self.thinking.init_seen.get() {
            self.pre_init_prompts.borrow_mut().push(Sent {
                text: text.to_owned(),
                handoff,
            });
        }
    }

    fn flush_queued_dispatch(&self) {
        if self.delivering.get() || self.dispatching_queue.get() {
            return;
        }
        if self.queued_dispatch_pending.replace(false) {
            let strong = self.weak_self.borrow().as_ref().and_then(|w| w.upgrade());
            if let Some(strong) = strong {
                if !self.stopping.get() && !self.state.borrow().running_turn {
                    if let Some(mode) = self.deferred_mode.take() {
                        // Restart after the final envelope reached the view, before user input.
                        let restarting = ScopedFlag::set(&self.dispatching_queue);
                        strong.command(Command::SetMode { mode });
                        drop(restarting);
                    }
                }
                strong.dispatch_queued_prompt();
            }
        }
    }

    fn dispatch_queued_prompt(self: &Rc<Self>) {
        if self.stopping.get() || self.dispatching_queue.get() || self.state.borrow().running_turn {
            return;
        }
        if self.delivering.get() {
            self.queued_dispatch_pending.set(true);
            return;
        }
        self.queued_dispatch_pending.set(false);
        let _dispatching = ScopedFlag::set(&self.dispatching_queue);
        // Each pass removes one queued item; an empty queue ends the synchronous drain.
        loop {
            if self.queued_prompts.borrow().is_empty() && self.held_retries.borrow().is_empty() {
                return;
            }
            if !self.root_prompt_allowed() {
                return;
            }
            let retry = self.held_retries.borrow_mut().pop_front();
            if let Some(retry) = retry {
                self.prompt_in_flight.set(true);
                if !self.dispatch_sent_prompt(&retry.text, retry.handoff.clone()) {
                    self.prompt_in_flight.set(false);
                    self.held_retries.borrow_mut().push_front(retry);
                    return;
                }
                if self.state.borrow().running_turn || !self.queued_dispatch_pending.replace(false)
                {
                    return;
                }
                continue;
            }
            let queued = self.queued_prompts.borrow_mut().pop_front();
            let Some(queued) = queued else { return };
            if !self.apply_retired() {
                self.queued_prompts.borrow_mut().push_front(queued);
                return;
            }
            let generation = self.generation.get();
            self.deliver(
                &Envelope::new(Event::ItemStarted {
                    kind: ItemKind::UserMessage,
                    title: String::new(),
                    input: None,
                    parent: None,
                })
                .item(queued.item_id.clone()),
            );
            // The sink can close or switch this session. Keep the message unsent in that case.
            if generation != self.generation.get() || self.state.borrow().running_turn {
                let item = queued.item_id.clone();
                self.queued_prompts.borrow_mut().push_front(queued);
                self.deliver(
                    &Envelope::new(Event::ItemStarted {
                        kind: ItemKind::UserMessage,
                        title: "queued".to_owned(),
                        input: Some(serde_json::json!({"queued": true})),
                        parent: None,
                    })
                    .item(item),
                );
                return;
            }
            let pt = self.provider_thread.borrow().clone();
            let provider = (!pt.is_empty()).then_some(pt.as_str());
            if !queued.stored {
                if let Err(e) = self.store.append_user_message_with_id(
                    &self.thread,
                    provider,
                    &queued.item_id,
                    &queued.text,
                ) {
                    warn!(error = %e, "could not persist the queued user message");
                }
            }
            let handoff = if Self::is_slash_command(&queued.text) || self.handoff_in_flight.get() {
                None
            } else {
                self.pending_handoff.borrow().clone()
            };
            let news = self.news_for_prompt(&queued.text, handoff.is_some());
            let sent = match (&handoff, &news) {
                (Some(h), _) => provider_message_with_handoff(&h.summary, &h.fence, &queued.text),
                (None, Some(news)) => {
                    agent_core::delegation::wake::with_user_prompt(&news.text, &queued.text)
                }
                (None, None) => queued.text.clone(),
            };
            self.prompt_in_flight.set(true);
            let queued = QueuedPrompt {
                stored: true,
                ..queued
            };
            let accepted = self.dispatch_sent_prompt(&sent, handoff);
            self.settle_news(news, accepted);
            if !accepted {
                self.hold_queued_prompt(queued);
                return;
            }
            self.note_unspawned_prompt(queued);
            // A local slash command can finish synchronously. Drain its next turn iteratively.
            if self.state.borrow().running_turn || !self.queued_dispatch_pending.replace(false) {
                return;
            }
        }
    }

    fn pop_queued_prompt(&self) -> Option<(String, String)> {
        let queued = self.queued_prompts.borrow_mut().pop_back()?;
        Some((queued.item_id, queued.text))
    }

    /// Stores the mode the user chose (`wanted_mode`), so a reopened thread starts from it on
    /// whichever agent it is on, rather than from that agent's profile default: a thread switched
    /// from Claude in Ask to an agent whose profile defaults to Accept edits must not reopen there.
    /// Dangerous permissive mode is never stored: the last safe choice is.
    fn record_chosen_mode(&self) {
        self.emit(Envelope::new(Event::ModeChosen {
            mode: self.safe_mode.get(),
        }));
    }

    fn choose_mode(&self, mode: Mode) {
        if mode != Mode::Bypass {
            self.safe_mode.set(mode);
        }
        let previous = self.wanted_mode.replace(mode);
        if previous != mode {
            self.note_grants_outlive_mode();
        }
        self.record_chosen_mode();
    }

    /// Arms `handoff` for the next prompt and stores it (under the current provider thread), so
    /// a restart before it is delivered re-arms it.
    fn arm_handoff(&self, handoff: Handoff) {
        self.emit(handoff_seeded(&handoff));
        self.handoff_in_flight.set(false);
        *self.pending_handoff.borrow_mut() = Some(handoff);
    }

    fn respond_approval(self: &Rc<Self>, request: &str, decision: Decision) {
        self.respond_approval_with_rule(request, decision, None);
    }

    /// The answer and custom rule acted on with saved rules `saved_rules_on` or not
    /// ([`crate::always_allow::kept_answer`]), and whether the answer was downgraded: a
    /// downgraded "Always allow" is a one-off Allow and drops its custom rule.
    fn answer_under_policy(
        decision: Decision,
        custom_rule: Option<crate::always_allow::Rule>,
        saved_rules_on: bool,
    ) -> (Decision, Option<crate::always_allow::Rule>, bool) {
        let kept = crate::always_allow::kept_answer(decision, saved_rules_on);
        let downgraded = kept != decision;
        let custom_rule = custom_rule.filter(|_| !downgraded);
        (kept, custom_rule, downgraded)
    }

    fn respond_approval_with_rule(
        self: &Rc<Self>,
        request: &str,
        decision: Decision,
        custom_rule: Option<crate::always_allow::Rule>,
    ) {
        // Authority may narrow while a human is reading a card. Re-check at the actual
        // response boundary, including plain Allow; rule validation alone cannot protect it.
        let decision = self.child_answer(request, decision);
        let permission = self.codex_permissions.borrow().get(request).cloned();
        if let Some(card) = permission {
            self.respond_codex_permissions(request, decision, &card, false);
            return;
        }
        if let Some(handle) = self.approval() {
            let keeps_rule = matches!(decision, Decision::AllowForSession | Decision::AllowAlways);
            if keeps_rule {
                let weak = Rc::downgrade(self);
                let answered = request.to_owned();
                handle.respond_with_rule_then(
                    request,
                    decision,
                    custom_rule.clone(),
                    move |decision| {
                        if let Some(inner) = weak.upgrade() {
                            inner.emit(
                                Envelope::new(Event::ApprovalResolved { decision })
                                    .request(answered),
                            );
                        }
                    },
                );
            } else {
                let answered = handle.respond_with_rule(request, decision, custom_rule.clone());
                self.finish_hook_approval(request, decision, answered);
            }
            return; // the hook owns agy's answers; a taken request is no longer pending
        }
        // Automatic Pattern Approvals off: an "Always allow" (a card shown before the switch
        // turned off) is allowed once, keeping nothing. agy's server applies the same itself.
        let (decision, custom_rule, saved_rules_off) = Self::answer_under_policy(
            decision,
            custom_rule,
            crate::always_allow::saved_rules_enabled(),
        );
        // App-owned scope: the rule is recorded here; the agent only ever hears a plain
        // allow (Claude without `updatedPermissions`, Codex `accept`). No rule, no claim of one:
        // the card then says "Allowed".
        let pending = self.pending_rules.borrow_mut().remove(request);
        let plan =
            matches!(decision, Decision::AllowAlways | Decision::AllowForSession).then(|| {
                // Re-checked at keep: a child keeps only a rule its gate lets cover this call,
                // never a custom one (it could reach further than the call).
                if self.child.is_some()
                    && !self.child_may_keep(request, pending.as_ref(), &custom_rule)
                {
                    return Err(CHILD_RULE_REFUSED.to_owned());
                }
                self.plan_rule(pending.as_ref(), decision, custom_rule)
            });
        let decision = match &plan {
            Some(Err(_)) => Decision::Allow,
            _ => decision,
        };
        // Claude and Codex never acknowledge an answer, so once the adapter has accepted it and
        // written it, the card is resolved here; otherwise it would sit at "Allow…" for good.
        // A rule is kept only then: an answer the adapter refuses (the request was denied at its
        // deadline, cancelled or expired) keeps nothing.
        let sent = self.command(Command::Approve {
            request: request.to_owned(),
            decision,
            updated_input: None,
            message: None,
        });
        if !sent {
            return;
        }
        match plan {
            Some(Ok((rule, warnings))) => {
                if let Some(text) = crate::always_allow::kept_with_warnings(&rule, &warnings) {
                    self.emit(Envelope::new(Event::Notice { text }));
                }
                self.keep_rule(rule, decision);
            }
            Some(Err(why)) => self.emit(Envelope::new(Event::Notice {
                text: format!("Allowed once; no rule was kept: {why}."),
            })),
            None if saved_rules_off => self.emit(Envelope::new(Event::Notice {
                text: crate::always_allow::ALLOWED_ONCE_SAVED_RULES_OFF.to_owned(),
            })),
            None => {}
        }
        self.emit(Envelope::new(Event::ApprovalResolved { decision }).request(request));
    }

    fn finish_hook_approval(self: &Rc<Self>, request: &str, decision: Decision, answered: bool) {
        if answered {
            self.emit(Envelope::new(Event::ApprovalResolved { decision }).request(request));
        }
        // Stop turn still stops when a stricter mode denied the call or its hook went away.
        if decision == Decision::Cancel {
            self.command(Command::Interrupt);
        }
    }

    fn set_mode(self: &Rc<Self>, mode: Mode) {
        // A supervised child runs within its fixed task mode, and never in dangerous permissive
        // mode, whoever asks.
        if let Some(child) = &self.child {
            if !mode_within(mode, child.mode) {
                self.emit(Envelope::new(Event::Notice {
                    text: format!(
                        "This delegated agent cannot leave its task's mode ({}); it stays in its \
                         current mode.",
                        mode_label(child.mode)
                    ),
                }));
                let current = self.state.borrow().mode;
                self.emit(Envelope::new(Event::ModeChanged { mode: current }));
                return;
            }
        }
        // Dangerous permissive agy is its hook allowing everything: with no hook, there is
        // nothing to approve through, and the flag is never passed any other way.
        if mode == Mode::Bypass && self.ask_unavailable() {
            self.emit(Envelope::new(Event::Notice {
                text: "Antigravity cannot run in dangerous permissive mode until the approval \
                       hook is installed; it stays in its current mode."
                    .to_owned(),
            }));
            let current = self.state.borrow().mode;
            self.emit(Envelope::new(Event::ModeChanged { mode: current }));
            return;
        }
        if self.ask_unavailable() {
            if mode == Mode::Ask {
                self.choose_mode(mode);
                self.emit(Envelope::new(Event::Notice {
                    text:
                        "Antigravity cannot ask before edits until the approval hook is installed; \
                           it stays in its current mode."
                            .to_owned(),
                }));
                // The picker shows the mode actually running.
                let current = self.state.borrow().mode;
                self.emit(Envelope::new(Event::ModeChanged { mode: current }));
                return;
            } else if mode == Mode::Plan {
                self.emit(Envelope::new(Event::Notice {
                    text: "Antigravity Plan mode refuses shell commands, but may still edit files \
                           on disk without the approval hook installed."
                        .to_owned(),
                }));
            }
        }
        // Suspended: there is no process to tell. The mode is kept for the resume, which starts
        // in it (as any restart does), and the picker shows it now. Never dangerous permissive
        // mode: it is entered on a running agent only, through its own path.
        if self.suspended.get() {
            if mode == Mode::Bypass {
                self.deliver(&Envelope::new(Event::Notice {
                    text: "The agent is paused to save memory; send a message to resume it \
                           before choosing dangerous permissive mode."
                        .to_owned(),
                }));
                let current = self.state.borrow().mode;
                self.deliver(&Envelope::new(Event::ModeChanged { mode: current }));
                return;
            }
            self.choose_mode(mode);
            self.emit(Envelope::new(Event::ModeChanged { mode }));
            return;
        }
        let restarts = self.adapter.borrow().capabilities().mode_change_restarts;
        let running = self.state.borrow().running_turn;
        // Hookless agy's busy change was confirmed by the view. Carry the unfinished task
        // as well as native history, since transport writes may not have reached the agent.
        let handoff =
            if restarts && running && self.approval().is_none() && mode != self.state.borrow().mode
            {
                match self
                    .store
                    .read(|s| s.transcript_messages(&self.thread))
                    .and_then(|r| r)
                {
                    Ok(messages) => Some(build_handoff(
                        &messages,
                        &format!("Antigravity thread {}", self.thread),
                    )),
                    Err(e) => {
                        self.error(format!(
                            "Could not read the unfinished task to continue: {e}"
                        ));
                        return;
                    }
                }
            } else {
                None
            };
        // Failed preflight leaves the chosen and persisted modes exactly as they were.
        self.choose_mode(mode);
        if restarts && mode == self.state.borrow().mode {
            return;
        }
        if restarts && running && self.approval().is_some() {
            self.defer_mode(mode);
            return;
        }
        // A restart that carries the unfinished task forward (the handoff below and its
        // continuation) does not also queue a message still waiting in an unspawned start.
        let accepted = {
            let _carrying = handoff
                .is_some()
                .then(|| ScopedFlag::set(&self.carrying_task));
            self.command(Command::SetMode { mode })
        };
        if accepted {
            if let Some(a) = self.approval() {
                a.set_mode(mode);
            }
            if let Some(handoff) = handoff {
                if handoff.carried > 0 {
                    self.arm_handoff(handoff);
                }
                self.continue_after_switch(Driver::Agy);
            }
        } else {
            // Refused: whatever a stop took over has no continuation to carry it.
            self.settle_carried(false);
        }
    }

    fn defer_mode(&self, mode: Mode) {
        self.deferred_mode.set(Some(mode));
        // State and launch settings follow the effective hook policy, so a reload or model
        // switch cannot restore the old policy. The pending hint describes only the restart.
        self.apply(&Envelope::new(Event::ModeChanged { mode }));
        self.emit(Envelope::new(Event::ModeChangeDeferred {
            requested: mode,
            effective: mode,
        }));
    }

    fn control(self: &Rc<Self>, control: Control) -> String {
        let n = self.ctl_seq.get() + 1;
        self.ctl_seq.set(n);
        let id = format!("ctl-{n}");
        let result = self.adapter.borrow_mut().encode(Command::Control {
            id: id.clone(),
            control,
        });
        match result {
            Ok(actions) => {
                self.execute_with_control(actions, Some(&id));
                self.drain_outbox();
            }
            Err(e) => {
                self.report(&e);
                // The panel is waiting on this id.
                self.emit(
                    Envelope::new(Event::ControlResult {
                        ok: None,
                        error: Some(e.to_string()),
                    })
                    .request(id.clone()),
                );
            }
        }
        id
    }

    // ---- switching ----

    /// What a switch to `to` would cost, said before it happens: nothing for the same agent.
    fn switch_caveats(&self, to: Driver) -> Vec<String> {
        if to == self.adapter.borrow().driver() {
            return Vec::new();
        }
        let mut out = vec![format!(
            "Continues this thread in {} from a summary of it: reasoning and tool state stay \
             behind.",
            driver_label(to)
        )];
        let rules = self.session_rules.borrow().rules.len();
        if rules > 0 {
            out.push(session_rules_end(rules));
        }
        if self.wanted_mode.get() == Mode::Bypass {
            out.push("Dangerous permissive mode ends.".to_owned());
        }
        let can_ask = self.agy_can_ask.borrow().as_ref().is_some_and(|f| f());
        if to == Driver::Agy && !can_ask {
            out.push(format!(
                "Antigravity cannot ask you before acting here: its approval hook is not ready. \
                 {HOOKLESS_STILL}"
            ));
        }
        out
    }

    fn switch(self: &Rc<Self>, driver: Driver, model: Option<String>, effort: Option<String>) {
        let (caps, current_driver, mode, status_model, current_effort, running) = {
            let adapter = self.adapter.borrow();
            let state = self.state.borrow();
            (
                adapter.capabilities().clone(),
                adapter.driver(),
                state.mode,
                self.pending_model
                    .borrow()
                    .as_ref()
                    .map(|p| p.model.clone())
                    .or_else(|| state.model.clone()),
                self.pending_model
                    .borrow()
                    .as_ref()
                    .map(|p| p.effort.clone())
                    .unwrap_or_else(|| state.effort.clone()),
                state.running_turn,
            )
        };
        // No effort asked for on the same agent means "keep it", not "clear it".
        let effort = match (&effort, driver == current_driver) {
            (None, true) => current_effort.clone(),
            _ => effort,
        };
        let workspace = self.open.borrow().cwd.clone();
        let current = SessionState {
            selection: ModelSelection {
                driver: current_driver,
                model: status_model.clone().unwrap_or_default(),
                effort: current_effort,
            },
            mode,
            workspace: workspace.clone(),
            capabilities: caps.clone(),
        };
        let same_driver = driver == current_driver;
        let target_model = match (&model, same_driver) {
            (Some(m), _) => m.clone(),
            (None, true) => status_model.unwrap_or_default(),
            (None, false) => String::new(),
        };
        // The target's own capabilities are not known without its adapter; the policy only
        // reads the live session's.
        let target = SessionState {
            selection: ModelSelection {
                driver,
                model: target_model.clone(),
                effort: effort.clone(),
            },
            mode,
            workspace,
            capabilities: caps.clone(),
        };
        let plan = plan_selection(&caps, &current.selection, &target.selection);
        let transition = decide_transition(Some(&current), &target, true, Some(&plan));
        debug!(?transition, "model switch");
        // A choice made now settles any pending question about a retired model.
        // Persistence follows backend acceptance, never a click or a failed request.
        *self.retired.borrow_mut() = None;
        if same_driver
            && running
            && matches!(
                transition,
                Transition::SwitchModelInSession | Transition::RestartAndResume
            )
        {
            // The picker already confirmed Switch and Continue. A process replacement actually
            // interrupts this turn and expires its questions/approvals before resuming.
            let native = self.state.borrow().native_id.clone();
            // Transport writes are asynchronous: even a known native session might never
            // have received the latest task. Carry bounded context as well as native history.
            let handoff = match self
                .store
                .read(|s| s.transcript_messages(&self.thread))
                .and_then(|r| r)
            {
                Ok(messages) => build_handoff(
                    &messages,
                    &format!("{} thread {}", driver_label(driver), self.thread),
                ),
                Err(e) => {
                    self.error(format!(
                        "Could not read the unfinished task to continue: {e}"
                    ));
                    return;
                }
            };
            {
                let _carrying = ScopedFlag::set(&self.carrying_task);
                self.respawn(&OpenSessionDelta {
                    model: Some(target_model),
                    effort,
                    mode: None,
                    resume: native,
                });
            }
            if handoff.carried > 0 {
                self.arm_handoff(handoff);
            }
            self.continue_after_switch(driver);
            return;
        }
        // Suspended, same agent: no process to tell, and starting one here would skip a
        // delegation root's rebind. The choice is kept for the resume, which launches with it
        // (as any restart does), and shown now. Another agent starts its own process below.
        if self.suspended.get() && same_driver {
            {
                let mut open = self.open.borrow_mut();
                if !target_model.is_empty() {
                    open.model = Some(target_model.clone());
                }
                open.effort.clone_from(&effort);
            }
            self.state.borrow_mut().effort.clone_from(&effort);
            if !target_model.is_empty() {
                *self.selected.borrow_mut() = Some(target_model.clone());
                self.emit(Envelope::new(Event::ModelChanged {
                    model: target_model,
                }));
            }
            return;
        }
        match transition {
            Transition::Reuse => {}
            Transition::SwitchModelInSession => {
                self.ensure_alive();
                if !self.state.borrow().alive {
                    return;
                }
                self.prepare_model_change(target_model.clone(), effort.clone(), false);
                // The plan only picks this when the effort is unchanged, so passing the target's
                // is the same as keeping it.
                if !self.command(Command::SetModel {
                    model: target_model,
                    effort,
                }) {
                    self.reject_model_change();
                }
            }
            Transition::RestartAndResume => {
                self.ensure_alive();
                if !self.state.borrow().alive {
                    return;
                }
                self.prepare_model_change(target_model.clone(), effort.clone(), false);
                let result = self.adapter.borrow_mut().encode(Command::SetModel {
                    model: target_model.clone(),
                    effort: effort.clone(),
                });
                // Bound first: a `Ref` temporary in the match arm would live through `respawn`,
                // which borrows the state mutably.
                let native = self.state.borrow().native_id.clone();
                match result {
                    Ok(actions) => {
                        // An adapter that applies the effort in session (Codex, per turn) asks
                        // for no respawn; the status and the next launch still follow it.
                        self.execute(actions);
                        self.drain_outbox();
                    }
                    Err(AdapterError::Unsupported(_)) => self.respawn(&OpenSessionDelta {
                        model: Some(target_model),
                        effort,
                        mode: None,
                        resume: native,
                    }),
                    Err(e) => {
                        self.reject_model_change();
                        self.report(&e);
                    }
                }
            }
            Transition::CreateWithHandoff => {
                {
                    // Busy, the switch's continuation carries the unfinished task; idle, nothing
                    // does, and a message still waiting is queued again for the new agent.
                    let _carrying = running.then(|| ScopedFlag::set(&self.carrying_task));
                    self.create_with_handoff(driver, model, effort);
                }
                if running {
                    self.continue_after_switch(driver);
                }
            }
            Transition::Reject(reason) => self.error(reason),
        }
        if !running {
            self.dispatch_queued_prompt();
        }
    }

    /// A confirmed busy switch continues the task once on the replacement process. The
    /// provider's native history (or the pending handoff) supplies the original request;
    /// replaying that request could repeat tool actions that already completed.
    ///
    /// Messages the switch took over from a start that never spawned (`carried_unsent`) are
    /// settled here: carried by the continuation when it goes, queued again when it does not.
    fn continue_after_switch(self: &Rc<Self>, driver: Driver) {
        let sent = self.adapter.borrow().driver() == driver && self.state.borrow().alive;
        if sent {
            self.send_prompt_now(super::SWITCH_CONTINUATION);
        }
        self.settle_carried(sent);
    }

    /// Ends the switch's hold on `carried_unsent`: dropped when its continuation went (the
    /// handoff carries them), queued again when it did not (they reached no agent).
    ///
    /// A replacement that can still roll back (it has not started: `rollback` is set) keeps the
    /// hold: its rollback queues them again (the handoff goes with it), and its start lets them
    /// go ([`Self::confirm_carried`]).
    fn settle_carried(&self, continued: bool) {
        if continued && self.rollback.borrow().is_some() {
            return;
        }
        let carried = self.carried_unsent.take();
        if !continued {
            self.keep_unsent_prompts(carried);
        }
    }

    /// The replacement started: the task it carried, held messages included, is its own now.
    fn confirm_carried(&self) {
        self.carried_unsent.borrow_mut().clear();
    }

    fn create_with_handoff(
        self: &Rc<Self>,
        driver: Driver,
        model: Option<String>,
        effort: Option<String>,
    ) {
        let Some(factory) = self.factory.borrow().clone() else {
            self.error("Switching agent is not available in this thread.");
            return;
        };
        // Built from the store before anything is stopped, so a failure leaves the thread as it was.
        let messages = match self
            .store
            .read(|s| s.transcript_messages(&self.thread))
            .and_then(|r| r)
        {
            Ok(m) => m,
            Err(e) => {
                self.error(format!("Could not read the thread to hand it off: {e}"));
                return;
            }
        };
        let from = self.adapter.borrow().driver();
        let handoff = build_handoff(
            &messages,
            &format!("{} thread {}", driver_label(from), self.thread),
        );
        let carried = handoff.carried;

        let launch = match factory(driver) {
            Ok(launch) => launch,
            Err(reason) => {
                self.error(reason);
                return;
            }
        };
        let model = model.or_else(|| launch.default_model.clone());
        let effort = effort.or_else(|| launch.default_effort.clone());
        // The new provider thread comes first: if the store refuses, the old agent keeps running.
        // Queued to the store writer: a refusal there is reported by the writer, not here.
        let new_pt = match self.store.create_provider_thread(
            &self.thread,
            driver_name(driver),
            model.as_deref().unwrap_or("default"),
        ) {
            Ok(p) => p,
            Err(e) => {
                self.error(format!("Could not start the new agent's thread: {e}"));
                return;
            }
        };
        // What this switch replaces, so a new agent that never starts can take the thread back.
        let previous = {
            let state = self.state.borrow();
            Previous {
                driver: from,
                provider_thread: self.provider_thread.borrow().clone(),
                native_id: state.native_id.clone(),
                model: state.model.clone(),
                selected: self.selected.borrow().clone(),
                effort: state.effort.clone(),
            }
        };
        // The old process's closing events still belong to the old provider thread, and its
        // pending approvals expire on the old socket.
        self.stop_current();
        let (new_mode, mode_changed, hookless_agy) = self.install_agent(
            driver,
            new_pt,
            launch,
            Installed {
                model: model.clone(),
                selected: model.clone(),
                effort: effort.clone(),
                native_id: None,
            },
        );
        *self.rollback.borrow_mut() = Some(previous);
        let mut text = if carried == 0 {
            format!("Switched to {}", driver_label(driver))
        } else {
            handoff_notice(&format!("in {}", driver_label(driver)), &handoff)
        };
        // Session approvals belong to the agent they were given to; Always rules carry.
        if from != driver {
            let ended = self.end_session_rules();
            if ended > 0 {
                if !text.ends_with('.') {
                    text.push('.');
                }
                text.push(' ');
                text.push_str(&session_rules_ended(ended));
            }
        }
        if carried > 0 {
            self.arm_handoff(handoff);
        } else {
            self.handoff_in_flight.set(false);
            *self.pending_handoff.borrow_mut() = None;
        }
        self.emit(Envelope::new(Event::Notice { text }));
        if let Some(m) = model {
            self.state.borrow_mut().model = None;
            self.prepare_model_change(m.clone(), effort.clone(), true);
            self.emit(Envelope::new(Event::ModelChangeRequested { model: m }));
        }
        if mode_changed {
            self.emit(Envelope::new(Event::ModeChanged { mode: new_mode }));
        }
        if hookless_agy {
            self.hookless_notice();
        }
        self.start_process();
    }

    /// Makes `driver`, launched by `launch`, the thread's agent on provider thread `pt`, with
    /// `installed`'s model, effort and native session (resumed when there is one). Its process is
    /// not started. Returns the mode it runs in, whether that changed, and whether it is agy
    /// without its hook. The caller has stopped the previous process.
    fn install_agent(
        self: &Rc<Self>,
        driver: Driver,
        pt: String,
        launch: AgentLaunch,
        installed: Installed,
    ) -> (Mode, bool, bool) {
        *self.provider_thread.borrow_mut() = pt;
        *self.adapter.borrow_mut() = launch.adapter;
        self.adapter_epoch.set(self.adapter_epoch.get() + 1);
        *self.launch_env.borrow_mut() = launch.env;
        let (approval, unbound) = match launch.approval {
            Ok(handle) => (Some(handle), None),
            Err(reason) => (None, reason),
        };
        *self.approval.borrow_mut() = approval;
        *self.unbound.borrow_mut() = unbound;
        // The user's own mode, as far as the new agent can honour it (agy without its hook
        // cannot ask). A thread that went through hookless agy gets its Ask back here.
        // Dangerous permissive mode ends with the agent it was entered on.
        if self.wanted_mode.get() == Mode::Bypass {
            self.wanted_mode.set(self.safe_mode.get());
            self.emit(Envelope::new(Event::Notice {
                text: format!(
                    "Dangerous permissive mode has ended: it lasts for one agent session, so {} \
                     starts in your previous mode.",
                    driver_label(driver)
                ),
            }));
        }
        let hookless_agy = driver == Driver::Agy && self.approval().is_none();
        let new_mode = effective_mode(hookless_agy, self.wanted_mode.get());
        let mode_changed = self.state.borrow().mode != new_mode;
        {
            let mut open = self.open.borrow_mut();
            // The agent's own binary and arguments, from its profile.
            open.program = launch.program;
            open.extra_args = launch.extra_args;
            open.model = installed.model.clone();
            open.effort = installed.effort.clone();
            open.new_session_id = (installed.native_id.is_none() && driver == Driver::Claude)
                .then(|| glib::uuid_string_random().to_string());
            open.resume = installed.native_id.clone();
            open.mode = new_mode;
        }
        {
            let mut state = self.state.borrow_mut();
            state.model = installed.model;
            state.effort = installed.effort;
            state.native_id = installed.native_id;
            state.commands.clear();
            state.running_turn = false;
            state.mode = new_mode;
        }
        *self.selected.borrow_mut() = installed.selected;
        self.attach_approval();
        // The user's choice, stored with the agent it now applies to (see record_chosen_mode).
        self.record_chosen_mode();
        (new_mode, mode_changed, hookless_agy)
    }

    /// Takes the thread back to the agent a switch left, because the new one never started
    /// (`reason`). The old provider thread is active again and its own session resumes, with a
    /// fresh launch (and approval socket: the old one closed with its process). The handoff the
    /// new agent was owed is withdrawn: the old agent has the history in its own session.
    fn roll_back(self: &Rc<Self>, previous: Previous, reason: &str) {
        let failed = driver_label(self.adapter.borrow().driver());
        let back = driver_label(previous.driver);
        let factory = self.factory.borrow().clone();
        let launch = match factory.map(|f| f(previous.driver)) {
            Some(Ok(launch)) => launch,
            Some(Err(why)) => {
                self.error(format!(
                    "{failed} did not start: {reason}. {back} could not be restarted either: {why}"
                ));
                return;
            }
            None => {
                self.error(format!("{failed} did not start: {reason}."));
                return;
            }
        };
        if let Err(e) = self
            .store
            .set_active_provider_thread(&self.thread, &previous.provider_thread)
        {
            self.error(format!(
                "{failed} did not start: {reason}. Could not go back to {back}: {e}"
            ));
            return;
        }
        warn!(%failed, %back, %reason, "a switch's new agent never started; rolling back");
        let model = previous.model.clone();
        let (new_mode, mode_changed, hookless_agy) = self.install_agent(
            previous.driver,
            previous.provider_thread,
            launch,
            Installed {
                model: previous.model,
                selected: previous.selected,
                effort: previous.effort,
                native_id: previous.native_id,
            },
        );
        // A seeded handoff with nothing carried withdraws the one stored for the failed agent,
        // so a reopen does not feed it to this one.
        self.handoff_in_flight.set(false);
        *self.pending_handoff.borrow_mut() = None;
        self.emit(handoff_spent());
        self.error(format!("{failed} did not start: {reason}. Back on {back}."));
        if let Some(m) = model {
            self.emit(Envelope::new(Event::ModelChanged { model: m }));
        }
        if mode_changed {
            self.emit(Envelope::new(Event::ModeChanged { mode: new_mode }));
        }
        if hookless_agy {
            self.hookless_notice();
        }
        self.start_process();
        // Messages the failed switch took over reached no agent, and its handoff is gone: the
        // old agent gets them as they were written.
        let carried = self.carried_unsent.take();
        self.keep_unsent_prompts(carried);
    }

    fn status(&self) -> SessionStatus {
        let hookless = self.ask_unavailable();
        let adapter = self.adapter.borrow();
        let state = self.state.borrow();
        let mut capabilities = adapter.capabilities().clone();
        if hookless {
            capabilities.live_approvals = false;
        }
        SessionStatus {
            driver: adapter.driver(),
            model: state.model.clone(),
            pending_model: self
                .pending_model
                .borrow()
                .as_ref()
                .map(|p| p.model.clone()),
            effort: state.effort.clone(),
            mode: state.mode,
            running_turn: state.running_turn,
            alive: state.alive,
            capabilities,
            commands: state.commands.clone(),
        }
    }
}

/// What a cross-agent switch replaced, kept until the new agent reports a session: a new agent
/// that cannot be spawned, or exits before it ever started (signed out, a broken install), takes
/// the thread back to this one rather than stranding it with no agent.
struct Previous {
    driver: Driver,
    provider_thread: String,
    native_id: Option<String>,
    model: Option<String>,
    selected: Option<String>,
    effort: Option<String>,
}

/// The model, effort and native session an agent is installed with (see
/// [`Inner::install_agent`]).
struct Installed {
    model: Option<String>,
    /// The model as last chosen (an alias where the agent has one).
    selected: Option<String>,
    effort: Option<String>,
    /// The native session to resume; `None` starts a new one.
    native_id: Option<String>,
}

/// A handoff ready to ride on the next prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Handoff {
    /// The fenced, redacted history (see `agent_core::handoff_budget`).
    pub summary: String,
    /// The random marker its fences use; the prompt's user marker must use the same one.
    pub fence: String,
    /// Messages carried, of `total` in the thread.
    pub carried: usize,
    pub total: usize,
}

// ---- Codex permission requests and MCP forms (plan 2026-10-11, lane L1) ----
//
// The adapter types each request; the session adds what only it knows: the home folder, the
// sensitive-path list, Codex's own folder, what masking would change. Nothing here keeps or
// matches an app rule.

/// How long the on-disk check of a permission card may take (a hung network mount must not hide
/// the card for ever).
const PERMISSION_CHECK_DEADLINE: Duration = Duration::from_secs(3);
/// Said on a card whose places could not be checked in time (when shown or when answered).
const CHECK_FAILED: &str =
    "Could not check these places on disk in time, so it can only be denied.";
/// Said on a card shown again because what it asks for changed while it waited for an answer.
const CHECK_CHANGED: &str = "This request changed while it was waiting; check it again.";

/// Runs `job` (a disk check) off the main thread, giving up after `deadline`. `None`: it did not
/// finish in time, or panicked: nothing can be vouched for.
async fn run_permission_check<F>(job: F, deadline: Duration) -> Option<codex_policy::Verdict>
where
    F: FnOnce() -> codex_policy::Verdict + Send + 'static,
{
    match glib::future_with_timeout(deadline, gtk4::gio::spawn_blocking(job)).await {
        Ok(Ok(verdict)) => Some(verdict),
        _ => None,
    }
}

/// [`run_permission_check`] of [`codex_policy::judge_check`] with the standard deadline.
async fn check_permissions(check: codex_policy::Check) -> Option<codex_policy::Verdict> {
    run_permission_check(
        move || codex_policy::judge_check(&check),
        PERMISSION_CHECK_DEADLINE,
    )
    .await
}

impl Inner {
    /// What a card is judged against now: its input, the thread's folder, the Codex child's
    /// homes, whether Codex may write the project unasked, and where grants already let it
    /// write. Writability is the sandbox the running (or next) turn was sent, as the adapter
    /// reports it (unknown counts as writable), or Accept edits chosen for later turns, where a
    /// session grant still holds. Grants are this session's and this turn's write grants.
    fn permission_check(&self, input: &serde_json::Value) -> codex_policy::Check {
        let live = self.adapter.borrow().workspace_writable().unwrap_or(true);
        let mut granted = self.codex_grant_paths.borrow().clone();
        granted.extend(self.codex_turn_grant_paths.borrow().iter().cloned());
        codex_policy::Check {
            input: input.clone(),
            workspace: PathBuf::from(self.open.borrow().cwd.clone()),
            homes: codex_policy::child_homes(
                &self.launch_env.borrow(),
                crate::codex_home::process_env(),
            ),
            project_writable: live || self.wanted_mode.get() == Mode::AcceptEdits,
            granted,
        }
    }

    /// Takes a Codex permission card out of the stream while its targets are checked on disk
    /// (off the main thread, with a deadline), then shows it narrowed
    /// ([`Self::show_codex_permissions`]). Returns the envelope to deliver now: anything else
    /// unchanged, or a card with no request id narrowed to Deny and Stop turn (it cannot be
    /// answered, but it must not offer what it could not grant). `None`: held, or a duplicate of
    /// a card already held or shown.
    fn hold_codex_permissions(self: &Rc<Self>, generation: u64, env: Envelope) -> Option<Envelope> {
        let Event::ApprovalRequested { tool, input, .. } = &env.event else {
            return Some(env);
        };
        if tool != agent_core::codex::PERMISSIONS_TOOL {
            return Some(env);
        }
        let Some(request) = env.request.clone() else {
            return Some(Self::deny_only(env, None));
        };
        // A child's inherited authority first: a denial is answered at once, and a forced card
        // grants nothing for the session.
        match self.gate_verdict(tool, input, None) {
            Some(GateVerdict::Deny { reason }) => {
                self.deny_by_gate(
                    Self::deny_only(env, Some(reason.clone())),
                    &request,
                    &reason,
                );
                return None;
            }
            Some(GateVerdict::AskOnly { .. }) => {
                self.ask_only.borrow_mut().insert(request.clone());
            }
            Some(GateVerdict::Defer) | None => {}
        }
        let shown = self
            .codex_permissions
            .borrow()
            .values()
            .any(|c| c.wire == request);
        if shown || !self.held_permissions.borrow_mut().insert(request.clone()) {
            return None;
        }
        let check = self.permission_check(input);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let verdict = check_permissions(check).await;
            let Some(inner) = weak.upgrade() else { return };
            // Answered by the agent, expired or replaced while it was checked: nothing to show.
            if inner.generation.get() != generation
                || !inner.held_permissions.borrow_mut().remove(&request)
            {
                return;
            }
            let shown = inner.show_codex_permissions(&env, &request, verdict, None);
            inner.emit(shown);
            inner.auto_codex_permissions(&request);
        });
        None
    }

    /// Bypass grants only what the checked card can show, for this turn, through the same
    /// answer-time filesystem recheck as an explicit Allow. Deny-only cards still ask.
    fn auto_codex_permissions(self: &Rc<Self>, shown: &str) {
        if !self.codex_permission_bypass() {
            return;
        }
        let card = self.codex_permissions.borrow().get(shown).cloned();
        if let Some(card) = card.filter(|c| c.allow) {
            if card.round >= 3 {
                self.emit(Envelope::new(Event::Notice {
                    text: "This permission request changed repeatedly; review its latest card \
                           and answer it explicitly."
                        .to_owned(),
                }));
                return;
            }
            self.respond_codex_permissions(shown, Decision::Allow, &card, true);
        }
    }

    fn codex_permission_bypass(&self) -> bool {
        self.child.is_none()
            && self.wanted_mode.get() == Mode::Bypass
            && self.state.borrow().mode == Mode::Bypass
    }

    /// `env` with only Deny and Stop turn offered, marked unrenderable, with `warning`.
    fn deny_only(mut env: Envelope, warning: Option<String>) -> Envelope {
        if let Event::ApprovalRequested { input, options, .. } = &mut env.event {
            options.retain(|d| matches!(d, Decision::Deny | Decision::Cancel));
            if let Some(fields) = input.as_object_mut() {
                fields.insert("renderable".to_owned(), false.into());
                fields.insert("session_allowed".to_owned(), false.into());
                if let Some(text) = warning {
                    fields.insert("warning".to_owned(), text.into());
                }
            }
        }
        env
    }

    /// The card to show for `original` (the adapter's envelope) under the id `shown`, narrowed
    /// by `verdict` (`None`: not checked in time, so Deny and Stop turn only), with `note` before
    /// its warning; records what its answer may be. A card that masking would change (it shows
    /// a secret) can only be denied: what is granted must be what is shown. Where an entry
    /// really leads elsewhere, its target gains `"resolved"`.
    fn show_codex_permissions(
        &self,
        original: &Envelope,
        shown: &str,
        verdict: Option<codex_policy::Verdict>,
        note: Option<&str>,
    ) -> Envelope {
        let wire = original.request.clone().unwrap_or_default();
        let mut env = original.clone();
        env.request = Some(shown.to_owned());
        let masked = crate::approval_server::masked_for_view(original.clone());
        let shown_exactly = matches!((&masked.event, &original.event), (
            Event::ApprovalRequested { title: mt, input: mi, .. },
            Event::ApprovalRequested { title, input, .. },
        ) if mt == title && mi == input);
        let renderable = matches!(&original.event, Event::ApprovalRequested { input, options, .. }
            if input.get("renderable") == Some(&serde_json::Value::Bool(true))
                && options.contains(&Decision::Allow));
        let summary = match &original.event {
            Event::ApprovalRequested { input, .. } => input
                .get("summary")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("what it asked for")
                .to_owned(),
            _ => String::new(),
        };
        let with_note = |warning: Option<String>| match (note, warning) {
            (Some(note), Some(w)) => Some(format!("{note} {w}")),
            (Some(note), None) => Some(note.to_owned()),
            (None, w) => w,
        };
        let refused = if !renderable {
            Some(None)
        } else if !shown_exactly {
            Some(Some(
                "Part of this request looks like a secret, so agent-terminal cannot show it \
                 exactly; it can only be denied."
                    .to_owned(),
            ))
        } else if verdict.is_none() {
            Some(Some(CHECK_FAILED.to_owned()))
        } else {
            None
        };
        let (env, card) = match (refused, verdict) {
            (refused @ Some(_), _) | (refused @ None, None) => {
                let warning = with_note(refused.flatten());
                let card = PermissionCard {
                    wire,
                    original: original.clone(),
                    allow: false,
                    session_allowed: false,
                    warning: warning.clone(),
                    summary,
                    verdict: codex_policy::Verdict::default(),
                    checking: false,
                    round: 0,
                };
                (Self::deny_only(env, warning), card)
            }
            (None, Some(mut verdict)) => {
                // A card a child's gate forced grants nothing for the session.
                if self.ask_only.borrow().contains(&wire) {
                    verdict.session_allowed = false;
                }
                let warning = with_note(verdict.warning.clone());
                if let Event::ApprovalRequested { input, options, .. } = &mut env.event {
                    options.retain(|d| match d {
                        Decision::AllowForSession => verdict.session_allowed,
                        Decision::AllowAlways => false,
                        _ => true,
                    });
                    if let Some(fields) = input.as_object_mut() {
                        fields.insert("session_allowed".to_owned(), verdict.session_allowed.into());
                        if let Some(text) = &warning {
                            fields.insert("warning".to_owned(), text.clone().into());
                        }
                        if let Some(entries) = fields
                            .get_mut("entries")
                            .and_then(serde_json::Value::as_array_mut)
                        {
                            for (entry, resolved) in entries.iter_mut().zip(&verdict.resolved) {
                                if let (Some(target), Some(resolved)) = (
                                    entry
                                        .get_mut("target")
                                        .and_then(serde_json::Value::as_object_mut),
                                    resolved,
                                ) {
                                    target.insert("resolved".to_owned(), resolved.clone().into());
                                }
                            }
                        }
                    }
                }
                let card = PermissionCard {
                    wire,
                    original: original.clone(),
                    allow: true,
                    session_allowed: verdict.session_allowed,
                    warning,
                    summary,
                    verdict,
                    checking: false,
                    round: 0,
                };
                (env, card)
            }
        };
        self.codex_permissions
            .borrow_mut()
            .insert(shown.to_owned(), card);
        env
    }

    /// Answers a Codex permission card shown as `shown`. Refused, with a notice and nothing
    /// sent: Always allow (never offered), Allow for a card that can only be denied, Allow for
    /// session where it was withheld. An Allow is checked on disk again first; if anything a
    /// grant depends on moved, nothing is sent and the card is shown again with the new verdict.
    fn respond_codex_permissions(
        self: &Rc<Self>,
        shown: &str,
        decision: Decision,
        card: &PermissionCard,
        automatic: bool,
    ) {
        let refusal = match decision {
            Decision::AllowAlways => Some(
                "Codex permission requests cannot be always allowed; choose Allow or Deny."
                    .to_owned(),
            ),
            Decision::Allow | Decision::AllowForSession if !card.allow => Some(
                "This permission request can only be denied: agent-terminal could not show it \
                 exactly or check it."
                    .to_owned(),
            ),
            Decision::AllowForSession if !card.session_allowed => Some(format!(
                "Allow for session is not available for this request. {}",
                card.warning
                    .as_deref()
                    .unwrap_or("Choose Allow to grant it for this turn, or Deny.")
            )),
            _ => None,
        };
        if let Some(text) = refusal {
            self.emit(Envelope::new(Event::Notice { text }));
            return;
        }
        if !matches!(decision, Decision::Allow | Decision::AllowForSession) {
            self.send_codex_permissions(shown, decision, card);
            return;
        }
        if card.checking {
            return; // the same answer is already being checked
        }
        if let Some(open) = self.codex_permissions.borrow_mut().get_mut(shown) {
            open.checking = true;
        }
        let input = match &card.original.event {
            Event::ApprovalRequested { input, .. } => input.clone(),
            _ => serde_json::Value::Null,
        };
        let check = self.permission_check(&input);
        let generation = self.generation.get();
        let shown = shown.to_owned();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let verdict = check_permissions(check).await;
            let Some(inner) = weak.upgrade() else { return };
            let card = inner.codex_permissions.borrow().get(&shown).cloned();
            let Some(card) = card.filter(|_| inner.generation.get() == generation) else {
                return; // expired or answered meanwhile
            };
            if automatic && !inner.codex_permission_bypass() {
                if let Some(open) = inner.codex_permissions.borrow_mut().get_mut(&shown) {
                    open.checking = false;
                }
                return;
            }
            if verdict.as_ref() == Some(&card.verdict) {
                inner.send_codex_permissions(&shown, decision, &card);
                return;
            }
            // Something moved: nothing is sent; the card is shown again, as it is now.
            let round = card.round + 1;
            let again = format!("{}~{round}", card.wire);
            inner.codex_permissions.borrow_mut().remove(&shown);
            inner.emit(Envelope::new(Event::ApprovalExpired).request(shown.as_str()));
            inner
                .codex_shown
                .borrow_mut()
                .insert(card.wire.clone(), again.clone());
            let env = inner.show_codex_permissions(
                &card.original,
                &again,
                verdict.clone(),
                verdict.is_some().then_some(CHECK_CHANGED),
            );
            if let Some(next) = inner.codex_permissions.borrow_mut().get_mut(&again) {
                next.round = round;
            }
            inner.emit(env);
            if automatic {
                inner.auto_codex_permissions(&again);
            }
        });
    }

    /// Sends a checked answer: the grant names literal paths by where they really lead (the
    /// card showed both). A session grant is said in a notice and remembered until the Codex
    /// process ends, with where it lets Codex write.
    fn send_codex_permissions(
        self: &Rc<Self>,
        shown: &str,
        decision: Decision,
        card: &PermissionCard,
    ) {
        // The on-disk check awaited above; authority can change while it runs.
        let decision = self.child_answer(shown, decision);
        let allow = matches!(decision, Decision::Allow | Decision::AllowForSession);
        let renames = (allow && !card.verdict.renames.is_empty()).then(|| {
            serde_json::Value::Object(
                card.verdict
                    .renames
                    .iter()
                    .map(|(from, to)| (from.clone(), serde_json::Value::String(to.clone())))
                    .collect(),
            )
        });
        let sent = self.command(Command::Approve {
            request: card.wire.clone(),
            decision,
            updated_input: renames,
            message: None,
        });
        if !sent {
            if let Some(open) = self.codex_permissions.borrow_mut().get_mut(shown) {
                open.checking = false;
            }
            return;
        }
        self.codex_permissions.borrow_mut().remove(shown);
        self.codex_shown.borrow_mut().remove(&card.wire);
        if decision == Decision::Allow {
            // A later session grant beneath this turn's grant could be renamed through it.
            self.codex_turn_grant_paths
                .borrow_mut()
                .extend(card.verdict.writes.iter().cloned());
        }
        if decision == Decision::AllowForSession {
            self.codex_grants.borrow_mut().push(card.summary.clone());
            self.codex_grant_paths
                .borrow_mut()
                .extend(card.verdict.writes.iter().cloned());
            self.emit(Envelope::new(Event::Notice {
                text: format!(
                    "Codex has extra access for this session: {}. It lasts until this thread's \
                     Codex session restarts.",
                    card.summary
                ),
            }));
        }
        self.emit(Envelope::new(Event::ApprovalResolved { decision }).request(shown));
    }

    /// The id the view knows a Codex permission card by, for an envelope that names the
    /// adapter's request (the card may have been shown again under a new id).
    fn route_codex_permission(&self, mut env: Envelope) -> Envelope {
        if matches!(
            env.event,
            Event::ApprovalExpired | Event::ApprovalResolved { .. }
        ) {
            if let Some(shown) = env
                .request
                .as_ref()
                .and_then(|wire| self.codex_shown.borrow().get(wire).cloned())
            {
                env.request = Some(shown);
            }
        }
        env
    }

    /// A mode change does not take back what Codex was allowed for its session: say so.
    fn note_grants_outlive_mode(&self) {
        if self.adapter.borrow().driver() != Driver::Codex {
            return;
        }
        let grants = self.codex_grants.borrow().clone();
        if grants.is_empty() {
            return;
        }
        self.emit(Envelope::new(Event::Notice {
            text: format!(
                "The extra access Codex was allowed for this session still applies in the new \
                 mode until this thread's Codex session restarts: {}.",
                grants.join("; ")
            ),
        }));
    }

    /// Declines, before it is shown, a Codex form whose link or field names masking would change
    /// (they carry what looks like a secret: the card could not show what is answered). Returns
    /// the envelope to deliver, or `None` when it was declined (the notice is out).
    fn gate_form(self: &Rc<Self>, env: Envelope) -> Option<Envelope> {
        let Event::FormRequested { server, form, .. } = &env.event else {
            return Some(env);
        };
        let Some(request) = env.request.clone() else {
            return Some(env);
        };
        let masked = crate::approval_server::masked_for_view(env.clone());
        let Event::FormRequested {
            form: masked_form, ..
        } = &masked.event
        else {
            return Some(env);
        };
        let text = match (form, masked_form) {
            (agent_core::form::Form::Link { url }, agent_core::form::Form::Link { url: shown })
                if url != shown =>
            {
                agent_core::codex::refused_link_notice(
                    server,
                    "asked you to open a link that carries what looks like a secret",
                    shown,
                )
            }
            (
                agent_core::form::Form::Fields { fields },
                agent_core::form::Form::Fields { fields: shown },
            ) if form_keys(fields) != form_keys(shown) => format!(
                "The {server} tool server sent a form whose field names or choices look like \
                 secrets; it was declined."
            ),
            _ => return Some(env),
        };
        let server = server.clone();
        info!(server = %server, "declining a form masking would change");
        self.command(Command::SubmitForm {
            request,
            action: agent_core::form::FormAction::Decline,
            content: None,
        });
        self.emit(Envelope::new(Event::Notice { text }));
        None
    }
}

// ---- delegation: hooks and a supervised child's inherited authority ----

/// Said when a supervised child's answer would keep a rule its gate does not allow.
const CHILD_RULE_REFUSED: &str = "a delegated agent keeps rules only for files in its own worktree";

/// A mode as the picker names it, for notices.
fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Ask => "Ask before edits",
        Mode::AcceptEdits => "Accept edits",
        Mode::Plan => "Plan",
        Mode::Bypass => "Dangerous permissive",
    }
}

impl Inner {
    /// Revalidates a pending grant without retaining any borrow across the live gate callback.
    fn child_answer(&self, request: &str, decision: Decision) -> Decision {
        if self.child.is_none()
            || !matches!(
                decision,
                Decision::Allow | Decision::AllowForSession | Decision::AllowAlways
            )
        {
            return decision;
        }
        let input = self.approval_inputs.borrow().get(request).cloned();
        let Some((tool, input)) = input else {
            return Decision::Deny;
        };
        let cwd = input.get("cwd").and_then(serde_json::Value::as_str);
        match self.gate_verdict(&tool, &input, cwd) {
            Some(GateVerdict::Deny { .. }) => Decision::Deny,
            Some(GateVerdict::AskOnly { .. }) => {
                self.ask_only.borrow_mut().insert(request.to_owned());
                Decision::Allow
            }
            Some(GateVerdict::Defer) | None => decision,
        }
    }

    /// Shows an adapter envelope to the delegation hooks, before masking and persistence.
    fn observe(&self, env: &Envelope) {
        if let Some(hooks) = &self.hooks {
            hooks.observed(ProcessGeneration(self.process_generation.get()), env);
        }
    }

    /// A supervised child's gate verdict on a tool call; `None` for a root.
    fn gate_verdict(
        &self,
        tool: &str,
        input: &serde_json::Value,
        cwd: Option<&str>,
    ) -> Option<GateVerdict> {
        self.child
            .as_ref()
            .map(|child| child.gate.judge(tool, input, cwd))
    }

    /// Whether a session or Always rule may match `action` here: always in a root; in a
    /// child, only where its gate says.
    fn rule_may_apply(&self, action: &crate::always_allow::Action, cwd: Option<&str>) -> bool {
        self.child
            .as_ref()
            .is_none_or(|child| child.gate.rule_may_apply(action, cwd))
    }

    /// Whether a child may keep a rule for the request `request` (its gate allowed rules on the
    /// call, the card was not forced, and no custom rule replaces the call's own).
    fn child_may_keep(
        &self,
        request: &str,
        pending: Option<&PendingRule>,
        custom: &Option<crate::always_allow::Rule>,
    ) -> bool {
        custom.is_none()
            && !self.ask_only.borrow().contains(request)
            && pending.is_some_and(|(action, cwd, _)| self.rule_may_apply(action, cwd.as_deref()))
    }

    /// The child's inherited authority on a Claude or Codex approval, before every auto-allow
    /// path (Bypass, Accept edits, session and Always rules): a denial is answered at once and
    /// its card shown resolved (`None` returned); a forced card (`AskOnly`) goes on with no
    /// grant to offer. A root's approvals pass unchanged.
    fn gate_child_approval(self: &Rc<Self>, env: Envelope) -> Option<Envelope> {
        if self.child.is_none() || self.adapter.borrow().driver() == Driver::Agy {
            return Some(env); // agy's socket judges its own calls
        }
        let Event::ApprovalRequested { tool, input, .. } = &env.event else {
            return Some(env);
        };
        if tool == agent_core::codex::PERMISSIONS_TOOL {
            return Some(env); // judged by `hold_codex_permissions`
        }
        let Some(request) = env.request.clone() else {
            return Some(env);
        };
        let cwd = input
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        match self.gate_verdict(tool, input, cwd.as_deref()) {
            Some(GateVerdict::Deny { reason }) => {
                self.deny_by_gate(env, &request, &reason);
                None
            }
            Some(GateVerdict::AskOnly { .. }) => {
                self.ask_only.borrow_mut().insert(request);
                Some(env)
            }
            Some(GateVerdict::Defer) | None => Some(env),
        }
    }

    /// Shows `card` and answers its request Deny at once, saying why.
    fn deny_by_gate(self: &Rc<Self>, card: Envelope, request: &str, reason: &str) {
        info!("a delegated agent's call was denied by its inherited authority");
        let mut card = card;
        if let Event::ApprovalRequested { options, .. } = &mut card.event {
            options.retain(|d| matches!(d, Decision::Deny | Decision::Cancel));
        }
        self.emit(card);
        let denied = self.command(Command::Approve {
            request: request.to_owned(),
            decision: Decision::Deny,
            updated_input: None,
            message: Some(format!("Denied by agent-terminal: {reason}")),
        });
        if denied {
            self.emit(
                Envelope::new(Event::ApprovalResolved {
                    decision: Decision::Deny,
                })
                .request(request),
            );
        }
        self.emit(Envelope::new(Event::Notice {
            text: format!("Denied without asking: {reason}."),
        }));
    }
}

/// The names and choice values a form's answer is keyed on.
fn form_keys(fields: &[agent_core::form::FormField]) -> Vec<String> {
    fields
        .iter()
        .flat_map(|f| {
            let values = match &f.input {
                agent_core::form::FieldInput::Choice { options, .. } => {
                    options.iter().map(|o| o.value.clone()).collect()
                }
                _ => Vec::new(),
            };
            std::iter::once(f.name.clone()).chain(values)
        })
        .collect()
}

/// The handoff a thread still owes its current agent, from its stored history: the latest
/// `HandoffSeeded` not yet delivered. `None` when there is none.
///
/// Delivery is the stored acknowledgement ([`handoff_spent`]: a `HandoffSeeded` carrying
/// nothing), written once the turn that carried the handoff completed; it also withdraws one a
/// switch rolled back. A completed turn alone proves nothing: Claude's prompt-less init turn
/// completes before any prompt carried it. Nor does a native session id (agy reports one at
/// spawn), nor the prompt having been written (the agent may have died reading it). Only a seed
/// written before acknowledgements existed (`acknowledged: false`) is still spent by any
/// completed turn after it, as it always was.
pub(crate) fn undelivered_handoff<'a>(
    events: impl IntoIterator<Item = &'a Envelope>,
) -> Option<Handoff> {
    let mut pending = None;
    let mut legacy = false;
    for env in events {
        match &env.event {
            Event::HandoffSeeded { carried: 0, .. } => pending = None,
            Event::HandoffSeeded {
                summary,
                fence,
                carried,
                total,
                acknowledged,
            } => {
                pending = Some(Handoff {
                    summary: summary.clone(),
                    fence: fence.clone(),
                    carried: *carried,
                    total: *total,
                });
                legacy = !acknowledged;
            }
            Event::TurnCompleted {
                state: TurnState::Completed,
                ..
            } if legacy => pending = None,
            _ => {}
        }
    }
    pending
}

/// Where the prompt carrying the handoff is, while it is in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandoffTurn {
    /// Sent (or a turn ended that was not its own): no turn has started since.
    Waiting,
    /// The first turn to start after it was sent, and whether that turn produced a top-level
    /// item (an answer, a tool call; never a background sub-agent's).
    Open { answered: bool },
}

/// What a turn ending means for the handoff a prompt carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandoffTurnEnd {
    /// No handoff was in flight.
    NotCarried,
    /// Not the turn that carried it: it stays in flight for the next turn to start.
    StillInFlight,
    /// The next prompt carries it again.
    Undelivered,
    /// The agent had it. Acknowledged ([`handoff_spent`]).
    Delivered,
}

/// [`HandoffTurnEnd`] for a turn ending in `state`, with a handoff `in_flight` whose prompt is
/// `turn`; `from_task` when the agent ran the turn itself (Claude's task-notification turn).
///
/// The correlation: the first turn to start after the prompt went out is the one that reads it,
/// and spends it only when it completes with a top-level item. A turn already open when it went
/// out never spends it: Claude may fold the line into that turn or deliver it to the next one
/// (best effort near a turn's end), and its stream gives no evidence of which (no echo of the
/// user line), so that turn's end leaves it in flight. Dispatch is not held until a startup turn ends: a
/// startup turn may wait for the prompt to end (it would never send), and a real Claude may run
/// none (the W0 recording opens its first turn only after the prompt). Every case the stream
/// cannot tie to our prompt
/// keeps the handoff, deliberately preferring to send it again (a duplicate costs tokens) over
/// spending it wrongly (the agent would lose the context):
/// - no turn started since it was sent (a turn already open then, or a stray `result`): a
///   completed one leaves it in flight, a failed one sends it again;
/// - a completed turn with no top-level item (Claude's prompt-less startup turn): not ours,
///   still in flight for the next turn to start;
/// - a turn the agent ran itself (a task notification), which may have taken our prompt in or
///   not: sent again with the next prompt;
/// - failed or interrupted: sent again with the next prompt.
fn handoff_turn_end(
    in_flight: bool,
    turn: HandoffTurn,
    state: TurnState,
    from_task: bool,
) -> HandoffTurnEnd {
    if !in_flight {
        return HandoffTurnEnd::NotCarried;
    }
    match (turn, state, from_task) {
        (HandoffTurn::Waiting, TurnState::Completed, _) => HandoffTurnEnd::StillInFlight,
        (HandoffTurn::Waiting, _, _) => HandoffTurnEnd::Undelivered,
        (HandoffTurn::Open { .. }, _, true) => HandoffTurnEnd::Undelivered,
        (HandoffTurn::Open { answered: true }, TurnState::Completed, false) => {
            HandoffTurnEnd::Delivered
        }
        (HandoffTurn::Open { answered: false }, TurnState::Completed, false) => {
            HandoffTurnEnd::StillInFlight
        }
        (HandoffTurn::Open { .. }, _, false) => HandoffTurnEnd::Undelivered,
    }
}

/// The stored `HandoffSeeded` for `handoff`: spent only by its acknowledgement
/// ([`handoff_spent`]).
fn handoff_seeded(handoff: &Handoff) -> Envelope {
    Envelope::new(Event::HandoffSeeded {
        summary: handoff.summary.clone(),
        fence: handoff.fence.clone(),
        carried: handoff.carried,
        total: handoff.total,
        acknowledged: true,
    })
}

/// A `HandoffSeeded` carrying nothing: the earlier one was delivered (the turn that carried it
/// completed), or withdrawn (the switch it was for rolled back). Either way, nothing is owed.
fn handoff_spent() -> Envelope {
    Envelope::new(Event::HandoffSeeded {
        summary: String::new(),
        fence: String::new(),
        carried: 0,
        total: 0,
        acknowledged: true,
    })
}

/// The mode the user last chose in a thread, from its stored history (the latest `ModeChosen`).
pub(crate) fn chosen_mode<'a>(events: impl IntoIterator<Item = &'a Envelope>) -> Option<Mode> {
    events
        .into_iter()
        .filter_map(|env| match env.event {
            Event::ModeChosen { mode } => Some(mode),
            _ => None,
        })
        .last()
}

/// Whether a Codex patch approval only touches files Accept edits may write unasked: it names at
/// least one change, and every target ([`crate::always_allow::codex_change_targets`]: each path
/// and rename target) is inside `workspace`, with no `..` or symlink on the way, and not
/// sensitive (agy's containment check). A missing or non-text target fails closed.
fn codex_edits_contained(input: &serde_json::Value, workspace: &Path, home: Option<&Path>) -> bool {
    crate::always_allow::codex_change_targets(input).is_some_and(|targets| {
        targets
            .iter()
            .all(|p| crate::approval_server::path_auto_matchable(Path::new(p), workspace, home))
    })
}

/// What the switch notice adds when session approvals end.
fn session_rules_ended(n: usize) -> String {
    format!("Session approvals ({n}) ended.")
}

/// [`session_rules_ended`], said before the switch.
fn session_rules_end(n: usize) -> String {
    match n {
        1 => "Your session approval ends.".to_owned(),
        n => format!("Your {n} session approvals end."),
    }
}

/// What a NEW thread carrying `handoff` (a fork, compact-by-handoff, a continuation) is stored
/// with: the `HandoffSeeded` its session re-arms from history ([`undelivered_handoff`]), then the
/// notice saying what it carried, from `source`. Nothing when it carries nothing. Written with the
/// thread itself, so a page closed before its session starts keeps the handoff.
pub(crate) fn seeded_handoff_events(handoff: &Handoff, source: &str) -> Vec<Envelope> {
    if handoff.carried == 0 {
        return Vec::new();
    }
    vec![
        handoff_seeded(handoff),
        Envelope::new(Event::Notice {
            text: handoff_notice(&format!("from {source}"), handoff),
        }),
    ]
}

/// What a thread switched in place to `driver` before any session ran it is stored with: the
/// `HandoffSeeded` the new agent's session re-arms from history, then the same notice a live
/// switch shows. Carrying nothing, only "Switched to <agent>".
pub(crate) fn switched_handoff_events(handoff: &Handoff, driver: Driver) -> Vec<Envelope> {
    let label = driver_label(driver);
    if handoff.carried == 0 {
        return vec![Envelope::new(Event::Notice {
            text: format!("Switched to {label}"),
        })];
    }
    vec![
        handoff_seeded(handoff),
        Envelope::new(Event::Notice {
            text: handoff_notice(&format!("in {label}"), handoff),
        }),
    ]
}

/// What a handoff carried, said plainly: all of the thread, or how much of it (the budget keeps
/// the first request and the most recent work), and what never travels. `place` reads after
/// "Continuing": "in Codex", "from “title”".
fn handoff_notice(place: &str, handoff: &Handoff) -> String {
    const NEVER: &str =
        "Reasoning and tool state are not carried, and long tool output is shortened.";
    if handoff.total == 1 {
        format!("Continuing {place} with the 1 earlier message. {NEVER}")
    } else if handoff.carried >= handoff.total {
        format!(
            "Continuing {place} with all {} earlier messages. {NEVER}",
            handoff.total
        )
    } else {
        format!(
            "Continuing {place} with {} of {} earlier messages (the first request and the most \
             recent work). {NEVER}",
            handoff.carried, handoff.total
        )
    }
}

/// A fresh handoff fence: a random UUID without its dashes (32 hex digits), so carried text cannot
/// guess it and forge a fence or the user marker.
fn new_fence() -> String {
    glib::uuid_string_random().replace('-', "")
}

/// The budgeted, fenced and redacted handoff of a thread's history. Tool items are carried as
/// tool output (capped, untrusted); items with no text are skipped, and an item still open (its
/// process died with it) is described as interrupted.
pub(crate) fn build_handoff(messages: &[TranscriptMessage], source: &str) -> Handoff {
    let history: Vec<HistoricalMessage> = messages
        .iter()
        .filter(|m| !m.text.trim().is_empty())
        .map(|m| HistoricalMessage {
            role: match m.role.as_str() {
                "user" => Role::User,
                "tool" => Role::Tool,
                _ => Role::Assistant,
            },
            kind: m.kind.clone(),
            text: m.text.clone(),
            item_id: m.item_id.clone(),
            status: if m.status == "open" {
                "interrupted".to_owned()
            } else {
                m.status.clone()
            },
        })
        .collect();
    // A fresh session has used nothing; the window is unknown, so the budget is the default cap.
    let budget = handoff_budget(
        DEFAULT_HANDOFF_TOKEN_CAP,
        HANDOFF_PROMPT_ALLOWANCE,
        &[],
        0,
        None,
        None,
        None,
    );
    let coverage = handoff_coverage(
        source,
        history.first().map(|m| m.item_id.as_str()),
        history.last().map(|m| m.item_id.as_str()),
    );
    let fence = new_fence();
    let selection = select_history(&history, &coverage, budget, &fence);
    Handoff {
        summary: render_history(&selection.messages, &selection.context, &fence),
        carried: selection.messages.len(),
        total: history.len(),
        fence,
    }
}

impl ChatBackend for ChatSession {
    fn send_prompt(&self, text: &str) {
        self.inner.user_spoke();
        self.inner.send_prompt(text);
    }

    fn queue_prompt(&self, text: &str) {
        self.inner.user_spoke();
        self.inner.queue_prompt(text);
    }

    fn steer_prompt(&self, text: &str) {
        self.inner.user_spoke();
        self.inner.steer_prompt(text);
    }

    fn interrupt(&self) {
        self.inner.command(Command::Interrupt);
    }

    fn respond_approval(&self, request: &str, decision: Decision) {
        self.inner.respond_approval(request, decision);
    }

    fn respond_approval_with_rule(
        &self,
        request: &str,
        decision: Decision,
        custom_rule: Option<crate::always_allow::Rule>,
    ) {
        self.inner
            .respond_approval_with_rule(request, decision, custom_rule);
    }

    fn answer_questions(&self, request: &str, answers: serde_json::Value) {
        // No agent acknowledges an answer: once the adapter has accepted and written it,
        // the card is resolved here, or it would sit at "Sending…" for good.
        if self.inner.command(Command::Answer {
            request: request.to_owned(),
            answers,
        }) {
            self.inner
                .emit(Envelope::new(Event::QuestionResolved { answered: true }).request(request));
        }
    }

    fn respond_form(
        &self,
        request: &str,
        action: agent_core::form::FormAction,
        content: Option<serde_json::Value>,
    ) -> bool {
        // Like a question: nothing acknowledges the answer, so the card is resolved once the
        // adapter has accepted and written it. A refused answer emits nothing and returns
        // false, and the view reopens the card with a line saying so.
        let accepted = self.inner.command(Command::SubmitForm {
            request: request.to_owned(),
            action,
            content,
        });
        if accepted {
            self.inner
                .emit(Envelope::new(Event::FormResolved { action }).request(request));
        }
        accepted
    }

    fn switch(&self, driver: Driver, model: Option<String>, effort: Option<String>) {
        self.inner.switch(driver, model, effort);
    }

    fn switch_caveats(&self, to: Driver) -> Vec<String> {
        self.inner.switch_caveats(to)
    }

    fn reload_session(&self) {
        self.inner.reload_session();
    }

    fn set_mode(&self, mode: Mode) {
        self.inner.set_mode(mode);
    }

    fn control(&self, control: Control) -> String {
        self.inner.control(control)
    }

    fn status(&self) -> SessionStatus {
        self.inner.status()
    }

    fn pop_queued_prompt(&self) -> Option<(String, String)> {
        self.inner.pop_queued_prompt()
    }
}

/// Rewrites an exit event for a stop the app asked for: the adapter cannot tell that from a
/// crash, but the session can.
fn as_deliberate_stop(env: &mut Envelope) {
    match &mut env.event {
        Event::SessionExited { expected, .. } => *expected = true,
        Event::TurnCompleted { state, error, .. } if *state == TurnState::Failed => {
            *state = TurnState::Interrupted;
            *error = None;
        }
        _ => {}
    }
}

#[cfg(test)]
mod scripted_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{in_loop, pump_until};
    use agent_core::agy::AgyAdapter;
    use agent_core::caps::Capabilities;
    use std::path::Path;
    use std::time::Duration;

    mod delegated_tests;
    mod teardown_tests;

    const FIXTURE: &str = include_str!("../../crates/agent-core/tests/fixtures/agy-edit.ndjson");
    const CONVERSATION: &str = "3af90996-e4fe-44be-8d6c-ca20da039f6f";

    type Seen = Rc<RefCell<Vec<Envelope>>>;

    fn make_sink() -> (EnvelopeSink, Seen) {
        let seen: Seen = Rc::default();
        let s = seen.clone();
        (
            Rc::new(move |e: &Envelope| s.borrow_mut().push(e.clone())),
            seen,
        )
    }

    fn has(seen: &Seen, f: impl Fn(&Event) -> bool) -> bool {
        seen.borrow().iter().any(|e| f(&e.event))
    }

    fn exited_count(seen: &Seen) -> usize {
        seen.borrow()
            .iter()
            .filter(|e| matches!(e.event, Event::SessionExited { .. }))
            .count()
    }

    /// A shell "agent" in `dir`: logs its arguments and the approval env, reads one line, then
    /// replays the recorded agy frames and exits.
    fn fake_agy(dir: &Path) -> OpenSession {
        let frames: Vec<String> = FIXTURE
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["dir"] == "out")
            .map(|v| v["frame"].to_string())
            .collect();
        std::fs::write(dir.join("frames.ndjson"), frames.join("\n") + "\n").expect("frames");
        // The captured init omits a model. When this fake is launched with a selection,
        // report it like the backend does so tests verify adoption rather than a click.
        let selected_frames: Vec<String> = frames
            .iter()
            .map(|line| {
                let mut frame: serde_json::Value = serde_json::from_str(line).expect("frame");
                if frame["event"] == "init" {
                    frame["init"]["model"] = serde_json::json!("gemini-flash");
                }
                frame.to_string()
            })
            .collect();
        std::fs::write(
            dir.join("frames-flash.ndjson"),
            selected_frames.join("\n") + "\n",
        )
        .expect("frames");
        let script = dir.join("agent.sh");
        std::fs::write(
            &script,
            concat!(
                "D=$(dirname \"$0\")\n",
                "echo \"$@\" >> \"$D/args.log\"\n",
                "echo \"${AGENT_TERMINAL_APPROVAL_SOCKET-unset}\" >> \"$D/sock.log\"\n",
                "echo \"${AGENT_TERMINAL_HOOK_BIN-unset}\" >> \"$D/hookbin.log\"\n",
                "FRAMES=frames.ndjson\n",
                "PREV=''\n",
                "for ARG in \"$@\"; do\n",
                "  if [ \"$PREV\" = --model ] && [ \"$ARG\" = gemini-flash ]; then FRAMES=frames-flash.ndjson; fi\n",
                "  PREV=$ARG\n",
                "done\n",
                "read line\n",
                "cat \"$D/$FRAMES\"\n",
            ),
        )
        .expect("script");
        OpenSession {
            program: "/bin/sh".into(),
            extra_args: vec![script.to_string_lossy().into_owned()],
            cwd: dir.to_string_lossy().into_owned(),
            model: None,
            effort: None,
            mode: Mode::Ask,
            resume: None,
            new_session_id: None,
            approval_hook: false,
            delegation: None,
        }
    }

    fn lines(path: std::path::PathBuf) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn fresh(store: &Store, cwd: &str) -> ThreadId {
        store.create_thread(cwd, Some("t")).expect("thread")
    }

    #[test]
    fn replays_an_agy_turn_into_the_sink_and_the_store_and_resumes_after_exit() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, &tmp.path().to_string_lossy());
            let (sink, seen) = make_sink();
            let session = ChatSession::new(
                Box::new(AgyAdapter::new("agy")),
                fake_agy(tmp.path()),
                store.clone(),
                thread.clone(),
                sink,
                None,
            );
            // Without the hook agy cannot ask: Ask runs as Plan, and the user is told.
            assert!(has(
                &seen,
                |e| matches!(e, Event::Notice { text } if text.contains("cannot ask you before acting"))
            ));
            assert_eq!(session.status().mode, Mode::Plan);

            session.send_prompt("add multiply");
            assert!(pump_until(ctx, 15, || exited_count(&seen) == 1), "no exit");

            let first = seen.borrow()[seen.borrow().len().saturating_sub(1)].clone();
            assert!(matches!(
                first.event,
                Event::SessionExited { code: Some(0), .. }
            ));
            assert!(has(&seen, |e| matches!(e, Event::SessionStarted { .. })));
            assert!(has(&seen, |e| matches!(e, Event::TurnCompleted { .. })));
            assert!(has(&seen, |e| matches!(e, Event::ContentDelta { .. })));
            // The user's own message is echoed first.
            assert!(matches!(
                seen.borrow()
                    .iter()
                    .find(|e| matches!(
                        e.event,
                        Event::ItemStarted {
                            kind: ItemKind::UserMessage,
                            ..
                        }
                    ))
                    .map(|e| &e.event),
                Some(Event::ItemStarted { .. })
            ));

            let status = session.status();
            assert!(!status.alive && !status.running_turn);
            let pts = store.provider_threads(&thread).expect("pts");
            assert_eq!(pts.len(), 1);
            assert_eq!(pts[0].native_id.as_deref(), Some(CONVERSATION));
            let stored = store.events(&thread, None, 1000).expect("events");
            assert!(stored
                .iter()
                .any(|(_, e)| matches!(e.event, Event::TurnCompleted { .. })));
            assert!(stored
                .iter()
                .all(|(_, e)| !matches!(e.event, Event::Unknown) || e.raw.is_some()));
            let msgs = store.transcript_messages(&thread).expect("msgs");
            assert_eq!(msgs[0].text, "add multiply");

            // The process is dead: the next prompt restarts it resuming the native session.
            session.send_prompt("again");
            assert!(
                pump_until(ctx, 15, || exited_count(&seen) == 2),
                "no second exit"
            );
            let args = lines(tmp.path().join("args.log"));
            assert_eq!(args.len(), 2, "{args:?}");
            assert!(!args[0].contains("--conversation"), "{args:?}");
            assert!(
                args[1].contains(&format!("--conversation {CONVERSATION}")),
                "{args:?}"
            );
            assert!(args.iter().all(|a| a.contains("--mode plan")), "{args:?}");
            assert!(args
                .iter()
                .all(|a| !a.contains("--dangerously-skip-permissions")));
        });
    }

    #[test]
    fn reloading_restarts_the_process_and_resumes_the_same_provider_conversation() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, &tmp.path().to_string_lossy());
            let (sink, seen) = make_sink();
            let session = ChatSession::new(
                Box::new(AgyAdapter::new("agy")),
                fake_agy(tmp.path()),
                store,
                thread,
                sink,
                None,
            );

            session.send_prompt("remember this conversation");
            assert!(
                pump_until(ctx, 15, || exited_count(&seen) == 1),
                "no first exit"
            );

            session.reload_session();
            assert!(
                pump_until(ctx, 15, || lines(tmp.path().join("args.log")).len() == 2),
                "reloaded process did not start"
            );
            assert!(session.status().alive);
            let args = lines(tmp.path().join("args.log"));
            assert!(
                args[1].contains(&format!("--conversation {CONVERSATION}")),
                "{args:?}"
            );
        });
    }

    /// A Claude "agent" that takes the handshake, reports session `s1` (Claude's init opens a
    /// turn) and ends that turn, then logs every line it is sent for as long as it runs,
    /// completing a turn for each prompt.
    fn idle_claude(dir: &Path) -> OpenSession {
        scripted(
            dir,
            "IFS= read -r handshake\n\
             echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s1\",\"model\":\"m\",\"claude_code_version\":\"2.1.293\"}'\n\
             echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}'\n\
             while IFS= read -r line; do printf '%s\\n' \"$line\" >> \"$D/prompts.log\"; \
             case \"$line\" in *'\"type\":\"user\"'*) \
             echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}';; esac; done",
        )
    }

    /// [`idle_claude`] that answers each prompt as a real Claude does: an `init` opening the
    /// turn, a line of text, then its result.
    fn answering_claude(dir: &Path) -> OpenSession {
        scripted(
            dir,
            "IFS= read -r handshake\n\
             echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s1\",\"model\":\"m\",\"claude_code_version\":\"2.1.293\"}'\n\
             echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}'\n\
             while IFS= read -r line; do printf '%s\\n' \"$line\" >> \"$D/prompts.log\"; \
             case \"$line\" in *'\"type\":\"user\"'*) \
             echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s1\",\"model\":\"m\",\"claude_code_version\":\"2.1.293\"}'; \
             echo '{\"type\":\"assistant\",\"uuid\":\"u1\",\"parent_tool_use_id\":null,\"message\":{\"id\":\"m1\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}'; \
             echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}';; esac; done",
        )
    }

    /// Idle suspension stops the agent process without a word to the view or the store, and
    /// the next prompt starts it again resuming the same native session.
    #[test]
    fn an_idle_session_suspends_silently_and_the_next_prompt_resumes_it() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, &tmp.path().to_string_lossy());
            let (sink, seen) = make_sink();
            let session = ChatSession::new(
                Box::new(agent_core::claude::ClaudeAdapter::new()),
                idle_claude(tmp.path()),
                store.clone(),
                thread.clone(),
                sink,
                None,
            );
            let turns_done = |seen: &Seen| {
                seen.borrow()
                    .iter()
                    .filter(|e| matches!(e.event, Event::TurnCompleted { .. }))
                    .count()
            };
            let life = |starting: bool, resuming: bool, suspended: bool| crate::chat::Lifecycle {
                alive: !suspended,
                starting,
                resuming,
                suspended,
            };
            session.reload_session();
            assert_eq!(
                session.lifecycle(),
                life(true, false, false),
                "a fresh start is starting, not resuming"
            );
            assert!(
                pump_until(ctx, 10, || has(&seen, |e| matches!(
                    e,
                    Event::SessionStarted { native_id, .. } if native_id == "s1"
                )) && turns_done(&seen) == 1),
                "the agent never reported its session and ended its init turn"
            );
            assert_eq!(
                session.lifecycle(),
                life(false, false, false),
                "up once it reported"
            );
            assert_eq!(
                session.inner.suspend_blocker(),
                Some("no conversation yet to resume"),
                "Claude's prompt-less init turn leaves no transcript to resume"
            );
            session.send_prompt("first");
            assert!(pump_until(ctx, 10, || turns_done(&seen) == 2));
            assert!(session.status().alive);
            let before = seen.borrow().len();
            let stored = || {
                session
                    .inner
                    .store
                    .read(|s| s.events(&thread, None, 1000))
                    .expect("read")
                    .expect("events")
                    .into_iter()
                    .map(|(_, e)| e.event)
                    .collect::<Vec<_>>()
            };
            let stored_before = stored();

            assert_eq!(session.inner.suspend_blocker(), None);
            assert!(session.suspend_if_idle(), "an idle session suspends");
            assert!(!session.status().alive);
            assert_eq!(session.lifecycle(), life(false, false, true), "paused");
            assert!(
                !session.suspend_if_idle(),
                "a suspended one has nothing to stop"
            );
            pump_until(ctx, 1, || false);
            assert_eq!(seen.borrow().len(), before, "nothing reached the view");
            assert_eq!(stored(), stored_before, "nothing about it was stored");

            // A mode picked while paused is kept for the resume, not refused for a dead process;
            // dangerous permissive mode is not, and nothing about it is stored.
            session.set_mode(Mode::Plan);
            assert!(has(&seen, |e| matches!(
                e,
                Event::ModeChanged { mode: Mode::Plan }
            )));
            session.set_mode(Mode::Bypass);
            assert_eq!(
                session.status().mode,
                Mode::Plan,
                "Bypass refused while paused"
            );
            // So is a model picked on the same agent, without starting anything.
            session.switch(Driver::Claude, Some("opus".into()), None);
            assert!(has(&seen, |e| matches!(
                e,
                Event::ModelChanged { model } if model == "opus"
            )));
            assert_eq!(
                lines(tmp.path().join("args.log")).len(),
                1,
                "nothing started"
            );
            assert!(!has(&seen, |e| matches!(e, Event::Error { .. })));
            assert!(
                !stored().iter().any(|e| matches!(e, Event::Notice { .. })),
                "the paused notice is not stored"
            );

            assert_eq!(
                session.lifecycle(),
                life(false, false, true),
                "a mode or model picked while paused starts nothing"
            );
            session.send_prompt("after a pause");
            assert_eq!(
                session.lifecycle(),
                life(true, true, false),
                "the message sent while paused resumes it"
            );
            assert!(
                pump_until(ctx, 10, || lines(tmp.path().join("prompts.log"))
                    .iter()
                    .any(|l| l.contains("after a pause"))),
                "the prompt never reached a resumed agent"
            );
            let launches = lines(tmp.path().join("args.log"));
            assert_eq!(launches.len(), 2, "{launches:?}");
            assert!(!launches[0].contains("--resume"), "{launches:?}");
            assert!(launches[1].contains("--resume=s1"), "{launches:?}");
            assert!(
                launches[1].contains("--permission-mode plan"),
                "the resume starts in the mode picked while paused: {launches:?}"
            );
            assert!(
                launches[1].contains("--model opus"),
                "the resume starts with the model picked while paused: {launches:?}"
            );
            assert!(
                pump_until(ctx, 10, || session.lifecycle() == life(false, false, false)),
                "the resumed agent never reported: {:?}",
                session.lifecycle()
            );
        });
    }

    /// An agent counts as up only once its adapter accepted the answer to its handshake: blank,
    /// malformed, foreign or refused replies leave it starting. The lifecycle listener hears the
    /// change even when no envelope carries it (Codex's `initialize` result has none), so no
    /// header or row is left on a stale "Starting…".
    #[test]
    fn an_agent_is_up_only_once_its_adapter_accepts_the_handshake_answer() {
        in_loop(|ctx| {
            type Make = fn() -> Box<dyn Adapter>;
            let claude: Make = || Box::new(agent_core::claude::ClaudeAdapter::new());
            let codex: Make = || Box::new(agent_core::codex::CodexAdapter::new());
            let claude_reply = |subtype: &str| {
                serde_json::json!({"type": "control_response", "response": {
                    "subtype": subtype, "request_id": "init-1"}})
                .to_string()
            };
            let noise = || vec![String::new(), "not json".to_owned()];
            let cases: Vec<(&str, Make, Vec<String>, bool)> = vec![
                ("claude noise", claude, noise(), false),
                (
                    "claude foreign reply",
                    claude,
                    vec![serde_json::json!({"type": "control_response", "response": {
                        "subtype": "success", "request_id": "other-9"}})
                    .to_string()],
                    false,
                ),
                ("claude refused", claude, vec![claude_reply("error")], false),
                (
                    "claude unknown reply",
                    claude,
                    vec![claude_reply("unexpected")],
                    false,
                ),
                (
                    "claude success",
                    claude,
                    vec![claude_reply("success")],
                    true,
                ),
                ("codex noise", codex, noise(), false),
                (
                    "codex refused",
                    codex,
                    vec![r#"{"id":1,"error":{"message":"unsupported client"}}"#.to_owned()],
                    false,
                ),
                (
                    "codex silent success",
                    codex,
                    vec![r#"{"id":1,"result":{}}"#.to_owned()],
                    true,
                ),
            ];
            for (name, make, lines, up) in cases {
                let tmp = tempfile::tempdir().expect("tmp");
                let (session, seen) = recording(ctx, tmp.path(), make());
                let told = Rc::new(RefCell::new(Vec::<crate::chat::Lifecycle>::new()));
                let t = told.clone();
                session.set_lifecycle_listener(Rc::new(move |l| t.borrow_mut().push(l)));
                assert!(
                    session.lifecycle().starting,
                    "{name}: starting before any answer"
                );
                let before = seen.borrow().len();
                for line in &lines {
                    feed(&session, line);
                }
                let life = session.lifecycle();
                assert_eq!(life.alive && !life.starting, up, "{name}: {life:?}");
                let told_up = told.borrow().iter().any(|l| l.alive && !l.starting);
                assert_eq!(told_up, up, "{name}: told {:?}", told.borrow());
                if name == "codex silent success" {
                    assert_eq!(
                        seen.borrow().len(),
                        before,
                        "no envelope carried it, and the listener still heard it"
                    );
                }
                session.shutdown(false);
            }

            // Output the adapter cannot accept, then an exit: never up, and told it ended.
            let tmp = tempfile::tempdir().expect("tmp");
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, &tmp.path().to_string_lossy());
            let (sink, _seen) = make_sink();
            let session = ChatSession::new(
                claude(),
                scripted(
                    tmp.path(),
                    "IFS= read -r handshake\necho ''\necho 'not json'\nexit 3",
                ),
                store,
                thread,
                sink,
                None,
            );
            let told = Rc::new(RefCell::new(Vec::<crate::chat::Lifecycle>::new()));
            let t = told.clone();
            session.set_lifecycle_listener(Rc::new(move |l| t.borrow_mut().push(l)));
            session.reload_session();
            assert!(session.lifecycle().starting, "{:?}", session.lifecycle());
            assert!(
                pump_until(ctx, 10, || !session.lifecycle().alive),
                "the agent never exited"
            );
            assert!(
                !told.borrow().iter().any(|l| l.alive && !l.starting),
                "never up: {:?}",
                told.borrow()
            );
            assert_eq!(
                told.borrow().last().copied(),
                Some(crate::chat::Lifecycle::default()),
                "the exit itself was told"
            );
        });
    }

    /// The suspended same-agent switch with no model named keeps the one the agent reported:
    /// it never resumes on an empty model or on the store's `"default"` sentinel.
    #[test]
    fn a_paused_switch_naming_no_model_keeps_the_reported_one() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, &tmp.path().to_string_lossy());
            let (sink, seen) = make_sink();
            let session = ChatSession::new(
                Box::new(agent_core::claude::ClaudeAdapter::new()),
                idle_claude(tmp.path()),
                store,
                thread,
                sink,
                None,
            );
            session.reload_session();
            assert!(pump_until(ctx, 10, || has(&seen, |e| matches!(
                e,
                Event::TurnCompleted { .. }
            ))));
            session.send_prompt("first");
            assert!(pump_until(ctx, 10, || seen
                .borrow()
                .iter()
                .filter(|e| matches!(e.event, Event::TurnCompleted { .. }))
                .count()
                == 2));
            assert!(session.suspend_if_idle(), "an idle session suspends");
            session.switch(Driver::Claude, None, Some("high".into()));
            assert!(
                !has(&seen, |e| matches!(
                    e,
                    Event::ModelChanged { model } if model.is_empty() || model == "default"
                )),
                "no empty or sentinel model is shown"
            );
            session.send_prompt("after a pause");
            assert!(pump_until(ctx, 10, || lines(tmp.path().join("args.log"))
                .len()
                == 2));
            let launches = lines(tmp.path().join("args.log"));
            assert!(
                launches[1].contains("--model m"),
                "the reported model is kept: {launches:?}"
            );
            assert!(
                !launches[1].contains("--model default"),
                "never the sentinel: {launches:?}"
            );
            assert!(launches[1].contains("--effort high"), "{launches:?}");
        });
    }

    /// A new thread's handoff is stored with the thread ([`seeded_handoff_events`]); its
    /// session re-arms it from history. Claude's prompt-less init turn does not spend it: closed
    /// and reopened after that turn, the thread still owes it, and the first prompt carries it
    /// exactly once. The completed turn that carried it is stored as its delivery.
    #[test]
    fn a_new_threads_stored_handoff_rides_on_the_first_prompt_once() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, &tmp.path().to_string_lossy());
            let handoff = Handoff {
                summary: "earlier context".into(),
                fence: "0123456789abcdef0123456789abcdef".into(),
                carried: 2,
                total: 3,
            };
            for env in seeded_handoff_events(&handoff, "“source”") {
                store.append_event(&thread, None, &env).expect("stored");
            }
            let history = || -> Vec<Envelope> {
                store
                    .events(&thread, None, 1000)
                    .expect("events")
                    .into_iter()
                    .map(|(_, e)| e)
                    .collect()
            };
            let carrying = || {
                history()
                    .iter()
                    .filter(
                        |e| matches!(e.event, Event::HandoffSeeded { carried, .. } if carried > 0),
                    )
                    .count()
            };
            let completed = || {
                history()
                    .iter()
                    .filter(|e| {
                        matches!(
                            e.event,
                            Event::TurnCompleted {
                                state: TurnState::Completed,
                                ..
                            }
                        )
                    })
                    .count()
            };
            assert!(history()
                .iter()
                .any(|e| matches!(&e.event, Event::Notice { text } if text.contains("2 of 3"))));
            let open = || {
                let (sink, _seen) = make_sink();
                let session = ChatSession::new(
                    Box::new(agent_core::claude::ClaudeAdapter::new()),
                    answering_claude(tmp.path()),
                    store.clone(),
                    thread.clone(),
                    sink,
                    None,
                );
                let owed = undelivered_handoff(&history()).expect("owed to the new session");
                session.restore_handoff(owed);
                session
            };

            // Opened, its init turn completes with no prompt, and it is closed again.
            let first = open();
            first.reload_session();
            assert!(
                pump_until(ctx, 10, || completed() == 1),
                "the init turn never completed"
            );
            first.shutdown(true);
            drop(first);
            assert!(
                undelivered_handoff(&history()).is_some(),
                "a prompt-less turn carried nothing"
            );

            // Reopened: the first prompt carries it, once.
            let second = open();
            second.send_prompt("first");
            assert!(pump_until(ctx, 10, || lines(
                tmp.path().join("prompts.log")
            )
            .iter()
            .any(|l| l.contains("first"))));
            let sent = lines(tmp.path().join("prompts.log"));
            assert_eq!(
                sent.iter()
                    .filter(|l| l.contains("earlier context"))
                    .count(),
                1,
                "the handoff rides on the first prompt, once: {sent:?}"
            );
            assert!(sent
                .iter()
                .any(|l| l.contains("first") && l.contains("earlier context")));
            assert_eq!(carrying(), 1, "never stored twice");
            // Its turn completed: stored as delivered, so a later reopen owes nothing.
            assert!(
                pump_until(ctx, 10, || undelivered_handoff(&history()).is_none()),
                "the delivery was not stored"
            );
        });
    }

    /// A Claude that writes `before` (frames that open a turn, when not empty: the prompt is then
    /// sent only once that turn is open), takes a prompt carrying the handoff, writes `turn` (the
    /// rest of a turn that is not the prompt's own), then exits before answering the prompt.
    /// Nothing may be acknowledged: a reopen still owes the handoff. `shown` must hold for one
    /// event, so the fixture provably played its part.
    fn early_prompt_then_another_turn_then_exit(
        before: &str,
        turn: &str,
        shown: fn(&Event) -> bool,
    ) {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, &tmp.path().to_string_lossy());
            let handoff = Handoff {
                summary: "earlier context".into(),
                fence: "0123456789abcdef0123456789abcdef".into(),
                carried: 2,
                total: 3,
            };
            for env in seeded_handoff_events(&handoff, "“source”") {
                store.append_event(&thread, None, &env).expect("stored");
            }
            let history = || -> Vec<Envelope> {
                store
                    .events(&thread, None, 1000)
                    .expect("events")
                    .into_iter()
                    .map(|(_, e)| e)
                    .collect()
            };
            let open = scripted(
                tmp.path(),
                &format!(
                    "IFS= read -r handshake\n\
                     {before}\n\
                     IFS= read -r line\n\
                     printf '%s\\n' \"$line\" >> \"$D/prompts.log\"\n\
                     {turn}\n\
                     exit 0"
                ),
            );
            let (sink, seen) = make_sink();
            let session = ChatSession::new(
                Box::new(agent_core::claude::ClaudeAdapter::new()),
                open,
                store.clone(),
                thread.clone(),
                sink,
                None,
            );
            session.restore_handoff(undelivered_handoff(&history()).expect("owed"));
            if !before.is_empty() {
                session.reload_session();
                assert!(
                    pump_until(ctx, 10, || session.status().running_turn),
                    "the earlier turn never opened"
                );
            }
            session.send_prompt("first");
            assert!(
                pump_until(ctx, 10, || has(&seen, |e| matches!(
                    e,
                    Event::TurnCompleted {
                        state: TurnState::Completed,
                        ..
                    }
                )) && !session.status().alive),
                "the other turn never completed, or the agent never exited"
            );
            assert!(has(&seen, shown), "the fixture did not play its part");
            assert!(lines(tmp.path().join("prompts.log"))
                .iter()
                .any(|l| l.contains("earlier context")));
            assert!(
                !history()
                    .iter()
                    .any(|e| matches!(e.event, Event::HandoffSeeded { carried: 0, .. })),
                "that turn is not the one that carried it"
            );
            assert_eq!(
                undelivered_handoff(&history()),
                Some(handoff),
                "a reopen still owes it"
            );
        });
    }

    const CLAUDE_INIT: &str = "echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s1\",\"model\":\"m\",\"claude_code_version\":\"2.1.293\"}'";
    const CLAUDE_RESULT: &str =
        "echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}'";

    /// Claude's prompt-less startup turn completes before the early prompt is read.
    #[test]
    fn an_init_turn_completing_before_the_prompted_one_does_not_spend_the_handoff() {
        early_prompt_then_another_turn_then_exit(
            "",
            &format!("{CLAUDE_INIT}\n{CLAUDE_RESULT}"),
            |e| matches!(e, Event::TurnStarted { .. }),
        );
    }

    /// (a) A background sub-agent's item (it has a parent) arrives in that startup turn: it is
    /// no answer to our prompt.
    #[test]
    fn a_background_childs_item_is_no_answer_to_the_handoff() {
        let child = "echo '{\"type\":\"assistant\",\"uuid\":\"sub1\",\"parent_tool_use_id\":\"toolu_bg\",\"message\":{\"id\":\"m-sub1\",\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_child\",\"name\":\"Read\",\"input\":{\"file_path\":\"/x\"}}]}}'";
        early_prompt_then_another_turn_then_exit(
            "",
            &format!("{CLAUDE_INIT}\n{child}\n{CLAUDE_RESULT}"),
            |e| {
                matches!(
                    e,
                    Event::ItemStarted {
                        parent: Some(_),
                        ..
                    }
                )
            },
        );
    }

    /// (b) A turn Claude runs itself for a finished background task answers and completes while
    /// the handoff is in flight: it is not our prompt's turn.
    #[test]
    fn a_task_notification_turn_does_not_spend_the_handoff() {
        let answer = "echo '{\"type\":\"assistant\",\"uuid\":\"u1\",\"parent_tool_use_id\":null,\"message\":{\"id\":\"m1\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"The task finished.\"}]}}'";
        let result = "echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"origin\":{\"kind\":\"task-notification\",\"producer\":\"session-task\"}}'";
        early_prompt_then_another_turn_then_exit(
            "",
            &format!("{CLAUDE_INIT}\n{answer}\n{result}"),
            |e| matches!(e, Event::ItemStarted { parent: None, kind, .. } if *kind != ItemKind::UserMessage),
        );
    }

    /// A turn already open when the prompt goes out finishes with a top-level answer of its own
    /// (buffered before it read the line) and a successful result; the agent exits before the
    /// next turn would have read the prompt. Claude delivers a line written near a turn's end to
    /// the next turn, so the open one proves nothing.
    #[test]
    fn a_turn_open_when_the_prompt_went_out_does_not_spend_the_handoff() {
        let answer = "echo '{\"type\":\"assistant\",\"uuid\":\"u0\",\"parent_tool_use_id\":null,\"message\":{\"id\":\"m0\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"Done with the earlier request.\"}]}}'";
        early_prompt_then_another_turn_then_exit(
            CLAUDE_INIT,
            &format!("{answer}\n{CLAUDE_RESULT}"),
            |e| matches!(e, Event::ItemStarted { parent: None, kind, .. } if *kind != ItemKind::UserMessage),
        );
    }

    #[test]
    fn a_turn_spends_the_handoff_only_once_it_answered() {
        use HandoffTurn::{Open, Waiting};
        use HandoffTurnEnd as E;
        use TurnState::{Completed, Failed, Interrupted};
        let end =
            |in_flight, turn, state, from_task| handoff_turn_end(in_flight, turn, state, from_task);
        let answered = Open { answered: true };
        let silent = Open { answered: false };
        assert_eq!(end(false, answered, Completed, false), E::NotCarried);
        // Its own turn, answered and completed: delivered.
        assert_eq!(end(true, answered, Completed, false), E::Delivered);
        // A turn with no top-level answer (a prompt-less startup turn): not its own.
        assert_eq!(end(true, silent, Completed, false), E::StillInFlight);
        // No turn started since it was sent: a stray result.
        assert_eq!(end(true, Waiting, Completed, false), E::StillInFlight);
        assert_eq!(end(true, Waiting, Failed, false), E::Undelivered);
        // A turn the agent ran itself: never spends it, sent again.
        assert_eq!(end(true, answered, Completed, true), E::Undelivered);
        assert_eq!(end(true, answered, Interrupted, false), E::Undelivered);
        assert_eq!(end(true, silent, Failed, false), E::Undelivered);
    }

    #[test]
    fn a_handoff_carrying_nothing_is_not_stored() {
        let empty = Handoff {
            summary: String::new(),
            fence: "0123456789abcdef0123456789abcdef".into(),
            carried: 0,
            total: 0,
        };
        assert!(seeded_handoff_events(&empty, "“source”").is_empty());
    }

    /// Suspension waits for anything a stopped process would lose: no session id yet, a turn
    /// running, dangerous permissive mode (session-only), a delegated worker's process.
    #[test]
    fn a_session_with_anything_going_on_is_not_suspended() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, &tmp.path().to_string_lossy());
            let (sink, seen) = make_sink();
            let session = ChatSession::new(
                Box::new(agent_core::claude::ClaudeAdapter::new()),
                idle_claude(tmp.path()),
                store,
                thread,
                sink,
                None,
            );
            assert!(!session.suspend_if_idle(), "nothing is running yet");
            session.reload_session();
            assert!(pump_until(ctx, 10, || has(&seen, |e| matches!(
                e,
                Event::TurnCompleted { .. }
            ))));
            // A session id with no prompted turn on it may name no transcript to resume.
            assert_eq!(
                session.inner.suspend_blocker(),
                Some("no conversation yet to resume")
            );
            session.send_prompt("first");
            assert!(pump_until(ctx, 10, || session.inner.has_conversation.get()
                && !session.status().running_turn));
            assert_eq!(session.inner.suspend_blocker(), None, "idle now");
            // Codex's own session grants would end unannounced.
            session
                .inner
                .codex_grants
                .borrow_mut()
                .push("Network access".into());
            assert_eq!(
                session.inner.suspend_blocker(),
                Some("Codex session grants")
            );
            session.inner.codex_grants.borrow_mut().clear();
            // Provider workers live in the agent's process: stopping it would end them, even
            // with no turn running. Only an observed exit lets go.
            let workers = |state| {
                Envelope::new(Event::WorkersUpdated {
                    workers: vec![agent_core::event::WorkerSnapshot {
                        id: "w1".into(),
                        name: None,
                        task: None,
                        state,
                        activity: None,
                        delegation: None,
                    }],
                    waiting_for: Vec::new(),
                })
            };
            session
                .inner
                .emit(workers(agent_core::event::WorkerState::Running));
            assert_eq!(
                session.inner.suspend_blocker(),
                Some("active provider workers")
            );
            assert!(!session.suspend_if_idle(), "a running worker keeps it up");
            session
                .inner
                .emit(workers(agent_core::event::WorkerState::Stopping));
            assert_eq!(
                session.inner.suspend_blocker(),
                Some("active provider workers"),
                "a stop in progress is still active"
            );
            session
                .inner
                .emit(workers(agent_core::event::WorkerState::Completed));
            assert_eq!(
                session.inner.suspend_blocker(),
                None,
                "settled workers let go"
            );
            session.inner.state.borrow_mut().mode = Mode::Bypass;
            assert!(
                !session.suspend_if_idle(),
                "Bypass would not survive a resume"
            );
            session.inner.state.borrow_mut().mode = Mode::Ask;
            session.send_prompt("a turn that never ends");
            assert!(!session.suspend_if_idle(), "a turn is running");
            assert!(session.status().alive, "refusals stop nothing");
        });
    }

    fn retired(old: &str, new: &str) -> RetiredModel {
        RetiredModel {
            model: old.into(),
            replacement: Replacement {
                model: new.into(),
                display: new.to_uppercase(),
                effort: None,
            },
        }
    }

    fn open_on(model: &str) -> OpenSession {
        OpenSession {
            program: "/bin/true".into(),
            extra_args: Vec::new(),
            cwd: "/".into(),
            model: Some(model.into()),
            effort: None,
            mode: Mode::Ask,
            resume: None,
            new_session_id: None,
            approval_hook: false,
            delegation: None,
        }
    }

    #[test]
    fn a_prompt_on_a_retired_model_first_moves_to_its_replacement_and_says_so() {
        in_loop(|_| {
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, "/w");
            let (sink, seen) = make_sink();
            let log: Log = Rc::default();
            let session = ChatSession::new(
                FakeAdapter::boxed(Driver::Claude, &log),
                open_on("claude-sonnet-3-7"),
                store.clone(),
                thread.clone(),
                sink,
                None,
            );
            session.set_retired(Some(retired("claude-sonnet-3-7", "sonnet")));
            session.send_prompt("hello");
            let cmds = log.borrow().clone();
            let set = cmds
                .iter()
                .position(|c| matches!(c, Command::SetModel { model, .. } if model == "sonnet"))
                .expect("the replacement was set");
            let prompt = cmds
                .iter()
                .position(|c| matches!(c, Command::Prompt { .. }))
                .expect("the prompt was sent");
            assert!(set < prompt, "the turn must not start on the dead model");
            assert!(has(&seen, |e| matches!(e, Event::Notice { text }
                if text == "Continued on SONNET (claude-sonnet-3-7 was retired)")));
            // What a reopened thread resumes on is the replacement (an alias), not the dead id.
            session.inner.emit(Envelope::new(Event::ModelChanged {
                model: "sonnet".into(),
            }));
            let pt = store.provider_threads(&thread).expect("pts");
            assert_eq!(pt[0].model, "sonnet");
            // Asked once: the next prompt does not repeat it.
            let before = log.borrow().len();
            session.send_prompt("again");
            assert_eq!(log.borrow().len(), before + 1, "just the prompt");
        });
    }

    #[test]
    fn choosing_a_model_yourself_settles_the_retired_question() {
        in_loop(|_| {
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, "/w");
            let (sink, seen) = make_sink();
            let log: Log = Rc::default();
            let session = ChatSession::new(
                FakeAdapter::boxed(Driver::Claude, &log),
                open_on("old"),
                store,
                thread,
                sink,
                None,
            );
            session.set_retired(Some(retired("old", "sonnet")));
            session.switch(Driver::Claude, Some("opus".into()), None);
            session.send_prompt("hi");
            assert!(!log
                .borrow()
                .iter()
                .any(|c| matches!(c, Command::SetModel { model, .. } if model == "sonnet")));
            assert!(!has(&seen, |e| matches!(e, Event::Notice { text }
                if text.contains("was retired"))));
        });
    }

    #[test]
    fn an_agy_resume_after_a_retirement_never_carries_the_retired_model() {
        in_loop(|_| {
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, "/w");
            let (sink, _seen) = make_sink();
            let mut open = open_on("gemini-2-pro-high");
            open.resume = Some("conv-1".into());
            let session = ChatSession::new(
                Box::new(AgyAdapter::new("agy")),
                open,
                store,
                thread,
                sink,
                None,
            );
            session.set_retired(Some(retired("gemini-2-pro-high", "gemini-4-pro-high")));
            session.send_prompt("hello");
            // The restart put the replacement on argv and kept the conversation.
            let argv = session.launch_spec().argv;
            assert!(argv.iter().any(|a| a == "gemini-4-pro-high"), "{argv:?}");
            assert!(
                !argv.iter().any(|a| a.contains("gemini-2-pro")),
                "the retired model is still on argv: {argv:?}"
            );
            assert!(argv.iter().any(|a| a == "conv-1"), "{argv:?}");
        });
    }

    #[test]
    fn hook_flag_and_socket_env_come_from_one_value() {
        in_loop(|ctx| {
            // Handle present: flag on, both env vars exported and non-blank, same socket.
            let tmp = tempfile::tempdir().expect("tmp");
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, "/w");
            let (sink, seen) = make_sink();
            let handle = ApprovalHandle::bind(
                &tmp.path().join("rt"),
                "t1",
                tmp.path(),
                Mode::Ask,
                Duration::from_secs(30),
            )
            .expect("bind");
            let open = OpenSession {
                approval_hook: false, // the session must not trust the caller's value
                delegation: None,
                ..fake_agy(tmp.path())
            };
            let session = ChatSession::new(
                Box::new(AgyAdapter::new("agy")),
                open,
                store.clone(),
                thread,
                sink,
                Some(handle.clone()),
            );
            let spec = session.launch_spec();
            assert!(spec
                .argv
                .iter()
                .any(|a| a == "--dangerously-skip-permissions"));
            let env: std::collections::HashMap<_, _> = spec.env.iter().cloned().collect();
            let socket = env
                .get("AGENT_TERMINAL_APPROVAL_SOCKET")
                .expect("socket var");
            assert!(!socket.trim().is_empty());
            assert_eq!(Path::new(socket), handle.socket_path());
            assert!(Path::new(socket).exists(), "listening");
            let hook = env.get("AGENT_TERMINAL_HOOK_BIN").expect("hook bin var");
            assert!(Path::new(hook).is_absolute());
            assert!(
                !has(&seen, |e| matches!(e, Event::Notice { .. })),
                "no hook notice"
            );
            assert_eq!(session.status().mode, Mode::Ask);

            // The spawned process really got them.
            session.send_prompt("go");
            assert!(pump_until(ctx, 15, || exited_count(&seen) == 1));
            assert_eq!(
                lines(tmp.path().join("sock.log")),
                std::slice::from_ref(socket)
            );
            assert_eq!(
                lines(tmp.path().join("hookbin.log")),
                std::slice::from_ref(hook)
            );
            assert!(
                lines(tmp.path().join("args.log"))[0].contains("--dangerously-skip-permissions")
            );
            drop(session);

            // No handle: flag off, no env, plan mode.
            let tmp = tempfile::tempdir().expect("tmp");
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, "/w");
            let (sink, seen) = make_sink();
            let open = OpenSession {
                approval_hook: true, // ignored: there is no socket to export
                delegation: None,
                ..fake_agy(tmp.path())
            };
            let session = ChatSession::new(
                Box::new(AgyAdapter::new("agy")),
                open,
                store,
                thread,
                sink,
                None,
            );
            let spec = session.launch_spec();
            assert!(spec.env.is_empty());
            assert!(!spec
                .argv
                .iter()
                .any(|a| a == "--dangerously-skip-permissions"));
            assert!(spec.argv.windows(2).any(|w| w == ["--mode", "plan"]));
            session.send_prompt("go");
            assert!(pump_until(ctx, 15, || exited_count(&seen) == 1));
            assert_eq!(lines(tmp.path().join("sock.log")), ["unset"]);
            assert_eq!(lines(tmp.path().join("hookbin.log")), ["unset"]);
        });
    }

    /// A proof that was `Ok` when the socket was bound must not pass the skip flag to a later
    /// process: a hook file planted after it (here a workspace plugin hook) is caught by the
    /// fresh check every hooked agy start runs, and that process starts hookless.
    #[test]
    fn a_hook_planted_after_the_proof_keeps_the_next_agy_start_hookless() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let home = tempfile::tempdir().expect("home");
            let config = home.path().join(".gemini/config");
            std::fs::create_dir_all(&config).expect("mkdir");
            std::fs::write(
                config.join("hooks.json"),
                crate::hook_config::install_entry_json(),
            )
            .expect("hook");
            let env = LaunchEnv {
                env: vec![("HOME".into(), home.path().to_string_lossy().into_owned())],
                unset: crate::hook_config::CONFIG_REDIRECT_VARS
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            };
            let target = crate::hook_config::HookTarget::new(&env, Some(tmp.path().to_path_buf()));
            assert_eq!(
                crate::hook_config::check_target(&target),
                Ok(()),
                "proof Ok"
            );
            set_test_hook_check(std::sync::Arc::new(crate::hook_config::check_target));
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, &tmp.path().to_string_lossy());
            let (sink, seen) = make_sink();
            let handle = ApprovalHandle::bind(
                &tmp.path().join("rt"),
                "t1",
                tmp.path(),
                Mode::Ask,
                Duration::from_secs(30),
            )
            .expect("bind");
            let session = ChatSession::with_env(
                Box::new(AgyAdapter::new("agy")),
                fake_agy(tmp.path()),
                store,
                thread,
                sink,
                Ok(handle),
                env,
            );
            session.send_prompt("go");
            assert!(pump_until(ctx, 15, || exited_count(&seen) == 1));
            let args = lines(tmp.path().join("args.log"));
            assert!(
                args[0].contains("--dangerously-skip-permissions"),
                "{args:?}"
            );

            // Planted after the proof, before the next start (agy restarts for the next prompt).
            let planted = tmp.path().join(".agents/plugins/p/hooks.json");
            std::fs::create_dir_all(planted.parent().expect("parent")).expect("mkdir");
            std::fs::write(&planted, crate::hook_config::install_entry_json()).expect("plant");
            session.send_prompt("again");
            assert!(pump_until(ctx, 15, || exited_count(&seen) == 2));
            let args = lines(tmp.path().join("args.log"));
            assert_eq!(args.len(), 2, "{args:?}");
            assert!(
                !args[1].contains("--dangerously-skip-permissions"),
                "the skip flag shipped with an unchecked hook in force: {args:?}"
            );
            assert!(args[1].contains("--mode plan"), "{args:?}");
            assert_eq!(lines(tmp.path().join("sock.log"))[1], "unset");
            assert!(has(&seen, |e| matches!(
                e,
                Event::Notice { text } if text.contains(".agents/plugins/p/hooks.json")
            )));
            assert_eq!(session.status().mode, Mode::Plan);
        });
    }

    /// A start waiting on its proof holds what is written to it, then writes it in order; a
    /// failed proof still delivers the prompt, to the hookless process.
    #[test]
    fn a_start_waiting_on_its_hook_proof_holds_the_prompt() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (release, wait) = std::sync::mpsc::channel::<()>();
            let wait = std::sync::Mutex::new(wait);
            set_test_hook_check(std::sync::Arc::new(move |_| {
                let _ = wait
                    .lock()
                    .expect("lock")
                    .recv_timeout(Duration::from_secs(10));
                Err("planted for the test".to_owned())
            }));
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, "/w");
            let (sink, seen) = make_sink();
            let handle = ApprovalHandle::bind(
                &tmp.path().join("rt"),
                "t1",
                tmp.path(),
                Mode::Ask,
                Duration::from_secs(30),
            )
            .expect("bind");
            let session = ChatSession::new(
                Box::new(AgyAdapter::new("agy")),
                fake_agy(tmp.path()),
                store,
                thread,
                sink,
                Some(handle),
            );
            session.send_prompt("go");
            assert!(!pump_until(ctx, 1, || tmp.path().join("args.log").exists()));
            assert!(
                session.inner.proc.borrow().is_none(),
                "nothing spawns unproven"
            );
            release.send(()).expect("release");
            assert!(pump_until(ctx, 15, || exited_count(&seen) == 1));
            let args = lines(tmp.path().join("args.log"));
            assert!(
                !args[0].contains("--dangerously-skip-permissions"),
                "{args:?}"
            );
            assert!(has(&seen, |e| matches!(
                e,
                Event::TurnCompleted {
                    state: TurnState::Completed,
                    ..
                }
            )));
        });
    }

    const REFUSED_NOTICE: &str = "checked again just before it started";

    /// A session whose sink runs `on_refusal` (once) when the refused-proof notice arrives, with
    /// the session's inner state; the first hook check fails, later ones wait for `release`.
    fn refusal_reentry(
        on_refusal: impl Fn(&Rc<Inner>) + 'static,
    ) -> (
        tempfile::TempDir,
        Rc<ChatSession>,
        Seen,
        std::sync::mpsc::Sender<()>,
    ) {
        let tmp = tempfile::tempdir().expect("tmp");
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let wait = std::sync::Mutex::new(wait);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        set_test_hook_check(std::sync::Arc::new(move |_| {
            if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                return Err("refused for the test".to_owned());
            }
            let _ = wait
                .lock()
                .expect("lock")
                .recv_timeout(Duration::from_secs(10));
            Ok(())
        }));
        let slot: Rc<RefCell<Option<std::rc::Weak<Inner>>>> = Rc::default();
        let seen: Seen = Rc::default();
        let fired = Cell::new(false);
        let sink: EnvelopeSink = {
            let (slot, seen) = (slot.clone(), seen.clone());
            Rc::new(move |e: &Envelope| {
                seen.borrow_mut().push(e.clone());
                let refusal =
                    matches!(&e.event, Event::Notice { text } if text.contains(REFUSED_NOTICE));
                if refusal && !fired.replace(true) {
                    let inner = slot.borrow().as_ref().and_then(std::rc::Weak::upgrade);
                    if let Some(inner) = inner {
                        on_refusal(&inner);
                    }
                }
            })
        };
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let thread = fresh(&store, "/w");
        let handle = ApprovalHandle::bind(
            &tmp.path().join("rt"),
            "t1",
            tmp.path(),
            Mode::Ask,
            Duration::from_secs(30),
        )
        .expect("bind");
        let session = ChatSession::new(
            Box::new(AgyAdapter::new("agy")),
            fake_agy(tmp.path()),
            store,
            thread,
            sink,
            Some(handle),
        );
        *slot.borrow_mut() = Some(Rc::downgrade(&session.inner));
        (tmp, session, seen, release)
    }

    /// A sink that stops the session while the refused proof is being said: nothing spawns.
    #[test]
    fn a_stop_during_the_refused_hook_notice_spawns_nothing() {
        in_loop(|ctx| {
            let (tmp, session, seen, release) = refusal_reentry(|inner| inner.stop_current());
            session.send_prompt("go");
            assert!(pump_until(ctx, 10, || has(&seen, |e| matches!(
                e,
                Event::Notice { text } if text.contains(REFUSED_NOTICE)
            ))));
            assert!(!pump_until(ctx, 1, || tmp.path().join("args.log").exists()));
            assert!(session.inner.proc.borrow().is_none(), "nothing spawned");
            assert!(!session.status().alive);
            drop(release);
        });
    }

    /// A sink that replaces the start with a hooked one whose own proof is still out: the
    /// refused start spawns nothing, and nothing gets the skip flag before that proof is in.
    #[test]
    fn a_replacement_during_the_refused_hook_notice_waits_for_its_own_proof() {
        in_loop(|ctx| {
            let rt = tempfile::tempdir().expect("rt");
            let replacement = ApprovalHandle::bind(
                &rt.path().join("rt"),
                "t2",
                rt.path(),
                Mode::Ask,
                Duration::from_secs(30),
            )
            .expect("bind");
            let (tmp, session, seen, release) = refusal_reentry(move |inner| {
                // A hooked agy in Ask (Plan never takes the flag), like a fresh switch to it.
                inner.stop_current();
                *inner.approval.borrow_mut() = Some(replacement.clone());
                inner.open.borrow_mut().mode = Mode::Ask;
                inner.state.borrow_mut().mode = Mode::Ask;
                inner.start_process();
            });
            session.send_prompt("go");
            assert!(pump_until(ctx, 10, || has(&seen, |e| matches!(
                e,
                Event::Notice { text } if text.contains(REFUSED_NOTICE)
            ))));
            assert!(!pump_until(ctx, 1, || tmp.path().join("args.log").exists()));
            assert!(session.inner.proc.borrow().is_none(), "A spawned nothing");
            assert!(
                session.inner.starting.borrow().is_some(),
                "B still waits on its proof"
            );
            // B's proof comes in: B alone spawns, with the flag it was proven for.
            release.send(()).expect("release");
            assert!(pump_until(ctx, 10, || tmp.path().join("args.log").exists()));
            assert!(pump_until(ctx, 10, || !lines(tmp.path().join("args.log")).is_empty()));
            let args = lines(tmp.path().join("args.log"));
            assert_eq!(args.len(), 1, "{args:?}");
            assert!(
                args[0].contains("--dangerously-skip-permissions"),
                "{args:?}"
            );
        });
    }

    #[test]
    fn unsupported_commands_become_error_envelopes() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, "/w");
            let (sink, seen) = make_sink();
            let session = ChatSession::new(
                Box::new(AgyAdapter::new("agy")),
                fake_agy(tmp.path()),
                store,
                thread,
                sink,
                None,
            );
            session.respond_approval("r1", Decision::Allow);
            assert!(has(
                &seen,
                |e| matches!(e, Event::Error { message } if message.contains("not supported"))
            ));
            assert!(
                !has(&seen, |e| matches!(e, Event::ApprovalResolved { .. })),
                "a refused answer leaves the card open"
            );

            let id = session.control(Control::McpStatus);
            assert_eq!(id, "ctl-1");
            assert_eq!(session.control(Control::ContextUsage), "ctl-2");
            assert!(seen.borrow().iter().any(|e| {
                e.request.as_deref() == Some("ctl-1")
                    && matches!(e.event, Event::ControlResult { error: Some(_), .. })
            }));
            // Let the replay process finish so the test does not leave one behind.
            drop(session);
            let _ = ctx;
        });
    }

    #[test]
    fn a_stop_the_app_asked_for_is_never_reported_as_a_crash() {
        // The live bug: switching an idle Claude thread to agy showed "Turn failed: The agent
        // exited unexpectedly" (Claude's `init` had opened a turn).
        let mut adapter = agent_core::claude::ClaudeAdapter::new();
        adapter.feed(
            r#"{"type":"system","subtype":"init","session_id":"s1","model":"opus","cwd":"/w"}"#,
        );
        let mut envs = adapter.on_exit(None);
        assert!(envs.iter().any(|e| matches!(
            e.event,
            Event::TurnCompleted {
                state: TurnState::Failed,
                ..
            }
        )));
        envs.iter_mut().for_each(as_deliberate_stop);
        for e in &envs {
            match &e.event {
                Event::SessionExited { expected, .. } => assert!(*expected),
                Event::TurnCompleted { state, error, .. } => {
                    assert_eq!(*state, TurnState::Interrupted);
                    assert!(error.is_none());
                }
                _ => {}
            }
        }
    }

    #[test]
    fn an_answer_the_adapter_accepts_resolves_the_card() {
        // Claude never acknowledges an answer: without this the card sits at "Allow…" for good.
        in_loop(|_| {
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, "/w");
            let (sink, seen) = make_sink();
            let log: Log = Rc::default();
            let session = ChatSession::new(
                FakeAdapter::boxed(Driver::Claude, &log),
                open_on("opus"),
                store,
                thread,
                sink,
                None,
            );
            // No card was seen for "r1", so there is nothing a rule could keep: "Allow for
            // session" resolves as a plain Allow (never a claim of a rule that does not exist).
            session.respond_approval("r1", Decision::AllowForSession);
            assert!(seen.borrow().iter().any(|e| {
                e.request.as_deref() == Some("r1")
                    && matches!(
                        e.event,
                        Event::ApprovalResolved {
                            decision: Decision::Allow
                        }
                    )
            }));
            assert!(has(
                &seen,
                |e| matches!(e, Event::Notice { text } if text.starts_with("Allowed once"))
            ));
        });
    }

    #[test]
    fn same_driver_agy_model_switch_restarts_and_resumes() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, "/w");
            let (sink, seen) = make_sink();
            let session = ChatSession::new(
                Box::new(AgyAdapter::new("agy")),
                fake_agy(tmp.path()),
                store,
                thread,
                sink,
                None,
            );
            session.send_prompt("hi");
            assert!(pump_until(ctx, 15, || exited_count(&seen) >= 1));
            session.switch(Driver::Agy, Some("gemini-flash".into()), None);
            assert!(has(
                &seen,
                |e| matches!(e, Event::ModelChangeRequested { model } if model == "gemini-flash")
            ));
            assert_eq!(
                session.status().pending_model.as_deref(),
                Some("gemini-flash")
            );
            session.send_prompt("again");
            assert!(pump_until(ctx, 15, || exited_count(&seen) >= 3));
            assert_eq!(session.status().model.as_deref(), Some("gemini-flash"));
            assert_eq!(session.status().pending_model, None);
            let args = lines(tmp.path().join("args.log"));
            let last = args.last().expect("args");
            assert!(last.contains("--model gemini-flash"), "{args:?}");
            assert!(
                last.contains(&format!("--conversation {CONVERSATION}")),
                "{args:?}"
            );
        });
    }

    // ---- handoff ----

    type Log = Rc<RefCell<Vec<Command>>>;

    /// An adapter whose process is `cat` and which records every command it is asked to encode.
    struct FakeAdapter {
        driver: Driver,
        caps: Capabilities,
        log: Log,
        /// `encode(SetModel)` answers Unsupported (an adapter that cannot restart itself).
        refuse_set_model: bool,
        /// `on_exit` reports a turn that was still running, as a real adapter does mid-turn.
        open_turn: bool,
        /// Its binary does not exist: spawning it fails.
        unspawnable: bool,
        write_commands: bool,
        refuse_steer: bool,
    }

    impl FakeAdapter {
        fn boxed(driver: Driver, log: &Log) -> Box<dyn Adapter> {
            Self::unboxed(driver, log)
        }

        fn refusing_set_model(driver: Driver, log: &Log) -> Box<dyn Adapter> {
            Box::new(Self {
                refuse_set_model: true,
                ..*Self::unboxed(driver, log)
            })
        }

        fn unboxed(driver: Driver, log: &Log) -> Box<Self> {
            Box::new(Self {
                driver,
                caps: Capabilities::of(driver),
                log: log.clone(),
                refuse_set_model: false,
                open_turn: false,
                unspawnable: false,
                write_commands: false,
                refuse_steer: false,
            })
        }

        /// [`Self::boxed`], as a plain `fn` pointer.
        fn unboxed_dyn(driver: Driver, log: &Log) -> Box<dyn Adapter> {
            Self::unboxed(driver, log)
        }

        fn unspawnable(driver: Driver, log: &Log) -> Box<dyn Adapter> {
            Box::new(Self {
                unspawnable: true,
                ..*Self::unboxed(driver, log)
            })
        }

        fn with_open_turn(driver: Driver, log: &Log) -> Box<dyn Adapter> {
            Box::new(Self {
                open_turn: true,
                ..*Self::unboxed(driver, log)
            })
        }

        fn writing_with_open_turn(driver: Driver, log: &Log) -> Box<dyn Adapter> {
            Box::new(Self {
                open_turn: true,
                write_commands: true,
                ..*Self::unboxed(driver, log)
            })
        }
    }

    impl Adapter for FakeAdapter {
        fn driver(&self) -> Driver {
            self.driver
        }
        fn capabilities(&self) -> &Capabilities {
            &self.caps
        }
        fn prompt_starts_turn(&self, text: &str) -> bool {
            !text.trim().starts_with('/')
        }
        fn argv(&self, _: &OpenSession) -> Vec<String> {
            if self.unspawnable {
                return vec!["/nonexistent/agent-terminal-test-agent".into()];
            }
            vec!["/bin/cat".into()]
        }
        fn handshake(&mut self) -> Vec<String> {
            Vec::new()
        }
        fn encode(&mut self, command: Command) -> Result<Vec<Action>, AdapterError> {
            let refuse = self.refuse_set_model && matches!(command, Command::SetModel { .. });
            let refuse_steer = self.refuse_steer && matches!(command, Command::Steer { .. });
            let lines = self
                .write_commands
                .then(|| vec![serde_json::to_string(&command).expect("command")]);
            self.log.borrow_mut().push(command);
            if refuse {
                return Err(AdapterError::Unsupported("set model"));
            }
            if refuse_steer {
                return Err(AdapterError::Unsupported("steer"));
            }
            Ok(lines.map(Action::Write).into_iter().collect())
        }
        fn feed(&mut self, line: &str) -> Vec<Envelope> {
            if self.write_commands {
                if let Ok(Command::Prompt { text }) = serde_json::from_str(line) {
                    return vec![Envelope::new(Event::Notice { text })];
                }
            }
            Vec::new()
        }
        fn feed_stderr(&mut self, _: &str) -> Vec<Envelope> {
            Vec::new()
        }
        fn feed_side(&mut self, _: &str, _: &str, _: bool) -> Vec<Envelope> {
            Vec::new()
        }
        fn on_exit(&mut self, code: Option<i32>) -> Vec<Envelope> {
            let mut out = Vec::new();
            if self.open_turn {
                out.push(Envelope::new(Event::TurnCompleted {
                    state: TurnState::Failed,
                    usage: None,
                    cost_usd: None,
                    error: Some("the agent exited mid-turn".into()),
                }));
            }
            out.push(Envelope::new(Event::SessionExited {
                code,
                expected: true,
            }));
            out
        }
    }

    #[test]
    fn shutdown_persists_an_interrupted_turn_and_the_exit() {
        in_loop(|_| {
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, "/w");
            let (sink, seen) = make_sink();
            let log: Log = Rc::default();
            let session = ChatSession::new(
                FakeAdapter::with_open_turn(Driver::Claude, &log),
                open_on("sonnet"),
                store.clone(),
                thread.clone(),
                sink,
                None,
            );
            session.send_prompt("hello");
            session.shutdown(false);
            let stored = store.events(&thread, None, 1000).expect("events");
            let completed: Vec<_> = stored
                .iter()
                .filter_map(|(_, e)| match &e.event {
                    Event::TurnCompleted { state, error, .. } => Some((*state, error.clone())),
                    _ => None,
                })
                .collect();
            assert_eq!(completed, vec![(TurnState::Interrupted, None)]);
            assert!(stored
                .iter()
                .any(|(_, e)| matches!(e.event, Event::SessionExited { .. })));
            assert!(has(&seen, |e| matches!(e, Event::SessionExited { .. })));
            assert!(!session.status().alive && !session.status().running_turn);
            // A second call finds nothing to stop and writes nothing more.
            let before = store.events(&thread, None, 1000).expect("events").len();
            session.shutdown(false);
            let after = store.events(&thread, None, 1000).expect("events").len();
            assert_eq!(before, after);
        });
    }

    fn unread_after_shutdown(keep_read: bool, read_first: bool) -> bool {
        let mut unread = false;
        in_loop(|_| {
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, "/w");
            let (sink, _seen) = make_sink();
            let log: Log = Rc::default();
            let session = ChatSession::new(
                FakeAdapter::boxed(Driver::Claude, &log),
                open_on("sonnet"),
                store.clone(),
                thread.clone(),
                sink,
                None,
            );
            if read_first {
                store.mark_read(&thread).expect("read");
            }
            session.shutdown(keep_read);
            let all = store.list_threads(true).expect("list");
            unread = all.iter().find(|t| t.id == thread).expect("thread").unread;
        });
        unread
    }

    #[test]
    fn shutdown_leaves_a_read_thread_read_and_an_unread_one_unread() {
        // Without keep_read the closing events make a read thread unread (the bug).
        assert!(unread_after_shutdown(false, true));
        assert!(!unread_after_shutdown(true, true));
        // Unread before: the caller passes keep_read false, and it stays unread.
        assert!(unread_after_shutdown(false, false));
    }

    /// An adapter over `cat` whose outbox writes a line on every prompt and, when it sees that
    /// line come back on stdout, another one: what Codex's queued prompts and replies need.
    struct OutboxAdapter {
        caps: Capabilities,
        outbox: agent_core::adapter::Outbox,
    }

    impl Adapter for OutboxAdapter {
        fn driver(&self) -> Driver {
            Driver::Codex
        }
        fn capabilities(&self) -> &Capabilities {
            &self.caps
        }
        fn argv(&self, _: &OpenSession) -> Vec<String> {
            vec!["/bin/cat".into()]
        }
        fn handshake(&mut self) -> Vec<String> {
            Vec::new()
        }
        fn encode(&mut self, command: Command) -> Result<Vec<Action>, AdapterError> {
            if matches!(command, Command::Prompt { .. }) {
                self.outbox
                    .actions
                    .push(Action::Write(vec!["from-encode".into()]));
                self.outbox.events.push(Envelope::new(Event::Notice {
                    text: "evt-encode".into(),
                }));
            }
            Ok(Vec::new())
        }
        fn feed(&mut self, line: &str) -> Vec<Envelope> {
            if line == "from-encode" {
                self.outbox
                    .actions
                    .push(Action::Write(vec!["from-feed".into()]));
            }
            vec![Envelope::new(Event::Notice {
                text: format!("fed:{line}"),
            })]
        }
        fn feed_stderr(&mut self, _: &str) -> Vec<Envelope> {
            Vec::new()
        }
        fn feed_side(&mut self, _: &str, _: &str, _: bool) -> Vec<Envelope> {
            Vec::new()
        }
        fn on_exit(&mut self, _: Option<i32>) -> Vec<Envelope> {
            Vec::new()
        }
        fn drain_outbox(&mut self) -> agent_core::adapter::Outbox {
            std::mem::take(&mut self.outbox)
        }
    }

    #[test]
    fn the_adapters_outbox_is_executed_and_dispatched_after_encode_and_feed() {
        in_loop(|ctx| {
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, "/w");
            let (sink, seen) = make_sink();
            let session = ChatSession::new(
                Box::new(OutboxAdapter {
                    caps: Capabilities::codex(),
                    outbox: agent_core::adapter::Outbox::default(),
                }),
                OpenSession {
                    program: "unused".into(),
                    extra_args: Vec::new(),
                    cwd: "/".into(),
                    model: None,
                    effort: None,
                    mode: Mode::Ask,
                    resume: None,
                    new_session_id: None,
                    approval_hook: false,
                    delegation: None,
                },
                store,
                thread,
                sink,
                None,
            );
            session.send_prompt("hello");
            let notice = |want: &str| {
                has(
                    &seen,
                    |e| matches!(e, Event::Notice { text } if text == want),
                )
            };
            // The encode's outbox event is dispatched at once; its write reaches `cat`, which
            // echoes it back through `feed`, whose own outbox write is then carried out too.
            assert!(notice("evt-encode"));
            assert!(
                pump_until(ctx, 10, || notice("fed:from-feed")),
                "the write queued by a feed never reached the process"
            );
            assert!(notice("fed:from-encode"));
        });
    }

    fn launch_of(adapter: Box<dyn Adapter>) -> Result<AgentLaunch, String> {
        Ok(AgentLaunch {
            adapter,
            program: "unused".into(),
            extra_args: Vec::new(),
            default_model: None,
            default_effort: None,
            env: LaunchEnv::default(),
            approval: Err(None),
        })
    }

    #[test]
    fn a_cross_agent_switch_starts_the_profiles_binary_on_the_chosen_or_default_model() {
        in_loop(|_| {
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, "/w");
            let (sink, seen) = make_sink();
            let log: Log = Rc::default();
            let session = ChatSession::new(
                FakeAdapter::boxed(Driver::Agy, &log),
                OpenSession {
                    program: "/usr/bin/agy".into(),
                    extra_args: vec!["--agy-only".into()],
                    cwd: "/".into(),
                    model: Some("gemini-3.1-pro-high".into()),
                    effort: None,
                    mode: Mode::Plan,
                    resume: None,
                    new_session_id: None,
                    approval_hook: false,
                    delegation: None,
                },
                store,
                thread,
                sink,
                None,
            );
            let made: Rc<Cell<usize>> = Rc::default();
            let count = made.clone();
            let agy_log = log.clone();
            session.set_adapter_factory(Rc::new(move |d| {
                count.set(count.get() + 1);
                Ok(AgentLaunch {
                    adapter: match d {
                        Driver::Claude => Box::new(agent_core::claude::ClaudeAdapter::new()),
                        Driver::Agy | Driver::Codex => FakeAdapter::boxed(d, &agy_log),
                    },
                    program: "/bin/true".into(),
                    extra_args: vec!["--profile-arg".into()],
                    default_model: (d == Driver::Claude).then(|| "claude-sonnet-5-5".into()),
                    default_effort: None,
                    env: LaunchEnv {
                        env: vec![("FROM_ENV_FILE".into(), "1".into())],
                        unset: vec!["CLAUDECODE".into()],
                    },
                    approval: Err(None),
                })
            }));

            // No model named: the agent's configured default.
            session.switch(Driver::Claude, None, None);
            assert_eq!(made.get(), 1);
            let spec = session.launch_spec();
            assert_eq!(spec.argv[0], "/bin/true");
            assert!(spec.argv.contains(&"--profile-arg".to_owned()));
            assert!(!spec.argv.contains(&"--agy-only".to_owned()));
            assert!(spec
                .argv
                .windows(2)
                .any(|w| w == ["--model", "claude-sonnet-5-5"]));
            assert!(spec.env.contains(&("FROM_ENV_FILE".into(), "1".into())));
            assert!(spec.unset.contains(&"CLAUDECODE".to_owned()));
            assert_eq!(session.status().model, None);
            assert_eq!(
                session.status().pending_model.as_deref(),
                Some("claude-sonnet-5-5")
            );
            session.inner.emit(Envelope::new(Event::SessionStarted {
                native_id: "profile-sonnet".into(),
                model: Some("claude-sonnet-5-5".into()),
                cwd: None,
            }));
            assert_eq!(session.status().model.as_deref(), Some("claude-sonnet-5-5"));

            // A model picked from the catalogue wins over the default.
            session.switch(Driver::Agy, Some("gemini-3.1-pro-high".into()), None);
            session.switch(Driver::Claude, Some("claude-opus-5-5".into()), None);
            let spec = session.launch_spec();
            assert!(spec
                .argv
                .windows(2)
                .any(|w| w == ["--model", "claude-opus-5-5"]));
            assert!(has(
                &seen,
                |e| matches!(e, Event::ModelChangeRequested { model } if model == "claude-opus-5-5")
            ));
        });
    }

    fn claude_on(mode: Mode, log: &Log) -> (Rc<ChatSession>, Seen) {
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let thread = fresh(&store, "/w");
        let (sink, seen) = make_sink();
        let session = ChatSession::new(
            FakeAdapter::boxed(Driver::Claude, log),
            OpenSession {
                program: "unused".into(),
                extra_args: Vec::new(),
                cwd: "/".into(),
                model: None,
                effort: None,
                mode,
                resume: None,
                new_session_id: None,
                approval_hook: false,
                delegation: None,
            },
            store,
            thread,
            sink,
            None,
        );
        let log2 = log.clone();
        session.set_adapter_factory(Rc::new(move |d| launch_of(FakeAdapter::boxed(d, &log2))));
        (session, seen)
    }

    #[test]
    fn the_launch_environment_tracks_a_cross_agent_switch() {
        in_loop(|_| {
            let log: Log = Rc::default();
            let (session, _) = claude_on(Mode::Plan, &log);
            assert_eq!(session.launch_environment(), LaunchEnv::default());
            let agy_env = LaunchEnv {
                env: vec![("HOME".into(), "/agy/profile".into())],
                unset: vec![],
            };
            let expected = agy_env.clone();
            session.set_adapter_factory(Rc::new(move |driver| {
                let mut launch = launch_of(FakeAdapter::boxed(driver, &log))?;
                launch.env = agy_env.clone();
                Ok(launch)
            }));
            session.switch(Driver::Agy, None, None);
            assert_eq!(session.status().driver, Driver::Agy);
            assert_eq!(session.launch_environment(), expected);
        });
    }

    #[test]
    fn hookless_agy_keeps_accept_edits_and_turns_ask_into_plan_until_you_leave_it() {
        in_loop(|_| {
            // Accept edits is a real agy flag: it carries over.
            let log: Log = Rc::default();
            let (session, _) = claude_on(Mode::AcceptEdits, &log);
            session.switch(Driver::Agy, Some("gemini-flash".into()), None);
            assert_eq!(session.status().driver, Driver::Agy);
            assert_eq!(session.status().mode, Mode::AcceptEdits);

            // Ask cannot be honoured without the hook: Plan, and the user is told why.
            let log: Log = Rc::default();
            let (session, seen) = claude_on(Mode::Ask, &log);
            session.switch(Driver::Agy, Some("gemini-flash".into()), None);
            assert_eq!(session.status().mode, Mode::Plan);
            assert!(has(
                &seen,
                |e| matches!(e, Event::Notice { text } if text.contains("cannot ask you before acting"))
            ));
            // Back on Claude, the thread is in Ask again (the live bug: it stayed in Plan).
            session.switch(Driver::Claude, Some("sonnet".into()), None);
            assert_eq!(session.status().driver, Driver::Claude);
            assert_eq!(session.status().mode, Mode::Ask);
        });
    }

    fn prompts(log: &Log) -> Vec<String> {
        log.borrow()
            .iter()
            .filter_map(|c| match c {
                Command::Prompt { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn create_with_handoff_moves_history_into_a_new_provider_thread() {
        in_loop(|_| {
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, "/w");
            let first = pt_of(&store, &thread);
            // History from the first agent, with a planted secret in the tool output.
            let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123"; // gitleaks:allow
            store
                .append_user_message(&thread, Some(&first), "please list the repo")
                .expect("user");
            for e in [
                Envelope::new(Event::ItemStarted {
                    kind: ItemKind::AssistantMessage,
                    title: String::new(),
                    input: None,
                    parent: None,
                })
                .item("a1"),
                Envelope::new(Event::ContentSnapshot {
                    stream: StreamKind::Assistant,
                    text: format!("Found three files. token={secret}"),
                })
                .item("a1"),
                Envelope::new(Event::ItemCompleted {
                    status: agent_core::event::ItemStatus::Completed,
                    output: None,
                    error: None,
                })
                .item("a1"),
            ] {
                store
                    .append_event(&thread, Some(&first), &e)
                    .expect("event");
            }

            let (sink, seen) = make_sink();
            let log_agy: Log = Rc::default();
            let log_claude: Log = Rc::default();
            let session = ChatSession::new(
                FakeAdapter::boxed(Driver::Agy, &log_agy),
                OpenSession {
                    program: "unused".into(),
                    extra_args: vec!["--profile-only".into()],
                    cwd: "/".into(),
                    model: Some("gemini-pro".into()),
                    effort: None,
                    mode: Mode::Plan,
                    resume: None,
                    new_session_id: None,
                    approval_hook: false,
                    delegation: None,
                },
                store.clone(),
                thread.clone(),
                sink,
                None,
            );
            // Without a factory a cross-agent switch is refused, and nothing changes.
            session.switch(Driver::Claude, Some("opus".into()), None);
            assert!(has(&seen, |e| matches!(e, Event::Error { .. })));
            assert_eq!(session.status().driver, Driver::Agy);
            assert_eq!(store.provider_threads(&thread).expect("pts").len(), 1);

            let log = log_claude.clone();
            session.set_adapter_factory(Rc::new(move |d| launch_of(FakeAdapter::boxed(d, &log))));
            session.switch(Driver::Claude, Some("opus".into()), None);

            let status = session.status();
            assert_eq!(status.driver, Driver::Claude);
            assert_eq!(status.model, None);
            assert_eq!(status.pending_model.as_deref(), Some("opus"));
            assert!(status.alive);
            assert!(has(&seen, |e| matches!(e, Event::Notice { text }
                if text.starts_with("Continuing in Claude with all 2 earlier messages."))));
            assert!(has(
                &seen,
                |e| matches!(e, Event::ModelChangeRequested { model } if model == "opus")
            ));
            let pts = store.provider_threads(&thread).expect("pts");
            assert_eq!(pts.len(), 2);
            assert_eq!(pts[1].driver, "claude");
            assert_eq!(
                store.active_provider_thread(&thread).expect("active"),
                Some(pts[1].id.clone())
            );

            // The handoff rides on the next prompt only, and the store keeps the user's words.
            // A slash command is never wrapped and does not spend the handoff.
            session.send_prompt("/model");
            session.send_prompt("now summarize");
            session.send_prompt("and again");
            let sent = prompts(&log_claude);
            assert_eq!(sent.len(), 3);
            assert_eq!(sent[0], "/model");
            let sent = &sent[1..];
            assert!(
                sent[0].starts_with("Context handoff (HANDOFF-"),
                "{}",
                sent[0]
            );
            assert!(sent[0].contains("please list the repo"));
            assert!(sent[0].contains("Found three files"));
            // The user's own words come last, after the fenced user marker.
            assert!(sent[0].ends_with("):\nnow summarize"), "{}", sent[0]);
            assert_eq!(sent[0].matches("\nUser message (HANDOFF-").count(), 1);
            assert!(!sent[0].contains(secret), "secret leaked into the handoff");
            assert_eq!(sent[1], "and again");
            assert!(prompts(&log_agy).is_empty());
            let msgs = store.transcript_messages(&thread).expect("msgs");
            assert!(msgs.iter().any(|m| m.text == "now summarize"));
            assert!(!msgs.iter().any(|m| m.text.contains("Context handoff")));
            // New events belong to the new provider thread.
            let all = store.events(&thread, None, 1000).expect("events");
            assert!(all.len() > 5);

            // the handoff is stored, so a restart now (nothing completed yet)
            // re-arms the very same one.
            let stored = |store: &Store| {
                let events = store.events(&thread, None, 10_000).expect("events");
                undelivered_handoff(events.iter().map(|(_, e)| e))
            };
            let before = stored(&store).expect("stored and undelivered");
            assert!(sent[0].contains(&format!("HANDOFF-{}", before.fence)));

            let turn_ends = |state| {
                session.inner.emit(Envelope::new(Event::TurnCompleted {
                    state,
                    usage: None,
                    cost_usd: None,
                    error: None,
                }));
            };
            // The turn that carried it failed (the agent died reading it): it goes out again.
            turn_ends(TurnState::Failed);
            session.send_prompt("try once more");
            let sent = prompts(&log_claude);
            assert!(
                sent[3].starts_with("Context handoff (HANDOFF-"),
                "{}",
                sent[3]
            );
            assert!(sent[3].ends_with("):\ntry once more"));
            assert!(stored(&store).is_some());

            // The process dies before that turn even opened (no TurnCompleted at all): the
            // handoff is not stuck "in flight", it goes out again with the next prompt.
            session.inner.finish_process(Some(1), false);
            session.send_prompt("after the crash");
            let sent = prompts(&log_claude);
            assert!(
                sent[4].starts_with("Context handoff (HANDOFF-"),
                "{}",
                sent[4]
            );

            // A completed turn with no answer is not the one that carried it (Claude's
            // prompt-less init turn): still owed.
            turn_ends(TurnState::Completed);
            assert!(
                stored(&store).is_some(),
                "an unanswered turn spends nothing"
            );
            // The turn of that prompt, completed with an answer, spends it, in the session and
            // in the store.
            session
                .inner
                .emit(Envelope::new(Event::TurnStarted { model: None }));
            session.inner.emit(
                Envelope::new(Event::ItemStarted {
                    kind: ItemKind::AssistantMessage,
                    title: String::new(),
                    input: None,
                    parent: None,
                })
                .item("answer"),
            );
            turn_ends(TurnState::Completed);
            session.send_prompt("thanks");
            assert_eq!(prompts(&log_claude)[5], "thanks");
            assert!(
                stored(&store).is_none(),
                "delivered: not re-armed on reopen"
            );
        });
    }

    /// The user's chosen mode is stored at every switch and on every choice, so a reopen starts
    /// from it: Ask, not the Plan hookless agy ran, nor another agent's profile default.
    #[test]
    fn the_chosen_mode_is_stored_through_switches_and_choices() {
        in_loop(|_| {
            let log: Log = Rc::default();
            let (session, _) = claude_on(Mode::Ask, &log);
            let store = session.inner.store.clone();
            let thread = session.inner.thread.clone();
            let chosen = || {
                let events = store
                    .reader()
                    .events(&thread, None, 10_000)
                    .expect("events");
                chosen_mode(events.iter().map(|(_, e)| e))
            };
            session.switch(Driver::Agy, Some("gemini-flash".into()), None);
            assert_eq!(session.status().mode, Mode::Plan, "hookless agy runs Plan");
            assert_eq!(chosen(), Some(Mode::Ask), "but the choice stored is Ask");
            assert_eq!(session.wanted_mode(), Mode::Ask);

            session.set_mode(Mode::AcceptEdits);
            assert_eq!(chosen(), Some(Mode::AcceptEdits));
        });
    }

    #[test]
    fn hookless_agy_records_an_ask_choice_and_reopens_with_it() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (session, _) = recording(ctx, tmp.path(), Box::new(AgyAdapter::new("agy")));
            let store = session.inner.store.clone();
            let thread = session.inner.thread.clone();
            let chosen = || {
                let events = store
                    .reader()
                    .events(&thread, None, 10_000)
                    .expect("events");
                chosen_mode(events.iter().map(|(_, e)| e))
            };
            session.set_mode(Mode::AcceptEdits);
            assert_eq!(chosen(), Some(Mode::AcceptEdits));
            session.send_prompt("first");
            let generation = session.inner.generation.get();
            let argv = session.launch_spec().argv;
            session.set_mode(Mode::Ask);
            assert_eq!(chosen(), Some(Mode::Ask));
            assert_eq!(session.inner.wanted_mode.get(), Mode::Ask);
            assert_eq!(session.inner.safe_mode.get(), Mode::Ask);
            assert_eq!(
                session.status().mode,
                Mode::AcceptEdits,
                "the hookless process cannot ask"
            );
            assert_eq!(session.inner.open.borrow().mode, Mode::AcceptEdits);
            assert_eq!(session.inner.generation.get(), generation);
            assert_eq!(session.launch_spec().argv, argv);
            assert!(session.status().running_turn);
            let mut open = session.inner.open.borrow().clone();
            open.mode = chosen().expect("stored choice");
            session.shutdown(false);
            let (sink, _) = make_sink();
            let reopened = ChatSession::new(
                Box::new(AgyAdapter::new("agy")),
                open,
                store,
                thread,
                sink,
                None,
            );
            assert_eq!(reopened.wanted_mode(), Mode::Ask);
            assert_eq!(
                reopened.status().mode,
                Mode::Plan,
                "reopening uses the safe fallback for Ask"
            );
            assert!(reopened
                .launch_spec()
                .argv
                .windows(2)
                .any(|a| a == ["--mode", "plan"]));
        });
    }

    #[test]
    fn a_hook_stop_turn_interrupts_even_when_its_reply_is_refused() {
        in_loop(|_| {
            for answered in [false, true] {
                for decision in [Decision::Allow, Decision::Deny, Decision::Cancel] {
                    let log: Log = Rc::default();
                    let (session, seen) = claude_on(Mode::Ask, &log);
                    session.switch(Driver::Agy, Some("gemini-flash".into()), None);
                    log.borrow_mut().clear();
                    // Exercise the hook's post-response path without a socket: false includes
                    // an answer-time denial and a request the hook no longer waits for.
                    session.inner.finish_hook_approval("h1", decision, answered);
                    assert_eq!(
                        log.borrow()
                            .iter()
                            .filter(|c| matches!(c, Command::Interrupt))
                            .count(),
                        usize::from(decision == Decision::Cancel)
                    );
                    assert!(!log
                        .borrow()
                        .iter()
                        .any(|c| matches!(c, Command::Approve { .. })));
                    assert_eq!(resolution(&seen, "h1"), answered.then_some(decision));
                }
            }
        });
    }

    /// Dangerous permissive mode is session-only: no process starts in it, it is never stored
    /// as the thread's choice nor handed to a fork, and a switch to another agent ends it.
    #[test]
    fn dangerous_permissive_mode_is_never_stored_and_ends_with_its_agent() {
        in_loop(|_| {
            // Asked to start in it (a hand-edited default, say): it starts in Ask.
            let log: Log = Rc::default();
            let (session, _) = claude_on(Mode::Bypass, &log);
            assert_eq!(session.status().mode, Mode::Ask);
            assert_eq!(session.inner.open.borrow().mode, Mode::Ask);
            assert_eq!(session.wanted_mode(), Mode::Ask);

            let log: Log = Rc::default();
            let (session, seen) = claude_on(Mode::AcceptEdits, &log);
            let store = session.inner.store.clone();
            let thread = session.inner.thread.clone();
            let chosen = || {
                let events = store
                    .reader()
                    .events(&thread, None, 10_000)
                    .expect("events");
                chosen_mode(events.iter().map(|(_, e)| e))
            };
            session.set_mode(Mode::Bypass);
            assert!(log
                .borrow()
                .iter()
                .any(|c| matches!(c, Command::SetMode { mode: Mode::Bypass })));
            // The agent confirms it.
            session
                .inner
                .emit(Envelope::new(Event::ModeChanged { mode: Mode::Bypass }));
            assert_eq!(session.status().mode, Mode::Bypass);
            assert_eq!(chosen(), Some(Mode::AcceptEdits), "never stored");
            assert_eq!(
                session.wanted_mode(),
                Mode::AcceptEdits,
                "a fork gets the safe one"
            );

            session.switch(Driver::Codex, Some("gpt-5-codex".into()), None);
            assert_eq!(session.status().driver, Driver::Codex);
            assert_eq!(session.status().mode, Mode::AcceptEdits);
            assert_eq!(session.inner.open.borrow().mode, Mode::AcceptEdits);
            assert!(has(&seen, |e| matches!(e, Event::Notice { text }
                if text.starts_with("Dangerous permissive mode has ended"))));
            assert_eq!(chosen(), Some(Mode::AcceptEdits));
        });
    }

    /// Hookless agy has nothing to approve through: the pick is refused, and the flag never set.
    #[test]
    fn hookless_agy_refuses_dangerous_permissive_mode() {
        in_loop(|_| {
            let log: Log = Rc::default();
            let (session, seen) = claude_on(Mode::Ask, &log);
            session.switch(Driver::Agy, Some("gemini-flash".into()), None);
            let before = session.status().mode;
            session.set_mode(Mode::Bypass);
            assert_eq!(session.status().mode, before);
            assert!(!log
                .borrow()
                .iter()
                .any(|c| matches!(c, Command::SetMode { mode: Mode::Bypass })));
            assert!(has(&seen, |e| matches!(e, Event::Notice { text }
                if text.contains("cannot run in dangerous permissive mode"))));
        });
    }

    /// A side process (agy's usage, say) that finishes after the thread switched agent answers
    /// its request with an error; its output never reaches the new agent's adapter.
    #[test]
    fn a_side_result_from_before_a_switch_is_answered_not_fed_to_the_new_agent() {
        in_loop(|_| {
            let (_, _, _, session, seen, _) =
                agy_thread_switching_to(FakeAdapter::unboxed_dyn, FakeAdapter::unboxed_dyn);
            let before = session.inner.adapter_epoch.get();
            session.switch(Driver::Claude, Some("opus".into()), None);
            assert_ne!(session.inner.adapter_epoch.get(), before);

            let errors = |id: &str| {
                seen.borrow()
                    .iter()
                    .filter(|e| {
                        e.request.as_deref() == Some(id)
                            && matches!(&e.event, Event::ControlResult { error: Some(_), .. })
                    })
                    .count()
            };
            session
                .inner
                .finish_side(before, "ctl-old", "{\"usage\":1}", true);
            assert_eq!(errors("ctl-old"), 1, "the stale request is answered");

            // A result for the current agent goes to its adapter as before (the fake adapter
            // answers nothing).
            let now = session.inner.adapter_epoch.get();
            session.inner.finish_side(now, "ctl-new", "{}", true);
            assert_eq!(errors("ctl-new"), 0);
        });
    }

    /// The notice says how much of the thread a handoff carried, and what never travels.
    #[test]
    fn a_handoff_notice_says_how_much_was_carried() {
        let h = |carried, total| Handoff {
            summary: String::new(),
            fence: String::new(),
            carried,
            total,
        };
        let all = handoff_notice("in Codex", &h(12, 12));
        assert!(
            all.starts_with("Continuing in Codex with all 12 earlier messages."),
            "{all}"
        );
        let some = handoff_notice("in Codex", &h(40, 112));
        assert!(
            some.starts_with("Continuing in Codex with 40 of 112 earlier messages"),
            "{some}"
        );
        assert!(some.contains("not carried"), "{some}");
        let one = handoff_notice("in Codex", &h(1, 1));
        assert!(
            one.starts_with("Continuing in Codex with the 1 earlier message."),
            "{one}"
        );
    }

    #[test]
    fn a_hook_not_checked_yet_is_not_called_missing() {
        let unchecked = hookless_text(Some(HOOK_NOT_CHECKED));
        assert!(unchecked.contains("not been checked yet"), "{unchecked}");
        assert!(!unchecked.contains("not installed"), "{unchecked}");
        assert!(hookless_text(None).contains("not installed"));
    }

    #[test]
    fn a_refused_hook_proof_says_why_without_claiming_it_is_installed() {
        for reason in [
            "cannot verify agy's approval hook: ANTIGRAVITY_APP_DATA_DIR is set and may redirect its configuration",
            crate::hook_config::DIFFERENT_HOME,
        ] {
            let text = hookless_text(Some(reason));
            assert!(text.contains(reason), "{text}");
            assert!(!text.contains("installed"), "{text}");
            assert!(text.contains("applies file edits"), "{text}");
        }
    }

    /// An agy thread with one user message and a known native session, a sink, and logs of what
    /// each agent was sent; the factory launches Claude with `claude` and agy with `agy`.
    fn agy_thread_switching_to(
        claude: fn(Driver, &Log) -> Box<dyn Adapter>,
        agy: fn(Driver, &Log) -> Box<dyn Adapter>,
    ) -> (
        Rc<Store>,
        ThreadId,
        ProviderThreadId,
        Rc<ChatSession>,
        Seen,
        Log,
    ) {
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let thread = fresh(&store, "/w");
        let first = pt_of(&store, &thread);
        store
            .append_user_message(&thread, Some(&first), "please list the repo")
            .expect("user");
        let (sink, seen) = make_sink();
        let log_agy: Log = Rc::default();
        let log_claude: Log = Rc::default();
        let session = ChatSession::new(
            FakeAdapter::boxed(Driver::Agy, &log_agy),
            OpenSession {
                program: "unused".into(),
                extra_args: Vec::new(),
                cwd: "/".into(),
                model: Some("gemini-pro".into()),
                effort: None,
                mode: Mode::Plan,
                resume: None,
                new_session_id: None,
                approval_hook: false,
                delegation: None,
            },
            store.clone(),
            thread.clone(),
            sink,
            None,
        );
        // agy is up, on its own native session.
        session.inner.emit(Envelope::new(Event::SessionStarted {
            native_id: "agy-conv-1".into(),
            model: None,
            cwd: None,
        }));
        let (ag, cl) = (log_agy.clone(), log_claude);
        session.set_adapter_factory(Rc::new(move |d| match d {
            Driver::Claude => launch_of(claude(d, &cl)),
            _ => launch_of(agy(d, &ag)),
        }));
        (store, thread, first, session, seen, log_agy)
    }

    /// A switch whose new agent never starts takes the thread back to the old one, resuming its
    /// own session, with nothing left owed on reopen.
    #[test]
    fn a_switch_whose_new_agent_cannot_spawn_goes_back_to_the_old_one() {
        in_loop(|_| {
            let (store, thread, first, session, seen, log_agy) =
                agy_thread_switching_to(FakeAdapter::unspawnable, FakeAdapter::unboxed_dyn);
            session.switch(Driver::Claude, Some("opus".into()), None);

            let status = session.status();
            assert_eq!(status.driver, Driver::Agy, "rolled back");
            assert!(status.alive, "the old agent runs again");
            assert_eq!(
                store.active_provider_thread(&thread).expect("active"),
                Some(first)
            );
            assert_eq!(
                session.inner.open.borrow().resume.as_deref(),
                Some("agy-conv-1"),
                "its own session resumes"
            );
            assert!(has(&seen, |e| matches!(e, Event::Error { message }
                if message.contains("did not start") && message.contains("Back on"))));
            let events = store.events(&thread, None, 10_000).expect("events");
            assert!(undelivered_handoff(events.iter().map(|(_, e)| e)).is_none());
            session.send_prompt("go on");
            assert_eq!(prompts(&log_agy).last().map(String::as_str), Some("go on"));
        });
    }

    /// Neither agent can be spawned: the rollback reports both failures and stops there, with no
    /// panic and no loop.
    #[test]
    fn a_rollback_whose_old_agent_fails_too_reports_it_and_stops() {
        in_loop(|_| {
            let (_, _, _, session, seen, _) =
                agy_thread_switching_to(FakeAdapter::unspawnable, FakeAdapter::unspawnable);
            session.switch(Driver::Claude, None, None);
            assert_eq!(session.status().driver, Driver::Agy, "rolled back");
            assert!(!session.status().alive, "and agy could not start either");
            assert!(
                session.inner.rollback.borrow().is_none(),
                "no second rollback"
            );
            assert!(has(&seen, |e| matches!(e, Event::Error { message }
                if message.contains("Back on"))));
        });
    }

    #[test]
    fn a_new_agent_that_exits_before_starting_rolls_back_but_not_once_it_started() {
        in_loop(|_| {
            // It spawns, then exits before reporting a session (signed out, say).
            let (store, thread, first, session, _, _) =
                agy_thread_switching_to(FakeAdapter::unboxed_dyn, FakeAdapter::unboxed_dyn);
            session.switch(Driver::Claude, Some("opus".into()), None);
            assert_eq!(session.status().driver, Driver::Claude);
            let generation = session.inner.generation.get();
            session.inner.on_exit(generation, Some(1));
            assert_eq!(session.status().driver, Driver::Agy);
            assert_eq!(
                store.active_provider_thread(&thread).expect("active"),
                Some(first)
            );

            // Once the new agent reported its session, an exit is an ordinary one.
            let (_, _, _, session, _, _) =
                agy_thread_switching_to(FakeAdapter::unboxed_dyn, FakeAdapter::unboxed_dyn);
            session.switch(Driver::Claude, Some("opus".into()), None);
            session.inner.emit(Envelope::new(Event::SessionStarted {
                native_id: "claude-1".into(),
                model: None,
                cwd: None,
            }));
            let generation = session.inner.generation.get();
            session.inner.on_exit(generation, Some(1));
            assert_eq!(session.status().driver, Driver::Claude, "no rollback");
        });
    }

    #[test]
    fn an_undelivered_handoff_is_the_latest_seeded_with_no_completed_turn_after_it() {
        // Written before acknowledgements existed: any completed turn spends it.
        let seeded = |fence: &str| {
            Envelope::new(Event::HandoffSeeded {
                summary: format!("summary {fence}"),
                fence: fence.to_owned(),
                carried: 2,
                total: 3,
                acknowledged: false,
            })
        };
        let turn = |state| {
            Envelope::new(Event::TurnCompleted {
                state,
                usage: None,
                cost_usd: None,
                error: None,
            })
        };
        let pending = |events: &[Envelope]| undelivered_handoff(events).map(|h| h.fence);
        let handoff = |fence: &str| Handoff {
            summary: format!("summary {fence}"),
            fence: fence.to_owned(),
            carried: 2,
            total: 3,
        };

        assert_eq!(pending(&[]), None);
        assert_eq!(pending(&[seeded("a")]).as_deref(), Some("a"));
        assert_eq!(pending(&[seeded("a"), turn(TurnState::Completed)]), None);
        // Written now: only its acknowledgement spends it, never a completed turn alone
        // (Claude's prompt-less init turn carried nothing).
        let new = handoff_seeded(&handoff("n"));
        assert_eq!(
            pending(&[new.clone(), turn(TurnState::Completed)]).as_deref(),
            Some("n")
        );
        assert_eq!(
            pending(&[new.clone(), turn(TurnState::Completed), handoff_spent()]),
            None
        );
        // A legacy seed replaced by a new one follows the new one's rule.
        assert_eq!(
            pending(&[seeded("a"), new, turn(TurnState::Completed)]).as_deref(),
            Some("n")
        );
        // A turn that did not complete proves nothing.
        assert_eq!(
            pending(&[
                seeded("a"),
                turn(TurnState::Failed),
                turn(TurnState::Interrupted)
            ])
            .as_deref(),
            Some("a")
        );
        // Switched again before anything completed: the newer handoff is the one owed.
        assert_eq!(pending(&[seeded("a"), seeded("b")]).as_deref(), Some("b"));
        // A completed turn before the switch says nothing about the handoff after it.
        assert_eq!(
            pending(&[turn(TurnState::Completed), seeded("c")]).as_deref(),
            Some("c")
        );
        // The full value comes back, not only the fence.
        let h = undelivered_handoff(&[seeded("d")]).expect("pending");
        assert_eq!(
            (h.summary.as_str(), h.carried, h.total),
            ("summary d", 2, 3)
        );
    }

    // ---- canary, forced plan, ordering ----

    /// A state-changing agy step, as agy prints it before (or without) any hook query.
    const STEP: &str = r#"{"event":"step_update","conversation_id":"c1","step_update":{"conversation_id":"c1","state":"RUNNING","step_index":1,"step_type":"tool","tool_name":"run_command","tool_info":{"parameters":{"CommandLine":"touch x"}}}}"#;

    /// A shell "agent" that logs its arguments and approval env, then runs `body`.
    fn scripted(dir: &Path, body: &str) -> OpenSession {
        let script = dir.join("agent.sh");
        let text = format!(
            "D=$(dirname \"$0\")\n\
             echo \"$@\" >> \"$D/args.log\"\n\
             echo \"${{AGENT_TERMINAL_APPROVAL_SOCKET-unset}}\" >> \"$D/sock.log\"\n\
             {body}\n"
        );
        std::fs::write(&script, text).expect("script");
        OpenSession {
            program: "/bin/sh".into(),
            extra_args: vec![script.to_string_lossy().into_owned()],
            cwd: dir.to_string_lossy().into_owned(),
            model: None,
            effort: None,
            mode: Mode::Ask,
            resume: None,
            new_session_id: None,
            approval_hook: false,
            delegation: None,
        }
    }

    /// A Claude known to take `--thinking-display` gets it; one that exits at start with
    /// an unknown-option error is restarted ONCE without it, its reasoning shows as not shared,
    /// and the session starts with no error shown.
    #[test]
    fn claude_refusing_the_thinking_flag_restarts_once_without_it() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, &tmp.path().to_string_lossy());
            let (sink, seen) = make_sink();
            let open = scripted(
                tmp.path(),
                "IFS= read -r handshake\n\
                 IFS= read -r line\n\
                 printf '%s\\n' \"$line\" >> \"$D/prompts.log\"\n\
                 IFS= read -r line\n\
                 printf '%s\\n' \"$line\" >> \"$D/prompts.log\"\n\
                 case \"$*\" in *--thinking-display*) \
                 echo \"error: unknown option '--thinking-display'\" >&2; exit 1;; esac\n\
                 echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s1\",\"model\":\"m\",\"claude_code_version\":\"2.1.293\"}'\n\
                 exec sleep 30",
            );
            // The probe said this binary is a version that takes the flag.
            crate::claude_probe::note_version(&open.program, "2.1.293 (Claude Code)");
            let session = ChatSession::new(
                Box::new(agent_core::claude::ClaudeAdapter::new()),
                open,
                store,
                thread,
                sink,
                None,
            );
            session.inner.arm_handoff(Handoff {
                summary: "earlier context".into(),
                fence: "retry-fence".into(),
                carried: 1,
                total: 1,
            });
            session.send_prompt("sent before init");
            session.steer_prompt("also sent before init");
            let log = tmp.path().join("args.log");
            assert!(pump_until(ctx, 10, || {
                session.status().alive
                    && std::fs::read_to_string(&log).is_ok_and(|l| l.lines().count() == 2)
                    && lines(tmp.path().join("prompts.log")).len() == 4
            }));
            let sent = lines(tmp.path().join("prompts.log"));
            assert_eq!(
                sent[0], sent[2],
                "the original prompt and handoff are replayed once"
            );
            assert_eq!(sent[1], sent[3], "pre-init live input is replayed once too");
            assert!(sent[2].contains("sent before init"));
            assert!(sent[2].contains("earlier context"));
            assert!(sent[3].contains("also sent before init"));
            assert!(!sent[3].contains("earlier context"));
            assert_eq!(
                session
                    .inner
                    .store
                    .read(|s| s.transcript_messages(&session.inner.thread))
                    .expect("read")
                    .expect("messages")
                    .len(),
                2,
                "replaying input never stores it twice"
            );
            let launches = std::fs::read_to_string(&log).expect("args");
            let launches: Vec<&str> = launches.lines().collect();
            assert!(
                launches[0].contains("--thinking-display summarized"),
                "{launches:?}"
            );
            assert!(!launches[1].contains("--thinking-display"), "{launches:?}");
            let seen = seen.borrow();
            assert!(
                seen.iter().any(|e| e.event
                    == Event::Reasoning {
                        state: agent_core::event::ReasoningState::Unavailable,
                        label: agent_core::event::ReasoningLabel::Thinking,
                    }
                    && e.item
                        .as_deref()
                        .is_some_and(|i| i.starts_with("thinking-unavailable"))),
                "the block says the thinking is not shared"
            );
            assert!(
                !seen.iter().any(|e| matches!(&e.event, Event::Error { .. })
                    || matches!(
                        e.event,
                        Event::SessionExited {
                            expected: false,
                            ..
                        }
                    )),
                "no error for the refused flag: {seen:?}"
            );
            drop(seen);
            // Never passed again in this thread, and the binary is not offered it either.
            assert!(!session
                .launch_spec()
                .argv
                .iter()
                .any(|a| a == "--thinking-display"));
            assert!(!crate::claude_probe::thinking_display_allowed("/bin/sh"));
        });
    }

    /// The retry is for a failed exit before the CLI started. A Claude that sent its
    /// `init` and then exits, or one that exits 0, takes the normal path: no restart.
    #[test]
    fn the_thinking_retry_needs_a_failed_exit_before_init() {
        for (case, body) in [
            (
                "after init",
                "echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s1\",\"model\":\"m\",\"claude_code_version\":\"2.1.293\"}'\n\
                 echo \"error: unknown option '--thinking-display'\" >&2\nsleep 0.2\nexit 1",
            ),
            (
                "exit 0",
                "echo \"error: unknown option '--thinking-display'\" >&2\nexit 0",
            ),
        ] {
            in_loop(|ctx| {
                let tmp = tempfile::tempdir().expect("tmp");
                let store = Rc::new(Store::open_in_memory().expect("store"));
                let thread = fresh(&store, &tmp.path().to_string_lossy());
                let (sink, seen) = make_sink();
                let open = scripted(tmp.path(), body);
                crate::claude_probe::note_version(&open.program, "2.1.293");
                let session = ChatSession::new(
                    Box::new(agent_core::claude::ClaudeAdapter::new()),
                    open,
                    store,
                    thread,
                    sink,
                    None,
                );
                assert!(
                    pump_until(ctx, 10, || seen
                        .borrow()
                        .iter()
                        .any(|e| matches!(e.event, Event::SessionExited { .. }))),
                    "{case}: exit seen"
                );
                // Give a (wrong) restart the chance to happen.
                pump_until(ctx, 1, || false);
                let launches = std::fs::read_to_string(tmp.path().join("args.log")).expect("args");
                assert_eq!(launches.lines().count(), 1, "{case}: no retry");
                assert!(launches.contains("--thinking-display"), "{case}");
                assert!(
                    !seen
                        .borrow()
                        .iter()
                        .any(|e| matches!(e.event, Event::Reasoning { .. })),
                    "{case}: no not-shared block"
                );
                assert!(!session.status().alive, "{case}");
            });
        }
    }

    fn bound_handle(dir: &Path) -> ApprovalHandle {
        ApprovalHandle::bind(
            &dir.join("rt"),
            "t1",
            dir,
            Mode::Ask,
            Duration::from_secs(30),
        )
        .expect("bind")
    }

    #[test]
    fn agy_mode_change_waits_for_the_turn() {
        in_loop(|ctx| {
            use std::io::{BufRead as _, Write as _};
            let tmp = tempfile::tempdir().expect("tmp");
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, &tmp.path().to_string_lossy());
            let (sink, seen) = make_sink();
            let handle = bound_handle(tmp.path());
            let socket = handle.socket_path().to_owned();
            let session = ChatSession::new(
                Box::new(AgyAdapter::new("agy")),
                scripted(tmp.path(), "cat > \"$D/stdin.log\""),
                store,
                thread,
                sink,
                Some(handle),
            );
            session.send_prompt("work");
            let generation = session.inner.generation.get();
            session.set_mode(Mode::Plan);
            assert_eq!(session.inner.generation.get(), generation, "no stop");
            assert!(session.status().running_turn);
            assert_eq!(session.inner.deferred_mode.get(), Some(Mode::Plan));
            assert!(has(&seen, |e| matches!(
                e,
                Event::ModeChangeDeferred {
                    requested: Mode::Plan,
                    effective: Mode::Plan
                }
            )));
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut hook = std::os::unix::net::UnixStream::connect(socket).expect("connect");
                hook.set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("timeout");
                let query = agent_core::approval::ApprovalQuery {
                    id: "edit".into(),
                    conversation_id: "c1".into(),
                    tool: "write_to_file".into(),
                    args: serde_json::json!({"TargetFile": "work.rs", "CodeContent": "x"}),
                    cwd: None,
                };
                writeln!(
                    hook,
                    "{}",
                    agent_core::approval::encode_query(&query).expect("query")
                )
                .expect("send");
                let mut reply = String::new();
                std::io::BufReader::new(hook)
                    .read_line(&mut reply)
                    .expect("read");
                tx.send(agent_core::approval::parse_reply(&reply).expect("reply"))
                    .expect("send reply");
            });
            let mut reply = None;
            assert!(pump_until(ctx, 5, || {
                reply = rx.try_recv().ok();
                reply.is_some()
            }));
            assert_eq!(
                reply.expect("reply").decision,
                Decision::Deny,
                "hook changed immediately"
            );
            session.inner.emit(Envelope::new(Event::TurnCompleted {
                state: TurnState::Completed,
                error: None,
                usage: None,
                cost_usd: None,
            }));
            assert_eq!(
                session.inner.generation.get(),
                generation + 2,
                "one stop and one start"
            );
            assert_eq!(session.inner.deferred_mode.get(), None);
            let argv = session.launch_spec().argv;
            assert!(argv.windows(2).any(|a| a == ["--mode", "plan"]), "{argv:?}");
        });
    }

    #[test]
    fn a_deferred_agy_mode_restarts_before_queued_input() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (session, seen) = recording(ctx, tmp.path(), Box::new(AgyAdapter::new("agy")));
            session.send_prompt("first");
            session.queue_prompt("second");
            let generation = session.inner.generation.get();
            // Exercise the hooked deferral path without binding a socket in the sandbox.
            session.inner.defer_mode(Mode::AcceptEdits);
            session.inner.defer_mode(Mode::Plan);
            assert_eq!(session.inner.generation.get(), generation);
            let mut model = crate::chat::view::model::Transcript::new();
            for env in seen.borrow().iter() {
                model.apply(env, Driver::Agy);
            }
            assert_eq!(model.pending_mode, Some(Mode::Plan));
            session.inner.emit(Envelope::new(Event::TurnCompleted {
                state: TurnState::Completed,
                error: None,
                usage: None,
                cost_usd: None,
            }));
            assert_eq!(session.inner.generation.get(), generation + 2);
            assert_eq!(session.inner.deferred_mode.get(), None);
            assert!(pump_until(ctx, 5, || stdin_log(tmp.path()).contains("second")));
            let argv = session.launch_spec().argv;
            assert!(argv.windows(2).any(|a| a == ["--mode", "plan"]), "{argv:?}");
            assert!(
                session.status().running_turn,
                "queued input opens the next turn"
            );
            for env in seen.borrow().iter() {
                model.apply(env, Driver::Agy);
            }
            assert_eq!(model.pending_mode, None);
        });
    }

    #[test]
    fn hookless_agy_reselecting_its_mode_keeps_the_turn() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (session, _) = recording(ctx, tmp.path(), Box::new(AgyAdapter::new("agy")));
            session.send_prompt("first");
            let generation = session.inner.generation.get();
            assert_eq!(session.status().mode, Mode::Plan);
            assert!(!session.status().capabilities.live_approvals);
            session.set_mode(Mode::Plan);
            assert_eq!(session.inner.generation.get(), generation);
            assert!(session.status().running_turn);
            assert_eq!(session.inner.deferred_mode.get(), None);
        });
    }

    #[test]
    fn a_confirmed_hookless_agy_mode_change_continues_before_queued_input() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (session, _) = recording(ctx, tmp.path(), Box::new(AgyAdapter::new("agy")));
            session.send_prompt("first");
            session.queue_prompt("second");
            let generation = session.inner.generation.get();
            // The view calls the backend only after Switch and Continue was confirmed.
            session.set_mode(Mode::AcceptEdits);
            assert_eq!(session.inner.generation.get(), generation + 2);
            assert!(pump_until(ctx, 5, || stdin_log(tmp.path())
                .contains("Continue the unfinished task")));
            assert!(!stdin_log(tmp.path()).contains("second"));
            assert!(session.status().running_turn);
            session.inner.emit(Envelope::new(Event::TurnCompleted {
                state: TurnState::Completed,
                error: None,
                usage: None,
                cost_usd: None,
            }));
            assert!(pump_until(ctx, 5, || stdin_log(tmp.path()).contains("second")));
            assert_eq!(stdin_log(tmp.path()).matches("second").count(), 1);
        });
    }

    #[test]
    fn a_reload_adopts_the_deferred_agy_mode_without_an_extra_restart() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (session, seen) = recording(ctx, tmp.path(), Box::new(AgyAdapter::new("agy")));
            session.send_prompt("first");
            session.inner.defer_mode(Mode::AcceptEdits);
            let generation = session.inner.generation.get();
            session.reload_session();
            assert_eq!(session.inner.generation.get(), generation + 2);
            assert_eq!(session.inner.deferred_mode.get(), None);
            assert_eq!(session.status().mode, Mode::AcceptEdits);
            let mut model = crate::chat::view::model::Transcript::new();
            for env in seen.borrow().iter() {
                model.apply(env, Driver::Agy);
            }
            assert_eq!(model.pending_mode, None);
            session.inner.emit(Envelope::new(Event::TurnCompleted {
                state: TurnState::Completed,
                error: None,
                usage: None,
                cost_usd: None,
            }));
            assert_eq!(
                session.inner.generation.get(),
                generation + 2,
                "no deferred restart remains"
            );
        });
    }

    #[test]
    fn hookless_agy_mode_change_keeps_its_choice_when_the_store_read_fails() {
        in_loop(|ctx| {
            use agent_kit::store_writer::{Shutdown, StoreWriter};
            let tmp = tempfile::tempdir().expect("tmp");
            let path = tmp.path().join("threads.sqlite");
            let store = Rc::new(Store::open(&path).expect("store"));
            let thread = fresh(&store, &tmp.path().to_string_lossy());
            let writer = StoreWriter::spawn(&path).expect("writer");
            let handle = StoreHandle::Queued {
                reader: store.clone(),
                writer: writer.clone(),
            };
            let (sink, seen) = make_sink();
            let session = ChatSession::new(
                Box::new(AgyAdapter::new("agy")),
                scripted(tmp.path(), "cat > \"$D/stdin.log\""),
                handle,
                thread,
                sink,
                None,
            );
            session.send_prompt("first");
            session.queue_prompt("second");
            assert!(pump_until(ctx, 5, || stdin_log(tmp.path()).contains("first")));
            let generation = session.inner.generation.get();
            let argv = session.launch_spec().argv;
            let chosen = seen
                .borrow()
                .iter()
                .filter(|e| matches!(e.event, Event::ModeChosen { .. }))
                .count();
            assert_eq!(writer.shutdown(Duration::from_secs(5)), Shutdown::Drained);
            assert!(
                session.inner.store.read(|_| ()).is_err(),
                "the read really fails"
            );
            session.set_mode(Mode::AcceptEdits);
            assert_eq!(
                session.inner.wanted_mode.get(),
                Mode::Ask,
                "initial Ask is mapped to Plan without the hook"
            );
            assert_eq!(session.inner.safe_mode.get(), Mode::Ask);
            assert_eq!(session.status().mode, Mode::Plan);
            assert_eq!(session.inner.open.borrow().mode, Mode::Plan);
            assert_eq!(session.inner.deferred_mode.get(), None);
            assert_eq!(session.inner.pending_handoff.borrow().as_ref(), None);
            assert_eq!(session.inner.generation.get(), generation);
            assert_eq!(session.launch_spec().argv, argv);
            assert!(session.status().running_turn);
            assert_eq!(session.inner.queued_prompts.borrow().len(), 1);
            assert_eq!(
                seen.borrow()
                    .iter()
                    .filter(|e| matches!(e.event, Event::ModeChosen { .. }))
                    .count(),
                chosen
            );
            assert!(has(&seen, |e| matches!(e, Event::Error { message }
                if message.contains("Could not read the unfinished task to continue"))));
        });
    }

    /// An agy thread in Ask keeps Ask through a model switch and through a
    /// compact-style same-agent restart onto a launch whose profile default is another mode.
    /// No mode change is announced, and the restart never passes `--mode plan`.
    #[test]
    fn agy_model_switch_and_compact_keep_the_threads_mode() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, &tmp.path().to_string_lossy());
            let (sink, seen) = make_sink();
            let handle = bound_handle(tmp.path());
            let session = ChatSession::new(
                Box::new(AgyAdapter::new("agy")),
                scripted(tmp.path(), "exec sleep 30"),
                store,
                thread,
                sink,
                Some(handle),
            );
            let dir = tmp.path().to_owned();
            session.set_adapter_factory(Rc::new(move |_| {
                // The profile's default mode (Plan) is what a fresh launch binds the hook to.
                let profile_default = ApprovalHandle::bind(
                    &dir.join("rt2"),
                    "t2",
                    &dir,
                    Mode::Plan,
                    Duration::from_secs(30),
                )
                .expect("bind");
                Ok(AgentLaunch {
                    adapter: Box::new(AgyAdapter::new("agy")),
                    program: "/bin/sh".into(),
                    extra_args: vec![dir.join("agent.sh").to_string_lossy().into_owned()],
                    default_model: None,
                    default_effort: None,
                    env: LaunchEnv::default(),
                    approval: Ok(profile_default),
                })
            }));
            assert!(pump_until(ctx, 10, || session.status().alive));
            assert_eq!(session.status().mode, Mode::Ask);
            let modes = |seen: &Seen| {
                seen.borrow()
                    .iter()
                    .filter(|e| matches!(e.event, Event::ModeChanged { .. }))
                    .count()
            };
            let before = modes(&seen);

            session.switch(Driver::Agy, Some("gemini-flash".into()), None);
            assert_eq!(session.status().mode, Mode::Ask, "model switch");
            let argv = session.launch_spec().argv;
            assert!(!argv.iter().any(|a| a == "plan"), "{argv:?}");
            assert!(
                argv.iter().any(|a| a == "--dangerously-skip-permissions"),
                "Ask with the hook is skip-permissions behind the hook: {argv:?}"
            );

            // Compact by handoff restarts the same agent on a fresh launch.
            session.inner.create_with_handoff(Driver::Agy, None, None);
            assert_eq!(session.status().mode, Mode::Ask, "compact");
            let argv = session.launch_spec().argv;
            assert!(!argv.iter().any(|a| a == "plan"), "{argv:?}");
            assert_eq!(modes(&seen), before, "no silent or announced mode change");
        });
    }

    fn hook_missing_error(seen: &Seen) -> bool {
        has(
            seen,
            |e| matches!(e, Event::Error { message } if message.contains("approval hook is not active")),
        )
    }

    #[test]
    fn a_tool_with_no_hook_query_stops_agy_and_restarts_it_read_only() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, "/w");
            let (sink, seen) = make_sink();
            let handle = bound_handle(tmp.path());
            let socket = handle.socket_path().to_owned();
            let open = scripted(tmp.path(), &format!("echo '{STEP}'\nexec sleep 30"));
            let session = ChatSession::new(
                Box::new(AgyAdapter::new("agy")),
                open,
                store,
                thread,
                sink,
                Some(handle),
            );
            session.set_canary_grace(Duration::from_millis(200));
            assert!(
                pump_until(ctx, 15, || hook_missing_error(&seen)),
                "no canary"
            );
            assert!(pump_until(ctx, 15, || {
                lines(tmp.path().join("args.log")).len() == 2
            }));
            let args = lines(tmp.path().join("args.log"));
            assert!(
                args[0].contains("--dangerously-skip-permissions"),
                "{args:?}"
            );
            assert!(
                !args[1].contains("--dangerously-skip-permissions"),
                "{args:?}"
            );
            assert!(args[1].contains("--mode plan"), "{args:?}");
            let socks = lines(tmp.path().join("sock.log"));
            assert_eq!(socks[1], "unset", "the restart has no approval socket");
            assert_eq!(session.status().mode, Mode::Plan);
            assert!(session.status().alive);
            assert!(!socket.exists(), "the approval socket is closed");
        });
    }

    /// A tool step whose agy is gone before the grace ends (it exited, or the thread was stopped)
    /// trips nothing: the canary watches a running process, and must not start one in Plan mode.
    #[test]
    fn the_canary_never_restarts_an_agy_that_already_stopped() {
        for stop_it in [false, true] {
            in_loop(|ctx| {
                let tmp = tempfile::tempdir().expect("tmp");
                let store = Rc::new(Store::open_in_memory().expect("store"));
                let thread = fresh(&store, "/w");
                let (sink, seen) = make_sink();
                let handle = bound_handle(tmp.path());
                // Exits on its own, or lives until the test stops it.
                let tail = if stop_it { "\nexec sleep 30" } else { "" };
                let open = scripted(tmp.path(), &format!("echo '{STEP}'{tail}"));
                let session = ChatSession::new(
                    Box::new(AgyAdapter::new("agy")),
                    open,
                    store,
                    thread,
                    sink,
                    Some(handle),
                );
                session.set_canary_grace(Duration::from_millis(200));
                let started = |seen: &Seen| has(seen, |e| matches!(e, Event::ItemStarted { .. }));
                assert!(pump_until(ctx, 15, || started(&seen)), "no tool step");
                if stop_it {
                    session.inner.stop_current();
                }
                assert!(
                    pump_until(ctx, 15, || !session.status().alive),
                    "agy still running"
                );
                // Well past the grace: nothing trips, nothing starts again.
                assert!(
                    !pump_until(ctx, 1, || hook_missing_error(&seen)),
                    "the canary tripped on a stopped agy (stop_it: {stop_it})"
                );
                assert_eq!(
                    lines(tmp.path().join("args.log")).len(),
                    1,
                    "agy was started again (stop_it: {stop_it})"
                );
                assert!(!session.status().alive);
            });
        }
    }

    #[test]
    fn agy_stop_turn_denies_the_hook_and_interrupts_the_turn() {
        in_loop(|ctx| {
            use std::io::{BufRead as _, Write as _};
            let tmp = tempfile::tempdir().expect("tmp");
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, "/w");
            let (sink, seen) = make_sink();
            let handle = bound_handle(tmp.path());
            let socket = handle.socket_path().to_owned();
            let log: Log = Rc::default();
            let session = ChatSession::new(
                FakeAdapter::boxed(Driver::Agy, &log),
                scripted(tmp.path(), "exec sleep 30"),
                store,
                thread,
                sink,
                Some(handle),
            );
            let mut hook = std::os::unix::net::UnixStream::connect(&socket).expect("connect");
            let query = agent_core::approval::ApprovalQuery {
                id: "h1".into(),
                conversation_id: "c1".into(),
                tool: "run_command".into(),
                args: serde_json::json!({"CommandLine": "git push"}),
                cwd: None,
            };
            writeln!(
                hook,
                "{}",
                agent_core::approval::encode_query(&query).expect("enc")
            )
            .expect("send");
            assert!(pump_until(ctx, 10, || has(&seen, |e| {
                matches!(e, Event::ApprovalRequested { options, .. } if options.contains(&Decision::Cancel))
            })));
            session.respond_approval("h1", Decision::Cancel);
            assert_eq!(resolution(&seen, "h1"), Some(Decision::Cancel));
            assert!(
                log.borrow().iter().any(|c| matches!(c, Command::Interrupt)),
                "the turn is interrupted like the stop button"
            );
            hook.set_nonblocking(true).expect("nonblocking");
            let mut reader = std::io::BufReader::new(&hook);
            let mut reply = String::new();
            assert!(pump_until(ctx, 10, || {
                let _ = reader.read_line(&mut reply);
                reply.ends_with('\n')
            }));
            let reply = agent_core::approval::parse_reply(&reply).expect("reply");
            assert_eq!(reply.decision, Decision::Deny, "agy hears a deny");
        });
    }

    #[test]
    fn a_second_or_expired_agy_answer_sends_no_approval_command() {
        for expired in [false, true] {
            in_loop(|ctx| {
                use std::io::Write as _;
                let tmp = tempfile::tempdir().expect("tmp");
                let store = Rc::new(Store::open_in_memory().expect("store"));
                let thread = fresh(&store, "/w");
                let (sink, seen) = make_sink();
                let handle = bound_handle(tmp.path());
                let log: Log = Rc::default();
                let session = ChatSession::new(
                    FakeAdapter::boxed(Driver::Agy, &log),
                    scripted(tmp.path(), "exec sleep 30"),
                    store,
                    thread,
                    sink,
                    Some(handle.clone()),
                );
                let mut hook =
                    std::os::unix::net::UnixStream::connect(handle.socket_path()).expect("connect");
                let query = agent_core::approval::ApprovalQuery {
                    id: "h1".into(),
                    conversation_id: "c1".into(),
                    tool: "run_command".into(),
                    args: serde_json::json!({"CommandLine": "ls"}),
                    cwd: None,
                };
                writeln!(
                    hook,
                    "{}",
                    agent_core::approval::encode_query(&query).expect("encode")
                )
                .expect("send");
                assert!(pump_until(ctx, 10, || handle.has_pending("h1")));
                if expired {
                    handle.expire_all("the approval deadline passed");
                } else {
                    session.respond_approval("h1", Decision::AllowForSession);
                }
                session.respond_approval("h1", Decision::AllowAlways);
                assert!(!log
                    .borrow()
                    .iter()
                    .any(|c| matches!(c, Command::Approve { .. })));
                if !expired {
                    assert!(pump_until(ctx, 10, || resolution(&seen, "h1")
                        == Some(Decision::AllowForSession)));
                }
            });
        }
    }

    #[test]
    fn a_hookless_agy_answer_reports_unsupported_and_never_resolves() {
        in_loop(|_| {
            let tmp = tempfile::tempdir().expect("tmp");
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, "/w");
            let (sink, seen) = make_sink();
            let session = ChatSession::new(
                Box::new(AgyAdapter::new("agy")),
                fake_agy(tmp.path()),
                store,
                thread,
                sink,
                None,
            );
            for decision in [
                Decision::Allow,
                Decision::AllowForSession,
                Decision::AllowAlways,
                Decision::Deny,
                Decision::Cancel,
            ] {
                let before = seen.borrow().len();
                session.respond_approval("gone", decision);
                assert!(
                    seen.borrow()[before..].iter().any(|e| matches!(&e.event,
                    Event::Error { message } if message.contains("not supported"))),
                    "{decision:?}"
                );
                assert_eq!(resolution(&seen, "gone"), None);
                assert!(session.inner.session_rules.borrow().rules.is_empty());
            }
        });
    }

    #[test]
    fn agy_stop_turn_interrupts_after_plan_denies_the_pending_hook_card() {
        in_loop(|ctx| {
            use std::io::Write as _;
            let tmp = tempfile::tempdir().expect("tmp");
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, "/w");
            let (sink, seen) = make_sink();
            let handle = bound_handle(tmp.path());
            let socket = handle.socket_path().to_owned();
            let log: Log = Rc::default();
            let session = ChatSession::new(
                FakeAdapter::boxed(Driver::Agy, &log),
                scripted(tmp.path(), "exec sleep 30"),
                store,
                thread,
                sink,
                Some(handle.clone()),
            );
            session.send_prompt("first");
            let mut hook = std::os::unix::net::UnixStream::connect(socket).expect("connect");
            let query = agent_core::approval::ApprovalQuery {
                id: "h1".into(),
                conversation_id: "c1".into(),
                tool: "run_command".into(),
                args: serde_json::json!({"CommandLine": "touch x"}),
                cwd: None,
            };
            writeln!(
                hook,
                "{}",
                agent_core::approval::encode_query(&query).expect("query")
            )
            .expect("send");
            assert!(pump_until(ctx, 10, || handle.has_pending("h1")));
            session.set_mode(Mode::Plan);
            assert!(!handle.has_pending("h1"));
            assert_eq!(resolution(&seen, "h1"), Some(Decision::Deny));
            session.respond_approval("h1", Decision::Cancel);
            assert_eq!(
                log.borrow()
                    .iter()
                    .filter(|c| matches!(c, Command::Interrupt))
                    .count(),
                1
            );
            assert!(!log
                .borrow()
                .iter()
                .any(|c| matches!(c, Command::Approve { .. })));
            assert_eq!(
                resolution(&seen, "h1"),
                Some(Decision::Deny),
                "a stale Cancel does not overwrite the denial"
            );
            let resolutions = seen
                .borrow()
                .iter()
                .filter_map(|e| match e.event {
                    Event::ApprovalResolved { decision } if e.request.as_deref() == Some("h1") => {
                        Some(decision)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(resolutions, [Decision::Deny]);
        });
    }

    #[test]
    fn a_tool_whose_hook_query_arrived_first_is_left_alone() {
        in_loop(|ctx| {
            use std::io::Write as _;
            let tmp = tempfile::tempdir().expect("tmp");
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, "/w");
            let (sink, seen) = make_sink();
            let handle = bound_handle(tmp.path());
            let socket = handle.socket_path().to_owned();
            // agy prints the step only once the test has sent the hook query.
            let body = format!(
                "while [ ! -e \"$D/go\" ]; do sleep 0.05; done\necho '{STEP}'\nexec sleep 30"
            );
            let session = ChatSession::new(
                Box::new(AgyAdapter::new("agy")),
                scripted(tmp.path(), &body),
                store,
                thread,
                sink,
                Some(handle),
            );
            session.set_canary_grace(Duration::from_millis(200));
            let mut hook = std::os::unix::net::UnixStream::connect(&socket).expect("connect");
            let query = agent_core::approval::ApprovalQuery {
                id: "h1".into(),
                conversation_id: "c1".into(),
                tool: "run_command".into(),
                args: serde_json::json!({"CommandLine": "touch x"}),
                cwd: None,
            };
            writeln!(
                hook,
                "{}",
                agent_core::approval::encode_query(&query).expect("enc")
            )
            .expect("send");
            assert!(pump_until(ctx, 10, || has(&seen, |e| {
                matches!(e, Event::ApprovalRequested { .. })
            })));
            std::fs::write(tmp.path().join("go"), "").expect("go");
            // Long enough for the step to arrive and the grace to pass.
            assert!(
                !pump_until(ctx, 2, || hook_missing_error(&seen)),
                "false positive"
            );
            assert_eq!(lines(tmp.path().join("args.log")).len(), 1, "not restarted");
            assert_eq!(session.status().mode, Mode::Ask);
        });
    }

    #[test]
    fn an_installed_hook_that_could_not_bind_says_why_not_how_to_install() {
        let missing = hookless_text(None);
        assert!(missing.contains("not installed") && missing.contains("hooks.json"));
        let unbound = hookless_text(Some("the running binary was replaced on disk; restart"));
        assert!(unbound.contains("installed, but the running binary was replaced"));
        assert!(!unbound.contains("hooks.json"), "{unbound}");
        assert!(unbound.contains("applies file edits"));
    }

    #[test]
    fn hookless_agy_takes_plan_and_accept_edits_as_flags_but_never_ask() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, "/w");
            let (sink, seen) = make_sink();
            let session = ChatSession::new(
                Box::new(AgyAdapter::new("agy")),
                scripted(tmp.path(), "exec sleep 30"),
                store,
                thread,
                sink,
                None,
            );
            // Asked for Ask (the scripted session's mode): Plan, and the user is told why.
            assert_eq!(session.status().mode, Mode::Plan);
            assert!(has(
                &seen,
                |e| matches!(e, Event::Notice { text } if text.contains("cannot ask you before acting"))
            ));
            assert!(pump_until(ctx, 10, || lines(tmp.path().join("args.log"))
                .len()
                == 1));

            // Accept edits goes to agy's own flag, through a respawn.
            session.set_mode(Mode::AcceptEdits);
            assert_eq!(session.status().mode, Mode::AcceptEdits);
            assert!(pump_until(ctx, 10, || lines(tmp.path().join("args.log"))
                .len()
                == 2));

            // Ask is refused and changes nothing.
            session.set_mode(Mode::Ask);
            assert_eq!(session.status().mode, Mode::AcceptEdits);
            assert!(has(
                &seen,
                |e| matches!(e, Event::Notice { text } if text.contains("cannot ask before edits"))
            ));
            // A model switch (respawn) keeps the mode.
            session.switch(Driver::Agy, Some("gemini-flash".into()), None);
            assert_eq!(session.status().mode, Mode::AcceptEdits);
            assert!(pump_until(ctx, 10, || lines(tmp.path().join("args.log"))
                .len()
                == 3));

            let args = lines(tmp.path().join("args.log"));
            assert!(args[0].contains("--mode plan"), "{args:?}");
            assert!(args[1].contains("--mode accept-edits"), "{args:?}");
            assert!(args[2].contains("--mode accept-edits"), "{args:?}");
            assert!(
                args.iter()
                    .all(|a| !a.contains("--dangerously-skip-permissions")),
                "{args:?}"
            );
        });
    }

    #[test]
    fn a_failed_new_provider_thread_leaves_the_old_agent_running() {
        in_loop(|_| {
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let (sink, seen) = make_sink();
            let log: Log = Rc::default();
            // A thread the store does not know: no provider thread can be created for it.
            let session = ChatSession::new(
                FakeAdapter::boxed(Driver::Agy, &log),
                OpenSession {
                    program: "unused".into(),
                    extra_args: Vec::new(),
                    cwd: "/".into(),
                    model: Some("gemini-pro".into()),
                    effort: None,
                    mode: Mode::Plan,
                    resume: None,
                    new_session_id: None,
                    approval_hook: false,
                    delegation: None,
                },
                store,
                "no-such-thread".into(),
                sink,
                None,
            );
            session.set_adapter_factory(Rc::new(move |d| {
                launch_of(FakeAdapter::boxed(d, &Log::default()))
            }));
            assert!(session.status().alive);
            session.switch(Driver::Claude, Some("opus".into()), None);
            assert!(has(
                &seen,
                |e| matches!(e, Event::Error { message } if message.contains("Could not start"))
            ));
            let status = session.status();
            assert_eq!(status.driver, Driver::Agy, "still the old agent");
            assert!(status.alive, "and still running");
        });
    }

    #[test]
    fn a_switch_to_an_agent_that_is_not_ready_is_refused_and_changes_nothing() {
        in_loop(|_| {
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, "/w");
            let (sink, seen) = make_sink();
            let log: Log = Rc::default();
            let session = ChatSession::new(
                FakeAdapter::boxed(Driver::Agy, &log),
                OpenSession {
                    program: "unused".into(),
                    extra_args: Vec::new(),
                    cwd: "/".into(),
                    model: Some("gemini-pro".into()),
                    effort: None,
                    mode: Mode::Plan,
                    resume: None,
                    new_session_id: None,
                    approval_hook: false,
                    delegation: None,
                },
                store.clone(),
                thread.clone(),
                sink,
                None,
            );
            session.set_adapter_factory(Rc::new(|d| {
                Err(format!("{} is not available", driver_label(d)))
            }));
            session.switch(Driver::Claude, Some("opus".into()), None);
            assert!(has(&seen, |e| matches!(e, Event::Error { message }
                if message == "Claude is not available")));
            assert_eq!(session.status().driver, Driver::Agy, "still the old agent");
            assert!(session.status().alive);
            assert_eq!(store.provider_threads(&thread).expect("pts").len(), 1);
        });
    }

    #[test]
    fn an_adapter_that_cannot_set_the_model_is_respawned_without_a_borrow_panic() {
        in_loop(|_| {
            let tmp = tempfile::tempdir().expect("tmp");
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, &tmp.path().to_string_lossy());
            let (sink, seen) = make_sink();
            let log: Log = Rc::default();
            let session = ChatSession::new(
                FakeAdapter::refusing_set_model(Driver::Agy, &log),
                OpenSession {
                    program: "unused".into(),
                    extra_args: Vec::new(),
                    cwd: "/".into(),
                    model: Some("gemini-pro".into()),
                    effort: None,
                    mode: Mode::Plan,
                    resume: Some("native-1".into()),
                    new_session_id: None,
                    approval_hook: false,
                    delegation: None,
                },
                store,
                thread,
                sink,
                None,
            );
            session.switch(Driver::Agy, Some("gemini-flash".into()), None);
            assert!(has(
                &seen,
                |e| matches!(e, Event::ModelChangeRequested { model } if model == "gemini-flash")
            ));
            let status = session.status();
            assert_eq!(status.model.as_deref(), Some("gemini-pro"));
            assert_eq!(status.pending_model.as_deref(), Some("gemini-flash"));
            assert!(status.alive);
        });
    }

    #[test]
    fn slash_commands_are_recognised_by_their_first_word_only() {
        for yes in ["/model", "/compact now", "  /usage", "/user:thing arg"] {
            assert!(Inner::is_slash_command(yes), "{yes}");
        }
        for no in ["", "/", "/home/me/file.rs explain", "hello /model", "//x"] {
            assert!(!Inner::is_slash_command(no), "{no}");
        }
    }

    #[test]
    fn a_confirmed_model_switch_stops_the_active_turn_and_resumes_with_the_choice() {
        in_loop(|_| {
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, "/w");
            let (sink, seen) = make_sink();
            let log: Log = Rc::default();
            let session = ChatSession::new(
                FakeAdapter::with_open_turn(Driver::Claude, &log),
                open_on("sonnet"),
                store,
                thread,
                sink,
                None,
            );
            session.inner.emit(Envelope::new(Event::SessionStarted {
                native_id: "native-before-switch".into(),
                model: Some("sonnet".into()),
                cwd: None,
            }));
            session.inner.emit(Envelope::new(Event::TurnStarted {
                model: Some("sonnet".into()),
            }));
            let before = session.inner.generation.get();
            session.switch(Driver::Claude, Some("opus".into()), None);
            assert!(
                session.status().running_turn,
                "the replacement turn continues automatically"
            );
            let sent = prompts(&log);
            assert_eq!(sent.len(), 1, "continue exactly once");
            assert!(sent[0].contains("Continue the unfinished task after the model switch."));
            assert_eq!(session.status().pending_model.as_deref(), Some("opus"));
            assert!(session.inner.generation.get() > before);
            assert_eq!(session.inner.open.borrow().model.as_deref(), Some("opus"));
            assert_eq!(
                session.inner.open.borrow().resume.as_deref(),
                Some("native-before-switch")
            );
            assert!(has(&seen, |e| matches!(
                e,
                Event::TurnCompleted {
                    state: TurnState::Interrupted,
                    ..
                }
            )));
        });
    }

    fn pt_of(store: &Store, thread: &str) -> ProviderThreadId {
        create_provider_thread(store, thread, Driver::Agy, Some("gemini-pro")).expect("pt")
    }

    #[test]
    fn submitted_work_switches_before_turn_started_and_carries_the_original_task() {
        in_loop(|_| {
            for native in [None, Some("native-submitted")] {
                let store = Rc::new(Store::open_in_memory().expect("store"));
                let thread = fresh(&store, "/w");
                let (sink, _) = make_sink();
                let log: Log = Rc::default();
                let session = ChatSession::new(
                    FakeAdapter::boxed(Driver::Claude, &log),
                    open_on("sonnet"),
                    store,
                    thread,
                    sink,
                    None,
                );
                if let Some(native) = native {
                    session.inner.emit(Envelope::new(Event::SessionStarted {
                        native_id: native.into(),
                        model: Some("sonnet".into()),
                        cwd: None,
                    }));
                }
                session.send_prompt("Finish the unique submitted task");
                assert!(
                    session.status().running_turn,
                    "submitted inference is busy before acknowledgement"
                );
                let before = session.inner.generation.get();
                session.switch(Driver::Claude, Some("opus".into()), None);
                assert!(session.inner.generation.get() > before);
                let sent = prompts(&log);
                assert_eq!(sent.len(), 2, "original submission plus one continuation");
                assert!(sent[1].contains("Finish the unique submitted task"));
                assert!(sent[1].contains("Do not repeat completed actions."));
                assert_eq!(session.inner.open.borrow().resume.as_deref(), native);
                session.shutdown(false);
            }
        });
    }

    #[test]
    fn reordered_model_replies_preserve_the_newest_request_and_latest_accepted_choice() {
        in_loop(|_| {
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, "/w");
            let (sink, _) = make_sink();
            let log: Log = Rc::default();
            let session = ChatSession::new(
                FakeAdapter::boxed(Driver::Claude, &log),
                open_on("sonnet"),
                store.clone(),
                thread.clone(),
                sink,
                None,
            );
            let requested = |model: &str, request: &str| {
                session.inner.emit(
                    Envelope::new(Event::ModelChangeRequested {
                        model: model.into(),
                    })
                    .request(request),
                )
            };
            let accepted = |model: &str, request: &str| {
                session.inner.emit(
                    Envelope::new(Event::ModelChanged {
                        model: model.into(),
                    })
                    .request(request),
                )
            };
            requested("opus", "older-opus");
            requested("haiku", "newer-haiku");
            accepted("opus", "older-opus");
            assert_eq!(session.status().pending_model.as_deref(), Some("haiku"));
            session.inner.emit(
                Envelope::new(Event::ModelChangeFailed {
                    model: "haiku".into(),
                    message: "unavailable".into(),
                })
                .request("newer-haiku"),
            );
            assert_eq!(session.status().model.as_deref(), Some("opus"));
            assert!(session.status().pending_model.is_none());
            assert_eq!(session.inner.open.borrow().model.as_deref(), Some("opus"));
            assert_eq!(
                store.provider_threads(&thread).expect("providers")[0].model,
                "opus"
            );

            requested("haiku", "old-haiku");
            requested("opus", "intervening-opus");
            requested("haiku", "latest-haiku");
            accepted("haiku", "old-haiku");
            assert_eq!(
                session.status().pending_model.as_deref(),
                Some("haiku"),
                "same model name cannot acknowledge a later request"
            );
            accepted("haiku", "latest-haiku");
            assert!(session.status().pending_model.is_none());
            session.shutdown(false);
        });
    }

    #[test]
    fn claude_aliases_follow_the_reported_family_and_actual_fallbacks_are_preserved() {
        assert_eq!(
            accepted_model_choice(Driver::Claude, Some("sonnet"), "claude-sonnet-4-6"),
            "sonnet"
        );
        assert_eq!(
            accepted_model_choice(Driver::Claude, Some("opus"), "claude-sonnet-4-6"),
            "claude-sonnet-4-6"
        );
        assert_eq!(
            accepted_model_choice(Driver::Codex, Some("sonnet"), "actual-model"),
            "actual-model"
        );
    }

    #[test]
    fn build_handoff_redacts_skips_empty_items_and_marks_open_ones_interrupted() {
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123"; // gitleaks:allow
        let msg = |role: &str, kind: &str, text: &str, status: &str, id: &str| TranscriptMessage {
            role: role.into(),
            kind: kind.into(),
            text: text.into(),
            item_id: id.into(),
            status: status.into(),
        };
        let history = [
            msg("user", "user_message", "run it", "open", "u1"),
            msg("tool", "command", &format!("out {secret}"), "open", "c1"),
            msg("tool", "tool", "", "open", "c2"),
            msg("assistant", "assistant_message", "done", "completed", "a1"),
        ];
        let handoff = build_handoff(&history, "Claude thread t");
        assert_eq!(handoff.carried, 3, "the empty item is skipped");
        assert_eq!(handoff.total, 3);
        let summary = &handoff.summary;
        assert!(!summary.contains(secret));
        assert!(summary.contains("****0123"));
        assert!(summary.contains("status=interrupted"));
        assert!(summary.contains("Claude thread t"));
        // The tool's output is carried as a tool's, fenced with this handoff's own marker.
        assert!(agent_core::handoff_budget::is_valid_fence(&handoff.fence));
        assert!(summary.contains(&format!(
            "<<<HANDOFF-{} BEGIN role=tool kind=command",
            handoff.fence
        )));
        assert_eq!(build_handoff(&[], "x").carried, 0);
        // Each handoff draws its own fence.
        assert_ne!(build_handoff(&history, "t").fence, handoff.fence);
    }

    // ---- app-owned approval rules ----

    /// A session on a real adapter whose process records what it is sent in `stdin.log`, with
    /// its Always rules in `dir`.
    fn recording(
        ctx: &glib::MainContext,
        dir: &Path,
        adapter: Box<dyn Adapter>,
    ) -> (Rc<ChatSession>, Seen) {
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let thread = fresh(&store, &dir.to_string_lossy());
        let (sink, seen) = make_sink();
        let open = scripted(dir, "cat > \"$D/stdin.log\"");
        let session = ChatSession::new(adapter, open, store, thread, sink, None);
        *session.inner.saved_rules.borrow_mut() = Some(crate::always_allow::StoreCache::new(
            dir.join(crate::always_allow::FILE_NAME),
        ));
        assert!(pump_until(ctx, 10, || session.inner.state.borrow().alive));
        (session, seen)
    }

    fn feed(session: &ChatSession, line: &str) {
        let generation = session.inner.generation.get();
        session.inner.on_line(generation, line, false);
    }

    fn can_use_tool(id: &str, command: &str) -> String {
        serde_json::json!({"type": "control_request", "request_id": id, "request": {
            "subtype": "can_use_tool", "tool_name": "Bash", "tool_use_id": format!("tu-{id}"),
            "input": {"command": command},
            "permission_suggestions": [
                {"type": "addRules", "behavior": "allow", "destination": "localSettings",
                 "rules": [{"toolName": "Bash", "ruleContent": command}]},
                {"type": "setMode", "mode": "acceptEdits", "destination": "session"}]}})
        .to_string()
    }

    fn resolution(seen: &Seen, id: &str) -> Option<Decision> {
        seen.borrow().iter().find_map(|e| match e.event {
            Event::ApprovalResolved { decision } if e.request.as_deref() == Some(id) => {
                Some(decision)
            }
            _ => None,
        })
    }

    fn offered(seen: &Seen, id: &str) -> Vec<Decision> {
        seen.borrow()
            .iter()
            .find_map(|e| match &e.event {
                Event::ApprovalRequested { options, .. } if e.request.as_deref() == Some(id) => {
                    Some(options.clone())
                }
                _ => None,
            })
            .expect("approval card")
    }

    /// The Always rules in `dir` once the off-thread save has written `count` of them.
    fn saved_rules_once(
        ctx: &glib::MainContext,
        dir: &Path,
        count: usize,
    ) -> crate::always_allow::AlwaysRules {
        let file = dir.join(crate::always_allow::FILE_NAME);
        assert!(
            pump_until(ctx, 10, || {
                crate::always_allow::AlwaysRules::open(&file).is_ok_and(|r| r.rules.len() == count)
            }),
            "{count} saved rules"
        );
        crate::always_allow::AlwaysRules::open(&file).expect("saved")
    }

    fn stdin_log(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("stdin.log")).unwrap_or_default()
    }

    /// Live: after "Allow for session" on `touch b.tmp` with the user's exact pattern,
    /// `touch b.tmp` is answered by the app, while `mkdir dd` and `touch b.tmp; rm x` ask; Claude
    /// is never sent `updatedPermissions` and the mode stays Ask.
    #[test]
    fn claude_allow_for_session_is_app_owned_and_exact() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (session, seen) = recording(
                ctx,
                tmp.path(),
                Box::new(agent_core::claude::ClaudeAdapter::new()),
            );
            feed(&session, &can_use_tool("r1", "touch b.tmp"));
            assert!(offered(&seen, "r1").contains(&Decision::AllowForSession));
            let pattern = crate::always_allow::Rule::new("", "Bash", "touch b.tmp").expect("rule");
            session.respond_approval_with_rule("r1", Decision::AllowForSession, Some(pattern));
            assert_eq!(resolution(&seen, "r1"), Some(Decision::AllowForSession));
            assert!(pump_until(ctx, 10, || stdin_log(tmp.path()).contains("\"r1\"")));

            // These ask: fed first, so the auto-answer below proves they had their turn.
            feed(&session, &can_use_tool("r3", "mkdir dd"));
            feed(&session, &can_use_tool("r4", "touch b.tmp; rm x"));
            feed(&session, &can_use_tool("r2", "touch b.tmp"));
            assert!(pump_until(ctx, 10, || resolution(&seen, "r2").is_some()));
            assert_eq!(resolution(&seen, "r2"), Some(Decision::Allow));
            assert_eq!(resolution(&seen, "r3"), None, "mkdir dd asks");
            assert_eq!(resolution(&seen, "r4"), None, "a compound command asks");
            let r4 = offered(&seen, "r4");
            assert!(
                !r4.contains(&Decision::AllowForSession) && !r4.contains(&Decision::AllowAlways),
                "a command no rule can match offers no rule: {r4:?}"
            );

            assert!(pump_until(ctx, 10, || stdin_log(tmp.path()).contains("\"r2\"")));
            let log = stdin_log(tmp.path());
            assert!(!log.contains("updatedPermissions"), "{log}");
            assert!(!log.contains("acceptEdits"), "{log}");
            assert_eq!(session.inner.state.borrow().mode, Mode::Ask);
            assert!(!has(&seen, |e| matches!(e, Event::ModeChanged { .. })));
        });
    }

    /// A command carrying an `sk-` token reaches the view masked, in the title,
    /// input and reason; it is offered neither "Allow for session" nor "Always allow"; the
    /// user's prompt snapshot is masked too. The call the app answers is still the real one.
    #[test]
    fn a_secret_in_an_approval_or_prompt_reaches_the_view_masked() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (session, seen) = recording(
                ctx,
                tmp.path(),
                Box::new(agent_core::claude::ClaudeAdapter::new()),
            );
            let secret = "sk-abcdefghijklmnopqrstuvwxyz0123456789"; // gitleaks:allow
            feed(
                &session,
                &can_use_tool(
                    "c1",
                    &format!("curl -H 'x-key: {secret}' https://example.test"),
                ),
            );
            let card = seen
                .borrow()
                .iter()
                .find(|e| matches!(e.event, Event::ApprovalRequested { .. }))
                .cloned()
                .expect("approval card");
            let shown = serde_json::to_string(&card).expect("json");
            assert!(!shown.contains(secret), "the view saw the secret: {shown}");
            assert!(shown.contains("****6789"), "{shown}");
            let options = offered(&seen, "c1");
            assert!(options.contains(&Decision::Allow) && options.contains(&Decision::Deny));
            assert!(
                !options.contains(&Decision::AllowForSession)
                    && !options.contains(&Decision::AllowAlways),
                "a secret-bearing command offers no rule: {options:?}"
            );

            session.send_prompt(&format!("use {secret} please"));
            // Every other event kind is masked too, not only approvals.
            let leak = format!("key {secret} here");
            for event in [
                Event::ItemStarted {
                    kind: ItemKind::Command,
                    title: leak.clone(),
                    input: Some(serde_json::json!({"command": leak})),
                    parent: None,
                },
                Event::ItemCompleted {
                    status: ItemStatus::Failed,
                    output: Some(leak.clone()),
                    error: Some(leak.clone()),
                },
                Event::ContentDelta {
                    stream: StreamKind::Assistant,
                    text: leak.clone(),
                },
                Event::QuestionRequested {
                    source: Default::default(),
                    questions: vec![agent_core::event::Question {
                        id: "q".into(),
                        header: leak.clone(),
                        question: leak.clone(),
                        options: vec![agent_core::event::QuestionOption {
                            label: leak.clone(),
                            description: Some(leak.clone()),
                        }],
                        multi_select: false,
                    }],
                },
                Event::Notice { text: leak.clone() },
                Event::Error {
                    message: leak.clone(),
                },
                Event::TurnCompleted {
                    state: TurnState::Failed,
                    usage: None,
                    cost_usd: None,
                    error: Some(leak.clone()),
                },
            ] {
                session.inner.emit(Envelope::new(event).item("i1"));
            }
            assert!(has(
                &seen,
                |e| matches!(e, Event::Notice { text } if text.contains("****6789"))
            ));
            let all = serde_json::to_string(&*seen.borrow()).expect("json");
            assert!(!all.contains(secret), "a secret reached the sink: {all}");
        });
    }

    /// A Claude request nobody answers is denied at the deadline, with a deny frame to
    /// the agent and a notice; one answered in time sends nothing more.
    #[test]
    fn an_unanswered_claude_request_is_denied_at_the_deadline() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (session, seen) = recording(
                ctx,
                tmp.path(),
                Box::new(agent_core::claude::ClaudeAdapter::new()),
            );
            session
                .inner
                .approval_deadline
                .set(Duration::from_millis(60));
            feed(&session, &can_use_tool("late", "touch b.tmp"));
            feed(&session, &can_use_tool("prompt", "touch c.tmp"));
            session.respond_approval("prompt", Decision::Allow);
            assert!(pump_until(ctx, 10, || resolution(&seen, "late").is_some()));
            assert_eq!(resolution(&seen, "late"), Some(Decision::Deny));
            assert!(pump_until(ctx, 10, || stdin_log(tmp.path()).contains("\"late\"")));
            let log = stdin_log(tmp.path());
            let frame = log
                .lines()
                .find(|l| l.contains("\"late\""))
                .expect("a frame for the late request");
            assert!(frame.contains("\"behavior\":\"deny\""), "{frame}");
            assert!(has(
                &seen,
                |e| matches!(e, Event::Notice { text } if text.contains("denied"))
            ));
            assert_eq!(
                log.matches("\"request_id\":\"prompt\"").count(),
                1,
                "the answered request is not answered twice: {log}"
            );
            assert!(session
                .inner
                .adapter
                .borrow()
                .pending_request_ids()
                .is_empty());
        });
    }

    const W0_CODEX_SHELL: &str =
        include_str!("../../tests/fixtures/real/codex/ask-shell-decline.jsonl");

    /// The Codex frame: "Allow for session" sends a plain `accept` and records a rule; the
    /// same command then auto-approves, `ls -la; id` asks.
    #[test]
    fn codex_allow_for_session_replays_the_w0_frame() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (session, seen) = recording(
                ctx,
                tmp.path(),
                Box::new(agent_core::codex::CodexAdapter::new()),
            );
            let frame = W0_CODEX_SHELL
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .find(|r| r["frame"]["method"] == "item/commandExecution/requestApproval")
                .expect("W0 frame")["frame"]
                .clone();
            let again = |id: u64, item: &str, command: &str| {
                let mut f = frame.clone();
                f["id"] = serde_json::json!(id);
                f["params"]["itemId"] = serde_json::json!(item);
                f["params"]["command"] = serde_json::json!(command);
                f.to_string()
            };
            feed(&session, &frame.to_string());
            let first = seen
                .borrow()
                .iter()
                .find(|e| matches!(e.event, Event::ApprovalRequested { .. }))
                .and_then(|e| e.request.clone())
                .expect("card");
            assert!(offered(&seen, &first).contains(&Decision::AllowForSession));
            session.respond_approval(&first, Decision::AllowForSession);
            assert!(pump_until(ctx, 10, || {
                stdin_log(tmp.path()).contains(r#"{"id":0,"result":{"decision":"accept"}}"#)
            }));
            let log = stdin_log(tmp.path());
            assert!(!log.contains("acceptForSession"), "{log}");
            assert!(!log.contains("Execpolicy"), "{log}");

            feed(
                &session,
                &again(1, "item-b", "/usr/bin/zsh -lc 'ls -la; id'"),
            );
            feed(&session, &again(2, "item-c", "/usr/bin/zsh -lc 'ls -la'"));
            let request_of = |item: &str| {
                seen.borrow()
                    .iter()
                    .find(|e| {
                        e.item.as_deref() == Some(item)
                            && matches!(e.event, Event::ApprovalRequested { .. })
                    })
                    .and_then(|e| e.request.clone())
                    .expect("card")
            };
            let (compound, same) = (request_of("item-b"), request_of("item-c"));
            assert!(pump_until(ctx, 10, || resolution(&seen, &same).is_some()));
            assert_eq!(resolution(&seen, &same), Some(Decision::Allow));
            assert_eq!(resolution(&seen, &compound), None, "ls -la; id asks");
        });
    }

    /// The 2026-10-11 Codex thread: "Allow for session" on `cat <repo>/AGENTS.md` asked again for
    /// `cat AGENTS.md`. A read now keeps `cat …` for the workspace: the next `cat` of another
    /// file, from a sub-folder, is answered; one outside the workspace still asks.
    #[test]
    fn codex_allow_for_session_on_a_read_covers_the_next_read_of_that_program() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let ws = std::fs::canonicalize(tmp.path()).expect("canon");
            std::fs::create_dir_all(ws.join("src")).expect("src");
            let root = ws.to_string_lossy().into_owned();
            let (session, seen) =
                recording(ctx, &ws, Box::new(agent_core::codex::CodexAdapter::new()));
            let ask = |id: u64, item: &str, command: String, cwd: &str| {
                serde_json::json!({"id": id, "method": "item/commandExecution/requestApproval",
                    "params": {"threadId": "th", "turnId": "tu", "itemId": item,
                    "command": command, "cwd": cwd,
                    "availableDecisions": ["accept", "decline", "cancel"]}})
                .to_string()
            };
            let request_of = |item: &str| {
                seen.borrow()
                    .iter()
                    .find(|e| {
                        e.item.as_deref() == Some(item)
                            && matches!(e.event, Event::ApprovalRequested { .. })
                    })
                    .and_then(|e| e.request.clone())
                    .expect("card")
            };
            feed(
                &session,
                &ask(
                    0,
                    "a",
                    format!("/usr/bin/zsh -lc 'cat {root}/AGENTS.md'"),
                    &root,
                ),
            );
            let first = request_of("a");
            session.respond_approval(&first, Decision::AllowForSession);
            assert_eq!(resolution(&seen, &first), Some(Decision::AllowForSession));

            let src = format!("{root}/src");
            feed(
                &session,
                &ask(1, "b", "/usr/bin/zsh -lc 'cat lib.rs'".into(), &src),
            );
            feed(
                &session,
                &ask(2, "c", "/usr/bin/zsh -lc 'cat /etc/hosts'".into(), &root),
            );
            feed(
                &session,
                &ask(3, "d", "/usr/bin/zsh -lc 'head lib.rs'".into(), &src),
            );
            let (next, outside, other) = (request_of("b"), request_of("c"), request_of("d"));
            assert!(pump_until(ctx, 10, || resolution(&seen, &next).is_some()));
            assert_eq!(resolution(&seen, &next), Some(Decision::Allow));
            assert_eq!(
                resolution(&seen, &outside),
                None,
                "outside the workspace asks"
            );
            assert_eq!(resolution(&seen, &other), None, "another program asks");
        });
    }

    const W0_CODEX_ACCEPT_EDITS: &str =
        include_str!("../../tests/fixtures/real/codex/untrusted-workspacewrite.jsonl");

    /// The frames of `fixture` whose method is one of `methods`, the recording's sandbox
    /// replaced by `dir`.
    fn w0_frames(fixture: &str, dir: &Path, methods: &[&str]) -> Vec<String> {
        let dir = dir.to_string_lossy();
        fixture
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|r| r["dir"] == "out")
            .map(|r| r["frame"].clone())
            .filter(|f| f["method"].as_str().is_some_and(|m| methods.contains(&m)))
            .map(|f| f.to_string().replace("/tmp/w0-sandbox", &dir))
            .collect()
    }

    fn request_for(seen: &Seen, method_tool: &str) -> Option<String> {
        seen.borrow().iter().find_map(|e| match &e.event {
            Event::ApprovalRequested { tool, .. } if tool == method_tool => e.request.clone(),
            _ => None,
        })
    }

    const W0_CODEX_EDIT: &str =
        include_str!("../../tests/fixtures/real/codex/ask-edit-decline.jsonl");

    fn claude_tool(id: &str, tool: &str, input: serde_json::Value) -> String {
        serde_json::json!({"type": "control_request", "request_id": id, "request": {
            "subtype": "can_use_tool", "tool_name": tool, "tool_use_id": format!("tu-{id}"),
            "input": input}})
        .to_string()
    }

    /// Session and Always are offered for a contained edit and for one MCP tool. A Codex
    /// patch's path comes from its `item/started` frame (`ask-edit-decline.jsonl`).
    /// The Always rule is matched by kind: a new Codex session and a Claude session both have
    /// the same edit answered by the app. An edit outside the workspace is offered neither.
    #[test]
    fn always_rules_cover_contained_edits_and_mcp_tools_across_agents() {
        let methods = ["item/started", "item/fileChange/requestApproval"];
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let codex = || Box::new(agent_core::codex::CodexAdapter::new());
            let claude = || Box::new(agent_core::claude::ClaudeAdapter::new());
            let target = tmp.path().join("fixture.txt");

            let (session, seen) = recording(ctx, tmp.path(), codex());
            for f in w0_frames(W0_CODEX_EDIT, tmp.path(), &methods) {
                feed(&session, &f);
            }
            let edit = request_for(&seen, "file_change").expect("edit card");
            let options = offered(&seen, &edit);
            assert!(options.contains(&Decision::AllowForSession), "{options:?}");
            assert!(options.contains(&Decision::AllowAlways), "{options:?}");
            session.respond_approval(&edit, Decision::AllowAlways);
            assert_eq!(resolution(&seen, &edit), Some(Decision::AllowAlways));
            let saved = saved_rules_once(ctx, tmp.path(), 1);
            assert_eq!(saved.rules[0].kind, crate::always_allow::ActionKind::Edit);
            assert_eq!(saved.rules[0].origin, "codex");
            drop(session);

            // A new Codex session: the same patch is accepted by the app.
            let (session, seen) = recording(ctx, tmp.path(), codex());
            for f in w0_frames(W0_CODEX_EDIT, tmp.path(), &methods) {
                feed(&session, &f);
            }
            let again = request_for(&seen, "file_change").expect("edit card");
            assert!(pump_until(ctx, 10, || resolution(&seen, &again).is_some()));
            assert_eq!(resolution(&seen, &again), Some(Decision::Allow));
            drop(session);

            // Claude editing the same file is matched by kind; an MCP tool can be Always too.
            let (session, seen) = recording(ctx, tmp.path(), claude());
            let input =
                serde_json::json!({"file_path": target, "old_string": "a", "new_string": "b"});
            feed(&session, &claude_tool("e1", "Edit", input));
            assert!(pump_until(ctx, 10, || resolution(&seen, "e1").is_some()));
            assert_eq!(resolution(&seen, "e1"), Some(Decision::Allow));
            feed(
                &session,
                &claude_tool("m1", "mcp__fs__write", serde_json::json!({"p": 1})),
            );
            assert!(offered(&seen, "m1").contains(&Decision::AllowAlways));
            session.respond_approval("m1", Decision::AllowAlways);
            saved_rules_once(ctx, tmp.path(), 2);
            drop(session);
            let (session, seen) = recording(ctx, tmp.path(), claude());
            feed(
                &session,
                &claude_tool("m2", "mcp__fs__write", serde_json::json!({"p": 2})),
            );
            assert!(pump_until(ctx, 10, || resolution(&seen, "m2").is_some()));
            assert_eq!(resolution(&seen, "m2"), Some(Decision::Allow));
            feed(
                &session,
                &claude_tool("m3", "mcp__fs__delete", serde_json::json!({})),
            );
            // Outside the workspace: offered all the same, the user's to choose (kept with a
            // warning), and not answered by any rule yet.
            let outside = serde_json::json!({"file_path": "/etc/hosts", "old_string": "a", "new_string": "b"});
            feed(&session, &claude_tool("e2", "Edit", outside));
            let e2 = offered(&seen, "e2");
            assert!(
                e2.contains(&Decision::AllowAlways) && e2.contains(&Decision::AllowForSession),
                "{e2:?}"
            );
            pump_until(ctx, 1, || false);
            assert_eq!(
                resolution(&seen, "m3"),
                None,
                "another tool of the server asks"
            );
            assert_eq!(resolution(&seen, "e2"), None);
        });
    }

    /// Accept edits chosen while an Ask turn runs is deferred to the next turn,
    /// so an edit that turn asks for is not answered by the app.
    #[test]
    fn codex_accept_edits_chosen_mid_turn_does_not_auto_accept_that_turn() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (session, seen) = recording(
                ctx,
                tmp.path(),
                Box::new(agent_core::codex::CodexAdapter::new()),
            );
            let frames = w0_frames(
                W0_CODEX_ACCEPT_EDITS,
                tmp.path(),
                &[
                    "turn/started",
                    "item/started",
                    "item/fileChange/requestApproval",
                ],
            );
            let approval_at = frames
                .iter()
                .position(|f| f.contains("item/fileChange/requestApproval"))
                .expect("edit request");
            feed(&session, &frames[0]); // the Ask turn starts
            session.set_mode(Mode::AcceptEdits);
            assert!(has(&seen, |e| matches!(
                e,
                Event::ModeChangeDeferred {
                    requested: Mode::AcceptEdits,
                    effective: Mode::Ask
                }
            )));
            assert_eq!(session.inner.state.borrow().mode, Mode::Ask);
            for f in &frames[1..=approval_at] {
                feed(&session, f);
            }
            let edit = request_for(&seen, "file_change").expect("edit card");
            pump_until(ctx, 1, || false);
            assert_eq!(resolution(&seen, &edit), None, "the Ask turn's edit asks");
            assert!(!stdin_log(tmp.path()).contains(r#""result":{"decision":"accept"}"#));
        });
    }

    /// Replaying `codex/untrusted-workspacewrite.jsonl`: in Accept edits the
    /// in-workspace edit is answered `accept` by the app and the shell `ls -la` shows a card. In
    /// Ask, or for an edit outside the workspace, the edit asks too.
    #[test]
    fn codex_accept_edits_auto_accepts_workspace_edits_and_shell_still_asks() {
        let methods = [
            "turn/started",
            "item/started",
            "item/fileChange/requestApproval",
            "item/commandExecution/requestApproval",
        ];
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (session, seen) = recording(
                ctx,
                tmp.path(),
                Box::new(agent_core::codex::CodexAdapter::new()),
            );
            session.set_mode(Mode::AcceptEdits);
            assert_eq!(session.inner.state.borrow().mode, Mode::AcceptEdits);
            for f in w0_frames(W0_CODEX_ACCEPT_EDITS, tmp.path(), &methods) {
                feed(&session, &f);
            }
            let edit = request_for(&seen, "file_change").expect("edit card");
            let shell = request_for(&seen, "command_execution").expect("shell card");
            assert!(pump_until(ctx, 10, || resolution(&seen, &edit).is_some()));
            assert_eq!(resolution(&seen, &edit), Some(Decision::Allow));
            assert!(pump_until(ctx, 10, || {
                stdin_log(tmp.path()).contains(r#"{"id":0,"result":{"decision":"accept"}}"#)
            }));
            assert_eq!(resolution(&seen, &shell), None, "shell asks");
            assert!(!stdin_log(tmp.path()).contains(r#""id":1,"result""#));
        });
        for (mode, sandbox) in [(Mode::Ask, None), (Mode::AcceptEdits, Some("/etc"))] {
            in_loop(|ctx| {
                let tmp = tempfile::tempdir().expect("tmp");
                let (session, seen) = recording(
                    ctx,
                    tmp.path(),
                    Box::new(agent_core::codex::CodexAdapter::new()),
                );
                session.set_mode(mode);
                let target = sandbox.map_or_else(|| tmp.path().to_owned(), PathBuf::from);
                for f in w0_frames(W0_CODEX_ACCEPT_EDITS, &target, &methods[..3]) {
                    feed(&session, &f);
                }
                let edit = request_for(&seen, "file_change").expect("edit card");
                pump_until(ctx, 1, || false);
                assert_eq!(resolution(&seen, &edit), None, "{mode:?} {sandbox:?} asks");
            });
        }
    }

    /// On Accept edits: the same in-workspace patch, carrying an escalation (a grant root),
    /// is not accepted by the app.
    #[test]
    fn codex_accept_edits_never_accepts_a_request_with_an_escalation() {
        let methods = [
            "turn/started",
            "item/started",
            "item/fileChange/requestApproval",
        ];
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (session, seen) = recording(
                ctx,
                tmp.path(),
                Box::new(agent_core::codex::CodexAdapter::new()),
            );
            session.set_mode(Mode::AcceptEdits);
            for f in w0_frames(W0_CODEX_ACCEPT_EDITS, tmp.path(), &methods) {
                let mut v: serde_json::Value = serde_json::from_str(&f).expect("frame");
                if v["method"] == "item/fileChange/requestApproval" {
                    v["params"]["grantRoot"] = serde_json::json!("/");
                }
                feed(&session, &v.to_string());
            }
            let edit = request_for(&seen, "file_change").expect("edit card");
            pump_until(ctx, 1, || false);
            assert_eq!(resolution(&seen, &edit), None, "an escalation asks");
        });
    }

    /// On Accept edits: every target of a multi-change patch (paths and rename targets) must
    /// pass the containment check; one outside, or one that is not text, makes it ask.
    #[test]
    fn codex_accept_edits_checks_every_target_of_a_patch() {
        let tmp = tempfile::tempdir().expect("tmp");
        let ws = std::fs::canonicalize(tmp.path()).expect("canon");
        let p = |name: &str| ws.join(name).to_string_lossy().into_owned();
        let change = |path: serde_json::Value, moved: serde_json::Value| serde_json::json!({"path": path, "kind": {"type": "update", "move_path": moved}});
        let null = serde_json::Value::Null;
        let inside = serde_json::json!({"changes": [
            change(p("a.txt").into(), null.clone()),
            change(p("b.txt").into(), p("c.txt").into()),
        ]});
        assert!(codex_edits_contained(&inside, &ws, None));
        for bad in [
            serde_json::json!({"changes": [change(p("a.txt").into(), null.clone()),
                change("/etc/hosts".into(), null.clone())]}),
            serde_json::json!({"changes": [change(p("a.txt").into(), "/etc/hosts".into())]}),
            serde_json::json!({"changes": [change(serde_json::json!(7), null.clone())]}),
            serde_json::json!({"changes": [change(p("AGENTS.md").into(), null.clone())]}),
            serde_json::json!({"changes": []}),
            serde_json::json!({}),
        ] {
            assert!(!codex_edits_contained(&bad, &ws, None), "{bad}");
        }
    }

    /// From the Codex frame: "Always allow" saved with the dialog's prefill (the
    /// bare `ls -la`) is a rule a new session matches the same wrapped request with.
    #[test]
    fn codex_always_rule_from_the_prefill_matches_the_w0_request_next_time() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let cwd = tmp.path().to_string_lossy().into_owned();
            let frame = |id: u64, item: &str| {
                let mut f = W0_CODEX_SHELL
                    .lines()
                    .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                    .find(|r| r["frame"]["method"] == "item/commandExecution/requestApproval")
                    .expect("W0 frame")["frame"]
                    .clone();
                f["id"] = serde_json::json!(id);
                f["params"]["itemId"] = serde_json::json!(item);
                f["params"]["cwd"] = serde_json::json!(cwd);
                f
            };
            let card = |seen: &Seen, item: &str| {
                seen.borrow()
                    .iter()
                    .find_map(|e| match &e.event {
                        Event::ApprovalRequested { tool, input, .. }
                            if e.item.as_deref() == Some(item) =>
                        {
                            Some((
                                e.request.clone().expect("request"),
                                tool.clone(),
                                input.clone(),
                            ))
                        }
                        _ => None,
                    })
                    .expect("card")
            };
            let (session, seen) = recording(
                ctx,
                tmp.path(),
                Box::new(agent_core::codex::CodexAdapter::new()),
            );
            feed(&session, &frame(0, "item-a").to_string());
            let (request, tool, input) = card(&seen, "item-a");
            assert!(offered(&seen, &request).contains(&Decision::AllowAlways));
            let prefill = crate::always_allow::dialog_prefill(&tool, &input);
            assert_eq!(prefill, "ls -la");
            let rule = crate::always_allow::Rule::new("", tool, prefill).expect("rule");
            session.respond_approval_with_rule(&request, Decision::AllowAlways, Some(rule));
            assert_eq!(resolution(&seen, &request), Some(Decision::AllowAlways));
            let saved = saved_rules_once(ctx, tmp.path(), 1);
            assert_eq!(saved.rules[0].detail(), "ls -la");
            drop(session);

            // Next time: a new session, no session rules, only the saved one.
            let (session, seen) = recording(
                ctx,
                tmp.path(),
                Box::new(agent_core::codex::CodexAdapter::new()),
            );
            feed(&session, &frame(0, "item-b").to_string());
            let (again, _, _) = card(&seen, "item-b");
            assert!(pump_until(ctx, 10, || resolution(&seen, &again).is_some()));
            assert_eq!(resolution(&seen, &again), Some(Decision::Allow));
        });
    }

    /// Codex asks to approve an MCP tool call as a fieldless form. It is an approval card like
    /// Claude's MCP calls: "Allow for session" keeps a rule that answers the next call of that
    /// tool, and a request the adapter could not tie to one call is never answered by it.
    #[test]
    fn a_codex_mcp_session_rule_answers_the_next_call_of_that_tool() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let started = |item: &str| {
                serde_json::json!({"method": "item/started", "params": {"threadId": "th",
                    "turnId": "tu", "item": {"type": "mcpToolCall", "id": item,
                    "server": "ollama", "tool": "ollama_ps", "status": "inProgress",
                    "arguments": {}}}})
                .to_string()
            };
            let ask = |id: u64| {
                serde_json::json!({"id": id, "method": "mcpServer/elicitation/request",
                    "params": {"threadId": "th", "turnId": "tu", "serverName": "ollama",
                    "mode": "form", "_meta": {"codex_approval_kind": "mcp_tool_call"},
                    "message": "Allow the ollama MCP server to run tool \"ollama_ps\"?",
                    "requestedSchema": {"type": "object", "properties": {}}}})
                .to_string()
            };
            let card = |seen: &Seen, n: usize| {
                seen.borrow()
                    .iter()
                    .filter_map(|e| match &e.event {
                        Event::ApprovalRequested { tool, .. } => {
                            Some((e.request.clone().expect("request"), tool.clone()))
                        }
                        _ => None,
                    })
                    .nth(n)
                    .expect("card")
            };
            let (session, seen) = recording(
                ctx,
                tmp.path(),
                Box::new(agent_core::codex::CodexAdapter::new()),
            );
            feed(&session, &started("c1"));
            feed(&session, &ask(0));
            let (first, tool) = card(&seen, 0);
            assert_eq!(tool, "mcp__ollama__ollama_ps");
            assert!(!seen
                .borrow()
                .iter()
                .any(|e| matches!(e.event, Event::FormRequested { .. })));
            assert!(offered(&seen, &first).contains(&Decision::AllowForSession));
            assert_eq!(resolution(&seen, &first), None, "no rule yet: it asks");
            session.respond_approval(&first, Decision::AllowForSession);
            assert_eq!(resolution(&seen, &first), Some(Decision::AllowForSession));

            feed(&session, &started("c2"));
            feed(&session, &ask(1));
            let (second, _) = card(&seen, 1);
            assert!(pump_until(ctx, 10, || resolution(&seen, &second).is_some()));
            assert_eq!(resolution(&seen, &second), Some(Decision::Allow));

            // A second request for the same call names no tool, so the rule cannot answer it.
            feed(&session, &ask(2));
            let (third, tool) = card(&seen, 2);
            assert_eq!(tool, agent_core::codex::MCP_UNRESOLVED_TOOL);
            assert!(!pump_until(ctx, 1, || resolution(&seen, &third).is_some()));
            assert!(!offered(&seen, &third).contains(&Decision::AllowForSession));
        });
    }

    /// An unusable rules file is kept byte-for-byte, saved rules are off with a notice
    /// naming it, and "Always allow" still holds for the session.
    #[test]
    fn a_corrupt_rules_file_is_never_overwritten_and_is_named() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let rules = tmp.path().join(crate::always_allow::FILE_NAME);
            std::fs::write(&rules, "{ not json").expect("corrupt");
            let (session, seen) = recording(
                ctx,
                tmp.path(),
                Box::new(agent_core::claude::ClaudeAdapter::new()),
            );
            feed(&session, &can_use_tool("r1", "touch b.tmp"));
            assert!(offered(&seen, "r1").contains(&Decision::AllowAlways));
            session.respond_approval("r1", Decision::AllowAlways);
            assert_eq!(std::fs::read(&rules).expect("read"), b"{ not json");
            let named = rules.display().to_string();
            assert!(has(
                &seen,
                |e| matches!(e, Event::Notice { text } if text.contains(&named) && text.contains("untouched"))
            ));
            feed(&session, &can_use_tool("r2", "touch b.tmp"));
            assert!(pump_until(ctx, 10, || resolution(&seen, "r2").is_some()));
            assert_eq!(std::fs::read(&rules).expect("read"), b"{ not json");
        });
    }

    /// Session approvals end on a cross-agent switch (and the notice says how many),
    /// survive a same-agent model switch, and an Always rule carries to the other agent by kind.
    #[test]
    fn session_rules_end_on_a_cross_agent_switch_and_always_rules_carry() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (session, seen) = recording(
                ctx,
                tmp.path(),
                Box::new(agent_core::claude::ClaudeAdapter::new()),
            );
            let log: Log = Rc::default();
            let log2 = log.clone();
            session.set_adapter_factory(Rc::new(move |d| launch_of(FakeAdapter::boxed(d, &log2))));
            feed(&session, &can_use_tool("r1", "touch b.tmp"));
            session.respond_approval("r1", Decision::AllowForSession);
            feed(&session, &can_use_tool("r2", "ls -la"));
            session.respond_approval("r2", Decision::AllowAlways);
            assert_eq!(session.inner.session_rules.borrow().rules.len(), 2);

            session.switch(Driver::Claude, Some("claude-opus-5-5".into()), None);
            assert_eq!(
                session.inner.session_rules.borrow().rules.len(),
                2,
                "a same-agent model switch keeps them"
            );

            // Said before the switch, in the picker: nothing for the same agent.
            assert!(session.switch_caveats(Driver::Claude).is_empty());
            let said = session.switch_caveats(Driver::Agy);
            assert!(
                said[0].starts_with("Continues this thread in Antigravity from a summary"),
                "{said:?}"
            );
            assert!(
                said.contains(&"Your 2 session approvals end.".to_owned()),
                "{said:?}"
            );
            let cannot_ask = |said: &[String]| {
                said.iter()
                    .any(|c| c.starts_with("Antigravity cannot ask you before acting"))
            };
            assert!(
                cannot_ask(&said),
                "no proof of the hook: say agy cannot ask"
            );
            session.set_agy_can_ask(Rc::new(|| true));
            assert!(!cannot_ask(&session.switch_caveats(Driver::Agy)));

            session.switch(Driver::Agy, None, None);
            assert!(has(&seen, |e| matches!(e, Event::Notice { text }
                if text.starts_with("Switched to Antigravity") && text.ends_with("Session approvals (2) ended."))));
            assert!(session.inner.session_rules.borrow().rules.is_empty());

            let workspace = tmp.path().to_owned();
            let cwd = workspace.to_string_lossy().into_owned();
            let agy = crate::always_allow::Context {
                driver: "agy",
                workspace: &workspace,
                cwd: Some(&cwd),
                home: None,
            };
            let touch = crate::always_allow::Action::Shell {
                command: "touch b.tmp".into(),
            };
            assert!(!session
                .inner
                .session_rules
                .borrow()
                .matches_session(&touch, &agy));
            let file = workspace.join(crate::always_allow::FILE_NAME);
            assert!(pump_until(ctx, 10, || {
                crate::always_allow::AlwaysRules::open(&file).is_ok_and(|r| r.rules.len() == 1)
            }));
            let saved = crate::always_allow::AlwaysRules::open(&file).expect("saved");
            assert_eq!(saved.rules.len(), 1);
            assert_eq!(saved.rules[0].origin, "claude");
            let ls = crate::always_allow::Action::Shell {
                command: "ls -la".into(),
            };
            assert!(
                saved.matches_any(&ls, &agy),
                "an Always rule matches agy by kind"
            );
            assert!(!saved.matches_any(&touch, &agy));
        });
    }

    /// An answer that arrives after the deadline denied the request keeps no rule, whatever
    /// the user's pattern.
    #[test]
    fn a_late_answer_after_the_deadline_keeps_no_rule() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (session, seen) = recording(
                ctx,
                tmp.path(),
                Box::new(agent_core::claude::ClaudeAdapter::new()),
            );
            session
                .inner
                .approval_deadline
                .set(Duration::from_millis(50));
            feed(&session, &can_use_tool("r1", "ls -la"));
            assert!(pump_until(ctx, 10, || resolution(&seen, "r1").is_some()));
            assert_eq!(resolution(&seen, "r1"), Some(Decision::Deny));
            let pattern = crate::always_allow::Rule::new("", "Bash", "ls *").expect("rule");
            session.respond_approval_with_rule("r1", Decision::AllowAlways, Some(pattern));
            assert!(session.inner.session_rules.borrow().rules.is_empty());
            // Give a stray save every chance to land.
            for _ in 0..50 {
                ctx.iteration(false);
            }
            assert!(!tmp.path().join(crate::always_allow::FILE_NAME).exists());
        });
    }

    /// With Automatic Pattern Approvals off, a saved rule approves nothing and no card offers
    /// "Always allow"; an "Always allow" on a card shown before is allowed once and keeps
    /// nothing, and session grants still approve. On again, the saved rule answers as before.
    #[test]
    fn saved_rules_off_neither_approve_nor_are_offered_nor_written() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let file = tmp.path().join(crate::always_allow::FILE_NAME);
            let saved =
                || crate::always_allow::AlwaysRules::open(&file).map_or(0, |r| r.rules.len());
            let (session, seen) = recording(
                ctx,
                tmp.path(),
                Box::new(agent_core::claude::ClaudeAdapter::new()),
            );
            feed(&session, &can_use_tool("r1", "ls -la"));
            assert!(offered(&seen, "r1").contains(&Decision::AllowAlways));
            session.respond_approval("r1", Decision::AllowAlways);
            assert!(pump_until(ctx, 10, || saved() == 1));
            // From here on only the saved rule could answer `ls -la`.
            session.inner.end_session_rules();
            feed(&session, &can_use_tool("r2", "ls -la"));
            assert!(pump_until(ctx, 10, || resolution(&seen, "r2").is_some()));
            assert_eq!(
                resolution(&seen, "r2"),
                Some(Decision::Allow),
                "on: it applies"
            );
            feed(&session, &can_use_tool("late", "touch b.tmp"));
            assert!(offered(&seen, "late").contains(&Decision::AllowAlways));

            let off = crate::always_allow::PolicyGuard::saved_rules(false);
            feed(&session, &can_use_tool("r3", "ls -la"));
            assert!(
                !pump_until(ctx, 1, || resolution(&seen, "r3").is_some()),
                "off: the saved rule does not answer it"
            );
            assert!(offered(&seen, "r3").contains(&Decision::AllowForSession));
            assert!(!offered(&seen, "r3").contains(&Decision::AllowAlways));
            session.respond_approval("r3", Decision::Deny);

            // The earlier card's "Always allow" is allowed once: no rule, no session grant, and
            // not the typed pattern either.
            let pattern = crate::always_allow::Rule::new("", "Bash", "touch *").expect("rule");
            session.respond_approval_with_rule("late", Decision::AllowAlways, Some(pattern));
            assert_eq!(resolution(&seen, "late"), Some(Decision::Allow));
            assert!(has(&seen, |e| matches!(e, Event::Notice { text }
                if text == crate::always_allow::ALLOWED_ONCE_SAVED_RULES_OFF)));
            for _ in 0..50 {
                ctx.iteration(false);
            }
            assert_eq!(saved(), 1, "nothing was written");
            assert!(session.inner.session_rules.borrow().rules.is_empty());
            feed(&session, &can_use_tool("r4", "touch b.tmp"));
            assert!(
                !pump_until(ctx, 1, || resolution(&seen, "r4").is_some()),
                "it asks again"
            );
            // A session grant chosen now still answers.
            session.respond_approval("r4", Decision::AllowForSession);
            assert_eq!(resolution(&seen, "r4"), Some(Decision::AllowForSession));
            feed(&session, &can_use_tool("r5", "touch b.tmp"));
            assert!(pump_until(ctx, 10, || resolution(&seen, "r5").is_some()));
            assert_eq!(
                resolution(&seen, "r5"),
                Some(Decision::Allow),
                "the session grant"
            );

            drop(off);
            feed(&session, &can_use_tool("r6", "ls -la"));
            assert!(pump_until(ctx, 10, || resolution(&seen, "r6").is_some()));
            assert_eq!(resolution(&seen, "r6"), Some(Decision::Allow), "on again");
        });
    }

    #[test]
    fn an_always_answer_is_downgraded_only_while_saved_rules_are_off() {
        let pattern = || crate::always_allow::Rule::new("", "Bash", "ls *").ok();
        let (d, custom, off) = Inner::answer_under_policy(Decision::AllowAlways, pattern(), true);
        assert_eq!(d, Decision::AllowAlways);
        assert!(custom.is_some());
        assert!(!off);
        let (d, custom, off) = Inner::answer_under_policy(Decision::AllowAlways, pattern(), false);
        assert_eq!(d, Decision::Allow, "allowed once");
        assert!(custom.is_none(), "a pattern typed for good is dropped");
        assert!(off);
        for decision in [Decision::Allow, Decision::AllowForSession, Decision::Deny] {
            let (d, _, off) = Inner::answer_under_policy(decision, None, false);
            assert_eq!(d, decision);
            assert!(!off);
        }
        let (d, custom, _) =
            Inner::answer_under_policy(Decision::AllowForSession, pattern(), false);
        assert_eq!(d, Decision::AllowForSession);
        assert!(custom.is_some(), "a session answer keeps its own pattern");
    }

    /// Claude's working directory moves with `cd`, so a relative path is never offered a
    /// rule; an absolute one inside the workspace is.
    #[test]
    fn claude_relative_paths_are_never_offered_a_rule() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (session, seen) = recording(
                ctx,
                tmp.path(),
                Box::new(agent_core::claude::ClaudeAdapter::new()),
            );
            let edit = |id: &str, path: &str| {
                serde_json::json!({"type": "control_request", "request_id": id, "request": {
                    "subtype": "can_use_tool", "tool_name": "Edit", "tool_use_id": format!("tu-{id}"),
                    "input": {"file_path": path, "old_string": "a", "new_string": "b"}}})
                .to_string()
            };
            feed(&session, &edit("e1", "a.rs"));
            assert!(!offered(&seen, "e1").contains(&Decision::AllowForSession));
            let abs = tmp.path().join("a.rs").to_string_lossy().into_owned();
            feed(&session, &edit("e2", &abs));
            assert!(offered(&seen, "e2").contains(&Decision::AllowForSession));
            assert!(
                offered(&seen, "e2").contains(&Decision::AllowAlways),
                "U2: a contained edit can be kept always"
            );
            assert!(!offered(&seen, "e1").contains(&Decision::AllowAlways));
        });
    }

    /// In dangerous permissive mode a request that still reaches the app (Codex should send
    /// none on `never`) is approved by it, an escalation included: the user agreed to all.
    #[test]
    fn dangerous_permissive_mode_approves_whatever_still_asks() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (session, seen) = recording(
                ctx,
                tmp.path(),
                Box::new(agent_core::codex::CodexAdapter::new()),
            );
            session.set_mode(Mode::Bypass);
            assert_eq!(session.inner.state.borrow().mode, Mode::Bypass);
            let cwd = tmp.path().to_string_lossy().into_owned();
            let frame =
                serde_json::json!({"id": 0, "method": "item/commandExecution/requestApproval",
                "params": {
                    "kind": "escalated", "threadId": "th", "turnId": "tu", "itemId": "i0",
                    "command": "/usr/bin/zsh -lc 'rm -rf build'", "cwd": cwd,
                    "availableDecisions": ["accept", "cancel"],
                }})
                .to_string();
            feed(&session, &frame);
            let card = request_for(&seen, "command_execution").expect("card");
            assert!(pump_until(ctx, 10, || resolution(&seen, &card).is_some()));
            assert_eq!(resolution(&seen, &card), Some(Decision::Allow));
            assert!(pump_until(ctx, 10, || {
                stdin_log(tmp.path()).contains(r#"{"id":0,"result":{"decision":"accept"}}"#)
            }));
        });
    }

    /// A Codex request carrying a field a plain call does not have (an escalation) is never
    /// auto-accepted by a matching rule.
    #[test]
    fn codex_requests_with_extra_fields_are_never_auto_accepted() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (session, seen) = recording(
                ctx,
                tmp.path(),
                Box::new(agent_core::codex::CodexAdapter::new()),
            );
            let cwd = tmp.path().to_string_lossy().into_owned();
            let frame = |id: u64, item: &str, extra: Option<(&str, serde_json::Value)>| {
                let mut params = serde_json::json!({
                    "kind": "command", "threadId": "th", "turnId": "tu", "itemId": item,
                    "command": "/usr/bin/zsh -lc 'ls -la'", "cwd": cwd,
                    "availableDecisions": ["accept", "cancel"],
                });
                if let Some((k, v)) = extra {
                    params[k] = v;
                }
                serde_json::json!({"id": id, "method": "item/commandExecution/requestApproval",
                    "params": params})
                .to_string()
            };
            let request_of = |item: &str| {
                seen.borrow()
                    .iter()
                    .find(|e| {
                        e.item.as_deref() == Some(item)
                            && matches!(e.event, Event::ApprovalRequested { .. })
                    })
                    .and_then(|e| e.request.clone())
                    .expect("card")
            };
            feed(&session, &frame(0, "i0", None));
            session.respond_approval(&request_of("i0"), Decision::AllowForSession);
            feed(
                &session,
                &frame(
                    1,
                    "i1",
                    Some((
                        "additionalPermissions",
                        serde_json::json!({"network": true}),
                    )),
                ),
            );
            feed(
                &session,
                &frame(2, "i2", Some(("kind", serde_json::json!("escalated")))),
            );
            feed(&session, &frame(3, "i3", None));
            let (extra, kind, plain) = (request_of("i1"), request_of("i2"), request_of("i3"));
            assert!(pump_until(ctx, 10, || resolution(&seen, &plain).is_some()));
            assert_eq!(resolution(&seen, &extra), None, "an extra field asks");
            assert_eq!(resolution(&seen, &kind), None, "another kind asks");
        });
    }

    #[test]
    fn enrich_agy_step_reads_thinking_from_native_transcript() {
        let tmp = tempfile::tempdir().expect("tmp");
        let conv_id = "test-conv-12345678";
        let logs_dir = tmp
            .path()
            .join(".gemini/antigravity-cli/brain")
            .join(conv_id)
            .join(".system_generated/logs");
        std::fs::create_dir_all(&logs_dir).expect("mkdir");

        // Write a transcript entry containing a thinking block for step 2
        let transcript_line = serde_json::json!({
            "step_index": 2,
            "source": "MODEL",
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "thinking": "Observing the file contents to see existing functions.",
            "tool_calls": [{"name": "view_file"}]
        });
        std::fs::write(
            logs_dir.join("transcript.jsonl"),
            format!("{transcript_line}\n"),
        )
        .expect("write transcript");

        // Set HOME temporarily so find_agy_thinking locates the test transcript
        let _home = HomeGuard::set(tmp.path());

        in_loop(|ctx| {
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, "/w");
            let (sink, seen) = make_sink();
            let session = ChatSession::new(
                Box::new(agent_core::agy::AgyAdapter::new("agy")),
                OpenSession {
                    program: "unused".into(),
                    extra_args: Vec::new(),
                    cwd: "/".into(),
                    model: None,
                    effort: None,
                    mode: Mode::Plan,
                    resume: None,
                    new_session_id: None,
                    approval_hook: false,
                    delegation: None,
                },
                store,
                thread,
                sink,
                None,
            );

            // AGY stdout frame without thinking
            let stdout_frame = serde_json::json!({
                "event": "step_update",
                "step_update": {
                    "conversation_id": conv_id,
                    "state": "ACTIVE",
                    "step_index": 2,
                    "step_type": "tool",
                    "tool_name": "view_file",
                    "tool_info": {
                        "name": "view_file",
                        "parameters": {"AbsolutePath": "/work/file.rs"}
                    }
                }
            })
            .to_string();

            session
                .inner
                .on_line(session.inner.generation.get(), &stdout_frame, false);

            // No file I/O on the main thread: on_line returns with the line held back, and the
            // transcript is read on a worker before the adapter sees the step.
            let reasoning_started = || {
                seen.borrow().iter().any(|env| {
                    matches!(
                        &env.event,
                        Event::ItemStarted {
                            kind: ItemKind::Reasoning,
                            ..
                        }
                    )
                })
            };
            assert!(
                session.inner.agy_reading.get().is_some(),
                "the transcript read is in flight"
            );
            assert!(!reasoning_started(), "the step waits for the worker read");
            // A line that arrives meanwhile queues behind it, keeping the adapter's order.
            session.inner.on_line(
                session.inner.generation.get(),
                r#"{"event":"unknown_marker"}"#,
                false,
            );
            assert_eq!(session.inner.agy_hold.borrow().len(), 1);

            // The session emits a Reasoning ItemStarted + ContentDelta with the thinking text.
            assert!(
                pump_until(ctx, 5, reasoning_started),
                "expected ItemKind::Reasoning to be emitted"
            );
            assert!(session.inner.agy_reading.get().is_none());
            assert!(session.inner.agy_hold.borrow().is_empty());

            // Whole, as a snapshot: agy's thinking is never streamed for show.
            let reasoning_delta = seen.borrow().iter().find_map(|env| {
                if let Event::ContentSnapshot {
                    stream: StreamKind::Reasoning,
                    text,
                } = &env.event
                {
                    Some(text.clone())
                } else {
                    None
                }
            });
            assert_eq!(
                reasoning_delta.as_deref(),
                Some("Observing the file contents to see existing functions.")
            );

            // Redundant DONE step update should not emit a second Reasoning ItemStarted
            let done_frame = serde_json::json!({
                "event": "step_update",
                "step_update": {
                    "conversation_id": conv_id,
                    "state": "DONE",
                    "step_index": 2,
                    "step_type": "tool",
                    "tool_name": "view_file",
                    "tool_info": {
                        "name": "view_file",
                        "output": "file content",
                        "parameters": {"AbsolutePath": "/work/file.rs"}
                    }
                }
            })
            .to_string();

            session
                .inner
                .on_line(session.inner.generation.get(), &done_frame, false);

            let reasoning_count = seen
                .borrow()
                .iter()
                .filter(|env| {
                    matches!(
                        &env.event,
                        Event::ItemStarted {
                            kind: ItemKind::Reasoning,
                            ..
                        }
                    )
                })
                .count();
            assert_eq!(reasoning_count, 1, "thinking item should not be duplicated");
        });
    }

    /// An agy session on a transcript with thinking for step 2, and a first `step_update` for
    /// that step already sent (so a read is in flight).
    fn agy_mid_read(tmp: &Path) -> (Rc<ChatSession>, Seen) {
        let logs = tmp.join(".gemini/antigravity-cli/brain/cv/.system_generated/logs");
        std::fs::create_dir_all(&logs).expect("mkdir");
        std::fs::write(
            logs.join("transcript.jsonl"),
            "{\"step_index\":2,\"thinking\":\"pondering\"}\n",
        )
        .expect("write");
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let thread = fresh(&store, "/w");
        let (sink, seen) = make_sink();
        let session = ChatSession::new(
            Box::new(agent_core::agy::AgyAdapter::new("agy")),
            OpenSession {
                program: "unused".into(),
                extra_args: Vec::new(),
                cwd: "/".into(),
                model: None,
                effort: None,
                mode: Mode::Plan,
                resume: None,
                new_session_id: None,
                approval_hook: false,
                delegation: None,
            },
            store,
            thread,
            sink,
            None,
        );
        let frame = r#"{"event":"step_update","step_update":{"conversation_id":"cv","state":"ACTIVE","step_index":2,"step_type":"tool","tool_name":"view_file"}}"#;
        session
            .inner
            .on_line(session.inner.generation.get(), frame, false);
        assert!(
            session.inner.agy_reading.get().is_some(),
            "a read is in flight"
        );
        (session, seen)
    }

    fn reasoning_started(seen: &Seen) -> bool {
        has(seen, |e| {
            matches!(
                e,
                Event::ItemStarted {
                    kind: ItemKind::Reasoning,
                    ..
                }
            )
        })
    }

    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Points `HOME` at a test directory for as long as it lives, one test at a time, and puts
    /// it back on drop (removing it when it was unset), even if the test panics.
    struct HomeGuard {
        previous: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl HomeGuard {
        fn set(home: &Path) -> Self {
            let lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var_os("HOME");
            std::env::set_var("HOME", home);
            Self {
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(home) => std::env::set_var("HOME", home),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn an_exit_during_a_transcript_read_fires_once_after_the_held_lines() {
        let tmp = tempfile::tempdir().expect("tmp");
        let _home = HomeGuard::set(tmp.path());
        in_loop(|ctx| {
            let (session, seen) = agy_mid_read(tmp.path());
            let generation = session.inner.generation.get();
            session
                .inner
                .on_line(generation, r#"{"event":"held_marker"}"#, false);
            session.inner.on_exit(generation, Some(0));
            // Deferred: nothing exited yet, the line and the exit wait behind the read.
            assert_eq!(exited_count(&seen), 0);
            assert!(session.inner.agy_deferred_exit.get().is_some());

            assert!(pump_until(ctx, 5, || exited_count(&seen) == 1), "no exit");
            for _ in 0..50 {
                ctx.iteration(false);
            }
            assert_eq!(exited_count(&seen), 1, "the exit fires exactly once");
            let events = seen.borrow();
            let unknown = events
                .iter()
                .position(|e| matches!(e.event, Event::Unknown))
                .expect("the held line reached the adapter");
            let exit = events
                .iter()
                .position(|e| matches!(e.event, Event::SessionExited { .. }))
                .expect("exit");
            assert!(unknown < exit, "the held line comes before the exit");
            assert!(session.inner.agy_deferred_exit.get().is_none());
        });
    }

    #[test]
    fn a_restart_during_a_transcript_read_drops_the_stale_read() {
        let tmp = tempfile::tempdir().expect("tmp");
        let _home = HomeGuard::set(tmp.path());
        in_loop(|ctx| {
            let (session, seen) = agy_mid_read(tmp.path());
            let generation = session.inner.generation.get();
            session
                .inner
                .on_line(generation, r#"{"event":"held_marker"}"#, false);
            session.inner.on_exit(generation, Some(0));
            session.inner.stop_current();
            assert!(session.inner.agy_reading.get().is_none());
            assert!(session.inner.agy_hold.borrow().is_empty());
            assert!(session.inner.agy_deferred_exit.get().is_none());
            let before = seen.borrow().len();
            for _ in 0..200 {
                ctx.iteration(false);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            // The old read's result was dropped: no thinking, no held line, no second exit.
            assert!(!reasoning_started(&seen));
            assert!(!has(&seen, |e| matches!(e, Event::Unknown)));
            assert_eq!(exited_count(&seen), 1, "only the stop's own exit");
            assert_eq!(seen.borrow().len(), before);
        });
    }
}
