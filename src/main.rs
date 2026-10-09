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
mod always_allow;
mod approval_hook;
mod approval_server;
mod availability;
mod chat;
mod claude_probe;
mod codex_probe;
pub mod config;
mod diff_tool;
mod environment_review;
mod hook_config;
mod icons;
mod model_catalog;
mod palette;
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

const APP_ID: &str = "ca.nuvek.AgentTerminal";

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
    // Hidden: the startup splash and skeleton of a launch that never finishes, on a loop.
    if std::env::args().skip(1).any(|a| a == "--loading-demo") {
        return window::loading_demo();
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
        icons::register();
        load_css();
        window::watch_config_file(app);
        diff_tool::sweep_stale();

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
        #[cfg(target_os = "macos")]
        {
            app.set_accels_for_action("win.new-tab", &["<Meta>t", "<Primary><Shift>T"]);
            app.set_accels_for_action("win.close-tab", &["<Meta>w", "<Primary><Shift>W"]);
            app.set_accels_for_action("win.copy", &["<Meta>c", "<Primary><Shift>C"]);
            app.set_accels_for_action("win.paste", &["<Meta>v", "<Primary><Shift>V"]);
            app.set_accels_for_action("win.search", &["<Meta>f", "<Primary><Shift>F"]);
            app.set_accels_for_action(
                "win.toggle-sidebar",
                &["F9", "<Meta>b", "<Primary><Shift>B"],
            );
        }
        #[cfg(not(target_os = "macos"))]
        {
            app.set_accels_for_action("win.new-tab", &["<Ctrl><Shift>T"]);
            app.set_accels_for_action("win.close-tab", &["<Ctrl><Shift>W"]);
            app.set_accels_for_action("win.copy", &["<Ctrl><Shift>C"]);
            app.set_accels_for_action("win.paste", &["<Ctrl><Shift>V"]);
            app.set_accels_for_action("win.search", &["<Ctrl><Shift>F"]);
            app.set_accels_for_action("win.toggle-sidebar", &["F9", "<Primary><Shift>B"]);
        }

        for (action, accels) in [
            ("win.restart-tab", &["<Primary><Shift>R"][..]),
            ("win.toggle-diff", &["<Primary><Shift>D"]),
            ("win.new-tab-worktree", &["<Primary><Shift>G"]),
            ("win.new-tab-folder", &["<Primary><Shift>O"]),
            ("win.checkpoint-now", &["<Primary><Shift>S"]),
            ("win.resume-session", &["<Primary><Shift>E"]),
            ("win.toggle-drawer", &["<Ctrl>grave", "<Primary>J"]),
            ("win.preferences", &["<Primary>comma"]),
            ("win.shortcuts", &["<Primary>question"]),
            // Both the shifted and unshifted key, so Ctrl+= works on layouts
            // where + needs Shift.
            (
                "win.zoom-in",
                &["<Primary>plus", "<Primary>equal", "<Primary>KP_Add"],
            ),
            ("win.zoom-out", &["<Primary>minus", "<Primary>KP_Subtract"]),
            ("win.zoom-reset", &["<Primary>0", "<Primary>KP_0"]),
            ("win.next-tab", &["<Primary>Tab", "<Primary>Page_Down"]),
            (
                "win.previous-tab",
                &["<Primary><Shift>Tab", "<Primary>Page_Up"],
            ),
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

    // The old side of any file opened in an external diff tool is deleted with the app, and no
    // agent outlives it.
    app.connect_shutdown(|_| {
        diff_tool::remove_own();
        agent_proc::terminate_all();
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

/// Loads global application styles. Their colours are names (`@at_bg`, `@at_accent`…) that
/// `palette::apply` defines for the chosen theme; the window applies it before it draws.
fn load_css() {
    // Until a window applies the configured theme: the app's own, so a name never misses.
    palette::apply(config::ThemeChoice::default());
    let provider = CssProvider::new();
    provider.load_from_data(
        "
        window {
            background-color: @at_bg;
        }
        headerbar {
            background-color: @at_bg_deep;
            color: @at_fg;
            border-bottom: 1px solid @at_border;
        }
        label {
            color: @at_fg;
        }
        /* A coloured button sets its own text colour; the rule above, aimed at labels, would
           otherwise beat it (pale lavender on the lavender Allow button). */
        button.suggested-action label,
        button.destructive-action label {
            color: inherit;
        }
        .terminal-container {
            padding: 10px;
        }
        .title-1 {
            font-size: 24pt;
            font-weight: bold;
            color: @at_accent;
        }
        .command-text {
            font-family: monospace;
            background-color: @at_bg_deep;
            padding: 12px;
            border-radius: 6px;
            color: @at_fg;
            border: 1px solid @at_border;
        }
        button.suggested-action {
            background-color: @at_accent;
            color: @at_bg;
            font-weight: bold;
            padding: 8px 20px;
            border-radius: 6px;
        }
        button.suggested-action:hover {
            background-color: @at_accent_hover;
        }
        .loading-text {
            font-size: 16pt;
            font-weight: bold;
            color: @at_fg;
        }
        .loading-subtext {
            font-size: 11pt;
            color: @at_fg_soft;
        }
        .exit-bar {
            background-color: @at_danger_bg;
            border-bottom: 1px solid @at_danger_border;
            padding: 8px 12px;
        }
        .exit-bar-text {
            color: @at_danger_text;
            font-weight: bold;
        }
        .warning-indicator {
            color: @at_danger;
        }
        .success-indicator {
            color: @at_success;
        }
        /* Deliberately distinct from both: an indicator whose source could not
           be read must never be mistaken for a healthy one. */
        .unknown-indicator {
            color: @at_fg_soft;
        }
        /* 3.0 thread sidebar */
        .thread-sidebar {
            background-color: mix(@at_bg_deep, @at_bg, 0.35);
            border-right: 1px solid @at_border;
        }
        .thread-list {
            background-color: transparent;
        }
        .thread-list row.folder-row {
            padding-top: 10px;
        }
        .folder-label {
            color: @at_fg_soft;
            margin-left: 6px;
        }
        .dim-label {
            opacity: 1.0;
            color: @at_fg_dim;
        }
        .thread-row {
            padding: 2px 2px;
        }
        .thread-title {
            color: @at_fg;
        }
        .thread-title.open {
            color: @at_fg_strong;
            font-weight: bold;
        }
        /* The agents' own colours: not themed, they say which agent it is. */
        .thread-dot.dot-claude { color: #e8846b; }
        .thread-dot.dot-agy { color: #5b9cf6; }
        .thread-dot.dot-codex { color: #4cc38a; }
        .thread-term { color: @at_fg_soft; }
        .thread-badge.badge-approval { color: @at_warning; }
        /* A thread waiting for approval while you are elsewhere glows until you look at it. */
        @keyframes attention-glow {
            from { background-color: alpha(@at_warning, 0.06); box-shadow: inset 3px 0 0 alpha(@at_warning, 0.5); }
            to { background-color: alpha(@at_warning, 0.22); box-shadow: inset 3px 0 0 @at_warning; }
        }
        row.needs-attention {
            animation: attention-glow 1.1s ease-in-out infinite alternate;
            border-radius: 6px;
        }
        row.needs-attention .thread-title { color: @at_warning_soft; }
        /* When the sidebar is collapsed, its toggle button pulses if another thread needs attention. */
        button.needs-attention {
            animation: attention-glow 1.1s ease-in-out infinite alternate;
            color: @at_warning;
        }
        button.has-unread {
            color: @at_accent;
        }
        /* Background work (sub-agents, background commands) the main agent is waiting on. */
        .thread-background { color: @at_info; }
        .thread-badge.badge-limited { color: @at_danger; }
        .thread-badge.badge-unread { color: @at_accent; }
        .thread-close, .thread-delete {
            min-width: 20px;
            min-height: 20px;
            padding: 0;
            opacity: 0.45;
        }
        /* Delete stays out of sight until you point at its row (or reach it with the keyboard). */
        .thread-delete { opacity: 0; }
        .thread-row:hover .thread-close,
        .thread-row:hover .thread-delete,
        .thread-delete:focus-visible {
            opacity: 0.85;
        }
        .thread-close:hover {
            opacity: 1.0;
        }
        .thread-delete:hover {
            opacity: 1.0;
            color: @at_danger;
        }
        .sidebar-footer {
            border-top: 1px solid @at_border;
        }
        .terminal-drawer {
            border-top: 1px solid @at_border;
        }
        .hook-entry {
            color: @at_fg;
        }
        headerbar .subtitle {
            font-size: 9pt;
            color: @at_fg_soft;
        }
        ",
    );
    gtk4::style_context_add_provider_for_display(
        &gdk::Display::default().expect("Could not connect to a display."),
        &provider,
        STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

#[cfg(test)]
mod brand_css_tests {
    use super::*;

    /// Needs a display and the brand CSS loaded process-wide, so it is not part of the window
    /// smoke test. Run it on a private display: the preview MCP's `preview_app` with the test
    /// binary and `coloured_buttons_keep_their_text_colour --ignored --nocapture`.
    #[test]
    #[ignore = "loads the brand CSS on a display; run on a private display"]
    fn coloured_buttons_keep_their_text_colour() {
        gtk4::init().expect("GTK init");
        adw::init().expect("adw init");
        load_css();
        let window = gtk4::Window::new();
        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let allow = gtk4::Button::with_label("Allow");
        allow.add_css_class("suggested-action");
        let plain = gtk4::Label::new(Some("text"));
        row.append(&allow);
        row.append(&plain);
        window.set_child(Some(&row));
        window.present();
        let ctx = glib::MainContext::default();
        while ctx.iteration(false) {}
        let label = allow
            .child()
            .and_then(|c| c.downcast::<gtk4::Label>().ok())
            .expect("a label");
        let rgb = |c: gdk::RGBA| [c.red(), c.green(), c.blue()].map(|v| (v * 255.0).round() as u8);
        eprintln!(
            "allow label {:?}, plain label {:?}",
            rgb(label.color()),
            rgb(plain.color())
        );
        assert_eq!(
            rgb(label.color()),
            [0x18, 0x14, 0x25],
            "dark text on the Allow button"
        );
        assert_eq!(
            rgb(plain.color()),
            [0xc8, 0xc8, 0xff],
            "other labels keep the brand colour"
        );
        window.destroy();
    }
}
