//! The chat thread widget: transcript, composer with typeahead, approvals and panels.
//!
//! [`ChatView`] renders one thread from canonical [`Envelope`]s (fed through [`ChatView::sink`])
//! and calls [`ChatBackend`] for every user action. It never touches a process, the store or
//! an adapter.
//!
//! Structure:
//! - `model`: the pure envelope → item reducer (expand state, approvals, gauge, plan);
//! - `markdown`: pure markdown → Pango-markup/code blocks;
//! - `typeahead`, `payload`: pure key handling, completion and control-payload readers;
//! - `transcript`, `cards`, `composer`, `header`, `panels`: the widgets;
//! - `demo`: a scripted fake backend for `--chat-demo`.
//!
//! Re-entrancy rule: a backend may call the sink synchronously from inside any trait method,
//! so no `RefCell` borrow is ever held across a backend call.

mod background;
mod cards;
mod composer;
mod delegation;
pub mod demo;
mod difftext;
mod follow;
mod form_card;
mod form_draft;
mod git_card;
mod header;
mod interruption;
pub(crate) mod markdown;
pub mod model;
mod panels;
mod payload;
mod permission;
mod quick_replies;
mod subagent_shelf;
mod subagents;
mod thinking;
mod transcript;
mod typeahead;
pub mod usage;

#[cfg(test)]
mod gap_tests;
#[cfg(test)]
mod resize_tests;
#[cfg(test)]
mod ux_tests;

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::{Rc, Weak};

use adw::subclass::prelude::*;
use agent_core::adapter::{Control, Driver, Mode};
use agent_core::commands::{builtins, compact_text, BuiltinAction, Trigger};
use agent_core::event::Event;
use agent_core::event::{BackgroundTask, BackgroundTaskKind, Envelope, PlanStep, StepStatus};
use gtk4::prelude::*;
use gtk4::{gdk, glib};

use super::workspace_files;
use super::{ChatBackend, DiffAsk, DiffSource, EnvelopeSink, ModelSource, SessionStatus};
use crate::account_status::AccountStatus;
use cards::{RowEvent, RowSink};
use composer::{Composer, ComposerHost};
use header::{Header, MODES};
use model::{Change, Tone, Transcript};
use panels::{ModelListener, PanelCtx, Requests};
use transcript::TranscriptView;
use usage::UsageIndicator;

/// Something the view cannot do itself; the window (wave 3) connects to these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewAction {
    /// `/clear`, `/new`.
    NewThread,
    /// `/handoff [agent]`.
    Handoff { target: Option<Driver> },
    /// `/fork`.
    Fork,
    /// `/rewind`.
    Rewind,
    /// `/compact` on an agent with no compaction command: continue in a fresh thread of the same
    /// agent through a budgeted handoff ("handoff-to-self").
    CompactByHandoff,
    /// The user picked `mode` in the header for this thread's `driver`: the window offers to make
    /// it that agent's default for new threads.
    ModeChosen { driver: Driver, mode: Mode },
    /// "Don't suggest these" on the quick-reply chips: the window turns the setting off, which
    /// applies at once, and says where it turns back on.
    StopQuickReplies,
}

type ActionHandler = Rc<dyn Fn(&ViewAction)>;

const STYLE: &str = include_str!("view/style.css");

fn mode_change_asks_first(status: &SessionStatus, mode: Mode) -> bool {
    status.running_turn
        && status.capabilities.mode_change_restarts
        && !status.capabilities.live_approvals
        && mode != status.mode
        && matches!(mode, Mode::Plan | Mode::AcceptEdits)
}

thread_local! {
    static STYLE_LOADED: Cell<bool> = const { Cell::new(false) };
}

/// Loads the embedded style sheet once per process (above the app's brand CSS).
fn load_style() {
    if STYLE_LOADED.with(Cell::get) {
        return;
    }
    let Some(display) = gdk::Display::default() else {
        return;
    };
    crate::brand::install();
    let provider = gtk4::CssProvider::new();
    provider.load_from_data(STYLE);
    gtk4::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
    STYLE_LOADED.with(|l| l.set(true));
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct ChatView {
        pub(super) inner: RefCell<Option<Rc<Inner>>>,
        pub(super) toasts: RefCell<Option<adw::ToastOverlay>>,
        pub(super) text_style: RefCell<Option<(gdk::Display, gtk4::CssProvider, String)>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ChatView {
        const NAME: &'static str = "AgentTerminalChatView";
        type Type = super::ChatView;
        type ParentType = gtk4::Box;
    }

    impl ObjectImpl for ChatView {
        fn dispose(&self) {
            if let Some((display, provider, _)) = self.text_style.borrow_mut().take() {
                gtk4::style_context_remove_provider_for_display(&display, &provider);
            }
            if let Some(inner) = self.inner.borrow_mut().take() {
                inner.composer.dispose();
                inner.disconnect_models();
            }
        }
    }
    impl WidgetImpl for ChatView {}
    impl BoxImpl for ChatView {}
}

glib::wrapper! {
    pub struct ChatView(ObjectSubclass<imp::ChatView>)
        @extends gtk4::Box, gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget, gtk4::Orientable;
}

/// What picking a background task shows.
enum BackgroundTarget {
    /// The sub-agent panel for this item.
    Subagent(String),
    /// The task's output, read from the agent.
    Output,
}

/// The view's state. Held strongly only by the widget; every handler holds a `Weak`.
pub(crate) struct Inner {
    backend: Rc<dyn ChatBackend>,
    model: Rc<RefCell<Transcript>>,
    transcript: Rc<TranscriptView>,
    composer: Rc<Composer>,
    header: Header,
    plan: PlanPanel,
    interruption: interruption::InterruptionShelf,
    /// The bouncing dots above the composer while the agent works.
    thinking: thinking::ThinkingStrip,
    requests: Rc<Requests>,
    /// Both agents' model lists for the picker (`None`: the backend's `ListModels`).
    models: RefCell<Option<Rc<dyn ModelSource>>>,
    /// The id of the view's listener on `models`, removed when the view goes away.
    model_conn: Cell<Option<u64>>,
    /// Account and plan usage: fed by this thread's `QuotaUpdated` events, shown in the header.
    account: RefCell<Option<Rc<AccountStatus>>>,
    usage: RefCell<Option<Rc<UsageIndicator>>>,
    /// The open model picker's refresh hook (see [`PanelCtx::model_listener`]).
    model_listener: ModelListener,
    actions: RefCell<Vec<ActionHandler>>,
    /// Where file-change cards get their diff and open it in the external tool.
    diffs: RefCell<Option<Rc<dyn DiffSource>>>,
    /// Items changed since the last flush (streaming deltas are coalesced per frame).
    dirty: RefCell<Vec<String>>,
    flush_queued: Cell<bool>,
    widget: glib::WeakRef<ChatView>,
    /// Where every card's events go (the transcript's and the sub-agent panel's).
    row_sink: RowSink,
    /// The header's list of the thread's sub-agents.
    subagent_button: Rc<subagents::SubagentButton>,
    /// The sticky card above the composer listing the sub-agents still at work.
    subagent_shelf: Rc<subagent_shelf::SubagentShelf>,
    /// The sub-agent shown on its own, while its dialog is open.
    subagent_panel: RefCell<Option<Rc<subagents::SubagentPanel>>>,
    /// Sub-agent items (and their steps) changed since the last flush.
    subagent_changes: RefCell<Vec<String>>,
    delegation: RefCell<delegation::Workers>,
    worker_inspector: RefCell<Option<Rc<delegation::Inspector>>>,
    worker_flush_queued: Cell<bool>,
    /// The thinking strip's list of background tasks.
    background_button: Rc<background::BackgroundButton>,
    /// The background shell task whose output is shown, while its dialog is open.
    task_panel: RefCell<Option<Rc<background::TaskOutputPanel>>>,
    /// The thread's folder: what the app's own `@` file index lists for agents that do not
    /// answer `Control::FileSuggestions` themselves.
    workspace: RefCell<Option<std::path::PathBuf>>,
    file_seq: Cell<u64>,
    /// The folder's file list, reused for a few seconds between keystrokes.
    file_index: RefCell<workspace_files::Index>,
    /// The agent process's [`crate::chat::Lifecycle`] as the header last showed it: an envelope
    /// that moves it (the agent answering, exiting) refreshes the header.
    shown_lifecycle: Cell<crate::chat::Lifecycle>,
}

impl ChatView {
    pub fn new(backend: Rc<dyn ChatBackend>) -> Self {
        load_style();
        let view: Self = glib::Object::builder()
            .property("orientation", gtk4::Orientation::Vertical)
            .build();
        view.add_css_class("chat-view");

        let inner = Rc::new_cyclic(|weak: &Weak<Inner>| {
            let w = weak.clone();
            let sink: RowSink = Rc::new(move |e| {
                if let Some(inner) = w.upgrade() {
                    inner.row_event(e);
                }
            });
            let w = weak.clone();
            let subagent_button = subagents::SubagentButton::new(move |id| {
                if let Some(inner) = w.upgrade() {
                    inner.open_subagent(id);
                }
            });
            let w = weak.clone();
            let w_cleared = weak.clone();
            let subagent_shelf = subagent_shelf::SubagentShelf::new(
                move |id| {
                    if let Some(inner) = w.upgrade() {
                        inner.open_subagent(id);
                    }
                },
                move || {
                    if let Some(inner) = w_cleared.upgrade() {
                        inner.refresh_subagents(&[]);
                    }
                },
            );
            let w = weak.clone();
            let background_button = background::BackgroundButton::new(move |task| {
                if let Some(inner) = w.upgrade() {
                    inner.open_background(task);
                }
            });
            let w = weak.clone();
            let interruption = interruption::InterruptionShelf::new(sink.clone(), move |id| {
                if let Some(inner) = w.upgrade() {
                    inner.transcript.focus_pending_card(&id);
                }
            });
            Inner {
                backend,
                model: Rc::new(RefCell::new(Transcript::new())),
                transcript: TranscriptView::new(sink.clone()),
                composer: Composer::new(),
                header: Header::new(),
                plan: PlanPanel::new(),
                interruption,
                thinking: thinking::ThinkingStrip::new(),
                requests: Rc::new(Requests::default()),
                models: RefCell::new(None),
                model_conn: Cell::new(None),
                account: RefCell::new(None),
                usage: RefCell::new(None),
                model_listener: ModelListener::default(),
                actions: RefCell::new(Vec::new()),
                diffs: RefCell::new(None),
                dirty: RefCell::new(Vec::new()),
                flush_queued: Cell::new(false),
                widget: view.downgrade(),
                row_sink: sink,
                subagent_button,
                subagent_shelf,
                subagent_panel: RefCell::new(None),
                subagent_changes: RefCell::new(Vec::new()),
                delegation: RefCell::new(delegation::Workers::default()),
                worker_inspector: RefCell::new(None),
                worker_flush_queued: Cell::new(false),
                background_button,
                task_panel: RefCell::new(None),
                workspace: RefCell::new(None),
                file_seq: Cell::new(0),
                file_index: RefCell::new(workspace_files::Index::default()),
                shown_lifecycle: Cell::new(crate::chat::Lifecycle::default()),
            }
        });

        inner.header.set_subagents(inner.subagent_button.widget());
        inner.thinking.append(inner.background_button.widget());
        view.append(&inner.header.bin);
        // Toasts cover the transcript only, never the shelves or the composer where the next
        // action is (see `ChatView::toast_overlay`).
        let toasts = adw::ToastOverlay::new();
        toasts.set_vexpand(true);
        toasts.set_child(Some(inner.transcript.widget()));
        view.append(&toasts);
        *view.imp().toasts.borrow_mut() = Some(toasts);
        let bottom = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        bottom.add_css_class("chat-bottom");
        let clamp = adw::Clamp::new();
        clamp.set_maximum_size(860);
        clamp.set_tightening_threshold(640);
        let column = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        column.append(&inner.plan.revealer);
        column.append(&inner.subagent_shelf.revealer);
        column.append(&inner.interruption.revealer);
        // Right above where the reply is typed: the one place the eye already is.
        column.append(&inner.thinking.revealer);
        column.append(inner.composer.widget());
        clamp.set_child(Some(&column));
        bottom.append(&clamp);
        view.append(&bottom);

        inner.transcript.connect_load_older(inner.model.clone());
        let host: Rc<dyn ComposerHost> = inner.clone();
        inner.composer.set_host(Rc::downgrade(&host));
        inner.connect_header();
        inner.install_shortcuts(view.upcast_ref());
        inner.refresh_status();
        inner.transcript.reset(&inner.model.borrow());

        *view.imp().inner.borrow_mut() = Some(inner);
        view
    }

    fn inner(&self) -> Option<Rc<Inner>> {
        self.imp().inner.borrow().clone()
    }

    /// Where this thread's toasts go: over the transcript, so one never covers the shelves or
    /// the composer.
    pub fn toast_overlay(&self) -> Option<adw::ToastOverlay> {
        self.imp().toasts.borrow().clone()
    }

    /// Applies one envelope from the backend.
    pub fn apply(&self, env: &Envelope) {
        if let Some(inner) = self.inner() {
            inner.apply(env);
        }
    }

    /// Bulk-loads stored history (thread open, restore): reduces every envelope into the model
    /// and then materialises only the newest window of rows, instead of building a widget per
    /// item. Control replies in history are dropped (nobody is waiting for them). Every envelope
    /// is credited to the thread's current agent; [`ChatView::replay_by_agent`] credits each to
    /// its own.
    pub fn replay(&self, envs: &[Envelope]) {
        self.replay_with(envs.iter().map(|e| (None, e)), envs.len());
    }

    /// [`ChatView::replay`] of stored history that names, per envelope, the agent that produced
    /// it (`None`: unknown, credited to the current agent). A thread that switched agents then
    /// shows each reply under its own agent and its "Continued in" dividers, as it did live.
    pub fn replay_by_agent(&self, envs: &[(Option<Driver>, Envelope)]) {
        self.replay_with(envs.iter().map(|(d, e)| (*d, e)), envs.len());
    }

    fn replay_with<'a>(
        &self,
        envs: impl Iterator<Item = (Option<Driver>, &'a Envelope)>,
        count: usize,
    ) {
        let Some(inner) = self.inner() else {
            return;
        };
        let started = std::time::Instant::now();
        let status = inner.backend.status();
        let current = status.driver;
        {
            let mut model = inner.model.borrow_mut();
            for (driver, env) in envs {
                model.apply(env, driver.unwrap_or(current));
            }
            // With no live agent behind the view (it is built before its session starts), what
            // the history left open can never finish: settle it now, before any row is built. A
            // session that starts (and resumes) afterwards emits fresh events of its own.
            if !status.alive && model.settle_stale() {
                tracing::info!("chat view: settled a stored turn left open");
            }
            // Reasoning loaded from the store says so.
            model.mark_replayed();
        }
        inner.dirty.borrow_mut().clear();
        inner.transcript.reset(&inner.model.borrow());
        let model = inner.model.borrow();
        inner.plan.set(&model.plan);
        inner.interruption.update(&model);
        inner.header.set_gauge(model.gauge.as_ref());
        if let Some(mode) = model.mode {
            inner.header.set_mode(mode);
        }
        inner.header.set_pending_mode(model.pending_mode);
        inner.header.set_extra_access(model.extra_access());
        inner.composer.set_history(model.user_messages());
        drop(model);
        inner.refresh_status();
        inner.refresh_subagents(&[]);
        tracing::info!(
            envelopes = count,
            ms = started.elapsed().as_millis() as u64,
            "chat view: replayed history"
        );
    }

    /// Where the backend delivers envelopes. Holds the view weakly.
    pub fn sink(&self) -> EnvelopeSink {
        let weak = self.downgrade();
        Rc::new(move |env: &Envelope| {
            if let Some(view) = weak.upgrade() {
                view.apply(env);
            }
        })
    }

    /// Called for every [`ViewAction`] (new thread, handoff, fork, rewind, compact fallback).
    pub fn connect_action(&self, f: impl Fn(&ViewAction) + 'static) {
        if let Some(inner) = self.inner() {
            inner.actions.borrow_mut().push(Rc::new(f));
        }
    }

    /// Tells the view the thread's folder, so `@` lists its files for agents that do not list
    /// them themselves (agy, Codex).
    pub fn set_workspace(&self, dir: &std::path::Path) {
        if let Some(inner) = self.inner() {
            *inner.workspace.borrow_mut() = Some(dir.to_path_buf());
        }
    }

    /// Gives file-change cards a source for their diffs and the external diff tool. Without one
    /// (the demo) "View diff" says there is none.
    pub fn set_diff_source(&self, source: Rc<dyn DiffSource>) {
        if let Some(inner) = self.inner() {
            *inner.diffs.borrow_mut() = Some(source);
        }
    }

    /// Feeds the model picker both agents' full model lists. Without a source the picker asks
    /// the backend (`Control::ListModels`), which lists only the current agent.
    pub fn set_model_source(&self, source: Rc<dyn ModelSource>) {
        let Some(inner) = self.inner() else { return };
        // One connection per source; the open picker (if any) is the listener. The hook is
        // cloned out of its cell first, so it may re-register or clear itself.
        let listener = inner.model_listener.clone();
        let id = source.connect_changed(Box::new(move || {
            let hook = listener.borrow().clone();
            if let Some(hook) = hook {
                hook();
            }
        }));
        // A source that was set before is let go of first.
        inner.disconnect_models();
        inner.model_conn.set(Some(id));
        *inner.models.borrow_mut() = Some(source);
    }

    /// Shows the usage indicator in the header, filtered to the thread's current agent, and
    /// feeds `status` from this thread's `QuotaUpdated` events.
    ///
    /// Contract: Claude reports its plan windows on every turn by itself. agy reports none
    /// during a turn, so the HOST should send `Control::Usage` after each agy `TurnCompleted`
    /// (its reply becomes a `QuotaUpdated` that lands here).
    pub fn set_account_status(&self, status: Rc<AccountStatus>) {
        let Some(inner) = self.inner() else { return };
        let driver = inner.backend.status().driver;
        let indicator = UsageIndicator::new(status.clone(), Some(driver));
        indicator.connect_repainted(inner.header.refitter());
        inner.header.set_usage(indicator.widget());
        *inner.account.borrow_mut() = Some(status);
        *inner.usage.borrow_mut() = Some(indicator);
    }

    /// Opens the model picker (what the header's agent chip does).
    pub fn open_model_picker(&self) {
        if let Some(inner) = self.inner() {
            inner.run_action(BuiltinAction::OpenModelPicker, "");
        }
    }

    pub fn focus_composer(&self) {
        if let Some(inner) = self.inner() {
            inner.composer.grab_focus();
        }
    }

    /// Whether the composer holds unsent text (a thread with one is not blank).
    pub fn has_draft(&self) -> bool {
        self.inner()
            .is_some_and(|inner| !inner.composer.text().trim().is_empty())
    }

    /// Puts `text` in the composer, as typing it would (tests).
    #[cfg(test)]
    pub(crate) fn set_draft(&self, text: &str) {
        if let Some(inner) = self.inner() {
            inner.composer.set_text(text);
        }
    }

    /// Stops the thread's agent process and starts it again on the same conversation (a
    /// recovery tool: Restart Agent in the window menu).
    pub fn restart_agent(&self) {
        if let Some(inner) = self.inner() {
            inner.backend.reload_session();
            inner.refresh_status();
        }
    }

    /// Goes to the newest message and follows the stream again.
    pub fn scroll_to_end(&self) {
        if let Some(inner) = self.inner() {
            inner.transcript.scroll_to_end();
        }
    }

    /// The thread's mode as its replayed history last set it (what the picker shows), so a
    /// reopened thread's session starts in it; `None` when the history never changed it.
    pub fn replayed_mode(&self) -> Option<Mode> {
        self.inner().and_then(|i| i.model.borrow().mode)
    }

    /// Re-reads [`ChatBackend::status`] into the header (after an external switch).
    pub fn refresh_status(&self) {
        if let Some(inner) = self.inner() {
            inner.refresh_status();
        }
    }

    /// Scale chat content and controls without changing the desktop theme or other windows.
    pub fn set_text_scale(&self, scale: f64) {
        let scale = if scale.is_finite() {
            scale.clamp(0.5, 3.0)
        } else {
            1.0
        };
        let display = self.display();
        let mut style = self.imp().text_style.borrow_mut();
        let (_, provider, class) = style.get_or_insert_with(|| {
            static NEXT_SCALE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let id = NEXT_SCALE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let class = format!("chat-scale-{id}");
            self.add_css_class(&class);
            let provider = gtk4::CssProvider::new();
            gtk4::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 2,
            );
            (display, provider, class)
        });
        provider.load_from_data(&format!(".{class} {{ font-size: {scale}em; }}"));
        if let Some(inner) = self.inner() {
            inner.header.set_text_scale(scale);
        }
    }
}

