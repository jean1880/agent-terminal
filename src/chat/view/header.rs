//! The strip at the top of the chat view: agent + model chip, mode dropdown, the thread's
//! activity (its turn and its background work) and the context gauge.

use std::cell::{Cell, RefCell};

use adw::prelude::*;
use agent_core::adapter::{Driver, Mode};

use super::cards::{accent_class, driver_name, label};
use super::model::{format_tokens, Activity, Gauge};

/// Dropdown order of the modes. Driven by index both ways; the guard test keeps it complete.
pub const MODES: [Mode; 3] = [Mode::Ask, Mode::AcceptEdits, Mode::Plan];

pub fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Ask => "Ask before edits",
        Mode::AcceptEdits => "Accept edits",
        Mode::Plan => "Plan",
    }
}

pub fn mode_index(mode: Mode) -> u32 {
    match mode {
        Mode::Ask => 0,
        Mode::AcceptEdits => 1,
        Mode::Plan => 2,
    }
}

/// Gauge fill (0..=1) and the auto-compact threshold as a fraction, when known.
pub fn gauge_fraction(g: &Gauge) -> Option<(f64, Option<f64>)> {
    let max = g.max.filter(|m| *m > 0)? as f64;
    let fill = (g.used as f64 / max).clamp(0.0, 1.0);
    let compact = g.auto_compact_at.map(|a| (a as f64 / max).clamp(0.0, 1.0));
    Some((fill, compact))
}

pub fn gauge_text(g: &Gauge) -> String {
    match g.max {
        Some(max) if max > 0 => format!("{} / {}", format_tokens(g.used), format_tokens(max)),
        _ => format!("{} tokens", format_tokens(g.used)),
    }
}

/// The header's own width breakpoints (sp), widest first. Each drops more of the strip, so it
/// fits the thread at any window size: the usage meter (also in the sidebar), then the context
/// gauge, then the sub-agents button and the model's name, with tighter spacing.
const HIDE_USAGE_BELOW: f64 = 880.0;
const HIDE_GAUGE_BELOW: f64 = 700.0;
const COMPACT_BELOW: f64 = 500.0;
/// The narrowest the strip supports: the whole window at its minimum.
const MIN_WIDTH: i32 = 340;

pub struct Header {
    /// What the view packs: the strip in its breakpoint bin.
    pub bin: adw::BreakpointBin,
    root: gtk4::Box,
    pub chip: gtk4::Button,
    brand: gtk4::Image,
    agent: gtk4::Label,
    model: gtk4::Label,
    pub mode: gtk4::DropDown,
    /// Set while the view changes the dropdown itself, so that is not taken as a user choice.
    pub mode_guard: Cell<bool>,
    pub reload: gtk4::Button,
    activity: gtk4::Box,
    /// Where [`Self::set_usage`] and [`Self::set_subagents`] put their widgets, so the
    /// breakpoints can drop each.
    usage_slot: gtk4::Box,
    subagent_slot: gtk4::Box,
    activity_text: gtk4::Label,
    spinner: gtk4::Spinner,
    /// The status the header shows now.
    shown: RefCell<Activity>,
    pub gauge: gtk4::Button,
    level: gtk4::LevelBar,
    gauge_label: gtk4::Label,
    last_driver: Cell<Option<Driver>>,
}

/// Grows `bin`'s height request to `root`'s minimum height. Never shrinks it: the strip changes
/// by a few pixels as items come and go, and a header that jumps with each would be worse.
fn fit_height(bin: &adw::BreakpointBin, root: &gtk4::Box) {
    let needed = root.measure(gtk4::Orientation::Vertical, -1).0;
    if needed > bin.height_request() {
        bin.set_height_request(needed);
    }
}

impl Header {
    /// Re-fits the bin to the strip after its contents changed (see [`fit_height`]).
    pub fn fit_height(&self) {
        fit_height(&self.bin, &self.root);
    }

