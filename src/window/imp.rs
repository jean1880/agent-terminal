//! Private implementation details of the AgentTerminalWindow.

use crate::config::Profile;
use crate::theme::Theme;
use crate::utils::{get_startup_command, resolve_profile, resolve_working_directory};
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
const LOGO_SVG: &str = include_str!("../../assets/com.jdesroches.AgentTerminal.svg");

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
    /// Per-tab, because VTE holds the search regex and match position on the
    /// terminal itself — a single window-level bar would leak one tab's search
    /// state into another.
    search_bar: gtk4::SearchBar,
    search_entry: gtk4::SearchEntry,
    /// Which profile this tab is running, so a restart reuses it and the session
    /// file can record it.
    profile: Option<String>,
    /// The session this tab was opened to resume, so restarting a crashed tab
    /// resumes the same conversation instead of silently starting a new one.
    session_id: Option<String>,
}

/// A request to open a tab resuming a session.
///
/// Queued when it arrives before the window has a tab view — a freshly created
/// window is still resolving its profile off-thread when the command line that
/// created it asks for the tab.
#[derive(Debug, Clone)]
pub struct ResumeRequest {
    pub session_id: String,
    /// Where to resume. `None` means look it up in the profile's session store.
    pub dir: Option<String>,
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
        .label("Starting your session…")
        .css_classes(["loading-text"])
        .build();

    let loading_sub = Label::builder()
        .label("Spawning terminal session")
        .css_classes(["loading-subtext"])
        .build();

    if let Some(img) = logo_image {
        loading_box.append(&img);
    }
    loading_box.append(&loading_label);
    loading_box.append(&loading_sub);
    loading_box
}

/// Applies the font and cursor settings to a terminal.
///
/// Split out so a settings change can re-apply to every open tab without
/// rebuilding the rest of the terminal's configuration.
fn apply_appearance(terminal: &Terminal, config: &crate::config::TerminalConfig) {
    let font_desc = gtk4::pango::FontDescription::from_string(&config.font);
    terminal.set_font(Some(&font_desc));

    terminal.set_cursor_shape(match config.cursor_shape {
        crate::config::CursorShapeChoice::Block => CursorShape::Block,
        crate::config::CursorShapeChoice::Ibeam => CursorShape::Ibeam,
        crate::config::CursorShapeChoice::Underline => CursorShape::Underline,
    });
    terminal.set_cursor_blink_mode(if config.cursor_blink {
        CursorBlinkMode::On
    } else {
        CursorBlinkMode::Off
    });
}

/// PCRE2 flags. VTE requires MULTILINE on search regexes; UTF makes the search
/// character- rather than byte-oriented.
const PCRE2_CASELESS: u32 = 0x0000_0008;
const PCRE2_MULTILINE: u32 = 0x0000_0400;
const PCRE2_UTF: u32 = 0x0008_0000;

