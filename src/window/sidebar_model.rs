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

/// Whether `row` glows: it waits for an approval and you are not looking at it (another thread
/// is shown, or the window is not focused).
pub fn needs_attention(row: &SidebarRow, shown: Option<&RowKey>, window_focused: bool) -> bool {
    row.badge == Some(Badge::NeedsApproval) && (shown != Some(&row.key) || !window_focused)
}

/// What happened to a thread that may need the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attention {
    /// An approval or a question is waiting.
    Asked,
    /// An approval ran out (agy denies after its deadline): the card is gone.
    Expired,
    /// A turn finished.
    TurnDone,
}

/// How the window reacts to an [`Attention`] event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reaction {
    /// Leave the thread unread (a sidebar mark that outlives the event).
    pub unread: bool,
    /// Raise a desktop notification.
    pub notify: bool,
}

/// The reaction to `event` on a thread that is `shown` (selected) in a window that is
/// `focused`. Looking at it means both; anything else needs a trace. A request for the user is
/// always announced when they are not looking, a finished turn only when `notify_on_bell`.
pub fn react(event: Attention, shown: bool, focused: bool, notify_on_bell: bool) -> Reaction {
    let looking = shown && focused;
    match event {
        // The glow covers a shown thread, so unread only marks what is out of view.
        Attention::Asked => Reaction {
            unread: !shown,
            notify: !looking,
        },
        Attention::Expired => Reaction {
            unread: !looking,
            notify: false,
        },
        Attention::TurnDone => Reaction {
            unread: !looking,
            notify: !looking && notify_on_bell,
        },
    }
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
    /// Archived threads are hidden unless the sidebar's "Show archived" toggle is on.
    pub archived: bool,
}

/// Rows matching `query` (case-insensitive, on title and folder), grouped by folder. Groups are
/// ordered by their newest row and rows newest first, so the folder being worked in is on top.
/// Archived rows are left out unless `show_archived` (or the thread is open in this window).
pub fn group_rows(
    rows: Vec<SidebarRow>,
    query: &str,
    show_archived: bool,
) -> Vec<(String, Vec<SidebarRow>)> {
    let query = query.trim().to_lowercase();
    let mut groups: Vec<(String, Vec<SidebarRow>)> = Vec::new();
    for row in rows.into_iter().filter(|r| {
        (show_archived || !r.archived || r.open)
            && (query.is_empty()
                || r.title.to_lowercase().contains(&query)
                || r.folder.to_lowercase().contains(&query))
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

/// The agent a handoff with no named target goes to: the next one after `from` in
/// [`Driver::ALL`] order (wrapping) that `usable` accepts. `None` when there is no other usable
/// agent.
pub fn handoff_target(from: Driver, usable: impl Fn(Driver) -> bool) -> Option<Driver> {
    let all = Driver::ALL;
    let at = all.iter().position(|d| *d == from)?;
    (1..all.len())
        .map(|step| all[(at + step) % all.len()])
        .find(|d| usable(*d))
}

pub fn driver_label(driver: Driver) -> &'static str {
    driver.info().label
}

pub fn driver_key(driver: Driver) -> &'static str {
    driver.info().key
}

// ---------------------------------------------------------------------------------------------
// The thread context menu
// ---------------------------------------------------------------------------------------------

/// What a thread-menu entry does. Encoded into one string target of the `win.thread-menu`
/// action ([`ThreadAction::encode`]), so the menu holds data and no closures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadAction {
    /// A NEW thread on `driver` (same folder), seeded with this thread's handoff.
    ContinueIn {
        thread: String,
        driver: Driver,
        model: Option<String>,
        effort: Option<String>,
    },
    /// This thread, in place, moved to `driver`/`model`/`effort` like the header's picker.
    SwitchTo {
        thread: String,
        driver: Driver,
        model: Option<String>,
        effort: Option<String>,
    },
    Rename(String),
    Archive(String),
    Unarchive(String),
    Delete(String),
    OpenFolder(String),
    CopyId(String),
}

/// Separates the fields of an encoded [`ThreadAction`] (never part of an id, model or effort).
const SEP: char = '\u{1f}';

