//! The app's motion: the shared timings and the one reduced-motion check.
//!
//! CSS transitions and animations, and Stack/Revealer transitions, stop by themselves when the
//! desktop turns animations off (`gtk-enable-animations`). Anything drawn on its own frame clock
//! (the closing screen, the menu button) does not, and must ask [`animations_enabled`].

/// A small state change: a status icon replacing its spinner, a hover.
pub const FAST_MS: u32 = 150;
/// A page of a view giving way to another (splash to window, loading to content).
pub const CROSSFADE_MS: u32 = 220;

/// Whether the desktop wants animations (`gtk-enable-animations`). Without a display (tests)
/// it says yes, as GTK's own default does.
pub fn animations_enabled() -> bool {
    gtk4::Settings::default().is_none_or(|s| s.is_gtk_enable_animations())
}
