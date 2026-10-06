//! The chat-first shell: thread pages (a [`ChatView`] over a [`ChatSession`]) inside the hidden
//! tab view, the thread sidebar, the terminal drawer, and the 2.x features re-homed onto them.
//!
//! A thread page is an ordinary [`TabState`] with `chat: Some(..)`, so the registry, the diff
//! panel, checkpoints, worktrees and attention keep working through one code path. Its
//! `terminal` is the drawer's shell, spawned the first time the drawer opens.
//!
//! Re-entrancy: a session may call its sink (and so [`AgentTerminalWindow::on_thread_envelope`])
//! from inside any call into it, so no `tabs` borrow is ever held across a call into a session
//! or a view.

use std::collections::HashMap;
use std::rc::Rc;

use agent_core::adapter::{Adapter, Control, Driver, Mode, OpenSession};
use agent_core::catalog::{model_notice, ModelNotice};
use agent_core::event::{Decision, Envelope, Event, ItemKind};
use agent_kit::store::Store;

use super::diffs::ThreadDiffs;
use super::*;
use crate::account_status::AccountStatus;
use crate::agent_proc::AgentEnv;
use crate::approval_server::ApprovalHandle;
use crate::availability::{
    classify, probe_targets, unavailable_banner, AgentAvailability, Availability,
};
use crate::chat::session::{
    build_handoff, retired_notice, AgentLaunch, Approval, ChatSession, LaunchEnv, RetiredModel,
};
use crate::chat::view::usage::UsageIndicator;
use crate::chat::view::{ChatView, ViewAction};
use crate::chat::{ChatBackend, EnvelopeSink, SessionStatus};
use crate::config::{profile_driver, Profile};
use crate::model_catalog::{retirement_banner, ModelCatalog};
use crate::window::sidebar_model::{
    badge_for, driver_key, driver_label, group_rows, handoff_target, needs_attention, parse_driver,
    relative_time, resume_as, stored_model, thread_title, Badge, ResumeAs, RowKey, SidebarRow,
};

/// The store key holding the open-thread list (`{"open": [ids], "selected": id}`).
const OPEN_THREADS_KEY: &str = "open_threads";
/// Most events replayed into a thread's view when it opens.
const REPLAY_LIMIT: usize = 50_000;
/// How often the account/usage indicator refreshes while the window is focused.
const ACCOUNT_REFRESH: std::time::Duration = std::time::Duration::from_secs(600);

/// One chat thread page's state, beside its [`TabState`].
pub(super) struct ChatTab {
    pub(super) thread: String,
    /// The agent when the page was opened, until the session reports its own.
    pub(super) driver: Driver,
    pub(super) title: String,
    /// Holds the chat view once built (the page is built the first time it is shown).
    holder: gtk4::Box,
    pub(super) slot: Rc<SessionSlot>,
    view: Option<ChatView>,
    building: bool,
    /// The drawer: the paned's end child, hidden until toggled.
    drawer: gtk4::Box,
    drawer_paned: gtk4::Paned,
    drawer_started: bool,
    rate_banner: adw::Banner,
    /// Shown when the thread's model is no longer offered by its agent.
    model_banner: adw::Banner,
    running: bool,
    approval: bool,
    rate_limited: bool,
    unread: bool,
    /// The user message item awaiting its text, for the title of an untitled thread.
    title_item: Option<String>,
    /// Sent once the session exists (Continue In from a terminal page).
    pending_prompt: Option<String>,
    /// Seeded into the session once it exists (fork, compact-by-handoff).
    pending_handoff: Option<(String, usize, String)>,
    /// The reasoning effort the session starts with, overriding the profile's default (a thread
    /// continued in another agent at a chosen effort).
    pending_effort: Option<String>,
    /// A switch (agent, model, effort) to apply once the session exists (the thread menu's
    /// "Switch this thread to" on a thread that is not built yet).
    pub(super) pending_switch: Option<(Driver, Option<String>, Option<String>)>,
    /// The banner is showing because the thread's agent is missing or off (not a rate limit).
    unavailable: bool,
    /// A session start is in flight for a view that was built without one (its agent was not
    /// Ready); stops a second availability change from starting it twice.
    starting: bool,
    /// The thread's pre-turn baselines and the source of its file-change cards' diffs.
    pub(super) diffs: Rc<ThreadDiffs>,
}

// ---------------------------------------------------------------------------------------------
// The backend the view is built with, before its session exists
// ---------------------------------------------------------------------------------------------

/// The view needs a backend at construction and the session needs the view's sink, so the view
/// gets this slot, which forwards to the session once it is set.
pub(super) struct SessionSlot {
    session: RefCell<Option<Rc<ChatSession>>>,
    /// The view, to tell the user when a prompt cannot go anywhere yet (the thread's agent is
    /// not Ready, so no session exists). Weak: the view owns this slot.
    view: RefCell<Option<glib::WeakRef<ChatView>>>,
    driver: Driver,
    model: Option<String>,
    mode: Mode,
}

impl SessionSlot {
    pub(super) fn get(&self) -> Option<Rc<ChatSession>> {
        self.session.borrow().clone()
    }

    pub(super) fn driver(&self) -> Driver {
        match self.get() {
            Some(s) => s.status().driver,
            None => self.driver,
        }
    }
}

impl ChatBackend for SessionSlot {
    fn send_prompt(&self, text: &str) {
        match self.get() {
            Some(s) => s.send_prompt(text),
            None => {
                warn!("a prompt arrived before the thread's session started");
                let view = self.view.borrow().as_ref().and_then(|v| v.upgrade());
                if let Some(view) = view {
                    view.apply(&Envelope::new(Event::Error {
                        message: format!(
                            "{} is not running for this thread yet, so the message was not sent.",
                            driver_label(self.driver)
                        ),
                    }));
                }
            }
        }
    }
    fn interrupt(&self) {
        if let Some(s) = self.get() {
            s.interrupt();
        }
    }
    fn respond_approval(&self, request: &str, decision: Decision) {
        if let Some(s) = self.get() {
            s.respond_approval(request, decision);
        }
    }
    fn answer_questions(&self, request: &str, answers: serde_json::Value) {
        if let Some(s) = self.get() {
            s.answer_questions(request, answers);
        }
    }
    fn switch(&self, driver: Driver, model: Option<String>, effort: Option<String>) {
        if let Some(s) = self.get() {
            s.switch(driver, model, effort);
        }
    }
    fn set_mode(&self, mode: Mode) {
        if let Some(s) = self.get() {
            s.set_mode(mode);
        }
    }
    fn control(&self, control: Control) -> String {
        match self.get() {
            Some(s) => s.control(control),
            None => "ctl-unstarted".to_owned(),
        }
    }
    fn status(&self) -> SessionStatus {
        match self.get() {
            Some(s) => s.status(),
            None => SessionStatus {
                driver: self.driver,
                model: self.model.clone(),
                effort: None,
                mode: self.mode,
                running_turn: false,
                alive: false,
                capabilities: agent_core::caps::Capabilities::of(self.driver),
                commands: Vec::new(),
            },
        }
    }
}

// ---------------------------------------------------------------------------------------------
// App-wide state: the store and the resolved agent binaries
// ---------------------------------------------------------------------------------------------

/// A chat agent's binary and its profile's environment, resolved off the main thread.
#[derive(Debug, Clone, Default)]
pub(super) struct ResolvedAgent {
    /// `None`: not found by any probe (the command name is then tried as-is).
    pub(super) program: Option<String>,
    pub(super) env: Vec<(String, String)>,
}

thread_local! {
    static STORE: RefCell<Option<Rc<Store>>> = const { RefCell::new(None) };
    /// Where the app's store lives, once it opened a file one. `None` (memory only, tests): there
    /// is nothing a second connection could open, so store jobs run in place.
    static STORE_PATH: RefCell<Option<std::path::PathBuf>> = const { RefCell::new(None) };
    /// Keyed by (command, env file): a profile edit is a new key, so nothing goes stale.
    static RESOLVED: RefCell<HashMap<(String, Option<String>), ResolvedAgent>> =
        RefCell::new(HashMap::new());
    static CHAT_RESTORED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The app's one thread store, opened on first use. A store that cannot be opened falls back
/// to memory (threads then last for this run), and says so in the log.
pub(super) fn app_store() -> Rc<Store> {
    match try_app_store() {
        Ok(store) => store,
        // Unreachable in practice: `setup_shell` calls `try_app_store` first and puts a dialog up
        // when even the in-memory store cannot open, before anything else asks for it.
        Err(e) => {
            error!("SQLite is unusable: {e}");
            std::process::exit(70)
        }
    }
}

/// [`app_store`], reporting the one failure it cannot recover from: no SQLite at all.
pub(super) fn try_app_store() -> Result<Rc<Store>, String> {
    if let Some(store) = STORE.with(|s| s.borrow().clone()) {
        return Ok(store);
    }
    let store = open_store()?;
    STORE.with(|s| *s.borrow_mut() = Some(store.clone()));
    Ok(store)
}

#[cfg(not(test))]
fn open_store() -> Result<Rc<Store>, String> {
    let path = agent_kit::store::default_path(
        env::var("XDG_STATE_HOME").ok().as_deref(),
        env::var("HOME").ok().as_deref(),
    );
    if let Some(path) = path {
        match Store::open(&path) {
            Ok(store) => {
                STORE_PATH.with(|p| *p.borrow_mut() = Some(path));
                return Ok(Rc::new(store));
            }
            Err(e) => error!("Cannot open the thread store at {}: {e}", path.display()),
        }
    }
    warn!("Threads are kept in memory for this run only");
    in_memory_store()
}

/// Whether [`store_job`] really leaves the main thread (a file store); false for the in-memory
/// fallback and under test, where it runs the job in place.
fn store_is_async() -> bool {
    STORE_PATH.with(|p| p.borrow().is_some())
}

/// Runs `job` against the thread store off the main thread, on a connection of its own (SQLite
/// connections are not `Send`; WAL lets it run beside the main one), and returns its result.
/// `None`: the worker could not open the store or panicked, already logged. With no file store
/// the job runs in place on the app's.
pub(super) async fn store_job<T: Send + 'static>(
    job: impl FnOnce(&Store) -> T + Send + 'static,
) -> Option<T> {
    let Some(path) = STORE_PATH.with(|p| p.borrow().clone()) else {
        return Some(job(&app_store()));
    };
    gtk4::gio::spawn_blocking(move || match Store::open(&path) {
        Ok(store) => Some(job(&store)),
        Err(e) => {
            warn!("Cannot open the thread store on a worker: {e}");
            None
        }
    })
    .await
    .unwrap_or_else(|_| {
        error!("A store job panicked on the worker thread");
        None
    })
}

/// A thread to create (see [`AgentTerminalWindow::create_chat_thread`]).
pub(super) struct NewThread {
    pub(super) driver: Driver,
    /// The folder (the agent's, else the starting directory, when `None`).
    pub(super) dir: Option<String>,
    /// Sent once the session has started.
    pub(super) prompt: Option<String>,
    pub(super) model: Option<String>,
    pub(super) effort: Option<String>,
    pub(super) title: Option<String>,
    /// A budgeted, redacted handoff (summary, messages carried, source) seeded into the session.
    pub(super) handoff: Option<(String, usize, String)>,
}

impl NewThread {
    pub(super) fn new(driver: Driver) -> Self {
        Self {
            driver,
            dir: None,
            prompt: None,
            model: None,
            effort: None,
            title: None,
            handoff: None,
        }
    }
}

/// `model` and `effort` as they should be started: unchanged unless the agent's list, fetched in
/// this run, no longer has `model`, then its replacement (and the record of the move).
fn decide_live_model(
    models: &[agent_core::catalog::CatalogModel],
    fresh: bool,
    model: Option<String>,
    effort: Option<String>,
) -> (Option<String>, Option<String>, Option<RetiredModel>) {
    match model_notice(models, fresh, model.as_deref(), effort.as_deref()) {
        ModelNotice::Retired {
            model: old,
            replacement: Some(replacement),
        } => (
            Some(replacement.model.clone()),
            replacement.effort.clone().or(effort),
            Some(RetiredModel {
                model: old,
                replacement,
            }),
        ),
        _ => (model, effort, None),
    }
}

/// [`decide_live_model`] against the app's catalogue.
fn live_model(
    driver: Driver,
    model: Option<String>,
    effort: Option<String>,
) -> (Option<String>, Option<String>, Option<RetiredModel>) {
    let catalog = ModelCatalog::shared();
    decide_live_model(
        &catalog.models_of(driver),
        catalog.is_fresh(driver),
        model,
        effort,
    )
}

/// What starting a built thread's session needs.
struct StartJob {
    thread: String,
    driver: Driver,
    dir: String,
    profile: Profile,
    resolved: ResolvedAgent,
    /// The active provider thread (its native session id and model), to resume.
    provider: Option<agent_kit::store::ProviderThread>,
    /// agy's hook verdict (always `Ok` for the others).
    hook: Result<(), String>,
}

/// What a thread page needs from the store to be built, read in one off-thread job.
#[derive(Default)]
struct LoadedThread {
    history: Vec<Envelope>,
    /// The active provider thread (its native session id and model), to resume.
    provider: Option<agent_kit::store::ProviderThread>,
}

fn load_provider(
    store: &Store,
    thread: &str,
) -> agent_kit::store::Result<Option<agent_kit::store::ProviderThread>> {
    let Some(id) = store.active_provider_thread(thread)? else {
        return Ok(None);
    };
    Ok(store
        .provider_threads(thread)?
        .into_iter()
        .find(|p| p.id == id))
}

/// Reads a thread for its page. A thread that points at an agent's own session but holds no
/// events yet (a resumed session, or one listed from the agent's history) first gets that
/// session's transcript imported, so the conversation shows. Runs on a store worker.
///
/// A store error fails the whole load: a thread read as empty with no provider would start a
/// fresh session and re-point the thread at it, losing the one it had.
fn load_thread(
    store: &Store,
    thread: &str,
    home: Option<&std::path::Path>,
) -> agent_kit::store::Result<LoadedThread> {
    let provider = load_provider(store, thread)?;
    let mut history = read_history(store, thread)?;
    if history.is_empty() {
        if let (Some(home), Some(pt)) = (home, provider.as_ref()) {
            if import_native_history(store, thread, pt, home) {
                history = read_history(store, thread)?;
            }
        }
    }
    Ok(LoadedThread { history, provider })
}

/// How many of the agents' own recent sessions are listed as threads.
const RECENT_SESSIONS: usize = 20;