impl ThreadAction {
    /// `kind SEP thread [SEP driver SEP model SEP effort]`; an absent model or effort is empty.
    pub fn encode(&self) -> String {
        let simple = |kind: &str, thread: &str| format!("{kind}{SEP}{thread}");
        let full = |kind: &str,
                    thread: &str,
                    driver: Driver,
                    model: &Option<String>,
                    effort: &Option<String>| {
            format!(
                "{kind}{SEP}{thread}{SEP}{}{SEP}{}{SEP}{}",
                driver_key(driver),
                model.as_deref().unwrap_or(""),
                effort.as_deref().unwrap_or("")
            )
        };
        match self {
            Self::ContinueIn {
                thread,
                driver,
                model,
                effort,
            } => full("continue", thread, *driver, model, effort),
            Self::SwitchTo {
                thread,
                driver,
                model,
                effort,
            } => full("switch", thread, *driver, model, effort),
            Self::Rename(t) => simple("rename", t),
            Self::Archive(t) => simple("archive", t),
            Self::Unarchive(t) => simple("unarchive", t),
            Self::Delete(t) => simple("delete", t),
            Self::OpenFolder(t) => simple("folder", t),
            Self::CopyId(t) => simple("copy-id", t),
        }
    }

    /// The inverse of [`Self::encode`]; `None` for anything else.
    pub fn decode(text: &str) -> Option<Self> {
        let mut parts = text.split(SEP);
        let kind = parts.next()?;
        let thread = parts.next().filter(|t| !t.is_empty())?.to_owned();
        let rest: Vec<&str> = parts.collect();
        let non_empty = |s: &&str| !s.is_empty();
        let agent = |rest: &[&str]| -> Option<(Driver, Option<String>, Option<String>)> {
            let [driver, model, effort] = rest else {
                return None;
            };
            Some((
                parse_driver(driver)?,
                Some(*model).filter(non_empty).map(str::to_owned),
                Some(*effort).filter(non_empty).map(str::to_owned),
            ))
        };
        let simple = |action: Self| rest.is_empty().then_some(action);
        match kind {
            "continue" => agent(&rest).map(|(driver, model, effort)| Self::ContinueIn {
                thread,
                driver,
                model,
                effort,
            }),
            "switch" => agent(&rest).map(|(driver, model, effort)| Self::SwitchTo {
                thread,
                driver,
                model,
                effort,
            }),
            "rename" => simple(Self::Rename(thread)),
            "archive" => simple(Self::Archive(thread)),
            "unarchive" => simple(Self::Unarchive(thread)),
            "delete" => simple(Self::Delete(thread)),
            "folder" => simple(Self::OpenFolder(thread)),
            "copy-id" => simple(Self::CopyId(thread)),
            _ => None,
        }
    }
}

/// One entry of a thread's context menu, before it becomes a `gio::Menu`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry {
    Item {
        label: String,
        action: ThreadAction,
        enabled: bool,
    },
    Submenu {
        label: String,
        enabled: bool,
        entries: Vec<MenuEntry>,
    },
    /// A group set off by a separator.
    Section(Vec<MenuEntry>),
}

/// An enabled agent and the models the catalogue lists for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuAgent {
    pub driver: Driver,
    pub models: Vec<agent_core::catalog::CatalogModel>,
}

/// Everything the thread menu depends on.
#[derive(Debug, Clone)]
pub struct ThreadMenuInput {
    pub thread: String,
    pub archived: bool,
    /// The thread has at least one message to hand over ("Continue in" needs one).
    pub has_messages: bool,
    /// Only enabled (and installed) agents appear.
    pub agents: Vec<MenuAgent>,
}

/// The agents a menu offers: every driver `usable` accepts (see `availability`), in registry
/// order, each with the catalogue models listed for it. A cached model list of an agent that is
/// not usable is kept in the catalogue but never shown.
pub fn menu_agents(
    catalog: &[agent_core::catalog::CatalogModel],
    usable: impl Fn(Driver) -> bool,
) -> Vec<MenuAgent> {
    Driver::ALL
        .into_iter()
        .filter(|d| usable(*d))
        .map(|driver| MenuAgent {
            driver,
            models: catalog
                .iter()
                .filter(|m| m.driver == driver)
                .cloned()
                .collect(),
        })
        .collect()
}

