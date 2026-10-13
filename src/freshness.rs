//! How old a background reading is: when to read it again, and how to show it. GTK-free and
//! clock-injected (every function takes `now`, in epoch seconds), so it is tested without a
//! display or a real clock.
//!
//! Two questions, kept apart on purpose:
//! - **Due** ([`is_due`]): may a probe run now? Answered against a TTL from the last attempt or
//!   success, so a failing agent is not re-probed on every focus.
//! - **Freshness** ([`freshness`]): may the value be shown as healthy? Answered from the last
//!   *successful* read only. Fresh under 2×TTL, Aging up to 6×TTL, Expired beyond (or never read).
//!   Only Fresh data may use healthy styling; Expired is the "could not read" state.
//!
//! Neither predicate says whether a model list was fetched in *this run*: argv decisions keep
//! using [`crate::model_catalog::ModelCatalog::is_fresh`] for that.

use agent_core::adapter::Driver;

/// Account and usage: probed again when the last reading is this old (seconds). A live
/// `QuotaUpdated` from a thread counts as a reading, so an agent in use is never probed.
pub const ACCOUNT_TTL: i64 = 10 * 60;
/// Model lists: fetched again in the background when this old.
pub const MODEL_TTL: i64 = 60 * 60;
/// Opening the model picker refreshes lists older than this (the list shows from cache at once).
/// Also the retry floor for an agent whose list is empty, so a failing agent is not re-probed on
/// every focus.
pub const MODEL_PICKER_AGE: i64 = 15 * 60;
/// A clock check that lands this close to a TTL already counts as due: the 10-minute timer would
/// otherwise just miss a reading taken a few seconds after the previous tick, and wait 20.
pub const DUE_SLACK: i64 = 60;

/// One optional time (epoch seconds) per agent: a last read or a last attempt. Persisted as is
/// in the model cache, where a missing field reads as never.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReadAt {
    #[serde(default)]
    pub claude: Option<i64>,
    #[serde(default)]
    pub agy: Option<i64>,
    #[serde(default)]
    pub codex: Option<i64>,
}

impl ReadAt {
    pub fn get(&self, driver: Driver) -> Option<i64> {
        match driver {
            Driver::Claude => self.claude,
            Driver::Agy => self.agy,
            Driver::Codex => self.codex,
        }
    }

    pub fn set(&mut self, driver: Driver, at: Option<i64>) {
        match driver {
            Driver::Claude => self.claude = at,
            Driver::Agy => self.agy = at,
            Driver::Codex => self.codex = at,
        }
    }
}

/// The later of two optional times.
pub fn latest(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// The next moment at or after `now` when a reading taken at one of `reads` changes how it may
/// be shown (Fresh to Aging at 2×`ttl`, Aging to Expired past 6×`ttl`): when to repaint.
pub fn next_boundary(reads: impl IntoIterator<Item = i64>, now: i64, ttl: i64) -> Option<i64> {
    reads
        .into_iter()
        .flat_map(|at| [at.saturating_add(2 * ttl), at.saturating_add(6 * ttl + 1)])
        .filter(|t| *t > now)
        .min()
}

/// The wall clock, in epoch seconds: what callers pass as `now` outside tests.
pub fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// How a reading may be shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Recent enough for normal (healthy) styling.
    Fresh,
    /// Shown muted, with when it was read.
    Aging,
    /// Too old (or never read): the could-not-read state.
    Expired,
}

/// The age of a reading taken at `at`, or `None` when there is none or it lies in the future
/// beyond [`DUE_SLACK`] (a clock that moved back: the timestamp vouches for nothing). A small
/// skew counts as "just now".
pub fn age_of(at: Option<i64>, now: i64) -> Option<i64> {
    let age = now.checked_sub(at?)?;
    (age >= -DUE_SLACK).then_some(age.max(0))
}

/// Fresh below 2×`ttl`, Aging up to 6×`ttl`, Expired beyond, or when `age` is `None`.
pub fn freshness(age: Option<i64>, ttl: i64) -> Freshness {
    match age {
        Some(a) if a < 2 * ttl => Freshness::Fresh,
        Some(a) if a <= 6 * ttl => Freshness::Aging,
        _ => Freshness::Expired,
    }
}

/// Whether something last read (or tried) at `last` should be read again at `now`: never read,
/// a timestamp in the future, or within [`DUE_SLACK`] of `ttl` old.
pub fn is_due(last: Option<i64>, now: i64, ttl: i64) -> bool {
    match last {
        None => true,
        Some(at) => {
            let age = now.saturating_sub(at);
            age < -DUE_SLACK || age.saturating_add(DUE_SLACK) >= ttl
        }
    }
}

/// `just now`, `38 min ago`, `3 h ago`, …
pub fn ago(age: i64) -> String {
    crate::utils::describe_age(std::time::Duration::from_secs(
        u64::try_from(age).unwrap_or(0),
    ))
}

/// The muted line for an Aging reading: `Last read 14:32 (38 min ago)`, plus why it is old when
/// the last attempt failed. `clock` is the reading's local `HH:MM`, formatted by the caller.
pub fn last_read_text(clock: &str, age: i64, failed: bool) -> String {
    let mut text = format!("Last read {clock} ({})", ago(age));
    if failed {
        text.push_str("; could not refresh");
    }
    text
}

