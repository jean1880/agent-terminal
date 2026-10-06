//! The chat-first shell: thread pages (a [`ChatView`] over a [`ChatSession`]) inside the hidden
//! tab view, the thread sidebar, the terminal drawer, and the 2.x features re-homed onto them.
//!
//! A thread page is an ordinary [`TabState`] with `chat: Some(..)`, so the registry, the diff
//! panel, checkpoints, worktrees and attention keep working through one code path. Its
//! `terminal` is the drawer's shell, spawned the first time the drawer opens.
//!
//! Re-entrancy: a session may call its sink (and so [`AgentTerminalWindow::on_thread_envelope`])
//! from inside any call into it, so no `tabs` borrow is ever held across a call into a session
//! or a view.

use std::collections::HashMap;
use std::rc::Rc;

use agent_core::adapter::{Adapter, Control, Driver, Mode, OpenSession};
use agent_core::event::{Decision, Envelope, Event, ItemKind};
use agent_kit::store::Store;

use super::*;
use crate::account_status::AccountStatus;
use crate::approval_server::ApprovalHandle;
use crate::chat::session::{build_handoff, AgentLaunch, ChatSession, LaunchEnv};
use crate::chat::view::usage::UsageIndicator;
use crate::chat::view::{ChatView, ViewAction};
use crate::chat::{ChatBackend, EnvelopeSink, SessionStatus};
use crate::config::{profile_driver, Profile};
use crate::model_catalog::ModelCatalog;
use crate::window::sidebar_model::{
    badge_for, driver_key, driver_label, group_rows, other_driver, parse_driver, relative_time,
    resume_as, stored_model, thread_title, Badge, ResumeAs, RowKey, SidebarRow,
};

/// The store key holding the open-thread list (`{"open": [ids], "selected": id}`).
const OPEN_THREADS_KEY: &str = "open_threads";
/// Most events replayed into a thread's view when it opens.
const REPLAY_LIMIT: usize = 50_000;
/// How often the account/usage indicator refreshes while the window is focused.
const ACCOUNT_REFRESH: std::time::Duration = std::time::Duration::from_secs(600);

/// One chat thread page's state, beside its [`TabState`].
pub(super) struct ChatTab {
    pub(super) thread: String,
    /// The agent when the page was opened, until the session reports its own.
    pub(super) driver: Driver,
    pub(super) title: String,
    /// Holds the chat view once built (the page is built the first time it is shown).
    holder: gtk4::Box,
    pub(super) slot: Rc<SessionSlot>,
    view: Option<ChatView>,
    building: bool,
    /// The drawer: the paned's end child, hidden until toggled.
    drawer: gtk4::Box,
    drawer_paned: gtk4::Paned,
    drawer_started: bool,
    rate_banner: adw::Banner,
    running: bool,
    approval: bool,
    rate_limited: bool,
    unread: bool,
    /// The user message item awaiting its text, for the title of an untitled thread.
    title_item: Option<String>,
    /// Sent once the session exists (Continue In from a terminal page).
    pending_prompt: Option<String>,
    /// Seeded into the session once it exists (fork, compact-by-handoff).
    pending_handoff: Option<(String, usize, String)>,
}

// ---------------------------------------------------------------------------------------------
// The backend the view is built with, before its session exists
// ---------------------------------------------------------------------------------------------

/// The view needs a backend at construction and the session needs the view's sink, so the view
/// gets this slot, which forwards to the session once it is set.
pub(super) struct SessionSlot {
    session: RefCell<Option<Rc<ChatSession>>>,
    driver: Driver,
    model: Option<String>,
    mode: Mode,
}

impl SessionSlot {
    fn get(&self) -> Option<Rc<ChatSession>> {
        self.session.borrow().clone()
    }

    pub(super) fn driver(&self) -> Driver {
        match self.get() {
            Some(s) => s.status().driver,
            None => self.driver,
        }
    }
}

impl ChatBackend for SessionSlot {
    fn send_prompt(&self, text: &str) {
        match self.get() {
            Some(s) => s.send_prompt(text),
            None => warn!("a prompt arrived before the thread's session started"),
        }
    }
    fn interrupt(&self) {
        if let Some(s) = self.get() {
            s.interrupt();
        }
    }
    fn respond_approval(&self, request: &str, decision: Decision) {
        if let Some(s) = self.get() {
            s.respond_approval(request, decision);
        }
    }
    fn answer_questions(&self, request: &str, answers: serde_json::Value) {
        if let Some(s) = self.get() {
            s.answer_questions(request, answers);
        }
    }
    fn switch(&self, driver: Driver, model: Option<String>, effort: Option<String>) {
        if let Some(s) = self.get() {
            s.switch(driver, model, effort);
        }
    }
    fn set_mode(&self, mode: Mode) {
        if let Some(s) = self.get() {
            s.set_mode(mode);
        }
    }
    fn control(&self, control: Control) -> String {
        match self.get() {
            Some(s) => s.control(control),
            None => "ctl-unstarted".to_owned(),
        }
    }
    fn status(&self) -> SessionStatus {
        match self.get() {
            Some(s) => s.status(),
            None => SessionStatus {
                driver: self.driver,
                model: self.model.clone(),
                effort: None,
                mode: self.mode,
                running_turn: false,
                alive: false,
                capabilities: match self.driver {
                    Driver::Claude => agent_core::caps::Capabilities::claude(),
                    Driver::Agy => agent_core::caps::Capabilities::agy(),
                },
                commands: Vec::new(),
            },
        }
    }
}

// ---------------------------------------------------------------------------------------------
// App-wide state: the store and the resolved agent binaries
// ---------------------------------------------------------------------------------------------

/// A chat agent's binary and its profile's environment, resolved off the main thread.
#[derive(Debug, Clone, Default)]
pub(super) struct ResolvedAgent {
    /// `None`: not found by any probe (the command name is then tried as-is).
    pub(super) program: Option<String>,
    pub(super) env: Vec<(String, String)>,
}

