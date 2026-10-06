//! Agent Terminal
//!
//! A standalone GTK4 terminal purpose-built for driving an AI coding CLI
//! (Claude, Antigravity/`agy`, or Gemini) in a focused, tabbed window.

use gtk4::prelude::*;
use gtk4::{gdk, gio, glib, CssProvider, STYLE_PROVIDER_PRIORITY_APPLICATION};
use std::io::IsTerminal;
use tracing::{debug, info, warn};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter};

mod account_status;
mod agent_proc;
mod approval_hook;
mod approval_server;
mod chat;
mod claude_probe;
mod codex_probe;
pub mod config;
mod hook_config;
mod model_catalog;
mod probe;
#[cfg(test)]
mod testutil;
mod theme;
mod utils;
mod window;
// The GTK-free logic lives in `agent-kit`; these imports keep the
// `crate::git::…` style paths used throughout the app.
use agent_kit::{diff, git, handoff, restore, worktree};
use window::AgentTerminalWindow;

const APP_ID: &str = "com.jdesroches.AgentTerminal";

/// Application entry point.
fn main() -> glib::ExitCode {
    // agy's PreToolUse gate: a short-lived blocking client, before logging or any GTK setup.
    if std::env::args().nth(1).as_deref() == Some("--approval-hook") {
        approval_hook::main();
        return glib::ExitCode::SUCCESS;
    }
    init_logging();

    // Hidden: a window holding only the chat view on a scripted backend (screenshots, review).
    if std::env::args().skip(1).any(|a| a == "--chat-demo") {
        return chat::view::demo::run();
    }

    info!(
        "Starting Agent Terminal (v{})...",
        env!("CARGO_PKG_VERSION")
    );
    // HANDLES_COMMAND_LINE so `--resume` reaches the *running* instance: a second
    // launch forwards its arguments over D-Bus and exits, and the tab opens in
    // the window that is already there.
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    app.add_main_option(
        "resume",
        glib::Char::from(b'r'),
        glib::OptionFlags::NONE,
        glib::OptionArg::String,
        "Open a tab resuming the session with this ID",
        Some("SESSION_ID"),
    );
    app.add_main_option(
        "dir",
        glib::Char::from(b'd'),
        glib::OptionFlags::NONE,
        glib::OptionArg::String,
        "Directory to resume in (default: where the session was recorded)",
        Some("DIR"),
    );
    app.connect_handle_local_options(check_local_options);
    app.connect_command_line(handle_command_line);

    app.connect_startup(|app| {
        debug!("Application startup: loading CSS");
        load_css();
        window::watch_config_file(app);

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

        // Targets of the out-of-quota notification. App-level, because a
        // notification is delivered to the application, not to a window: each
        // asks every window until the one holding the tab answers.
        let show_tab = gtk4::gio::SimpleAction::new("show-tab", Some(&u64::static_variant_type()));
        show_tab.connect_activate(glib::clone!(
            #[weak]
            app,
            move |_, target| {
                let Some(key) = target.and_then(|t| t.get::<u64>()) else {
                    return;
                };
                if !windows(&app).any(|w| w.show_tab(key)) {
                    info!("Notification named tab {key}, which has since closed");
                }
            }
        ));
        app.add_action(&show_tab);

        let continue_tab_in = gtk4::gio::SimpleAction::new(
            "continue-tab-in",
            Some(&<(u64, String)>::static_variant_type()),
        );
        continue_tab_in.connect_activate(glib::clone!(
            #[weak]
            app,
            move |_, target| {
                let Some((key, profile)) = target.and_then(|t| t.get::<(u64, String)>()) else {
                    return;
                };
                if !windows(&app).any(|w| w.continue_tab_in(key, &profile)) {
                    info!("Notification named tab {key}, which has since closed");
                }
            }
        ));
        app.add_action(&continue_tab_in);

        // All shortcuts are application accelerators bound to window actions, so
        // they are caught before VTE sees the key press.
        // Anything handled by a controller on the terminal widget stops working
        // the moment focus moves — to the settings dialog, the search entry, a
        // header button — which is exactly when a user reaches for copy or zoom.
        for (action, accels) in [
            ("win.new-tab", &["<Ctrl><Shift>T"][..]),
            ("win.restart-tab", &["<Ctrl><Shift>R"]),
            ("win.close-tab", &["<Ctrl><Shift>W"]),
            ("win.copy", &["<Ctrl><Shift>C"]),
            ("win.paste", &["<Ctrl><Shift>V"]),
            ("win.search", &["<Ctrl><Shift>F"]),
            ("win.toggle-diff", &["<Ctrl><Shift>D"]),
            ("win.new-tab-worktree", &["<Ctrl><Shift>G"]),
            ("win.toggle-sidebar", &["F9", "<Ctrl>B"]),
            ("win.toggle-drawer", &["<Ctrl>grave"]),
            // Both the shifted and unshifted key, so Ctrl+= works on layouts
            // where + needs Shift.
            (
                "win.zoom-in",
                &["<Ctrl>plus", "<Ctrl>equal", "<Ctrl>KP_Add"],
            ),
            ("win.zoom-out", &["<Ctrl>minus", "<Ctrl>KP_Subtract"]),
            ("win.zoom-reset", &["<Ctrl>0", "<Ctrl>KP_0"]),
            ("win.next-tab", &["<Ctrl>Tab", "<Ctrl>Page_Down"]),
            ("win.previous-tab", &["<Ctrl><Shift>Tab", "<Ctrl>Page_Up"]),
        ] {
            app.set_accels_for_action(action, accels);
        }

        // Alt+1..9 select a tab by position; Alt+9 means "last", as is
        // conventional, rather than the ninth tab specifically.
        for n in 1..=9i32 {
            let index = if n == 9 { -1 } else { n - 1 };
            app.set_accels_for_action(&format!("win.select-tab({index})"), &[&format!("<Alt>{n}")]);
            // Ctrl+Alt+N opens a tab as the Nth configured profile, in the
            // current tab's directory.
            app.set_accels_for_action(
                &format!("win.new-tab-profile-at({})", n - 1),
                &[&format!("<Ctrl><Alt>{n}")],
            );
        }
    });

    app.connect_activate(|app| {
        info!("Application activated: creating window");
        let window = AgentTerminalWindow::new(app);
        info!("Window created, presenting...");
        window.present();
        info!("Window presented");
    });

    info!("Running application loop...");
    let exit_code = app.run();
    info!("Application loop exited with code: {:?}", exit_code);
    exit_code
}

