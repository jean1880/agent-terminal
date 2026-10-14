//! Small pieces the background probes share (Claude, Codex, agy): coalescing concurrent callers
//! onto one process, and joining a fixed number of background tasks.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use crate::agent_proc::AgentEnv;

/// What one agent's background probe runs: its resolved binary and the environment its threads
/// get (profile env file, `clear_env`), so a probe sees the account a thread would.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProbeTarget {
    pub program: String,
    pub env: AgentEnv,
}

/// The probes of one refresh: one per agent that is ready (see `availability::probe_targets`).
/// A missing, disabled or not yet detected agent has none, so its binary is never spawned.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProbeTargets {
    targets: Vec<(agent_core::adapter::Driver, ProbeTarget)>,
}

impl ProbeTargets {
    pub fn new(targets: Vec<(agent_core::adapter::Driver, ProbeTarget)>) -> Self {
        Self { targets }
    }

    pub fn get(&self, driver: agent_core::adapter::Driver) -> Option<&ProbeTarget> {
        self.targets
            .iter()
            .find(|(d, _)| *d == driver)
            .map(|(_, t)| t)
    }

    /// How many background tasks a refresh runs.
    pub fn count(&self) -> u8 {
        u8::try_from(self.targets.len()).unwrap_or(u8::MAX)
    }
}

/// Callers waiting for one in-flight probe's result. A probe is expensive (a process), so the
/// first caller starts it and callers with the same target join it. Other targets wait for
/// their own probe, coalesced by target and run in arrival order.
/// Serial probes avoid overlapping processes; each target's wait is bounded by the probes' own timeouts.
pub struct InFlight<R> {
    running: RefCell<Option<Flight<R>>>,
    queued: RefCell<VecDeque<Flight<R>>>,
}

type Waiter<R> = Box<dyn FnOnce(&R)>;

struct Flight<R> {
    target: ProbeTarget,
    waiters: Vec<Waiter<R>>,
}

impl<R> InFlight<R> {
    pub const fn new() -> Self {
        Self {
            running: RefCell::new(None),
            queued: RefCell::new(VecDeque::new()),
        }
    }

    /// Registers `done`. True when the caller must start the probe (none was running).
    pub fn join(&self, target: &ProbeTarget, done: impl FnOnce(&R) + 'static) -> bool {
        let mut running = self.running.borrow_mut();
        match running.as_mut() {
            Some(flight) if flight.target == *target => {
                flight.waiters.push(Box::new(done));
                false
            }
            Some(_) => {
                let mut queued = self.queued.borrow_mut();
                if let Some(flight) = queued.iter_mut().find(|f| f.target == *target) {
                    flight.waiters.push(Box::new(done));
                } else {
                    queued.push_back(Flight {
                        target: target.clone(),
                        waiters: vec![Box::new(done)],
                    });
                }
                false
            }
            None => {
                *running = Some(Flight {
                    target: target.clone(),
                    waiters: vec![Box::new(done)],
                });
                true
            }
        }
    }

    /// Hands `result` only to this target's waiters. Returns the next target the owner must
    /// probe, reserved before callbacks run so a reentrant caller joins rather than overtakes it.
    pub fn finish(&self, result: &R) -> Option<ProbeTarget> {
        let finished = self.running.borrow_mut().take();
        let next = self.queued.borrow_mut().pop_front();
        let target = next.as_ref().map(|f| f.target.clone());
        *self.running.borrow_mut() = next;
        for w in finished.into_iter().flat_map(|f| f.waiters) {
            w(result);
        }
        target
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
        let target = ProbeTarget::default();
        for i in 0..3 {
            let got = got.clone();
            if flight.join(&target, move |r| got.borrow_mut().push((i, *r))) {
                starts += 1;
            }
        }
        assert_eq!(starts, 1);
        assert_eq!(flight.finish(&7), None);
        assert_eq!(*got.borrow(), [(0, 7), (1, 7), (2, 7)]);
        // Cleared: the next caller starts a new probe.
        assert!(flight.join(&target, |_| {}));
    }

    #[test]
    fn a_caller_with_another_target_gets_its_own_probe() {
        let flight: InFlight<String> = InFlight::new();
        let got = Rc::new(RefCell::new(Vec::new()));
        let a = ProbeTarget {
            program: "old".into(),
            ..Default::default()
        };
        let b = ProbeTarget {
            program: "new".into(),
            ..Default::default()
        };
        let mut c = b.clone();
        c.env.unset.push("HOME".into());
        for (i, target) in [(0, &a), (1, &b), (2, &b), (3, &c), (4, &a)] {
            let got = got.clone();
            assert_eq!(
                flight.join(target, move |r| got.borrow_mut().push((i, r.clone()))),
                i == 0
            );
        }
        let mut target = Some(a.clone());
        let mut starts = Vec::new();
        while let Some(t) = target {
            starts.push(t.clone());
            // Fake runner: a target's program and environment determine its result.
            target = flight.finish(&format!("{}:{:?}", t.program, t.env.unset));
        }
        assert_eq!(starts, [a, b, c]);
        assert_eq!(
            *got.borrow(),
            [
                (0, "old:[]".into()),
                (4, "old:[]".into()),
                (1, "new:[]".into()),
                (2, "new:[]".into()),
                (3, "new:[\"HOME\"]".into())
            ]
        );
    }

    #[test]
    fn a_reentrant_waiter_joins_the_reserved_next_probe() {
        let flight = Rc::new(InFlight::<u8>::new());
        let a = ProbeTarget::default();
        let b = ProbeTarget {
            program: "next".into(),
            ..Default::default()
        };
        let got = Rc::new(Cell::new(0));
        let (f, target, g) = (flight.clone(), b.clone(), got.clone());
        assert!(flight.join(&a, move |_| {
            assert!(!f.join(&target, move |r| g.set(*r)));
        }));
        assert!(!flight.join(&b, |_| {}));
        assert_eq!(flight.finish(&1), Some(b));
        assert_eq!(got.get(), 0);
        assert_eq!(flight.finish(&2), None);
        assert_eq!(got.get(), 2);
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
    fn an_agent_is_probed_only_when_it_has_a_target() {
        use agent_core::adapter::Driver;
        let t = ProbeTargets::default();
        assert_eq!(t.count(), 0);
        assert!(t.get(Driver::Codex).is_none());
        let t = ProbeTargets::new(vec![(Driver::Codex, ProbeTarget::default())]);
        assert_eq!(t.count(), 1);
        assert!(t.get(Driver::Codex).is_some() && t.get(Driver::Claude).is_none());
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
