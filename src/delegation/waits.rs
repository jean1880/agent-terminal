//! Wait registrations belong to a connection and principal turn.
use super::supervisor::Supervisor;
use agent_core::delegation::WorkerId;
impl Supervisor {
    pub(super) fn release_waits(&self, worker: &WorkerId) {
        self.state.borrow_mut().waits.retain(|_, w| w != worker);
    }
}
