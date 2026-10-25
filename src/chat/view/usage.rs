//! The usage indicator: per agent, an accent dot and two thin bars with their percentages, the
//! non-weekly window and the weekly one, each the most used of its kind (amber from 75 %, red
//! from 90 %); a popover lists the account and every window.
//!
//! It reads [`AccountStatus`] and repaints whenever that changes. One widget serves both the
//! chat header (compact meter filtered to the current agent) and the sidebar footer. Both
//! detail popovers show every available agent from the same shared snapshots.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use agent_core::adapter::Driver;
use agent_core::event::QuotaWindow;
use agent_core::quota::{parse_rfc3339, resets_in_text};
use gtk4::prelude::*;

use super::cards::{accent_class, brand_image, label, wrapped_text};
use crate::account_status::{AccountStatus, Snapshot};
use crate::availability::AgentAvailability;
use crate::freshness::{
    age_of, ago, freshness, last_read_text, next_boundary, now_epoch, Freshness, ACCOUNT_TTL,
};

const WARN_AT: f64 = 0.75;
const CRITICAL_AT: f64 = 0.90;

/// How alarming a used share is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Normal,
    Warn,
    Critical,
}

pub fn severity(used: f64) -> Severity {
    if used >= CRITICAL_AT {
        Severity::Critical
    } else if used >= WARN_AT {
        Severity::Warn
    } else {
        Severity::Normal
    }
}

impl Severity {
    pub fn css_class(self) -> &'static str {
        match self {
            Severity::Normal => "usage-ok",
            Severity::Warn => "usage-warn",
            Severity::Critical => "usage-crit",
        }
    }
}

/// `43 %`, rounded, never above 100.
pub fn percent_text(used: f64) -> String {
    format!("{} %", (used.clamp(0.0, 1.0) * 100.0).round() as u32)
}

/// `Gemini Models · Weekly` or just `Weekly`.
pub fn window_title(w: &QuotaWindow) -> String {
    match &w.group {
        Some(g) => format!("{g} · {}", w.label),
        None => w.label.clone(),
    }
}

/// `resets in 3 h 47 min`, when the window has a parseable reset time.
pub fn resets_text(w: &QuotaWindow, now: i64) -> Option<String> {
    let at = parse_rfc3339(w.resets_at.as_deref()?)?;
    Some(resets_in_text(at - now))
}

