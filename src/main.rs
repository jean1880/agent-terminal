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
        label {
            color: #c8c8ff;
        }
        .terminal-container {
            padding: 10px;
        }
        .title-1 {
            font-size: 24pt;
            font-weight: bold;
            color: #8e75ff;
        }
        .subtitle {
            font-size: 14pt;
            color: #a0a0ff;
        }
        .command-text {
            font-family: monospace;
            background-color: #120f1d;
            padding: 12px;
            border-radius: 6px;
            color: #c8c8ff;
            border: 1px solid #2d2444;
        }
        button.suggested-action {
            background-color: #8e75ff;
            color: #ffffff;
            font-weight: bold;
            padding: 8px 20px;
            border-radius: 6px;
        }
        button.suggested-action:hover {
            background-color: #7a61e0;
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
