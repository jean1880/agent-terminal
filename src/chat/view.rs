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

mod cards;
mod composer;
pub mod demo;
mod difftext;
mod header;
mod interruption;
mod markdown;
pub mod model;
mod panels;
mod payload;
mod subagent_shelf;
mod subagents;
mod thinking;
mod transcript;
mod typeahead;
pub mod usage;

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::{Rc, Weak};

use adw::subclass::prelude::*;
use agent_core::adapter::{Control, Driver, Mode};
use agent_core::commands::{builtins, compact_text, BuiltinAction, Trigger};
use agent_core::event::{Envelope, PlanStep, StepStatus};
use gtk4::prelude::*;
use gtk4::{gdk, glib};

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
}

type ActionHandler = Rc<dyn Fn(&ViewAction)>;

const STYLE: &str = include_str!("view/style.css");

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
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ChatView {
        const NAME: &'static str = "AgentTerminalChatView";
        type Type = super::ChatView;
        type ParentType = gtk4::Box;
    }

    impl ObjectImpl for ChatView {
        fn dispose(&self) {
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
            let subagent_shelf = subagent_shelf::SubagentShelf::new(move |id| {
                if let Some(inner) = w.upgrade() {
                    inner.open_subagent(id);
                }
            });
            let w = weak.clone();
            let interruption = interruption::InterruptionShelf::new(sink.clone(), move |id| {
                if let Some(inner) = w.upgrade() {
                    inner.transcript.scroll_to_card(&id);
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
            }
        });

        inner.header.set_subagents(inner.subagent_button.widget());
        view.append(&inner.header.bin);
        view.append(inner.transcript.widget());
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
        inner.refresh_status();
        inner.transcript.reset(&inner.model.borrow());

        *view.imp().inner.borrow_mut() = Some(inner);
        view
    }

    fn inner(&self) -> Option<Rc<Inner>> {
        self.imp().inner.borrow().clone()
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
    }

    fn handle(self: &Rc<Self>, changes: Vec<Change>) {
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
                Change::Plan => self.plan.set(&self.model.borrow().plan),
                Change::Gauge => self.header.set_gauge(self.model.borrow().gauge.as_ref()),
                Change::Mode => {
                    if let Some(mode) = self.model.borrow().mode {
                        self.header.set_mode(mode);
                    }
                }
                Change::Running => self.refresh_status(),
                // The hint names the `$ skills` trigger only once the agent lists skills.
                Change::Commands => self.composer.refresh_placeholder(),
                Change::Control { request, result } => self.control_result(&request, result),
            }
        }
        // Model and agent changes arrive as several event kinds; the header is cheap to redo.
        self.refresh_agent_chip();
        self.interruption.update(&self.model.borrow());
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
        let agents = model.subagents();
        // Every sub-agent the thread started: the running ones first, then the finished ones
        // with how they ended (a replayed thread, or one whose sub-agents are all done, still
        // lists them).
        self.subagent_button.set(&subagents::running_first(&agents));
        self.subagent_shelf.set(&model, &agents);
        if let Some(panel) = self.subagent_panel.borrow().as_ref() {
            let summary = agents.iter().find(|a| a.id == panel.id());
            panel.refresh(&model, summary, changed);
        }
    }

    /// Shows one sub-agent on its own, in a dialog that follows it until closed.
    fn open_subagent(self: &Rc<Self>, id: &str) {
        let panel = subagents::SubagentPanel::new(id, self.row_sink.clone());
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
        // A dialog needs a window to sit on; a view that is not in one (yet) has nowhere to show it.
        if let Some(parent) = self.widget.upgrade().filter(|w| w.root().is_some()) {
            adw::prelude::AdwDialogExt::present(panel.dialog(), Some(&parent));
        }
        *self.subagent_panel.borrow_mut() = Some(panel);
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

    fn refresh_status(&self) {
        let status = self.backend.status();
        let running = status.running_turn || self.model.borrow().running;
        // The header counts background work too; the stop button follows the main turn only.
        let activity = self.model.borrow().activity(running);
        self.header.set_activity(&activity);
        self.thinking
            .set(&activity, cards::driver_name(status.driver));
        self.composer.set_running(running);
        self.refresh_agent_chip();
        self.header
            .set_mode(self.model.borrow().mode.unwrap_or(status.mode));
        self.header
            .gauge
            .set_sensitive(status.capabilities.context_usage);
        self.interruption.update(&self.model.borrow());
        self.composer.refresh_placeholder();
    }

    fn refresh_agent_chip(&self) {
        let status = self.backend.status();
        let model = status
            .model
            .clone()
            .or_else(|| self.model.borrow().current_model().map(str::to_owned));
        self.header.set_agent(status.driver, model.as_deref());
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
            if inner.set_mode(mode) {
                // Only a pick made in the header, never a mode the agent reported.
                let driver = inner.backend.status().driver;
                inner.emit(ViewAction::ModeChosen { driver, mode });
            }
        });
    }

    /// Asks the backend for `mode`; false when the agent has no such mode (nothing was asked).
    fn set_mode(self: &Rc<Self>, mode: Mode) -> bool {
        let caps = self.backend.status().capabilities;
        if mode == Mode::Plan && !caps.plan_mode {
            let changes = self
                .model
                .borrow_mut()
                .push_notice("This agent has no read-only planning mode.", Tone::Warning);
            self.handle(changes);
            self.refresh_status();
            return false;
        }
        self.backend.set_mode(mode);
        true
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
            RowEvent::Approve { request, decision } => {
                let changes = self
                    .model
                    .borrow_mut()
                    .mark_approval_sent(&request, decision);
                self.handle(changes);
                self.backend.respond_approval(&request, decision);
                // The decision is made: typing goes back to the composer, not the spent card.
                self.composer.grab_focus();
            }
            RowEvent::Answer { request, answers } => {
                let changes = self.model.borrow_mut().mark_questions_sent(&request);
                self.handle(changes);
                self.backend.answer_questions(&request, answers);
                self.composer.grab_focus();
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
                            "{} has no compaction command, so the thread continues in a fresh \
                             session with a budgeted summary.",
                            cards::driver_name(status.driver)
                        ),
                        Tone::Info,
                    );
                    self.handle(changes);
                    self.emit(ViewAction::CompactByHandoff);
                }
            },
            BuiltinAction::SetMode => match typeahead::parse_mode(args) {
                Some(mode) => {
                    self.set_mode(mode);
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
                let changes = self
                    .model
                    .borrow_mut()
                    .push_notice(help_text(&status.capabilities), Tone::Info);
                self.handle(changes);
            }
        }
    }
}

