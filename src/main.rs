//! Gemini Terminal
//!
//! A standalone GTK4 terminal application specifically themed and configured
//! for interacting with Gemini AI.

use gtk4::prelude::*;
use gtk4::{gdk, glib, CssProvider, STYLE_PROVIDER_PRIORITY_APPLICATION};
use tracing::{info, debug};
use tracing_subscriber::EnvFilter;

mod window;
mod utils;
use window::GeminiWindow;

const APP_ID: &str = "com.jdesroches.GeminiTerminal";

/// Application entry point.
fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .init();

    info!("Starting Gemini Terminal (v0.2.0)...");
    let app = adw::Application::builder().application_id(APP_ID).build();

    app.connect_startup(|_| {
        debug!("Application startup: loading CSS");
        load_css();
    });

    app.connect_activate(|app| {
        info!("Application activated: creating window");
        let window = GeminiWindow::new(app);
        info!("Window created, presenting...");
        window.present();
        info!("Window presented");
    });

    info!("Running application loop...");
    let exit_code = app.run();
    info!("Application loop exited with code: {:?}", exit_code);
    exit_code
}

/// Loads global application styles.
fn load_css() {
    let provider = CssProvider::new();
    provider.load_from_data(
        "
        @define-color accent_color #8e75ff;
        @define-color accent_bg_color #8e75ff;
        @define-color window_bg_color #181425;
        @define-color headerbar_bg_color #120f1d;

        window {
            background-color: @window_bg_color;
        }
        headerbar {
            background-color: @headerbar_bg_color;
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
            color: @accent_color;
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
            background-color: @accent_bg_color;
            color: #ffffff;
            font-weight: bold;
            padding: 8px 20px;
            border-radius: 6px;
        }
        button.suggested-action:hover {
            background-color: #7a61e0;
        }
        @keyframes rotate {
            0% { transform: rotate(0deg); }
            25% { transform: rotate(45deg); }
            100% { transform: rotate(360deg); }
        }
        .loading-container {
            background-color: @window_bg_color;
        }
        .loading-icon {
            animation: rotate 1.8s infinite cubic-bezier(0.4, 0.0, 0.2, 1);
            margin-bottom: 32px;
        }
        .loading-text {
            font-size: 16pt;
            font-weight: bold;
            color: @accent_color;
        }
        .loading-subtext {
            font-size: 10pt;
            color: #a0a0ff;
            margin-top: 8px;
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