    /// A callback for widgets placed in the strip that change size on their own (the usage
    /// meter): re-fits the bin, holding it weakly.
    pub fn refitter(&self) -> impl Fn() + 'static {
        let (bin, root) = (self.bin.downgrade(), self.root.downgrade());
        move || {
            if let (Some(bin), Some(root)) = (bin.upgrade(), root.upgrade()) {
                fit_height(&bin, &root);
            }
        }
    }

    pub fn new() -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
        root.add_css_class("chat-header");

        let chip = gtk4::Button::new();
        chip.add_css_class("agent-chip");
        chip.set_tooltip_text(Some("Switch model or agent (/model)"));
        let chip_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let brand = gtk4::Image::new();
        brand.set_pixel_size(14);
        brand.add_css_class("accent-dot");
        let agent = label("", &["chip-agent"]);
        let model = label("", &["chip-model"]);
        model.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
        model.set_max_width_chars(28);
        chip_box.append(&brand);
        chip_box.append(&agent);
        chip_box.append(&model);
        chip_box.append(&gtk4::Image::from_icon_name("at-pan-down-symbolic"));
        chip.set_child(Some(&chip_box));
        root.append(&chip);

        let names: Vec<&str> = MODES.iter().map(|m| mode_label(*m)).collect();
        let mode = gtk4::DropDown::from_strings(&names);
        mode.add_css_class("mode-dropdown");
        mode.set_tooltip_text(Some("Interaction mode (/mode)"));
        root.append(&mode);

        let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        root.append(&spacer);

        let reload = gtk4::Button::from_icon_name("at-view-refresh-symbolic");
        reload.add_css_class("flat");
        reload.add_css_class("session-reload");
        reload.set_valign(gtk4::Align::Center);
        reload.set_tooltip_text(Some("Reload session"));
        root.append(&reload);

        let activity = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        activity.add_css_class("activity");
        let spinner = gtk4::Spinner::new();
        activity.append(&spinner);
        let activity_text = label("", &["activity-text"]);
        activity_text.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        activity_text.set_max_width_chars(48);
        activity.append(&activity_text);
        activity.set_visible(false);
        root.append(&activity);
        let usage_slot = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        root.append(&usage_slot);
        let subagent_slot = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        root.append(&subagent_slot);

        let gauge = gtk4::Button::new();
        gauge.add_css_class("flat");
        gauge.add_css_class("gauge");
        let gauge_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let level = gtk4::LevelBar::new();
        level.set_min_value(0.0);
        level.set_max_value(1.0);
        level.set_width_request(96);
        level.set_valign(gtk4::Align::Center);
        level.add_css_class("context-level");
        let gauge_label = label("", &["gauge-text"]);
        gauge_box.append(&level);
        gauge_box.append(&gauge_label);
        gauge.set_child(Some(&gauge_box));
        gauge.set_visible(false);
        // In a slot of its own: `set_gauge` shows and hides the gauge itself.
        let gauge_slot = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        gauge_slot.append(&gauge);
        root.append(&gauge_slot);

        let bin = adw::BreakpointBin::new();
        bin.set_child(Some(&root));
        // With breakpoints, the bin's size request is its minimum, in both directions. The
        // width is fixed; the height follows the strip, whose items change (the usage meter is
        // two lines once it has data): see `fit_height`.
        bin.set_width_request(MIN_WIDTH);
        // A breakpoint can bring an item back that is taller than what was measured. Not from
        // inside the allocation that switched it.
        bin.connect_current_breakpoint_notify({
            let (bin, root) = (bin.downgrade(), root.downgrade());
            move |_| {
                let (bin, root) = (bin.clone(), root.clone());
                gtk4::glib::idle_add_local_once(move || {
                    if let (Some(bin), Some(root)) = (bin.upgrade(), root.upgrade()) {
                        fit_height(&bin, &root);
                    }
                });
            }
        });
        let breakpoint = |below: f64| {
            let b = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
                adw::BreakpointConditionLengthType::MaxWidth,
                below,
                adw::LengthUnit::Sp,
            ));
            bin.add_breakpoint(b.clone());
            b
        };
        let hidden = false.to_value();
        // Added widest first: where several match, the last added (the narrowest) applies, so
        // each repeats the ones before it.
        let usage = breakpoint(HIDE_USAGE_BELOW);
        usage.add_setter(&usage_slot, "visible", Some(&hidden));
        let gauge_off = breakpoint(HIDE_GAUGE_BELOW);
        for slot in [
            usage_slot.upcast_ref::<gtk4::Widget>(),
            gauge_slot.upcast_ref(),
        ] {
            gauge_off.add_setter(slot, "visible", Some(&hidden));
        }
        let compact = breakpoint(COMPACT_BELOW);
        for slot in [
            usage_slot.upcast_ref::<gtk4::Widget>(),
            gauge_slot.upcast_ref(),
            subagent_slot.upcast_ref(),
            model.upcast_ref(),
        ] {
            compact.add_setter(slot, "visible", Some(&hidden));
        }
        compact.add_setter(&root, "spacing", Some(&6.to_value()));
        // A class, not a `css-classes` setter: that would replace GTK's own (`horizontal`).
        compact.connect_apply({
            let root = root.downgrade();
            move |_| {
                if let Some(root) = root.upgrade() {
                    root.add_css_class("compact");
                }
            }
        });
        compact.connect_unapply({
            let root = root.downgrade();
            move |_| {
                if let Some(root) = root.upgrade() {
                    root.remove_css_class("compact");
                }
            }
        });

        fit_height(&bin, &root);
        Self {
            bin,
            root,
            chip,
            brand,
            agent,
            model,
            mode,
            mode_guard: Cell::new(false),
            reload,
            activity,
            usage_slot,
            subagent_slot,
            activity_text,
            spinner,
            shown: RefCell::new(Activity::Idle),
            gauge,
            level,
            gauge_label,
            last_driver: Cell::new(None),
        }
    }

    /// Puts the plan-usage indicator after the activity (dropped first as the strip narrows).
    pub fn set_usage(&self, widget: &impl IsA<gtk4::Widget>) {
        while let Some(old) = self.usage_slot.first_child() {
            self.usage_slot.remove(&old);
        }
        self.usage_slot.append(widget);
        self.fit_height();
    }

    /// Puts the sub-agents button before the context gauge.
    pub fn set_subagents(&self, widget: &impl IsA<gtk4::Widget>) {
        self.subagent_slot.append(widget);
        self.fit_height();
    }

    pub fn set_agent(&self, driver: Driver, model: Option<&str>) {
        if let Some(prev) = self.last_driver.get() {
            self.chip.remove_css_class(accent_class(prev));
        }
        self.chip.add_css_class(accent_class(driver));
        self.last_driver.set(Some(driver));
        self.brand
            .set_from_gicon(&crate::icons::driver_icon(driver));
        self.agent.set_text(driver_name(driver));
        self.model.set_text(model.unwrap_or("default model"));
        self.fit_height();
    }

    pub fn set_mode(&self, mode: Mode) {
        self.mode_guard.set(true);
        self.mode.set_selected(mode_index(mode));
        self.mode_guard.set(false);
    }

    /// The session control stops an active turn, or reloads an idle session so its next agent
    /// process picks up current launch configuration.
    pub fn set_reload_running(&self, running: bool) {
        let (icon, tooltip) = if running {
            ("at-media-playback-stop-symbolic", "Stop current turn")
        } else {
            ("at-view-refresh-symbolic", "Reload session")
        };
        self.reload.set_icon_name(icon);
        self.reload.set_tooltip_text(Some(tooltip));
    }

    /// Shows what the thread is doing: the main agent working, only background work left (its
    /// spinner in the background colour), finished, or nothing.
    pub fn set_activity(&self, activity: &Activity) {
        if *self.shown.borrow() == *activity {
            return;
        }
        let text = activity.text();
        self.activity.set_visible(text.is_some());
        let text = text.unwrap_or_default();
        self.activity_text.set_text(&text);
        self.activity
            .set_tooltip_text((!text.is_empty()).then_some(text.as_str()));
        let busy = activity.busy();
        self.spinner.set_visible(busy);
        self.spinner.set_spinning(busy);
        let waiting = matches!(activity, Activity::Waiting { .. });
        let finished = matches!(activity, Activity::Finished);
        for (class, on) in [
            ("activity-background", waiting),
            ("activity-finished", finished),
        ] {
            if on {
                self.activity.add_css_class(class);
            } else {
                self.activity.remove_css_class(class);
            }
        }
        *self.shown.borrow_mut() = activity.clone();
        self.fit_height();
    }

    /// Whether the header says the main agent is working (tests).
    #[cfg(test)]
    pub fn running_shown(&self) -> bool {
        matches!(*self.shown.borrow(), Activity::Working { .. }) && self.activity.is_visible()
    }

    /// The status text the header shows, if any (tests).
    #[cfg(test)]
    pub fn activity_shown(&self) -> Option<String> {
        self.activity
            .is_visible()
            .then(|| self.activity_text.text().to_string())
    }

    pub fn set_gauge(&self, gauge: Option<&Gauge>) {
        self.show_gauge(gauge);
        self.fit_height();
    }

    fn show_gauge(&self, gauge: Option<&Gauge>) {
        let Some(g) = gauge else {
            self.gauge.set_visible(false);
            return;
        };
        self.gauge.set_visible(true);
        self.gauge_label.set_text(&gauge_text(g));
        match gauge_fraction(g) {
            Some((fill, compact)) => {
                self.level.set_visible(true);
                self.level.set_value(fill);
                // Past the auto-compact threshold the bar turns amber; near the end, red.
                self.level
                    .remove_offset_value(Some(gtk4::LEVEL_BAR_OFFSET_LOW));
                self.level
                    .remove_offset_value(Some(gtk4::LEVEL_BAR_OFFSET_HIGH));
                self.level
                    .remove_offset_value(Some(gtk4::LEVEL_BAR_OFFSET_FULL));
                let warn = compact.unwrap_or(0.8);
                self.level.add_offset_value("ok", warn);
                self.level.add_offset_value("warn", 0.95_f64.max(warn));
                self.level.add_offset_value("hot", 1.0);
                let tip = match g.auto_compact_at {
                    Some(at) => format!(
                        "Context window: {} used. Auto-compacts at {}.",
                        gauge_text(g),
                        format_tokens(at)
                    ),
                    None => format!("Context window: {} used.", gauge_text(g)),
                };
                self.gauge.set_tooltip_text(Some(&tip));
            }
            None => self.level.set_visible(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_array_is_complete_and_indexed_both_ways() {
        // Exhaustive: adding a Mode fails to compile until MODES and mode_index are updated.
        for m in [Mode::Ask, Mode::AcceptEdits, Mode::Plan] {
            match m {
                Mode::Ask | Mode::AcceptEdits | Mode::Plan => {}
            }
            assert_eq!(MODES[mode_index(m) as usize], m);
        }
        assert_eq!(MODES.len(), 3);
    }

    #[test]
    fn gauge_maths() {
        let g = Gauge {
            used: 50_000,
            max: Some(200_000),
            auto_compact_at: Some(160_000),
        };
        assert_eq!(gauge_fraction(&g), Some((0.25, Some(0.8))));
        assert_eq!(gauge_text(&g), "50k / 200k");
        let unknown = Gauge {
            used: 1_234,
            max: None,
            auto_compact_at: None,
        };
        assert_eq!(gauge_fraction(&unknown), None);
        assert_eq!(gauge_text(&unknown), "1.2k tokens");
        let over = Gauge {
            used: 300,
            max: Some(200),
            auto_compact_at: None,
        };
        assert_eq!(gauge_fraction(&over), Some((1.0, None)));
    }
}
