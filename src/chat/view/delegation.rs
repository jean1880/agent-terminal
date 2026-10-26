//! Scoped worker reducers and inspectors. Child envelopes never enter `Inner::model`.
//! Native protocol request ids stay in a private lookup and only exact scoped actions leave it.
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::{Rc, Weak};

use agent_core::adapter::{Driver, Mode};
use agent_core::delegation::scope::{Scope, ScopedEnvelope};
use agent_core::delegation::wire::WorkerSummary;
use agent_core::delegation::WorkerId;
use agent_core::event::{Decision, Envelope, Event, WorkerDelegation, WorkerOrigin, WorkerState};
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;

use super::cards::{RowEvent, RowSink};
use super::model::{Activity, PendingInterruption, SubagentSummary, ToolStatus, Transcript};
use super::subagents::SubagentPanel;
use super::{ChatView, Inner};
use crate::chat::{DiffAsk, WorkerAction};
use crate::delegation::seam::WorkerSubscriber;

#[derive(Default)]
pub(super) struct Workers {
    root: Option<WorkerId>,
    records: Vec<Worker>,
    pending: VecDeque<(ScopedEnvelope, bool)>,
    pending_bytes: usize,
    overflowed: bool,
    /// The overflow notice is shown once per backlog, not once per dropped envelope.
    overflow_noticed: bool,
}

struct Worker {
    summary: WorkerSummary,
    histories: Vec<History>,
    canonical_scope: Option<Scope>,
}

struct History {
    scope: Scope,
    model: Transcript,
    identity: Option<agent_core::delegation::identity::Identity>,
    state: WorkerState,
    /// native → opaque, plus opaque → native for answers and diff requests.
    requests: HashMap<String, String>,
    native_requests: HashMap<String, String>,
    items: HashMap<String, String>,
    native_items: HashMap<String, String>,
}

impl History {
    fn new(scope: Scope) -> Self {
        Self {
            scope,
            model: Transcript::new(),
            identity: None,
            state: WorkerState::Unknown,
            requests: HashMap::new(),
            native_requests: HashMap::new(),
            items: HashMap::new(),
            native_items: HashMap::new(),
        }
    }

    fn item_id(&mut self, native: &str) -> String {
        if let Some(id) = self.items.get(native) {
            return id.clone();
        }
        let id = format!("delegated-item-{}", self.items.len());
        self.items.insert(native.to_owned(), id.clone());
        self.native_items.insert(id.clone(), native.to_owned());
        id
    }

    fn request_id(&mut self, native: &str) -> String {
        if let Some(id) = self.requests.get(native) {
            return id.clone();
        }
        let id = format!("delegated-request-{}", self.requests.len());
        self.requests.insert(native.to_owned(), id.clone());
        self.native_requests.insert(id.clone(), native.to_owned());
        id
    }

    fn apply(&mut self, envelope: &Envelope, driver: Driver) {
        match &envelope.event {
            Event::TurnStarted { .. } => self.state = WorkerState::Running,
            Event::TurnCompleted { state, .. } => {
                self.state = match state {
                    agent_core::event::TurnState::Completed => WorkerState::Completed,
                    agent_core::event::TurnState::Failed => WorkerState::Failed,
                    agent_core::event::TurnState::Interrupted => WorkerState::Stopped,
                }
            }
            Event::SessionExited { code, .. } => {
                self.state = if code.is_some_and(|code| code != 0) {
                    WorkerState::Failed
                } else {
                    WorkerState::Stopped
                }
            }
            _ => {}
        }
        let mut local = envelope.clone();
        local.raw = None;
        local.item = envelope.item.as_deref().map(|id| self.item_id(id));
        local.request = envelope.request.as_deref().map(|id| self.request_id(id));
        if let Event::ItemStarted { parent, .. } = &mut local.event {
            *parent = parent.as_deref().map(|id| self.item_id(id));
        }
        self.model.apply(&local, driver);
    }
}

impl Worker {
    fn live_scope(&self) -> Option<Scope> {
        if let Some(scope) = &self.canonical_scope {
            return Some(scope.clone());
        }
        let latest = self
            .histories
            .iter()
            .max_by_key(|history| history.scope.generation.0)?;
        Some(Scope {
            run: self.summary.run_id.clone(),
            ..latest.scope.clone()
        })
    }

    fn summary(&self) -> SubagentSummary {
        let identity = self.summary.identity.as_ref();
        let delegation = identity.map(|identity| WorkerDelegation {
            worker_id: self.summary.worker_id.clone(),
            origin: self.summary.origin,
            driver: identity.driver,
            profile_id: Some(identity.profile_id.clone()),
            parent: self.summary.parent_id.clone(),
            run: self.summary.run_id.clone(),
            depth: self.summary.depth,
            model: identity.model.clone(),
            effort: identity.effort.clone(),
            controls: self.summary.controls,
            status: self.summary.delegation.clone(),
        });
        SubagentSummary {
            id: self.summary.worker_id.as_str().to_owned(),
            name: self.summary.name.clone(),
            task: self
                .histories
                .iter()
                .flat_map(|history| history.model.user_messages())
                .next()
                .and_then(|text| text.lines().next().map(str::to_owned))
                .unwrap_or_default(),
            status: match self.summary.state {
                WorkerState::Starting
                | WorkerState::Running
                | WorkerState::Waiting
                | WorkerState::Stopping => ToolStatus::Running,
                WorkerState::Failed => ToolStatus::Failed,
                WorkerState::Stopped | WorkerState::Unknown => ToolStatus::Interrupted,
                WorkerState::Completed | WorkerState::Closed => ToolStatus::Completed,
            },
            steps: self
                .histories
                .iter()
                .map(|history| history.model.order().len())
                .sum(),
            lifecycle: Some(self.summary.state),
            activity: self.summary.last_activity.clone(),
            driver: identity.map(|identity| identity.driver),
            delegation,
            requested_model: None,
            requested_effort: None,
            reported_model: None,
        }
    }
}

impl Workers {
    pub(super) fn summaries(&self) -> Vec<SubagentSummary> {
        self.records.iter().map(Worker::summary).collect()
    }

    pub(super) fn has_worker(&self, id: &str) -> bool {
        self.worker(id).is_some()
    }

    fn live_histories(&self) -> impl Iterator<Item = (&Worker, &History)> {
        self.records.iter().filter_map(|worker| {
            let scope = worker.live_scope()?;
            worker
                .histories
                .iter()
                .find(|history| history.scope == scope)
                .map(|history| (worker, history))
        })
    }

    /// What a worker waits on the user for, as the parent's status says it: an approval
    /// (`NeedsApproval`) before a question or form (`NeedsAnswer`); `None` when none waits.
    /// A worker's requests count as its parent's own: the thread is held up either way.
    pub(super) fn needs_user(&self) -> Option<Activity> {
        let mut needs = None;
        for (_, history) in self.live_histories() {
            match history.model.pending_interruption() {
                Some(PendingInterruption::Approval { .. }) => return Some(Activity::NeedsApproval),
                Some(_) => needs = Some(Activity::NeedsAnswer),
                None => {}
            }
        }
        needs
    }