/// What the model catalogue reports for the picker's footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CatalogStatus {
    /// A fetch is running now.
    pub refreshing: bool,
    /// The last fetch of some shown agent failed.
    pub failed: bool,
    /// The oldest successful read among the shown lists (`None`: one was never read, or was
    /// read from a cache that carried no time).
    pub read_at: Option<i64>,
}

/// The picker footer's text (its Refresh button sits beside it): `Refreshing…`, `Updated 3 h
/// ago`, `Could not refresh; showing list from 3 h ago`.
pub fn picker_footer(status: CatalogStatus, now: i64) -> String {
    let age = age_of(status.read_at, now);
    match (status.refreshing, status.failed, age) {
        (true, _, _) => "Refreshing…".to_owned(),
        (false, true, Some(a)) => format!("Could not refresh; showing list from {}", ago(a)),
        (false, true, None) => "Could not refresh".to_owned(),
        (false, false, Some(a)) => format!("Updated {}", ago(a)),
        (false, false, None) => "Not updated yet".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: i64 = 600;

    #[test]
    fn freshness_scales_from_the_ttl_and_never_is_expired() {
        assert_eq!(freshness(Some(0), T), Freshness::Fresh);
        assert_eq!(freshness(Some(2 * T - 1), T), Freshness::Fresh);
        assert_eq!(freshness(Some(2 * T), T), Freshness::Aging);
        assert_eq!(freshness(Some(6 * T), T), Freshness::Aging);
        assert_eq!(freshness(Some(6 * T + 1), T), Freshness::Expired);
        assert_eq!(freshness(None, T), Freshness::Expired);
    }

    #[test]
    fn a_future_timestamp_vouches_for_nothing_but_a_small_skew_is_now() {
        let now = 10_000;
        assert_eq!(age_of(None, now), None);
        assert_eq!(age_of(Some(now - 30), now), Some(30));
        assert_eq!(age_of(Some(now + 5), now), Some(0), "skew");
        assert_eq!(age_of(Some(now + DUE_SLACK + 1), now), None);
        assert_eq!(
            freshness(age_of(Some(now + 3600), now), T),
            Freshness::Expired
        );
    }

    #[test]
    fn due_counts_from_the_last_reading_with_slack_and_a_future_time_is_due() {
        let now = 100_000;
        assert!(is_due(None, now, T), "never read");
        assert!(!is_due(Some(now), now, T));
        assert!(!is_due(Some(now - (T - DUE_SLACK) + 1), now, T));
        // A timer tick a few seconds short of the TTL still counts.
        assert!(is_due(Some(now - (T - 3)), now, T));
        assert!(is_due(Some(now - T), now, T));
        assert!(is_due(Some(now + DUE_SLACK + 1), now, T), "future");
        assert!(!is_due(Some(now + 5), now, T), "skew is not future");
    }

    #[test]
    fn the_next_repaint_is_the_nearest_freshness_boundary_ahead() {
        let now = 100_000;
        // Read just now: it turns Aging at 2×TTL.
        assert_eq!(next_boundary([now], now, T), Some(now + 2 * T));
        // Already Aging: the next change is Expired, just past 6×TTL.
        assert_eq!(
            next_boundary([now - 3 * T], now, T),
            Some(now - 3 * T + 6 * T + 1)
        );
        // The nearest of several; long expired readings change no more.
        assert_eq!(
            next_boundary([now - 7 * T, now - T, now], now, T),
            Some(now + T)
        );
        assert_eq!(next_boundary([now - 7 * T], now, T), None);
        assert_eq!(next_boundary([], now, T), None);
        assert_eq!(latest(Some(1), Some(2)), Some(2));
        assert_eq!(latest(None, Some(2)), Some(2));
        assert_eq!(latest(None, None), None);
    }

    #[test]
    fn the_aging_line_says_when_and_whether_the_refresh_failed() {
        assert_eq!(
            last_read_text("14:32", 38 * 60, false),
            "Last read 14:32 (38 min ago)"
        );
        assert_eq!(
            last_read_text("14:32", 38 * 60, true),
            "Last read 14:32 (38 min ago); could not refresh"
        );
    }

    #[test]
    fn the_picker_footer_has_one_line_per_state() {
        let now = 50_000;
        let st = |refreshing, failed, read_at| CatalogStatus {
            refreshing,
            failed,
            read_at,
        };
        assert_eq!(picker_footer(st(true, true, None), now), "Refreshing…");
        assert_eq!(
            picker_footer(st(false, false, Some(now - 3 * 3600)), now),
            "Updated 3 h ago"
        );
        assert_eq!(
            picker_footer(st(false, true, Some(now - 3 * 3600)), now),
            "Could not refresh; showing list from 3 h ago"
        );
        assert_eq!(
            picker_footer(st(false, true, None), now),
            "Could not refresh"
        );
        assert_eq!(
            picker_footer(st(false, false, None), now),
            "Not updated yet"
        );
        assert_eq!(
            picker_footer(st(false, false, Some(now - 5)), now),
            "Updated just now"
        );
    }
}
