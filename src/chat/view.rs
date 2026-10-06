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
mod markdown;
pub mod model;
mod panels;
mod payload;
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
            Inner {
                backend,
                model: Rc::new(RefCell::new(Transcript::new())),
                transcript: TranscriptView::new(sink),
                composer: Composer::new(),
                header: Header::new(),
                plan: PlanPanel::new(),
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
            }
        });

        view.append(&inner.header.root);
        view.append(inner.transcript.widget());
        let bottom = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        bottom.add_css_class("chat-bottom");
        let clamp = adw::Clamp::new();
        clamp.set_maximum_size(860);
        clamp.set_tightening_threshold(640);
        let column = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        column.append(&inner.plan.revealer);
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
    /// item. Control replies in history are dropped (nobody is waiting for them).
    pub fn replay(&self, envs: &[Envelope]) {
        let Some(inner) = self.inner() else {
            return;
        };
        let started = std::time::Instant::now();
        let driver = inner.backend.status().driver;
        {
            let mut model = inner.model.borrow_mut();
            for env in envs {
                model.apply(env, driver);
            }
        }
        inner.dirty.borrow_mut().clear();
        inner.transcript.reset(&inner.model.borrow());
        let model = inner.model.borrow();
        inner.plan.set(&model.plan);
        inner.header.set_gauge(model.gauge.as_ref());
        if let Some(mode) = model.mode {
            inner.header.set_mode(mode);
        }
        drop(model);
        inner.refresh_status();
        tracing::info!(
            envelopes = envs.len(),
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
        inner.header.insert_usage(indicator.widget());
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
            }
        });
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
        self.header.set_running(running);
        self.composer.set_running(running);
        self.refresh_agent_chip();
        self.header
            .set_mode(self.model.borrow().mode.unwrap_or(status.mode));
        self.header
            .gauge
            .set_sensitive(status.capabilities.context_usage);
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
            inner.set_mode(mode);
        });
    }

    fn set_mode(self: &Rc<Self>, mode: Mode) {
        let caps = self.backend.status().capabilities;
        if mode == Mode::Plan && !caps.plan_mode {
            let changes = self
                .model
                .borrow_mut()
                .push_notice("This agent has no read-only planning mode.", Tone::Warning);
            self.handle(changes);
            self.refresh_status();
            return;
        }
        self.backend.set_mode(mode);
    }

    fn row_event(self: &Rc<Self>, e: RowEvent) {
        match e {
            RowEvent::Toggle { id, expanded } => {
                self.model.borrow_mut().set_expanded(&id, expanded);
                self.transcript.updated(&self.model.borrow(), &id);
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
            self.transcript
                .show_diff(id, &Err("No diff is available for this edit.".to_owned()));
            return;
        };
        let (weak, id) = (Rc::downgrade(self), id.to_owned());
        source.load(
            ask,
            Box::new(move |reply| {
                if let Some(inner) = weak.upgrade() {
                    inner.transcript.show_diff(&id, &reply);
                }
            }),
        );
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
                Some(mode) => self.set_mode(mode),
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
        placeholder_text(&self.backend.status())
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
        let card = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        card.add_css_class("plan-card");
        let toggle = gtk4::Button::new();
        toggle.add_css_class("flat");
        toggle.add_css_class("plan-toggle");
        let head = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let icon = gtk4::Image::from_icon_name("at-view-list-bullet-symbolic");
        let title = cards::label("Plan", &["plan-title"]);
        title.set_hexpand(true);
        let chevron = gtk4::Image::from_icon_name("at-pan-down-symbolic");
        head.append(&icon);
        head.append(&title);
        head.append(&chevron);
        toggle.set_child(Some(&head));
        card.append(&toggle);
        let steps = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        steps.add_css_class("plan-steps");
        let body = gtk4::Revealer::new();
        body.set_reveal_child(true);
        body.set_child(Some(&steps));
        card.append(&body);
        toggle.connect_clicked(glib::clone!(
            #[weak]
            body,
            #[weak]
            chevron,
            move |_| {
                let open = !body.reveals_child();
                body.set_reveal_child(open);
                chevron.set_icon_name(Some(if open {
                    "at-pan-down-symbolic"
                } else {
                    "at-pan-up-symbolic"
                }));
            }
        ));
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

    /// GTK checks, run from the window test (GTK belongs to the one thread that initialised it).
    pub(crate) fn ui_checks() {
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
        let (title, openable) = state("withheld").flatten().expect("withheld row");
        assert!(title.contains("not shared"), "{title}");
        assert!(!openable, "nothing to open");
        let (title, openable) = state("shared").flatten().expect("shared row");
        assert_eq!(title, "Thought process");
        assert!(openable);
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
            })
            .request("r1"),
        );
        sink(
            &Envelope::new(Event::ApprovalRequested {
                tool: "Bash".into(),
                title: None,
                input: json!({"command": "rm -rf build"}),
                reason: None,
                options: vec![Decision::Allow],
                response: ResponseCapability::Live,
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
            })
            .request("r"),
            Driver::Claude,
        );
        assert_eq!(
            t.mark_approval_sent("r", Decision::Allow),
            vec![Change::Updated("approval:r".into())]
        );
    }
}
