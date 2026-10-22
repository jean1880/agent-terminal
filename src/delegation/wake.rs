//! Waking a root with its sub-agents' news, so it never has to poll for their results.
//!
//! A child's finished run, its failure and every message it sends its parent are queued for
//! the root (when the root is that parent), coalesced for [`WAKE_COALESCE`], and offered to the
//! root's session (`ChatSession::wake_with`), which posts them as a notice and starts a turn
//! with them. A root that cannot take them now is offered them again every [`WAKE_RETRY`] and
//! after each of its turns. What a page (`cross_agent_wait`, `cross_agent_read`) already showed
//! the root is never sent again. The decisions, the framing and the budget are
//! `agent_core::delegation::wake`.
use super::supervisor::{Supervisor, WorkerRt};
use crate::chat::session::WakeOutcome;
use agent_core::delegation::wake::{self, Item, Outcome, Shown};
use agent_core::delegation::{MessageId, WorkerId};
use gtk4::glib;
use std::rc::Rc;
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
        let (closed, blocked, session) = {
            let state = self.state.borrow();
            if state.wakes.get(root).is_none_or(|q| q.pending() == 0) {
                return;
            }
            let r = state.roots.get(root);
            (
                state.shutting_down || r.is_none_or(|r| r.closed),
                r.is_some_and(|r| r.revoking || r.transferring)
                    || state.workers.get(root).is_some_and(|w| w.preparing),
                state.workers.get(root).and_then(WorkerRt::session),
            )
        };
        if closed {
            self.forget_wakes(root);
            return;
        }
        let Some(session) = session else {
            return;
        };
        if blocked {
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
                info!(%root, items = items.len(), "root told its sub-agents' news");
            }
            WakeOutcome::Held => {}
            WakeOutcome::Busy => {
                if let Some(queue) = self.state.borrow_mut().wakes.get_mut(root) {
                    queue.restore(items, dropped);
                }
                self.schedule_wake(root, WAKE_RETRY);
            }
        }
    }
}
