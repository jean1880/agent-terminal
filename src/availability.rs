//! Which chat agents can be used right now, as detected, one state per driver.
//!
//! The app scans for the agents on start (and again on a timer, on focus after a while, and when
//! Settings change); everything that offers an agent (the New Thread menu, the default agent, the
//! model picker, the thread menu, the usage indicator, the background probes) asks this one object
//! and offers only what is [`Availability::Ready`]. Nothing is assumed: an agent not yet scanned
//! is [`Availability::Detecting`], not usable.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use agent_core::adapter::Driver;

use crate::agent_proc::AgentEnv;
use crate::probe::{ListenerSet, ProbeTarget, ProbeTargets};

/// How an agent stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// The scan has not answered yet.
    Detecting,
    /// Found: the binary to run.
    Ready(String),
    /// The scan found no binary.
    Missing,
    /// Switched off in Settings (not scanned).
    Disabled,
}

impl Availability {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }

    /// The binary, when [`Self::Ready`].
    pub fn path(&self) -> Option<&str> {
        match self {
            Self::Ready(path) => Some(path),
            _ => None,
        }
    }

    /// One line for Settings.
    pub fn describe(&self, driver: Driver) -> String {
        match self {
            Self::Detecting => "Detecting…".to_owned(),
            Self::Ready(path) => format!("Ready: {path}"),
            Self::Missing => format!("Not found. {}", driver.info().install_hint),
            Self::Disabled => "Off: not offered for new threads or switching".to_owned(),
        }
    }
}

/// The state a scan result stands for. `scanned` is `None` before the scan answered, then the
/// binary it found (or `None` for none). A disabled agent is never scanned.
pub fn classify(disabled: bool, scanned: Option<Option<&str>>) -> Availability {
    if disabled {
        return Availability::Disabled;
    }
    match scanned {
        None => Availability::Detecting,
        Some(Some(path)) => Availability::Ready(path.to_owned()),
        Some(None) => Availability::Missing,
    }
}

/// The probes a refresh may run: one per agent that is [`Availability::Ready`], in the
/// environment `env_of` gives it. A missing or disabled agent's binary is never spawned.
pub fn probe_targets(
    states: &[(Driver, Availability)],
    env_of: impl Fn(Driver) -> AgentEnv,
) -> ProbeTargets {
    ProbeTargets::new(
        states
            .iter()
            .filter_map(|(driver, state)| {
                state.path().map(|program| {
                    (
                        *driver,
                        ProbeTarget {
                            program: program.to_owned(),
                            env: env_of(*driver),
                        },
                    )
                })
            })
            .collect(),
    )
}

/// What an open thread's banner says when its agent is not usable, and its button: `None` while
/// the agent is ready or still being detected. `other` is the agent a handoff would go to (the
/// next ready one), so the button offers "Continue in ‹other›" only when there is one.
pub fn unavailable_banner(
    driver: Driver,
    state: &Availability,
    other: Option<Driver>,
) -> Option<(String, Option<String>)> {
    let why = match state {
        Availability::Missing => "is not available",
        Availability::Disabled => "is switched off",
        Availability::Detecting | Availability::Ready(_) => return None,
    };
    Some((
        format!("{} {why}", driver.info().label),
        other.map(|o| format!("Continue in {}", o.info().label)),
    ))
}

/// Rescans older than this are redone when the window regains focus.
pub const STALE_AFTER: Duration = Duration::from_secs(60);

/// The app-wide availability of every driver.
pub struct AgentAvailability {
    states: RefCell<Vec<(Driver, Availability)>>,
    listeners: ListenerSet,
    last_scan: Cell<Option<Instant>>,
}

thread_local! {
    static SHARED: Rc<AgentAvailability> = Rc::new(AgentAvailability::new());
}

impl Default for AgentAvailability {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentAvailability {
    /// Every driver starts [`Availability::Detecting`].
    pub fn new() -> Self {
        Self {
            states: RefCell::new(
                Driver::ALL
                    .into_iter()
                    .map(|d| (d, Availability::Detecting))
                    .collect(),
            ),
            listeners: ListenerSet::default(),
            last_scan: Cell::new(None),
        }
    }

    /// The app-wide instance (one per main thread).
    pub fn shared() -> Rc<AgentAvailability> {
        SHARED.with(Rc::clone)
    }

    pub fn get(&self, driver: Driver) -> Availability {
        self.states
            .borrow()
            .iter()
            .find(|(d, _)| *d == driver)
            .map_or(Availability::Detecting, |(_, s)| s.clone())
    }

    pub fn is_ready(&self, driver: Driver) -> bool {
        self.get(driver).is_ready()
    }

    /// A copy of every state, in registry order.
    pub fn all(&self) -> Vec<(Driver, Availability)> {
        self.states.borrow().clone()
    }

    /// Whether any agent is still being detected.
    pub fn any_detecting(&self) -> bool {
        self.states
            .borrow()
            .iter()
            .any(|(_, s)| *s == Availability::Detecting)
    }

