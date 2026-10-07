//! Private implementation details of the AgentTerminalWindow.

mod agents_prefs;
mod diff_prefs;
mod diffs;
mod thread_menu;
mod threads;

use super::diff_panel::DiffPanel;
use crate::config::{Profile, SessionFormat};
use crate::handoff::QuotaState;
use crate::theme::Theme;
use crate::utils::{get_startup_command, resolve_profile, resolve_working_directory, Launch};
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
    /// The ID this tab's new session was started under, for a profile that can
    /// pin one. Deliberately separate from `session_id`: restart does not
    /// resume it, because a session that died at launch never wrote the
    /// transcript a resume would need.
    pinned_id: Option<String>,
    /// Wall-clock launch time, to find a session whose ID the CLI chose itself.
    started_at: std::time::SystemTime,
    /// Offers a hand-off when the session runs out of quota.
    quota_banner: adw::Banner,
    quota: QuotaState,
    /// Set by every screen update and cleared when the screen is checked for
    /// quota markers, so an idle tab is never re-read.
    screen_dirty: std::rc::Rc<std::cell::Cell<bool>>,
    /// Unique for the life of the process, across windows, so a desktop
    /// notification can name this tab after it was sent.
    key: u64,
    /// Whether a quota notification is out for this tab, so a transcript that
    /// flickers to unreadable and back does not raise a second one.
    quota_notified: bool,
    /// Whether this tab's bell raised the (shared) bell notification and has
    /// not been looked at since. The notification is withdrawn once no tab in
    /// any window is still waiting. Tracked here rather than read from the
    /// page's attention marker, which the quota banner also sets.
    bell_pending: bool,
    /// When the terminal last showed new output. A turn is taken to have
    /// ended once this has been quiet for [`CHECKPOINT_QUIET`]. Kept apart
    /// from `screen_dirty`, which the quota poll consumes.
    last_output: std::rc::Rc<std::cell::Cell<std::time::Instant>>,
    checkpoint: CheckpointTrack,
    /// What changed in this tab's repository, beside the terminal.
    diff_panel: DiffPanel,
    /// Set for a tab opened by New Tab in Worktree, so closing it can offer
    /// to remove a worktree left clean.
    worktree: Option<crate::worktree::WorktreeInfo>,
    /// A chat thread page (3.0). `None`: a 2.x terminal page. For a thread, `terminal`
    /// is its drawer's shell.
    chat: Option<threads::ChatTab>,
}

/// How long a tab's output must be still before its turn counts as over and
/// its working tree is checkpointed. A CLI that redraws constantly (a clock in
/// its status line) never goes quiet; its bell is then the only trigger.
const CHECKPOINT_QUIET: std::time::Duration = std::time::Duration::from_secs(8);

/// After this long, an attempt whose result never arrived is treated as lost
/// and the tab may try again. Its result can go missing when the tab is
/// dragged between windows at the moment it lands. Comfortably above the
/// worst case of a snapshot: every git step at its timeout, plus the retry.
const CHECKPOINT_STALE: std::time::Duration = std::time::Duration::from_secs(120);

/// Per-tab checkpoint bookkeeping. See [`crate::git`].
#[derive(Default)]
struct CheckpointTrack {
    /// The `last_output` a checkpoint was last attempted for, so a quiet tab
    /// is snapshotted once rather than on every poll.
    attempted_for: Option<std::time::Instant>,
    /// When the attempt in flight started, if one is.
    started: Option<std::time::Instant>,
    /// Why the last attempt failed. Shown on the tab until one succeeds.
    error: Option<String>,
}

impl CheckpointTrack {
    fn running(&self) -> bool {
        self.started
            .is_some_and(|started| started.elapsed() < CHECKPOINT_STALE)
    }
}

/// What a checkpoint attempt came to, off the main thread.
enum CheckpointResult {
    NotRepo,
    Done(crate::git::CheckpointOutcome),
    Failed(String),
}

/// A tab's tooltip: the worktree branch it was opened on, if any, and its
/// latest checkpoint, if any.
fn tab_tooltip(branch: Option<&str>, checkpoint: Option<&str>) -> String {
    match (branch, checkpoint) {
        (Some(branch), Some(checkpoint)) => format!("⎇ {branch} · {checkpoint}"),
        (Some(branch), None) => format!("⎇ {branch}"),
        (None, Some(checkpoint)) => checkpoint.to_string(),
        (None, None) => String::new(),
    }
}

/// What a checkpoint result shows: the toast a manual request gets, and the
/// tab tooltip, which only a new checkpoint changes.
#[derive(Debug, PartialEq, Eq)]
struct CheckpointReport {
    toast: String,
    tooltip: Option<String>,
}

fn describe_checkpoint(result: &CheckpointResult, time: &str) -> CheckpointReport {
    use crate::git::CheckpointOutcome;
    let plain = |toast: &str| CheckpointReport {
        toast: toast.to_string(),
        tooltip: None,
    };
    match result {
        CheckpointResult::NotRepo => plain("Not a git repository: nothing to checkpoint"),
        CheckpointResult::Done(CheckpointOutcome::Busy) => {
            plain("Git is busy in this repository; try again shortly")
        }
        CheckpointResult::Done(CheckpointOutcome::Unchanged) => {
            plain("No changes since the last checkpoint")
        }
        CheckpointResult::Done(CheckpointOutcome::Created(cp, skipped)) => {
            let mut text = format!("Checkpoint {} · {time}", cp.seq);
            if !skipped.is_empty() {
                text.push_str(&format!(" · {} path(s) not captured", skipped.len()));
            }
            CheckpointReport {
                toast: text.clone(),
                tooltip: Some(text),
            }
        }
        CheckpointResult::Failed(err) => plain(&format!("Checkpoint failed: {err}")),
    }
}

/// Finds `dir`'s repository and checkpoints it. Blocking.
fn run_checkpoint(dir: &str, key: u64, label: &str) -> CheckpointResult {
    match crate::git::discover(std::path::Path::new(dir)) {
        Ok(None) => CheckpointResult::NotRepo,
        Ok(Some(repo)) => match crate::git::take_checkpoint(&repo, key, label) {
            Ok(outcome) => CheckpointResult::Done(outcome),
            Err(err) => CheckpointResult::Failed(err),
        },
        Err(err) => CheckpointResult::Failed(err),
    }
}

/// Source of [`TabState::key`].
static NEXT_TAB_KEY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Per-process offset for [`TabState::key`].
///
/// GNOME keeps a notification, and its target, across restarts of both the app
/// and the shell. Keys counting from 1 in every process would let a notification
/// left by a crashed run select, and hand off, whichever new tab reused its key.
static TAB_KEY_BASE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

