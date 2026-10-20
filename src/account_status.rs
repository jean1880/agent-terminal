//! Who each agent is signed in as and how much of its plan is used, app-wide.
//!
//! Fed two ways: [`AccountStatus::observe`] ingests the `QuotaUpdated` envelopes a thread's
//! adapter emits (Claude: every turn; agy: after the host requests `Control::Usage` once a turn
//! completes; Codex: `account/rateLimits/updated`), and [`AccountStatus::refresh`] probes the
//! agents without a prompt so the indicator is filled before any thread has run (Claude through
//! the shared [`crate::claude_probe`], Codex through [`crate::codex_probe`], agy through
//! `-p /usage` plus its signed-in account file).
//!
//! The email address is shown in the UI only and is never logged.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use agent_core::adapter::Driver;
use agent_core::event::{Account, Envelope, Event, QuotaWindow};
use agent_core::quota::{agy_account, agy_signed_out, agy_usage};
use gtk4::{gio, glib};
use serde_json::Value;
use tracing::{debug, warn};

use crate::agent_proc::{run_side_full, AGY_TIMEOUT};
use crate::freshness::{
    age_of, freshness, is_due, latest, now_epoch, Freshness, ReadAt, ACCOUNT_TTL,
};
use crate::probe::{join_n, ListenerSet, ProbeTargets};
use crate::{claude_probe, codex_probe};

/// One agent's account and quota windows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub account: Option<Account>,
    pub windows: Vec<QuotaWindow>,
    /// When a window was last updated (epoch seconds); `None` before the first update.
    pub updated_at: Option<i64>,
    /// The agent says no one is signed in to it: installed, but its login was skipped or has
    /// expired. Its turns fail and its usage cannot be read until the user signs in.
    pub signed_out: bool,
    /// The last successful reading (epoch seconds): a probe that answered, or a live
    /// `QuotaUpdated` from a thread. How it may be shown follows from this
    /// ([`crate::freshness`]); `None` reads as Expired.
    pub read_at: Option<i64>,
    /// The last probe failed (an error or a timeout): old data says it could not refresh.
    pub refresh_failed: bool,
    /// A probe of this agent is running: with nothing to show yet, the indicator says it is
    /// loading rather than that it could not read.
    pub probing: bool,
}

/// Merges an update in: a new account replaces the old one (`None` keeps it), and windows are
/// matched by (group, label): known ones are replaced in place, new ones appended, windows the
/// update does not mention are kept (a per-turn event carries only two of the plan's windows).
/// Returns whether anything changed.
pub fn merge(
    snap: &mut Snapshot,
    account: Option<Account>,
    windows: Vec<QuotaWindow>,
    now: i64,
) -> bool {
    let mut changed = false;
    if let Some(account) = account {
        if snap.account.as_ref() != Some(&account) {
            snap.account = Some(account);
            changed = true;
        }
        // An account the agent reports is someone signed in.
        if snap.signed_out {
            snap.signed_out = false;
            changed = true;
        }
    }
    for w in windows {
        match snap
            .windows
            .iter_mut()
            .find(|k| k.group == w.group && k.label == w.label)
        {
            Some(known) if *known != w => {
                *known = w;
                changed = true;
            }
            Some(_) => {}
            None => {
                snap.windows.push(w);
                changed = true;
            }
        }
        snap.updated_at = Some(now);
    }
    changed
}

/// `~/.gemini/google_accounts.json`, from the value of `HOME`.
pub fn google_accounts_path(home: Option<PathBuf>) -> Option<PathBuf> {
    let home = home.filter(|h| h.is_absolute())?;
    Some(home.join(".gemini/google_accounts.json"))
}

fn agy_accounts_path(env: &crate::agent_proc::AgentEnv) -> Option<PathBuf> {
    google_accounts_path(crate::agent_home::effective_home(env))
}

/// The app-wide account and usage of each agent.
///
/// Ceiling: ONE slot per driver. Every profile of a driver shares it, and [`Self::observe`] takes
/// no profile, so two profiles signed in to different accounts of one agent would overwrite each
/// other here; the probe runs in the agent's configured profile's environment. Keying by
/// profile is a separate change; any key it introduces must come from the profile name and the
/// env-file path, never from env values, and is never logged or persisted.
///
/// When to probe: [`Self::refresh_if_due`] (focus, the periodic timer) probes an agent only when
/// its last reading or attempt is [`ACCOUNT_TTL`] old; a live `QuotaUpdated` counts as a reading,
/// so an agent in use is never probed. A signed-out agy is never probed by a clock: only
/// [`Self::recheck`] (the user's "Check again") does.
pub struct AccountStatus {
    claude: RefCell<Snapshot>,
    agy: RefCell<Snapshot>,
    codex: RefCell<Snapshot>,
    listeners: ListenerSet,
    /// Told of every successful reading, changed or not (`read_at` moved): the usage file.
    read_listeners: ListenerSet,
    refreshing: Cell<bool>,
    /// Each agent's last finished probe in this run (Claude, agy, Codex), successful or not: with
    /// `read_at`, the due clock.
    tried_at: Cell<ReadAt>,
    /// Agents the user asked to check again ([`Self::recheck`]): probed by the next refresh even
    /// if signed out (agy is otherwise skipped then).
    forced: RefCell<Vec<Driver>>,
    /// A refresh asked for while one ran: run again with these targets once it ends, so a
    /// "Check again" is never dropped.
    queued: RefCell<Option<ProbeTargets>>,
}

