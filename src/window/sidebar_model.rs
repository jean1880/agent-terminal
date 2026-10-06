//! The thread sidebar's pure logic: which rows show, grouped and ordered how, with which badge
//! and relative time. No GTK, so it is tested without a display.

use agent_core::adapter::Driver;

/// What a sidebar row opens.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RowKey {
    /// A chat thread in the store.
    Thread(String),
    /// A 2.x terminal page, by its tab key.
    Terminal(u64),
}

/// A row's state badge, in priority order: only the most urgent one shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Badge {
    NeedsApproval,
    RateLimited,
    Running,
    Unread,
}

impl Badge {
    pub fn css_class(self) -> &'static str {
        match self {
            Badge::NeedsApproval => "badge-approval",
            Badge::RateLimited => "badge-limited",
            Badge::Running => "badge-running",
            Badge::Unread => "badge-unread",
        }
    }

    pub fn tooltip(self) -> &'static str {
        match self {
            Badge::NeedsApproval => "Needs your approval",
            Badge::RateLimited => "Rate limited",
            Badge::Running => "Working",
            Badge::Unread => "New activity",
        }
    }
}

/// The badge for a thread's state, or none.
pub fn badge_for(running: bool, approval: bool, rate_limited: bool, unread: bool) -> Option<Badge> {
    [
        (approval, Badge::NeedsApproval),
        (rate_limited, Badge::RateLimited),
        (running, Badge::Running),
        (unread, Badge::Unread),
    ]
    .into_iter()
    .find_map(|(on, badge)| on.then_some(badge))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarRow {
    pub key: RowKey,
    pub title: String,
    /// The working directory; rows are grouped by it.
    pub folder: String,
    /// Last activity, milliseconds since the epoch.
    pub updated_ms: i64,
    /// `None`: a terminal page.
    pub driver: Option<Driver>,
    pub badge: Option<Badge>,
    /// Open as a page in this window.
    pub open: bool,
}

/// Rows matching `query` (case-insensitive, on title and folder), grouped by folder. Groups are
/// ordered by their newest row and rows newest first, so the folder being worked in is on top.
pub fn group_rows(rows: Vec<SidebarRow>, query: &str) -> Vec<(String, Vec<SidebarRow>)> {
    let query = query.trim().to_lowercase();
    let mut groups: Vec<(String, Vec<SidebarRow>)> = Vec::new();
    for row in rows.into_iter().filter(|r| {
        query.is_empty()
            || r.title.to_lowercase().contains(&query)
            || r.folder.to_lowercase().contains(&query)
    }) {
        match groups.iter_mut().find(|(f, _)| *f == row.folder) {
            Some((_, list)) => list.push(row),
            None => groups.push((row.folder.clone(), vec![row])),
        }
    }
    for (_, list) in &mut groups {
        list.sort_by_key(|r| std::cmp::Reverse(r.updated_ms));
    }
    groups.sort_by(|a, b| {
        let newest = |g: &[SidebarRow]| g.first().map_or(i64::MIN, |r| r.updated_ms);
        newest(&b.1).cmp(&newest(&a.1))
    });
    groups
}

/// A compact age: `now`, `5m`, `3h`, `2d`, `6w`.
pub fn relative_time(now_ms: i64, then_ms: i64) -> String {
    let secs = (now_ms - then_ms).max(0) / 1000;
    match secs {
        s if s < 60 => "now".to_owned(),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s if s < 14 * 86_400 => format!("{}d", s / 86_400),
        s => format!("{}w", s / (7 * 86_400)),
    }
}

/// A thread title from its first message: the first non-blank line, at most 60 characters.
pub fn thread_title(text: &str) -> String {
    const MAX: usize = 60;
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    if line.chars().count() <= MAX {
        return line.to_owned();
    }
    let cut: String = line.chars().take(MAX - 1).collect();
    format!("{}…", cut.trim_end())
}

/// What a resumed native session becomes. A Claude or agy session is resumed as a chat
/// thread whose provider thread carries the native id; any other CLI's session can only be
/// resumed in a terminal page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeAs {
    Chat { driver: Driver, native_id: String },
    Terminal,
}

pub fn resume_as(driver: Option<Driver>, session_id: &str) -> ResumeAs {
    match driver {
        Some(driver) => ResumeAs::Chat {
            driver,
            native_id: session_id.to_owned(),
        },
        None => ResumeAs::Terminal,
    }
}

/// The store's provider-thread model, where `"default"` means "the CLI's own default".
pub fn stored_model(model: Option<&str>) -> Option<String> {
    model
        .filter(|m| !m.is_empty() && *m != "default")
        .map(str::to_owned)
}

/// Every chat agent, in the order handoffs and menus offer them.
pub const DRIVERS: [Driver; 3] = [Driver::Claude, Driver::Agy, Driver::Codex];

/// The agent a handoff with no named target goes to: the next one after `from` in
/// [`DRIVERS`] order (wrapping) that `usable` accepts. `None` when there is no other usable agent.
pub fn handoff_target(from: Driver, usable: impl Fn(Driver) -> bool) -> Option<Driver> {
    let at = DRIVERS.iter().position(|d| *d == from)?;
    (1..DRIVERS.len())
        .map(|step| DRIVERS[(at + step) % DRIVERS.len()])
        .find(|d| usable(*d))
}

