//! The strip at the top of the chat view: agent + model chip, mode dropdown, the thread's
//! activity (its turn and its background work) and the context gauge.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use agent_core::adapter::{Driver, Mode};

use super::cards::{accent_class, driver_name};
use super::model::{format_tokens, Activity, Gauge};

/// Hookless agy cannot enforce a new policy mid-turn without interrupting it.
pub fn confirm_mode_switch(parent: &impl IsA<gtk4::Widget>, then: impl Fn() + 'static) {
    let dialog = adw::AlertDialog::new(
        Some("Change Antigravity mode now?"),
        Some(
            "Antigravity is still working without its approval hook. Changing mode interrupts \
              this turn and continues the unfinished task in the selected mode. Any approval \
              or question waiting for your answer is cancelled; the new turn can ask again.",
        ),
    );
    dialog.add_responses(&[
        ("cancel", "Keep Waiting"),
        ("switch", "Switch and Continue"),
    ]);
    dialog.set_response_appearance("switch", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.connect_response(None, move |_, response| {
        if response == "switch" {
            then();
        }
    });
    dialog.present(Some(parent));
}

fn label(text: &str, classes: &[&str]) -> gtk4::Label {
    let l = super::cards::label(text, classes);
    l.set_selectable(false);
    l
}

/// Dropdown order of the modes. Driven by index both ways; the guard test keeps it complete.
/// The dangerous one is last, below a divider ([`mode_factory`]).
pub const MODES: [Mode; 4] = [Mode::Ask, Mode::AcceptEdits, Mode::Plan, Mode::Bypass];

pub fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Ask => "Ask before edits",
        Mode::AcceptEdits => "Accept edits",
        Mode::Plan => "Plan",
        Mode::Bypass => "Dangerous permissive",
    }
}

/// The names the narrow header uses, in [`MODES`] order.
const SHORT_NAMES: [&str; 4] = ["Ask", "Accept", "Plan", "Danger"];

pub fn mode_index(mode: Mode) -> u32 {
    match mode {
        Mode::Ask => 0,
        Mode::AcceptEdits => 1,
        Mode::Plan => 2,
        Mode::Bypass => 3,
    }
}

/// Draws the mode names: the dangerous mode with a warning icon in danger colours and, in the
/// popup's list (`list`), a divider above it.
fn mode_factory(list: bool) -> gtk4::SignalListItemFactory {
    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup(move |_, obj| {
        let Some(item) = obj.downcast_ref::<gtk4::ListItem>() else {
            return;
        };
        let column = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        let divider = gtk4::Separator::new(gtk4::Orientation::Horizontal);
        divider.add_css_class("mode-divider");
        divider.set_visible(false);
        column.append(&divider);
        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let icon = gtk4::Image::from_icon_name("at-dialog-warning-symbolic");
        icon.set_visible(false);
        row.append(&icon);
        let text = label("", &[]);
        text.set_hexpand(true);
        text.set_xalign(0.0);
        row.append(&text);
        // GTK's own list marks the current mode; a custom factory has to do it itself.
        let check = gtk4::Image::from_icon_name("at-object-select-symbolic");
        check.set_visible(false);
        if list {
            item.property_expression("selected")
                .bind(&check, "visible", gtk4::Widget::NONE);
        }
        row.append(&check);
        column.append(&row);
        item.set_child(Some(&column));
    });
    factory.connect_bind(move |_, obj| {
        let Some(item) = obj.downcast_ref::<gtk4::ListItem>() else {
            return;
        };
        let Some(column) = item.child() else { return };
        let (Some(divider), Some(row)) = (column.first_child(), column.last_child()) else {
            return;
        };
        let (Some(icon), Some(text)) = (
            row.first_child(),
            row.first_child().and_then(|i| i.next_sibling()),
        ) else {
            return;
        };
        let Some(text) = text.downcast_ref::<gtk4::Label>() else {
            return;
        };
        let name = item
            .item()
            .and_downcast::<gtk4::StringObject>()
            .map(|s| s.string());
        text.set_text(name.as_deref().unwrap_or(""));
        let danger = item.position() == mode_index(Mode::Bypass);
        divider.set_visible(list && danger);
        icon.set_visible(danger);
        if danger {
            row.add_css_class("mode-danger-row");
        } else {
            row.remove_css_class("mode-danger-row");
        }
    });
    factory
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
/// fits the thread at any window size: the activity's words, then the usage meter and the context
/// gauge, then the sub-agents button and the model's name, with tighter spacing.
// With explicit active/history counts the complete strip measures 956 px.
// Drop the activity's words before that width; the usage meter stays until the gauge goes.
const HIDE_ACTIVITY_WORDS_BELOW: f64 = 976.0;
// The complete strip measures 741 px with a thread sidebar open. Hide the gauge before the
// content pane reaches that size so its parent never has to expand beyond the window.
const HIDE_GAUGE_BELOW: f64 = 760.0;
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
    pending_mode: gtk4::Label,
    /// Set while the view changes the dropdown itself, so that is not taken as a user choice.
    pub mode_guard: Rc<Cell<bool>>,
    breakpoints: [adw::Breakpoint; 3],
    /// The "Extra access" chip: shown while the user has granted Codex session-long access.
    access: gtk4::MenuButton,
    access_list: gtk4::Label,
    shown_access: RefCell<Vec<String>>,
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

/// The effort the chip adds after the model, if any: not when the model's id already ends with
/// it (agy composes `<base>-<effort>`), nor when there is none.
fn chip_effort<'a>(model_text: &str, effort: Option<&'a str>) -> Option<&'a str> {
    let effort = effort.filter(|e| !e.is_empty())?;
    let base = model_text.trim_end_matches(" (pending)");
    (!base.ends_with(&format!("-{effort}"))).then_some(effort)
}