/// The application's terminal windows.
fn windows(app: &adw::Application) -> impl Iterator<Item = AgentTerminalWindow> {
    app.windows()
        .into_iter()
        .filter_map(|w| w.downcast::<AgentTerminalWindow>().ok())
}

/// Validates the options in the *launching* process, before they are forwarded.
///
/// Doing it here means a bad ID or directory is reported on the terminal that
/// typed it. The primary instance's stderr may be nowhere, and printing to the
/// caller from there (`g_application_command_line_printerr`) would need GLib
/// 2.80. It's also the one place where a relative `--dir` still means the
/// caller's directory, so it's made absolute here.
fn check_local_options(
    _app: &adw::Application,
    options: &glib::VariantDict,
) -> std::ops::ControlFlow<glib::ExitCode> {
    use std::ops::ControlFlow::{Break, Continue};
    const USAGE_ERROR: u8 = 2;
    let usage_error = |message: String| {
        eprintln!("agent-terminal: {message}");
        Break(glib::ExitCode::from(USAGE_ERROR))
    };

    let resume = options.lookup::<String>("resume").ok().flatten();
    let dir = options.lookup::<String>("dir").ok().flatten();

    let Some(resume) = resume else {
        return match dir {
            Some(_) => usage_error("--dir only applies with --resume".to_string()),
            None => Continue(()),
        };
    };

    match utils::validate_session_id(&resume) {
        Ok(id) => options.insert_value("resume", &id.to_variant()),
        Err(reason) => return usage_error(reason),
    }

    if let Some(dir) = dir {
        let path = std::path::PathBuf::from(utils::expand_tilde(&dir));
        let path = match std::env::current_dir() {
            Ok(cwd) if path.is_relative() => cwd.join(path),
            _ => path,
        };
        if !path.is_dir() {
            return usage_error(format!("--dir {} is not a directory", path.display()));
        }
        options.insert_value("dir", &path.to_string_lossy().to_variant());
    }
    Continue(())
}