/// Gives each recent native session a thread, unless one already holds it or the user deleted
/// that thread. The thread is dated with the session's time and titled after it; its history is
/// imported when it is first opened ([`load_thread`]). Returns how many were added.
fn link_recent_sessions(
    store: &Store,
    sessions: &[agent_kit::native::NativeSession],
    fallback_cwd: &str,
) -> usize {
    let mut added = 0;
    for s in sessions {
        let key = driver_key(s.driver);
        let known = store
            .thread_with_native_id(key, &s.native_id)
            .map(|t| t.is_some())
            .unwrap_or(true);
        let dismissed = store.native_dismissed(key, &s.native_id).unwrap_or(true);
        if known || dismissed {
            continue;
        }
        let cwd = s.cwd.as_deref().unwrap_or(fallback_cwd);
        let made = store.link_native_thread(
            cwd,
            s.title.as_deref(),
            s.modified.saturating_mul(1000),
            key,
            &s.native_id,
        );
        match made {
            Ok(_) => added += 1,
            Err(e) => warn!("Cannot list a {} session: {e}", driver_label(s.driver)),
        }
    }
    added
}

/// Imports `pt`'s native session into the store; whether anything was stored. A transcript that
/// cannot be read leaves a notice instead (so it is not retried on every open).
fn import_native_history(
    store: &Store,
    thread: &str,
    pt: &agent_kit::store::ProviderThread,
    home: &std::path::Path,
) -> bool {
    let (Some(native_id), Some(driver)) = (pt.native_id.clone(), Driver::from_key(&pt.driver))
    else {
        return false;
    };
    let session = agent_kit::native::NativeSession {
        driver,
        native_id,
        cwd: None,
        title: None,
        modified: 0,
    };
    let envs = match agent_kit::native::read_native_history(home, &session) {
        Ok(envs) => envs,
        Err(e) => {
            warn!(
                "Cannot import {} session history: {e}",
                driver_label(driver)
            );
            vec![Envelope::new(Event::Notice {
                text: format!("The earlier conversation could not be loaded: {e}"),
            })]
        }
    };
    if envs.is_empty() {
        return false;
    }
    match store.import_events(thread, Some(&pt.id), &envs) {
        Ok(n) => {
            info!("Imported {n} events of a {} session", driver_label(driver));
            true
        }
        Err(e) => {
            warn!("Cannot store the imported history: {e}");
            false
        }
    }
}

/// How long a session start waits for its agent's model list to be fetched in this run, so a
/// model the agent retired is ported before it reaches argv. A slow probe does not hold a thread
/// hostage: after this it starts, and the later `set_retired` path still catches the model.
const CATALOG_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// Polls `done` every `step` until it holds or `limit` passes; whether it held.
pub(super) async fn wait_until(
    limit: std::time::Duration,
    step: std::time::Duration,
    done: impl Fn() -> bool,
) -> bool {
    let started = std::time::Instant::now();
    while !done() {
        if started.elapsed() >= limit {
            return false;
        }
        glib::timeout_future(step).await;
    }
    true
}

/// Waits (at most [`CATALOG_WAIT`]) for `driver`'s model list to be fresh, or for its fetch to
/// have failed: a failed probe (offline, signed out) must not cost every thread open 3 s.
async fn wait_for_fresh_catalog(driver: Driver) {
    let catalog = ModelCatalog::shared();
    wait_until(CATALOG_WAIT, std::time::Duration::from_millis(100), || {
        catalog.is_settled(driver)
    })
    .await;
    if !catalog.is_fresh(driver) {
        info!(
            "{} model list not fresh after {:?}; starting on the stored model",
            driver_label(driver),
            CATALOG_WAIT
        );
    }
}

/// Everything [`AgentTerminalWindow::build_thread`] needs once the agent is resolved and the
/// thread read.
struct BuildJob {
    thread: String,
    driver: Driver,
    dir: String,
    profile: Profile,
    resolved: ResolvedAgent,
    loaded: LoadedThread,
    /// agy's hook verdict (always `Ok` for the others, which do not use the hook).
    hook: Result<(), String>,
}

/// The saved open-thread ids that still exist (archived or deleted ones drop out), at most `max`.
fn restorable(saved: &serde_json::Value, known: &[String], max: usize) -> Vec<String> {
    saved["open"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .filter(|id| known.contains(id))
                .take(max)
                .collect()
        })
        .unwrap_or_default()
}

/// The thread to bring into view: the saved selection when it is among those reopened, else the
/// first of them.
fn selected_to_restore(saved: &serde_json::Value, open: &[String]) -> Option<String> {
    saved["selected"]
        .as_str()
        .filter(|id| open.iter().any(|o| o == id))
        .or(open.first().map(String::as_str))
        .map(str::to_owned)
}

/// The thread's history for the view, oldest first (read in batches): all of it, or for a very
/// long thread its newest [`REPLAY_LIMIT`] events, cut to start at a turn so no step arrives
/// without its start, behind a notice saying earlier messages are not shown. (Reading the oldest
/// window showed a long thread's beginning and none of its recent turns.)
fn read_history(store: &Store, thread: &str) -> agent_kit::store::Result<Vec<Envelope>> {
    read_history_within(store, thread, REPLAY_LIMIT)
}

fn read_history_within(
    store: &Store,
    thread: &str,
    limit: usize,
) -> agent_kit::store::Result<Vec<Envelope>> {
    let start = store.tail_after(thread, limit)?;
    let mut history = Vec::new();
    let mut after = start;
    loop {
        let batch = store.events(thread, after, 2_000)?;
        let Some((last, _)) = batch.last() else {
            break;
        };
        after = Some(*last);
        history.extend(batch.into_iter().map(|(_, env)| env));
        if history.len() >= limit {
            break;
        }
    }
    if start.is_some() {
        // The first whole turn starts at its prompt, which is stored before the agent's
        // `TurnStarted`: cut at the last user message before that, else at the turn itself. A
        // window holding no turn start (one turn longer than the limit) starts at its first
        // prompt, or as it is when it has none.
        let prompt = |e: &Envelope| {
            matches!(
                e.event,
                Event::ItemStarted {
                    kind: ItemKind::UserMessage,
                    ..
                }
            )
        };
        let cut = match history
            .iter()
            .position(|e| matches!(e.event, Event::TurnStarted { .. }))
        {
            Some(turn) => history[..turn].iter().rposition(prompt).unwrap_or(turn),
            None => history.iter().position(prompt).unwrap_or(0),
        };
        history.drain(..cut);
        history.insert(
            0,
            Envelope::new(Event::Notice {
                text: "This thread is long: only its most recent part is shown here.".to_owned(),
            }),
        );
    }
    Ok(history)
}

/// Tests never touch the real `~/.local/state`.
#[cfg(test)]
fn open_store() -> Result<Rc<Store>, String> {
    in_memory_store()
}

fn in_memory_store() -> Result<Rc<Store>, String> {
    // An in-memory SQLite that cannot open means no SQLite at all.
    Store::open_in_memory()
        .map(Rc::new)
        .map_err(|e| e.to_string())
}

/// The cached resolution of `profile`'s command and env file, if done.
pub(super) fn cached_agent(profile: &Profile) -> Option<ResolvedAgent> {
    let key = (profile.command.clone(), profile.env_file.clone());
    RESOLVED.with(|r| r.borrow().get(&key).cloned())
}

/// Resolves `profile`'s binary (the same probes as detection) and sources its env file, off
/// the main thread. Cached per (command, env file).
pub(super) async fn resolve_agent(profile: Profile) -> ResolvedAgent {
    if let Some(found) = cached_agent(&profile) {
        return found;
    }
    let (path, home, shell) = env_triplet();
    let command = profile.command.clone();
    let env_file = profile.env_file.clone();
    let resolved = gtk4::gio::spawn_blocking(move || {
        let probe = crate::utils::SystemProbe::new(path, home, shell);
        let program = probe.locate(&command);
        let env = env_file
            .as_deref()
            .filter(|f| !f.trim().is_empty())
            .map(crate::utils::load_env_file)
            .unwrap_or_default();
        ResolvedAgent { program, env }
    })
    .await
    .unwrap_or_else(|_| {
        error!("Agent resolution panicked on the worker thread");
        ResolvedAgent::default()
    });
    crate::utils::cache_command_available(&profile.command, resolved.program.is_some());
    RESOLVED.with(|r| {
        r.borrow_mut().insert(
            (profile.command.clone(), profile.env_file.clone()),
            resolved.clone(),
        )
    });
    resolved
}