fn next_tab_key() -> u64 {
    let base = *TAB_KEY_BASE.get_or_init(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64)
    });
    base.wrapping_add(NEXT_TAB_KEY.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

impl TabState {
    /// The session this tab is running, if the terminal knows it.
    fn known_session_id(&self) -> Option<&str> {
        self.session_id.as_deref().or(self.pinned_id.as_deref())
    }
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
    /// The profile to resume with, by name. `None` means the active profile,
    /// or the first that can resume. A session ID means nothing to any CLI
    /// but the one that recorded it, so callers that know which one say so.
    pub profile: Option<String>,
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
        .icon_name("at-go-up-symbolic")
        .tooltip_text("Previous match (Shift+Enter)")
        .build();
    let next = Button::builder()
        .icon_name("at-go-down-symbolic")
        .tooltip_text("Next match (Enter)")
        .build();

    let case_sensitive = gtk4::ToggleButton::builder()
        .icon_name("at-format-text-italic-symbolic")
        .tooltip_text("Match case")
        .build();
    let use_regex = gtk4::ToggleButton::builder()
        .icon_name("at-system-search-symbolic")
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
    // Weak: this closure is connected to the entry's and toggles' own signals,
    // so a strong capture of any of them is a cycle that keeps the whole tab —
    // terminal, scrollback and PTY — alive after it closes.
    let update = {
        let terminal = terminal.downgrade();
        let entry = entry.downgrade();
        let case_sensitive = case_sensitive.downgrade();
        let use_regex = use_regex.downgrade();
        move || {
            let (Some(terminal), Some(entry), Some(case_sensitive), Some(use_regex)) = (
                terminal.upgrade(),
                entry.upgrade(),
                case_sensitive.upgrade(),
                use_regex.upgrade(),
            ) else {
                return;
            };
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

thread_local! {
    /// The process's one copy of the settings. GTK is single-threaded, so
    /// thread-local is process-wide in practice.
    static SHARED_CONFIG: std::rc::Rc<RefCell<crate::config::TerminalConfig>> =
        std::rc::Rc::new(RefCell::new(initial_config()));

    /// Whether a window has already reopened the previous session. Only the
    /// first window of a launch does: a later "New Window" wants one fresh
    /// tab, not a second copy of the last session.
    static SESSION_RESTORED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };

    /// Tabs mid-way through a drag from one window's tab view to another's,
    /// between the source's `page-detached` and the target's `page-attached`.
    static TRANSFERRING: RefCell<Vec<TabState>> = const { RefCell::new(Vec::new()) };

    /// Counts profile re-resolutions, so a slow one that finishes after a
    /// newer one is discarded rather than applied last.
    static PROFILE_GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };

    /// Watches config.json for hand edits, once per process.
    static CONFIG_MONITOR: RefCell<Option<gtk4::gio::FileMonitor>> = const { RefCell::new(None) };
}

/// The settings a process starts with. Tests start from defaults: loading
/// would read — and could migrate into, or copy files beside — the real
/// ~/.config of whoever runs them.
#[cfg(not(test))]
fn initial_config() -> crate::config::TerminalConfig {
    crate::config::TerminalConfig::load()
}

#[cfg(test)]
fn initial_config() -> crate::config::TerminalConfig {
    crate::config::TerminalConfig::default()
}

/// The window a widget currently sits in.
///
/// Signal handlers on a tab's widgets look their window up through this
/// rather than capturing it: a tab can be dragged into another window, and a
/// captured one would keep answering for a tab it no longer holds.
fn window_of(widget: &impl IsA<gtk4::Widget>) -> Option<super::AgentTerminalWindow> {
    widget
        .as_ref()
        .root()
        .and_downcast::<super::AgentTerminalWindow>()
}

/// Settings shared by every window.
///
/// Each window used to load its own copy and save the whole of it, so a change
/// made in one window was reverted by the next save from any other — even a
/// zoom. Hand edits to config.json are picked up as they happen; see
/// [`AgentTerminalWindow::watch_config_file`].
pub struct SharedConfig(std::rc::Rc<RefCell<crate::config::TerminalConfig>>);

impl Default for SharedConfig {
    fn default() -> Self {
        Self(SHARED_CONFIG.with(std::rc::Rc::clone))
    }
}

impl std::ops::Deref for SharedConfig {
    type Target = RefCell<crate::config::TerminalConfig>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Internal state for the AgentTerminalWindow.
#[derive(Default)]
pub struct AgentTerminalWindow {
    pub header: RefCell<Option<adw::HeaderBar>>,
    pub window_title: RefCell<Option<adw::WindowTitle>>,
    pub tab_view: RefCell<Option<adw::TabView>>,
    /// One entry per open tab. Pruned when a page is detached.
    tabs: RefCell<Vec<TabState>>,
    pub config: SharedConfig,
    /// The settings dialog's font-scale spin, while it is open, so zooming
    /// from the keyboard keeps it in step instead of leaving it stale.
    font_scale_spin: RefCell<glib::WeakRef<gtk4::SpinButton>>,
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
    /// Set while a transcript quota check is running off-thread, so a slow
    /// disk cannot stack up overlapping checks.
    quota_poll_running: std::cell::Cell<bool>,
    /// Wraps the tab view, for short confirmations that need no dialog.
    toast_overlay: RefCell<Option<adw::ToastOverlay>>,
    /// The header's "New Thread With" submenu, refilled when agent detection finishes.
    new_with_menu: RefCell<Option<gtk4::gio::Menu>>,
    /// The stored threads as of the last load (archived included): the sidebar draws from this
    /// and never reads the store itself. Reloaded off the main thread when the store changes.
    summaries: RefCell<Option<Vec<agent_kit::store::ThreadSummary>>>,
    summaries_loading: std::cell::Cell<bool>,
    /// A change arrived while a reload was running: load again when it ends.
    summaries_stale: std::cell::Cell<bool>,
    /// The sidebar's "Show archived" toggle.
    show_archived: std::cell::Cell<bool>,
    /// A new thread asked for while agents were still being detected: (folder, prompt).
    pending_new_thread: RefCell<Option<(Option<String>, Option<String>)>>,
    /// The latest open-thread list not yet written, and whether a writer is running.
    open_threads_pending: RefCell<Option<String>>,
    open_threads_writing: std::cell::Cell<bool>,
    /// The thread sidebar beside the pages.
    split_view: RefCell<Option<adw::OverlaySplitView>>,
    sidebar: RefCell<Option<std::rc::Rc<threads::Sidebar>>>,
    /// The tab view, or the empty state when no page is open.
    pages_stack: RefCell<Option<gtk4::Stack>>,
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
        // Before any card is built, so its "Open in …" button starts right.
        crate::diff_tool::DiffTools::shared().set(self.config.borrow().diff_tool.clone());
        crate::diff_tool::DiffTools::shared()
            .set_expand_by_default(self.config.borrow().diffs_expanded);
        self.setup_ui();
        self.setup_actions();
        self.start_quota_watch();
        self.start_checkpoint_watch();
        self.watch_focus();
        self.report_config_problem();
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
        // Every live thread session closes its history out before the window (and, on the last
        // window, `terminate_all`) takes the processes down. The envelopes are stored before
        // they reach the sink, so the view going away does not lose them.
        self.shutdown_thread_sessions();
        // GNOME keeps a notification after its app exits, counted on the dock
        // badge until withdrawn, and nothing would be left to act on it.
        self.withdraw_notifications();
        self.parent_close_request()
    }
}
impl ApplicationWindowImpl for AgentTerminalWindow {}
impl AdwApplicationWindowImpl for AgentTerminalWindow {}

impl AgentTerminalWindow {
    /// Tells the user, once, that their settings file could not be used.
    /// Deferred to idle so the dialog has a presented window to attach to.
    fn report_config_problem(&self) {
        let Some(problem) = crate::config::take_load_problem() else {
            return;
        };
        let obj = self.obj();
        glib::idle_add_local_once(glib::clone!(
            #[weak]
            obj,
            move || present_message(&obj, "Settings Not Loaded", &problem)
        ));
    }

    /// Focusing the window acknowledges the bell of the tab in view, the same
    /// way selecting a tab does.
    fn watch_focus(&self) {
        self.obj().connect_is_active_notify(|window| {
            if !window.is_active() {
                return;
            }
            let imp = window.imp();
            let selected = imp
                .tab_view
                .borrow()
                .as_ref()
                .and_then(|view| view.selected_page());
            if let Some(page) = selected {
                imp.acknowledge_bell(&page);
            }
        });
    }

    /// Clears `page`'s pending bell, and withdraws the bell notification once
    /// no tab in any window is still waiting on it.
    fn acknowledge_bell(&self, page: &adw::TabPage) {
        if let Some(tab) = self.tabs.borrow_mut().iter_mut().find(|t| &t.page == page) {
            tab.bell_pending = false;
        }
        self.withdraw_bell_if_answered();
    }

    /// Withdraws the shared bell notification unless some tab, in any window,
    /// has rung and not been looked at.
    fn withdraw_bell_if_answered(&self) {
        let waiting = std::cell::Cell::new(false);
        self.for_each_window(|window| {
            if window.tabs.borrow().iter().any(|t| t.bell_pending) {
                waiting.set(true);
            }
        });
        if waiting.get() {
            return;
        }
        if let Some(app) = self.obj().application() {
            app.withdraw_notification("agent-terminal-bell");
        }
    }

    /// Watches config.json and applies hand edits as they are saved.
    ///
    /// Profiles and indicators have no settings UI, so editing the file while
    /// the app runs is the ordinary way to change them. Without this the app
    /// would keep its old copy and the next in-app save — a zoom — would
    /// write it back over the edit. Installed once per process.
    ///
    /// Called from the application's `startup`, not from a window: a window
    /// has no application yet while it is being constructed (GtkWindow's
    /// `application` is not a construct property), so installing it there
    /// silently never happened.
    pub fn watch_config_file(app: &adw::Application) {
        if CONFIG_MONITOR.with(|m| m.borrow().is_some()) {
            return;
        }
        let file = gtk4::gio::File::for_path(crate::config::TerminalConfig::config_path());
        let monitor = match file.monitor_file(
            gtk4::gio::FileMonitorFlags::WATCH_MOVES,
            None::<&gtk4::gio::Cancellable>,
        ) {
            Ok(monitor) => monitor,
            Err(err) => {
                warn!("Cannot watch the settings file for edits: {err}");
                return;
            }
        };

        // An editor's save arrives as several events (write, close, rename),
        // and a file read in the middle of one may not parse yet; act once
        // the burst has settled.
        let pending: std::rc::Rc<RefCell<Option<glib::SourceId>>> = std::rc::Rc::default();
        monitor.connect_changed(glib::clone!(
            #[weak]
            app,
            move |_, _, _, _| {
                if let Some(id) = pending.borrow_mut().take() {
                    id.remove();
                }
                let source = glib::timeout_add_local_once(
                    std::time::Duration::from_millis(300),
                    glib::clone!(
                        #[weak]
                        app,
                        #[strong]
                        pending,
                        move || {
                            pending.replace(None);
                            let window = app
                                .windows()
                                .into_iter()
                                .find_map(|w| w.downcast::<super::AgentTerminalWindow>().ok());
                            if let Some(window) = window {
                                window.imp().reload_config_from_disk();
                            }
                        }
                    ),
                );
                pending.replace(Some(source));
            }
        ));
        CONFIG_MONITOR.with(|m| *m.borrow_mut() = Some(monitor));
    }

    /// Applies config.json as it now stands on disk, if something other than
    /// this process changed it.
    fn reload_config_from_disk(&self) {
        let change = self.config.borrow().check_disk();
        let new = match change {
            crate::config::DiskChange::Unchanged => return,
            crate::config::DiskChange::Invalid(problem) => {
                warn!("{problem}");
                // An editor that autosaves mid-edit produces a run of broken
                // versions; one dialog at a time is enough.
                if self.obj().visible_dialog().is_none() {
                    present_message(&self.obj(), "Settings File Has an Error", &problem);
                }
                return;
            }
            crate::config::DiskChange::Updated(new) => new,
        };

        info!("Settings file changed on disk; applying it");
        let profiles_changed = {
            let old = self.config.borrow();
            old.default_profile != new.default_profile || old.profiles != new.profiles
        };
        *self.config.borrow_mut() = *new;
        crate::diff_tool::DiffTools::shared().set(self.config.borrow().diff_tool.clone());
        crate::diff_tool::DiffTools::shared()
            .set_expand_by_default(self.config.borrow().diffs_expanded);

        let (theme, scrollback, scale) = {
            let config = self.config.borrow();
            (config.theme, config.scrollback_lines, config.font_scale)
        };
        self.apply_appearance_to_all();
        self.for_each_terminal_everywhere(|term| {
            Theme::apply(term, theme);
            term.set_scrollback_lines(i64::from(scrollback));
            term.set_font_scale(scale);
        });
        self.recolour_diff_panels(theme);
        // An open Settings dialog's font-scale spin follows too; its other
        // rows show the old values until reopened.
        self.for_each_window(|window| {
            if let Some(spin) = window.font_scale_spin.borrow().upgrade() {
                spin.set_value(scale);
            }
        });
        if profiles_changed {
            // Ceiling: the header's profile menus and the indicators are built
            // with the window; new windows show the edited lists.
            self.refresh_profile_selection();
        }
    }

    /// Returns the terminal of the currently selected tab, if any.
    fn current_terminal(&self) -> Option<Terminal> {
        let page = self.tab_view.borrow().as_ref()?.selected_page()?;
        self.tabs
            .borrow()
            .iter()
            .find(|t| t.page == page)
            // A thread's drawer shell counts only while it has the keyboard: copy, paste and
            // search otherwise belong to the chat.
            .filter(|t| t.chat.is_none() || t.terminal.has_focus())
            .map(|t| t.terminal.clone())
    }

    /// Applies a closure to every open tab's terminal.
    fn for_each_terminal(&self, f: impl Fn(&Terminal)) {
        for tab in self.tabs.borrow().iter() {
            f(&tab.terminal);
        }
    }

    /// Applies a closure to every window of the application, this one included.
    /// Settings are shared, so a change made in one window applies to all.
    fn for_each_window(&self, f: impl Fn(&Self)) {
        let Some(app) = self.obj().application() else {
            f(self);
            return;
        };
        for window in app.windows() {
            if let Ok(window) = window.downcast::<super::AgentTerminalWindow>() {
                f(window.imp());
            }
        }
    }

    /// Applies a closure to every tab's terminal in every window.
    fn for_each_terminal_everywhere(&self, f: impl Fn(&Terminal)) {
        self.for_each_window(|window| window.for_each_terminal(&f));
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
                } else if let Some(focus) = gtk4::prelude::GtkWindowExt::focus(&obj) {
                    let _ = focus.activate_action("clipboard.copy", None);
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
                } else if let Some(focus) = gtk4::prelude::GtkWindowExt::focus(&obj) {
                    let _ = focus.activate_action("clipboard.paste", None);
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
                // Through the same door as the sidebar's X, so a running thread asks first.
                let imp = obj.imp();
                if let Some(key) = imp.selected_row_key() {
                    imp.close_row(&key);
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

        // New Tab As <profile> and New Tab in Folder As <profile>. Parameterised
        // by profile name rather than index, so the menus stay correct if the
        // profile list changes underneath them.
        for (name, in_folder) in [("new-tab-profile", false), ("new-tab-folder-profile", true)] {
            let action = gtk4::gio::SimpleAction::new(name, Some(&String::static_variant_type()));
            action.connect_activate(glib::clone!(
                #[weak]
                obj,
                move |_, target| {
                    let Some(profile_name) = target.and_then(|t| t.get::<String>()) else {
                        warn!("{name} activated without a profile name");
                        return;
                    };
                    let imp = obj.imp();
                    let profile = imp
                        .config
                        .borrow()
                        .profiles
                        .iter()
                        .find(|p| p.name == profile_name)
                        .cloned();
                    match profile {
                        Some(profile) if in_folder => imp.new_tab_in_folder(Some(profile)),
                        Some(profile) => imp.new_tab_with_profile(&profile),
                        None => warn!("No profile named '{profile_name}'"),
                    }
                }
            ));
            obj.add_action(&action);
        }

        // New Tab in Worktree, as the active profile or a named one.
        let new_tab_worktree = gtk4::gio::SimpleAction::new("new-tab-worktree", None);
        new_tab_worktree.connect_activate(glib::clone!(
            #[weak]
            obj,
            move |_, _| {
                debug!("Action: New Tab in Worktree");
                obj.imp().new_tab_in_worktree(None);
            }
        ));
        obj.add_action(&new_tab_worktree);
        let new_tab_worktree_as = gtk4::gio::SimpleAction::new(
            "new-tab-worktree-profile",
            Some(&String::static_variant_type()),
        );
        new_tab_worktree_as.connect_activate(glib::clone!(
            #[weak]
            obj,
            move |_, target| {
                let Some(profile_name) = target.and_then(|t| t.get::<String>()) else {
                    warn!("new-tab-worktree-profile activated without a profile name");
                    return;
                };
                let imp = obj.imp();
                let profile = imp
                    .config
                    .borrow()
                    .profiles
                    .iter()
                    .find(|p| p.name == profile_name)
                    .cloned();
                match profile {
                    Some(profile) => imp.new_tab_in_worktree(Some(profile)),
                    None => warn!("No profile named '{profile_name}'"),
                }
            }
        ));
        obj.add_action(&new_tab_worktree_as);

        // Ctrl+Alt+1..9 open the Nth profile, so switching CLI (e.g. to Gemini
        // when Claude runs out of tokens) needs no menu. Indexed because an
        // accelerator is bound once per app, before any profile list is known.
        let new_tab_profile_at =
            gtk4::gio::SimpleAction::new("new-tab-profile-at", Some(&i32::static_variant_type()));
        new_tab_profile_at.connect_activate(glib::clone!(
            #[weak]
            obj,
            move |_, target| {
                let Some(index) = target.and_then(|t| t.get::<i32>()) else {
                    return;
                };
                let imp = obj.imp();
                let profile = usize::try_from(index)
                    .ok()
                    .and_then(|i| imp.config.borrow().profiles.get(i).cloned());
                match profile {
                    Some(profile) => imp.new_tab_with_profile(&profile),
                    None => debug!("No profile at position {index}"),
                }
            }
        ));
        obj.add_action(&new_tab_profile_at);

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

        // Checkpoint the current tab's working tree now, whatever the setting:
        // asking is an explicit choice.
        let checkpoint_action = gtk4::gio::SimpleAction::new("checkpoint-now", None);
        checkpoint_action.connect_activate(glib::clone!(
            #[weak]
            obj,
            move |_, _| {
                debug!("Action: Checkpoint Now");
                let imp = obj.imp();
                let page = imp
                    .tab_view
                    .borrow()
                    .as_ref()
                    .and_then(|v| v.selected_page());
                if let Some(page) = page {
                    imp.request_checkpoint(&page, true);
                }
            }
        ));
        obj.add_action(&checkpoint_action);

        let toggle_diff = gtk4::gio::SimpleAction::new("toggle-diff", None);
        toggle_diff.connect_activate(glib::clone!(
            #[weak]
            obj,
            move |_, _| {
                debug!("Action: Toggle Diff Panel");
                obj.imp().toggle_diff_panel();
            }
        ));
        obj.add_action(&toggle_diff);

        // 3.0: the thread sidebar and a thread's terminal drawer.
        let toggle_sidebar = gtk4::gio::SimpleAction::new("toggle-sidebar", None);
        toggle_sidebar.connect_activate(glib::clone!(
            #[weak]
            obj,
            move |_, _| obj.imp().toggle_sidebar()
        ));
        obj.add_action(&toggle_sidebar);
        let toggle_drawer = gtk4::gio::SimpleAction::new("toggle-drawer", None);
        toggle_drawer.connect_activate(glib::clone!(
            #[weak]
            obj,
            move |_, _| obj.imp().toggle_drawer()
        ));
        obj.add_action(&toggle_drawer);

        // New Thread With <agent>, by driver key.
        let new_thread_agent =
            gtk4::gio::SimpleAction::new("new-thread-agent", Some(&String::static_variant_type()));
        new_thread_agent.connect_activate(glib::clone!(
            #[weak]
            obj,
            move |_, target| {
                let driver = target
                    .and_then(|t| t.get::<String>())
                    .and_then(|k| crate::window::sidebar_model::parse_driver(&k));
                let imp = obj.imp();
                imp.new_chat_thread(driver, imp.current_dir(), None);
            }
        ));
        obj.add_action(&new_thread_agent);

        // New Terminal Thread as <profile>: the 2.x terminal page, for any profile.
        let new_terminal = gtk4::gio::SimpleAction::new(
            "new-terminal-profile",
            Some(&String::static_variant_type()),
        );
        new_terminal.connect_activate(glib::clone!(
            #[weak]
            obj,
            move |_, target| {
                let Some(name) = target.and_then(|t| t.get::<String>()) else {
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
                    Some(profile) => {
                        let dir = profile_tab_dir(profile.dir.as_deref(), imp.current_dir());
                        imp.add_terminal_tab(Some(&profile), dir.as_deref());
                    }
                    None => warn!("No profile named '{name}'"),
                }
            }
        ));
        obj.add_action(&new_terminal);

        // New Tab in Folder Action (opens a folder picker)
        let new_tab_folder_action = gtk4::gio::SimpleAction::new("new-tab-folder", None);
        new_tab_folder_action.connect_activate(glib::clone!(
            #[weak]
            obj,
            move |_, _| {
                debug!("Action: New Tab in Folder");
                obj.imp().new_tab_in_folder(None);
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
                obj.imp().show_session_browser(None);
            }
        ));
        obj.add_action(&resume_action);

        // Both targeted by profile name, like the new-tab-as actions.
        for name in ["resume-session-profile", "continue-in"] {
            let action = gtk4::gio::SimpleAction::new(name, Some(&String::static_variant_type()));
            action.connect_activate(glib::clone!(
                #[weak]
                obj,
                move |_, target| {
                    let Some(profile_name) = target.and_then(|t| t.get::<String>()) else {
                        warn!("{name} activated without a profile name");
                        return;
                    };
                    debug!("Action: {name} {profile_name}");
                    let imp = obj.imp();
                    if name == "continue-in" {
                        imp.continue_in(&profile_name);
                    } else {
                        imp.show_session_browser(Some(&profile_name));
                    }
                }
            ));
            obj.add_action(&action);
        }
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
        // Weak, so the refresh timer ends with the window instead of keeping its
        // button alive and running the check forever after the window closes.
        let button = button.downgrade();
        // One check at a time: a check slower than the refresh interval would
        // otherwise overlap the next, and the older result could land last.
        let in_flight = std::rc::Rc::new(std::cell::Cell::new(false));
        let evaluate = move || -> bool {
            let Some(button) = button.upgrade() else {
                return false;
            };
            if in_flight.replace(true) {
                return true;
            }
            let indicator = indicator.clone();
            let detail = detail.clone();
            let in_flight = in_flight.clone();
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
                in_flight.set(false);

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
            true
        };

        evaluate();

        // Re-check on an interval where one is configured. The predecessor read
        // its source once at window construction and never again, so a drift that
        // appeared later was never shown.
        if let Some(secs) = refresh.filter(|s| *s > 0) {
            glib::timeout_add_local(std::time::Duration::from_secs(secs), move || {
                if evaluate() {
                    glib::ControlFlow::Continue
                } else {
                    glib::ControlFlow::Break
                }
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
            .icon_name("at-document-properties-symbolic")
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

        // Chat-first: the sidebar navigates; the tab view is the hidden page stack, with no
        // tab bar. "New" in the header is a split button: click opens a thread on the default
        // agent in the current folder; the menu has every other way to start one.
        if let Some(header) = self.header.borrow().as_ref() {
            let new_btn = adw::SplitButton::builder()
                .icon_name("at-list-add-symbolic")
                .tooltip_text("New Thread (Ctrl+Shift+T)")
                .menu_model(&self.build_new_menu())
                .build();
            new_btn.connect_clicked(glib::clone!(
                #[weak]
                obj,
                move |_| {
                    obj.imp().new_tab();
                }
            ));
            header.pack_start(&new_btn);

            let drawer_btn = Button::builder()
                .icon_name("at-utilities-terminal-symbolic")
                .tooltip_text("Terminal Drawer (Ctrl+`)")
                .action_name("win.toggle-drawer")
                .build();
            let diff_btn = Button::builder()
                .icon_name("at-view-dual-symbolic")
                .tooltip_text("Show or Hide Changes (Ctrl+Shift+D)")
                .action_name("win.toggle-diff")
                .build();
            header.pack_end(&diff_btn);
            header.pack_end(&drawer_btn);
        }
        self.setup_shell(container, &tab_view);

        // Immediately confirm tab closures (no unsaved-state prompt for a terminal).
        tab_view.connect_close_page(|view, page| {
            view.close_page_finish(page, true);
            glib::Propagation::Stop // the closure handled the close request
        });

        // The last page closing leaves the empty state (see `setup_shell`), not a closed
        // window: the threads are still in the sidebar.

        // Forget a tab's tracked state when it is removed — or, when it is being
        // dragged to another window, park it for that window to pick up.
        tab_view.connect_page_detached(glib::clone!(
            #[weak]
            obj,
            move |view, page, _| {
                let imp = obj.imp();
                let (leaving, staying): (Vec<TabState>, Vec<TabState>) =
                    imp.tabs.take().into_iter().partition(|t| &t.page == page);
                imp.tabs.replace(staying);

                if view.is_transferring_page() {
                    TRANSFERRING.with(|parked| parked.borrow_mut().extend(leaving));
                    return;
                }
                // A notification offering to hand off a closed tab would do
                // nothing when clicked, and a closed tab's bell cannot be
                // looked at any more.
                for tab in &leaving {
                    // A closed thread page ends its session on purpose, so the stored history
                    // closes out (open items, approvals, the turn) instead of ending open.
                    if let Some(chat) = &tab.chat {
                        if let Some(session) = chat.slot.get() {
                            session.shutdown();
                        }
                        imp.withdraw_thread_notifications(&chat.thread);
                    }
                    imp.withdraw_quota_notification(tab.key);
                    if let Some(worktree) = &tab.worktree {
                        imp.offer_worktree_removal(worktree.clone());
                    }
                }
                imp.withdraw_bell_if_answered();
            }
        ));

        // A tab dragged in from another window: adopt the state that window
        // parked. Its signal handlers find their window through the widget
        // tree (window_of), so they now answer to this one.
        tab_view.connect_page_attached(glib::clone!(
            #[weak]
            obj,
            move |_, page, _| {
                let adopted: Vec<TabState> = TRANSFERRING.with(|parked| {
                    let (mine, others) = parked
                        .take()
                        .into_iter()
                        .partition(|t: &TabState| &t.page == page);
                    parked.replace(others);
                    mine
                });
                if !adopted.is_empty() {
                    debug!("Adopting a tab dragged in from another window");
                    obj.imp().tabs.borrow_mut().extend(adopted);
                }
            }
        ));

        // Keep the header title in sync with the active tab.
        tab_view.connect_selected_page_notify(glib::clone!(
            #[weak]
            obj,
            move |view| {
                let imp = obj.imp();
                // Looking at a tab is the acknowledgement, so clear its marker
                // and its bell. GNOME keeps a notification, and the dock's
                // unread badge, until the app withdraws it — even across
                // restarts — so one never withdrawn stays counted forever. At
                // startup nothing is pending, so this also clears a notification
                // a previous run left behind.
                if let Some(page) = view.selected_page() {
                    page.set_needs_attention(false);
                    imp.acknowledge_bell(&page);
                    // A thread page titles the window itself (title and folder).
                    if imp
                        .tabs
                        .borrow()
                        .iter()
                        .any(|t| t.page == page && t.chat.is_some())
                    {
                        return;
                    }
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
        // 2.x terminal pages come back as before; threads open as they were left. Nothing
        // restored shows the empty state rather than a blank thread nobody asked for.
        let pending: Vec<ResumeRequest> = self.pending_resumes.take();
        self.restore_previous_session(profile);
        self.restore_open_threads();
        for request in pending {
            self.open_resume_tab(request);
        }
        self.refresh_sidebar();
    }

    /// (Re)fills "New Thread With" with the agents that are installed and enabled. Detection is
    /// asynchronous, so this runs again when it finishes.
    pub(super) fn fill_new_with_menu(&self) {
        let Some(with) = self.new_with_menu.borrow().clone() else {
            return;
        };
        with.remove_all();
        let ready: Vec<_> = agent_core::adapter::Driver::ALL
            .into_iter()
            .filter(|d| self.agent_usable(*d))
            .collect();
        if ready.is_empty() {
            // An item whose action does not exist is drawn insensitive.
            let label = if crate::availability::AgentAvailability::shared().any_detecting() {
                "Detecting agents…"
            } else {
                "No agent installed and enabled"
            };
            with.append_item(&gtk4::gio::MenuItem::new(Some(label), Some("win.no-agent")));
        }
        for driver in ready {
            let item = gtk4::gio::MenuItem::new(
                Some(crate::window::sidebar_model::driver_label(driver)),
                None,
            );
            item.set_action_and_target_value(
                Some("win.new-thread-agent"),
                Some(&crate::window::sidebar_model::driver_key(driver).to_variant()),
            );
            with.append_item(&item);
        }
    }

    /// The header's New menu: threads first, then resume and hand-off, then terminal pages.
    fn build_new_menu(&self) -> gtk4::gio::Menu {
        let menu = gtk4::gio::Menu::new();
        let threads = gtk4::gio::Menu::new();
        threads.append(Some("New Thread"), Some("win.new-tab"));
        let with = gtk4::gio::Menu::new();
        *self.new_with_menu.borrow_mut() = Some(with.clone());
        self.fill_new_with_menu();
        threads.append_submenu(Some("New Thread With"), &with);
        threads.append(Some("New Thread in Folder…"), Some("win.new-tab-folder"));
        threads.append(
            Some("New Thread in Worktree…"),
            Some("win.new-tab-worktree"),
        );
        menu.append_section(None, &threads);
        menu.append_section(None, &self.build_session_section());
        let terminal = gtk4::gio::Menu::new();
        terminal.append_submenu(
            Some("New Terminal Thread"),
            &self.build_profile_menu("win.new-terminal-profile", |_| true),
        );
        menu.append_section(None, &terminal);
        menu
    }

    /// Opens a new tab rooted in the current tab's directory (fast path).
    fn new_tab(&self) {
        self.new_chat_thread(None, self.current_dir(), None);
    }

    /// Prompts for a folder, then opens a new tab rooted there running `profile`,
    /// or the active profile when `None`.
    fn new_tab_in_folder(&self, profile: Option<Profile>) {
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
                            match profile {
                                // A profile with no chat adapter runs as a terminal page.
                                Some(p) if crate::config::profile_driver(&p).is_none() => {
                                    imp.add_terminal_tab(Some(&p), Some(&dir));
                                }
                                p => imp.new_chat_thread(
                                    p.as_ref().and_then(crate::config::profile_driver),
                                    Some(dir),
                                    None,
                                ),
                            }
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
        self.add_terminal_tab_launching(profile, dir_override, Launch::Fresh);
    }

    /// [`Self::add_terminal_tab`], resuming a session or starting one with a
    /// prompt as `launch` says.
    ///
    /// A new session on a profile that can pin its ID gets a fresh UUID, so the
    /// tab knows its own transcript from the start — which quota detection and
    /// the hand-off brief both depend on.
    fn add_terminal_tab_launching(
        &self,
        profile: Option<&Profile>,
        dir_override: Option<&str>,
        launch: Launch<'_>,
    ) {
        let pinned_id = match launch {
            Launch::Resume(_) => None,
            _ => profile
                .filter(|p| p.can_pin_session_id())
                .map(|_| glib::uuid_string_random().to_string()),
        };
        let launch = match launch {
            Launch::Fresh if pinned_id.is_some() => Launch::New {
                session_id: pinned_id.as_deref(),
                prompt: None,
            },
            Launch::New { prompt, .. } => Launch::New {
                session_id: pinned_id.as_deref(),
                prompt,
            },
            other => other,
        };
        let session_id = match launch {
            Launch::Resume(id) => Some(id),
            _ => None,
        };
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
        // Titles quote the CLI's own message, which is data, not markup. The
        // button targets win.continue-in; its profile is set when revealed. The
        // empty placeholder target is only there so the action's string type
        // matches: with none, GTK logged a type-mismatch warning per banner.
        // The button has no label, and so is not shown, until a real one is set.
        let quota_banner = adw::Banner::builder()
            .use_markup(false)
            .action_name("win.continue-in")
            .action_target(&"".to_variant())
            .build();
        let screen_dirty = std::rc::Rc::new(std::cell::Cell::new(true));
        let last_output = std::rc::Rc::new(std::cell::Cell::new(std::time::Instant::now()));
        terminal.connect_contents_changed(glib::clone!(
            #[strong]
            screen_dirty,
            #[strong]
            last_output,
            move |_| {
                screen_dirty.set(true);
                last_output.set(std::time::Instant::now());
            }
        ));
        // The terminal and, beside it, the tab's diff panel. The panel keeps
        // its width as the window resizes; the terminal takes the rest.
        let (diff_panel, paned) = {
            let config = self.config.borrow();
            let panel = DiffPanel::new(&Theme::diff_colours(config.theme));
            panel.set_shown(config.diff_panel_visible);
            let paned = gtk4::Paned::builder()
                .orientation(Orientation::Horizontal)
                .start_child(&stack)
                .end_child(&panel.root)
                .resize_start_child(true)
                .resize_end_child(false)
                .shrink_end_child(false)
                .vexpand(true)
                .build();
            (panel, paned)
        };
        self.wire_diff_panel(&diff_panel, &paned);
        let tab_content = Box::builder().orientation(Orientation::Vertical).build();
        tab_content.append(&quota_banner);
        tab_content.append(&exit_bar);
        tab_content.append(&search_bar);
        tab_content.append(&paned);

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

        // Window looked up at click time: the tab may have been dragged to
        // another window since.
        restart_btn.connect_clicked(glib::clone!(
            #[weak]
            page,
            move |button| {
                if let Some(obj) = window_of(button) {
                    obj.imp().restart_tab(&page);
                }
            }
        ));
        close_btn.connect_clicked(glib::clone!(
            #[weak]
            page,
            move |button| {
                let Some(obj) = window_of(button) else {
                    return;
                };
                let view = obj.imp().tab_view.borrow().clone();
                if let Some(view) = view {
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
            pinned_id: pinned_id.clone(),
            started_at: std::time::SystemTime::now(),
            quota_banner,
            quota: QuotaState::Unknown,
            screen_dirty,
            key: next_tab_key(),
            quota_notified: false,
            bell_pending: false,
            last_output,
            checkpoint: CheckpointTrack::default(),
            diff_panel: diff_panel.clone(),
            worktree: None,
            chat: None,
        });

        self.spawn_session(&terminal, &stack, profile, &work_dir, launch);
        if diff_panel.root.is_visible() {
            self.refresh_diff(&page);
        }
        // Selected before it was registered: list it and title the window now.
        self.page_shown(&page);
        self.refresh_sidebar();
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
    ///
    /// Each handler finds its window through [`window_of`] when it runs, not by
    /// capture, so a tab dragged into another window reports to that one.
    fn wire_tab_signals(&self, terminal: &Terminal, page: &adw::TabPage) {
        // Terminal title drives the tab label and (when active) the window title.
        terminal.connect_window_title_changed(glib::clone!(
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
                let Some(obj) = window_of(terminal) else {
                    return;
                };
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
            page,
            move |terminal| {
                let Some(obj) = window_of(terminal) else {
                    return;
                };
                let imp = obj.imp();
                // Info, not debug: which CLIs ring at the end of a turn decides
                // whether the bell or the quiet timer drives their checkpoints.
                // Demote to debug once that is recorded (plan WP0.1, F4/F5).
                info!("Bell in tab \"{}\"", page.title());
                if imp.config.borrow().checkpoints {
                    imp.request_checkpoint(&page, false);
                }
                let is_selected = imp
                    .tab_view
                    .borrow()
                    .as_ref()
                    .and_then(|view| view.selected_page())
                    .as_ref()
                    == Some(&page);
                // The tab in view of a focused window is already being looked
                // at. The tab in view of a window in the background is not:
                // that is the "finished while I was elsewhere" case the
                // notification exists for.
                if is_selected && obj.is_active() {
                    return;
                }

                // Only a background tab gets the marker: on the tab in view it
                // would stay lit until the user switched away and back.
                if !is_selected {
                    debug!("Bell in a background tab; marking it as needing attention");
                    page.set_needs_attention(true);
                }
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
            page,
            move |terminal, status| {
                info!("Terminal child exited: {}", describe_exit(status));
                let Some(obj) = window_of(terminal) else {
                    return;
                };
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
        // Once per launch: later windows open one fresh tab.
        if SESSION_RESTORED.with(|done| done.replace(true)) {
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
        let mut missing = Vec::new();
        for tab in &state.tabs {
            let profile = tab
                .profile
                .as_ref()
                .and_then(|name| profiles.iter().find(|p| &p.name == name))
                .or(fallback);
            // A folder can be gone since — a removed worktree, most often. The
            // tab still opens, in the starting folder, but not silently.
            if !std::path::Path::new(&tab.dir).is_dir() {
                missing.push(tab.dir.clone());
            }
            self.add_terminal_tab(profile, Some(&tab.dir));
        }
        if let Some(first) = missing.first() {
            let others = missing.len() - 1;
            let more = if others == 0 {
                String::new()
            } else {
                format!(" and {others} more")
            };
            warn!(
                "Restored tab folder(s) no longer exist: {}",
                missing.join(", ")
            );
            self.show_toast(&format!(
                "{first}{more} no longer exists; opened in your home folder instead"
            ));
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
            .filter(|t| t.chat.is_none())
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
        // Without a default action, clicking it plainly activates the app,
        // which opens a second window rather than showing this tab.
        // Marked pending under the borrow, sent after it: GApplication is not
        // called into with a tab borrowed.
        let key = self
            .tabs
            .borrow_mut()
            .iter_mut()
            .find(|t| &t.page == page)
            .map(|t| {
                t.bell_pending = true;
                t.key
            });
        if let Some(key) = key {
            notification
                .set_default_action_and_target_value("app.show-tab", Some(&key.to_variant()));
        }
        // One id, so repeated bells replace rather than stack up.
        app.send_notification(Some("agent-terminal-bell"), &notification);
    }

    /// Re-applies font and cursor settings to every open tab.
    fn apply_appearance_to_all(&self) {
        let config = self.config.borrow();
        self.for_each_terminal_everywhere(|term| apply_appearance(term, &config));
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
    /// The user is recovering a specific tab and expects to land back where they
    /// were. Opening before closing keeps the window from dropping to zero tabs.
    fn restart_tab(&self, page: &adw::TabPage) {
        let Some(tab_view) = self.tab_view.borrow().clone() else {
            return;
        };
        // A thread restarts its agent itself (on the next prompt); there is no page to swap.
        if self
            .tabs
            .borrow()
            .iter()
            .any(|t| &t.page == page && t.chat.is_some())
        {
            return;
        }
        let (dir, session_id, profile) = self
            .tabs
            .borrow()
            .iter()
            .find(|t| &t.page == page)
            .map(|t| (Some(t.dir.clone()), t.session_id.clone(), t.profile.clone()))
            .unwrap_or_default();
        info!("Restarting session in {:?}", dir);
        match session_id {
            // A resumed tab resumes again, rather than trading the conversation
            // the user asked for for a blank one — and with its own profile,
            // since another CLI cannot resume this one's session. Its directory
            // is already known, so this opens synchronously, before the old page
            // closes. Checking the profile first matters: if resume was switched
            // off in config, open_resume_tab would add nothing and closing
            // would lose the tab.
            Some(session_id) if self.resume_profile_named(profile.as_deref()).is_some() => self
                .open_resume_tab(ResumeRequest {
                    session_id,
                    dir,
                    profile,
                }),
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
        menu.append_submenu(
            Some("New Tab As"),
            &self.build_profile_menu("win.new-tab-profile", |_| true),
        );
        menu.append(Some("New Tab in Folder…"), Some("win.new-tab-folder"));
        menu.append_submenu(
            Some("New Tab in Folder As"),
            &self.build_profile_menu("win.new-tab-folder-profile", |_| true),
        );
        menu.append(Some("New Tab in Worktree…"), Some("win.new-tab-worktree"));
        menu.append_submenu(
            Some("New Tab in Worktree As"),
            &self.build_profile_menu("win.new-tab-worktree-profile", |_| true),
        );
        menu.append(Some("New Window"), Some("app.new-window"));
        menu.append(Some("Restart Session"), Some("win.restart-tab"));
        menu.append(Some("Checkpoint Now"), Some("win.checkpoint-now"));
        menu.append(Some("Show or Hide Changes"), Some("win.toggle-diff"));
        menu.append_section(None, &self.build_session_section());

        let section = gtk4::gio::Menu::new();
        section.append(Some("Copy"), Some("win.copy"));
        section.append(Some("Paste"), Some("win.paste"));
        menu.append_section(None, &section);

        let popover = gtk4::PopoverMenu::builder()
            .menu_model(&menu)
            .has_arrow(false)
            .build();
        popover.set_parent(terminal);
        // A child attached with set_parent must be detached by hand before its
        // parent goes: VTE does not know about it, so nothing else would.
        terminal.connect_destroy(glib::clone!(
            #[weak]
            popover,
            move |_| popover.unparent()
        ));

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
        // Every path in (keys, the spin's arrows) steps by 0.1; rounding here
        // keeps float error (1.2000000000000002) out of config.json.
        let scale = (scale * 10.0).round() / 10.0;
        debug!("Setting font scale: {}", scale);
        self.config.borrow_mut().font_scale = scale;
        self.for_each_window(|window| {
            window.for_each_terminal(|term| term.set_font_scale(scale));
            // Setting an unchanged value emits nothing, so the spin's own
            // handler calling back in here ends the round trip.
            if let Some(spin) = window.font_scale_spin.borrow().upgrade() {
                spin.set_value(scale);
            }
        });
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
        launch: Launch<'_>,
    ) {
        let obj = self.obj();
        let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let command = get_startup_command(profile, launch);
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
                    // Hidden behind the loading screen, the terminal could not
                    // take focus when the window opened, so it lands on a header
                    // button instead. Take it now if this is the tab in view —
                    // but not from an open dialog, or a keypress meant for the
                    // dialog would go to the CLI.
                    let take_focus = window_of(terminal).is_some_and(|obj| {
                        obj.visible_dialog().is_none()
                            && obj.imp().current_terminal().as_ref() == Some(terminal)
                    });
                    if take_focus {
                        terminal.grab_focus();
                    }
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
            .vexpand(true)
            .build();
        crate::icons::set_status_icon(&status_page, "at-utilities-terminal-symbolic");

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

        let page = adw::PreferencesPage::builder()
            .title("Terminal")
            .icon_name("at-utilities-terminal-symbolic")
            .build();
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
        *self.font_scale_spin.borrow_mut() = font_scale_spin.downgrade();

        let cli_client_row = adw::ComboRow::builder()
            .title("Active CLI Client")
            .subtitle("Used by new tabs; open tabs keep running")
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

        let notify_quota_row = adw::SwitchRow::builder()
            .title("Notify When Out of Quota")
            .subtitle("Raise a desktop notification offering to continue in another CLI")
            .active(config.notify_on_quota)
            .build();

        let restore_row = adw::SwitchRow::builder()
            .title("Restore Tabs on Launch")
            .subtitle("Reopen the last window's tabs in their folders, as fresh sessions")
            .active(config.restore_session)
            .build();

        let checkpoints_row = adw::SwitchRow::builder()
            .title("Checkpoint Each Turn")
            .subtitle("Snapshot a git repository into hidden refs when a turn ends, for diffing")
            .active(config.checkpoints)
            .build();

        let worktree_root_entry = gtk4::Entry::builder()
            .text(&config.worktree_root)
            .hexpand(true)
            .valign(gtk4::Align::Center)
            .placeholder_text("Blank: a hidden folder beside the repository")
            .build();
        let worktree_root_row = adw::ActionRow::builder().title("Worktree Folder").build();
        worktree_root_row.add_suffix(&worktree_root_entry);

        group.add(&starting_directory_row);
        group.add(&scrollback_row);
        group.add(&font_row);
        group.add(&font_scale_row);
        group.add(&cursor_row);
        group.add(&blink_row);
        group.add(&cli_client_row);
        group.add(&theme_row);
        group.add(&notify_row);
        group.add(&notify_quota_row);
        group.add(&restore_row);
        group.add(&checkpoints_row);
        group.add(&worktree_root_row);
        page.add(&group);
        self.add_diff_tool_group(&page);
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
                // Off means off: a notification already raised would otherwise
                // stay on the dock badge until something withdrew it.
                if !row.is_active() {
                    imp.for_each_window(|window| {
                        for tab in window.tabs.borrow_mut().iter_mut() {
                            tab.bell_pending = false;
                        }
                    });
                    imp.withdraw_bell_if_answered();
                }
                imp.schedule_config_save();
            }
        ));

        notify_quota_row.connect_active_notify(glib::clone!(
            #[weak]
            obj,
            move |row| {
                let imp = obj.imp();
                imp.config.borrow_mut().notify_on_quota = row.is_active();
                if !row.is_active() {
                    imp.for_each_window(Self::withdraw_quota_notifications);
                }
                imp.schedule_config_save();
            }
        ));

        restore_row.connect_active_notify(glib::clone!(
            #[weak]
            obj,
            move |row| {
                let imp = obj.imp();
                imp.config.borrow_mut().restore_session = row.is_active();
                // Nothing is recorded while this is off, so a layout left behind
                // now would come back, however old, the day it is turned on.
                if !row.is_active() {
                    crate::config::SessionState::default().save();
                }
                imp.schedule_config_save();
            }
        ));

        worktree_root_entry.connect_changed(glib::clone!(
            #[weak]
            obj,
            #[weak]
            worktree_root_row,
            move |entry| {
                let text = entry.text().to_string();
                // Like the starting directory: a folder that does not exist is
                // refused here, not discovered when a worktree cannot be made.
                if !directory_is_usable(&text) {
                    entry.add_css_class("error");
                    worktree_root_row.set_subtitle("That directory does not exist");
                    return;
                }
                entry.remove_css_class("error");
                worktree_root_row.set_subtitle("");
                let imp = obj.imp();
                if imp.config.borrow().worktree_root == text {
                    return;
                }
                imp.config.borrow_mut().worktree_root = text;
                imp.schedule_config_save();
            }
        ));

        checkpoints_row.connect_active_notify(glib::clone!(
            #[weak]
            obj,
            move |row| {
                let imp = obj.imp();
                imp.config.borrow_mut().checkpoints = row.is_active();
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
                obj.imp().for_each_terminal_everywhere(|term| {
                    term.set_scrollback_lines(i64::from(lines))
                });
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
                    .for_each_terminal_everywhere(|term| Theme::apply(term, theme));
                obj.imp().recolour_diff_panels(theme);
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
                    "Profile selection changed to {}; applies to new tabs",
                    chosen.as_deref().unwrap_or("auto-detect")
                );
                imp.refresh_profile_selection();
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

        self.add_agents_page(&dialog);
        dialog.present(Some(obj.upcast_ref::<gtk4::Widget>()));
    }

    /// Re-resolves the selected profile for the tabs opened from now on.
    ///
    /// The open tabs keep running: replacing the active one used to end a live
    /// conversation just because the default changed. Resolution can shell out,
    /// so it takes the same off-thread path as startup rather than freezing the
    /// window while the settings dialog is open.
    fn refresh_profile_selection(&self) {
        let obj = self.obj();
        let (profiles, preferred) = {
            let config = self.config.borrow();
            (config.profiles.clone(), config.default_profile.clone())
        };
        let (path, home, shell) = env_triplet();
        // A slow resolution (an -ic probe) can finish after a quicker one
        // started later — for a newer choice, or a newer profile list under
        // auto-detect. Only the latest request may land.
        let generation = PROFILE_GENERATION.with(|g| {
            g.set(g.get() + 1);
            g.get()
        });
        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            obj,
            async move {
                let resolved = resolve_active_profile(profiles, preferred, path, home, shell).await;
                if PROFILE_GENERATION.with(std::cell::Cell::get) != generation {
                    debug!("Dropping a profile resolution a newer one has superseded");
                    return;
                }
                // The default is shared, so every window's new tabs follow it.
                obj.imp().for_each_window(|window| {
                    *window.active_profile.borrow_mut() = resolved.clone();
                });
            }
        ));
    }

    /// Opens a new tab running `profile`, rooted in that profile's directory when
    /// it names one, else in the current tab's — switching CLI mid-task should
    /// not lose the project.
    fn new_tab_with_profile(&self, profile: &Profile) {
        let dir = profile_tab_dir(profile.dir.as_deref(), self.current_dir());
        info!("Opening a thread for profile '{}'", profile.name);
        match crate::config::profile_driver(profile) {
            Some(driver) => self.new_chat_thread(Some(driver), dir, None),
            None => self.add_terminal_tab(Some(profile), dir.as_deref()),
        }
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

    /// The named profile if it can resume; with no name, [`Self::resume_profile`].
    ///
    /// A named profile that cannot resume yields `None` rather than a
    /// substitute: handing its session ID to a different CLI would fail.
    fn resume_profile_named(&self, name: Option<&str>) -> Option<Profile> {
        match name {
            Some(name) => self
                .config
                .borrow()
                .profiles
                .iter()
                .find(|p| p.name == name && p.can_resume())
                .cloned(),
            None => self.resume_profile(),
        }
    }

    /// Opens a tab resuming `request.session_id`.
    ///
    /// Without an explicit directory, the profile's session store is searched
    /// off the main thread for where the session was recorded. A CLI that scopes
    /// sessions per project cannot find one from any other directory, and it
    /// says so only inside the tab, so a failed lookup is reported here.
    fn open_resume_tab(&self, request: ResumeRequest) {
        let ResumeRequest {
            session_id,
            dir,
            profile,
        } = request;
        let Some(profile) = self.resume_profile_named(profile.as_deref()) else {
            warn!("No profile can resume session {session_id}");
            present_message(
                &self.obj(),
                "Cannot Resume Session",
                "No configured profile knows how to resume a session. Add \"resume_args\" \
                 to a profile in config.json, e.g. [\"--resume\", \"{id}\"].",
            );
            return;
        };

        let format = profile.session_format;
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
                // Claude and agy sessions resume as chat threads; other CLIs in a terminal.
                if self.resume_mapped(&profile, &session_id, dir.clone()) {
                    return;
                }
                self.add_terminal_tab_launching(
                    Some(&profile),
                    dir.as_deref(),
                    Launch::Resume(&session_id),
                );
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
                    crate::utils::find_session_dir_in(format, &store, &lookup_id)
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
                if imp.resume_mapped(&profile, &session_id, dir.clone()) {
                    return;
                }
                imp.add_terminal_tab_launching(
                    Some(&profile),
                    dir.as_deref(),
                    Launch::Resume(&session_id),
                );
            }
        ));
    }

    /// Lists the resumable sessions, newest first, and resumes the one picked.
    ///
    /// Falls back to [`Self::show_resume_id_dialog`] when the profile declares no
    /// session store, since there is then nothing to list. The store is read off
    /// the main thread; the dialog shows a spinner meanwhile.
    fn show_session_browser(&self, profile_name: Option<&str>) {
        let obj = self.obj();
        let Some(profile) = self.resume_profile_named(profile_name) else {
            // Reuses open_resume_tab's explanation rather than a second copy.
            self.show_resume_id_dialog(profile_name.map(str::to_string));
            return;
        };
        let Some(store) = profile.session_store.clone() else {
            self.show_resume_id_dialog(Some(profile.name.clone()));
            return;
        };
        let title_pointer = profile.session_title.clone();
        let format = profile.session_format;
        let profile_name = profile.name.clone();

        let dialog = adw::Dialog::builder()
            .title(format!("Resume {} Session", profile.name))
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
            .title("No Sessions Found")
            .build();
        crate::icons::set_status_icon(&empty, "at-document-open-recent-symbolic");

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
                    profile: Some(profile_name.clone()),
                });
            }
        ));

        let id_profile = profile.name.clone();
        enter_id.connect_clicked(glib::clone!(
            #[weak]
            obj,
            #[weak]
            dialog,
            move |_| {
                dialog.close();
                obj.imp().show_resume_id_dialog(Some(id_profile.clone()));
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
                    crate::utils::list_sessions_in(format, &store, title_pointer.as_deref())
                })
                .await
                .unwrap_or_else(|_| Err("Listing sessions panicked".to_string()));

                let found = match listing {
                    Ok(found) => found,
                    Err(reason) => {
                        warn!("Could not list sessions: {reason}");
                        // Distinct from an empty store: an unreadable one must
                        // not look like there is simply nothing to resume.
                        crate::icons::set_status_icon(&empty, "at-dialog-warning-symbolic");
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
                    row.add_suffix(&Image::from_icon_name("at-go-next-symbolic"));
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

    /// Prompts for a session ID, then opens a tab resuming it with `profile`,
    /// or the active profile when `None`.
    ///
    /// The profile is carried through from the session browser: an ID means
    /// nothing to any CLI but the one that recorded it, so one pasted into
    /// "Resume Agy Session" must go to Agy, not to whichever CLI is active.
    ///
    /// The Resume button stays disabled until the ID is valid, so a bad paste is
    /// caught in the dialog rather than surfacing later as a failed tab.
    fn show_resume_id_dialog(&self, profile: Option<String>) {
        let obj = self.obj();
        let entry = gtk4::Entry::builder()
            .placeholder_text("Session ID")
            .activates_default(true)
            .build();

        let heading = match &profile {
            Some(name) => format!("Resume {name} Session"),
            None => "Resume Session".to_string(),
        };
        let dialog = adw::AlertDialog::new(
            Some(&heading),
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
                            profile: profile.clone(),
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

    /// Builds a "… as <profile>" menu, one item per configured profile, each
    /// activating `action` with the profile name.
    ///
    /// Rebuilt on demand rather than cached, so editing config.json and
    /// reopening the window is enough to see a new profile.
    fn build_profile_menu(&self, action: &str, include: fn(&Profile) -> bool) -> gtk4::gio::Menu {
        let menu = gtk4::gio::Menu::new();
        for profile in self.config.borrow().profiles.iter().filter(|p| include(p)) {
            // The profile name is the action target, so the action handler does
            // not depend on menu ordering.
            let item = gtk4::gio::MenuItem::new(Some(&profile.name), None);
            item.set_action_and_target_value(Some(action), Some(&profile.name.to_variant()));
            menu.append_item(&item);
        }
        menu
    }

    /// Hands the current tab's task to `target_name` in a new tab.
    ///
    /// The source CLI is usually out of quota and cannot be asked for a
    /// summary, so the brief is assembled here — transcript, working tree, or
    /// failing both the screen — off the main thread, written privately, and
    /// named in the new session's opening prompt. The source tab is left as it
    /// is, so its session can be resumed once its quota resets.
    fn continue_in(&self, target_name: &str) {
        let target = self
            .config
            .borrow()
            .profiles
            .iter()
            .find(|p| p.name == target_name)
            .cloned();
        let Some(target) = target.filter(Profile::can_take_prompt) else {
            present_message(
                &self.obj(),
                "Cannot Hand Off",
                &format!(
                    "Profile '{target_name}' cannot start with a prompt. Add \"prompt_args\" \
                     to it in config.json, e.g. [\"{{prompt}}\"]."
                ),
            );
            return;
        };
        let Some(page) = self
            .tab_view
            .borrow()
            .as_ref()
            .and_then(|view| view.selected_page())
        else {
            return;
        };
        // A thread hands itself over in place: budgeted, redacted, shown as a switch divider.
        if let Some(slot) = self.current_slot() {
            use crate::chat::ChatBackend as _;
            match crate::config::profile_driver(&target) {
                Some(driver) => slot.switch(driver, None, None),
                None => present_message(
                    &self.obj(),
                    "Cannot Continue There",
                    &format!(
                        "'{}' has no chat adapter. Open it from New Terminal Thread instead.",
                        target.name
                    ),
                ),
            }
            return;
        }

        let (source, dir, session_id, since_ms, screen_tail) = {
            let tabs = self.tabs.borrow();
            let Some(tab) = tabs.iter().find(|t| t.page == page) else {
                warn!("Hand-off asked for a tab that is not tracked");
                return;
            };
            let source = tab.profile.as_deref().and_then(|name| {
                self.config
                    .borrow()
                    .profiles
                    .iter()
                    .find(|p| p.name == name)
                    .cloned()
            });
            let since_ms = tab
                .started_at
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                .unwrap_or(0);
            (
                source,
                tab.dir.clone(),
                tab.known_session_id().map(str::to_string),
                since_ms,
                screen_text(&tab.terminal).map(|t| crate::handoff::tail_lines(&t, 80)),
            )
        };

        // The menus are shared by every tab, so they cannot leave out the one
        // CLI this tab is running; handing a session to itself is refused here.
        if source.as_ref().is_some_and(|p| p.name == target.name) {
            present_message(
                &self.obj(),
                "Already Running Here",
                &format!(
                    "This tab is already running {}. Choose another CLI to continue in.",
                    target.name
                ),
            );
            return;
        }
        let from = source
            .as_ref()
            .map_or_else(|| "The previous session".to_string(), |p| p.name.clone());
        let format = source
            .as_ref()
            .map_or(SessionFormat::Jsonl, |p| p.session_format);
        let store = source.as_ref().and_then(|p| p.session_store.clone());
        let written_at = glib::DateTime::now_local()
            .and_then(|now| now.format("%Y-%m-%d %H:%M %Z"))
            .map(|s| s.to_string())
            .unwrap_or_default();
        let unix_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let briefs = crate::handoff::briefs_dir(
            env::var("XDG_STATE_HOME").ok().as_deref(),
            env::var("HOME").ok().as_deref(),
        );
        info!(
            "Handing off from '{from}' to '{}' in {dir} (session {:?})",
            target.name, session_id
        );

        let obj = self.obj();
        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            obj,
            async move {
                let to = target.name.clone();
                let from_for_brief = from.clone();
                let tab_dir = dir.clone();
                let written = gtk4::gio::spawn_blocking(move || {
                    use crate::handoff::*;
                    // An AGY tab's ID is AGY's own choice; find the session it
                    // recorded in this directory since the tab opened.
                    let session_id = session_id.or_else(|| match (format, store.as_deref()) {
                        (SessionFormat::AgyHistory, Some(store)) => {
                            latest_agy_session_in(store, &dir, since_ms)
                        }
                        _ => None,
                    });
                    let input = BriefInput {
                        conversation: gather_conversation(
                            format,
                            store.as_deref(),
                            session_id.as_deref(),
                        ),
                        tree: read_working_tree(&dir),
                        from: from_for_brief.clone(),
                        to: to.clone(),
                        dir,
                        session_id,
                        written_at,
                        screen_tail,
                    };
                    write_brief(
                        &briefs,
                        &brief_stem(unix_secs, &from_for_brief, &to),
                        &render_brief(&input),
                    )
                })
                .await
                .unwrap_or_else(|_| Err("Writing the brief panicked".to_string()));

                match written {
                    Ok(path) => {
                        let prompt = crate::handoff::handoff_prompt(&from, &path);
                        match crate::config::profile_driver(&target) {
                            // The brief's pointer is the new thread's first message.
                            Some(driver) => {
                                obj.imp()
                                    .new_chat_thread(Some(driver), Some(tab_dir), Some(prompt))
                            }
                            None => obj.imp().add_terminal_tab_launching(
                                Some(&target),
                                Some(&tab_dir),
                                Launch::New {
                                    session_id: None,
                                    prompt: Some(&prompt),
                                },
                            ),
                        }
                    }
                    Err(reason) => {
                        error!("Hand-off failed: {reason}");
                        present_message(&obj, "Hand-Off Failed", &reason);
                    }
                }
            }
        ));
    }

    /// The profile a quota banner offers to continue in: the first other
    /// profile that can take a hand-off and is not known to be missing.
    fn handoff_target(&self, source: Option<&str>) -> Option<String> {
        self.config
            .borrow()
            .profiles
            .iter()
            .find(|p| {
                Some(p.name.as_str()) != source
                    && p.can_take_prompt()
                    && crate::utils::cached_command_available(&p.command) != Some(false)
            })
            .map(|p| p.name.clone())
    }

    /// Checks every tab's quota every few seconds for the life of the window.
    fn start_quota_watch(&self) {
        const QUOTA_POLL_SECS: u32 = 10;
        let obj = self.obj();
        glib::timeout_add_seconds_local(
            QUOTA_POLL_SECS,
            glib::clone!(
                #[weak]
                obj,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    obj.imp().poll_quota();
                    glib::ControlFlow::Continue
                }
            ),
        );
    }

    /// Checks every couple of seconds for tabs whose output has gone quiet.
    /// In-memory only: git runs just for a tab that is due.
    fn start_checkpoint_watch(&self) {
        const CHECKPOINT_POLL_SECS: u32 = 2;
        let obj = self.obj();
        glib::timeout_add_seconds_local(
            CHECKPOINT_POLL_SECS,
            glib::clone!(
                #[weak]
                obj,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    obj.imp().poll_checkpoints();
                    glib::ControlFlow::Continue
                }
            ),
        );
    }

    /// Checkpoints each tab that has shown output since its last attempt and
    /// has been quiet for [`CHECKPOINT_QUIET`] since.
    fn poll_checkpoints(&self) {
        if !self.config.borrow().checkpoints {
            return;
        }
        let now = std::time::Instant::now();
        let due: Vec<adw::TabPage> = self
            .tabs
            .borrow()
            .iter()
            .filter(|tab| {
                let last = tab.last_output.get();
                !tab.checkpoint.running()
                    && tab.checkpoint.attempted_for != Some(last)
                    && now.saturating_duration_since(last) >= CHECKPOINT_QUIET
            })
            .map(|tab| tab.page.clone())
            .collect();
        for page in due {
            self.request_checkpoint(&page, false);
        }
    }

    /// Snapshots `page`'s working tree off the main thread. `manual` reports
    /// every outcome as a toast; an automatic attempt stays silent unless git
    /// fails, which marks the tab.
    fn request_checkpoint(&self, page: &adw::TabPage, manual: bool) {
        let (dir, key, label) = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(tab) = tabs.iter_mut().find(|t| &t.page == page) else {
                return;
            };
            if tab.checkpoint.running() {
                return;
            }
            tab.checkpoint.started = Some(std::time::Instant::now());
            tab.checkpoint.attempted_for = Some(tab.last_output.get());
            (
                tab.dir.clone(),
                tab.key,
                tab.profile.clone().unwrap_or_else(|| "session".to_string()),
            )
        };
        let child = page.child();
        glib::MainContext::default().spawn_local(async move {
            let result = gtk4::gio::spawn_blocking(move || run_checkpoint(&dir, key, &label))
                .await
                .unwrap_or_else(|_| {
                    CheckpointResult::Failed("the checkpoint thread panicked".to_string())
                });
            // Looked up afterwards, not captured: the tab may have been
            // dragged to another window meanwhile, or closed.
            let Some(obj) = window_of(&child) else {
                return;
            };
            // Through the registry, not TabView::page, which is a critical
            // (and a panic in the bindings) for a child it does not hold.
            let page = obj
                .imp()
                .tabs
                .borrow()
                .iter()
                .find(|t| t.page.child() == child)
                .map(|t| t.page.clone());
            if let Some(page) = page {
                obj.imp().apply_checkpoint(&page, result, manual);
            }
        });
    }

    fn apply_checkpoint(&self, page: &adw::TabPage, result: CheckpointResult, manual: bool) {
        let time = glib::DateTime::now_local()
            .and_then(|now| now.format("%H:%M"))
            .map(|s| s.to_string())
            .unwrap_or_default();
        let report = describe_checkpoint(&result, &time);
        // Decided under the borrow; the page is touched after it is released,
        // since its property setters emit notify signals synchronously.
        let (error, branch) = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(tab) = tabs.iter_mut().find(|t| &t.page == page) else {
                return;
            };
            let branch = tab.worktree.as_ref().map(|w| w.branch.clone());
            let track = &mut tab.checkpoint;
            track.started = None;
            match &result {
                CheckpointResult::Done(crate::git::CheckpointOutcome::Busy) => {
                    // Git is mid-operation; the next poll tries again.
                    track.attempted_for = None;
                }
                CheckpointResult::Done(crate::git::CheckpointOutcome::Created(cp, _)) => {
                    info!("Checkpoint {} for tab {}", cp.refname, tab.key);
                    track.error = None;
                }
                CheckpointResult::Failed(err) => {
                    warn!("Checkpoint failed in {}: {err}", tab.dir);
                    track.error = Some(err.clone());
                }
                CheckpointResult::NotRepo
                | CheckpointResult::Done(crate::git::CheckpointOutcome::Unchanged) => {
                    track.error = None;
                }
            }
            (track.error.clone(), branch)
        };

        if let Some(tooltip) = &report.tooltip {
            page.set_tooltip(&tab_tooltip(branch.as_deref(), Some(tooltip)));
        }
        match error {
            Some(err) => {
                page.set_indicator_icon(Some(&gtk4::gio::ThemedIcon::new(
                    "at-dialog-warning-symbolic",
                )));
                page.set_indicator_tooltip(&format!("Checkpoint failed: {err}"));
            }
            None => {
                page.set_indicator_icon(None::<&gtk4::gio::Icon>);
                page.set_indicator_tooltip("");
            }
        }
        if manual {
            self.show_toast(&report.toast);
        }
        // A new checkpoint moves the "last turn" and "this tab" bases.
        if matches!(
            result,
            CheckpointResult::Done(crate::git::CheckpointOutcome::Created(..))
        ) {
            self.refresh_diff(page);
        }

        // Optional turn command: runs asynchronously on turn quiescence.
        if let Some(cmd) = &self.config.borrow().turn_command {
            if let Some(prog) = cmd.first() {
                let prog = prog.clone();
                let (sess, dir) = {
                    let tabs = self.tabs.borrow();
                    let tab = tabs.iter().find(|t| &t.page == page);
                    (
                        tab.and_then(|t| t.session_id.clone().or_else(|| t.pinned_id.clone()))
                            .unwrap_or_default(),
                        tab.map(|t| t.dir.clone()).unwrap_or_default(),
                    )
                };
                let args: Vec<String> = cmd[1..]
                    .iter()
                    .map(|arg| arg.replace("{id}", &sess).replace("{dir}", &dir))
                    .collect();
                gtk4::gio::spawn_blocking(move || {
                    let mut command = std::process::Command::new(prog);
                    command.args(&args);
                    let _ = command.status();
                });
            }
        }
    }

    /// Connects a new tab's diff panel: its refresh requests, its first
    /// placement, and remembering the width it is dragged to.
    fn wire_diff_panel(&self, panel: &DiffPanel, paned: &gtk4::Paned) {
        // Found at click time, like the exit bar's buttons: the tab may have
        // been dragged to another window since.
        let root = panel.root.clone();
        panel.connect_refresh(move || {
            let Some(obj) = window_of(&root) else {
                return;
            };
            let imp = obj.imp();
            let page = imp
                .tabs
                .borrow()
                .iter()
                .find(|t| t.diff_panel.root == root)
                .map(|t| t.page.clone());
            if let Some(page) = page {
                imp.refresh_diff(&page);
            }
        });

        // "Open in …" on a file row.
        let root = panel.root.clone();
        panel.connect_open(move |file| {
            let Some(obj) = window_of(&root) else {
                return;
            };
            obj.imp().open_panel_file(&root, file);
        });

        let root = panel.root.clone();
        let base_of = panel.clone();
        panel.connect_undo(move |target| {
            let Some(obj) = window_of(&root) else {
                return;
            };
            let imp = obj.imp();
            let page = imp
                .tabs
                .borrow()
                .iter()
                .find(|t| t.diff_panel.root == root)
                .map(|t| t.page.clone());
            let to = match base_of.base() {
                crate::diff::DiffBase::ThisTab => "how they were when this tab started",
                _ => "how they were before the last turn",
            };
            if let Some(page) = page {
                imp.confirm_restore(&page, target, to);
            }
        });

        // A Paned cannot be told "give the end child N pixels" before it has
        // measured its layout, and showing the panel changes that layout. So
        // the saved width is applied from the paned's own layout notifications,
        // once per show (DiffPanel::place), and only a later move that is a
        // real drag is saved (DiffPanel::dragged_width). The handlers go when
        // the paned is disposed with its tab.
        let placer = panel.clone();
        paned.connect_notify_local(Some("max-position"), move |paned, _| {
            if let Some(obj) = window_of(paned) {
                let width = obj.imp().config.borrow().diff_panel_width;
                placer.place(paned, width);
            }
        });
        let watcher = panel.clone();
        paned.connect_position_notify(move |paned| {
            let Some(obj) = window_of(paned) else {
                return;
            };
            let imp = obj.imp();
            let saved = imp.config.borrow().diff_panel_width;
            // A layout change can arrive as a position change alone.
            watcher.place(paned, saved);
            let Some(width) = watcher.dragged_width(paned) else {
                return;
            };
            if width != saved {
                imp.config.borrow_mut().diff_panel_width = width;
                imp.schedule_config_save();
            }
        });
    }

    /// Shows or hides the current tab's diff panel. The choice also becomes
    /// the default for new tabs.
    fn toggle_diff_panel(&self) {
        let Some(page) = self
            .tab_view
            .borrow()
            .as_ref()
            .and_then(|v| v.selected_page())
        else {
            return;
        };
        let Some(panel) = self
            .tabs
            .borrow()
            .iter()
            .find(|t| t.page == page)
            .map(|t| t.diff_panel.clone())
        else {
            return;
        };
        let visible = !panel.root.is_visible();
        // Placed by the paned's layout notifications once it has re-measured.
        panel.set_shown(visible);
        self.config.borrow_mut().diff_panel_visible = visible;
        self.schedule_config_save();
        if visible {
            self.refresh_diff(&page);
        }
    }

    /// Recomputes `page`'s diff off the main thread, if its panel is showing.
    fn refresh_diff(&self, page: &adw::TabPage) {
        let Some((dir, key, panel)) = self
            .tabs
            .borrow()
            .iter()
            .find(|t| &t.page == page)
            .map(|t| (t.dir.clone(), t.key, t.diff_panel.clone()))
        else {
            return;
        };
        if !panel.root.is_visible() {
            return;
        }
        let base = panel.base();
        let generation = panel.begin();
        glib::MainContext::default().spawn_local(async move {
            let result = gtk4::gio::spawn_blocking(move || {
                crate::git::tab_diff(std::path::Path::new(&dir), key, base)
            })
            .await
            .unwrap_or_else(|_| Err("the diff thread panicked".to_string()));
            panel.show(generation, result);
        });
    }

    /// Opens a diff-panel file in the external diff tool, off the main thread. A missing tool or
    /// binary, or any failure, is a toast.
    fn open_panel_file(&self, panel_root: &gtk4::Box, file: super::diff_panel::OpenFile) {
        let Some(tool) = crate::diff_tool::DiffTools::shared().get() else {
            self.show_toast(crate::diff_tool::NO_TOOL_HINT);
            return;
        };
        let Some(dir) = self
            .tabs
            .borrow()
            .iter()
            .find(|t| t.diff_panel.root == *panel_root)
            .map(|t| t.dir.clone())
        else {
            return;
        };
        let new_side = match file.to_checkpoint {
            Some(rev) => crate::diff_tool::NewSide::Rev(rev),
            None => crate::diff_tool::NewSide::Working,
        };
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            let result = crate::diff_tool::open_from_dir(
                tool,
                std::path::PathBuf::from(dir),
                file.path,
                file.from,
                new_side,
            )
            .await;
            if let (Err(why), Some(obj)) = (result, obj.upgrade()) {
                obj.imp().show_toast(&why);
            }
        });
    }

    /// Follows a theme change in every window's diff panels.
    fn recolour_diff_panels(&self, theme: crate::config::ThemeChoice) {
        let colours = Theme::diff_colours(theme);
        self.for_each_window(|window| {
            let panels: Vec<DiffPanel> = window
                .tabs
                .borrow()
                .iter()
                .map(|t| t.diff_panel.clone())
                .collect();
            for panel in panels {
                panel.set_colours(&colours);
            }
        });
    }

    /// New Tab in Worktree: checks, off the main thread, that the current
    /// tab is in a repository, then asks for the branch to create.
    fn new_tab_in_worktree(&self, profile: Option<Profile>) {
        let Some(dir) = self.current_dir() else {
            return;
        };
        let obj = self.obj();
        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            obj,
            async move {
                let found = gtk4::gio::spawn_blocking(move || {
                    let dir = std::path::Path::new(&dir);
                    crate::worktree::main_toplevel(dir)
                        .map(|main| (main, crate::worktree::current_branch(dir)))
                })
                .await
                .unwrap_or_else(|_| Err("the repository check panicked".to_string()));
                match found {
                    Ok((main, branch)) => obj.imp().show_worktree_dialog(main, branch, profile),
                    Err(err) => present_message(
                        &obj,
                        "Not in a Git Repository",
                        &format!(
                            "New Tab in Worktree starts from the current tab's repository, \
                             and its folder is not in one.\n\n{err}"
                        ),
                    ),
                }
            }
        ));
    }

    /// Asks for the new branch and its base, checking both as they are typed.
    fn show_worktree_dialog(
        &self,
        main: std::path::PathBuf,
        current: Option<String>,
        profile: Option<Profile>,
    ) {
        let root = {
            let root = self.config.borrow().worktree_root.trim().to_string();
            (!root.is_empty()).then(|| std::path::PathBuf::from(crate::utils::expand_tilde(&root)))
        };

        let branch = gtk4::Entry::builder()
            .placeholder_text("feat/my-change")
            .activates_default(true)
            .build();
        let base = gtk4::Entry::builder()
            .text(current.as_deref().unwrap_or("HEAD"))
            .activates_default(true)
            .build();
        let problem = Label::builder()
            .wrap(true)
            .xalign(0.0)
            .css_classes(["error"])
            .build();
        let location = Label::builder()
            .wrap(true)
            .xalign(0.0)
            .selectable(true)
            .css_classes(["dim-label", "caption"])
            .build();
        let note = Label::builder()
            .label(
                "Ignored files (node_modules, .env, build output) are not copied into a \
                 new worktree.",
            )
            .wrap(true)
            .xalign(0.0)
            .css_classes(["dim-label", "caption"])
            .build();
        let form = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(6)
            .build();
        form.append(&Label::builder().label("Branch").xalign(0.0).build());
        form.append(&branch);
        form.append(&Label::builder().label("Starting from").xalign(0.0).build());
        form.append(&base);
        form.append(&location);
        form.append(&problem);
        form.append(&note);

        let dialog = adw::AlertDialog::new(
            Some("New Tab in Worktree"),
            Some("Creates a branch in a worktree of its own and opens a tab there."),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("create", "Create")]);
        dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("create"));
        dialog.set_close_response("cancel");
        dialog.set_response_enabled("create", false);
        dialog.set_extra_child(Some(&form));

        // Each edit supersedes the check before it: only the newest may
        // enable Create. Checked after a pause, and off the main thread.
        let generation = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let validate = std::rc::Rc::new(glib::clone!(
            #[weak]
            dialog,
            #[weak]
            branch,
            #[weak]
            base,
            #[weak]
            problem,
            #[weak]
            location,
            #[strong]
            main,
            #[strong]
            root,
            #[strong]
            generation,
            move || {
                let current = generation.get().wrapping_add(1);
                generation.set(current);
                dialog.set_response_enabled("create", false);
                let name = branch.text().trim().to_string();
                let start = base.text().trim().to_string();
                if name.is_empty() {
                    location.set_text("");
                    problem.set_text("");
                    return;
                }
                location.set_text(&format!(
                    "In {}",
                    crate::worktree::default_path(&main, &name, root.as_deref()).display()
                ));
                let main = main.clone();
                let generation = generation.clone();
                glib::timeout_add_local_once(
                    std::time::Duration::from_millis(250),
                    glib::clone!(
                        #[weak]
                        dialog,
                        #[weak]
                        problem,
                        move || {
                            if generation.get() != current {
                                return;
                            }
                            glib::MainContext::default().spawn_local(async move {
                                let checked = gtk4::gio::spawn_blocking(move || {
                                    crate::worktree::check_new_branch(&main, &name)?;
                                    crate::worktree::resolve_base(&main, &start).map(|_| ())
                                })
                                .await
                                .unwrap_or_else(|_| Err("the check panicked".to_string()));
                                if generation.get() != current {
                                    return;
                                }
                                match checked {
                                    Ok(()) => {
                                        problem.set_text("");
                                        dialog.set_response_enabled("create", true);
                                    }
                                    Err(err) => problem.set_text(&err),
                                }
                            });
                        }
                    ),
                );
            }
        ));
        for entry in [&branch, &base] {
            let validate = validate.clone();
            entry.connect_changed(move |_| validate());
        }

        let obj = self.obj();
        branch.grab_focus();
        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            obj,
            async move {
                let response = dialog
                    .choose_future(Some(obj.upcast_ref::<gtk4::Widget>()))
                    .await;
                if response != "create" {
                    return;
                }
                let name = branch.text().trim().to_string();
                let start = base.text().trim().to_string();
                let path = crate::worktree::default_path(&main, &name, root.as_deref());
                let info = crate::worktree::WorktreeInfo {
                    path: path.clone(),
                    branch: name.clone(),
                    main_toplevel: main.clone(),
                };
                // Checked again: the repository may have moved on since.
                let created = gtk4::gio::spawn_blocking(move || {
                    crate::worktree::check_new_branch(&main, &name)?;
                    let commit = crate::worktree::resolve_base(&main, &start)?;
                    crate::worktree::add(&main, &name, &path, &commit)
                })
                .await
                .unwrap_or_else(|_| Err("creating the worktree panicked".to_string()));
                match created {
                    Ok(()) => {
                        info!(
                            "Created worktree {} on {}",
                            info.path.display(),
                            info.branch
                        );
                        obj.imp().open_worktree_tab(info, profile);
                    }
                    Err(err) => present_message(&obj, "Could Not Create the Worktree", &err),
                }
            }
        ));
    }

    /// Opens a tab in a worktree just created, and remembers it is one.
    fn open_worktree_tab(&self, info: crate::worktree::WorktreeInfo, profile: Option<Profile>) {
        let dir = info.path.to_string_lossy().to_string();
        match &profile {
            Some(p) if crate::config::profile_driver(p).is_none() => {
                self.add_terminal_tab(Some(p), Some(&dir));
            }
            p => self.new_chat_thread(
                p.as_ref().and_then(crate::config::profile_driver),
                Some(dir.clone()),
                None,
            ),
        }
        let page = {
            let mut tabs = self.tabs.borrow_mut();
            // The page just added, unless adding failed and the last is another.
            tabs.last_mut().filter(|t| t.dir == dir).map(|t| {
                t.worktree = Some(info.clone());
                t.page.clone()
            })
        };
        if let Some(page) = page {
            page.set_tooltip(&tab_tooltip(Some(&info.branch), None));
        }
    }

    /// When a worktree tab closes and leaves its worktree clean, offers to
    /// remove it. Never offered for a worktree another open tab is still in,
    /// or one with anything uncommitted — that is simply kept.
    fn offer_worktree_removal(&self, info: crate::worktree::WorktreeInfo) {
        if self.worktree_in_use(&info.path) {
            return;
        }
        let obj = self.obj();
        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            obj,
            async move {
                let path = info.path.clone();
                let clean = gtk4::gio::spawn_blocking(move || crate::worktree::is_clean(&path))
                    .await
                    .unwrap_or_else(|_| Err("the status check panicked".to_string()));
                match clean {
                    Ok(true) => {}
                    Ok(false) => {
                        debug!("Worktree {} has changes; keeping it", info.path.display());
                        return;
                    }
                    Err(err) => {
                        debug!(
                            "Worktree {} not offered for removal: {err}",
                            info.path.display()
                        );
                        return;
                    }
                }
                let toast = adw::Toast::builder()
                    .title(format!("Worktree ⎇ {} is clean", info.branch))
                    .use_markup(false)
                    .button_label("Remove")
                    .timeout(10)
                    .build();
                toast.connect_button_clicked(glib::clone!(
                    #[weak]
                    obj,
                    move |_| obj.imp().remove_worktree(info.clone())
                ));
                if let Some(overlay) = obj.imp().toast_overlay.borrow().as_ref() {
                    overlay.add_toast(toast);
                }
            }
        ));
    }

    /// Whether any open tab, in any window, is in the worktree at `path`.
    fn worktree_in_use(&self, path: &std::path::Path) -> bool {
        let in_use = std::cell::Cell::new(false);
        self.for_each_window(|window| {
            if window
                .tabs
                .borrow()
                .iter()
                .any(|t| std::path::Path::new(&t.dir).starts_with(path))
            {
                in_use.set(true);
            }
        });
        in_use.get()
    }

    /// Removes a worktree the user asked to, keeping its branch.
    fn remove_worktree(&self, info: crate::worktree::WorktreeInfo) {
        // Asked again at the click: a tab may have opened there while the
        // offer was up, and git does not check who is inside a worktree.
        // Whether it is still clean, `worktree remove` checks itself.
        if self.worktree_in_use(&info.path) {
            self.show_toast("A tab is open in that worktree now; it was kept");
            return;
        }
        let obj = self.obj();
        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            obj,
            async move {
                let (main, path) = (info.main_toplevel.clone(), info.path.clone());
                let removed =
                    gtk4::gio::spawn_blocking(move || crate::worktree::remove(&main, &path))
                        .await
                        .unwrap_or_else(|_| Err("removing the worktree panicked".to_string()));
                match removed {
                    Ok(()) => {
                        info!("Removed worktree {}", info.path.display());
                        obj.imp().show_toast(&format!(
                            "Removed the worktree; branch {} is kept",
                            info.branch
                        ));
                    }
                    Err(err) => present_message(&obj, "Could Not Remove the Worktree", &err),
                }
            }
        ));
    }

    /// Undo: pins the working tree, shows what restoring `page`'s tree to
    /// `target` would do, and does it only if confirmed. `to` finishes
    /// "Puts … back to" in the confirmation.
    fn confirm_restore(&self, page: &adw::TabPage, target: String, to: &'static str) {
        let Some((dir, key, mid_turn)) =
            self.tabs
                .borrow()
                .iter()
                .find(|t| &t.page == page)
                .map(|t| {
                    let quiet_for = t.last_output.get().elapsed();
                    (t.dir.clone(), t.key, quiet_for < CHECKPOINT_QUIET)
                })
        else {
            return;
        };
        let obj = self.obj();
        let page = page.clone();
        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            obj,
            async move {
                let prepared = gtk4::gio::spawn_blocking(move || {
                    let repo = crate::git::discover(std::path::Path::new(&dir))?
                        .ok_or_else(|| "This tab's folder is not in a repository".to_string())?;
                    let plan = crate::restore::prepare(&repo, key, &target)?;
                    Ok::<_, String>((repo, plan))
                })
                .await
                .unwrap_or_else(|_| Err("preparing the undo panicked".to_string()));
                let (repo, plan) = match prepared {
                    Ok(prepared) => prepared,
                    Err(err) => {
                        present_message(&obj, "Could Not Prepare the Undo", &err);
                        return;
                    }
                };
                if plan.changed.is_empty() {
                    obj.imp()
                        .show_toast("Nothing to undo: the files already match");
                    return;
                }

                let dialog = adw::AlertDialog::new(Some("Undo These Changes?"), None);
                let text = Label::builder()
                    .label(crate::restore::summary(&plan, to, mid_turn))
                    .wrap(true)
                    .xalign(0.0)
                    // Not selectable: as the dialog's first focusable widget it
                    // would open with all of its text selected.
                    .selectable(false)
                    .build();
                let scroll = ScrolledWindow::builder()
                    .hscrollbar_policy(gtk4::PolicyType::Never)
                    .max_content_height(360)
                    .propagate_natural_height(true)
                    .child(&text)
                    .build();
                dialog.set_extra_child(Some(&scroll));
                dialog.add_responses(&[("cancel", "Cancel"), ("restore", "Undo Changes")]);
                dialog.set_response_appearance("restore", adw::ResponseAppearance::Destructive);
                dialog.set_default_response(Some("cancel"));
                dialog.set_close_response("cancel");
                let response = dialog
                    .choose_future(Some(obj.upcast_ref::<gtk4::Widget>()))
                    .await;
                if response != "restore" {
                    // Cancelled: the undo point is not needed.
                    gtk4::gio::spawn_blocking(move || crate::restore::discard(&repo, &plan.pinned));
                    return;
                }

                let pinned = plan.pinned.commit.clone();
                let done = gtk4::gio::spawn_blocking(move || crate::restore::apply(&repo, &plan))
                    .await
                    .unwrap_or_else(|_| Err("the undo panicked".to_string()));
                let imp = obj.imp();
                match done {
                    Ok(done) if done.mismatched.is_empty() && done.refused.is_empty() => {
                        info!("Restored a tab's working tree; undo point {pinned}");
                        let toast = adw::Toast::builder()
                            .title("Changes undone")
                            .button_label("Undo")
                            .timeout(15)
                            .build();
                        toast.connect_button_clicked(glib::clone!(
                            #[weak]
                            obj,
                            #[weak]
                            page,
                            move |_| obj.imp().confirm_restore(
                                &page,
                                pinned.clone(),
                                "how they were before the undo"
                            )
                        ));
                        if let Some(overlay) = imp.toast_overlay.borrow().as_ref() {
                            overlay.add_toast(toast);
                        }
                    }
                    Ok(done) => {
                        warn!(
                            "Restore left differences: {:?}; refused: {:?}",
                            done.mismatched, done.refused
                        );
                        present_message(
                            &obj,
                            "Undo Was Incomplete",
                            &format!(
                                "These still differ from the checkpoint: {}\n\nNot deleted, \
                                 being outside this folder: {}\n\nThe files as they were \
                                 before are kept as commit {pinned}; restore it with\n\
                                 git restore --source={pinned} --worktree -- :/",
                                if done.mismatched.is_empty() {
                                    "none".to_string()
                                } else {
                                    done.mismatched.join(", ")
                                },
                                if done.refused.is_empty() {
                                    "none".to_string()
                                } else {
                                    done.refused.join(", ")
                                },
                            ),
                        );
                    }
                    Err(err) => present_message(&obj, "Could Not Undo the Changes", &err),
                }
                imp.refresh_diff(&page);
            }
        ));
    }

    /// A short confirmation over the tab view. The text is data, not markup.
    fn show_toast(&self, text: &str) {
        if let Some(overlay) = self.toast_overlay.borrow().as_ref() {
            overlay.add_toast(adw::Toast::builder().title(text).use_markup(false).build());
        }
    }

    /// One quota check across all tabs.
    ///
    /// A tab whose session the terminal knows, on a profile with transcripts,
    /// is read from its transcript — the structured signal. Otherwise a profile
    /// with `limit_markers` has its screen searched, but only if it changed.
    fn poll_quota(&self) {
        let profiles = self.config.borrow().profiles.clone();
        let mut pages = Vec::new();
        let mut jobs: Vec<(String, String)> = Vec::new();
        let mut screen_states = Vec::new();

        for tab in self.tabs.borrow().iter() {
            let Some(profile) = tab
                .profile
                .as_deref()
                .and_then(|name| profiles.iter().find(|p| p.name == name))
            else {
                continue;
            };
            match (
                profile.session_format,
                profile.session_store.as_deref(),
                tab.known_session_id(),
            ) {
                (SessionFormat::Jsonl, Some(store), Some(id)) => {
                    pages.push(tab.page.clone());
                    jobs.push((store.to_string(), id.to_string()));
                }
                _ => {
                    let Some(markers) = profile.limit_markers.as_deref() else {
                        continue;
                    };
                    if markers.is_empty() || !tab.screen_dirty.replace(false) {
                        continue;
                    }
                    let rows = tab.terminal.row_count();
                    let state = match text_above_cursor(&tab.terminal, rows) {
                        Some(text) => match crate::handoff::screen_quota_line(&text, markers) {
                            Some(line) => QuotaState::Exhausted { detail: line },
                            None => QuotaState::Available,
                        },
                        None => QuotaState::Unknown,
                    };
                    screen_states.push((tab.page.clone(), state));
                }
            }
        }
        for (page, state) in screen_states {
            self.apply_quota_state(&page, state);
        }

        if jobs.is_empty() || self.quota_poll_running.get() {
            return;
        }
        self.quota_poll_running.set(true);
        let obj = self.obj();
        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            obj,
            async move {
                // Ceiling: each check lists the store's project directories to
                // find the transcript, a few hundred stat calls every poll.
                // Upgrade path: remember the path once it is found.
                let states = gtk4::gio::spawn_blocking(move || {
                    jobs.iter()
                        .map(|(store, id)| {
                            crate::utils::find_transcript(store, id)
                                .map_or(QuotaState::Unknown, |path| {
                                    crate::handoff::transcript_quota_state(&path)
                                })
                        })
                        .collect::<Vec<_>>()
                })
                .await
                .unwrap_or_default();
                let imp = obj.imp();
                imp.quota_poll_running.set(false);
                for (page, state) in pages.iter().zip(states) {
                    imp.apply_quota_state(page, state);
                }
            }
        ));
    }

    /// Shows or hides a tab's quota banner.
    ///
    /// `Unknown` changes nothing on screen: an unreadable transcript neither
    /// proves nor disproves the limit, so it must not hide a banner that is up.
    fn apply_quota_state(&self, page: &adw::TabPage, state: QuotaState) {
        let target = {
            let tabs = self.tabs.borrow();
            let Some(tab) = tabs.iter().find(|t| &t.page == page) else {
                return;
            };
            if tab.quota == state {
                return;
            }
            self.handoff_target(tab.profile.as_deref())
        };

        // Decided under the borrow, sent after it: the notification calls
        // into GApplication, which must not run with a tab borrowed.
        let mut notify: Option<(u64, String, String)> = None;
        let mut withdraw: Option<u64> = None;
        {
            let mut tabs = self.tabs.borrow_mut();
            let Some(tab) = tabs.iter_mut().find(|t| &t.page == page) else {
                return;
            };
            match &state {
                QuotaState::Exhausted { detail } => {
                    let who = tab.profile.as_deref().unwrap_or("This session");
                    info!("{who} is out of quota: {detail}");
                    tab.quota_banner
                        .set_title(&format!("{who} is out of quota — {detail}"));
                    match &target {
                        Some(name) => {
                            tab.quota_banner
                                .set_button_label(Some(&format!("Continue in {name}")));
                            tab.quota_banner
                                .set_action_target_value(Some(&name.to_variant()));
                        }
                        None => tab.quota_banner.set_button_label(None),
                    }
                    tab.quota_banner.set_revealed(true);
                    // The tab in view shows its banner; a marker there would
                    // stay lit until the user switched away and back.
                    let in_view = self
                        .tab_view
                        .borrow()
                        .as_ref()
                        .and_then(|view| view.selected_page())
                        .as_ref()
                        == Some(page);
                    if !in_view {
                        page.set_needs_attention(true);
                    }
                    if !tab.quota_notified && self.config.borrow().notify_on_quota {
                        tab.quota_notified = true;
                        notify = Some((tab.key, who.to_string(), detail.clone()));
                    }
                }
                QuotaState::Available => {
                    tab.quota_banner.set_revealed(false);
                    if std::mem::take(&mut tab.quota_notified) {
                        withdraw = Some(tab.key);
                    }
                }
                QuotaState::Unknown => {}
            }
            tab.quota = state;
        }

        if let Some((key, who, detail)) = notify {
            self.send_quota_notification(key, &who, &detail, target.as_deref());
        }
        if let Some(key) = withdraw {
            self.withdraw_quota_notification(key);
        }
    }

    fn quota_notification_id(key: u64) -> String {
        format!("agent-terminal-quota-{key}")
    }

    /// Raises the desktop notification for a tab out of quota.
    ///
    /// Clicking the body brings the tab forward; the button, when there is a
    /// profile to hand off to, runs the same hand-off as the banner. Both go
    /// through app actions keyed by the tab, since a notification outlives the
    /// focus and selection it was sent under.
    fn send_quota_notification(&self, key: u64, who: &str, detail: &str, target: Option<&str>) {
        let Some(app) = self.obj().application() else {
            return;
        };
        let notification = gtk4::gio::Notification::new(&format!("{who} is out of quota"));
        notification.set_body(Some(detail));
        notification.set_priority(gtk4::gio::NotificationPriority::High);
        notification.set_default_action_and_target_value("app.show-tab", Some(&key.to_variant()));
        if let Some(target) = target {
            notification.add_button_with_target_value(
                &format!("Continue in {target}"),
                "app.continue-tab-in",
                Some(&(key, target.to_string()).to_variant()),
            );
        }
        app.send_notification(Some(&Self::quota_notification_id(key)), &notification);
    }

    /// Withdraws every notification raised for this window's tabs. The bell's
    /// is shared by every window, so it goes only if no other is still waiting.
    fn withdraw_notifications(&self) {
        for tab in self.tabs.borrow_mut().iter_mut() {
            tab.bell_pending = false;
        }
        self.withdraw_bell_if_answered();
        self.withdraw_quota_notifications();
    }

    /// Withdraws the out-of-quota notifications raised for this window's tabs.
    fn withdraw_quota_notifications(&self) {
        let Some(app) = self.obj().application() else {
            return;
        };
        // Decided under the borrow, withdrawn after it: GApplication must not
        // be called into with a tab borrowed.
        let keys: Vec<u64> = self
            .tabs
            .borrow_mut()
            .iter_mut()
            .filter_map(|tab| std::mem::take(&mut tab.quota_notified).then_some(tab.key))
            .collect();
        for key in keys {
            app.withdraw_notification(&Self::quota_notification_id(key));
        }
    }

    fn withdraw_quota_notification(&self, key: u64) {
        if let Some(app) = self.obj().application() {
            app.withdraw_notification(&Self::quota_notification_id(key));
        }
    }

    /// Selects the tab with `key` and brings its window forward. `false` if
    /// this window does not hold it.
    pub fn show_tab(&self, key: u64) -> bool {
        let page = self
            .tabs
            .borrow()
            .iter()
            .find(|t| t.key == key)
            .map(|t| t.page.clone());
        let Some(page) = page else {
            return false;
        };
        if let Some(view) = self.tab_view.borrow().as_ref() {
            view.set_selected_page(&page);
        }
        self.obj().present();
        true
    }

    /// [`Self::continue_in`] for the tab with `key`, from a notification.
    pub fn continue_tab_in(&self, key: u64, target: &str) -> bool {
        if !self.show_tab(key) {
            return false;
        }
        self.continue_in(target);
        true
    }

    /// The session items shared by the new-tab dropdown and the context menu:
    /// resume (with the active profile, or as a chosen one), and hand the
    /// current tab's task to another CLI.
    fn build_session_section(&self) -> gtk4::gio::Menu {
        let section = gtk4::gio::Menu::new();
        section.append(Some("Resume Session…"), Some("win.resume-session"));
        section.append_submenu(
            Some("Resume Session As"),
            &self.build_profile_menu("win.resume-session-profile", Profile::can_resume),
        );
        section.append_submenu(
            Some("Continue In"),
            &self.build_profile_menu("win.continue-in", Profile::can_take_prompt),
        );
        section
    }
}

