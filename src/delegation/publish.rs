//! One sanitisation boundary feeds persistence, inspection and subscribers.
use super::seam::ChildPublish;
use super::supervisor::Supervisor;
use agent_core::delegation::scope::{Scope, ScopedEnvelope};
use agent_core::delegation::wire::EventCategory;
use agent_core::delegation::{ProcessGeneration, WorkerId};
use agent_core::event::{Envelope, Event, StreamKind, TurnState};
use agent_core::redact::stream::{StreamKey, StreamSanitiser};
use agent_kit::store::delegation_typed::{Completion, LinkSpec, PublishEvent};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::{Rc, Weak};

/// The event's sequence number and, for a run's completion, the parent's report message: the
/// row that carries the result, so the parent's wake is skipped exactly when a page showed it.
/// How a page could have shown a run's end to the parent: by its report message, the row with
/// the result (a page showing only the "turn completed" event has not shown it), else by the
/// event.
fn shown_by(
    seq: i64,
    report: Option<agent_core::delegation::MessageId>,
) -> agent_core::delegation::wake::Shown {
    use agent_core::delegation::wake::Shown;
    report.map_or(Shown::Event(seq), Shown::Message)
}

type Receipt = std::sync::mpsc::Receiver<
    agent_kit::store::Result<(i64, Option<agent_core::delegation::MessageId>)>,
>;
struct PendingPublication {
    scope: Scope,
    envelope: Envelope,
    receipt: Receipt,
    bytes: usize,
    bootstrap: bool,
    /// A run's terminal event: the result it carries, for the parent's wake.
    result: Option<String>,
}

