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

/// Change listeners that can be disconnected: a view that goes away must not leave its
/// callback behind in an app-wide object.
#[derive(Default)]
pub struct ListenerSet {
    next: Cell<u64>,
    items: RefCell<Vec<(u64, Listener)>>,
}

type Listener = Rc<dyn Fn()>;

impl ListenerSet {
    /// Registers `f`; the id disconnects it again.
    pub fn add(&self, f: impl Fn() + 'static) -> u64 {
        let id = self.next.get() + 1;
        self.next.set(id);
        self.items.borrow_mut().push((id, Rc::new(f)));
        id
    }

    pub fn remove(&self, id: u64) {
        self.items.borrow_mut().retain(|(i, _)| *i != id);
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.items.borrow().len()
    }

    /// Calls every listener. They are cloned out first, so one may call back into the owner,
    /// connect or disconnect.
    pub fn notify(&self) {
        let items: Vec<_> = self.items.borrow().iter().map(|(_, f)| f.clone()).collect();
        for f in items {
            f();
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
    fn a_disconnected_listener_is_not_called_and_one_may_disconnect_during_notify() {
        let set = Rc::new(ListenerSet::default());
        let hits = Rc::new(Cell::new(0));
        let h = hits.clone();
        let a = set.add(move || h.set(h.get() + 1));
        let h = hits.clone();
        let s = set.clone();
        let b = set.add(move || {
            h.set(h.get() + 10);
            s.remove(a); // from inside a notify
        });
        set.notify();
        assert_eq!(hits.get(), 11);
        set.notify();
        assert_eq!(hits.get(), 21, "a was removed, b ran again");
        set.remove(b);
        assert_eq!(set.len(), 0);
        set.notify();
        assert_eq!(hits.get(), 21);
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
