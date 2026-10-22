//! The cross-agent delegation supervisor.
//!
//! One application-owned runtime drives authenticated tools, scoped child sessions and
//! committed worker activity. It is shared by windows and never owned by an MCP process.

use std::cell::RefCell;
use std::rc::Rc;

pub(crate) mod seam;

mod agy_l2;
mod attest;
mod authority;
mod capacity;
mod catalog_info;
mod child;
mod deferred;
mod ipc;
mod launch;
mod ops;
mod publish;
mod readback;
mod reconcile;
mod rundir;
pub(crate) mod services;
pub(crate) mod supervisor;
mod waits;
mod wake;
mod workspace;

pub(crate) use services::AppServices;
pub(crate) use supervisor::Supervisor;

thread_local! {
    /// The app's one supervisor, on the GTK main thread. No window holds it strongly.
    static SUPERVISOR: RefCell<Option<Rc<Supervisor>>> = const { RefCell::new(None) };
}

/// The running supervisor; `None` before [`start`] finished or after shutdown.
pub(crate) fn supervisor() -> Option<Rc<Supervisor>> {
    SUPERVISOR.with(|s| s.borrow().clone())
}

/// Starts the supervisor on the main context (reconcile, sweep, bind).
pub(crate) fn start(services: AppServices) {
    STARTING.with(|s| {
        if s.replace(true) {
            return;
        }
        gtk4::glib::spawn_future_local(async move {
            match Supervisor::start(
                services,
                agent_core::delegation::Limits::default(),
                Rc::new(supervisor::P1Routes),
            )
            .await
            {
                Ok(supervisor) => SUPERVISOR.with(|s| *s.borrow_mut() = Some(supervisor)),
                Err(e) => tracing::error!("Cannot start delegation runtime: {e}"),
            }
            FINISHED.with(|s| s.set(true));
        });
    });
}

thread_local! {
    static STARTING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FINISHED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Waits for startup reconciliation and socket binding. A failed startup stays unavailable.
pub(crate) async fn ready() -> Option<Rc<Supervisor>> {
    while STARTING.with(|s| s.get()) && !FINISHED.with(|s| s.get()) {
        gtk4::glib::timeout_future(std::time::Duration::from_millis(10)).await;
    }
    supervisor()
}

/// The AppShutdown revocation row: every supervised process still to be seen exiting.
pub(crate) fn shutdown_all() -> Vec<seam::ChildStop> {
    supervisor().map(|s| s.shutdown()).unwrap_or_default()
}

/// Runs the main context until the revocations `shutdown_all` started are committed, at most
/// `wait`. They are futures on this context: without this they would only run in GLib's flush
/// after `shutdown`, once the store writer has closed, and be refused.
pub(crate) fn settle_shutdown(wait: std::time::Duration) {
    if let Some(sup) = supervisor() {
        sup.settle_revokes(wait);
    }
}

/// Removes the socket and the overlays of exited generations. Idempotent.
pub(crate) fn final_cleanup() {
    SUPERVISOR.with(|s| {
        s.borrow_mut().take();
    });
}
