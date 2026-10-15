//! Lane A: the supervisor (§1.3, §2, §5.4). Wave 0 stub: every entry point fails closed.

use std::rc::{Rc, Weak};

use agent_core::adapter::Driver;
use agent_core::delegation::authority::RevocationEvent;
use agent_core::delegation::route::{RouteQuery, RouteSpec};
use agent_core::delegation::{admit_route, ErrorCode, Limits, ToolError, WorkerId};

use super::seam::{ChildStop, DelegationHooks, WorkerSubscriber};
use super::services::AppServices;
use crate::chat::session::ChatSession;
use crate::config::Profile;

/// Which routes may be spawned. Production uses [`P1Routes`]; tests inject their own.
pub(crate) trait RouteTable {
    fn admit(&self, q: &RouteQuery) -> RouteSpec;
}

/// The P1 table: every route `Unverified` until P5/P8 flip it.
pub(crate) struct P1Routes;

impl RouteTable for P1Routes {
    fn admit(&self, q: &RouteQuery) -> RouteSpec {
        admit_route(q)
    }
}

/// What `SessionSlot::send_prompt` does with a root prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootTurnGate {
    Proceed,
    /// A rebind runs first: queue the prompt; the supervisor dispatches it after `rebind_start`.
    Deferred,
}

/// The app-owned supervisor (§2).
pub(crate) struct Supervisor {
    services: AppServices,
    limits: Limits,
    routes: Rc<dyn RouteTable>,
}

fn not_built() -> ToolError {
    ToolError::new(ErrorCode::RuntimeUnavailable, "delegation is not built yet")
}

impl Supervisor {
    pub(crate) async fn start(
        _services: AppServices,
        _limits: Limits,
        _routes: Rc<dyn RouteTable>,
    ) -> Result<Rc<Self>, String> {
        Err("delegation is not built yet".to_owned())
    }

    /// Waits for the previous root generation's exit, then stages a binding (§2.3).
    pub(crate) async fn prepare_root(
        self: &Rc<Self>,
        _thread: &str,
        _profile: &Profile,
        _driver: Driver,
    ) -> Option<Rc<dyn DelegationHooks>> {
        None
    }

    pub(crate) fn attach_root_session(&self, _thread: &str, _session: Weak<ChatSession>) {}

    pub(crate) fn before_root_turn(self: &Rc<Self>, _thread: &str) -> RootTurnGate {
        RootTurnGate::Proceed
    }

    pub(crate) fn on_root_turn_end(&self, _thread: &str) {}

    pub(crate) fn root_stop(self: &Rc<Self>, _thread: &str) {}

    /// Root Stop for a closing root page: every unexited descendant generation (§2.3).
    pub(crate) fn root_closing(self: &Rc<Self>, _thread: &str) -> Vec<ChildStop> {
        Vec::new()
    }

    pub(crate) fn root_busy(&self, _thread: &str) -> bool {
        false
    }

    pub(crate) fn subscribe(&self, _root_thread: &str, _sub: Weak<dyn WorkerSubscriber>) {}

    /// A child's session, for scoped approvals only (P6).
    pub(crate) fn child_session(&self, _worker: &WorkerId) -> Option<Rc<ChatSession>> {
        None
    }

    /// Human inspector input to a child: queued, reserved and E7-checked like any delivery (§4.5).
    pub(crate) fn submit_human(self: &Rc<Self>, _worker: &WorkerId, _text: &str) {}

    pub(crate) async fn revoke(
        self: &Rc<Self>,
        _root: &WorkerId,
        _event: RevocationEvent,
    ) -> Result<(), ToolError> {
        Err(not_built())
    }
}