thread_local! {
    static STORE: RefCell<Option<Rc<Store>>> = const { RefCell::new(None) };
    /// Keyed by (command, env file): a profile edit is a new key, so nothing goes stale.
    static RESOLVED: RefCell<HashMap<(String, Option<String>), ResolvedAgent>> =
        RefCell::new(HashMap::new());
    static CHAT_RESTORED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The app's one thread store, opened on first use. A store that cannot be opened falls back
/// to memory (threads then last for this run), and says so in the log.
pub(super) fn app_store() -> Rc<Store> {
    if let Some(store) = STORE.with(|s| s.borrow().clone()) {
        return store;
    }
    let store = open_store();
    STORE.with(|s| *s.borrow_mut() = Some(store.clone()));
    store
}

#[cfg(not(test))]
fn open_store() -> Rc<Store> {
    let path = agent_kit::store::default_path(
        env::var("XDG_STATE_HOME").ok().as_deref(),
        env::var("HOME").ok().as_deref(),
    );
    if let Some(path) = path {
        match Store::open(&path) {
            Ok(store) => return Rc::new(store),
            Err(e) => error!("Cannot open the thread store at {}: {e}", path.display()),
        }
    }
    warn!("Threads are kept in memory for this run only");
    in_memory_store()
}

/// Tests never touch the real `~/.local/state`.
#[cfg(test)]
fn open_store() -> Rc<Store> {
    in_memory_store()
}

fn in_memory_store() -> Rc<Store> {
    match Store::open_in_memory() {
        Ok(store) => Rc::new(store),
        // An in-memory SQLite that cannot open means no SQLite at all.
        Err(e) => panic!("SQLite is unusable: {e}"),
    }
}

/// The cached resolution of `profile`'s command and env file, if done.
pub(super) fn cached_agent(profile: &Profile) -> Option<ResolvedAgent> {
    let key = (profile.command.clone(), profile.env_file.clone());
    RESOLVED.with(|r| r.borrow().get(&key).cloned())
}

/// Resolves `profile`'s binary (the same probes as detection) and sources its env file, off
/// the main thread. Cached per (command, env file).
pub(super) async fn resolve_agent(profile: Profile) -> ResolvedAgent {
    if let Some(found) = cached_agent(&profile) {
        return found;
    }
    let (path, home, shell) = env_triplet();
    let command = profile.command.clone();
    let env_file = profile.env_file.clone();
    let resolved = gtk4::gio::spawn_blocking(move || {
        let probe = crate::utils::SystemProbe::new(path, home, shell);
        let program = probe.locate(&command);
        let env = env_file
            .as_deref()
            .filter(|f| !f.trim().is_empty())
            .map(crate::utils::load_env_file)
            .unwrap_or_default();
        ResolvedAgent { program, env }
    })
    .await
    .unwrap_or_else(|_| {
        error!("Agent resolution panicked on the worker thread");
        ResolvedAgent::default()
    });
    crate::utils::cache_command_available(&profile.command, resolved.program.is_some());
    RESOLVED.with(|r| {
        r.borrow_mut().insert(
            (profile.command.clone(), profile.env_file.clone()),
            resolved.clone(),
        )
    });
    resolved
}

/// What the app removes from an agent's environment: the configured `clear_env` (a launching
/// agent session's markers) and any inherited approval socket, which only the app may set.
fn unset_list(patterns: &[String]) -> Vec<String> {
    let mut env: Vec<String> = std::env::vars().map(|(k, v)| format!("{k}={v}")).collect();
    let mut names = crate::utils::strip_env(&mut env, patterns);
    for own in [
        agent_core::approval::ENV_SOCKET,
        crate::approval_server::ENV_HOOK_BIN,
    ] {
        if !names.iter().any(|n| n == own) {
            names.push(own.to_owned());
        }
    }
    names
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Builds the adapter, its approval socket and environment for `driver` from the config, using
/// the cached resolution (a switch cannot wait). Not resolved yet: the command name is tried.
fn agent_launch(driver: Driver, thread: &str, cwd: &str) -> AgentLaunch {
    let config = SharedConfig::default();
    let (profile, clear) = {
        let c = config.borrow();
        (
            c.agent_profile(driver)
                .cloned()
                .unwrap_or_else(|| crate::config::new_agent_profile(driver)),
            c.clear_env.clone(),
        )
    };
    let resolved = cached_agent(&profile).unwrap_or_default();
    let program = resolved
        .program
        .clone()
        .unwrap_or_else(|| profile.command.clone());
    let adapter: std::boxed::Box<dyn Adapter> = match driver {
        Driver::Claude => std::boxed::Box::new(agent_core::claude::ClaudeAdapter::new()),
        Driver::Agy => std::boxed::Box::new(agent_core::agy::AgyAdapter::new(program.clone())),
    };
    let approval = match driver {
        Driver::Claude => None,
        Driver::Agy => bind_approval(thread, cwd, profile.default_mode.unwrap_or_default()),
    };
    AgentLaunch {
        adapter,
        program,
        extra_args: profile.args.clone(),
        default_model: profile.default_model.clone(),
        default_effort: profile.default_effort.clone(),
        env: LaunchEnv {
            env: resolved.env,
            unset: unset_list(&clear),
        },
        approval,
    }
}

/// agy's approval socket, only once its hooks file is proven to install the hook. `None`: agy
/// runs read-only and the session explains how to install it.
fn bind_approval(thread: &str, cwd: &str, mode: Mode) -> Option<ApprovalHandle> {
    let home = env::var("HOME").ok();
    match ApprovalHandle::bind_checked(home.as_deref(), thread, std::path::Path::new(cwd), mode) {
        Ok(handle) => Some(handle),
        Err(reason) => {
            info!("agy runs read-only in this thread: {reason}");
            None
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The sidebar
// ---------------------------------------------------------------------------------------------

pub(super) struct Sidebar {
    pub(super) root: gtk4::Box,
    list: gtk4::ListBox,
    search: gtk4::SearchEntry,
    /// Row index → what it opens (`None` for a folder header).
    keys: RefCell<Vec<Option<RowKey>>>,
    /// Set while the list is rebuilt, so selecting the current row is not taken as a click.
    rebuilding: std::cell::Cell<bool>,
    /// Held for the sidebar's lifetime: the footer's usage indicator.
    _usage: Rc<UsageIndicator>,
}

impl AgentTerminalWindow {
    /// Builds the chat-first shell into `container`: the sidebar beside the (tab-bar-less) tab
    /// view, with an empty state when no page is open.
    pub(super) fn setup_shell(&self, container: &Box, tab_view: &adw::TabView) {
        let obj = self.obj();
        let split = adw::OverlaySplitView::new();
        split.set_vexpand(true);
        split.set_min_sidebar_width(240.0);
        split.set_max_sidebar_width(340.0);
        split.set_show_sidebar(!self.config.borrow().sidebar_collapsed);

        let sidebar = self.build_sidebar();
        split.set_sidebar(Some(&sidebar.root));

        let toast_overlay = adw::ToastOverlay::new();
        let pages = gtk4::Stack::new();
        pages.add_named(tab_view, Some("threads"));
        pages.add_named(&self.build_empty_state(), Some("empty"));
        pages.set_visible_child_name("empty");
        toast_overlay.set_child(Some(&pages));
        toast_overlay.set_vexpand(true);
        split.set_content(Some(&toast_overlay));
        container.append(&split);
        *self.toast_overlay.borrow_mut() = Some(toast_overlay);
        *self.pages_stack.borrow_mut() = Some(pages);

        // Narrow windows overlay the sidebar instead of squeezing the thread.
        let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            720.0,
            adw::LengthUnit::Sp,
        ));
        breakpoint.add_setter(&split, "collapsed", Some(&true.to_value()));
        obj.add_breakpoint(breakpoint);

        if let Some(header) = self.header.borrow().as_ref() {
            let toggle = gtk4::ToggleButton::builder()
                .icon_name("sidebar-show-symbolic")
                .tooltip_text("Show or Hide Threads (F9, Ctrl+B)")
                .active(split.shows_sidebar())
                .build();
            split
                .bind_property("show-sidebar", &toggle, "active")
                .bidirectional()
                .build();
            // Only a user's toggle is remembered, not the narrow-width overlay closing itself.
            toggle.connect_clicked(glib::clone!(
                #[weak]
                obj,
                move |t| obj.imp().remember_sidebar(t.is_active())
            ));
            header.pack_start(&toggle);
        }
        *self.split_view.borrow_mut() = Some(split);
        *self.sidebar.borrow_mut() = Some(sidebar);

        self.wire_thread_pages(tab_view);
        self.watch_account_status();
        self.refresh_sidebar();
        // Relative times age; a minute is their resolution.
        glib::timeout_add_seconds_local(
            60,
            glib::clone!(
                #[weak]
                obj,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    obj.imp().refresh_sidebar();
                    glib::ControlFlow::Continue
                }
            ),
        );
    }

    fn remember_sidebar(&self, shown: bool) {
        if self.config.borrow().sidebar_collapsed == !shown {
            return;
        }
        self.config.borrow_mut().sidebar_collapsed = !shown;
        self.schedule_config_save();
    }

    /// F9 / Ctrl+B.
    pub(super) fn toggle_sidebar(&self) {
        let split = self.split_view.borrow().clone();
        if let Some(split) = split {
            let shown = !split.shows_sidebar();
            split.set_show_sidebar(shown);
            // Collapsed (narrow) mode overlays it; that is not a preference to keep.
            if !split.is_collapsed() {
                self.remember_sidebar(shown);
            }
        }
    }

    fn build_empty_state(&self) -> gtk4::Widget {
        let obj = self.obj();
        let page = adw::StatusPage::builder()
            .icon_name("chat-message-new-symbolic")
            .title("No thread open")
            .description(
                "Start a new thread, or pick one from the sidebar.\n\
                 Ctrl+Shift+T new thread · Ctrl+Shift+G new thread in a worktree · F9 threads",
            )
            .vexpand(true)
            .build();
        let new = Button::builder()
            .label("New Thread")
            .halign(Align::Center)
            .css_classes(["suggested-action", "pill"])
            .build();
        new.connect_clicked(glib::clone!(
            #[weak]
            obj,
            move |_| obj.imp().new_tab()
        ));
        page.set_child(Some(&new));
        page.upcast()
    }

    fn build_sidebar(&self) -> Rc<Sidebar> {
        let obj = self.obj();
        let root = Box::builder()
            .orientation(Orientation::Vertical)
            .css_classes(["thread-sidebar"])
            .build();
        let top = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(6)
            .margin_top(8)
            .margin_bottom(6)
            .margin_start(8)
            .margin_end(8)
            .build();
        let search = gtk4::SearchEntry::builder()
            .placeholder_text("Search threads")
            .hexpand(true)
            .build();
        let new = Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("New Thread (Ctrl+Shift+T)")
            .css_classes(["flat"])
            .build();
        new.connect_clicked(glib::clone!(
            #[weak]
            obj,
            move |_| obj.imp().new_tab()
        ));
        top.append(&search);
        top.append(&new);
        root.append(&top);

        let list = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::Single)
            .css_classes(["navigation-sidebar", "thread-list"])
            .build();
        let scrolled = ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vexpand(true)
            .child(&list)
            .build();
        root.append(&scrolled);

        let footer = Box::builder()
            .orientation(Orientation::Vertical)
            .css_classes(["sidebar-footer"])
            .build();
        // Every agent's usage and account, updated on every turn.
        let usage = UsageIndicator::new(AccountStatus::shared(), None);
        footer.append(usage.widget());
        root.append(&footer);

        let sidebar = Rc::new(Sidebar {
            root,
            list: list.clone(),
            search: search.clone(),
            keys: RefCell::new(Vec::new()),
            rebuilding: std::cell::Cell::new(false),
            _usage: usage,
        });
        search.connect_search_changed(glib::clone!(
            #[weak]
            obj,
            move |_| obj.imp().refresh_sidebar()
        ));
        let weak_sidebar = Rc::downgrade(&sidebar);
        list.connect_row_activated(glib::clone!(
            #[weak]
            obj,
            move |_, row| {
                let Some(sidebar) = weak_sidebar.upgrade() else {
                    return;
                };
                if sidebar.rebuilding.get() {
                    return;
                }
                let key = usize::try_from(row.index())
                    .ok()
                    .and_then(|i| sidebar.keys.borrow().get(i).cloned().flatten());
                if let Some(key) = key {
                    obj.imp().open_row(&key);
                }
            }
        ));
        sidebar
    }

    /// Opens (or shows) what a sidebar row stands for.
    fn open_row(&self, key: &RowKey) {
        match key {
            RowKey::Thread(id) => {
                self.open_thread(id, true);
            }
            RowKey::Terminal(key) => {
                self.show_tab(*key);
            }
        }
        // In the narrow overlay mode, picking a thread is done with the sidebar.
        if let Some(split) = self.split_view.borrow().as_ref() {
            if split.is_collapsed() {
                split.set_show_sidebar(false);
            }
        }
    }

    /// The rows the sidebar shows: every stored thread, with the live state of the open ones,
    /// and the open terminal pages.
    pub(super) fn sidebar_rows(&self) -> Vec<SidebarRow> {
        let store = app_store();
        let summaries = store.list_threads(false).unwrap_or_else(|e| {
            warn!("Cannot list threads: {e}");
            Vec::new()
        });
        let tabs = self.tabs.borrow();
        let mut rows: Vec<SidebarRow> = summaries
            .into_iter()
            .map(|s| {
                let tab = tabs
                    .iter()
                    .find(|t| t.chat.as_ref().is_some_and(|c| c.thread == s.id));
                let chat = tab.and_then(|t| t.chat.as_ref());
                let badge = match chat {
                    Some(c) => badge_for(c.running, c.approval, c.rate_limited, c.unread),
                    // A closed thread with events nobody has seen.
                    None => badge_for(false, false, false, s.unread && s.updated_at > 0),
                };
                let title = match chat {
                    Some(c) if !c.title.is_empty() => c.title.clone(),
                    _ if !s.title.is_empty() => s.title.clone(),
                    _ => "New thread".to_owned(),
                };
                SidebarRow {
                    key: RowKey::Thread(s.id.clone()),
                    title,
                    folder: s.cwd.clone(),
                    updated_ms: s.updated_at,
                    driver: chat
                        .map(|c| c.driver)
                        .or_else(|| s.driver.as_deref().and_then(parse_driver)),
                    badge,
                    open: tab.is_some(),
                }
            })
            .collect();
        for tab in tabs.iter().filter(|t| t.chat.is_none()) {
            rows.push(SidebarRow {
                key: RowKey::Terminal(tab.key),
                title: tab.page.title().to_string(),
                folder: tab.dir.clone(),
                updated_ms: tab
                    .started_at
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0),
                driver: None,
                badge: tab.page.needs_attention().then_some(Badge::Unread),
                open: true,
            });
        }
        rows
    }

    /// Rebuilds the sidebar list. Cheap: a few dozen rows, rebuilt on state changes.
    pub(super) fn refresh_sidebar(&self) {
        let Some(sidebar) = self.sidebar.borrow().clone() else {
            return;
        };
        let rows = self.sidebar_rows();
        let selected = self.selected_row_key();
        let groups = group_rows(rows, &sidebar.search.text());
        let home = env::var("HOME").unwrap_or_default();
        let now = now_ms();

        sidebar.rebuilding.set(true);
        while let Some(child) = sidebar.list.first_child() {
            sidebar.list.remove(&child);
        }
        let mut keys = Vec::new();
        let mut select = None;
        for (folder, rows) in groups {
            let header = gtk4::ListBoxRow::builder()
                .activatable(false)
                .selectable(false)
                .css_classes(["folder-row"])
                .build();
            let label = Label::builder()
                .label(crate::utils::tildify(&folder, &home))
                .xalign(0.0)
                .ellipsize(gtk4::pango::EllipsizeMode::Start)
                .css_classes(["folder-label", "caption-heading"])
                .tooltip_text(&folder)
                .build();
            header.set_child(Some(&label));
            sidebar.list.append(&header);
            keys.push(None);
            for row in rows {
                let widget = self.sidebar_row_widget(&row, now);
                sidebar.list.append(&widget);
                if Some(&row.key) == selected.as_ref() {
                    select = Some(widget.clone());
                }
                keys.push(Some(row.key));
            }
        }
        *sidebar.keys.borrow_mut() = keys;
        sidebar.list.select_row(select.as_ref());
        sidebar.rebuilding.set(false);
    }

    fn sidebar_row_widget(&self, row: &SidebarRow, now: i64) -> gtk4::ListBoxRow {
        let obj = self.obj();
        let line = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(8)
            .css_classes(["thread-row"])
            .build();
        match row.driver {
            Some(driver) => {
                let dot = Label::builder()
                    .label("●")
                    .css_classes(["thread-dot", &format!("dot-{}", driver_key(driver))])
                    .tooltip_text(driver_label(driver))
                    .build();
                line.append(&dot);
            }
            None => {
                let icon = Image::from_icon_name("utilities-terminal-symbolic");
                icon.set_tooltip_text(Some("Terminal thread"));
                icon.add_css_class("thread-term");
                line.append(&icon);
            }
        }
        let title = Label::builder()
            .label(&row.title)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(if row.open {
                vec!["thread-title", "open"]
            } else {
                vec!["thread-title"]
            })
            .tooltip_text(&row.title)
            .build();
        line.append(&title);
        if let Some(badge) = row.badge {
            if badge == Badge::Running {
                let spinner = gtk4::Spinner::builder().spinning(true).build();
                spinner.set_tooltip_text(Some(badge.tooltip()));
                line.append(&spinner);
            } else {
                let mark = Label::builder()
                    .label("●")
                    .css_classes(["thread-badge", badge.css_class()])
                    .tooltip_text(badge.tooltip())
                    .build();
                line.append(&mark);
            }
        }
        let age = Label::builder()
            .label(relative_time(now, row.updated_ms))
            .css_classes(["thread-age", "dim-label", "caption"])
            .build();
        line.append(&age);
        if row.open {
            let close = Button::builder()
                .icon_name("window-close-symbolic")
                .tooltip_text("Close (the thread stays in the list)")
                .css_classes(["flat", "circular", "thread-close"])
                .valign(Align::Center)
                .build();
            let key = row.key.clone();
            close.connect_clicked(glib::clone!(
                #[weak]
                obj,
                move |_| obj.imp().close_row(&key)
            ));
            line.append(&close);
        }
        gtk4::ListBoxRow::builder().child(&line).build()
    }

    fn close_row(&self, key: &RowKey) {
        let page = self.tabs.borrow().iter().find_map(|t| {
            let matches = match key {
                RowKey::Thread(id) => t.chat.as_ref().is_some_and(|c| &c.thread == id),
                RowKey::Terminal(k) => t.chat.is_none() && t.key == *k,
            };
            matches.then(|| t.page.clone())
        });
        let view = self.tab_view.borrow().clone();
        if let (Some(page), Some(view)) = (page, view) {
            view.close_page(&page);
        }
    }

    fn selected_row_key(&self) -> Option<RowKey> {
        let page = self.tab_view.borrow().as_ref()?.selected_page()?;
        let tabs = self.tabs.borrow();
        let tab = tabs.iter().find(|t| t.page == page)?;
        Some(match &tab.chat {
            Some(c) => RowKey::Thread(c.thread.clone()),
            None => RowKey::Terminal(tab.key),
        })
    }

    // -----------------------------------------------------------------------------------------
    // Page lifecycle
    // -----------------------------------------------------------------------------------------

    /// Selection, removal and the empty state, for every page kind.
    fn wire_thread_pages(&self, tab_view: &adw::TabView) {
        let obj = self.obj();
        tab_view.connect_selected_page_notify(glib::clone!(
            #[weak]
            obj,
            move |view| {
                let imp = obj.imp();
                if let Some(page) = view.selected_page() {
                    imp.page_shown(&page);
                }
                imp.save_open_threads();
                imp.refresh_sidebar();
            }
        ));
        tab_view.connect_notify_local(
            Some("n-pages"),
            glib::clone!(
                #[weak]
                obj,
                move |view, _| {
                    let imp = obj.imp();
                    if let Some(stack) = imp.pages_stack.borrow().as_ref() {
                        stack.set_visible_child_name(if view.n_pages() == 0 {
                            "empty"
                        } else {
                            "threads"
                        });
                    }
                    if view.n_pages() == 0 {
                        if let Some(title) = imp.window_title.borrow().as_ref() {
                            title.set_title("Agent Terminal");
                            title.set_subtitle("");
                        }
                        obj.set_title(Some("Agent Terminal"));
                    }
                }
            ),
        );
        // After the registry dropped the page (the 2.x handler runs first).
        tab_view.connect_page_detached(glib::clone!(
            #[weak]
            obj,
            move |view, _, _| {
                if view.is_transferring_page() {
                    return;
                }
                let imp = obj.imp();
                imp.save_open_threads();
                imp.refresh_sidebar();
            }
        ));
    }

    /// A page came into view: build a thread on first show, clear its unread state, and put
    /// its title and folder in the header.
    pub(super) fn page_shown(&self, page: &adw::TabPage) {
        let info = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(tab) = tabs.iter_mut().find(|t| &t.page == page) else {
                return;
            };
            let dir = tab.dir.clone();
            tab.chat.as_mut().map(|chat| {
                chat.unread = false;
                (chat.thread.clone(), chat.title.clone(), dir)
            })
        };
        let Some((thread, title, dir)) = info else {
            // A terminal page: the 2.x handler puts its terminal title in the subtitle.
            if let Some(window_title) = self.window_title.borrow().as_ref() {
                window_title.set_title("Terminal");
            }
            return;
        };
        if let Err(e) = app_store().mark_read(&thread) {
            debug!("Cannot mark thread read: {e}");
        }
        let home = env::var("HOME").unwrap_or_default();
        if let Some(window_title) = self.window_title.borrow().as_ref() {
            window_title.set_title(if title.is_empty() {
                "New thread"
            } else {
                &title
            });
            window_title.set_subtitle(&crate::utils::tildify(&dir, &home));
        }
        self.ensure_built(page);
    }

    /// Records the open threads (and the one in view) so the next launch reopens them.
    pub(super) fn save_open_threads(&self) {
        // Nothing to record before the shell exists, or while it is being torn down.
        if self.tab_view.borrow().is_none() || !CHAT_RESTORED.with(std::cell::Cell::get) {
            return;
        }
        let selected = self.selected_row_key();
        let open: Vec<String> = self
            .tabs
            .borrow()
            .iter()
            .filter_map(|t| t.chat.as_ref().map(|c| c.thread.clone()))
            .collect();
        let selected = match selected {
            Some(RowKey::Thread(id)) => Some(id),
            _ => None,
        };
        let value = serde_json::json!({ "open": open, "selected": selected });
        if let Err(e) = app_store().set_meta(OPEN_THREADS_KEY, &value.to_string()) {
            warn!("Cannot record the open threads: {e}");
        }
    }

    /// Reopens the threads open when the app last closed (first window of a launch only).
    /// Only the one in view is built now; the rest are built when first shown.
    pub(super) fn restore_open_threads(&self) -> bool {
        if CHAT_RESTORED.with(|done| done.replace(true)) {
            return false;
        }
        let saved = app_store().meta(OPEN_THREADS_KEY).ok().flatten();
        let Some(value) = saved.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        else {
            return false;
        };
        let known: Vec<String> = app_store()
            .list_threads(false)
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.id)
            .collect();
        let open: Vec<String> = value["open"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .filter(|id| known.contains(id))
                    .take(crate::config::SessionState::MAX_TABS)
                    .collect()
            })
            .unwrap_or_default();
        if open.is_empty() {
            return false;
        }
        info!("Reopening {} thread(s) from the last session", open.len());
        for id in &open {
            self.open_thread(id, false);
        }
        let selected = value["selected"]
            .as_str()
            .filter(|id| open.iter().any(|o| o == id))
            .or(open.first().map(String::as_str))
            .map(str::to_owned);
        if let Some(id) = selected {
            self.open_thread(&id, true);
        }
        true
    }

    /// Shows the thread's page, adding it if it is not open. `select` brings it into view (and
    /// so builds it).
    pub(super) fn open_thread(&self, thread: &str, select: bool) -> Option<adw::TabPage> {
        let existing = self.tabs.borrow().iter().find_map(|t| {
            t.chat
                .as_ref()
                .is_some_and(|c| c.thread == thread)
                .then(|| t.page.clone())
        });
        let page = match existing {
            Some(page) => page,
            None => self.add_thread_page(thread)?,
        };
        if select {
            let view = self.tab_view.borrow().clone();
            if let Some(view) = view {
                if view.selected_page().as_ref() == Some(&page) {
                    self.page_shown(&page);
                } else {
                    view.set_selected_page(&page);
                }
            }
        }
        Some(page)
    }

    /// Adds an (unbuilt) page for a stored thread.
    fn add_thread_page(&self, thread: &str) -> Option<adw::TabPage> {
        let summary = app_store()
            .list_threads(true)
            .ok()?
            .into_iter()
            .find(|s| s.id == thread)?;
        let driver = summary
            .driver
            .as_deref()
            .and_then(parse_driver)
            .or_else(|| self.default_agent())
            .unwrap_or(Driver::Claude);
        let tab_view = self.tab_view.borrow().clone()?;
        let config = self.config.borrow().clone();
        let mode = config
            .agent_profile(driver)
            .and_then(|p| p.default_mode)
            .unwrap_or_default();

        let terminal = Terminal::new();
        let stack = Stack::builder().vexpand(true).build();
        let scrolled = ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .child(&terminal)
            .css_classes(["terminal-container"])
            .build();
        stack.add_named(&build_loading_box(), Some("loading"));
        stack.add_named(&scrolled, Some("terminal"));
        stack.set_visible_child_name("loading");
        let (search_bar, search_entry) = build_search_bar(&terminal);
        let (exit_bar, exit_label, _, _) = build_exit_bar();
        let drawer = Box::builder()
            .orientation(Orientation::Vertical)
            .visible(false)
            .css_classes(["terminal-drawer"])
            .build();
        drawer.append(&search_bar);
        drawer.append(&stack);

        let holder = Box::builder()
            .orientation(Orientation::Vertical)
            .vexpand(true)
            .build();
        let drawer_paned = gtk4::Paned::builder()
            .orientation(Orientation::Vertical)
            .start_child(&holder)
            .end_child(&drawer)
            .resize_start_child(true)
            .shrink_end_child(false)
            .vexpand(true)
            .build();

        let diff_panel = DiffPanel::new(&Theme::diff_colours(config.theme));
        diff_panel.set_shown(config.diff_panel_visible);
        let paned = gtk4::Paned::builder()
            .orientation(Orientation::Horizontal)
            .start_child(&drawer_paned)
            .end_child(&diff_panel.root)
            .resize_start_child(true)
            .resize_end_child(false)
            .shrink_end_child(false)
            .vexpand(true)
            .build();
        self.wire_diff_panel(&diff_panel, &paned);

        let rate_banner = adw::Banner::builder().use_markup(false).build();
        let thread_id = thread.to_owned();
        rate_banner.connect_button_clicked(glib::clone!(
            #[weak(rename_to = banner)]
            rate_banner,
            move |b| {
                if let Some(obj) = window_of(b) {
                    obj.imp().continue_rate_limited(&thread_id);
                }
                banner.set_revealed(false);
            }
        ));
        let content = Box::builder().orientation(Orientation::Vertical).build();
        content.append(&rate_banner);
        content.append(&paned);

        let page = tab_view.append(&content);
        let title = if summary.title.is_empty() {
            "New thread".to_owned()
        } else {
            summary.title.clone()
        };
        page.set_title(&title);

        self.configure_terminal(&terminal);
        self.wire_input_controllers(&terminal);
        // The drawer's shell exiting just closes the drawer; it never takes the thread.
        terminal.connect_child_exited(glib::clone!(
            #[weak]
            page,
            move |terminal, _| {
                if let Some(obj) = window_of(terminal) {
                    obj.imp().drawer_exited(&page);
                }
            }
        ));

        let slot = Rc::new(SessionSlot {
            session: RefCell::new(None),
            driver,
            model: None,
            mode,
        });
        self.tabs.borrow_mut().push(TabState {
            page: page.clone(),
            terminal,
            dir: summary.cwd.clone(),
            stack,
            exit_bar,
            exit_label,
            spawned_at: std::time::Instant::now(),
            search_bar,
            search_entry,
            profile: None,
            session_id: None,
            pinned_id: None,
            started_at: std::time::SystemTime::now(),
            quota_banner: adw::Banner::new(""),
            quota: QuotaState::Unknown,
            screen_dirty: std::rc::Rc::new(std::cell::Cell::new(false)),
            key: next_tab_key(),
            quota_notified: false,
            bell_pending: false,
            last_output: std::rc::Rc::new(std::cell::Cell::new(std::time::Instant::now())),
            checkpoint: CheckpointTrack::default(),
            diff_panel: diff_panel.clone(),
            worktree: None,
            chat: Some(ChatTab {
                thread: thread.to_owned(),
                driver,
                title: summary.title.clone(),
                holder,
                slot,
                view: None,
                building: false,
                drawer,
                drawer_paned,
                drawer_started: false,
                rate_banner,
                running: false,
                approval: false,
                rate_limited: false,
                unread: false,
                title_item: None,
                pending_prompt: None,
                pending_handoff: None,
            }),
        });
        if diff_panel.root.is_visible() {
            self.refresh_diff(&page);
        }
        self.refresh_sidebar();
        Some(page)
    }

    /// Builds a thread's view and starts its session, the first time its page is shown.
    fn ensure_built(&self, page: &adw::TabPage) {
        let job = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(tab) = tabs.iter_mut().find(|t| &t.page == page) else {
                return;
            };
            let dir = tab.dir.clone();
            let Some(chat) = tab.chat.as_mut() else {
                return;
            };
            if chat.view.is_some() || chat.building {
                if let Some(view) = &chat.view {
                    let view = view.clone();
                    glib::idle_add_local_once(move || view.focus_composer());
                }
                return;
            }
            chat.building = true;
            (chat.thread.clone(), chat.driver, dir)
        };
        let (thread, driver, dir) = job;
        let profile = self
            .config
            .borrow()
            .agent_profile(driver)
            .cloned()
            .unwrap_or_else(|| crate::config::new_agent_profile(driver));
        let obj = self.obj();
        let page = page.clone();
        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            obj,
            async move {
                let resolved = resolve_agent(profile.clone()).await;
                obj.imp()
                    .build_thread(&page, &thread, driver, &dir, &profile, resolved);
            }
        ));
    }

    fn build_thread(
        &self,
        page: &adw::TabPage,
        thread: &str,
        driver: Driver,
        dir: &str,
        profile: &Profile,
        resolved: ResolvedAgent,
    ) {
        let Some((slot, holder)) = self.tabs.borrow().iter().find_map(|t| {
            (&t.page == page)
                .then(|| t.chat.as_ref().map(|c| (c.slot.clone(), c.holder.clone())))
                .flatten()
        }) else {
            return; // closed while resolving
        };
        let store = app_store();

        // The view first, so no envelope of the session's start is lost.
        let backend: Rc<dyn ChatBackend> = slot.clone();
        let view = ChatView::new(backend);
        view.set_vexpand(true);
        view.set_model_source(ModelCatalog::shared());
        view.set_account_status(AccountStatus::shared());
        let mut history = Vec::new();
        let mut after = None;
        loop {
            match store.events(thread, after, 2_000) {
                Ok(batch) if !batch.is_empty() => {
                    after = batch.last().map(|(seq, _)| *seq);
                    history.extend(batch.into_iter().map(|(_, env)| env));
                    if history.len() >= REPLAY_LIMIT {
                        break;
                    }
                }
                Ok(_) => break,
                Err(e) => {
                    warn!("Cannot read thread history: {e}");
                    break;
                }
            }
        }
        view.replay(&history);
        holder.append(&view);

        let thread_id = thread.to_owned();
        let view_sink = view.sink();
        let weak_slot = Rc::downgrade(&slot);
        let weak_obj = self.obj().downgrade();
        let sink: EnvelopeSink = Rc::new(move |env: &Envelope| {
            view_sink(env);
            let driver = weak_slot.upgrade().map_or(driver, |s| s.driver());
            if let Some(obj) = weak_obj.upgrade() {
                obj.imp().on_thread_envelope(&thread_id, driver, env);
            }
        });

        // Resume the active provider thread's native session, on its model.
        let active = store.active_provider_thread(thread).ok().flatten();
        let provider = active.and_then(|id| {
            store
                .provider_threads(thread)
                .ok()?
                .into_iter()
                .find(|p| p.id == id)
        });
        let resume = provider.as_ref().and_then(|p| p.native_id.clone());
        let model = provider
            .as_ref()
            .and_then(|p| stored_model(Some(&p.model)))
            .or_else(|| profile.default_model.clone());
        let mode = profile.default_mode.unwrap_or_default();
        let program = resolved
            .program
            .clone()
            .unwrap_or_else(|| profile.command.clone());
        let open = OpenSession {
            program: program.clone(),
            extra_args: profile.args.clone(),
            cwd: dir.to_owned(),
            model,
            effort: profile.default_effort.clone(),
            mode,
            new_session_id: (resume.is_none() && driver == Driver::Claude)
                .then(|| glib::uuid_string_random().to_string()),
            resume,
            approval_hook: false,
        };
        let adapter: std::boxed::Box<dyn Adapter> = match driver {
            Driver::Claude => std::boxed::Box::new(agent_core::claude::ClaudeAdapter::new()),
            Driver::Agy => std::boxed::Box::new(agent_core::agy::AgyAdapter::new(program)),
        };
        let approval = match driver {
            Driver::Claude => None,
            Driver::Agy => bind_approval(thread, dir, mode),
        };
        let clear = self.config.borrow().clear_env.clone();
        info!(
            "Opening thread {thread} on {} in {dir}",
            driver_label(driver)
        );
        let session = ChatSession::with_env(
            adapter,
            open,
            store,
            thread.to_owned(),
            sink,
            approval,
            LaunchEnv {
                env: resolved.env,
                unset: unset_list(&clear),
            },
        );
        let (thread_for, dir_for) = (thread.to_owned(), dir.to_owned());
        session.set_adapter_factory(Rc::new(move |d| agent_launch(d, &thread_for, &dir_for)));
        *slot.session.borrow_mut() = Some(session.clone());
        view.refresh_status();

        let weak_obj = self.obj().downgrade();
        let thread_id = thread.to_owned();
        view.connect_action(move |action| {
            if let Some(obj) = weak_obj.upgrade() {
                obj.imp().view_action(&thread_id, action);
            }
        });

        let (prompt, handoff) = {
            let mut tabs = self.tabs.borrow_mut();
            let chat = tabs
                .iter_mut()
                .find(|t| &t.page == page)
                .and_then(|t| t.chat.as_mut());
            match chat {
                Some(chat) => {
                    chat.view = Some(view.clone());
                    chat.building = false;
                    (chat.pending_prompt.take(), chat.pending_handoff.take())
                }
                None => (None, None),
            }
        };
        if let Some((summary, carried, source)) = handoff {
            session.seed_handoff(summary, carried, &source);
        }
        if let Some(prompt) = prompt {
            session.send_prompt(&prompt);
        }
        if self
            .tab_view
            .borrow()
            .as_ref()
            .and_then(|v| v.selected_page())
            .as_ref()
            == Some(page)
        {
            view.focus_composer();
        }
    }

    // -----------------------------------------------------------------------------------------
    // Live thread events
    // -----------------------------------------------------------------------------------------

    fn on_thread_envelope(&self, thread: &str, driver: Driver, env: &Envelope) {
        let in_view = self.selected_row_key() == Some(RowKey::Thread(thread.to_owned()));
        let focused = self.obj().is_active();
        enum After {
            Nothing,
            Sidebar,
            TurnDone { page: adw::TabPage, attention: bool },
            Title(String),
            RateLimited(adw::Banner, Driver),
        }
        let after = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(tab) = tabs
                .iter_mut()
                .find(|t| t.chat.as_ref().is_some_and(|c| c.thread == thread))
            else {
                return;
            };
            let page = tab.page.clone();
            let last_output = tab.last_output.clone();
            let Some(chat) = tab.chat.as_mut() else {
                return;
            };
            let switched = chat.driver != driver;
            chat.driver = driver;
            let after = match &env.event {
                Event::TurnStarted { .. } => {
                    chat.running = true;
                    chat.rate_limited = false;
                    chat.rate_banner.set_revealed(false);
                    After::Sidebar
                }
                Event::TurnCompleted { .. } => {
                    chat.running = false;
                    chat.approval = false;
                    let attention = !(in_view && focused);
                    if attention && !in_view {
                        chat.unread = true;
                    }
                    // The turn's end is "output went quiet" for the checkpoint bookkeeping.
                    last_output.set(std::time::Instant::now());
                    After::TurnDone { page, attention }
                }
                Event::SessionExited { .. } => {
                    chat.running = false;
                    chat.approval = false;
                    After::Sidebar
                }
                Event::ApprovalRequested { .. } | Event::QuestionRequested { .. } => {
                    chat.approval = true;
                    if !in_view {
                        chat.unread = true;
                    }
                    After::Sidebar
                }
                Event::ApprovalResolved { .. }
                | Event::ApprovalExpired
                | Event::QuestionResolved { .. } => {
                    chat.approval = false;
                    After::Sidebar
                }
                Event::RateLimited { .. } => {
                    chat.rate_limited = true;
                    After::RateLimited(chat.rate_banner.clone(), driver)
                }
                Event::ItemStarted {
                    kind: ItemKind::UserMessage,
                    ..
                } if chat.title.is_empty() => {
                    chat.title_item = env.item.clone();
                    After::Nothing
                }
                Event::ContentSnapshot { text, .. }
                    if chat.title.is_empty()
                        && env.item.is_some()
                        && env.item == chat.title_item =>
                {
                    let title = thread_title(text);
                    if title.is_empty() || title.starts_with('/') {
                        After::Nothing
                    } else {
                        chat.title = title.clone();
                        chat.title_item = None;
                        After::Title(title)
                    }
                }
                _ => After::Nothing,
            };
            // A switch to the other agent recolours the row.
            match after {
                After::Nothing if switched => After::Sidebar,
                other => other,
            }
        };
        match after {
            After::Nothing => {}
            After::Sidebar => self.refresh_sidebar(),
            After::Title(title) => {
                if let Err(e) = app_store().rename_thread(thread, &title) {
                    warn!("Cannot title the thread: {e}");
                }
                if let Some(page) = self.page_of_thread(thread) {
                    page.set_title(&title);
                    if in_view {
                        if let Some(w) = self.window_title.borrow().as_ref() {
                            w.set_title(&title);
                        }
                    }
                }
                self.refresh_sidebar();
            }
            After::RateLimited(banner, driver) => {
                let other = other_driver(driver);
                banner.set_title(&format!("{} hit its rate limit", driver_label(driver)));
                banner.set_button_label(Some(&format!("Continue in {}", driver_label(other))));
                banner.set_revealed(true);
                self.refresh_sidebar();
            }
            After::TurnDone { page, attention } => {
                if self.config.borrow().checkpoints {
                    self.request_checkpoint(&page, false);
                }
                if in_view {
                    let _ = app_store().mark_read(thread);
                }
                if attention {
                    self.notify_bell(&page);
                }
                // agy reports no quota per turn: ask for it now, once this turn has unwound.
                if driver == Driver::Agy {
                    if let Some(slot) = self.slot_of(thread) {
                        glib::idle_add_local_once(move || {
                            slot.control(Control::Usage);
                        });
                    }
                }
                self.refresh_sidebar();
            }
        }
    }

    fn page_of_thread(&self, thread: &str) -> Option<adw::TabPage> {
        self.tabs.borrow().iter().find_map(|t| {
            t.chat
                .as_ref()
                .is_some_and(|c| c.thread == thread)
                .then(|| t.page.clone())
        })
    }

    fn slot_of(&self, thread: &str) -> Option<Rc<SessionSlot>> {
        self.tabs.borrow().iter().find_map(|t| {
            t.chat
                .as_ref()
                .filter(|c| c.thread == thread)
                .map(|c| c.slot.clone())
        })
    }

    /// The selected page's thread session, when it is a chat page.
    pub(super) fn current_slot(&self) -> Option<Rc<SessionSlot>> {
        let page = self.tab_view.borrow().as_ref()?.selected_page()?;
        self.tabs
            .borrow()
            .iter()
            .find(|t| t.page == page)
            .and_then(|t| t.chat.as_ref().map(|c| c.slot.clone()))
    }

    /// The rate-limit banner's button: hand the thread to the other agent.
    fn continue_rate_limited(&self, thread: &str) {
        if let Some(slot) = self.slot_of(thread) {
            let target = other_driver(slot.driver());
            slot.switch(target, None, None);
        }
    }

    fn view_action(&self, thread: &str, action: &ViewAction) {
        let Some(slot) = self.slot_of(thread) else {
            return;
        };
        let dir = self.page_of_thread(thread).and_then(|p| {
            self.tabs
                .borrow()
                .iter()
                .find(|t| t.page == p)
                .map(|t| t.dir.clone())
        });
        match action {
            ViewAction::NewThread => self.new_chat_thread(Some(slot.driver()), dir, None),
            ViewAction::Handoff { target } => {
                let target = target.unwrap_or_else(|| other_driver(slot.driver()));
                slot.switch(target, None, None);
            }
            ViewAction::Fork | ViewAction::CompactByHandoff => {
                self.fork_thread(thread, slot.driver(), dir);
            }
            ViewAction::Rewind => {
                if let Some(page) = self.page_of_thread(thread) {
                    self.rewind(&page);
                }
            }
        }
    }

    /// A new thread on the same agent and folder, carrying a budgeted, redacted handoff of
    /// this one.
    fn fork_thread(&self, thread: &str, driver: Driver, dir: Option<String>) {
        let store = app_store();
        let messages = match store.transcript_messages(thread) {
            Ok(m) => m,
            Err(e) => {
                present_message(&self.obj(), "Cannot Fork the Thread", &e.to_string());
                return;
            }
        };
        let title = self
            .tabs
            .borrow()
            .iter()
            .find_map(|t| {
                t.chat
                    .as_ref()
                    .filter(|c| c.thread == thread)
                    .map(|c| c.title.clone())
            })
            .unwrap_or_default();
        let source = if title.is_empty() {
            "the previous thread".to_owned()
        } else {
            format!("“{title}”")
        };
        let (summary, carried) = build_handoff(&messages, &format!("thread {thread}"));
        self.new_chat_thread(Some(driver), dir, None);
        // The new thread is the one just selected; it is built later, so the handoff waits.
        let page = self
            .tab_view
            .borrow()
            .as_ref()
            .and_then(|v| v.selected_page());
        if let Some(page) = page {
            let built = {
                let mut tabs = self.tabs.borrow_mut();
                let chat = tabs
                    .iter_mut()
                    .find(|t| t.page == page)
                    .and_then(|t| t.chat.as_mut());
                match chat {
                    Some(chat) if chat.view.is_none() => {
                        chat.pending_handoff = Some((summary.clone(), carried, source.clone()));
                        false
                    }
                    Some(_) => true,
                    None => false,
                }
            };
            if built {
                if let Some(session) = self.current_slot().and_then(|s| s.get()) {
                    session.seed_handoff(summary, carried, &source);
                }
            }
        }
    }

    /// `/rewind`: undo the last turn's changes in the thread's folder, through the 2.x
    /// checkpoint restore (confirmed, and itself undoable).
    fn rewind(&self, page: &adw::TabPage) {
        let Some((dir, key)) = self
            .tabs
            .borrow()
            .iter()
            .find(|t| &t.page == page)
            .map(|t| (t.dir.clone(), t.key))
        else {
            return;
        };
        let obj = self.obj();
        let page = page.clone();
        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            obj,
            async move {
                let diff = gtk4::gio::spawn_blocking(move || {
                    crate::git::tab_diff(
                        std::path::Path::new(&dir),
                        key,
                        crate::diff::DiffBase::LastTurn,
                    )
                })
                .await
                .unwrap_or_else(|_| Err("the diff thread panicked".to_string()));
                match diff {
                    Ok(crate::git::DiffOutcome::Ready(d)) => match d.undo_to {
                        Some(target) => obj.imp().confirm_restore(
                            &page,
                            target,
                            "how they were before the last turn",
                        ),
                        None => obj.imp().show_toast("The last turn changed no files"),
                    },
                    Ok(crate::git::DiffOutcome::NotRepo) => obj
                        .imp()
                        .show_toast("Rewind needs a git repository: this folder is not in one"),
                    Ok(crate::git::DiffOutcome::Unavailable(why)) => obj.imp().show_toast(&why),
                    Err(e) => present_message(&obj, "Cannot Rewind", &e),
                }
            }
        ));
    }

    // -----------------------------------------------------------------------------------------
    // New threads
    // -----------------------------------------------------------------------------------------

    /// The agent new threads start on: see [`crate::config::choose_default_agent`].
    pub(super) fn default_agent(&self) -> Option<Driver> {
        let config = self.config.borrow();
        let usable = |d: Driver| {
            config.agent_profile(d).is_some_and(|p| {
                !p.disabled && crate::utils::cached_command_available(&p.command) != Some(false)
            })
        };
        let profile_driver = config.selected_profile().and_then(profile_driver);
        crate::config::choose_default_agent(config.default_agent, profile_driver, usable)
    }

    /// Creates a thread in `dir` (the starting folder when `None`) on `driver` (the default
    /// agent when `None`), opens and shows it. `prompt` is sent once it has started.
    pub(super) fn new_chat_thread(
        &self,
        driver: Option<Driver>,
        dir: Option<String>,
        prompt: Option<String>,
    ) {
        let Some(driver) = driver.or_else(|| self.default_agent()) else {
            present_message(
                &self.obj(),
                "No Chat Agent Available",
                "Neither Claude nor Antigravity (agy) is installed and enabled. Install one, \
                 or enable it in Settings → Agents. Terminal threads still work from the New \
                 Thread menu.",
            );
            return;
        };
        let home = env::var("HOME").unwrap_or_else(|_| "/".to_string());
        let requested = dir.unwrap_or_else(|| {
            self.config
                .borrow()
                .agent_profile(driver)
                .and_then(|p| p.dir.clone())
                .unwrap_or_else(|| self.config.borrow().starting_directory.clone())
        });
        let cwd = resolve_working_directory(&requested, &home);
        let store = app_store();
        let thread = match store.create_thread(&cwd, None) {
            Ok(id) => id,
            Err(e) => {
                present_message(&self.obj(), "Cannot Create a Thread", &e.to_string());
                return;
            }
        };
        // The agent is recorded now, so the sidebar and a later reopen know it before the
        // session starts. The session adopts this provider thread.
        let model = self
            .config
            .borrow()
            .agent_profile(driver)
            .and_then(|p| p.default_model.clone());
        if let Err(e) = store
            .add_provider_thread(
                &thread,
                driver_key(driver),
                model.as_deref().unwrap_or("default"),
            )
            .and_then(|pt| store.set_active_provider_thread(&thread, &pt))
        {
            warn!("Cannot record the thread's agent: {e}");
        }
        info!("New {} thread in {cwd}", driver_label(driver));
        if let Some(page) = self.open_thread(&thread, false) {
            if let Some(chat) = self
                .tabs
                .borrow_mut()
                .iter_mut()
                .find(|t| t.page == page)
                .and_then(|t| t.chat.as_mut())
            {
                chat.driver = driver;
                chat.pending_prompt = prompt;
            }
            self.open_thread(&thread, true);
        }
    }

    /// Resumes a native Claude or agy session as a chat thread: a new thread whose provider
    /// thread carries that native id.
    pub(super) fn resume_as_thread(&self, driver: Driver, native_id: &str, dir: Option<String>) {
        let home = env::var("HOME").unwrap_or_else(|_| "/".to_string());
        let cwd = resolve_working_directory(
            &dir.unwrap_or_else(|| self.config.borrow().starting_directory.clone()),
            &home,
        );
        let store = app_store();
        let made = store.create_thread(&cwd, None).and_then(|thread| {
            let pt = store.add_provider_thread(&thread, driver_key(driver), "default")?;
            store.set_native_id(&pt, native_id)?;
            store.set_active_provider_thread(&thread, &pt)?;
            Ok(thread)
        });
        match made {
            Ok(thread) => {
                info!(
                    "Resuming {} session {native_id} as a thread",
                    driver_label(driver)
                );
                self.open_thread(&thread, true);
            }
            Err(e) => present_message(&self.obj(), "Cannot Resume the Session", &e.to_string()),
        }
    }

    /// Routes a resume to a chat thread or, for a CLI with no adapter, a terminal page.
    pub(super) fn resume_mapped(
        &self,
        profile: &Profile,
        session_id: &str,
        dir: Option<String>,
    ) -> bool {
        match resume_as(profile_driver(profile), session_id) {
            ResumeAs::Chat { driver, native_id } => {
                self.resume_as_thread(driver, &native_id, dir);
                true
            }
            ResumeAs::Terminal => false,
        }
    }

    // -----------------------------------------------------------------------------------------
    // The terminal drawer
    // -----------------------------------------------------------------------------------------

    /// Ctrl+`: shows or hides the current thread's shell, starting it the first time.
    pub(super) fn toggle_drawer(&self) {
        let Some(page) = self
            .tab_view
            .borrow()
            .as_ref()
            .and_then(|v| v.selected_page())
        else {
            return;
        };
        let state = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(tab) = tabs.iter_mut().find(|t| t.page == page) else {
                return;
            };
            let dir = tab.dir.clone();
            let terminal = tab.terminal.clone();
            let stack = tab.stack.clone();
            let Some(chat) = tab.chat.as_mut() else {
                return; // a terminal page is all terminal already
            };
            let open = !chat.drawer.is_visible();
            let start = open && !chat.drawer_started;
            if start {
                chat.drawer_started = true;
            }
            (
                open,
                start,
                chat.drawer.clone(),
                chat.drawer_paned.clone(),
                terminal,
                stack,
                dir,
                chat.view.clone(),
            )
        };
        let (open, start, drawer, paned, terminal, stack, dir, view) = state;
        drawer.set_visible(open);
        if open {
            // Two thirds chat, one third shell, on first show.
            let height = paned.height();
            if height > 0 {
                paned.set_position(height * 2 / 3);
            }
            if start {
                self.spawn_session(&terminal, &stack, None, &dir, Launch::Fresh);
            }
            terminal.grab_focus();
        } else if let Some(view) = view {
            view.focus_composer();
        }
    }

    fn drawer_exited(&self, page: &adw::TabPage) {
        let view = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(chat) = tabs
                .iter_mut()
                .find(|t| &t.page == page)
                .and_then(|t| t.chat.as_mut())
            else {
                return;
            };
            chat.drawer_started = false;
            chat.drawer.set_visible(false);
            chat.view.clone()
        };
        if let Some(view) = view {
            view.focus_composer();
        }
    }

    // -----------------------------------------------------------------------------------------
    // Catalogue and account status
    // -----------------------------------------------------------------------------------------

    /// Resolves both chat agents and refreshes the model catalogue and the account status with
    /// their binaries; again every ten minutes while the window is focused.
    fn watch_account_status(&self) {
        self.refresh_agent_data();
        let obj = self.obj();
        glib::timeout_add_local(
            ACCOUNT_REFRESH,
            glib::clone!(
                #[weak]
                obj,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    if obj.is_active() {
                        obj.imp().refresh_agent_data();
                    }
                    glib::ControlFlow::Continue
                }
            ),
        );
    }

    pub(super) fn refresh_agent_data(&self) {
        let profiles: Vec<Profile> = [Driver::Claude, Driver::Agy]
            .into_iter()
            .map(|d| {
                self.config
                    .borrow()
                    .agent_profile(d)
                    .cloned()
                    .unwrap_or_else(|| crate::config::new_agent_profile(d))
            })
            .collect();
        glib::MainContext::default().spawn_local(async move {
            let mut programs = Vec::new();
            for profile in profiles {
                let resolved = resolve_agent(profile.clone()).await;
                programs.push(resolved.program.unwrap_or(profile.command));
            }
            if let [claude, agy] = programs.as_slice() {
                ModelCatalog::shared().refresh(claude, agy);
                AccountStatus::shared().refresh(claude, agy);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_list_always_drops_an_inherited_approval_socket() {
        let names = unset_list(&[]);
        assert!(names.iter().any(|n| n == agent_core::approval::ENV_SOCKET));
        assert!(names
            .iter()
            .any(|n| n == crate::approval_server::ENV_HOOK_BIN));
    }

    #[test]
    fn an_unstarted_slot_reports_its_thread_agent() {
        let slot = SessionSlot {
            session: RefCell::new(None),
            driver: Driver::Agy,
            model: Some("gemini".into()),
            mode: Mode::Plan,
        };
        let status = slot.status();
        assert_eq!(status.driver, Driver::Agy);
        assert!(!status.alive);
        assert_eq!(slot.control(Control::Usage), "ctl-unstarted");
    }
}