/// The model and effort to hand to `switch` for a catalogue row at `effort`: agy folds the effort
/// into the model id, the others take it separately.
fn model_choice(
    model: &agent_core::catalog::CatalogModel,
    effort: Option<&str>,
) -> (Option<String>, Option<String>) {
    let effort = effort.filter(|e| model.efforts.iter().any(|x| x == e));
    (Some(model.model_id_for(effort)), effort.map(str::to_owned))
}

/// An agent's submenu: its default model first, then every model (a model with efforts is a
/// submenu of them, the model's own default marked).
fn agent_entries(
    agent: &MenuAgent,
    enabled: bool,
    action: impl Fn(Driver, Option<String>, Option<String>) -> ThreadAction,
) -> Vec<MenuEntry> {
    let mut entries = vec![MenuEntry::Item {
        label: "Default model".to_owned(),
        action: action(agent.driver, None, None),
        enabled,
    }];
    for m in &agent.models {
        if m.efforts.is_empty() {
            let (model, effort) = model_choice(m, None);
            entries.push(MenuEntry::Item {
                label: m.display.clone(),
                action: action(agent.driver, model, effort),
                enabled,
            });
            continue;
        }
        let default = m.default_effort();
        let mut efforts = vec![MenuEntry::Item {
            label: "Default effort".to_owned(),
            action: action(agent.driver, Some(m.model_id_for(None)), None),
            enabled,
        }];
        for e in &m.efforts {
            let (model, effort) = model_choice(m, Some(e));
            efforts.push(MenuEntry::Item {
                label: if default == Some(e.as_str()) {
                    format!("{e} (default)")
                } else {
                    e.clone()
                },
                action: action(agent.driver, model, effort),
                enabled,
            });
        }
        entries.push(MenuEntry::Submenu {
            label: m.display.clone(),
            enabled,
            entries: efforts,
        });
    }
    entries
}

/// The right-click menu of a thread: continue it in another agent (a new thread), switch it in
/// place, then rename / archive / delete, then open its folder and copy its id.
pub fn thread_menu(input: &ThreadMenuInput) -> Vec<MenuEntry> {
    let by_agent =
        |label: &str,
         enabled: bool,
         action: &dyn Fn(Driver, Option<String>, Option<String>) -> ThreadAction| {
            MenuEntry::Submenu {
                label: label.to_owned(),
                enabled: enabled && !input.agents.is_empty(),
                entries: input
                    .agents
                    .iter()
                    .map(|agent| MenuEntry::Submenu {
                        label: driver_label(agent.driver).to_owned(),
                        enabled,
                        entries: agent_entries(agent, enabled, action),
                    })
                    .collect(),
            }
        };
    let thread = input.thread.clone();
    let t = thread.clone();
    let continue_in = by_agent(
        "Continue in",
        input.has_messages,
        &move |driver, model, effort| ThreadAction::ContinueIn {
            thread: t.clone(),
            driver,
            model,
            effort,
        },
    );
    let t = thread.clone();
    let switch_to = by_agent(
        "Switch this thread to",
        true,
        &move |driver, model, effort| ThreadAction::SwitchTo {
            thread: t.clone(),
            driver,
            model,
            effort,
        },
    );
    let item = |label: &str, action: ThreadAction| MenuEntry::Item {
        label: label.to_owned(),
        action,
        enabled: true,
    };
    vec![
        MenuEntry::Section(vec![continue_in, switch_to]),
        MenuEntry::Section(vec![
            item("Rename…", ThreadAction::Rename(thread.clone())),
            if input.archived {
                item("Unarchive", ThreadAction::Unarchive(thread.clone()))
            } else {
                item("Archive", ThreadAction::Archive(thread.clone()))
            },
            item("Delete…", ThreadAction::Delete(thread.clone())),
        ]),
        MenuEntry::Section(vec![
            item("Open folder", ThreadAction::OpenFolder(thread.clone())),
            item("Copy thread id", ThreadAction::CopyId(thread)),
        ]),
    ]
}

