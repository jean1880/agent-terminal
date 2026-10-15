//! Lane A: the child session factory (§3.1). Wave 0: only the session API Lane B freezes, held
//! here so it is reachable before Lane A calls it.

use std::rc::Rc;

use agent_core::adapter::{Adapter, OpenSession};
use agent_kit::store::ThreadId;

use super::seam::{DelegatedInput, DelegatedOutcome, SessionOptions};
use crate::chat::session::{Approval, ChatSession, LaunchEnv, SessionStore};
use crate::chat::EnvelopeSink;

/// The constructor Lane A builds supervised children with (§3.4).
pub(crate) type WithOptions = fn(
    Box<dyn Adapter>,
    OpenSession,
    SessionStore,
    ThreadId,
    EnvelopeSink,
    Approval,
    LaunchEnv,
    SessionOptions,
) -> Rc<ChatSession>;

pub(crate) const WITH_OPTIONS: WithOptions = ChatSession::with_options;

/// Every other frozen `ChatSession` method of §1.2, in one place.
pub(crate) fn session_api(session: &ChatSession, input: DelegatedInput) -> DelegatedOutcome {
    let _ = session.interrupt_run();
    let _ = session.effective_mode();
    let _ = session.process_generation();
    let _ = session.exit_watches();
    let _ = session.rebind_stop();
    session.expire_approvals("");
    session.rebind_start();
    session.deliver_delegated(input)
}
