//! The strip at the top of the chat view: agent + model chip, mode dropdown, turn activity and
//! the context gauge.

use std::cell::Cell;

use agent_core::adapter::{Driver, Mode};
use gtk4::prelude::*;

use super::cards::{accent_class, driver_name, label};
use super::model::{format_tokens, Gauge};

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

pub struct Header {
    pub root: gtk4::Box,
    pub chip: gtk4::Button,
    brand: gtk4::Image,
    agent: gtk4::Label,
    model: gtk4::Label,
    pub mode: gtk4::DropDown,
    /// Set while the view changes the dropdown itself, so that is not taken as a user choice.
    pub mode_guard: Cell<bool>,
    activity: gtk4::Box,
    spinner: gtk4::Spinner,
    pub gauge: gtk4::Button,
    level: gtk4::LevelBar,
    gauge_label: gtk4::Label,
    last_driver: Cell<Option<Driver>>,
}

impl Header {
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

        let activity = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        activity.add_css_class("activity");
        let spinner = gtk4::Spinner::new();
        activity.append(&spinner);
        activity.append(&label("Working…", &["activity-text"]));
        activity.set_visible(false);
        root.append(&activity);

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
        root.append(&gauge);

        Self {
            root,
            chip,
            brand,
            agent,
            model,
            mode,
            mode_guard: Cell::new(false),
            activity,
            spinner,
            gauge,
            level,
            gauge_label,
            last_driver: Cell::new(None),
        }
    }

    /// Puts the usage indicator just before the context gauge.
    pub fn insert_usage(&self, widget: &impl IsA<gtk4::Widget>) {
        self.root.insert_child_after(widget, Some(&self.activity));
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
    }

    pub fn set_mode(&self, mode: Mode) {
        self.mode_guard.set(true);
        self.mode.set_selected(mode_index(mode));
        self.mode_guard.set(false);
    }

    pub fn set_running(&self, running: bool) {
        self.activity.set_visible(running);
        self.spinner.set_spinning(running);
    }

    pub fn set_gauge(&self, gauge: Option<&Gauge>) {
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