pub fn driver_label(driver: Driver) -> &'static str {
    match driver {
        Driver::Claude => "Claude",
        Driver::Agy => "Antigravity",
        Driver::Codex => "Codex",
    }
}

pub fn driver_key(driver: Driver) -> &'static str {
    match driver {
        Driver::Claude => "claude",
        Driver::Agy => "agy",
        Driver::Codex => "codex",
    }
}

pub fn parse_driver(name: &str) -> Option<Driver> {
    match name {
        "claude" => Some(Driver::Claude),
        "agy" => Some(Driver::Agy),
        "codex" => Some(Driver::Codex),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, folder: &str, at: i64) -> SidebarRow {
        SidebarRow {
            key: RowKey::Thread(id.into()),
            title: format!("Thread {id}"),
            folder: folder.into(),
            updated_ms: at,
            driver: Some(Driver::Claude),
            badge: None,
            open: false,
        }
    }

    #[test]
    fn rows_group_by_folder_newest_group_and_row_first() {
        let groups = group_rows(
            vec![
                row("a", "/w/one", 10),
                row("b", "/w/two", 30),
                row("c", "/w/one", 40),
                row("d", "/w/two", 20),
            ],
            "",
        );
        let shape: Vec<(&str, Vec<&str>)> = groups
            .iter()
            .map(|(f, rows)| {
                (
                    f.as_str(),
                    rows.iter()
                        .map(|r| match &r.key {
                            RowKey::Thread(id) => id.as_str(),
                            RowKey::Terminal(_) => "t",
                        })
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            [("/w/one", vec!["c", "a"]), ("/w/two", vec!["b", "d"])]
        );
    }

    #[test]
    fn search_matches_title_or_folder_case_insensitively() {
        let rows = vec![row("a", "/w/One", 1), row("b", "/w/two", 2)];
        assert_eq!(group_rows(rows.clone(), "one").len(), 1);
        assert_eq!(group_rows(rows.clone(), "THREAD B")[0].0, "/w/two");
        assert!(group_rows(rows, "nothing").is_empty());
    }

    #[test]
    fn the_most_urgent_badge_wins() {
        assert_eq!(
            badge_for(true, true, true, true),
            Some(Badge::NeedsApproval)
        );
        assert_eq!(badge_for(true, false, true, true), Some(Badge::RateLimited));
        assert_eq!(badge_for(true, false, false, true), Some(Badge::Running));
        assert_eq!(badge_for(false, false, false, true), Some(Badge::Unread));
        assert_eq!(badge_for(false, false, false, false), None);
    }

    #[test]
    fn relative_times_are_compact() {
        let now = 1_000_000_000;
        assert_eq!(relative_time(now, now + 5), "now");
        assert_eq!(relative_time(now, now - 5 * 60_000), "5m");
        assert_eq!(relative_time(now, now - 3 * 3_600_000), "3h");
        assert_eq!(relative_time(now, now - 2 * 86_400_000), "2d");
        assert_eq!(relative_time(now, now - 21 * 86_400_000), "3w");
    }

    #[test]
    fn titles_come_from_the_first_line_and_are_bounded() {
        assert_eq!(thread_title("\n  fix the build \nmore"), "fix the build");
        let long = "x".repeat(80);
        let t = thread_title(&long);
        assert_eq!(t.chars().count(), 60);
        assert!(t.ends_with('…'));
        assert_eq!(thread_title(""), "");
    }

    #[test]
    fn resume_maps_chat_agents_to_threads_and_others_to_terminals() {
        assert_eq!(
            resume_as(Some(Driver::Agy), "abc"),
            ResumeAs::Chat {
                driver: Driver::Agy,
                native_id: "abc".into()
            }
        );
        assert_eq!(resume_as(None, "abc"), ResumeAs::Terminal);
        assert_eq!(stored_model(Some("default")), None);
        assert_eq!(stored_model(Some("opus")).as_deref(), Some("opus"));
        for d in DRIVERS {
            assert_eq!(parse_driver(driver_key(d)), Some(d));
            // Exhaustive: a new driver fails to compile until it is in DRIVERS.
            match d {
                Driver::Claude | Driver::Agy | Driver::Codex => {}
            }
            assert_ne!(handoff_target(d, |_| true), Some(d));
        }
        assert_eq!(driver_label(Driver::Codex), "Codex");
    }

    #[test]
    fn the_default_handoff_target_cycles_to_the_next_usable_agent() {
        use Driver::{Agy, Claude, Codex};
        let all = |_| true;
        assert_eq!(handoff_target(Claude, all), Some(Agy));
        assert_eq!(handoff_target(Agy, all), Some(Codex));
        assert_eq!(handoff_target(Codex, all), Some(Claude));
        // Skips an agent that is off or not installed, and wraps.
        assert_eq!(handoff_target(Claude, |d| d == Codex), Some(Codex));
        assert_eq!(handoff_target(Codex, |d| d == Agy), Some(Agy));
        // Never itself, and nothing when it is the only one.
        assert_eq!(handoff_target(Claude, |d| d == Claude), None);
        assert_eq!(handoff_target(Agy, |_| false), None);
    }
}