    fn worker(&self, id: &str) -> Option<&Worker> {
        self.records
            .iter()
            .find(|worker| worker.summary.worker_id.as_str() == id)
    }

    fn registry(&mut self, summaries: &[WorkerSummary]) {
        if let Some(root) = summaries
            .iter()
            .find(|summary| summary.origin == WorkerOrigin::Root)
        {
            if self.root.as_ref().is_some_and(|id| *id != root.worker_id) {
                return;
            }
            self.root = Some(root.worker_id.clone());
        }
        for summary in summaries
            .iter()
            .filter(|summary| summary.origin != WorkerOrigin::Root)
        {
            if let Some(worker) = self
                .records
                .iter_mut()
                .find(|worker| worker.summary.worker_id == summary.worker_id)
            {
                worker.summary = summary.clone();
                let live = worker.live_scope();
                for history in &mut worker.histories {
                    if Some(&history.scope) != live.as_ref()
                        || matches!(
                            summary.state,
                            WorkerState::Closed
                                | WorkerState::Stopped
                                | WorkerState::Unknown
                                | WorkerState::Failed
                        )
                    {
                        history.model.settle_stale();
                        history.model.mark_replayed();
                    }
                }
            } else {
                self.records.push(Worker {
                    summary: summary.clone(),
                    histories: Vec::new(),
                    canonical_scope: None,
                });
            }
        }
        let pending = std::mem::take(&mut self.pending);
        self.pending_bytes = 0;
        for (envelope, replayed) in pending {
            self.apply(&envelope, replayed);
        }
        if self.pending.is_empty() {
            self.overflow_noticed = false;
        }
    }

    /// Whether a drop since the last call still needs its notice.
    fn take_overflow_notice(&mut self) -> bool {
        let notice = self.overflowed && !self.overflow_noticed;
        self.overflow_noticed |= self.overflowed;
        self.overflowed = false;
        notice
    }

    fn set_scope(&mut self, worker: &WorkerId, scope: Option<Scope>) {
        if let Some(scope) = scope.as_ref() {
            if scope.worker != *worker || self.root.as_ref().is_some_and(|root| *root != scope.root)
            {
                return;
            }
            self.root.get_or_insert_with(|| scope.root.clone());
        }
        if let Some(record) = self
            .records
            .iter_mut()
            .find(|record| record.summary.worker_id == *worker)
        {
            record.canonical_scope = scope;
            let live = record.live_scope();
            for history in &mut record.histories {
                if Some(&history.scope) != live.as_ref() {
                    history.model.settle_stale();
                    history.model.mark_replayed();
                } else {
                    history.identity = record.summary.identity.clone();
                }
            }
        }
    }

    fn apply(&mut self, scoped: &ScopedEnvelope, replayed: bool) {
        if scoped.scope.worker == scoped.scope.root {
            return;
        }
        if self
            .root
            .as_ref()
            .is_some_and(|root| *root != scoped.scope.root)
        {
            return;
        }
        let index = self.records.iter().position(|worker| {
            worker.summary.worker_id == scoped.scope.worker && worker.summary.identity.is_some()
        });
        let Some(index) = index else {
            let bytes =
                serde_json::to_vec(&scoped.envelope).map_or(usize::MAX, |bytes| bytes.len());
            if self.pending.len() < 64
                && self.pending_bytes.saturating_add(bytes) <= 4 * 1024 * 1024
            {
                self.pending_bytes += bytes;
                self.pending.push_back((scoped.clone(), replayed));
            } else {
                self.overflowed = true;
            }
            return;
        };
        self.root.get_or_insert_with(|| scoped.scope.root.clone());
        let worker = &mut self.records[index];
        let Some(driver) = worker
            .summary
            .identity
            .as_ref()
            .map(|identity| identity.driver)
        else {
            return;
        };
        let at = worker
            .histories
            .iter()
            .position(|history| history.scope == scoped.scope)
            .unwrap_or_else(|| {
                let mut history = History::new(scoped.scope.clone());
                history.identity = worker.summary.identity.clone();
                worker.histories.push(history);
                worker.histories.len() - 1
            });
        worker.histories[at].apply(&scoped.envelope, driver);
        if replayed
            && matches!(
                worker.summary.state,
                WorkerState::Unknown
                    | WorkerState::Stopped
                    | WorkerState::Closed
                    | WorkerState::Failed
            )
        {
            worker.histories[at].model.settle_stale();
            worker.histories[at].model.mark_replayed();
        }
    }
}

struct Subscriber {
    root_thread: String,
    view: gtk4::glib::WeakRef<ChatView>,
}

impl WorkerSubscriber for Subscriber {
    fn scoped(&self, envelope: &ScopedEnvelope) {
        self.apply(envelope, false);
    }
    fn replayed(&self, envelope: &ScopedEnvelope) {
        self.apply(envelope, true);
    }
    fn registry(&self, root_thread: &str, workers: &[WorkerSummary]) {
        if root_thread != self.root_thread {
            return;
        }
        let Some(view) = self.view.upgrade() else {
            return;
        };
        let Some(inner) = view.imp().inner.borrow().clone() else {
            return;
        };
        let scopes: Vec<_> = workers
            .iter()
            .map(|worker| {
                (
                    worker.worker_id.clone(),
                    inner.backend.worker_scope(&worker.worker_id),
                )
            })
            .collect();
        let mut state = inner.delegation.borrow_mut();
        state.registry(workers);
        for (worker, scope) in scopes {
            state.set_scope(&worker, scope);
        }
        drop(state);
        inner.queue_worker_flush();
    }
}

impl Subscriber {
    fn apply(&self, envelope: &ScopedEnvelope, replayed: bool) {
        let Some(view) = self.view.upgrade() else {
            return;
        };
        let Some(inner) = view.imp().inner.borrow().clone() else {
            return;
        };
        let scope = inner.backend.worker_scope(&envelope.scope.worker);
        let mut state = inner.delegation.borrow_mut();
        state.set_scope(&envelope.scope.worker, scope);
        state.apply(envelope, replayed);
        drop(state);
        inner.queue_worker_flush();
        if inner.delegation.borrow_mut().take_overflow_notice() {
            inner.worker_notice(
                "Some worker activity could not be displayed because its registry was unavailable.",
            );
        }
    }
}

impl ChatView {
    pub(crate) fn delegation_subscriber(&self, root_thread: &str) -> Rc<dyn WorkerSubscriber> {
        Rc::new(Subscriber {
            root_thread: root_thread.to_owned(),
            view: self.downgrade(),
        })
    }
}

pub(super) struct Inspector {
    worker: String,
    panel: Rc<SubagentPanel>,
    history: gtk4::DropDown,
    scopes: RefCell<Vec<Scope>>,
    selected: RefCell<Option<Scope>>,
    /// What the controls last showed. A click never retargets itself to a replacement run.
    action_scope: RefCell<Option<Scope>>,
    updating: Cell<bool>,
    follow_latest: Cell<bool>,
    entry: gtk4::Entry,
    send: gtk4::Button,
    cancel: gtk4::Button,
    close: gtk4::Button,
    state: gtk4::Label,
}