thread_local! {
    static SHARED: Rc<AccountStatus> = Rc::new(AccountStatus::new());
}

impl AccountStatus {
    pub fn new() -> Self {
        Self {
            claude: RefCell::default(),
            agy: RefCell::default(),
            codex: RefCell::default(),
            listeners: ListenerSet::default(),
            read_listeners: ListenerSet::default(),
            refreshing: Cell::new(false),
            tried_at: Cell::new(ReadAt::default()),
            forced: RefCell::default(),
            queued: RefCell::default(),
        }
    }

    /// The app-wide instance (one per main thread).
    pub fn shared() -> Rc<AccountStatus> {
        SHARED.with(Rc::clone)
    }

    fn slot(&self, driver: Driver) -> &RefCell<Snapshot> {
        match driver {
            Driver::Claude => &self.claude,
            Driver::Agy => &self.agy,
            Driver::Codex => &self.codex,
        }
    }

    /// A copy of the agent's current snapshot.
    pub fn snapshot(&self, driver: Driver) -> Snapshot {
        self.slot(driver).borrow().clone()
    }

    /// Calls `f` (on the main thread) after every change. The id disconnects it.
    pub fn connect_changed(&self, f: impl Fn() + 'static) -> u64 {
        self.listeners.add(f)
    }

    pub fn disconnect(&self, id: u64) {
        self.listeners.remove(id);
    }

    /// Calls `f` after every successful reading, even one that changed nothing shown (so no
    /// repaint): what keeps an outside reader's `read_at` current. Never disconnected.
    pub fn connect_read(&self, f: impl Fn() + 'static) -> u64 {
        self.read_listeners.add(f)
    }

    fn notify(&self) {
        self.listeners.notify();
    }

    fn update(&self, driver: Driver, account: Option<Account>, windows: Vec<QuotaWindow>) {
        self.update_at(driver, account, windows, now_epoch());
    }

    /// A successful reading at `now` (a probe that answered, or a live update): merged in, and
    /// the agent's clock restarts. Listeners hear of it when the data changed or it was not
    /// already shown as Fresh.
    fn update_at(
        &self,
        driver: Driver,
        account: Option<Account>,
        windows: Vec<QuotaWindow>,
        now: i64,
    ) {
        let changed = {
            let mut snap = self.slot(driver).borrow_mut();
            let was_fresh = !snap.refresh_failed
                && freshness(age_of(snap.read_at, now), ACCOUNT_TTL) == Freshness::Fresh;
            snap.read_at = Some(now);
            snap.refresh_failed = false;
            merge(&mut snap, account, windows, now) || !was_fresh
        };
        if changed {
            self.notify();
        }
        self.read_listeners.notify();
    }

    /// A probe of `driver` failed: what is shown stays, and says it could not refresh.
    fn probe_failed(&self, driver: Driver) {
        let changed = !std::mem::replace(&mut self.slot(driver).borrow_mut().refresh_failed, true);
        if changed {
            self.notify();
        }
    }

    fn mark_tried(&self, driver: Driver, now: i64) {
        let mut tried = self.tried_at.get();
        tried.set(driver, Some(now));
        self.tried_at.set(tried);
    }

    /// Records whether a probe of `driver` is running; listeners hear of a change. Only the
    /// probes themselves (and the demo's loading scene) set it.
    pub(crate) fn set_probing(&self, driver: Driver, probing: bool) {
        let changed =
            std::mem::replace(&mut self.slot(driver).borrow_mut().probing, probing) != probing;
        if changed {
            self.notify();
        }
    }

    /// `driver`'s probe ended at `now`, answered or not: its clock restarts and it is no longer
    /// loading.
    fn probe_finished(&self, driver: Driver, now: i64) {
        self.mark_tried(driver, now);
        self.set_probing(driver, false);
    }

    /// Whether a background check at `now` should probe `driver`: the user asked to check it, or
    /// its last reading (or failed attempt) is [`ACCOUNT_TTL`] old, or there was none.
    pub fn is_due(&self, driver: Driver, now: i64) -> bool {
        if self.is_rechecking(driver) {
            return true;
        }
        let last = latest(
            self.slot(driver).borrow().read_at,
            self.tried_at.get().get(driver),
        );
        is_due(last, now, ACCOUNT_TTL)
    }

    /// Records whether `driver` says no one is signed in. Signing out also forgets the account
    /// and windows shown for it, which belonged to whoever was signed in before.
    pub fn set_signed_out(&self, driver: Driver, signed_out: bool) {
        let changed = {
            let mut snap = self.slot(driver).borrow_mut();
            if snap.signed_out == signed_out {
                false
            } else {
                snap.signed_out = signed_out;
                if signed_out {
                    snap.account = None;
                    snap.windows.clear();
                }
                true
            }
        };
        if changed {
            self.notify();
        }
    }

    /// The user signed in (or says so): the next [`Self::refresh`] probes `driver` again, agy
    /// included. The verdict stands until that probe answers, so nothing claims "signed in"
    /// before anything checked.
    pub fn recheck(&self, driver: Driver) {
        let mut forced = self.forced.borrow_mut();
        if !forced.contains(&driver) {
            forced.push(driver);
        }
    }