    /// Sets one agent's state; listeners are told only when it changed.
    pub fn set(&self, driver: Driver, state: Availability) {
        let changed = {
            let mut states = self.states.borrow_mut();
            match states.iter_mut().find(|(d, _)| *d == driver) {
                Some((_, known)) if *known != state => {
                    *known = state;
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.listeners.notify();
        }
    }

    /// Records that a scan began now.
    pub fn mark_scan_started(&self) {
        self.last_scan.set(Some(Instant::now()));
    }

    /// Whether the last scan began more than [`STALE_AFTER`] ago (or there has been none).
    pub fn scan_is_stale(&self) -> bool {
        self.last_scan
            .get()
            .is_none_or(|at| at.elapsed() > STALE_AFTER)
    }

    /// Calls `f` after every change. The id disconnects it.
    pub fn connect_changed(&self, f: impl Fn() + 'static) -> u64 {
        self.listeners.add(f)
    }

    pub fn disconnect(&self, id: u64) {
        self.listeners.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scan_result_maps_to_one_state_and_disabled_wins() {
        assert_eq!(classify(false, None), Availability::Detecting);
        assert_eq!(
            classify(false, Some(Some("/usr/bin/claude"))),
            Availability::Ready("/usr/bin/claude".into())
        );
        assert_eq!(classify(false, Some(None)), Availability::Missing);
        // Off in Settings, whatever the scan says (or has not said yet).
        for scanned in [None, Some(None), Some(Some("/usr/bin/x"))] {
            assert_eq!(classify(true, scanned), Availability::Disabled);
        }
    }

    #[test]
    fn only_a_ready_agent_is_usable() {
        assert!(Availability::Ready("/x".into()).is_ready());
        for s in [
            Availability::Detecting,
            Availability::Missing,
            Availability::Disabled,
        ] {
            assert!(!s.is_ready(), "{s:?}");
            assert_eq!(s.path(), None);
        }
    }

    #[test]
    fn every_driver_starts_detecting_and_changes_notify_once() {
        let a = AgentAvailability::new();
        assert_eq!(a.all().len(), Driver::ALL.len());
        assert!(a.any_detecting());
        assert!(Driver::ALL.into_iter().all(|d| !a.is_ready(d)));

        let hits = Rc::new(Cell::new(0));
        let h = hits.clone();
        let id = a.connect_changed(move || h.set(h.get() + 1));
        a.set(Driver::Codex, Availability::Ready("/usr/bin/codex".into()));
        a.set(Driver::Codex, Availability::Ready("/usr/bin/codex".into()));
        assert_eq!(hits.get(), 1, "an unchanged state is not a change");
        assert!(a.is_ready(Driver::Codex));
        a.set(Driver::Claude, Availability::Missing);
        a.set(Driver::Agy, Availability::Disabled);
        assert!(!a.any_detecting());
        a.disconnect(id);
        a.set(Driver::Claude, Availability::Detecting);
        assert_eq!(hits.get(), 3);
    }

    #[test]
    fn a_scan_is_stale_until_one_has_started_and_for_a_minute_after() {
        let a = AgentAvailability::new();
        assert!(a.scan_is_stale());
        a.mark_scan_started();
        assert!(!a.scan_is_stale());
    }

    #[test]
    fn probes_run_for_ready_agents_only_in_their_own_environment() {
        let states = vec![
            (Driver::Claude, Availability::Ready("/bin/claude".into())),
            (Driver::Agy, Availability::Missing),
            (Driver::Codex, Availability::Disabled),
        ];
        let targets = probe_targets(&states, |d| AgentEnv {
            env: vec![("WHO".into(), d.info().key.into())],
            unset: Vec::new(),
        });
        assert_eq!(targets.count(), 1);
        let claude = targets.get(Driver::Claude).expect("claude is ready");
        assert_eq!(claude.program, "/bin/claude");
        assert_eq!(claude.env.env, [("WHO".to_owned(), "claude".to_owned())]);
        assert!(
            targets.get(Driver::Agy).is_none(),
            "a missing binary is never spawned"
        );
        assert!(targets.get(Driver::Codex).is_none());
        let none = probe_targets(&[(Driver::Claude, Availability::Detecting)], |_| {
            AgentEnv::default()
        });
        assert_eq!(none.count(), 0);
    }

    #[test]
    fn the_default_agent_is_chosen_among_ready_agents_only() {
        use crate::config::choose_default_agent;
        let a = AgentAvailability::new();
        // Nothing scanned yet: nothing is guessed.
        assert_eq!(choose_default_agent(None, None, |d| a.is_ready(d)), None);
        assert!(a.any_detecting());
        a.set(Driver::Claude, Availability::Missing);
        a.set(Driver::Agy, Availability::Disabled);
        assert_eq!(choose_default_agent(None, None, |d| a.is_ready(d)), None);
        a.set(Driver::Codex, Availability::Ready("/usr/bin/codex".into()));
        assert!(!a.any_detecting());
        // Even an explicit choice that is not ready falls through to one that is.
        assert_eq!(
            choose_default_agent(Some(Driver::Claude), Some(Driver::Agy), |d| a.is_ready(d)),
            Some(Driver::Codex)
        );
    }

    #[test]
    fn a_thread_on_an_unavailable_agent_offers_to_continue_elsewhere() {
        let banner = |state: &Availability, other| unavailable_banner(Driver::Claude, state, other);
        assert_eq!(
            banner(&Availability::Missing, Some(Driver::Codex)),
            Some((
                "Claude is not available".to_owned(),
                Some("Continue in Codex".to_owned())
            ))
        );
        assert_eq!(
            banner(&Availability::Disabled, None),
            Some(("Claude is switched off".to_owned(), None)),
            "no button when there is nowhere to go"
        );
        assert_eq!(banner(&Availability::Detecting, Some(Driver::Agy)), None);
        assert_eq!(
            banner(&Availability::Ready("/x".into()), Some(Driver::Agy)),
            None
        );
    }

    #[test]
    fn descriptions_say_what_to_do() {
        let missing = Availability::Missing.describe(Driver::Codex);
        assert!(
            missing.starts_with("Not found") && missing.contains("Codex"),
            "{missing}"
        );
        assert!(Availability::Ready("/usr/bin/agy".into())
            .describe(Driver::Agy)
            .contains("/usr/bin/agy"));
    }
}
