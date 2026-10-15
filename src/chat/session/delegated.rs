//! Lane B: a session's typed delegated-input API (`docs/delegation/p4-supervisor.md` §3.4).
//!
//! Wave 0 stubs with frozen signatures. They fail closed: no delegated input is ever delivered,
//! and a session cannot be built with hooks or a child policy until Lane B lands.

use std::rc::Rc;

use agent_core::adapter::{Adapter, Mode, OpenSession};
use agent_core::delegation::{ErrorCode, ProcessGeneration};
use agent_kit::store::ThreadId;

use super::{Approval, ChatSession, LaunchEnv, SessionStore};
use crate::agent_proc::ExitWatch;
use crate::chat::{ChatBackend, EnvelopeSink};
use crate::delegation::seam::{DelegatedInput, DelegatedOutcome, SessionOptions};

impl ChatSession {
    /// [`ChatSession::with_env`] with delegation hooks and, for a supervised child, its policy.
    #[allow(clippy::too_many_arguments)] // `with_env`'s seven, plus the options
    pub(crate) fn with_options(
        adapter: Box<dyn Adapter>,
        open: OpenSession,
        store: SessionStore,
        thread: ThreadId,
        sink: EnvelopeSink,
        approval: Approval,
        env: LaunchEnv,
        opts: SessionOptions,
    ) -> Rc<Self> {
        // Wave 0: refuse to run a child or a bound root without the seam behind it. Routes are
        // all unverified, so nothing in the app reaches this with options set.
        assert!(
            opts.hooks.is_none() && opts.child.is_none(),
            "delegated sessions are not built yet"
        );
        Self::with_env(adapter, open, store, thread, sink, approval, env)
    }

    /// Delivers one reserved, E7-checked input (`StartTurn`, `WriteLive` or `SteerNow`).
    pub(crate) fn deliver_delegated(&self, _input: DelegatedInput) -> DelegatedOutcome {
        DelegatedOutcome::Refused(ErrorCode::InternalError)
    }

    /// Interrupts the running turn; false when there is none.
    pub(crate) fn interrupt_run(&self) -> bool {
        false
    }

    /// Ends every pending approval (revocation): Claude and Codex deny, agy expires.
    pub(crate) fn expire_approvals(&self, _reason: &str) {}

    /// Stops the process for a rebind and returns that generation's watch; nothing restarts it
    /// until [`Self::rebind_start`].
    pub(crate) fn rebind_stop(&self) -> Option<ExitWatch> {
        None
    }

    pub(crate) fn rebind_start(&self) {}

    pub(crate) fn effective_mode(&self) -> Mode {
        self.status().mode
    }

    /// The contract's process generation: bumped only when a process starts.
    pub(crate) fn process_generation(&self) -> ProcessGeneration {
        ProcessGeneration(0)
    }

    /// The watch of every generation not yet seen to exit.
    pub(crate) fn exit_watches(&self) -> Vec<ExitWatch> {
        self.exit_watch().into_iter().collect()
    }
}