impl Inspector {
    fn new(inner: &Rc<Inner>, worker: &str) -> Rc<Self> {
        let weak = Rc::downgrade(inner);
        let id = worker.to_owned();
        let sink: RowSink = Rc::new(move |event| {
            if let Some(inner) = weak.upgrade() {
                inner.worker_row(&id, event);
            }
        });
        let panel = SubagentPanel::new(worker, sink);
        let weak = Rc::downgrade(inner);
        panel.set_refresher(move || {
            if let Some(inner) = weak.upgrade() {
                inner.refresh_worker_inspector();
            }
        });
        let history = gtk4::DropDown::from_strings(&[]);
        history.set_tooltip_text(Some("Worker run and process history"));
        let state = gtk4::Label::new(None);
        state.set_xalign(0.0);
        state.set_wrap(true);
        let entry = gtk4::Entry::new();
        entry.set_placeholder_text(Some("Send this worker a message"));
        entry.set_hexpand(true);
        let send = gtk4::Button::with_label("Send");
        let cancel = gtk4::Button::with_label("Interrupt run");
        let close = gtk4::Button::with_label("Close worker");
        let actions = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        actions.append(&entry);
        actions.append(&send);
        let controls = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        controls.append(&cancel);
        controls.append(&close);
        panel.add_controls(&history);
        panel.add_controls(&state);
        panel.add_controls(&actions);
        panel.add_controls(&controls);
        let inspector = Rc::new(Self {
            worker: worker.to_owned(),
            panel,
            history,
            scopes: RefCell::new(Vec::new()),
            selected: RefCell::new(None),
            action_scope: RefCell::new(None),
            updating: Cell::new(false),
            follow_latest: Cell::new(true),
            entry,
            send,
            cancel,
            close,
            state,
        });
        let weak = Rc::downgrade(inner);
        let own = Rc::downgrade(&inspector);
        inspector.history.connect_selected_notify(move |dropdown| {
            let (Some(inner), Some(inspector)) = (weak.upgrade(), own.upgrade()) else {
                return;
            };
            if inspector.updating.get() {
                return;
            }
            inspector.follow_latest.set(false);
            *inspector.selected.borrow_mut() = inspector
                .scopes
                .borrow()
                .get(dropdown.selected() as usize)
                .cloned();
            inspector.panel.reset_steps();
            inner.refresh_worker_inspector();
        });
        for (button, kind) in [
            (&inspector.send, 0),
            (&inspector.cancel, 1),
            (&inspector.close, 2),
        ] {
            let weak = Rc::downgrade(inner);
            let own = Rc::downgrade(&inspector);
            button.connect_clicked(move |_| {
                let (Some(inner),Some(inspector)) = (weak.upgrade(), own.upgrade()) else { return };
                let scope = inspector.action_scope.borrow().clone();
                let Some(scope) = scope else { inner.worker_notice("This worker has no live process scope."); return };
                let action = match kind {
                    0 => {
                        let text = inspector.entry.text().trim().to_owned();
                        if text.is_empty() { return; }
                        WorkerAction::Send { scope, text }
                    }
                    1 => WorkerAction::Cancel { scope },
                    _ => WorkerAction::Close { scope },
                };
                if inner.backend.worker_action(action) { if kind == 0 { inspector.entry.set_text(""); } }
                else { inner.worker_notice("The worker changed or this action is unavailable. Refresh its current run."); }
            });
        }
        inspector
    }
}

impl Inner {
    /// Keep the root's request first, then a live worker's. The scope is the one displayed,
    /// never inferred from a request id (which another worker can also use).
    pub(super) fn refresh_interruption(&self) {
        let model = self.model.borrow();
        let workers = self.delegation.borrow();
        let waiting: usize = workers
            .live_histories()
            .map(|(_, history)| history.model.pending_count())
            .sum();
        let root = model.pending_interruption();
        if root.is_some() || waiting == 0 {
            *self.interruption_scope.borrow_mut() = None;
            if waiting == 0 {
                self.interruption.update(&model);
            } else {
                self.interruption
                    .update_pending(root, model.pending_count() + waiting, None);
            }
            return;
        }
        let child = workers.live_histories().find_map(|(worker, history)| {
            history
                .model
                .pending_interruption()
                .map(|pending| (worker, history, pending))
        });
        if let Some((worker, history, pending)) = child {
            *self.interruption_scope.borrow_mut() = Some(history.scope.clone());
            self.interruption
                .update_pending(Some(pending), waiting, Some(&worker.summary.name));
        }
    }

    /// Dangerous permissive mode approves whatever still reaches the thread, its live workers'
    /// approvals included: each is answered Allow (once, no rule) on the shelf's own route, so
    /// the worker re-checks its inherited authority at the answer and still denies what that
    /// forbids. Questions and forms, and a card that offers no Allow, still wait for the user.
    /// Called deferred, never while the supervisor publishes.
    fn allow_worker_requests(self: &Rc<Self>) {
        if self.backend.status().mode != Mode::Bypass {
            return;
        }
        let pending: Vec<(Scope, String)> = self
            .delegation
            .borrow()
            .live_histories()
            .flat_map(|(_, history)| {
                history
                    .model
                    .pending_approvals_offering(Decision::Allow)
                    .into_iter()
                    .map(|request| (history.scope.clone(), request))
            })
            .collect();
        // One attempt per request: one the worker could not take stays on the shelf.
        self.worker_auto_allowed
            .borrow_mut()
            .retain(|tried| pending.contains(tried));
        for (scope, request) in pending {
            if !self
                .worker_auto_allowed
                .borrow_mut()
                .insert((scope.clone(), request.clone()))
            {
                continue;
            }
            tracing::info!("dangerous permissive mode: allowing a delegated agent's request");
            self.worker_request(
                &scope,
                RowEvent::Approve {
                    request,
                    decision: Decision::Allow,
                    custom_rule: None,
                },
            );
        }
    }

    pub(super) fn interruption_row(self: &Rc<Self>, event: RowEvent) {
        let scope = self.interruption_scope.borrow().clone();
        if let Some(scope) = scope {
            self.worker_request(&scope, event);
        } else {
            self.row_event(event);
        }
    }

    pub(super) fn jump_interruption(self: &Rc<Self>, id: &str) {
        let scope = self.interruption_scope.borrow().clone();
        let Some(scope) = scope else {
            self.transcript.focus_pending_card(id);
            return;
        };
        // A queued jump must not land in a replacement process with the same local ids.
        if !self
            .delegation
            .borrow()
            .worker(scope.worker.as_str())
            .is_some_and(|worker| worker.live_scope().as_ref() == Some(&scope))
        {
            return;
        }
        if self.open_worker(scope.worker.as_str()) {
            if let Some(inspector) = self.worker_inspector.borrow().clone() {
                *inspector.selected.borrow_mut() = Some(scope);
                inspector.follow_latest.set(false);
                self.refresh_worker_inspector();
                inspector.panel.focus_pending_card(id);
            }
        }
    }

