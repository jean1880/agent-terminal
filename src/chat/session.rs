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
use std::rc::Rc;
use std::time::Duration;

use agent_core::adapter::{
    Action, Adapter, AdapterError, Command, Control, Driver, Mode, OpenSession, OpenSessionDelta,
};
use agent_core::catalog::Replacement;
use agent_core::event::{AgentCommand, Decision, Envelope, Event, ItemKind, StreamKind, TurnState};
use agent_core::handoff_budget::{
    handoff_budget, handoff_coverage, provider_message_with_handoff, render_history,
    select_history, HistoricalMessage, Role, DEFAULT_HANDOFF_TOKEN_CAP,
};
use agent_core::transition::{
    decide_transition, plan_selection, ModelSelection, SessionState, Transition,
};
use agent_kit::store::{ProviderThreadId, Store, ThreadId, TranscriptMessage};
use gtk4::glib;
use tracing::{debug, info, warn};

use super::{ChatBackend, EnvelopeSink, SessionStatus};
use crate::agent_proc::{run_side, AgentEnv, AgentProcess, SpawnSpec, AGY_TIMEOUT};
use crate::approval_server::ApprovalHandle;

/// Process environment the adapter does not own: what the agent's profile adds (its env file)
/// and what the app removes (`clear_env`, a launching agent session's markers).
pub type LaunchEnv = AgentEnv;

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
    /// agy: the approval socket for the new process. `None`: no hook, so agy cannot ask (Ask runs
    /// as Plan; see [`effective_mode`]).
    pub approval: Option<ApprovalHandle>,
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
const CANARY_GRACE: Duration = Duration::from_secs(3);

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