    /// Whether a check the user asked for is still waiting for its probe.
    pub fn is_rechecking(&self, driver: Driver) -> bool {
        self.forced.borrow().contains(&driver)
    }

    /// Whether `driver` is known to have no one signed in.
    pub fn is_signed_out(&self, driver: Driver) -> bool {
        self.slot(driver).borrow().signed_out
    }

    /// Ingests an envelope from `driver`'s thread; only `QuotaUpdated` matters here.
    pub fn observe(&self, driver: Driver, envelope: &Envelope) {
        if let Event::QuotaUpdated { account, windows } = &envelope.event {
            self.update(driver, account.clone(), windows.clone());
        }
    }

    /// Probes every agent in the background and returns at once. No agent is sent a prompt, and
    /// each runs in the environment its threads get, so the account shown is the one a thread
    /// would use. Asked again while a refresh runs, it runs once more when that one ends.
    ///
    /// A signed-out agy is not probed again: probing it starts agy's own login, which opens a
    /// browser each time. [`Self::recheck`] (the user's "Check again") probes it once more.
    pub fn refresh(self: &Rc<Self>, targets: &ProbeTargets) {
        if self.refreshing.replace(true) {
            debug!("account status refresh already running; queued another");
            *self.queued.borrow_mut() = Some(targets.clone());
            return;
        }
        self.run(targets, |_| true);
    }

    /// The background check (focus, the periodic timer): probes only the agents that are due at
    /// `now` ([`Self::is_due`]). Nothing due: nothing spawns. A refresh already running answers
    /// for this one.
    pub fn refresh_if_due(self: &Rc<Self>, targets: &ProbeTargets, now: i64) {
        if self.refreshing.get() {
            debug!("account status refresh already running");
            return;
        }
        let due: Vec<Driver> = Driver::ALL
            .into_iter()
            .filter(|d| self.is_due(*d, now))
            .collect();
        if due
            .iter()
            .all(|d| !self.probeable(*d) || targets.get(*d).is_none())
        {
            return;
        }
        self.refreshing.set(true);
        // `run` notifies as each probe starts: an agent whose reading has aged is redrawn muted
        // while its probe runs, and one with nothing to show is drawn loading.
        self.run(targets, |d| due.contains(&d));
    }

    /// A caller asked for fresh usage (`cross_agent_capabilities` with `refresh`): probes the
    /// agents whose last reading *or attempt* is at least `floor` seconds old at `now`, so a
    /// failing probe cannot be restarted by every call. The same gate as the clock otherwise: a
    /// signed-out agy is never probed (that opens its browser login), and this never marks an
    /// agent for "Check again". Decided per agent, whatever else runs: an agent already being
    /// probed (by the clock, the user or another caller) is not probed twice, and the others
    /// that are due start now. Returns the agents the caller may wait for: those started and
    /// those already being probed.
    pub fn refresh_if_older_than(
        self: &Rc<Self>,
        targets: &ProbeTargets,
        floor: i64,
        now: i64,
    ) -> Vec<Driver> {
        let in_flight: Vec<Driver> = Driver::ALL
            .into_iter()
            .filter(|d| targets.get(*d).is_some() && self.slot(*d).borrow().probing)
            .collect();
        let jobs = self.jobs(targets, |d| {
            let last = latest(self.slot(d).borrow().read_at, self.tried_at.get().get(d));
            is_due(last, now, floor.saturating_add(crate::freshness::DUE_SLACK))
        });
        let mut waiting: Vec<Driver> = jobs.iter().map(|(d, _)| *d).collect();
        waiting.extend(in_flight);
        if !jobs.is_empty() {
            // Not the clock's latch: a refresh started by the clock or the user keeps its own.
            self.spawn_probes(jobs, Rc::new(|| {}));
        }
        waiting
    }

    /// [`Self::refresh_if_older_than`], then waits (on the main loop, never blocking it) until
    /// every probe it may wait for has ended, or `timeout`. The listener is removed on return
    /// and when the future is dropped; a probe still running is left to finish, and updates the
    /// shared status for everyone.
    pub async fn refresh_and_wait(
        self: &Rc<Self>,
        targets: &ProbeTargets,
        floor: i64,
        now: i64,
        timeout: std::time::Duration,
    ) {
        let waiting = self.refresh_if_older_than(targets, floor, now);
        let done = {
            let me = Rc::downgrade(self);
            move || {
                me.upgrade()
                    .is_none_or(|st| waiting.iter().all(|d| !st.slot(*d).borrow().probing))
            }
        };
        if done() {
            return;
        }
        let (send, answer) = tokio::sync::oneshot::channel::<()>();
        let send = RefCell::new(Some(send));
        let id = self.connect_changed(move || {
            if done() {
                if let Some(send) = send.borrow_mut().take() {
                    let _ = send.send(());
                }
            }
        });
        /// Disconnects the listener however the wait ends (answered, timed out or dropped).
        struct Listening(std::rc::Weak<AccountStatus>, u64);
        impl Drop for Listening {
            fn drop(&mut self) {
                if let Some(st) = self.0.upgrade() {
                    st.disconnect(self.1);
                }
            }
        }
        let _listening = Listening(Rc::downgrade(self), id);
        if glib::future_with_timeout(timeout, answer).await.is_err() {
            debug!("usage refresh still running after the wait");
        }
    }

