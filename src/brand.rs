//! The app's brand and motion: the agents' colours as the stylesheets name them, the shared
//! timings, and the one reduced-motion check.
//!
//! The agents' colours (the app icon's three cursor dots) come from agent-core's
//! `DriverInfo::accent_hex` and nowhere else: [`install`] defines them for every stylesheet as
//! `@brand_claude`, `@brand_agy` and `@brand_codex`, and Rust drawing reads [`rgb`]. They are
//! not themed: they say which agent it is, under every theme. A light window (only the desktop's
//! own theme can be one) gets them darkened just enough to stay legible ([`rgb_on_light`]).
//! [`dots`] is the icon's three dots as widgets, one of them lit by [`set_lead`] (or every agent
//! at work, by [`set_working`]).
//!
//! CSS transitions and animations, and Stack/Revealer transitions, stop by themselves when the
//! desktop turns animations off (`gtk-enable-animations`). Anything drawn on its own frame clock
//! (the closing screen, the menu button) does not, and must ask [`animations_enabled`].

use std::cell::RefCell;

use adw::prelude::*;
use agent_core::adapter::Driver;
use gtk4::gdk;

/// A small state change: a status icon replacing its spinner, a hover.
pub const FAST_MS: u32 = 150;
/// A page of a view giving way to another (splash to window, loading to content).
pub const CROSSFADE_MS: u32 = 220;

/// Whether the desktop wants animations (`gtk-enable-animations`). Without a display (tests)
/// it says yes, as GTK's own default does.
pub fn animations_enabled() -> bool {
    gtk4::Settings::default().is_none_or(|s| s.is_gtk_enable_animations())
}

/// The stylesheet name of `driver`'s colour, without the `@`: `brand_claude`.
pub fn colour_name(driver: Driver) -> String {
    format!("brand_{}", driver.info().key)
}

/// `driver`'s colour as cairo takes it, each channel 0.0..=1.0.
pub fn rgb(driver: Driver) -> (f64, f64, f64) {
    let hex = driver.info().accent_hex.trim_start_matches('#');
    let channel = |at: usize| {
        hex.get(at..at + 2)
            .and_then(|byte| u8::from_str_radix(byte, 16).ok())
            .map_or(0.0, |byte| f64::from(byte) / 255.0)
    };
    (channel(0), channel(2), channel(4))
}

/// The least contrast an agent's colour keeps against the window (WCAG's minimum for graphics).
const MIN_CONTRAST: f64 = 3.0;

