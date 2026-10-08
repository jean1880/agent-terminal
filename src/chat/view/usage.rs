//! The usage indicator: per agent, an accent dot and two thin bars with their percentages, the
//! short (5-hour) window and the weekly one, each the most used of its kind (amber from 75 %, red
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

use super::cards::{accent_class, brand_image, driver_name, label};
use crate::account_status::{AccountStatus, Snapshot};
use crate::availability::AgentAvailability;

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

/// The windows the compact meter shows: the most-used short window (the 5-hour one) and the
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

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
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
}

impl Drop for UsageIndicator {
    fn drop(&mut self) {
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
        });
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
        self.render_snapshots(&self.shown());
    }

    fn render_snapshots(&self, shown: &[(Driver, Snapshot)]) {
        while let Some(child) = self.compact.first_child() {
            self.compact.remove(&child);
        }
        while let Some(child) = self.details.first_child() {
            self.details.remove(&child);
        }
        self.root.set_visible(!shown.is_empty());
        let label = match self.filter.get() {
            Some(driver) => format!("Usage for {}; show all agents", driver_name(driver)),
            None => "Agent usage; show all agents".to_owned(),
        };
        self.root
            .update_property(&[gtk4::accessible::Property::Label(&label)]);
        self.root.set_tooltip_text(Some(&label));
        let now = now_epoch();
        for (driver, snap) in shown {
            if self.filter.get().is_none_or(|f| f == *driver) {
                self.compact.append(&compact_form(*driver, snap));
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

/// Accent dot + one thin row per window kind: `5h` and `wk`, each with its bar and percentage.
fn compact_form(driver: Driver, snap: &Snapshot) -> gtk4::Box {
    let b = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    b.add_css_class(accent_class(driver));
    b.append(&brand_image(driver));
    if snap.signed_out {
        b.append(&label("signed out", &["usage-text", "usage-crit"]));
        b.set_tooltip_text(Some(&format!(
            "{} is not signed in. {}",
            driver_name(driver),
            driver.info().sign_in_hint
        )));
        return b;
    }
    let (short, weekly) = split_windows(&snap.windows);
    if short.is_none() && weekly.is_none() {
        b.append(&label("Unknown", &["usage-text"]));
        return b;
    }
    let rows = gtk4::Box::new(gtk4::Orientation::Vertical, 1);
    rows.set_valign(gtk4::Align::Center);
    let mut tips = vec![driver_name(driver).to_owned()];
    for (tag, window) in [("5h", short), ("wk", weekly)] {
        let Some(w) = window else { continue };
        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 5);
        row.append(&label(tag, &["usage-text", "usage-tag"]));
        row.append(&bar(w.used, 48));
        row.append(&label(
            &percent_text(w.used),
            &["usage-text", severity(w.used).css_class()],
        ));
        rows.append(&row);
        tips.push(format!("{} {}", window_title(w), percent_text(w.used)));
    }
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
    head.append(&label(driver_name(driver), &["usage-agent"]));
    section.append(&head);
    if snap.signed_out {
        let l = label("Not signed in", &["usage-text", "usage-crit"]);
        l.set_halign(gtk4::Align::Start);
        section.append(&l);
        let hint = label(driver.info().sign_in_hint, &["dim-label", "usage-account"]);
        hint.set_halign(gtk4::Align::Start);
        hint.set_wrap(true);
        hint.set_xalign(0.0);
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
    if snap.windows.is_empty() {
        let l = label("No usage data yet", &["dim-label"]);
        l.set_halign(gtk4::Align::Start);
        section.append(&l);
    }
    for w in &snap.windows {
        let row = gtk4::Box::new(gtk4::Orientation::Vertical, 3);
        let top = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let title = label(&window_title(w), &["usage-window"]);
        title.set_halign(gtk4::Align::Start);
        title.set_hexpand(true);
        title.set_xalign(0.0);
        top.append(&title);
        top.append(&label(
            &format!("{} used", percent_text(w.used)),
            &["usage-text", severity(w.used).css_class()],
        ));
        row.append(&top);
        let b = bar(w.used, -1);
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

    pub(crate) fn compact_scope_does_not_filter_provider_comparison() {
        let usage = UsageIndicator::new(Rc::new(AccountStatus::new()), Some(Driver::Codex));
        let mut zero = Snapshot::default();
        zero.windows.push(named("5-hour", 0.0));
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
        fn labels(widget: &gtk4::Widget) -> String {
            let mut text = widget
                .downcast_ref::<gtk4::Label>()
                .map(|l| l.text().to_string())
                .unwrap_or_default();
            let mut child = widget.first_child();
            while let Some(w) = child {
                text.push_str(&labels(&w));
                text.push('\n');
                child = w.next_sibling();
            }
            text
        }
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