    /// A signed-out agy is never probed unless the user asked to check it: probing it starts
    /// agy's own login, which opens a browser.
    fn probeable(&self, driver: Driver) -> bool {
        !(driver == Driver::Agy && self.is_signed_out(driver) && !self.is_rechecking(driver))
    }

    /// The probes to start for the agents `wanted` accepts: each has a target (missing, disabled
    /// and still-detecting agents have none), may be probed (a signed-out agy only when the user
    /// asked to check it), and is not being probed already (never two probes of one agent).
    fn jobs(
        &self,
        targets: &ProbeTargets,
        wanted: impl Fn(Driver) -> bool,
    ) -> Vec<(Driver, crate::probe::ProbeTarget)> {
        Driver::ALL
            .into_iter()
            .filter(|d| self.probeable(*d) && wanted(*d) && !self.slot(*d).borrow().probing)
            .filter_map(|d| targets.get(d).map(|t| (d, t.clone())))
            .collect()
    }

    /// Probes the agents with a target that `wanted` accepts; `refreshing` is already set.
    fn run(self: &Rc<Self>, targets: &ProbeTargets, wanted: impl Fn(Driver) -> bool) {
        let jobs = self.jobs(targets, wanted);
        // Each probed agent's own check is answered by this refresh.
        self.forced
            .borrow_mut()
            .retain(|d| !jobs.iter().any(|(j, _)| j == d));
        if jobs.is_empty() {
            self.refreshing.set(false);
            return; // nothing is ready: nothing is spawned
        }
        let finish = {
            let me = self.clone();
            join_n(jobs.len() as u8, move || {
                me.refreshing.set(false);
                let again = me.queued.borrow_mut().take();
                if let Some(targets) = again {
                    me.refresh(&targets);
                }
            })
        };
        self.spawn_probes(jobs, finish);
    }

