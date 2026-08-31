//! Private implementation details of the AntigravityWindow.

use crate::theme::Theme;
use crate::utils::{detect_cli_binary, get_startup_command, resolve_working_directory};
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk4::glib;
use gtk4::{Align, Box, Button, Image, Label, Orientation, ScrolledWindow, Stack};
use std::cell::RefCell;
use std::env;
use tracing::{debug, error, info, warn};
use vte4::prelude::*;
use vte4::{CursorBlinkMode, CursorShape, Format, PtyFlags, Terminal};

/// Static logo SVG for standalone binary.
const LOGO_SVG: &str = include_str!("../../assets/antigravity_logo.svg");

/// Per-tab state, tracked explicitly so terminal/directory lookups never depend
/// on walking the tab's widget hierarchy. `dir` is the directory the tab was
/// launched in — a running Claude/agy session cannot re-root itself, so it stays
/// valid for the tab's whole lifetime and drives "new tab in same directory".
struct TabState {
    page: adw::TabPage,
    terminal: Terminal,
    dir: String,
    /// Holds the loading screen and the terminal. Kept so a session that dies
    /// before printing anything can still be switched into view — otherwise the
    /// tab would sit on the loading screen with the error hidden behind it.
    stack: Stack,
    /// Revealed when a session exits non-zero, instead of closing the tab.
    exit_bar: Box,
    exit_label: Label,
    /// When the session was spawned, used to tell a failure to launch apart from
    /// a crash part-way through a session.
    spawned_at: std::time::Instant,
}

/// Renders VTE's `child-exited` status as something a person can act on.
///
/// The signal carries the raw `waitpid` status, not an exit code — a CLI exiting
/// 1 arrives here as 256 — so reporting it verbatim would put a meaningless
/// number in front of the user.
fn describe_exit(status: i32) -> String {
    use std::os::unix::process::ExitStatusExt;

    let exit = std::process::ExitStatus::from_raw(status);
    if let Some(code) = exit.code() {
        format!("exit status {code}")
    } else if let Some(signal) = exit.signal() {
        format!("killed by signal {signal}")
    } else {
        format!("wait status {status}")
    }
}

/// Whether a `child-exited` status represents an ordinary, deliberate exit.
fn exited_cleanly(status: i32) -> bool {
    use std::os::unix::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(status).code() == Some(0)
}

/// A session that exits non-zero used to take its tab — and, if it was the last
/// tab, the whole window — with it, so a CLI that panicked left nothing on
/// screen to read. This bar replaces that: the tab stays, the scrollback stays,
/// and the exit is stated with a way to recover.
fn build_exit_bar() -> (Box, Label, Button, Button) {
    let bar = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(12)
        .visible(false)
        .css_classes(["exit-bar"])
        .build();

    let label = Label::builder()
        .hexpand(true)
        .xalign(0.0)
        .wrap(true)
        .css_classes(["exit-bar-text"])
        .build();

    let restart = Button::builder()
        .label("Restart")
        .valign(Align::Center)
        .css_classes(["suggested-action"])
        .build();

    let close = Button::builder()
        .label("Close Tab")
        .valign(Align::Center)
        .build();

    bar.append(&label);
    bar.append(&restart);
    bar.append(&close);
    (bar, label, restart, close)
}

/// Builds the centered logo + text shown while a session is starting.
fn build_loading_box() -> Box {
    let loading_box = Box::builder()
        .orientation(Orientation::Vertical)
        .valign(Align::Center)
        .halign(Align::Center)
        .spacing(10)
        .css_classes(["loading-container"])
        .build();

    // Logo (embedded SVG)
    let logo_image = if let Ok(loader) = gtk4::gdk_pixbuf::PixbufLoader::with_type("svg") {
        loader.set_size(128, 128);
        let load_result = loader
            .write(LOGO_SVG.as_bytes())
            .and_then(|()| loader.close())
            .and_then(|()| {
                loader.pixbuf().ok_or(glib::Error::new(
                    gtk4::gio::IOErrorEnum::Failed,
                    "Failed to get pixbuf",
                ))
            });

        match load_result {
            Ok(pixbuf) => {
                let texture = gtk4::gdk::Texture::for_pixbuf(&pixbuf);
                let img = Image::builder()
                    .pixel_size(96)
                    .css_classes(["loading-icon"])
                    .build();
                img.set_paintable(Some(&texture));
                Some(img)
            }
            Err(e) => {
                error!("Failed to load embedded logo: {}", e);
                None
            }
        }
    } else {
        error!("SVG PixbufLoader not available");
        None
    };

    let loading_label = Label::builder()
        .label("Antigravity is calculating...")
        .css_classes(["loading-text"])
        .build();

    let loading_sub = Label::builder()
        .label("Spawning secure terminal session")
        .css_classes(["loading-subtext"])
        .build();

    if let Some(img) = logo_image {
        loading_box.append(&img);
    }
    loading_box.append(&loading_label);
    loading_box.append(&loading_sub);
    loading_box
}

/// The three environment values CLI detection needs, read on the main thread so
/// they can be moved to a worker.
fn env_triplet() -> (Option<String>, Option<String>, Option<String>) {
    (
        env::var("PATH").ok(),
        env::var("HOME").ok(),
        env::var("SHELL").ok(),
    )
}

/// Resolves the CLI binary for `client` without blocking the main thread.
///
/// Detection runs `which`, stats a handful of paths, and finally `$SHELL -ic`,
/// which sources the user's rc file — seconds on a heavy shell, during which the
/// window previously could not even repaint. The result is cached process-wide,
/// so a second window or a settings change never pays for it twice.
async fn resolve_cli_binary(
    client: crate::config::CliClient,
    path: Option<String>,
    home: Option<String>,
    shell: Option<String>,
) -> Option<String> {
    if let Some(cached) = crate::utils::cached_cli_binary(client) {
        debug!("Reusing cached CLI detection for {:?}", client);
        return cached;
    }

    let detected = gtk4::gio::spawn_blocking(move || {
        detect_cli_binary(client, path.as_deref(), home.as_deref(), shell.as_deref())
    })
    .await
    .unwrap_or_else(|_| {
        error!("CLI detection panicked on the worker thread; treating as not found");
        None
    });

    crate::utils::cache_cli_binary(client, detected.clone());
    detected
}

