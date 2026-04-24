//! Gemini Terminal
//!
//! A standalone GTK4 terminal application specifically themed and configured
//! for interacting with Gemini AI.

use gtk4::prelude::*;
use gtk4::{gdk, glib, Application, CssProvider, STYLE_PROVIDER_PRIORITY_APPLICATION};

mod window;
use window::GeminiWindow;

const APP_ID: &str = "com.jdesroches.GeminiTerminal";

/// Application entry point.
fn main() -> glib::ExitCode {
    let app = Application::builder().application_id(APP_ID).build();

    app.connect_startup(|_| {
        load_css();
    });

    app.connect_activate(|app| {
        let window = GeminiWindow::new(app);
        window.present();
    });

    app.run()
}

/// Loads global application styles.
fn load_css() {
    let provider = CssProvider::new();
    provider.load_from_data(
        "
        window {
            background-color: #181425;
        }
        headerbar {
            background-color: #120f1d;
            color: #c8c8ff;
            border-bottom: 1px solid #2d2444;
        }
        .terminal-container {
            padding: 10px;
        }
    ",
    );

    if let Some(display) = gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
