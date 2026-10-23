//! Waking a root with its sub-agents' news, so it never has to poll for their results.
//!
//! A child's finished run, its failure and every message it sends its parent are queued for
//! the root (when the root is that parent), coalesced for [`WAKE_COALESCE`], and offered to the
//! root's session (`ChatSession::wake_with`), which posts them as a notice and starts a turn
//! with them. A root that cannot take them now is offered them again every [`WAKE_RETRY`] and
//! after each of its turns. A root in dangerous permissive mode is never woken and never
//! retried: its thread is told what waits once per new item, the user's next message carries
//! it, and leaving the mode offers it again. What a page (`cross_agent_wait`,
//! `cross_agent_read`) already showed the root is never sent again. The decisions, the framing
//! and the budget are `agent_core::delegation::wake`.
use super::supervisor::{Supervisor, WorkerRt};
use crate::chat::session::{HeldNews, TakenNews, WakeOutcome};
use agent_core::delegation::wake::{self, Item, Outcome, Shown};
use agent_core::delegation::{MessageId, WorkerId};
use gtk4::glib;
use std::rc::{Rc, Weak};
use std::time::Duration;
use tracing::info;

/// How long news waits for more before it is offered: sub-agents finishing close together
/// share one wake turn.
pub(super) const WAKE_COALESCE: Duration = Duration::from_millis(250);
/// How often news a busy root could not take yet is offered again.
pub(super) const WAKE_RETRY: Duration = Duration::from_secs(1);

/// The model a worker runs, as its identity reports it (else as requested).
fn model_of(w: &WorkerRt) -> Option<String> {
    let identity =
        w.row.identity_json.as_deref().and_then(|s| {
            serde_json::from_str::<agent_core::delegation::identity::Identity>(s).ok()
        })?;
    identity.model.effective.or(identity.model.requested)
}

impl Supervisor {
    /// Queues `child`'s news for its parent, when that parent is the tree's root (a nested
    /// parent is a worker in a run of its own, which its own parent hears the end of). `text`
    /// is redacted here; `shown` is how a page could already have shown it to the root.
    pub(super) fn queue_wake(
        self: &Rc<Self>,
        child: &WorkerId,
        outcome: Outcome,
        text: &str,
        shown: Shown,
    ) {
        let root = {
            let mut state = self.state.borrow_mut();
            let Some(w) = state.workers.get(child) else {
                return;
            };
            if w.row.parent_id.as_deref() != Some(w.root.as_str()) {
                return;
            }
            let root = w.root.clone();
            let item = Item {
                worker: child.clone(),
                name: w.row.name.clone(),
                driver: w.driver,
                model: model_of(w),
                outcome,
                text: agent_core::redact::redact(text),
                shown,
            };
            if state.shutting_down || state.roots.get(&root).is_none_or(|r| r.closed) {
                return;
            }
            // Stopped by the user: news that lands after the stop (a child's end the store had
            // not yet confirmed when the user stopped) is not queued either, or the user's next
            // message would let it through. It stays in the root's mailbox.
            if state
                .workers
                .get(&root)
                .and_then(WorkerRt::session)
                .is_some_and(|session| session.wakes_held())
            {
                return;
            }
            if !state.wakes.entry(root.clone()).or_default().push(item) {
                return;
            }
            root
        };
        info!(%child, %root, ?outcome, "sub-agent news queued for its root");
        self.schedule_wake(&root, WAKE_COALESCE);
    }

    /// A page was shown to `caller`: it covered every event of `scope` up to `through`, and
    /// presented `presented` to it. News it showed is not woken for.
    pub(super) fn wake_shown(
        &self,
        caller: &WorkerId,
        scope: &[WorkerId],
        through: Option<u64>,
        presented: &[MessageId],
    ) {
        let mut state = self.state.borrow_mut();
        if !state.roots.contains_key(caller) {
            return;
        }
        let queue = state.wakes.entry(caller.clone()).or_default();
        if let Some(through) = through.and_then(|t| i64::try_from(t).ok()) {
            queue.covered(scope, through);
        }
        queue.presented(presented);
    }

    /// The user stopped the root, or its thread is closing: its waiting news is forgotten (it
    /// stays in its mailbox) and its session takes none until the user speaks to it again.
    pub(super) fn forget_wakes(&self, root: &WorkerId) {
        if let Some(queue) = self.state.borrow_mut().wakes.get_mut(root) {
            queue.clear();
        }
    }

    /// Offers `root` its news after `delay`, unless an offer is already scheduled.
    pub(super) fn schedule_wake(self: &Rc<Self>, root: &WorkerId, delay: Duration) {
        {
            let mut state = self.state.borrow_mut();
            if state.wakes.get(root).is_none_or(|q| q.pending() == 0)
                || !state.wake_armed.insert(root.clone())
            {
                return;
            }
        }
        let weak = self.weak.clone();
        let root = root.clone();
        glib::spawn_future_local(async move {
            glib::timeout_future(delay).await;
            let Some(sup) = weak.upgrade() else {
                return;
            };
            sup.state.borrow_mut().wake_armed.remove(&root);
            sup.offer_wake(&root);
        });
    }