pub(super) struct Publisher {
    pub supervisor: Weak<Supervisor>,
    pub worker: WorkerId,
    sanitiser: RefCell<StreamSanitiser>,
    keys: RefCell<HashMap<(u64, String, String), StreamKey>>,
    pending: Rc<RefCell<VecDeque<PendingPublication>>>,
    pending_bytes: Rc<Cell<usize>>,
    publishing: Rc<Cell<bool>>,
    result: RefCell<String>,
    failed: Rc<Cell<bool>>,
}
impl Publisher {
    pub(super) fn new(supervisor: Weak<Supervisor>, worker: WorkerId) -> Self {
        Self {
            supervisor,
            worker,
            sanitiser: RefCell::new(StreamSanitiser::new(Default::default())),
            keys: RefCell::new(HashMap::new()),
            pending: Rc::new(RefCell::new(VecDeque::new())),
            pending_bytes: Rc::new(Cell::new(0)),
            publishing: Rc::new(Cell::new(false)),
            result: RefCell::new(String::new()),
            failed: Rc::new(Cell::new(false)),
        }
    }
    fn enqueue(&self, env: Envelope) {
        if self.failed.get() {
            return;
        }
        let Some(sup) = self.supervisor.upgrade() else {
            return;
        };
        let Some(scope) = sup.scope(&self.worker) else {
            return;
        };
        let Some(event) = sup.publication(&self.worker, env.clone(), category(&env.event)) else {
            return;
        };
        let bootstrap = sup
            .state
            .borrow()
            .workers
            .get(&self.worker)
            .is_some_and(|w| w.pending.is_some());
        if !bootstrap {
            match &env.event {
                Event::ContentDelta {
                    stream: StreamKind::Assistant,
                    text,
                } => self.result.borrow_mut().push_str(text),
                Event::ContentSnapshot {
                    stream: StreamKind::Assistant,
                    text,
                } => *self.result.borrow_mut() = text.clone(),
                _ => {}
            }
            let mut result = self.result.borrow_mut();
            if result.len() > 16384 {
                let suffix = "\n[result truncated; inspect the worker transcript]";
                let mut end = 16384 - suffix.len();
                while !result.is_char_boundary(end) {
                    end -= 1;
                }
                result.truncate(end);
                result.push_str(suffix);
            }
        }
        let completion = if !bootstrap {
            if let (Event::TurnCompleted { state, usage, .. }, Some(run)) = (&env.event, &scope.run)
            {
                Some(Completion {
                    run: run.clone(),
                    state: match state {
                        TurnState::Completed => {
                            agent_core::delegation::lifecycle::RunState::Completed
                        }
                        TurnState::Interrupted => {
                            agent_core::delegation::lifecycle::RunState::Interrupted
                        }
                        TurnState::Failed => agent_core::delegation::lifecycle::RunState::Failed,
                    },
                    worker_state: match state {
                        TurnState::Completed => agent_core::event::WorkerState::Completed,
                        TurnState::Interrupted => agent_core::event::WorkerState::Waiting,
                        TurnState::Failed => agent_core::event::WorkerState::Failed,
                    },
                    reason: None,
                    usage_json: usage.as_ref().and_then(|u| serde_json::to_string(u).ok()),
                    result: Some(std::mem::take(&mut *self.result.borrow_mut())),
                    event: event.clone(),
                })
            } else {
                None
            }
        } else {
            None
        };
        let bytes = serde_json::to_vec(&event.envelope)
            .map_or(4 * 1024 * 1024, |v| v.len())
            .saturating_add(event.links.len() * 512)
            .saturating_add(
                completion
                    .as_ref()
                    .and_then(|c| c.result.as_ref())
                    .map_or(0, String::len),
            );
        if self.pending.borrow().len() >= 4096
            || self.pending_bytes.get().saturating_add(bytes) > 4 * 1024 * 1024
        {
            self.failed.set(true);
            sup.fail_worker(
                &self.worker,
                super::supervisor::error(
                    agent_core::delegation::ErrorCode::PersistenceFailed,
                    "delegated publication backlog exceeded its bound",
                ),
            );
            return;
        }
        let terminal = completion.is_some();
        let result = completion.as_ref().and_then(|c| c.result.clone());
        let job = move |s: &agent_kit::store::Store| {
            if let Some(mut completion) = completion {
                let terminal = s.delegation_run(completion.run.as_str())?.is_some_and(|r| {
                    matches!(
                        r.state.as_str(),
                        "completed" | "interrupted" | "failed" | "unknown"
                    )
                });
                if !terminal {
                    if let Some(link) = completion.event.links.first() {
                        if s.delegation_worker(link.worker.as_str())?
                            .is_some_and(|w| w.state == "closed")
                        {
                            completion.worker_state = agent_core::event::WorkerState::Closed;
                        }
                    }
                    return s
                        .complete_delegation_run(&completion)
                        .map(|c| (c.event.seq, c.notification.map(|(id, _)| id)));
                }
            }
            if event.links.is_empty() {
                s.append_event(
                    &event.thread,
                    event.provider_thread.as_deref(),
                    &event.envelope,
                )
                .map(|seq| (seq, None))
            } else {
                s.publish_delegation_event(&event).map(|p| (p.seq, None))
            }
        };
        let receipt = if let Some(writer) = sup.services.handle.writer() {
            if terminal {
                writer.durable_call_sized("complete delegated event", bytes, job)
            } else {
                writer.call_result_sized("publish delegated event", bytes, job)
            }
        } else {
            let (tx, rx) = std::sync::mpsc::channel();
            let _ = tx.send(sup.services.handle.reader().atomically(job));
            rx
        };
        self.pending_bytes.set(self.pending_bytes.get() + bytes);
        self.pending.borrow_mut().push_back(PendingPublication {
            scope,
            envelope: env,
            receipt,
            bytes,
            bootstrap,
            result,
        });
        if self.publishing.replace(true) {
            return;
        }
        let queue = self.pending.clone();
        let pending_bytes = self.pending_bytes.clone();
        let publishing = self.publishing.clone();
        let weak = self.supervisor.clone();
        let worker = self.worker.clone();
        let failed = self.failed.clone();
        gtk4::glib::spawn_future_local(async move {
            loop {
                let next = queue.borrow_mut().pop_front();
                let Some(next) = next else {
                    publishing.set(false);
                    return;
                };
                let receipt = next.receipt;
                let result = gtk4::gio::spawn_blocking(move || {
                    receipt
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .map_err(|_| agent_kit::store::StoreError::WriterGone)?
                })
                .await
                .unwrap_or(Err(agent_kit::store::StoreError::WriterGone));
                pending_bytes.set(pending_bytes.get().saturating_sub(next.bytes));
                let Some(sup) = weak.upgrade() else {
                    publishing.set(false);
                    return;
                };
                match result {
                    Ok(stored) => sup.on_child_envelope(
                        &worker,
                        next.scope,
                        next.envelope,
                        stored,
                        next.bootstrap,
                        next.result.as_deref(),
                    ),
                    Err(_) => {
                        failed.set(true);
                        queue.borrow_mut().clear();
                        pending_bytes.set(0);
                        publishing.set(false);
                        sup.fail_worker(
                            &worker,
                            super::supervisor::error(
                                agent_core::delegation::ErrorCode::PersistenceFailed,
                                "delegated event did not commit",
                            ),
                        );
                        return;
                    }
                }
            }
        });
    }
    fn flush(&self, generation: ProcessGeneration, item: Option<&str>, cancel: bool) {
        let keys = {
            let mut all = self.keys.borrow_mut();
            let selected = all
                .keys()
                .filter(|(g, i, _)| *g == generation.0 && item.is_none_or(|x| x == i))
                .cloned()
                .collect::<Vec<_>>();
            selected
                .into_iter()
                .filter_map(|k| all.remove(&k))
                .collect::<Vec<_>>()
        };
        for key in keys {
            let out = if cancel {
                self.sanitiser.borrow_mut().cancel(&key)
            } else {
                self.sanitiser.borrow_mut().finish(&key)
            };
            if out.text.is_empty() {
                continue;
            }
            let stream =
                super::supervisor::parse_enum(&key.stream).unwrap_or(StreamKind::Assistant);
            let env = Envelope::new(if out.replace {
                Event::ContentSnapshot {
                    stream,
                    text: out.text,
                }
            } else {
                Event::ContentDelta {
                    stream,
                    text: out.text,
                }
            })
            .item(key.item);
            if self
                .supervisor
                .upgrade()
                .and_then(|s| s.scope(&self.worker))
                .is_some_and(|scope| scope.generation == generation)
            {
                self.enqueue(env);
            }
        }
    }
}
impl ChildPublish for Publisher {
    fn publish(&self, generation: ProcessGeneration, mut env: Envelope) -> Option<Envelope> {
        let sup = self.supervisor.upgrade()?;
        let scope = sup.scope(&self.worker)?;
        if scope.generation != generation {
            return None;
        }
        env.raw = None; // Raw provider frames may carry capabilities; they never leave this boundary.
        if let Event::ContentDelta { stream, text } | Event::ContentSnapshot { stream, text } =
            &env.event
        {
            let stream = *stream;
            let key = scope.stream_key(env.item.as_deref().unwrap_or("unidentified"), stream);
            self.keys.borrow_mut().insert(
                (generation.0, key.item.clone(), key.stream.clone()),
                key.clone(),
            );
            let out = if matches!(env.event, Event::ContentSnapshot { .. }) {
                self.sanitiser.borrow_mut().replace_snapshot(&key, text)
            } else {
                self.sanitiser.borrow_mut().push(&key, text)
            };
            if self.sanitiser.borrow().saturated() {
                let worker = self.worker.clone();
                let s = sup.clone();
                gtk4::glib::spawn_future_local(async move {
                    s.fail_worker(
                        &worker,
                        super::supervisor::error(
                            agent_core::delegation::ErrorCode::InternalError,
                            "stream sanitiser capacity exceeded",
                        ),
                    );
                });
            }
            if out.text.is_empty() {
                return None;
            }
            env.event = if out.replace {
                Event::ContentSnapshot {
                    stream,
                    text: out.text,
                }
            } else {
                Event::ContentDelta {
                    stream,
                    text: out.text,
                }
            };
        } else if matches!(env.event, Event::ItemCompleted { .. }) {
            self.flush(generation, env.item.as_deref(), false);
        }
        agent_kit::store::scrub_envelope(env).ok()
    }
    fn persist(&self, _thread: &str, _provider: Option<&str>, env: &Envelope) {
        self.enqueue(env.clone());
    }
    fn cancel_generation(&self, generation: ProcessGeneration) {
        self.flush(generation, None, true);
    }
}
/// A run's end, as its parent's wake says it.
fn wake_outcome(state: TurnState) -> agent_core::delegation::wake::Outcome {
    use agent_core::delegation::wake::Outcome;
    match state {
        TurnState::Completed => Outcome::Completed,
        TurnState::Interrupted => Outcome::Interrupted,
        TurnState::Failed => Outcome::Failed,
    }
}
pub(super) fn category(event: &Event) -> Option<EventCategory> {
    Some(match event {
        Event::SessionStarted { .. }
        | Event::SessionExited { .. }
        | Event::TurnStarted { .. }
        | Event::TurnCompleted { .. }
        | Event::ModeChanged { .. } => EventCategory::Lifecycle,
        Event::ItemStarted { .. } | Event::ItemCompleted { .. } => EventCategory::Activity,
        Event::ApprovalRequested { .. } | Event::ApprovalResolved { .. } => EventCategory::Approval,
        Event::DelegatedMessage { .. } => EventCategory::Message,
        Event::Error { .. } => EventCategory::Error,
        _ => return None,
    })
}
impl Supervisor {
    pub(super) fn scope(&self, worker: &WorkerId) -> Option<Scope> {
        let state = self.state.borrow();
        let w = state.workers.get(worker)?;
        Some(Scope {
            root: w.root.clone(),
            worker: worker.clone(),
            generation: w.generation,
            run: w.run.clone().or_else(|| w.last_run.clone()),
        })
    }
    pub(super) fn publication(
        &self,
        worker: &WorkerId,
        env: Envelope,
        category: Option<EventCategory>,
    ) -> Option<PublishEvent> {
        let state = self.state.borrow();
        let w = state.workers.get(worker)?;
        let mut links = Vec::new();
        let mut current = Some(worker.clone());
        while let Some(id) = current {
            let node = state.workers.get(&id)?;
            if let Some(category) = category {
                links.push(LinkSpec {
                    worker: id.clone(),
                    run: node.run.clone().or_else(|| node.last_run.clone()),
                    process_generation: node.generation.0,
                    category,
                    invocation: None,
                });
            }
            current = node.row.parent();
        }
        // Every publication needs a link; token deltas have no public-page category and use
        // the ordinary backing transcript append below instead.
        Some(PublishEvent {
            thread: w.row.backing_thread_id.clone()?,
            provider_thread: None,
            envelope: env,
            links,
            worker_states: Vec::new(),
            run_states: Vec::new(),
            aliases: Vec::new(),
        })
    }
    pub(super) fn on_child_envelope(
        self: &std::rc::Rc<Self>,
        worker: &WorkerId,
        scope: Scope,
        env: Envelope,
        // The event's sequence number, and a completion's report message (see `Receipt`).
        (seq, report): (i64, Option<agent_core::delegation::MessageId>),
        bootstrap: bool,
        result: Option<&str>,
    ) {
        let mut ended = None;
        {
            let mut state = self.state.borrow_mut();
            let Some(w) = state.workers.get_mut(worker) else {
                return;
            };
            let before = w.row.state.clone();
            if w.row.state != "closed"
                && w.generation == scope.generation
                && w.run.as_ref().or(w.last_run.as_ref()) == scope.run.as_ref()
            {
                match &env.event {
                    Event::TurnStarted { .. }
                        if !bootstrap && !w.readback.failed && w.run == scope.run =>
                    {
                        w.row.state = "running".into();
                        w.opened_run = scope.run.clone();
                    }
                    Event::ApprovalRequested { .. } => w.row.state = "waiting".into(),
                    Event::ApprovalResolved { .. } => w.row.state = "running".into(),
                    Event::TurnCompleted { state, usage, .. }
                        if !bootstrap
                            && !w.readback.failed
                            && w.run == scope.run
                            && w.run.is_some() =>
                    {
                        if let Some(mut identity) = w.row.identity_json.as_deref().and_then(|s| {
                            serde_json::from_str::<agent_core::delegation::identity::Identity>(s)
                                .ok()
                        }) {
                            if *state == TurnState::Completed {
                                agent_core::delegation::identity::observe(&mut identity,agent_core::delegation::identity::IdentityObservation::TurnSucceeded);
                            }
                            super::attest::sanitise_identity(&mut identity);
                            w.row.identity_json = serde_json::to_string(&identity).ok();
                            let id = worker.to_string();
                            self.services.write(
                                "delegated identity after turn",
                                None,
                                false,
                                move |s| s.set_delegation_worker_identity(&id, &identity),
                            );
                        }
                        ended = Some((*state, usage.clone()));
                        if w.run.as_ref() == scope.run.as_ref() {
                            w.last_run = w.run.take();
                        }
                        w.row.state = match state {
                            TurnState::Completed => "completed",
                            TurnState::Interrupted => "waiting",
                            TurnState::Failed => "failed",
                        }
                        .into();
                    }
                    Event::ContentSnapshot {
                        stream: StreamKind::Assistant,
                        text,
                    }
                    | Event::ContentDelta {
                        stream: StreamKind::Assistant,
                        text,
                    } => {
                        let mut text = agent_core::redact::redact(text);
                        let mut end = 512.min(text.len());
                        while !text.is_char_boundary(end) {
                            end -= 1;
                        }
                        text.truncate(end);
                        w.row.last_activity = Some(text);
                    }
                    Event::SessionStarted { native_id, .. } => {
                        let worker = worker.clone();
                        let native = native_id.clone();
                        let driver = w.driver;
                        self.services.write(
                            "record delegated native session",
                            None,
                            true,
                            move |s| {
                                s.record_child_native_session(&worker, driver, "default", &native)?;
                                Ok(())
                            },
                        );
                    }
                    _ => {}
                }
            }
            if w.row.state != before {
                tracing::info!(
                    %worker,
                    from = %before,
                    to = %w.row.state,
                    "delegated worker state changed"
                );
            }
        }
        if let Some((state, usage)) = ended {
            let _ = usage;
            self.state
                .borrow_mut()
                .controls
                .retain(|_, entry| &entry.worker != worker);
            self.drain_deferred(worker);
            self.queue_wake(
                worker,
                wake_outcome(state),
                result.unwrap_or_default(),
                shown_by(seq, report),
            );
        }
        let scoped = ScopedEnvelope {
            scope,
            envelope: env,
        };
        let subscribers = {
            let mut state = self.state.borrow_mut();
            state.subscribers.retain(|(_, s)| s.strong_count() > 0);
            let thread = state
                .roots
                .get(&scoped.scope.root)
                .map(|r| r.row.root_thread_id.clone());
            state
                .subscribers
                .iter()
                .filter(|(t, _)| Some(t) == thread.as_ref())
                .filter_map(|(_, s)| s.upgrade())
                .collect::<Vec<_>>()
        };
        for sub in subscribers {
            sub.committed(&scoped, seq);
        }
        self.notify_registry();
    }
    pub(super) fn complete_run(
        self: &std::rc::Rc<Self>,
        worker: &WorkerId,
        state: TurnState,
        usage: Option<String>,
    ) {
        let (run, result) = {
            let mut state = self.state.borrow_mut();
            let Some(w) = state.workers.get_mut(worker) else {
                return;
            };
            let Some(run) = w.run.clone() else {
                return;
            };
            (run, w.row.last_activity.clone())
        };
        self.state
            .borrow_mut()
            .controls
            .retain(|_, entry| &entry.worker != worker);
        let env = Envelope::new(Event::Notice {
            text: result
                .clone()
                .unwrap_or_else(|| "Delegated run ended".into()),
        });
        let Some(mut event) = self.publication(worker, env, Some(EventCategory::Result)) else {
            return;
        };
        for link in &mut event.links {
            if link.worker == *worker {
                link.run = Some(run.clone());
            }
        }
        let run_state = match state {
            TurnState::Completed => agent_core::delegation::lifecycle::RunState::Completed,
            TurnState::Interrupted => agent_core::delegation::lifecycle::RunState::Interrupted,
            TurnState::Failed => agent_core::delegation::lifecycle::RunState::Failed,
        };
        let worker_state = match state {
            TurnState::Completed => agent_core::event::WorkerState::Completed,
            TurnState::Interrupted => agent_core::event::WorkerState::Waiting,
            TurnState::Failed => agent_core::event::WorkerState::Failed,
        };
        let mut completion = Completion {
            run,
            state: run_state,
            worker_state,
            reason: None,
            usage_json: usage,
            result,
            event,
        };
        let sup = self.clone();
        let worker = worker.clone();
        let stored_worker = worker.clone();
        let completed_run = completion.run.clone();
        let reported = completion.result.clone();
        gtk4::glib::spawn_future_local(async move {
            if let Ok(completed) = sup
                .services
                .durable("complete delegated run", move |s| {
                    if s.delegation_worker(stored_worker.as_str())?
                        .is_some_and(|w| w.state == "closed")
                    {
                        completion.worker_state = agent_core::event::WorkerState::Closed;
                    }
                    s.complete_delegation_run(&completion)
                })
                .await
            {
                sup.queue_wake(
                    &worker,
                    wake_outcome(state),
                    reported.as_deref().unwrap_or_default(),
                    shown_by(
                        completed.event.seq,
                        completed.notification.map(|(id, _)| id),
                    ),
                );
                if let Some(w) = sup
                    .state
                    .borrow_mut()
                    .workers
                    .get_mut(&worker)
                    .filter(|w| w.run.as_ref() == Some(&completed_run))
                {
                    w.last_run = w.run.take();
                    if w.row.state != "closed" {
                        w.row.state = match state {
                            TurnState::Completed => "completed",
                            TurnState::Interrupted => "waiting",
                            TurnState::Failed => "failed",
                        }
                        .into();
                    }
                }
                sup.drain_deferred(&worker);
                sup.notify_registry();
            } else {
                sup.fail_worker(
                    &worker,
                    super::supervisor::error(
                        agent_core::delegation::ErrorCode::PersistenceFailed,
                        "delegated completion did not commit",
                    ),
                );
            }
        });
    }
}