/// Presents a one-button informational dialog anchored to `parent`.
fn present_message(parent: &super::AntigravityWindow, heading: &str, body: &str) {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_response("ok", "OK");
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("ok");
    dialog.present(Some(parent.upcast_ref::<gtk4::Widget>()));
}

/// Internal state for the AntigravityWindow.
#[derive(Default)]
pub struct AntigravityWindow {
    pub header: RefCell<Option<adw::HeaderBar>>,
    pub window_title: RefCell<Option<adw::WindowTitle>>,
    pub tab_view: RefCell<Option<adw::TabView>>,
    /// One entry per open tab. Pruned when a page is detached.
    tabs: RefCell<Vec<TabState>>,
    pub config: RefCell<crate::config::TerminalConfig>,
    /// The CLI binary resolved at startup, cached so opening a new tab does not
    /// re-run detection (which may block on an interactive shell) on the UI
    /// thread. Refreshed when the configured client changes.
    pub detected_binary: RefCell<Option<String>>,
    /// A queued config save, cancelled and re-armed whenever a setting changes
    /// again before it fires. Held so it can also be flushed on window close.
    pending_save: RefCell<Option<glib::SourceId>>,
}

#[glib::object_subclass]
impl ObjectSubclass for AntigravityWindow {
    const NAME: &'static str = "AntigravityWindow";
    type Type = super::AntigravityWindow;
    type ParentType = adw::ApplicationWindow;
}

impl ObjectImpl for AntigravityWindow {
    fn constructed(&self) {
        self.parent_constructed();
        *self.config.borrow_mut() = crate::config::TerminalConfig::load();
        self.setup_ui();
        self.setup_actions();
    }
}

impl WidgetImpl for AntigravityWindow {}

impl WindowImpl for AntigravityWindow {
    /// Flushes any debounced config save before the window goes away, so a quick
    /// zoom-then-quit does not lose the change it was still waiting to write.
    fn close_request(&self) -> glib::Propagation {
        self.flush_pending_save();
        self.parent_close_request()
    }
}
impl ApplicationWindowImpl for AntigravityWindow {}
impl AdwApplicationWindowImpl for AntigravityWindow {}

impl AntigravityWindow {
    /// Returns the terminal of the currently selected tab, if any.
    fn current_terminal(&self) -> Option<Terminal> {
        let page = self.tab_view.borrow().as_ref()?.selected_page()?;
        self.tabs
            .borrow()
            .iter()
            .find(|t| t.page == page)
            .map(|t| t.terminal.clone())
    }

    /// Applies a closure to every open tab's terminal.
    fn for_each_terminal(&self, f: impl Fn(&Terminal)) {
        for tab in self.tabs.borrow().iter() {
            f(&tab.terminal);
        }
    }

    /// The launch directory of the currently selected tab, if tracked.
    fn current_dir(&self) -> Option<String> {
        let page = self.tab_view.borrow().as_ref()?.selected_page()?;
        self.tabs
            .borrow()
            .iter()
            .find(|t| t.page == page)
            .map(|t| t.dir.clone())
    }

    /// Sets up GAction handlers for context menu items.
    fn setup_actions(&self) {
        let obj = self.obj();

        // Copy Action
        let copy_action = gtk4::gio::SimpleAction::new("copy", None);
        copy_action.connect_activate(glib::clone!(
            #[weak]
            obj,
            move |_, _| {
                if let Some(terminal) = obj.imp().current_terminal() {
                    debug!("Action: Copy");
                    terminal.copy_clipboard_format(Format::Text);
                }
            }
        ));
        obj.add_action(&copy_action);

        // Paste Action
        let paste_action = gtk4::gio::SimpleAction::new("paste", None);
        paste_action.connect_activate(glib::clone!(
            #[weak]
            obj,
            move |_, _| {
                if let Some(terminal) = obj.imp().current_terminal() {
                    debug!("Action: Paste");
                    terminal.paste_clipboard();
                }
            }
        ));
        obj.add_action(&paste_action);

        // New Tab Action (bound to Ctrl+Shift+T in main.rs)
        let new_tab_action = gtk4::gio::SimpleAction::new("new-tab", None);
        new_tab_action.connect_activate(glib::clone!(
            #[weak]
            obj,
            move |_, _| {
                debug!("Action: New Tab");
                obj.imp().new_tab();
            }
        ));
        obj.add_action(&new_tab_action);

        // Restart Session Action. restart_tab already existed with exactly the
        // right semantics but was reachable only as a side effect of closing the
        // settings dialog — there was no way to ask for it directly.
        let restart_action = gtk4::gio::SimpleAction::new("restart-tab", None);
        restart_action.connect_activate(glib::clone!(
            #[weak]
            obj,
            move |_, _| {
                debug!("Action: Restart Session");
                let imp = obj.imp();
                let page = imp
                    .tab_view
                    .borrow()
                    .as_ref()
                    .and_then(|v| v.selected_page());
                if let Some(page) = page {
                    imp.restart_tab(&page);
                }
            }
        ));
        obj.add_action(&restart_action);

        // New Tab in Folder Action (opens a folder picker)
        let new_tab_folder_action = gtk4::gio::SimpleAction::new("new-tab-folder", None);
        new_tab_folder_action.connect_activate(glib::clone!(
            #[weak]
            obj,
            move |_, _| {
                debug!("Action: New Tab in Folder");
                obj.imp().new_tab_in_folder();
            }
        ));
        obj.add_action(&new_tab_folder_action);
    }

    /// Checks the system for Ansible configuration drift by reading the drift report.
    #[cfg(feature = "homelab-drift")]
    fn get_drift_status() -> (bool, u32) {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
        let path = std::path::PathBuf::from(home).join("scripts/rag_indexer/drift_report.txt");
        if let Ok(content) = std::fs::read_to_string(path) {
            let lines = u32::try_from(content.lines().filter(|l| !l.trim().is_empty()).count())
                .unwrap_or(0);
            (lines > 0, lines)
        } else {
            (false, 0)
        }
    }