    pub(super) fn queue_worker_flush(self: &Rc<Self>) {
        if self.worker_flush_queued.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        gtk4::glib::idle_add_local_once(move || {
            if let Some(inner) = weak.upgrade() {
                inner.worker_flush_queued.set(false);
                // A worker's run starting or ending changes the parent's status and dots too.
                inner.refresh_status();
                inner.refresh_subagents(&[]);
                inner.refresh_worker_inspector();
                inner.allow_worker_requests();
            }
        });
    }

    fn worker_notice(&self, message: &str) {
        if let Some(overlay) = self.widget.upgrade().and_then(|view| view.toast_overlay()) {
            overlay.add_toast(adw::Toast::new(message));
        }
    }

    pub(super) fn open_worker(self: &Rc<Self>, id: &str) -> bool {
        if self.delegation.borrow().worker(id).is_none() {
            return false;
        }
        let inspector = Inspector::new(self, id);
        *self.worker_inspector.borrow_mut() = Some(inspector.clone());
        self.refresh_worker_inspector();
        let weak = Rc::downgrade(self);
        let own = Rc::downgrade(&inspector);
        adw::prelude::AdwDialogExt::connect_closed(inspector.panel.dialog(), move |_| {
            if let (Some(inner), Some(closed)) = (weak.upgrade(), own.upgrade()) {
                if inner
                    .worker_inspector
                    .borrow()
                    .as_ref()
                    .is_some_and(|open| Rc::ptr_eq(open, &closed))
                {
                    inner.worker_inspector.borrow_mut().take();
                }
            }
        });
        self.present_dialog(inspector.panel.dialog());
        true
    }

    pub(super) fn refresh_worker_inspector(&self) {
        let Some(inspector) = self.worker_inspector.borrow().clone() else {
            return;
        };
        let workers = self.delegation.borrow();
        let Some(worker) = workers.worker(&inspector.worker) else {
            return;
        };
        let scopes: Vec<_> = worker
            .histories
            .iter()
            .map(|history| history.scope.clone())
            .collect();
        let live = worker.live_scope();
        let preferred = live
            .as_ref()
            .and_then(|live| {
                scopes
                    .iter()
                    .find(|scope| *scope == live && scope.run.is_some())
                    .cloned()
            })
            .or_else(|| {
                live.as_ref().and_then(|live| {
                    scopes
                        .iter()
                        .rev()
                        .find(|scope| scope.generation == live.generation && scope.run.is_some())
                        .cloned()
                })
            })
            .or_else(|| scopes.last().cloned());
        let previous = inspector.selected.borrow().clone();
        let selected = inspector
            .selected
            .borrow()
            .clone()
            .filter(|scope| !inspector.follow_latest.get() && scopes.contains(scope))
            .or_else(|| preferred.clone());
        if selected != previous {
            inspector.panel.reset_steps();
        }
        if *inspector.scopes.borrow() != scopes {
            let labels: Vec<_> = scopes
                .iter()
                .map(|scope| {
                    format!(
                        "Process {} · {}",
                        scope.generation.0,
                        scope
                            .run
                            .as_ref()
                            .map(|run| format!("Run {}", run.as_str()))
                            .unwrap_or_else(|| "Startup and shutdown".to_owned())
                    )
                })
                .collect();
            let labels: Vec<_> = labels.iter().map(String::as_str).collect();
            inspector.updating.set(true);
            inspector
                .history
                .set_model(Some(&gtk4::StringList::new(&labels)));
            inspector.history.set_selected(
                selected
                    .as_ref()
                    .and_then(|selected| scopes.iter().position(|scope| scope == selected))
                    .map_or(gtk4::INVALID_LIST_POSITION, |index| index as u32),
            );
            inspector.updating.set(false);
            *inspector.scopes.borrow_mut() = scopes;
        }
        let selected_index = selected
            .as_ref()
            .and_then(|selected| {
                inspector
                    .scopes
                    .borrow()
                    .iter()
                    .position(|scope| scope == selected)
            })
            .map_or(gtk4::INVALID_LIST_POSITION, |index| index as u32);
        if inspector.history.selected() != selected_index {
            inspector.updating.set(true);
            inspector.history.set_selected(selected_index);
            inspector.updating.set(false);
        }
        *inspector.selected.borrow_mut() = selected.clone();
        let current = (inspector.scopes.borrow().is_empty() && live.is_some())
            || (selected.is_some()
                && selected == preferred
                && selected
                    .as_ref()
                    .zip(live.as_ref())
                    .is_some_and(|(selected, live)| selected.generation == live.generation));
        *inspector.action_scope.borrow_mut() = if current { live } else { None };
        inspector
            .send
            .set_sensitive(current && worker.summary.controls.send);
        inspector
            .cancel
            .set_sensitive(current && worker.summary.controls.cancel);
        inspector
            .close
            .set_sensitive(current && worker.summary.controls.close);
        inspector
            .entry
            .set_sensitive(current && worker.summary.controls.send);
        inspector.state.set_text(&format!(
            "{}{}",
            worker
                .summary
                .delegation
                .reason
                .as_deref()
                .unwrap_or("Supervised worker"),
            if current {
                ""
            } else {
                " · historical run; controls unavailable"
            }
        ));
        if let Some(history) = worker
            .histories
            .iter()
            .find(|history| Some(&history.scope) == selected.as_ref())
        {
            let changed = history.model.all_item_ids();
            let mut summary = worker.summary();
            if !current {
                summary.lifecycle = Some(history.state);
                summary.steps = history.model.all_item_ids().len();
                if let (Some(delegation), Some(identity)) =
                    (&mut summary.delegation, &history.identity)
                {
                    delegation.driver = identity.driver;
                    delegation.profile_id = Some(identity.profile_id.clone());
                    delegation.model = identity.model.clone();
                    delegation.effort = identity.effort.clone();
                    summary.driver = Some(identity.driver);
                }
            }
            inspector
                .panel
                .refresh_scoped(&history.model, &summary, &changed);
        } else {
            inspector
                .panel
                .refresh_scoped(&Transcript::new(), &worker.summary(), &[]);
        }
    }

    fn worker_row(self: &Rc<Self>, worker_id: &str, event: RowEvent) {
        let inspector = self.worker_inspector.borrow().clone();
        let Some(inspector) = inspector.filter(|inspector| inspector.worker == worker_id) else {
            return;
        };
        let selected = inspector.selected.borrow().clone();
        let Some(scope) = selected else { return };
        if let RowEvent::Toggle { id, expanded } = event {
            if let Some(worker) = self
                .delegation
                .borrow_mut()
                .records
                .iter_mut()
                .find(|worker| worker.summary.worker_id.as_str() == worker_id)
            {
                if let Some(history) = worker
                    .histories
                    .iter_mut()
                    .find(|history| history.scope == scope)
                {
                    history.model.set_expanded(&id, expanded);
                }
            }
            self.refresh_worker_inspector();
            return;
        }
        if matches!(event, RowEvent::LoadDiff { .. } | RowEvent::OpenDiff { .. }) {
            self.worker_diff(&inspector, &scope, event);
            return;
        }
        self.worker_request(&scope, event);
    }

