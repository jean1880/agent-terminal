//! The app's brand and motion: the agents' colours as the stylesheets name them, the shared
//! timings, and the one reduced-motion check.
//!
//! The agents' colours (the app icon's three cursor dots) come from agent-core's
//! `DriverInfo::accent_hex` and nowhere else: [`install`] defines them for every stylesheet as
//! `@brand_claude`, `@brand_agy` and `@brand_codex`, and Rust drawing reads [`rgb`]. They are
//! not themed: they say which agent it is, under every theme.
//!
//! CSS transitions and animations, and Stack/Revealer transitions, stop by themselves when the
//! desktop turns animations off (`gtk-enable-animations`). Anything drawn on its own frame clock
//! (the closing screen, the menu button) does not, and must ask [`animations_enabled`].

use std::cell::Cell;

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

/// The stylesheet defining every agent's colour name.
pub fn css() -> String {
    Driver::ALL
        .iter()
        .map(|&driver| {
            format!(
                "@define-color {} {};\n",
                colour_name(driver),
                driver.info().accent_hex
            )
        })
        .collect()
}

thread_local! {
    static INSTALLED: Cell<bool> = const { Cell::new(false) };
}

/// Defines the agents' colour names for every stylesheet on the display, once per process.
/// Every loader of a stylesheet naming them calls it first, so no path draws one undefined.
pub fn install() {
    if INSTALLED.with(Cell::get) {
        return;
    }
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let provider = gtk4::CssProvider::new();
    provider.load_from_data(&css());
    gtk4::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    INSTALLED.with(|i| i.set(true));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_agent_has_a_named_colour_from_agent_core() {
        let css = css();
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