    /// Adds the homelab Ansible-drift indicator to the header. Compiled out
    /// unless the `homelab-drift` feature (on by default) is enabled.
    #[cfg(not(feature = "homelab-drift"))]
    fn add_health_indicator(&self, _header: &adw::HeaderBar) {}

    /// Adds a header button reflecting Ansible configuration drift; clicking it
    /// shows the drift report and offers to hand it to the CLI for debugging.
    #[cfg(feature = "homelab-drift")]
    fn add_health_indicator(&self, header: &adw::HeaderBar) {
        let obj = self.obj();
        let (has_drift, drift_lines) = Self::get_drift_status();
        let health_btn = gtk4::Button::builder()
            .icon_name(if has_drift {
                "dialog-warning-symbolic"
            } else {
                "security-high-symbolic"
            })
            .tooltip_text(if has_drift {
                format!("Warning: {} configuration drift(s) detected", drift_lines)
            } else {
                "System configuration fully synchronized".to_string()
            })
            .build();

        health_btn.add_css_class(if has_drift {
            "warning-indicator"
        } else {
            "success-indicator"
        });

        health_btn.connect_clicked(glib::clone!(#[weak] obj, move |_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
            let path = std::path::PathBuf::from(&home).join("scripts/rag_indexer/drift_report.txt");
            let mut drift_detected = false;
            let mut content = String::new();

            if let Ok(c) = std::fs::read_to_string(&path) {
                if c.lines().filter(|l| !l.trim().is_empty()).count() > 0 {
                    drift_detected = true;
                    content = c;
                }
            }

            if drift_detected {
                let dialog = adw::AlertDialog::new(
                    Some("Configuration Drift Detected"),
                    Some(&format!("The following drifts were detected:\n\n{content}")),
                );
                dialog.add_response("close", "Close");
                dialog.add_response("debug", "Debug Issue");
                dialog.set_response_appearance("debug", adw::ResponseAppearance::Suggested);
                dialog.set_default_response(Some("close"));
                dialog.set_close_response("close");

                dialog.connect_response(None, glib::clone!(#[weak] obj, move |_, response| {
                    if response == "debug" {
                        if let Some(terminal) = obj.imp().current_terminal() {
                            let prompt = format!("Can you help me debug and fix this ansible drift issue? Here is the drift report:\n\n{content}\n");
                            terminal.feed_child(prompt.as_bytes());
                        }
                    }
                }));
                dialog.present(Some(obj.upcast_ref::<gtk4::Widget>()));
            } else {
                present_message(
                    &obj,
                    "System Health",
                    "System configuration is fully synchronized. No configuration drift detected.",
                );
            }
        }));

        header.pack_end(&health_btn);
    }

    /// Initializes the user interface, switching between terminal and welcome screen.
    fn setup_ui(&self) {
        let obj = self.obj();
        debug!("Setting up UI for Antigravity Terminal");

        obj.set_default_width(950);
        obj.set_default_height(650);
        obj.set_title(Some("Antigravity Terminal"));

        let content = Box::builder().orientation(Orientation::Vertical).build();

        // Modern AdwHeaderBar
        let window_title = adw::WindowTitle::new("Antigravity Terminal", "");
        let header = adw::HeaderBar::builder()
            .title_widget(&window_title)
            .build();

        let settings_btn = gtk4::Button::builder()
            .icon_name("document-properties-symbolic")
            .tooltip_text("Settings")
            .build();
        settings_btn.connect_clicked(glib::clone!(
            #[weak]
            obj,
            move |_| {
                let imp = obj.imp();
                imp.show_preferences();
            }
        ));
        header.pack_end(&settings_btn);

        // Homelab-specific Ansible drift indicator (feature-gated).
        self.add_health_indicator(&header);

        content.append(&header);
        *self.header.borrow_mut() = Some(header);
        *self.window_title.borrow_mut() = Some(window_title);

        // Show a temporary "Detecting" state. A live spinner, not a static icon:
        // detection now runs off the main thread, so this page can actually
        // animate rather than being a frozen placeholder.
        let spinner = gtk4::Spinner::builder()
            .spinning(true)
            .width_request(32)
            .height_request(32)
            .build();
        let status_page = adw::StatusPage::builder()
            .title("Starting up…")
            .description("Looking for an AI CLI…")
            .vexpand(true)
            .child(&spinner)
            .build();

        content.append(&status_page);
        obj.set_content(Some(&content));

        let selected_client = self.config.borrow().cli_client;
        let (path, home, shell) = env_triplet();

        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            obj,
            #[weak]
            content,
            async move {
                let detected = resolve_cli_binary(selected_client, path, home, shell).await;

                let imp = obj.imp();
                *imp.detected_binary.borrow_mut() = detected.clone();

                content.remove(&status_page);

                if let Some(ref binary) = detected {
                    info!("CLI binary '{}' detected, setting up terminal UI", binary);
                    imp.setup_terminal_ui(&content, Some(binary));
                } else {
                    warn!("No compatible CLI detected, setting up welcome UI");
                    imp.setup_welcome_ui(&content);
                }
            }
        ));
    }

    /// Sets up the tabbed terminal interface, then opens the first tab.
    fn setup_terminal_ui(&self, container: &Box, cli_binary: Option<&str>) {
        let obj = self.obj();
        debug!("Initializing tabbed terminal UI");

        let tab_view = adw::TabView::new();
        *self.tab_view.borrow_mut() = Some(tab_view.clone());

        // `autohide` hides the bar whenever a single tab (or none) is open,
        // so it only appears once there are actually multiple tabs.
        let tab_bar = adw::TabBar::builder()
            .view(&tab_view)
            .autohide(true)
            .expand_tabs(true)
            .build();

        // "New tab" button in the header bar.
        if let Some(header) = self.header.borrow().as_ref() {
            let new_tab_btn = Button::builder()
                .icon_name("tab-new-symbolic")
                .tooltip_text("New Tab (Ctrl+Shift+T)")
                .build();
            new_tab_btn.connect_clicked(glib::clone!(
                #[weak]
                obj,
                move |_| {
                    obj.imp().new_tab();
                }
            ));
            header.pack_start(&new_tab_btn);

            let new_tab_folder_btn = Button::builder()
                .icon_name("folder-new-symbolic")
                .tooltip_text("New Tab in Folder…")
                .build();
            new_tab_folder_btn.connect_clicked(glib::clone!(
                #[weak]
                obj,
                move |_| {
                    obj.imp().new_tab_in_folder();
                }
            ));
            header.pack_start(&new_tab_folder_btn);
        }

        container.append(&tab_bar);
        container.append(&tab_view);

        // Immediately confirm tab closures (no unsaved-state prompt for a terminal).
        tab_view.connect_close_page(|view, page| {
            view.close_page_finish(page, true);
            glib::Propagation::Stop // the closure handled the close request
        });

        // Close the window once the last tab is gone.
        tab_view.connect_notify_local(
            Some("n-pages"),
            glib::clone!(
                #[weak]
                obj,
                move |view, _| {
                    if view.n_pages() == 0 {
                        info!("Last tab closed, closing window");
                        obj.close();
                    }
                }
            ),
        );

        // Forget a tab's tracked state when it is removed.
        tab_view.connect_page_detached(glib::clone!(
            #[weak]
            obj,
            move |_, page, _| {
                obj.imp().tabs.borrow_mut().retain(|t| &t.page != page);
            }
        ));

        // Keep the header title in sync with the active tab.
        tab_view.connect_selected_page_notify(glib::clone!(
            #[weak]
            obj,
            move |view| {
                let imp = obj.imp();
                let session_info = view
                    .selected_page()
                    .map(|p| p.title().to_string())
                    .unwrap_or_default();
                if let Some(window_title) = imp.window_title.borrow().as_ref() {
                    window_title.set_subtitle(&session_info);
                }
                if session_info.is_empty() {
                    obj.set_title(Some("Antigravity Terminal"));
                } else {
                    obj.set_title(Some(&format!("Antigravity Terminal — {}", session_info)));
                }
            }
        ));

        self.add_terminal_tab(cli_binary, None);
    }

    /// Opens a new tab rooted in the current tab's directory (fast path).
    fn new_tab(&self) {
        let dir = self.current_dir();
        let detected = self.detected_binary.borrow().clone();
        self.add_terminal_tab(detected.as_deref(), dir.as_deref());
    }

    /// Replaces the active tab with a fresh session using the current config
    /// (client + starting directory). A running TUI client ignores a piped
    /// "exit", so we open a replacement tab and close the old page directly.
    /// Opening before closing keeps the window from dropping to zero tabs.
    fn restart_current_tab(&self) {
        let Some(tab_view) = self.tab_view.borrow().clone() else {
            return;
        };
        let old_page = tab_view.selected_page();
        let detected = self.detected_binary.borrow().clone();
        // Root the replacement in the configured starting directory (which may
        // have just changed); add_terminal_tab falls back to $HOME.
        self.add_terminal_tab(detected.as_deref(), None);
        if let Some(page) = old_page {
            tab_view.close_page(&page);
        }
    }

    /// Prompts for a folder, then opens a new tab rooted there.
    fn new_tab_in_folder(&self) {
        let obj = self.obj();
        let dialog = gtk4::FileDialog::builder()
            .title("Select Folder for New Tab")
            .accept_label("Open")
            .modal(true)
            .build();

        // Start the picker in the current tab's directory when known.
        if let Some(dir) = self.current_dir() {
            dialog.set_initial_folder(Some(&gtk4::gio::File::for_path(&dir)));
        }

        // FileDialog is future-based, so the dialog stays alive for as long as the
        // future is held and there is no manual destroy() to forget.
        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            obj,
            async move {
                let folder = dialog
                    .select_folder_future(Some(obj.upcast_ref::<gtk4::Window>()))
                    .await;
                match folder {
                    Ok(file) => {
                        if let Some(path) = file.path() {
                            let imp = obj.imp();
                            let dir = path.to_string_lossy().to_string();
                            let detected = imp.detected_binary.borrow().clone();
                            imp.add_terminal_tab(detected.as_deref(), Some(&dir));
                        }
                    }
                    // Dismissing the picker is a normal outcome, not a failure.
                    Err(err) => debug!("Folder selection cancelled or failed: {err}"),
                }
            }
        ));
    }

    /// Builds a terminal, wraps it in a tab page, and spawns the CLI session.
    ///
    /// `dir_override` roots the tab in a specific directory; when `None` the
    /// configured starting directory (falling back to `$HOME`) is used.
    fn add_terminal_tab(&self, cli_binary: Option<&str>, dir_override: Option<&str>) {
        debug!("Adding terminal tab");
        let terminal = Terminal::new();

        // Create Stack for transition
        let stack = Stack::builder()
            .transition_type(gtk4::StackTransitionType::Crossfade)
            .transition_duration(500)
            .vexpand(true)
            .build();

        let loading_box = build_loading_box();

        // Terminal Container
        let scrolled = ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .child(&terminal)
            .css_classes(["terminal-container"])
            .build();

        stack.add_named(&loading_box, Some("loading"));
        stack.add_named(&scrolled, Some("terminal"));
        stack.set_visible_child_name("loading");

        // The page holds an (initially hidden) exit bar above the stack, so a
        // dead session can report itself without the tab being torn down.
        let (exit_bar, exit_label, restart_btn, close_btn) = build_exit_bar();
        let tab_content = Box::builder().orientation(Orientation::Vertical).build();
        tab_content.append(&exit_bar);
        tab_content.append(&stack);

        // Add the page and focus it.
        let tab_view = match self.tab_view.borrow().as_ref() {
            Some(view) => view.clone(),
            None => {
                warn!("add_terminal_tab called before tab view was initialized");
                return;
            }
        };
        let page = tab_view.append(&tab_content);
        page.set_title("Terminal");
        tab_view.set_selected_page(&page);

        let obj = self.obj();
        restart_btn.connect_clicked(glib::clone!(
            #[weak]
            obj,
            #[weak]
            page,
            move |_| obj.imp().restart_tab(&page)
        ));
        close_btn.connect_clicked(glib::clone!(
            #[weak]
            obj,
            #[weak]
            page,
            move |_| {
                if let Some(view) = obj.imp().tab_view.borrow().as_ref() {
                    view.close_page(&page);
                }
            }
        ));

        self.configure_terminal(&terminal);
        self.wire_tab_signals(&terminal, &page);
        self.wire_input_controllers(&terminal);

        // Launch the CLI from the override (per-tab dir) or configured starting
        // directory, falling back to $HOME. Record it so "new tab in same
        // directory" can reuse this tab's root.
        let home_dir = env::var("HOME").unwrap_or_else(|_| "/".to_string());
        let requested_dir = dir_override
            .map(str::to_string)
            .unwrap_or_else(|| self.config.borrow().starting_directory.clone());
        let work_dir = resolve_working_directory(&requested_dir, &home_dir);
        self.tabs.borrow_mut().push(TabState {
            page: page.clone(),
            terminal: terminal.clone(),
            dir: work_dir.clone(),
            stack: stack.clone(),
            exit_bar,
            exit_label,
            spawned_at: std::time::Instant::now(),
        });

        self.spawn_session(&terminal, &stack, cli_binary, &work_dir);
    }

    /// Applies theme, font, cursor, scrollback, and capability settings.
    fn configure_terminal(&self, terminal: &Terminal) {
        Theme::apply(terminal, self.config.borrow().theme);

        // High-quality developer monospace font
        let font_desc =
            gtk4::pango::FontDescription::from_string("JetBrains Mono, Fira Code, Monospace 11");
        terminal.set_font(Some(&font_desc));

        terminal.set_cursor_blink_mode(CursorBlinkMode::On);
        terminal.set_cursor_shape(CursorShape::Block);

        let config = self.config.borrow();
        terminal.set_scrollback_lines(i64::from(config.scrollback_lines));
        terminal.set_font_scale(config.font_scale);

        terminal.set_enable_sixel(true);
        terminal.set_allow_hyperlink(true);
        info!("Terminal configured, setting up controllers and signals");
    }

    /// Wires the terminal's title and exit signals to the tab and window.
    fn wire_tab_signals(&self, terminal: &Terminal, page: &adw::TabPage) {
        let obj = self.obj();

        // Terminal title drives the tab label and (when active) the window title.
        terminal.connect_window_title_changed(glib::clone!(
            #[weak]
            obj,
            #[weak]
            page,
            move |terminal| {
                let title = terminal.window_title();
                debug!("Terminal window title changed: {:?}", title);
                let session_info = title.as_deref().unwrap_or("");

                page.set_title(if session_info.is_empty() {
                    "Terminal"
                } else {
                    session_info
                });

                // Only drive the window title from the active tab.
                let imp = obj.imp();
                let is_active = imp
                    .tab_view
                    .borrow()
                    .as_ref()
                    .and_then(|view| view.selected_page())
                    .as_ref()
                    == Some(&page);
                if !is_active {
                    return;
                }

                if let Some(window_title) = imp.window_title.borrow().as_ref() {
                    window_title.set_subtitle(session_info);
                    if session_info.is_empty() {
                        obj.set_title(Some("Antigravity Terminal"));
                    } else {
                        obj.set_title(Some(&format!("Antigravity Terminal — {}", session_info)));
                    }
                };
            }
        ));

        // A clean exit closes the tab, as before — that is someone typing `exit`
        // or Ctrl-D and expecting the tab to go away. A non-zero exit does NOT:
        // it used to close the tab and, if it was the last one, the whole window,
        // so a CLI that panicked took its own stack trace off screen before it
        // could be read.
        terminal.connect_child_exited(glib::clone!(
            #[weak]
            obj,
            #[weak]
            page,
            move |_, status| {
                info!("Terminal child exited: {}", describe_exit(status));
                let imp = obj.imp();
                if exited_cleanly(status) {
                    if let Some(tab_view) = imp.tab_view.borrow().as_ref() {
                        tab_view.close_page(&page);
                    };
                } else {
                    imp.reveal_exit_bar(&page, status);
                }
            }
        ));
    }

    /// Keeps a failed tab open and explains why, leaving the scrollback readable.
    fn reveal_exit_bar(&self, page: &adw::TabPage, status: i32) {
        // A session that dies almost immediately never really started — usually a
        // missing binary or a shell rc that aborts — which is a different problem
        // from a session that ran and then crashed, so say which one it was.
        const LAUNCH_FAILURE_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

        let tabs = self.tabs.borrow();
        let Some(tab) = tabs.iter().find(|t| &t.page == page) else {
            warn!("Exit reported for a tab that is no longer tracked");
            return;
        };

        let detail = describe_exit(status);
        let message = if tab.spawned_at.elapsed() < LAUNCH_FAILURE_WINDOW {
            format!("Session failed to start ({detail})")
        } else {
            format!("Session ended unexpectedly ({detail})")
        };
        tab.exit_label.set_label(&message);

        // If the session died before printing anything the tab is still showing
        // the loading screen, which would hide whatever it did manage to write.
        tab.stack.set_visible_child_name("terminal");
        tab.exit_bar.set_visible(true);
        page.set_title("Session ended");
    }

    /// Replaces `page` with a fresh session rooted in the same directory.
    ///
    /// Distinct from [`Self::restart_current_tab`], which deliberately re-reads
    /// the configured starting directory because that is what just changed. Here
    /// the user is recovering a specific tab and expects to land back where they
    /// were. Opening before closing keeps the window from dropping to zero tabs.
    fn restart_tab(&self, page: &adw::TabPage) {
        let Some(tab_view) = self.tab_view.borrow().clone() else {
            return;
        };
        let dir = self
            .tabs
            .borrow()
            .iter()
            .find(|t| &t.page == page)
            .map(|t| t.dir.clone());
        let detected = self.detected_binary.borrow().clone();
        info!("Restarting session in {:?}", dir);
        self.add_terminal_tab(detected.as_deref(), dir.as_deref());
        tab_view.close_page(page);
    }

    /// Attaches the right-click menu, ctrl-click hyperlink, and key shortcuts.
    fn wire_input_controllers(&self, terminal: &Terminal) {
        let obj = self.obj();

        // Context Menu (Right Click)
        let menu = gtk4::gio::Menu::new();
        menu.append(Some("New Tab"), Some("win.new-tab"));
        menu.append(Some("New Tab in Folder…"), Some("win.new-tab-folder"));
        menu.append(Some("New Window"), Some("app.new-window"));
        menu.append(Some("Restart Session"), Some("win.restart-tab"));

        let section = gtk4::gio::Menu::new();
        section.append(Some("Copy"), Some("win.copy"));
        section.append(Some("Paste"), Some("win.paste"));
        menu.append_section(None, &section);

        let popover = gtk4::PopoverMenu::builder()
            .menu_model(&menu)
            .has_arrow(false)
            .build();
        popover.set_parent(terminal);

        let click_gesture = gtk4::GestureClick::new();
        click_gesture.set_button(3); // Right click
        click_gesture.connect_pressed(glib::clone!(
            #[weak]
            popover,
            move |gesture, _, x, y| {
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                let rect = gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1);
                popover.set_pointing_to(Some(&rect));
                popover.popup();
            }
        ));
        terminal.add_controller(click_gesture);

        // Hyperlink Click Handler (Ctrl + Left Click)
        let link_click_gesture = gtk4::GestureClick::new();
        link_click_gesture.set_button(1); // Left click
        link_click_gesture.connect_pressed(glib::clone!(
            #[weak]
            terminal,
            move |gesture, _, _x, _y| {
                if let Some(event) = gesture.current_event() {
                    let modifiers = event.modifier_state();
                    if modifiers.contains(gtk4::gdk::ModifierType::CONTROL_MASK) {
                        if let Some(uri) = terminal.hyperlink_hover_uri() {
                            gesture.set_state(gtk4::EventSequenceState::Claimed);
                            debug!("Opening hyperlink: {}", uri);
                            // UriLauncher replaces the deprecated gtk4::show_uri.
                            // Fire-and-forget: the portal owns the outcome, and a
                            // failure to launch is not actionable from here.
                            gtk4::UriLauncher::new(&uri).launch(
                                None::<&gtk4::Window>,
                                None::<&gtk4::gio::Cancellable>,
                                |result| {
                                    if let Err(err) = result {
                                        warn!("Failed to open hyperlink: {err}");
                                    }
                                },
                            );
                        }
                    }
                }
            }
        ));
        terminal.add_controller(link_click_gesture);

        // Keyboard Shortcuts (Copy/Paste/Zoom)
        let key_controller = gtk4::EventControllerKey::new();
        key_controller.connect_key_pressed(glib::clone!(
            #[weak]
            terminal,
            #[weak]
            obj,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_ctrl, key, _code, state| {
                let is_ctrl = state.contains(gtk4::gdk::ModifierType::CONTROL_MASK);
                let is_shift = state.contains(gtk4::gdk::ModifierType::SHIFT_MASK);

                match key {
                    gtk4::gdk::Key::C | gtk4::gdk::Key::c if is_ctrl && is_shift => {
                        debug!("Hotkey: Copy");
                        terminal.copy_clipboard_format(Format::Text);
                        glib::Propagation::Stop
                    }
                    gtk4::gdk::Key::V | gtk4::gdk::Key::v if is_ctrl && is_shift => {
                        debug!("Hotkey: Paste");
                        terminal.paste_clipboard();
                        glib::Propagation::Stop
                    }
                    gtk4::gdk::Key::plus | gtk4::gdk::Key::equal if is_ctrl => {
                        obj.imp().set_font_scale(terminal.font_scale() + 0.1);
                        glib::Propagation::Stop
                    }
                    gtk4::gdk::Key::minus if is_ctrl => {
                        obj.imp()
                            .set_font_scale((terminal.font_scale() - 0.1).max(0.1));
                        glib::Propagation::Stop
                    }
                    k if k.to_unicode() == Some('0') && is_ctrl => {
                        obj.imp().set_font_scale(1.0);
                        glib::Propagation::Stop
                    }
                    _ => glib::Propagation::Proceed,
                }
            }
        ));
        terminal.add_controller(key_controller);
    }

    /// Sets the font scale on all tabs and queues a save.
    fn set_font_scale(&self, scale: f64) {
        debug!("Setting font scale: {}", scale);
        self.for_each_terminal(|term| term.set_font_scale(scale));
        self.config.borrow_mut().font_scale = scale;
        self.schedule_config_save();
    }

    /// Persists the config once changes have settled.
    ///
    /// Zoom shortcuts repeat many times a second while a key is held, and the
    /// previous behaviour rewrote config.json on every one of them. Re-arming a
    /// single timer coalesces a burst into one write; [`Self::flush_pending_save`]
    /// covers the case where the window closes before it fires.
    fn schedule_config_save(&self) {
        const SAVE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(500);

        if let Some(pending) = self.pending_save.borrow_mut().take() {
            pending.remove();
        }

        let obj = self.obj();
        let source = glib::timeout_add_local_once(
            SAVE_DEBOUNCE,
            glib::clone!(
                #[weak]
                obj,
                move || {
                    let imp = obj.imp();
                    // The source fires once and is consumed; clear it before saving so
                    // flush_pending_save cannot try to remove an already-dead source.
                    imp.pending_save.replace(None);
                    imp.config.borrow().save();
                }
            ),
        );
        self.pending_save.replace(Some(source));
    }

    /// Cancels a queued save and writes immediately, if one was pending.
    fn flush_pending_save(&self) {
        if let Some(pending) = self.pending_save.borrow_mut().take() {
            pending.remove();
            debug!("Flushing pending config save");
            self.config.borrow().save();
        }
    }

    /// Spawns the shell/CLI in the terminal and reveals it once output appears.
    fn spawn_session(
        &self,
        terminal: &Terminal,
        stack: &Stack,
        cli_binary: Option<&str>,
        work_dir: &str,
    ) {
        let obj = self.obj();
        let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let command = get_startup_command(cli_binary);
        info!(
            "Spawning terminal with shell: {}, command: {:?}",
            shell, command
        );

        // Switch off the loading screen as soon as the command prints anything,
        // then disconnect: this signal fires on every screen update for the life
        // of the tab, and after the first one there is nothing left for it to do.
        let reveal_handler: std::rc::Rc<RefCell<Option<glib::SignalHandlerId>>> =
            std::rc::Rc::new(RefCell::new(None));
        let handler_id = terminal.connect_contents_changed(glib::clone!(
            #[weak]
            stack,
            #[strong]
            reveal_handler,
            move |terminal| {
                if stack.visible_child_name().as_deref() == Some("loading") {
                    debug!("Terminal content detected, switching from loading screen");
                    stack.set_visible_child_name("terminal");
                }
                if let Some(id) = reveal_handler.borrow_mut().take() {
                    terminal.disconnect(id);
                }
            }
        ));
        reveal_handler.replace(Some(handler_id));

        // Inherit the current user environment, then guarantee terminal
        // capability vars so Claude Code renders its full TUI (status line, etc.).
        // When launched from the desktop menu the GUI process has no TERM/COLORTERM,
        // which makes Claude fall back to a degraded renderer that drops the status line.
        let mut env_strs: Vec<String> = glib::environ()
            .iter()
            .map(|os| os.to_string_lossy().to_string())
            .collect();
        if !env_strs.iter().any(|s| s.starts_with("TERM=")) {
            env_strs.push("TERM=xterm-256color".to_string());
        }
        if !env_strs.iter().any(|s| s.starts_with("COLORTERM=")) {
            env_strs.push("COLORTERM=truecolor".to_string());
        }
        let env_ptrs: Vec<&str> = env_strs.iter().map(String::as_str).collect();

        terminal.spawn_async(
            PtyFlags::DEFAULT,
            Some(work_dir),
            &[&shell, &command[0], &command[1]],
            &env_ptrs,
            glib::SpawnFlags::DEFAULT,
            || {},
            -1,
            None::<&gtk4::gio::Cancellable>,
            glib::clone!(
                #[weak]
                obj,
                #[weak]
                stack,
                move |result| {
                    match result {
                        Ok(_) => info!("Terminal process spawned, waiting for content..."),
                        Err(err) => {
                            error!("Error spawning terminal: {}", err);
                            stack.set_visible_child_name("terminal"); // Show terminal anyway so error is visible
                            present_message(
                                &obj,
                                "Terminal Error",
                                &format!("Error spawning terminal: {err}"),
                            );
                        }
                    }
                }
            ),
        );
    }

    /// Sets up the welcome screen using AdwStatusPage.
    fn setup_welcome_ui(&self, container: &Box) {
        let obj = self.obj();

        let status_page = adw::StatusPage::builder()
            .title("Welcome to Antigravity Terminal")
            .description("The Antigravity CLI was not detected on your system. We checked your PATH and interactive shell environment (-ic).\n\nTo get started, please install it using the official script:\ncurl -fsSL https://antigravity.google/cli/install.sh | bash\n\nThen launch it:\nagy")
            .icon_name("utilities-terminal-symbolic")
            .vexpand(true)
            .build();

        let refresh_button = Button::builder()
            .label("Check for agy again")
            .halign(Align::Center)
            .margin_top(20)
            .css_classes(["suggested-action"])
            .build();

        refresh_button.connect_clicked(glib::clone!(
            #[weak]
            obj,
            move |_| {
                let imp = obj.imp();
                imp.setup_ui();
            }
        ));

        status_page.set_child(Some(&refresh_button));
        container.append(&status_page);
    }

    fn show_preferences(&self) {
        let obj = self.obj();
        let config = self.config.borrow().clone();

        let dialog = adw::PreferencesDialog::builder()
            .search_enabled(false)
            .title("Settings")
            .build();

        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::new();
        group.set_title("Terminal Preferences");

        let starting_directory_entry = gtk4::Entry::builder()
            .text(&config.starting_directory)
            .hexpand(true)
            .valign(gtk4::Align::Center)
            .placeholder_text("Leave blank to default to Home directory")
            .build();

        let starting_directory_row = adw::ActionRow::builder()
            .title("Starting Directory")
            .build();
        starting_directory_row.add_suffix(&starting_directory_entry);

        let scroll_adj = gtk4::Adjustment::new(
            config.scrollback_lines as f64,
            100.0,
            100000.0,
            100.0,
            1000.0,
            0.0,
        );
        let scroll_spin = gtk4::SpinButton::builder()
            .adjustment(&scroll_adj)
            .valign(gtk4::Align::Center)
            .build();

        let scrollback_row = adw::ActionRow::builder().title("Scrollback Lines").build();
        scrollback_row.add_suffix(&scroll_spin);

        // Built from CliClient::ALL, like the theme row below it. The previous
        // hand-written index match ran in both directions (0 => Auto to read,
        // Auto => 0 to preselect) with a `_ => Auto` arm swallowing anything
        // unexpected, so the list and the enum could silently disagree.
        let client_names: Vec<String> = crate::config::CliClient::ALL
            .iter()
            .map(ToString::to_string)
            .collect();
        let client_name_refs: Vec<&str> = client_names.iter().map(String::as_str).collect();
        let client_model = gtk4::StringList::new(&client_name_refs);
        let selected_index = crate::config::CliClient::ALL
            .iter()
            .position(|c| *c == config.cli_client)
            .unwrap_or(0) as u32;

        let font_scale_adj = gtk4::Adjustment::new(config.font_scale, 0.5, 3.0, 0.1, 0.5, 0.0);
        let font_scale_spin = gtk4::SpinButton::builder()
            .adjustment(&font_scale_adj)
            .digits(1)
            .valign(gtk4::Align::Center)
            .build();

        let font_scale_row = adw::ActionRow::builder().title("Font Scale").build();
        font_scale_row.add_suffix(&font_scale_spin);

        let cli_client_row = adw::ComboRow::builder()
            .title("Active CLI Client")
            .model(&client_model)
            .selected(selected_index)
            .build();

        let theme_names: Vec<String> = crate::config::ThemeChoice::ALL
            .iter()
            .map(ToString::to_string)
            .collect();
        let theme_name_refs: Vec<&str> = theme_names.iter().map(String::as_str).collect();
        let theme_model = gtk4::StringList::new(&theme_name_refs);
        let theme_index = crate::config::ThemeChoice::ALL
            .iter()
            .position(|t| *t == config.theme)
            .unwrap_or(0) as u32;
        let theme_row = adw::ComboRow::builder()
            .title("Terminal Theme")
            .model(&theme_model)
            .selected(theme_index)
            .build();

        group.add(&starting_directory_row);
        group.add(&scrollback_row);
        group.add(&font_scale_row);
        group.add(&cli_client_row);
        group.add(&theme_row);
        page.add(&group);
        dialog.add(&page);

        // Settings apply as they change rather than in one batch when the dialog
        // closes. AdwPreferencesDialog has no close-request signal to hang a batch
        // commit on, and applying per-row is better behaved anyway: each change is
        // visible immediately, and a rejected value (see the directory row) never
        // reaches the config at all.

        scroll_spin.connect_value_changed(glib::clone!(
            #[weak]
            obj,
            move |spin| {
                let lines = spin.value() as u32;
                obj.imp().config.borrow_mut().scrollback_lines = lines;
                obj.imp()
                    .for_each_terminal(|term| term.set_scrollback_lines(i64::from(lines)));
                obj.imp().schedule_config_save();
            }
        ));

        font_scale_spin.connect_value_changed(glib::clone!(
            #[weak]
            obj,
            move |spin| {
                // Reuses the shared setter, so the zoom shortcuts and this row
                // debounce through the same timer.
                obj.imp().set_font_scale(spin.value());
            }
        ));

        theme_row.connect_selected_notify(glib::clone!(
            #[weak]
            obj,
            move |row| {
                let theme = crate::config::ThemeChoice::ALL
                    .get(row.selected() as usize)
                    .copied()
                    .unwrap_or_default();
                obj.imp().config.borrow_mut().theme = theme;
                // Themes apply live to every open tab; no restart needed.
                obj.imp()
                    .for_each_terminal(|term| Theme::apply(term, theme));
                obj.imp().schedule_config_save();
            }
        ));

        cli_client_row.connect_selected_notify(glib::clone!(
            #[weak]
            obj,
            move |row| {
                let Some(client) = crate::config::CliClient::ALL
                    .get(row.selected() as usize)
                    .copied()
                else {
                    return;
                };
                let imp = obj.imp();
                if imp.config.borrow().cli_client == client {
                    return;
                }
                imp.config.borrow_mut().cli_client = client;
                imp.schedule_config_save();
                info!("CLI client changed to {}, restarting session", client);
                imp.restart_with_client(client);
            }
        ));

        starting_directory_entry.connect_changed(glib::clone!(
            #[weak]
            obj,
            #[weak]
            starting_directory_row,
            move |entry| {
                let text = entry.text().to_string();
                // A path that does not exist used to be accepted, saved, and then
                // silently swapped for $HOME at spawn time with only a log line —
                // the user saw a tab in the wrong place and no explanation. Say so
                // here instead, and refuse to persist it.
                if !directory_is_usable(&text) {
                    entry.add_css_class("error");
                    starting_directory_row.set_subtitle("That directory does not exist");
                    return;
                }
                entry.remove_css_class("error");
                starting_directory_row.set_subtitle("");

                let imp = obj.imp();
                if imp.config.borrow().starting_directory == text {
                    return;
                }
                imp.config.borrow_mut().starting_directory = text;
                imp.schedule_config_save();
            }
        ));

        dialog.present(Some(obj.upcast_ref::<gtk4::Widget>()));
    }

    /// Re-resolves the CLI binary for `client`, then replaces the active tab.
    ///
    /// Detection can shell out, so it takes the same off-thread path as startup
    /// rather than freezing the window while the settings dialog is open.
    fn restart_with_client(&self, client: crate::config::CliClient) {
        let obj = self.obj();
        let (path, home, shell) = env_triplet();
        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            obj,
            async move {
                let detected = resolve_cli_binary(client, path, home, shell).await;
                let imp = obj.imp();
                *imp.detected_binary.borrow_mut() = detected;
                imp.restart_current_tab();
            }
        ));
    }
}