/// Escapes PCRE2 metacharacters so a plain-text search means what it says.
fn escape_for_search(text: &str) -> String {
    const META: [char; 17] = [
        '\\', '^', '$', '.', '[', ']', '|', '(', ')', '?', '*', '+', '{', '}', '-', '#', '/',
    ];
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        if META.contains(&ch) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// Builds the search bar shown above a tab's terminal.
///
/// 10,000 lines of default scrollback had no way to search it — the most
/// conspicuous gap against every other terminal, and worse here because an agent
/// session generates far more output than a person typing commands.
fn build_search_bar(terminal: &Terminal) -> (gtk4::SearchBar, gtk4::SearchEntry) {
    let entry = gtk4::SearchEntry::builder()
        .placeholder_text("Search scrollback")
        .hexpand(true)
        .build();

    let previous = Button::builder()
        .icon_name("go-up-symbolic")
        .tooltip_text("Previous match (Shift+Enter)")
        .build();
    let next = Button::builder()
        .icon_name("go-down-symbolic")
        .tooltip_text("Next match (Enter)")
        .build();

    let case_sensitive = gtk4::ToggleButton::builder()
        .icon_name("format-text-italic-symbolic")
        .tooltip_text("Match case")
        .build();
    let use_regex = gtk4::ToggleButton::builder()
        .icon_name("system-search-symbolic")
        .tooltip_text("Regular expression")
        .build();

    let row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();
    row.append(&entry);
    row.append(&previous);
    row.append(&next);
    row.append(&case_sensitive);
    row.append(&use_regex);

    let bar = gtk4::SearchBar::builder()
        .child(&row)
        .show_close_button(true)
        .build();
    bar.connect_entry(&entry);

    // Recompiling on every change keeps the highlight in step with the query.
    let update = {
        let terminal = terminal.clone();
        let entry = entry.clone();
        let case_sensitive = case_sensitive.clone();
        let use_regex = use_regex.clone();
        move || {
            let text = entry.text().to_string();
            if text.is_empty() {
                terminal.search_set_regex(None, 0);
                return;
            }
            let pattern = if use_regex.is_active() {
                text
            } else {
                escape_for_search(&text)
            };
            let mut flags = PCRE2_MULTILINE | PCRE2_UTF;
            if !case_sensitive.is_active() {
                flags |= PCRE2_CASELESS;
            }
            match vte4::Regex::for_search(&pattern, flags) {
                Ok(regex) => terminal.search_set_regex(Some(&regex), 0),
                // An in-progress regex is invalid more often than not, so this is
                // an expected state rather than an error worth shouting about.
                Err(err) => debug!("Search pattern not usable yet: {err}"),
            }
        }
    };

    entry.connect_search_changed({
        let update = update.clone();
        move |_| update()
    });
    for toggle in [&case_sensitive, &use_regex] {
        toggle.connect_toggled({
            let update = update.clone();
            move |_| update()
        });
    }

    entry.connect_activate(glib::clone!(
        #[weak]
        terminal,
        move |_| {
            terminal.search_find_next();
        }
    ));
    next.connect_clicked(glib::clone!(
        #[weak]
        terminal,
        move |_| {
            terminal.search_find_next();
        }
    ));
    previous.connect_clicked(glib::clone!(
        #[weak]
        terminal,
        move |_| {
            terminal.search_find_previous();
        }
    ));

    (bar, entry)
}

/// The first entry of the profile dropdown: "resolve automatically".
const AUTO_PROFILE_LABEL: &str = "Auto-detect";
/// Its index. Every profile sits at its list position plus one.
const AUTO_INDEX: u32 = 0;

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
async fn resolve_active_profile(
    profiles: Vec<crate::config::Profile>,
    preferred: Option<String>,
    path: Option<String>,
    home: Option<String>,
    shell: Option<String>,
) -> Option<crate::config::Profile> {
    // Results already known are handed to the worker rather than looked up from
    // it: the cache is a main-thread thread_local, so the worker cannot read it.
    let known: std::collections::HashMap<String, bool> = profiles
        .iter()
        .filter_map(|p| {
            crate::utils::cached_command_available(&p.command).map(|v| (p.command.clone(), v))
        })
        .collect();

    if known.len() == profiles.len() {
        debug!("Every profile command is already resolved; skipping the probe");
        return resolve_profile(&profiles, preferred.as_deref(), |command| {
            known.get(command).copied().unwrap_or(false)
        })
        .cloned();
    }

    let (chosen, discovered) = gtk4::gio::spawn_blocking(move || {
        let probe = crate::utils::SystemProbe::new(path, home, shell);
        let mut discovered: Vec<(String, bool)> = Vec::new();

        // resolve_profile short-circuits on the first usable profile, so an
        // installed first choice still costs exactly one lookup — the closure is
        // only called for commands it actually needs to know about.
        let chosen = resolve_profile(&profiles, preferred.as_deref(), |command| {
            if let Some(known) = known.get(command) {
                return *known;
            }
            if let Some((_, seen)) = discovered.iter().find(|(c, _)| c == command) {
                return *seen;
            }
            let available = probe.command_available(command);
            discovered.push((command.to_string(), available));
            available
        })
        .cloned();

        (chosen, discovered)
    })
    .await
    .unwrap_or_else(|_| {
        error!("Profile resolution panicked on the worker thread; treating as unavailable");
        (None, Vec::new())
    });

    for (command, available) in discovered {
        crate::utils::cache_command_available(&command, available);
    }
    chosen
}

/// Presents a one-button informational dialog anchored to `parent`.
fn present_message(parent: &super::AgentTerminalWindow, heading: &str, body: &str) {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_response("ok", "OK");
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("ok");
    dialog.present(Some(parent.upcast_ref::<gtk4::Widget>()));
}

/// Internal state for the AgentTerminalWindow.
#[derive(Default)]
pub struct AgentTerminalWindow {
    pub header: RefCell<Option<adw::HeaderBar>>,
    pub window_title: RefCell<Option<adw::WindowTitle>>,
    pub tab_view: RefCell<Option<adw::TabView>>,
    /// One entry per open tab. Pruned when a page is detached.
    tabs: RefCell<Vec<TabState>>,
    pub config: RefCell<crate::config::TerminalConfig>,
    /// The profile resolved at startup, cached so opening a new tab does not
    /// re-run resolution (which may block on an interactive shell) on the UI
    /// thread. Refreshed when the selected profile changes.
    pub active_profile: RefCell<Option<Profile>>,
    /// A queued config save, cancelled and re-armed whenever a setting changes
    /// again before it fires. Held so it can also be flushed on window close.
    pending_save: RefCell<Option<glib::SourceId>>,
    /// Resume requests that arrived before the tab view existed.
    pending_resumes: RefCell<Vec<ResumeRequest>>,
    /// Set once the window settles on the welcome screen, which has no tab
    /// view and never will — so a resume must not wait for one.
    no_cli: std::cell::Cell<bool>,
}

#[glib::object_subclass]
impl ObjectSubclass for AgentTerminalWindow {
    const NAME: &'static str = "AgentTerminalWindow";
    type Type = super::AgentTerminalWindow;
    type ParentType = adw::ApplicationWindow;
}

impl ObjectImpl for AgentTerminalWindow {
    fn constructed(&self) {
        self.parent_constructed();
        *self.config.borrow_mut() = crate::config::TerminalConfig::load();
        self.setup_ui();
        self.setup_actions();
    }
}

impl WidgetImpl for AgentTerminalWindow {}

impl WindowImpl for AgentTerminalWindow {
    /// Flushes any debounced config save before the window goes away, so a quick
    /// zoom-then-quit does not lose the change it was still waiting to write.
    fn close_request(&self) -> glib::Propagation {
        // Both before the window goes: a quick zoom-then-quit must not lose the
        // change still waiting on the debounce timer, and the tab layout is only
        // knowable while the tabs still exist.
        self.save_session();
        self.flush_pending_save();
        self.parent_close_request()
    }
}
impl ApplicationWindowImpl for AgentTerminalWindow {}
impl AdwApplicationWindowImpl for AgentTerminalWindow {}

impl AgentTerminalWindow {
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

        // Zoom, search and tab navigation as window actions rather than key
        // handlers on the terminal widget. Anything bound to the terminal is
        // inert the moment focus moves elsewhere — the settings dialog, the
        // search entry — which is why copy, paste and zoom used to stop working
        // in exactly the situations where you would reach for them.
        for (name, delta) in [("zoom-in", 0.1_f64), ("zoom-out", -0.1)] {
            let action = gtk4::gio::SimpleAction::new(name, None);
            action.connect_activate(glib::clone!(
                #[weak]
                obj,
                move |_, _| {
                    let imp = obj.imp();
                    let current = imp.config.borrow().font_scale;
                    // Floor rather than clamp to zero: a scale of 0 renders an
                    // invisible terminal with no obvious way back.
                    imp.set_font_scale((current + delta).clamp(0.5, 3.0));
                }
            ));
            obj.add_action(&action);
        }

        let zoom_reset = gtk4::gio::SimpleAction::new("zoom-reset", None);
        zoom_reset.connect_activate(glib::clone!(
            #[weak]
            obj,
            move |_, _| obj.imp().set_font_scale(1.0)
        ));
        obj.add_action(&zoom_reset);

        let search_action = gtk4::gio::SimpleAction::new("search", None);
        search_action.connect_activate(glib::clone!(
            #[weak]
            obj,
            move |_, _| obj.imp().toggle_search()
        ));
        obj.add_action(&search_action);

        let close_tab = gtk4::gio::SimpleAction::new("close-tab", None);
        close_tab.connect_activate(glib::clone!(
            #[weak]
            obj,
            move |_, _| {
                let imp = obj.imp();
                let view = imp.tab_view.borrow().clone();
                if let Some(view) = view {
                    if let Some(page) = view.selected_page() {
                        view.close_page(&page);
                    }
                }
            }
        ));
        obj.add_action(&close_tab);

        for (name, forward) in [("next-tab", true), ("previous-tab", false)] {
            let action = gtk4::gio::SimpleAction::new(name, None);
            action.connect_activate(glib::clone!(
                #[weak]
                obj,
                move |_, _| {
                    if let Some(view) = obj.imp().tab_view.borrow().as_ref() {
                        // select_next_page stops at the end; wrap explicitly so
                        // cycling works the way it does in every other terminal.
                        let moved = if forward {
                            view.select_next_page()
                        } else {
                            view.select_previous_page()
                        };
                        if !moved && view.n_pages() > 0 {
                            let wrap = if forward { 0 } else { view.n_pages() - 1 };
                            view.set_selected_page(&view.nth_page(wrap));
                        }
                    }
                }
            ));
            obj.add_action(&action);
        }

        // Alt+1..9 jump straight to a tab. Parameterised so one action covers all
        // nine rather than nine near-identical ones.
        let select_tab =
            gtk4::gio::SimpleAction::new("select-tab", Some(&i32::static_variant_type()));
        select_tab.connect_activate(glib::clone!(
            #[weak]
            obj,
            move |_, target| {
                let Some(index) = target.and_then(|t| t.get::<i32>()) else {
                    return;
                };
                if let Some(view) = obj.imp().tab_view.borrow().as_ref() {
                    if view.n_pages() == 0 {
                        return;
                    }
                    // -1 means "the last tab", which is what Alt+9 conventionally
                    // does regardless of how many tabs are actually open.
                    let target = if index < 0 { view.n_pages() - 1 } else { index };
                    if target < view.n_pages() {
                        view.set_selected_page(&view.nth_page(target));
                    }
                }
            }
        ));
        obj.add_action(&select_tab);

        // New Tab As <profile>. Parameterised by profile name rather than index,
        // so it stays correct if the profile list changes underneath the menu.
        let new_tab_profile_action =
            gtk4::gio::SimpleAction::new("new-tab-profile", Some(&String::static_variant_type()));
        new_tab_profile_action.connect_activate(glib::clone!(
            #[weak]
            obj,
            move |_, target| {
                let Some(name) = target.and_then(|t| t.get::<String>()) else {
                    warn!("new-tab-profile activated without a profile name");
                    return;
                };
                let imp = obj.imp();
                let profile = imp
                    .config
                    .borrow()
                    .profiles
                    .iter()
                    .find(|p| p.name == name)
                    .cloned();
                match profile {
                    Some(profile) => imp.new_tab_with_profile(&profile),
                    None => warn!("No profile named '{name}'"),
                }
            }
        ));
        obj.add_action(&new_tab_profile_action);

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

        // Resume Session Action (prompts for a session ID)
        let resume_action = gtk4::gio::SimpleAction::new("resume-session", None);
        resume_action.connect_activate(glib::clone!(
            #[weak]
            obj,
            move |_, _| {
                debug!("Action: Resume Session");
                obj.imp().show_session_browser();
            }
        ));
        obj.add_action(&resume_action);
    }

    /// Adds the configured status indicators to the header bar.
    ///
    /// This replaces a single hard-coded Ansible-drift button whose source path
    /// no longer existed. Because a missing file read as "no drift", it rendered
    /// a green shield permanently — reporting a healthy system it had never
    /// actually checked. Indicators are data now, and unreadable is its own
    /// state rather than a synonym for fine.
    fn add_status_indicators(&self, header: &adw::HeaderBar) {
        let indicators = self.config.borrow().indicators.clone();
        if indicators.len() > crate::config::MAX_INDICATORS {
            warn!(
                "{} indicators configured; showing the first {}",
                indicators.len(),
                crate::config::MAX_INDICATORS
            );
        }

        for indicator in indicators.into_iter().take(crate::config::MAX_INDICATORS) {
            let button = gtk4::Button::builder()
                .icon_name(&indicator.icon_unknown)
                .tooltip_text(format!("{}: checking…", indicator.label))
                .build();
            header.pack_end(&button);
            self.drive_indicator(button, indicator);
        }
    }

    /// Evaluates one indicator now, and on its refresh interval if it has one.
    fn drive_indicator(&self, button: Button, indicator: crate::config::Indicator) {
        let obj = self.obj();
        // The latest detail, so the click handler shows what the button reflects
        // rather than re-reading and possibly disagreeing with its own icon.
        let detail = std::rc::Rc::new(RefCell::new(String::new()));

        button.connect_clicked(glib::clone!(
            #[weak]
            obj,
            #[strong]
            detail,
            #[strong]
            indicator,
            move |_| {
                obj.imp()
                    .present_indicator_detail(&indicator, &detail.borrow());
            }
        ));

        let refresh = indicator.refresh_secs;
        let evaluate = move || {
            let indicator = indicator.clone();
            let button = button.clone();
            let detail = detail.clone();
            glib::MainContext::default().spawn_local(async move {
                let source = indicator.source.clone();
                // A configured command is arbitrary and may block; it never runs
                // on the main thread.
                let state =
                    gtk4::gio::spawn_blocking(move || crate::utils::read_indicator(&source))
                        .await
                        .unwrap_or_else(|_| crate::utils::IndicatorState::Unknown {
                            reason: "Indicator check panicked".to_string(),
                        });

                let (icon, css, tooltip) = match &state {
                    crate::utils::IndicatorState::Ok => (
                        &indicator.icon_ok,
                        "success-indicator",
                        format!("{}: OK", indicator.label),
                    ),
                    crate::utils::IndicatorState::Warn { .. } => (
                        &indicator.icon_warn,
                        "warning-indicator",
                        format!("{}: needs attention", indicator.label),
                    ),
                    crate::utils::IndicatorState::Unknown { reason } => (
                        &indicator.icon_unknown,
                        "unknown-indicator",
                        format!("{}: unknown — {reason}", indicator.label),
                    ),
                };

                info!("Indicator {}", tooltip);
                button.set_icon_name(icon);
                button.set_tooltip_text(Some(&tooltip));
                for class in [
                    "success-indicator",
                    "warning-indicator",
                    "unknown-indicator",
                ] {
                    button.remove_css_class(class);
                }
                button.add_css_class(css);
                detail.replace(state.detail().to_string());
            });
        };

        evaluate();

        // Re-check on an interval where one is configured. The predecessor read
        // its source once at window construction and never again, so a drift that
        // appeared later was never shown.
        if let Some(secs) = refresh.filter(|s| *s > 0) {
            glib::timeout_add_local(std::time::Duration::from_secs(secs), move || {
                evaluate();
                glib::ControlFlow::Continue
            });
        }
    }

    /// Shows an indicator's detail, and for SendToTerminal offers to hand it over.
    fn present_indicator_detail(&self, indicator: &crate::config::Indicator, detail: &str) {
        let obj = self.obj();
        let body = if detail.trim().is_empty() {
            "Nothing to report.".to_string()
        } else {
            detail.to_string()
        };

        if indicator.action == crate::config::IndicatorAction::ShowOutput
            || detail.trim().is_empty()
        {
            present_message(&obj, &indicator.label, &body);
            return;
        }

        // SendToTerminal types content into a live agent's stdin. That was a
        // default affordance when the only source was one hard-coded file; with
        // user-configurable sources it has to be an explicit, previewed choice.
        let dialog = adw::AlertDialog::new(Some(&indicator.label), Some(&body));
        dialog.add_response("close", "Close");
        dialog.add_response("send", "Send to Session");
        dialog.set_response_appearance("send", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("close"));
        dialog.set_close_response("close");

        let detail = detail.to_string();
        dialog.connect_response(
            None,
            glib::clone!(
                #[weak]
                obj,
                move |_, response| {
                    if response != "send" {
                        return;
                    }
                    if let Some(terminal) = obj.imp().current_terminal() {
                        terminal.feed_child(detail.as_bytes());
                    }
                }
            ),
        );
        dialog.present(Some(obj.upcast_ref::<gtk4::Widget>()));
    }

    /// Initializes the user interface, switching between terminal and welcome screen.
    fn setup_ui(&self) {
        let obj = self.obj();
        debug!("Setting up UI for Agent Terminal");

        obj.set_default_width(950);
        obj.set_default_height(650);
        obj.set_title(Some("Agent Terminal"));

        let content = Box::builder().orientation(Orientation::Vertical).build();

        // Modern AdwHeaderBar
        let window_title = adw::WindowTitle::new("Agent Terminal", "");
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

        // Configured status indicators, if any.
        self.add_status_indicators(&header);

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

        let (profiles, preferred) = {
            let config = self.config.borrow();
            (config.profiles.clone(), config.default_profile.clone())
        };
        let (path, home, shell) = env_triplet();

        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            obj,
            #[weak]
            content,
            async move {
                let resolved = resolve_active_profile(profiles, preferred, path, home, shell).await;

                let imp = obj.imp();
                *imp.active_profile.borrow_mut() = resolved.clone();

                content.remove(&status_page);

                if let Some(ref profile) = resolved {
                    info!(
                        "Profile '{}' resolved to '{}', setting up terminal UI",
                        profile.name, profile.command
                    );
                    imp.setup_terminal_ui(&content, Some(profile));
                } else {
                    warn!("No compatible CLI detected, setting up welcome UI");
                    imp.setup_welcome_ui(&content);
                }
            }
        ));
    }

    /// Sets up the tabbed terminal interface, then opens the first tab.
    fn setup_terminal_ui(&self, container: &Box, profile: Option<&Profile>) {
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

        // "New tab" in the header bar. A split button rather than a plain one:
        // clicking it clones the current tab's profile and directory as before,
        // while the dropdown launches any configured profile directly. That is
        // the whole point of profiles being data — a second CLI, or the same one
        // rooted in a different project, is one click away without a trip
        // through Settings.
        if let Some(header) = self.header.borrow().as_ref() {
            let new_tab_menu = self.build_profile_menu();
            let resume_section = gtk4::gio::Menu::new();
            resume_section.append(Some("Resume Session…"), Some("win.resume-session"));
            new_tab_menu.append_section(None, &resume_section);

            let new_tab_btn = adw::SplitButton::builder()
                .icon_name("tab-new-symbolic")
                .tooltip_text("New Tab (Ctrl+Shift+T)")
                .menu_model(&new_tab_menu)
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
                // Looking at a tab is the acknowledgement, so clear its marker.
                if let Some(page) = view.selected_page() {
                    page.set_needs_attention(false);
                }
                let session_info = view
                    .selected_page()
                    .map(|p| p.title().to_string())
                    .unwrap_or_default();
                if let Some(window_title) = imp.window_title.borrow().as_ref() {
                    window_title.set_subtitle(&session_info);
                }
                if session_info.is_empty() {
                    obj.set_title(Some("Agent Terminal"));
                } else {
                    obj.set_title(Some(&format!("Agent Terminal — {}", session_info)));
                }
            }
        ));

        // A window opened *for* a resume shows that session rather than an
        // extra blank tab beside it. Restored tabs still come back: restoring is
        // the user's standing preference, and the resume is added to it.
        let pending: Vec<ResumeRequest> = self.pending_resumes.take();
        if !self.restore_previous_session(profile) && pending.is_empty() {
            self.add_terminal_tab(profile, None);
        }
        for request in pending {
            self.open_resume_tab(request);
        }
    }

    /// Opens a new tab rooted in the current tab's directory (fast path).
    fn new_tab(&self) {
        let dir = self.current_dir();
        let profile = self.active_profile.borrow().clone();
        self.add_terminal_tab(profile.as_ref(), dir.as_deref());
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
        let profile = self.active_profile.borrow().clone();
        // Root the replacement in the configured starting directory (which may
        // have just changed); add_terminal_tab falls back to $HOME.
        self.add_terminal_tab(profile.as_ref(), None);
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
                            let profile = imp.active_profile.borrow().clone();
                            imp.add_terminal_tab(profile.as_ref(), Some(&dir));
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
    fn add_terminal_tab(&self, profile: Option<&Profile>, dir_override: Option<&str>) {
        self.add_terminal_tab_resuming(profile, dir_override, None);
    }

    /// [`Self::add_terminal_tab`], optionally resuming `session_id`.
    fn add_terminal_tab_resuming(
        &self,
        profile: Option<&Profile>,
        dir_override: Option<&str>,
        session_id: Option<&str>,
    ) {
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
        let (search_bar, search_entry) = build_search_bar(&terminal);
        let tab_content = Box::builder().orientation(Orientation::Vertical).build();
        tab_content.append(&exit_bar);
        tab_content.append(&search_bar);
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
            search_bar,
            search_entry,
            profile: profile.map(|p| p.name.clone()),
            session_id: session_id.map(str::to_string),
        });

        self.spawn_session(&terminal, &stack, profile, &work_dir, session_id);
    }

    /// Applies theme, font, cursor, scrollback, and capability settings.
    fn configure_terminal(&self, terminal: &Terminal) {
        let config = self.config.borrow();
        Theme::apply(terminal, config.theme);
        apply_appearance(terminal, &config);

        terminal.set_scrollback_lines(i64::from(config.scrollback_lines));
        terminal.set_font_scale(config.font_scale);

        // Wrap around so the last match leads back to the first rather than
        // silently doing nothing.
        terminal.search_set_wrap_around(true);

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
                        obj.set_title(Some("Agent Terminal"));
                    } else {
                        obj.set_title(Some(&format!("Agent Terminal — {}", session_info)));
                    }
                };
            }
        ));

        // A CLI rings the bell when it wants attention — typically when a long
        // turn has finished. If that tab is not the one being looked at, mark it
        // so, which is the whole reason to use this over a general terminal.
        terminal.connect_bell(glib::clone!(
            #[weak]
            obj,
            #[weak]
            page,
            move |_| {
                let imp = obj.imp();
                let is_selected = imp
                    .tab_view
                    .borrow()
                    .as_ref()
                    .and_then(|view| view.selected_page())
                    .as_ref()
                    == Some(&page);
                if is_selected {
                    return;
                }

                debug!("Bell in a background tab; marking it as needing attention");
                page.set_needs_attention(true);
                imp.notify_bell(&page);
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

    /// Reopens the tabs from the previous window, if there are any.
    ///
    /// Returns whether anything was restored, so the caller can fall back to
    /// opening a single default tab. `fallback` covers a recorded profile that no
    /// longer resolves — a tab in the right directory is more useful than no tab.
    fn restore_previous_session(&self, fallback: Option<&Profile>) -> bool {
        if !self.config.borrow().restore_session {
            return false;
        }

        let state = crate::config::SessionState::load();
        if state.tabs.is_empty() {
            return false;
        }

        info!(
            "Restoring {} tab(s) from the previous session",
            state.tabs.len()
        );
        let profiles = self.config.borrow().profiles.clone();
        for tab in &state.tabs {
            let profile = tab
                .profile
                .as_ref()
                .and_then(|name| profiles.iter().find(|p| &p.name == name))
                .or(fallback);
            self.add_terminal_tab(profile, Some(&tab.dir));
        }

        if let Some(view) = self.tab_view.borrow().as_ref() {
            if state.selected < view.n_pages() as usize {
                view.set_selected_page(&view.nth_page(state.selected as i32));
            }
        }
        true
    }

    /// Records the open tabs so the next launch can reopen them.
    fn save_session(&self) {
        if !self.config.borrow().restore_session {
            return;
        }

        let selected = self
            .tab_view
            .borrow()
            .as_ref()
            .and_then(|view| view.selected_page())
            .and_then(|page| self.tabs.borrow().iter().position(|t| t.page == page))
            .unwrap_or(0);

        let tabs: Vec<crate::config::SessionTab> = self
            .tabs
            .borrow()
            .iter()
            .take(crate::config::SessionState::MAX_TABS)
            .map(|t| crate::config::SessionTab {
                profile: t.profile.clone(),
                dir: t.dir.clone(),
            })
            .collect();

        debug!("Saving {} tab(s) to the session file", tabs.len());
        crate::config::SessionState { tabs, selected }.save();
    }

    /// Optionally raises a desktop notification for a background tab's bell.
    ///
    /// Off by default: a notification per bell is intrusive if the CLI uses it
    /// for anything other than "I am finished". The in-window attention marker
    /// always applies and costs nothing.
    fn notify_bell(&self, page: &adw::TabPage) {
        if !self.config.borrow().notify_on_bell {
            return;
        }
        let Some(app) = self.obj().application() else {
            return;
        };

        let title = page.title();
        let notification = gtk4::gio::Notification::new("Session needs attention");
        notification.set_body(Some(&format!("{title} is waiting")));
        notification.set_priority(gtk4::gio::NotificationPriority::Normal);
        // One id, so repeated bells replace rather than stack up.
        app.send_notification(Some("agent-terminal-bell"), &notification);
    }

    /// Re-applies font and cursor settings to every open tab.
    fn apply_appearance_to_all(&self) {
        let config = self.config.borrow();
        self.for_each_terminal(|term| apply_appearance(term, &config));
    }

    /// Reveals the current tab's search bar and puts the cursor in it.
    fn toggle_search(&self) {
        let Some(page) = self
            .tab_view
            .borrow()
            .as_ref()
            .and_then(|view| view.selected_page())
        else {
            return;
        };
        let tabs = self.tabs.borrow();
        let Some(tab) = tabs.iter().find(|t| t.page == page) else {
            return;
        };

        let revealing = !tab.search_bar.is_search_mode();
        tab.search_bar.set_search_mode(revealing);
        if revealing {
            tab.search_entry.grab_focus();
        } else {
            // Drop the highlight so a dismissed search leaves no residue.
            tab.terminal.search_set_regex(None, 0);
            tab.terminal.grab_focus();
        }
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
        let (dir, session_id) = self
            .tabs
            .borrow()
            .iter()
            .find(|t| &t.page == page)
            .map(|t| (Some(t.dir.clone()), t.session_id.clone()))
            .unwrap_or_default();
        info!("Restarting session in {:?}", dir);
        match session_id {
            // A resumed tab resumes again, rather than trading the conversation
            // the user asked for for a blank one. Its directory is already known,
            // so this opens synchronously, before the old page closes. Checking
            // the profile first matters: if resume was switched off in config,
            // open_resume_tab would add nothing and closing would lose the tab.
            Some(session_id) if self.resume_profile().is_some() => {
                self.open_resume_tab(ResumeRequest { session_id, dir })
            }
            _ => {
                let profile = self.active_profile.borrow().clone();
                self.add_terminal_tab(profile.as_ref(), dir.as_deref());
            }
        }
        tab_view.close_page(page);
    }

    /// Attaches the right-click menu and the ctrl-click hyperlink handler.
    fn wire_input_controllers(&self, terminal: &Terminal) {
        // Context Menu (Right Click)
        let menu = gtk4::gio::Menu::new();
        menu.append(Some("New Tab"), Some("win.new-tab"));
        menu.append_submenu(Some("New Tab As"), &self.build_profile_menu());
        menu.append(Some("New Tab in Folder…"), Some("win.new-tab-folder"));
        menu.append(Some("Resume Session…"), Some("win.resume-session"));
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

        // Copy, paste and zoom used to live here, on a controller attached to the
        // terminal, which meant they went dead whenever focus was anywhere else
        // and were duplicated once per tab. They are application accelerators
        // bound to window actions now (see main.rs), leaving nothing that has to
        // be handled widget-locally.
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
        profile: Option<&Profile>,
        work_dir: &str,
        resume: Option<&str>,
    ) {
        let obj = self.obj();
        let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let command = get_startup_command(profile, resume);
        let env_file = profile.and_then(|p| p.env_file.clone());
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

        // Drop the launching agent session's own identity before the new session
        // sees it. Inheriting the environment wholesale is deliberate — it is how
        // nvm- and asdf-managed CLIs stay reachable — but when this terminal was
        // itself started from inside an agent session, the parent's session
        // markers come with it and the CLI we spawn concludes it is a nested
        // child. Stripping happens before the env_file merge below, so a profile
        // can deliberately put any of these back.
        let cleared = crate::utils::strip_env(&mut env_strs, &self.config.borrow().clear_env);
        if !cleared.is_empty() {
            info!(
                "Cleared {} inherited session variable(s) from the child environment: {}",
                cleared.len(),
                cleared.join(", ")
            );
        }

        if !env_strs.iter().any(|s| s.starts_with("TERM=")) {
            env_strs.push("TERM=xterm-256color".to_string());
        }
        if !env_strs.iter().any(|s| s.starts_with("COLORTERM=")) {
            env_strs.push("COLORTERM=truecolor".to_string());
        }
        // A profile's env_file is sourced in a subshell, which is a subprocess and
        // so must not run on the main thread. The terminal widget already exists
        // and is showing the loading screen, so deferring the spawn by one turn of
        // the loop costs nothing visible.
        let work_dir = work_dir.to_string();
        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            obj,
            #[weak]
            stack,
            #[weak]
            terminal,
            async move {
                if let Some(path) = env_file {
                    let extra =
                        gtk4::gio::spawn_blocking(move || crate::utils::load_env_file(&path))
                            .await
                            .unwrap_or_default();

                    // Later entries win in the environment block, so appending
                    // lets the profile override an inherited value.
                    for (key, value) in extra {
                        debug!("Env file sets {key}");
                        env_strs.push(format!("{key}={value}"));
                    }
                }

                let env_ptrs: Vec<&str> = env_strs.iter().map(String::as_str).collect();

                terminal.spawn_async(
                    PtyFlags::DEFAULT,
                    Some(&work_dir),
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
                                Ok(_) => {
                                    info!("Terminal process spawned, waiting for content...")
                                }
                                Err(err) => {
                                    error!("Error spawning terminal: {}", err);
                                    // Show the terminal anyway so the error is visible.
                                    stack.set_visible_child_name("terminal");
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
        ));
    }

    /// Sets up the welcome screen using AdwStatusPage.
    fn setup_welcome_ui(&self, container: &Box) {
        let obj = self.obj();

        // Nothing can run, so a queued resume cannot either; say so rather than
        // drop it silently.
        self.no_cli.set(true);
        let pending: Vec<ResumeRequest> = self.pending_resumes.take();
        if !pending.is_empty() {
            warn!(
                "Dropping {} resume request(s): no CLI detected",
                pending.len()
            );
            present_message(
                &obj,
                "Cannot Resume Session",
                "No AI CLI was found, so there is nothing to resume the session with.",
            );
        }

        // Name whatever is actually configured. The previous copy told every user
        // to install `agy` from antigravity.google even when they had explicitly
        // selected Claude.
        let description = {
            let config = self.config.borrow();
            match config.selected_profile() {
                Some(profile) => format!(
                    concat!(
                        "The profile \"{}\" is selected, but its command `{}` was not ",
                        "found. Your PATH, the usual install directories, and your ",
                        "interactive shell environment (-ic) were all checked.\n\n",
                        "Install it, or choose a different profile in Settings, then ",
                        "check again."
                    ),
                    profile.name, profile.command
                ),
                None => {
                    let commands: Vec<&str> =
                        config.profiles.iter().map(|p| p.command.as_str()).collect();
                    format!(
                        concat!(
                            "No configured AI CLI was found. Your PATH, the usual ",
                            "install directories, and your interactive shell ",
                            "environment (-ic) were all checked for: {}.\n\n",
                            "Install one of them, or add a profile to ",
                            "~/.config/agent-terminal/config.json, then check again."
                        ),
                        if commands.is_empty() {
                            "nothing — no profiles are configured".to_string()
                        } else {
                            commands.join(", ")
                        }
                    )
                }
            }
        };

        let status_page = adw::StatusPage::builder()
            .title("No AI CLI detected")
            .description(description)
            .icon_name("utilities-terminal-symbolic")
            .vexpand(true)
            .build();

        let refresh_button = Button::builder()
            .label("Check again")
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

        // Built from the configured profiles. Index 0 is auto-detection, so the
        // list is one longer than the profile list and every later index is
        // offset by one — see AUTO_INDEX below, which is the only place that
        // relationship is expressed.
        let mut client_names: Vec<String> = vec![AUTO_PROFILE_LABEL.to_string()];
        client_names.extend(config.profiles.iter().map(|p| p.name.clone()));
        let client_name_refs: Vec<&str> = client_names.iter().map(String::as_str).collect();
        let client_model = gtk4::StringList::new(&client_name_refs);
        let selected_index = config
            .default_profile
            .as_ref()
            .and_then(|name| config.profiles.iter().position(|p| &p.name == name))
            .map_or(AUTO_INDEX, |i| i as u32 + 1);

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

        // Font family and size were a hard-coded constant while the *scale* was a
        // setting, which is an odd place to have drawn the line.
        let font_button = gtk4::FontDialogButton::new(Some(gtk4::FontDialog::new()));
        font_button.set_font_desc(&gtk4::pango::FontDescription::from_string(&config.font));
        font_button.set_valign(gtk4::Align::Center);
        let font_row = adw::ActionRow::builder().title("Terminal Font").build();
        font_row.add_suffix(&font_button);

        let cursor_names: Vec<String> = crate::config::CursorShapeChoice::ALL
            .iter()
            .map(ToString::to_string)
            .collect();
        let cursor_name_refs: Vec<&str> = cursor_names.iter().map(String::as_str).collect();
        let cursor_row = adw::ComboRow::builder()
            .title("Cursor Shape")
            .model(&gtk4::StringList::new(&cursor_name_refs))
            .selected(
                crate::config::CursorShapeChoice::ALL
                    .iter()
                    .position(|c| *c == config.cursor_shape)
                    .unwrap_or(0) as u32,
            )
            .build();

        let blink_row = adw::SwitchRow::builder()
            .title("Blinking Cursor")
            .active(config.cursor_blink)
            .build();

        let notify_row = adw::SwitchRow::builder()
            .title("Notify on Session Bell")
            .subtitle("Raise a desktop notification when a background tab needs attention")
            .active(config.notify_on_bell)
            .build();

        let restore_row = adw::SwitchRow::builder()
            .title("Restore Tabs on Launch")
            .subtitle("Reopen the tabs that were open when the window last closed")
            .active(config.restore_session)
            .build();

        group.add(&starting_directory_row);
        group.add(&scrollback_row);
        group.add(&font_row);
        group.add(&font_scale_row);
        group.add(&cursor_row);
        group.add(&blink_row);
        group.add(&cli_client_row);
        group.add(&theme_row);
        group.add(&notify_row);
        group.add(&restore_row);
        page.add(&group);
        dialog.add(&page);

        font_button.connect_font_desc_notify(glib::clone!(
            #[weak]
            obj,
            move |button| {
                let Some(desc) = button.font_desc() else {
                    return;
                };
                let imp = obj.imp();
                imp.config.borrow_mut().font = desc.to_str().to_string();
                imp.apply_appearance_to_all();
                imp.schedule_config_save();
            }
        ));

        cursor_row.connect_selected_notify(glib::clone!(
            #[weak]
            obj,
            move |row| {
                let shape = crate::config::CursorShapeChoice::ALL
                    .get(row.selected() as usize)
                    .copied()
                    .unwrap_or_default();
                let imp = obj.imp();
                imp.config.borrow_mut().cursor_shape = shape;
                imp.apply_appearance_to_all();
                imp.schedule_config_save();
            }
        ));

        blink_row.connect_active_notify(glib::clone!(
            #[weak]
            obj,
            move |row| {
                let imp = obj.imp();
                imp.config.borrow_mut().cursor_blink = row.is_active();
                imp.apply_appearance_to_all();
                imp.schedule_config_save();
            }
        ));

        notify_row.connect_active_notify(glib::clone!(
            #[weak]
            obj,
            move |row| {
                let imp = obj.imp();
                imp.config.borrow_mut().notify_on_bell = row.is_active();
                imp.schedule_config_save();
            }
        ));

        restore_row.connect_active_notify(glib::clone!(
            #[weak]
            obj,
            move |row| {
                let imp = obj.imp();
                imp.config.borrow_mut().restore_session = row.is_active();
                imp.schedule_config_save();
            }
        ));

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
                let imp = obj.imp();
                let chosen = if row.selected() == AUTO_INDEX {
                    None
                } else {
                    let index = (row.selected() - 1) as usize;
                    match imp.config.borrow().profiles.get(index) {
                        Some(profile) => Some(profile.name.clone()),
                        // The list is rebuilt from the profiles each time the
                        // dialog opens, so this should not happen; do nothing
                        // rather than silently selecting something else.
                        None => {
                            warn!("Profile row {index} has no matching profile");
                            return;
                        }
                    }
                };

                if imp.config.borrow().default_profile == chosen {
                    return;
                }
                imp.config.borrow_mut().default_profile = chosen.clone();
                imp.schedule_config_save();
                info!(
                    "Profile selection changed to {}, restarting session",
                    chosen.as_deref().unwrap_or("auto-detect")
                );
                imp.restart_with_profile_selection();
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

    /// Re-resolves the selected profile, then replaces the active tab.
    ///
    /// Resolution can shell out, so it takes the same off-thread path as startup
    /// rather than freezing the window while the settings dialog is open.
    fn restart_with_profile_selection(&self) {
        let obj = self.obj();
        let (profiles, preferred) = {
            let config = self.config.borrow();
            (config.profiles.clone(), config.default_profile.clone())
        };
        let (path, home, shell) = env_triplet();
        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            obj,
            async move {
                let resolved = resolve_active_profile(profiles, preferred, path, home, shell).await;
                let imp = obj.imp();
                *imp.active_profile.borrow_mut() = resolved;
                imp.restart_current_tab();
            }
        ));
    }

    /// Opens a new tab running `profile`, rooted in that profile's directory when
    /// it names one.
    fn new_tab_with_profile(&self, profile: &Profile) {
        let dir = profile.dir.clone();
        info!("Opening a tab for profile '{}'", profile.name);
        self.add_terminal_tab(Some(profile), dir.as_deref());
    }

    /// Opens a tab resuming a session, or queues it until the window can.
    ///
    /// The entry point for the command line and the Resume dialog. A new window
    /// has no tab view until profile resolution finishes; the queue is drained
    /// by [`Self::setup_terminal_ui`] (or reported by the welcome screen).
    pub fn request_resume(&self, request: ResumeRequest) {
        if self.tab_view.borrow().is_some() {
            self.open_resume_tab(request);
        } else if self.no_cli.get() {
            warn!(
                "Cannot resume {}: no CLI detected in this window",
                request.session_id
            );
        } else {
            debug!(
                "Queueing resume of {} until the window is ready",
                request.session_id
            );
            self.pending_resumes.borrow_mut().push(request);
        }
    }

    /// The profile to resume with: the active one if it can, otherwise the
    /// first configured profile that can and is not known to be missing.
    fn resume_profile(&self) -> Option<Profile> {
        if let Some(active) = self.active_profile.borrow().as_ref() {
            if active.can_resume() {
                return Some(active.clone());
            }
        }
        self.config
            .borrow()
            .profiles
            .iter()
            // Only a command *known* to be missing is skipped. One not probed yet
            // is tried: if it is missing, the tab's exit bar says so.
            .find(|p| {
                p.can_resume() && crate::utils::cached_command_available(&p.command) != Some(false)
            })
            .cloned()
    }

    /// Opens a tab resuming `request.session_id`.
    ///
    /// Without an explicit directory, the profile's session store is searched
    /// off the main thread for where the session was recorded. A CLI that scopes
    /// sessions per project cannot find one from any other directory, and it
    /// says so only inside the tab, so a failed lookup is reported here.
    fn open_resume_tab(&self, request: ResumeRequest) {
        let ResumeRequest { session_id, dir } = request;
        let Some(profile) = self.resume_profile() else {
            warn!("No profile can resume session {session_id}");
            present_message(
                &self.obj(),
                "Cannot Resume Session",
                "No configured profile knows how to resume a session. Add \"resume_args\" \
                 to a profile in config.json, e.g. [\"--resume\", \"{id}\"].",
            );
            return;
        };

        let store = match (&dir, &profile.session_store) {
            (None, Some(store)) => store.clone(),
            // An explicit directory wins, and a profile without a store has
            // nothing to look up: use the profile or starting directory.
            _ => {
                info!(
                    "Resuming session {session_id} with profile '{}' in {:?}",
                    profile.name, dir
                );
                let dir = dir.or_else(|| profile.dir.clone());
                self.add_terminal_tab_resuming(Some(&profile), dir.as_deref(), Some(&session_id));
                return;
            }
        };

        let obj = self.obj();
        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            obj,
            async move {
                let lookup_id = session_id.clone();
                let found = gtk4::gio::spawn_blocking(move || {
                    crate::utils::find_session_dir(&store, &lookup_id)
                })
                .await
                .unwrap_or_else(|_| {
                    error!("Session lookup panicked on the worker thread");
                    None
                });

                // The window may have closed while the lookup ran. Spawning now
                // would start a CLI in a window nobody can see or reach.
                if !obj.is_visible() {
                    warn!("Window closed before session {session_id} could be resumed");
                    return;
                }
                let imp = obj.imp();
                let dir = match found {
                    Some(dir) => {
                        info!("Session {session_id} was recorded in {dir}");
                        Some(dir)
                    }
                    None => {
                        let fallback = profile.dir.clone();
                        present_message(
                            &obj,
                            "Session Directory Not Found",
                            &format!(
                                "Could not find where session {session_id} was recorded, so it \
                                 is being resumed from the default directory. If the CLI says \
                                 the conversation does not exist, reopen it with \
                                 `agent-terminal --resume {session_id} --dir <project>`."
                            ),
                        );
                        fallback
                    }
                };
                imp.add_terminal_tab_resuming(Some(&profile), dir.as_deref(), Some(&session_id));
            }
        ));
    }

    /// Lists the resumable sessions, newest first, and resumes the one picked.
    ///
    /// Falls back to [`Self::show_resume_id_dialog`] when the profile declares no
    /// session store, since there is then nothing to list. The store is read off
    /// the main thread; the dialog shows a spinner meanwhile.
    fn show_session_browser(&self) {
        let obj = self.obj();
        let Some(profile) = self.resume_profile() else {
            // Reuses open_resume_tab's explanation rather than a second copy.
            self.show_resume_id_dialog();
            return;
        };
        let Some(store) = profile.session_store.clone() else {
            self.show_resume_id_dialog();
            return;
        };
        let title_pointer = profile.session_title.clone();

        let dialog = adw::Dialog::builder()
            .title("Resume Session")
            .content_width(620)
            .content_height(560)
            .build();

        let header = adw::HeaderBar::new();
        let enter_id = Button::builder()
            .label("Enter ID…")
            .tooltip_text("Resume a session by pasting its ID")
            .build();
        header.pack_start(&enter_id);

        let search = gtk4::SearchEntry::builder()
            .placeholder_text("Search by title, folder or ID")
            .hexpand(true)
            .build();

        let list = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::None)
            .valign(Align::Start)
            .css_classes(["boxed-list"])
            .build();
        let scrolled = ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vexpand(true)
            .child(&list)
            .build();

        let spinner = gtk4::Spinner::builder()
            .spinning(true)
            .width_request(32)
            .height_request(32)
            .halign(Align::Center)
            .valign(Align::Center)
            .build();
        let empty = adw::StatusPage::builder()
            .icon_name("document-open-recent-symbolic")
            .title("No Sessions Found")
            .build();

        let pages = Stack::builder().vexpand(true).build();
        pages.add_named(&spinner, Some("loading"));
        pages.add_named(&scrolled, Some("list"));
        pages.add_named(&empty, Some("empty"));
        pages.set_visible_child_name("loading");

        let content = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(12)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        content.append(&search);
        content.append(&pages);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&content));
        dialog.set_child(Some(&toolbar));

        // Row index → session, and row index → lower-cased search text. Rows
        // are appended once and never reordered, and filtering hides rows
        // without changing their index, so the index is a stable key.
        let sessions: std::rc::Rc<RefCell<Vec<crate::utils::SessionSummary>>> =
            std::rc::Rc::default();
        let haystacks: std::rc::Rc<RefCell<Vec<String>>> = std::rc::Rc::default();

        list.set_filter_func(glib::clone!(
            #[weak]
            search,
            #[strong]
            haystacks,
            #[upgrade_or]
            true,
            move |row| {
                let query = search.text().to_lowercase();
                let query = query.trim();
                query.is_empty()
                    || usize::try_from(row.index())
                        .ok()
                        .and_then(|i| haystacks.borrow().get(i).cloned())
                        .is_some_and(|text| text.contains(query))
            }
        ));
        search.connect_search_changed(glib::clone!(
            #[weak]
            list,
            move |_| list.invalidate_filter()
        ));
        // Enter resumes the top match, so type-then-Enter works without a mouse.
        search.connect_activate(glib::clone!(
            #[weak]
            list,
            move |_| {
                let mut index = 0;
                while let Some(row) = list.row_at_index(index) {
                    if row.is_child_visible() {
                        row.activate();
                        return;
                    }
                    index += 1;
                }
            }
        ));

        list.connect_row_activated(glib::clone!(
            #[weak]
            obj,
            #[weak]
            dialog,
            #[strong]
            sessions,
            move |_, row| {
                let Some(session) = usize::try_from(row.index())
                    .ok()
                    .and_then(|i| sessions.borrow().get(i).cloned())
                else {
                    return;
                };
                dialog.close();
                // The directory is already known from the listing, so the tab
                // opens without a second lookup. When it is not known, None
                // takes the lookup path, which reports the failure properly.
                obj.imp().request_resume(ResumeRequest {
                    session_id: session.id,
                    dir: session.dir,
                });
            }
        ));

        enter_id.connect_clicked(glib::clone!(
            #[weak]
            obj,
            #[weak]
            dialog,
            move |_| {
                dialog.close();
                obj.imp().show_resume_id_dialog();
            }
        ));

        dialog.present(Some(obj.upcast_ref::<gtk4::Widget>()));
        search.grab_focus();

        // The weak refs upgrade once, when the task first runs, so a dialog
        // closed mid-scan stays alive until the scan finishes. That's bounded
        // (one scan, no cycle) and harmless: filling a closed dialog shows
        // nothing. The listing and every per-file summary run in this one
        // spawn_blocking, not lazily per row.
        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            list,
            #[weak]
            pages,
            #[weak]
            empty,
            async move {
                let listing = gtk4::gio::spawn_blocking(move || {
                    crate::utils::list_sessions(&store, title_pointer.as_deref())
                })
                .await
                .unwrap_or_else(|_| Err("Listing sessions panicked".to_string()));

                let found = match listing {
                    Ok(found) => found,
                    Err(reason) => {
                        warn!("Could not list sessions: {reason}");
                        // Distinct from an empty store: an unreadable one must
                        // not look like there is simply nothing to resume.
                        empty.set_icon_name(Some("dialog-warning-symbolic"));
                        empty.set_title("Could Not Read Sessions");
                        empty.set_description(Some(&reason));
                        pages.set_visible_child_name("empty");
                        return;
                    }
                };
                debug!("Listed {} session(s)", found.len());
                if found.is_empty() {
                    pages.set_visible_child_name("empty");
                    return;
                }

                let now = std::time::SystemTime::now();
                let home = env::var("HOME").unwrap_or_default();
                for session in &found {
                    let dir = session
                        .dir
                        .as_deref()
                        .map(|d| crate::utils::tildify(d, &home))
                        .unwrap_or_else(|| "folder unknown".to_string());
                    let age = now
                        .duration_since(session.modified)
                        .map(crate::utils::describe_age)
                        .unwrap_or_else(|_| "just now".to_string());
                    let title = session.title.as_deref().unwrap_or(&session.id);

                    let row = adw::ActionRow::builder()
                        .title(title)
                        .subtitle(format!("{dir} · {age}"))
                        // Titles and paths are data; & or < must not be
                        // read as Pango markup.
                        .use_markup(false)
                        .activatable(true)
                        .tooltip_text(&session.id)
                        .build();
                    row.add_suffix(&Image::from_icon_name("go-next-symbolic"));
                    list.append(&row);

                    haystacks
                        .borrow_mut()
                        .push(format!("{title}\n{dir}\n{}", session.id).to_lowercase());
                }
                *sessions.borrow_mut() = found;
                pages.set_visible_child_name("list");
            }
        ));
    }

    /// Prompts for a session ID, then opens a tab resuming it.
    ///
    /// The Resume button stays disabled until the ID is valid, so a bad paste is
    /// caught in the dialog rather than surfacing later as a failed tab.
    fn show_resume_id_dialog(&self) {
        let obj = self.obj();
        let entry = gtk4::Entry::builder()
            .placeholder_text("Session ID")
            .activates_default(true)
            .build();

        let dialog = adw::AlertDialog::new(
            Some("Resume Session"),
            Some("Opens a new tab that resumes the session with this ID."),
        );
        dialog.set_extra_child(Some(&entry));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("resume", "Resume");
        dialog.set_response_appearance("resume", adw::ResponseAppearance::Suggested);
        dialog.set_response_enabled("resume", false);
        dialog.set_default_response(Some("resume"));
        dialog.set_close_response("cancel");

        entry.connect_changed(glib::clone!(
            #[weak]
            dialog,
            move |entry| {
                let text = entry.text();
                let valid = crate::utils::validate_session_id(&text);
                dialog.set_response_enabled("resume", valid.is_ok());
                // Say why, but not for an empty field the user has not typed in yet.
                match valid {
                    Err(reason) if !text.is_empty() => {
                        entry.add_css_class("error");
                        entry.set_tooltip_text(Some(&reason));
                    }
                    _ => {
                        entry.remove_css_class("error");
                        entry.set_tooltip_text(None);
                    }
                }
            }
        ));

        dialog.connect_response(
            Some("resume"),
            glib::clone!(
                #[weak]
                obj,
                #[weak]
                entry,
                move |_, _| {
                    let text = entry.text();
                    match crate::utils::validate_session_id(&text) {
                        Ok(id) => obj.imp().request_resume(ResumeRequest {
                            session_id: id.to_string(),
                            dir: None,
                        }),
                        // Unreachable while the button tracks validity; logged,
                        // not trusted, in case Enter slips past it.
                        Err(reason) => warn!("Ignoring invalid session ID: {reason}"),
                    }
                }
            ),
        );

        dialog.present(Some(obj.upcast_ref::<gtk4::Widget>()));
        entry.grab_focus();
    }

    /// Builds the "new tab as…" menu, one item per configured profile.
    ///
    /// Rebuilt on demand rather than cached, so editing config.json and
    /// reopening the window is enough to see a new profile.
    fn build_profile_menu(&self) -> gtk4::gio::Menu {
        let menu = gtk4::gio::Menu::new();
        for profile in self.config.borrow().profiles.iter() {
            // The profile name is the action target, so the action handler does
            // not depend on menu ordering.
            let item = gtk4::gio::MenuItem::new(Some(&profile.name), None);
            item.set_action_and_target_value(
                Some("win.new-tab-profile"),
                Some(&profile.name.to_variant()),
            );
            menu.append_item(&item);
        }
        menu
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

        // Constructing a window loads (and may migrate) the configuration, which
        // without this pointed at the developer's real ~/.config and wrote to it
        // — a unit test with a side effect on the machine running it. Redirecting
        // XDG_CONFIG_HOME keeps it in a temp directory.
        //
        // Safe despite tests running in parallel: this is the only test that
        // reaches config_dir() at all, since the config tests all use explicit
        // paths via load_from/save_to.
        let config_home = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", config_home.path());

        let app = adw::Application::builder()
            .application_id("org.test.Window")
            .build();
        let window = super::super::AgentTerminalWindow::new(&app);

        assert_eq!(window.title(), Some("Agent Terminal".into()));

        // Prove the redirection actually took: configuration landed in the temp
        // directory rather than anywhere near the real one.
        assert!(
            config_home.path().join("agent-terminal").exists(),
            "window construction did not use the redirected config home"
        );
    }
}
