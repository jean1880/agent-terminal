//! `$XDG_CACHE_HOME/agent-terminal/usage.json`: each agent's plan-quota windows and the headroom
//! they leave, for tools outside the app (contract v1). Rewritten when [`AccountStatus`] changes,
//! off the main thread, atomically (`0600` temp file, `sync_all`, rename).
//!
//! Never the account: the file is built from the windows and timestamps alone, so no email or
//! plan label can reach it.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use agent_core::adapter::Driver;
use agent_core::model_tags::{headroom, load, Headroom};
use gtk4::{gio, glib};
use serde::Serialize;
use tracing::warn;

use crate::account_status::{AccountStatus, Snapshot};
use crate::freshness::{age_of, now_epoch, ACCOUNT_TTL};

/// A reading older than this says nothing about headroom (contract v1: 2 × `ACCOUNT_TTL`).
pub const HEADROOM_MAX_AGE: i64 = 2 * ACCOUNT_TTL;
/// The file's format version.
pub const VERSION: u32 = 1;
/// Changes are written at most this often (probes report in bursts).
const COALESCE: std::time::Duration = std::time::Duration::from_secs(1);

/// The headroom `snap` shows at `now`.
pub fn headroom_of(snap: &Snapshot, now: i64) -> Headroom {
    headroom(&snap.windows, age_of(snap.read_at, now), HEADROOM_MAX_AGE)
}