    /// Offers `root` what waits for it now. With no session (its thread is not open) the news
    /// waits for one to be attached; a busy one gets it again later.
    fn offer_wake(self: &Rc<Self>, root: &WorkerId) {
        let (closed, blocked, session, pending) = {
            let state = self.state.borrow();
            let pending = state.wakes.get(root).map_or(0, wake::Queue::pending);
            if pending == 0 {
                return;
            }
            let r = state.roots.get(root);
            (
                state.shutting_down || r.is_none_or(|r| r.closed),
                r.is_some_and(|r| r.revoking || r.transferring)
                    || state.workers.get(root).is_some_and(|w| w.preparing),
                state.workers.get(root).and_then(WorkerRt::session),
                pending,
            )
        };
        if closed {
            info!(%root, items = pending, "sub-agent news dropped: its root closed");
            self.forget_wakes(root);
            return;
        }
        let Some(session) = session else {
            // Offered again when a session is attached.
            self.note_hold(root, "no session is attached", pending);
            return;
        };
        if blocked {
            self.note_hold(
                root,
                "a delegation rebind or revocation is under way",
                pending,
            );
            self.schedule_wake(root, WAKE_RETRY);
            return;
        }
        let Some((items, dropped)) = self
            .state
            .borrow_mut()
            .wakes
            .get_mut(root)
            .map(|q| q.take())
        else {
            return;
        };
        let text = wake::frame(&items, dropped, wake::WAKE_BUDGET);
        // No borrow is held: the session's notice reaches the window synchronously.
        match session.wake_with(&text) {
            WakeOutcome::Started | WakeOutcome::Noticed => {
                if let Some(queue) = self.state.borrow_mut().wakes.get_mut(root) {
                    queue.delivered();
                }
                info!(%root, items = items.len(), "root told its sub-agents' news");
            }
            WakeOutcome::Held => {
                info!(%root, items = items.len(), "sub-agent news dropped: the user stopped its root");
            }
            WakeOutcome::Busy => {
                let count = items.len();
                if let Some(queue) = self.state.borrow_mut().wakes.get_mut(root) {
                    queue.restore(items, dropped);
                }
                let reason = session.wake_blocker().unwrap_or("its root is busy");
                self.note_hold(root, reason, count);
                self.schedule_wake(root, WAKE_RETRY);
            }
            WakeOutcome::HeldForUser => {
                let fresh = {
                    let mut state = self.state.borrow_mut();
                    state.wakes.get_mut(root).map_or(0, |queue| {
                        queue.restore(items, dropped);
                        let fresh = queue.unannounced();
                        queue.announced();
                        fresh
                    })
                };
                let pending = self
                    .state
                    .borrow()
                    .wakes
                    .get(root)
                    .map_or(0, wake::Queue::pending);
                self.note_hold(root, "dangerous permissive mode", pending);
                session.hold_news_for_user(Rc::new(RootNews {
                    supervisor: self.weak.clone(),
                    root: root.clone(),
                }));
                // Told once per new item; never retried on a timer: the user's next message
                // takes it, leaving the mode offers it again, and new news offers it anew.
                if fresh > 0 {
                    session.note(wake::held_for_user_notice(pending));
                }
            }
        }
    }

    /// Logs that `root`'s `items` were held for `reason`, once until the reason changes.
    fn note_hold(&self, root: &WorkerId, reason: &'static str, items: usize) {
        let changed = self
            .state
            .borrow_mut()
            .wakes
            .get_mut(root)
            .is_some_and(|q| q.held(reason));
        if changed {
            info!(%root, items, reason, "sub-agent news held for its root");
        }
    }

    /// Takes `root`'s news for the user's prompt ([`HeldNews::take`]): nothing while its tree is
    /// closed or being rebound, which the normal wake waits out.
    fn take_held_news(self: &Rc<Self>, root: &WorkerId) -> Option<TakenNews> {
        let (items, dropped) = {
            let mut state = self.state.borrow_mut();
            let r = state.roots.get(root);
            if state.shutting_down
                || r.is_none_or(|r| r.closed || r.revoking || r.transferring)
                || state.workers.get(root).is_some_and(|w| w.preparing)
            {
                return None;
            }
            let queue = state.wakes.get_mut(root)?;
            if queue.pending() == 0 {
                return None;
            }
            queue.delivered();
            queue.take()
        };
        info!(%root, items = items.len(), "sub-agent news taken for the user's message");
        let text = wake::frame(&items, dropped, wake::WAKE_BUDGET);
        let weak = self.weak.clone();
        let root = root.clone();
        Some(TakenNews {
            text,
            give_back: Box::new(move || {
                let Some(sup) = weak.upgrade() else {
                    return;
                };
                let count = items.len();
                if let Some(queue) = sup.state.borrow_mut().wakes.get_mut(&root) {
                    queue.restore(items, dropped);
                }
                info!(%root, items = count, "sub-agent news put back: the user's message was not sent");
            }),
        })
    }
}

/// A root's held news, as its session reaches it ([`ChatSession::hold_news_for_user`]). Holds
/// the supervisor weakly: no session/runtime ownership cycle.
///
/// [`ChatSession::hold_news_for_user`]: crate::chat::session::ChatSession::hold_news_for_user
struct RootNews {
    supervisor: Weak<Supervisor>,
    root: WorkerId,
}

impl HeldNews for RootNews {
    fn take(&self) -> Option<TakenNews> {
        self.supervisor.upgrade()?.take_held_news(&self.root)
    }

    fn release(&self) {
        if let Some(sup) = self.supervisor.upgrade() {
            info!(root = %self.root, "root left dangerous permissive mode; its news is offered");
            sup.schedule_wake(&self.root, WAKE_COALESCE);
        }
    }
}