    fn worker_request(self: &Rc<Self>, scope: &Scope, event: RowEvent) {
        let worker_id = scope.worker.as_str();
        let native = |request: &str| {
            self.delegation
                .borrow()
                .worker(worker_id)
                .filter(|worker| worker.live_scope().as_ref() == Some(scope))
                .and_then(|worker| {
                    worker
                        .histories
                        .iter()
                        .find(|history| &history.scope == scope)
                })
                .and_then(|history| history.native_requests.get(request).cloned())
        };
        let (request, action) = match event {
            RowEvent::Approve {
                request,
                decision,
                custom_rule,
            } => {
                let Some(native) = native(&request) else {
                    self.worker_notice("This approval belongs to a previous worker run.");
                    return;
                };
                (
                    request,
                    WorkerAction::Approve {
                        scope: scope.clone(),
                        request: native,
                        decision,
                        custom_rule,
                    },
                )
            }
            RowEvent::Answer { request, answers } => {
                let Some(native) = native(&request) else {
                    self.worker_notice("This question belongs to a previous worker run.");
                    return;
                };
                (
                    request,
                    WorkerAction::Answer {
                        scope: scope.clone(),
                        request: native,
                        answers,
                    },
                )
            }
            RowEvent::Form {
                request,
                action,
                content,
            } => {
                let Some(native) = native(&request) else {
                    self.worker_notice("This form belongs to a previous worker run.");
                    return;
                };
                (
                    request,
                    WorkerAction::Form {
                        scope: scope.clone(),
                        request: native,
                        action,
                        content,
                    },
                )
            }
            _ => return,
        };
        // Clone just the small local update discriminator before handing ownership away.
        let mark = match &action {
            WorkerAction::Approve { decision, .. } => Some(*decision),
            _ => None,
        };
        let is_form = matches!(&action, WorkerAction::Form { .. });
        if !self.backend.worker_action(action) {
            self.worker_notice("This worker request is no longer answerable.");
            return;
        }
        if let Some(worker) = self
            .delegation
            .borrow_mut()
            .records
            .iter_mut()
            .find(|worker| worker.summary.worker_id.as_str() == worker_id)
        {
            if let Some(history) = worker
                .histories
                .iter_mut()
                .find(|history| &history.scope == scope)
            {
                if let Some(decision) = mark {
                    history.model.mark_approval_sent(&request, decision);
                } else if is_form {
                    history.model.mark_form_sent(&request);
                } else {
                    history.model.mark_questions_sent(&request);
                }
            }
        }
        self.refresh_worker_inspector();
        self.refresh_status();
    }