    /// Starts `jobs`; `finish` runs as each one ends.
    fn spawn_probes(
        self: &Rc<Self>,
        jobs: Vec<(Driver, crate::probe::ProbeTarget)>,
        finish: Rc<dyn Fn()>,
    ) {
        // Every probed agent is loading until its probe ends; one repaint says so for all.
        for (driver, _) in &jobs {
            self.slot(*driver).borrow_mut().probing = true;
        }
        self.notify();
        for (driver, target) in jobs {
            let (me, done) = (self.clone(), finish.clone());
            match driver {
                Driver::Claude => {
                    claude_probe::probe_shared(&target.program, &target.env, move |result| {
                        match result {
                            Ok(probe) => {
                                me.set_signed_out(driver, probe.signed_out);
                                me.update(driver, probe.account.clone(), probe.windows.clone());
                            }
                            Err(_) => me.probe_failed(driver),
                        }
                        me.probe_finished(driver, now_epoch());
                        done();
                    });
                }
                Driver::Codex => {
                    codex_probe::probe_shared(&target.program, &target.env, move |result| {
                        match result {
                            Ok(probe) => {
                                me.set_signed_out(driver, probe.signed_out);
                                me.update(driver, probe.account.clone(), probe.windows.clone());
                            }
                            Err(_) => me.probe_failed(driver),
                        }
                        me.probe_finished(driver, now_epoch());
                        done();
                    });
                }
                Driver::Agy => {
                    let target = target.clone();
                    glib::spawn_future_local(async move {
                        let side = run_side_full(
                            vec![
                                target.program,
                                "-p".to_owned(),
                                "/usage".to_owned(),
                                "--output-format".to_owned(),
                                "json".to_owned(),
                            ],
                            None,
                            &target.env,
                            AGY_TIMEOUT,
                        )
                        .await;
                        let (out, ok) = (side.stdout, side.ok);
                        // Its login prompt (stderr), or the error it ends with (stdout): it
                        // waits for a browser login, so this is usually a timeout.
                        if agy_signed_out(&side.stderr) || agy_signed_out(&out) {
                            warn!("agy is not signed in");
                            me.set_signed_out(Driver::Agy, true);
                            me.probe_finished(Driver::Agy, now_epoch());
                            done();
                            return;
                        }
                        if !ok {
                            // Nothing was read: what is shown stays, says it could not refresh,
                            // and the reading's clock does not restart.
                            warn!("agy /usage failed or timed out");
                            me.probe_failed(Driver::Agy);
                            me.probe_finished(Driver::Agy, now_epoch());
                            done();
                            return;
                        }
                        let windows = serde_json::from_str::<Value>(out.trim())
                            .map(|v| agy_usage(&v))
                            .unwrap_or_default();
                        me.set_signed_out(Driver::Agy, false);
                        // A few hundred bytes of JSON, read off the main thread like all file I/O
                        // here.
                        let account = match agy_accounts_path(&target.env) {
                            Some(path) => {
                                gio::spawn_blocking(move || std::fs::read_to_string(path))
                                    .await
                                    .ok()
                                    .and_then(Result::ok)
                                    .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                                    .and_then(|v| agy_account(&v))
                            }
                            None => None,
                        };
                        me.update(Driver::Agy, account, windows);
                        me.probe_finished(Driver::Agy, now_epoch());
                        done();
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(group: Option<&str>, label: &str, used: f64) -> QuotaWindow {
        QuotaWindow {
            group: group.map(str::to_owned),
            label: label.into(),
            used,
            resets_at: None,
        }
    }

    fn acct(label: &str) -> Account {
        Account {
            label: label.into(),
            plan: None,
            provider: None,
        }
    }

    /// Signed out forgets the previous account; an account reported later is someone signed in;
    /// a recheck keeps the verdict until the probe it queues answers.
    #[test]
    fn signed_out_follows_what_the_agent_reports() {
        let status = AccountStatus::new();
        status.update(
            Driver::Codex,
            Some(acct("a")),
            vec![win(None, "5-hour", 0.5)],
        );
        status.set_signed_out(Driver::Codex, true);
        let snap = status.snapshot(Driver::Codex);
        assert!(snap.signed_out);
        assert_eq!(snap.account, None, "not the old login's account");
        assert!(snap.windows.is_empty());
        assert!(!status.is_signed_out(Driver::Claude), "per agent");

        status.update(Driver::Codex, Some(acct("b")), vec![]);
        assert!(
            !status.is_signed_out(Driver::Codex),
            "an account means signed in"
        );

        status.set_signed_out(Driver::Codex, true);
        assert!(!status.is_rechecking(Driver::Codex));
        status.recheck(Driver::Codex);
        assert!(
            status.is_signed_out(Driver::Codex),
            "nothing has checked yet"
        );
        assert!(status.is_rechecking(Driver::Codex));
        assert!(!status.is_rechecking(Driver::Claude), "per agent");
    }

    #[test]
    fn merge_keeps_the_account_when_the_update_has_none() {
        let mut s = Snapshot::default();
        assert!(merge(&mut s, Some(acct("a")), vec![], 1));
        assert!(!merge(&mut s, None, vec![], 2));
        assert_eq!(s.account, Some(acct("a")));
        assert_eq!(s.updated_at, None, "no window yet");
        assert!(!merge(&mut s, Some(acct("a")), vec![], 3), "same account");
        assert!(merge(&mut s, Some(acct("b")), vec![], 4));
    }

    #[test]
    fn merge_matches_windows_by_group_and_label() {
        let mut s = Snapshot::default();
        assert!(merge(
            &mut s,
            None,
            vec![win(None, "5-hour", 0.1), win(None, "Weekly", 0.4)],
            10
        ));
        // A per-turn update changes one and leaves the scoped window from get_usage alone.
        assert!(merge(
            &mut s,
            None,
            vec![win(None, "Weekly (Fable)", 0.0)],
            11
        ));
        assert!(merge(&mut s, None, vec![win(None, "5-hour", 0.2)], 12));
        assert_eq!(s.windows.len(), 3);
        assert_eq!(s.windows[0].used, 0.2);
        assert_eq!(s.windows[1].used, 0.4);
        assert_eq!(s.updated_at, Some(12));
        // Identical update: no change reported, but the data is fresh.
        assert!(!merge(&mut s, None, vec![win(None, "5-hour", 0.2)], 13));
        assert_eq!(s.updated_at, Some(13));
        // Same label in another group is a different window.
        assert!(merge(
            &mut s,
            None,
            vec![win(Some("Gemini"), "5-hour", 0.9)],
            14
        ));
        assert_eq!(s.windows.len(), 4);
        assert!(s
            .windows
            .iter()
            .any(|w| w.group.as_deref() == Some("Gemini") && w.used == 0.9));
    }

    #[test]
    fn observe_routes_by_driver_and_notifies_on_change_only() {
        let st = AccountStatus::new();
        let hits = Rc::new(Cell::new(0));
        let h = hits.clone();
        st.connect_changed(move || h.set(h.get() + 1));
        let quota = |used| {
            Envelope::new(Event::QuotaUpdated {
                account: None,
                windows: vec![win(None, "Weekly", used)],
            })
        };
        st.observe(Driver::Agy, &quota(0.3));
        st.observe(Driver::Agy, &quota(0.3));
        st.observe(Driver::Claude, &quota(0.5));
        st.observe(
            Driver::Claude,
            &Envelope::new(Event::Notice { text: "x".into() }),
        );
        assert_eq!(hits.get(), 2);
        st.observe(Driver::Codex, &quota(0.7));
        assert_eq!(st.snapshot(Driver::Codex).windows[0].used, 0.7);
        assert_eq!(st.snapshot(Driver::Agy).windows[0].used, 0.3);
        assert_eq!(st.snapshot(Driver::Claude).windows[0].used, 0.5);
    }

    const NOW: i64 = 1_800_000_000;

    #[test]
    fn an_agent_is_due_by_its_clock_and_a_live_update_restarts_it() {
        use crate::freshness::DUE_SLACK;
        let st = AccountStatus::new();
        assert!(st.is_due(Driver::Claude, NOW), "never read");
        st.update_at(Driver::Claude, None, vec![win(None, "5-hour", 0.1)], NOW);
        assert!(!st.is_due(Driver::Claude, NOW + 60));
        assert!(!st.is_due(Driver::Claude, NOW + ACCOUNT_TTL - DUE_SLACK - 1));
        assert!(st.is_due(Driver::Claude, NOW + ACCOUNT_TTL));
        // A turn's QuotaUpdated at NOW + 500 is a reading: not due at NOW + TTL any more.
        st.update_at(
            Driver::Claude,
            None,
            vec![win(None, "5-hour", 0.2)],
            NOW + 500,
        );
        assert!(!st.is_due(Driver::Claude, NOW + ACCOUNT_TTL));
        // And through `observe`, on the real clock.
        st.observe(
            Driver::Agy,
            &Envelope::new(Event::QuotaUpdated {
                account: None,
                windows: vec![win(None, "Weekly", 0.3)],
            }),
        );
        assert!(!st.is_due(Driver::Agy, now_epoch() + ACCOUNT_TTL - DUE_SLACK - 5));
        // A failed attempt restarts the clock too (no retry on every focus) and is shown.
        st.mark_tried(Driver::Codex, NOW);
        st.probe_failed(Driver::Codex);
        assert!(!st.is_due(Driver::Codex, NOW + 60));
        assert!(st.snapshot(Driver::Codex).refresh_failed);
        // A future reading (the clock moved back) is due.
        assert!(st.is_due(Driver::Claude, NOW - 3600));
        // The user's "Check again" is due at once.
        st.recheck(Driver::Claude);
        assert!(st.is_due(Driver::Claude, NOW + 60));
    }

    #[test]
    fn freshness_follows_the_last_success_not_the_last_attempt() {
        use crate::freshness::{age_of, freshness, Freshness};
        let st = AccountStatus::new();
        st.update_at(Driver::Claude, None, vec![win(None, "5-hour", 0.1)], NOW);
        st.mark_tried(Driver::Claude, NOW + 3 * ACCOUNT_TTL);
        st.probe_failed(Driver::Claude);
        let snap = st.snapshot(Driver::Claude);
        assert_eq!(snap.read_at, Some(NOW));
        assert_eq!(
            freshness(age_of(snap.read_at, NOW + 3 * ACCOUNT_TTL), ACCOUNT_TTL),
            Freshness::Aging
        );
        // The next success clears the failure.
        st.update_at(Driver::Claude, None, vec![], NOW + 4 * ACCOUNT_TTL);
        assert!(!st.snapshot(Driver::Claude).refresh_failed);
    }

    /// Fake agents that leave a mark each time one is started.
    fn marking_targets(dir: &std::path::Path) -> (ProbeTargets, std::path::PathBuf) {
        let mark = dir.join("started");
        let program = crate::claude_probe::tests::fake_claude(
            dir,
            &format!("echo x >> '{}'\nexit 1", mark.display()),
        );
        let target = crate::probe::ProbeTarget {
            program,
            env: crate::agent_proc::AgentEnv::default(),
        };
        let targets = ProbeTargets::new(
            Driver::ALL
                .into_iter()
                .map(|d| (d, target.clone()))
                .collect(),
        );
        (targets, mark)
    }

    #[test]
    fn a_check_with_fresh_readings_spawns_nothing() {
        use crate::testutil::{in_loop, pump_until};
        let dir = tempfile::tempdir().unwrap();
        let (targets, mark) = marking_targets(dir.path());
        in_loop(|ctx| {
            let st = Rc::new(AccountStatus::new());
            let now = now_epoch();
            for d in Driver::ALL {
                st.update_at(d, None, vec![win(None, "5-hour", 0.1)], now);
            }
            st.refresh_if_due(&targets, now + 60);
            assert!(!st.refreshing.get(), "no probe started");
            pump_until(ctx, 1, || false);
            assert!(!mark.exists(), "focus with fresh data spawned an agent");
        });
    }

    #[test]
    fn a_signed_out_agy_is_never_probed_by_a_clock_only_by_check_again() {
        use crate::testutil::{in_loop, pump_until};
        let dir = tempfile::tempdir().unwrap();
        let (all, mark) = marking_targets(dir.path());
        let agy_only = ProbeTargets::new(vec![(
            Driver::Agy,
            all.get(Driver::Agy).cloned().expect("agy target"),
        )]);
        in_loop(|ctx| {
            let st = Rc::new(AccountStatus::new());
            st.set_signed_out(Driver::Agy, true);
            // Long overdue, yet a clock never probes it (probing opens agy's browser login).
            st.refresh_if_due(&agy_only, NOW + 100 * ACCOUNT_TTL);
            assert!(!st.refreshing.get());
            pump_until(ctx, 1, || false);
            assert!(!mark.exists(), "a signed-out agy was probed by a clock");
            // "Check again" does.
            st.recheck(Driver::Agy);
            st.refresh_if_due(&agy_only, NOW);
            assert!(st.refreshing.get());
            assert!(pump_until(ctx, 30, || !st.refreshing.get()));
            assert!(mark.exists());
            assert!(!st.is_rechecking(Driver::Agy), "answered");
        });
    }

    /// An agent is loading from the moment its probe starts (listeners hear of it) until that
    /// probe ends, failed included; an agent not probed never is.
    #[test]
    fn an_agent_is_loading_only_while_its_probe_runs() {
        use crate::testutil::{in_loop, pump_until};
        let dir = tempfile::tempdir().unwrap();
        let (all, _mark) = marking_targets(dir.path());
        let claude_only = ProbeTargets::new(vec![(
            Driver::Claude,
            all.get(Driver::Claude).cloned().expect("claude target"),
        )]);
        in_loop(|ctx| {
            let st = Rc::new(AccountStatus::new());
            let seen = Rc::new(Cell::new(false));
            let (s, w) = (seen.clone(), Rc::downgrade(&st));
            st.connect_changed(move || {
                if w.upgrade()
                    .is_some_and(|st| st.snapshot(Driver::Claude).probing)
                {
                    s.set(true);
                }
            });
            st.refresh(&claude_only);
            assert!(st.snapshot(Driver::Claude).probing);
            assert!(seen.get(), "the indicator was told");
            assert!(!st.snapshot(Driver::Codex).probing, "no target, no probe");
            assert!(pump_until(ctx, 30, || !st.refreshing.get()));
            let snap = st.snapshot(Driver::Claude);
            assert!(!snap.probing, "the failed probe ended it");
            assert!(snap.refresh_failed);
        });
    }

    /// `refresh=true` probes at most once a minute per agent, counted from the last attempt too,
    /// so a failing probe cannot be restarted by every call; and it never probes a signed-out agy.
    #[test]
    fn an_on_demand_refresh_has_a_floor_and_never_opens_agy_login() {
        use crate::testutil::{in_loop, pump_until};
        let dir = tempfile::tempdir().unwrap();
        let (all, mark) = marking_targets(dir.path());
        let claude_only = ProbeTargets::new(vec![(
            Driver::Claude,
            all.get(Driver::Claude).cloned().expect("claude target"),
        )]);
        let starts = || {
            std::fs::read_to_string(&mark)
                .map(|t| t.lines().count())
                .unwrap_or(0)
        };
        in_loop(|ctx| {
            let st = Rc::new(AccountStatus::new());
            let now = now_epoch();
            // Read 30 s ago: under the floor, nothing spawns.
            st.update_at(
                Driver::Claude,
                None,
                vec![win(None, "5-hour", 0.1)],
                now - 30,
            );
            assert!(st.refresh_if_older_than(&claude_only, 60, now).is_empty());
            pump_until(ctx, 1, || false);
            assert_eq!(starts(), 0);
            // 61 s old: one probe (the fake fails).
            assert_eq!(
                st.refresh_if_older_than(&claude_only, 60, now + 31),
                vec![Driver::Claude]
            );
            assert!(pump_until(ctx, 30, || !st.snapshot(Driver::Claude).probing));
            // One probe: it starts the fake twice (the requests and `--version`, side by side),
            // and may answer before the second has written its mark.
            pump_until(ctx, 10, || starts() >= 2);
            let probed = starts();
            assert!(probed >= 1);
            assert!(st.snapshot(Driver::Claude).refresh_failed);
            // The failed attempt restarts the floor: an immediate call spawns nothing.
            let again = now_epoch();
            assert!(st.refresh_if_older_than(&claude_only, 60, again).is_empty());
            pump_until(ctx, 1, || false);
            assert_eq!(
                starts(),
                probed,
                "a failing probe was restarted inside the floor"
            );

            // A signed-out agy, long overdue: never probed this way, never marked for recheck.
            let agy_only = ProbeTargets::new(vec![(
                Driver::Agy,
                all.get(Driver::Agy).cloned().expect("agy target"),
            )]);
            st.set_signed_out(Driver::Agy, true);
            assert!(st
                .refresh_if_older_than(&agy_only, 60, again + 100 * ACCOUNT_TTL)
                .is_empty());
            assert!(!st.is_rechecking(Driver::Agy));
            pump_until(ctx, 1, || false);
            assert_eq!(starts(), probed, "a signed-out agy was probed");
        });
    }

    /// The wait ends when the probe does, or at its timeout, and leaves no listener behind either
    /// way (nor when the future is dropped).
    #[test]
    fn waiting_for_a_refresh_removes_its_listener() {
        use crate::testutil::in_loop;
        let dir = tempfile::tempdir().unwrap();
        let (all, _mark) = marking_targets(dir.path());
        let claude_only = ProbeTargets::new(vec![(
            Driver::Claude,
            all.get(Driver::Claude).cloned().expect("claude target"),
        )]);
        in_loop(|ctx| {
            let st = Rc::new(AccountStatus::new());
            let before = st.listeners.len();
            ctx.block_on(st.refresh_and_wait(
                &claude_only,
                60,
                now_epoch(),
                std::time::Duration::from_secs(30),
            ));
            assert!(!st.snapshot(Driver::Claude).probing, "waited for the probe");
            assert_eq!(st.listeners.len(), before);
            // Nothing due: returns at once, nothing connected.
            ctx.block_on(st.refresh_and_wait(
                &claude_only,
                60,
                now_epoch(),
                std::time::Duration::from_secs(30),
            ));
            assert_eq!(st.listeners.len(), before);
            // A timeout shorter than the probe: the listener goes, the probe carries on.
            let slow_dir = tempfile::tempdir().unwrap();
            let slow = crate::probe::ProbeTarget {
                program: crate::claude_probe::tests::fake_claude(slow_dir.path(), "sleep 5"),
                env: crate::agent_proc::AgentEnv::default(),
            };
            let st2 = Rc::new(AccountStatus::new());
            ctx.block_on(st2.refresh_and_wait(
                &ProbeTargets::new(vec![(Driver::Claude, slow)]),
                60,
                now_epoch(),
                std::time::Duration::from_millis(50),
            ));
            assert_eq!(st2.listeners.len(), 0);
            assert!(st2.snapshot(Driver::Claude).probing, "left to finish");
            // The caller goes away mid-wait (its future is dropped): the listener goes too.
            let slow_dir3 = tempfile::tempdir().unwrap();
            let slow3 = crate::probe::ProbeTarget {
                program: crate::claude_probe::tests::fake_claude(slow_dir3.path(), "sleep 5"),
                env: crate::agent_proc::AgentEnv::default(),
            };
            let st3 = Rc::new(AccountStatus::new());
            // The wait's own timeout is long; the caller's is short, so its future is dropped
            // mid-wait, listener connected.
            let seen = Rc::new(Cell::new(0usize));
            {
                // Record the listener count while the wait is parked.
                let (st3, seen) = (st3.clone(), seen.clone());
                glib::spawn_future_local(async move {
                    glib::timeout_future(std::time::Duration::from_millis(20)).await;
                    seen.set(st3.listeners.len());
                });
            }
            let targets3 = ProbeTargets::new(vec![(Driver::Claude, slow3)]);
            let outcome = ctx.block_on(glib::future_with_timeout(
                std::time::Duration::from_millis(200),
                st3.refresh_and_wait(
                    &targets3,
                    60,
                    now_epoch(),
                    std::time::Duration::from_secs(30),
                ),
            ));
            assert!(outcome.is_err(), "the caller gave up first");
            assert_eq!(seen.get(), 1, "the wait was listening when dropped");
            assert_eq!(st3.listeners.len(), 0, "dropping the wait disconnected it");
            assert!(st3.snapshot(Driver::Claude).probing, "left to finish");
        });
    }

    /// An agent whose probe the clock already runs is not probed twice, and does not stop an
    /// on-demand refresh from starting the other agents that are due.
    #[test]
    fn an_on_demand_refresh_is_decided_per_agent() {
        use crate::testutil::{in_loop, pump_until};
        let slow_dir = tempfile::tempdir().unwrap();
        let slow = crate::probe::ProbeTarget {
            program: crate::claude_probe::tests::fake_claude(slow_dir.path(), "sleep 3"),
            env: crate::agent_proc::AgentEnv::default(),
        };
        let dir = tempfile::tempdir().unwrap();
        let (all, _mark) = marking_targets(dir.path());
        let both = ProbeTargets::new(vec![
            (Driver::Claude, slow),
            (
                Driver::Codex,
                all.get(Driver::Codex).cloned().expect("codex target"),
            ),
        ]);
        in_loop(|ctx| {
            let st = Rc::new(AccountStatus::new());
            let now = now_epoch();
            // The clock probes Claude only (Codex was just read).
            st.update_at(
                Driver::Codex,
                None,
                vec![win(None, "5-hour", 0.1)],
                now - 90,
            );
            st.mark_tried(Driver::Codex, now);
            st.refresh_if_due(&both, now);
            assert!(st.refreshing.get() && st.snapshot(Driver::Claude).probing);
            // On demand, 61 s later: Codex is due and starts despite the clock's refresh;
            // Claude, already being probed, is waited for, not probed again.
            let waiting = st.refresh_if_older_than(&both, 60, now + 61);
            assert!(waiting.contains(&Driver::Codex) && waiting.contains(&Driver::Claude));
            assert!(st.snapshot(Driver::Codex).probing, "started");
            assert!(pump_until(ctx, 30, || !st.snapshot(Driver::Codex).probing));
            assert!(pump_until(ctx, 30, || !st.refreshing.get()));
        });
    }

    /// Every successful reading reaches the read listeners, even one that changes nothing shown
    /// (no repaint), so the usage file's `read_at` keeps up with a live agent.
    #[test]
    fn a_reading_that_changes_nothing_still_reaches_the_read_listeners() {
        let st = AccountStatus::new();
        let (changed, read) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
        let (c, r) = (changed.clone(), read.clone());
        st.connect_changed(move || c.set(c.get() + 1));
        st.connect_read(move || r.set(r.get() + 1));
        st.update_at(Driver::Claude, None, vec![win(None, "5-hour", 0.1)], NOW);
        assert_eq!((changed.get(), read.get()), (1, 1));
        st.update_at(
            Driver::Claude,
            None,
            vec![win(None, "5-hour", 0.1)],
            NOW + 120,
        );
        assert_eq!(changed.get(), 1, "no repaint for the same data while fresh");
        assert_eq!(read.get(), 2);
        assert_eq!(st.snapshot(Driver::Claude).read_at, Some(NOW + 120));
    }

    #[test]
    fn accounts_file_lives_under_the_home() {
        assert_eq!(
            google_accounts_path(Some("/home/u".into())),
            Some(PathBuf::from("/home/u/.gemini/google_accounts.json"))
        );
        assert_eq!(google_accounts_path(Some("rel".into())), None);
        assert_eq!(google_accounts_path(None), None);
    }

    #[test]
    fn agy_account_is_read_from_the_profile_home() {
        let profile = tempfile::tempdir().expect("profile home");
        let path = profile.path().join(".gemini/google_accounts.json");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        let json = serde_json::json!({"active": "profile@example.test"});
        std::fs::write(&path, json.to_string()).expect("account");
        let env = crate::agent_proc::AgentEnv {
            env: vec![("HOME".into(), profile.path().to_string_lossy().into_owned())],
            unset: vec!["HOME".into()],
        };
        let found = agy_accounts_path(&env).expect("path");
        assert_eq!(found, path, "never read the app's account file");
        let value = serde_json::from_str::<Value>(&std::fs::read_to_string(found).expect("read"))
            .expect("json");
        let account = agy_account(&value).expect("account");
        assert_eq!(account.label, "profile@example.test");
        assert_eq!(
            agy_accounts_path(&crate::agent_proc::AgentEnv {
                env: vec![],
                unset: vec!["HOME".into()],
            }),
            None
        );
    }
}
