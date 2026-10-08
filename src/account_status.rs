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
use std::time::{SystemTime, UNIX_EPOCH};

use agent_core::adapter::Driver;
use agent_core::event::{Account, Envelope, Event, QuotaWindow};
use agent_core::quota::{agy_account, agy_signed_out, agy_usage};
use gtk4::{gio, glib};
use serde_json::Value;
use tracing::{debug, warn};

use crate::agent_proc::{run_side_full, AGY_TIMEOUT};
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

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// `~/.gemini/google_accounts.json`, from the value of `HOME`.
pub fn google_accounts_path(home: Option<&str>) -> Option<PathBuf> {
    let home = home.filter(|h| std::path::Path::new(h).is_absolute())?;
    Some(
        PathBuf::from(home)
            .join(".gemini")
            .join("google_accounts.json"),
    )
}

pub struct AccountStatus {
    claude: RefCell<Snapshot>,
    agy: RefCell<Snapshot>,
    codex: RefCell<Snapshot>,
    listeners: ListenerSet,
    refreshing: Cell<bool>,
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
            refreshing: Cell::new(false),
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

    fn notify(&self) {
        self.listeners.notify();
    }

    fn update(&self, driver: Driver, account: Option<Account>, windows: Vec<QuotaWindow>) {
        let changed = merge(
            &mut self.slot(driver).borrow_mut(),
            account,
            windows,
            now_epoch(),
        );
        if changed {
            self.notify();
        }
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
        // Missing, disabled and still-detecting agents have no target; a signed-out agy is
        // skipped unless the user asked to check it.
        let jobs: Vec<(Driver, crate::probe::ProbeTarget)> = Driver::ALL
            .into_iter()
            .filter(|d| !(*d == Driver::Agy && self.is_signed_out(*d) && !self.is_rechecking(*d)))
            .filter_map(|d| targets.get(d).map(|t| (d, t.clone())))
            .collect();
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
        for (driver, target) in jobs {
            let (me, done) = (self.clone(), finish.clone());
            match driver {
                Driver::Claude => {
                    claude_probe::probe_shared(&target.program, &target.env, move |result| {
                        if let Ok(probe) = result {
                            me.set_signed_out(driver, probe.signed_out);
                            me.update(driver, probe.account.clone(), probe.windows.clone());
                        }
                        done();
                    });
                }
                Driver::Codex => {
                    codex_probe::probe_shared(&target.program, &target.env, move |result| {
                        if let Ok(probe) = result {
                            me.set_signed_out(driver, probe.signed_out);
                            me.update(driver, probe.account.clone(), probe.windows.clone());
                        }
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
                            done();
                            return;
                        }
                        let windows = if ok {
                            serde_json::from_str::<Value>(out.trim())
                                .map(|v| agy_usage(&v))
                                .unwrap_or_default()
                        } else {
                            warn!("agy /usage failed or timed out");
                            Vec::new()
                        };
                        if ok {
                            me.set_signed_out(Driver::Agy, false);
                        }
                        // A few hundred bytes of JSON, read off the main thread like all file I/O
                        // here.
                        let account =
                            match google_accounts_path(std::env::var("HOME").ok().as_deref()) {
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

    #[test]
    fn accounts_file_lives_under_the_home() {
        assert_eq!(
            google_accounts_path(Some("/home/u")),
            Some(PathBuf::from("/home/u/.gemini/google_accounts.json"))
        );
        assert_eq!(google_accounts_path(Some("rel")), None);
        assert_eq!(google_accounts_path(None), None);
    }
}
