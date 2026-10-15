//! Only an observed exit releases process capacity.
use super::supervisor::Supervisor;
use crate::agent_proc::{ExitWatch, StopPhase};
use agent_core::delegation::limits::Usage;
use agent_core::delegation::WorkerId;
use gtk4::glib;
use std::time::{Duration, Instant};
pub(super) async fn wait_exit(watch: &ExitWatch) {
    let started = Instant::now();
    let mut killed = false;
    while watch.phase() != StopPhase::Exited {
        if !killed && started.elapsed() >= Duration::from_secs(5) {
            watch.kill_now();
            killed = true;
        }
        glib::timeout_future(Duration::from_millis(20)).await;
    }
}
impl Supervisor {
    pub(super) fn usage(&self, root: &WorkerId) -> Usage {
        let state = self.state.borrow();
        let mut u = Usage::default();
        for w in state.workers.values().filter(|w| w.row.parent_id.is_some()) {
            u.live_processes += w.generations.len() as u32;
            if w.run.is_some() {
                u.active_runs_app += 1;
                if &w.root == root {
                    u.active_runs_root += 1;
                }
            }
            if w.pending.is_some() {
                u.pending_app += 1;
                if &w.root == root {
                    u.pending_root += 1;
                }
            }
        }
        u.waits_app = state.waits.len() as u32;
        u
    }
}