fn set_mode_names(mode: &gtk4::DropDown, guard: &Cell<bool>, names: &[&str]) {
    let selected = mode.selected();
    guard.set(true);
    mode.set_model(Some(&gtk4::StringList::new(names)));
    mode.set_selected(selected);
    guard.set(false);
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
        mode.set_factory(Some(&mode_factory(false)));
        mode.set_list_factory(Some(&mode_factory(true)));
        let mode_guard = Rc::new(Cell::new(false));
        mode.add_css_class("mode-dropdown");
        mode.set_tooltip_text(Some("Interaction mode (/mode)"));
        let mode_box = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        mode_box.set_valign(gtk4::Align::Center);
        mode_box.append(&mode);
        let pending_mode = label("", &["dim-label"]);
        pending_mode.set_wrap(true);
        pending_mode.set_max_width_chars(18);
        pending_mode.set_visible(false);
        mode_box.append(&pending_mode);
        root.append(&mode_box);

        let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        root.append(&spacer);

        // What the user allowed Codex beyond its usual limits for this session. A menu button,
        // so the list is one keypress away, not only in a tooltip.
        let access = gtk4::MenuButton::new();
        access.add_css_class("flat");
        access.add_css_class("extra-access");
        access.set_valign(gtk4::Align::Center);
        let access_inner = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        access_inner.append(&gtk4::Image::from_icon_name("at-security-medium-symbolic"));
        let access_text = label("Extra access", &["extra-access-text"]);
        access_inner.append(&access_text);
        access.set_child(Some(&access_inner));
        let access_list = label("", &["extra-access-list"]);
        access_list.set_wrap(true);
        access_list.set_max_width_chars(48);
        access_list.set_selectable(false);
        let access_pop_box = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        access_pop_box.set_margin_top(8);
        access_pop_box.set_margin_bottom(8);
        access_pop_box.set_margin_start(10);
        access_pop_box.set_margin_end(10);
        access_pop_box.append(&label(
            "Allowed for this Codex session",
            &["extra-access-title"],
        ));
        access_pop_box.append(&access_list);
        access_pop_box.append(&label(
            "This lasts until this thread's Codex session restarts.",
            &["dim-label"],
        ));
        let access_pop = gtk4::Popover::new();
        access_pop.set_child(Some(&access_pop_box));
        access.set_popover(Some(&access_pop));
        access.set_visible(false);
        root.append(&access);

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
        // The activity's words go before the usage meter: the spinner and its tooltip still say
        // what the thread is doing, and the meter is the one thing here with no stand-in.
        let words_off = breakpoint(HIDE_ACTIVITY_WORDS_BELOW);
        words_off.add_setter(&activity_text, "visible", Some(&hidden));
        let gauge_off = breakpoint(HIDE_GAUGE_BELOW);
        for slot in [
            usage_slot.upcast_ref::<gtk4::Widget>(),
            gauge_slot.upcast_ref(),
            activity_text.upcast_ref(),
        ] {
            gauge_off.add_setter(slot, "visible", Some(&hidden));
        }
        let compact = breakpoint(COMPACT_BELOW);
        for slot in [
            usage_slot.upcast_ref::<gtk4::Widget>(),
            gauge_slot.upcast_ref(),
            subagent_slot.upcast_ref(),
            model.upcast_ref(),
            agent.upcast_ref(),
            // The spinner stays; the words are in the activity's tooltip and the shelf.
            activity_text.upcast_ref(),
            // The shield stays, with the list in its menu and its accessible name.
            access_text.upcast_ref(),
        ] {
            compact.add_setter(slot, "visible", Some(&hidden));
        }
        compact.add_setter(&root, "spacing", Some(&6.to_value()));
        // A class, not a `css-classes` setter: that would replace GTK's own (`horizontal`).
        compact.connect_apply({
            let root = root.downgrade();
            let mode = mode.downgrade();
            let guard = mode_guard.clone();
            move |_| {
                if let Some(root) = root.upgrade() {
                    root.add_css_class("compact");
                }
                if let Some(mode) = mode.upgrade() {
                    set_mode_names(&mode, &guard, &SHORT_NAMES);
                }
            }
        });
        compact.connect_unapply({
            let root = root.downgrade();
            let mode = mode.downgrade();
            let guard = mode_guard.clone();
            move |_| {
                if let Some(root) = root.upgrade() {
                    root.remove_css_class("compact");
                }
                if let Some(mode) = mode.upgrade() {
                    let names: Vec<_> = MODES.iter().map(|m| mode_label(*m)).collect();
                    set_mode_names(&mode, &guard, &names);
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
            pending_mode,
            mode_guard,
            breakpoints: [words_off, gauge_off, compact],
            access,
            access_list,
            shown_access: RefCell::new(Vec::new()),
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

    pub fn set_agent(
        &self,
        driver: Driver,
        model: Option<&str>,
        pending: Option<&str>,
        effort: Option<&str>,
    ) {
        if let Some(prev) = self.last_driver.get() {
            self.chip.remove_css_class(accent_class(prev));
        }
        self.chip.add_css_class(accent_class(driver));
        self.last_driver.set(Some(driver));
        self.brand
            .set_from_gicon(&crate::icons::driver_icon(driver));
        self.agent.set_text(driver_name(driver));
        let mut model_text = match pending {
            Some(target) => match model {
                Some(previous) => format!("{previous} → {target} (pending)"),
                None => format!("{target} (pending)"),
            },
            None => match model {
                Some(m) => m.to_owned(),
                None => crate::config::agent_settings_model(driver, None)
                    .unwrap_or_else(|| "default model".to_owned()),
            },
        };
        if let Some(effort) = chip_effort(&model_text, effort) {
            model_text.push_str(" · ");
            model_text.push_str(effort);
        }
        self.model.set_text(&model_text);
        // The class starts the chip's pulse; adding it again while pending does not restart it.
        if pending.is_some() {
            self.chip.add_css_class("pending");
        } else {
            self.chip.remove_css_class("pending");
        }
        let name = format!("{} · {}", driver_name(driver), model_text);
        self.chip
            .update_property(&[gtk4::accessible::Property::Label(&name)]);
        self.chip
            .set_tooltip_text(Some(&format!("{name} · Switch model or agent (/model)")));
        self.fit_height();
    }

    pub fn set_mode(&self, mode: Mode) {
        self.mode_guard.set(true);
        self.mode.set_selected(mode_index(mode));
        self.mode_guard.set(false);
        if mode == Mode::Bypass {
            self.mode.add_css_class("mode-danger");
        } else {
            self.mode.remove_css_class("mode-danger");
        }
        self.mode
            .update_property(&[gtk4::accessible::Property::Label(mode_label(mode))]);
    }

    pub fn set_text_scale(&self, scale: f64) {
        for (breakpoint, width) in self.breakpoints.iter().zip([
            HIDE_ACTIVITY_WORDS_BELOW,
            HIDE_GAUGE_BELOW,
            COMPACT_BELOW,
        ]) {
            breakpoint.set_condition(Some(&adw::BreakpointCondition::new_length(
                adw::BreakpointConditionLengthType::MaxWidth,
                width * scale,
                adw::LengthUnit::Sp,
            )));
        }
        self.fit_height();
    }

    pub fn set_pending_mode(&self, pending: Option<Mode>) {
        let hint = pending.map(|mode| format!("Next turn: {}", mode_label(mode)));
        let effective = MODES
            .get(self.mode.selected() as usize)
            .copied()
            .unwrap_or(Mode::Ask);
        let effective_hint = format!("{} (/mode)", mode_label(effective));
        self.pending_mode.set_text(hint.as_deref().unwrap_or(""));
        self.pending_mode.set_visible(pending.is_some());
        // The class starts its pulse, so it comes and goes with the hint.
        if pending.is_some() {
            self.pending_mode.add_css_class("pending-mode");
        } else {
            self.pending_mode.remove_css_class("pending-mode");
        }
        self.mode
            .set_tooltip_text(hint.as_deref().or(Some(&effective_hint)));
        self.mode
            .update_property(&[gtk4::accessible::Property::Description(
                hint.as_deref().unwrap_or("Effective interaction mode"),
            )]);
        self.fit_height();
    }

    /// Shows the "Extra access" chip while `grants` (one line each) is not empty, and lists
    /// them in its tooltip and menu. Cheap to call on every change.
    pub fn set_extra_access(&self, grants: &[String]) {
        if *self.shown_access.borrow() == grants {
            return;
        }
        *self.shown_access.borrow_mut() = grants.to_vec();
        let list = grants.join("\n");
        self.access_list.set_text(&list);
        self.access.set_visible(!grants.is_empty());
        let tip = format!("Allowed for this Codex session:\n{list}");
        self.access.set_tooltip_text(Some(&tip));
        self.access
            .update_property(&[gtk4::accessible::Property::Label(&format!(
                "Extra access for this Codex session: {}",
                grants.join(", ")
            ))]);
        self.fit_height();
    }

    /// The chip's list while it shows, else `None` (tests).
    #[cfg(test)]
    pub fn extra_access_shown(&self) -> Option<(String, String)> {
        self.access.is_visible().then(|| {
            (
                self.access_list.text().to_string(),
                self.access.tooltip_text().unwrap_or_default().to_string(),
            )
        })
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
        let waiting = matches!(
            activity,
            Activity::Waiting { .. } | Activity::WaitingForWorkers { .. }
        );
        let finished = matches!(
            activity,
            Activity::Finished | Activity::Failed | Activity::Stopped
        );
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

    /// Whether the activity's words (not just its spinner) are showing (tests).
    #[cfg(test)]
    pub fn activity_words_shown(&self) -> bool {
        self.activity.is_visible() && self.activity_text.is_visible()
    }

    /// The right edge of the last visible item of the strip, and the width the strip has, in px
    /// (tests): the item is clipped by the window when the first exceeds the second.
    #[cfg(test)]
    pub fn right_edge(&self) -> (f32, f32) {
        let mut edge = 0f32;
        let mut child = self.root.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            if c.is_visible() && c.width() > 0 {
                if let Some(b) = c.compute_bounds(&self.root) {
                    edge = edge.max(b.x() + b.width());
                }
            }
        }
        (edge, self.bin.width() as f32)
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
        for m in [Mode::Ask, Mode::AcceptEdits, Mode::Plan, Mode::Bypass] {
            match m {
                Mode::Ask | Mode::AcceptEdits | Mode::Plan | Mode::Bypass => {}
            }
            assert_eq!(MODES[mode_index(m) as usize], m);
        }
        assert_eq!(MODES.len(), 4);
        assert_eq!(SHORT_NAMES.len(), MODES.len());
        // The divider sets the dangerous mode apart from the others: it must stay last.
        assert_eq!(MODES.last(), Some(&Mode::Bypass));
    }

    #[test]
    fn the_chip_names_an_effort_once() {
        assert_eq!(chip_effort("opus", Some("high")), Some("high"));
        assert_eq!(
            chip_effort("opus → sonnet (pending)", Some("low")),
            Some("low")
        );
        // agy's id already carries it.
        assert_eq!(chip_effort("gemini-3-pro-high", Some("high")), None);
        assert_eq!(
            chip_effort("gemini-3-pro-high (pending)", Some("high")),
            None
        );
        assert_eq!(chip_effort("opus", None), None);
        assert_eq!(chip_effort("opus", Some("")), None);
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
