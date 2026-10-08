//! What the window shows while it starts: the animated app icon and name first, then, if the
//! wait goes on, a skeleton of the UI that is coming.
//!
//! The skeletons copy the real layout's geometry (the split view's sidebar bounds, the sidebar's
//! search row and thread rows, the thread header, the 860 px transcript column, the composer),
//! so when the real widgets land they fill the shapes the user is already looking at instead
//! of snapping in. Every animation is CSS, so it stops when GTK animations are switched off.

use adw::prelude::*;
use gtk4::{gdk, glib, Align, Box, Label, Orientation};
use std::cell::Cell;

const STYLE: &str = include_str!("loading.css");

/// How long the splash is shown before a startup that is still waiting moves on to the skeleton:
/// long enough for its intro (`loading.css`) to play out.
pub(super) const SKELETON_AFTER_MS: u64 = 2200;
/// The splash always plays this long (its intro, `loading.css`, settles by then), even when the
/// window is ready sooner; the `skip_load_animation` setting drops it.
pub(super) const SPLASH_MIN_MS: u64 = 2000;
/// Every loading-to-loaded swap crossfades for this long.
pub(super) const CROSSFADE_MS: u32 = 220;

/// The app icon's size on the splash. Its cursor dots are laid over it in its own units.
const ICON_PX: i32 = 128;
/// The icon's three cursor dots (`assets/ca.nuvek.AgentTerminal.svg`): centre x, and the
/// agent accent each one is drawn in. All sit at y = 66 with r = 5.5 on the 128 px canvas;
/// `icons::tests` fails if the art moves them.
const DOTS: [(i32, &str); 3] = [(64, "claude"), (80, "agy"), (96, "codex")];
const DOT_CY: i32 = 66;
const DOT_PX: i32 = 12;
// The dots are laid out by their top-left corner, so they must be even-sized to centre exactly
// on the art's, and they must fit inside the icon.
const _: () = {
    assert!(DOT_PX % 2 == 0);
    assert!(DOT_CY - DOT_PX / 2 >= 0 && DOT_CY + DOT_PX / 2 <= ICON_PX);
    assert!(DOTS[0].0 - DOT_PX / 2 >= 0 && DOTS[2].0 + DOT_PX / 2 <= ICON_PX);
};

thread_local! {
    static STYLE_LOADED: Cell<bool> = const { Cell::new(false) };
}

/// Loads the stylesheet once per process (above the brand CSS, like the chat view's).
fn load_style() {
    if STYLE_LOADED.with(Cell::get) {
        return;
    }
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let provider = gtk4::CssProvider::new();
    provider.load_from_data(STYLE);
    gtk4::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
    STYLE_LOADED.with(|l| l.set(true));
}

/// The splash: the app icon bubbling up, its cursor dots popping in and bouncing, the app's
/// name folding down beneath it, and `status` (what it is waiting for), which fades in only once
/// the wait is long enough to read it. The choreography is in `loading.css`.
pub(super) fn splash(status: &str) -> gtk4::Widget {
    load_style();
    let root = Box::builder()
        .orientation(Orientation::Vertical)
        .valign(Align::Center)
        .halign(Align::Center)
        .vexpand(true)
        .spacing(14)
        .css_classes(["splash"])
        .build();

    let icon = gtk4::Overlay::builder()
        .halign(Align::Center)
        .css_classes(["splash-icon"])
        .build();
    let art = gtk4::Image::builder().pixel_size(ICON_PX).build();
    art.set_paintable(Some(&crate::icons::hero_paintable(
        crate::icons::APP_ART_BARE,
        ICON_PX,
        &art,
    )));
    icon.set_child(Some(&art));
    for (cx, agent) in DOTS {
        let dot = Box::builder()
            .halign(Align::Start)
            .valign(Align::Start)
            .margin_start(cx - DOT_PX / 2)
            .margin_top(DOT_CY - DOT_PX / 2)
            .width_request(DOT_PX)
            .height_request(DOT_PX)
            .css_classes(["splash-dot", &format!("dot-{agent}")])
            .can_target(false)
            .build();
        icon.add_overlay(&dot);
    }
    root.append(&icon);

    root.append(
        &Label::builder()
            .label("Agent Terminal")
            .css_classes(["splash-name"])
            .build(),
    );
    root.append(
        &Label::builder()
            .label(status)
            .css_classes(["splash-status"])
            .build(),
    );
    clipped(&root)
}

/// `child` in a frame that asks for no size of its own: a placeholder must never be what holds
/// the window open (or pushes the header out of it). In a window smaller than the placeholder's
/// layout, it is clipped. It never scrolls: it is not interactive.
fn clipped(child: &impl IsA<gtk4::Widget>) -> gtk4::Widget {
    gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::External)
        .vscrollbar_policy(gtk4::PolicyType::External)
        .hexpand(true)
        .vexpand(true)
        .can_target(false)
        .child(child)
        .build()
        .upcast()
}

