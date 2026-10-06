//! Who each agent is signed in as and how much of its plan is used, app-wide.
//!
//! Fed two ways: [`AccountStatus::observe`] ingests the `QuotaUpdated` envelopes a thread's
//! adapter emits (Claude: every turn; agy: after the host requests `Control::Usage` once a turn
//! completes), and [`AccountStatus::refresh`] probes both agents without a prompt so the
//! indicator is filled before any thread has run (Claude through the shared
//! [`crate::claude_probe`], agy through `-p /usage` plus its signed-in account file).
//!
//! The email address is shown in the UI only and is never logged.

// The window wires `shared()` and `refresh()` (parallel change); until then they are unused.
#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_core::adapter::Driver;
use agent_core::event::{Account, Envelope, Event, QuotaWindow};
use agent_core::quota::{agy_account, agy_usage};
use gtk4::{gio, glib};
use serde_json::Value;
use tracing::{debug, warn};

use crate::agent_proc::run_side;
use crate::claude_probe::probe_shared;

const AGY_TIMEOUT: Duration = Duration::from_secs(20);

/// One agent's account and quota windows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub account: Option<Account>,
    pub windows: Vec<QuotaWindow>,
    /// When a window was last updated (epoch seconds); `None` before the first update.
    pub updated_at: Option<i64>,
}

impl Snapshot {
    /// The window with the highest usage (what the compact indicator shows).
    pub fn most_used(&self) -> Option<&QuotaWindow> {
        self.windows.iter().max_by(|a, b| {
            a.used
                .partial_cmp(&b.used)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    }
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

type Listener = Rc<dyn Fn()>;

pub struct AccountStatus {
    claude: RefCell<Snapshot>,
    agy: RefCell<Snapshot>,
    listeners: RefCell<Vec<Listener>>,
    refreshing: Cell<bool>,
}

thread_local! {
    static SHARED: Rc<AccountStatus> = Rc::new(AccountStatus::new());
}

impl AccountStatus {
    fn new() -> Self {
        Self {
            claude: RefCell::default(),
            agy: RefCell::default(),
            listeners: RefCell::default(),
            refreshing: Cell::new(false),
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
        }
    }

    /// A copy of the agent's current snapshot.
    pub fn snapshot(&self, driver: Driver) -> Snapshot {
        self.slot(driver).borrow().clone()
    }

    /// Calls `f` (on the main thread) after every change.
    pub fn connect_changed(&self, f: impl Fn() + 'static) {
        self.listeners.borrow_mut().push(Rc::new(f));
    }

    fn notify(&self) {
        // Clone out of the cell: a listener may call back into this object.
        let listeners: Vec<Listener> = self.listeners.borrow().clone();
        for l in listeners {
            l();
        }
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

    /// Ingests an envelope from `driver`'s thread; only `QuotaUpdated` matters here.
    pub fn observe(&self, driver: Driver, envelope: &Envelope) {
        if let Event::QuotaUpdated { account, windows } = &envelope.event {
            self.update(driver, account.clone(), windows.clone());
        }
    }

    /// Probes both agents in the background and returns at once (a probe already running makes
    /// this a no-op). Neither agent is sent a prompt.
    pub fn refresh(self: &Rc<Self>, claude_program: &str, agy_program: &str) {
        if self.refreshing.replace(true) {
            debug!("account status refresh already running");
            return;
        }
        let pending = Rc::new(Cell::new(2u8));
        let finish = {
            let me = self.clone();
            move || {
                pending.set(pending.get() - 1);
                if pending.get() == 0 {
                    me.refreshing.set(false);
                }
            }
        };

        let (me, done) = (self.clone(), finish.clone());
        probe_shared(claude_program, move |result| {
            if let Ok(probe) = result {
                me.update(Driver::Claude, probe.account.clone(), probe.windows.clone());
            }
            done();
        });

        let (me, program) = (self.clone(), agy_program.to_owned());
        glib::spawn_future_local(async move {
            let usage = run_side(
                vec![
                    program,
                    "-p".to_owned(),
                    "/usage".to_owned(),
                    "--output-format".to_owned(),
                    "json".to_owned(),
                ],
                None,
            );
            let windows = match glib::future_with_timeout(AGY_TIMEOUT, usage).await {
                Ok((out, true)) => serde_json::from_str::<Value>(out.trim())
                    .map(|v| agy_usage(&v))
                    .unwrap_or_default(),
                Ok((_, false)) => {
                    warn!("agy /usage failed");
                    Vec::new()
                }
                Err(_) => {
                    warn!("agy /usage timed out");
                    Vec::new()
                }
            };
            // A few hundred bytes of JSON, read off the main thread like all file I/O here.
            let account = match google_accounts_path(std::env::var("HOME").ok().as_deref()) {
                Some(path) => gio::spawn_blocking(move || std::fs::read_to_string(path))
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                    .and_then(|v| agy_account(&v)),
                None => None,
            };
            me.update(Driver::Agy, account, windows);
            finish();
        });
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
        assert_eq!(s.most_used().unwrap().used, 0.9);
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