    fn worker_diff(self: &Rc<Self>, inspector: &Rc<Inspector>, scope: &Scope, event: RowEvent) {
        let (id, external) = match event {
            RowEvent::LoadDiff { id } => (id, false),
            RowEvent::OpenDiff { id } => (id, true),
            _ => return,
        };
        let ask = {
            let workers = self.delegation.borrow();
            workers
                .worker(&inspector.worker)
                .and_then(|worker| {
                    worker
                        .histories
                        .iter()
                        .find(|history| history.scope == *scope)
                })
                .and_then(|history| {
                    let item = history.model.get(&id)?;
                    let super::model::Body::Tool(tool) = &item.body else {
                        return None;
                    };
                    Some(DiffAsk {
                        item: history.native_items.get(&id)?.clone(),
                        input: tool.input.clone().unwrap_or_else(|| {
                            serde_json::from_str(&tool.input_text)
                                .unwrap_or(serde_json::Value::Null)
                        }),
                    })
                })
        };
        let source = self.backend.worker_diff_source(scope);
        let (Some(ask), Some(source)) = (ask, source) else {
            inspector.panel.show_diff(
                &id,
                &Err("No diff is available for this worker's recorded turn.".to_owned()),
            );
            return;
        };
        if external {
            source.open_external(ask);
            return;
        }
        let weak: Weak<Inspector> = Rc::downgrade(inspector);
        let scope = scope.clone();
        source.load(
            ask,
            Box::new(move |reply| {
                if let Some(inspector) = weak.upgrade() {
                    if inspector.selected.borrow().as_ref() == Some(&scope) {
                        inspector.panel.show_diff(&id, &reply);
                    }
                }
            }),
        );
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use agent_core::delegation::identity::Identity;
    use agent_core::delegation::wire::{
        Controls, DelegationStatus, DelegationStatusCode, ThisRunSupport, WorkspaceInfo,
        WorkspaceKind,
    };
    use agent_core::delegation::{ProcessGeneration, ProfileId, RunId};
    use agent_core::event::{Decision, ItemKind, ResponseCapability, StreamKind, TurnState};
    use serde_json::json;

    fn scope(worker: &str, generation: u64, run: &str) -> Scope {
        Scope {
            root: WorkerId::parse("w_rootroot").expect("root"),
            worker: WorkerId::parse(worker).expect("worker"),
            generation: ProcessGeneration(generation),
            run: Some(RunId::parse(run).expect("run")),
        }
    }

    fn summary(worker: &str, run: &str) -> WorkerSummary {
        WorkerSummary {
            worker_id: WorkerId::parse(worker).expect("worker"),
            parent_id: Some(WorkerId::parse("w_rootroot").expect("root")),
            name: "Builder".into(),
            origin: WorkerOrigin::Supervised,
            state: WorkerState::Running,
            run_id: Some(RunId::parse(run).expect("run")),
            run_state: None,
            identity: Some(Identity::new(
                Driver::Codex,
                ProfileId::parse("test").expect("profile"),
            )),
            depth: 1,
            can_spawn: false,
            controls: Controls {
                send: true,
                this_run: ThisRunSupport::Strict,
                cancel: true,
                close: true,
            },
            task_mode: None,
            workspace: WorkspaceInfo {
                kind: WorkspaceKind::SharedReadOnly,
                path: None,
                base_commit: None,
            },
            delegation: DelegationStatus::new(DelegationStatusCode::Available),
            last_activity: None,
            updated_at: "2026-10-15T00:00:00Z".into(),
        }
    }

    fn feed(workers: &mut Workers, scope: &Scope, envelope: Envelope) {
        workers.apply(
            &ScopedEnvelope {
                scope: scope.clone(),
                envelope,
            },
            false,
        );
    }

    #[test]
    fn child_turn_usage_and_identical_native_items_stay_inside_their_scope() {
        let mut root = Transcript::new();
        root.apply(
            &Envelope::new(Event::TurnStarted {
                model: Some("root-model".into()),
            }),
            Driver::Claude,
        );
        root.apply(
            &Envelope::new(Event::UsageUpdated {
                used: 42,
                max: Some(100),
                auto_compact_at: None,
            }),
            Driver::Claude,
        );
        let mut workers = Workers::default();
        workers.registry(&[
            summary("w_workerAA", "r_firstRun"),
            summary("w_workerBB", "r_secondRun"),
        ]);
        for (worker, run, text) in [
            ("w_workerAA", "r_firstRun", "first"),
            ("w_workerBB", "r_secondRun", "second"),
        ] {
            let scope = scope(worker, 1, run);
            feed(
                &mut workers,
                &scope,
                Envelope::item_started(
                    "same-native-item",
                    ItemKind::AssistantMessage,
                    "",
                    None,
                    None,
                ),
            );
            feed(
                &mut workers,
                &scope,
                Envelope::delta("same-native-item", StreamKind::Assistant, text),
            );
            feed(
                &mut workers,
                &scope,
                Envelope::new(Event::UsageUpdated {
                    used: 999,
                    max: Some(1000),
                    auto_compact_at: None,
                }),
            );
            feed(
                &mut workers,
                &scope,
                Envelope::turn_completed(TurnState::Completed, None),
            );
        }
        assert!(
            root.running,
            "a child's completion never completes the root turn"
        );
        assert_eq!(root.gauge.as_ref().expect("gauge").used, 42);
        assert!(root.order().is_empty());
        for (worker, text) in [("w_workerAA", "first"), ("w_workerBB", "second")] {
            let history = &workers.worker(worker).expect("worker").histories[0];
            let item = history
                .model
                .get(history.items.get("same-native-item").expect("opaque item"))
                .expect("item");
            assert!(format!("{:?}", item.body).contains(text));
            assert_eq!(history.native_items[&item.id], "same-native-item");
        }
    }

    #[test]
    fn generation_history_keeps_requests_opaque_and_old_cards_expire() {
        let mut workers = Workers::default();
        workers.registry(&[summary("w_workerAA", "r_firstRun")]);
        let approval = || {
            Envelope::new(Event::ApprovalRequested {
                tool: "shell".into(),
                title: None,
                input: json!({"command":"true"}),
                reason: None,
                options: vec![Decision::Allow, Decision::Deny],
                response: ResponseCapability::Live,
                remembers: None,
            })
            .request("native-request-never-a-root-request")
        };
        feed(
            &mut workers,
            &scope("w_workerAA", 1, "r_firstRun"),
            approval(),
        );
        workers.registry(&[summary("w_workerAA", "r_secondRun")]);
        feed(
            &mut workers,
            &scope("w_workerAA", 2, "r_secondRun"),
            approval(),
        );
        workers.registry(&[summary("w_workerAA", "r_secondRun")]);
        let worker = workers.worker("w_workerAA").expect("worker");
        assert_eq!(worker.histories.len(), 2, "rebind history is retained");
        assert_eq!(
            worker.live_scope(),
            Some(scope("w_workerAA", 2, "r_secondRun"))
        );
        for history in &worker.histories {
            let request = &history.requests["native-request-never-a-root-request"];
            assert!(request.starts_with("delegated-request-"));
            let item = history
                .model
                .get(&format!("approval:{request}"))
                .expect("approval");
            let super::super::model::Body::Approval(approval) = &item.body else {
                panic!("approval")
            };
            if history.scope.generation.0 == 1 {
                assert_eq!(approval.state, super::super::model::ApprovalState::Expired);
            } else {
                assert_eq!(approval.state, super::super::model::ApprovalState::Pending);
            }
        }
    }

    #[test]
    fn an_overflowing_backlog_gives_one_notice_until_the_registry_drains_it() {
        let mut workers = Workers::default();
        let scoped = scope("w_workerAA", 1, "r_firstRun");
        let mut notices = 0;
        for _ in 0..200 {
            feed(
                &mut workers,
                &scoped,
                Envelope::new(Event::TurnStarted { model: None }),
            );
            notices += usize::from(workers.take_overflow_notice());
        }
        assert_eq!(notices, 1);
        workers.registry(&[summary("w_workerAA", "r_firstRun")]);
        assert!(workers.pending.is_empty());
        assert!(!workers.take_overflow_notice());
        let other = scope("w_workerBB", 1, "r_firstRun");
        for _ in 0..65 {
            feed(
                &mut workers,
                &other,
                Envelope::new(Event::TurnStarted { model: None }),
            );
        }
        assert!(
            workers.take_overflow_notice(),
            "a new backlog is told again"
        );
    }

    #[test]
    fn out_of_order_registry_events_are_bounded_and_wrong_roots_are_refused() {
        let mut workers = Workers::default();
        let scoped = scope("w_workerAA", 1, "r_firstRun");
        for _ in 0..65 {
            feed(
                &mut workers,
                &scoped,
                Envelope::new(Event::TurnStarted { model: None }),
            );
        }
        assert_eq!(workers.pending.len(), 64);
        assert!(workers.overflowed);
        workers.registry(&[summary("w_workerAA", "r_firstRun")]);
        assert!(workers.pending.is_empty());
        let mut wrong = scoped.clone();
        wrong.root = WorkerId::parse("w_otherroo").expect("root");
        feed(
            &mut workers,
            &wrong,
            Envelope::turn_completed(TurnState::Completed, None),
        );
        assert!(
            workers.worker("w_workerAA").expect("worker").histories[0]
                .model
                .running
        );
    }

    #[test]
    fn queued_worker_scope_comes_only_from_the_backend() {
        let mut workers = Workers::default();
        workers.registry(&[summary("w_workerAA", "r_firstRun")]);
        assert!(workers
            .worker("w_workerAA")
            .expect("worker")
            .live_scope()
            .is_none());
        let authoritative = scope("w_workerAA", 0, "r_firstRun");
        let worker = authoritative.worker.clone();
        workers.set_scope(&worker, Some(scope("w_workerBB", 0, "r_firstRun")));
        assert!(workers
            .worker("w_workerAA")
            .expect("worker")
            .live_scope()
            .is_none());
        workers.set_scope(&worker, Some(authoritative.clone()));
        assert_eq!(
            workers.worker("w_workerAA").expect("worker").live_scope(),
            Some(authoritative)
        );
        assert!(workers
            .worker("w_workerAA")
            .expect("worker")
            .histories
            .is_empty());
    }

    struct Backend {
        actions: RefCell<Vec<WorkerAction>>,
        diff_scopes: RefCell<Vec<Scope>>,
        root_answers: Cell<u32>,
        mode: Cell<Mode>,
    }

    impl crate::chat::ChatBackend for Backend {
        fn worker_action(&self, action: WorkerAction) -> bool {
            self.actions.borrow_mut().push(action);
            true
        }
        fn worker_diff_source(&self, scope: &Scope) -> Option<Rc<dyn crate::chat::DiffSource>> {
            self.diff_scopes.borrow_mut().push(scope.clone());
            None
        }
        fn send_prompt(&self, _: &str) {}
        fn interrupt(&self) {}
        fn respond_approval(&self, _: &str, _: Decision) {
            self.root_answers.set(self.root_answers.get() + 1);
        }
        fn answer_questions(&self, _: &str, _: serde_json::Value) {
            self.root_answers.set(self.root_answers.get() + 1);
        }
        fn switch(&self, _: Driver, _: Option<String>, _: Option<String>) {}
        fn reload_session(&self) {}
        fn set_mode(&self, _: agent_core::adapter::Mode) {}
        fn control(&self, _: agent_core::adapter::Control) -> String {
            String::new()
        }
        fn status(&self) -> crate::chat::SessionStatus {
            let mut status = super::super::tests::status_of(Driver::Claude);
            status.mode = self.mode.get();
            status
        }
    }

    /// Called by the existing construction smoke test on its private display.
    pub(crate) fn ui_checks() {
        worker_shelf_ui_checks();
        bypass_allows_worker_requests_ui_checks();
        let backend = Rc::new(Backend {
            actions: RefCell::new(Vec::new()),
            diff_scopes: RefCell::new(Vec::new()),
            root_answers: Cell::new(0),
            mode: Cell::new(Mode::Ask),
        });
        let view = ChatView::new(backend.clone());
        let subscriber = view.delegation_subscriber("root-thread");
        subscriber.registry("unrelated", &[summary("w_workerAA", "r_firstRun")]);
        let inner = view.inner().expect("inner");
        assert!(inner.delegation.borrow().summaries().is_empty());
        subscriber.registry("root-thread", &[summary("w_workerAA", "r_firstRun")]);
        let scope1 = scope("w_workerAA", 1, "r_firstRun");
        subscriber.scoped(&ScopedEnvelope {
            scope: scope1.clone(),
            envelope: Envelope::item_started(
                "native-edit",
                ItemKind::FileChange,
                "Edit child file",
                Some(json!({"file_path":"child.rs"})),
                None,
            ),
        });
        subscriber.scoped(&ScopedEnvelope {
            scope: scope1.clone(),
            envelope: Envelope::new(Event::ApprovalRequested {
                tool: "Edit".into(),
                title: None,
                input: json!({"file_path":"child.rs"}),
                reason: None,
                options: vec![Decision::Allow, Decision::Deny],
                response: ResponseCapability::Live,
                remembers: None,
            })
            .request("native-permission")
            .item("native-edit"),
        });
        inner.refresh_subagents(&[]);
        // A worker's request holds up its parent as the parent's own would, and the worker's
        // agent is lit beside the parent's.
        inner.refresh_status();
        assert_eq!(inner.thinking.shown().as_deref(), Some("Needs approval"));
        assert!(
            inner.interruption.revealer.reveals_child(),
            "a child's approval must reach the parent's pinned shelf"
        );
        assert_eq!(inner.thinking.dot_states(), ["lead", "dimmed", "working"]);
        assert!(inner.open_worker("w_workerAA"));
        let inspector = inner.worker_inspector.borrow().clone().expect("inspector");
        assert_eq!(
            inspector.panel.steps_shown(),
            2,
            "same file-change and approval cards"
        );
        assert_eq!(
            inner.subagent_button.state().2,
            ["w_workerAA"],
            "one canonical worker row"
        );
        let (request, item) = {
            let workers = inner.delegation.borrow();
            let history = &workers.worker("w_workerAA").expect("worker").histories[0];
            (
                history.requests["native-permission"].clone(),
                history.items["native-edit"].clone(),
            )
        };
        inner.worker_row(
            "w_workerAA",
            RowEvent::Approve {
                request,
                decision: Decision::Allow,
                custom_rule: None,
            },
        );
        assert!(
            matches!(backend.actions.borrow().last(),Some(WorkerAction::Approve{scope,request,..}) if scope==&scope1&&request=="native-permission")
        );
        assert_eq!(backend.root_answers.get(), 0);
        inner.worker_row("w_workerAA", RowEvent::LoadDiff { id: item });
        assert_eq!(
            backend.diff_scopes.borrow().as_slice(),
            std::slice::from_ref(&scope1)
        );
        inspector.entry.set_text("Continue the child task");
        inspector.send.emit_clicked();
        assert!(
            matches!(backend.actions.borrow().last(),Some(WorkerAction::Send{scope,text}) if scope==&scope1&&text=="Continue the child task")
        );
        subscriber.registry("root-thread", &[summary("w_workerAA", "r_secondRun")]);
        let scope2 = scope("w_workerAA", 2, "r_secondRun");
        subscriber.scoped(&ScopedEnvelope {
            scope: scope2.clone(),
            envelope: Envelope::new(Event::TurnStarted { model: None }),
        });
        inner.refresh_worker_inspector();
        assert_eq!(*inspector.selected.borrow(), Some(scope2));
        assert_eq!(inspector.scopes.borrow().len(), 2);
        inspector.history.set_selected(0);
        assert_eq!(*inspector.selected.borrow(), Some(scope1));
        assert!(
            !inspector.send.is_sensitive(),
            "history cannot send into the replacement run"
        );
        assert!(inspector.action_scope.borrow().is_none());
        let count = backend.actions.borrow().len();
        inspector.send.emit_clicked();
        assert_eq!(
            backend.actions.borrow().len(),
            count,
            "even a queued click cannot retarget a historical inspector"
        );
        assert!(
            inner.model.borrow().order().is_empty(),
            "child rows never enter root transcript"
        );
        // The parent's own turn is not running, but its worker's is: the parent waits on it.
        inner.refresh_status();
        assert_eq!(
            inner.thinking.shown().as_deref(),
            Some("Waiting on 1 background task…")
        );
        assert_eq!(inner.thinking.dot_states(), ["lead", "dimmed", "working"]);
        // Between runs a worker is idle: its agent dims, and nothing holds the parent.
        let mut idle = summary("w_workerAA", "r_secondRun");
        idle.run_id = None;
        idle.state = WorkerState::Waiting;
        subscriber.registry("root-thread", &[idle]);
        inner.refresh_status();
        assert_eq!(inner.thinking.shown(), None);
    }

    fn worker_shelf_ui_checks() {
        let backend = Rc::new(Backend {
            actions: RefCell::new(Vec::new()),
            diff_scopes: RefCell::new(Vec::new()),
            root_answers: Cell::new(0),
            mode: Cell::new(Mode::Ask),
        });
        let view = ChatView::new(backend.clone());
        let subscriber = view.delegation_subscriber("root-thread");
        let inner = view.inner().expect("inner");
        let mut first = summary("w_workerAA", "r_firstRun");
        first.name = "First worker".into();
        let mut second = summary("w_workerBB", "r_firstRun");
        second.name = "Second worker".into();
        subscriber.registry("root-thread", &[first.clone(), second.clone()]);
        let first_scope = scope("w_workerAA", 1, "r_firstRun");
        let second_scope = scope("w_workerBB", 1, "r_firstRun");
        let approval = |request: &str| {
            Envelope::new(Event::ApprovalRequested {
                tool: "Bash".into(),
                title: None,
                input: json!({"command":"true"}),
                reason: None,
                options: vec![Decision::Allow, Decision::Deny],
                response: ResponseCapability::Live,
                remembers: None,
            })
            .request(request)
        };
        for (scope, request) in [
            (&first_scope, "same-id"),
            (&first_scope, "later"),
            (&second_scope, "same-id"),
        ] {
            subscriber.scoped(&ScopedEnvelope {
                scope: scope.clone(),
                envelope: approval(request),
            });
        }
        // Root and child request ids can collide. Root requests retain their existing route.
        view.apply(&approval("delegated-request-1"));
        assert!(inner.interruption_scope.borrow().is_none());
        assert_eq!(inner.interruption.shown().1, 4);
        inner.interruption.allow_btn.emit_clicked();
        assert_eq!(backend.root_answers.get(), 1);
        assert!(backend.actions.borrow().is_empty());
        assert_eq!(
            inner.interruption_scope.borrow().as_ref(),
            Some(&first_scope)
        );
        assert_eq!(
            inner.interruption.shown(),
            ("First worker: Approval required: Bash".into(), 3)
        );

        // The shelf answers without an inspector being open and advances to the next request.
        assert!(inner.worker_inspector.borrow().is_none());
        inner.interruption.allow_btn.emit_clicked();
        assert!(
            matches!(backend.actions.borrow().last(), Some(WorkerAction::Approve {scope, request, decision: Decision::Allow, ..}) if scope == &first_scope && request == "later")
        );
        assert_eq!(inner.interruption.shown().1, 2);
        inner.interruption.jump_btn.emit_clicked();
        let inspector = inner
            .worker_inspector
            .borrow()
            .clone()
            .expect("review opens worker");
        assert_eq!(*inspector.selected.borrow(), Some(first_scope.clone()));
        // Even another worker's open inspector cannot redirect the shelf's response.
        assert!(inner.open_worker("w_workerBB"));
        assert!(inner.interruption.answer(Decision::Deny));
        assert!(
            matches!(backend.actions.borrow().last(), Some(WorkerAction::Approve {scope, request, decision: Decision::Deny, ..}) if scope == &first_scope && request == "same-id")
        );
        assert_eq!(
            inner.interruption_scope.borrow().as_ref(),
            Some(&second_scope)
        );
        assert_eq!(
            inner.interruption.shown().0,
            "Second worker: Approval required: Bash"
        );

        // An event can invalidate the displayed scope before its queued repaint. Reject a
        // click in that interval even with a permissive backend, and never answer the root.
        second.run_id = Some(RunId::parse("r_secondRun").expect("run"));
        subscriber.registry("root-thread", &[first, second]);
        let actions = backend.actions.borrow().len();
        inner.interruption.allow_btn.emit_clicked();
        assert_eq!(backend.actions.borrow().len(), actions);
        assert_eq!(backend.root_answers.get(), 1);
        inner.refresh_status();
        assert!(!inner.interruption.revealer.reveals_child());

        let replacement = scope("w_workerBB", 2, "r_secondRun");
        subscriber.scoped(&ScopedEnvelope {
            scope: replacement.clone(),
            envelope: Envelope::new(Event::QuestionRequested {
                questions: vec![],
                source: agent_core::event::QuestionSource::Agent,
            })
            .request("question"),
        });
        inner.refresh_status();
        assert!(inner.interruption.revealer.reveals_child());
        assert!(!super::super::interruption::placed(
            &inner.interruption.allow_btn
        ));
        inner.interruption.jump_btn.emit_clicked();
        let inspector = inner
            .worker_inspector
            .borrow()
            .clone()
            .expect("question review");
        assert_eq!(*inspector.selected.borrow(), Some(replacement.clone()));
        subscriber.scoped(&ScopedEnvelope {
            scope: replacement.clone(),
            envelope: Envelope::new(Event::ApprovalExpired).request("question"),
        });
        subscriber.scoped(&ScopedEnvelope {
            scope: replacement.clone(),
            envelope: Envelope::new(Event::ApprovalRequested {
                tool: super::super::permission::TOOL.into(),
                title: None,
                input: json!({}),
                reason: None,
                options: vec![Decision::Allow, Decision::Deny],
                response: ResponseCapability::Live,
                remembers: None,
            })
            .request("permissions"),
        });
        inner.refresh_status();
        assert!(
            !inner.interruption.answer(Decision::Allow),
            "a cut permission list still needs full review"
        );
        assert!(!super::super::interruption::placed(
            &inner.interruption.allow_btn
        ));
        subscriber.scoped(&ScopedEnvelope {
            scope: replacement,
            envelope: Envelope::new(Event::ApprovalResolved {
                decision: Decision::Deny,
            })
            .request("permissions"),
        });
        inner.refresh_status();
        assert!(!inner.interruption.revealer.reveals_child());
        assert!(
            inner
                .model
                .borrow()
                .order()
                .iter()
                .all(|id| id == "approval:delegated-request-1"),
            "worker requests stay outside the root transcript"
        );
    }

    /// Dangerous permissive mode answers a live worker's approvals Allow, once each, and
    /// nothing else: not its questions, not a card without Allow, not the root's own requests.
    fn bypass_allows_worker_requests_ui_checks() {
        let backend = Rc::new(Backend {
            actions: RefCell::new(Vec::new()),
            diff_scopes: RefCell::new(Vec::new()),
            root_answers: Cell::new(0),
            mode: Cell::new(Mode::Ask),
        });
        let view = ChatView::new(backend.clone());
        let subscriber = view.delegation_subscriber("root-thread");
        let inner = view.inner().expect("inner");
        subscriber.registry("root-thread", &[summary("w_workerAA", "r_firstRun")]);
        let live = scope("w_workerAA", 1, "r_firstRun");
        let approval = |request: &str, options: Vec<Decision>| {
            Envelope::new(Event::ApprovalRequested {
                tool: "Bash".into(),
                title: None,
                input: json!({"command":"true"}),
                reason: None,
                options,
                response: ResponseCapability::Live,
                remembers: None,
            })
            .request(request)
        };
        for envelope in [
            approval("run", vec![Decision::Allow, Decision::Deny]),
            approval("deny-only", vec![Decision::Deny, Decision::Cancel]),
            Envelope::new(Event::QuestionRequested {
                questions: vec![],
                source: agent_core::event::QuestionSource::Agent,
            })
            .request("question"),
        ] {
            subscriber.scoped(&ScopedEnvelope {
                scope: live.clone(),
                envelope,
            });
        }
        view.apply(&approval("root-own", vec![Decision::Allow, Decision::Deny]));

        // Outside Bypass every request waits for the user.
        inner.allow_worker_requests();
        assert!(backend.actions.borrow().is_empty());

        backend.mode.set(Mode::Bypass);
        inner.allow_worker_requests();
        let allowed: Vec<_> = backend
            .actions
            .borrow()
            .iter()
            .map(|action| match action {
                WorkerAction::Approve {
                    scope,
                    request,
                    decision,
                    custom_rule,
                } => {
                    assert_eq!(scope, &live);
                    assert!(custom_rule.is_none(), "an automatic answer keeps no rule");
                    (request.clone(), *decision)
                }
                _ => panic!("only approvals are answered"),
            })
            .collect();
        assert_eq!(allowed, [("run".to_owned(), Decision::Allow)]);
        assert_eq!(
            backend.root_answers.get(),
            0,
            "the root's own card is its own"
        );

        // Answered once: a later sweep does not send it again.
        inner.allow_worker_requests();
        assert_eq!(backend.actions.borrow().len(), 1);

        // A request arriving while in Bypass is answered too.
        subscriber.scoped(&ScopedEnvelope {
            scope: live.clone(),
            envelope: approval("later", vec![Decision::Allow, Decision::Deny]),
        });
        inner.allow_worker_requests();
        assert!(
            matches!(backend.actions.borrow().last(), Some(WorkerAction::Approve { request, decision: Decision::Allow, .. }) if request == "later")
        );
        assert_eq!(backend.actions.borrow().len(), 2);
    }
}