/// The load `snap` shows at `now` (`None` when its headroom is unknown).
pub fn load_of(snap: &Snapshot, now: i64) -> Option<f64> {
    load(&snap.windows, age_of(snap.read_at, now), HEADROOM_MAX_AGE)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WindowEntry {
    pub group: Option<String>,
    pub label: String,
    pub used: f64,
    pub resets_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentEntry {
    pub headroom: Headroom,
    /// The fullest window's share used (0.0–1.0); null exactly when `headroom` is `unknown`.
    pub load: Option<f64>,
    /// `None` (null): never read successfully in this run.
    pub read_at: Option<i64>,
    pub refresh_failed: bool,
    pub signed_out: bool,
    pub windows: Vec<WindowEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UsageDocument {
    pub version: u32,
    pub written_at: i64,
    /// Keyed by driver key (`claude`, `agy`, `codex`). An agent the app knows nothing about yet
    /// (never read, not signed out, no failed probe) is left out.
    pub agents: std::collections::BTreeMap<String, AgentEntry>,
}

/// The file's contents for `snapshots` at `now`. Only windows and timestamps are copied.
pub fn document(snapshots: &[(Driver, Snapshot)], now: i64) -> UsageDocument {
    let agents = snapshots
        .iter()
        .filter(|(_, s)| s.read_at.is_some() || s.signed_out || s.refresh_failed)
        .map(|(driver, s)| {
            (
                key(*driver).to_owned(),
                AgentEntry {
                    headroom: headroom_of(s, now),
                    load: load_of(s, now),
                    read_at: s.read_at,
                    refresh_failed: s.refresh_failed,
                    signed_out: s.signed_out,
                    windows: s
                        .windows
                        .iter()
                        .map(|w| WindowEntry {
                            group: w.group.clone(),
                            label: w.label.clone(),
                            used: if w.used.is_finite() { w.used } else { 0.0 },
                            resets_at: w.resets_at.clone(),
                        })
                        .collect(),
                },
            )
        })
        .collect();
    UsageDocument {
        version: VERSION,
        written_at: now,
        agents,
    }
}

fn key(driver: Driver) -> &'static str {
    driver.info().key
}

/// `<cache dir>/agent-terminal/usage.json`.
pub fn usage_path(xdg_cache_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    crate::model_catalog::cache_file(xdg_cache_home, home, "usage.json")
}

thread_local! {
    static INSTALLED: Cell<bool> = const { Cell::new(false) };
}

/// Keeps the file in step with [`AccountStatus::shared`] for the rest of the process: once per
/// process, however many windows call it.
pub fn install() {
    if INSTALLED.with(|i| i.replace(true)) {
        return;
    }
    let Some(path) = usage_path(
        std::env::var("XDG_CACHE_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    ) else {
        warn!("No cache directory: usage.json is not written");
        return;
    };
    let writer = Writer::new(
        path,
        Rc::new(|| {
            let status = AccountStatus::shared();
            Driver::ALL
                .into_iter()
                .map(|d| (d, status.snapshot(d)))
                .collect()
        }),
    );
    // The status is app-wide and lives as long as the process: so do these listeners. A change
    // of what is shown, and every successful reading (which moves `read_at` without one).
    let status = AccountStatus::shared();
    let w = writer.clone();
    status.connect_changed(move || w.schedule());
    let w = writer.clone();
    status.connect_read(move || w.schedule());
    writer.schedule();
}

/// The agents last written.
type Written = std::collections::BTreeMap<String, AgentEntry>;

/// Whether `new` says nothing `last` did not: the same agents and entries, except for `read_at`
/// moving by less than [`READ_AT_STEP`] (a live agent re-read every turn need not rewrite the
/// file each time, but external readers must not see it go stale).
pub(crate) fn same_as_written(last: &Written, new: &Written) -> bool {
    last.len() == new.len()
        && last.iter().zip(new).all(|((ka, a), (kb, b))| {
            let moved = match (a.read_at, b.read_at) {
                (Some(x), Some(y)) => (y - x).abs() >= READ_AT_STEP,
                (x, y) => x != y,
            };
            ka == kb
                && !moved
                && AgentEntry {
                    read_at: b.read_at,
                    ..a.clone()
                } == *b
        })
}

/// A `read_at` that moved this much (seconds) is written even when nothing else changed.
const READ_AT_STEP: i64 = 60;

/// Writes the file, one write at a time: a change while a write runs is written once it ends,
/// so an older write can never land over a newer one. A failed write is not remembered as
/// written, so the next change or reading retries it.
pub(crate) struct Writer {
    path: PathBuf,
    snapshots: Rc<dyn Fn() -> Vec<(Driver, Snapshot)>>,
    scheduled: Cell<bool>,
    in_flight: Cell<bool>,
    /// Asked to write while a write ran.
    dirty: Cell<bool>,
    last: RefCell<Option<Written>>,
}

impl Writer {
    pub(crate) fn new(
        path: PathBuf,
        snapshots: Rc<dyn Fn() -> Vec<(Driver, Snapshot)>>,
    ) -> Rc<Self> {
        Rc::new(Self {
            path,
            snapshots,
            scheduled: Cell::new(false),
            in_flight: Cell::new(false),
            dirty: Cell::new(false),
            last: RefCell::new(None),
        })
    }

    /// Writes within [`COALESCE`] (probes report in bursts).
    pub(crate) fn schedule(self: &Rc<Self>) {
        if self.scheduled.replace(true) {
            return;
        }
        let me = self.clone();
        glib::spawn_future_local(async move {
            glib::timeout_future(COALESCE).await;
            me.scheduled.set(false);
            me.flush().await;
        });
    }

    /// Writes the current state now unless it was written already; while a write runs, marks it
    /// to run again once that one ends.
    pub(crate) async fn flush(self: &Rc<Self>) {
        if self.in_flight.replace(true) {
            self.dirty.set(true);
            return;
        }
        loop {
            self.dirty.set(false);
            self.write_once().await;
            if !self.dirty.get() {
                break;
            }
        }
        self.in_flight.set(false);
    }

    async fn write_once(&self) {
        let doc = document(&(self.snapshots)(), now_epoch());
        if self
            .last
            .borrow()
            .as_ref()
            .is_some_and(|last| same_as_written(last, &doc.agents))
        {
            return;
        }
        let bytes = match serde_json::to_vec_pretty(&doc) {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "could not encode usage.json");
                return;
            }
        };
        let path = self.path.clone();
        let written =
            gio::spawn_blocking(move || agent_kit::fsutil::write_private_atomic(&path, &bytes))
                .await
                .unwrap_or_else(|_| Err(std::io::Error::other("the write did not finish")));
        match written {
            Ok(()) => *self.last.borrow_mut() = Some(doc.agents),
            Err(e) => warn!(error = %e, "could not write usage.json"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::event::{Account, QuotaWindow};

    const NOW: i64 = 1_792_540_800;

    fn snap(used: f64, read_at: Option<i64>) -> Snapshot {
        Snapshot {
            account: Some(Account {
                label: "person@example.test".into(),
                plan: Some("Max plan".into()),
                provider: Some("provider-secret".into()),
            }),
            windows: vec![QuotaWindow {
                group: None,
                label: "5-hour".into(),
                used,
                resets_at: Some("2026-10-20T22:00:00Z".into()),
            }],
            updated_at: read_at,
            signed_out: false,
            read_at,
            refresh_failed: false,
            probing: true,
        }
    }

    #[test]
    fn the_file_follows_contract_v1() {
        let doc = document(
            &[
                (Driver::Claude, snap(0.42, Some(NOW - 100))),
                (Driver::Codex, snap(0.85, Some(NOW - 10))),
                (Driver::Agy, Snapshot::default()),
            ],
            NOW,
        );
        let v = serde_json::to_value(&doc).expect("ser");
        assert_eq!(
            v,
            serde_json::json!({
                "version": 1,
                "written_at": NOW,
                "agents": {
                    "claude": {"headroom": "ok", "load": 0.42, "read_at": NOW - 100, "refresh_failed": false,
                               "signed_out": false,
                               "windows": [{"group": null, "label": "5-hour", "used": 0.42,
                                            "resets_at": "2026-10-20T22:00:00Z"}]},
                    "codex": {"headroom": "low", "load": 0.85, "read_at": NOW - 10, "refresh_failed": false,
                              "signed_out": false,
                              "windows": [{"group": null, "label": "5-hour", "used": 0.85,
                                           "resets_at": "2026-10-20T22:00:00Z"}]}
                }
            }),
            "an agent never read is left out"
        );
    }

    #[test]
    fn no_account_field_is_ever_serialized() {
        let mut signed_out = Snapshot {
            signed_out: true,
            ..Snapshot::default()
        };
        signed_out.refresh_failed = true;
        let doc = document(
            &[
                (Driver::Claude, snap(0.1, Some(NOW))),
                (Driver::Agy, signed_out),
                (Driver::Codex, snap(f64::NAN, Some(NOW))),
            ],
            NOW,
        );
        let text = serde_json::to_string(&doc).expect("ser");
        for leaked in [
            "account",
            "person@example.test",
            "Max plan",
            "provider",
            "probing",
        ] {
            assert!(!text.contains(leaked), "{leaked} in {text}");
        }
        assert_eq!(doc.agents["agy"].headroom, Headroom::Unknown);
        assert_eq!(doc.agents["agy"].read_at, None);
        assert_eq!(doc.agents["agy"].load, None);
        assert!(text.contains(r#""load":null"#), "null, not absent: {text}");
        assert_eq!(
            doc.agents["codex"].windows[0].used, 0.0,
            "NaN never reaches JSON"
        );
    }

    #[test]
    fn an_old_reading_has_unknown_headroom() {
        let s = snap(0.99, Some(NOW - HEADROOM_MAX_AGE));
        assert_eq!(headroom_of(&s, NOW), Headroom::Exhausted);
        assert_eq!(headroom_of(&s, NOW + 1), Headroom::Unknown);
        assert_eq!(headroom_of(&snap(0.99, None), NOW), Headroom::Unknown);
    }

    #[test]
    fn a_read_at_that_moved_a_minute_is_news() {
        let now = now_epoch();
        let doc = |read_at: i64| document(&[(Driver::Claude, snap(0.4, Some(read_at)))], now);
        let last = doc(now - 100).agents;
        assert!(same_as_written(&last, &doc(now - 100).agents));
        assert!(same_as_written(&last, &doc(now - 41).agents), "59 s");
        assert!(!same_as_written(&last, &doc(now - 40).agents), "60 s");
        let other = document(&[(Driver::Claude, snap(0.5, Some(now - 100)))], now).agents;
        assert!(!same_as_written(&last, &other), "a window changed");
        let more = document(
            &[
                (Driver::Claude, snap(0.4, Some(now - 100))),
                (Driver::Codex, snap(0.1, Some(now))),
            ],
            now,
        )
        .agents;
        assert!(!same_as_written(&last, &more), "another agent");
    }

    type Source = Rc<RefCell<Vec<(Driver, Snapshot)>>>;

    fn writer_at(path: PathBuf) -> (Rc<Writer>, Source) {
        let source: Source = Rc::new(RefCell::new(vec![(
            Driver::Claude,
            snap(0.1, Some(now_epoch())),
        )]));
        let s = source.clone();
        (
            Writer::new(path, Rc::new(move || s.borrow().clone())),
            source,
        )
    }

    fn written_used(path: &std::path::Path) -> f64 {
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("json");
        v["agents"]["claude"]["windows"][0]["used"]
            .as_f64()
            .expect("used")
    }

    /// One write at a time, and a change during a write is written after it: the file always
    /// ends on the newest state.
    #[test]
    fn writes_are_serialized_and_the_last_state_wins() {
        use crate::testutil::{in_loop, pump_until};
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("usage.json");
        let (writer, source) = writer_at(path.clone());
        in_loop(|ctx| {
            let done = Rc::new(Cell::new(0));
            let (w, d) = (writer.clone(), done.clone());
            glib::spawn_future_local(async move {
                w.flush().await;
                d.set(d.get() + 1);
            });
            // The first write is now in flight (its blocking job queued): change, flush again.
            assert!(pump_until(ctx, 5, || writer.in_flight.get()));
            source.borrow_mut()[0].1 = snap(0.9, Some(now_epoch()));
            let (w, d) = (writer.clone(), done.clone());
            glib::spawn_future_local(async move {
                w.flush().await;
                d.set(d.get() + 1);
            });
            assert!(pump_until(ctx, 10, || done.get() == 2 && !writer.in_flight.get()));
            assert_eq!(written_used(&path), 0.9);
            // Nothing new: not written again.
            std::fs::remove_file(&path).expect("rm");
            ctx.block_on(writer.flush());
            assert!(!path.exists());
            // While a write is in flight, a flush only marks it to run again: it never starts a
            // second, concurrent write.
            writer.in_flight.set(true);
            source.borrow_mut()[0].1 = snap(0.5, Some(now_epoch()));
            ctx.block_on(writer.flush());
            assert!(!path.exists(), "no concurrent write");
            assert!(writer.dirty.get());
        });
    }

    /// A failed write is not taken as written: the next flush retries it.
    #[test]
    fn a_failed_write_is_retried() {
        use crate::testutil::in_loop;
        let dir = tempfile::tempdir().expect("tmp");
        // The parent is a file: the write fails.
        let blocker = dir.path().join("cache");
        std::fs::write(&blocker, "").expect("file");
        let path = blocker.join("usage.json");
        let (writer, _source) = writer_at(path.clone());
        in_loop(|ctx| {
            ctx.block_on(writer.flush());
            assert!(!path.exists());
            assert!(writer.last.borrow().is_none(), "not remembered as written");
            std::fs::remove_file(&blocker).expect("rm");
            ctx.block_on(writer.flush());
            assert_eq!(
                written_used(&path),
                0.1,
                "the same state, written this time"
            );
        });
    }

    #[test]
    fn the_file_lives_in_the_cache_directory() {
        assert_eq!(
            usage_path(Some("/xdg"), Some("/home/u")),
            Some(PathBuf::from("/xdg/agent-terminal/usage.json"))
        );
        assert_eq!(
            usage_path(None, Some("/home/u")),
            Some(PathBuf::from("/home/u/.cache/agent-terminal/usage.json"))
        );
        assert_eq!(usage_path(Some("rel"), None), None);
    }
}