/// One skeleton block. `delay` (0–3) staggers its pulse, so the blocks shimmer as a wave down
/// the page rather than blinking together.
fn bone(width: i32, height: i32, delay: u8) -> Box {
    Box::builder()
        .width_request(width)
        .height_request(height)
        .valign(Align::Center)
        .css_classes(["bone", &format!("bone-d{}", delay.min(3))])
        .build()
}

/// A line of text: fills the row, short of its end by `short_by` px, so it reads as a ragged
/// paragraph at any width instead of a fixed bar that overflows a narrow window.
fn text_line(height: i32, short_by: i32, delay: u8) -> Box {
    let line = bone(-1, height, delay);
    line.set_hexpand(true);
    line.set_margin_end(short_by);
    line
}

/// The sidebar's thread list while it loads: folder headings with thread rows under them, in
/// the real rows' paddings (dot, title, age). Used as the list's placeholder.
pub(super) fn sidebar_rows() -> gtk4::Widget {
    load_style();
    let root = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(2)
        .margin_start(6)
        .margin_end(6)
        .css_classes(["skeleton"])
        .build();
    // (folder heading width, thread title widths)
    let groups: [(i32, &[i32]); 2] = [(96, &[132, 104, 150]), (120, &[118, 86])];
    let mut delay = 0u8;
    for (folder, titles) in groups {
        let heading = bone(folder, 9, delay % 4);
        heading.set_halign(Align::Start);
        heading.set_margin_top(14);
        heading.set_margin_bottom(6);
        heading.set_margin_start(12);
        root.append(&heading);
        for &title in titles {
            delay += 1;
            let row = Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(8)
                .margin_start(10)
                .margin_end(10)
                .height_request(34)
                .build();
            let dot = bone(14, 14, delay % 4);
            dot.add_css_class("bone-round");
            row.append(&dot);
            let text = bone(title, 11, delay % 4);
            text.set_hexpand(true);
            text.set_halign(Align::Start);
            row.append(&text);
            row.append(&bone(22, 9, delay % 4));
            root.append(&row);
        }
    }
    root.upcast()
}

/// A thread page while its history is read and its view built: the thread header (agent and
/// mode pills, status and context gauge), a few turns in the transcript column, the composer.
pub(super) fn chat() -> gtk4::Widget {
    load_style();
    let root = Box::builder()
        .orientation(Orientation::Vertical)
        .vexpand(true)
        .hexpand(true)
        .css_classes(["skeleton", "skeleton-chat"])
        .build();

    let header = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .css_classes(["skeleton-thread-header"])
        .build();
    let agent = bone(176, 28, 0);
    agent.add_css_class("bone-pill");
    let mode = bone(116, 28, 0);
    mode.add_css_class("bone-pill");
    header.append(&agent);
    header.append(&mode);
    let spacer = Box::builder().hexpand(true).build();
    header.append(&spacer);
    header.append(&bone(54, 9, 1));
    let gauge = bone(140, 6, 1);
    gauge.add_css_class("bone-pill");
    header.append(&gauge);
    root.append(&header);

    let column = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(10)
        .margin_top(18)
        .margin_start(24)
        .margin_end(24)
        .build();
    // A user's message (right), the agent's answer (ragged lines and a tool card), and the
    // next message.
    column.append(&user_bubble(240, 0));
    for (short_by, delay) in [(40, 1), (110, 1), (70, 2), (260, 2)] {
        column.append(&text_line(11, short_by, delay));
    }
    let card = text_line(64, 0, 2);
    card.add_css_class("bone-card");
    card.set_margin_top(6);
    card.set_margin_bottom(6);
    column.append(&card);
    for (short_by, delay) in [(90, 3), (180, 3)] {
        column.append(&text_line(11, short_by, delay));
    }
    column.append(&user_bubble(180, 3));
    root.append(&clamped(&column, true));

    let composer = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .margin_start(24)
        .margin_end(24)
        .margin_top(8)
        .margin_bottom(16)
        .css_classes(["bone", "bone-d0", "bone-composer"])
        .height_request(56)
        .build();
    root.append(&clamped(&composer, false));
    clipped(&root)
}

fn user_bubble(width: i32, delay: u8) -> Box {
    let bubble = bone(width, 34, delay);
    bubble.add_css_class("bone-bubble");
    bubble.set_halign(Align::End);
    bubble.set_margin_top(4);
    bubble.set_margin_bottom(8);
    bubble
}

/// In the transcript's column: the same clamp as the chat view's.
fn clamped(child: &impl IsA<gtk4::Widget>, vexpand: bool) -> adw::Clamp {
    adw::Clamp::builder()
        .maximum_size(860)
        .tightening_threshold(640)
        .vexpand(vexpand)
        .valign(if vexpand { Align::Start } else { Align::End })
        .child(child)
        .build()
}