/// What the app removes from an agent's environment: the configured `clear_env` (a launching
/// agent session's markers) and any inherited approval socket, which only the app may set.
fn unset_list(patterns: &[String]) -> Vec<String> {
    // `vars_os`, lossy: one non-UTF-8 variable in the user's environment must not panic here.
    let mut env: Vec<String> = std::env::vars_os()
        .map(|(k, v)| format!("{}={}", k.to_string_lossy(), v.to_string_lossy()))
        .collect();
    let mut names = crate::utils::strip_env(&mut env, patterns);
    for own in [
        agent_core::approval::ENV_SOCKET,
        crate::approval_server::ENV_HOOK_BIN,
    ] {
        if !names.iter().any(|n| n == own) {
            names.push(own.to_owned());
        }
    }
    names
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Builds the adapter, its approval socket and environment for `driver` from the config, using
/// the cached resolution (a switch cannot wait). An agent that is not Ready (missing, switched
/// off, still being detected) or has no resolved binary is refused with the reason: nothing is
/// ever spawned from a guessed command name.
fn agent_launch(driver: Driver, thread: &str, cwd: &str) -> Result<AgentLaunch, String> {
    agent_launch_with(
        &AgentAvailability::shared().get(driver),
        driver,
        thread,
        cwd,
    )
}

/// [`agent_launch`] against an explicit availability.
fn agent_launch_with(
    state: &Availability,
    driver: Driver,
    thread: &str,
    cwd: &str,
) -> Result<AgentLaunch, String> {
    if !state.is_ready() {
        return Err(not_ready_reason(driver, state));
    }
    let config = SharedConfig::default();
    let (profile, clear) = {
        let c = config.borrow();
        (
            c.agent_profile(driver)
                .cloned()
                .unwrap_or_else(|| crate::config::new_agent_profile(driver)),
            c.clear_env.clone(),
        )
    };
    let resolved = cached_agent(&profile).unwrap_or_default();
    let Some(program) = resolved.program.clone() else {
        return Err(not_ready_reason(driver, &Availability::Missing));
    };
    let adapter = make_adapter(driver, &program);
    let approval = match driver {
        // Claude and Codex raise approvals over their own protocol, not the hook socket.
        Driver::Claude | Driver::Codex => Err(None),
        Driver::Agy => bind_approval(
            cached_hook_verdict(),
            thread,
            cwd,
            profile.default_mode.unwrap_or_default(),
        ),
    };
    Ok(AgentLaunch {
        adapter,
        program,
        extra_args: profile.args.clone(),
        default_model: profile.default_model.clone(),
        default_effort: profile.default_effort.clone(),
        env: LaunchEnv {
            env: resolved.env,
            unset: unset_list(&clear),
        },
        approval,
    })
}

/// Why `driver` cannot be started in `state`, as the thread says it.
fn not_ready_reason(driver: Driver, state: &Availability) -> String {
    let why = match state {
        Availability::Disabled => "is switched off",
        Availability::Detecting => "is still being detected",
        Availability::Missing | Availability::Ready(_) => "is not available",
    };
    format!("{} {why}.", driver_label(driver))
}

/// The adapter for `driver`. Codex reports this app's version in `initialize`'s `clientInfo`.
fn make_adapter(driver: Driver, program: &str) -> std::boxed::Box<dyn Adapter> {
    match driver {
        Driver::Claude => std::boxed::Box::new(agent_core::claude::ClaudeAdapter::new()),
        Driver::Agy => std::boxed::Box::new(agent_core::agy::AgyAdapter::new(program.to_owned())),
        Driver::Codex => std::boxed::Box::new(
            agent_core::codex::CodexAdapter::new().client_version(env!("CARGO_PKG_VERSION")),
        ),
    }
}

/// agy's approval socket, only once its hooks file is proven to install the hook (`hook` is the
/// verdict of [`check_hook`]). Without one agy runs without the skip flag and cannot ask before
/// acting; the session says why: how to install the hook (`Err(None)`), or, when it is installed
/// but the socket failed, the reason (`Err(Some(_))`, such as a binary replaced by an upgrade).
fn bind_approval(hook: Result<(), String>, thread: &str, cwd: &str, mode: Mode) -> Approval {
    let installed = hook.is_ok();
    ApprovalHandle::bind_checked(hook, thread, std::path::Path::new(cwd), mode).map_err(|reason| {
        info!("agy runs without its approval hook in this thread: {reason}");
        installed.then_some(reason)
    })
}

thread_local! {
    /// The last verdict on agy's hooks file; `None` until the first check finishes.
    static HOOK_VERDICT: RefCell<Option<Result<(), String>>> = const { RefCell::new(None) };
}

/// Reads agy's hooks file off the main thread and remembers the verdict. A thread being built
/// awaits this; a switch to agy (which cannot wait) uses [`cached_hook_verdict`].
pub(super) async fn check_hook() -> Result<(), String> {
    let home = env::var("HOME").ok();
    let verdict =
        gtk4::gio::spawn_blocking(move || crate::hook_config::check_installed(home.as_deref()))
            .await
            .unwrap_or_else(|_| Err("the hooks check panicked".to_owned()));
    HOOK_VERDICT.with(|v| *v.borrow_mut() = Some(verdict.clone()));
    verdict
}

/// The last [`check_hook`] verdict. Fail-closed: before any check has finished it is an `Err`, so
/// agy starts without the skip flag rather than guess. (A stale `Ok` is safe too: the session's
/// canary stops agy and restarts it without the flag when a tool runs without a hook query.)
fn cached_hook_verdict() -> Result<(), String> {
    HOOK_VERDICT
        .with(|v| v.borrow().clone())
        .unwrap_or_else(|| Err("the approval hook has not been checked yet".to_owned()))
}

/// Starts a [`check_hook`] without waiting for it (window start, focus, the periodic refresh).
pub(super) fn refresh_hook_verdict() {
    glib::MainContext::default().spawn_local(async {
        // Only the cached verdict matters here.
        let _ = check_hook().await;
    });
}

// ---------------------------------------------------------------------------------------------
// The sidebar
// ---------------------------------------------------------------------------------------------

pub(super) struct Sidebar {
    pub(super) root: gtk4::Box,
    pub(super) list: gtk4::ListBox,
    search: gtk4::SearchEntry,
    /// Row index → what it opens (`None` for a folder header).
    pub(super) keys: RefCell<Vec<Option<RowKey>>>,
    /// Set while the list is rebuilt, so selecting the current row is not taken as a click.
    rebuilding: std::cell::Cell<bool>,
    /// Held for the sidebar's lifetime: the footer's usage indicator.
    _usage: Rc<UsageIndicator>,
}

impl AgentTerminalWindow {
    /// Builds the chat-first shell into `container`: the sidebar beside the (tab-bar-less) tab
    /// view, with an empty state when no page is open.
    pub(super) fn setup_shell(&self, container: &Box, tab_view: &adw::TabView) {
        let obj = self.obj();
        // Threads need a store; if not even an in-memory SQLite opens there is nothing to show.
        if let Err(e) = try_app_store() {
            let dialog = adw::AlertDialog::new(
                Some("Threads Are Unavailable"),
                Some(&format!(
                    "Agent Terminal cannot use SQLite on this system, so it cannot keep \
                     threads.\n\n{e}"
                )),
            );
            dialog.add_response("quit", "Quit");
            dialog.set_default_response(Some("quit"));
            dialog.set_close_response("quit");
            dialog.connect_response(
                None,
                glib::clone!(
                    #[weak]
                    obj,
                    move |_, _| {
                        if let Some(app) = obj.application() {
                            app.quit();
                        }
                    }
                ),
            );
            dialog.present(Some(obj.upcast_ref::<gtk4::Widget>()));
            return;
        }
        let split = adw::OverlaySplitView::new();
        split.set_vexpand(true);
        split.set_min_sidebar_width(240.0);
        split.set_max_sidebar_width(340.0);
        split.set_show_sidebar(!self.config.borrow().sidebar_collapsed);

        self.setup_thread_menu_actions();
        let sidebar = self.build_sidebar();
        split.set_sidebar(Some(&sidebar.root));

        let toast_overlay = adw::ToastOverlay::new();
        let pages = gtk4::Stack::new();
        pages.add_named(tab_view, Some("threads"));
        pages.add_named(&self.build_empty_state(), Some("empty"));
        pages.set_visible_child_name("empty");
        toast_overlay.set_child(Some(&pages));
        toast_overlay.set_vexpand(true);
        split.set_content(Some(&toast_overlay));
        container.append(&split);
        *self.toast_overlay.borrow_mut() = Some(toast_overlay);
        *self.pages_stack.borrow_mut() = Some(pages);

        // Narrow windows overlay the sidebar instead of squeezing the thread.
        let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            720.0,
            adw::LengthUnit::Sp,
        ));
        breakpoint.add_setter(&split, "collapsed", Some(&true.to_value()));
        obj.add_breakpoint(breakpoint);

        if let Some(header) = self.header.borrow().as_ref() {
            let toggle = gtk4::ToggleButton::builder()
                .icon_name("at-sidebar-show-symbolic")
                .tooltip_text("Show or Hide Threads (F9, Ctrl+B)")
                .active(split.shows_sidebar())
                .build();
            split
                .bind_property("show-sidebar", &toggle, "active")
                .bidirectional()
                .build();
            // Only a user's toggle is remembered, not the narrow-width overlay closing itself.
            toggle.connect_clicked(glib::clone!(
                #[weak]
                obj,
                move |t| obj.imp().remember_sidebar(t.is_active())
            ));
            header.pack_start(&toggle);
        }
        *self.split_view.borrow_mut() = Some(split);
        *self.sidebar.borrow_mut() = Some(sidebar);

        self.wire_thread_pages(tab_view);
        self.watch_availability();
        self.watch_account_status();
        self.refresh_sidebar();
        // Relative times age; a minute is their resolution.
        glib::timeout_add_seconds_local(
            60,
            glib::clone!(
                #[weak]
                obj,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    obj.imp().refresh_sidebar();
                    glib::ControlFlow::Continue
                }
            ),
        );
    }

    fn remember_sidebar(&self, shown: bool) {
        if self.config.borrow().sidebar_collapsed == !shown {
            return;
        }
        self.config.borrow_mut().sidebar_collapsed = !shown;
        self.schedule_config_save();
    }

    /// F9 / Ctrl+B.
    pub(super) fn toggle_sidebar(&self) {
        let split = self.split_view.borrow().clone();
        if let Some(split) = split {
            let shown = !split.shows_sidebar();
            split.set_show_sidebar(shown);
            // Collapsed (narrow) mode overlays it; that is not a preference to keep.
            if !split.is_collapsed() {
                self.remember_sidebar(shown);
            }
        }
    }

    fn build_empty_state(&self) -> gtk4::Widget {
        let obj = self.obj();
        let page = adw::StatusPage::builder()
            .title("No thread open")
            .description(
                "Start a new thread, or pick one from the sidebar.\n\
                 Ctrl+Shift+T new thread · Ctrl+Shift+G new thread in a worktree · F9 threads",
            )
            .vexpand(true)
            .build();
        page.set_paintable(Some(&crate::icons::hero_paintable(
            crate::icons::APP_ART,
            128,
            &page,
        )));
        let new = Button::builder()
            .label("New Thread")
            .halign(Align::Center)
            .css_classes(["suggested-action", "pill"])
            .build();
        new.connect_clicked(glib::clone!(
            #[weak]
            obj,
            move |_| obj.imp().new_tab()
        ));
        page.set_child(Some(&new));
        page.upcast()
    }

    fn build_sidebar(&self) -> Rc<Sidebar> {
        let obj = self.obj();
        let root = Box::builder()
            .orientation(Orientation::Vertical)
            .css_classes(["thread-sidebar"])
            .build();
        let top = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(6)
            .margin_top(8)
            .margin_bottom(6)
            .margin_start(8)
            .margin_end(8)
            .build();
        let search = gtk4::SearchEntry::builder()
            .placeholder_text("Search threads")
            .hexpand(true)
            .build();
        let new = Button::builder()
            .icon_name("at-list-add-symbolic")
            .tooltip_text("New Thread (Ctrl+Shift+T)")
            .css_classes(["flat"])
            .build();
        new.connect_clicked(glib::clone!(
            #[weak]
            obj,
            move |_| obj.imp().new_tab()
        ));
        top.append(&search);
        top.append(&new);
        root.append(&top);

        let list = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::Single)
            .css_classes(["navigation-sidebar", "thread-list"])
            .build();
        let scrolled = ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vexpand(true)
            .child(&list)
            .build();
        root.append(&scrolled);

        let footer = Box::builder()
            .orientation(Orientation::Vertical)
            .css_classes(["sidebar-footer"])
            .build();
        let archived = gtk4::ToggleButton::builder()
            .label("Show archived")
            .tooltip_text("Also list archived threads")
            .css_classes(["flat", "caption"])
            .halign(Align::Start)
            .build();
        archived.connect_toggled(glib::clone!(
            #[weak]
            obj,
            move |button| {
                let imp = obj.imp();
                imp.show_archived.set(button.is_active());
                imp.refresh_sidebar();
            }
        ));
        footer.append(&archived);
        // Every agent's usage and account, updated on every turn.
        let usage = UsageIndicator::new(AccountStatus::shared(), None);
        footer.append(usage.widget());
        root.append(&footer);

        let sidebar = Rc::new(Sidebar {
            root,
            list: list.clone(),
            search: search.clone(),
            keys: RefCell::new(Vec::new()),
            rebuilding: std::cell::Cell::new(false),
            _usage: usage,
        });
        search.connect_search_changed(glib::clone!(
            #[weak]
            obj,
            move |_| obj.imp().refresh_sidebar()
        ));
        let weak_sidebar = Rc::downgrade(&sidebar);
        list.connect_row_activated(glib::clone!(
            #[weak]
            obj,
            move |_, row| {
                let Some(sidebar) = weak_sidebar.upgrade() else {
                    return;
                };
                if sidebar.rebuilding.get() {
                    return;
                }
                let key = usize::try_from(row.index())
                    .ok()
                    .and_then(|i| sidebar.keys.borrow().get(i).cloned().flatten());
                if let Some(key) = key {
                    obj.imp().open_row(&key);
                }
            }
        ));
        self.install_thread_menu_triggers(&sidebar);
        sidebar
    }

    /// Opens (or shows) what a sidebar row stands for.
    fn open_row(&self, key: &RowKey) {
        match key {
            RowKey::Thread(id) => {
                self.open_thread(id, true);
            }
            RowKey::Terminal(key) => {
                self.show_tab(*key);
            }
        }
        // In the narrow overlay mode, picking a thread is done with the sidebar.
        if let Some(split) = self.split_view.borrow().as_ref() {
            if split.is_collapsed() {
                split.set_show_sidebar(false);
            }
        }
    }

    /// The rows the sidebar shows: every stored thread, with the live state of the open ones,
    /// and the open terminal pages.
    pub(super) fn sidebar_rows(&self) -> Vec<SidebarRow> {
        // From the last load: the sidebar never reads the store on the main thread.
        let summaries = self.summaries.borrow().clone().unwrap_or_default();
        let tabs = self.tabs.borrow();
        let mut rows: Vec<SidebarRow> = summaries
            .into_iter()
            .map(|s| {
                let tab = tabs
                    .iter()
                    .find(|t| t.chat.as_ref().is_some_and(|c| c.thread == s.id));
                let chat = tab.and_then(|t| t.chat.as_ref());
                let badge = match chat {
                    Some(c) => badge_for(c.running, c.approval, c.rate_limited, c.unread),
                    // A closed thread with events nobody has seen.
                    None => badge_for(false, false, false, s.unread && s.updated_at > 0),
                };
                let title = match chat {
                    Some(c) if !c.title.is_empty() => c.title.clone(),
                    _ if !s.title.is_empty() => s.title.clone(),
                    _ => "New thread".to_owned(),
                };
                SidebarRow {
                    key: RowKey::Thread(s.id.clone()),
                    title,
                    folder: s.cwd.clone(),
                    updated_ms: s.updated_at,
                    driver: chat
                        .map(|c| c.driver)
                        .or_else(|| s.driver.as_deref().and_then(parse_driver)),
                    badge,
                    open: tab.is_some(),
                    archived: s.archived,
                }
            })
            .collect();
        for tab in tabs.iter().filter(|t| t.chat.is_none()) {
            rows.push(SidebarRow {
                key: RowKey::Terminal(tab.key),
                title: tab.page.title().to_string(),
                folder: tab.dir.clone(),
                updated_ms: tab
                    .started_at
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0),
                driver: None,
                badge: tab.page.needs_attention().then_some(Badge::Unread),
                open: true,
                archived: false,
            });
        }
        rows
    }

    /// The thread list changed in the store (a thread made, renamed, archived, deleted, or its
    /// events moved it): load it again, off the main thread, and redraw when it lands. Bursts
    /// coalesce into one more load. With no file store (memory, tests) it loads in place.
    pub(super) fn reload_summaries(&self) {
        if !store_is_async() {
            let list = app_store().list_threads(true).unwrap_or_else(|e| {
                warn!("Cannot list threads: {e}");
                Vec::new()
            });
            *self.summaries.borrow_mut() = Some(list);
            self.refresh_sidebar();
            return;
        }
        if self.summaries_loading.replace(true) {
            self.summaries_stale.set(true);
            return;
        }
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            loop {
                let loaded = store_job(|store| store.list_threads(true)).await;
                let Some(obj) = obj.upgrade() else { return };
                let imp = obj.imp();
                match loaded {
                    Some(Ok(list)) => *imp.summaries.borrow_mut() = Some(list),
                    Some(Err(e)) => warn!("Cannot list threads: {e}"),
                    None => {}
                }
                if imp.summaries_stale.replace(false) {
                    continue;
                }
                imp.summaries_loading.set(false);
                imp.refresh_sidebar();
                return;
            }
        });
    }

    /// Runs a store write off the main thread, logs a failure, then reloads the thread list.
    pub(super) fn write_store(
        &self,
        what: &'static str,
        job: impl FnOnce(&Store) -> agent_kit::store::Result<()> + Send + 'static,
    ) {
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            if let Some(Err(e)) = store_job(job).await {
                warn!("Cannot {what}: {e}");
            }
            if let Some(obj) = obj.upgrade() {
                obj.imp().reload_summaries();
            }
        });
    }

    /// A stored thread's summary: from the loaded list, else (a thread made a moment ago) a
    /// one-row read.
    pub(super) fn summary_of(&self, thread: &str) -> Option<agent_kit::store::ThreadSummary> {
        let cached = self
            .summaries
            .borrow()
            .as_ref()
            .and_then(|l| l.iter().find(|s| s.id == thread).cloned());
        cached.or_else(|| app_store().thread_summary(thread).ok().flatten())
    }

    /// Redraws the sidebar from what is loaded, with the live state of the open pages. Cheap: a
    /// few dozen rows, rebuilt on state changes. Nothing loaded yet: asks for the load.
    pub(super) fn refresh_sidebar(&self) {
        let Some(sidebar) = self.sidebar.borrow().clone() else {
            return;
        };
        if self.summaries.borrow().is_none() {
            // The load redraws when it lands (at once when there is no file store).
            self.reload_summaries();
            return;
        }
        let rows = self.sidebar_rows();
        let selected = self.selected_row_key();
        let focused = self.obj().is_active();
        let groups = group_rows(rows, &sidebar.search.text(), self.show_archived.get());
        let home = env::var("HOME").unwrap_or_default();
        let now = now_ms();

        sidebar.rebuilding.set(true);
        while let Some(child) = sidebar.list.first_child() {
            sidebar.list.remove(&child);
        }
        let mut keys = Vec::new();
        let mut select = None;
        for (folder, rows) in groups {
            let header = gtk4::ListBoxRow::builder()
                .activatable(false)
                .selectable(false)
                .css_classes(["folder-row"])
                .build();
            let label = Label::builder()
                .label(crate::utils::tildify(&folder, &home))
                .xalign(0.0)
                .ellipsize(gtk4::pango::EllipsizeMode::Start)
                .css_classes(["folder-label", "caption-heading"])
                .tooltip_text(&folder)
                .build();
            header.set_child(Some(&label));
            sidebar.list.append(&header);
            keys.push(None);
            for row in rows {
                let widget = self.sidebar_row_widget(&row, now);
                if needs_attention(&row, selected.as_ref(), focused) {
                    widget.add_css_class("needs-attention");
                }
                sidebar.list.append(&widget);
                if Some(&row.key) == selected.as_ref() {
                    select = Some(widget.clone());
                }
                keys.push(Some(row.key));
            }
        }
        *sidebar.keys.borrow_mut() = keys;
        sidebar.list.select_row(select.as_ref());
        sidebar.rebuilding.set(false);
    }

    fn sidebar_row_widget(&self, row: &SidebarRow, now: i64) -> gtk4::ListBoxRow {
        let obj = self.obj();
        let line = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(8)
            .css_classes(["thread-row"])
            .build();
        match row.driver {
            Some(driver) => {
                let dot = Image::from_gicon(&crate::icons::driver_icon(driver));
                dot.set_pixel_size(14);
                dot.add_css_class("thread-dot");
                dot.add_css_class(&format!("dot-{}", driver_key(driver)));
                dot.set_tooltip_text(Some(driver_label(driver)));
                line.append(&dot);
            }
            None => {
                let icon = Image::from_icon_name("at-utilities-terminal-symbolic");
                icon.set_tooltip_text(Some("Terminal thread"));
                icon.add_css_class("thread-term");
                line.append(&icon);
            }
        }
        let title = Label::builder()
            .label(&row.title)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(if row.open {
                vec!["thread-title", "open"]
            } else {
                vec!["thread-title"]
            })
            .tooltip_text(&row.title)
            .build();
        line.append(&title);
        if let Some(badge) = row.badge {
            if badge == Badge::Running {
                let spinner = gtk4::Spinner::builder().spinning(true).build();
                spinner.set_tooltip_text(Some(badge.tooltip()));
                line.append(&spinner);
            } else {
                let mark = Label::builder()
                    .label("●")
                    .css_classes(["thread-badge", badge.css_class()])
                    .tooltip_text(badge.tooltip())
                    .build();
                line.append(&mark);
            }
        }
        let age = Label::builder()
            .label(relative_time(now, row.updated_ms))
            .css_classes(["thread-age", "dim-label", "caption"])
            .build();
        line.append(&age);
        if let RowKey::Thread(thread_id) = &row.key {
            let delete = Button::builder()
                .icon_name("at-user-trash-symbolic")
                .tooltip_text("Delete thread…")
                .css_classes(["flat", "circular", "thread-delete"])
                .valign(Align::Center)
                .build();
            let thread = thread_id.clone();
            delete.connect_clicked(glib::clone!(
                #[weak]
                obj,
                move |_| obj.imp().confirm_delete(&thread)
            ));
            line.append(&delete);
        }
        if row.open {
            let close = Button::builder()
                .icon_name("at-window-close-symbolic")
                .tooltip_text("Close (the thread stays in the list)")
                .css_classes(["flat", "circular", "thread-close"])
                .valign(Align::Center)
                .build();
            let key = row.key.clone();
            close.connect_clicked(glib::clone!(
                #[weak]
                obj,
                move |_| obj.imp().close_row(&key)
            ));
            line.append(&close);
        }
        gtk4::ListBoxRow::builder().child(&line).build()
    }

    pub(super) fn close_row(&self, key: &RowKey) {
        let page = self.tabs.borrow().iter().find_map(|t| {
            let matches = match key {
                RowKey::Thread(id) => t.chat.as_ref().is_some_and(|c| &c.thread == id),
                RowKey::Terminal(k) => t.chat.is_none() && t.key == *k,
            };
            matches.then(|| t.page.clone())
        });
        let view = self.tab_view.borrow().clone();
        if let (Some(page), Some(view)) = (page, view) {
            view.close_page(&page);
        }
    }

    pub(super) fn selected_row_key(&self) -> Option<RowKey> {
        let page = self.tab_view.borrow().as_ref()?.selected_page()?;
        let tabs = self.tabs.borrow();
        let tab = tabs.iter().find(|t| t.page == page)?;
        Some(match &tab.chat {
            Some(c) => RowKey::Thread(c.thread.clone()),
            None => RowKey::Terminal(tab.key),
        })
    }

    // -----------------------------------------------------------------------------------------
    // Page lifecycle
    // -----------------------------------------------------------------------------------------

    /// Selection, removal and the empty state, for every page kind.
    fn wire_thread_pages(&self, tab_view: &adw::TabView) {
        let obj = self.obj();
        tab_view.connect_selected_page_notify(glib::clone!(
            #[weak]
            obj,
            move |view| {
                let imp = obj.imp();
                if let Some(page) = view.selected_page() {
                    imp.page_shown(&page);
                }
                imp.save_open_threads();
                imp.refresh_sidebar();
            }
        ));
        tab_view.connect_notify_local(
            Some("n-pages"),
            glib::clone!(
                #[weak]
                obj,
                move |view, _| {
                    let imp = obj.imp();
                    if let Some(stack) = imp.pages_stack.borrow().as_ref() {
                        stack.set_visible_child_name(if view.n_pages() == 0 {
                            "empty"
                        } else {
                            "threads"
                        });
                    }
                    if view.n_pages() == 0 {
                        if let Some(title) = imp.window_title.borrow().as_ref() {
                            title.set_title("Agent Terminal");
                            title.set_subtitle("");
                        }
                        obj.set_title(Some("Agent Terminal"));
                    }
                }
            ),
        );
        // After the registry dropped the page (the 2.x handler runs first).
        tab_view.connect_page_detached(glib::clone!(
            #[weak]
            obj,
            move |view, _, _| {
                if view.is_transferring_page() {
                    return;
                }
                let imp = obj.imp();
                imp.save_open_threads();
                imp.refresh_sidebar();
            }
        ));
    }

    /// A page came into view: build a thread on first show, clear its unread state, and put
    /// its title and folder in the header.
    pub(super) fn page_shown(&self, page: &adw::TabPage) {
        let info = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(tab) = tabs.iter_mut().find(|t| &t.page == page) else {
                return;
            };
            let dir = tab.dir.clone();
            tab.chat.as_mut().map(|chat| {
                chat.unread = false;
                (chat.thread.clone(), chat.title.clone(), dir)
            })
        };
        let Some((thread, title, dir)) = info else {
            // A terminal page: the 2.x handler puts its terminal title in the subtitle.
            if let Some(window_title) = self.window_title.borrow().as_ref() {
                window_title.set_title("Terminal");
            }
            return;
        };
        let id = thread.clone();
        self.write_store("mark the thread read", move |s| s.mark_read(&id));
        let home = env::var("HOME").unwrap_or_default();
        if let Some(window_title) = self.window_title.borrow().as_ref() {
            window_title.set_title(if title.is_empty() {
                "New thread"
            } else {
                &title
            });
            window_title.set_subtitle(&crate::utils::tildify(&dir, &home));
        }
        self.ensure_built(page);
    }

    /// Records the open threads (and the one in view) so the next launch reopens them.
    pub(super) fn save_open_threads(&self) {
        // Nothing to record before the shell exists, or while it is being torn down.
        if self.tab_view.borrow().is_none() || !CHAT_RESTORED.with(std::cell::Cell::get) {
            return;
        }
        let selected = self.selected_row_key();
        let open: Vec<String> = self
            .tabs
            .borrow()
            .iter()
            .filter_map(|t| t.chat.as_ref().map(|c| c.thread.clone()))
            .collect();
        let selected = match selected {
            Some(RowKey::Thread(id)) => Some(id),
            _ => None,
        };
        let value = serde_json::json!({ "open": open, "selected": selected });
        self.queue_open_threads(value.to_string());
    }

    /// Writes the open-thread list off the main thread. Writes are one at a time and in order,
    /// and a burst (several pages opening) keeps only the newest value, so a slow disk can
    /// neither reorder them nor pile them up.
    fn queue_open_threads(&self, value: String) {
        *self.open_threads_pending.borrow_mut() = Some(value);
        if self.open_threads_writing.replace(true) {
            return; // the running writer picks the new value up
        }
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            loop {
                let Some(obj) = obj.upgrade() else { return };
                let next = obj.imp().open_threads_pending.borrow_mut().take();
                let Some(value) = next else {
                    obj.imp().open_threads_writing.set(false);
                    return;
                };
                drop(obj);
                if let Some(Err(e)) = store_job(move |s| s.set_meta(OPEN_THREADS_KEY, &value)).await
                {
                    warn!("Cannot record the open threads: {e}");
                }
            }
        });
    }

    /// Reopens the threads open when the app last closed (first window of a launch only).
    /// Only the one in view is built now; the rest are built when first shown.
    ///
    /// The stored list is read off the main thread, so the reopening lands a moment later. The
    /// saved selection is then applied only when nothing else has been put in view meanwhile (a
    /// resume the window was opened for).
    pub(super) fn restore_open_threads(&self) {
        if CHAT_RESTORED.with(|done| done.replace(true)) || try_app_store().is_err() {
            return;
        }
        self.list_recent_sessions();
        let obj = self.obj().downgrade();
        let before = self.tabs.borrow().len();
        glib::MainContext::default().spawn_local(async move {
            let loaded = store_job(|store| {
                (
                    store.meta(OPEN_THREADS_KEY).ok().flatten(),
                    store.list_threads(false).unwrap_or_default(),
                )
            })
            .await;
            let (Some((saved, threads)), Some(obj)) = (loaded, obj.upgrade()) else {
                return;
            };
            let Some(value) =
                saved.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            else {
                return;
            };
            let known: Vec<String> = threads.into_iter().map(|s| s.id).collect();
            let open = restorable(&value, &known, crate::config::SessionState::MAX_TABS);
            if open.is_empty() {
                return;
            }
            let imp = obj.imp();
            // Pages added since the restore began belong to something else (a resume): leave
            // that in view.
            let others_in_view = imp.tabs.borrow().len() > before;
            info!("Reopening {} thread(s) from the last session", open.len());
            for id in &open {
                imp.open_thread(id, false);
            }
            if others_in_view {
                return;
            }
            if let Some(id) = selected_to_restore(&value, &open) {
                imp.open_thread(&id, true);
            }
        });
    }

    /// Lists the agents' own recent sessions (Claude Code and agy, newest [`RECENT_SESSIONS`])
    /// as threads in the sidebar, once per run. The scan reads only file metadata and the tail
    /// of each transcript, off the main thread; a session's history is imported when opened.
    fn list_recent_sessions(&self) {
        // Tests never read the real home's agent histories (`link_recent_sessions` is tested on
        // its own).
        // Nor does the memory-only fallback store: its jobs run in place on the main thread,
        // and what it would list is gone at exit anyway.
        if cfg!(test) || !store_is_async() {
            return;
        }
        let Some(home) = env::var_os("HOME").map(std::path::PathBuf::from) else {
            return;
        };
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            let added = store_job(move |store| {
                let sessions = agent_kit::native::recent_native_sessions(&home, RECENT_SESSIONS);
                link_recent_sessions(store, &sessions, &home.to_string_lossy())
            })
            .await
            .unwrap_or(0);
            if added > 0 {
                info!("Listed {added} recent agent session(s) as threads");
                if let Some(obj) = obj.upgrade() {
                    obj.imp().refresh_sidebar();
                }
            }
        });
    }

    /// Shows the thread's page, adding it if it is not open. `select` brings it into view (and
    /// so builds it).
    pub(super) fn open_thread(&self, thread: &str, select: bool) -> Option<adw::TabPage> {
        let existing = self.tabs.borrow().iter().find_map(|t| {
            t.chat
                .as_ref()
                .is_some_and(|c| c.thread == thread)
                .then(|| t.page.clone())
        });
        let page = match existing {
            Some(page) => page,
            None => self.add_thread_page(thread)?,
        };
        if select {
            let view = self.tab_view.borrow().clone();
            if let Some(view) = view {
                if view.selected_page().as_ref() == Some(&page) {
                    self.page_shown(&page);
                } else {
                    view.set_selected_page(&page);
                }
            }
        }
        Some(page)
    }

    /// Adds an (unbuilt) page for a stored thread.
    fn add_thread_page(&self, thread: &str) -> Option<adw::TabPage> {
        let summary = self.summary_of(thread)?;
        let driver = summary
            .driver
            .as_deref()
            .and_then(parse_driver)
            .or_else(|| self.default_agent())
            .unwrap_or(Driver::Claude);
        let tab_view = self.tab_view.borrow().clone()?;
        let config = self.config.borrow().clone();
        let mode = config
            .agent_profile(driver)
            .and_then(|p| p.default_mode)
            .unwrap_or_default();

        let terminal = Terminal::new();
        let stack = Stack::builder().vexpand(true).build();
        let scrolled = ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .child(&terminal)
            .css_classes(["terminal-container"])
            .build();
        stack.add_named(&build_loading_box(), Some("loading"));
        stack.add_named(&scrolled, Some("terminal"));
        stack.set_visible_child_name("loading");
        let (search_bar, search_entry) = build_search_bar(&terminal);
        let (exit_bar, exit_label, _, _) = build_exit_bar();
        let drawer = Box::builder()
            .orientation(Orientation::Vertical)
            .visible(false)
            .css_classes(["terminal-drawer"])
            .build();
        drawer.append(&search_bar);
        drawer.append(&stack);

        let holder = Box::builder()
            .orientation(Orientation::Vertical)
            .vexpand(true)
            .build();
        let drawer_paned = gtk4::Paned::builder()
            .orientation(Orientation::Vertical)
            .start_child(&holder)
            .end_child(&drawer)
            .resize_start_child(true)
            .shrink_end_child(false)
            .vexpand(true)
            .build();

        let diff_panel = DiffPanel::new(&Theme::diff_colours(config.theme));
        diff_panel.set_shown(config.diff_panel_visible);
        let paned = gtk4::Paned::builder()
            .orientation(Orientation::Horizontal)
            .start_child(&drawer_paned)
            .end_child(&diff_panel.root)
            .resize_start_child(true)
            .resize_end_child(false)
            .shrink_end_child(false)
            .vexpand(true)
            .build();
        self.wire_diff_panel(&diff_panel, &paned);

        let rate_banner = adw::Banner::builder().use_markup(false).build();
        let thread_id = thread.to_owned();
        rate_banner.connect_button_clicked(glib::clone!(
            #[weak(rename_to = banner)]
            rate_banner,
            move |b| {
                if let Some(obj) = window_of(b) {
                    obj.imp().continue_rate_limited(&thread_id);
                }
                banner.set_revealed(false);
            }
        ));
        // Another banner for a model its agent no longer offers: "Choose…" opens the picker.
        let model_banner = adw::Banner::builder()
            .use_markup(false)
            .button_label("Choose…")
            .build();
        let thread_id = thread.to_owned();
        model_banner.connect_button_clicked(move |b| {
            if let Some(obj) = window_of(b) {
                obj.imp().choose_model(&thread_id);
            }
        });
        let content = Box::builder().orientation(Orientation::Vertical).build();
        content.append(&rate_banner);
        content.append(&model_banner);
        content.append(&paned);

        let page = tab_view.append(&content);
        let title = if summary.title.is_empty() {
            "New thread".to_owned()
        } else {
            summary.title.clone()
        };
        page.set_title(&title);

        self.configure_terminal(&terminal);
        self.wire_input_controllers(&terminal);
        // The drawer's shell exiting just closes the drawer; it never takes the thread.
        terminal.connect_child_exited(glib::clone!(
            #[weak]
            page,
            move |terminal, _| {
                if let Some(obj) = window_of(terminal) {
                    obj.imp().drawer_exited(&page);
                }
            }
        ));

        let slot = Rc::new(SessionSlot {
            session: RefCell::new(None),
            view: RefCell::new(None),
            driver,
            model: None,
            mode,
        });
        let key = next_tab_key();
        let diffs = {
            let holder = holder.downgrade();
            ThreadDiffs::new(
                &summary.cwd,
                key,
                driver_label(driver),
                Rc::new(move |text: &str| {
                    if let Some(window) = holder.upgrade().as_ref().and_then(window_of) {
                        window.imp().show_toast(text);
                    }
                }),
            )
        };
        self.tabs.borrow_mut().push(TabState {
            page: page.clone(),
            terminal,
            dir: summary.cwd.clone(),
            stack,
            exit_bar,
            exit_label,
            spawned_at: std::time::Instant::now(),
            search_bar,
            search_entry,
            profile: None,
            session_id: None,
            pinned_id: None,
            started_at: std::time::SystemTime::now(),
            quota_banner: adw::Banner::new(""),
            quota: QuotaState::Unknown,
            screen_dirty: std::rc::Rc::new(std::cell::Cell::new(false)),
            key,
            quota_notified: false,
            bell_pending: false,
            last_output: std::rc::Rc::new(std::cell::Cell::new(std::time::Instant::now())),
            checkpoint: CheckpointTrack::default(),
            diff_panel: diff_panel.clone(),
            worktree: None,
            chat: Some(ChatTab {
                thread: thread.to_owned(),
                driver,
                title: summary.title.clone(),
                holder,
                slot,
                view: None,
                building: false,
                drawer,
                drawer_paned,
                drawer_started: false,
                rate_banner,
                model_banner,
                running: false,
                approval: false,
                rate_limited: false,
                unread: false,
                title_item: None,
                pending_prompt: None,
                pending_handoff: None,
                pending_effort: None,
                pending_switch: None,
                unavailable: false,
                starting: false,
                diffs,
            }),
        });
        if diff_panel.root.is_visible() {
            self.refresh_diff(&page);
        }
        self.refresh_sidebar();
        Some(page)
    }

    /// Builds a thread's view and starts its session, the first time its page is shown.
    fn ensure_built(&self, page: &adw::TabPage) {
        let job = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(tab) = tabs.iter_mut().find(|t| &t.page == page) else {
                return;
            };
            let dir = tab.dir.clone();
            let Some(chat) = tab.chat.as_mut() else {
                return;
            };
            if chat.view.is_some() || chat.building {
                if let Some(view) = &chat.view {
                    let view = view.clone();
                    // Selecting a thread shows its newest messages, wherever it was left.
                    glib::idle_add_local_once(move || {
                        view.scroll_to_end();
                        view.focus_composer();
                    });
                }
                return;
            }
            chat.building = true;
            (chat.thread.clone(), chat.driver, dir)
        };
        let (thread, driver, dir) = job;
        let profile = self
            .config
            .borrow()
            .agent_profile(driver)
            .cloned()
            .unwrap_or_else(|| crate::config::new_agent_profile(driver));
        let obj = self.obj();
        let page = page.clone();
        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            obj,
            async move {
                let resolved = resolve_agent(profile.clone()).await;
                // A long thread's history is read (and decoded) off the main thread; only the
                // replay into the view happens here.
                let loaded = store_job({
                    let thread = thread.clone();
                    // Importing reads the agent's transcript; only where store jobs really run
                    // off the main thread (never with the memory-only fallback, or under test).
                    let home = (store_is_async() && !cfg!(test))
                        .then(|| env::var_os("HOME").map(std::path::PathBuf::from))
                        .flatten();
                    move |store| load_thread(store, &thread, home.as_deref())
                })
                .await;
                // A thread that cannot be read is not built (an empty one would start a fresh
                // session and re-point the thread at it); selecting it again retries.
                let loaded = match loaded {
                    Some(Ok(loaded)) => loaded,
                    failed => {
                        if let Some(Err(e)) = failed {
                            warn!("Cannot read thread {thread}: {e}");
                        }
                        obj.imp().thread_load_failed(&page);
                        return;
                    }
                };
                // agy's hooks file is read off the main thread too; the verdict gates the socket.
                let hook = match driver {
                    Driver::Agy => check_hook().await,
                    Driver::Claude | Driver::Codex => Ok(()),
                };
                // A session that will start now waits (briefly) for a fresh model list, so a
                // retired model is ported before it reaches argv.
                if AgentAvailability::shared().is_ready(driver) {
                    wait_for_fresh_catalog(driver).await;
                }
                obj.imp().build_thread(
                    &page,
                    BuildJob {
                        thread,
                        driver,
                        dir,
                        profile,
                        resolved,
                        loaded,
                        hook,
                    },
                );
            }
        ));
    }

    /// A thread's page whose store read failed: left unbuilt, so showing it again retries.
    fn thread_load_failed(&self, page: &adw::TabPage) {
        if let Some(chat) = self
            .tabs
            .borrow_mut()
            .iter_mut()
            .find(|t| &t.page == page)
            .and_then(|t| t.chat.as_mut())
        {
            chat.building = false;
        }
        self.show_toast("Could not read this thread from the store. Select it again to retry.");
    }

    fn build_thread(&self, page: &adw::TabPage, job: BuildJob) {
        let BuildJob {
            thread,
            driver,
            dir,
            profile,
            resolved,
            loaded,
            hook,
        } = job;
        let Some((slot, holder, diffs)) = self.tabs.borrow().iter().find_map(|t| {
            (&t.page == page)
                .then(|| {
                    t.chat
                        .as_ref()
                        .map(|c| (c.slot.clone(), c.holder.clone(), c.diffs.clone()))
                })
                .flatten()
        }) else {
            return; // closed while resolving
        };
        let history = loaded.history;

        // The view first, so no envelope of the session's start is lost. It is built from the
        // store whatever the agent's state: history is readable without the agent.
        let backend: Rc<dyn ChatBackend> = slot.clone();
        let view = ChatView::new(backend);
        view.set_vexpand(true);
        view.set_model_source(ModelCatalog::shared());
        view.set_account_status(AccountStatus::shared());
        view.set_diff_source(diffs);
        view.replay(&history);
        holder.append(&view);
        *slot.view.borrow_mut() = Some(view.downgrade());

        let weak_obj = self.obj().downgrade();
        let thread_id = thread.clone();
        view.connect_action(move |action| {
            if let Some(obj) = weak_obj.upgrade() {
                obj.imp().view_action(&thread_id, action);
            }
        });
        if let Some(chat) = self
            .tabs
            .borrow_mut()
            .iter_mut()
            .find(|t| &t.page == page)
            .and_then(|t| t.chat.as_mut())
        {
            chat.view = Some(view.clone());
            chat.building = false;
        }

        // No session unless the agent is Ready (found and switched on): a missing or disabled
        // agent is never spawned from its command name. The banner says so and offers to continue
        // elsewhere; the session starts by itself if the agent becomes Ready while this thread
        // is open (`start_pending_sessions`).
        let ready = AgentAvailability::shared().is_ready(driver) && resolved.program.is_some();
        if ready {
            self.start_session(
                page,
                StartJob {
                    thread,
                    driver,
                    dir,
                    profile,
                    resolved,
                    provider: loaded.provider,
                    hook,
                },
            );
        } else {
            info!(
                "Thread {thread} opened without a session: {} is not ready",
                driver_label(driver)
            );
            self.update_unavailable_banners();
            if self
                .tab_view
                .borrow()
                .as_ref()
                .and_then(|v| v.selected_page())
                .as_ref()
                == Some(page)
            {
                view.focus_composer();
            }
        }
    }

    /// Starts the session of a thread whose view is built, and applies what was waiting for it.
    fn start_session(&self, page: &adw::TabPage, job: StartJob) {
        let StartJob {
            thread,
            driver,
            dir,
            profile,
            resolved,
            provider,
            hook,
        } = job;
        let (thread, dir, profile) = (thread.as_str(), dir.as_str(), &profile);
        let Some((slot, view)) = self.tabs.borrow_mut().iter_mut().find_map(|t| {
            (&t.page == page).then(|| {
                t.chat.as_mut().and_then(|c| {
                    c.starting = false;
                    Some((c.slot.clone(), c.view.clone()?))
                })
            })?
        }) else {
            return; // closed while resolving
        };
        if slot.get().is_some() {
            return; // already running
        }
        let store = app_store();

        let thread_id = thread.to_owned();
        let view_sink = view.sink();
        let weak_slot = Rc::downgrade(&slot);
        let weak_obj = self.obj().downgrade();
        let sink: EnvelopeSink = Rc::new(move |env: &Envelope| {
            view_sink(env);
            let driver = weak_slot.upgrade().map_or(driver, |s| s.driver());
            if let Some(obj) = weak_obj.upgrade() {
                obj.imp().on_thread_envelope(&thread_id, driver, env);
            }
        });

        // Resume the active provider thread's native session, on its model.
        let resume = provider.as_ref().and_then(|p| p.native_id.clone());
        // An effort chosen for this thread (continued in another agent) beats the profile's.
        let effort = self
            .tabs
            .borrow_mut()
            .iter_mut()
            .find(|t| &t.page == page)
            .and_then(|t| t.chat.as_mut())
            .and_then(|c| c.pending_effort.take())
            .or_else(|| profile.default_effort.clone());
        // A model its agent no longer offers (judged on a list fetched in this run) is never
        // started: the thread resumes on the replacement, and says so.
        let (model, effort, ported) = live_model(
            driver,
            provider
                .as_ref()
                .and_then(|p| stored_model(Some(&p.model)))
                .or_else(|| profile.default_model.clone()),
            effort,
        );
        // The thread's own last mode (what its picker shows), else the profile's: starting in
        // the profile's left the picker showing one mode while the agent ran in another.
        let mode = view
            .replayed_mode()
            .or(profile.default_mode)
            .unwrap_or_default();
        let program = resolved
            .program
            .clone()
            .unwrap_or_else(|| profile.command.clone());
        let open = OpenSession {
            program: program.clone(),
            extra_args: profile.args.clone(),
            cwd: dir.to_owned(),
            model,
            effort,
            mode,
            new_session_id: (resume.is_none() && driver == Driver::Claude)
                .then(|| glib::uuid_string_random().to_string()),
            resume,
            approval_hook: false,
        };
        let adapter = make_adapter(driver, &program);
        let approval = match driver {
            Driver::Claude | Driver::Codex => Err(None),
            Driver::Agy => bind_approval(hook, thread, dir, mode),
        };
        let clear = self.config.borrow().clear_env.clone();
        info!(
            "Opening thread {thread} on {} in {dir}",
            driver_label(driver)
        );
        let session = ChatSession::with_env(
            adapter,
            open,
            store,
            thread.to_owned(),
            sink,
            approval,
            LaunchEnv {
                env: resolved.env,
                unset: unset_list(&clear),
            },
        );
        let (thread_for, dir_for) = (thread.to_owned(), dir.to_owned());
        session.set_adapter_factory(Rc::new(move |d| agent_launch(d, &thread_for, &dir_for)));
        *slot.session.borrow_mut() = Some(session.clone());
        view.refresh_status();
        if let Some(moved) = &ported {
            session.note(retired_notice(moved));
            if let Some(pt) = provider.as_ref().map(|p| p.id.clone()) {
                let new_model = moved.replacement.model.clone();
                self.write_store("record the replacement model", move |s| {
                    s.set_provider_model(&pt, &new_model)
                });
            }
        }

        let (prompt, handoff, switch) = {
            let mut tabs = self.tabs.borrow_mut();
            let chat = tabs
                .iter_mut()
                .find(|t| &t.page == page)
                .and_then(|t| t.chat.as_mut());
            match chat {
                Some(chat) => (
                    chat.pending_prompt.take(),
                    chat.pending_handoff.take(),
                    chat.pending_switch.take(),
                ),
                None => (None, None, None),
            }
        };
        if let Some((summary, carried, source)) = handoff {
            session.seed_handoff(summary, carried, &source);
        }
        if let Some((driver, model, effort)) = switch {
            session.switch(driver, model, effort);
        }
        if let Some(prompt) = prompt {
            session.send_prompt(&prompt);
        }
        // The thread is built: judge its model against the current catalogue too.
        self.evaluate_models();
        if self
            .tab_view
            .borrow()
            .as_ref()
            .and_then(|v| v.selected_page())
            .as_ref()
            == Some(page)
        {
            view.focus_composer();
        }
    }

    // -----------------------------------------------------------------------------------------
    // Live thread events
    // -----------------------------------------------------------------------------------------

    fn on_thread_envelope(&self, thread: &str, driver: Driver, env: &Envelope) {
        // A model change (the user's pick, a port) may settle or raise a retirement question.
        if matches!(env.event, Event::ModelChanged { .. }) {
            let weak = self.obj().downgrade();
            glib::idle_add_local_once(move || {
                if let Some(obj) = weak.upgrade() {
                    obj.imp().evaluate_models();
                }
            });
        }
        let in_view = self.selected_row_key() == Some(RowKey::Thread(thread.to_owned()));
        let focused = self.obj().is_active();
        enum After {
            Nothing,
            Sidebar,
            TurnDone { page: adw::TabPage, attention: bool },
            Title(String),
            RateLimited(adw::Banner, Driver),
        }
        let after = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(tab) = tabs
                .iter_mut()
                .find(|t| t.chat.as_ref().is_some_and(|c| c.thread == thread))
            else {
                return;
            };
            let page = tab.page.clone();
            let last_output = tab.last_output.clone();
            let Some(chat) = tab.chat.as_mut() else {
                return;
            };
            let switched = chat.driver != driver;
            chat.driver = driver;
            let after = match &env.event {
                Event::TurnStarted { .. } => {
                    chat.running = true;
                    chat.rate_limited = false;
                    chat.rate_banner.set_revealed(false);
                    // The state of the files before the turn, for "View diff".
                    chat.diffs.turn_started(self.config.borrow().checkpoints);
                    After::Sidebar
                }
                Event::ItemStarted {
                    kind: ItemKind::FileChange,
                    ..
                } => {
                    if let Some(item) = &env.item {
                        chat.diffs.item_started(item);
                    }
                    After::Nothing
                }
                Event::TurnCompleted { .. } => {
                    chat.diffs.turn_ended();
                    chat.running = false;
                    chat.approval = false;
                    let attention = !(in_view && focused);
                    if attention && !in_view {
                        chat.unread = true;
                    }
                    // The turn's end is "output went quiet" for the checkpoint bookkeeping.
                    last_output.set(std::time::Instant::now());
                    After::TurnDone { page, attention }
                }
                Event::SessionExited { .. } => {
                    chat.diffs.turn_ended();
                    chat.running = false;
                    chat.approval = false;
                    After::Sidebar
                }
                Event::ApprovalRequested { .. } | Event::QuestionRequested { .. } => {
                    chat.approval = true;
                    if !in_view {
                        chat.unread = true;
                    }
                    After::Sidebar
                }
                Event::ApprovalResolved { .. }
                | Event::ApprovalExpired
                | Event::QuestionResolved { .. } => {
                    chat.approval = false;
                    After::Sidebar
                }
                Event::RateLimited { .. } => {
                    chat.rate_limited = true;
                    After::RateLimited(chat.rate_banner.clone(), driver)
                }
                Event::ItemStarted {
                    kind: ItemKind::UserMessage,
                    ..
                } if chat.title.is_empty() => {
                    chat.title_item = env.item.clone();
                    After::Nothing
                }
                Event::ContentSnapshot { text, .. }
                    if chat.title.is_empty()
                        && env.item.is_some()
                        && env.item == chat.title_item =>
                {
                    let title = thread_title(text);
                    if title.is_empty() || title.starts_with('/') {
                        After::Nothing
                    } else {
                        chat.title = title.clone();
                        chat.title_item = None;
                        After::Title(title)
                    }
                }
                _ => After::Nothing,
            };
            // A switch to the other agent recolours the row.
            match after {
                After::Nothing if switched => After::Sidebar,
                other => other,
            }
        };
        match after {
            After::Nothing => {}
            After::Sidebar => self.refresh_sidebar(),
            After::Title(title) => {
                let (id, stored) = (thread.to_owned(), title.clone());
                self.write_store("title the thread", move |s| s.rename_thread(&id, &stored));
                if let Some(page) = self.page_of_thread(thread) {
                    page.set_title(&title);
                    if in_view {
                        if let Some(w) = self.window_title.borrow().as_ref() {
                            w.set_title(&title);
                        }
                    }
                }
                self.refresh_sidebar();
            }
            After::RateLimited(banner, driver) => {
                banner.set_title(&format!("{} hit its rate limit", driver_label(driver)));
                // With no other usable agent there is nothing to continue in: no button.
                match handoff_target(driver, |d| self.agent_usable(d)) {
                    Some(other) => banner
                        .set_button_label(Some(&format!("Continue in {}", driver_label(other)))),
                    None => banner.set_button_label(None),
                }
                banner.set_revealed(true);
                self.refresh_sidebar();
            }
            After::TurnDone { page, attention } => {
                if self.config.borrow().checkpoints {
                    self.request_checkpoint(&page, false);
                }
                // The turn's events moved the thread in the list (and unread); read it again.
                if in_view {
                    let id = thread.to_owned();
                    self.write_store("mark the thread read", move |s| s.mark_read(&id));
                } else {
                    self.reload_summaries();
                }
                if attention {
                    self.notify_bell(&page);
                }
                // agy reports no quota per turn: ask for it now, once this turn has unwound.
                if driver == Driver::Agy {
                    if let Some(slot) = self.slot_of(thread) {
                        glib::idle_add_local_once(move || {
                            slot.control(Control::Usage);
                        });
                    }
                }
                self.refresh_sidebar();
            }
        }
    }

    pub(super) fn page_of_thread(&self, thread: &str) -> Option<adw::TabPage> {
        self.tabs.borrow().iter().find_map(|t| {
            t.chat
                .as_ref()
                .is_some_and(|c| c.thread == thread)
                .then(|| t.page.clone())
        })
    }

    pub(super) fn slot_of(&self, thread: &str) -> Option<Rc<SessionSlot>> {
        self.tabs.borrow().iter().find_map(|t| {
            t.chat
                .as_ref()
                .filter(|c| c.thread == thread)
                .map(|c| c.slot.clone())
        })
    }

    /// The selected page's thread session, when it is a chat page.
    pub(super) fn current_slot(&self) -> Option<Rc<SessionSlot>> {
        let page = self.tab_view.borrow().as_ref()?.selected_page()?;
        self.tabs
            .borrow()
            .iter()
            .find(|t| t.page == page)
            .and_then(|t| t.chat.as_ref().map(|c| c.slot.clone()))
    }

    /// The rate-limit banner's button: hand the thread to the other agent.
    fn continue_rate_limited(&self, thread: &str) {
        if let Some(slot) = self.slot_of(thread) {
            match handoff_target(slot.driver(), |d| self.agent_usable(d)) {
                Some(target) => slot.switch(target, None, None),
                None => self.show_toast("No other agent is installed and enabled"),
            }
        }
    }

    fn view_action(&self, thread: &str, action: &ViewAction) {
        let Some(slot) = self.slot_of(thread) else {
            return;
        };
        let dir = self.page_of_thread(thread).and_then(|p| {
            self.tabs
                .borrow()
                .iter()
                .find(|t| t.page == p)
                .map(|t| t.dir.clone())
        });
        match action {
            ViewAction::NewThread => self.new_chat_thread(Some(slot.driver()), dir, None),
            ViewAction::Handoff { target } => {
                match target.or_else(|| handoff_target(slot.driver(), |d| self.agent_usable(d))) {
                    Some(target) => slot.switch(target, None, None),
                    None => self.show_toast("No other agent is installed and enabled"),
                }
            }
            ViewAction::Fork | ViewAction::CompactByHandoff => {
                self.fork_thread(thread, slot.driver(), dir);
            }
            ViewAction::Rewind => {
                if let Some(page) = self.page_of_thread(thread) {
                    self.rewind(&page);
                }
            }
            ViewAction::ModeChosen { driver, mode } => self.offer_default_mode(*driver, *mode),
        }
    }

    /// A new thread on the same agent and folder, carrying a budgeted, redacted handoff of
    /// this one.
    fn fork_thread(&self, thread: &str, driver: Driver, dir: Option<String>) {
        let store = app_store();
        let messages = match store.transcript_messages(thread) {
            Ok(m) => m,
            Err(e) => {
                present_message(&self.obj(), "Cannot Fork the Thread", &e.to_string());
                return;
            }
        };
        let title = self
            .tabs
            .borrow()
            .iter()
            .find_map(|t| {
                t.chat
                    .as_ref()
                    .filter(|c| c.thread == thread)
                    .map(|c| c.title.clone())
            })
            .unwrap_or_default();
        let source = if title.is_empty() {
            "the previous thread".to_owned()
        } else {
            format!("“{title}”")
        };
        let (summary, carried) = build_handoff(&messages, &format!("thread {thread}"));
        self.new_chat_thread(Some(driver), dir, None);
        // The new thread is the one just selected; it is built later, so the handoff waits.
        let page = self
            .tab_view
            .borrow()
            .as_ref()
            .and_then(|v| v.selected_page());
        if let Some(page) = page {
            let built = {
                let mut tabs = self.tabs.borrow_mut();
                let chat = tabs
                    .iter_mut()
                    .find(|t| t.page == page)
                    .and_then(|t| t.chat.as_mut());
                match chat {
                    Some(chat) if chat.view.is_none() => {
                        chat.pending_handoff = Some((summary.clone(), carried, source.clone()));
                        false
                    }
                    Some(_) => true,
                    None => false,
                }
            };
            if built {
                if let Some(session) = self.current_slot().and_then(|s| s.get()) {
                    session.seed_handoff(summary, carried, &source);
                }
            }
        }
    }

    /// `/rewind`: undo the last turn's changes in the thread's folder, through the 2.x
    /// checkpoint restore (confirmed, and itself undoable).
    fn rewind(&self, page: &adw::TabPage) {
        let Some((dir, key)) = self
            .tabs
            .borrow()
            .iter()
            .find(|t| &t.page == page)
            .map(|t| (t.dir.clone(), t.key))
        else {
            return;
        };
        let obj = self.obj();
        let page = page.clone();
        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            obj,
            async move {
                let diff = gtk4::gio::spawn_blocking(move || {
                    crate::git::tab_diff(
                        std::path::Path::new(&dir),
                        key,
                        crate::diff::DiffBase::LastTurn,
                    )
                })
                .await
                .unwrap_or_else(|_| Err("the diff thread panicked".to_string()));
                match diff {
                    Ok(crate::git::DiffOutcome::Ready(d)) => match d.undo_to {
                        Some(target) => obj.imp().confirm_restore(
                            &page,
                            target,
                            "how they were before the last turn",
                        ),
                        None => obj.imp().show_toast("The last turn changed no files"),
                    },
                    Ok(crate::git::DiffOutcome::NotRepo) => obj
                        .imp()
                        .show_toast("Rewind needs a git repository: this folder is not in one"),
                    Ok(crate::git::DiffOutcome::Unavailable(why)) => obj.imp().show_toast(&why),
                    Err(e) => present_message(&obj, "Cannot Rewind", &e),
                }
            }
        ));
    }

    // -----------------------------------------------------------------------------------------
    // New threads
    // -----------------------------------------------------------------------------------------

    /// Whether `driver` can take a thread: only when detection found it and it is enabled
    /// ([`Availability::Ready`]). Nothing is assumed: an agent still being detected is not usable.
    pub(super) fn agent_usable(&self, driver: Driver) -> bool {
        AgentAvailability::shared().is_ready(driver)
    }

    /// The agent new threads start on: see [`crate::config::choose_default_agent`].
    pub(super) fn default_agent(&self) -> Option<Driver> {
        let (explicit, profile_driver) = {
            let config = self.config.borrow();
            (
                config.default_agent,
                config.selected_profile().and_then(profile_driver),
            )
        };
        crate::config::choose_default_agent(explicit, profile_driver, |d| self.agent_usable(d))
    }

    /// Creates a thread in `dir` (the starting folder when `None`) on `driver` (the default
    /// agent when `None`), opens and shows it. `prompt` is sent once it has started.
    pub(super) fn new_chat_thread(
        &self,
        driver: Option<Driver>,
        dir: Option<String>,
        prompt: Option<String>,
    ) {
        let Some(driver) = driver.or_else(|| self.default_agent()) else {
            // Still looking for the agents: wait for the scan rather than guess (or give up).
            if AgentAvailability::shared().any_detecting() {
                *self.pending_new_thread.borrow_mut() = Some((dir, prompt));
                self.show_toast("Detecting agents…");
                return;
            }
            present_message(
                &self.obj(),
                "No Chat Agent Available",
                "None of Claude, Antigravity (agy) or Codex is installed and enabled. Install one, \
                 or enable it in Settings → Agents. Terminal threads still work from the New \
                 Thread menu.",
            );
            return;
        };
        self.create_chat_thread(NewThread {
            dir,
            prompt,
            ..NewThread::new(driver)
        });
    }

    /// Creates a thread, opens and shows it. Whatever `new` leaves unset comes from the agent's
    /// profile. Returns its page.
    pub(super) fn create_chat_thread(&self, new: NewThread) -> Option<adw::TabPage> {
        let NewThread {
            driver,
            dir,
            prompt,
            model,
            effort,
            title,
            handoff,
        } = new;
        let home = env::var("HOME").unwrap_or_else(|_| "/".to_string());
        let requested = dir.unwrap_or_else(|| {
            self.config
                .borrow()
                .agent_profile(driver)
                .and_then(|p| p.dir.clone())
                .unwrap_or_else(|| self.config.borrow().starting_directory.clone())
        });
        let cwd = resolve_working_directory(&requested, &home);
        let store = app_store();
        let thread = match store.create_thread(&cwd, title.as_deref()) {
            Ok(id) => id,
            Err(e) => {
                present_message(&self.obj(), "Cannot Create a Thread", &e.to_string());
                return None;
            }
        };
        // The agent is recorded now, so the sidebar and a later reopen know it before the
        // session starts. The session adopts this provider thread.
        let model = model.or_else(|| {
            self.config
                .borrow()
                .agent_profile(driver)
                .and_then(|p| p.default_model.clone())
        });
        // A profile default the agent has retired is never started: the suggestion is used.
        let (model, effort, _) = live_model(driver, model, effort);
        if let Err(e) = store
            .add_provider_thread(
                &thread,
                driver_key(driver),
                model.as_deref().unwrap_or("default"),
            )
            .and_then(|pt| store.set_active_provider_thread(&thread, &pt))
        {
            warn!("Cannot record the thread's agent: {e}");
        }
        info!("New {} thread in {cwd}", driver_label(driver));
        self.reload_summaries();
        let page = self.open_thread(&thread, false)?;
        if let Some(chat) = self
            .tabs
            .borrow_mut()
            .iter_mut()
            .find(|t| t.page == page)
            .and_then(|t| t.chat.as_mut())
        {
            chat.driver = driver;
            chat.pending_prompt = prompt;
            chat.pending_effort = effort;
            chat.pending_handoff = handoff;
            if let Some(title) = title {
                chat.title = title;
            }
        }
        self.open_thread(&thread, true);
        Some(page)
    }

    /// Resumes a native Claude or agy session as a chat thread: a new thread whose provider
    /// thread carries that native id.
    pub(super) fn resume_as_thread(&self, driver: Driver, native_id: &str, dir: Option<String>) {
        let home = env::var("HOME").unwrap_or_else(|_| "/".to_string());
        let cwd = resolve_working_directory(
            &dir.unwrap_or_else(|| self.config.borrow().starting_directory.clone()),
            &home,
        );
        let store = app_store();
        let made = store.create_thread(&cwd, None).and_then(|thread| {
            let pt = store.add_provider_thread(&thread, driver_key(driver), "default")?;
            store.set_native_id(&pt, native_id)?;
            store.set_active_provider_thread(&thread, &pt)?;
            Ok(thread)
        });
        match made {
            Ok(thread) => {
                info!(
                    "Resuming {} session {native_id} as a thread",
                    driver_label(driver)
                );
                self.open_thread(&thread, true);
            }
            Err(e) => present_message(&self.obj(), "Cannot Resume the Session", &e.to_string()),
        }
    }

    /// Routes a resume to a chat thread or, for a CLI with no adapter, a terminal page.
    pub(super) fn resume_mapped(
        &self,
        profile: &Profile,
        session_id: &str,
        dir: Option<String>,
    ) -> bool {
        match resume_as(profile_driver(profile), session_id) {
            ResumeAs::Chat { driver, native_id } => {
                self.resume_as_thread(driver, &native_id, dir);
                true
            }
            ResumeAs::Terminal => false,
        }
    }

    // -----------------------------------------------------------------------------------------
    // The terminal drawer
    // -----------------------------------------------------------------------------------------

    /// Ctrl+`: shows or hides the current thread's shell, starting it the first time.
    pub(super) fn toggle_drawer(&self) {
        let Some(page) = self
            .tab_view
            .borrow()
            .as_ref()
            .and_then(|v| v.selected_page())
        else {
            return;
        };
        let state = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(tab) = tabs.iter_mut().find(|t| t.page == page) else {
                return;
            };
            let dir = tab.dir.clone();
            let terminal = tab.terminal.clone();
            let stack = tab.stack.clone();
            let Some(chat) = tab.chat.as_mut() else {
                return; // a terminal page is all terminal already
            };
            let open = !chat.drawer.is_visible();
            let start = open && !chat.drawer_started;
            if start {
                chat.drawer_started = true;
            }
            (
                open,
                start,
                chat.drawer.clone(),
                chat.drawer_paned.clone(),
                terminal,
                stack,
                dir,
                chat.view.clone(),
            )
        };
        let (open, start, drawer, paned, terminal, stack, dir, view) = state;
        drawer.set_visible(open);
        if open {
            // Two thirds chat, one third shell, on first show.
            let height = paned.height();
            if height > 0 {
                paned.set_position(height * 2 / 3);
            }
            if start {
                self.spawn_session(&terminal, &stack, None, &dir, Launch::Fresh);
            }
            terminal.grab_focus();
        } else if let Some(view) = view {
            view.focus_composer();
        }
    }

    fn drawer_exited(&self, page: &adw::TabPage) {
        let view = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(chat) = tabs
                .iter_mut()
                .find(|t| &t.page == page)
                .and_then(|t| t.chat.as_mut())
            else {
                return;
            };
            chat.drawer_started = false;
            chat.drawer.set_visible(false);
            chat.view.clone()
        };
        if let Some(view) = view {
            view.focus_composer();
        }
    }

    // -----------------------------------------------------------------------------------------
    // Catalogue and account status
    // -----------------------------------------------------------------------------------------

    /// Resolves both chat agents and refreshes the model catalogue and the account status with
    /// their binaries; again every ten minutes while the window is focused.
    fn watch_account_status(&self) {
        self.refresh_agent_data();
        let obj = self.obj();
        glib::timeout_add_local(
            ACCOUNT_REFRESH,
            glib::clone!(
                #[weak]
                obj,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    if obj.is_active() {
                        obj.imp().refresh_agent_data();
                    }
                    glib::ControlFlow::Continue
                }
            ),
        );
    }

    /// Scans for the agents and refreshes everything that depends on what is there: each
    /// driver's [`Availability`] (the one source for every menu and picker), then the model
    /// catalogue and the account status for the agents that turned out Ready. A switched-off
    /// agent is known at once; the others keep their last state until the scan answers.
    pub(super) fn refresh_agent_data(&self) {
        // Also what a switch to agy needs: its hook verdict, read off the main thread.
        refresh_hook_verdict();
        let availability = AgentAvailability::shared();
        availability.mark_scan_started();
        let (profiles, clear): (Vec<(Driver, Profile)>, Vec<String>) = {
            let config = self.config.borrow();
            (
                Driver::ALL
                    .into_iter()
                    .map(|d| {
                        let profile = config
                            .agent_profile(d)
                            .cloned()
                            .unwrap_or_else(|| crate::config::new_agent_profile(d));
                        (d, profile)
                    })
                    .collect(),
                config.clear_env.clone(),
            )
        };
        for (driver, profile) in &profiles {
            if profile.disabled {
                availability.set(*driver, Availability::Disabled);
            } else if availability.get(*driver) == Availability::Disabled {
                availability.set(*driver, Availability::Detecting);
            }
        }
        // Agents that were missing, or whose binary has gone, are looked for again.
        forget_stale_resolutions();
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            let availability = AgentAvailability::shared();
            let mut envs: Vec<(Driver, AgentEnv)> = Vec::new();
            for (driver, profile) in profiles {
                if profile.disabled {
                    continue;
                }
                let resolved = resolve_agent(profile).await;
                // Probes run where the threads run: the profile's env file and `clear_env`.
                envs.push((
                    driver,
                    AgentEnv {
                        env: resolved.env.clone(),
                        unset: unset_list(&clear),
                    },
                ));
                availability.set(driver, classify(false, Some(resolved.program.as_deref())));
            }
            // Only what is Ready is probed. The same gate holds for a thread's session
            // (`build_thread`, `start_session`) and a cross-agent switch (`agent_launch`): a
            // missing, switched-off or still-detecting agent is never spawned.
            let targets = probe_targets(&availability.all(), |d| {
                envs.iter()
                    .find(|(driver, _)| *driver == d)
                    .map(|(_, env)| env.clone())
                    .unwrap_or_default()
            });
            ModelCatalog::shared().refresh(&targets);
            AccountStatus::shared().refresh(&targets);
            if let Some(obj) = obj.upgrade() {
                obj.imp().on_availability_changed();
            }
        });
    }

    /// Connects the window to [`AgentAvailability`] and re-scans when the window regains focus
    /// and the last scan is over a minute old (a binary installed or removed meanwhile).
    fn watch_availability(&self) {
        let obj = self.obj();
        AgentAvailability::shared().connect_changed(glib::clone!(
            #[weak]
            obj,
            move || obj.imp().on_availability_changed()
        ));
        // A fresh model list may retire a thread's model (or bring it back).
        ModelCatalog::shared().connect_changed(glib::clone!(
            #[weak]
            obj,
            move || obj.imp().evaluate_models()
        ));
        obj.connect_is_active_notify(|window| {
            if window.is_active() && AgentAvailability::shared().scan_is_stale() {
                window.imp().refresh_agent_data();
            }
            // A thread waiting for approval glows while you are away from it.
            window.imp().refresh_sidebar();
        });
    }

    /// An agent's availability changed (or a scan ended): everything that offers agents follows.
    pub(super) fn on_availability_changed(&self) {
        self.fill_new_with_menu();
        // The picker hides agents that are not ready; an open one redraws.
        ModelCatalog::shared().notify();
        self.update_unavailable_banners();
        self.start_pending_sessions();
        self.evaluate_models();
        // A "new thread" asked for while the agents were still being detected.
        if !AgentAvailability::shared().any_detecting() {
            let pending = self.pending_new_thread.borrow_mut().take();
            if let Some((dir, prompt)) = pending {
                self.new_chat_thread(None, dir, prompt);
            }
        }
    }

    /// Judges every built thread's model against its agent's CURRENT catalogue: one the agent no
    /// longer offers gets a banner (with "Choose…") and, when there is a replacement, the session
    /// is told to move to it before the next turn. A list that is only cached, or still loading,
    /// is never grounds for calling a model retired.
    pub(super) fn evaluate_models(&self) {
        let catalog = ModelCatalog::shared();
        let availability = AgentAvailability::shared();
        let built: Vec<(adw::Banner, Rc<ChatSession>)> = self
            .tabs
            .borrow()
            .iter()
            .filter_map(|t| t.chat.as_ref())
            .filter_map(|c| Some((c.model_banner.clone(), c.slot.get()?)))
            .collect();
        for (banner, session) in built {
            let status = session.status();
            let driver = status.driver;
            let notice = if availability.is_ready(driver) {
                model_notice(
                    &catalog.models_of(driver),
                    catalog.is_fresh(driver),
                    session.selected_model().as_deref(),
                    status.effort.as_deref(),
                )
            } else {
                ModelNotice::None
            };
            match retirement_banner(driver_label(driver), &notice) {
                Some(text) => {
                    banner.set_title(&text);
                    banner.set_revealed(true);
                }
                None => banner.set_revealed(false),
            }
            session.set_retired(match notice {
                ModelNotice::Retired {
                    model,
                    replacement: Some(replacement),
                } => Some(RetiredModel { model, replacement }),
                _ => None,
            });
        }
    }

    /// The model banner's "Choose…": the thread's own model picker.
    fn choose_model(&self, thread: &str) {
        let view = self
            .tabs
            .borrow()
            .iter()
            .filter_map(|t| t.chat.as_ref())
            .find(|c| c.thread == thread)
            .and_then(|c| c.view.clone());
        if let Some(view) = view {
            view.open_model_picker();
        }
    }

    /// An open thread whose agent is missing or switched off says so, and offers to continue in
    /// another agent; the banner goes away again when the agent is back.
    fn update_unavailable_banners(&self) {
        let availability = AgentAvailability::shared();
        let mut tabs = self.tabs.borrow_mut();
        for chat in tabs.iter_mut().filter_map(|t| t.chat.as_mut()) {
            let driver = chat.slot.driver();
            let state = availability.get(driver);
            let other = handoff_target(driver, |d| availability.is_ready(d));
            match unavailable_banner(driver, &state, other) {
                Some((title, button)) => {
                    chat.rate_banner.set_title(&title);
                    chat.rate_banner.set_button_label(button.as_deref());
                    chat.rate_banner.set_revealed(true);
                    chat.unavailable = true;
                }
                None if chat.unavailable && state.is_ready() => {
                    chat.rate_banner.set_revealed(false);
                    chat.unavailable = false;
                }
                None => {}
            }
        }
    }

    /// Starts the session of every open thread that was built without one because its agent was
    /// not Ready and now is. Each start re-resolves the agent and reads the hook verdict and the
    /// provider thread off the main thread, then waits briefly for a fresh model list, exactly
    /// like a first build.
    fn start_pending_sessions(&self) {
        let availability = AgentAvailability::shared();
        let waiting: Vec<(adw::TabPage, String, Driver, String)> = self
            .tabs
            .borrow_mut()
            .iter_mut()
            .filter_map(|t| {
                let chat = t.chat.as_mut()?;
                let driver = chat.slot.driver();
                if chat.view.is_none()
                    || chat.starting
                    || chat.slot.get().is_some()
                    || !availability.is_ready(driver)
                {
                    return None;
                }
                chat.starting = true;
                Some((t.page.clone(), chat.thread.clone(), driver, t.dir.clone()))
            })
            .collect();
        for (page, thread, driver, dir) in waiting {
            let profile = self
                .config
                .borrow()
                .agent_profile(driver)
                .cloned()
                .unwrap_or_else(|| crate::config::new_agent_profile(driver));
            let obj = self.obj().downgrade();
            glib::MainContext::default().spawn_local(async move {
                let resolved = resolve_agent(profile.clone()).await;
                // A store that cannot be read starts nothing: a session started without the
                // thread's provider would re-point the thread at a fresh session.
                let provider = match store_job({
                    let thread = thread.clone();
                    move |store| load_provider(store, &thread)
                })
                .await
                {
                    Some(Ok(provider)) => Some(provider),
                    Some(Err(e)) => {
                        warn!("Cannot read thread {thread} to start its session: {e}");
                        None
                    }
                    None => None,
                };
                let hook = match driver {
                    Driver::Agy => check_hook().await,
                    Driver::Claude | Driver::Codex => Ok(()),
                };
                if AgentAvailability::shared().is_ready(driver) {
                    wait_for_fresh_catalog(driver).await;
                }
                let Some(obj) = obj.upgrade() else { return };
                let ready =
                    AgentAvailability::shared().is_ready(driver) && resolved.program.is_some();
                let Some(provider) = provider.filter(|_| ready) else {
                    // Gone again while resolving, or the store failed: wait for the next change.
                    if let Some(chat) = obj
                        .imp()
                        .tabs
                        .borrow_mut()
                        .iter_mut()
                        .find(|t| t.page == page)
                        .and_then(|t| t.chat.as_mut())
                    {
                        chat.starting = false;
                    }
                    return;
                };
                obj.imp().start_session(
                    &page,
                    StartJob {
                        thread,
                        driver,
                        dir,
                        profile,
                        resolved,
                        provider,
                        hook,
                    },
                );
            });
        }
    }
}