/// Up to `rows` rows of a terminal's buffer ending just above the cursor, as
/// plain text — the live screen, not wherever the user has scrolled to.
///
/// The quota watcher calls this every few seconds on the main thread (VTE can
/// only be read there), so it reads just those rows rather than the whole
/// scrollback that [`screen_text`] copies. The cursor's own row is left out:
/// in a line-based CLI it is where the user types, so a message that merely
/// mentions "quota exceeded" does not raise a banner while it is written.
/// Ceiling: a TUI that parks its cursor below an input box still has that box
/// scanned, and a sent message echoed above the cursor can still match.
fn text_above_cursor(terminal: &Terminal, rows: i64) -> Option<String> {
    // Absolute buffer rows, the same coordinates text_range_format takes.
    let (_, cursor_row) = terminal.cursor_position();
    let end = cursor_row - 1;
    if end < 0 {
        return None;
    }
    let start = (end - rows + 1).max(0);
    let (text, _) =
        terminal.text_range_format(Format::Text, start, 0, end, terminal.column_count());
    text.map(|t| t.to_string())
}

/// A terminal's scrollback and screen as plain text, or `None` if VTE could
/// not write it. Copies the whole scrollback: for on-demand use only (a
/// hand-off brief), never on a timer — see [`text_above_cursor`].
fn screen_text(terminal: &Terminal) -> Option<String> {
    let stream = gtk4::gio::MemoryOutputStream::new_resizable();
    if let Err(err) = terminal.write_contents_sync(
        &stream,
        vte4::WriteFlags::Default,
        None::<&gtk4::gio::Cancellable>,
    ) {
        warn!("Could not read the terminal's contents: {err}");
        return None;
    }
    stream.close(None::<&gtk4::gio::Cancellable>).ok()?;
    Some(String::from_utf8_lossy(&stream.steal_as_bytes()).into_owned())
}