/// The whole shell while startup is still finding the agents: the sidebar (if the user keeps it
/// shown) beside a thread page, in a split view with the real one's bounds.
pub(super) fn shell(show_sidebar: bool) -> gtk4::Widget {
    load_style();
    let split = adw::OverlaySplitView::builder()
        .vexpand(true)
        .min_sidebar_width(240.0)
        .max_sidebar_width(340.0)
        .show_sidebar(show_sidebar)
        .build();

    let sidebar = Box::builder()
        .orientation(Orientation::Vertical)
        .css_classes(["thread-sidebar", "skeleton"])
        .build();
    let search = text_line(34, 0, 0);
    search.add_css_class("bone-field");
    search.set_margin_top(8);
    search.set_margin_bottom(6);
    search.set_margin_start(8);
    search.set_margin_end(8);
    sidebar.append(&search);
    let rows = sidebar_rows();
    rows.set_vexpand(true);
    sidebar.append(&rows);
    let footer = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(8)
        .css_classes(["sidebar-footer"])
        .build();
    let archived = bone(96, 10, 2);
    archived.set_halign(Align::Start);
    archived.set_margin_top(10);
    archived.set_margin_start(14);
    footer.append(&archived);
    let usage = text_line(22, 0, 3);
    usage.set_margin_start(12);
    usage.set_margin_end(12);
    usage.set_margin_bottom(10);
    footer.append(&usage);
    sidebar.append(&footer);

    split.set_sidebar(Some(&sidebar));
    split.set_content(Some(&chat()));
    clipped(&split)
}

/// Stops a skeleton's pulse (it is not loading any more, say a read that failed) without
/// moving anything.
pub(super) fn settle(skeleton: &gtk4::Widget) {
    skeleton.add_css_class("skeleton-idle");
}

/// Undoes [`settle`]: loading again.
pub(super) fn unsettle(skeleton: &gtk4::Widget) {
    skeleton.remove_css_class("skeleton-idle");
}

/// Fades a widget in as it replaces a skeleton.
pub(super) fn reveal(widget: &impl IsA<gtk4::Widget>) {
    load_style();
    widget.add_css_class("loaded-in");
}

/// `agent-terminal --loading-demo`: the startup sequence of a launch that never finishes
/// detecting, on a loop (splash, then the shell skeleton), for screenshots and review.
pub(crate) fn demo() -> glib::ExitCode {
    const LOOP_MS: u64 = 7000;
    let app = adw::Application::builder()
        .application_id("ca.nuvek.AgentTerminal.LoadingDemo")
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(|app| {
        crate::icons::register();
        crate::load_css();
        let stack = gtk4::Stack::builder()
            .transition_type(gtk4::StackTransitionType::Crossfade)
            .transition_duration(CROSSFADE_MS)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&stack));
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Agent Terminal")
            .default_width(950)
            .default_height(650)
            .content(&toolbar)
            .build();
        window.present();

        // One run of the sequence; fresh widgets each time, so the entrance animations replay.
        let play = move |stack: &gtk4::Stack| {
            while let Some(child) = stack.first_child() {
                stack.remove(&child);
            }
            stack.add_named(&splash("Looking for an AI CLI…"), Some("splash"));
            glib::timeout_add_local_once(
                std::time::Duration::from_millis(SKELETON_AFTER_MS),
                glib::clone!(
                    #[weak]
                    stack,
                    move || {
                        stack.add_named(&shell(true), Some("skeleton"));
                        stack.set_visible_child_name("skeleton");
                    }
                ),
            );
        };
        play(&stack);
        glib::timeout_add_local(
            std::time::Duration::from_millis(LOOP_MS),
            glib::clone!(
                #[weak]
                stack,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    play(&stack);
                    glib::ControlFlow::Continue
                }
            ),
        );
    });
    // GApplication must not see `--loading-demo` (it would reject an unknown option).
    let argv0 = std::env::args()
        .next()
        .unwrap_or_else(|| "agent-terminal".into());
    app.run_with_args(&[argv0])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every class the builders give a bone has a rule (a renamed class would leave a bare,
    /// invisible box).
    #[test]
    fn every_bone_class_is_styled() {
        for class in [
            ".bone ",
            ".bone-d1",
            ".bone-d2",
            ".bone-d3",
            ".bone-round",
            ".bone-pill",
            ".bone-card",
            ".bone-bubble",
            ".bone-field",
            ".bone-composer",
            ".skeleton-idle",
            ".skeleton-thread-header",
            ".splash-icon",
            ".splash-dot",
            ".dot-claude",
            ".dot-agy",
            ".dot-codex",
            ".splash-name",
            ".splash-status",
            ".loaded-in",
        ] {
            assert!(STYLE.contains(class), "no rule for {class}");
        }
    }
}