/// Whether a configured starting directory can actually be used.
///
/// Blank is valid and means "$HOME"; `~` is expanded the same way
/// [`resolve_working_directory`] expands it, so the dialog accepts exactly what
/// the spawn path will accept.
fn directory_is_usable(dir: &str) -> bool {
    let trimmed = dir.trim();
    if trimmed.is_empty() {
        return true;
    }
    let home = env::var("HOME").unwrap_or_else(|_| "/".to_string());
    let expanded = if let Some(rest) = trimmed.strip_prefix('~') {
        format!("{home}{rest}")
    } else {
        trimmed.to_string()
    };
    std::path::Path::new(&expanded).is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_gtk() {
        if !gtk4::is_initialized_main_thread() {
            gtk4::init().expect("GTK init failed");
        }
    }

    #[test]
    fn exit_status_is_decoded_not_echoed() {
        // VTE hands over the raw waitpid status, so `exit 1` arrives as 256.
        // Reporting that verbatim would show the user a meaningless number.
        assert_eq!(describe_exit(256), "exit status 1");
        assert_eq!(describe_exit(0), "exit status 0");
        assert_eq!(describe_exit(2 << 8), "exit status 2");
        // Low bits set means killed by a signal rather than a normal exit.
        assert_eq!(describe_exit(9), "killed by signal 9");
    }

    #[test]
    fn only_a_zero_exit_counts_as_clean() {
        assert!(exited_cleanly(0));
        assert!(!exited_cleanly(256), "exit 1 must not be treated as clean");
        assert!(
            !exited_cleanly(9),
            "a signal death must not be treated as clean"
        );
    }

    #[test]
    fn test_window_initialization() {
        init_gtk();
        let app = adw::Application::builder()
            .application_id("org.test.Window")
            .build();
        let window = super::super::AntigravityWindow::new(&app);

        assert_eq!(window.title(), Some("Antigravity Terminal".into()));
    }
}