struct Inner {
    adapter: RefCell<Box<dyn Adapter>>,
    open: RefCell<OpenSession>,
    store: Rc<Store>,
    thread: ThreadId,
    provider_thread: RefCell<ProviderThreadId>,
    sink: EnvelopeSink,
    /// Cleared (never re-set) when the canary proves the hook is not gating agy.
    approval: RefCell<Option<ApprovalHandle>>,
    /// How long a tool step may wait for its hook query to show up.
    canary_grace: Cell<Duration>,
    factory: RefCell<Option<AdapterFactory>>,
    launch_env: RefCell<LaunchEnv>,
    proc: RefCell<Option<AgentProcess>>,
    state: RefCell<State>,
    /// A rendered, redacted handoff waiting for the next user prompt.
    pending_handoff: RefCell<Option<String>>,
    /// The thread's model was retired by its agent: the next prompt first moves to this one.
    retired: RefCell<Option<RetiredModel>>,
    /// The model the thread was last SET to (an alias where the agent has one), as opposed to the
    /// resolved id its events report: what is stored and what retirement is judged on.
    selected: RefCell<Option<String>>,
    ctl_seq: Cell<u64>,
    local_seq: Cell<u64>,
    /// Bumped on every (re)start and stop: callbacks of an older process are ignored.
    generation: Cell<u64>,
    /// The mode the user last chose. The running mode can differ (agy without its hook cannot
    /// ask), and a switch to an agent that can honour it goes back to it.
    wanted_mode: Cell<Mode>,
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
        store: Rc<Store>,
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
            approval,
            LaunchEnv::default(),
        )
    }

    /// [`Self::new`] with the profile's environment applied to every process it starts.
    pub fn with_env(
        adapter: Box<dyn Adapter>,
        mut open: OpenSession,
        store: Rc<Store>,
        thread: ThreadId,
        sink: EnvelopeSink,
        approval: Option<ApprovalHandle>,
        env: LaunchEnv,
    ) -> Rc<Self> {
        let driver = adapter.driver();
        open.approval_hook = approval.is_some();
        let hookless_agy = driver == Driver::Agy && approval.is_none();
        let wanted_mode = open.mode;
        open.mode = effective_mode(hookless_agy, wanted_mode);
        let provider_thread = match store.active_provider_thread(&thread) {
            Ok(Some(p)) => p,
            _ => match create_provider_thread(&store, &thread, driver, open.model.as_deref()) {
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
            canary_grace: Cell::new(CANARY_GRACE),
            factory: RefCell::new(None),
            launch_env: RefCell::new(env),
            proc: RefCell::new(None),
            state: RefCell::new(state),
            pending_handoff: RefCell::new(None),
            retired: RefCell::new(None),
            selected: RefCell::new(selected),
            ctl_seq: Cell::new(0),
            local_seq: Cell::new(0),
            generation: Cell::new(0),
            wanted_mode: Cell::new(wanted_mode),
        });
        inner.attach_approval();
        // Said when the user's Ask could not be honoured; a thread already in Plan or Accept
        // edits does not repeat it on every reopen.
        if inner.state.borrow().mode != wanted_mode {
            inner.hookless_notice();
        }
        inner.start_process();
        Rc::new(Self { inner })
    }

    /// Lets `switch` build an adapter for the other driver (needed for `CreateWithHandoff`).
    pub fn set_adapter_factory(&self, factory: AdapterFactory) {
        *self.inner.factory.borrow_mut() = Some(factory);
    }

    /// Seeds a new thread with a budgeted, redacted handoff (fork, compact-by-handoff): it
    /// rides on the next real prompt, exactly like a cross-agent switch's.
    pub fn seed_handoff(&self, summary: String, carried: usize, source: &str) {
        if carried == 0 {
            return;
        }
        *self.inner.pending_handoff.borrow_mut() = Some(summary);
        self.inner.emit(Envelope::new(Event::Notice {
            text: format!("Continuing from {source} with {carried} earlier messages"),
        }));
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

    /// Adds a notice to the thread (stored and shown like any event).
    pub fn note(&self, text: impl Into<String>) {
        self.inner
            .emit(Envelope::new(Event::Notice { text: text.into() }));
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
        self.emit(Envelope::new(Event::Notice {
            text: format!(
                "Antigravity cannot ask you before acting: the approval hook is not installed. \
                 It refuses shell commands, but applies file edits on its own in every mode \
                 (in Plan it writes a plan first, then edits). Ask before edits is not \
                 available. To have it ask you first, add this top-level entry to \
                 ~/.gemini/config/hooks.json:\n{}",
                crate::hook_config::install_entry_json()
            ),
        }));
    }

    // ---- events ----

    /// Persists (under the current provider thread), updates the status, then delivers.
    fn emit(&self, env: Envelope) {
        let pt = self.provider_thread.borrow().clone();
        let provider = (!pt.is_empty()).then_some(pt.as_str());
        if let Err(e) = self.store.append_event(&self.thread, provider, &env) {
            warn!(error = %e, "could not persist an event");
        }
        self.apply(&env);
        (self.sink)(&env);
    }

    /// Delivers without persisting (the store already has it).
    fn deliver(&self, env: &Envelope) {
        self.apply(env);
        (self.sink)(env);
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
                if !native_id.is_empty() {
                    let pt = self.provider_thread.borrow().clone();
                    // A resume that started a new native session leaves the old one behind:
                    // it is this thread's history now, never a separate session to list.
                    let previous = self.state.borrow().native_id.clone();
                    if let Some(old) = previous.filter(|old| old != native_id) {
                        let driver = driver_name(self.adapter.borrow().driver());
                        if let Err(e) = self.store.dismiss_native(driver, &old) {
                            warn!(error = %e, "could not record the replaced native session");
                        }
                    }
                    if let Err(e) = self.store.set_native_id(&pt, native_id) {
                        warn!(error = %e, "could not record the native session id");
                    }
                    self.state.borrow_mut().native_id = Some(native_id.clone());
                }
                if let Some(m) = model {
                    self.set_model(m);
                }
            }
            Event::TurnStarted { model } => {
                if let Some(m) = model {
                    self.set_model(m);
                }
                self.state.borrow_mut().running_turn = true;
            }
            Event::TurnCompleted { .. } => self.state.borrow_mut().running_turn = false,
            Event::ModelChanged { model } => self.set_model(model),
            Event::ModeChanged { mode } => {
                self.state.borrow_mut().mode = *mode;
                self.open.borrow_mut().mode = *mode;
                if let Some(a) = self.approval() {
                    a.set_mode(*mode);
                }
            }
            Event::CommandsChanged { commands } => {
                self.state.borrow_mut().commands = commands.clone();
            }
            _ => {}
        }
    }

    fn set_model(&self, model: &str) {
        self.state.borrow_mut().model = Some(model.to_owned());
        self.open.borrow_mut().model = Some(model.to_owned());
    }

    // ---- process ----

    fn launch_spec(&self) -> SpawnSpec {
        // One source for the hook flag and the socket env var: both come from `approval`.
        let approval = self.approval();
        self.open.borrow_mut().approval_hook = approval.is_some();
        let open = self.open.borrow();
        let argv = self.adapter.borrow().argv(&open);
        let launch = self.launch_env.borrow();
        // The profile's environment first, so it can never override the approval socket.
        let mut env = launch.env.clone();
        env.extend(approval.map(|a| a.env()).unwrap_or_default());
        SpawnSpec {
            argv,
            cwd: (!open.cwd.is_empty()).then(|| open.cwd.clone()),
            env,
            unset: launch.unset.clone(),
        }
    }

    fn start_process(self: &Rc<Self>) {
        let spec = self.launch_spec();
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
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
                let handshake = self.adapter.borrow_mut().handshake();
                for line in &handshake {
                    proc.write_line(line);
                }
                *self.proc.borrow_mut() = Some(proc);
                self.state.borrow_mut().alive = true;
            }
            Err(e) => {
                self.state.borrow_mut().alive = false;
                self.error(e);
            }
        }
    }

    fn approval(&self) -> Option<ApprovalHandle> {
        self.approval.borrow().clone()
    }

    fn on_line(self: &Rc<Self>, generation: u64, line: &str, stderr: bool) {
        if generation != self.generation.get() {
            return;
        }
        let envelopes = if stderr {
            self.adapter.borrow_mut().feed_stderr(line)
        } else {
            self.adapter.borrow_mut().feed(line)
        };
        for env in envelopes {
            self.emit(env.clone());
            self.watch_tool_step(&env);
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
        let Event::ItemStarted { kind, .. } = &env.event else {
            return;
        };
        let Some(tools) = canary_tools(*kind) else {
            return;
        };
        let Some(approval) = self.approval() else {
            return;
        };
        if self.adapter.borrow().driver() != Driver::Agy || approval.consume_query(tools) {
            return;
        }
        let weak = Rc::downgrade(self);
        let generation = self.generation.get();
        let grace = self.canary_grace.get();
        glib::spawn_future_local(async move {
            glib::timeout_future(grace).await;
            let Some(inner) = weak.upgrade() else { return };
            if inner.generation.get() != generation {
                return; // that process is already gone
            }
            if inner.approval().is_some_and(|a| a.consume_query(tools)) {
                return;
            }
            inner.trip_canary();
        });
    }

    /// The hook is not gating agy: stop it now and carry on without the skip flag (plan mode, no
    /// socket), so commands are refused again.
    fn trip_canary(self: &Rc<Self>) {
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
    }

    fn on_exit(&self, generation: u64, code: Option<i32>) {
        if generation != self.generation.get() {
            return;
        }
        info!(code = ?code, "agent exited");
        self.finish_process(code, false);
    }

    /// Closes out the current process: adapter exit envelopes, status, pending approvals.
    /// `deliberate`: the app stopped it (a switch or a close), so its exit is expected and an
    /// open turn was interrupted, never failed.
    fn finish_process(&self, code: Option<i32>, deliberate: bool) {
        let proc = self.proc.borrow_mut().take();
        drop(proc); // terminates it when it is still running
        let mut envelopes = self.adapter.borrow_mut().on_exit(code);
        if deliberate {
            envelopes.iter_mut().for_each(as_deliberate_stop);
        }
        {
            let mut state = self.state.borrow_mut();
            state.alive = false;
            state.running_turn = false;
        }
        for env in envelopes {
            self.emit(env);
        }
        if let Some(a) = self.approval() {
            a.expire_all("the agent exited");
        }
    }

    /// Stops the process on purpose; its own exit callback is then ignored.
    fn stop_current(&self) {
        self.generation.set(self.generation.get() + 1);
        self.finish_process(None, true);
    }

    /// A dead process (agy after an interrupt) is restarted resuming its native session.
    fn ensure_alive(self: &Rc<Self>) {
        if self.state.borrow().alive {
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

    // ---- commands ----

    /// Encodes and carries out one command. False when the adapter refused it.
    fn command(self: &Rc<Self>, command: Command) -> bool {
        let result = self.adapter.borrow_mut().encode(command);
        match result {
            Ok(actions) => {
                self.execute(actions);
                self.drain_outbox();
                true
            }
            Err(e) => {
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
                self.emit(env.clone());
                self.watch_tool_step(&env);
            }
            self.execute(outbox.actions);
        }
    }

    fn execute(self: &Rc<Self>, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Write(lines) => {
                    let written = {
                        let proc = self.proc.borrow();
                        match proc.as_ref().filter(|p| p.is_alive()) {
                            Some(p) => {
                                for line in &lines {
                                    p.write_line(line);
                                }
                                true
                            }
                            None => false,
                        }
                    };
                    if !written {
                        self.error("The agent is not running.");
                    }
                }
                Action::Interrupt => {
                    if let Some(p) = self.proc.borrow().as_ref() {
                        p.interrupt();
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
        glib::spawn_future_local(async move {
            let (stdout, ok) = run_side(argv, cwd, &env, AGY_TIMEOUT).await;
            let Some(inner) = weak.upgrade() else { return };
            let envelopes = inner.adapter.borrow_mut().feed_side(&id, &stdout, ok);
            for env in envelopes {
                inner.emit(env);
            }
            inner.drain_outbox();
        });
    }

    /// Stops the process and starts a new one with `delta` applied (the adapter set
    /// `resume` to the native id so history carries over).
    fn respawn(self: &Rc<Self>, delta: &OpenSessionDelta) {
        self.stop_current();
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
                self.state.borrow_mut().effort = Some(e.clone());
            }
            if let Some(mode) = mode {
                open.mode = mode;
            }
            open.resume = delta.resume.clone().or(native);
            open.new_session_id = None;
        }
        if let Some(m) = &delta.model {
            self.emit(Envelope::new(Event::ModelChanged { model: m.clone() }));
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

    fn next_local_id(&self, prefix: &str) -> String {
        let n = self.local_seq.get() + 1;
        self.local_seq.set(n);
        format!("{prefix}-{n}")
    }

    /// A prompt must not start a turn on a retired model: move to its replacement first.
    fn apply_retired(self: &Rc<Self>) {
        let Some(retired) = self.retired.borrow_mut().take() else {
            return;
        };
        let driver = self.adapter.borrow().driver();
        self.switch(
            driver,
            Some(retired.replacement.model.clone()),
            retired.replacement.effort.clone(),
        );
        self.emit(Envelope::new(Event::Notice {
            text: retired_notice(&retired),
        }));
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

    fn send_prompt(self: &Rc<Self>, text: &str) {
        self.apply_retired();
        let pt = self.provider_thread.borrow().clone();
        let provider = (!pt.is_empty()).then_some(pt.as_str());
        let item = match self.store.append_user_message(&self.thread, provider, text) {
            Ok(item) => item,
            Err(e) => {
                warn!(error = %e, "could not persist the user message");
                self.next_local_id("user")
            }
        };
        // The store already holds these two; the view still needs to see them.
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
                text: text.to_owned(),
            })
            .item(item),
        );

        // The handoff rides on the first real prompt: never on a slash command (it would stop
        // being one), and it is only spent once the agent accepted the prompt.
        let handoff = if Self::is_slash_command(text) {
            None
        } else {
            self.pending_handoff.borrow().clone()
        };
        let sent = match &handoff {
            Some(summary) => provider_message_with_handoff(summary, text),
            None => text.to_owned(),
        };
        self.ensure_alive();
        if self.command(Command::Prompt { text: sent }) && handoff.is_some() {
            *self.pending_handoff.borrow_mut() = None;
        }
    }

    fn respond_approval(self: &Rc<Self>, request: &str, decision: Decision) {
        if let Some(handle) = self.approval() {
            if handle.respond(request, decision) {
                self.emit(Envelope::new(Event::ApprovalResolved { decision }).request(request));
                return;
            }
        }
        // Claude and Codex never acknowledge an answer, so once the adapter has accepted it and
        // written it, the card is resolved here; otherwise it would sit at "Allow…" for good.
        let sent = self.command(Command::Approve {
            request: request.to_owned(),
            decision,
            updated_input: None,
            message: None,
        });
        if sent {
            self.emit(Envelope::new(Event::ApprovalResolved { decision }).request(request));
        }
    }

    fn set_mode(self: &Rc<Self>, mode: Mode) {
        self.wanted_mode.set(mode);
        if self.ask_unavailable() && mode == Mode::Ask {
            self.emit(Envelope::new(Event::Notice {
                text: "Antigravity cannot ask before edits until the approval hook is installed; \
                       it stays in its current mode."
                    .to_owned(),
            }));
            // The picker shows the mode actually running.
            let current = self.state.borrow().mode;
            self.emit(Envelope::new(Event::ModeChanged { mode: current }));
            return;
        }
        if self.command(Command::SetMode { mode }) {
            if let Some(a) = self.approval() {
                a.set_mode(mode);
            }
        }
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
                self.execute(actions);
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

    fn switch(self: &Rc<Self>, driver: Driver, model: Option<String>, effort: Option<String>) {
        let (caps, current_driver, mode, status_model, current_effort) = {
            let adapter = self.adapter.borrow();
            let state = self.state.borrow();
            (
                adapter.capabilities().clone(),
                adapter.driver(),
                state.mode,
                state.model.clone(),
                state.effort.clone(),
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
        // A choice made now settles any pending question about a retired model, and a model
        // switched to on this agent is what a reopened thread resumes on.
        *self.retired.borrow_mut() = None;
        if matches!(
            transition,
            Transition::SwitchModelInSession | Transition::RestartAndResume
        ) && !target_model.is_empty()
        {
            self.persist_model(&target_model);
        }
        match transition {
            Transition::Reuse => {}
            Transition::SwitchModelInSession => {
                // The plan only picks this when the effort is unchanged, so passing the target's
                // is the same as keeping it.
                self.command(Command::SetModel {
                    model: target_model,
                    effort,
                });
            }
            Transition::RestartAndResume => {
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
                        let respawns = actions.iter().any(|a| matches!(a, Action::Respawn(_)));
                        if let (false, Some(e)) = (respawns, &effort) {
                            self.open.borrow_mut().effort = Some(e.clone());
                            self.state.borrow_mut().effort = Some(e.clone());
                        }
                        self.execute(actions);
                        self.drain_outbox();
                    }
                    Err(AdapterError::Unsupported(_)) => self.respawn(&OpenSessionDelta {
                        model: Some(target_model),
                        effort,
                        mode: None,
                        resume: native,
                    }),
                    Err(e) => self.report(&e),
                }
            }
            Transition::CreateWithHandoff => self.create_with_handoff(driver, model, effort),
            Transition::Reject(reason) => self.error(reason),
        }
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
        let messages = match self.store.transcript_messages(&self.thread) {
            Ok(m) => m,
            Err(e) => {
                self.error(format!("Could not read the thread to hand it off: {e}"));
                return;
            }
        };
        let from = self.adapter.borrow().driver();
        let (summary, carried) = build_handoff(
            &messages,
            &format!("{} thread {}", driver_label(from), self.thread),
        );

        let launch = match factory(driver) {
            Ok(launch) => launch,
            Err(reason) => {
                self.error(reason);
                return;
            }
        };
        let model = model.or(launch.default_model);
        let effort = effort.or(launch.default_effort);
        // The new provider thread comes first: if the store refuses, the old agent keeps running.
        let new_pt =
            match create_provider_thread(&self.store, &self.thread, driver, model.as_deref()) {
                Ok(p) => p,
                Err(e) => {
                    self.error(format!("Could not start the new agent's thread: {e}"));
                    return;
                }
            };
        // The old process's closing events still belong to the old provider thread, and its
        // pending approvals expire on the old socket.
        self.stop_current();
        *self.provider_thread.borrow_mut() = new_pt;
        *self.adapter.borrow_mut() = launch.adapter;
        *self.launch_env.borrow_mut() = launch.env;
        *self.approval.borrow_mut() = launch.approval;
        // The user's own mode, as far as the new agent can honour it (agy without its hook
        // cannot ask). A thread that went through hookless agy gets its Ask back here.
        let hookless_agy = driver == Driver::Agy && self.approval().is_none();
        let new_mode = effective_mode(hookless_agy, self.wanted_mode.get());
        let mode_changed = self.state.borrow().mode != new_mode;
        {
            let mut open = self.open.borrow_mut();
            // The new agent's own binary and arguments, from its profile.
            open.program = launch.program;
            open.extra_args = launch.extra_args;
            open.model = model.clone();
            open.effort = effort.clone();
            open.resume = None;
            open.new_session_id =
                (driver == Driver::Claude).then(|| glib::uuid_string_random().to_string());
            open.mode = new_mode;
        }
        {
            let mut state = self.state.borrow_mut();
            state.model = model.clone();
            *self.selected.borrow_mut() = model.clone();
            state.effort = effort.clone();
            state.native_id = None;
            state.commands.clear();
            state.running_turn = false;
            state.mode = new_mode;
        }
        self.attach_approval();
        *self.pending_handoff.borrow_mut() = (carried > 0).then_some(summary);
        self.emit(Envelope::new(Event::Notice {
            text: if carried == 0 {
                format!("Switched to {}", driver_label(driver))
            } else {
                format!(
                    "Continuing in {} with {carried} earlier messages",
                    driver_label(driver)
                )
            },
        }));
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
    }

    fn status(&self) -> SessionStatus {
        let adapter = self.adapter.borrow();
        let state = self.state.borrow();
        SessionStatus {
            driver: adapter.driver(),
            model: state.model.clone(),
            effort: state.effort.clone(),
            mode: state.mode,
            running_turn: state.running_turn,
            alive: state.alive,
            capabilities: adapter.capabilities().clone(),
            commands: state.commands.clone(),
        }
    }
}

/// The budgeted, redacted handoff of a thread's history and how many messages it carries.
/// Tool items count as assistant history; items with no text are skipped, and an item still
/// open (its process died with it) is described as interrupted.
pub(crate) fn build_handoff(messages: &[TranscriptMessage], source: &str) -> (String, usize) {
    let history: Vec<HistoricalMessage> = messages
        .iter()
        .filter(|m| !m.text.trim().is_empty())
        .map(|m| HistoricalMessage {
            role: if m.role == "user" {
                Role::User
            } else {
                Role::Assistant
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
    let selection = select_history(&history, &coverage, budget);
    let carried = selection.messages.len();
    (
        render_history(&selection.messages, &selection.context),
        carried,
    )
}

impl ChatBackend for ChatSession {
    fn send_prompt(&self, text: &str) {
        self.inner.send_prompt(text);
    }

    fn interrupt(&self) {
        self.inner.command(Command::Interrupt);
    }

    fn respond_approval(&self, request: &str, decision: Decision) {
        self.inner.respond_approval(request, decision);
    }

    fn answer_questions(&self, request: &str, answers: serde_json::Value) {
        self.inner.command(Command::Answer {
            request: request.to_owned(),
            answers,
        });
    }

    fn switch(&self, driver: Driver, model: Option<String>, effort: Option<String>) {
        self.inner.switch(driver, model, effort);
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
mod tests {
    use super::*;
    use crate::testutil::{in_loop, pump_until};
    use agent_core::agy::AgyAdapter;
    use agent_core::caps::Capabilities;
    use std::path::Path;
    use std::time::Duration;

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
        let script = dir.join("agent.sh");
        std::fs::write(
            &script,
            concat!(
                "D=$(dirname \"$0\")\n",
                "echo \"$@\" >> \"$D/args.log\"\n",
                "echo \"${AGENT_TERMINAL_APPROVAL_SOCKET-unset}\" >> \"$D/sock.log\"\n",
                "echo \"${AGENT_TERMINAL_HOOK_BIN-unset}\" >> \"$D/hookbin.log\"\n",
                "read line\n",
                "cat \"$D/frames.ndjson\"\n",
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
            session.respond_approval("r1", Decision::AllowForSession);
            assert!(seen.borrow().iter().any(|e| {
                e.request.as_deref() == Some("r1")
                    && matches!(
                        e.event,
                        Event::ApprovalResolved {
                            decision: Decision::AllowForSession
                        }
                    )
            }));
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
                |e| matches!(e, Event::ModelChanged { model } if model == "gemini-flash")
            ));
            assert_eq!(session.status().model.as_deref(), Some("gemini-flash"));
            session.send_prompt("again");
            assert!(pump_until(ctx, 15, || exited_count(&seen) >= 3));
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
        fn argv(&self, _: &OpenSession) -> Vec<String> {
            vec!["/bin/cat".into()]
        }
        fn handshake(&mut self) -> Vec<String> {
            Vec::new()
        }
        fn encode(&mut self, command: Command) -> Result<Vec<Action>, AdapterError> {
            let refuse = self.refuse_set_model && matches!(command, Command::SetModel { .. });
            self.log.borrow_mut().push(command);
            if refuse {
                return Err(AdapterError::Unsupported("set model"));
            }
            Ok(Vec::new())
        }
        fn feed(&mut self, _: &str) -> Vec<Envelope> {
            Vec::new()
        }
        fn feed_stderr(&mut self, _: &str) -> Vec<Envelope> {
            Vec::new()
        }
        fn feed_side(&mut self, _: &str, _: &str, _: bool) -> Vec<Envelope> {
            Vec::new()
        }
        fn on_exit(&mut self, code: Option<i32>) -> Vec<Envelope> {
            vec![Envelope::new(Event::SessionExited {
                code,
                expected: true,
            })]
        }
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
            approval: None,
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
                    approval: None,
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
            assert_eq!(spec.unset, ["CLAUDECODE"]);
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
                |e| matches!(e, Event::ModelChanged { model } if model == "claude-opus-5-5")
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
            assert_eq!(status.model.as_deref(), Some("opus"));
            assert!(status.alive);
            assert!(has(&seen, |e| matches!(e, Event::Notice { text }
                if text == "Continuing in Claude with 2 earlier messages")));
            assert!(has(
                &seen,
                |e| matches!(e, Event::ModelChanged { model } if model == "opus")
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
            assert!(sent[0].starts_with("Context handoff:"), "{}", sent[0]);
            assert!(sent[0].contains("please list the repo"));
            assert!(sent[0].contains("Found three files"));
            assert!(sent[0].contains("User message:\nnow summarize"));
            assert!(!sent[0].contains(secret), "secret leaked into the handoff");
            assert_eq!(sent[1], "and again");
            assert!(prompts(&log_agy).is_empty());
            let msgs = store.transcript_messages(&thread).expect("msgs");
            assert!(msgs.iter().any(|m| m.text == "now summarize"));
            assert!(!msgs.iter().any(|m| m.text.contains("Context handoff")));
            // New events belong to the new provider thread.
            let all = store.events(&thread, None, 1000).expect("events");
            assert!(all.len() > 5);
        });
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
                },
                store,
                thread,
                sink,
                None,
            );
            session.switch(Driver::Agy, Some("gemini-flash".into()), None);
            assert!(has(
                &seen,
                |e| matches!(e, Event::ModelChanged { model } if model == "gemini-flash")
            ));
            let status = session.status();
            assert_eq!(status.model.as_deref(), Some("gemini-flash"));
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

    fn pt_of(store: &Store, thread: &str) -> ProviderThreadId {
        create_provider_thread(store, thread, Driver::Agy, Some("gemini-pro")).expect("pt")
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
        let (summary, carried) = build_handoff(&history, "Claude thread t");
        assert_eq!(carried, 3, "the empty item is skipped");
        assert!(!summary.contains(secret));
        assert!(summary.contains("****0123"));
        assert!(summary.contains("status=interrupted"));
        assert!(summary.contains("Claude thread t"));
        assert_eq!(build_handoff(&[], "x").1, 0);
    }
}