/// Forgets resolutions that found nothing or whose binary is gone, so the next scan looks again
/// (the shell probe is costly, so a binary that is still there is kept).
fn forget_stale_resolutions() {
    RESOLVED.with(|r| {
        r.borrow_mut().retain(|_, agent| {
            agent
                .program
                .as_deref()
                .is_some_and(|p| std::path::Path::new(p).exists())
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native(
        driver: Driver,
        id: &str,
        cwd: Option<&str>,
        modified: i64,
    ) -> agent_kit::native::NativeSession {
        agent_kit::native::NativeSession {
            driver,
            native_id: id.into(),
            cwd: cwd.map(str::to_owned),
            title: Some(format!("session {id}")),
            modified,
        }
    }

    #[test]
    fn opening_a_thread_on_a_native_session_imports_its_history_once() {
        const TRANSCRIPT: &str = include_str!(
            "../../../crates/agent-core/tests/fixtures/claude-transcript-synthetic.jsonl"
        );
        let home = tempfile::tempdir().expect("home");
        let project = home.path().join(".claude/projects/-repo");
        std::fs::create_dir_all(&project).expect("projects");
        let id = "0b3c6c1e-5d2a-4f0e-9a1b-2c3d4e5f6a7b";
        std::fs::write(project.join(format!("{id}.jsonl")), TRANSCRIPT).expect("transcript");

        let store = Store::open_in_memory().expect("store");
        let thread = store
            .link_native_thread("/repo", None, 1_000, "claude", id)
            .expect("link");
        let load = |thread: &str, home: Option<&std::path::Path>| {
            load_thread(&store, thread, home).expect("load")
        };
        let first = load(&thread, Some(home.path()));
        assert!(
            first.history.iter().any(|e| matches!(
                e.event,
                Event::ItemStarted {
                    kind: ItemKind::UserMessage,
                    ..
                }
            )),
            "the conversation shows"
        );
        // Opened again: read from the store, never imported twice.
        let again = load(&thread, Some(home.path()));
        assert_eq!(again.history.len(), first.history.len());
        // No home (the memory-only store, tests): nothing is read from disk.
        let other = store
            .link_native_thread(
                "/repo",
                None,
                1_000,
                "claude",
                "1b3c6c1e-5d2a-4f0e-9a1b-2c3d4e5f6a7b",
            )
            .expect("link");
        assert!(load(&other, None).history.is_empty());

        // A transcript that is not there: one notice, stored, so it is not retried every open.
        let missing = load(&other, Some(home.path()));
        assert!(matches!(
            missing.history.as_slice(),
            [e] if matches!(&e.event, Event::Notice { text } if text.contains("could not be loaded"))
        ));
        assert_eq!(load(&other, Some(home.path())).history.len(), 1);
    }

    #[test]
    fn recent_sessions_become_threads_once_in_session_order_and_never_return_after_a_delete() {
        let store = Store::open_in_memory().expect("store");
        let sessions = [
            native(Driver::Claude, "c-new", Some("/w/new"), 2_000),
            native(Driver::Agy, "a-old", None, 1_000),
        ];
        assert_eq!(link_recent_sessions(&store, &sessions, "/home/u"), 2);
        let threads = store.list_threads(false).expect("list");
        let titles: Vec<_> = threads.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(
            titles,
            ["session c-new", "session a-old"],
            "newest session first"
        );
        assert_eq!(threads[0].cwd, "/w/new");
        assert_eq!(
            threads[1].cwd, "/home/u",
            "no recorded folder: the fallback"
        );
        assert_eq!(threads[1].driver.as_deref(), Some("agy"));
        // The thread resumes the session: its provider thread carries the native id.
        let pt = load_provider(&store, &threads[0].id)
            .expect("read")
            .expect("provider");
        assert_eq!(pt.native_id.as_deref(), Some("c-new"));

        // A second scan adds nothing.
        assert_eq!(link_recent_sessions(&store, &sessions, "/home/u"), 0);
        // A deleted one stays gone.
        store.delete_thread(&threads[1].id).expect("delete");
        assert_eq!(link_recent_sessions(&store, &sessions, "/home/u"), 0);
        assert_eq!(store.list_threads(true).expect("list").len(), 1);
    }

    #[test]
    fn a_rescan_keeps_found_binaries_that_still_exist_and_looks_again_for_the_rest() {
        let dir = tempfile::tempdir().expect("tmp");
        let present = dir.path().join("claude");
        std::fs::write(&present, "").expect("file");
        let agent = |program: Option<String>| ResolvedAgent {
            program,
            env: Vec::new(),
        };
        RESOLVED.with(|r| {
            let mut r = r.borrow_mut();
            r.clear();
            r.insert(
                ("claude".into(), None),
                agent(Some(present.display().to_string())),
            );
            r.insert(("agy".into(), None), agent(None));
            r.insert(
                ("codex".into(), None),
                agent(Some("/nonexistent/codex".into())),
            );
        });
        forget_stale_resolutions();
        let kept: Vec<String> = RESOLVED.with(|r| r.borrow().keys().map(|k| k.0.clone()).collect());
        assert_eq!(
            kept,
            ["claude"],
            "missing and vanished entries are probed again"
        );
        RESOLVED.with(|r| r.borrow_mut().clear());
    }

    #[test]
    fn an_agent_that_is_not_ready_is_never_launched() {
        for (state, text) in [
            (Availability::Missing, "Claude is not available."),
            (Availability::Disabled, "Claude is switched off."),
            (Availability::Detecting, "Claude is still being detected."),
        ] {
            let refused = agent_launch_with(&state, Driver::Claude, "t", "/w");
            assert_eq!(refused.err().as_deref(), Some(text));
        }
    }

    #[test]
    fn the_catalogue_wait_ends_at_the_limit_or_as_soon_as_the_list_is_fresh() {
        use std::time::Duration;
        let ctx = glib::MainContext::new();
        let step = Duration::from_millis(5);
        // Never fresh: gives up at the limit (and does not hang).
        let started = std::time::Instant::now();
        let fresh = ctx.block_on(wait_until(Duration::from_millis(40), step, || false));
        assert!(!fresh);
        assert!(started.elapsed() < Duration::from_secs(2));
        // Fresh after a few polls: returns true well before the limit.
        let polls = std::cell::Cell::new(0);
        let fresh = ctx.block_on(wait_until(Duration::from_secs(5), step, || {
            polls.set(polls.get() + 1);
            polls.get() > 3
        }));
        assert!(fresh);
        // Already fresh: no waiting at all.
        assert!(ctx.block_on(wait_until(Duration::ZERO, step, || true)));
    }

    #[test]
    fn unset_list_always_drops_an_inherited_approval_socket() {
        let names = unset_list(&[]);
        assert!(names.iter().any(|n| n == agent_core::approval::ENV_SOCKET));
        assert!(names
            .iter()
            .any(|n| n == crate::approval_server::ENV_HOOK_BIN));
    }

    #[test]
    fn an_unstarted_slot_reports_its_thread_agent() {
        let slot = SessionSlot {
            session: RefCell::new(None),
            view: RefCell::new(None),
            driver: Driver::Agy,
            model: Some("gemini".into()),
            mode: Mode::Plan,
        };
        let status = slot.status();
        assert_eq!(status.driver, Driver::Agy);
        assert!(!status.alive);
        assert_eq!(slot.control(Control::Usage), "ctl-unstarted");
        let codex = SessionSlot {
            session: RefCell::new(None),
            view: RefCell::new(None),
            driver: Driver::Codex,
            model: None,
            mode: Mode::Ask,
        };
        assert_eq!(codex.status().driver, Driver::Codex);
        assert!(codex.status().capabilities.live_approvals);
    }

    #[test]
    fn a_retired_model_is_replaced_only_on_a_fresh_list_and_the_move_is_recorded() {
        use agent_core::catalog::CatalogModel;
        let model = |id: &str| CatalogModel {
            driver: Driver::Claude,
            id: id.into(),
            display: id.to_uppercase(),
            description: None,
            efforts: vec!["low".into(), "high".into()],
            default_effort: None,
            via: None,
        };
        let list = vec![model("default"), model("sonnet")];
        let run =
            |fresh, m: &str| decide_live_model(&list, fresh, Some(m.into()), Some("high".into()));
        // Cached or loading: left alone, even if it looks gone.
        assert_eq!(
            run(false, "claude-sonnet-3-7").0.as_deref(),
            Some("claude-sonnet-3-7")
        );
        // A listed model is left alone.
        assert_eq!(run(true, "sonnet").0.as_deref(), Some("sonnet"));
        assert!(run(true, "sonnet").2.is_none());
        // A retired one is replaced, keeping the effort the target offers.
        let (m, e, moved) = run(true, "claude-sonnet-3-7");
        assert_eq!((m.as_deref(), e.as_deref()), (Some("sonnet"), Some("high")));
        assert_eq!(moved.expect("recorded").model, "claude-sonnet-3-7");
        // No model at all is the agent's own default: nothing to port.
        assert_eq!(
            decide_live_model(&list, true, None, None),
            (None, None, None)
        );
    }

    #[test]
    fn an_unchecked_hook_is_treated_as_not_installed() {
        HOOK_VERDICT.with(|v| *v.borrow_mut() = None);
        let before = cached_hook_verdict();
        assert!(before
            .as_ref()
            .is_err_and(|e| e.contains("not been checked")));
        // And it stays closed through the binding: no socket, so agy runs without the skip flag.
        // The hook is not known to be installed, so the notice explains how to install it.
        assert!(matches!(
            bind_approval(before, "t", "/tmp", Mode::Ask),
            Err(None)
        ));
        // Installed, but the socket cannot be used (a relative workspace is refused before
        // anything is created): the notice carries that reason instead.
        assert!(matches!(
            bind_approval(Ok(()), "t", "relative", Mode::Ask),
            Err(Some(reason)) if !reason.is_empty()
        ));
        HOOK_VERDICT.with(|v| *v.borrow_mut() = Some(Err("not installed".into())));
        assert_eq!(cached_hook_verdict(), Err("not installed".to_owned()));
        HOOK_VERDICT.with(|v| *v.borrow_mut() = Some(Ok(())));
        assert_eq!(cached_hook_verdict(), Ok(()));
        HOOK_VERDICT.with(|v| *v.borrow_mut() = None);
    }

    #[test]
    fn saved_open_threads_restore_only_what_still_exists_in_order_and_capped() {
        let saved = serde_json::json!({"open": ["a", "gone", "b", "c"], "selected": "b"});
        let known: Vec<String> = ["a", "b", "c"].map(str::to_owned).to_vec();
        let open = restorable(&saved, &known, 2);
        assert_eq!(open, ["a", "b"]);
        assert_eq!(selected_to_restore(&saved, &open).as_deref(), Some("b"));
        // A selection that was not reopened falls back to the first.
        let saved = serde_json::json!({"open": ["a", "b"], "selected": "c"});
        assert_eq!(selected_to_restore(&saved, &open).as_deref(), Some("a"));
        assert!(restorable(&serde_json::json!({}), &known, 5).is_empty());
        assert_eq!(selected_to_restore(&serde_json::json!({}), &[]), None);
    }

    #[test]
    fn history_is_read_in_batches_up_to_the_replay_limit() {
        let store = Store::open_in_memory().expect("store");
        let thread = store.create_thread("/w", Some("t")).expect("thread");
        for _ in 0..2_500 {
            store
                .append_event(
                    &thread,
                    None,
                    &Envelope::new(Event::Notice { text: "x".into() }),
                )
                .expect("event");
        }
        // More than one batch of 2,000, in order, nothing lost.
        let history = read_history(&store, &thread).expect("read");
        assert_eq!(history.len(), 2_500);
        assert!(read_history(&store, "nope").expect("read").is_empty());
    }

    #[test]
    fn a_long_thread_replays_its_newest_turns_behind_a_notice() {
        let store = Store::open_in_memory().expect("store");
        let thread = store.create_thread("/w", Some("t")).expect("thread");
        let note = |text: String| Envelope::new(Event::Notice { text });
        // Each turn as a session stores it: the prompt first, then the agent's turn start.
        for turn in 0..10 {
            store
                .append_event(
                    &thread,
                    None,
                    &Envelope::new(Event::ItemStarted {
                        kind: ItemKind::UserMessage,
                        title: format!("p{turn}"),
                        input: None,
                        parent: None,
                    })
                    .item(format!("u{turn}")),
                )
                .expect("event");
            store
                .append_event(
                    &thread,
                    None,
                    &Envelope::new(Event::TurnStarted {
                        model: Some(format!("t{turn}")),
                    }),
                )
                .expect("event");
            for step in 0..3 {
                store
                    .append_event(&thread, None, &note(format!("{turn}.{step}")))
                    .expect("event");
            }
        }
        let text = |e: &Envelope| match &e.event {
            Event::Notice { text } => text.clone(),
            Event::TurnStarted { model } => model.clone().unwrap_or_default(),
            Event::ItemStarted { title, .. } => title.clone(),
            _ => String::new(),
        };
        // 50 events, limit 12: the newest 12 start mid-turn 7, so the cut moves on to turn 8,
        // starting at its prompt.
        let history = read_history_within(&store, &thread, 12).expect("read");
        let got: Vec<String> = history.iter().map(text).collect();
        assert!(got[0].contains("only its most recent part"), "{got:?}");
        assert_eq!(got[1..3], ["p8", "t8"], "{got:?}");
        assert_eq!(got.last().map(String::as_str), Some("9.2"));
        assert_eq!(got.len(), 1 + 10);
        // One turn longer than the window: no turn start in it, so it is shown from its first
        // prompt, or as it is.
        let long = store.create_thread("/w", Some("long")).expect("thread");
        for step in 0..10 {
            if step == 7 {
                // A queued follow-up prompt inside the long turn: the window starts there.
                store
                    .append_event(
                        &long,
                        None,
                        &Envelope::new(Event::ItemStarted {
                            kind: ItemKind::UserMessage,
                            title: "follow-up".into(),
                            input: None,
                            parent: None,
                        })
                        .item("u-long"),
                    )
                    .expect("event");
            }
            store
                .append_event(&long, None, &note(format!("s{step}")))
                .expect("event");
        }
        // Newest 5: s6, follow-up, s7, s8, s9; the cut drops s6.
        let got: Vec<String> = read_history_within(&store, &long, 5)
            .expect("read")
            .iter()
            .map(text)
            .collect();
        assert_eq!(got[1..], ["follow-up", "s7", "s8", "s9"], "{got:?}");
        // A thread that fits is replayed whole, with no notice.
        let whole = read_history_within(&store, &thread, 50).expect("read");
        assert_eq!(whole.len(), 50);
    }

    #[test]
    fn a_store_job_runs_off_the_main_thread_on_a_connection_of_its_own() {
        use crate::testutil::{in_loop, pump_until};
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("threads.db");
        let main_store = Store::open(&path).expect("open");
        let thread = main_store.create_thread("/w", Some("t")).expect("thread");
        STORE_PATH.with(|p| *p.borrow_mut() = Some(path));
        assert!(store_is_async());

        let main_thread = std::thread::current().id();
        let got = in_loop(|ctx| {
            let out = Rc::new(RefCell::new(None));
            let o = out.clone();
            let id = thread.clone();
            glib::spawn_future_local(async move {
                let result = store_job(move |store| {
                    (
                        std::thread::current().id(),
                        store.thread_summary(&id).map(|s| s.map(|s| s.title)),
                    )
                })
                .await;
                *o.borrow_mut() = Some(result);
            });
            assert!(pump_until(ctx, 15, || out.borrow().is_some()), "no result");
            let got = out.borrow_mut().take();
            got
        });
        STORE_PATH.with(|p| *p.borrow_mut() = None);
        let (worker, title) = got.flatten().expect("the job ran");
        assert_ne!(worker, main_thread, "the job ran on the main thread");
        assert_eq!(title.expect("query").as_deref(), Some("t"));
        // A write from the worker is visible to the main connection (WAL).
        assert!(main_store.thread_summary(&thread).expect("read").is_some());
    }
}