impl Inner {
    /// Removes the view's listener from the model source (app-wide objects must not keep a
    /// callback for a view that is gone).
    fn disconnect_models(&self) {
        let (source, id) = (self.models.borrow().clone(), self.model_conn.take());
        if let (Some(source), Some(id)) = (source, id) {
            source.disconnect(id);
        }
    }

    fn view(&self) -> Option<ChatView> {
        self.widget.upgrade()
    }

    fn panel_ctx(&self) -> Option<PanelCtx> {
        Some(PanelCtx {
            backend: self.backend.clone(),
            requests: self.requests.clone(),
            parent: self.view()?.upcast(),
            models: self.models.borrow().clone(),
            model_listener: self.model_listener.clone(),
        })
    }

    fn emit(&self, action: ViewAction) {
        let handlers: Vec<_> = self.actions.borrow().clone();
        if handlers.is_empty() {
            tracing::info!(?action, "chat view action has no handler yet");
        }
        for h in handlers {
            h(&action);
        }
    }

    fn apply(self: &Rc<Self>, env: &Envelope) {
        let driver = self.backend.status().driver;
        // Clone out of the cell: observers repaint widgets, which must not re-enter this borrow.
        let account = self.account.borrow().clone();
        if let Some(account) = account {
            account.observe(driver, env);
        }
        let changes = self.model.borrow_mut().apply(env, driver);
        self.handle(changes);
        if matches!(env.event, Event::TurnCompleted { .. }) && polls_context(driver) {
            self.poll_context_soon();
        }
        // Phase changes are discrete events: never walk the transcript for each text delta. The
        // agent's first answer (or its exit) ends its "Starting…" or "Resuming…".
        if self.backend.lifecycle() != self.shown_lifecycle.get()
            || matches!(
                env.event,
                Event::ItemStarted { .. }
                    | Event::ItemCompleted { .. }
                    | Event::ApprovalRequested { .. }
                    | Event::ApprovalResolved { .. }
                    | Event::ApprovalExpired
                    | Event::QuestionRequested { .. }
                    | Event::QuestionResolved { .. }
                    | Event::FormRequested { .. }
                    | Event::FormResolved { .. }
            )
        {
            self.refresh_status();
        }
    }

    fn handle(self: &Rc<Self>, changes: Vec<Change>) {
        // The shelf reads only approvals, questions and forms: a streamed delta (an assistant,
        // reasoning or tool item's update) or a new row of another kind never moves it, so
        // only other changes rescan the transcript for it.
        let shelf = {
            let model = self.model.borrow();
            changes.iter().any(|change| match change {
                Change::Added(id) | Change::Updated(id) => model.get(id).is_none_or(|item| {
                    matches!(
                        item.body,
                        model::Body::Approval(_) | model::Body::Question(_) | model::Body::Form(_)
                    )
                }),
                _ => true,
            })
        };
        // A sub-agent or one of its steps coming or changing moves the explorer: noted here,
        // refreshed with the next flush, once per frame (other items never touch it).
        {
            let model = self.model.borrow();
            let mut noted = self.subagent_changes.borrow_mut();
            for change in &changes {
                if let Change::Added(id) | Change::Updated(id) = change {
                    if model.in_subagent(id) && !noted.contains(id) {
                        noted.push(id.clone());
                    }
                }
            }
            if !noted.is_empty() {
                drop((noted, model));
                self.queue_flush();
            }
        }
        for change in changes {
            match change {
                Change::Added(id) => {
                    // Rows are built at once; a pending update for them is redundant.
                    self.dirty.borrow_mut().retain(|d| *d != id);
                    self.transcript.added(&self.model.borrow(), &id);
                }
                Change::Updated(id) => {
                    let mut dirty = self.dirty.borrow_mut();
                    if !dirty.contains(&id) {
                        dirty.push(id);
                    }
                    drop(dirty);
                    self.queue_flush();
                }
                Change::Removed(id) => {
                    self.dirty.borrow_mut().retain(|d| d != &id);
                    self.transcript.removed(&self.model.borrow(), &id);
                }
                Change::Plan => self.plan.set(&self.model.borrow().plan),
                Change::Gauge => self.header.set_gauge(self.model.borrow().gauge.as_ref()),
                Change::Mode => {
                    if let Some(mode) = self.model.borrow().mode {
                        self.header.set_mode(mode);
                    }
                    self.header
                        .set_pending_mode(self.model.borrow().pending_mode);
                }
                Change::Running => self.refresh_status(),
                Change::Workers => {
                    let ids: Vec<_> = self
                        .model
                        .borrow()
                        .subagents()
                        .into_iter()
                        .filter(|a| a.lifecycle.is_some())
                        .map(|a| a.id)
                        .collect();
                    self.refresh_subagents(&ids);
                }
                // The hint names the `$ skills` trigger only once the agent lists skills.
                Change::Commands => self.composer.refresh_placeholder(),
                Change::Control { request, result } => self.control_result(&request, result),
            }
        }
        // Model and agent changes arrive as several event kinds; the header is cheap to redo.
        self.refresh_agent_chip();
        self.header
            .set_extra_access(self.model.borrow().extra_access());
        if shelf {
            self.interruption.update(&self.model.borrow());
        }
    }

    fn queue_flush(self: &Rc<Self>) {
        if self.flush_queued.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(inner) = weak.upgrade() {
                inner.flush_queued.set(false);
                let ids: Vec<String> = std::mem::take(&mut *inner.dirty.borrow_mut());
                let model = inner.model.borrow();
                let mut seen = HashSet::new();
                for id in ids.iter().filter(|id| seen.insert(id.as_str())) {
                    inner.transcript.updated(&model, id);
                }
                drop(model);
                let changed = std::mem::take(&mut *inner.subagent_changes.borrow_mut());
                if !changed.is_empty() {
                    inner.refresh_subagents(&changed);
                }
            }
        });
    }

    /// The header's sub-agent list, and the open sub-agent panel (adding its new steps and
    /// updating the `changed` ones), from the model.
    fn refresh_subagents(&self, changed: &[String]) {
        let model = self.model.borrow();
        let own = model.subagents();
        let agents = Transcript::with_supervised(own.clone(), self.delegation.borrow().summaries());
        // Every sub-agent the thread started: the running ones first, then the finished ones
        // with how they ended (a replayed thread, or one whose sub-agents are all done, still
        // lists them).
        self.subagent_button.set(&subagents::running_first(&agents));
        self.subagent_shelf.set(&model, &agents);
        if let Some(panel) = self.subagent_panel.borrow().as_ref() {
            let summary = agents.iter().find(|a| a.id == panel.id());
            panel.refresh(&model, summary, changed);
        }
        drop(model);
        // A background sub-agent's row opens once its item is known.
        self.refresh_background(&own, &agents);
    }

    /// Expands every sub-agent card in the thread, or collapses them all when every one is
    /// already open (Ctrl+Shift+A). False when the thread has none.
    fn toggle_all_subagents(self: &Rc<Self>) -> bool {
        let (ids, expand) = {
            let model = self.model.borrow();
            let ids: Vec<String> = model.subagents().into_iter().map(|a| a.id).collect();
            let expand = ids
                .iter()
                .any(|id| model.get(id).is_some_and(|item| !item.expanded));
            (ids, expand)
        };
        if ids.is_empty() {
            return false;
        }
        for id in ids {
            self.row_event(RowEvent::Toggle {
                id,
                expanded: expand,
            });
        }
        true
    }

    /// The thread's keys for its pending approval and its sub-agent cards: active while focus is
    /// in this thread, in the capture phase so they reach it before the composer, and each lets
    /// its key through when there is nothing for it to do (Ctrl+Backspace still deletes a word,
    /// Ctrl+Enter is untouched, when no approval is waiting).
    fn install_shortcuts(self: &Rc<Self>, view: &gtk4::Widget) {
        let controller = gtk4::ShortcutController::new();
        controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        controller.set_scope(gtk4::ShortcutScope::Local);
        use agent_core::event::Decision;
        let approvals = [
            ("<Control>Return|<Control>KP_Enter", Decision::Allow),
            (
                "<Control><Shift>Return|<Control><Shift>KP_Enter",
                Decision::AllowForSession,
            ),
            (
                "<Control><Alt>Return|<Control><Alt>KP_Enter",
                Decision::AllowAlways,
            ),
            ("<Control>BackSpace", Decision::Deny),
            ("<Control>period", Decision::Cancel),
        ];
        for (keys, decision) in approvals {
            let weak = Rc::downgrade(self);
            let action = gtk4::CallbackAction::new(move |_, _| {
                let answered = weak
                    .upgrade()
                    .is_some_and(|inner| inner.interruption.answer(decision));
                if answered {
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
            controller.add_shortcut(gtk4::Shortcut::new(
                gtk4::ShortcutTrigger::parse_string(keys),
                Some(action),
            ));
        }
        let weak = Rc::downgrade(self);
        let toggle = gtk4::CallbackAction::new(move |_, _| {
            if weak
                .upgrade()
                .is_some_and(|inner| inner.toggle_all_subagents())
            {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        controller.add_shortcut(gtk4::Shortcut::new(
            gtk4::ShortcutTrigger::parse_string("<Control><Shift>a"),
            Some(toggle),
        ));
        view.add_controller(controller);
    }

    /// Shows one sub-agent on its own, in a dialog that follows it until closed.
    fn open_subagent(self: &Rc<Self>, id: &str) {
        if self.open_worker(id) {
            return;
        }
        let panel = subagents::SubagentPanel::new(id, self.row_sink.clone());
        let weak = Rc::downgrade(self);
        panel.set_refresher(move || {
            if let Some(inner) = weak.upgrade() {
                inner.refresh_subagents(&[]);
            }
        });
        {
            let model = self.model.borrow();
            let summary = model.subagents().into_iter().find(|a| a.id == id);
            panel.refresh(&model, summary.as_ref(), &[]);
        }
        let weak = Rc::downgrade(self);
        let shown = id.to_owned();
        adw::prelude::AdwDialogExt::connect_closed(panel.dialog(), move |_| {
            if let Some(inner) = weak.upgrade() {
                let mut open = inner.subagent_panel.borrow_mut();
                if open.as_ref().is_some_and(|p| p.id() == shown) {
                    *open = None;
                }
            }
        });
        self.present_dialog(panel.dialog());
        *self.subagent_panel.borrow_mut() = Some(panel);
    }

    /// Presents `dialog` on the view's window. False when the view is not in one (yet): a dialog
    /// needs a window to sit on.
    pub(super) fn present_dialog(&self, dialog: &adw::Dialog) -> bool {
        let Some(parent) = self.widget.upgrade().filter(|w| w.root().is_some()) else {
            return false;
        };
        adw::prelude::AdwDialogExt::present(dialog, Some(&parent));
        true
    }

    /// Where a background task leads: its sub-agent's panel, or its output (`None`: nothing).
    /// `own`: the model's [`Transcript::subagents`], computed once by the caller.
    fn background_target(
        &self,
        task: &BackgroundTask,
        own: &[model::SubagentSummary],
    ) -> Option<BackgroundTarget> {
        if task.kind == BackgroundTaskKind::Agent {
            // Claude's tasks name their `Agent` call; a provider worker is its own item.
            let id = task.tool_use_id.as_deref().unwrap_or(&task.id);
            let known = own.iter().any(|a| a.id == id) || self.delegation.borrow().has_worker(id);
            return known.then(|| BackgroundTarget::Subagent(id.to_owned()));
        }
        self.backend
            .status()
            .capabilities
            .task_output
            .then_some(BackgroundTarget::Output)
    }

    /// The strip's background task list, from the model. `own` and `agents`: the model's
    /// [`Transcript::subagents`] and [`Transcript::subagents_with`] the delegation registry's
    /// workers, which the caller has already computed.
    fn refresh_background(
        &self,
        own: &[model::SubagentSummary],
        agents: &[model::SubagentSummary],
    ) {
        let tasks = self.model.borrow().background_with(agents);
        let listed = tasks
            .into_iter()
            .map(|task| background::ListedTask {
                openable: self.background_target(&task, own).is_some(),
                task,
            })
            .collect();
        self.background_button.set(listed);
    }

    fn open_background(self: &Rc<Self>, task: &BackgroundTask) {
        let own = self.model.borrow().subagents();
        match self.background_target(task, &own) {
            Some(BackgroundTarget::Subagent(id)) => self.open_subagent(&id),
            Some(BackgroundTarget::Output) => self.open_task_output(task),
            None => {}
        }
    }

    /// Shows one background task's output in a dialog, read from the agent once a second until
    /// the dialog closes or the task has ended (then once more, for its last lines).
    fn open_task_output(self: &Rc<Self>, task: &BackgroundTask) {
        let panel = background::TaskOutputPanel::new(task);
        if !self.present_dialog(panel.dialog()) {
            return;
        }
        *self.task_panel.borrow_mut() = Some(panel.clone());
        if self.poll_task_output(&panel) == glib::ControlFlow::Break {
            return;
        }
        let weak = Rc::downgrade(self);
        let panel = Rc::downgrade(&panel);
        glib::timeout_add_local(std::time::Duration::from_secs(1), move || {
            match (weak.upgrade(), panel.upgrade()) {
                (Some(inner), Some(panel)) => inner.poll_task_output(&panel),
                _ => glib::ControlFlow::Break,
            }
        });
    }

    fn poll_task_output(&self, panel: &Rc<background::TaskOutputPanel>) -> glib::ControlFlow {
        if panel.closed() {
            let mut open = self.task_panel.borrow_mut();
            if open.as_ref().is_some_and(|p| Rc::ptr_eq(p, panel)) {
                *open = None;
            }
            return glib::ControlFlow::Break;
        }
        let running = self
            .model
            .borrow()
            .background_tasks()
            .iter()
            .any(|t| t.id == panel.task_id());
        if !running {
            panel.mark_ended();
        }
        if !panel.begin_read() {
            return glib::ControlFlow::Continue;
        }
        let weak = Rc::downgrade(panel);
        let control = Control::TaskOutput {
            task_id: panel.task_id().to_owned(),
        };
        self.requests
            .ask(self.backend.as_ref(), control, move |result| {
                if let Some(panel) = weak.upgrade() {
                    panel.show(result.map(|v| payload::task_output(&v)));
                }
            });
        if panel.ended() {
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    }

    fn control_result(&self, request: &str, result: Result<serde_json::Value, String>) {
        if self.composer.is_waiting_for(request) {
            let files = result
                .map(|v| payload::file_suggestions(&v))
                .unwrap_or_default();
            self.composer.file_reply(request, files);
            return;
        }
        self.requests.resolve(request, result);
    }

    /// Asks the agent for its context occupancy once this turn has unwound, so the gauge fills
    /// without the user opening `/context`. The reply is a `UsageUpdated` from the adapter; the
    /// request itself needs no answer here.
    fn poll_context_soon(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(inner) = weak.upgrade() else { return };
            let status = inner.backend.status();
            if !status.alive || !status.capabilities.context_usage {
                return;
            }
            inner
                .requests
                .ask(inner.backend.as_ref(), Control::ContextUsage, |_| {});
        });
    }

    fn refresh_status(&self) {
        let status = self.backend.status();
        let running = status.running_turn || self.model.borrow().running;
        // The header counts background work too, delegated workers included; the stop button
        // follows the main turn only.
        // Every sub-agent, walked out of the model once for everything below.
        let (activity, sub_agents, own, agents) = {
            let model = self.model.borrow();
            let workers = self.delegation.borrow();
            let own = model.subagents();
            let agents = Transcript::with_supervised(own.clone(), workers.summaries());
            // A worker waiting on the user holds up the thread as the agent's own request would;
            // an approval outranks a question, whoever asks.
            let activity = match (model.activity_among(running, &agents), workers.needs_user()) {
                (own @ model::Activity::NeedsApproval, _) => own,
                (_, Some(needs)) => needs,
                (own, None) => own,
            };
            let drivers = Transcript::running_drivers_among(&agents);
            (activity, drivers, own, agents)
        };
        // The agent's own start, resume or pause shows in the header only: the strip by the
        // composer keeps the turn's own activity.
        let lifecycle = self.backend.lifecycle();
        self.shown_lifecycle.set(lifecycle);
        self.header
            .set_activity(&activity.clone().with_lifecycle(lifecycle));
        let stop_note = status.capabilities.unsupported_note(
            agent_core::caps::Feature::InterruptKeepsProcess,
            status.driver.info().label,
        );
        self.thinking.set(&activity, status.driver, &sub_agents);
        self.refresh_background(&own, &agents);
        self.composer.set_running(running, stop_note.as_deref());
        self.refresh_agent_chip();
        self.header
            .set_mode(self.model.borrow().mode.unwrap_or(status.mode));
        self.header
            .set_pending_mode(self.model.borrow().pending_mode);
        self.header
            .gauge
            .set_sensitive(status.capabilities.context_usage);
        self.interruption.update(&self.model.borrow());
        self.composer.refresh_placeholder();
    }

    fn refresh_agent_chip(&self) {
        let status = self.backend.status();
        let model = status.model.clone().or_else(|| {
            status
                .pending_model
                .is_none()
                .then(|| self.model.borrow().current_model().map(str::to_owned))
                .flatten()
        });
        self.header.set_agent(
            status.driver,
            model.as_deref(),
            status.pending_model.as_deref(),
            status.effort.as_deref(),
        );
        // A new thread's empty transcript lights its agent's dot; a switch moves the light.
        self.transcript.set_agent(status.driver);
        // The composer's hint names the agent too: it must follow a switch, which arrives as
        // events and never goes through `refresh_status` (that only runs on running changes).
        self.composer.refresh_placeholder();
        let usage = self.usage.borrow().clone();
        if let Some(usage) = usage {
            usage.set_filter(Some(status.driver));
        }
    }

    fn connect_header(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.header.chip.connect_clicked(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.run_action(BuiltinAction::OpenModelPicker, "");
            }
        });
        let weak = Rc::downgrade(self);
        self.header.gauge.connect_clicked(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.run_action(BuiltinAction::OpenContext, "");
            }
        });
        let weak = Rc::downgrade(self);
        self.header.mode.connect_selected_notify(move |dd| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            if inner.header.mode_guard.get() {
                return;
            }
            let Some(mode) = MODES.get(dd.selected() as usize).copied() else {
                return;
            };
            // Never offered as a default: session-only, and only once confirmed.
            if mode == Mode::Bypass {
                inner.confirm_bypass();
                return;
            }
            inner.set_mode(mode, true);
        });
    }

    /// Asks before dangerous permissive mode, every time it is picked. Until the user agrees, the
    /// picker shows the mode still running; Cancel (the default, and Escape) changes nothing.
    fn confirm_bypass(self: &Rc<Self>) {
        self.refresh_status();
        let weak = Rc::downgrade(self);
        panels::confirm_bypass(&self.header.mode, self.backend.status().driver, move || {
            if let Some(inner) = weak.upgrade() {
                inner.set_mode(Mode::Bypass, false);
            }
        });
    }

    /// Asks the backend for `mode`; false when the agent has no such mode (nothing was asked).
    fn set_mode(self: &Rc<Self>, mode: Mode, header_pick: bool) -> bool {
        let status = self.backend.status();
        let caps = &status.capabilities;
        if mode == Mode::Plan && !caps.plan_mode {
            let changes = self
                .model
                .borrow_mut()
                .push_notice("This agent has no read-only planning mode.", Tone::Warning);
            self.handle(changes);
            self.refresh_status();
            return false;
        }
        if mode == Mode::Ask {
            if let Some(note) = caps.unsupported_note(
                agent_core::caps::Feature::LiveApprovals,
                self.backend.status().driver.info().label,
            ) {
                let changes = self.model.borrow_mut().push_notice(note, Tone::Warning);
                self.handle(changes);
                self.refresh_status();
                return false;
            }
        }
        if mode_change_asks_first(&status, mode) {
            self.refresh_status();
            let weak = Rc::downgrade(self);
            header::confirm_mode_switch(&self.header.mode, move || {
                let Some(inner) = weak.upgrade() else { return };
                // A dialog can outlive an agent switch. Never apply its choice to another agent.
                if inner.backend.status().driver == status.driver {
                    inner.apply_mode_choice(mode, header_pick);
                }
            });
            return false;
        }
        self.apply_mode_choice(mode, header_pick);
        true
    }

    fn apply_mode_choice(&self, mode: Mode, header_pick: bool) {
        self.backend.set_mode(mode);
        if header_pick {
            let driver = self.backend.status().driver;
            self.emit(ViewAction::ModeChosen { driver, mode });
        }
    }

    /// Takes the quick-reply chips away and hands focus back to the composer.
    fn dismiss_quick_replies(self: &Rc<Self>) {
        let changes = self.model.borrow_mut().dismiss_quick_replies();
        self.handle(changes);
        self.composer.grab_focus();
    }

    fn row_event(self: &Rc<Self>, e: RowEvent) {
        match e {
            RowEvent::Toggle { id, expanded } => {
                self.model.borrow_mut().set_expanded(&id, expanded);
                self.transcript.updated(&self.model.borrow(), &id);
                // The same card may be open in the sub-agent panel too.
                if self.model.borrow().in_subagent(&id) {
                    self.subagent_changes.borrow_mut().push(id);
                    self.queue_flush();
                }
            }
            RowEvent::Approve {
                request,
                decision,
                custom_rule,
            } => {
                let changes = self
                    .model
                    .borrow_mut()
                    .mark_approval_sent(&request, decision);
                self.handle(changes);
                self.backend
                    .respond_approval_with_rule(&request, decision, custom_rule);
                // The decision is made: typing goes back to the composer, not the spent card.
                self.composer.grab_focus();
            }
            RowEvent::Answer { request, answers } => {
                let changes = self.model.borrow_mut().mark_questions_sent(&request);
                self.handle(changes);
                self.backend.answer_questions(&request, answers);
                self.composer.grab_focus();
            }
            RowEvent::Form {
                request,
                action,
                content,
            } => {
                let changes = self.model.borrow_mut().mark_form_sent(&request);
                self.handle(changes);
                if self.backend.respond_form(&request, action, content) {
                    self.composer.grab_focus();
                } else {
                    // Refused: the card is answerable again, with a line saying so.
                    let changes = self.model.borrow_mut().mark_form_refused(&request);
                    self.handle(changes);
                }
            }
            RowEvent::QuickReply { text } => self.composer.fill(&text),
            RowEvent::DismissQuickReplies => self.dismiss_quick_replies(),
            RowEvent::StopQuickReplies => {
                self.dismiss_quick_replies();
                self.emit(ViewAction::StopQuickReplies);
            }
            RowEvent::LoadDiff { id } => self.load_diff(&id),
            RowEvent::OpenDiff { id } => {
                let source = self.diffs.borrow().clone();
                if let (Some(source), Some(ask)) = (source, self.diff_ask(&id)) {
                    source.open_external(ask);
                }
            }
        }
    }

    /// What a file-change card asks the diff source about: its id and tool input.
    fn diff_ask(&self, id: &str) -> Option<DiffAsk> {
        let model = self.model.borrow();
        let item = model.get(id)?;
        let model::Body::Tool(tool) = &item.body else {
            return None;
        };
        let input = match &tool.input {
            Some(v) if !v.is_null() => v.clone(),
            _ => serde_json::from_str(&tool.input_text).unwrap_or(serde_json::Value::Null),
        };
        Some(DiffAsk {
            item: id.to_owned(),
            input,
        })
    }

    /// A card's "View diff": asks the source and hands the answer back to that card.
    fn load_diff(self: &Rc<Self>, id: &str) {
        let source = self.diffs.borrow().clone();
        let (Some(source), Some(ask)) = (source, self.diff_ask(id)) else {
            self.show_diff(id, &Err("No diff is available for this edit.".to_owned()));
            return;
        };
        let (weak, id) = (Rc::downgrade(self), id.to_owned());
        source.load(
            ask,
            Box::new(move |reply| {
                if let Some(inner) = weak.upgrade() {
                    inner.show_diff(&id, &reply);
                }
            }),
        );
    }

    /// A computed diff goes to every copy of the card: the transcript's and, for a sub-agent's
    /// edit, the open panel's.
    fn show_diff(&self, id: &str, reply: &super::DiffReply) {
        self.transcript.show_diff(id, reply);
        if let Some(panel) = self.subagent_panel.borrow().as_ref() {
            panel.show_diff(id, reply);
        }
    }

    /// Runs a built-in locally. `args` is whatever followed the command name.
    fn run_action(self: &Rc<Self>, action: BuiltinAction, args: &str) {
        let status = self.backend.status();
        match action {
            BuiltinAction::OpenModelPicker => {
                if let Some(ctx) = self.panel_ctx() {
                    panels::model_picker(&ctx);
                }
            }
            BuiltinAction::OpenMcpPanel => {
                if let Some(ctx) = self.panel_ctx() {
                    panels::mcp_panel(&ctx);
                }
            }
            BuiltinAction::OpenSettings => {
                if let Some(ctx) = self.panel_ctx() {
                    panels::settings_panel(&ctx);
                }
            }
            BuiltinAction::OpenUsage => {
                if let Some(ctx) = self.panel_ctx() {
                    panels::usage_panel(&ctx);
                }
            }
            BuiltinAction::OpenContext => {
                if !status.capabilities.context_usage {
                    return;
                }
                let gauge = self.model.borrow().gauge;
                if let Some(ctx) = self.panel_ctx() {
                    panels::context_panel(&ctx, gauge);
                }
            }
            BuiltinAction::Compact => match compact_text(&status.capabilities) {
                Some(text) => self.backend.send_prompt(&text),
                None => {
                    let changes = self.model.borrow_mut().push_notice(
                        format!(
                            "{} has no compaction command, so the conversation continues in a \
                             new thread with a budgeted summary.",
                            status.driver.info().label
                        ),
                        Tone::Info,
                    );
                    self.handle(changes);
                    self.emit(ViewAction::CompactByHandoff);
                }
            },
            BuiltinAction::SetMode => match typeahead::parse_mode(args) {
                Some(Mode::Bypass) => self.confirm_bypass(),
                Some(mode) => {
                    self.set_mode(mode, false);
                }
                None => {
                    // No (or an unknown) argument: open the dropdown instead of guessing.
                    if !args.is_empty() {
                        let changes = self.model.borrow_mut().push_notice(
                            format!("Unknown mode “{args}”. Use plan, default or accept-edits."),
                            Tone::Warning,
                        );
                        self.handle(changes);
                    }
                    self.header.mode.activate();
                }
            },
            BuiltinAction::Handoff => {
                let target = typeahead::parse_driver(args);
                self.emit(ViewAction::Handoff { target });
            }
            BuiltinAction::Fork => self.emit(ViewAction::Fork),
            BuiltinAction::Rewind => self.emit(ViewAction::Rewind),
            BuiltinAction::NewThread => self.emit(ViewAction::NewThread),
            BuiltinAction::Help => {
                let changes = self.model.borrow_mut().push_notice(
                    help_text(&status.capabilities, status.driver.info().label),
                    Tone::Info,
                );
                self.handle(changes);
            }
        }
    }
}

