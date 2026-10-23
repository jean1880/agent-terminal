//! Embedded style sheets, installed once per process.

use std::cell::Cell;
use std::thread::LocalKey;

use gtk4::gdk;

/// Installs `css` on the default display, above the app's brand CSS, the first time `flag`
/// (the caller's own) is seen unset; nothing happens without a display, and `flag` stays unset
/// so a later call tries again.
pub fn install_once(flag: &'static LocalKey<Cell<bool>>, css: &str) {
    if flag.with(Cell::get) {
        return;
    }
    let Some(display) = gdk::Display::default() else {
        return;
    };
    crate::brand::install();
    let provider = gtk4::CssProvider::new();
    provider.load_from_data(css);
    gtk4::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
    flag.with(|l| l.set(true));
}