pub fn parse_driver(name: &str) -> Option<Driver> {
    Driver::from_key(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_announced_unless_you_are_looking_at_it() {
        let r = |shown, focused| react(Attention::Asked, shown, focused, false);
        // Not gated on notify_on_bell (false here).
        assert_eq!((r(false, true).notify, r(false, true).unread), (true, true));
        assert_eq!(
            (r(false, false).notify, r(false, false).unread),
            (true, true)
        );
        // Shown but the window is away: announced, no unread (the glow carries it).
        assert_eq!(
            (r(true, false).notify, r(true, false).unread),
            (true, false)
        );
        assert_eq!((r(true, true).notify, r(true, true).unread), (false, false));
    }

    #[test]
    fn an_expired_approval_leaves_a_trace_unless_you_are_looking() {
        for (shown, focused, unread) in [
            (false, true, true),
            (false, false, true),
            (true, false, true),
            (true, true, false),
        ] {
            let r = react(Attention::Expired, shown, focused, true);
            assert_eq!((r.unread, r.notify), (unread, false), "{shown} {focused}");
        }
    }

    #[test]
    fn a_finished_turn_is_unread_when_not_looked_at_and_notifies_only_by_setting() {
        // A shown thread in an unfocused window keeps an unread mark.
        assert!(react(Attention::TurnDone, true, false, false).unread);
        assert!(!react(Attention::TurnDone, true, true, true).unread);
        assert!(!react(Attention::TurnDone, true, true, true).notify);
        assert!(!react(Attention::TurnDone, false, true, false).notify);
        assert!(react(Attention::TurnDone, false, true, true).notify);
        assert!(react(Attention::TurnDone, true, false, true).notify);
    }

    fn row(id: &str, folder: &str, at: i64) -> SidebarRow {
        SidebarRow {
            key: RowKey::Thread(id.into()),
            title: format!("Thread {id}"),
            folder: folder.into(),
            updated_ms: at,
            driver: Some(Driver::Claude),
            badge: None,
            open: false,
            archived: false,
        }
    }

    #[test]
    fn a_thread_waiting_for_approval_glows_only_while_you_are_away_from_it() {
        let mut waiting = row("w", "/w", 1);
        waiting.badge = Some(Badge::NeedsApproval);
        let shown = |id: &str| RowKey::Thread(id.into());
        // Another thread shown, or this one shown in an unfocused window: it glows.
        assert!(needs_attention(&waiting, Some(&shown("other")), true));
        assert!(needs_attention(&waiting, None, true));
        assert!(needs_attention(&waiting, Some(&shown("w")), false));
        // In front of you: no glow (the approval card is right there).
        assert!(!needs_attention(&waiting, Some(&shown("w")), true));
        // Other states never glow.
        let mut running = row("r", "/w", 1);
        running.badge = Some(Badge::Running);
        assert!(!needs_attention(&running, Some(&shown("other")), false));
        assert!(!needs_attention(&row("n", "/w", 1), None, false));
    }

    #[test]
    fn archived_rows_hide_behind_the_toggle_unless_open() {
        let mut archived = row("old", "/w/one", 5);
        archived.archived = true;
        let mut open_archived = row("open", "/w/one", 6);
        open_archived.archived = true;
        open_archived.open = true;
        let rows = vec![row("a", "/w/one", 10), archived, open_archived];
        let ids = |groups: Vec<(String, Vec<SidebarRow>)>| -> Vec<String> {
            groups
                .into_iter()
                .flat_map(|(_, r)| r)
                .map(|r| match r.key {
                    RowKey::Thread(id) => id,
                    RowKey::Terminal(_) => "t".into(),
                })
                .collect()
        };
        assert_eq!(ids(group_rows(rows.clone(), "", false)), ["a", "open"]);
        assert_eq!(
            ids(group_rows(rows.clone(), "", true)),
            ["a", "open", "old"]
        );
        // The search still applies to what is shown.
        assert_eq!(ids(group_rows(rows, "thread old", true)), ["old"]);
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
            false,
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
        assert_eq!(group_rows(rows.clone(), "one", false).len(), 1);
        assert_eq!(group_rows(rows.clone(), "THREAD B", false)[0].0, "/w/two");
        assert!(group_rows(rows, "nothing", false).is_empty());
    }

    // ---- the thread menu ----

    fn model(driver: Driver, id: &str, efforts: &[&str]) -> agent_core::catalog::CatalogModel {
        agent_core::catalog::CatalogModel {
            driver,
            id: id.into(),
            display: id.to_uppercase(),
            description: None,
            efforts: efforts.iter().map(|e| (*e).to_owned()).collect(),
            default_effort: None,
            via: None,
        }
    }

    fn input(has_messages: bool, archived: bool) -> ThreadMenuInput {
        ThreadMenuInput {
            thread: "t1".into(),
            archived,
            has_messages,
            agents: vec![
                MenuAgent {
                    driver: Driver::Claude,
                    models: vec![model(Driver::Claude, "opus", &["low", "high"])],
                },
                MenuAgent {
                    driver: Driver::Agy,
                    models: vec![model(Driver::Agy, "gemini-pro", &["low", "high"])],
                },
                MenuAgent {
                    driver: Driver::Codex,
                    models: vec![model(Driver::Codex, "gpt", &[])],
                },
            ],
        }
    }

    /// Every item reachable in a submenu, depth first, as (label, action).
    fn items(entries: &[MenuEntry]) -> Vec<(String, ThreadAction, bool)> {
        entries
            .iter()
            .flat_map(|e| match e {
                MenuEntry::Item {
                    label,
                    action,
                    enabled,
                } => vec![(label.clone(), action.clone(), *enabled)],
                MenuEntry::Submenu { entries, .. } | MenuEntry::Section(entries) => items(entries),
            })
            .collect()
    }

    fn submenu<'a>(entries: &'a [MenuEntry], label: &str) -> &'a MenuEntry {
        entries
            .iter()
            .flat_map(|e| match e {
                MenuEntry::Section(s) => s.iter().collect::<Vec<_>>(),
                other => vec![other],
            })
            .find(|e| matches!(e, MenuEntry::Submenu { label: l, .. } if l == label))
            .unwrap_or_else(|| panic!("no submenu {label}"))
    }

    #[test]
    fn the_menu_offers_every_enabled_agent_with_its_models_and_efforts() {
        let menu = thread_menu(&input(true, false));
        let MenuEntry::Submenu {
            entries: agents,
            enabled,
            ..
        } = submenu(&menu, "Continue in")
        else {
            panic!("a submenu");
        };
        assert!(*enabled);
        let labels: Vec<String> = agents
            .iter()
            .map(|a| match a {
                MenuEntry::Submenu { label, .. } => label.clone(),
                _ => panic!("agent submenus only"),
            })
            .collect();
        assert_eq!(
            labels,
            [
                "Claude".to_owned(),
                "Antigravity".to_owned(),
                "Codex".to_owned(),
            ]
        );
        // Plain labels: no emoji (a PopoverMenu submenu entry draws no icon).
        assert!(labels.iter().all(|l| l.is_ascii()));
        let all = items(&[submenu(&menu, "Continue in").clone()]);
        let claude: Vec<_> = all
            .iter()
            .filter(|(_, a, _)| {
                matches!(
                    a,
                    ThreadAction::ContinueIn {
                        driver: Driver::Claude,
                        ..
                    }
                )
            })
            .collect();
        // Default model, the model's default effort and each effort.
        assert_eq!(claude.len(), 4);
        assert_eq!(claude[0].0, "Default model");
        assert!(matches!(
            &claude[0].1,
            ThreadAction::ContinueIn {
                model: None,
                effort: None,
                ..
            }
        ));
        assert!(claude.iter().any(|(l, a, _)| l == "high"
            && matches!(a, ThreadAction::ContinueIn { model: Some(m), effort: Some(e), .. }
                if m == "opus" && e == "high")));
        // agy folds the effort into the model id.
        // (its first effort is the default when `medium` is not offered, and is marked as such)
        assert!(all.iter().any(|(l, a, _)| l == "low (default)"
            && matches!(a, ThreadAction::ContinueIn { driver: Driver::Agy, model: Some(m), effort: Some(e), .. }
                if m == "gemini-pro-low" && e == "low")));
        // A model with no efforts is a plain item.
        assert!(all.iter().any(|(l, a, _)| l == "GPT"
            && matches!(a, ThreadAction::ContinueIn { driver: Driver::Codex, model: Some(m), effort: None, .. }
                if m == "gpt")));
        // Switch uses the same agents with its own action.
        let switch = items(&[submenu(&menu, "Switch this thread to").clone()]);
        assert_eq!(switch.len(), all.len());
        assert!(switch
            .iter()
            .all(|(_, a, on)| *on && matches!(a, ThreadAction::SwitchTo { .. })));
    }

    #[test]
    fn only_usable_agents_reach_the_menu_even_when_the_catalogue_caches_their_models() {
        let catalog = vec![
            model(Driver::Claude, "opus", &[]),
            model(Driver::Agy, "gemini", &[]),
            model(Driver::Codex, "gpt", &[]),
        ];
        let agents = menu_agents(&catalog, |d| d != Driver::Agy);
        assert_eq!(
            agents.iter().map(|a| a.driver).collect::<Vec<_>>(),
            [Driver::Claude, Driver::Codex]
        );
        assert!(agents.iter().all(|a| a.models.len() == 1));
        assert!(menu_agents(&catalog, |_| false).is_empty());
    }

    #[test]
    fn continue_is_disabled_without_messages_and_empty_without_agents() {
        let menu = thread_menu(&input(false, false));
        let cont = items(&[submenu(&menu, "Continue in").clone()]);
        assert!(!cont.is_empty() && cont.iter().all(|(_, _, on)| !on));
        // Switching in place does not need a history.
        let switch = items(&[submenu(&menu, "Switch this thread to").clone()]);
        assert!(switch.iter().all(|(_, _, on)| *on));

        let mut none = input(true, false);
        none.agents.clear();
        let menu = thread_menu(&none);
        for label in ["Continue in", "Switch this thread to"] {
            assert!(matches!(
                submenu(&menu, label),
                MenuEntry::Submenu { enabled: false, entries, .. } if entries.is_empty()
            ));
        }
    }

    #[test]
    fn archive_toggles_and_the_housekeeping_entries_are_always_there() {
        let labels = |archived| -> Vec<String> {
            let menu = thread_menu(&input(true, archived));
            menu.iter()
                .filter_map(|e| match e {
                    MenuEntry::Section(s) => Some(s),
                    _ => None,
                })
                .flat_map(|s| s.iter())
                .filter_map(|e| match e {
                    MenuEntry::Item { label, .. } => Some(label.clone()),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            labels(false),
            [
                "Rename…",
                "Archive",
                "Delete…",
                "Open folder",
                "Copy thread id"
            ]
        );
        assert_eq!(
            labels(true),
            [
                "Rename…",
                "Unarchive",
                "Delete…",
                "Open folder",
                "Copy thread id"
            ]
        );
    }

    #[test]
    fn actions_round_trip_through_their_target_string() {
        let actions = [
            ThreadAction::ContinueIn {
                thread: "t".into(),
                driver: Driver::Codex,
                model: Some("gpt-5-codex".into()),
                effort: Some("high".into()),
            },
            ThreadAction::SwitchTo {
                thread: "t".into(),
                driver: Driver::Agy,
                model: None,
                effort: None,
            },
            ThreadAction::Rename("t".into()),
            ThreadAction::Archive("t".into()),
            ThreadAction::Unarchive("t".into()),
            ThreadAction::Delete("t".into()),
            ThreadAction::OpenFolder("t".into()),
            ThreadAction::CopyId("t".into()),
        ];
        for a in actions {
            assert_eq!(ThreadAction::decode(&a.encode()), Some(a.clone()), "{a:?}");
        }
        // Garbage and malformed targets decode to nothing, never to a wrong action.
        for bad in [
            "",
            "delete",
            "delete\u{1f}",
            "bogus\u{1f}t",
            "delete\u{1f}t\u{1f}extra",
            "continue\u{1f}t\u{1f}nope\u{1f}\u{1f}",
            "continue\u{1f}t\u{1f}claude",
        ] {
            assert_eq!(ThreadAction::decode(bad), None, "{bad:?}");
        }
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
        for d in Driver::ALL {
            assert_eq!(parse_driver(driver_key(d)), Some(d));
            // Exhaustive: a new driver fails to compile until it is in Driver::ALL.
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
