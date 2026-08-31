//! Antigravity Terminal
//!
//! A standalone GTK4 terminal application specifically themed and configured
//! for interacting with Antigravity AI.

use gtk4::prelude::*;
use gtk4::{gdk, glib, CssProvider, STYLE_PROVIDER_PRIORITY_APPLICATION};
use std::io::IsTerminal;
use tracing::{debug, info};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter};

pub mod config;
mod theme;
mod utils;
mod window;
use window::AntigravityWindow;

const APP_ID: &str = "com.jdesroches.AntigravityTerminal";

/// Application entry point.
fn main() -> glib::ExitCode {
    init_logging();

    info!(
        "Starting Antigravity Terminal (v{})...",
        env!("CARGO_PKG_VERSION")
    );
    let app = adw::Application::builder().application_id(APP_ID).build();

    app.connect_startup(|app| {
        debug!("Application startup: loading CSS");
        load_css();

        // Add "New Window" action
        let new_window_action = gtk4::gio::SimpleAction::new("new-window", None);
        new_window_action.connect_activate(glib::clone!(
            #[weak]
            app,
            move |_, _| {
                app.activate();
            }
        ));
        app.add_action(&new_window_action);

        // Bind Ctrl+Shift+T to the per-window "new tab" action. Using an app
        // accelerator means it is caught before VTE sees the key press.
        app.set_accels_for_action("win.new-tab", &["<Ctrl><Shift>T"]);
        app.set_accels_for_action("win.restart-tab", &["<Ctrl><Shift>R"]);
    });

    app.connect_activate(|app| {
        info!("Application activated: creating window");
        let window = AntigravityWindow::new(app);
        info!("Window created, presenting...");
        window.present();
        info!("Window presented");
    });

    info!("Running application loop...");
    let exit_code = app.run();
    info!("Application loop exited with code: {:?}", exit_code);
    exit_code
}

/// Initializes logging.
///
/// Prefers the systemd journal so a desktop-launched session is discoverable
/// with `journalctl --user -t antigravity-terminal -b`, and falls back to
/// stderr (used by `make start-local`) when the journal is unavailable. Level
/// defaults to `info` and is overridable via `RUST_LOG`. Also installs a panic
/// hook so a crash lands in the log instead of vanishing with the process.
fn init_logging() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // Send everything to the journal for desktop-launched sessions. Option<Layer>
    // is itself a no-op Layer, so a missing journal just drops this layer.
    let journald_layer = tracing_journald::layer()
        .map(|layer| layer.with_syslog_identifier("antigravity-terminal".to_string()))
        .map_err(|err| eprintln!("journald unavailable ({err}); relying on stderr"))
        .ok();

    // Also log to the terminal when one is attached (e.g. `make start-local`).
    let stderr_layer = std::io::stderr()
        .is_terminal()
        .then(|| fmt::layer().with_writer(std::io::stderr));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(journald_layer)
        .with(stderr_layer)
        .init();

    install_panic_hook();
}

/// Routes panics through `tracing` (so they reach the journal) before running
/// the default hook, which still prints the message and any backtrace.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("<non-string panic payload>");
        tracing::error!(panic.location = %location, "panic: {}", message);
        default_hook(info);
    }));
}

/// Loads global application styles.
fn load_css() {
    let provider = CssProvider::new();
    provider.load_from_data(
        "
        @define-color accent_color #b49bff;
        @define-color accent_bg_color #b49bff;
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
            color: #181425;
            font-weight: bold;
            padding: 8px 20px;
            border-radius: 6px;
        }
        button.suggested-action:hover {
            background-color: #9d80ff;
        }
        .loading-text {
            font-size: 16pt;
            font-weight: bold;
            color: #c8c8ff;
        }
        .loading-subtext {
            font-size: 11pt;
            color: #a0a0ff;
        }
        .exit-bar {
            background-color: #3a1f2b;
            border-bottom: 1px solid #7a3b4c;
            padding: 8px 12px;
        }
        .exit-bar-text {
            color: #ffc4c4;
            font-weight: bold;
        }
        .warning-indicator {
            color: #ff7878;
        }
        .success-indicator {
            color: #4ee8b0;
        }
        ",
    );
    gtk4::style_context_add_provider_for_display(
        &gdk::Display::default().expect("Could not connect to a display."),
        &provider,
        STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}