/// Where a "new tab as <profile>" tab is rooted: the profile's own directory
/// wins, then the current tab's; `None` falls through to the starting directory.
fn profile_tab_dir(profile_dir: Option<&str>, current_dir: Option<String>) -> Option<String> {
    profile_dir.map(str::to_string).or(current_dir)
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
    std::path::Path::new(&crate::utils::expand_tilde_with(trimmed, &home)).is_dir()
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
    fn checkpoint_results_become_toasts_and_only_new_ones_change_the_tooltip() {
        use crate::git::{Checkpoint, CheckpointOutcome, SkipReason, Skipped};
        let created = CheckpointResult::Done(CheckpointOutcome::Created(
            Checkpoint {
                seq: 4,
                refname: "refs/agent-terminal/1/0004".into(),
                commit: "c".into(),
                tree: "t".into(),
            },
            vec![Skipped {
                path: ".env".into(),
                reason: SkipReason::Secret,
            }],
        ));
        assert_eq!(
            describe_checkpoint(&created, "14:02"),
            CheckpointReport {
                toast: "Checkpoint 4 · 14:02 · 1 path(s) not captured".into(),
                tooltip: Some("Checkpoint 4 · 14:02 · 1 path(s) not captured".into()),
            }
        );
        for other in [
            CheckpointResult::NotRepo,
            CheckpointResult::Done(CheckpointOutcome::Busy),
            CheckpointResult::Done(CheckpointOutcome::Unchanged),
            CheckpointResult::Failed("boom".into()),
        ] {
            let report = describe_checkpoint(&other, "14:02");
            assert_eq!(report.tooltip, None);
            assert!(!report.toast.is_empty());
        }
        assert_eq!(
            describe_checkpoint(&CheckpointResult::Failed("boom".into()), "").toast,
            "Checkpoint failed: boom"
        );
    }

    #[test]
    fn a_worktree_tab_names_its_branch_before_its_checkpoint() {
        assert_eq!(
            tab_tooltip(Some("feat/x"), Some("Checkpoint 2 · 14:02")),
            "⎇ feat/x · Checkpoint 2 · 14:02"
        );
        assert_eq!(tab_tooltip(Some("feat/x"), None), "⎇ feat/x");
        assert_eq!(
            tab_tooltip(None, Some("Checkpoint 1 · 09:00")),
            "Checkpoint 1 · 09:00"
        );
        assert_eq!(tab_tooltip(None, None), "");
    }

    #[test]
    fn a_lost_checkpoint_attempt_expires() {
        let mut track = CheckpointTrack::default();
        assert!(!track.running());
        track.started = Some(std::time::Instant::now());
        assert!(track.running());
        // A result that never came back must not stop checkpoints for good.
        track.started = std::time::Instant::now().checked_sub(CHECKPOINT_STALE);
        assert!(!track.running());
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
    fn every_window_sees_one_config() {
        // Per-window copies let one window's save revert another's change.
        let first = SharedConfig::default();
        let second = SharedConfig::default();
        first.borrow_mut().scrollback_lines = 4242;
        assert_eq!(second.borrow().scrollback_lines, 4242);
    }

    #[test]
    fn tab_keys_are_unique_and_not_reused_across_runs() {
        // A stored notification from an earlier run names that run's keys;
        // counting from 1 again would point it at an unrelated live tab.
        let first = next_tab_key();
        let second = next_tab_key();
        assert_ne!(first, second);
        assert!(first > 1 << 32, "keys must carry a per-process offset");
    }

    #[test]
    fn profile_tab_follows_current_tab_unless_profile_pins_a_dir() {
        let current = Some("/work/project".to_string());
        // Switching CLI mid-task keeps the project.
        assert_eq!(
            profile_tab_dir(None, current.clone()).as_deref(),
            Some("/work/project")
        );
        // A profile rooted elsewhere still wins.
        assert_eq!(
            profile_tab_dir(Some("~/other"), current).as_deref(),
            Some("~/other")
        );
        // No tab yet: fall through to the starting directory.
        assert_eq!(profile_tab_dir(None, None), None);
    }

    #[test]
    fn test_window_initialization() {
        init_gtk();

        // Under test a window starts from default settings (initial_config), so
        // constructing one must not touch any config directory — it used to
        // read, and could migrate into, the developer's real ~/.config.
        // XDG_CONFIG_HOME is redirected as a net: if that regresses, the
        // damage lands in a temp directory and the assertion below says so.
        //
        // Safe despite tests running in parallel: this is the only test that
        // could reach config_dir() at all, since the config tests all use
        // explicit paths via load_from/save_to.
        let config_home = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", config_home.path());

        // Registered, so `startup` has run before a window is added — as in
        // the real app. NON_UNIQUE: no session bus name to claim, which also
        // keeps the test independent of whether a bus is running.
        let app = adw::Application::builder()
            .application_id("org.test.Window")
            .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk4::gio::Cancellable>)
            .expect("registering a non-unique application");
        let window = super::super::AgentTerminalWindow::new(&app);

        assert_eq!(window.title(), Some("Agent Terminal".into()));
        assert!(
            !config_home.path().join("agent-terminal").exists(),
            "window construction touched the config directory"
        );

        // Here rather than in a test of its own: GTK belongs to the one
        // thread that initialised it, and tests run on several.
        diff_panel_shows_each_outcome();
        crate::chat::view::tests::ui_checks();
        crate::chat::view::tests::diff_ui_checks();
        crate::chat::view::tests::reasoning_ui_checks();
        crate::chat::view::tests::subagent_ui_checks();
        thread_menu::tests::gtk_checks();
        chat_shell_opens_threads_and_lists_them(&window);
    }

    /// The chat-first shell, without a main loop: nothing is resolved or spawned (the agent
    /// commands point nowhere anyway), only the pages, the registry and the sidebar.
    fn chat_shell_opens_threads_and_lists_them(window: &super::super::AgentTerminalWindow) {
        use agent_core::adapter::Driver;
        let imp = window.imp();
        for p in imp.config.borrow_mut().profiles.iter_mut() {
            p.command = format!("/nonexistent/{}", p.command);
        }
        let container = Box::new(Orientation::Vertical, 0);
        imp.setup_terminal_ui(&container, None);
        assert_eq!(
            imp.tabs.borrow().len(),
            0,
            "nothing restored, no blank thread"
        );

        let dir = tempfile::tempdir().unwrap();
        let dir_s = dir.path().to_string_lossy().into_owned();
        imp.new_chat_thread(Some(Driver::Agy), Some(dir_s.clone()), None);
        imp.new_chat_thread(Some(Driver::Claude), Some(dir_s.clone()), None);
        let tabs = imp.tabs.borrow();
        assert_eq!(tabs.len(), 2);
        assert!(tabs.iter().all(|t| t.chat.is_some() && t.dir == dir_s));
        assert_eq!(tabs[0].chat.as_ref().map(|c| c.driver), Some(Driver::Agy));
        drop(tabs);
        let rows = imp.sidebar_rows();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.open && r.folder == dir_s));

        // Archive and delete go through the store; the sidebar follows the loaded list.
        let ids: Vec<String> = rows
            .iter()
            .filter_map(|r| match &r.key {
                crate::window::sidebar_model::RowKey::Thread(id) => Some(id.clone()),
                crate::window::sidebar_model::RowKey::Terminal(_) => None,
            })
            .collect();
        let store = threads::app_store();
        store.set_archived(&ids[0], true).expect("archive");
        imp.reload_summaries();
        let rows = imp.sidebar_rows();
        assert_eq!(rows.iter().filter(|r| r.archived).count(), 1);
        let hidden = crate::window::sidebar_model::group_rows(rows.clone(), "", false);
        assert_eq!(
            hidden.iter().map(|(_, r)| r.len()).sum::<usize>(),
            2,
            "an archived thread that is open in the window stays listed"
        );
        store.delete_thread(&ids[0]).expect("delete");
        imp.reload_summaries();
        assert_eq!(
            imp.sidebar_rows().len(),
            1,
            "the deleted thread left the list"
        );
        assert!(imp.summary_of(&ids[0]).is_none());
    }

    fn diff_panel_shows_each_outcome() {
        use crate::git::{DiffOutcome, TabDiff};
        let panel = DiffPanel::new(&Theme::diff_colours(Default::default()));

        let generation = panel.begin();
        panel.show(generation, Err("fatal: <bad>".into()));
        let (page, title, ..) = panel.visible_state();
        assert_eq!((page.as_str(), title.as_str()), ("status", "Git failed"));

        let generation = panel.begin();
        panel.show(generation, Ok(DiffOutcome::NotRepo));
        assert_eq!(panel.visible_state().1, "Not a git repository");

        let generation = panel.begin();
        panel.show(
            generation,
            Ok(DiffOutcome::Ready(TabDiff {
                stats: Vec::new(),
                text: String::new(),
                omitted_lines: 0,
                too_large: false,
                undo_to: None,
                from: String::new(),
                to_checkpoint: None,
            })),
        );
        assert_eq!(panel.visible_state().1, "No changes");
        assert!(!panel.undo_offered());

        let text = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n";
        let generation = panel.begin();
        panel.show(
            generation,
            Ok(DiffOutcome::Ready(TabDiff {
                stats: crate::diff::parse_numstat(b"1\t1\tx\0"),
                text: text.into(),
                omitted_lines: 3,
                too_large: false,
                undo_to: Some("c0ffee".into()),
                from: "c0ffee".into(),
                to_checkpoint: None,
            })),
        );
        assert!(panel.undo_offered());
        let (page, _, summary, shown, rows) = panel.visible_state();
        assert_eq!(page, "diff");
        assert_eq!(summary, "1 file, +1 −1");
        assert!(shown.starts_with(text));
        assert!(shown.ends_with("3 more lines not shown"), "{shown}");
        assert_eq!(rows, 1);

        // Before the paned has a size, nothing is placed and no position is
        // taken for the user's choice of width.
        let paned = gtk4::Paned::builder()
            .orientation(Orientation::Horizontal)
            .end_child(&panel.root)
            .build();
        panel.set_shown(true);
        panel.place(&paned, 400);
        assert_eq!(panel.dragged_width(&paned), None);

        // A slow refresh finishing after a newer one started is dropped.
        let stale = panel.begin();
        let current = panel.begin();
        panel.show(stale, Ok(DiffOutcome::NotRepo));
        assert_eq!(panel.visible_state().0, "diff");
        panel.show(current, Ok(DiffOutcome::NotRepo));
        assert_eq!(panel.visible_state().0, "status");
    }
}