/// The windows the compact meter shows: the most-used non-weekly window and the
/// most-used weekly one (a per-model week such as "Weekly (Fable)" counts as weekly). Labels come
/// from the adapters, which name every weekly window "Weekly…".
pub fn split_windows(windows: &[QuotaWindow]) -> (Option<&QuotaWindow>, Option<&QuotaWindow>) {
    let most = |weekly: bool| {
        windows
            .iter()
            .filter(|w| w.label.starts_with("Weekly") == weekly)
            .max_by(|a, b| {
                a.used
                    .partial_cmp(&b.used)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    };
    (most(false), most(true))
}

fn compact_visible(shown: &[(Driver, Snapshot)], filter: Option<Driver>) -> bool {
    shown
        .iter()
        .any(|(driver, _)| filter.is_none_or(|f| f == *driver))
}

fn window_tag(w: &QuotaWindow) -> String {
    if w.label.starts_with("Weekly") {
        return "wk".to_owned();
    }
    if let Some((length, unit)) = w.label.split_once('-') {
        let suffix = match unit {
            "day" => Some("d"),
            "hour" => Some("h"),
            "min" => Some("m"),
            _ => None,
        };
        if let Some(suffix) = suffix.filter(|_| length.parse::<u64>().is_ok_and(|n| n > 0)) {
            return format!("{length}{suffix}");
        }
    }
    w.label
        .split_whitespace()
        .next()
        .unwrap_or("")
        .chars()
        .take(4)
        .collect()
}

fn bar(used: f64, width: i32) -> gtk4::ProgressBar {
    let bar = gtk4::ProgressBar::new();
    bar.set_fraction(used.clamp(0.0, 1.0));
    bar.set_width_request(width);
    bar.set_valign(gtk4::Align::Center);
    bar.add_css_class("usage-bar");
    bar.add_css_class(severity(used).css_class());
    bar
}

pub struct UsageIndicator {
    root: gtk4::MenuButton,
    compact: gtk4::Box,
    details: gtk4::Box,
    status: Rc<AccountStatus>,
    filter: Cell<Option<Driver>>,
    /// The change listener, removed again when the indicator goes away.
    listener: Cell<Option<u64>>,
    /// Likewise for availability: an agent that stops being ready leaves the indicator.
    availability_listener: Cell<Option<u64>>,
    /// Run after each repaint (its size may have changed): see [`Self::connect_repainted`].
    on_repaint: RefCell<Vec<Box<dyn Fn()>>>,
    /// The repaint due when a shown reading next changes freshness (Fresh to Aging, Aging to
    /// Expired), so an unfocused window that probes nothing still stops looking healthy.
    boundary: RefCell<Option<gtk4::glib::SourceId>>,
    /// This indicator, for the boundary timer to call back without keeping it alive.
    me: RefCell<std::rc::Weak<UsageIndicator>>,
}

impl Drop for UsageIndicator {
    fn drop(&mut self) {
        if let Some(source) = self.boundary.take() {
            source.remove();
        }
        if let Some(id) = self.listener.take() {
            self.status.disconnect(id);
        }
        if let Some(id) = self.availability_listener.take() {
            AgentAvailability::shared().disconnect(id);
        }
    }
}

impl UsageIndicator {
    /// `filter`: show only this agent (the chat header follows the thread's agent); `None`
    /// shows both. Repaints on every [`AccountStatus`] change.
    pub fn new(status: Rc<AccountStatus>, filter: Option<Driver>) -> Rc<Self> {
        // Its colours (by usage, and the signed-out tag) are in the chat stylesheet, which the
        // sidebar's indicator is built before any chat view loads.
        super::load_style();
        let root = gtk4::MenuButton::new();
        root.add_css_class("flat");
        root.add_css_class("usage-indicator");
        root.set_tooltip_text(Some("Plan usage"));
        let compact = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        root.set_child(Some(&compact));
        let details = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        details.set_margin_top(12);
        details.set_margin_bottom(12);
        details.set_margin_start(14);
        details.set_margin_end(14);
        details.set_width_request(300);
        let popover = gtk4::Popover::new();
        popover.add_css_class("usage-popover");
        popover.set_child(Some(&details));
        root.set_popover(Some(&popover));

        let this = Rc::new(Self {
            root,
            compact,
            details,
            status,
            filter: Cell::new(filter),
            listener: Cell::new(None),
            availability_listener: Cell::new(None),
            on_repaint: RefCell::new(Vec::new()),
            boundary: RefCell::new(None),
            me: RefCell::new(std::rc::Weak::new()),
        });
        *this.me.borrow_mut() = Rc::downgrade(&this);
        this.repaint();
        let weak = Rc::downgrade(&this);
        let id = AgentAvailability::shared().connect_changed(move || {
            if let Some(this) = weak.upgrade() {
                this.repaint();
            }
        });
        this.availability_listener.set(Some(id));
        let weak = Rc::downgrade(&this);
        let id = this.status.connect_changed(move || {
            if let Some(this) = weak.upgrade() {
                this.repaint();
            }
        });
        this.listener.set(Some(id));
        // "Resets in" ages while the popover is closed: rebuild it as it opens.
        let weak = Rc::downgrade(&this);
        popover.connect_show(move |_| {
            if let Some(this) = weak.upgrade() {
                this.repaint();
            }
        });
        this
    }

    pub fn widget(&self) -> &gtk4::MenuButton {
        &self.root
    }

    /// Lists the agents one under another rather than side by side (the sidebar, which is
    /// narrow).
    pub fn stack_agents(&self) {
        self.compact.set_orientation(gtk4::Orientation::Vertical);
        self.compact.set_spacing(4);
    }

    /// Follows the thread when it moves to the other agent.
    pub fn set_filter(&self, filter: Option<Driver>) {
        if self.filter.replace(filter) != filter {
            self.repaint();
        }
    }

    fn shown(&self) -> Vec<(Driver, Snapshot)> {
        let availability = AgentAvailability::shared();
        Driver::ALL
            .into_iter()
            .filter(|d| availability.is_ready(*d))
            .map(|d| (d, self.status.snapshot(d)))
            .collect()
    }

    fn repaint(&self) {
        let shown = self.shown();
        self.render_snapshots(&shown);
        self.schedule_boundary(&shown, now_epoch());
    }

    /// Replaces the pending boundary repaint with one at the next moment a shown reading
    /// changes freshness (none when nothing will).
    fn schedule_boundary(&self, shown: &[(Driver, Snapshot)], now: i64) {
        if let Some(old) = self.boundary.take() {
            old.remove(); // still pending: a fired one clears itself first
        }
        let reads = shown
            .iter()
            .filter(|(_, s)| !s.signed_out)
            .filter_map(|(_, s)| s.read_at);
        let Some(at) = next_boundary(reads, now, ACCOUNT_TTL) else {
            return;
        };
        let secs = u32::try_from(at.saturating_sub(now).max(1)).unwrap_or(u32::MAX);
        let weak = self.me.borrow().clone();
        let id = gtk4::glib::timeout_add_seconds_local_once(secs, move || {
            if let Some(this) = weak.upgrade() {
                // This source is done: forget it without removing it, then repaint (which
                // schedules the next boundary).
                let _fired = this.boundary.take();
                this.repaint();
            }
        });
        *self.boundary.borrow_mut() = Some(id);
    }

    fn render_snapshots(&self, shown: &[(Driver, Snapshot)]) {
        while let Some(child) = self.compact.first_child() {
            self.compact.remove(&child);
        }
        while let Some(child) = self.details.first_child() {
            self.details.remove(&child);
        }
        self.root
            .set_visible(compact_visible(shown, self.filter.get()));
        let label = match self.filter.get() {
            Some(driver) => format!("Usage for {}; show all agents", driver.info().label),
            None => "Agent usage; show all agents".to_owned(),
        };
        self.root
            .update_property(&[gtk4::accessible::Property::Label(&label)]);
        self.root.set_tooltip_text(Some(&label));
        let now = now_epoch();
        for (driver, snap) in shown {
            if self.filter.get().is_none_or(|f| f == *driver) {
                self.compact.append(&compact_form(*driver, snap, now));
            }
            self.details.append(&detail_form(*driver, snap, now));
        }
        for f in self.on_repaint.borrow().iter() {
            f();
        }
    }

    /// Calls `f` after every repaint, when the indicator may have changed size (a container
    /// that fixes its own size, such as the chat header's, re-fits to it).
    pub fn connect_repainted(&self, f: impl Fn() + 'static) {
        self.on_repaint.borrow_mut().push(Box::new(f));
    }
}

/// How a snapshot's reading may be shown at `now` (see [`crate::freshness`]).
pub fn reading_freshness(snap: &Snapshot, now: i64) -> Freshness {
    freshness(age_of(snap.read_at, now), ACCOUNT_TTL)
}

/// `14:32`, local time, for a reading taken at `at`.
fn clock(at: i64) -> String {
    gtk4::glib::DateTime::from_unix_local(at)
        .and_then(|t| t.format("%H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// The line saying when an agent's usage was read: `Updated 14:32` while Fresh, `Last read 14:32
/// (38 min ago)` once Aging, and for Expired data that was once read, why it is not shown.
pub fn reading_text(snap: &Snapshot, now: i64) -> Option<String> {
    let at = snap.read_at?;
    let age = age_of(Some(at), now).unwrap_or(0);
    Some(match reading_freshness(snap, now) {
        Freshness::Fresh if !snap.refresh_failed => format!("Updated {}", clock(at)),
        Freshness::Fresh | Freshness::Aging => last_read_text(&clock(at), age, snap.refresh_failed),
        Freshness::Expired => format!(
            "Could not read recently (last read {}, {})",
            clock(at),
            ago(age)
        ),
    })
}

/// Whether the agent's usage is drawn loading: a probe of it runs and there is nothing to show
/// (never read, no window, or too old to show). Old data that can still be shown stays shown
/// while it refreshes; a signed-out agent says so instead.
pub fn is_loading(snap: &Snapshot, now: i64) -> bool {
    snap.probing
        && !snap.signed_out
        && (snap.windows.is_empty() || reading_freshness(snap, now) == Freshness::Expired)
}

/// The loading meter's text.
const LOADING_TEXT: &str = "Checking…";

/// The loading meter's tooltip.
pub fn loading_tooltip(agent: &str) -> String {
    format!("{agent} · Checking usage…")
}

/// The "Unknown" meter's tooltip: always says why there is nothing to show.
pub fn unknown_tooltip(agent: &str, read: Option<&str>) -> String {
    match read {
        Some(read) => format!("{agent} · {read}"),
        None => format!("{agent} · No usage data could be read"),
    }
}

/// The severity class a value is drawn with: an Aging reading never gets the healthy colour.
fn shown_class(used: f64, aging: bool) -> &'static str {
    match severity(used) {
        Severity::Normal if aging => "usage-stale",
        s => s.css_class(),
    }
}

/// How many rows the loaded meter will draw: one per window kind the last reading had, else
/// both, the usual shape.
fn expected_rows(windows: &[QuotaWindow]) -> usize {
    match split_windows(windows) {
        (None, None) => 2,
        (short, weekly) => usize::from(short.is_some()) + usize::from(weekly.is_some()),
    }
}

/// The loading text laid out as the loaded meter's rows (same spacing, same text style), so the
/// meter keeps its height when the reading arrives: "Checking…" on the first, blank lines below.
fn loading_rows(windows: &[QuotaWindow]) -> gtk4::Box {
    let rows = gtk4::Box::new(gtk4::Orientation::Vertical, 1);
    rows.set_valign(gtk4::Align::Center);
    rows.append(&label(LOADING_TEXT, &["usage-text"]));
    for _ in 1..expected_rows(windows) {
        rows.append(&label("", &["usage-text"]));
    }
    rows
}

/// Accent dot + one thin row per window kind, labelled by its length, with a bar and percentage.
/// Only a Fresh reading is drawn as is; an Aging one is muted (never green) with when it was
/// read; an Expired one is the could-not-read state ("Unknown"), unless a probe is fetching one,
/// which draws the agent's dot spinning ([`is_loading`]).
fn compact_form(driver: Driver, snap: &Snapshot, now: i64) -> gtk4::Box {
    let b = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    b.add_css_class(accent_class(driver));
    if is_loading(snap, now) {
        b.add_css_class("usage-loading");
        b.append(&crate::brand::drip(driver));
        b.append(&loading_rows(&snap.windows));
        b.set_tooltip_text(Some(&loading_tooltip(driver.info().label)));
        return b;
    }
    b.append(&brand_image(driver));
    if snap.signed_out {
        b.append(&label("signed out", &["usage-text", "usage-crit"]));
        b.set_tooltip_text(Some(&format!(
            "{} is not signed in. {}",
            driver.info().label,
            driver.info().sign_in_hint
        )));
        return b;
    }
    let state = reading_freshness(snap, now);
    let read = reading_text(snap, now);
    let (short, weekly) = split_windows(&snap.windows);
    if state == Freshness::Expired || (short.is_none() && weekly.is_none()) {
        b.append(&label("Unknown", &["usage-text"]));
        b.set_tooltip_text(Some(&unknown_tooltip(driver.info().label, read.as_deref())));
        return b;
    }
    let aging = state == Freshness::Aging || snap.refresh_failed;
    if aging {
        b.add_css_class("usage-stale-meter");
    }
    let rows = gtk4::Box::new(gtk4::Orientation::Vertical, 1);
    rows.set_valign(gtk4::Align::Center);
    let mut tips = vec![driver.info().label.to_owned()];
    for window in [short, weekly] {
        let Some(w) = window else { continue };
        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 5);
        row.append(&label(&window_tag(w), &["usage-text", "usage-tag"]));
        let meter = bar(w.used, 48);
        if aging {
            meter.remove_css_class(severity(w.used).css_class());
            meter.add_css_class(shown_class(w.used, true));
        }
        row.append(&meter);
        row.append(&label(
            &percent_text(w.used),
            &["usage-text", shown_class(w.used, aging)],
        ));
        rows.append(&row);
        tips.push(format!("{} {}", window_title(w), percent_text(w.used)));
    }
    tips.extend(read);
    b.append(&rows);
    b.set_tooltip_text(Some(&tips.join(" · ")));
    b
}

/// Account line and every window of one agent.
fn detail_form(driver: Driver, snap: &Snapshot, now: i64) -> gtk4::Box {
    let section = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    section.add_css_class(accent_class(driver));
    let head = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    head.append(&brand_image(driver));
    head.append(&label(driver.info().label, &["usage-agent"]));
    section.append(&head);
    if snap.signed_out {
        let l = label("Not signed in", &["usage-text", "usage-crit"]);
        l.set_halign(gtk4::Align::Start);
        section.append(&l);
        let hint = wrapped_text(driver.info().sign_in_hint, &["dim-label", "usage-account"]);
        hint.set_halign(gtk4::Align::Start);
        section.append(&hint);
        return section;
    }
    if let Some(account) = &snap.account {
        let line = match &account.plan {
            Some(plan) => format!("{} · {plan}", account.label),
            None => account.label.clone(),
        };
        let l = label(&line, &["dim-label", "usage-account"]);
        l.set_halign(gtk4::Align::Start);
        l.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
        section.append(&l);
    }
    let state = reading_freshness(snap, now);
    let aging = state == Freshness::Aging || (state == Freshness::Fresh && snap.refresh_failed);
    if let Some(text) = reading_text(snap, now) {
        let l = wrapped_text(&text, &["dim-label", "usage-resets"]);
        l.set_halign(gtk4::Align::Start);
        section.append(&l);
    }
    // Expired: the could-not-read state; values that old are not shown as current.
    let windows: &[QuotaWindow] = if state == Freshness::Expired {
        &[]
    } else {
        &snap.windows
    };
    if is_loading(snap, now) {
        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        row.add_css_class("usage-loading");
        row.append(&crate::brand::drip(driver));
        row.append(&label("Checking usage…", &["dim-label"]));
        section.append(&row);
    } else if windows.is_empty() {
        let l = label("No usage data yet", &["dim-label"]);
        l.set_halign(gtk4::Align::Start);
        section.append(&l);
    }
    for w in windows {
        let row = gtk4::Box::new(gtk4::Orientation::Vertical, 3);
        let top = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let title = label(&window_title(w), &["usage-window"]);
        title.set_halign(gtk4::Align::Start);
        title.set_hexpand(true);
        title.set_xalign(0.0);
        top.append(&title);
        top.append(&label(
            &format!("{} used", percent_text(w.used)),
            &["usage-text", shown_class(w.used, aging)],
        ));
        row.append(&top);
        let b = bar(w.used, -1);
        if aging {
            b.remove_css_class(severity(w.used).css_class());
            b.add_css_class(shown_class(w.used, true));
        }
        b.set_hexpand(true);
        row.append(&b);
        if let Some(text) = resets_text(w, now) {
            let l = label(&text, &["dim-label", "usage-resets"]);
            l.set_halign(gtk4::Align::Start);
            row.append(&l);
        }
        section.append(&row);
    }
    section
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn fallback_window_tags_fit_four_characters() {
        for (label, tag) in [
            ("Primary quota", "Prim"),
            ("Secondary quota", "Seco"),
            ("Échéance courte", "Éché"),
            ("使用可能量", "使用可能"),
            ("Now", "Now"),
            ("", ""),
        ] {
            assert_eq!(window_tag(&named(label, 0.1)), tag);
        }
    }

    #[test]
    #[ignore = "native GTK; run alone in an isolated display"]
    fn native_usage_chip_hides_when_its_agent_is_not_shown() {
        gtk4::init().expect("GTK init");
        let usage = UsageIndicator::new(Rc::new(AccountStatus::new()), Some(Driver::Codex));
        let shown = [(Driver::Claude, Snapshot::default())];
        usage.render_snapshots(&shown);
        assert!(!usage.root.property::<bool>("visible"));
        assert!(usage.compact.first_child().is_none());
        assert!(usage.details.first_child().is_some());
        usage.filter.set(Some(Driver::Claude));
        usage.render_snapshots(&shown);
        assert!(usage.root.property::<bool>("visible"));
        assert!(usage.compact.first_child().is_some());
    }

    #[test]
    fn the_usage_chip_hides_when_its_agent_is_not_ready() {
        let shown = [(Driver::Claude, Snapshot::default())];
        assert!(!compact_visible(&shown, Some(Driver::Codex)));
        assert!(compact_visible(&shown, Some(Driver::Claude)));
        assert!(compact_visible(&shown, None));
        assert!(!compact_visible(&[], None));
        assert!(!compact_visible(&[], Some(Driver::Claude)));
    }

    #[test]
    fn a_two_day_codex_window_is_not_tagged_5h() {
        let windows = agent_core::quota::codex_rate_limits(&serde_json::json!({
            "rateLimits": {"primary": {"usedPercent": 20, "windowDurationMins": 2880}}
        }));
        let (short, weekly) = split_windows(&windows);
        assert!(weekly.is_none());
        assert_eq!(window_tag(short.unwrap()), "2d");
        for (label, tag) in [
            ("5-hour", "5h"),
            ("Weekly (Fable)", "wk"),
            ("3-hour", "3h"),
            ("15-min", "15m"),
            ("Primary quota", "Prim"),
            ("Échéance courte", "Éché"),
        ] {
            assert_eq!(window_tag(&named(label, 0.1)), tag);
        }
    }

    pub(crate) fn compact_scope_does_not_filter_provider_comparison() {
        let usage = UsageIndicator::new(Rc::new(AccountStatus::new()), Some(Driver::Codex));
        let mut zero = Snapshot::default();
        zero.windows.push(named("5-hour", 0.0));
        zero.read_at = Some(now_epoch());
        let snapshots = [
            (Driver::Claude, zero),
            (Driver::Agy, Snapshot::default()),
            (Driver::Codex, Snapshot::default()),
        ];
        usage.render_snapshots(&snapshots);
        fn children(widget: &impl IsA<gtk4::Widget>) -> usize {
            let mut n = 0;
            let mut child = widget.first_child();
            while let Some(w) = child {
                n += 1;
                child = w.next_sibling();
            }
            n
        }
        let labels = |widget: &gtk4::Widget| crate::testutil::label_texts(widget).join("\n");
        assert_eq!(
            children(&usage.compact),
            1,
            "header remains current-provider only"
        );
        assert_eq!(
            children(&usage.details),
            3,
            "comparison includes all providers"
        );
        assert!(labels(usage.compact.upcast_ref()).contains("Unknown"));
        let details = labels(usage.details.upcast_ref());
        assert!(
            details.contains("0 % used"),
            "reported zero remains a real value"
        );
        assert!(
            details.contains("No usage data yet"),
            "unknown is distinct from zero"
        );
        usage.filter.set(Some(Driver::Claude));
        usage.render_snapshots(&snapshots);
        assert_eq!(
            children(&usage.details),
            3,
            "switching retains the comparison scope"
        );
        assert!(labels(usage.compact.upcast_ref()).contains("0 %"));
    }

    fn win(group: Option<&str>, resets: Option<&str>) -> QuotaWindow {
        QuotaWindow {
            group: group.map(str::to_owned),
            label: "Weekly".into(),
            used: 0.5,
            resets_at: resets.map(str::to_owned),
        }
    }

    fn named(label: &str, used: f64) -> QuotaWindow {
        QuotaWindow {
            group: None,
            label: label.into(),
            used,
            resets_at: None,
        }
    }

    #[test]
    fn the_meter_keeps_the_five_hour_and_weekly_windows_apart() {
        // Claude's shape: a 5-hour window, the weekly one and a per-model week.
        let w = [
            named("5-hour", 0.56),
            named("Weekly", 0.18),
            named("Weekly (Fable)", 0.40),
        ];
        let (short, weekly) = split_windows(&w);
        assert_eq!(short.map(|w| w.label.as_str()), Some("5-hour"));
        // The busiest week is the one that bites first.
        assert_eq!(weekly.map(|w| w.label.as_str()), Some("Weekly (Fable)"));
        // Only one kind known: the other is absent, not borrowed.
        let only_week = [named("Weekly", 0.3)];
        assert_eq!(split_windows(&only_week).0, None);
        assert!(split_windows(&only_week).1.is_some());
        assert_eq!(split_windows(&[]), (None, None));
    }

    #[test]
    fn only_a_fresh_reading_is_healthy_and_old_ones_say_when_they_were_read() {
        let now = 1_800_000_000;
        let snap = |age: Option<i64>, failed| Snapshot {
            windows: vec![named("5-hour", 0.1)],
            read_at: age.map(|a| now - a),
            refresh_failed: failed,
            ..Snapshot::default()
        };
        // Fresh: healthy colour, "Updated HH:MM".
        assert_eq!(
            reading_freshness(&snap(Some(60), false), now),
            Freshness::Fresh
        );
        assert!(
            reading_text(&snap(Some(60), false), now).is_some_and(|t| t.starts_with("Updated "))
        );
        assert_eq!(shown_class(0.1, false), "usage-ok");
        // Aging (over 2×TTL): never green, and the line says how old.
        let aging = snap(Some(3 * ACCOUNT_TTL), true);
        assert_eq!(reading_freshness(&aging, now), Freshness::Aging);
        assert_eq!(shown_class(0.1, true), "usage-stale");
        assert_eq!(
            shown_class(0.95, true),
            "usage-crit",
            "a warning stays a warning"
        );
        let text = reading_text(&aging, now).unwrap();
        assert!(
            text.starts_with("Last read ") && text.ends_with("(30 min ago); could not refresh"),
            "{text}"
        );
        // Fresh but the last refresh failed: not shown as a clean "Updated".
        assert!(
            reading_text(&snap(Some(60), true), now).is_some_and(|t| t.starts_with("Last read "))
        );
        // Expired (over 6×TTL) or never read: the could-not-read state.
        assert_eq!(
            reading_freshness(&snap(Some(7 * ACCOUNT_TTL), false), now),
            Freshness::Expired
        );
        assert_eq!(
            reading_freshness(&snap(None, false), now),
            Freshness::Expired
        );
        assert_eq!(reading_text(&snap(None, false), now), None);
        // Never read: the Unknown meter still says why.
        assert_eq!(
            unknown_tooltip("Codex", None),
            "Codex · No usage data could be read"
        );
        assert_eq!(
            unknown_tooltip("Codex", Some("Could not read recently")),
            "Codex · Could not read recently"
        );
        assert!(reading_text(&snap(Some(7 * ACCOUNT_TTL), false), now)
            .is_some_and(|t| t.starts_with("Could not read recently")));
    }

    /// Loading is a probe fetching what is not there to show; shown data, a signed-out agent and
    /// a probe that ended are never drawn loading.
    #[test]
    fn only_a_probe_with_nothing_to_show_is_drawn_loading() {
        let now = 1_800_000_000;
        let snap = |windows: Vec<QuotaWindow>, age: Option<i64>, probing, signed_out| Snapshot {
            windows,
            read_at: age.map(|a| now - a),
            probing,
            signed_out,
            ..Snapshot::default()
        };
        let five = || vec![named("5-hour", 0.4)];
        assert!(
            is_loading(&snap(vec![], None, true, false), now),
            "never read"
        );
        assert!(
            is_loading(&snap(five(), Some(7 * ACCOUNT_TTL), true, false), now),
            "too old to show"
        );
        assert!(
            is_loading(&snap(vec![], Some(60), true, false), now),
            "read, but no window"
        );
        assert!(
            !is_loading(&snap(five(), Some(60), true, false), now),
            "fresh data stays shown"
        );
        assert!(
            !is_loading(&snap(five(), Some(3 * ACCOUNT_TTL), true, false), now),
            "aging data stays shown, muted"
        );
        assert!(
            !is_loading(&snap(vec![], None, false, false), now),
            "no probe: Unknown"
        );
        assert!(
            !is_loading(&snap(vec![], None, true, true), now),
            "signed out says so"
        );
        assert_eq!(loading_tooltip("Codex"), "Codex · Checking usage…");
    }

    #[test]
    #[ignore = "native GTK; run alone in an isolated display"]
    fn native_a_loading_agent_spins_its_own_dot() {
        gtk4::init().expect("GTK init");
        let snap = Snapshot {
            probing: true,
            ..Snapshot::default()
        };
        for driver in Driver::ALL {
            let form = compact_form(driver, &snap, now_epoch());
            assert!(form.has_css_class("usage-loading"));
            let drip = form
                .first_child()
                .and_downcast::<gtk4::Overlay>()
                .expect("the drip leads");
            assert!(drip.has_css_class("brand-drip"));
            let dot = format!("dot-{}", driver.info().key);
            assert!(drip.child().is_none(), "no dot in the middle");
            let (mut orbits, mut falls, mut ripples) = (0, 0, 0);
            let mut child = drip.first_child();
            while let Some(part) = child {
                if part.has_css_class("drip-orbit") {
                    let drop = part.first_child().expect("a drop");
                    assert!(drop.has_css_class(&dot) && drop.has_css_class("drip-drop"));
                    orbits += 1;
                } else if part.has_css_class("drip-fall") {
                    falls += 1;
                } else if part.has_css_class("drip-ripple") {
                    ripples += 1;
                }
                assert!(
                    part.has_css_class(&dot) || part.has_css_class("drip-orbit"),
                    "{driver:?}'s own colour"
                );
                child = part.next_sibling();
            }
            // One droplet: the loop's and the falling one hand over, never shown together.
            assert_eq!((orbits, falls, ripples), (1, 1, 1));
            let rows = form.last_child().expect("the rows");
            let text = rows
                .first_child()
                .and_downcast::<gtk4::Label>()
                .expect("text");
            assert_eq!(text.text(), LOADING_TEXT);
            // As tall as the meter it stands for: two rows, both window kinds.
            let loaded = compact_form(
                driver,
                &Snapshot {
                    windows: vec![named("5-hour", 0.4), named("Weekly", 0.2)],
                    read_at: Some(now_epoch()),
                    ..Snapshot::default()
                },
                now_epoch(),
            );
            let height = |w: &gtk4::Box| w.measure(gtk4::Orientation::Vertical, -1).1;
            assert_eq!(
                height(&form),
                height(&loaded),
                "{driver:?} keeps its height"
            );
        }
        assert_eq!(expected_rows(&[]), 2, "never read: the usual two");
        assert_eq!(expected_rows(&[named("Weekly", 0.1)]), 1, "as last read");
        // The probe ended with nothing read: the could-not-read state.
        let form = compact_form(Driver::Claude, &Snapshot::default(), now_epoch());
        assert!(!form.has_css_class("usage-loading"));
    }

    #[test]
    fn colour_thresholds() {
        assert_eq!(severity(0.0), Severity::Normal);
        assert_eq!(severity(0.7499), Severity::Normal);
        assert_eq!(severity(0.75), Severity::Warn);
        assert_eq!(severity(0.8999), Severity::Warn);
        assert_eq!(severity(0.9), Severity::Critical);
        assert_eq!(severity(1.0), Severity::Critical);
        assert_eq!(Severity::Warn.css_class(), "usage-warn");
        assert_eq!(Severity::Critical.css_class(), "usage-crit");
        assert_eq!(Severity::Normal.css_class(), "usage-ok");
    }

    #[test]
    fn percent_rounds_and_clamps() {
        assert_eq!(percent_text(0.0), "0 %");
        assert_eq!(percent_text(0.426), "43 %");
        assert_eq!(percent_text(0.994), "99 %");
        assert_eq!(percent_text(1.7), "100 %");
        assert_eq!(percent_text(-1.0), "0 %");
    }

    #[test]
    fn titles_and_reset_phrases() {
        assert_eq!(window_title(&win(None, None)), "Weekly");
        assert_eq!(
            window_title(&win(Some("Gemini Models"), None)),
            "Gemini Models · Weekly"
        );
        let now = parse_rfc3339("2026-10-06T13:12:00Z").unwrap();
        let w = win(None, Some("2026-10-06T16:59:24Z"));
        assert_eq!(
            resets_text(&w, now).as_deref(),
            Some("resets in 3 h 48 min")
        );
        assert_eq!(
            resets_text(&win(None, Some("2026-10-06T10:00:00Z")), now).as_deref(),
            Some("resets now")
        );
        assert_eq!(resets_text(&win(None, None), now), None);
        assert_eq!(resets_text(&win(None, Some("soon")), now), None);
    }
}