/// Handles a launch's command line, in the primary instance.
///
/// With no options this is a plain activation, a new window as before. With
/// `--resume`, the tab opens in the active window, or in a new window when
/// there isn't one. The options were checked by [`check_local_options`], but
/// the ID is re-checked: anything on the session bus can send a command line.
fn handle_command_line(
    app: &adw::Application,
    cmdline: &gio::ApplicationCommandLine,
) -> glib::ExitCode {
    let options = cmdline.options_dict();
    let Some(resume) = options.lookup::<String>("resume").ok().flatten() else {
        app.activate();
        return glib::ExitCode::SUCCESS;
    };
    let session_id = match utils::validate_session_id(&resume) {
        Ok(id) => id.to_string(),
        Err(reason) => {
            warn!("Refusing a forwarded resume request: {reason}");
            return glib::ExitCode::FAILURE;
        }
    };
    // check_local_options made this absolute and checked it. A forwarded value
    // that isn't both came from somewhere else, so drop it and fall back to
    // the session-store lookup rather than guess what it was relative to.
    let dir = options
        .lookup::<String>("dir")
        .ok()
        .flatten()
        .filter(|dir| {
            let path = std::path::Path::new(dir);
            let usable = path.is_absolute() && path.is_dir();
            if !usable {
                warn!("Ignoring a forwarded --dir that is not an absolute directory: {dir}");
            }
            usable
        });

    info!("Command line asks to resume session {session_id}");
    let window = app
        .active_window()
        .and_downcast::<AgentTerminalWindow>()
        .unwrap_or_else(|| AgentTerminalWindow::new(app));
    window.resume_session(session_id, dir);
    window.present();
    glib::ExitCode::SUCCESS
}

/// Initializes logging.
///
/// Prefers the systemd journal so a desktop-launched session is discoverable
/// with `journalctl --user -t agent-terminal -b`, and falls back to
/// stderr (used by `make start-local`) when the journal is unavailable. Level
/// defaults to `info` and is overridable via `RUST_LOG`. Also installs a panic
/// hook so a crash lands in the log instead of vanishing with the process.
fn init_logging() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // Send everything to the journal for desktop-launched sessions. Option<Layer>
    // is itself a no-op Layer, so a missing journal just drops this layer.
    let journald_layer = tracing_journald::layer()
        .map(|layer| layer.with_syslog_identifier("agent-terminal".to_string()))
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
    // The brand chrome is dark; popovers, menus and entries follow it instead of the light
    // default (their text would otherwise inherit the brand's pale label colour on white).
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
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
        /* Deliberately distinct from both: an indicator whose source could not
           be read must never be mistaken for a healthy one. */
        .unknown-indicator {
            color: #a0a0ff;
        }
        /* 3.0 thread sidebar */
        .thread-sidebar {
            background-color: #141120;
            border-right: 1px solid #2d2444;
        }
        .thread-list {
            background-color: transparent;
        }
        .thread-list row.folder-row {
            padding-top: 10px;
        }
        .folder-label {
            color: #8a84b8;
            margin-left: 6px;
        }
        .thread-row {
            padding: 2px 2px;
        }
        .thread-title {
            color: #c8c8ff;
        }
        .thread-title.open {
            color: #ebe9ff;
            font-weight: bold;
        }
        .thread-dot.dot-claude { color: #f0a37a; }
        .thread-dot.dot-agy { color: #6ec8ff; }
        .thread-dot.dot-codex { color: #4cc38a; }
        .thread-term { color: #a0a0ff; }
        .thread-badge.badge-approval { color: #ffcc66; }
        .thread-badge.badge-limited { color: #ff7878; }
        .thread-badge.badge-unread { color: #b49bff; }
        .thread-close {
            min-width: 20px;
            min-height: 20px;
            padding: 0;
        }
        .sidebar-footer {
            border-top: 1px solid #2d2444;
        }
        .terminal-drawer {
            border-top: 1px solid #2d2444;
        }
        .hook-entry {
            color: #c8c8ff;
        }
        headerbar .subtitle {
            font-size: 9pt;
            color: #8a84b8;
        }
        ",
    );
    gtk4::style_context_add_provider_for_display(
        &gdk::Display::default().expect("Could not connect to a display."),
        &provider,
        STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}
