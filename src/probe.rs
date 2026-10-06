//! Small pieces the background probes share (Claude, Codex, agy): coalescing concurrent callers
//! onto one process, and joining a fixed number of background tasks.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::agent_proc::AgentEnv;

/// What one agent's background probe runs: its resolved binary and the environment its threads
/// get (profile env file, `clear_env`), so a probe sees the account a thread would.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProbeTarget {
    pub program: String,
    pub env: AgentEnv,
}

/// The probes of one refresh. Codex is `None` when it is not installed or not enabled: nothing
/// is spawned for it then.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProbeTargets {
    pub claude: ProbeTarget,
    pub agy: ProbeTarget,
    pub codex: Option<ProbeTarget>,
}

impl ProbeTargets {
    /// How many background tasks a refresh runs.
    pub fn count(&self) -> u8 {
        2 + u8::from(self.codex.is_some())
    }
}

/// Callers waiting for one in-flight probe's result. A probe is expensive (a process), so the
/// first caller starts it and the rest join it.
pub struct InFlight<R> {
    waiters: RefCell<Option<Vec<Waiter<R>>>>,
}

type Waiter<R> = Box<dyn FnOnce(&R)>;

impl<R> InFlight<R> {
    pub const fn new() -> Self {
        Self {
            waiters: RefCell::new(None),
        }
    }

    /// Registers `done`. True when the caller must start the probe (none was running).
    pub fn join(&self, done: impl FnOnce(&R) + 'static) -> bool {
        let mut waiters = self.waiters.borrow_mut();
        match waiters.as_mut() {
            Some(list) => {
                list.push(Box::new(done));
                false
            }
            None => {
                *waiters = Some(vec![Box::new(done)]);
                true
            }
        }
    }

    /// Hands `result` to every waiter and clears the slot for the next probe.
    pub fn finish(&self, result: &R) {
        let waiters = self.waiters.borrow_mut().take().unwrap_or_default();
        for w in waiters {
            w(result);
        }
    }
}

/// Returns a callback to call once per task; the `n`th call runs `all_done`.
pub fn join_n(n: u8, all_done: impl Fn() + 'static) -> Rc<dyn Fn()> {
    let pending = Cell::new(n);
    Rc::new(move || {
        pending.set(pending.get().saturating_sub(1));
        if pending.get() == 0 {
            all_done();
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_first_caller_starts_and_all_get_the_result() {
        let flight: InFlight<u8> = InFlight::new();
        let got = Rc::new(RefCell::new(Vec::new()));
        let mut starts = 0;
        for i in 0..3 {
            let got = got.clone();
            if flight.join(move |r| got.borrow_mut().push((i, *r))) {
                starts += 1;
            }
        }
        assert_eq!(starts, 1);
        flight.finish(&7);
        assert_eq!(*got.borrow(), [(0, 7), (1, 7), (2, 7)]);
        // Cleared: the next caller starts a new probe.
        assert!(flight.join(|_| {}));
    }

    #[test]
    fn codex_is_probed_only_when_it_has_a_target() {
        let mut t = ProbeTargets::default();
        assert_eq!(t.count(), 2);
        t.codex = Some(ProbeTarget::default());
        assert_eq!(t.count(), 3);
    }

    #[test]
    fn join_n_fires_on_the_last_call_only() {
        let fired = Rc::new(Cell::new(0));
        let f = fired.clone();
        let done = join_n(3, move || f.set(f.get() + 1));
        done();
        done();
        assert_eq!(fired.get(), 0);
        done();
        assert_eq!(fired.get(), 1);
    }
}