/// The `/help` text: the built-ins this agent can back, then the keys.
fn help_text(caps: &agent_core::caps::Capabilities) -> String {
    let mut s = String::from("Commands\n");
    for b in builtins().iter().filter(|b| b.available(caps)) {
        let hint = b.hint.map(|h| format!(" {h}")).unwrap_or_default();
        s.push_str(&format!("  /{}{hint} — {}\n", b.name, b.description));
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
        // What you just sent, and the reply to it, are where the view goes.
        self.transcript.scroll_to_end();
        self.backend.send_prompt(text);
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

    fn command_items(&self, trigger: &Trigger) -> Vec<agent_core::commands::CompletionItem> {
        let status = self.backend.status();
        composer::items_for(trigger, &status.commands, &status.capabilities)
    }

    fn request_files(&self, query: &str) -> Option<String> {
        if !self.backend.status().capabilities.file_suggestions {
            return None;
        }
        Some(self.backend.control(Control::FileSuggestions {
            query: query.to_owned(),
        }))
    }

    fn placeholder(&self) -> String {
        if self.model.borrow().pending_interruption().is_some() {
            "Awaiting approval above...".to_owned()
        } else {
            placeholder_text(&self.backend.status())
        }
    }
}

/// The composer's hint for the thread's CURRENT agent: its name and the triggers it backs.
fn placeholder_text(status: &SessionStatus) -> String {
    let mut tips = vec!["/ commands"];
    if status.capabilities.file_suggestions {
        tips.push("@ files");
    }
    if status
        .commands
        .iter()
        .any(|c| c.kind == agent_core::event::AgentCommandKind::Skill)
    {
        tips.push("$ skills");
    }
    format!(
        "Message {}  ·  {}",
        cards::driver_name(status.driver),
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
        self.title
            .set_text(&format!("Plan  ·  {done} of {} done", plan.len()));
        for step in plan {
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
            row.add_css_class("plan-step");
            let (glyph, class) = step_glyph(step.status);
            row.add_css_class(class);
            row.append(&cards::label(glyph, &["step-glyph"]));
            let text = cards::label(&step.text, &["step-text"]);
            text.set_wrap(true);
            text.set_hexpand(true);
            row.append(&text);
            self.steps.append(&row);
        }
        self.revealer.set_reveal_child(!plan.is_empty());
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use agent_core::event::Decision;

    fn status_of(driver: Driver) -> SessionStatus {
        SessionStatus {
            driver,
            model: None,
            effort: None,
            mode: Mode::Ask,
            running_turn: false,
            alive: true,
            capabilities: agent_core::caps::Capabilities::of(driver),
            commands: Vec::new(),
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

    /// A backend whose status the test changes, as a session does on a switch.
    struct SwitchableBackend {
        status: RefCell<SessionStatus>,
    }

    impl ChatBackend for SwitchableBackend {
        fn send_prompt(&self, _: &str) {}
        fn interrupt(&self) {}
        fn respond_approval(&self, _: &str, _: Decision) {}
        fn answer_questions(&self, _: &str, _: serde_json::Value) {}
        fn switch(&self, driver: Driver, _: Option<String>, _: Option<String>) {
            *self.status.borrow_mut() = status_of(driver);
        }
        fn set_mode(&self, _: Mode) {}
        fn control(&self, _: Control) -> String {
            String::new()
        }
        fn status(&self) -> SessionStatus {
            self.status.borrow().clone()
        }
    }

    /// A thread stored mid-turn, reopened: replayed before its session starts (no live agent),
    /// it must show no phantom running turn, no answerable card and no background work.
    fn stale_replay_checks() {
        use agent_core::event::{
            BackgroundTask, BackgroundTaskKind, Event, ItemKind, ResponseCapability,
        };
        let mut dead = status_of(Driver::Claude);
        dead.alive = false;
        let view = ChatView::new(Rc::new(SwitchableBackend {
            status: RefCell::new(dead),
        }));
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
            Envelope::new(Event::ItemStarted {
                kind: ItemKind::Subagent,
                title: "Agent".into(),
                input: Some(serde_json::json!({"subagent_type": "Explore"})),
                parent: None,
            })
            .item("agent1"),
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
        assert_eq!(inner.header.activity_shown().as_deref(), Some("Finished"));
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

    /// The dots above the composer show while a turn runs, name the agent, and go when it ends.
    fn thinking_strip_follows_the_turn() {
        use agent_core::event::{Event, TurnState};
        let backend = Rc::new(SwitchableBackend {
            status: RefCell::new(status_of(Driver::Claude)),
        });
        let view = ChatView::new(backend);
        let inner = view.inner().expect("a built view");
        assert_eq!(inner.thinking.shown(), None, "a new thread shows nothing");
        view.apply(&Envelope::new(Event::TurnStarted { model: None }));
        assert_eq!(
            inner.thinking.shown().as_deref(),
            Some("Claude is thinking…")
        );
        view.apply(&Envelope::new(Event::TurnCompleted {
            state: TurnState::Completed,
            usage: None,
            cost_usd: None,
            error: None,
        }));
        assert_eq!(inner.thinking.shown(), None, "gone once the turn ends");
    }

    /// GTK checks, run from the window test (GTK belongs to the one thread that initialised it).
    pub(crate) fn ui_checks() {
        stale_replay_checks();
        interruption::tests::answering_from_the_shelf_reenters_safely();
        thinking_strip_follows_the_turn();
        subagent_shelf::tests::shelf_follows_running_subagents();
        let backend = Rc::new(SwitchableBackend {
            status: RefCell::new(status_of(Driver::Claude)),
        });
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
            view.sink()(&Envelope::new(Event::TurnCompleted {
                state: TurnState::Completed,
                usage: None,
                cost_usd: None,
                error: None,
            }));
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
            assert_eq!(shown(), (Some("Finished".into()), false));
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
        gtk4::init().expect("GTK init");
        let ctx = glib::MainContext::default();
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
        let backend = Rc::new(SwitchableBackend {
            status: RefCell::new(status_of(Driver::Claude)),
        });
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
                    cards::Row::Static(w) => {
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
        both(
            Envelope::new(Event::ItemStarted {
                kind: ItemKind::Subagent,
                title: "Task".into(),
                input: Some(
                    serde_json::json!({"subagent_type": "Explore", "description": "Map the crate"}),
                ),
                parent: None,
            })
            .item("bg1"),
        );
        both(
            Envelope::new(Event::ItemStarted {
                kind: ItemKind::FileRead,
                title: "Read".into(),
                input: Some(serde_json::json!({"file_path": "src/main.rs"})),
                parent: Some("bg1".into()),
            })
            .item("bg1-read"),
        );
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
        both(
            Envelope::new(Event::ItemCompleted {
                status: ItemStatus::Completed,
                output: Some("mapped".into()),
                error: None,
            })
            .item("bg1-read"),
        );
        both(
            Envelope::new(Event::ItemCompleted {
                status: ItemStatus::Completed,
                output: Some("done".into()),
                error: None,
            })
            .item("bg1"),
        );
        both(Envelope::new(Event::TurnCompleted {
            state: agent_core::event::TurnState::Completed,
            usage: None,
            cost_usd: None,
            error: None,
        }));
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
        use agent_core::event::{Event, ItemKind, ItemStatus, StreamKind};

        let backend = Rc::new(SwitchableBackend {
            status: RefCell::new(status_of(Driver::Claude)),
        });
        let view = ChatView::new(backend);
        let sink = view.sink();
        let think = |id: &str, text: Option<&str>| {
            sink(
                &Envelope::new(Event::ItemStarted {
                    kind: ItemKind::Reasoning,
                    title: "Thinking".into(),
                    input: None,
                    parent: None,
                })
                .item(id),
            );
            if let Some(text) = text {
                sink(
                    &Envelope::new(Event::ContentDelta {
                        stream: StreamKind::Reasoning,
                        text: text.into(),
                    })
                    .item(id),
                );
            }
            sink(
                &Envelope::new(Event::ItemCompleted {
                    status: ItemStatus::Completed,
                    output: None,
                    error: None,
                })
                .item(id),
            );
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
    }

    /// The sub-agent explorer: the header lists the thread's sub-agents, and an open panel
    /// follows the one it shows as new steps arrive.
    pub(crate) fn subagent_ui_checks() {
        use agent_core::event::{Event, ItemKind};
        use serde_json::json;

        let backend = Rc::new(SwitchableBackend {
            status: RefCell::new(status_of(Driver::Claude)),
        });
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
            sink(
                &Envelope::new(Event::ItemStarted {
                    kind,
                    title: "Task".into(),
                    input,
                    parent: parent.map(str::to_owned),
                })
                .item(id),
            );
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
        assert_eq!(count, "1 sub-agent · 1 running");
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
        assert_eq!((count.as_str(), ids.len()), ("1 sub-agent · 1 running", 1));

        // A second one starts: running ones are listed first.
        step("agent2", ItemKind::Subagent, None, None);
        pump();
        // Done: it stays listed, after the running one and labelled done, and the panel the
        // user opened stays.
        sink(
            &Envelope::new(Event::ItemCompleted {
                status: agent_core::event::ItemStatus::Completed,
                output: Some("mapped".into()),
                error: None,
            })
            .item("agent1"),
        );
        pump();
        let (visible, count, ids) = inner.subagent_button.state();
        assert!(visible, "a finished sub-agent keeps the button");
        assert_eq!(count, "2 sub-agents · 1 running");
        assert_eq!(ids, ["agent2", "agent1"], "running first, finished after");
        assert_eq!(
            inner.subagent_button.statuses(),
            ["running", "done · 2 steps"]
        );
        assert_eq!(panel().steps_shown(), 2, "the open panel keeps showing it");

        // A replayed thread whose sub-agent finished long ago still lists it.
        let replayed = ChatView::new(Rc::new(SwitchableBackend {
            status: RefCell::new(status_of(Driver::Claude)),
        }));
        replayed.replay(&[
            Envelope::new(Event::ItemStarted {
                kind: ItemKind::Subagent,
                title: "Task".into(),
                input: Some(json!({"subagent_type": "Explore", "description": "Old work"})),
                parent: None,
            })
            .item("old"),
            Envelope::new(Event::ItemCompleted {
                status: agent_core::event::ItemStatus::Completed,
                output: Some("found it".into()),
                error: None,
            })
            .item("old"),
        ]);
        pump();
        let r = replayed.inner().expect("replayed view");
        assert_eq!(
            r.subagent_button.state(),
            (true, "1 sub-agent".to_owned(), vec!["old".to_owned()])
        );
        assert_eq!(r.subagent_button.statuses(), ["done"]);
    }

    /// GTK checks of the diff viewer: the card's toggle, its buttons, the approval's diff.
    pub(crate) fn diff_ui_checks() {
        use agent_core::event::{Event, ItemKind, ResponseCapability};
        use agent_kit::filediff::{Origin, Shown};
        use serde_json::json;

        let backend = Rc::new(SwitchableBackend {
            status: RefCell::new(status_of(Driver::Claude)),
        });
        let view = ChatView::new(backend);
        let diff_text = "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n";
        let source = Rc::new(FakeDiffs {
            asks: RefCell::default(),
            opened: RefCell::default(),
            reply: Ok(Shown {
                text: diff_text.to_owned(),
                origin: Origin::Checkpoint,
                omitted_lines: 0,
                files: vec!["src/a.rs".to_owned()],
            }),
        });
        view.set_diff_source(source.clone());
        let sink = view.sink();
        let edit_input = json!({"file_path": "/w/src/a.rs", "old_string": "b", "new_string": "B"});
        sink(
            &Envelope::new(Event::ItemStarted {
                kind: ItemKind::FileChange,
                title: "Edit".into(),
                input: Some(edit_input.clone()),
                parent: None,
            })
            .item("edit1"),
        );
        sink(
            &Envelope::new(Event::ItemStarted {
                kind: ItemKind::Command,
                title: "ls".into(),
                input: Some(json!({"command": "ls"})),
                parent: None,
            })
            .item("cmd1"),
        );
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
        sink(
            &Envelope::new(Event::ItemStarted {
                kind: ItemKind::FileChange,
                title: "Write".into(),
                input: None,
                parent: None,
            })
            .item("edit2"),
        );
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
                remembers: Some("Bash(rm -rf build) in this project".into()),
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
        sink(&Envelope::new(Event::ItemStarted {
            kind: ItemKind::FileChange,
            title: "a.rs".into(),
            input: Some(json!([{"path": "a.rs", "kind": {"type": "update"}, "diff": "@@ -1 +1 @@\n-o\n+n\n"}])),
            parent: None,
        })
        .item("cx1"));
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
    }

    #[test]
    fn help_lists_only_backed_builtins() {
        let claude = help_text(&agent_core::caps::Capabilities::claude());
        assert!(claude.contains("/mcp"));
        assert!(claude.contains("/mode plan|default|accept-edits"));
        let agy = help_text(&agent_core::caps::Capabilities::agy());
        assert!(!agy.contains("/mcp"));
        assert!(agy.contains("Esc stops"));
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
        if gtk4::init().is_err() {
            return;
        }
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