/// WCAG relative luminance of an sRGB colour.
fn luminance((r, g, b): (f64, f64, f64)) -> f64 {
    let linear = |c: f64| {
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

/// WCAG contrast ratio of two colours, 1.0..=21.0.
fn contrast(a: (f64, f64, f64), b: (f64, f64, f64)) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// `driver`'s colour for a light window: the accent darkened just enough to keep
/// [`MIN_CONTRAST`] against white. The bright accents themselves fall to about 2.2-2.8:1 there.
/// Only the desktop's own theme (Settings → System) can be light; every bundled theme is dark.
pub fn rgb_on_light(driver: Driver) -> (f64, f64, f64) {
    let (r, g, b) = rgb(driver);
    let white = (1.0, 1.0, 1.0);
    (0..=100)
        .map(|step| 1.0 - f64::from(step) / 100.0)
        .map(|keep| (r * keep, g * keep, b * keep))
        .find(|&shade| contrast(shade, white) >= MIN_CONTRAST)
        .unwrap_or((0.0, 0.0, 0.0))
}

fn hex((r, g, b): (f64, f64, f64)) -> String {
    let byte = |c: f64| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", byte(r), byte(g), byte(b))
}

/// The stylesheet defining every agent's colour name: the accents themselves on a dark window,
/// their [`rgb_on_light`] shades on a light one.
pub fn css(dark: bool) -> String {
    Driver::ALL
        .iter()
        .map(|&driver| {
            let value = if dark {
                driver.info().accent_hex.to_owned()
            } else {
                hex(rgb_on_light(driver))
            };
            format!("@define-color {} {value};\n", colour_name(driver))
        })
        .collect()
}

thread_local! {
    static PROVIDER: RefCell<Option<gtk4::CssProvider>> = const { RefCell::new(None) };
}

/// Whether the window is dark. Without libadwaita running (some tests), it is: the app's own
/// themes all are.
fn window_is_dark() -> bool {
    !adw::is_initialized() || adw::StyleManager::default().is_dark()
}

/// Defines the agents' colour names for every stylesheet on the display, once per process, and
/// redefines them whenever the window turns light or dark. Every loader of a stylesheet naming
/// them calls it first, so no path draws one undefined.
pub fn install() {
    if PROVIDER.with(|p| p.borrow().is_some()) {
        return;
    }
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let provider = gtk4::CssProvider::new();
    provider.load_from_data(&css(window_is_dark()));
    gtk4::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    if adw::is_initialized() {
        adw::StyleManager::default().connect_dark_notify(|style| {
            PROVIDER.with(|p| {
                if let Some(provider) = p.borrow().as_ref() {
                    provider.load_from_data(&css(style.is_dark()));
                }
            });
        });
    }
    PROVIDER.with(|p| p.replace(Some(provider)));
}

/// The app icon's three cursor dots as widgets, one per agent in slot order (`Driver::ALL`):
/// each a `brand-dot` in its agent's colour. [`set_lead`] lights one, [`set_working`] several.
pub fn dots() -> gtk4::Box {
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 5);
    row.add_css_class("brand-dots");
    row.set_valign(gtk4::Align::Center);
    for driver in Driver::ALL {
        let dot = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        dot.add_css_class("brand-dot");
        dot.add_css_class(&format!("dot-{}", driver.info().key));
        dot.set_valign(gtk4::Align::Center);
        row.append(&dot);
    }
    row
}

/// The loading mark for one agent, in its colour (CSS `brand-drip`): one droplet, never two. It
/// goes once round a loop counter-clockwise to the top, hangs there, falls through the loop and
/// lands low in it as a flattened ripple, whose spread edge just meets the bottom of the mark;
/// once the ripple fades a new droplet forms at the top. The droplet that loops and the one that
/// falls are two widgets handing over at the top, never shown together. Decorative: the label
/// beside it says what is loading. With animations off the droplet rests at the top.
pub fn drip(driver: Driver) -> gtk4::Overlay {
    let dot_class = format!("dot-{}", driver.info().key);
    let drop = |extra: Option<&str>| {
        let drop = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        for class in ["brand-dot", dot_class.as_str(), "drip-drop"]
            .into_iter()
            .chain(extra)
        {
            drop.add_css_class(class);
        }
        drop.set_halign(gtk4::Align::Center);
        drop.set_valign(gtk4::Align::Start);
        drop
    };
    let overlay = gtk4::Overlay::builder()
        .accessible_role(gtk4::AccessibleRole::Presentation)
        .valign(gtk4::Align::Center)
        .build();
    overlay.add_css_class("brand-drip");
    // The droplet on its loop: it rides a box turned about its centre.
    let orbit = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    orbit.add_css_class("drip-orbit");
    orbit.set_halign(gtk4::Align::Center);
    orbit.set_valign(gtk4::Align::Start);
    orbit.append(&drop(None));
    overlay.add_overlay(&orbit);
    // The same droplet falling: it takes over at the top as the loop's one goes out.
    overlay.add_overlay(&drop(Some("drip-fall")));
    // The ripple where it lands: a ring in the agent's colour, squashed flat as water seen at an
    // angle. Centred here; the stylesheet lowers it.
    let ripple = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    ripple.add_css_class("drip-ripple");
    ripple.add_css_class(&dot_class);
    ripple.set_halign(gtk4::Align::Center);
    ripple.set_valign(gtk4::Align::Center);
    overlay.add_overlay(&ripple);
    overlay
}

/// Lights `lead`'s dot in a [`dots`] row and dims the other two; `None` shows all three alike.
/// The dots ease between the two (CSS), so a switch of agent reads as the light moving.
pub fn set_lead(dots: &gtk4::Box, lead: Option<Driver>) {
    let mut child = dots.first_child();
    for driver in Driver::ALL {
        let Some(dot) = child else { return };
        match lead {
            Some(lead) if lead == driver => {
                dot.remove_css_class("dimmed");
                dot.add_css_class("lead");
            }
            Some(_) => {
                dot.remove_css_class("lead");
                dot.add_css_class("dimmed");
            }
            None => {
                dot.remove_css_class("lead");
                dot.remove_css_class("dimmed");
            }
        }
        child = dot.next_sibling();
    }
}

/// Lights every agent at work in a [`dots`] row: `lead` (the thread's own agent) as
/// [`set_lead`] does, each other driver in `also` as `working`, and dims the rest. A driver in
/// both is the lead. Only dimmed dots lose their colour, so a thread whose Claude waits on an
/// agy sub-agent shows both lit.
pub fn set_working(dots: &gtk4::Box, lead: Driver, also: &[Driver]) {
    let mut child = dots.first_child();
    for driver in Driver::ALL {
        let Some(dot) = child else { return };
        let state = if driver == lead {
            "lead"
        } else if also.contains(&driver) {
            "working"
        } else {
            "dimmed"
        };
        for class in ["lead", "working", "dimmed"] {
            if class == state {
                dot.add_css_class(class);
            } else {
                dot.remove_css_class(class);
            }
        }
        child = dot.next_sibling();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_agent_has_a_named_colour_from_agent_core() {
        let css = css(true);
        for driver in Driver::ALL {
            let line = format!(
                "@define-color {} {};",
                colour_name(driver),
                driver.info().accent_hex
            );
            assert!(css.contains(&line), "{line}");
        }
        assert_eq!(colour_name(Driver::Claude), "brand_claude");
        assert_eq!(
            rgb(Driver::Agy),
            (91.0 / 255.0, 156.0 / 255.0, 246.0 / 255.0)
        );
    }

    /// On a light window every agent's colour keeps 3:1 against white (the bright accents do
    /// not: Codex's is about 2.2:1), darkened no more than that needs, and keeps its hue.
    #[test]
    fn a_light_window_gets_legible_shades_of_the_same_colours() {
        let white = (1.0, 1.0, 1.0);
        let light = css(false);
        for driver in Driver::ALL {
            let accent = rgb(driver);
            let shade = rgb_on_light(driver);
            assert!(
                contrast(accent, white) < MIN_CONTRAST,
                "{driver:?} needed no shade"
            );
            assert!(contrast(shade, white) >= MIN_CONTRAST, "{driver:?}");
            // One step (1%) lighter would fall short: darkened only as far as needed.
            let keep = shade.0 / accent.0;
            let lighter = (
                accent.0 * (keep + 0.01),
                accent.1 * (keep + 0.01),
                accent.2 * (keep + 0.01),
            );
            assert!(contrast(lighter, white) < MIN_CONTRAST, "{driver:?}");
            // The same hue: every channel scaled alike.
            let ratio = shade.0 / accent.0;
            assert!((shade.2 / accent.2 - ratio).abs() < 1e-9, "{driver:?}");
            let line = format!("@define-color {} {};", colour_name(driver), hex(shade));
            assert!(light.contains(&line), "{line}");
        }
        assert_eq!(hex(rgb(Driver::Claude)), Driver::Claude.info().accent_hex);
    }

    /// GTK refuses an empty class with a critical: the looping droplet has no extra class and
    /// must add none (every loading mark logged one).
    #[test]
    fn a_drip_adds_no_empty_class() {
        gtk4::init().expect("GTK init");
        let warnings = crate::testutil::GtkWarningCapture::start();
        for driver in Driver::ALL {
            let _ = drip(driver);
        }
        let empty: Vec<_> = warnings
            .take()
            .into_iter()
            .filter(|w| w.contains("css_class"))
            .collect();
        assert!(empty.is_empty(), "{empty:?}");
    }

    /// The agents' colours have one source: no stylesheet or drawing code writes one out
    /// (they name `@brand_*`, or read [`rgb`]), so a change to agent-core recolours everything.
    #[test]
    fn no_copy_of_an_agent_colour_outside_agent_core() {
        let sources = [
            ("main.rs", include_str!("main.rs")),
            ("chat/view/style.css", include_str!("chat/view/style.css")),
            ("window/loading.css", include_str!("window/loading.css")),
            ("window/loading.rs", include_str!("window/loading.rs")),
            (
                "window/imp/pack_animation.rs",
                include_str!("window/imp/pack_animation.rs"),
            ),
            (
                "chat/view/thinking.rs",
                include_str!("chat/view/thinking.rs"),
            ),
        ];
        for driver in Driver::ALL {
            let hex = driver.info().accent_hex.trim_start_matches('#');
            for (file, text) in sources {
                assert!(
                    !text.to_ascii_lowercase().contains(hex),
                    "{file} writes out {}'s colour #{hex}; name @{} instead",
                    driver.info().label,
                    colour_name(driver)
                );
            }
        }
    }
}