/// Whether the host must ask for the context gauge itself after each turn. Codex pushes its
/// occupancy (`thread/tokenUsage/updated`); Claude and agy report it only when asked
/// (`get_context_usage`, agy's running token count), which left Claude's gauge empty.
fn polls_context(driver: Driver) -> bool {
    !matches!(driver, Driver::Codex)
}

/// The notice for a whole prompt that is a built-in this agent cannot back (`/mcp` on Codex):
/// what it lacks, as the greyed-control tooltip would say. `None` for anything else, including
/// a command the agent lists itself (that one is the agent's to run).
fn unavailable_builtin_note(text: &str, status: &SessionStatus) -> Option<String> {
    let name = text.trim().strip_prefix('/')?.split_whitespace().next()?;
    let (_, feature) = agent_core::commands::unavailable_builtin(name, &status.capabilities)?;
    let normalized = name.to_lowercase();
    if status.commands.iter().any(|c| c.name == normalized) {
        return None;
    }
    status
        .capabilities
        .unsupported_note(feature, status.driver.info().label)
}

/// The `/help` text: the built-ins this agent can back, then the ones it cannot (with why), then
/// the keys.
fn help_text(caps: &agent_core::caps::Capabilities, agent: &str) -> String {
    let mut s = String::from("Commands\n");
    for b in builtins().iter().filter(|b| b.available(caps)) {
        let hint = b.hint.map(|h| format!(" {h}")).unwrap_or_default();
        s.push_str(&format!("  /{}{hint} — {}\n", b.name, b.description));
    }
    let mut missing: Vec<String> = builtins()
        .iter()
        .filter(|b| !b.available(caps))
        .filter_map(|b| {
            let note = caps.unsupported_note(b.feature()?, agent)?;
            Some(format!("  /{} — {note}\n", b.name))
        })
        .collect();
    // Facts no command stands for (stop, @ files, compaction, reasoning, ...) are told too.
    let command_features: Vec<_> = builtins().iter().filter_map(|b| b.feature()).collect();
    missing.extend(
        agent_core::caps::Feature::ALL
            .iter()
            .filter(|f| !command_features.contains(*f))
            .filter_map(|f| caps.unsupported_note(*f, agent))
            .map(|note| format!("  {note}\n")),
    );
    if !missing.is_empty() {
        s.push_str("\nNot available here\n");
        missing.iter().for_each(|line| s.push_str(line));
    }
    s.push_str(
        "\nKeys\n  Enter sends · Shift+Enter adds a line · Esc stops the running turn\n  \
         / commands · @ files · $ skills · ↑↓ and Tab in the list",
    );
    s
}

impl ComposerHost for Inner {
    fn submit(&self, text: &str) {
        let status = self.backend.status();
        if let Some(cl) = typeahead::parse_command_line(text, &status.capabilities) {
            // Built-ins never reach the agent.
            if let Some(me) = self.view().and_then(|v| v.inner()) {
                me.run_action(cl.builtin.action, &cl.args);
            }
            return;
        }
        if let Some(note) = unavailable_builtin_note(text, &status) {
            // A built-in this agent cannot back: say so rather than send it as a prompt.
            if let Some(me) = self.view().and_then(|v| v.inner()) {
                let changes = me.model.borrow_mut().push_notice(note, Tone::Info);
                me.handle(changes);
            }
            return;
        }
        // What you just sent, and the reply to it, are where the view goes.
        self.transcript.scroll_to_end();
        // A prompt that starts a new round of work: the last one's finished workers leave the
        // dock. One sent mid-turn joins the running turn, whose finished workers stay.
        if !status.running_turn {
            self.subagent_shelf.clear_finished();
            self.refresh_subagents(&[]);
        }
        if status.running_turn && status.capabilities.live_input {
            // Straight to the agent, which takes it into the running turn.
            self.backend.steer_prompt(text);
        } else if status.running_turn {
            self.backend.queue_prompt(text);
        } else {
            self.backend.send_prompt(text);
        }
        // A paused agent starts again for this message: the header says "Resuming…" at once.
        if let Some(me) = self.view().and_then(|v| v.inner()) {
            me.refresh_status();
        }
    }

    fn interrupt(&self) {
        self.backend.interrupt();
    }

    fn running(&self) -> bool {
        self.backend.status().running_turn
    }

    fn run_builtin(&self, name: &str) {
        let Some(b) = builtins().iter().find(|b| b.name == name) else {
            return;
        };
        if let Some(me) = self.view().and_then(|v| v.inner()) {
            me.run_action(b.action, "");
        }
    }

    fn pop_queued_prompt(&self) -> Option<String> {
        let (id, text) = self.backend.pop_queued_prompt()?;
        let changes = self.model.borrow_mut().remove_item(&id);
        if let Some(me) = self.view().and_then(|v| v.inner()) {
            me.handle(changes);
        }
        Some(text)
    }

    fn command_items(&self, trigger: &Trigger) -> Vec<agent_core::commands::CompletionItem> {
        let status = self.backend.status();
        composer::items_for(trigger, &status.commands, &status.capabilities)
    }

    fn request_files(&self, query: &str) -> Option<String> {
        if self.backend.status().capabilities.file_suggestions {
            return Some(self.backend.control(Control::FileSuggestions {
                query: query.to_owned(),
            }));
        }
        // The agent lists no files: the app's own index of the thread's folder answers, off the
        // main thread. The reply goes through the same request id path as the agent's.
        let root = self.workspace.borrow().clone()?;
        let id = {
            self.file_seq.set(self.file_seq.get() + 1);
            format!("app-files-{}", self.file_seq.get())
        };
        let weak = self
            .view()
            .and_then(|v| v.inner())
            .map(|i| Rc::downgrade(&i))?;
        let (reply_id, query, seq) = (id.clone(), query.to_owned(), self.file_seq.get());
        glib::spawn_future_local(async move {
            // The folder's list is reused for a few seconds and only re-ranked per keystroke.
            // A newer query supersedes this one at each step, so stale work is dropped.
            let superseded = |inner: &Inner| inner.file_seq.get() != seq;
            // No strong reference is held across an await: the view may go away meanwhile.
            let cached = {
                let Some(inner) = weak.upgrade() else { return };
                let fresh = inner.file_index.borrow().fresh(&root);
                fresh
            };
            let list = match cached {
                Some(list) => list,
                None => {
                    let walked = root.clone();
                    let list = gtk4::gio::spawn_blocking(move || {
                        std::sync::Arc::new(workspace_files::list(&walked))
                    })
                    .await
                    .unwrap_or_default();
                    let Some(inner) = weak.upgrade() else { return };
                    inner.file_index.borrow_mut().store(&root, list.clone());
                    if superseded(&inner) {
                        return;
                    }
                    list
                }
            };
            let ranked = gtk4::gio::spawn_blocking(move || workspace_files::rank(&list, &query))
                .await
                .unwrap_or_default();
            if let Some(inner) = weak.upgrade() {
                if !superseded(&inner) {
                    inner.composer.file_reply(&reply_id, ranked);
                }
            }
        });
        Some(id)
    }

    fn placeholder(&self) -> String {
        // The question or form is above the composer, and typing is not the way to answer it.
        let waiting = match self.model.borrow().pending_interruption() {
            Some(model::PendingInterruption::Approval { .. }) => Some("Awaiting approval above..."),
            Some(model::PendingInterruption::Question { .. })
            | Some(model::PendingInterruption::Form { .. }) => {
                Some("Awaiting your answer above...")
            }
            None => None,
        };
        if let Some(waiting) = waiting {
            waiting.to_owned()
        } else {
            placeholder_text(&self.backend.status())
        }
    }
}

/// The composer's hint for the thread's CURRENT agent: its name and the triggers it backs.
fn placeholder_text(status: &SessionStatus) -> String {
    // `@` works for every agent: the agent's own list (Claude) or the app's index of the folder.
    let mut tips = vec!["/ commands", "@ files"];
    if status
        .commands
        .iter()
        .any(|c| c.kind == agent_core::event::AgentCommandKind::Skill)
    {
        tips.push("$ skills");
    }
    format!(
        "Message {}  ·  {}",
        status.driver.info().label,
        tips.join("  ·  ")
    )
}

// ---------------------------------------------------------------------------------------------
// Plan / todos
// ---------------------------------------------------------------------------------------------

/// The pinned plan panel above the composer.
struct PlanPanel {
    revealer: gtk4::Revealer,
    title: gtk4::Label,
    steps: gtk4::Box,
}

fn step_glyph(s: StepStatus) -> (&'static str, &'static str) {
    match s {
        StepStatus::Pending => ("○", "step-pending"),
        StepStatus::InProgress => ("◐", "step-active"),
        StepStatus::Completed => ("✓", "step-done"),
    }
}

impl PlanPanel {
    fn new() -> Self {
        let title = cards::label("Plan", &["plan-title"]);
        title.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        title.set_hexpand(true);
        let steps = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        steps.add_css_class("plan-steps");
        let card = cards::collapsible(
            "at-view-list-bullet-symbolic",
            &title,
            &steps,
            true,
            "the plan",
        )
        .card;
        let revealer = gtk4::Revealer::new();
        revealer.set_transition_type(gtk4::RevealerTransitionType::SlideUp);
        revealer.set_child(Some(&card));
        Self {
            revealer,
            title,
            steps,
        }
    }

