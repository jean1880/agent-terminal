//! The cross-agent delegation supervisor (`docs/delegation/p4-supervisor.md`).
//!
//! Wave 0: the frozen seam ([`seam`]) and a fail-closed stub for every module. Lanes A–D replace
//! bodies, never the signatures listed in the design's §1.2. Nothing here is reachable from the
//! app until Lane E wires it, so it ships dark.
// Wave 0 stubs are not called yet; lanes A–E fill them in.
#![allow(dead_code)]

use std::cell::RefCell;
use std::rc::Rc;

pub(crate) mod seam;

mod agy_l2;
mod attest;
mod authority;
mod capacity;
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

/// Starts the supervisor on the main context (reconcile, sweep, bind). Wave 0: does nothing.
pub(crate) fn start(_services: AppServices) {}

/// The AppShutdown revocation row: every supervised process still to be seen exiting.
pub(crate) fn shutdown_all() -> Vec<seam::ChildStop> {
    Vec::new()
}

/// Removes the socket and the overlays of exited generations. Idempotent.
pub(crate) fn final_cleanup() {}