    fn set(&self, plan: &[PlanStep]) {
        while let Some(c) = self.steps.first_child() {
            self.steps.remove(&c);
        }
        let done = plan
            .iter()
            .filter(|s| s.status == StepStatus::Completed)
            .count();
        let summary = format!("Plan  ·  {done} of {} done", plan.len());
        self.title.set_text(&summary);
        self.title.set_tooltip_text(Some(&summary));
        for step in plan {
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
            row.add_css_class("plan-step");
            let (glyph, class) = step_glyph(step.status);
            row.add_css_class(class);
            row.append(&cards::label(glyph, &["step-glyph"]));
            let text = cards::content_label(&step.text, &["step-text"]);
            // WordChar: one unbreakable word of a step must not set the thread's width.
            cards::wrapping(&text);
            row.append(&text);
            self.steps.append(&row);
        }
        self.revealer.set_reveal_child(model::plan_is_active(plan));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use agent_core::event::Decision;

    pub(super) use crate::testutil::native_init;

    /// Iterates the main loop until `cond` holds; false after 5 s.
    pub(super) fn pump_until(ctx: &glib::MainContext, cond: impl Fn() -> bool) -> bool {
        crate::testutil::pump_until(ctx, 5, cond)
    }

    pub(super) fn status_of(driver: Driver) -> SessionStatus {
        SessionStatus {
            driver,
            model: None,
            pending_model: None,
            effort: None,
            mode: Mode::Ask,
            running_turn: false,
            alive: true,
            capabilities: agent_core::caps::Capabilities::of(driver),
            commands: Vec::new(),
        }
    }

    #[test]
    fn hookless_agy_mode_change_asks_first() {
        for driver in Driver::ALL {
            let mut status = status_of(driver);
            status.mode = Mode::Plan;
            for running in [false, true] {
                status.running_turn = running;
                for hooked in [false, true] {
                    status.capabilities.live_approvals = hooked;
                    assert_eq!(
                        mode_change_asks_first(&status, Mode::AcceptEdits),
                        driver == Driver::Agy && running && !hooked
                    );
                    for unavailable_or_same in [Mode::Plan, Mode::Ask, Mode::Bypass] {
                        assert!(!mode_change_asks_first(&status, unavailable_or_same));
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "constructs GTK widgets; run on a private display"]
    fn hookless_agy_mode_confirmation_cancel_changes_nothing() {
        use adw::prelude::AdwWindowExt;
        gtk4::init().expect("GTK init");
        adw::init().expect("adw init");
        let backend = StubBackend::fixed(Driver::Agy);
        {
            let mut status = backend.status.borrow_mut();
            status.mode = Mode::Plan;
            status.running_turn = true;
            status.capabilities.live_approvals = false;
        }
        let view = ChatView::new(backend.clone());
        let window = adw::Window::new();
        window.set_content(Some(&view));
        window.present();
        let inner = view.imp().inner.borrow().clone().expect("inner");
        let actions = Rc::new(RefCell::new(Vec::new()));
        let log = actions.clone();
        view.connect_action(move |action| log.borrow_mut().push(action.clone()));
        assert!(!inner.set_mode(Mode::AcceptEdits, true));
        let dialog = window.visible_dialog().expect("confirmation");
        dialog.emit_by_name::<()>("response", &[&"cancel"]);
        assert_eq!(backend.status().mode, Mode::Plan);
        assert!(backend.status().running_turn);
        assert!(backend.modes.borrow().is_empty());
        assert!(actions.borrow().is_empty());
        window.close();
    }

    /// Transitions ease colour and opacity only: a transitioned size or spacing would relayout the
    /// transcript every frame and fight its stick-to-bottom. Every animation names a keyframes
    /// block that exists.
    #[test]
    fn motion_never_animates_layout() {
        // The value of every `property:` declaration, split into its comma-separated entries.
        let values = |property: &str| -> Vec<Vec<Vec<String>>> {
            let key = format!("{property}:");
            STYLE
                .match_indices(&key)
                // `transition:` must not also match inside `-transition:`-style longer names.
                .filter(|(at, _)| {
                    !STYLE[..*at].ends_with(|c: char| c.is_ascii_alphanumeric() || c == '-')
                })
                .map(|(at, _)| {
                    let start = at + key.len();
                    let end = STYLE[start..].find(';').map_or(STYLE.len(), |e| start + e);
                    STYLE[start..end]
                        .split(',')
                        .map(|entry| entry.split_whitespace().map(str::to_owned).collect())
                        .collect()
                })
                .collect()
        };
        let mut eased = 0;
        for property in ["transition-property", "transition"] {
            for entries in values(property) {
                eased += 1;
                for entry in &entries {
                    let animated = entry.first().map(String::as_str).unwrap_or("");
                    assert!(
                        animated != "all"
                            && !["height", "width", "margin", "padding"]
                                .iter()
                                .any(|layout| animated.contains(layout)),
                        "{property} on layout: {entry:?}"
                    );
                }
            }
        }
        assert!(eased > 0, "hover and state changes ease");
        for property in ["animation", "animation-name"] {
            for entries in values(property) {
                for entry in &entries {
                    let name = entry.first().map(String::as_str).unwrap_or("");
                    assert!(
                        STYLE.contains(&format!("@keyframes {name} ")),
                        "{property}: no @keyframes {name}"
                    );
                }
            }
        }
    }

    /// Every custom-drawn button class has a keyboard focus ring in the stylesheet.
    #[test]
    fn custom_buttons_have_a_focus_ring() {
        for class in [
            ".card-header",
            ".reasoning-toggle",
            ".plan-toggle",
            ".option-button",
            ".agent-chip",
            ".subagent-button",
            ".usage-indicator",
            ".gauge",
            ".code-copy",
            ".section-more",
            ".jump-button",
            ".send-button",
        ] {
            assert!(
                STYLE.contains(&format!("{class}:focus-visible")),
                "{class} has no :focus-visible rule"
            );
        }
    }

    #[test]
    fn the_placeholder_names_the_current_agent_and_its_triggers() {
        let claude = placeholder_text(&status_of(Driver::Claude));
        assert!(claude.starts_with("Message Claude"), "{claude}");
        assert!(claude.contains("@ files"));
        let agy = placeholder_text(&status_of(Driver::Agy));
        assert!(agy.starts_with("Message Antigravity"), "{agy}");
        assert!(!agy.contains("Claude"));
        assert!(placeholder_text(&status_of(Driver::Codex)).starts_with("Message Codex"));
    }

    struct PendingInitSession {
        session: Rc<crate::chat::session::ChatSession>,
        store: Rc<agent_kit::store::Store>,
        thread: String,
        events: Rc<RefCell<Vec<Envelope>>>,
    }

    fn pending_init_session() -> PendingInitSession {
        let store = Rc::new(agent_kit::store::Store::open_in_memory().expect("store"));
        let thread = store.create_thread("/", None).expect("thread");
        let events = Rc::new(RefCell::new(Vec::new()));
        let received = events.clone();
        let session = crate::chat::session::ChatSession::new(
            Box::new(agent_core::claude::ClaudeAdapter::new()),
            crate::testutil::open_session("/bin/cat", Vec::new(), "/"),
            store.clone(),
            thread.clone(),
            Rc::new(move |env| received.borrow_mut().push(env.clone())),
            None,
        );
        PendingInitSession {
            session,
            store,
            thread,
            events,
        }
    }

    struct ControlLog {
        session: Rc<crate::chat::session::ChatSession>,
        asked: RefCell<Vec<Control>>,
    }

    impl ChatBackend for ControlLog {
        fn send_prompt(&self, text: &str) {
            self.session.send_prompt(text);
        }
        fn interrupt(&self) {
            self.session.interrupt();
        }
        fn respond_approval(&self, request: &str, decision: Decision) {
            self.session.respond_approval(request, decision);
        }
        fn answer_questions(&self, request: &str, answers: serde_json::Value) {
            self.session.answer_questions(request, answers);
        }
        fn switch(&self, driver: Driver, model: Option<String>, effort: Option<String>) {
            self.session.switch(driver, model, effort);
        }
        fn reload_session(&self) {
            self.session.reload_session();
        }
        fn set_mode(&self, mode: Mode) {
            self.session.set_mode(mode);
        }
        fn control(&self, control: Control) -> String {
            self.asked.borrow_mut().push(control.clone());
            self.session.control(control)
        }
        fn status(&self) -> SessionStatus {
            self.session.status()
        }
    }

    #[test]
    fn an_unacknowledged_initialize_is_answered_on_shutdown() {
        crate::testutil::in_loop(|_| {
            let fixture = pending_init_session();
            fixture.session.shutdown(false);
            let replies: Vec<_> = fixture
                .events
                .borrow()
                .iter()
                .filter_map(|env| {
                    if let Event::ControlResult { ok, error } = &env.event {
                        Some((env.request.clone(), ok.clone(), error.clone()))
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(
                replies,
                [(
                    Some("init-1".to_owned()),
                    None,
                    Some("agent exited".to_owned())
                )]
            );
            assert!(!fixture
                .store
                .events(&fixture.thread, None, 1000)
                .expect("events")
                .iter()
                .any(|(_, env)| matches!(env.event, Event::Error { .. })));
        });
    }

    #[test]
    #[ignore = "constructs GTK widgets; run on a private display"]
    fn a_crash_does_not_store_a_context_poll_error() {
        gtk4::init().expect("GTK init");
        adw::init().expect("adw init");
        let context = glib::MainContext::default();
        let _guard = context.acquire().expect("main context");
        let fixture = pending_init_session();
        let backend = Rc::new(ControlLog {
            session: fixture.session.clone(),
            asked: RefCell::new(Vec::new()),
        });
        let view = ChatView::new(backend.clone());
        let inner = view.imp().inner.borrow().as_ref().expect("view").clone();
        // It was alive when the idle poll was scheduled, then died before the callback.
        inner.poll_context_soon();
        fixture.session.shutdown(false);
        let before = fixture
            .store
            .events(&fixture.thread, None, 1000)
            .expect("events")
            .len();
        while context.pending() {
            context.iteration(false);
        }
        // Shutdown answers the unacknowledged initialize handshake; that is not a context poll.
        assert!(fixture.events.borrow().iter().any(|env| env.request.as_deref() == Some("init-1")
            && matches!(&env.event, Event::ControlResult { error: Some(error), .. } if error == "agent exited")));
        assert!(
            !backend
                .asked
                .borrow()
                .iter()
                .any(|control| matches!(control, Control::ContextUsage)),
            "the dead process was never polled for context"
        );
        let stored = fixture
            .store
            .events(&fixture.thread, None, 1000)
            .expect("events");
        assert!(
            !stored
                .iter()
                .any(|(_, env)| matches!(env.event, Event::Error { .. })),
            "a crash must not store a context-poll error"
        );
        assert_eq!(stored.len(), before);
    }

    /// A backend for view tests: it records what the view asks of it (prompts, controls with
    /// distinct request ids, approvals, forms, modes) and answers as the test sets it.
    pub(super) struct StubBackend {
        pub(super) status: RefCell<SessionStatus>,
        /// Prompts sent, queued or steered.
        pub(super) sent: RefCell<Vec<String>>,
        pub(super) controls: RefCell<Vec<(String, Control)>>,
        pub(super) approvals: RefCell<Vec<(String, Decision)>>,
        pub(super) forms: RefCell<
            Vec<(
                String,
                agent_core::form::FormAction,
                Option<serde_json::Value>,
            )>,
        >,
        pub(super) modes: RefCell<Vec<Mode>>,
        /// Answer `false` to `respond_form`, as an adapter that refused it does.
        pub(super) refuse_form: Cell<bool>,
        /// A switch takes the new agent's status, as a session does.
        pub(super) switch_updates_status: Cell<bool>,
        /// Fail the test if questions are answered over the protocol.
        pub(super) panic_on_answer: Cell<bool>,
    }

    impl StubBackend {
        /// On `driver`; a switch changes its status.
        pub(super) fn new(driver: Driver) -> Rc<Self> {
            Self::with_status(status_of(driver))
        }

        /// With `status`; a switch changes it.
        pub(super) fn with_status(status: SessionStatus) -> Rc<Self> {
            Rc::new(Self {
                status: RefCell::new(status),
                sent: RefCell::default(),
                controls: RefCell::default(),
                approvals: RefCell::default(),
                forms: RefCell::default(),
                modes: RefCell::default(),
                refuse_form: Cell::new(false),
                switch_updates_status: Cell::new(true),
                panic_on_answer: Cell::new(false),
            })
        }

        /// On `driver` whatever is switched to.
        pub(super) fn fixed(driver: Driver) -> Rc<Self> {
            let backend = Self::new(driver);
            backend.switch_updates_status.set(false);
            backend
        }

        /// The request id of the first control `wanted` matches.
        pub(super) fn asked(&self, wanted: impl Fn(&Control) -> bool) -> Option<String> {
            self.controls
                .borrow()
                .iter()
                .find(|(_, c)| wanted(c))
                .map(|(id, _)| id.clone())
        }
    }

    impl ChatBackend for StubBackend {
        fn send_prompt(&self, text: &str) {
            self.sent.borrow_mut().push(text.to_owned());
        }
        fn interrupt(&self) {}
        fn respond_approval(&self, request: &str, decision: Decision) {
            self.approvals
                .borrow_mut()
                .push((request.to_owned(), decision));
        }
        fn answer_questions(&self, _: &str, _: serde_json::Value) {
            assert!(
                !self.panic_on_answer.get(),
                "a reply question is answered by a prompt, never over the protocol"
            );
        }
        fn respond_form(
            &self,
            request: &str,
            action: agent_core::form::FormAction,
            content: Option<serde_json::Value>,
        ) -> bool {
            self.forms
                .borrow_mut()
                .push((request.to_owned(), action, content));
            !self.refuse_form.get()
        }
        fn switch(&self, driver: Driver, _: Option<String>, _: Option<String>) {
            if self.switch_updates_status.get() {
                *self.status.borrow_mut() = status_of(driver);
            }
        }
        fn reload_session(&self) {}
        fn set_mode(&self, mode: Mode) {
            self.modes.borrow_mut().push(mode);
        }
        fn control(&self, control: Control) -> String {
            let id = format!("ctl-{}", self.controls.borrow().len() + 1);
            self.controls.borrow_mut().push((id.clone(), control));
            id
        }
        fn status(&self) -> SessionStatus {
            self.status.borrow().clone()
        }
    }

    /// Completion arrives while the turn is still running; history stays inspectable after
    /// reopening and subsequent interrupted/failed plans never become completed.
    #[test]
    #[ignore = "presents a window; run on a private display"]
    fn completed_plans_retire_to_history_and_new_turns_start_fresh() {
        use agent_core::event::{Event, TurnState};

        gtk4::init().expect("GTK init");
        adw::init().expect("adw init");
        let context = glib::MainContext::default();
        let until = |condition: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !condition() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "native plan condition timed out"
                );
                context.iteration(false);
            }
        };
        let plan = |status| {
            Envelope::new(Event::PlanUpdated {
                steps: vec![PlanStep {
                    text: "Inspect the synthetic plan history".into(),
                    status,
                }],
            })
        };
        let finished = |state| Envelope::turn_completed(state, None);
        let backend = StubBackend::new(Driver::Codex);
        let view = ChatView::new(backend.clone());
        let window = gtk4::Window::new();
        window.set_default_size(560, 700);
        window.set_child(Some(&view));
        window.present();
        until(&|| view.width() > 0);
        let inner = view.inner().expect("built view");
        let history_id = |status: StepStatus| {
            let model = inner.model.borrow();
            model
                .order()
                .iter()
                .find(|id| {
                    matches!(&model.get(id).expect("item").body,
                        model::Body::Plan { steps, .. } if steps[0].status == status)
                })
                .cloned()
                .expect("plan history row")
        };
        let events = [
            Envelope::new(Event::TurnStarted { model: None }),
            plan(StepStatus::Pending),
            plan(StepStatus::InProgress),
            plan(StepStatus::Completed),
            finished(TurnState::Completed),
        ];
        view.apply(&events[0]);
        for event in &events[1..3] {
            view.apply(event);
            assert!(inner.plan.revealer.reveals_child());
        }
        view.apply(&events[3]);
        assert!(inner.model.borrow().running, "completion precedes turn end");
        assert!(
            !inner.plan.revealer.reveals_child(),
            "completed shelf retires immediately"
        );
        let completed = history_id(StepStatus::Completed);
        inner.row_event(RowEvent::Toggle {
            id: completed.clone(),
            expanded: true,
        });
        until(&|| {
            inner
                .transcript
                .with_row(&completed, |row| row.widget().height() > 0)
                == Some(true)
        });
        assert!(
            inner
                .model
                .borrow()
                .get(&completed)
                .expect("completed plan")
                .expanded
        );
        let history = inner
            .transcript
            .with_row(&completed, |row| match row {
                cards::Row::Plan { root, steps, .. } => Some((
                    root.is_expanded(),
                    root.label().map(|text| text.to_string()),
                    steps
                        .first_child()
                        .and_downcast::<gtk4::Label>()
                        .map(|label| label.text().to_string()),
                )),
                _ => None,
            })
            .flatten()
            .expect("inspectable plan row");
        assert!(history.0, "history expands to expose step details");
        assert_eq!(history.1.as_deref(), Some("Completed plan"));
        assert_eq!(
            history.2.as_deref(),
            Some("Completed: Inspect the synthetic plan history")
        );
        let expander = inner
            .transcript
            .with_row(&completed, |row| match row {
                cards::Row::Plan { root, .. } => Some(root.clone()),
                _ => None,
            })
            .flatten()
            .expect("plan expander");
        // Invoke the widget's own notification path as a user expansion does, outside the
        // transcript accessor's borrow; it must persist the choice without re-entering GTK.
        for expanded in [false, true] {
            expander.set_expanded(expanded);
            assert_eq!(
                inner.model.borrow().get(&completed).expect("plan").expanded,
                expanded
            );
        }
        view.apply(&events[4]);
        backend.status.borrow_mut().alive = false;
        view.replay(&events);
        assert!(
            !inner.plan.revealer.reveals_child(),
            "reopen keeps the shelf retired"
        );
        let completed = history_id(StepStatus::Completed);
        assert!(inner.transcript.with_row(&completed, |_| ()).is_some());
        backend.status.borrow_mut().alive = true;

        for state in [TurnState::Interrupted, TurnState::Failed] {
            view.apply(&Envelope::new(Event::TurnStarted { model: None }));
            assert!(
                !inner.plan.revealer.reveals_child(),
                "new turn has no stale shelf"
            );
            view.apply(&plan(StepStatus::InProgress));
            assert!(inner.plan.revealer.reveals_child());
            view.apply(&finished(state));
            assert!(
                !inner.plan.revealer.reveals_child(),
                "finished turn retires incomplete work"
            );
            let history_order = inner.model.borrow().order().to_vec();
            view.apply(&plan(StepStatus::Pending));
            assert!(
                !inner.plan.revealer.reveals_child(),
                "late update cannot reopen the shelf"
            );
            assert_eq!(
                inner.model.borrow().order(),
                history_order,
                "late update cannot add history"
            );
            assert!(matches!(
                &inner.model.borrow().get(&completed).expect("completed history").body,
                model::Body::Plan { steps, .. } if steps[0].status == StepStatus::Completed
            ));
            let model = inner.model.borrow();
            let last = model
                .order()
                .iter()
                .rev()
                .find_map(|id| match &model.get(id)?.body {
                    model::Body::Plan { steps, outcome } => Some((steps, outcome)),
                    _ => None,
                })
                .expect("latest plan history");
            assert_eq!(last.0[0].status, StepStatus::InProgress);
            assert_eq!(*last.1, Some(state));
        }
        window.close();
    }

    /// A thread stored mid-turn, reopened: replayed before its session starts (no live agent),
    /// it must show no phantom running turn, no answerable card and no background work.
    fn stale_replay_checks() {
        use agent_core::event::{
            BackgroundTask, BackgroundTaskKind, Event, ItemKind, ResponseCapability,
        };
        let mut dead = status_of(Driver::Claude);
        dead.alive = false;
        let view = ChatView::new(StubBackend::with_status(dead));
        view.replay(&[
            Envelope::new(Event::TurnStarted { model: None }),
            Envelope::new(Event::ApprovalRequested {
                tool: "Bash".into(),
                title: None,
                input: serde_json::json!({"command": "ls"}),
                reason: None,
                options: vec![Decision::Allow, Decision::Deny],
                response: ResponseCapability::Live,
                remembers: None,
            })
            .request("r1"),
            Envelope::item_started(
                "agent1",
                ItemKind::Subagent,
                "Agent",
                Some(serde_json::json!({"subagent_type": "Explore"})),
                None,
            ),
            Envelope::new(Event::BackgroundTasks {
                tasks: vec![BackgroundTask {
                    id: "t1".into(),
                    kind: BackgroundTaskKind::Agent,
                    description: Some("Explore".into()),
                    tool_use_id: Some("agent1".into()),
                }],
            }),
        ]);
        let inner = view.inner().expect("view");
        {
            let model = inner.model.borrow();
            assert!(!model.running, "no phantom running turn");
            assert!(model.background.is_empty(), "no stale background work");
            match &model.get("approval:r1").expect("card").body {
                model::Body::Approval(a) => {
                    assert_eq!(a.state, model::ApprovalState::Expired);
                }
                other => panic!("not an approval: {other:?}"),
            }
            match &model.get("agent1").expect("agent").body {
                model::Body::Tool(t) => assert_eq!(t.status, model::ToolStatus::Interrupted),
                other => panic!("not a tool: {other:?}"),
            }
        }
        assert!(!inner.header.running_shown());
        assert_eq!(inner.header.activity_shown().as_deref(), Some("Stopped"));
        assert!(!inner.composer.shows_stop());
        let card = inner
            .transcript
            .with_row("approval:r1", |row| match row {
                cards::Row::Approval(a) => Some(a.actionable()),
                _ => None,
            })
            .flatten()
            .expect("approval row");
        assert!(
            !card.0 && !card.1,
            "the stale card offers no buttons: {card:?}"
        );
        assert_eq!(
            inner.subagent_button.statuses(),
            ["stopped"],
            "the sub-agent is not listed as running"
        );
    }

    /// Live, your message and the first row after it fade in, and later rows just appear;
    /// opening the thread again (a replay) builds every row without an entrance.
    fn live_rows_make_an_entrance_and_replayed_rows_do_not() {
        use agent_core::event::ItemKind;
        let started = |id: &str, kind: ItemKind| Envelope::item_started(id, kind, "", None, None);
        let envs = [
            started("u1", ItemKind::UserMessage),
            started("t1", ItemKind::Command),
            started("t2", ItemKind::Command),
        ];
        let backend = StubBackend::new(Driver::Claude);
        let entering = |view: &ChatView, id: &str| {
            view.inner()
                .and_then(|i| {
                    i.transcript
                        .with_row(id, |r| r.widget().has_css_class("row-enter"))
                })
                .unwrap_or(false)
        };
        let live = ChatView::new(backend.clone());
        for env in &envs {
            live.apply(env);
        }
        assert!(entering(&live, "u1"), "your message fades in");
        assert!(entering(&live, "t1"), "so does the reply's first row");
        assert!(!entering(&live, "t2"), "the rest of the turn just appears");
        let replayed = ChatView::new(backend);
        replayed.replay(&envs);
        for id in ["u1", "t1", "t2"] {
            assert!(!entering(&replayed, id), "{id} replayed with an entrance");
        }
    }

    /// An approval's card sits right above the command it let run (whose card started first),
    /// so the command's result reads after "Allowed", not before it.
    fn approval_row_precedes_its_step() {
        use agent_core::event::{Event, ItemKind, ResponseCapability};
        let backend = StubBackend::new(Driver::Claude);
        let view = ChatView::new(backend);
        view.apply(&Envelope::item_started(
            "t1",
            ItemKind::Command,
            "Bash",
            Some(serde_json::json!({"command": "cargo test"})),
            None,
        ));
        view.apply(
            &Envelope::new(Event::ApprovalRequested {
                tool: "Bash".into(),
                title: None,
                input: serde_json::json!({"command": "cargo test"}),
                reason: None,
                options: vec![Decision::Allow, Decision::Deny],
                response: ResponseCapability::Live,
                remembers: None,
            })
            .item("t1")
            .request("r1"),
        );
        let inner = view.inner().expect("a built view");
        let step = inner
            .transcript
            .with_row("t1", |r| r.widget())
            .expect("the command's row");
        let next = inner
            .transcript
            .with_row("approval:r1", |r| r.widget().next_sibling())
            .flatten();
        assert_eq!(
            next.as_ref(),
            Some(&step),
            "the approval sits right above its step"
        );
    }

    /// The dots above the composer show while a turn runs, name the agent, and go when it ends.
    fn thinking_strip_follows_the_turn() {
        use agent_core::event::{Event, TurnState};
        let backend = StubBackend::new(Driver::Claude);
        let view = ChatView::new(backend.clone());
        let inner = view.inner().expect("a built view");
        assert_eq!(inner.thinking.shown(), None, "a new thread shows nothing");
        // The empty transcript's dots light the thread's agent, and follow a switch.
        assert_eq!(inner.transcript.empty_lead().as_deref(), Some("claude"));
        backend.switch(Driver::Codex, None, None);
        view.sink()(&Envelope::new(Event::ModelChanged {
            model: "gpt-5-codex".into(),
        }));
        assert_eq!(inner.transcript.empty_lead().as_deref(), Some("codex"));
        backend.switch(Driver::Claude, None, None);
        view.sink()(&Envelope::new(Event::ModelChanged {
            model: "opus".into(),
        }));
        view.apply(&Envelope::new(Event::TurnStarted { model: None }));
        assert_eq!(
            inner.thinking.shown().as_deref(),
            Some("Claude is working…")
        );
        // The agent at work leads the wave; the other two keep it company, dimmed.
        assert_eq!(inner.thinking.dot_states(), ["lead", "dimmed", "dimmed"]);
        view.apply(&Envelope::turn_completed(TurnState::Completed, None));
        assert_eq!(inner.thinking.shown(), None, "gone once the turn ends");
    }

    fn model_chip_follows_pending_acceptance_and_rejection() {
        let mut status = status_of(Driver::Claude);
        status.model = Some("sonnet".into());
        let backend = StubBackend::with_status(status);
        let view = ChatView::new(backend.clone());
        let chip = view.inner().expect("view").header.chip.clone();
        let tooltip = || chip.tooltip_text().expect("model tooltip").to_string();
        view.sink()(&Envelope::new(Event::ModelChanged {
            model: "sonnet".into(),
        }));
        assert_eq!(
            tooltip(),
            "Claude · sonnet · Switch model or agent in this thread; stops the current turn (/model)"
        );

        backend.status.borrow_mut().pending_model = Some("opus".into());
        view.sink()(&Envelope::new(Event::ModelChangeRequested {
            model: "opus".into(),
        }));
        assert_eq!(
            tooltip(),
            "Claude · sonnet → opus (pending) · Switch model or agent in this thread; stops the current turn (/model)"
        );
        assert!(chip.has_css_class("pending"), "a pending switch pulses");
        {
            let mut status = backend.status.borrow_mut();
            status.model = Some("opus".into());
            status.pending_model = None;
            status.effort = Some("high".into());
        }
        view.sink()(&Envelope::new(Event::ModelChanged {
            model: "opus".into(),
        }));
        assert_eq!(
            tooltip(),
            "Claude · opus · high · Switch model or agent in this thread; stops the current turn (/model)",
            "the chip names the effort, which has no other place to show"
        );
        assert!(!chip.has_css_class("pending"), "a settled switch rests");

        backend.status.borrow_mut().effort = None;
        backend.status.borrow_mut().pending_model = Some("unavailable".into());
        view.sink()(&Envelope::new(Event::ModelChangeRequested {
            model: "unavailable".into(),
        }));
        assert!(tooltip().contains("opus → unavailable (pending)"));
        backend.status.borrow_mut().pending_model = None;
        view.sink()(&Envelope::new(Event::ModelChangeFailed {
            model: "unavailable".into(),
            message: "Model is unavailable".into(),
        }));
        assert_eq!(
            tooltip(),
            "Claude · opus · Switch model or agent in this thread; stops the current turn (/model)"
        );

        // The reducer still remembers Claude's model while the replacement provider starts.
        // A pending Codex selection must not make that old model look like a Codex model.
        {
            let mut status = backend.status.borrow_mut();
            *status = status_of(Driver::Codex);
            status.pending_model = Some("gpt-5-codex".into());
        }
        view.sink()(&Envelope::new(Event::ModelChangeRequested {
            model: "gpt-5-codex".into(),
        }));
        assert_eq!(
            tooltip(),
            "Codex · gpt-5-codex (pending) · Switch model or agent in this thread; stops the current turn (/model)"
        );
        {
            let mut status = backend.status.borrow_mut();
            status.model = Some("gpt-5-codex".into());
            status.pending_model = None;
        }
        view.sink()(&Envelope::new(Event::ModelChanged {
            model: "gpt-5-codex".into(),
        }));
        assert_eq!(
            tooltip(),
            "Codex · gpt-5-codex · Switch model or agent in this thread; stops the current turn (/model)"
        );
    }

    /// The whole view, header (usage meter, sub-agent counter), shelves and the demo
    /// transcript: no selectable label sits in a button or other click target.
    fn whole_view_has_no_dead_click_targets() {
        let backend = demo::demo_backend();
        let view = ChatView::new(backend.clone());
        backend.connect(view.sink());
        view.set_account_status(demo::demo_account_status());
        view.replay(&demo::DemoBackend::script_envelopes());
        let problems = crate::testutil::selectable_labels_in_click_targets(view.upcast_ref());
        assert!(problems.is_empty(), "dead click targets: {problems:#?}");
        let missing = crate::testutil::blocks_without_copy(view.upcast_ref());
        assert!(missing.is_empty(), "blocks with no Copy: {missing:#?}");
        // 4.5: every button is named for assistive tech and is a usable target.
        let problems = crate::testutil::button_problems(view.upcast_ref(), 24);
        assert!(problems.is_empty(), "button problems: {problems:#?}");
        // The walk saw the targets it guards.
        let all = crate::testutil::descendants(view.upcast_ref());
        let text = |t: &str| {
            all.iter()
                .filter_map(|w| w.downcast_ref::<gtk4::Label>())
                .any(|l| l.text().contains(t))
        };
        assert!(
            text("Thought") || text("Thinking"),
            "reasoning toggle built"
        );
    }

    /// Forms and permission requests through the whole view: the shelf, the thread's status,
    /// the composer's hint, the answer reaching the backend, and the "Extra access" chip.
    pub(crate) fn requests_ui_checks() {
        use agent_core::form::{FieldInput, Form, FormAction, FormField};
        use serde_json::json;

        let backend = StubBackend::fixed(Driver::Codex);
        let view = ChatView::new(backend.clone());
        let inner = view.inner().expect("inner");
        let hint = || inner.composer.placeholder_text();
        let form_row = |id: &str| {
            inner.transcript.with_row(id, |row| match row {
                cards::Row::Form(card) => Some((card.entry(0), card.status(), card.buttons())),
                _ => None,
            })
        };

        view.apply(
            &Envelope::new(Event::FormRequested {
                server: "github".into(),
                message: "Which repo?".into(),
                form: Form::Fields {
                    fields: vec![FormField {
                        name: "repo_name".into(),
                        title: None,
                        description: None,
                        required: true,
                        input: FieldInput::Text {
                            format: None,
                            min_length: None,
                            max_length: None,
                            default: None,
                        },
                    }],
                },
            })
            .request("f"),
        );
        assert!(
            inner.interruption.revealer.reveals_child(),
            "a form is a shelf entry"
        );
        assert_eq!(
            inner.header.activity_shown().as_deref(),
            Some("Needs your answer")
        );
        assert!(hint().starts_with("Awaiting your answer"), "{}", hint());
        let (entry, status, buttons) = form_row("form:f").flatten().expect("form card");
        assert_eq!(status, "Fill in Repo name");
        assert_eq!(buttons, ["Submit", "Decline"]);
        let entry = entry.expect("entry");
        entry.emit_activate();
        assert!(backend.forms.borrow().is_empty(), "invalid: nothing sent");
        entry.set_text("octo");
        // An answer the adapter refuses must not strand the card at "Sending…".
        backend.refuse_form.set(true);
        entry.emit_activate();
        let ctx = glib::MainContext::default();
        while ctx.iteration(false) {}
        assert_eq!(backend.forms.borrow().len(), 1, "it was tried");
        let refusal = inner
            .transcript
            .with_row("form:f", |row| match row {
                cards::Row::Form(card) => Some((card.refusal(), card.buttons())),
                _ => None,
            })
            .flatten()
            .expect("form card");
        assert_eq!(
            refusal.0.as_deref(),
            Some("Codex did not accept that answer. Try again, or decline.")
        );
        assert_eq!(refusal.1, ["Submit", "Decline"], "answerable again");
        assert!(
            inner.interruption.revealer.reveals_child(),
            "still waiting on the user"
        );
        backend.refuse_form.set(false);
        entry.emit_activate();
        assert_eq!(
            backend.forms.borrow()[1],
            (
                "f".to_owned(),
                FormAction::Accept,
                Some(json!({"repo_name": "octo"}))
            )
        );
        assert!(
            !inner.interruption.revealer.reveals_child(),
            "answered: the shelf lets go"
        );
        view.apply(
            &Envelope::new(Event::FormResolved {
                action: FormAction::Accept,
            })
            .request("f"),
        );
        let ctx = glib::MainContext::default();
        while ctx.iteration(false) {}
        let (_, status, buttons) = form_row("form:f").flatten().expect("form card");
        assert_eq!((status.as_str(), buttons.len()), ("Submitted", 0));
        assert!(hint().starts_with("Message Codex"), "{}", hint());

        // A permission request answered for the session: one click, no rule, and a chip.
        view.apply(
            &Envelope::new(Event::ApprovalRequested {
                tool: "codex_permissions".into(),
                title: Some("Codex asks to write to 1 place".into()),
                input: json!({
                    "cwd": "/w", "reason": null, "renderable": true,
                    "entries": [{"access": "write", "target": {"kind": "path", "path": "/w/out"}}],
                    "network": null, "session_allowed": true, "warning": null, "requested": {}
                }),
                reason: None,
                options: vec![
                    Decision::Allow,
                    Decision::AllowForSession,
                    Decision::Deny,
                    Decision::Cancel,
                ],
                response: agent_core::event::ResponseCapability::Live,
                remembers: None,
            })
            .request("p"),
        );
        assert!(
            inner.header.extra_access_shown().is_none(),
            "nothing granted yet"
        );
        assert!(!interruption::placed(&inner.interruption.allow_btn));
        let pressed = inner.transcript.with_row("approval:p", |row| match row {
            cards::Row::Approval(card) => Some(card.press("Allow for this session")),
            _ => None,
        });
        assert_eq!(pressed, Some(Some(true)));
        assert_eq!(
            *backend.approvals.borrow(),
            [("p".to_owned(), Decision::AllowForSession)]
        );
        view.apply(
            &Envelope::new(Event::ApprovalResolved {
                decision: Decision::AllowForSession,
            })
            .request("p"),
        );
        let (list, tip) = inner.header.extra_access_shown().expect("the chip shows");
        assert_eq!(list, "Write: /w/out");
        assert!(tip.contains("Write: /w/out"), "{tip}");
        view.apply(&Envelope::new(Event::SessionStarted {
            native_id: "n2".into(),
            model: None,
            cwd: None,
        }));
        assert!(
            inner.header.extra_access_shown().is_none(),
            "a new Codex process forgets the grant"
        );
    }

    /// GTK checks, run from the window test (GTK belongs to the one thread that initialised it).
    /// Ctrl+Shift+A: every sub-agent card opens when any is closed, then all close; a thread
    /// with none lets the key through.
    fn all_subagent_cards_toggle_together() {
        use agent_core::event::ItemKind;
        let view = ChatView::new(StubBackend::new(Driver::Claude));
        let inner = view.inner().expect("view");
        assert!(
            !inner.toggle_all_subagents(),
            "no sub-agents: the key goes through"
        );
        let agent = |id: &str| {
            Envelope::item_started(
                id,
                ItemKind::Subagent,
                "Agent",
                Some(serde_json::json!({"subagent_type": "Explore"})),
                None,
            )
        };
        view.replay(&[agent("a1"), agent("a2")]);
        let expanded = |inner: &Inner| {
            let model = inner.model.borrow();
            ["a1", "a2"].map(|id| model.get(id).is_some_and(|i| i.expanded))
        };
        // Sub-agent cards start open: the first press closes them all, the next opens them all.
        assert_eq!(expanded(&inner), [true, true]);
        assert!(inner.toggle_all_subagents());
        assert_eq!(expanded(&inner), [false, false], "all closed");
        assert!(inner.toggle_all_subagents());
        assert_eq!(expanded(&inner), [true, true], "all open again");
        // Mixed: one closed by hand, so the key opens the rest rather than closing the open one.
        inner.row_event(RowEvent::Toggle {
            id: "a1".into(),
            expanded: false,
        });
        assert!(inner.toggle_all_subagents());
        assert_eq!(expanded(&inner), [true, true], "any closed: open all");
    }

    /// A replay builds the newest rows at once and the rest of the window on idle, around a row
    /// streamed in meanwhile; jumping to a row not built yet builds the window first.
    fn replay_fills_the_window_in_steps() {
        use agent_core::event::ItemKind;
        use transcript::{FIRST_PAINT, WINDOW};
        let step = |id: &str| {
            Envelope::item_started(id, ItemKind::Command, format!("echo {id}"), None, None)
        };
        let history: Vec<Envelope> = (0..WINDOW + 50).map(|i| step(&format!("s{i}"))).collect();
        let pump = || {
            let ctx = glib::MainContext::default();
            while ctx.iteration(false) {}
        };

        let view = ChatView::new(StubBackend::new(Driver::Claude));
        let inner = view.inner().expect("view");
        view.replay(&history);
        assert_eq!(inner.transcript.materialised(), FIRST_PAINT);
        view.apply(&step("live"));
        pump();
        assert_eq!(inner.transcript.materialised(), WINDOW, "the window filled");
        let built = |id: &str| inner.transcript.with_row(id, |_| ()).is_some();
        assert!(
            built("live") && built("s51"),
            "the newest window, live row included"
        );
        assert!(!built("s50"), "nothing older than the window");

        let view = ChatView::new(StubBackend::new(Driver::Claude));
        let inner = view.inner().expect("view");
        view.replay(&history);
        inner.transcript.scroll_to_card("s60");
        assert!(
            inner.transcript.with_row("s60", |_| ()).is_some(),
            "a jump builds the rest of the window first"
        );
        assert_eq!(inner.transcript.materialised(), WINDOW);
        pump();
        assert_eq!(
            inner.transcript.materialised(),
            WINDOW,
            "and the fill is over"
        );

        // A reset while a fill is pending cancels it and starts over; a view dropped mid-fill
        // leaves nothing behind that runs.
        let view = ChatView::new(StubBackend::new(Driver::Claude));
        let inner = view.inner().expect("view");
        view.replay(&history);
        inner.transcript.reset(&inner.model.borrow());
        assert_eq!(inner.transcript.materialised(), FIRST_PAINT);
        pump();
        assert_eq!(inner.transcript.materialised(), WINDOW);
        let dropped = ChatView::new(StubBackend::new(Driver::Claude));
        dropped.replay(&history);
        drop(dropped);
        pump();
    }

    /// A jump to a card the fill has not built yet lands on it once it is laid out, and the
    /// fill's "keep my place" does not move the view away afterwards. Run on a private display.
    #[test]
    #[ignore = "presents a window; run on a private display"]
    fn a_jump_to_an_unbuilt_card_lands_on_it() {
        use agent_core::event::ItemKind;
        let ctx = crate::testutil::native_init();
        let view = ChatView::new(StubBackend::new(Driver::Claude));
        let window = gtk4::Window::new();
        window.set_default_size(560, 460);
        window.set_child(Some(&view));
        window.present();
        let inner = view.inner().expect("view");
        let history: Vec<Envelope> = (0..transcript::WINDOW + 50)
            .map(|i| {
                Envelope::item_started(
                    format!("s{i}"),
                    ItemKind::Command,
                    format!("echo s{i}"),
                    None,
                    None,
                )
            })
            .collect();
        view.replay(&history);
        assert!(
            inner.transcript.with_row("s60", |_| ()).is_none(),
            "s60 is in the window but not built yet"
        );
        inner.transcript.scroll_to_card("s60");
        let top = || {
            inner
                .transcript
                .row_bounds_in_viewport("s60")
                .map(|(bounds, _)| bounds.y())
        };
        let pump_for = |ms: u64| {
            let until = std::time::Instant::now() + std::time::Duration::from_millis(ms);
            while std::time::Instant::now() < until {
                while ctx.iteration(false) {}
                std::thread::sleep(std::time::Duration::from_millis(16));
            }
        };
        let landed = |y: Option<f32>| y.is_some_and(|y| (0.0..=120.0).contains(&y));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !landed(top()) && std::time::Instant::now() < deadline {
            pump_for(50);
        }
        let first = top();
        // Several more frames: nothing (the fill's anchor above all) throws the view elsewhere.
        pump_for(3_000);
        let after = top();
        eprintln!("s60 top: on landing {first:?}, 3 s later {after:?}");
        window.destroy();
        assert!(landed(first), "the jump never showed s60: {first:?}");
        assert!(landed(after), "the view moved off s60: {after:?}");
    }

    /// The model picker in a real window (a libadwaita one, so the dialog sits in its host).
    fn escape_closes_the_model_picker() {
        let backend = StubBackend::new(Driver::Claude);
        let view = ChatView::new(backend);
        let window = adw::Window::new();
        window.set_default_size(560, 700);
        adw::prelude::AdwWindowExt::set_content(&window, Some(&view));
        window.present();
        let context = glib::MainContext::default();
        assert!(crate::testutil::pump_until(&context, 5, || view.is_mapped()));
        let ctx = view
            .inner()
            .and_then(|i| i.panel_ctx())
            .expect("a built view");
        panels::tests::escape_in_the_pickers_search_closes_it(&ctx);
        window.destroy();
    }

    /// Live appends keep the rows built (nested ones included) within the budget while following,
    /// after a jump left the bottom, and after coming back; a jump to a step not built (evicted
    /// with its card, or under a card's cap) builds it and lands on it; "earlier steps" builds
    /// the rest of a card and keeps focus; a focused row is never evicted.
    pub(crate) fn live_append_keeps_the_window_bounded() {
        use agent_core::event::{ItemKind, ItemStatus};
        use transcript::{CHILD_CAP, ROW_BUDGET};
        let ctx = glib::MainContext::default();
        let pump = || while ctx.iteration(false) {};
        let view = ChatView::new(StubBackend::new(Driver::Claude));
        let window = gtk4::Window::new();
        window.set_default_size(700, 600);
        window.set_child(Some(&view));
        window.present();
        let inner = view.inner().expect("view");
        let t = &inner.transcript;
        let step = |id: &str, parent: Option<&str>| {
            view.apply(&Envelope::item_started(
                id,
                ItemKind::Command,
                format!("echo {id}"),
                None,
                parent,
            ));
            view.apply(&Envelope::item_completed(
                id,
                ItemStatus::Completed,
                Some("ok"),
                None,
            ));
        };
        let built = |id: &str| t.with_row(id, |_| ()).is_some();
        // Twelve sub-agents of 100 steps each, with plain steps between them, all live.
        for a in 0..12 {
            let agent = format!("a{a}");
            view.apply(&Envelope::item_started(
                &agent,
                ItemKind::Subagent,
                "Agent",
                None,
                None,
            ));
            for c in 0..100 {
                step(&format!("{agent}-c{c}"), Some(&agent));
                assert!(t.row_count() <= ROW_BUDGET, "{} rows", t.row_count());
            }
            view.apply(&Envelope::item_completed(
                &agent,
                ItemStatus::Completed,
                None,
                None,
            ));
            for s in 0..20 {
                step(&format!("{agent}-s{s}"), None);
            }
            pump();
            assert!(t.row_count() <= ROW_BUDGET, "{} rows", t.row_count());
        }
        assert!(!built("a0"), "the oldest card went with the budget");
        assert!(built("a11-c99") && built(&format!("a11-c{}", 100 - CHILD_CAP)));
        assert!(
            !built(&format!("a11-c{}", 99 - CHILD_CAP)),
            "a card builds its newest"
        );
        let (count, _) = t.earlier_of("a11").expect("an earlier-steps button");
        assert_eq!(count, 100 - CHILD_CAP);

        // A jump leaves the bottom; what arrives meanwhile stays within the budget.
        assert!(crate::testutil::pump_until(&ctx, 10, || {
            t.row_bounds_in_viewport("a11-s19")
                .is_some_and(|(b, _)| b.height() > 0.0)
        }));
        t.scroll_to_card("a11-s0");
        for s in 0..300 {
            step(&format!("late-{s}"), None);
        }
        pump();
        assert!(t.row_count() <= ROW_BUDGET, "{} rows", t.row_count());
        assert!(t.tail_len() > 0 && t.jump_shown(), "the rest waits below");
        assert!(built("a11-s0"), "the view kept what it showed");
        // Back to the end: the newest rows are built, and the window is bounded again.
        view.scroll_to_end();
        pump();
        assert!(built("late-299") && t.tail_len() == 0);
        assert!(t.row_count() <= ROW_BUDGET, "{} rows", t.row_count());

        // A jump to a step of an evicted card builds it and lands on it.
        assert!(!built("a0-c99"));
        t.scroll_to_card("a0-c99");
        assert!(built("a0-c99"), "the jump built what it needs");
        let landed = || {
            t.row_bounds_in_viewport("a0-c99")
                .is_some_and(|(b, _)| (0.0..=160.0).contains(&b.y()))
        };
        assert!(
            crate::testutil::pump_until(&ctx, 10, landed),
            "the jump never showed the step: {}",
            t.scroll_debug()
        );
        // A step under a built card's cap: built from it on, the button counting the rest.
        assert!(!built("a0-c3"));
        t.scroll_to_card("a0-c3");
        assert!(built("a0-c3") && built("a0-c59"));
        assert_eq!(t.earlier_of("a0").map(|(n, _)| n), Some(3));
        // "3 earlier steps" builds the rest, and focus moves to the first of them.
        let (_, button) = t.earlier_of("a0").expect("the button");
        assert_eq!(button.label().as_deref(), Some("3 earlier steps"));
        assert!(button.grab_focus());
        button.emit_clicked();
        assert!(built("a0-c0") && t.earlier_of("a0").is_none());
        let focus = gtk4::prelude::RootExt::focus(&window).expect("focus stays in the window");
        let first = t.with_row("a0-c0", |r| r.widget()).expect("built");
        assert!(
            focus.is_ancestor(&first),
            "focus moved to the first earlier step"
        );

        // A focused row is never evicted, however much arrives while following.
        view.scroll_to_end();
        for s in 0..400 {
            step(&format!("later-{s}"), None);
        }
        pump();
        assert!(built("a0-c0"), "the focused row stays");
        assert_eq!(
            gtk4::prelude::RootExt::focus(&window).as_ref(),
            Some(&focus),
            "and keeps focus"
        );
        assert!(t.row_count() <= ROW_BUDGET, "{} rows", t.row_count());
        assert!(!built("a1"), "the rows after it still go");
        window.destroy();
    }

    pub(crate) fn ui_checks() {
        replay_fills_the_window_in_steps();
        live_append_keeps_the_window_bounded();
        all_subagent_cards_toggle_together();
        model_chip_follows_pending_acceptance_and_rejection();
        stale_replay_checks();
        interruption::tests::answering_from_the_shelf_reenters_safely();
        interruption::tests::the_shelf_holds_requests_it_must_not_grant_unseen();
        requests_ui_checks();
        thinking_strip_follows_the_turn();
        follow::tests::a_followed_scroller_tracks_its_end_until_the_user_scrolls_up();
        composer::tests::stop_preserves_a_draft_and_completions_stay_in_the_field();
        panels::tests::changing_effort_does_not_switch_until_row_activation();
        panels::tests::a_reconnect_in_flight_shows_on_its_row();
        escape_closes_the_model_picker();
        usage::tests::compact_scope_does_not_filter_provider_comparison();
        cards::tests::file_changes_and_permission_details_have_distinct_actions();
        form_card::tests::form_and_link_cards();
        cards::tests::click_targets_hold_no_selectable_labels();
        cards::tests::blocks_copy_and_long_output_shows_full();
        git_card::tests::the_section_shows_the_reading_and_hides_for_anything_else();
        cards::tests::a_git_approval_reads_as_its_operation();
        whole_view_has_no_dead_click_targets();
        approval_row_precedes_its_step();
        live_rows_make_an_entrance_and_replayed_rows_do_not();
        subagent_shelf::tests::shelf_follows_running_subagents();
        subagents::tests::worker_group_updates_without_rebuilding_its_row();
        subagents::tests::a_long_sub_agent_opens_on_its_newest_steps();
        subagents::tests::names_are_never_squeezed_into_a_column();
        delegation::tests::ui_checks();
        let backend = StubBackend::new(Driver::Claude);
        let view = ChatView::new(backend.clone());
        // A focus-chasing viewport scrolled back up to a spent card and let go of the bottom.
        assert!(
            !view
                .inner()
                .is_some_and(|i| i.transcript.scrolls_to_focus()),
            "the transcript must not scroll to keyboard focus"
        );
        let hint = || {
            view.inner()
                .map(|i| i.composer.placeholder_text())
                .unwrap_or_default()
        };
        assert!(hint().starts_with("Message Claude"), "{}", hint());

        // The backend switches agent; the view hears of it only as events, never as a status
        // refresh, and the composer's hint must still follow.
        backend.switch(Driver::Agy, None, None);
        view.sink()(&Envelope::new(agent_core::event::Event::ModelChanged {
            model: "gemini-3.1-pro-high".into(),
        }));
        assert!(!hint().contains("Claude"), "{}", hint());
        assert!(hint().starts_with("Message Antigravity"), "{}", hint());

        backend.switch(Driver::Codex, None, None);
        view.sink()(&Envelope::new(agent_core::event::Event::ModelChanged {
            model: "gpt-5-codex".into(),
        }));
        assert!(hint().starts_with("Message Codex"), "{}", hint());

        // A mode picked in the header offers to become the agent's default; a mode the agent
        // reports moves the picker without offering anything.
        let chosen: Rc<RefCell<Vec<(Driver, Mode)>>> = Rc::default();
        let seen = chosen.clone();
        view.connect_action(move |a| {
            if let ViewAction::ModeChosen { driver, mode } = a {
                seen.borrow_mut().push((*driver, *mode));
            }
        });
        let picker = view.inner().map(|i| i.header.mode.clone()).expect("view");
        let index = |m: Mode| header::MODES.iter().position(|x| *x == m).unwrap_or(0) as u32;
        picker.set_selected(index(Mode::AcceptEdits));
        assert_eq!(*chosen.borrow(), [(Driver::Codex, Mode::AcceptEdits)]);
        view.sink()(&Envelope::new(agent_core::event::Event::ModeChanged {
            mode: Mode::Ask,
        }));
        assert_eq!(
            picker.selected(),
            index(Mode::Ask),
            "the picker follows the agent"
        );
        assert_eq!(chosen.borrow().len(), 1, "a reported mode offers nothing");
        view.sink()(&Envelope::new(Event::ModeChangeDeferred {
            requested: Mode::AcceptEdits,
            effective: Mode::Ask,
        }));
        assert_eq!(picker.selected(), index(Mode::Ask));
        assert_eq!(
            picker.tooltip_text().as_deref(),
            Some("Next turn: Accept edits")
        );
        view.sink()(&Envelope::new(Event::ModeChanged {
            mode: Mode::AcceptEdits,
        }));
        assert_eq!(picker.selected(), index(Mode::AcceptEdits));
        assert_eq!(
            picker.tooltip_text().as_deref(),
            Some("Accept edits (/mode)")
        );
        assert_eq!(
            chosen.borrow().len(),
            1,
            "deferred adoption is not another user choice"
        );

        // The header says whether the thread is still working, counting its background work;
        // the stop button follows the main turn only.
        {
            use agent_core::event::{BackgroundTask, BackgroundTaskKind, Event, TurnState};
            let inner = view.inner().expect("view");
            let shown = || (inner.header.activity_shown(), inner.composer.shows_stop());
            let background = |n: usize| {
                view.sink()(&Envelope::new(Event::BackgroundTasks {
                    tasks: (0..n)
                        .map(|i| BackgroundTask {
                            id: format!("t{i}"),
                            kind: BackgroundTaskKind::Agent,
                            description: Some(format!("Review part {i}")),
                            tool_use_id: None,
                        })
                        .collect(),
                }));
            };
            assert_eq!(shown(), (None, false), "a new thread says nothing");
            view.sink()(&Envelope::new(Event::TurnStarted { model: None }));
            assert_eq!(shown(), (Some("Working…".into()), true));
            background(2);
            assert_eq!(
                shown(),
                (Some("Working…, plus 2 in the background".into()), true)
            );
            view.sink()(&Envelope::turn_completed(TurnState::Completed, None));
            assert_eq!(
                shown(),
                (
                    Some(
                        "Main agent done, waiting on 2 background tasks: Review part 0, \
                         Review part 1"
                            .into()
                    ),
                    false
                ),
                "background work keeps the thread busy, but there is no turn to stop"
            );
            assert!(!inner.header.running_shown());
            background(0);
            assert_eq!(shown(), (Some("Completed".into()), false));
        }

        // The model-source listener goes away with the view.
        let source = Rc::new(CountingSource::default());
        view.set_model_source(source.clone());
        assert_eq!(source.listeners.len(), 1);
        view.set_model_source(source.clone());
        assert_eq!(source.listeners.len(), 1, "replacing a source leaves one");
        drop(view);
        assert_eq!(
            source.listeners.len(),
            0,
            "the dropped view left a listener"
        );
    }

    /// Answers every diff request at once with a canned reply, and records what was asked.
    struct FakeDiffs {
        asks: RefCell<Vec<DiffAsk>>,
        opened: RefCell<Vec<DiffAsk>>,
        reply: crate::chat::DiffReply,
    }

    impl DiffSource for FakeDiffs {
        fn load(&self, ask: DiffAsk, done: Box<dyn FnOnce(crate::chat::DiffReply)>) {
            self.asks.borrow_mut().push(ask);
            done(self.reply.clone());
        }
        fn open_external(&self, ask: DiffAsk) {
            self.opened.borrow_mut().push(ask);
        }
    }

    /// While following the bottom, the newest row stays in view through a whole scripted turn
    /// (thinking, streamed markdown with code, a sub-agent, an approval, tool cards, the summary)
    /// and through sending prompts. Measured as the user sees it: the last row's bottom edge
    /// against the visible area, not the adjustment. A row may overflow for a frame while it
    /// grows; it must not stay below the fold. Run like the test below.
    #[test]
    #[ignore = "presents a window; run on a private display"]
    fn the_newest_row_stays_in_view_while_following() {
        let ctx = crate::testutil::native_init();
        let backend = demo::demo_backend();
        let view = ChatView::new(backend.clone());
        backend.connect(view.sink());
        let window = gtk4::Window::new();
        window.set_default_size(560, 460);
        window.set_child(Some(&view));
        window.present();
        let inner = view.inner().expect("view");
        // Samples every ~16 ms for `ms`; returns the most frames a stretch of the last row below
        // the fold (while following) lasted, and the worst overflow. Frames, not milliseconds:
        // a headless display draws about once a second. A row that grows is drawn below the fold
        // for the frame it grew in at most; the next frame must show it.
        let watch = |ms: u64, what: &str| -> (u64, f64) {
            let until = std::time::Instant::now() + std::time::Duration::from_millis(ms);
            let (mut since, mut longest, mut worst) = (None, 0u64, 0f64);
            while std::time::Instant::now() < until {
                while ctx.iteration(false) {}
                std::thread::sleep(std::time::Duration::from_millis(16));
                let frame = inner.transcript.frames();
                match inner.transcript.last_row_overflow() {
                    Some((over, true)) if over > 2.0 => {
                        let start = *since.get_or_insert(frame);
                        let spanned = frame - start;
                        if spanned > longest {
                            longest = spanned;
                            eprintln!(
                                "  {what}: below the fold for {spanned} frames, over {over:.0}: {}",
                                inner.transcript.scroll_debug()
                            );
                        }
                        worst = worst.max(over);
                    }
                    _ => since = None,
                }
            }
            eprintln!("{what}: longest below-fold stretch {longest} frames, worst {worst:.0} px");
            crate::testutil::capture_window(
                &window,
                &format!("follow-bottom-{}", what.replace(' ', "-")),
            );
            (longest, worst)
        };
        backend.play_script();
        let script = watch(14_000, "scripted turn");
        let mut sends = Vec::new();
        for n in 0..3 {
            inner.submit(&format!("follow-up question {n}\nwith a second line"));
            sends.push(watch(3_500, &format!("send {n}")));
        }
        window.destroy();
        assert!(
            script.0 <= 1,
            "scripted turn left the newest row below the fold: {script:?}"
        );
        for (n, s) in sends.iter().enumerate() {
            assert!(
                s.0 <= 1,
                "send {n} left the newest row below the fold: {s:?}"
            );
        }
    }

    /// Needs a realised window and a running main loop, so it is not part of the window smoke
    /// test. Run it on a private display: the preview MCP's `preview_app` with the test binary
    /// and `jump_to_latest_returns_to_the_bottom --ignored --nocapture`.
    #[test]
    #[ignore = "presents a window; run on a private display"]
    fn jump_to_latest_returns_to_the_bottom() {
        use agent_core::event::Event;
        gtk4::init().expect("GTK init");
        let ctx = glib::MainContext::default();
        let settle = |ms: u64| {
            let until = std::time::Instant::now() + std::time::Duration::from_millis(ms);
            while std::time::Instant::now() < until {
                ctx.iteration(false);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        let backend = StubBackend::new(Driver::Claude);
        let view = ChatView::new(backend);
        let window = gtk4::Window::new();
        window.set_default_size(500, 400);
        window.set_child(Some(&view));
        window.present();
        let sink = view.sink();
        let note = |n: usize| {
            sink(&Envelope::new(Event::Notice {
                text: format!("message {n}\nwith a second line"),
            }));
        };
        for n in 0..400 {
            note(n);
        }
        settle(500);
        let (jump, adj) = view.inner().expect("view").transcript.scroll_parts();
        let at_end = |adj: &gtk4::Adjustment| adj.value() + adj.page_size() >= adj.upper() - 1.0;
        assert!(
            at_end(&adj),
            "follows the stream: {} {} {}",
            adj.value(),
            adj.page_size(),
            adj.upper()
        );

        // The user scrolls well up (then right to the top, which loads older rows), more
        // arrives, and the jump button shows; a click must bring the bottom back each time.
        let mut n = 400;
        for (round, up) in [600.0, f64::INFINITY, f64::INFINITY]
            .into_iter()
            .enumerate()
        {
            for _ in 0..3 {
                adj.set_value((adj.value() - up).max(0.0));
                settle(150);
            }
            note(n);
            n += 1;
            settle(300);
            eprintln!(
                "round {round} up: value {} page {} upper {} visible {}",
                adj.value(),
                adj.page_size(),
                adj.upper(),
                jump.is_visible()
            );
            assert!(!at_end(&adj), "stays where the user put it");
            assert!(jump.is_visible(), "the jump button shows");
            // A real click goes to whatever the pointer picks: it must be the button.
            let centre =
                gtk4::graphene::Point::new(jump.width() as f32 / 2.0, jump.height() as f32 / 2.0);
            let at = jump
                .compute_point(&window, &centre)
                .expect("button in the window");
            let picked = window.pick(
                f64::from(at.x()),
                f64::from(at.y()),
                gtk4::PickFlags::DEFAULT,
            );
            eprintln!(
                "picked at the button: {:?}",
                picked.as_ref().map(|w| w.type_().name())
            );
            assert!(
                picked.is_some_and(
                    |w| w == *jump.upcast_ref::<gtk4::Widget>() || w.is_ancestor(&jump)
                ),
                "a click on the button reaches it"
            );

            jump.emit_clicked();
            settle(500);
            eprintln!(
                "round {round} jump: value {} page {} upper {} visible {}",
                adj.value(),
                adj.page_size(),
                adj.upper(),
                jump.is_visible()
            );
            assert!(at_end(&adj), "jump returns to the bottom (round {round})");
            assert!(!jump.is_visible());
            note(n);
            n += 1;
            settle(300);
            assert!(at_end(&adj), "follows again after the jump");
        }
        window.destroy();
    }

    /// What the user sees of a view, for comparing a thread streamed while hidden with one
    /// streamed in view and one replayed from its stored events.
    #[derive(Debug, PartialEq)]
    struct Seen {
        /// Every materialised row, nested ones included: id, shown, allocated height.
        rows: Vec<(String, bool, i32)>,
        /// The header's sub-agent button: shown, count text, listed ids.
        subagents: (bool, String, Vec<String>),
        /// The header says the agent is working.
        running: bool,
        /// Approval cards: id, buttons shown, buttons sensitive, outcome line.
        approvals: Vec<(String, bool, bool, String)>,
        /// Tool cards still showing a spinner.
        spinning: Vec<String>,
        /// Reasoning cards: id, title, shown.
        reasoning: Vec<(String, String, bool)>,
        /// The jump-to-latest button shows.
        jump: bool,
        /// Dividers, notices and errors (their text), and replies (the agent credited).
        statics: Vec<(String, String)>,
    }

    fn seen(view: &ChatView) -> (Seen, Option<(f64, bool)>) {
        let inner = view.inner().expect("view");
        let model = inner.model.borrow();
        let mut ids = Vec::new();
        let mut stack: Vec<String> = model.order().iter().rev().cloned().collect();
        while let Some(id) = stack.pop() {
            if let Some(item) = model.get(&id) {
                stack.extend(item.children.iter().rev().cloned());
            }
            ids.push(id);
        }
        let t = &inner.transcript;
        let mut s = Seen {
            rows: Vec::new(),
            subagents: inner.subagent_button.state(),
            running: inner.header.running_shown(),
            approvals: Vec::new(),
            spinning: Vec::new(),
            reasoning: Vec::new(),
            jump: t.scroll_parts().0.is_visible(),
            statics: Vec::new(),
        };
        for id in ids {
            t.with_row(&id, |row| {
                let w = row.widget();
                s.rows.push((id.clone(), w.is_visible(), w.height()));
                match row {
                    cards::Row::Approval(a) => {
                        let (shown, sensitive, outcome) = a.actionable();
                        s.approvals.push((id.clone(), shown, sensitive, outcome));
                    }
                    cards::Row::Tool(c) if c.spinning() => s.spinning.push(id.clone()),
                    cards::Row::Reasoning(r) => {
                        let (title, shown) = r.reasoning_state();
                        s.reasoning.push((id.clone(), title, shown));
                    }
                    cards::Row::Assistant { root, .. } => {
                        // The agent the reply is credited to (its name in the row's head).
                        let mut name = String::new();
                        let mut stack: Vec<gtk4::Widget> = vec![root.clone().upcast()];
                        while let Some(w) = stack.pop() {
                            if w.has_css_class("agent-name") {
                                if let Some(l) = w.downcast_ref::<gtk4::Label>() {
                                    name = l.text().to_string();
                                }
                            }
                            let mut child = w.first_child();
                            while let Some(c) = child {
                                child = c.next_sibling();
                                stack.push(c);
                            }
                        }
                        s.statics.push((id.clone(), format!("reply by {name}")));
                    }
                    cards::Row::Static(w) | cards::Row::Switch { root: w, .. } => {
                        // Dividers and notices: the text they show.
                        let mut texts = Vec::new();
                        let mut stack = vec![w.clone()];
                        while let Some(w) = stack.pop() {
                            if let Some(l) = w.downcast_ref::<gtk4::Label>() {
                                texts.push(l.text().to_string());
                            }
                            let mut child = w.first_child();
                            while let Some(c) = child {
                                child = c.next_sibling();
                                stack.push(c);
                            }
                        }
                        s.statics.push((id.clone(), texts.join(" | ")));
                    }
                    _ => {}
                }
            });
        }
        (s, t.last_row_overflow())
    }

    /// A thread keeps receiving its stream while another tab is selected (its view unmapped: no
    /// layout, no frame ticks). Streams the demo turn, three sends and a sub-agent into two views
    /// at once: A in view the whole time, B on a tab page that is first shown (as a thread is
    /// built) and then left for another page. Each time B is shown again it must look exactly like
    /// A: every row's size, the newest row in view and followed, the sub-agent list, approval
    /// cards, thinking cards, spinners, the header. C replays B's recorded stream, as a thread
    /// opened later is built from the store, and must look the same too.
    ///
    /// A minimised window is not covered: neither this Wayland session nor XWayland lets a test
    /// restore a window it minimised itself (its frame clock never resumes), so the restored
    /// state cannot be observed.
    #[test]
    #[ignore = "presents a window; run on a private display"]
    fn a_thread_streamed_on_another_tab_shows_what_a_visible_one_does() {
        use agent_core::event::{Event, ItemKind, ItemStatus};
        gtk4::init().expect("GTK init");
        adw::init().expect("adw init");
        let ctx = glib::MainContext::default();
        let pump = |ms: u64| {
            let until = std::time::Instant::now() + std::time::Duration::from_millis(ms);
            while std::time::Instant::now() < until {
                while ctx.iteration(false) {}
                std::thread::sleep(std::time::Duration::from_millis(8));
            }
        };
        // Each view sits in a tab view like the app's, all of one fixed width (wide enough that
        // no card's minimum width widens it), side by side in ONE window: separate windows
        // overlap, and the compositor stops laying out a covered one, which is not what is
        // measured here.
        let holder = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let scroller = gtk4::ScrolledWindow::new();
        scroller.set_child(Some(&holder));
        let main = gtk4::Window::new();
        main.set_default_size(1400, 700);
        main.set_child(Some(&scroller));
        let tabs_with = |view: &ChatView| {
            let tabs = adw::TabView::new();
            tabs.set_size_request(820, 640);
            let other = tabs.append(&gtk4::Label::new(Some("another thread")));
            let page = tabs.append(view);
            tabs.set_selected_page(&page);
            (tabs, other, page)
        };
        let a_backend = demo::demo_backend();
        let a = ChatView::new(a_backend.clone());
        a_backend.connect(a.sink());
        let b_backend = demo::demo_backend();
        let b = ChatView::new(b_backend.clone());
        // B's stream as the store records it: each envelope with the agent B was on when it
        // arrived (the store knows it from the provider thread the event is stored under).
        type Credited = Vec<(Option<Driver>, Envelope)>;
        let recorded: Rc<RefCell<Credited>> = Rc::default();
        let (b_sink, rec) = (b.sink(), recorded.clone());
        let b_weak = Rc::downgrade(&b_backend);
        let b_stream: EnvelopeSink = Rc::new(move |env: &Envelope| {
            let driver = b_weak.upgrade().map(|b| b.status().driver);
            rec.borrow_mut().push((driver, env.clone()));
            b_sink(env);
        });
        b_backend.connect(b_stream.clone());
        let (ta, _, _) = tabs_with(&a);
        holder.append(&ta);
        let (tb, b_other, b_page) = tabs_with(&b);
        holder.append(&tb);
        main.present();
        pump(800);
        let hide_b = || tb.set_selected_page(&b_other);
        let show_b = || tb.set_selected_page(&b_page);
        hide_b();
        pump(300);
        assert!(!b.is_mapped(), "B is off screen while streamed");

        a_backend.play_script();
        b_backend.play_script();
        pump(14_000);
        for n in 0..3 {
            let text = format!("follow-up question {n}\nwith a second line");
            a.inner().expect("a").submit(&text);
            b.inner().expect("b").submit(&text);
            pump(3_500);
        }
        // A sub-agent starts while B is hidden: B must list it once shown.
        let both = |env: Envelope| {
            a.sink()(&env);
            b_stream(&env);
        };
        both(Envelope::new(Event::TurnStarted { model: None }));
        both(Envelope::item_started(
            "bg1",
            ItemKind::Subagent,
            "Task",
            Some(serde_json::json!({"subagent_type": "Explore", "description": "Map the crate"})),
            None,
        ));
        both(Envelope::item_started(
            "bg1-read",
            ItemKind::FileRead,
            "Read",
            Some(serde_json::json!({"file_path": "src/main.rs"})),
            Some("bg1"),
        ));
        pump(1_000);
        show_b();
        pump(2_500);
        let (mid_a, mid_oa) = seen(&a);
        let (mid_b, mid_ob) = seen(&b);
        eprintln!(
            "mid-turn A: overflow {mid_oa:?} sub-agents {:?}",
            mid_a.subagents
        );
        eprintln!(
            "mid-turn B: overflow {mid_ob:?} sub-agents {:?}",
            mid_b.subagents
        );

        // It finishes, and the turn ends, while B is hidden again.
        hide_b();
        pump(300);
        both(Envelope::item_completed(
            "bg1-read",
            ItemStatus::Completed,
            Some("mapped"),
            None,
        ));
        both(Envelope::item_completed(
            "bg1",
            ItemStatus::Completed,
            Some("done"),
            None,
        ));
        both(Envelope::turn_completed(
            agent_core::event::TurnState::Completed,
            None,
        ));
        pump(1_000);
        show_b();

        // C: a thread opened later, built from B's stored events (as `build_thread` does:
        // replayed before it is in a window, then shown).
        let c_backend = demo::demo_backend();
        let c = ChatView::new(c_backend);
        c.replay_by_agent(&recorded.borrow());
        let (tc, _, _) = tabs_with(&c);
        holder.append(&tc);
        pump(2_500);
        let (sa, oa) = seen(&a);
        let (sb, ob) = seen(&b);
        let (sc, oc) = seen(&c);
        for (name, s, o) in [("A", &sa, oa), ("B", &sb, ob), ("C", &sc, oc)] {
            eprintln!(
                "{name}: overflow {o:?} jump {} running {} subagents {:?} spinning {:?}\n  approvals {:?}\n  reasoning {:?}\n  statics {:?}",
                s.jump, s.running, s.subagents, s.spinning, s.approvals, s.reasoning, s.statics
            );
        }
        let diff = |x: &Seen, y: &Seen| -> Vec<String> {
            let mut out = Vec::new();
            let (xs, ys) = (&x.rows, &y.rows);
            if xs.len() != ys.len() {
                out.push(format!("row count {} vs {}", xs.len(), ys.len()));
            }
            for (r, s) in xs.iter().zip(ys) {
                if r != s {
                    out.push(format!("{r:?} vs {s:?}"));
                }
            }
            out
        };
        eprintln!("rows A vs B: {:#?}", diff(&sa, &sb));
        eprintln!("rows A vs C: {:#?}", diff(&sa, &sc));
        main.destroy();

        let ok_overflow =
            |o: Option<(f64, bool)>| o.is_some_and(|(over, stick)| stick && over <= 2.0);
        assert!(
            ok_overflow(mid_ob),
            "B mid-turn: newest row in view {mid_ob:?}"
        );
        assert_eq!(mid_a, mid_b, "B mid-turn differs from A");
        assert!(
            mid_b.subagents.0,
            "a sub-agent started while hidden is listed"
        );
        assert!(ok_overflow(oa), "A: newest row in view {oa:?}");
        assert!(ok_overflow(ob), "B: newest row in view {ob:?}");
        assert!(ok_overflow(oc), "C (replay): newest row in view {oc:?}");
        assert_eq!(
            sa, sb,
            "the thread streamed while hidden differs from the one in view"
        );
        // Replayed with each event's own agent, every reply and "Continued in" divider is
        // credited as it was live (C's backend is on Claude, the agent the stream began on,
        // not the one it ended on).
        assert!(
            sc.statics
                .iter()
                .any(|(_, s)| s.starts_with("Continued in")),
            "the agent switch divider survives the replay: {:?}",
            sc.statics
        );
        assert_eq!(sa, sc, "the replayed thread differs from the one in view");
    }

    /// A thinking block whose text never streams (the model withheld it) must not end as an
    /// empty, openable "Thought process"; one with text stays openable.
    pub(crate) fn reasoning_ui_checks() {
        use agent_core::event::{ItemKind, ItemStatus, StreamKind};

        let backend = StubBackend::new(Driver::Claude);
        let view = ChatView::new(backend);
        let sink = view.sink();
        let think = |id: &str, text: Option<&str>| {
            sink(&Envelope::item_started(
                id,
                ItemKind::Reasoning,
                "Thinking",
                None,
                None,
            ));
            if let Some(text) = text {
                sink(&Envelope::delta(id, StreamKind::Reasoning, text));
            }
            sink(&Envelope::item_completed(
                id,
                ItemStatus::Completed,
                None,
                None,
            ));
        };
        think("withheld", None);
        think("shared", Some("weighing two options"));
        // Row updates are flushed on idle, as a frame would.
        let ctx = glib::MainContext::default();
        while ctx.iteration(false) {}
        let state = |id: &str| {
            view.inner().and_then(|i| {
                i.transcript.with_row(id, |row| match row {
                    cards::Row::Reasoning(r) => Some(r.reasoning_state()),
                    _ => None,
                })
            })
        };
        let (_, shown) = state("withheld").flatten().expect("withheld row");
        assert!(
            !shown,
            "a withheld thought takes no space, so nothing jumps"
        );
        let (title, shown) = state("shared").flatten().expect("shared row");
        assert_eq!(title, "Thought process");
        assert!(shown);

        // Streaming must not re-lay out the whole block per frame: a collapsed card holds no
        // text, and an open one changes only its last paragraph's label.
        let delta = |text: &str| {
            sink(&Envelope::delta("long", StreamKind::Reasoning, text));
        };
        sink(&Envelope::item_started(
            "long",
            ItemKind::Reasoning,
            "Thinking",
            None,
            None,
        ));
        delta("first thought.\n\nsecond");
        while ctx.iteration(false) {}
        let labels = |id: &str| {
            view.inner()
                .and_then(|i| {
                    i.transcript.with_row(id, |row| match row {
                        cards::Row::Reasoning(r) => {
                            Some((r.reasoning_paragraphs(), r.reasoning_labels()))
                        }
                        _ => None,
                    })
                })
                .flatten()
                .expect("long row")
        };
        assert!(labels("long").0.is_empty(), "collapsed: no text laid out");
        if let Some(inner) = view.inner() {
            inner.row_event(RowEvent::Toggle {
                id: "long".into(),
                expanded: true,
            });
        }
        while ctx.iteration(false) {}
        let (texts, before) = labels("long");
        assert_eq!(texts, ["first thought.", "second"], "opening shows it all");
        delta(" thought, still going");
        while ctx.iteration(false) {}
        let (texts, after) = labels("long");
        assert_eq!(texts, ["first thought.", "second thought, still going"]);
        assert_eq!(
            before, after,
            "the paragraphs' labels are kept, not rebuilt"
        );

        // A Codex summary's markdown renders (bold, no `**`), and markup in the text stays
        // text.
        delta("\n\n**Planning** the listing\n\n<b>not bold</b>");
        while ctx.iteration(false) {}
        let (texts, labels_now) = labels("long");
        assert_eq!(texts[2], "Planning the listing", "no markers shown");
        assert_eq!(labels_now[2].label(), "<b>Planning</b> the listing");
        assert!(labels_now[2].uses_markup());
        assert_eq!(
            texts[3], "<b>not bold</b>",
            "source markup is shown as text"
        );
    }

    #[test]
    #[ignore = "presents a window; run on a private display"]
    fn long_tool_titles_keep_user_messages_inside_the_viewport() {
        let ctx = crate::testutil::native_init();
        let view = ChatView::new(StubBackend::new(Driver::Codex));
        let command = format!("git show {}", "a".repeat(820));
        let mut history = demo::DemoBackend::stress_history(180);
        history.push(Envelope::item_started(
            "wide-command",
            agent_core::event::ItemKind::Command,
            command.clone(),
            Some(serde_json::json!({"command": command})),
            None,
        ));
        history.push(Envelope::item_completed(
            "wide-command",
            agent_core::event::ItemStatus::Completed,
            Some("done"),
            None,
        ));
        let path = format!("src/{}.rs", "x".repeat(820));
        history.push(Envelope::item_started(
            "wide-file",
            agent_core::event::ItemKind::FileChange,
            path.clone(),
            Some(serde_json::json!([{
                "path": path, "kind": {"type": "add"}, "diff": "+example"
            }])),
            None,
        ));
        history.push(Envelope::item_started(
            "visible-user",
            agent_core::event::ItemKind::UserMessage,
            "",
            None,
            None,
        ));
        history.push(Envelope::snapshot(
            "visible-user",
            agent_core::event::StreamKind::Assistant,
            "Can you still see this message?",
        ));
        view.replay(&history);
        let holder = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        holder.append(&view);
        let hidden = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        hidden.set_visible(false);
        let pane = gtk4::Paned::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .start_child(&holder)
            .end_child(&hidden)
            .shrink_start_child(true)
            .shrink_end_child(true)
            .build();
        let window = gtk4::Window::new();
        window.set_default_size(560, 460);
        window.set_child(Some(&pane));
        window.present();
        let inner = view.inner().expect("view");
        assert!(
            crate::testutil::pump_until(&ctx, 8, || inner.transcript.frames() >= 3),
            "layout frames"
        );
        let overflow = crate::testutil::overflowing_bins(view.upcast_ref());
        let (user, width) = inner
            .transcript
            .row_bounds_in_viewport("visible-user")
            .expect("user in viewport");
        let bottom = inner.transcript.last_row_overflow().expect("last row");
        let report = inner.transcript.scroll_debug();
        window.destroy();
        assert!(
            overflow.is_empty(),
            "content exceeds its viewport: {overflow:?}\n{report}"
        );
        assert!(
            user.x() >= 0.0 && user.x() + user.width() <= width as f32,
            "user row exceeds {width}px: {user:?}\n{report}"
        );
        assert!(
            bottom.0 <= 2.0 && bottom.1,
            "newest user message below viewport: {bottom:?}\n{report}"
        );
    }

    /// Measures what a streamed thinking block costs the main loop, per delta: the view's own
    /// work (reduce + row update), the size request the next frame then pays, and the real frame
    /// times. Prints numbers; asserts nothing. Run on a private display like the tests above,
    /// with `stream_cost_of_a_long_thinking_block --ignored --nocapture`.
    #[test]
    #[ignore = "presents a window; a measurement, run on a private display"]
    fn stream_cost_of_a_long_thinking_block() {
        use agent_core::event::{Event, ItemKind, StreamKind};
        use std::time::{Duration, Instant};
        gtk4::init().expect("GTK init");
        let ctx = glib::MainContext::default();
        let backend = StubBackend::new(Driver::Claude);
        let view = ChatView::new(backend);
        let window = gtk4::Window::new();
        window.set_default_size(900, 700);
        window.set_child(Some(&view));
        window.present();
        let sink = view.sink();
        // A thread with history: the transcript box holds its full window of rows.
        for n in 0..140 {
            sink(&Envelope::new(Event::Notice {
                text: format!(
                    "earlier message {n}, long enough to wrap onto a second line of the transcript column"
                ),
            }));
        }
        let pump = |ms: u64| {
            let until = Instant::now() + Duration::from_millis(ms);
            while Instant::now() < until {
                while ctx.iteration(false) {}
                std::thread::sleep(Duration::from_millis(2));
            }
        };
        pump(800);

        // Real frame times: before-paint to after-paint covers update, layout and paint.
        let clock = window.frame_clock().expect("frame clock");
        let frames: Rc<RefCell<Vec<f64>>> = Rc::default();
        let start: Rc<Cell<Option<Instant>>> = Rc::default();
        let s = start.clone();
        clock.connect_before_paint(move |_| s.set(Some(Instant::now())));
        let (s, f) = (start.clone(), frames.clone());
        clock.connect_after_paint(move |_| {
            if let Some(t) = s.take() {
                f.borrow_mut().push(t.elapsed().as_secs_f64() * 1e3);
            }
        });

        let pct = |v: &mut Vec<f64>| -> String {
            if v.is_empty() {
                return "n/a".into();
            }
            v.sort_by(f64::total_cmp);
            let at = |p: f64| v[((v.len() - 1) as f64 * p) as usize];
            format!(
                "n {} p50 {:.2} p90 {:.2} p99 {:.2} max {:.2} ms",
                v.len(),
                at(0.5),
                at(0.9),
                at(0.99),
                v[v.len() - 1]
            )
        };
        let words =
            "the model weighs whether the cache invalidation belongs in the reducer or the view ";
        for (round, expanded) in [(0, false), (1, true)] {
            let id = format!("think-{round}");
            sink(&Envelope::item_started(
                &id,
                ItemKind::Reasoning,
                "Thinking",
                None,
                None,
            ));
            pump(50);
            if expanded {
                if let Some(inner) = view.inner() {
                    inner.row_event(RowEvent::Toggle {
                        id: id.clone(),
                        expanded: true,
                    });
                }
                pump(400);
            }
            frames.borrow_mut().clear();
            let (mut apply, mut measure) = (Vec::new(), Vec::new());
            // The view's own width: the size the frame's layout asks for, so cached rows hit.
            let width = view.width();
            let began = Instant::now();
            for n in 0..1500 {
                // Paragraphs of about 500 characters, as thinking comes.
                let mut chunk = words[(n * 7) % 60..(n * 7) % 60 + 20].to_owned();
                if n % 25 == 24 {
                    chunk.push_str("\n\n");
                }
                let t = Instant::now();
                sink(&Envelope::delta(&id, StreamKind::Reasoning, chunk));
                while ctx.iteration(false) {}
                apply.push(t.elapsed().as_secs_f64() * 1e3);
                // The size request the next frame pays for what changed.
                let t = Instant::now();
                let _ = view.measure(gtk4::Orientation::Vertical, width);
                measure.push(t.elapsed().as_secs_f64() * 1e3);
                if n % 300 == 299 {
                    let recent = &measure[measure.len() - 50..];
                    eprintln!(
                        "    at {} chars: size request mean {:.2} ms",
                        (n + 1) * 20,
                        recent.iter().sum::<f64>() / 50.0
                    );
                }
                // About Claude's pace: deltas a few ms apart, so frames interleave.
                if n % 4 == 0 {
                    std::thread::sleep(Duration::from_millis(4));
                    while ctx.iteration(false) {}
                }
            }
            let wall = began.elapsed().as_secs_f64();
            eprintln!(
                "thinking {} ({} chars): wall {wall:.2} s",
                if expanded { "expanded" } else { "collapsed" },
                1500 * 20
            );
            eprintln!("  delta handling: {}", pct(&mut apply));
            eprintln!("  size request:   {}", pct(&mut measure));
            eprintln!("  frames:         {}", pct(&mut frames.borrow_mut()));
            sink(&Envelope::item_completed(
                &id,
                agent_core::event::ItemStatus::Completed,
                None,
                None,
            ));
            pump(200);
        }

        // The session's own per-delta work: one scrubbed store row per delta, on the main loop.
        let dir = std::env::temp_dir().join(format!("at-stream-cost-{}", std::process::id()));
        let store =
            agent_kit::store::Store::open(&dir.join("threads.db")).expect("temp store opens");
        let thread = store.create_thread("/tmp", None).expect("thread");
        let mut writes = Vec::new();
        for n in 0..1500 {
            let frame = serde_json::json!({"type": "stream_event", "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": &words[(n * 7) % 60..(n * 7) % 60 + 20]}}});
            let env = Envelope::new(Event::ContentDelta {
                stream: StreamKind::Reasoning,
                text: words[(n * 7) % 60..(n * 7) % 60 + 20].into(),
            })
            .item("msg:0")
            .raw(frame);
            let t = Instant::now();
            store.append_event(&thread, None, &env).expect("append");
            writes.push(t.elapsed().as_secs_f64() * 1e3);
        }
        eprintln!("  store append per delta: {}", pct(&mut writes));
        // Once per item: the completion re-scrubs the item's deltas (`heal_split_deltas`).
        let t = Instant::now();
        store
            .append_event(
                &thread,
                None,
                &Envelope::item_completed(
                    "msg:0",
                    agent_core::event::ItemStatus::Completed,
                    None,
                    None,
                ),
            )
            .expect("append");
        eprintln!(
            "  store append of the completion (heal over 1500 deltas): {:.2} ms",
            t.elapsed().as_secs_f64() * 1e3
        );
        // The status the view reads per envelope, with a realistic slash-command list.
        let mut status = status_of(Driver::Claude);
        status.commands = (0..150)
            .map(|n| agent_core::event::AgentCommand {
                name: format!("command-{n}"),
                description: Some("a slash command with a sentence of description".into()),
                argument_hint: None,
                kind: agent_core::event::AgentCommandKind::Command,
            })
            .collect();
        let t = Instant::now();
        for _ in 0..1500 {
            std::hint::black_box(status.clone());
        }
        eprintln!(
            "  status clone (150 commands): {:.4} ms each",
            t.elapsed().as_secs_f64() * 1e3 / 1500.0
        );
        let _ = std::fs::remove_dir_all(&dir);
        window.destroy();
    }

    /// The sub-agent explorer: the header lists the thread's sub-agents, and an open panel
    /// follows the one it shows as new steps arrive.
    pub(crate) fn subagent_ui_checks() {
        use agent_core::event::ItemKind;
        use serde_json::json;

        let backend = StubBackend::new(Driver::Claude);
        let view = ChatView::new(backend);
        let inner = view.inner().expect("inner");
        let ctx = glib::MainContext::default();
        let pump = || while ctx.iteration(false) {};
        pump();
        assert!(
            !inner.subagent_button.state().0,
            "hidden with no sub-agents"
        );

        let sink = view.sink();
        let step = |id: &str, kind: ItemKind, parent: Option<&str>, input| {
            sink(&Envelope::item_started(id, kind, "Task", input, parent));
        };
        step(
            "agent1",
            ItemKind::Subagent,
            None,
            Some(
                json!({"subagent_type": "Explore", "description": "Map the crate", "prompt": "Map it"}),
            ),
        );
        step("read1", ItemKind::FileRead, Some("agent1"), None);
        pump();
        let (visible, count, ids) = inner.subagent_button.state();
        assert!(visible);
        assert_eq!(count, "1 active · 0 finished");
        assert_eq!(ids, ["agent1"]);

        inner.open_subagent("agent1");
        let panel = || inner.subagent_panel.borrow().clone().expect("panel open");
        assert_eq!(panel().steps_shown(), 1);
        let first = panel().row_widget("read1").expect("step row");
        step("read2", ItemKind::FileRead, Some("agent1"), None);
        // A step outside any sub-agent never touches the explorer.
        step("solo", ItemKind::Command, None, None);
        pump();
        assert_eq!(panel().steps_shown(), 2, "the panel follows new steps");
        assert_eq!(
            panel().row_widget("read1"),
            Some(first),
            "existing steps are updated in place, never rebuilt"
        );
        let (_, count, ids) = inner.subagent_button.state();
        assert_eq!((count.as_str(), ids.len()), ("1 active · 0 finished", 1));

        // A second one starts: running ones are listed first.
        step("agent2", ItemKind::Subagent, None, None);
        pump();
        // Done: it stays listed, after the running one and labelled done, and the panel the
        // user opened stays.
        sink(&Envelope::item_completed(
            "agent1",
            agent_core::event::ItemStatus::Completed,
            Some("mapped"),
            None,
        ));
        pump();
        let (visible, count, ids) = inner.subagent_button.state();
        assert!(visible, "a finished sub-agent keeps the button");
        assert_eq!(count, "1 active · 1 finished");
        assert_eq!(ids, ["agent2", "agent1"], "running first, finished after");
        assert_eq!(
            inner.subagent_button.statuses(),
            ["running", "completed · 2 steps"]
        );
        assert_eq!(panel().steps_shown(), 2, "the open panel keeps showing it");

        // A replayed thread whose sub-agent finished long ago still lists it.
        let replayed = ChatView::new(StubBackend::new(Driver::Claude));
        replayed.replay(&[
            Envelope::item_started(
                "old",
                ItemKind::Subagent,
                "Task",
                Some(json!({"subagent_type": "Explore", "description": "Old work"})),
                None,
            ),
            Envelope::item_completed(
                "old",
                agent_core::event::ItemStatus::Completed,
                Some("found it"),
                None,
            ),
        ]);
        pump();
        let r = replayed.inner().expect("replayed view");
        assert_eq!(
            r.subagent_button.state(),
            (
                true,
                "0 active · 1 finished".to_owned(),
                vec!["old".to_owned()]
            )
        );
        assert_eq!(r.subagent_button.statuses(), ["completed"]);
    }

    /// GTK checks of the diff viewer: the card's toggle, its buttons, the approval's diff.
    pub(crate) fn diff_ui_checks() {
        use agent_core::event::{Event, ItemKind, ResponseCapability};
        use agent_kit::filediff::{Origin, Shown};
        use serde_json::json;

        let backend = StubBackend::new(Driver::Claude);
        let view = ChatView::new(backend);
        let diff_text = "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n";
        let source = Rc::new(FakeDiffs {
            asks: RefCell::default(),
            opened: RefCell::default(),
            reply: Ok(Shown {
                text: diff_text.to_owned(),
                origin: Origin::Checkpoint,
                omitted_lines: 0,
                clipped: false,
                files: vec!["src/a.rs".to_owned()],
            }),
        });
        view.set_diff_source(source.clone());
        let sink = view.sink();
        let edit_input = json!({"file_path": "/w/src/a.rs", "old_string": "b", "new_string": "B"});
        sink(&Envelope::item_started(
            "edit1",
            ItemKind::FileChange,
            "Edit",
            Some(edit_input.clone()),
            None,
        ));
        sink(&Envelope::item_started(
            "cmd1",
            ItemKind::Command,
            "ls",
            Some(json!({"command": "ls"})),
            None,
        ));
        let inner = view.inner().expect("inner");
        let card = |id: &str| {
            inner.transcript.with_row(id, |row| match row {
                cards::Row::Tool(card) => Some(card.diff_buttons()),
                _ => None,
            })
        };

        // Only a file edit has a diff bar; the "Open in" button follows the configured tool.
        let tools = crate::diff_tool::DiffTools::shared();
        tools.set(None);
        assert_eq!(
            card("cmd1").flatten().map(|b| b.0),
            Some(false),
            "a command has no diff"
        );
        let (bar, open, _) = card("edit1").flatten().expect("edit card");
        assert!(bar && !open, "no tool configured: View diff only");
        tools.set(Some(agent_kit::difftool::PRESETS[0].tool()));
        assert_eq!(
            card("edit1").flatten(),
            Some((true, true, "Open in Meld".to_owned()))
        );
        tools.set(Some(agent_kit::difftool::DiffTool {
            name: "Kompare".into(),
            argv: vec!["kompare".into(), "{old}".into(), "{new}".into()],
        }));
        assert_eq!(
            card("edit1").flatten().map(|b| b.2),
            Some("Open in Kompare".to_owned())
        );

        // Toggling View diff asks the source with the item's id and input, and shows the answer.
        inner.transcript.with_row("edit1", |row| {
            if let cards::Row::Tool(c) = row {
                c.click_view_diff();
            }
        });
        let asks = source.asks.borrow().clone();
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].item, "edit1");
        assert_eq!(asks[0].input, edit_input);
        let state = inner
            .transcript
            .with_row("edit1", |row| match row {
                cards::Row::Tool(c) => Some(c.diff_state()),
                _ => None,
            })
            .flatten()
            .expect("state");
        assert!(state.0, "the toggle stays on");
        assert_eq!(state.2, diff_text);
        assert_eq!(state.3, "Changes this turn");
        assert_eq!(state.4.as_deref(), Some("diff"), "highlighted as a diff");

        // A reply that is an error says so instead of a diff.
        let failing = Rc::new(FakeDiffs {
            asks: RefCell::default(),
            opened: RefCell::default(),
            reply: Err("No diff is available for this edit.".to_owned()),
        });
        view.set_diff_source(failing);
        sink(&Envelope::item_started(
            "edit2",
            ItemKind::FileChange,
            "Write",
            None,
            None,
        ));
        inner.transcript.with_row("edit2", |row| {
            if let cards::Row::Tool(c) = row {
                c.click_view_diff();
            }
        });
        let state = inner
            .transcript
            .with_row("edit2", |row| match row {
                cards::Row::Tool(c) => Some(c.diff_state()),
                _ => None,
            })
            .flatten()
            .expect("state");
        assert_eq!(state.1, "No diff is available for this edit.");
        assert_eq!(state.2, "");

        // An Edit approval shows the proposed change as a diff, with the JSON behind "Show raw";
        // a command approval keeps its command line and has neither.
        sink(
            &Envelope::new(Event::ApprovalRequested {
                tool: "Edit".into(),
                title: None,
                input: edit_input,
                reason: None,
                options: vec![Decision::Allow, Decision::Deny],
                response: ResponseCapability::Live,
                remembers: None,
            })
            .request("r1"),
        );
        sink(
            &Envelope::new(Event::ApprovalRequested {
                tool: "Bash".into(),
                title: None,
                input: json!({"command": "rm -rf build"}),
                reason: None,
                options: vec![Decision::Allow, Decision::AllowAlways, Decision::Deny],
                response: ResponseCapability::Live,
                remembers: Some("“Always allow” saves: Bash(rm -rf build) in this project".into()),
            })
            .request("r2"),
        );
        let approval = |id: &str| {
            inner
                .transcript
                .with_row(id, |row| match row {
                    cards::Row::Approval(a) => Some(a.diff_view()),
                    _ => None,
                })
                .flatten()
                .expect("approval card")
        };
        let (files, diff_visible, raw_visible, text) = approval("approval:r1");
        assert_eq!(files, "/w/src/a.rs  +1 −1");
        assert!(diff_visible && raw_visible);
        assert!(text.contains("-b\n+B\n"), "{text}");
        let (files, diff_visible, raw_visible, _) = approval("approval:r2");
        assert_eq!(files, "$ rm -rf build", "the command stays prominent");
        assert!(!diff_visible && !raw_visible);
        // "Always allow" says exactly what it would save; a card without it says nothing.
        let remembers = |id: &str| {
            inner
                .transcript
                .with_row(id, |row| match row {
                    cards::Row::Approval(a) => a.remembers_text(),
                    _ => None,
                })
                .flatten()
        };
        assert_eq!(
            remembers("approval:r2").as_deref(),
            Some("“Always allow” saves: Bash(rm -rf build) in this project")
        );
        assert_eq!(remembers("approval:r1"), None);

        // A Codex file-change approval carries no diff itself; the item it names has it.
        sink(&Envelope::item_started(
            "cx1",
            ItemKind::FileChange,
            "a.rs",
            Some(
                json!([{"path": "a.rs", "kind": {"type": "update"}, "diff": "@@ -1 +1 @@\n-o\n+n\n"}]),
            ),
            None,
        ));
        sink(
            &Envelope::new(Event::ApprovalRequested {
                tool: "file_change".into(),
                title: Some("Apply file changes".into()),
                input: json!({"itemId": "cx1", "reason": "why"}),
                reason: None,
                options: vec![Decision::Allow],
                response: ResponseCapability::Live,
                remembers: None,
            })
            .request("r3")
            .item("cx1"),
        );
        let (_, diff_visible, raw_visible, text) = approval("approval:r3");
        assert!(diff_visible && raw_visible);
        assert!(text.contains("-o\n+n\n"), "{text}");

        tools.set(None);
    }

    #[derive(Default)]
    struct CountingSource {
        listeners: crate::probe::ListenerSet,
    }

    impl ModelSource for CountingSource {
        fn models(&self) -> Vec<agent_core::catalog::CatalogModel> {
            Vec::new()
        }
        fn connect_changed(&self, f: Box<dyn Fn()>) -> u64 {
            self.listeners.add(f)
        }
        fn disconnect(&self, id: u64) {
            self.listeners.remove(id);
        }
    }

    #[test]
    fn only_a_scroll_up_the_view_did_not_make_lets_go_of_the_bottom() {
        use transcript::{Look, TranscriptView as T};
        let look = |value, upper, page| Look { value, upper, page };
        // A one-line arrow-key or slow touchpad move up (well under the old 48 px slop) counts.
        assert!(T::user_moved_up(
            look(980.0, 2000.0, 600.0),
            look(1000.0, 2000.0, 600.0)
        ));
        // GTK clamping a transcript that shrank (a card collapsing) is a relayout, not the user.
        assert!(!T::user_moved_up(
            look(900.0, 1900.0, 600.0),
            look(1000.0, 2000.0, 600.0)
        ));
        // Nor is the visible height changing (the composer growing or shrinking around a send).
        assert!(!T::user_moved_up(
            look(960.0, 2000.0, 640.0),
            look(1000.0, 2000.0, 600.0)
        ));
        // Moving down, or not at all, never lets go.
        assert!(!T::user_moved_up(
            look(1000.0, 2000.0, 600.0),
            look(980.0, 2000.0, 600.0)
        ));
        assert!(!T::user_moved_up(
            look(1000.0, 2000.0, 600.0),
            look(1000.0, 2000.0, 600.0)
        ));
        // Either way counts as the user scrolling (it cancels a jump waiting for layout); a
        // relayout or no move does not.
        assert!(T::user_moved(
            look(1000.0, 2000.0, 600.0),
            look(980.0, 2000.0, 600.0)
        ));
        assert!(T::user_moved(
            look(980.0, 2000.0, 600.0),
            look(1000.0, 2000.0, 600.0)
        ));
        assert!(!T::user_moved(
            look(1100.0, 2100.0, 600.0),
            look(1000.0, 2000.0, 600.0)
        ));
        assert!(!T::user_moved(
            look(1000.0, 2000.0, 600.0),
            look(1000.0, 2000.0, 600.0)
        ));
    }

    #[test]
    fn help_lists_only_backed_builtins() {
        let claude = help_text(&agent_core::caps::Capabilities::claude(), "claude");
        assert!(claude.contains("/mcp"));
        assert!(claude.contains("/mode plan|default|accept-edits"));
        // Claude lacks only tool-server forms (Codex passes them on): no built-in is missing.
        let (claude_backed, claude_missing) = claude
            .split_once("Not available here")
            .expect("forms are explained");
        assert!(claude_missing.contains("forms from its tool servers"));
        assert!(!claude_missing.contains(" — "), "{claude_missing}");
        assert!(!claude_backed.contains("restarts"));
        let agy = help_text(&agent_core::caps::Capabilities::agy(), "Antigravity");
        let (backed, missing) = agy
            .split_once("Not available here")
            .expect("a missing section");
        assert!(!backed.contains("/mcp"));
        assert!(missing.contains("/mcp — Antigravity has no MCP panel."));
        assert!(agy.contains("Esc stops"));
    }

    /// The caps contract: every built-in that needs a capability is a working control when
    /// the agent has it, and a named, explained absence when it does not (`/help` lists it, and
    /// typing it says why instead of sending it to the agent as a prompt).
    #[test]
    fn every_capability_a_command_needs_is_a_control_or_an_explained_absence() {
        use agent_core::adapter::Driver;
        use agent_core::caps::Capabilities;
        for driver in Driver::ALL {
            let caps = Capabilities::of(driver);
            let name = driver.info().label;
            let help = help_text(&caps, name);
            let status = SessionStatus {
                driver,
                model: None,
                pending_model: None,
                effort: None,
                mode: Mode::Plan,
                running_turn: false,
                alive: true,
                capabilities: caps.clone(),
                commands: Vec::new(),
            };
            for feature in agent_core::caps::Feature::ALL {
                if let Some(note) = caps.unsupported_note(feature, name) {
                    assert!(help.contains(&note), "{name} /help omits: {note}");
                }
            }
            for b in builtins() {
                let typed = format!("/{}", b.name);
                match b.feature() {
                    Some(feature) if !caps.supports(feature) => {
                        let note = caps.unsupported_note(feature, name).expect("a note");
                        assert!(help.contains(&note), "{name} /help omits: {note}");
                        assert_eq!(
                            unavailable_builtin_note(&typed, &status),
                            Some(note),
                            "{name} {typed}"
                        );
                    }
                    _ => {
                        assert!(help.contains(&format!("/{}", b.name)), "{name} {typed}");
                        assert_eq!(unavailable_builtin_note(&typed, &status), None);
                    }
                }
            }
        }
    }

    /// `/fork` is a built-in for every agent, run locally, never sent to the agent as text.
    #[test]
    fn slash_fork_is_a_local_builtin_for_every_agent() {
        use agent_core::adapter::Driver;
        for driver in Driver::ALL {
            let caps = agent_core::caps::Capabilities::of(driver);
            for typed in ["/fork", "  /fork  ", "/FORK"] {
                let line = typeahead::parse_command_line(typed, &caps).expect("a built-in");
                assert_eq!(
                    line.builtin.action,
                    BuiltinAction::Fork,
                    "{driver:?} {typed}"
                );
            }
        }
    }

    #[test]
    fn codex_says_it_has_no_mcp_panel() {
        let status = SessionStatus {
            driver: Driver::Codex,
            model: None,
            pending_model: None,
            effort: None,
            mode: Mode::Plan,
            running_turn: false,
            alive: true,
            capabilities: agent_core::caps::Capabilities::codex(),
            commands: Vec::new(),
        };
        let note = unavailable_builtin_note("/mcp", &status).expect("a note");
        assert!(note.ends_with("has no MCP panel."), "{note}");
        assert_eq!(unavailable_builtin_note("/model", &status), None);
        assert_eq!(unavailable_builtin_note("hello /mcp", &status), None);
    }

    #[test]
    fn plan_glyphs() {
        assert_eq!(step_glyph(StepStatus::Completed).0, "✓");
        assert_eq!(step_glyph(StepStatus::InProgress).1, "step-active");
    }

    #[test]
    fn approval_round_trip_through_a_fake_backend() {
        // The reducer side of an approval: sent, then resolved by the agent's echo.
        let mut t = Transcript::new();
        t.apply(
            &Envelope::new(agent_core::event::Event::ApprovalRequested {
                tool: "Bash".into(),
                title: None,
                input: serde_json::json!({"command": "ls"}),
                reason: None,
                options: vec![Decision::Allow],
                response: agent_core::event::ResponseCapability::Live,
                remembers: None,
            })
            .request("r"),
            Driver::Claude,
        );
        assert_eq!(
            t.mark_approval_sent("r", Decision::Allow),
            vec![Change::Updated("approval:r".into())]
        );
    }

    #[test]
    #[ignore = "needs a display; run alone: cargo test interruption_shelf -- --ignored"]
    fn interruption_shelf_reveals_and_collapses() {
        gtk4::init().expect("GTK must be available for the native gate");
        let sent = Rc::new(RefCell::new(Vec::new()));
        let shelf = interruption::InterruptionShelf::new(
            Rc::new(move |e| sent.borrow_mut().push(e)),
            |_| {},
        );
        let mut t = Transcript::new();
        shelf.update(&t);
        assert!(!shelf.revealer.reveals_child());

        t.apply(
            &Envelope::new(agent_core::event::Event::ApprovalRequested {
                tool: "Bash".into(),
                title: None,
                input: serde_json::json!({"command": "cargo check"}),
                reason: None,
                options: vec![Decision::Allow, Decision::Deny],
                response: agent_core::event::ResponseCapability::Live,
                remembers: None,
            })
            .request("r1"),
            Driver::Claude,
        );
        shelf.update(&t);
        assert!(shelf.revealer.reveals_child());

        t.mark_approval_sent("r1", Decision::Allow);
        shelf.update(&t);
        assert!(!shelf.revealer.reveals_child());
    }
}
