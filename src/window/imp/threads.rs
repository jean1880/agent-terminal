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

mod delegation;

use agent_core::adapter::{Adapter, Control, Driver, Mode, OpenSession};
use agent_core::catalog::{model_notice, ModelNotice};
use agent_core::event::{BackgroundTask, Decision, Envelope, Event, ItemKind};
use agent_kit::store::{pending_prompt_key, Store};
use agent_kit::store_writer::{Shutdown, StoreHandle, StoreWriter, WORKER_FLUSH_WAIT};

use super::diffs::ThreadDiffs;
use super::*;
use crate::account_status::AccountStatus;
use crate::agent_proc::AgentEnv;
use crate::approval_server::ApprovalHandle;
use crate::availability::{
    classify, probe_targets, unavailable_banner, AgentAvailability, Availability,
};
use crate::chat::session::{
    build_handoff, retired_notice, AgentLaunch, Approval, ChatSession, LaunchEnv, RetiredModel,
};
use crate::chat::view::usage::UsageIndicator;
use crate::chat::view::{ChatView, ViewAction};
use crate::chat::{ChatBackend, EnvelopeSink, SessionStatus};
use crate::config::{profile_driver, Profile};
use crate::model_catalog::{retirement_banner, ModelCatalog};
use crate::window::sidebar_model::{
    badge_for, busy_reason, continue_label, driver_key, driver_label, fold_groups,
    forbids_unasked_edits, held_finish_due, is_busy, limit_over, needs_attention, parse_driver,
    react, redraw_for, relative_time, resume_as, row_tooltip, safe_handoff_target,
    sidebar_sections, stop_heading, stored_model, thread_title, turn_finishes_thread, Attention,
    Badge, ChildAsks, FolderGroup, OpenState, Redraw, ResumeAs, RowKey, SidebarModel, SidebarRow,
};
use crate::window::sidebar_model::{
    reopen_offer, sessions_added_toast, thread_activity, ThreadActivity,
};

/// Removes every row of `list`, and only rows: an open thread menu is also the list's child, and
/// `remove` refuses it, so walking `first_child` would never end.
fn clear_rows(list: &gtk4::ListBox) {
    while let Some(row) = list.row_at_index(0) {
        list.remove(&row);
    }
}

/// The store key holding each window's layout and the legacy merged open-thread list.
const OPEN_THREADS_KEY: &str = "open_threads";

/// A successful load still waits for the scan's answer; a failed load drops a parked switch.
fn keep_pending_switch(read_ok: bool, own: &Availability) -> bool {
    read_ok && matches!(own, Availability::Detecting)
}

/// How an in-place agent switch reaches a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SwitchRoute {
    /// Its session switches itself (a stopped or suspended one too: it restarts on the new one).
    Live,
    /// Kept in `pending_switch` until the session starting on the thread's own agent, or the
    /// build in flight, settles; applied then.
    Park,
    /// No session will start on the thread's own agent: the thread moves to the new one now.
    Rebind,
}

/// [`SwitchRoute`] for a thread with or without a session, its page `building` or not, and its
/// own agent `own`. A build in flight read the thread for its old agent, so a rebind waits for
/// it (the build applies the parked switch when it finds the agent unavailable).
pub(super) fn switch_route(has_session: bool, building: bool, own: &Availability) -> SwitchRoute {
    match (has_session, own) {
        (true, _) => SwitchRoute::Live,
        (false, Availability::Detecting) => SwitchRoute::Park,
        (false, _) if building => SwitchRoute::Park,
        (false, _) => SwitchRoute::Rebind,
    }
}

/// Moves `thread`, which no session runs, from `from` to `target` in the store, in one
/// transaction: a new provider thread for `target` with no native session and `model` (else the
/// agent's default), made active, then the thread's budgeted, redacted handoff
/// ([`build_handoff`]) and its notice ([`crate::chat::session::switched_handoff_events`]), for
/// the new agent's session to re-arm. Another agent's native id or model is never carried.
/// Returns the handoff (carrying nothing for a thread with no messages).
fn rebind_thread(
    store: &Store,
    thread: &str,
    from: Driver,
    target: Driver,
    model: Option<&str>,
) -> agent_kit::store::Result<crate::chat::session::Handoff> {
    let messages = store.transcript_messages(thread)?;
    let handoff = build_handoff(
        &messages,
        &format!("{} thread {thread}", driver_label(from)),
    );
    store.atomically(|s| {
        let pt = s.add_provider_thread(thread, driver_key(target), model.unwrap_or("default"))?;
        s.set_active_provider_thread(thread, &pt)?;
        s.set_thread_profile(thread, None)?;
        for env in crate::chat::session::switched_handoff_events(&handoff, target) {
            s.append_event(thread, Some(&pt), &env)?;
        }
        Ok(())
    })?;
    Ok(handoff)
}

fn scan_is_current(generation: u64) -> bool {
    SCAN_GENERATION.with(|current| generation == current.get())
}

#[cfg(test)]
pub(super) fn scan_generation_for_test() -> u64 {
    SCAN_GENERATION.with(std::cell::Cell::get)
}

/// Resolve weak owners and discard expired entries rather than retaining a dead window.
fn registry_owner<W, O>(
    registry: &mut HashMap<String, W>,
    thread: &str,
    upgrade: impl FnOnce(&W) -> Option<O>,
) -> Option<O> {
    let owner = registry.get(thread).and_then(upgrade);
    if owner.is_none() {
        registry.remove(thread);
    }
    owner
}

pub(super) fn thread_owner(thread: &str) -> Option<super::super::AgentTerminalWindow> {
    OPEN_THREADS.with(|registry| {
        registry_owner(&mut registry.borrow_mut(), thread, |weak| {
            weak.upgrade()
                .filter(|window| window.imp().page_of_thread(thread).is_some())
        })
    })
}

fn save_pending_prompt(store: &Store, thread: &str, prompt: &str) -> agent_kit::store::Result<()> {
    store.set_pending_prompt(thread, prompt)
}

#[derive(Default)]
struct LatestLayout {
    pending: Option<String>,
    queued: bool,
}

impl LatestLayout {
    fn offer(&mut self, value: String) -> bool {
        self.pending = Some(value);
        !std::mem::replace(&mut self.queued, true)
    }

    fn take(&mut self) -> Option<String> {
        self.queued = false;
        self.pending.take()
    }
}

type LayoutWrites = std::sync::Arc<std::sync::Mutex<LatestLayout>>;

struct LayoutTicket {
    writes: LayoutWrites,
    captured: bool,
}

impl Drop for LayoutTicket {
    fn drop(&mut self) {
        if !self.captured {
            // A rejected enqueue or failed batch can discard the placeholder before it runs.
            self.writes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .queued = false;
        }
    }
}

fn write_latest_layout(writes: &LayoutWrites, store: &Store) -> agent_kit::store::Result<()> {
    let value = writes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(value) = value {
        store.set_meta(OPEN_THREADS_KEY, &value)?;
    }
    Ok(())
}

/// The queued placeholder captures the newest value on the writer, so a close barrier always
/// includes it. No main-loop completion callback or blocking waiter submits a later snapshot.
fn queue_open_layout(
    writer: &StoreWriter,
    writes: &LayoutWrites,
    value: String,
) -> agent_kit::store::Result<()> {
    if !writes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .offer(value)
    {
        return Ok(());
    }
    let mut ticket = LayoutTicket {
        writes: writes.clone(),
        captured: false,
    };
    writer.write("record the open threads", None, move |store| {
        ticket.captured = true;
        write_latest_layout(&ticket.writes, store)
    })
}

fn window_layout(
    closing: bool,
    any_live_window: bool,
    open: Vec<String>,
    selected: Option<String>,
) -> Option<serde_json::Value> {
    // An individual close leaves only surviving windows in the reopen list. At quit, retain
    // every remaining window's layout while its pages still exist, even though all are closing.
    // This includes a previously closed window still waiting for its agents when the last closes.
    (!closing || !any_live_window).then(|| serde_json::json!({"open": open, "selected": selected}))
}

/// Shows only the selected page's content. `AdwTabView` merely unmaps an unselected page, and
/// GTK still restyles unmapped widgets on every window-wide change (focus in and out, a monitor
/// or scale change while resizing); invisible ones it skips. Profiled on a resize: 74% of the
/// main thread restyling every open thread's widgets. Measured on focus changes with 8 threads:
/// 35 ms a frame, 16 ms with the hidden ones invisible (`idle_cost_of_open_threads`).
///
/// An invisible page also measures nothing, so the window's minimum size is the shown page's,
/// not the largest of every open page's.
fn show_only_selected(view: &adw::TabView) {
    let selected = view.selected_page();
    for i in 0..view.n_pages() {
        let page = view.nth_page(i);
        page.child().set_visible(selected.as_ref() == Some(&page));
    }
}

/// Each page's focused widget when it was last left, for [`wire_page_visibility`].
type LastFocus = Rc<RefCell<Vec<(glib::WeakRef<adw::TabPage>, glib::WeakRef<gtk4::Widget>)>>>;

/// Whether `widget` is `page`'s content or inside it.
fn in_page(widget: &gtk4::Widget, page: &adw::TabPage) -> bool {
    let child = page.child();
    widget == &child || widget.is_ancestor(&child)
}

/// Keeps only the selected page visible ([`show_only_selected`]): on selection, and on a page
/// attached (new, or dragged in from another window, hidden there as an unselected page).
///
/// It also does what `AdwTabView` does for focus, which a hidden page defeats. `AdwTabView`
/// moves focus into a newly selected page before it notifies the selection, while that page is
/// still invisible, so the move fails; hiding the page left then drops its focus, and GTK moves
/// it to the window's first focusable widget (a header button, the sidebar). So when focus was
/// in the page left, it is remembered for that page, and the page shown gets back the widget it
/// last had (or its first focusable one) once laid out. A page that refocuses itself on show (a
/// thread's composer) is left alone, as is focus outside the pages (the sidebar a page was
/// picked from).
fn wire_page_visibility(tab_view: &adw::TabView) {
    tab_view.connect_page_attached(|view, _, _| show_only_selected(view));
    let memory = LastFocus::default();
    tab_view.connect_selected_page_notify(move |view| {
        let selected = view.selected_page();
        let before = view.root().and_then(|root| root.focus());
        let left = before.as_ref().and_then(|focus| {
            (0..view.n_pages())
                .map(|i| view.nth_page(i))
                .find(|page| Some(page) != selected.as_ref() && in_page(focus, page))
        });
        {
            let mut memory = memory.borrow_mut();
            memory.retain(|(page, _)| page.upgrade().is_some());
            if let (Some(page), Some(focus)) = (&left, &before) {
                memory.retain(|(p, _)| p.upgrade().as_ref() != Some(page));
                memory.push((page.downgrade(), focus.downgrade()));
            }
        }
        show_only_selected(view);
        let (Some(page), Some(_)) = (selected, left) else {
            return;
        };
        let target = memory
            .borrow()
            .iter()
            .find(|(p, _)| p.upgrade().as_ref() == Some(&page))
            .and_then(|(_, w)| w.upgrade())
            .filter(|w| in_page(w, &page));
        let page = page.downgrade();
        // After the page's first layout: the focus chain skips widgets with no allocation.
        glib::idle_add_local_once(move || {
            let Some(page) = page.upgrade() else {
                return;
            };
            let child = page.child();
            if !child.is_mapped() {
                return;
            }
            let focus = child.root().and_then(|root| root.focus());
            if focus.is_some_and(|f| in_page(&f, &page)) {
                return;
            }
            if !target.is_some_and(|w| w.grab_focus()) {
                child.child_focus(gtk4::DirectionType::TabForward);
            }
        });
    });
}

/// Both shapes are written so an older build can still reopen every thread.
fn saved_open_threads(
    windows: &[serde_json::Value],
    selected: Option<String>,
) -> serde_json::Value {
    let mut open = Vec::new();
    for window in windows {
        if let Some(ids) = window["open"].as_array() {
            for id in ids {
                if id.is_string() && !open.contains(id) {
                    open.push(id.clone());
                }
            }
        }
    }
    let selected = selected.filter(|id| open.iter().any(|v| v.as_str() == Some(id)));
    serde_json::json!({"windows": windows, "open": open, "selected": selected})
}
/// Most events replayed into a thread's view when it opens.
const REPLAY_LIMIT: usize = 50_000;
/// How often the account/usage indicator refreshes while the window is focused.
const ACCOUNT_REFRESH: std::time::Duration = std::time::Duration::from_secs(600);
/// At or below this window width the sidebar is overlaid on the thread rather than beside it.
/// Drive it from the allocated window width: `AdwApplicationWindow` breakpoints do not reliably
/// re-evaluate after a live Broadway/native resize, which can otherwise leave both panes side by
/// side and push the thread out of view.
pub(super) const SIDEBAR_COLLAPSE_SP: f64 = 720.0;

/// The "stop and switch?" question: its heading and body, for a thread titled `title` moving to
/// `driver` while work would be cut short.
pub(super) fn switch_warning(title: &str, driver: Driver) -> (String, String) {
    let agent = driver_label(driver);
    let subject = if title.is_empty() {
        "This thread".to_owned()
    } else {
        format!("“{title}”")
    };
    (
        format!("Stop the Running Work and Switch to {agent}?"),
        format!(
            "{subject} is working, waiting for you, or has sub-agents running. Switching to \
             {agent} stops the agent and every sub-agent of this thread before they finish; \
             other threads are not touched. Edits already made stay on disk."
        ),
    )
}

/// What closing a thread page would lose (the row's X, archive, the window): its running work
/// ([`ChatTab::busy`]), its unsent draft, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CloseReasons {
    pub(super) busy: bool,
    pub(super) draft: bool,
}

/// Why closing a thread must ask first; `None` when nothing would be lost. A draft is its own
/// reason, beside `busy`, which stays about work alone (idle suspension reads it).
pub(super) fn close_reasons(busy: bool, draft: bool) -> Option<CloseReasons> {
    (busy || draft).then_some(CloseReasons { busy, draft })
}

/// `“title”`, or "This thread" for an untitled one.
fn thread_subject(title: &str) -> String {
    if title.is_empty() {
        "This thread".to_owned()
    } else {
        format!("“{title}”")
    }
}

/// The question before closing (or archiving) one thread: its heading, body and the label of the
/// destructive answer.
pub(super) fn close_question(title: &str, reasons: CloseReasons) -> (String, String, &'static str) {
    let subject = thread_subject(title);
    match (reasons.busy, reasons.draft) {
        (true, true) => (
            "Stop the Running Turn and Close?".to_owned(),
            format!(
                "{subject} is working or waiting for you, and has an unsent message. Closing it \
                 stops the agent, and any sub-agents, before it finishes, and discards the \
                 message. Edits already made stay on disk."
            ),
            "Stop and Close",
        ),
        (true, false) => (
            "Stop the Running Turn and Close?".to_owned(),
            format!(
                "{subject} is working or waiting for you. Closing it stops the agent, and any \
                 sub-agents, before it finishes. Edits already made stay on disk."
            ),
            "Stop and Close",
        ),
        (false, _) => (
            "Discard the Unsent Message?".to_owned(),
            format!("{subject} has an unsent message. Closing the thread discards it."),
            "Discard and Close",
        ),
    }
}

/// The window-close question for the threads that would lose something, as (title, reasons):
/// its heading, body and the label of the destructive answer. `None` when the list is empty.
pub(super) fn window_close_question(
    threads: &[(String, CloseReasons)],
) -> Option<(String, String, &'static str)> {
    if threads.is_empty() {
        return None;
    }
    let busy = threads.iter().filter(|(_, r)| r.busy).count();
    let lines = threads
        .iter()
        .map(|(title, r)| {
            let title = if title.is_empty() {
                "New thread"
            } else {
                title
            };
            if r.draft {
                format!("• {title} has an unsent message")
            } else {
                format!("• {title}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some(if busy > 0 {
        (
            stop_heading(busy),
            format!(
                "Closing stops the agent, and any sub-agents, before they finish, and discards \
                 unsent messages:\n\n{lines}\n\nEdits already made stay on disk."
            ),
            "Stop and Close",
        )
    } else {
        (
            if threads.len() == 1 {
                "Discard the Unsent Message?".to_owned()
            } else {
                "Discard the Unsent Messages?".to_owned()
            },
            format!("Closing discards what has not been sent:\n\n{lines}"),
            "Discard and Close",
        )
    })
}

/// What a discarded blank thread is recreated from (its toast's Undo), fixed when it was
/// discarded: its agent, folder, profile, worktree, model, effort and mode (never dangerous
/// permissive mode, which ends with its agent).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct DiscardedThread {
    driver: Driver,
    dir: String,
    profile_id: Option<String>,
    worktree: Option<crate::worktree::WorktreeInfo>,
    model: Option<String>,
    effort: Option<String>,
    mode: Option<Mode>,
}

impl DiscardedThread {
    /// The thread to create again: the same folder (refused if it is gone, never replaced) and
    /// the same choices, through the ordinary creation path.
    pub(super) fn new_thread(&self) -> NewThread {
        NewThread {
            dir: Some(self.dir.clone()),
            model: self.model.clone(),
            effort: self.effort.clone(),
            mode: self.mode.map(Mode::session_safe),
            profile_id: self.profile_id.clone(),
            worktree: self.worktree.clone(),
            ..NewThread::new(self.driver)
        }
    }
}

/// What a blank thread was stored with, read in the job that deletes it.
#[derive(Debug, Clone, Default)]
pub(super) struct StoredChoices {
    pub(super) model: Option<String>,
    pub(super) attributes: agent_kit::store::ThreadAttributes,
    pub(super) mode: Option<Mode>,
}

impl DiscardedThread {
    /// Fills in from the store what the page could not say. `live`: the page had a session, so
    /// its model is the session's (`None` meaning the agent's own) and is kept. Without one the
    /// model is the stored one, and a profile, worktree or mode the page does not know yet (it
    /// closed before it was built) is the stored one too.
    pub(super) fn with_stored(mut self, stored: StoredChoices, live: bool) -> Self {
        if live {
            return self;
        }
        self.model = stored_model(stored.model.as_deref());
        self.profile_id = self.profile_id.or(stored.attributes.profile_id);
        self.worktree = self.worktree.or(stored.attributes.worktree);
        self.mode = self.mode.or(stored.mode).map(Mode::session_safe);
        self
    }
}

/// The [`DiscardedThread`] a blank thread page leaves: what its session runs with, else what
/// the page knows; `true` with it when it had a session. What the page cannot say is read from
/// the store as the thread is deleted ([`DiscardedThread::with_stored`]).
fn discarded_thread(tab: &TabState, chat: &ChatTab) -> (DiscardedThread, bool) {
    let session = chat.slot.get();
    let spec = DiscardedThread {
        driver: chat.driver,
        dir: tab.dir.clone(),
        profile_id: chat.profile_id.clone(),
        worktree: tab.worktree.clone(),
        model: session.as_ref().and_then(|s| s.selected_model()),
        effort: session
            .as_ref()
            .map_or_else(|| chat.pending_effort.clone(), |s| s.selected_effort()),
        mode: session
            .as_ref()
            .map(|s| s.wanted_mode())
            .or(chat.chosen_mode)
            .map(Mode::session_safe),
    };
    (spec, session.is_some())
}

/// What the user is told once a store write behind a thread action has answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WriteNotice {
    /// A toast; `undo` offers the action's Undo.
    Toast { text: String, undo: bool },
    /// A dialog: the write did not land.
    Failure { heading: &'static str, body: String },
}

/// The error text of a store write that did not answer in time.
const STORE_LATE: &str = "the thread store did not answer in time (it may still be saved)";

/// The notice for an archive (`archived`) or its undo, once written. `stopped`: archiving closed
/// the thread mid-turn, which Undo cannot resume, so the toast says so (and Undo only brings the
/// thread back into the list).
pub(super) fn archive_notice(
    archived: bool,
    stopped: bool,
    outcome: Option<Result<(), String>>,
) -> WriteNotice {
    match (outcome, archived) {
        (Some(Ok(())), true) => WriteNotice::Toast {
            text: if stopped {
                "Archived; its running turn was stopped".to_owned()
            } else {
                "Thread archived".to_owned()
            },
            undo: true,
        },
        (Some(Ok(())), false) => WriteNotice::Toast {
            text: "Thread restored".to_owned(),
            undo: false,
        },
        (failed, archived) => {
            let error = failed
                .and_then(Result::err)
                .unwrap_or_else(|| STORE_LATE.to_owned());
            WriteNotice::Failure {
                heading: if archived {
                    "Could Not Archive the Thread"
                } else {
                    "Could Not Restore the Thread"
                },
                body: match (archived, stopped) {
                    (true, true) => format!(
                        "The agent was stopped, but the thread could not be archived: {error}"
                    ),
                    (true, false) => format!("The thread could not be archived: {error}"),
                    (false, _) => format!("The thread could not be restored: {error}"),
                },
            }
        }
    }
}

/// The notice for a delete, once written. `stopped`: the thread's page was open, so its agent
/// was stopped before the write, whatever became of it.
pub(super) fn delete_notice(stopped: bool, outcome: Option<Result<(), String>>) -> WriteNotice {
    match outcome {
        Some(Ok(())) => WriteNotice::Toast {
            text: "Thread deleted".to_owned(),
            undo: false,
        },
        failed => {
            let error = failed
                .and_then(Result::err)
                .unwrap_or_else(|| STORE_LATE.to_owned());
            WriteNotice::Failure {
                heading: "Could Not Delete the Thread",
                body: if stopped {
                    format!("The agent was stopped, but the thread could not be deleted: {error}")
                } else {
                    format!("The thread could not be deleted: {error}")
                },
            }
        }
    }
}

/// The desktop notification id for a thread waiting on an approval or a question.
fn approval_notification_id(thread: &str) -> String {
    format!("approval-{thread}")
}

/// The desktop notification id for a thread whose turn finished (one per thread, so two
/// threads finishing never collapse into a notification that opens only the last).
fn turn_notification_id(thread: &str) -> String {
    format!("turn-{thread}")
}

/// Whether the idle sweep stops a thread's agent now: suspension on (`minutes` > 0), the
/// thread out of sight, nothing holding it ([`ChatTab::holds_suspension`]), and unseen and
/// unheard from for at least `minutes`. The session still has the last word
/// ([`ChatSession::suspend_if_idle`]).
fn suspension_due(minutes: u32, idle: std::time::Duration, shown: bool, held: bool) -> bool {
    minutes > 0
        && !shown
        && !held
        && idle >= std::time::Duration::from_secs(u64::from(minutes) * 60)
}

impl ChatTab {
    /// Whether closing this thread would cut work short. The one place the close question
    /// (the row's X, archive, the window) decides; extend it for background work.
    pub(super) fn busy(&self) -> bool {
        is_busy(
            self.running,
            self.background_work(false),
            self.waiting_on_user(),
        ) || crate::delegation::supervisor().is_some_and(|s| s.root_busy(&self.thread))
    }

    /// Whether anything this thread's agent started still runs behind its turn: background
    /// tasks, provider sub-agents (Codex workers) or, as `delegated` says, delegated workers
    /// ([`Supervisor::root_busy`]).
    pub(super) fn background_work(&self, delegated: bool) -> bool {
        !self.background.is_empty() || self.workers_active > 0 || delegated
    }

    /// Whether the thread waits for the user: its own approval or question, or one a delegated
    /// sub-agent raised.
    pub(super) fn waiting_on_user(&self) -> bool {
        self.approval || self.child_asks.any()
    }

    /// What closing this thread's page would lose: its work ([`Self::busy`]) or its unsent
    /// draft. `None`: nothing, so no question is asked.
    pub(super) fn closing_loses(&self) -> Option<CloseReasons> {
        close_reasons(
            self.busy(),
            self.view.as_ref().is_some_and(ChatView::has_draft),
        )
    }
}

/// One chat thread page's state, beside its [`TabState`].
pub(super) struct ChatTab {
    pub(super) thread: String,
    /// The agent when the page was opened, until the session reports its own.
    pub(super) driver: Driver,
    pub(super) title: String,
    /// Holds the chat view once built (the page is built the first time it is shown).
    holder: gtk4::Box,
    pub(super) slot: Rc<SessionSlot>,
    pub(super) view: Option<ChatView>,
    building: bool,
    /// The drawer: the paned's end child, hidden until toggled.
    drawer: gtk4::Box,
    drawer_paned: gtk4::Paned,
    drawer_started: bool,
    rate_banner: adw::Banner,
    /// Shown when the thread's model is no longer offered by its agent.
    model_banner: adw::Banner,
    /// Shown while the thread's agent is installed but no one is signed in to it.
    sign_in_banner: adw::Banner,
    running: bool,
    /// Background work still going (sub-agents, background commands), as the agent last listed
    /// it. The thread is finished only when this is empty and no turn runs.
    background: Vec<BackgroundTask>,
    /// The last turn ended while background work still ran, so its finish (unread mark,
    /// notification) waits for that work: released by the next turn's end or, with no turn
    /// following, once the list empties (`release_held_finish`).
    finish_held: bool,
    pub(super) approval: bool,
    /// Provider sub-agents (Codex workers) the agent's registry lists as still active.
    workers_active: usize,
    /// What delegated sub-agents wait on the user for, shown on this thread as its own.
    child_asks: ChildAsks,
    rate_limited: bool,
    pub(super) unread: bool,
    /// A "stop and close?" question is already on screen for this thread.
    confirming_close: bool,
    /// A "stop and switch?" question is already on screen for this thread.
    confirming_switch: bool,
    /// The user message item awaiting its text, for the title of an untitled thread.
    title_item: Option<String>,
    /// Sent once the session exists (Continue In from a terminal page); also saved as redacted
    /// unsent metadata so closing during a build does not lose it.
    pending_prompt: Option<String>,
    /// The thread was created carrying a handoff (fork, compact-by-handoff) its session has not
    /// started with yet. The handoff itself is stored with the thread and re-armed from history
    /// (`stored_handoff`); this only keeps an unstarted fork from looking blank.
    pending_handoff: bool,
    /// The profile the thread was opened with (its id), from its page's creation or its store
    /// read; honoured while that profile runs the thread's agent
    /// ([`crate::config::TerminalConfig::thread_profile`]).
    pub(super) profile_id: Option<String>,
    /// A handoff found stored and undelivered when the thread was built: re-armed (not seeded
    /// again) in whichever session starts first.
    stored_handoff: Option<crate::chat::session::Handoff>,
    /// The mode the user last chose in this thread, from its stored history: what its session
    /// starts from, before the agent's profile default.
    chosen_mode: Option<Mode>,
    /// The reasoning effort the session starts with, overriding the profile's default (a thread
    /// continued in another agent at a chosen effort).
    pending_effort: Option<String>,
    /// A switch (agent, model, effort) to apply once the session exists (the thread menu's
    /// "Switch this thread to" on a thread that is not built yet).
    pub(super) pending_switch: Option<(Driver, Option<String>, Option<String>)>,
    /// An in-place switch of a thread no session runs is being written (its new provider
    /// thread and handoff): no build starts, and no other switch is taken, until it lands.
    rebinding: bool,
    /// The banner is showing because the thread's agent is missing or off (not a rate limit).
    unavailable: bool,
    /// A session start is in flight for a view that was built without one (its agent was not
    /// Ready); stops a second availability change from starting it twice.
    starting: bool,
    /// The thread's pre-turn baselines and the source of its file-change cards' diffs.
    pub(super) diffs: Rc<ThreadDiffs>,
    /// When the thread was last shown, left or heard from: idle suspension counts from here.
    /// A cell, so the selection handlers stamp it under a shared borrow of the registry: a
    /// mutable one there can meet a borrow held up the stack, and panicking in a signal
    /// handler aborts the app.
    last_seen: std::cell::Cell<std::time::Instant>,
}

impl ChatTab {
    /// Whether stopping the agent to free its memory must wait: [`Self::busy`] (a turn,
    /// background work, a question, delegated workers), or anything about to happen (a build or
    /// start, a close or switch question on screen, something waiting to be sent or applied).
    fn holds_suspension(&self) -> bool {
        self.busy()
            || self.finish_held
            || self.building
            || self.starting
            || self.confirming_close
            || self.confirming_switch
            || self.pending_prompt.is_some()
            || self.pending_handoff
            || self.pending_switch.is_some()
            || self.rebinding
    }

    /// Where this thread's agent process is, for its sidebar row ([`thread_activity`]): the page
    /// being built or its session started count as starting.
    fn activity(&self) -> Option<ThreadActivity> {
        let life = self.slot.get().map(|s| s.lifecycle()).unwrap_or_default();
        thread_activity(
            self.building || self.starting,
            life.alive,
            life.starting,
            life.suspended,
        )
    }

    pub(super) fn reset_transfer_preparation(&mut self) {
        if self.view.is_none() {
            self.building = false;
        }
        if self.slot.get().is_none() {
            self.starting = false;
        }
    }

    /// Nothing on the page the user would miss: no title, nothing waiting to be sent or applied,
    /// no draft. The store has the last word ([`agent_kit::store::Store::delete_if_blank`]).
    pub(super) fn looks_blank(&self) -> bool {
        self.title.is_empty()
            && self.title_item.is_none()
            && self.pending_prompt.is_none()
            && !self.pending_handoff
            && self.pending_switch.is_none()
            && !self.rebinding
            && !self.view.as_ref().is_some_and(ChatView::has_draft)
    }
}

// ---------------------------------------------------------------------------------------------
// The backend the view is built with, before its session exists
// ---------------------------------------------------------------------------------------------

/// The view needs a backend at construction and the session needs the view's sink, so the view
/// gets this slot, which forwards to the session once it is set.
pub(super) struct SessionSlot {
    thread: Option<String>,
    switch_serial: Rc<std::cell::Cell<u64>>,
    start_attempt: std::cell::Cell<u64>,
    worker_subscriber: RefCell<Option<Rc<dyn crate::delegation::seam::WorkerSubscriber>>>,
    worker_diffs: RefCell<HashMap<agent_core::delegation::scope::Scope, Rc<ThreadDiffs>>>,
    session: RefCell<Option<Rc<ChatSession>>>,
    /// The view, to tell the user when a prompt cannot go anywhere yet (the thread's agent is
    /// not Ready, so no session exists). Weak: the view owns this slot.
    view: RefCell<Option<glib::WeakRef<ChatView>>>,
    /// The thread's agent while no session runs it: moved by an in-place switch made before
    /// any session started ([`AgentTerminalWindow::rebind_unstarted`]).
    driver: std::cell::Cell<Driver>,
    model: Option<String>,
    mode: Mode,
}

impl SessionSlot {
    pub(super) fn invalidate_pending_switch(&self) {
        self.switch_serial
            .set(self.switch_serial.get().wrapping_add(1));
    }
    pub(super) fn get(&self) -> Option<Rc<ChatSession>> {
        self.session.borrow().clone()
    }

    pub(super) fn driver(&self) -> Driver {
        match self.get() {
            Some(s) => s.status().driver,
            None => self.driver.get(),
        }
    }
}

impl ChatBackend for SessionSlot {
    fn worker_scope(
        &self,
        worker: &agent_core::delegation::WorkerId,
    ) -> Option<agent_core::delegation::scope::Scope> {
        crate::delegation::supervisor()?.worker_scope(self.thread.as_deref()?, worker)
    }
    fn worker_action(&self, action: crate::chat::WorkerAction) -> bool {
        delegation::worker_action(self, action)
    }
    fn worker_diff_source(
        &self,
        scope: &agent_core::delegation::scope::Scope,
    ) -> Option<Rc<dyn crate::chat::DiffSource>> {
        self.worker_diffs
            .borrow()
            .get(scope)
            .cloned()
            .map(|source| source as Rc<dyn crate::chat::DiffSource>)
    }
    fn send_prompt(&self, text: &str) {
        match self.get() {
            Some(s) => {
                let deferred = self.thread.as_deref().is_some_and(|thread| {
                    crate::delegation::supervisor().is_some_and(|supervisor| {
                        matches!(
                            supervisor.before_root_turn(thread),
                            crate::delegation::supervisor::RootTurnGate::Deferred
                        )
                    })
                });
                if deferred {
                    s.queue_prompt(text);
                } else {
                    s.send_prompt(text);
                }
            }
            None => {
                warn!("a prompt arrived before the thread's session started");
                let view = self.view.borrow().as_ref().and_then(|v| v.upgrade());
                if let Some(view) = view {
                    view.apply(&Envelope::new(Event::Error {
                        message: format!(
                            "{} is not running for this thread yet, so the message was not sent.",
                            driver_label(self.driver.get())
                        ),
                    }));
                }
            }
        }
    }
    fn queue_prompt(&self, text: &str) {
        match self.get() {
            Some(s) => s.queue_prompt(text),
            None => self.send_prompt(text),
        }
    }
    fn interrupt(&self) {
        self.switch_serial
            .set(self.switch_serial.get().wrapping_add(1));
        if let Some((supervisor, thread)) =
            crate::delegation::supervisor().zip(self.thread.as_deref())
        {
            supervisor.root_stop(thread);
        }
        if let Some(s) = self.get() {
            s.interrupt();
        }
    }
    fn respond_approval(&self, request: &str, decision: Decision) {
        if let Some(s) = self.get() {
            s.respond_approval(request, decision);
        }
    }
    fn respond_approval_with_rule(
        &self,
        request: &str,
        decision: Decision,
        custom_rule: Option<crate::always_allow::Rule>,
    ) {
        if let Some(s) = self.get() {
            s.respond_approval_with_rule(request, decision, custom_rule);
        }
    }
    fn answer_questions(&self, request: &str, answers: serde_json::Value) {
        if let Some(s) = self.get() {
            s.answer_questions(request, answers);
        }
    }
    fn respond_form(
        &self,
        request: &str,
        action: agent_core::form::FormAction,
        content: Option<serde_json::Value>,
    ) -> bool {
        self.get()
            .is_some_and(|s| s.respond_form(request, action, content))
    }
    fn switch(&self, driver: Driver, model: Option<String>, effort: Option<String>) {
        // Another agent ends this thread's running work and its sub-agents: its window asks
        // first. The same agent (another model or effort) never does.
        let window = self
            .thread
            .as_deref()
            .filter(|_| driver != self.status().driver)
            .and_then(|thread| Some((thread_owner(thread)?, thread.to_owned())));
        match window {
            Some((window, thread)) => window
                .imp()
                .switch_agent_confirmed(&thread, driver, model, effort),
            None => delegation::switch(self, driver, model, effort),
        }
    }
    fn reload_session(&self) {
        if let Some(s) = self.get() {
            let status = s.status();
            s.rebind_stop();
            delegation::switch(self, status.driver, status.model, status.effort);
        }
    }
    fn set_mode(&self, mode: Mode) {
        if let Some(s) = self.get() {
            let next = agent_core::delegation::authority::ceiling(mode.session_safe());
            if agent_core::delegation::authority::ceiling(s.effective_mode())
                .iter()
                .any(|task_mode| !next.contains(task_mode))
            {
                self.invalidate_pending_switch();
            }
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
                driver: self.driver.get(),
                model: self.model.clone(),
                pending_model: None,
                effort: None,
                mode: self.mode,
                running_turn: false,
                alive: false,
                capabilities: agent_core::caps::Capabilities::of(self.driver.get()),
                commands: Vec::new(),
            },
        }
    }
    fn lifecycle(&self) -> crate::chat::Lifecycle {
        self.get().map(|s| s.lifecycle()).unwrap_or_default()
    }
    fn pop_queued_prompt(&self) -> Option<(String, String)> {
        self.get().and_then(|s| s.pop_queued_prompt())
    }
}

// ---------------------------------------------------------------------------------------------
// App-wide state: the store and the resolved agent binaries
// ---------------------------------------------------------------------------------------------

/// Settings carried to a new thread. Bypass is always reduced to its safe mode.
#[derive(Debug, PartialEq, Eq)]
struct CarriedSettings {
    model: Option<String>,
    effort: Option<String>,
    mode: Option<Mode>,
}

fn carried_settings(
    model: Option<String>,
    effort: Option<String>,
    mode: Option<Mode>,
) -> CarriedSettings {
    CarriedSettings {
        model,
        effort,
        mode: mode.map(Mode::session_safe),
    }
}

/// A chat agent's binary and its profile's environment, resolved off the main thread.
#[derive(Debug, Clone, Default)]
pub(super) struct ResolvedAgent {
    /// `None`: not found by any probe (the command name is then tried as-is).
    pub(super) program: Option<String>,
    pub(super) env: Vec<(String, String)>,
    /// Binding a delegation credential must never silently fall back after an env-file error.
    pub(super) env_failed: bool,
}

thread_local! {
    static STORE: RefCell<Option<Rc<Store>>> = const { RefCell::new(None) };
    /// Where the app's store lives, once it opened a file one. `None` (memory only, tests): there
    /// is nothing a second connection could open, so store jobs run in place.
    static STORE_PATH: RefCell<Option<std::path::PathBuf>> = const { RefCell::new(None) };
    /// The store writer, once a file store opened: every write of the app goes through it, and
    /// `STORE` is then only read. `None` (memory only, tests): writes run in place.
    static WRITER: RefCell<Option<StoreWriter>> = const { RefCell::new(None) };
    /// Store write failures already shown to the user (the writer's count).
    static FAILURES_SHOWN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Why the store writer could not start, until a window has said so.
    static WRITER_START_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
    /// Keyed by (command, env file): a profile edit is a new key, so nothing goes stale.
    static RESOLVED: RefCell<HashMap<(String, Option<String>), ResolvedAgent>> =
        RefCell::new(HashMap::new());
    static OPEN_THREADS: RefCell<HashMap<String, glib::WeakRef<super::super::AgentTerminalWindow>>> = RefCell::new(HashMap::new());
    static OPEN_LAYOUT_WRITES: LayoutWrites = LayoutWrites::default();
    static SCAN_GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static CHAT_RESTORED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Threads whose page is closing for a delete or an archive: their detach discards nothing
    /// and offers no Undo ([`AgentTerminalWindow::close_row_without_discard`]).
    static NO_DISCARD: RefCell<std::collections::HashSet<String>> =
        RefCell::new(std::collections::HashSet::new());
    /// The last session's open threads, as `restore_open_threads` read them before any page
    /// opened, while the empty state offers to reopen them. `None` once reopened or when there
    /// is nothing to offer.
    static LAST_SESSION: RefCell<Option<LastSession>> = const { RefCell::new(None) };
}

/// The scan that may still link its sessions. Read on the writer too, so a scan waiting in its
/// queue is cancelled just like one still reading files.
static SESSION_SCAN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Starts a scan of the agents' own sessions: its generation. Any earlier scan is superseded.
fn begin_session_scan() -> u64 {
    SESSION_SCAN
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .wrapping_add(1)
}

/// "Show Sessions Started Outside Agent Terminal" went off: no scan in flight may link.
fn cancel_session_scans() {
    SESSION_SCAN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the scan begun as `scan` may link its sessions now: the setting is still on
/// (`enabled`) and no newer scan or cancellation came since.
fn session_scan_wanted(scan: u64, enabled: bool) -> bool {
    enabled && SESSION_SCAN.load(std::sync::atomic::Ordering::Relaxed) == scan
}

/// The open-thread layout the last session left, and how many of its threads still exist.
struct LastSession {
    /// The stored layout ([`OPEN_THREADS_KEY`]), as read before this run recorded its own.
    saved: String,
    /// Its threads that still existed when it was read ([`restorable`]).
    count: usize,
}

/// How many threads of the stored layout `saved` could be reopened against the stored thread
/// ids `known` (archived and deleted ones drop out): what the empty state's offer counts.
fn last_session_count(saved: Option<&str>, known: &[String]) -> usize {
    saved
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .map_or(0, |value| {
            restorable(&value, known, crate::config::SessionState::MAX_TABS).len()
        })
}

/// The app's one thread store, opened on first use. A store that cannot be opened falls back
/// to memory (threads then last for this run), and says so in the log.
pub(super) fn app_store() -> Rc<Store> {
    match try_app_store() {
        Ok(store) => store,
        // Unreachable in practice: `setup_shell` calls `try_app_store` first and puts a dialog up
        // when even the in-memory store cannot open, before anything else asks for it.
        Err(e) => {
            error!("SQLite is unusable: {e}");
            std::process::exit(70)
        }
    }
}

/// [`app_store`], reporting the one failure it cannot recover from: no SQLite at all.
pub(super) fn try_app_store() -> Result<Rc<Store>, String> {
    if let Some(store) = STORE.with(|s| s.borrow().clone()) {
        return Ok(store);
    }
    let store = open_store()?;
    STORE.with(|s| *s.borrow_mut() = Some(store.clone()));
    Ok(store)
}

#[cfg(not(test))]
fn open_store() -> Result<Rc<Store>, String> {
    // Screenshots: demo threads in memory. Never the real store, and (being in memory) never the
    // agents' own sessions either: see `list_recent_sessions`.
    if crate::chat::view::demo::demo_threads_requested() {
        warn!("AGENT_TERMINAL_DEMO_THREADS is set: showing demo threads, nothing is saved");
        let store = in_memory_store()?;
        let home = env::var("HOME").unwrap_or_default();
        if let Err(e) = crate::chat::view::demo::seed_demo_threads(&store, &home) {
            error!("Could not seed the demo threads: {e}");
        }
        return Ok(store);
    }
    let path = agent_kit::store::default_path(
        env::var("XDG_STATE_HOME").ok().as_deref(),
        env::var("HOME").ok().as_deref(),
    );
    if let Some(path) = path {
        match Store::open(&path) {
            Ok(store) => {
                // The writer opens its own connection (the schema is already migrated, so this
                // is one quick open). If it cannot start, the app keeps saving the way it always
                // did, on this connection: losing every write would be worse. That is an error,
                // and the user is told (`take_store_failure`).
                match StoreWriter::spawn(&path) {
                    Ok(writer) => {
                        let _ = PANIC_WRITER.set(writer.clone());
                        WRITER.with(|w| *w.borrow_mut() = Some(writer));
                    }
                    Err(e) => {
                        error!("Cannot start the store writer; saving on the main thread: {e}");
                        WRITER_START_ERROR.with(|s| {
                            *s.borrow_mut() = Some(format!(
                                "Threads are saved more slowly: the store writer could not start \
                                 ({e})"
                            ))
                        });
                    }
                }
                STORE_PATH.with(|p| *p.borrow_mut() = Some(path));
                return Ok(Rc::new(store));
            }
            Err(e) => error!("Cannot open the thread store at {}: {e}", path.display()),
        }
    }
    warn!("Threads are kept in memory for this run only");
    in_memory_store()
}

/// Whether [`store_job`] really leaves the main thread (a file store); false for the in-memory
/// fallback and under test, where it runs the job in place.
fn store_is_async() -> bool {
    STORE_PATH.with(|p| p.borrow().is_some())
}

/// The store writer, when there is one (a file store).
pub(super) fn store_writer() -> Option<StoreWriter> {
    // Opened with the store.
    let _ = try_app_store();
    WRITER.with(|w| w.borrow().clone())
}

/// How the main thread reaches the store: writes queued to the writer and reads on the app's
/// connection, or (memory only, tests, no writer) one store that runs writes in place.
pub(super) fn app_handle() -> StoreHandle {
    let reader = app_store();
    match WRITER.with(|w| w.borrow().clone()) {
        Some(writer) => StoreHandle::Queued { reader, writer },
        None => StoreHandle::Inline(reader),
    }
}

/// Runs `job` against the thread store off the main thread, on a connection of its own (SQLite
/// connections are not `Send`; WAL lets it run beside the main one), and returns its result.
/// It first waits (on the worker) for every write queued before it to be committed, so it reads
/// the app's own writes. For reading: a job that writes goes through [`write_job`].
/// `None`: the writer did not catch up in time (nothing is read then), or the worker could not
/// open the store or panicked; already logged. With no file store
/// the job runs in place on the app's.
pub(super) async fn store_job<T: Send + 'static>(
    job: impl FnOnce(&Store) -> T + Send + 'static,
) -> Option<T> {
    let Some(path) = STORE_PATH.with(|p| p.borrow().clone()) else {
        return Some(job(&app_store()));
    };
    let barrier = store_writer().map(|w| w.barrier());
    gtk4::gio::spawn_blocking(move || {
        if let Some(barrier) = barrier {
            // Never read stale rows: a page built from them could start a second session.
            if barrier.recv_timeout(WORKER_FLUSH_WAIT).is_err() {
                error!("The store writer did not catch up; a store read was not made");
                return None;
            }
        }
        match Store::open(&path) {
            Ok(store) => Some(job(&store)),
            Err(e) => {
                warn!("Cannot open the thread store on a worker: {e}");
                None
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        error!("A store job panicked on the worker thread");
        None
    })
}

/// Runs `job` on the store writer, in order after every write queued before it, and returns
/// its result once its batch is committed: for jobs that write (and may read their own writes
/// back). A job that returns `Err` leaves nothing behind (its savepoint is rolled back) and the
/// error comes back for the caller to show. `None`: the writer is gone, far behind, or the job
/// panicked; already logged. Without a writer it runs as [`store_job`] does, atomically.
pub(super) async fn write_job<T: Send + 'static>(
    what: &'static str,
    job: impl FnOnce(&Store) -> agent_kit::store::Result<T> + Send + 'static,
) -> Option<agent_kit::store::Result<T>> {
    write_job_or_late(what, job, || {}).await
}

/// [`write_job`], and when it does not answer in time, `late` runs on the main thread once the
/// writer does run it (however long that takes): what a caller does to show a write that lands
/// late (reload the thread list), never relying on a guess at when it will.
pub(super) async fn write_job_or_late<T: Send + 'static>(
    what: &'static str,
    job: impl FnOnce(&Store) -> agent_kit::store::Result<T> + Send + 'static,
    late: impl FnOnce() + 'static,
) -> Option<agent_kit::store::Result<T>> {
    use std::sync::mpsc::RecvTimeoutError;
    let Some(writer) = store_writer() else {
        // All of it or none of it, as on the writer.
        return store_job(move |s| s.atomically(job)).await;
    };
    let answer = writer.call_result(what, job);
    let waited = gtk4::gio::spawn_blocking(move || match answer.recv_timeout(WORKER_FLUSH_WAIT) {
        Ok(result) => Ok(Some(result)),
        Err(RecvTimeoutError::Timeout) => Err(answer),
        Err(RecvTimeoutError::Disconnected) => Ok(None),
    })
    .await;
    match waited {
        Ok(Ok(Some(result))) => Some(result),
        Ok(Err(answer)) => {
            error!(
                "A store write job did not answer in time (the writer is far behind); it may \
                 still be saved"
            );
            glib::MainContext::default().spawn_local(async move {
                let answered = gtk4::gio::spawn_blocking(move || answer.recv().is_ok())
                    .await
                    .unwrap_or(false);
                if answered {
                    info!("A late store write job finished");
                    late();
                }
            });
            None
        }
        Ok(Ok(None)) | Err(_) => {
            error!("A store write job did not answer (the writer stopped, or the job panicked)");
            None
        }
    }
}

/// Shows (once per failure) that the store writer could not save something: a toast naming the
/// last failure, which the journal has in full. Polled by every window; the first to see a new
/// failure shows it.
pub(super) fn take_store_failure() -> Option<String> {
    if let Some(start) = WRITER_START_ERROR.with(|s| s.borrow_mut().take()) {
        return Some(start);
    }
    let writer = WRITER.with(|w| w.borrow().clone())?;
    let (count, last) = writer.failures();
    if count <= FAILURES_SHOWN.with(std::cell::Cell::get) {
        return None;
    }
    FAILURES_SHOWN.with(|s| s.set(count));
    Some(last.unwrap_or_else(|| "Could not save part of a thread".to_owned()))
}

/// Writes still queued for the store (0 with no writer).
pub(super) fn store_writes_pending() -> usize {
    WRITER.with(|w| w.borrow().as_ref().map_or(0, StoreWriter::pending))
}

/// Drains the store writer and closes its connection, waiting at most `wait` (the app is
/// quitting: the windows are gone, so nothing is left on screen to freeze).
pub(crate) fn close_store_writer(wait: std::time::Duration) {
    let Some(writer) = WRITER.with(|w| w.borrow_mut().take()) else {
        return;
    };
    let started = std::time::Instant::now();
    let pending = writer.pending();
    match writer.shutdown(wait) {
        Shutdown::Drained => debug!(
            pending,
            ms = started.elapsed().as_millis(),
            "shutdown: store writer drained"
        ),
        Shutdown::TimedOut => error!(
            pending = writer.pending(),
            "shutdown: the store writer did not finish in time; its last writes may be lost"
        ),
        Shutdown::WriterGone => error!(
            pending = writer.pending(),
            "shutdown: the store writer had already stopped; writes still queued are lost"
        ),
    }
}

/// Set once with the writer: the panic hook may run on any thread, and the writer's
/// thread-local handle lives on the main one.
static PANIC_WRITER: std::sync::OnceLock<StoreWriter> = std::sync::OnceLock::new();

/// From the panic hook: give the store writer up to `wait` to commit what is queued before the
/// process goes down. Not on the writer's own thread (it cannot answer itself).
pub(crate) fn flush_store_for_panic(wait: std::time::Duration) {
    if std::thread::current().name() == Some("store-writer") {
        return;
    }
    if let Some(writer) = PANIC_WRITER.get() {
        let _ = writer.flush(wait);
    }
}

/// Stores a NEW thread in `cwd` on `driver`/`model`, with its provider thread active and `mode`
/// recorded as the user's choice when one is carried over (a continued or forked thread keeps
/// its source's mode rather than the profile's). Only the new thread is written: the source of
/// a fork is never touched. All or nothing: the agent record failing fails the whole create.
/// (The app queues [`record_new_thread_as`] to the store writer instead.)
#[cfg(test)]
fn record_new_thread(
    store: &Store,
    cwd: &str,
    title: Option<&str>,
    driver: Driver,
    model: Option<&str>,
    mode: Option<Mode>,
) -> agent_kit::store::Result<String> {
    let ids = NewThreadIds {
        thread: store.new_id()?,
        provider: store.new_id()?,
    };
    let spec = NewThreadSpec {
        title: title.map(str::to_owned),
        model: model.map(str::to_owned),
        mode,
        ..NewThreadSpec::new(cwd, driver)
    };
    record_new_thread_as(store, &ids, &spec)?;
    Ok(ids.thread)
}

/// The ids a new thread and its provider thread are created under, made before the write is
/// queued (so the page can open on the thread at once).
struct NewThreadIds {
    thread: String,
    provider: String,
}

/// What a new chat thread is stored with ([`record_new_thread_as`]).
struct NewThreadSpec {
    cwd: String,
    title: Option<String>,
    driver: Driver,
    /// `None`: the agent's own default.
    model: Option<String>,
    /// Recorded as the user's choice when one is carried over (a continued or forked thread
    /// keeps its source's mode rather than the profile's).
    mode: Option<Mode>,
    /// A budgeted, fenced and redacted handoff, with where it came from: stored as the thread's
    /// first events, so the session re-arms it from history whenever it starts.
    handoff: Option<(crate::chat::session::Handoff, String)>,
    attributes: agent_kit::store::ThreadAttributes,
}

impl NewThreadSpec {
    fn new(cwd: &str, driver: Driver) -> Self {
        Self {
            cwd: cwd.to_owned(),
            title: None,
            driver,
            model: None,
            mode: None,
            handoff: None,
            attributes: agent_kit::store::ThreadAttributes::default(),
        }
    }
}

/// What a thread made by `create_chat_thread` carries into its page once it is stored.
pub(super) enum CreationNotice {
    New,
    Fork,
    Compact(String),
}

struct CreatedThread {
    notice: Option<CreationNotice>,
    driver: Driver,
    prompt: Option<String>,
    effort: Option<String>,
    /// It carries a handoff (stored with it): the page is not blank before its session starts.
    handoff: bool,
    title: Option<String>,
    profile_id: Option<String>,
    worktree: Option<crate::worktree::WorktreeInfo>,
    /// The mode it was created in (stored as its choice), known before its page is built.
    mode: Option<Mode>,
}

/// Stores a NEW thread from `spec` under ids made beforehand (what the store writer runs): the
/// thread, its provider thread made active, the mode carried over, the handoff it carries and the
/// profile and worktree it was opened with, all through [`Store::create_thread_record`]. Only the
/// new thread is written: the source of a fork is never touched. All or nothing: a thread
/// without its agent would start a session on whatever the profile says, not on what was chosen.
fn record_new_thread_as(
    store: &Store,
    ids: &NewThreadIds,
    spec: &NewThreadSpec,
) -> agent_kit::store::Result<()> {
    let mut events = Vec::new();
    if let Some(mode) = spec.mode {
        events.push(Envelope::new(Event::ModeChosen { mode }));
    }
    if let Some((handoff, source)) = &spec.handoff {
        events.extend(crate::chat::session::seeded_handoff_events(handoff, source));
    }
    store.create_thread_record(&agent_kit::store::ThreadRecord {
        title: spec.title.as_deref(),
        provider: Some(agent_kit::store::ProviderRecord {
            id: Some(&ids.provider),
            driver: driver_key(spec.driver),
            model: spec.model.as_deref(),
            native_id: None,
        }),
        events,
        attributes: spec.attributes.clone(),
        ..agent_kit::store::ThreadRecord::new(&ids.thread, &spec.cwd)
    })
}

/// The profile a thread made from another (a fork, a continuation) is opened with: the
/// source's while it stays on the source's agent, else the target agent's own (`None`).
fn carried_profile(
    source_driver: Option<Driver>,
    source_profile: Option<String>,
    target: Driver,
) -> Option<String> {
    source_profile.filter(|_| source_driver == Some(target))
}

/// A closed source has no page attributes: its stored provider must identify the target agent
/// before its profile can be inherited. Missing or foreign providers carry no profile.
fn carried_stored_profile(loaded: &LoadedThread, target: Driver) -> Option<String> {
    carried_profile(
        loaded
            .provider
            .as_ref()
            .and_then(|p| Driver::from_key(&p.driver)),
        loaded.attributes.profile_id.clone(),
        target,
    )
}

/// Whether a thread's profile ends with `event`, from its agent `driver`. `profile_runs` is the
/// agent the thread's profile runs (`None`: no profile; `Some(None)`: one that is gone, which
/// [`crate::config::TerminalConfig::thread_profile`] already passes over). Only another agent
/// reporting its session started ends it: the events a switch emits before its agent starts do
/// not, since a switch whose agent never starts rolls back to the old one, on its own profile.
fn profile_outlived(profile_runs: Option<Option<Driver>>, driver: Driver, event: &Event) -> bool {
    matches!(event, Event::SessionStarted { .. })
        && matches!(profile_runs, Some(Some(runs)) if runs != driver)
}

/// The folders a new thread could start in, owned, so they can be checked off the main thread
/// ([`crate::utils::resolve_new_thread_dir`]).
struct ThreadFolders {
    explicit: Option<String>,
    chosen_profile: Option<String>,
    current: Option<String>,
    agent_profile: Option<String>,
    starting: String,
    home: String,
}

impl ThreadFolders {
    /// Blocking: stats the disk.
    fn resolve(&self) -> Result<crate::utils::NewThreadDir, String> {
        let dirs = crate::utils::NewThreadDirs {
            explicit: self.explicit.as_deref(),
            chosen_profile: self.chosen_profile.as_deref(),
            current: self.current.as_deref(),
            agent_profile: self.agent_profile.as_deref(),
            starting: &self.starting,
        };
        crate::utils::resolve_new_thread_dir(&dirs, &self.home, |d| {
            std::path::Path::new(d).is_dir()
        })
    }
}

/// A new thread asked for while the agents were still being detected: made once the default
/// agent is known ([`AgentTerminalWindow::new_chat_thread_with`]).
pub(super) type PendingNewThread = std::boxed::Box<dyn FnOnce(Driver) -> NewThread>;

/// A thread to create (see [`AgentTerminalWindow::create_chat_thread`]). Its folder is
/// [`crate::utils::resolve_new_thread_dir`]'s: `dir`, else the chosen profile's, else `here`,
/// else the agent's profile's, else the starting directory.
pub(super) struct NewThread {
    pub(super) notice: Option<CreationNotice>,
    pub(super) driver: Driver,
    /// A folder the user picked for this thread (a picker, a worktree, a folder's row, the
    /// thread forked): refused if it no longer exists, never replaced.
    pub(super) dir: Option<String>,
    /// The folder of the page in view, when no folder was picked.
    pub(super) here: Option<String>,
    /// Sent once the session has started.
    pub(super) prompt: Option<String>,
    pub(super) model: Option<String>,
    pub(super) effort: Option<String>,
    pub(super) title: Option<String>,
    /// A budgeted, fenced and redacted handoff, with where it came from, stored with the thread.
    pub(super) handoff: Option<(crate::chat::session::Handoff, String)>,
    /// The mode to start in, when the thread carries one over from another (a continued thread
    /// keeps its mode rather than adopting the profile default). `None`: the profile default.
    pub(super) mode: Option<Mode>,
    /// The profile the user chose for the thread (its id), when it is not simply the agent's
    /// own. Kept with the thread, and honoured while that profile runs the thread's agent.
    pub(super) profile_id: Option<String>,
    /// The worktree the thread opens in (New Thread in Isolated Git Worktree), kept with it.
    pub(super) worktree: Option<crate::worktree::WorktreeInfo>,
}

impl NewThread {
    pub(super) fn new(driver: Driver) -> Self {
        Self {
            driver,
            notice: None,
            dir: None,
            here: None,
            prompt: None,
            model: None,
            effort: None,
            title: None,
            handoff: None,
            mode: None,
            profile_id: None,
            worktree: None,
        }
    }
}

/// `model` and `effort` as they should be started: unchanged unless the agent's list, fetched in
/// this run, no longer has `model`, then its replacement (and the record of the move).
fn decide_live_model(
    models: &[agent_core::catalog::CatalogModel],
    fresh: bool,
    model: Option<String>,
    effort: Option<String>,
) -> (Option<String>, Option<String>, Option<RetiredModel>) {
    match model_notice(models, fresh, model.as_deref(), effort.as_deref()) {
        ModelNotice::Retired {
            model: old,
            replacement: Some(replacement),
        } => (
            Some(replacement.model.clone()),
            replacement.effort.clone().or(effort),
            Some(RetiredModel {
                model: old,
                replacement,
            }),
        ),
        _ => (model, effort, None),
    }
}

/// [`decide_live_model`] against the app's catalogue.
fn live_model(
    driver: Driver,
    model: Option<String>,
    effort: Option<String>,
) -> (Option<String>, Option<String>, Option<RetiredModel>) {
    let catalog = ModelCatalog::shared();
    decide_live_model(
        &catalog.models_of(driver),
        catalog.is_fresh(driver),
        model,
        effort,
    )
}

/// What starting a built thread's session needs.
struct StartJob {
    thread: String,
    driver: Driver,
    dir: String,
    profile: Profile,
    resolved: ResolvedAgent,
    /// The active provider thread (its native session id and model), to resume.
    provider: Option<agent_kit::store::ProviderThread>,
    /// agy's hook verdict (always `Ok` for the others).
    hook: Result<(), String>,
}

/// What a thread page needs from the store to be built, read in one off-thread job.
#[derive(Default)]
struct LoadedThread {
    pending_prompt: Option<String>,
    /// Stored events, each with the agent that produced it when the store knows.
    history: Vec<(Option<Driver>, Envelope)>,
    /// The active provider thread (its native session id and model), to resume.
    provider: Option<agent_kit::store::ProviderThread>,
    /// The profile and worktree the thread was opened with.
    attributes: agent_kit::store::ThreadAttributes,
}

fn load_provider(
    store: &Store,
    thread: &str,
) -> agent_kit::store::Result<Option<agent_kit::store::ProviderThread>> {
    let Some(id) = store.active_provider_thread(thread)? else {
        return Ok(None);
    };
    Ok(store
        .provider_threads(thread)?
        .into_iter()
        .find(|p| p.id == id))
}

/// Reads a thread for its page. A thread that points at an agent's own session but holds no
/// events yet (a resumed session, or one listed from the agent's history) first gets that
/// session's transcript imported, so the conversation shows. Runs on a store worker.
///
/// A store error fails the whole load: a thread read as empty with no provider would start a
/// fresh session and re-point the thread at it, losing the one it had.
#[cfg_attr(not(test), allow(dead_code))] // the app goes through `load_thread_with`
fn load_thread(
    store: &Store,
    thread: &str,
    home: Option<&std::path::Path>,
) -> agent_kit::store::Result<LoadedThread> {
    load_thread_with(store, thread, home, None)
}

/// [`load_thread`], storing an import through the store `writer` when there is one (the
/// history is read here, on the worker; only the insert runs on the writer).
fn load_thread_with(
    store: &Store,
    thread: &str,
    home: Option<&std::path::Path>,
    writer: Option<&StoreWriter>,
) -> agent_kit::store::Result<LoadedThread> {
    let provider = load_provider(store, thread)?;
    let mut history = read_history(store, thread)?;
    if history.is_empty() {
        if let (Some(home), Some(pt)) = (home, provider.as_ref()) {
            let imported = match writer {
                None => import_native_history(store, thread, pt, home),
                Some(writer) => native_history_of(pt, home).is_some_and(|(driver, envs)| {
                    let (thread, pt) = (thread.to_owned(), pt.id.clone());
                    writer
                        .call("import a session's history", move |s| {
                            store_imported(s, &thread, &pt, driver, &envs)
                        })
                        .recv_timeout(WORKER_FLUSH_WAIT)
                        .unwrap_or(false)
                }),
            };
            if imported {
                history = read_history(store, thread)?;
            }
        }
    }
    let pending_prompt = store
        .meta(&pending_prompt_key(thread))?
        .filter(|s| !s.is_empty());
    // A close inside the send's sink can precede its metadata clear. The sent message is
    // already queued then; never dispatch the same first prompt again on reopen.
    let pending_prompt = if store.has_messages(thread)? {
        None
    } else {
        pending_prompt
    };
    Ok(LoadedThread {
        history,
        provider,
        pending_prompt,
        attributes: store.thread_attributes(thread)?,
    })
}

/// How many of the agents' own recent sessions are listed as threads.
const RECENT_SESSIONS: usize = 20;

/// Gives each recent native session a thread, unless one already holds it or the user deleted
/// that thread. The thread is dated with the session's time and titled after it; its history is
/// imported when it is first opened ([`load_thread`]). Returns the agent of each one added.
fn link_recent_sessions(
    store: &Store,
    sessions: &[agent_kit::native::NativeSession],
    fallback_cwd: &str,
) -> Vec<Driver> {
    let mut added = Vec::new();
    for s in sessions {
        let key = driver_key(s.driver);
        let known = store
            .thread_with_native_id(key, &s.native_id)
            .map(|t| t.is_some())
            .unwrap_or(true);
        let dismissed = store.native_dismissed(key, &s.native_id).unwrap_or(true);
        if known || dismissed {
            continue;
        }
        let cwd = s.cwd.as_deref().unwrap_or(fallback_cwd);
        let made = store.link_native_thread(
            cwd,
            s.title.as_deref(),
            s.modified.saturating_mul(1000),
            key,
            &s.native_id,
        );
        match made {
            Ok(_) => added.push(s.driver),
            Err(e) => warn!("Cannot list a {} session: {e}", driver_label(s.driver)),
        }
    }
    added
}

/// Imports `pt`'s native session into the store; whether anything was stored. A transcript that
/// cannot be read leaves a notice instead (so it is not retried on every open).
fn import_native_history(
    store: &Store,
    thread: &str,
    pt: &agent_kit::store::ProviderThread,
    home: &std::path::Path,
) -> bool {
    native_history_of(pt, home)
        .is_some_and(|(driver, envs)| store_imported(store, thread, &pt.id, driver, &envs))
}

/// `pt`'s native session history (read from the agent's own files): what
/// [`import_native_history`] stores. `None`: nothing to import.
fn native_history_of(
    pt: &agent_kit::store::ProviderThread,
    home: &std::path::Path,
) -> Option<(Driver, Vec<Envelope>)> {
    let (Some(native_id), Some(driver)) = (pt.native_id.clone(), Driver::from_key(&pt.driver))
    else {
        return None;
    };
    let session = agent_kit::native::NativeSession {
        driver,
        native_id,
        cwd: None,
        title: None,
        modified: 0,
    };
    let envs = match agent_kit::native::read_native_history(home, &session) {
        Ok(envs) => envs,
        Err(e) => {
            warn!(
                "Cannot import {} session history: {e}",
                driver_label(driver)
            );
            vec![Envelope::new(Event::Notice {
                text: format!("The earlier conversation could not be loaded: {e}"),
            })]
        }
    };
    (!envs.is_empty()).then_some((driver, envs))
}

/// Stores an imported history; whether anything was stored.
fn store_imported(
    store: &Store,
    thread: &str,
    provider_thread: &str,
    driver: Driver,
    envs: &[Envelope],
) -> bool {
    match store.import_events(thread, Some(provider_thread), envs) {
        Ok(n) => {
            info!("Imported {n} events of a {} session", driver_label(driver));
            true
        }
        Err(e) => {
            warn!("Cannot store the imported history: {e}");
            false
        }
    }
}

/// Which background probes [`AgentTerminalWindow::refresh_agent_data`] runs after its scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Probes {
    /// Only what the catalogue's and the account status's clocks say is due.
    IfDue,
    /// Every ready agent: its profile changed, or the user asked to check again.
    Force,
}

/// How long a session start waits for its agent's model list to be fetched in this run, so a
/// model the agent retired is ported before it reaches argv. A slow probe does not hold a thread
/// hostage: after this it starts, and the later `set_retired` path still catches the model.
const CATALOG_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// Polls `done` every `step` until it holds or `limit` passes; whether it held.
pub(super) async fn wait_until(
    limit: std::time::Duration,
    step: std::time::Duration,
    done: impl Fn() -> bool,
) -> bool {
    let started = std::time::Instant::now();
    while !done() {
        if started.elapsed() >= limit {
            return false;
        }
        glib::timeout_future(step).await;
    }
    true
}

/// Waits (at most [`CATALOG_WAIT`]) for `driver`'s model list to be fresh, or for its fetch to
/// have failed: a failed probe (offline, signed out) must not cost every thread open 3 s. Only a
/// fetch in this run settles it, never the cache's age: argv decisions use the same predicate.
/// The one start-up fetch per ready agent makes the wait rare after the first seconds.
async fn wait_for_fresh_catalog(driver: Driver) {
    let catalog = ModelCatalog::shared();
    wait_for_catalog(&catalog, driver, CATALOG_WAIT).await;
    if !catalog.is_fresh(driver) {
        info!(
            "{} model list not fresh after {:?}; starting on the stored model",
            driver_label(driver),
            CATALOG_WAIT
        );
    }
}

/// Waits at most `limit` for `catalog`'s fetch of `driver` in this run to settle; whether it did.
async fn wait_for_catalog(
    catalog: &ModelCatalog,
    driver: Driver,
    limit: std::time::Duration,
) -> bool {
    wait_until(limit, std::time::Duration::from_millis(100), || {
        catalog.is_settled(driver)
    })
    .await
}

/// Everything [`AgentTerminalWindow::build_thread`] needs once the agent is resolved and the
/// thread read.
struct BuildJob {
    thread: String,
    driver: Driver,
    dir: String,
    profile: Profile,
    resolved: ResolvedAgent,
    loaded: LoadedThread,
    /// agy's hook verdict (always `Ok` for the others, which do not use the hook).
    hook: Result<(), String>,
}

/// The saved open-thread ids that still exist (archived or deleted ones drop out), at most `max`.
fn restorable(saved: &serde_json::Value, known: &[String], max: usize) -> Vec<String> {
    let layouts = saved["windows"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| vec![saved.clone()]);
    let mut open = Vec::new();
    for layout in layouts {
        for id in layout["open"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
        {
            if open.len() < max && known.iter().any(|k| k == id) && !open.iter().any(|o| o == id) {
                open.push(id.to_owned());
            }
        }
    }
    open
}

/// The thread to bring into view: the saved selection when it is among those reopened, else the
/// first of them.
fn selected_to_restore(saved: &serde_json::Value, open: &[String]) -> Option<String> {
    saved["selected"]
        .as_str()
        .filter(|id| open.iter().any(|o| o == id))
        .or_else(|| {
            saved["windows"].as_array()?.iter().find_map(|w| {
                w["selected"]
                    .as_str()
                    .filter(|id| open.iter().any(|o| o == id))
            })
        })
        .or(open.first().map(String::as_str))
        .map(str::to_owned)
}

/// The thread's history for the view, oldest first (read in batches): all of it, or for a very
/// long thread its newest [`REPLAY_LIMIT`] events, cut to start at a turn so no step arrives
/// without its start, behind a notice saying earlier messages are not shown. (Reading the oldest
/// window showed a long thread's beginning and none of its recent turns.)
fn read_history(
    store: &Store,
    thread: &str,
) -> agent_kit::store::Result<Vec<(Option<Driver>, Envelope)>> {
    read_history_within(store, thread, REPLAY_LIMIT)
}

/// [`read_history`] with its own limit. Each event comes with the agent that produced it (the
/// provider thread it was stored under), so a thread that switched agents replays each reply
/// under its own agent; `None` when the store does not know.
fn read_history_within(
    store: &Store,
    thread: &str,
    limit: usize,
) -> agent_kit::store::Result<Vec<(Option<Driver>, Envelope)>> {
    let start = store.tail_after(thread, limit)?;
    let mut history = Vec::new();
    let mut after = start;
    loop {
        let batch = store.events_by_agent(thread, after, 2_000)?;
        let Some(last) = batch.last() else {
            break;
        };
        after = Some(last.seq);
        history.extend(batch.into_iter().map(|e| {
            let driver = e.driver.as_deref().and_then(Driver::from_key);
            (driver, e.envelope)
        }));
        if history.len() >= limit {
            break;
        }
    }
    if start.is_some() {
        // The first whole turn starts at its prompt, which is stored before the agent's
        // `TurnStarted`: cut at the last user message before that, else at the turn itself. A
        // window holding no turn start (one turn longer than the limit) starts at its first
        // prompt, or as it is when it has none.
        let prompt = |(_, e): &(Option<Driver>, Envelope)| {
            matches!(
                e.event,
                Event::ItemStarted {
                    kind: ItemKind::UserMessage,
                    ..
                }
            )
        };
        let cut = match history
            .iter()
            .position(|(_, e)| matches!(e.event, Event::TurnStarted { .. }))
        {
            Some(turn) => history[..turn].iter().rposition(prompt).unwrap_or(turn),
            None => history.iter().position(prompt).unwrap_or(0),
        };
        history.drain(..cut);
        history.insert(
            0,
            (
                None,
                Envelope::new(Event::Notice {
                    text: "This thread is long: only its most recent part is shown here."
                        .to_owned(),
                }),
            ),
        );
    }
    Ok(history)
}

/// Tests never touch the real `~/.local/state`.
#[cfg(test)]
fn open_store() -> Result<Rc<Store>, String> {
    in_memory_store()
}

fn in_memory_store() -> Result<Rc<Store>, String> {
    // An in-memory SQLite that cannot open means no SQLite at all.
    Store::open_in_memory()
        .map(Rc::new)
        .map_err(|e| e.to_string())
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
    // Tests never find this machine's agents: only what a test seeded resolves, so no test can
    // start a real agent on PATH (one once did, through a profile it could not place).
    if cfg!(test) {
        return ResolvedAgent::default();
    }
    let (path, home, shell) = env_triplet();
    let command = profile.command.clone();
    let env_file = profile.env_file.clone();
    let resolved = gtk4::gio::spawn_blocking(move || {
        let probe = crate::utils::SystemProbe::new(path, home, shell);
        let program = probe.locate(&command);
        let sourced = env_file
            .as_deref()
            .filter(|f| !f.trim().is_empty())
            .map(crate::utils::load_env_file_checked)
            .unwrap_or_else(|| Ok(Vec::new()));
        let env_failed = sourced.is_err();
        ResolvedAgent {
            program,
            env: sourced.unwrap_or_default(),
            env_failed,
        }
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
pub(super) fn unset_list(patterns: &[String]) -> Vec<String> {
    // `vars_os`, lossy: one non-UTF-8 variable in the user's environment must not panic here.
    let mut env: Vec<String> = std::env::vars_os()
        .map(|(k, v)| format!("{}={}", k.to_string_lossy(), v.to_string_lossy()))
        .collect();
    let mut names = crate::utils::strip_env(&mut env, patterns);
    for own in [
        agent_core::approval::ENV_SOCKET,
        crate::approval_server::ENV_HOOK_BIN,
    ] {
        if !names.iter().any(|n| n == own) {
            names.push(own.to_owned());
        }
    }
    for own in agent_core::delegation::env::STRIP_ON_SPAWN {
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
/// the cached resolution (a switch cannot wait). An agent that is not Ready (missing, switched
/// off, still being detected) or has no resolved binary is refused with the reason: nothing is
/// ever spawned from a guessed command name.
fn agent_launch(driver: Driver, thread: &str, cwd: &str) -> Result<AgentLaunch, String> {
    // The thread's own profile while it runs `driver` (a restart or a roll back); a switch to
    // another agent gets that agent's own.
    let profile_id = thread_owner(thread).and_then(|window| {
        window
            .imp()
            .tabs
            .borrow()
            .iter()
            .filter_map(|t| t.chat.as_ref())
            .find(|c| c.thread == thread)
            .and_then(|c| c.profile_id.clone())
    });
    agent_launch_with(
        &AgentAvailability::shared().get(driver),
        driver,
        thread,
        cwd,
        profile_id.as_deref(),
    )
}

/// [`agent_launch`] against an explicit availability.
fn agent_launch_with(
    state: &Availability,
    driver: Driver,
    thread: &str,
    cwd: &str,
    profile_id: Option<&str>,
) -> Result<AgentLaunch, String> {
    if !state.is_ready() {
        return Err(not_ready_reason(driver, state));
    }
    let config = SharedConfig::default();
    let (profile, clear) = {
        let c = config.borrow();
        (
            c.thread_profile(profile_id, driver)
                .cloned()
                .unwrap_or_else(|| crate::config::new_agent_profile(driver)),
            c.clear_env.clone(),
        )
    };
    let resolved = cached_agent(&profile).unwrap_or_default();
    let Some(program) = resolved.program.clone() else {
        return Err(not_ready_reason(driver, &Availability::Missing));
    };
    let adapter = make_adapter(driver, &program);
    let env = LaunchEnv {
        env: resolved.env,
        unset: unset_list(&clear),
    };
    let approval = match driver {
        // Claude and Codex raise approvals over their own protocol, not the hook socket.
        Driver::Claude | Driver::Codex => Err(None),
        Driver::Agy => {
            let target = crate::hook_config::HookTarget::new(&env, Some(cwd.into()));
            let verdict = cached_hook_verdict(&target);
            // A switch cannot await I/O. Its missing proof is checked explicitly for the next launch.
            if HOOK_VERDICTS.with(|cache| cache.borrow().needs_check(&target)) {
                // The thread's own context (the default one on the app's main thread), so a test
                // on its own loop never contends for the global default.
                glib::spawn_future_local(async move {
                    let _ = check_hook(target).await;
                });
            }
            bind_approval(
                verdict,
                thread,
                cwd,
                profile.default_mode.unwrap_or_default(),
            )
        }
    };
    Ok(AgentLaunch {
        adapter,
        program,
        extra_args: profile.args.clone(),
        default_model: profile.effective_default_model(None),
        default_effort: profile.default_effort.clone(),
        env,
        approval,
    })
}

/// The mode a thread's session starts in: the user's own choice in the thread first (a carried
/// mode is recorded as one, see `create_chat_thread`), else the mode its history last ran in,
/// else the profile's default, else Ask.
fn start_mode(chosen: Option<Mode>, replayed: Option<Mode>, profile_default: Option<Mode>) -> Mode {
    chosen.or(replayed).or(profile_default).unwrap_or_default()
}

/// Why `driver` cannot be started in `state`, as the thread says it.
fn not_ready_reason(driver: Driver, state: &Availability) -> String {
    let why = match state {
        Availability::Disabled => "is switched off",
        Availability::Detecting => "is still being detected",
        Availability::Missing | Availability::Ready(_) => "is not available",
    };
    format!("{} {why}.", driver_label(driver))
}

/// The adapter for `driver`. Codex reports this app's version in `initialize`'s `clientInfo`.
fn make_adapter(driver: Driver, program: &str) -> std::boxed::Box<dyn Adapter> {
    match driver {
        Driver::Claude => std::boxed::Box::new(agent_core::claude::ClaudeAdapter::new()),
        Driver::Agy => std::boxed::Box::new(agent_core::agy::AgyAdapter::new(program.to_owned())),
        Driver::Codex => std::boxed::Box::new(
            agent_core::codex::CodexAdapter::new().client_version(env!("CARGO_PKG_VERSION")),
        ),
    }
}

/// agy's approval socket, only once its hooks file is proven to install the hook (`hook` is the
/// verdict of [`check_hook`]). Without one agy runs without the skip flag and cannot ask before
/// acting; the session says why: how to install the hook (`Err(None)`), or, when it is installed
/// but the socket failed, the reason (`Err(Some(_))`, such as a binary replaced by an upgrade).
fn bind_approval(hook: Result<(), String>, thread: &str, cwd: &str, mode: Mode) -> Approval {
    let installed = hook.is_ok();
    let refusal = hook
        .as_ref()
        .err()
        .filter(|reason| crate::hook_config::verification_refused(reason))
        .cloned();
    // Not checked yet is not "not installed": the notice says which.
    let unchecked = hook
        .as_ref()
        .is_err_and(|r| r == crate::chat::session::HOOK_NOT_CHECKED);
    ApprovalHandle::bind_checked(hook, thread, std::path::Path::new(cwd), mode).map_err(|reason| {
        info!("agy runs without its approval hook in this thread: {reason}");
        if unchecked {
            Some(crate::chat::session::HOOK_NOT_CHECKED.to_owned())
        } else if let Some(refusal) = &refusal {
            Some(refusal.clone())
        } else {
            installed.then_some(reason)
        }
    })
}

thread_local! {
    /// Independent launch proofs for the most recently checked home/directory pairs.
    static HOOK_VERDICTS: RefCell<crate::hook_config::HookCache> = RefCell::new(crate::hook_config::HookCache::default());
}

/// Reads agy's hooks file off the main thread and remembers the verdict. A thread being built
/// awaits this; a switch to agy (which cannot wait) uses [`cached_hook_verdict`].
pub(super) async fn check_hook(target: crate::hook_config::HookTarget) -> Result<(), String> {
    let checked = target.clone();
    let verdict = gtk4::gio::spawn_blocking(move || crate::hook_config::check_target(&target))
        .await
        .unwrap_or_else(|_| Err("the hooks check panicked".to_owned()));
    HOOK_VERDICTS.with(|cache| cache.borrow_mut().remember(&checked, verdict.clone()));
    verdict
}

/// Seeds test proofs through the same cache write as the asynchronous launch check.
#[cfg(test)]
fn remember_hook_probe(
    target: crate::hook_config::HookTarget,
    probe: &crate::hook_config::HookProbe,
) {
    HOOK_VERDICTS.with(|cache| {
        cache
            .borrow_mut()
            .remember(&target, crate::hook_config::probe_verdict(probe))
    });
}

/// The last [`check_hook`] verdict. Fail-closed: before any check has finished it is an `Err`, so
/// agy starts without the skip flag rather than guess. An `Ok` only binds the socket and drives
/// the UI: it never passes the flag by itself, because the session proves the hook sources again,
/// off the main thread, right before every agy process it starts with the flag
/// (`ChatSession`'s `start_process`), and starts it hookless if that fails.
fn cached_hook_verdict(target: &crate::hook_config::HookTarget) -> Result<(), String> {
    HOOK_VERDICTS.with(|cache| cache.borrow().lookup(target))
}

/// A completed check is useful only if the launch still has the same proven sources.
fn hook_for_launch(checked: Result<(), String>, env: &AgentEnv, cwd: &str) -> Result<(), String> {
    cached_hook_verdict(&crate::hook_config::HookTarget::new(env, Some(cwd.into()))).and(checked)
}

/// The current agy profile's hook sources from the same cached resolution a switch uses. An
/// unresolved profile is not a reason to guess the app's home.
fn cached_agy_hook_target(
    cwd: Option<std::path::PathBuf>,
) -> Option<crate::hook_config::HookTarget> {
    let config = SharedConfig::default();
    let config = config.borrow();
    let profile = config
        .agent_profile(Driver::Agy)
        .cloned()
        .unwrap_or_else(|| crate::config::new_agent_profile(Driver::Agy));
    let resolved = cached_agent(&profile)?;
    Some(crate::hook_config::HookTarget::new(
        &AgentEnv {
            env: resolved.env,
            unset: unset_list(&config.clear_env),
        },
        cwd,
    ))
}

/// Whether a switch to agy in `cwd` would get its approval hook now, from the cached proof the
/// switch itself uses. Only for saying so beforehand: no check runs and nothing is bound.
fn agy_hook_ready(cwd: &str) -> bool {
    cached_agy_hook_target(Some(cwd.into())).is_some_and(|t| cached_hook_verdict(&t).is_ok())
}

/// Resolves the profile off the main thread before a refresh or Settings checks its hook.
pub(super) async fn agy_hook_target(
    cwd: Option<std::path::PathBuf>,
) -> crate::hook_config::HookTarget {
    let config = SharedConfig::default();
    let (profile, clear) = {
        let config = config.borrow();
        (
            config
                .agent_profile(Driver::Agy)
                .cloned()
                .unwrap_or_else(|| crate::config::new_agent_profile(Driver::Agy)),
            config.clear_env.clone(),
        )
    };
    let resolved = resolve_agent(profile).await;
    crate::hook_config::HookTarget::new(
        &AgentEnv {
            env: resolved.env,
            unset: unset_list(&clear),
        },
        cwd,
    )
}

/// Every active agy environment and every possible switch target, without retaining env values.
fn hook_refresh_targets(
    directories: Vec<std::path::PathBuf>,
    mut active: Vec<crate::hook_config::HookTarget>,
    profile: crate::hook_config::HookTarget,
) -> Vec<crate::hook_config::HookTarget> {
    active.extend(directories.into_iter().map(|cwd| {
        let mut target = profile.clone();
        target.cwd = Some(cwd);
        target
    }));
    let mut seen = std::collections::HashSet::new();
    active.retain(|target| seen.insert(target.clone()));
    active
}

/// Explicitly checks each open thread's sources (window start, focus, the periodic refresh).
fn refresh_hook_verdict(
    directories: Vec<std::path::PathBuf>,
    active: Vec<crate::hook_config::HookTarget>,
) {
    glib::MainContext::default().spawn_local(async move {
        let profile = agy_hook_target(None).await;
        let targets = hook_refresh_targets(directories, active, profile);
        HOOK_VERDICTS.with(|cache| cache.borrow_mut().set_refresh_size(targets.len()));
        for target in targets {
            let _ = check_hook(target).await;
        }
    });
}

// ---------------------------------------------------------------------------------------------
// The sidebar
// ---------------------------------------------------------------------------------------------

/// A row's widget and the label holding its age, kept to update them without a rebuild.
struct DrawnRow {
    row: gtk4::ListBoxRow,
    age: Label,
}

impl DrawnRow {
    fn new(row: &gtk4::ListBoxRow) -> Self {
        let mut child = row.child().and_then(|line| line.first_child());
        let mut age = None;
        while let Some(widget) = child {
            child = widget.next_sibling();
            if widget.has_css_class("thread-age") {
                age = widget.downcast::<Label>().ok();
            }
        }
        Self {
            row: row.clone(),
            // Every row has one (`sidebar_row_widget`); a detached label keeps this total.
            age: age.unwrap_or_default(),
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Moves the sidebar's clock, so a test can age the rows.
    static CLOCK_SKEW_MS: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
}

/// The time the sidebar draws ages against.
fn sidebar_now() -> i64 {
    #[cfg(test)]
    {
        now_ms() + CLOCK_SKEW_MS.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        now_ms()
    }
}

/// How many times the sidebar was asked to refresh (the debug timing log numbers them).
static SIDEBAR_REFRESHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(super) struct Sidebar {
    pub(super) root: gtk4::Box,
    pub(super) list: gtk4::ListBox,
    search: gtk4::SearchEntry,
    /// Row index → what it opens (`None` for a folder header).
    pub(super) keys: RefCell<Vec<Option<RowKey>>>,
    /// The folder groups the user expanded; every other one is drawn collapsed.
    expanded: RefCell<std::collections::HashSet<String>>,
    /// Set while the list is rebuilt, so selecting the current row is not taken as a click.
    rebuilding: std::cell::Cell<bool>,
    /// What the list on screen shows (`None`: nothing drawn yet), to skip or shrink a redraw.
    drawn: RefCell<Option<SidebarModel>>,
    /// The widgets of the rows in `drawn`, in the order [`SidebarModel::rows`] lists them.
    drawn_rows: RefCell<Vec<DrawnRow>>,
    /// The thread list has not loaded yet, so the list shows skeleton rows (its placeholder).
    loading: std::cell::Cell<bool>,
    /// Those skeleton rows.
    placeholder: gtk4::Widget,
    /// Held for the sidebar's lifetime: the footer's usage indicator.
    _usage: Rc<UsageIndicator>,
}

fn sidebar_focus_target(
    sidebar: &Sidebar,
    focus: &gtk4::Widget,
) -> Option<(RowKey, Option<&'static str>)> {
    let mut widget = Some(focus.clone());
    let mut control = None;
    while let Some(current) = widget {
        for class in ["thread-close", "thread-delete"] {
            if current.has_css_class(class) {
                control = Some(class);
            }
        }
        if let Some(row) = current.downcast_ref::<gtk4::ListBoxRow>() {
            if row.parent().as_ref() == Some(sidebar.list.upcast_ref()) {
                let index = usize::try_from(row.index()).ok()?;
                let key = sidebar.keys.borrow().get(index)?.clone()?;
                return Some((key, control));
            }
        }
        widget = current.parent();
    }
    None
}

impl AgentTerminalWindow {
    /// Builds the chat-first shell into `container`: the sidebar beside the (tab-bar-less) tab
    /// view, with an empty state when no page is open.
    pub(super) fn setup_shell(&self, container: &Box, tab_view: &adw::TabView) {
        let obj = self.obj();
        // Threads need a store; if not even an in-memory SQLite opens there is nothing to show.
        if let Err(e) = try_app_store() {
            let dialog = adw::AlertDialog::new(
                Some("Threads Are Unavailable"),
                Some(&format!(
                    "Agent Terminal cannot use SQLite on this system, so it cannot keep \
                     threads.\n\n{e}"
                )),
            );
            dialog.add_response("quit", "Quit");
            dialog.set_default_response(Some("quit"));
            dialog.set_close_response("quit");
            dialog.connect_response(
                None,
                glib::clone!(
                    #[weak]
                    obj,
                    move |_, _| {
                        if let Some(app) = obj.application() {
                            app.quit();
                        }
                    }
                ),
            );
            dialog.present(Some(obj.upcast_ref::<gtk4::Widget>()));
            return;
        }
        self.start_delegation();
        let split = adw::OverlaySplitView::new();
        split.set_vexpand(true);
        split.set_min_sidebar_width(240.0);
        split.set_max_sidebar_width(340.0);
        split.set_show_sidebar(!self.config.borrow().sidebar_collapsed);

        self.setup_thread_menu_actions();
        let sidebar = self.build_sidebar();
        split.set_sidebar(Some(&sidebar.root));

        let toast_overlay = adw::ToastOverlay::new();
        let pages = gtk4::Stack::new();
        pages.add_named(tab_view, Some("threads"));
        pages.add_named(&self.build_empty_state(&split), Some("empty"));
        // Threads from the last session are about to reopen: hold their space with a thread's
        // skeleton rather than flash the empty state first. Only for as long as that lasts
        // (`settle_pages` removes it): a stack is as large as its largest page.
        pages.set_transition_type(gtk4::StackTransitionType::Crossfade);
        pages.set_transition_duration(loading::CROSSFADE_MS);
        self.restoring_threads
            .set(!CHAT_RESTORED.with(std::cell::Cell::get));
        if self.restoring_threads.get() {
            pages.add_named(&loading::chat(), Some("loading"));
            pages.set_visible_child_name("loading");
        } else {
            pages.set_visible_child_name("empty");
        }
        toast_overlay.set_child(Some(&pages));
        toast_overlay.set_vexpand(true);
        split.set_content(Some(&toast_overlay));
        container.append(&split);
        *self.toast_overlay.borrow_mut() = Some(toast_overlay);
        *self.pages_stack.borrow_mut() = Some(pages);

        // Narrow windows overlay the sidebar instead of squeezing the thread.
        let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            SIDEBAR_COLLAPSE_SP,
            adw::LengthUnit::Px,
        ));
        breakpoint.add_setter(&split, "collapsed", Some(&true.to_value()));
        obj.add_breakpoint(breakpoint);

        // Keep this in step with an actual resize as well as the declarative breakpoint above.
        // In particular this covers a restored or Broadway-hosted window, where the window-level
        // breakpoint can retain its initial state after the allocation changes.
        let update_collapsed = {
            let split = split.downgrade();
            let obj = obj.downgrade();
            move || {
                // Not allocated yet: leave the breakpoint's state alone rather than force 0 px.
                if let (Some(split), Some(obj)) =
                    (split.upgrade(), obj.upgrade().filter(|o| o.width() > 0))
                {
                    split.set_collapsed(obj.width() as f64 <= SIDEBAR_COLLAPSE_SP);
                }
            }
        };
        update_collapsed();
        on_window_resized(obj.upcast_ref(), update_collapsed);

        if let Some(header) = self.header.borrow().as_ref() {
            let toggle = gtk4::ToggleButton::builder()
                .icon_name("at-sidebar-show-symbolic")
                .tooltip_text("Show or Hide Threads (F9)")
                .active(split.shows_sidebar())
                .build();
            split
                .bind_property("show-sidebar", &toggle, "active")
                .bidirectional()
                .build();
            split.connect_notify_local(
                Some("show-sidebar"),
                glib::clone!(
                    #[weak]
                    obj,
                    move |_, _| obj.imp().update_sidebar_toggle_attention()
                ),
            );
            // Only a user's toggle is remembered, not the narrow-width overlay closing itself.
            toggle.connect_clicked(glib::clone!(
                #[weak]
                obj,
                move |t| {
                    let imp = obj.imp();
                    imp.remember_sidebar(t.is_active());
                    imp.update_sidebar_toggle_attention();
                }
            ));
            header.pack_start(&toggle);
            self.shell_controls
                .borrow_mut()
                .push(toggle.clone().upcast());
            *self.sidebar_toggle.borrow_mut() = Some(toggle);
        }
        *self.split_view.borrow_mut() = Some(split);
        *self.sidebar.borrow_mut() = Some(sidebar);

        self.wire_thread_pages(tab_view);
        self.watch_availability();
        self.watch_account_status();
        self.refresh_sidebar();
        // Relative times age; a minute is their resolution. Nobody sees them while the window is
        // hidden or minimised; a visible window keeps them current even unfocused, and regaining
        // focus refreshes once (`watch_availability`). Idle agents are stopped on the same minute,
        // hidden window or not.
        glib::timeout_add_seconds_local(
            60,
            glib::clone!(
                #[weak]
                obj,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    if obj.is_mapped() {
                        obj.imp().refresh_sidebar();
                    }
                    obj.imp().suspend_idle_threads();
                    glib::ControlFlow::Continue
                }
            ),
        );
    }

    /// Stops the agent of every thread here that has been out of sight and idle for
    /// `idle_suspend_minutes`, to free its memory (hundreds of MB each). Its conversation
    /// resumes with the next prompt. Nothing is stored or added to its transcript; its row says
    /// "Paused" and its header "Paused: resumes when you send".
    pub(super) fn suspend_idle_threads(&self) {
        let minutes = self.config.borrow().idle_suspend_minutes;
        if minutes == 0 || self.is_closing() {
            return;
        }
        let shown = self.shown_thread.borrow().clone();
        let now = std::time::Instant::now();
        // Collected first: a session stopping must not find the registry borrowed.
        type Due = (String, Rc<ChatSession>, Option<ChatView>);
        let due: Vec<Due> = self
            .tabs
            .borrow()
            .iter()
            .filter_map(|t| t.chat.as_ref())
            .filter(|c| {
                suspension_due(
                    minutes,
                    now.saturating_duration_since(c.last_seen.get()),
                    shown.as_deref() == Some(c.thread.as_str()),
                    c.holds_suspension(),
                )
            })
            .filter_map(|c| c.slot.get().map(|s| (c.thread.clone(), s, c.view.clone())))
            .collect();
        let mut paused = false;
        for (thread, session, view) in due {
            if session.suspend_if_idle() {
                info!("Stopped the idle agent of thread {thread} ({minutes} min out of sight); it resumes with the next prompt");
                paused = true;
                // The suspension is silent (no event reaches the view): its header is told.
                if let Some(view) = view {
                    view.refresh_status();
                }
            }
        }
        if paused {
            self.refresh_sidebar();
        }
    }

    fn remember_sidebar(&self, shown: bool) {
        if self.config.borrow().sidebar_collapsed == !shown {
            return;
        }
        self.config.borrow_mut().sidebar_collapsed = !shown;
        self.schedule_config_save();
    }

    /// F9.
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

    /// When the sidebar is collapsed or hidden, updates the toggle button's attention state
    /// (pulsing amber glow for blocking approvals/questions, or unread accent for completed turns).
    pub(super) fn update_sidebar_toggle_attention(&self) {
        let Some(toggle) = self.sidebar_toggle.borrow().clone() else {
            return;
        };
        let sidebar_shown = self
            .split_view
            .borrow()
            .as_ref()
            .is_some_and(|s| s.shows_sidebar());
        if sidebar_shown {
            toggle.remove_css_class("needs-attention");
            toggle.remove_css_class("has-unread");
            toggle.set_tooltip_text(Some("Show or Hide Threads (F9)"));
            return;
        }
        let rows = self.sidebar_rows();
        let selected = self.selected_row_key();
        let focused = self.obj().is_active();
        let needs_attn = rows
            .iter()
            .filter(|r| needs_attention(r, selected.as_ref(), focused))
            .count();
        let unread = rows
            .iter()
            .filter(|r| Some(&r.key) != selected.as_ref() && r.badge == Some(Badge::Unread))
            .count();

        if needs_attn > 0 {
            toggle.add_css_class("needs-attention");
            toggle.remove_css_class("has-unread");
            let text = match needs_attn {
                1 => "Show Threads (1 thread needs approval · F9)".to_string(),
                n => format!("Show Threads ({n} threads need approval · F9)"),
            };
            toggle.set_tooltip_text(Some(&text));
        } else if unread > 0 {
            toggle.remove_css_class("needs-attention");
            toggle.add_css_class("has-unread");
            let text = match unread {
                1 => "Show Threads (1 unread thread · F9)".to_string(),
                n => format!("Show Threads ({n} unread threads · F9)"),
            };
            toggle.set_tooltip_text(Some(&text));
        } else {
            toggle.remove_css_class("needs-attention");
            toggle.remove_css_class("has-unread");
            toggle.set_tooltip_text(Some("Show or Hide Threads (F9)"));
        }
    }

    fn build_empty_state(&self, split: &adw::OverlaySplitView) -> gtk4::Widget {
        let obj = self.obj();
        let page = adw::StatusPage::builder()
            .title("No thread open")
            .description(
                "Start a new thread, or pick one from the sidebar.\n\
                 Ctrl+Shift+T new thread · Ctrl+Shift+G new thread in an isolated worktree · F9 threads",
            )
            .vexpand(true)
            .build();
        page.set_paintable(Some(&crate::icons::hero_paintable(
            crate::icons::APP_ART,
            128,
            &page,
        )));
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
        let actions = Box::new(gtk4::Orientation::Vertical, 8);
        actions.append(&new);
        let show = Button::builder()
            .label("Show Threads")
            .halign(Align::Center)
            .tooltip_text("Show existing threads (F9, Ctrl+Shift+B)")
            .build();
        split
            .bind_property("show-sidebar", &show, "visible")
            .invert_boolean()
            .sync_create()
            .build();
        show.connect_clicked(glib::clone!(
            #[weak]
            obj,
            #[weak]
            split,
            move |_| {
                split.set_show_sidebar(true);
                if !split.is_collapsed() {
                    obj.imp().remember_sidebar(true);
                }
                if let Some(sidebar) = obj.imp().sidebar.borrow().as_ref() {
                    sidebar.search.grab_focus();
                }
            }
        ));
        actions.append(&show);
        // Shown once the last session's open threads are known, and only when there are some
        // that "Reopen Last Thread" (off) did not reopen.
        let reopen = Button::builder()
            .halign(Align::Center)
            .css_classes(["pill"])
            .tooltip_text("Open the threads that were open when Agent Terminal last closed")
            .visible(false)
            .build();
        reopen.connect_clicked(glib::clone!(
            #[weak]
            obj,
            move |_| obj.imp().reopen_last_session()
        ));
        actions.insert_child_after(&reopen, Some(&new));
        *self.reopen_button.borrow_mut() = Some(reopen);
        self.update_reopen_offer();
        page.set_child(Some(&actions));
        page.upcast()
    }

    /// Shows the empty state's reopen offer while the last session left threads to reopen
    /// ([`LAST_SESSION`]), and hides it otherwise.
    fn update_reopen_offer(&self) {
        let Some(button) = self.reopen_button.borrow().clone() else {
            return;
        };
        let offer = LAST_SESSION.with(|s| s.borrow().as_ref().and_then(|l| reopen_offer(l.count)));
        match offer {
            Some(label) => {
                button.set_label(&label);
                button.set_visible(true);
            }
            None => button.set_visible(false),
        }
    }

    /// The empty state's "Reopen N threads from your last session": reopens the saved layout
    /// (whatever "Reopen Last Thread" says) against the threads that still exist now, read off
    /// the main thread. Offered once: the snapshot is spent.
    fn reopen_last_session(&self) {
        let Some(last) = LAST_SESSION.with(|s| s.borrow_mut().take()) else {
            return;
        };
        self.for_each_window(|window| window.update_reopen_offer());
        let obj = self.obj().downgrade();
        let before = self.thread_pages();
        glib::MainContext::default().spawn_local(async move {
            let threads = store_job(|store| store.list_threads(false)).await;
            let Some(obj) = obj.upgrade() else { return };
            let imp = obj.imp();
            match threads {
                Some(Ok(threads)) => {
                    if !imp.reopen_saved(Some(last.saved), threads, before, true) {
                        imp.show_toast("Those threads are no longer in the list");
                    }
                }
                failed => {
                    if let Some(Err(e)) = failed {
                        warn!("Cannot list threads to reopen: {e}");
                    }
                    // Nothing was reopened: the offer stands.
                    LAST_SESSION.with(|s| *s.borrow_mut() = Some(last));
                    imp.for_each_window(|window| window.update_reopen_offer());
                    imp.show_toast("Could not read the thread list. Try again.");
                }
            }
            imp.settle_pages();
        });
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
            .icon_name("at-list-add-symbolic")
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
        // Until the first load lands (`refresh_sidebar`), rows in the shape of the real ones.
        let placeholder = loading::sidebar_rows();
        list.set_placeholder(Some(&placeholder));
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
        let archived = gtk4::ToggleButton::builder()
            .label("Show archived")
            .tooltip_text("Also list archived threads")
            .css_classes(["flat", "caption"])
            .halign(Align::Start)
            .build();
        archived.connect_toggled(glib::clone!(
            #[weak]
            obj,
            move |button| {
                let imp = obj.imp();
                imp.show_archived.set(button.is_active());
                imp.refresh_sidebar();
            }
        ));
        footer.append(&archived);
        // Every agent's usage and account, updated on every turn.
        let usage = UsageIndicator::new(AccountStatus::shared(), None);
        // Side by side, two agents' meters made the sidebar wider than its maximum.
        usage.stack_agents();
        footer.append(usage.widget());
        root.append(&footer);

        let sidebar = Rc::new(Sidebar {
            root,
            list: list.clone(),
            search: search.clone(),
            keys: RefCell::new(Vec::new()),
            expanded: RefCell::new(std::collections::HashSet::new()),
            rebuilding: std::cell::Cell::new(false),
            drawn: RefCell::new(None),
            drawn_rows: RefCell::new(Vec::new()),
            loading: std::cell::Cell::new(true),
            placeholder,
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
        self.install_thread_menu_triggers(&sidebar);
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
        // From the last load: the sidebar never reads the store on the main thread.
        let summaries = self.summaries.borrow().clone().unwrap_or_default();
        let tabs = self.tabs.borrow();
        // Looked up once per summary, not scanned for: the first open tab of a thread wins.
        let mut open: HashMap<&str, &TabState> = HashMap::new();
        for tab in tabs.iter() {
            if let Some(chat) = tab.chat.as_ref() {
                open.entry(chat.thread.as_str()).or_insert(tab);
            }
        }
        let mut rows: Vec<SidebarRow> = summaries
            .into_iter()
            .map(|s| {
                let tab = open.get(s.id.as_str()).copied();
                let chat = tab.and_then(|t| t.chat.as_ref());
                let badge = match chat {
                    Some(c) => badge_for(
                        c.running,
                        c.background_work(
                            crate::delegation::supervisor().is_some_and(|s| s.root_busy(&c.thread)),
                        ),
                        c.waiting_on_user(),
                        c.rate_limited,
                        c.unread,
                    ),
                    // A closed thread with events nobody has seen.
                    None => badge_for(false, false, false, false, s.unread && s.updated_at > 0),
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
                    activity: chat.and_then(ChatTab::activity),
                    background: chat
                        .map(|c| {
                            c.background
                                .iter()
                                .map(|t| t.description.clone().unwrap_or_default())
                                .collect()
                        })
                        .unwrap_or_default(),
                    open: if tab.is_some() {
                        OpenState::Here
                    } else if thread_owner(&s.id).is_some() {
                        OpenState::Elsewhere
                    } else {
                        OpenState::Closed
                    },
                    archived: s.archived,
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
                activity: None,
                background: Vec::new(),
                open: OpenState::Here,
                archived: false,
            });
        }
        rows
    }

    /// The thread list changed in the store (a thread made, renamed, archived, deleted, or its
    /// events moved it): load it again, off the main thread, and redraw when it lands. Bursts
    /// coalesce into one more load. With no file store (memory, tests) it loads in place.
    pub(super) fn reload_summaries(&self) {
        if !store_is_async() {
            let list = app_store().list_threads(true).unwrap_or_else(|e| {
                warn!("Cannot list threads: {e}");
                Vec::new()
            });
            *self.summaries.borrow_mut() = Some(list);
            self.refresh_sidebar();
            return;
        }
        if self.summaries_loading.replace(true) {
            self.summaries_stale.set(true);
            return;
        }
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            loop {
                let loaded = store_job(|store| store.list_threads(true)).await;
                let Some(obj) = obj.upgrade() else { return };
                let imp = obj.imp();
                match loaded {
                    Some(Ok(list)) => *imp.summaries.borrow_mut() = Some(list),
                    Some(Err(e)) => {
                        warn!("Cannot list threads: {e}");
                        // Nothing is on its way: the skeleton rows hold still.
                        if let Some(sidebar) = imp.sidebar.borrow().as_ref() {
                            loading::settle(&sidebar.placeholder);
                        }
                    }
                    None => {}
                }
                if imp.summaries_stale.replace(false) {
                    continue;
                }
                imp.summaries_loading.set(false);
                imp.refresh_sidebar();
                return;
            }
        });
    }

    /// Runs a store write on the store writer (in order with the threads' own writes), logs a
    /// failure, then reloads the thread list.
    pub(super) fn write_store(
        &self,
        what: &'static str,
        job: impl FnOnce(&Store) -> agent_kit::store::Result<()> + Send + 'static,
    ) {
        self.write_store_then(what, job, |_, _| {});
    }

    /// [`Self::write_store`], then `answered` with the writer's receipt once it has one: what a
    /// caller shows only once the write has landed (or failed). `None`: the writer is gone or
    /// far behind (the write may still land; the list reloads if it does). A failure is logged
    /// and the thread list reloaded before `answered` runs, which it does only while the window
    /// is still there.
    pub(super) fn write_store_then<T: Send + 'static>(
        &self,
        what: &'static str,
        job: impl FnOnce(&Store) -> agent_kit::store::Result<T> + Send + 'static,
        answered: impl FnOnce(&Self, Option<agent_kit::store::Result<T>>) + 'static,
    ) {
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            let reload = obj.clone();
            let late = move || {
                if let Some(obj) = reload.upgrade() {
                    obj.imp().reload_summaries();
                }
            };
            let outcome = write_job_or_late(what, job, late).await;
            if let Some(Err(e)) = &outcome {
                warn!("Cannot {what}: {e}");
            }
            if let Some(obj) = obj.upgrade() {
                let imp = obj.imp();
                imp.reload_summaries();
                answered(imp, outcome);
            }
        });
    }

    /// A stored thread's summary: from the loaded list, else (a thread made a moment ago) a
    /// one-row read, after the store writer has committed what is queued (the thread's creation
    /// may still be on its way).
    pub(super) fn summary_of(&self, thread: &str) -> Option<agent_kit::store::ThreadSummary> {
        let cached = self
            .summaries
            .borrow()
            .as_ref()
            .and_then(|l| l.iter().find(|s| s.id == thread).cloned());
        cached.or_else(|| {
            app_handle()
                .read(|s| s.thread_summary(thread))
                .ok()
                .and_then(Result::ok)
                .flatten()
        })
    }

    /// Redraws the sidebar from what is loaded, with the live state of the open pages. Cheap: a
    /// few dozen rows, rebuilt on state changes. Nothing loaded yet: asks for the load.
    pub(super) fn refresh_sidebar(&self) {
        let started = std::time::Instant::now();
        self.redraw_sidebar();
        let calls = SIDEBAR_REFRESHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let rows = self
            .sidebar
            .borrow()
            .as_ref()
            .map_or(0, |s| s.drawn_rows.borrow().len());
        debug!(
            "sidebar refresh #{calls}: {rows} rows in {:.2} ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
    }

    /// Does the work of [`Self::refresh_sidebar`].
    fn redraw_sidebar(&self) {
        let Some(sidebar) = self.sidebar.borrow().clone() else {
            return;
        };
        if self.summaries.borrow().is_none() {
            // The load redraws when it lands (at once when there is no file store).
            self.reload_summaries();
            return;
        }
        if sidebar.loading.replace(false) {
            sidebar.list.set_placeholder(None::<&gtk4::Widget>);
            loading::reveal(&sidebar.list);
        }
        let rows = self.sidebar_rows();
        let selected = self.selected_row_key();
        let focused = self.obj().is_active();
        let query = sidebar.search.text().to_string();
        let (active, groups) = sidebar_sections(rows, &query, self.show_archived.get());
        let groups = fold_groups(
            groups,
            &sidebar.expanded.borrow(),
            !query.trim().is_empty(),
            selected.as_ref(),
        );
        let model = SidebarModel {
            active,
            groups,
            selected,
            focused,
            query,
        };
        let now = sidebar_now();
        // Widgets that already show these rows are kept: a redraw replaces nothing the user may
        // be holding (focus, a press, an open menu, a rename), and costs a few label writes.
        let redraw = redraw_for(sidebar.drawn.borrow().as_ref(), &model);
        if redraw != Redraw::Rebuild {
            self.update_sidebar_in_place(&sidebar, &model, redraw, now);
            return;
        }
        let SidebarModel {
            active,
            groups,
            selected,
            focused,
            ..
        } = model.clone();
        let mut drawn_rows: Vec<DrawnRow> = Vec::new();
        let home = env::var("HOME").unwrap_or_default();

        // A refresh replaces widgets, but it must not replace the keyboard user's place.
        // Remember the thread identity and trailing control before removing the old rows.
        let sidebar_focus = gtk4::prelude::GtkWindowExt::focus(&*self.obj())
            .and_then(|focus| sidebar_focus_target(&sidebar, &focus));

        sidebar.rebuilding.set(true);
        clear_rows(&sidebar.list);
        let mut keys = Vec::new();
        let mut select = None;
        // Live work first, in a section of its own: each row names its folder (there is no
        // folder header above it), and a full divider separates it from the rest.
        if !active.is_empty() {
            let header = gtk4::ListBoxRow::builder()
                .activatable(false)
                .selectable(false)
                .css_classes(["folder-row", "active-section"])
                .build();
            header.set_child(Some(
                &Label::builder()
                    .label("Active")
                    .xalign(0.0)
                    .css_classes(["folder-label", "caption-heading"])
                    .tooltip_text("Threads working, waiting for you, or rate limited")
                    .build(),
            ));
            sidebar.list.append(&header);
            keys.push(None);
            for row in active {
                let widget = self.sidebar_row_widget(&row, now);
                if let Some(line) = widget.child().and_then(|c| c.downcast::<Box>().ok()) {
                    let folder = Label::builder()
                        .label(crate::utils::tildify(&row.folder, &home))
                        .ellipsize(gtk4::pango::EllipsizeMode::Start)
                        .max_width_chars(16)
                        .css_classes(["thread-folder", "dim-label", "caption"])
                        .tooltip_text(&row.folder)
                        .build();
                    // After the title, before the badge and age.
                    if let Some(title) = line.first_child().and_then(|dot| dot.next_sibling()) {
                        line.insert_child_after(&folder, Some(&title));
                    }
                }
                if needs_attention(&row, selected.as_ref(), focused) {
                    widget.add_css_class("needs-attention");
                }
                sidebar.list.append(&widget);
                if Some(&row.key) == selected.as_ref() {
                    select = Some(widget.clone());
                }
                drawn_rows.push(DrawnRow::new(&widget));
                keys.push(Some(row.key));
            }
            if !groups.is_empty() {
                let divider = gtk4::ListBoxRow::builder()
                    .activatable(false)
                    .selectable(false)
                    .focusable(false)
                    .css_classes(["active-divider"])
                    .child(&gtk4::Separator::new(Orientation::Horizontal))
                    .build();
                sidebar.list.append(&divider);
                keys.push(None);
            }
        }
        for group in groups {
            sidebar
                .list
                .append(&self.folder_header(&sidebar, &group, &home));
            keys.push(None);
            for row in group.rows {
                let widget = self.sidebar_row_widget(&row, now);
                if needs_attention(&row, selected.as_ref(), focused) {
                    widget.add_css_class("needs-attention");
                }
                sidebar.list.append(&widget);
                if Some(&row.key) == selected.as_ref() {
                    select = Some(widget.clone());
                }
                drawn_rows.push(DrawnRow::new(&widget));
                keys.push(Some(row.key));
            }
        }
        *sidebar.keys.borrow_mut() = keys;
        *sidebar.drawn_rows.borrow_mut() = drawn_rows;
        *sidebar.drawn.borrow_mut() = Some(model);
        sidebar.list.select_row(select.as_ref());
        sidebar.rebuilding.set(false);
        if let Some((key, control)) = sidebar_focus {
            let index = sidebar
                .keys
                .borrow()
                .iter()
                .position(|k| k.as_ref() == Some(&key));
            let row = index
                .and_then(|i| i32::try_from(i).ok())
                .and_then(|i| sidebar.list.row_at_index(i));
            if let Some(row) = row {
                let target = control.and_then(|class| {
                    let line = row.child()?;
                    let mut child = line.first_child();
                    while let Some(widget) = child {
                        child = widget.next_sibling();
                        if widget.has_css_class(class) {
                            return Some(widget);
                        }
                    }
                    None
                });
                target.unwrap_or_else(|| row.upcast()).grab_focus();
            } else if let Some(row) = sidebar.list.selected_row() {
                row.grab_focus();
            } else {
                sidebar.search.grab_focus();
            }
        }
        self.update_sidebar_toggle_attention();
    }

    /// A folder group's header: a toggle that expands or collapses the group (with a count of
    /// what a collapsed one hides) and a button that starts a new thread in the folder.
    fn folder_header(
        &self,
        sidebar: &Rc<Sidebar>,
        group: &FolderGroup,
        home: &str,
    ) -> gtk4::ListBoxRow {
        let obj = self.obj();
        let folder = group.folder.clone();
        let header = gtk4::ListBoxRow::builder()
            .activatable(false)
            .selectable(false)
            .css_classes(["folder-row"])
            .build();
        let line = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(2)
            .build();
        let inner = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(4)
            .build();
        inner.append(&gtk4::Image::from_icon_name(if group.expanded {
            "at-pan-down-symbolic"
        } else {
            "at-pan-end-symbolic"
        }));
        inner.append(
            &Label::builder()
                .label(crate::utils::tildify(&folder, home))
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk4::pango::EllipsizeMode::Start)
                .css_classes(["folder-label", "caption-heading"])
                .build(),
        );
        if group.hidden > 0 {
            inner.append(
                &Label::builder()
                    .label(group.hidden.to_string())
                    .css_classes(["folder-count", "dim-label", "caption"])
                    .build(),
            );
        }
        let toggle = Button::builder()
            .child(&inner)
            .hexpand(true)
            .tooltip_text(&folder)
            .css_classes(["flat", "folder-toggle"])
            .build();
        toggle.update_state(&[gtk4::accessible::State::Expanded(Some(group.expanded))]);
        let weak_sidebar = Rc::downgrade(sidebar);
        toggle.connect_clicked(glib::clone!(
            #[weak]
            obj,
            #[strong]
            folder,
            move |_| {
                let Some(sidebar) = weak_sidebar.upgrade() else {
                    return;
                };
                {
                    let mut expanded = sidebar.expanded.borrow_mut();
                    if !expanded.remove(&folder) {
                        expanded.insert(folder.clone());
                    }
                }
                obj.imp().refresh_sidebar();
                // The rebuild replaced the header: keep the keyboard on the new one.
                let mut row = sidebar.list.first_child();
                while let Some(widget) = row {
                    row = widget.next_sibling();
                    if widget.widget_name() == folder.as_str() {
                        if let Some(toggle) = widget.first_child().and_then(|l| l.first_child()) {
                            toggle.grab_focus();
                        }
                        break;
                    }
                }
            }
        ));
        let new = Button::builder()
            .icon_name("at-list-add-symbolic")
            .tooltip_text("New Thread in This Folder")
            .valign(Align::Center)
            .css_classes(["flat", "folder-new-thread"])
            .build();
        new.connect_clicked(glib::clone!(
            #[weak]
            obj,
            #[strong]
            folder,
            move |_| obj.imp().new_chat_thread(None, Some(folder.clone()), None)
        ));
        line.append(&toggle);
        line.append(&new);
        header.set_widget_name(&folder);
        header.set_child(Some(&line));
        header
    }

    /// The cheap redraw: the rows are the ones on screen, so only what follows the clock or the
    /// window changes, written onto the existing widgets. Nothing is removed or re-added, so
    /// focus, the selection's scroll position, a press in progress, an open menu and a rename
    /// all stay as they are.
    fn update_sidebar_in_place(
        &self,
        sidebar: &Sidebar,
        model: &SidebarModel,
        redraw: Redraw,
        now: i64,
    ) {
        let widgets = sidebar.drawn_rows.borrow();
        if redraw == Redraw::Restyle {
            for (row, drawn) in model.rows().zip(widgets.iter()) {
                if needs_attention(row, model.selected.as_ref(), model.focused) {
                    drawn.row.add_css_class("needs-attention");
                } else {
                    drawn.row.remove_css_class("needs-attention");
                }
            }
            let selected = model
                .rows()
                .zip(widgets.iter())
                .find(|(row, _)| Some(&row.key) == model.selected.as_ref())
                .map(|(_, drawn)| drawn.row.clone());
            if sidebar.list.selected_row() != selected {
                // Selecting is not a click on the row.
                sidebar.rebuilding.set(true);
                sidebar.list.select_row(selected.as_ref());
                sidebar.rebuilding.set(false);
            }
        }
        for (row, drawn) in model.rows().zip(widgets.iter()) {
            let age = relative_time(now, row.updated_ms);
            if drawn.age.label() != age {
                drawn.age.set_label(&age);
            }
        }
        drop(widgets);
        if let Some(drawn) = sidebar.drawn.borrow_mut().as_mut() {
            drawn.selected = model.selected.clone();
            drawn.focused = model.focused;
        }
        self.update_sidebar_toggle_attention();
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
                let dot = Image::from_gicon(&crate::icons::driver_icon(driver));
                dot.set_pixel_size(14);
                dot.add_css_class("thread-dot");
                dot.add_css_class(&format!("dot-{}", driver_key(driver)));
                dot.set_tooltip_text(Some(driver_label(driver)));
                line.append(&dot);
            }
            None => {
                let icon = Image::from_icon_name("at-utilities-terminal-symbolic");
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
            .css_classes(if row.open != OpenState::Closed {
                vec!["thread-title", "open"]
            } else {
                vec!["thread-title"]
            })
            .tooltip_text(&row.title)
            .build();
        line.append(&title);
        if row.open == OpenState::Elsewhere {
            let mark = Label::builder()
                .label("↗")
                .tooltip_text(row.open.description())
                .build();
            line.append(&mark);
        }
        let spins = row
            .badge
            .is_some_and(|b| matches!(b, Badge::Running | Badge::Background));
        match row.activity {
            // The agent is starting: the row spins, unless its badge already does.
            Some(ThreadActivity::Starting) if !spins => {
                let spinner = gtk4::Spinner::builder().spinning(true).build();
                spinner.add_css_class("thread-starting");
                spinner.set_tooltip_text(Some(ThreadActivity::Starting.tooltip()));
                line.append(&spinner);
            }
            Some(ThreadActivity::Paused) => {
                let paused = Label::builder()
                    .label("Paused")
                    .css_classes(["thread-paused", "caption"])
                    .tooltip_text(ThreadActivity::Paused.tooltip())
                    .build();
                line.append(&paused);
            }
            _ => {}
        }
        if let Some(badge) = row.badge {
            // Spinning means not finished: the main agent working, or background work (sub-agents,
            // background commands) it is still waiting on, which spins in its own colour.
            if matches!(badge, Badge::Running | Badge::Background) {
                let spinner = gtk4::Spinner::builder().spinning(true).build();
                if badge == Badge::Background {
                    spinner.add_css_class("thread-background");
                }
                spinner.set_tooltip_text(Some(&row_tooltip(badge, &row.background)));
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
        // One trailing button per row: an open row closes, a closed one deletes (shown on hover).
        // An open thread is deleted from its menu or with the Delete key.
        if let (RowKey::Thread(thread_id), OpenState::Closed) = (&row.key, row.open) {
            let delete = Button::builder()
                .icon_name("at-user-trash-symbolic")
                .tooltip_text("Delete thread…")
                .css_classes(["flat", "circular", "thread-delete"])
                .valign(Align::Center)
                .build();
            let thread = thread_id.clone();
            delete.connect_clicked(glib::clone!(
                #[weak]
                obj,
                move |_| obj.imp().confirm_delete(&thread)
            ));
            line.append(&delete);
        }
        if row.open == OpenState::Here {
            let close = Button::builder()
                .icon_name("at-window-close-symbolic")
                .tooltip_text("Close (the thread stays in the list; a running turn is stopped)")
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
        let widget = gtk4::ListBoxRow::builder().child(&line).build();
        let mut state = row
            .badge
            .map(|b| row_tooltip(b, &row.background))
            .unwrap_or_else(|| row.open.description().to_owned());
        if let Some(activity) = row
            .activity
            .filter(|a| matches!(a, ThreadActivity::Starting | ThreadActivity::Paused))
        {
            state = format!("{state}, {}", activity.tooltip());
        }
        widget.update_property(&[
            gtk4::accessible::Property::Label(&row.title),
            gtk4::accessible::Property::Description(&format!("{}, {state}", row.folder)),
        ]);
        widget
    }

    /// Whether closing `key` would cut a turn short: a thread running or awaiting an answer.
    pub(super) fn row_is_busy(&self, key: &RowKey) -> bool {
        let RowKey::Thread(id) = key else {
            return false;
        };
        self.tabs
            .borrow()
            .iter()
            .filter_map(|t| t.chat.as_ref())
            .any(|c| &c.thread == id && c.busy())
    }

    /// What closing `key`'s page would lose ([`ChatTab::closing_loses`]); `None` for a terminal
    /// page or a thread with nothing to lose.
    pub(super) fn row_close_reasons(&self, key: &RowKey) -> Option<CloseReasons> {
        let RowKey::Thread(id) = key else {
            return None;
        };
        self.tabs
            .borrow()
            .iter()
            .filter_map(|t| t.chat.as_ref())
            .find(|c| &c.thread == id)
            .and_then(ChatTab::closing_loses)
    }

    /// On a window close request: when any thread is busy or holds an unsent message, asks once
    /// and returns true (the close is held back). "Stop and Close" lets the next close request
    /// through, which stops every session; Cancel leaves the window open. False when nothing
    /// would be lost.
    pub(super) fn confirm_close_busy(&self) -> bool {
        let threads: Vec<(String, CloseReasons)> = self
            .tabs
            .borrow()
            .iter()
            .filter_map(|t| t.chat.as_ref())
            .filter_map(|c| Some((c.title.clone(), c.closing_loses()?)))
            .collect();
        let Some((heading, body, confirm)) = window_close_question(&threads) else {
            return false;
        };
        if self.close_prompt_open.replace(true) {
            return true; // one question at a time
        }
        let dialog = adw::AlertDialog::new(Some(&heading), Some(&body));
        dialog.add_responses(&[("cancel", "Cancel"), ("stop", confirm)]);
        dialog.set_response_appearance("stop", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            let Some(window) = obj.upgrade() else { return };
            let response = dialog
                .choose_future(Some(window.upcast_ref::<gtk4::Widget>()))
                .await;
            let imp = window.imp();
            imp.close_prompt_open.set(false);
            if response == "stop" {
                imp.close_confirmed.set(true);
                window.close();
                // Still here: the close was refused somewhere else. Ask again next time.
                imp.close_confirmed.set(false);
            }
        });
        true
    }

    /// Runs `then` once the user agrees to lose what `reasons` says closing `thread` loses (its
    /// running turn, its unsent message: closing or archiving ends its session and its page).
    /// Cancelling does nothing.
    pub(super) fn confirm_stop_then(
        &self,
        thread: &str,
        reasons: CloseReasons,
        then: impl FnOnce(&Self) + 'static,
    ) {
        // At most one question per thread: a second click on X while it is open does nothing.
        let first = self
            .tabs
            .borrow_mut()
            .iter_mut()
            .filter_map(|t| t.chat.as_mut())
            .find(|c| c.thread == thread)
            .is_some_and(|c| !std::mem::replace(&mut c.confirming_close, true));
        if !first {
            return;
        }
        let (heading, body, confirm) = close_question(&self.thread_title_of(thread), reasons);
        let dialog = adw::AlertDialog::new(Some(&heading), Some(&body));
        dialog.add_responses(&[("cancel", "Cancel"), ("stop", confirm)]);
        dialog.set_response_appearance("stop", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let (obj, thread) = (self.obj().downgrade(), thread.to_owned());
        glib::MainContext::default().spawn_local(async move {
            let Some(window) = obj.upgrade() else { return };
            let response = dialog
                .choose_future(Some(window.upcast_ref::<gtk4::Widget>()))
                .await;
            // Reopened for a later close; the tab may be gone already.
            if let Some(c) = window
                .imp()
                .tabs
                .borrow_mut()
                .iter_mut()
                .filter_map(|t| t.chat.as_mut())
                .find(|c| c.thread == thread)
            {
                c.confirming_close = false;
            }
            if response == "stop" {
                then(window.imp());
            }
        });
    }

    /// Whether changing this thread's agent would cut something short: a turn, an approval, a
    /// background task, or any of its sub-agents, working or idle (every one ends).
    pub(super) fn switch_would_stop_work(&self, thread: &str) -> bool {
        self.row_is_busy(&RowKey::Thread(thread.to_owned()))
            || crate::delegation::supervisor().is_some_and(|s| s.root_has_live_children(thread))
    }

    /// Moves `thread` to `driver`. When that would cut work short, asks first: Stop and Switch
    /// interrupts the turn and ends every sub-agent of this thread (no other thread's), then
    /// switches; Cancel changes nothing. With nothing running it switches at once.
    pub(super) fn switch_agent_confirmed(
        &self,
        thread: &str,
        driver: Driver,
        model: Option<String>,
        effort: Option<String>,
    ) {
        let Some(slot) = self.slot_of(thread) else {
            return;
        };
        if !self.switch_would_stop_work(thread) {
            delegation::switch(&slot, driver, model, effort);
            return;
        }
        // At most one question per thread: another pick while it is open does nothing.
        let first = self
            .tabs
            .borrow_mut()
            .iter_mut()
            .filter_map(|t| t.chat.as_mut())
            .find(|c| c.thread == thread)
            .is_some_and(|c| !std::mem::replace(&mut c.confirming_switch, true));
        if !first {
            return;
        }
        let (heading, body) = switch_warning(&self.thread_title_of(thread), driver);
        let dialog = adw::AlertDialog::new(Some(&heading), Some(&body));
        dialog.add_responses(&[("cancel", "Cancel"), ("stop", "Stop and Switch")]);
        dialog.set_response_appearance("stop", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let (obj, thread) = (self.obj().downgrade(), thread.to_owned());
        glib::MainContext::default().spawn_local(async move {
            let Some(window) = obj.upgrade() else { return };
            let response = dialog
                .choose_future(Some(window.upcast_ref::<gtk4::Widget>()))
                .await;
            if let Some(c) = window
                .imp()
                .tabs
                .borrow_mut()
                .iter_mut()
                .filter_map(|t| t.chat.as_mut())
                .find(|c| c.thread == thread)
            {
                c.confirming_switch = false;
            }
            if response != "stop" {
                return;
            }
            // The slot again: the page may have closed while the question was open.
            if let Some(slot) = window.imp().slot_of(&thread) {
                slot.interrupt();
                delegation::switch(&slot, driver, model, effort);
            }
        });
    }

    /// Closes the row's page, asking first when that would cut a turn short.
    pub(super) fn close_row(&self, key: &RowKey) {
        if let RowKey::Thread(thread) = key {
            if let Some(owner) = thread_owner(thread).filter(|w| w != &*self.obj()) {
                owner.imp().close_row(key);
                return;
            }
        }
        if let (Some(reasons), RowKey::Thread(thread)) = (self.row_close_reasons(key), key) {
            let key = key.clone();
            self.confirm_stop_then(thread, reasons, move |imp| imp.close_row_now(&key));
        } else {
            self.close_row_now(key);
        }
    }

    /// Closes the row's page with no question (a thread being deleted was already confirmed).
    pub(super) fn close_row_now(&self, key: &RowKey) {
        if matches!(key, RowKey::Thread(_)) {
            self.for_each_window(|window| window.close_local_row_now(key));
        } else {
            self.close_local_row_now(key);
        }
    }

    fn close_local_row_now(&self, key: &RowKey) {
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

    pub(super) fn selected_row_key(&self) -> Option<RowKey> {
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
        // First, so a page is visible before anything below builds or focuses it.
        wire_page_visibility(tab_view);
        tab_view.connect_selected_page_notify(glib::clone!(
            #[weak]
            obj,
            move |view| {
                let imp = obj.imp();
                imp.leave_shown_thread(view.selected_page().as_ref());
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
                    imp.settle_pages();
                    if view.n_pages() == 0 {
                        imp.sync_diff_toggle();
                        if let Some(title) = imp.window_title.borrow().as_ref() {
                            title.set_title("Agent Terminal");
                            title.set_subtitle("");
                            title.set_tooltip_text(None);
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
                imp.for_each_window(|window| window.refresh_sidebar());
            }
        ));
    }

    /// Another page (or none) came into view: the thread page that was shown, left blank, is
    /// closed, and its page's detach discards it ([`Self::discard_if_blank`]).
    fn leave_shown_thread(&self, now: Option<&adw::TabPage>) {
        let now_thread = now.and_then(|page| {
            self.tabs
                .borrow()
                .iter()
                .find(|t| &t.page == page)
                .and_then(|t| t.chat.as_ref().map(|c| c.thread.clone()))
        });
        let Some(left) = self
            .shown_thread
            .replace(now_thread.clone())
            .filter(|t| Some(t) != now_thread.as_ref())
        else {
            return;
        };
        // Idle suspension counts from the moment it went out of sight. A shared borrow: this
        // runs inside the selection change, which a caller holding the registry can trigger.
        if let Some(chat) = self
            .tabs
            .borrow()
            .iter()
            .filter_map(|t| t.chat.as_ref())
            .find(|c| c.thread == left)
        {
            chat.last_seen.set(std::time::Instant::now());
        }
        if !self.thread_page_looks_blank(&left) {
            return;
        }
        // Not from inside the selection change: once it has settled, and only if still blank
        // and still not in view.
        let obj = self.obj().downgrade();
        glib::idle_add_local_once(move || {
            let Some(obj) = obj.upgrade() else { return };
            let imp = obj.imp();
            let key = RowKey::Thread(left.clone());
            if imp.selected_row_key().as_ref() != Some(&key) && imp.thread_page_looks_blank(&left) {
                debug!("Closing blank thread {left}");
                imp.close_local_row_now(&key);
            }
        });
    }

    /// The thread has a page in this window that [`ChatTab::looks_blank`] and runs nothing.
    fn thread_page_looks_blank(&self, thread: &str) -> bool {
        // Pages reopened from the last session change the selection without the user leaving.
        if self.is_closing() || self.restoring_threads.get() {
            return false;
        }
        let blank = self
            .tabs
            .borrow()
            .iter()
            .filter_map(|t| t.chat.as_ref())
            .any(|c| c.thread == thread && c.looks_blank());
        blank && !self.row_is_busy(&RowKey::Thread(thread.to_owned()))
    }

    /// A thread page that looked blank was closed: deletes the thread if the store agrees it
    /// holds nothing (checked behind every write already queued for it). Once it is deleted, a
    /// toast offers Undo, which creates the thread again as it was (see [`DiscardedThread`]).
    pub(super) fn discard_if_blank(&self, tab: &TabState) {
        let Some(chat) = tab.chat.as_ref() else {
            return;
        };
        if self.is_closing()
            || !chat.looks_blank()
            || NO_DISCARD.with(|d| d.borrow().contains(&chat.thread))
        {
            return;
        }
        let (spec, live) = discarded_thread(tab, chat);
        let id = chat.thread.clone();
        let thread = id.clone();
        self.write_store_then(
            "discard a blank thread",
            // What it was stored with, once deleted; None when the store found something in it.
            move |s| {
                let stored = StoredChoices {
                    model: s.thread_summary(&id)?.and_then(|t| t.model),
                    attributes: s.thread_attributes(&id)?,
                    mode: crate::chat::session::chosen_mode(
                        s.events(&id, None, REPLAY_LIMIT)?.iter().map(|(_, e)| e),
                    ),
                };
                Ok(s.delete_if_blank(&id)?.then_some(stored))
            },
            move |imp, outcome| {
                if let Some(Ok(Some(stored))) = outcome {
                    info!("Discarded blank thread {thread}");
                    imp.offer_undo_discard(spec.with_stored(stored, live));
                }
            },
        );
    }

    /// Closes `thread`'s page, in whichever window has it, for a delete or an archive: that
    /// write decides the thread's fate, so its page's detach discards nothing (a discard racing
    /// an archive could delete the thread archived) and never offers to undo a discard.
    pub(super) fn close_row_without_discard(&self, thread: &str) {
        NO_DISCARD.with(|d| d.borrow_mut().insert(thread.to_owned()));
        // The page closes (and detaches) in place: `close-page` finishes at once.
        self.close_row_now(&RowKey::Thread(thread.to_owned()));
        NO_DISCARD.with(|d| d.borrow_mut().remove(thread));
    }

    /// The "Empty thread discarded" toast, whose Undo creates the thread again from `spec`.
    fn offer_undo_discard(&self, spec: DiscardedThread) {
        let toast = adw::Toast::builder()
            .title("Empty thread discarded")
            .use_markup(false)
            .button_label("Undo")
            .timeout(10)
            .build();
        let obj = self.obj().downgrade();
        toast.connect_button_clicked(move |_| {
            if let Some(window) = obj.upgrade() {
                window.imp().undo_discard(&spec);
            }
        });
        self.add_toast(toast);
    }

    /// Creates a discarded blank thread again, through the ordinary creation path: a folder that
    /// is gone is refused there, and a failed write says so there.
    pub(super) fn undo_discard(&self, spec: &DiscardedThread) -> Option<adw::TabPage> {
        info!("Recreating a discarded blank thread");
        self.create_chat_thread(spec.new_thread())
    }

    /// Shows the open pages, or the empty state when there are none. While the last session's
    /// threads are still being reopened, nothing open yet keeps the loading skeleton.
    fn settle_pages(&self) {
        let open = self
            .tab_view
            .borrow()
            .as_ref()
            .is_some_and(|v| v.n_pages() > 0);
        let restoring = self.restoring_threads.get();
        if let Some(stack) = self.pages_stack.borrow().as_ref() {
            let name = match (open, restoring) {
                (true, _) => "threads",
                (false, true) => "loading",
                (false, false) => "empty",
            };
            stack.set_visible_child_name(name);
            if !restoring {
                if let Some(skeleton) = stack.child_by_name("loading") {
                    // After the crossfade away from it, so it fades rather than vanishes.
                    let stack = stack.downgrade();
                    glib::timeout_add_local_once(
                        std::time::Duration::from_millis(u64::from(loading::CROSSFADE_MS) + 50),
                        move || {
                            // Settling again before this ran queued a second removal.
                            if let Some(stack) = stack
                                .upgrade()
                                .filter(|s| skeleton.parent().as_ref() == Some(s.upcast_ref()))
                            {
                                stack.remove(&skeleton);
                            }
                        },
                    );
                }
            }
        }
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
                chat.last_seen.set(std::time::Instant::now());
                (chat.thread.clone(), chat.title.clone(), dir)
            })
        };
        // Also here: a first page is selected before it is registered, then shown directly.
        *self.shown_thread.borrow_mut() = info.as_ref().map(|(thread, ..)| thread.clone());
        self.refresh_repo_presence(page);
        let Some((thread, title, dir)) = info else {
            // A terminal page: the 2.x handler puts its terminal title in the subtitle.
            if let Some(window_title) = self.window_title.borrow().as_ref() {
                window_title.set_title("Terminal");
                window_title.set_tooltip_text(None);
            }
            return;
        };
        let id = thread.clone();
        self.write_store("mark the thread read", move |s| s.mark_read(&id));
        // Shown in a focused window means seen; an unfocused one is withdrawn on focus.
        if self.obj().is_active() {
            self.withdraw_thread_notifications(&thread);
        }
        let home = env::var("HOME").unwrap_or_default();
        if let Some(window_title) = self.window_title.borrow().as_ref() {
            window_title.set_title(if title.is_empty() {
                "New thread"
            } else {
                &title
            });
            window_title.set_subtitle(&crate::utils::tildify(&dir, &home));
            window_title.set_tooltip_text(None);
        }
        // The branch joins the folder once read (in a repository on GitHub).
        self.refresh_branch(page);
        self.ensure_built(page);
    }

    /// Records the open threads (and the one in view) so the next launch reopens them.
    pub(super) fn save_open_threads(&self) {
        // Selection and detach callbacks during teardown must not shrink the closing snapshot.
        if self.is_closing() {
            return;
        }
        self.record_open_threads();
    }

    /// The close path's one snapshot, taken before agents stop or pages are detached.
    pub(super) fn save_open_threads_at_close(&self) {
        self.record_open_threads();
    }

    fn record_open_threads(&self) {
        // Nothing to record before the shell exists or the previous layout has been restored.
        if self.tab_view.borrow().is_none() || !CHAT_RESTORED.with(std::cell::Cell::get) {
            return;
        }
        let windows = RefCell::new(Vec::new());
        let focused = RefCell::new(None);
        let any_live_window = std::cell::Cell::new(false);
        self.for_each_window(|window| {
            if !window.is_closing() {
                any_live_window.set(true);
            }
        });
        self.for_each_window(|window| {
            let selected = match window.selected_row_key() {
                Some(RowKey::Thread(id)) => Some(id),
                _ => None,
            };
            let open: Vec<String> = window
                .tabs
                .borrow()
                .iter()
                .filter_map(|t| t.chat.as_ref().map(|c| c.thread.clone()))
                .collect();
            if let Some(layout) = window_layout(
                window.is_closing(),
                any_live_window.get(),
                open,
                selected.clone(),
            ) {
                if window.obj().is_active() {
                    *focused.borrow_mut() = Some(selected);
                }
                windows.borrow_mut().push(layout);
            }
        });
        let selected = focused
            .into_inner()
            .unwrap_or_else(|| match self.selected_row_key() {
                Some(RowKey::Thread(id)) => Some(id),
                _ => None,
            });
        let value = saved_open_threads(&windows.into_inner(), selected).to_string();
        OPEN_LAYOUT_WRITES.with(|writes| {
            if let Some(writer) = store_writer() {
                if let Err(e) = queue_open_layout(&writer, writes, value) {
                    warn!("Cannot record the open threads: {e}");
                }
            } else {
                writes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .offer(value);
                if let Err(e) = app_store().atomically(|store| write_latest_layout(writes, store)) {
                    warn!("Cannot record the open threads: {e}");
                }
            }
        });
    }

    /// Reopens the threads open when the app last closed (first window of a launch only, and
    /// only with `reopen_last_thread`). Only the one in view is built now; the rest are built
    /// when first shown.
    ///
    /// The stored list is read off the main thread, so the reopening lands a moment later. The
    /// saved selection is then applied only when nothing else has been put in view meanwhile (a
    /// resume the window was opened for).
    pub(super) fn restore_open_threads(&self) {
        if CHAT_RESTORED.with(|done| done.replace(true)) || try_app_store().is_err() {
            return;
        }
        self.list_recent_sessions();
        let obj = self.obj().downgrade();
        let before = self.thread_pages();
        glib::MainContext::default().spawn_local(async move {
            let loaded = store_job(|store| {
                (
                    store.meta(OPEN_THREADS_KEY).ok().flatten(),
                    store.list_threads(false).unwrap_or_default(),
                )
            })
            .await;
            let Some(obj) = obj.upgrade() else { return };
            let imp = obj.imp();
            if let Some((saved, threads)) = loaded {
                // With "Reopen Last Thread" off, the empty state offers what it would reopen.
                if !imp.config.borrow().reopen_last_thread {
                    let known: Vec<String> = threads.iter().map(|s| s.id.clone()).collect();
                    let count = last_session_count(saved.as_deref(), &known);
                    if let Some(saved) = saved.clone().filter(|_| count > 0) {
                        LAST_SESSION.with(|s| *s.borrow_mut() = Some(LastSession { saved, count }));
                    }
                    imp.for_each_window(|window| window.update_reopen_offer());
                }
                imp.reopen_saved(saved, threads, before, false);
            }
            // Whatever reopened (or nothing did): the loading skeleton gives way.
            imp.restoring_threads.set(false);
            imp.settle_pages();
        });
    }

    /// How many thread pages are open.
    fn thread_pages(&self) -> usize {
        self.tabs
            .borrow()
            .iter()
            .filter(|t| t.chat.is_some())
            .count()
    }

    /// Reopens the saved open-thread list `saved` against the stored `threads`. `before` is how
    /// many thread pages were open when the restore began. `asked`: the user asked for it (the
    /// empty state's offer), so "Reopen Last Thread" being off does not stop it. Whether any
    /// thread was reopened.
    fn reopen_saved(
        &self,
        saved: Option<String>,
        threads: Vec<agent_kit::store::ThreadSummary>,
        before: usize,
        asked: bool,
    ) -> bool {
        // Off (the default), the window starts on no thread; the list is still recorded, so
        // turning it on later picks up where the last run left off.
        if !asked && !self.config.borrow().reopen_last_thread {
            return false;
        }
        let Some(value) = saved.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        else {
            return false;
        };
        let known: Vec<String> = threads.into_iter().map(|s| s.id).collect();
        let open = restorable(&value, &known, crate::config::SessionState::MAX_TABS);
        if open.is_empty() {
            return false;
        }
        // Something else put in view since the restore began (a resume, a thread the user
        // opened) stays there. The 2.x terminal pages, which reopen once detection is done,
        // do not count: the saved thread is shown over them, as it always was. Asked for, the
        // reopened selection is what the user wants to see.
        let others_in_view = !asked && (self.resume_opened.get() || self.thread_pages() > before);
        info!("Reopening {} thread(s) from the last session", open.len());
        for id in &open {
            self.open_thread(id, false);
        }
        if others_in_view {
            return true;
        }
        if let Some(id) = selected_to_restore(&value, &open) {
            if self.open_thread(&id, true).is_some() && !asked {
                self.thread_selected_by_restore.set(true);
            }
        }
        true
    }

    /// Lists the agents' own recent sessions (Claude Code and agy, newest [`RECENT_SESSIONS`])
    /// as threads in the sidebar: once per run at startup, and when "Show Sessions Started
    /// Outside Agent Terminal" is turned on. The scan reads only file metadata and the tail of
    /// each transcript, off the main thread; a session's history is imported when opened. Any
    /// added are announced with a toast. With the setting off, nothing is scanned.
    fn list_recent_sessions(&self) {
        if !self.config.borrow().list_outside_sessions {
            return;
        }
        // Tests never read the real home's agent histories (`link_recent_sessions` is tested on
        // its own).
        // Nor does the memory-only fallback store: its jobs run in place on the main thread,
        // and what it would list is gone at exit anyway.
        if cfg!(test) || !store_is_async() {
            return;
        }
        let Some(home) = env::var_os("HOME").map(std::path::PathBuf::from) else {
            return;
        };
        let obj = self.obj().downgrade();
        let scan = begin_session_scan();
        glib::MainContext::default().spawn_local(async move {
            // The scan reads the agents' files on a worker; only linking runs on the writer.
            let scan_home = home.clone();
            let sessions = gtk4::gio::spawn_blocking(move || {
                agent_kit::native::recent_native_sessions(&scan_home, RECENT_SESSIONS)
            })
            .await
            .unwrap_or_default();
            // Turned off (or turned off and on again: a newer scan) while the files were read:
            // nothing is linked or announced.
            let enabled = obj
                .upgrade()
                .is_some_and(|o| o.imp().config.borrow().list_outside_sessions);
            if !session_scan_wanted(scan, enabled) {
                info!("A scan of recent agent sessions was dropped: the setting changed");
                return;
            }
            let added = write_job("list recent sessions as threads", move |store| {
                // The setting may have changed while this job waited behind other writes.
                if !session_scan_wanted(scan, true) {
                    return Ok(Vec::new());
                }
                Ok(link_recent_sessions(
                    store,
                    &sessions,
                    &home.to_string_lossy(),
                ))
            })
            .await
            .and_then(Result::ok)
            .unwrap_or_default();
            if let Some(toast) = sessions_added_toast(&added) {
                info!("Listed {} recent agent session(s) as threads", added.len());
                // New rows in the store: the list loaded meanwhile does not have them.
                if let Some(obj) = obj.upgrade() {
                    let imp = obj.imp();
                    imp.reload_summaries();
                    imp.show_toast(&toast);
                }
            }
        });
    }

    /// Settings → Behaviour → "Show Sessions Started Outside Agent Terminal". Applies at once:
    /// on, the scan runs now; off, it never runs (threads already listed stay).
    pub(super) fn set_list_outside_sessions(&self, on: bool) {
        if self.config.borrow().list_outside_sessions == on {
            return;
        }
        self.config.borrow_mut().list_outside_sessions = on;
        self.schedule_config_save();
        if on {
            self.list_recent_sessions();
        } else {
            // A scan still reading files must not link what it finds.
            cancel_session_scans();
        }
    }

    /// Shows the thread's page, adding it if it is not open. `select` brings it into view (and
    /// so builds it).
    pub(super) fn open_thread(&self, thread: &str, select: bool) -> Option<adw::TabPage> {
        if let Some(owner) = thread_owner(thread).filter(|w| w != &*self.obj()) {
            if owner.imp().is_closing() {
                self.show_toast("This thread is closing in another window");
                return None;
            }
            let page = owner.imp().open_thread(thread, true);
            owner.present();
            return page;
        }
        if self.is_closing() {
            return None;
        }
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

    pub(super) fn register_thread(&self, thread: &str) {
        OPEN_THREADS.with(|registry| {
            registry
                .borrow_mut()
                .insert(thread.to_owned(), self.obj().downgrade());
        });
    }

    pub(super) fn unregister_thread(&self, thread: &str) {
        OPEN_THREADS.with(|registry| {
            let mut registry = registry.borrow_mut();
            if registry
                .get(thread)
                .and_then(glib::WeakRef::upgrade)
                .as_ref()
                == Some(&*self.obj())
            {
                registry.remove(thread);
            }
        });
    }

    fn session_start_allowed(&self) -> bool {
        !self.is_closing()
    }

    /// Adds an (unbuilt) page for a stored thread.
    fn add_thread_page(&self, thread: &str) -> Option<adw::TabPage> {
        let summary = self.summary_of(thread)?;
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
        let stack = Stack::builder()
            .vexpand(true)
            .transition_type(gtk4::StackTransitionType::Crossfade)
            .transition_duration(loading::CROSSFADE_MS)
            .build();
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
        // Its only child until `build_thread` puts the view in its place. Still until the page is
        // first shown: nothing loads before then (`ensure_built` starts the pulse). An infinite
        // CSS animation anywhere in the window makes GTK walk every style node of the window on
        // every frame, so each restored thread not yet looked at kept the whole window ticking
        // (profiled: 31% of an idle main thread in `gtk_css_node_invalidate_timestamp`). Not
        // left to `show_only_selected` hiding the page: a pulse with nothing loading should not
        // run at all, and the walk it triggers covers the whole window, visible or not.
        let skeleton = loading::chat();
        loading::settle(&skeleton);
        holder.append(&skeleton);
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
        // No fixed sizes: the thread needs only what its view needs, and the diff panel narrows
        // (rather than pushing the thread out of the window) when there is no room for it.
        let paned = gtk4::Paned::builder()
            .orientation(Orientation::Horizontal)
            .start_child(&drawer_paned)
            .end_child(&diff_panel.root)
            .resize_start_child(true)
            .resize_end_child(false)
            // The chat's transcript and composer scroll internally. Let the pane itself shrink
            // at the window minimum rather than setting a larger minimum for the whole window.
            .shrink_start_child(true)
            .shrink_end_child(true)
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
        // Another banner for a model its agent no longer offers: "Choose…" opens the picker.
        let model_banner = adw::Banner::builder()
            .use_markup(false)
            .button_label("Choose…")
            .build();
        let thread_id = thread.to_owned();
        model_banner.connect_button_clicked(move |b| {
            if let Some(obj) = window_of(b) {
                obj.imp().choose_model(&thread_id);
            }
        });
        // And one for an agent nobody is signed in to: "Sign In…" opens its login in a terminal
        // tab, then "Check Again" looks once more.
        let sign_in_banner = adw::Banner::builder().use_markup(false).build();
        let thread_id = thread.to_owned();
        sign_in_banner.connect_button_clicked(move |b| {
            if let Some(obj) = window_of(b) {
                obj.imp().sign_in_from_thread(&thread_id);
            }
        });
        let content = Box::builder().orientation(Orientation::Vertical).build();
        content.append(&sign_in_banner);
        content.append(&rate_banner);
        content.append(&model_banner);
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
            thread: Some(summary.id.clone()),
            switch_serial: Rc::new(std::cell::Cell::new(0)),
            start_attempt: std::cell::Cell::new(0),
            worker_subscriber: RefCell::new(None),
            worker_diffs: RefCell::new(HashMap::new()),
            session: RefCell::new(None),
            view: RefCell::new(None),
            driver: std::cell::Cell::new(driver),
            model: None,
            mode,
        });
        let key = next_tab_key();
        let diffs = {
            let holder = holder.downgrade();
            ThreadDiffs::new(
                &summary.cwd,
                key,
                driver_label(driver),
                Rc::new(move |text: &str| {
                    if let Some(window) = holder.upgrade().as_ref().and_then(window_of) {
                        window.imp().show_toast(text);
                    }
                }),
            )
        };
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
            key,
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
                model_banner,
                sign_in_banner,
                running: false,
                background: Vec::new(),
                finish_held: false,
                approval: false,
                workers_active: 0,
                child_asks: ChildAsks::default(),
                rate_limited: false,
                // A restored thread that was never shown keeps the store's unread mark. Same
                // rule as a closed row in `sidebar_rows`: an empty thread (updated_at 0) has
                // nothing to read.
                unread: summary.unread && summary.updated_at > 0,
                confirming_close: false,
                confirming_switch: false,
                title_item: None,
                pending_prompt: None,
                pending_handoff: false,
                profile_id: None,
                stored_handoff: None,
                chosen_mode: None,
                pending_effort: None,
                pending_switch: None,
                rebinding: false,
                unavailable: false,
                starting: false,
                diffs,
                last_seen: std::cell::Cell::new(std::time::Instant::now()),
            }),
            discovered_repo: None,
            in_repo: None,
        });
        if diff_panel.root.is_visible() {
            self.refresh_diff(&page);
        }
        self.register_thread(thread);
        self.save_open_threads();
        self.update_sign_in_banners();
        self.for_each_window(|window| window.refresh_sidebar());
        Some(page)
    }

    /// Builds a thread's view and starts its session, the first time its page is shown.
    fn ensure_built(&self, page: &adw::TabPage) {
        if self.is_closing() {
            return;
        }
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
                    // Selecting a thread shows its newest messages, wherever it was left.
                    glib::idle_add_local_once(move || {
                        view.scroll_to_end();
                        view.focus_composer();
                    });
                }
                return;
            }
            // An in-place switch is being written: the build reads the thread once it has
            // landed (`finish_rebind` builds it then), never the agent it is leaving.
            if chat.rebinding {
                return;
            }
            chat.building = true;
            // A retry after a failed read: the skeleton pulses again.
            if let Some(skeleton) = chat.holder.first_child() {
                loading::unsettle(&skeleton);
            }
            (chat.thread.clone(), chat.driver, dir)
        };
        let (thread, driver, dir) = job;
        // Its agent is on its way: the row spins.
        self.refresh_sidebar();
        let obj = self.obj();
        let page = page.clone();
        let hook_clear = self.config.borrow().clear_env.clone();
        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            obj,
            async move {
                // A long thread's history is read (and decoded) off the main thread; only the
                // replay into the view happens here. Its profile and worktree come in the same
                // read, behind the writer's barrier, so the profile is chosen after it.
                let loaded = store_job({
                    let thread = thread.clone();
                    // Importing reads the agent's transcript; only where store jobs really run
                    // off the main thread (never with the memory-only fallback, or under test).
                    let home = (store_is_async() && !cfg!(test))
                        .then(|| env::var_os("HOME").map(std::path::PathBuf::from))
                        .flatten();
                    let writer = store_writer();
                    move |store| load_thread_with(store, &thread, home.as_deref(), writer.as_ref())
                })
                .await;
                if !obj.imp().session_start_allowed() {
                    return;
                }
                // A thread that cannot be read is not built (an empty one would start a fresh
                // session and re-point the thread at it); selecting it again retries.
                let loaded = match loaded {
                    Some(Ok(loaded)) => loaded,
                    failed => {
                        if let Some(Err(e)) = failed {
                            warn!("Cannot read thread {thread}: {e}");
                        }
                        obj.imp().thread_load_failed(&page);
                        return;
                    }
                };
                let profile = obj
                    .imp()
                    .thread_profile_for(loaded.attributes.profile_id.as_deref(), driver);
                let resolved = resolve_agent(profile.clone()).await;
                if !obj.imp().session_start_allowed() {
                    return;
                }
                // agy's hooks file is read off the main thread too; the verdict gates the socket.
                let hook = match driver {
                    Driver::Agy => {
                        let hook_env = AgentEnv {
                            env: resolved.env.clone(),
                            unset: unset_list(&hook_clear),
                        };
                        check_hook(crate::hook_config::HookTarget::new(
                            &hook_env,
                            Some(dir.clone().into()),
                        ))
                        .await
                    }
                    Driver::Claude | Driver::Codex => Ok(()),
                };
                if !obj.imp().session_start_allowed() {
                    return;
                }
                // A session that will start now waits (briefly) for a fresh model list, so a
                // retired model is ported before it reaches argv.
                if AgentAvailability::shared().is_ready(driver) {
                    wait_for_fresh_catalog(driver).await;
                    if !obj.imp().session_start_allowed() {
                        return;
                    }
                }
                obj.imp().build_thread(
                    &page,
                    BuildJob {
                        thread,
                        driver,
                        dir,
                        profile,
                        resolved,
                        loaded,
                        hook,
                    },
                );
            }
        ));
    }

    /// A thread's page whose store read failed: left unbuilt, so showing it again retries.
    fn thread_load_failed(&self, page: &adw::TabPage) {
        if let Some(chat) = self
            .tabs
            .borrow_mut()
            .iter_mut()
            .find(|t| &t.page == page)
            .and_then(|t| t.chat.as_mut())
        {
            chat.building = false;
            chat.pending_switch = None;
            // Not loading any more: the skeleton holds still until a retry.
            if let Some(skeleton) = chat.holder.first_child() {
                loading::settle(&skeleton);
            }
        }
        // Not starting any more: the row stops spinning.
        self.refresh_sidebar();
        self.show_toast("Could not read this thread from the store. Select it again to retry.");
    }

    fn build_thread(&self, page: &adw::TabPage, job: BuildJob) {
        if !self.session_start_allowed() {
            return;
        }
        let BuildJob {
            thread,
            driver,
            dir,
            profile,
            resolved,
            loaded,
            hook,
        } = job;
        let Some((slot, holder, diffs)) = self.tabs.borrow().iter().find_map(|t| {
            (&t.page == page)
                .then(|| {
                    t.chat
                        .as_ref()
                        .map(|c| (c.slot.clone(), c.holder.clone(), c.diffs.clone()))
                })
                .flatten()
        }) else {
            return; // closed while resolving
        };
        let history = loaded.history;

        // The demo window (`AGENT_TERMINAL_DEMO_THREADS`): the view runs on the scripted backend.
        #[cfg(not(test))]
        let demo = crate::chat::view::demo::demo_thread_backend();
        #[cfg(test)]
        let demo: Option<Rc<crate::chat::view::demo::DemoBackend>> = None;

        // The view first, so no envelope of the session's start is lost. It is built from the
        // store whatever the agent's state: history is readable without the agent.
        let backend: Rc<dyn ChatBackend> = match &demo {
            Some(demo) => demo.clone(),
            None => slot.clone(),
        };
        let view = ChatView::new(backend);
        view.set_text_scale(self.config.borrow().font_scale);
        view.set_vexpand(true);
        view.set_model_source(ModelCatalog::shared());
        view.set_account_status(AccountStatus::shared());
        view.set_diff_source(diffs);
        view.set_workspace(std::path::Path::new(&dir));
        if demo.is_some() {
            crate::chat::view::demo::configure_demo_view(&view);
        }
        view.replay_by_agent(&history);
        // The loading skeleton (see `add_thread_page`) gives way to the view.
        if let Some(skeleton) = holder.first_child() {
            holder.remove(&skeleton);
        }
        loading::reveal(&view);
        holder.append(&view);
        *slot.view.borrow_mut() = Some(view.downgrade());
        self.attach_worker_view(&thread, &slot, &view);

        let thread_id = thread.clone();
        view.connect_action(move |action| {
            if let Some(obj) = thread_owner(&thread_id) {
                obj.imp().view_action(&thread_id, action);
            }
        });
        let mut worktree_branch = None;
        if let Some(tab) = self.tabs.borrow_mut().iter_mut().find(|t| &t.page == page) {
            // A reopened worktree thread is one again: its tooltip, and the removal offer when
            // it closes (checked as still registered first: `offer_worktree_removal`).
            if tab.worktree.is_none() {
                if let Some(info) = loaded.attributes.worktree.clone() {
                    worktree_branch = Some(info.branch.clone());
                    tab.worktree = Some(info);
                }
            }
        }
        if let Some(branch) = worktree_branch {
            page.set_tooltip(&tab_tooltip(Some(&branch), None));
        }
        if let Some(chat) = self
            .tabs
            .borrow_mut()
            .iter_mut()
            .find(|t| &t.page == page)
            .and_then(|t| t.chat.as_mut())
        {
            chat.view = Some(view.clone());
            chat.profile_id = loaded.attributes.profile_id.clone();
            if chat.pending_prompt.is_none() {
                chat.pending_prompt = loaded.pending_prompt;
            }
            chat.building = false;
            // A switch whose handoff never reached the new agent (a restart, or it died on its
            // first turn): the session re-arms it once it starts. Ceiling: only the replayed
            // window is scanned, so a handoff more than the replay limit of events back, with no
            // completed turn since, is not re-armed (it would take that many events without one).
            chat.stored_handoff =
                crate::chat::session::undelivered_handoff(history.iter().map(|(_, env)| env));
            chat.chosen_mode =
                crate::chat::session::chosen_mode(history.iter().map(|(_, env)| env));
        }

        // The demo window starts no agent session (see `start_session`): the script plays.
        if let Some(demo) = demo {
            crate::chat::view::demo::play_demo(demo, view.sink());
            return;
        }

        // No session unless the agent is Ready (found and switched on): a missing or disabled
        // agent is never spawned from its command name. The banner says so and offers to continue
        // elsewhere; the session starts by itself if the agent becomes Ready while this thread
        // is open (`start_pending_sessions`).
        let ready = AgentAvailability::shared().is_ready(driver) && resolved.program.is_some();
        if ready {
            self.start_session(
                page,
                StartJob {
                    thread,
                    driver,
                    dir,
                    profile,
                    resolved,
                    provider: loaded.provider,
                    hook,
                },
            );
        } else {
            info!(
                "Thread {thread} opened without a session: {} is not ready",
                driver_label(driver)
            );
            // A switch parked while the thread was being built: its own agent will not start
            // it (still being detected excepted), so the thread moves to the new agent now.
            let parked = self
                .tabs
                .borrow_mut()
                .iter_mut()
                .find(|t| &t.page == page)
                .and_then(|t| t.chat.as_mut())
                .filter(|_| !keep_pending_switch(true, &AgentAvailability::shared().get(driver)))
                .and_then(|chat| chat.pending_switch.take());
            if let Some((target, model, effort)) = parked {
                self.switch_in_place_with(&thread, target, model, effort);
            }
            self.update_unavailable_banners();
            // Built, with no agent to start: the row stops spinning.
            self.refresh_sidebar();
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
    }

    /// Reserves the first asynchronous start before delegation preparation can yield. A switch
    /// must wait for that start just as it waits for a pending retry, rather than replacing the
    /// provider while the old start still owns its launch data.
    fn prepare_session_start(
        &self,
        thread: &str,
        slot: &Rc<SessionSlot>,
    ) -> delegation::PendingStart {
        if let Some(chat) = self
            .tabs
            .borrow_mut()
            .iter_mut()
            .filter_map(|t| t.chat.as_mut())
            .find(|c| c.thread == thread && Rc::ptr_eq(&c.slot, slot))
        {
            chat.starting = true;
        }
        delegation::PendingStart::new(slot)
    }

    /// Starts the session of a thread whose view is built, and applies what was waiting for it.
    fn start_session(&self, page: &adw::TabPage, job: StartJob) {
        if !job.profile.available_for_delegation || cfg!(test) {
            self.start_session_bound(page, job, None);
            return;
        }
        let weak = self.obj().downgrade();
        let Some(slot) = self.slot_of(&job.thread) else {
            return;
        };
        let pending = self.prepare_session_start(&job.thread, &slot);
        let page = page.clone();
        glib::MainContext::default().spawn_local(async move {
            let supervisor = crate::delegation::ready().await;
            let current = || {
                weak.upgrade()
                    .is_some_and(|window| pending.current(window.imp(), &job.thread))
            };
            let clear_cancelled = || {
                let Some(window) = weak.upgrade() else {
                    return;
                };
                pending.clear_cancelled(window.imp(), &job.thread);
            };
            if !current() {
                clear_cancelled();
                return;
            }
            // An opted-in agy root whose registration is unusable starts unbound, and says why.
            let mut problem = None;
            let hooks = match supervisor {
                Some(ref supervisor) => {
                    problem = supervisor.agy_root_problem(&job.profile, job.driver).await;
                    if problem.is_some() || !current() {
                        None
                    } else {
                        supervisor
                            .prepare_root(&job.thread, &job.profile, job.driver)
                            .await
                    }
                }
                None => None,
            };
            if !current() {
                clear_cancelled();
                return;
            }
            if let Some(obj) = weak.upgrade() {
                obj.imp().start_session_bound(&page, job, hooks);
                if let (Some(reason), Some(session)) = (problem, slot.get()) {
                    session.note(delegation::unbound_notice(&reason));
                }
            }
        });
    }

    fn start_session_bound(
        &self,
        page: &adw::TabPage,
        job: StartJob,
        hooks: Option<Rc<dyn crate::delegation::seam::DelegationHooks>>,
    ) {
        if !self.session_start_allowed() {
            return;
        }
        // The demo window starts no agent, by any path (`build_thread`, a pending start when an
        // agent becomes ready, a fork): its threads run the demo script.
        #[cfg(not(test))]
        if crate::chat::view::demo::demo_threads_requested() {
            return;
        }
        let StartJob {
            thread,
            driver,
            dir,
            profile,
            resolved,
            provider,
            hook,
        } = job;
        let (thread, dir, profile) = (thread.as_str(), dir.as_str(), &profile);
        let Some((slot, view)) = self.tabs.borrow_mut().iter_mut().find_map(|t| {
            (&t.page == page).then(|| {
                t.chat.as_mut().and_then(|c| {
                    c.starting = false;
                    Some((c.slot.clone(), c.view.clone()?))
                })
            })?
        }) else {
            return; // closed while resolving
        };
        if slot.get().is_some() {
            return; // already running
        }
        // The provider was read off the main thread, behind the writer's barrier: the session
        // does not read it again here.
        let store = crate::chat::session::SessionStore {
            handle: app_handle(),
            provider: Some(provider.as_ref().map(|p| p.id.clone())),
        };

        let thread_id = thread.to_owned();
        let view_sink = view.sink();
        let weak_slot = Rc::downgrade(&slot);
        let sink: EnvelopeSink = Rc::new(move |env: &Envelope| {
            view_sink(env);
            let driver = weak_slot.upgrade().map_or(driver, |s| s.driver());
            if let Some(obj) = thread_owner(&thread_id) {
                obj.imp().on_thread_envelope(&thread_id, driver, env);
            }
        });

        // Resume the active provider thread's native session, on its model.
        let resume = provider.as_ref().and_then(|p| p.native_id.clone());
        // An effort chosen for this thread (continued in another agent) beats the profile's.
        let effort = self
            .tabs
            .borrow_mut()
            .iter_mut()
            .find(|t| &t.page == page)
            .and_then(|t| t.chat.as_mut())
            .and_then(|c| c.pending_effort.take())
            .or_else(|| profile.default_effort.clone());
        // A model its agent no longer offers (judged on a list fetched in this run) is never
        // started: the thread resumes on the replacement, and says so.
        let (model, effort, ported) = live_model(
            driver,
            provider
                .as_ref()
                .and_then(|p| stored_model(Some(&p.model)))
                .or_else(|| profile.effective_default_model(None)),
            effort,
        );
        // The mode the user chose in this thread first (the session then runs what this agent
        // can honour of it), else the thread's own last running mode (what its picker shows),
        // else the profile's: starting in the profile's left the picker showing one mode while
        // the agent ran in another, and could reopen a switched thread in a laxer mode. A mode the
        // agent moved to on its own (Claude entering plan mode) is not a choice: the user's own
        // last choice wins on reopen.
        let chosen = self
            .tabs
            .borrow()
            .iter()
            .find(|t| &t.page == page)
            .and_then(|t| t.chat.as_ref())
            .and_then(|c| c.chosen_mode);
        let mode = start_mode(chosen, view.replayed_mode(), profile.default_mode);
        let program = resolved
            .program
            .clone()
            .unwrap_or_else(|| profile.command.clone());
        let open = OpenSession {
            program: program.clone(),
            extra_args: profile.args.clone(),
            cwd: dir.to_owned(),
            model,
            effort,
            mode,
            new_session_id: (resume.is_none() && driver == Driver::Claude)
                .then(|| glib::uuid_string_random().to_string()),
            resume,
            approval_hook: false,
            delegation: None,
        };
        let adapter = make_adapter(driver, &program);
        let clear = self.config.borrow().clear_env.clone();
        let launch_env = LaunchEnv {
            env: resolved.env,
            unset: unset_list(&clear),
        };
        let approval = match driver {
            Driver::Claude | Driver::Codex => Err(None),
            Driver::Agy => {
                // Settings may have changed clear_env while the hook check or catalogue waited.
                bind_approval(hook_for_launch(hook, &launch_env, dir), thread, dir, mode)
            }
        };
        info!(
            "Opening thread {thread} on {} in {dir}",
            driver_label(driver)
        );
        let session = ChatSession::with_options(
            adapter,
            open,
            store,
            thread.to_owned(),
            sink,
            approval,
            launch_env,
            crate::delegation::seam::SessionOptions {
                generation_base: crate::delegation::supervisor()
                    .map(|s| s.root_generation_base(thread))
                    .unwrap_or_default(),
                hooks,
                child: None,
            },
        );
        if !self.session_start_allowed() {
            session.shutdown(true);
            return;
        }
        let (thread_for, dir_for) = (thread.to_owned(), dir.to_owned());
        session.set_adapter_factory(Rc::new(move |d| agent_launch(d, &thread_for, &dir_for)));
        let dir_for = dir.to_owned();
        session.set_agy_can_ask(Rc::new(move || agy_hook_ready(&dir_for)));
        session.set_lifecycle_listener(Self::lifecycle_listener(thread));
        *slot.session.borrow_mut() = Some(session.clone());
        if let Some(supervisor) = crate::delegation::supervisor() {
            supervisor.attach_root_session(thread, Rc::downgrade(&session));
        }
        view.refresh_status();
        if let Some(moved) = &ported {
            session.note(retired_notice(moved));
            if let Some(pt) = provider.as_ref().map(|p| p.id.clone()) {
                let new_model = moved.replacement.model.clone();
                self.write_store("record the replacement model", move |s| {
                    s.set_provider_model(&pt, &new_model)
                });
            }
        }

        let (prompt, switch, stored) = {
            let mut tabs = self.tabs.borrow_mut();
            let chat = tabs
                .iter_mut()
                .find(|t| &t.page == page)
                .and_then(|t| t.chat.as_mut());
            match chat {
                Some(chat) => {
                    // A new thread's own handoff was stored with it: it is in `stored_handoff`.
                    chat.pending_handoff = false;
                    (
                        chat.pending_prompt.take(),
                        chat.pending_switch.take(),
                        chat.stored_handoff.take(),
                    )
                }
                None => (None, None, None),
            }
        };
        if let Some(stored) = stored {
            session.restore_handoff(stored);
        }
        if let Some((driver, model, effort)) = switch {
            delegation::switch(&slot, driver, model, effort);
        }
        if let Some(prompt) = prompt.filter(|_| self.session_start_allowed()) {
            session.send_prompt(&prompt);
        }
        // The thread is built: judge its model against the current catalogue too.
        self.evaluate_models();
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

    /// `thread`'s session says its lifecycle changed (its agent answered, exited, paused or is
    /// starting again), maybe with no event at all: its header, and its row if that now shows
    /// another activity, are brought up to date. Deferred, since the session calls this from
    /// inside itself.
    fn lifecycle_listener(thread: &str) -> crate::chat::LifecycleListener {
        let thread = thread.to_owned();
        Rc::new(move |_| {
            let thread = thread.clone();
            glib::idle_add_local_once(move || {
                let Some(obj) = thread_owner(&thread) else {
                    return;
                };
                let imp = obj.imp();
                let view = imp
                    .tabs
                    .borrow()
                    .iter()
                    .filter_map(|t| t.chat.as_ref())
                    .find(|c| c.thread == thread)
                    .and_then(|c| c.view.clone());
                if let Some(view) = view {
                    view.refresh_status();
                }
                if imp.activity_outdated(&thread) {
                    imp.refresh_sidebar();
                }
            });
        })
    }

    /// Whether `thread`'s agent activity ([`ChatTab::activity`]) differs from what its drawn
    /// sidebar row shows. A row not drawn (folded away, filtered out) is never outdated.
    fn activity_outdated(&self, thread: &str) -> bool {
        let now = self
            .tabs
            .borrow()
            .iter()
            .filter_map(|t| t.chat.as_ref())
            .find(|c| c.thread == thread)
            .and_then(ChatTab::activity);
        let Some(sidebar) = self.sidebar.borrow().clone() else {
            return false;
        };
        let key = RowKey::Thread(thread.to_owned());
        let drawn = sidebar.drawn.borrow();
        drawn
            .as_ref()
            .and_then(|m| m.rows().find(|r| r.key == key))
            .is_some_and(|r| r.activity != now)
    }

    fn on_thread_envelope(&self, thread: &str, driver: Driver, env: &Envelope) {
        if matches!(env.event, Event::TurnCompleted { .. }) {
            if let Some(supervisor) = crate::delegation::supervisor() {
                supervisor.on_root_turn_end(thread);
            }
        }
        // A model change (the user's pick, a port) may settle or raise a retirement question.
        if matches!(env.event, Event::ModelChanged { .. }) {
            let weak = self.obj().downgrade();
            glib::idle_add_local_once(move || {
                if let Some(obj) = weak.upgrade() {
                    obj.imp().evaluate_models();
                }
            });
        }
        let in_view = self.selected_row_key() == Some(RowKey::Thread(thread.to_owned()));
        let focused = self.obj().is_active();
        let delegated_work = crate::delegation::supervisor().is_some_and(|s| s.root_busy(thread));
        // Background work just ended with a finish held: released after this dispatch (an
        // exiting session empties the list just before its SessionExited, which drops the hold:
        // a close or a switch is not a finish).
        let mut release_held = false;
        // Set by the arms below, acted on after the tab borrow is released.
        let mut ask: Option<(adw::TabPage, bool)> = None; // (page, is a question)
        let mut withdraw_ask = false;
        // Switched in place to another agent, and that agent has started: the profile the thread
        // was opened with is no longer its agent's, so it follows the new agent's default (stored,
        // for a reopen). Not before: a switch whose agent never starts rolls back, and the old
        // agent must restart on its own profile.
        let mut clear_profile = false;
        enum After {
            Nothing,
            Sidebar,
            TurnDone { page: adw::TabPage, notify: bool },
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
            let profile_runs = chat.profile_id.as_deref().map(|id| {
                self.config
                    .borrow()
                    .profile_by_id(id)
                    .and_then(profile_driver)
            });
            if profile_outlived(profile_runs, driver, &env.event) {
                chat.profile_id = None;
                clear_profile = true;
            }
            // Heard from: idle suspension counts from the thread's last event too.
            chat.last_seen.set(std::time::Instant::now());
            if chat.rate_limited && limit_over(&env.event) {
                chat.rate_limited = false;
                chat.rate_banner.set_revealed(false);
            }
            let after = match &env.event {
                Event::TurnStarted { .. } => {
                    chat.running = true;
                    // This turn finishes the thread (or holds it again) when it ends.
                    chat.finish_held = false;
                    // The state of the files before the turn, for "View diff".
                    chat.diffs.turn_started(self.config.borrow().checkpoints);
                    After::Sidebar
                }
                Event::ItemStarted {
                    kind: ItemKind::FileChange,
                    ..
                } => {
                    if let Some(item) = &env.item {
                        chat.diffs.item_started(item);
                    }
                    After::Nothing
                }
                Event::TurnCompleted { state, error, .. } => {
                    chat.diffs.turn_ended();
                    chat.running = false;
                    chat.approval = false;
                    withdraw_ask = true;
                    if *state == agent_core::event::TurnState::Failed
                        && error
                            .as_deref()
                            .is_some_and(agent_core::quota::is_rate_limit_or_quota)
                    {
                        chat.rate_limited = true;
                    }
                    if chat.rate_limited {
                        After::RateLimited(chat.rate_banner.clone(), driver)
                    } else {
                        // With sub-agents or background commands still going the thread is not
                        // finished: no mark and no "finished" yet. Claude runs another turn to read
                        // their results, and that one finishes the thread.
                        let finished = turn_finishes_thread(chat.background_work(delegated_work));
                        let r = react(
                            Attention::TurnDone,
                            in_view,
                            focused,
                            self.config.borrow().notify_on_bell,
                        );
                        // A shown thread in an unfocused window keeps its mark until focus returns.
                        if finished {
                            chat.unread |= r.unread;
                        }
                        // An interrupted turn was stopped on purpose (a close, a quit, the stop
                        // button): there is nothing to announce, now or when the background ends.
                        let stopped = *state == agent_core::event::TurnState::Interrupted;
                        chat.finish_held = !finished && !stopped;
                        // The turn's end is "output went quiet" for the checkpoint bookkeeping.
                        last_output.set(std::time::Instant::now());
                        After::TurnDone {
                            page,
                            notify: finished && r.notify && !stopped,
                        }
                    }
                }
                Event::BackgroundTasks { tasks } => {
                    chat.background = tasks.clone();
                    release_held = held_finish_due(
                        chat.finish_held,
                        chat.running,
                        chat.background_work(delegated_work),
                    );
                    After::Sidebar
                }
                // Provider sub-agents (Codex workers) outlive the main turn like background
                // tasks do: they hold the finish, the close question and idle suspension.
                Event::WorkersUpdated { workers, .. } => {
                    chat.workers_active = workers.iter().filter(|w| w.state.is_active()).count();
                    release_held = held_finish_due(
                        chat.finish_held,
                        chat.running,
                        chat.background_work(delegated_work),
                    );
                    After::Sidebar
                }
                Event::SessionExited { .. } => {
                    chat.diffs.turn_ended();
                    chat.running = false;
                    chat.finish_held = false;
                    // A dead session runs nothing in the background either.
                    chat.background.clear();
                    chat.workers_active = 0;
                    // Its delegated sub-agents' asks stay: they outlive a restart of the root's
                    // process, and the registry drops those of workers that ended.
                    chat.approval = false;
                    withdraw_ask = true;
                    After::Sidebar
                }
                // A reply question is not an interruption: no sidebar ask, no notification.
                ev if is_interruption(ev) => {
                    chat.approval = true;
                    let r = react(Attention::Asked, in_view, focused, false);
                    chat.unread |= r.unread;
                    if r.notify {
                        // A tool server's form is a question for the user, not an approval.
                        let question = matches!(
                            env.event,
                            Event::QuestionRequested { .. } | Event::FormRequested { .. }
                        );
                        ask = Some((page, question));
                    }
                    After::Sidebar
                }
                Event::ApprovalExpired => {
                    chat.approval = false;
                    withdraw_ask = true;
                    // The deadline denied it unseen: the sidebar keeps a trace.
                    chat.unread |= react(Attention::Expired, in_view, focused, false).unread;
                    After::Sidebar
                }
                Event::ApprovalResolved { .. }
                | Event::QuestionResolved { .. }
                | Event::FormResolved { .. } => {
                    chat.approval = false;
                    withdraw_ask = true;
                    After::Sidebar
                }
                Event::RateLimited { .. } => {
                    chat.rate_limited = true;
                    After::RateLimited(chat.rate_banner.clone(), driver)
                }
                Event::Error { message } => {
                    if agent_core::quota::is_rate_limit_or_quota(message) {
                        chat.rate_limited = true;
                        After::RateLimited(chat.rate_banner.clone(), driver)
                    } else {
                        After::Sidebar
                    }
                }
                Event::QuotaUpdated { windows, .. } => {
                    if windows.iter().any(|w| w.used >= 1.0) {
                        chat.rate_limited = true;
                        After::RateLimited(chat.rate_banner.clone(), driver)
                    } else {
                        After::Sidebar
                    }
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
            // The notification is the thread's, its sub-agents' asks included: the agent's own
            // answer or turn end leaves it while a sub-agent still waits on the user.
            withdraw_ask &= !chat.child_asks.any();
            // A switch to the other agent recolours the row.
            match after {
                After::Nothing if switched => After::Sidebar,
                other => other,
            }
        };
        if withdraw_ask {
            if let Some(app) = self.obj().application() {
                app.withdraw_notification(&approval_notification_id(thread));
            }
        }
        if release_held {
            let weak = self.obj().downgrade();
            let thread = thread.to_owned();
            glib::idle_add_local_once(move || {
                if let Some(obj) = weak.upgrade() {
                    obj.imp().release_held_finish(&thread);
                }
            });
        }
        if clear_profile {
            let id = thread.to_owned();
            self.write_store("clear a switched thread's profile", move |s| {
                s.set_thread_profile(&id, None)
            });
        }
        if let Some((page, question)) = ask {
            let heading = if question {
                "A thread has a question"
            } else {
                "A thread needs approval"
            };
            self.notify_thread(&page, &approval_notification_id(thread), heading);
            self.bump_thread_attention_toast(&page, thread, question);
        }
        match after {
            After::Nothing => {}
            After::Sidebar => self.refresh_sidebar(),
            After::Title(title) => {
                let (id, stored) = (thread.to_owned(), title.clone());
                self.write_store("title the thread", move |s| s.rename_thread(&id, &stored));
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
                banner.set_title(&format!("{} hit its rate limit", driver_label(driver)));
                // With no other usable agent there is nothing to continue in: no button. An agy
                // that would edit unasked is offered only as a last resort, and says so.
                let unasked = self.agy_cannot_ask_for(thread);
                match safe_handoff_target(driver, |d| self.agent_usable(d), unasked) {
                    Some((other, flagged)) => {
                        banner.set_button_label(Some(&continue_label(other, flagged)));
                    }
                    None => banner.set_button_label(None),
                }
                banner.set_revealed(true);
                self.refresh_sidebar();
            }
            After::TurnDone { page, notify } => {
                if self.config.borrow().checkpoints {
                    self.request_checkpoint(&page, false);
                }
                // The turn may have switched branches, or made the folder a repository.
                if in_view {
                    self.refresh_branch(&page);
                    self.refresh_repo_presence(&page);
                }
                // The turn's events moved the thread in the list (and unread); read it again.
                // Only looking at it reads it: in an unfocused window the mark stays until
                // focus returns (`acknowledge_shown_thread`).
                if in_view && focused {
                    let id = thread.to_owned();
                    self.write_store("mark the thread read", move |s| s.mark_read(&id));
                } else {
                    self.reload_summaries();
                }
                if notify {
                    self.notify_thread(&page, &turn_notification_id(thread), "Thread finished");
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

    /// Finishes a thread whose last turn ended while background work still ran, once that work
    /// is gone and no turn followed: its unread mark and "Thread finished", as the turn's end
    /// would have given. Re-checked here, after the event that scheduled it.
    fn release_held_finish(&self, thread: &str) {
        if crate::delegation::supervisor().is_some_and(|s| s.root_busy(thread)) {
            return;
        }
        let in_view = self.selected_row_key() == Some(RowKey::Thread(thread.to_owned()));
        let focused = self.obj().is_active();
        let page = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(tab) = tabs
                .iter_mut()
                .find(|t| t.chat.as_ref().is_some_and(|c| c.thread == thread))
            else {
                return;
            };
            let page = tab.page.clone();
            let Some(chat) = tab.chat.as_mut() else {
                return;
            };
            if !held_finish_due(chat.finish_held, chat.running, chat.background_work(false)) {
                return;
            }
            chat.finish_held = false;
            let r = react(
                Attention::TurnDone,
                in_view,
                focused,
                self.config.borrow().notify_on_bell,
            );
            chat.unread |= r.unread;
            r.notify.then_some(page)
        };
        if in_view && focused {
            let id = thread.to_owned();
            self.write_store("mark the thread read", move |s| s.mark_read(&id));
        } else {
            self.reload_summaries();
        }
        if let Some(page) = page {
            self.notify_thread(&page, &turn_notification_id(thread), "Thread finished");
        }
        self.refresh_sidebar();
    }

    /// Raises a desktop notification for a thread page under its own `id`; clicking it shows
    /// that thread (`app.show-tab`). Unlike the terminal bell it is not a setting: the caller
    /// has already decided it is wanted.
    fn notify_thread(&self, page: &adw::TabPage, id: &str, heading: &str) {
        let Some(app) = self.obj().application() else {
            return;
        };
        let key = self
            .tabs
            .borrow()
            .iter()
            .find(|t| &t.page == page)
            .map(|t| t.key);
        let notification = gtk4::gio::Notification::new(heading);
        let title = page.title();
        notification.set_body(Some(if title.is_empty() {
            "New thread"
        } else {
            &title
        }));
        notification.set_priority(gtk4::gio::NotificationPriority::High);
        // Without a default action, clicking it activates the app and opens a second window.
        if let Some(key) = key {
            notification
                .set_default_action_and_target_value("app.show-tab", Some(&key.to_variant()));
        }
        // GApplication is called into with no tab borrowed (the borrow above is released).
        app.send_notification(Some(id), &notification);
    }

    /// Shows `toast` over the selected thread's transcript, so it never covers that thread's
    /// shelves or composer; over the whole window when no thread is selected (empty state,
    /// terminal page).
    pub(super) fn add_toast(&self, toast: adw::Toast) {
        #[cfg(test)]
        self.test_toasts.borrow_mut().push(toast.clone());
        let selected = self
            .tab_view
            .borrow()
            .as_ref()
            .and_then(|view| view.selected_page());
        let local = selected.and_then(|page| {
            self.tabs
                .borrow()
                .iter()
                .find(|t| t.page == page)
                .and_then(|t| t.chat.as_ref())
                .and_then(|chat| chat.view.as_ref())
                .and_then(|view| view.toast_overlay())
        });
        if let Some(overlay) = local.or_else(|| self.toast_overlay.borrow().clone()) {
            overlay.add_toast(toast);
        }
    }

    /// Bumps an interactive in-app toast when a thread needs attention and the user is away from it.
    fn bump_thread_attention_toast(&self, page: &adw::TabPage, thread: &str, question: bool) {
        let title = page.title();
        let name = if title.is_empty() { "Thread" } else { &title };
        let msg = if question {
            format!("'{name}' has a question")
        } else {
            format!("'{name}' needs approval")
        };
        let toast = adw::Toast::builder()
            .title(msg)
            .use_markup(false)
            .button_label("Switch")
            .timeout(10)
            .priority(adw::ToastPriority::High)
            .build();
        let obj = self.obj().downgrade();
        let thread_id = thread.to_owned();
        toast.connect_button_clicked(move |_| {
            if let Some(window) = obj.upgrade() {
                window.imp().open_thread(&thread_id, true);
            }
        });
        self.add_toast(toast);
    }

    /// Withdraws a thread's approval and turn-done notifications.
    pub(super) fn withdraw_thread_notifications(&self, thread: &str) {
        if let Some(app) = self.obj().application() {
            app.withdraw_notification(&approval_notification_id(thread));
            app.withdraw_notification(&turn_notification_id(thread));
        }
    }

    /// Ends every live thread session on purpose (the window is closing), so each thread's
    /// stored history is closed out, and withdraws their notifications. Sessions are collected
    /// first: their envelopes come back through `on_thread_envelope`, which borrows the tabs.
    ///
    /// Returns the agent processes still on their way out (SIGTERM sent, not yet exited), so the
    /// window can wait for them instead of the app blocking on them at quit.
    pub(super) fn shutdown_thread_sessions(&self) -> Vec<super::closing::StopTarget> {
        let (sessions, threads): (Vec<_>, Vec<_>) = self
            .tabs
            .borrow()
            .iter()
            .filter_map(|t| t.chat.as_ref())
            .map(|c| {
                c.slot
                    .switch_serial
                    .set(c.slot.switch_serial.get().wrapping_add(1));
                let label = (driver_label(c.slot.driver()).to_owned(), c.title.clone());
                ((c.slot.get(), !c.unread, label), c.thread.clone())
            })
            .unzip();
        let mut stopping = Vec::new();
        if let Some(supervisor) = crate::delegation::supervisor() {
            for thread in &threads {
                stopping.extend(
                    supervisor
                        .root_closing(thread)
                        .into_iter()
                        .map(delegation::stop_target),
                );
            }
        }
        for (session, keep_read, (agent, title)) in sessions {
            let Some(session) = session else { continue };
            // Before the shutdown, which drops the process (the watch outlives it).
            let watches = session.exit_watches();
            session.shutdown(keep_read);
            for watch in watches {
                stopping.push(super::closing::StopTarget {
                    agent: agent.clone(),
                    title: title.clone(),
                    watch,
                });
            }
        }
        for thread in threads {
            self.withdraw_thread_notifications(&thread);
        }
        stopping.retain(|t| t.watch.phase() != crate::agent_proc::StopPhase::Exited);
        stopping
    }

    /// The window regained focus: the shown thread is looked at, so its unread mark and its
    /// notifications go.
    pub(super) fn acknowledge_shown_thread(&self) {
        let Some(RowKey::Thread(thread)) = self.selected_row_key() else {
            return;
        };
        let was_unread = self
            .tabs
            .borrow_mut()
            .iter_mut()
            .filter_map(|t| t.chat.as_mut())
            .find(|c| c.thread == thread)
            .is_some_and(|c| std::mem::take(&mut c.unread));
        self.withdraw_thread_notifications(&thread);
        if was_unread {
            self.write_store("mark the thread read", move |s| s.mark_read(&thread));
        }
    }

    pub(super) fn page_of_thread(&self, thread: &str) -> Option<adw::TabPage> {
        self.tabs.borrow().iter().find_map(|t| {
            t.chat
                .as_ref()
                .is_some_and(|c| c.thread == thread)
                .then(|| t.page.clone())
        })
    }

    pub(super) fn slot_of(&self, thread: &str) -> Option<Rc<SessionSlot>> {
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

    /// Whether handing `thread` to agy would let it edit files unasked: the thread forbids that
    /// ([`forbids_unasked_edits`]: Ask or Plan; no session yet counts as forbidding) and agy's
    /// approval hook is not proven, so it would run without a way to ask. Ceiling: a hook that is
    /// installed but whose socket then fails to bind is only known once agy starts (its notice
    /// says so then).
    fn agy_cannot_ask_for(&self, thread: &str) -> bool {
        let wanted = self
            .slot_of(thread)
            .and_then(|s| s.get())
            .map(|s| s.wanted_mode());
        let cwd = self.tabs.borrow().iter().find_map(|t| {
            t.chat
                .as_ref()
                .filter(|c| c.thread == thread)
                .map(|_| t.dir.clone().into())
        });
        forbids_unasked_edits(wanted)
            && cached_agy_hook_target(cwd)
                .is_none_or(|target| cached_hook_verdict(&target).is_err())
    }

    /// The banners' "Continue in" button and a bare `/handoff`: hand the thread to the next
    /// usable agent, never silently to an agy that would edit unasked (see
    /// [`safe_handoff_target`]): that one is confirmed first.
    fn continue_rate_limited(&self, thread: &str) {
        let Some(slot) = self.slot_of(thread) else {
            return;
        };
        let target = safe_handoff_target(
            slot.driver(),
            |d| self.agent_usable(d),
            self.agy_cannot_ask_for(thread),
        );
        match target {
            Some((target, false)) => self.switch_in_place(thread, target),
            Some((target, true)) => self.confirm_unasked_switch(thread, target),
            None => self.show_toast("No other agent is installed and enabled"),
        }
    }

    /// Switches `thread` (open in this window) to `target` in place, on the agent's default model.
    pub(super) fn switch_in_place(&self, thread: &str, target: Driver) {
        self.switch_in_place_with(thread, target, None, None);
    }

    /// Switches `thread` (open in this window) to `target`/`model`/`effort` in place, whatever
    /// state it is in; never by forking it. A live session (stopped or suspended included)
    /// switches itself. Without one, the switch waits for the session or build in flight
    /// ([`switch_route`]), else the thread moves to `target` now ([`Self::rebind_unstarted`]).
    pub(super) fn switch_in_place_with(
        &self,
        thread: &str,
        target: Driver,
        model: Option<String>,
        effort: Option<String>,
    ) {
        let Some(slot) = self.slot_of(thread) else {
            return;
        };
        if slot.get().is_some() {
            slot.switch(target, model, effort);
            return;
        }
        let own = AgentAvailability::shared().get(slot.driver());
        let route = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(chat) = tabs
                .iter_mut()
                .filter_map(|t| t.chat.as_mut())
                .find(|c| c.thread == thread)
            else {
                return;
            };
            if chat.rebinding {
                None
            } else {
                let route = switch_route(false, chat.building || chat.starting, &own);
                if route == SwitchRoute::Park {
                    chat.pending_switch = Some((target, model.clone(), effort.clone()));
                }
                Some(route)
            }
        };
        match route {
            None => self.show_toast("This thread is already switching agent"),
            Some(SwitchRoute::Rebind) if target == slot.driver() => self.show_toast(&format!(
                "{} is not available, so this thread's model cannot be changed",
                driver_label(target)
            )),
            Some(SwitchRoute::Rebind) => self.rebind_unstarted(thread, target, model, effort),
            Some(SwitchRoute::Live | SwitchRoute::Park) => {}
        }
    }

    /// Moves `thread`, which no session runs and whose own agent will not start one, to
    /// `target` in place: a new provider thread (no native session, `model` or the agent's
    /// default: nothing of the old agent's is resumed) made active, with the thread's redacted,
    /// budgeted handoff stored for the new agent ([`rebind_thread`]). Nothing builds or starts
    /// the thread until that has landed (`rebinding`); then the page follows ([`Self::finish_rebind`]).
    fn rebind_unstarted(
        &self,
        thread: &str,
        target: Driver,
        model: Option<String>,
        effort: Option<String>,
    ) {
        let from = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(chat) = tabs
                .iter_mut()
                .filter_map(|t| t.chat.as_mut())
                .find(|c| c.thread == thread)
            else {
                return;
            };
            chat.rebinding = true;
            chat.pending_switch = None;
            chat.slot.driver()
        };
        info!(
            "Switching thread {thread} from {} to {} before any session ran it",
            driver_label(from),
            driver_label(target)
        );
        let id = thread.to_owned();
        let thread = thread.to_owned();
        self.write_store_then(
            "switch the thread's agent",
            move |store| rebind_thread(store, &id, from, target, model.as_deref()),
            move |imp, outcome| imp.finish_rebind(&thread, target, effort, outcome),
        );
    }

    /// The page side of [`Self::rebind_unstarted`], once the writer has answered: on success the
    /// page runs `target` (its session starts when that agent is Ready, with the stored handoff
    /// armed); on failure it stays as it was, and says why.
    fn finish_rebind(
        &self,
        thread: &str,
        target: Driver,
        effort: Option<String>,
        outcome: Option<agent_kit::store::Result<crate::chat::session::Handoff>>,
    ) {
        let label = driver_label(target);
        let landed = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(tab) = tabs
                .iter_mut()
                .find(|t| t.chat.as_ref().is_some_and(|c| c.thread == thread))
            else {
                return; // closed meanwhile: the store has the switch, a reopen reads it
            };
            let page = tab.page.clone();
            let Some(chat) = tab.chat.as_mut() else {
                return;
            };
            // An unanswered write may still land. Keep starts blocked until a reopen reads
            // its provider, rather than starting the old agent on the new agent's native id.
            chat.rebinding = outcome.is_none();
            match outcome {
                Some(Ok(handoff)) => {
                    chat.driver = target;
                    chat.profile_id = None;
                    chat.slot.driver.set(target);
                    chat.pending_effort = effort;
                    let notices = crate::chat::session::switched_handoff_events(&handoff, target);
                    if handoff.carried > 0 {
                        chat.stored_handoff = Some(handoff);
                        chat.pending_handoff = true;
                    }
                    Ok((page, chat.view.clone(), notices))
                }
                Some(Err(e)) => Err(format!("Could not switch this thread to {label}: {e}")),
                None => Err(format!(
                    "The switch to {label} was not confirmed by the store. Reopen the thread to \
                     see which agent it is on."
                )),
            }
        };
        let (page, view, notices) = match landed {
            Ok(landed) => landed,
            Err(message) => {
                self.show_toast(&message);
                return;
            }
        };
        match view {
            // Built: say so where the user is looking, then start it if its agent is Ready.
            Some(view) => {
                for env in notices
                    .iter()
                    .filter(|e| matches!(e.event, Event::Notice { .. }))
                {
                    view.apply(env);
                }
                view.refresh_status();
                self.update_unavailable_banners();
                self.start_pending_sessions();
            }
            // Not built: the build waited for this (`ensure_built`); it reads the new agent now.
            None => {
                let shown = self
                    .tab_view
                    .borrow()
                    .as_ref()
                    .and_then(|v| v.selected_page())
                    .is_some_and(|p| p == page);
                if shown {
                    self.ensure_built(&page);
                }
            }
        }
        self.for_each_window(|window| window.refresh_sidebar());
    }

    /// The thread of the page in view, if it is a chat thread.
    pub(super) fn current_thread(&self) -> Option<String> {
        let page = self.tab_view.borrow().as_ref()?.selected_page()?;
        self.tabs
            .borrow()
            .iter()
            .find(|t| t.page == page)
            .and_then(|t| t.chat.as_ref().map(|c| c.thread.clone()))
    }

    /// Runs `then` (an in-place switch of `thread`) now, or, while the thread's turn runs, once
    /// the user agrees to stop it: the switch stops the turn, and any approval or question in
    /// it. Every in-place switch entry (a typed `/handoff`, the sidebar's "Switch this thread
    /// to", the 2.x Continue In) comes through here, as the header picker asks for itself. The
    /// switch stops the turn itself, so nothing is interrupted before `then` decides to switch.
    pub(super) fn when_not_busy(&self, thread: &str, then: impl Fn(&Self) + 'static) {
        if self.is_closing() {
            return;
        }
        let busy = self
            .tabs
            .borrow()
            .iter()
            .filter_map(|t| t.chat.as_ref())
            .find(|c| c.thread == thread && c.busy())
            .and_then(|c| {
                busy_reason(
                    c.running,
                    c.background.len() + c.workers_active,
                    crate::delegation::supervisor().is_some_and(|s| s.root_busy(&c.thread)),
                    c.waiting_on_user(),
                )
                .map(|why| (c.driver, why))
            });
        let Some((from, why)) = busy else {
            then(self);
            return;
        };
        if self.unasked_prompt_open.replace(true) {
            self.show_toast("Answer the open question first");
            return;
        }
        let dialog = adw::AlertDialog::new(
            Some("Switch agents now?"),
            Some(&format!(
                "{} has unfinished work. {why} Switching continues with the selected agent; \
                 the new turn can ask again.",
                driver_label(from),
            )),
        );
        dialog.add_responses(&[
            ("cancel", "Keep Waiting"),
            ("switch", "Switch and Continue"),
        ]);
        dialog.set_response_appearance("switch", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            let Some(window) = obj.upgrade() else { return };
            let response = dialog
                .choose_future(Some(window.upcast_ref::<gtk4::Widget>()))
                .await;
            let imp = window.imp();
            imp.unasked_prompt_open.set(false);
            if response == "switch" && !imp.is_closing() {
                then(imp);
            }
        });
    }

    /// Asks before handing `thread` to `target` (agy without its hook) while the thread forbids
    /// unasked edits: it would apply edits on its own. One question at a time.
    fn confirm_unasked_switch(&self, thread: &str, target: Driver) {
        if self.unasked_prompt_open.replace(true) {
            return;
        }
        let label = driver_label(target);
        let dialog = adw::AlertDialog::new(
            Some(&format!("Continue in {label}?")),
            Some(&format!(
                "This thread does not allow edits you have not approved, but {label} cannot ask: \
                 its approval hook is not installed (or not checked yet). It refuses shell \
                 commands, but will edit files without asking, in every mode, Plan included."
            )),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("continue", "Continue Anyway")]);
        dialog.set_response_appearance("continue", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let obj = self.obj().downgrade();
        let thread = thread.to_owned();
        glib::MainContext::default().spawn_local(async move {
            let Some(window) = obj.upgrade() else { return };
            let response = dialog
                .choose_future(Some(window.upcast_ref::<gtk4::Widget>()))
                .await;
            window.imp().unasked_prompt_open.set(false);
            if response == "continue" {
                window.imp().switch_in_place(&thread, target);
            }
        });
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
            // In this thread's folder unless a more specific one applies; it may be gone.
            ViewAction::NewThread => {
                self.new_from_thread(thread, slot.driver(), dir);
            }
            // A named target is the user's own choice; a bare /handoff picks one like the banner.
            // Mid-turn, either asks first (when_not_busy): the switch stops the turn.
            ViewAction::Handoff {
                target: Some(target),
            } => {
                let (id, target) = (thread.to_owned(), *target);
                self.when_not_busy(thread, move |imp| imp.switch_in_place(&id, target));
            }
            ViewAction::Handoff { target: None } => {
                let id = thread.to_owned();
                self.when_not_busy(thread, move |imp| imp.continue_rate_limited(&id));
            }
            ViewAction::Fork => self.fork_thread(thread, slot.driver(), dir, false),
            ViewAction::CompactByHandoff => self.fork_thread(thread, slot.driver(), dir, true),
            ViewAction::Rewind => {
                if let Some(page) = self.page_of_thread(thread) {
                    self.rewind(&page);
                }
            }
            ViewAction::ModeChosen { driver, mode } => self.offer_default_mode(*driver, *mode),
            ViewAction::StopQuickReplies => {
                self.set_quick_replies(false);
                self.show_toast(
                    "Quick replies are off. Turn them back on in Settings, General, Replies.",
                );
            }
        }
    }

    /// A new thread on the same agent and folder, carrying a budgeted, redacted handoff of
    /// this one, its mode and its model. The source thread is only read: its transcript comes
    /// off the main thread, and the handoff travels with the NEW thread's own record
    /// (`NewThread::handoff`), never through whichever page happens to be selected.
    fn new_from_thread(&self, thread: &str, driver: Driver, dir: Option<String>) {
        let live = self.thread_carry_settings(thread);
        let thread = thread.to_owned();
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            let loaded = store_job(move |store| load_thread(store, &thread, None)).await;
            let Some(obj) = obj.upgrade() else { return };
            let loaded = match loaded {
                Some(Ok(loaded)) => loaded,
                Some(Err(e)) => {
                    present_message(&obj, "Cannot Create a Thread", &e.to_string());
                    return;
                }
                None => {
                    obj.imp().show_toast("Cannot read the source thread");
                    return;
                }
            };
            let settings = carried_settings(
                live.model.or_else(|| {
                    loaded
                        .provider
                        .as_ref()
                        .and_then(|p| stored_model(Some(&p.model)))
                }),
                None,
                live.mode.or_else(|| {
                    crate::chat::session::chosen_mode(loaded.history.iter().map(|(_, e)| e))
                }),
            );
            obj.imp().create_chat_thread(NewThread {
                here: dir,
                model: settings.model,
                mode: settings.mode,
                profile_id: loaded.attributes.profile_id,
                notice: Some(CreationNotice::New),
                ..NewThread::new(driver)
            });
        });
    }

    fn thread_carry_settings(&self, thread: &str) -> CarriedSettings {
        let tabs = self.tabs.borrow();
        let chat = tabs
            .iter()
            .filter_map(|t| t.chat.as_ref())
            .find(|c| c.thread == thread);
        match chat {
            Some(chat) => {
                let session = chat.slot.get();
                carried_settings(
                    session.as_ref().and_then(|s| s.selected_model()),
                    session
                        .as_ref()
                        .map_or_else(|| chat.pending_effort.clone(), |s| s.selected_effort()),
                    session
                        .as_ref()
                        .map(|s| s.wanted_mode())
                        .or(chat.chosen_mode)
                        .or_else(|| chat.view.as_ref().and_then(ChatView::replayed_mode)),
                )
            }
            None => carried_settings(None, None, None),
        }
    }

    pub(super) fn fork_thread(
        &self,
        thread: &str,
        driver: Driver,
        dir: Option<String>,
        compact: bool,
    ) {
        let settings = self.thread_carry_settings(thread);
        self.fork_thread_with(
            thread,
            driver,
            dir,
            settings.model,
            settings.effort,
            compact,
        );
    }

    fn fork_thread_with(
        &self,
        thread: &str,
        driver: Driver,
        dir: Option<String>,
        model: Option<String>,
        effort: Option<String>,
        compact: bool,
    ) {
        let (title, source_driver, source_profile) = self
            .tabs
            .borrow()
            .iter()
            .find_map(|t| {
                t.chat
                    .as_ref()
                    .filter(|c| c.thread == thread)
                    .map(|c| (c.title.clone(), Some(c.driver), c.profile_id.clone()))
            })
            .unwrap_or_default();
        // A fork on the same agent keeps the profile; continuing in another agent takes that
        // agent's own.
        let profile_id = carried_profile(source_driver, source_profile, driver);
        let source = if title.is_empty() {
            "the previous thread".to_owned()
        } else {
            format!("“{title}”")
        };
        let mode = self.thread_carry_settings(thread).mode;
        let (obj, thread) = (self.obj().downgrade(), thread.to_owned());
        glib::MainContext::default().spawn_local(async move {
            let loaded = store_job({
                let thread = thread.clone();
                move |store| {
                    let loaded = load_thread(store, &thread, None)?;
                    let messages = store.transcript_messages(&thread)?;
                    Ok::<_, agent_kit::store::StoreError>((
                        build_handoff(&messages, &format!("thread {thread}")),
                        loaded,
                    ))
                }
            })
            .await;
            let Some(obj) = obj.upgrade() else { return };
            let imp = obj.imp();
            let (handoff, loaded) = match loaded {
                Some(Ok(value)) => value,
                Some(Err(e)) => {
                    present_message(&obj, "Cannot Fork the Thread", &e.to_string());
                    return;
                }
                None => {
                    imp.show_toast("Cannot read the thread");
                    return;
                }
            };
            let settings = carried_settings(
                model.or_else(|| {
                    loaded
                        .provider
                        .as_ref()
                        .and_then(|p| stored_model(Some(&p.model)))
                }),
                effort,
                mode.or_else(|| {
                    crate::chat::session::chosen_mode(loaded.history.iter().map(|(_, e)| e))
                }),
            );
            let profile_id = profile_id.or_else(|| carried_stored_profile(&loaded, driver));
            imp.create_chat_thread(NewThread {
                dir,
                model: settings.model,
                effort: settings.effort,
                mode: settings.mode,
                handoff: Some((handoff, source)),
                profile_id,
                notice: Some(if compact {
                    CreationNotice::Compact(thread)
                } else {
                    CreationNotice::Fork
                }),
                ..NewThread::new(driver)
            });
        });
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

    /// Whether `driver` can take a thread: only when detection found it and it is enabled
    /// ([`Availability::Ready`]). Nothing is assumed: an agent still being detected is not usable.
    pub(super) fn agent_usable(&self, driver: Driver) -> bool {
        AgentAvailability::shared().is_ready(driver)
    }

    /// The agent new threads start on: see [`crate::config::choose_default_agent`].
    pub(super) fn default_agent(&self) -> Option<Driver> {
        let (explicit, profile_driver) = {
            let config = self.config.borrow();
            (
                config.default_agent,
                config.selected_profile().and_then(profile_driver),
            )
        };
        crate::config::choose_default_agent(explicit, profile_driver, |d| self.agent_usable(d))
    }

    /// Creates a thread in `dir` (a folder picked: refused if it is gone; `None`: see
    /// [`crate::utils::resolve_new_thread_dir`]) on `driver` (the default agent when `None`),
    /// opens and shows it. `prompt` is sent once it has started.
    pub(super) fn new_chat_thread(
        &self,
        driver: Option<Driver>,
        dir: Option<String>,
        prompt: Option<String>,
    ) {
        self.new_chat_thread_with(driver, move |driver| NewThread {
            dir,
            prompt,
            ..NewThread::new(driver)
        });
    }

    /// A new thread from the page in view: in its folder, unless something more specific
    /// applies ([`crate::utils::resolve_new_thread_dir`]).
    pub(super) fn new_chat_thread_here(&self, driver: Option<Driver>) {
        let here = self.current_dir();
        self.new_chat_thread_with(driver, move |driver| NewThread {
            here,
            ..NewThread::new(driver)
        });
    }

    /// Creates the thread `make` describes on `driver`, else on the default agent: while the
    /// agents are still being detected, once they are.
    pub(super) fn new_chat_thread_with(
        &self,
        driver: Option<Driver>,
        make: impl FnOnce(Driver) -> NewThread + 'static,
    ) {
        let Some(driver) = driver.or_else(|| self.default_agent()) else {
            // Still looking for the agents: wait for the scan rather than guess (or give up).
            if AgentAvailability::shared().any_detecting() {
                *self.pending_new_thread.borrow_mut() = Some(std::boxed::Box::new(make));
                self.show_toast("Detecting agents…");
                return;
            }
            present_message(
                &self.obj(),
                "No Chat Agent Available",
                "None of Claude, Antigravity or Codex is installed and enabled. Install one, \
                 or enable it in Settings → Agents.",
            );
            return;
        };
        self.create_chat_thread(make(driver));
    }

    /// The profile a thread on `driver` runs with, given the id it was opened with
    /// ([`crate::config::TerminalConfig::thread_profile`]); a bare one for an agent with none.
    pub(super) fn thread_profile_for(&self, profile_id: Option<&str>, driver: Driver) -> Profile {
        self.config
            .borrow()
            .thread_profile(profile_id, driver)
            .cloned()
            .unwrap_or_else(|| crate::config::new_agent_profile(driver))
    }

    /// The profile open thread `thread` runs on `driver` with.
    pub(super) fn thread_profile(&self, thread: &str, driver: Driver) -> Profile {
        let id = self
            .tabs
            .borrow()
            .iter()
            .filter_map(|t| t.chat.as_ref())
            .find(|c| c.thread == thread)
            .and_then(|c| c.profile_id.clone());
        self.thread_profile_for(id.as_deref(), driver)
    }

    /// The folders a new thread on `driver` could start in: `dir` (picked), the folder of the
    /// profile `profile_id` (chosen), `here` (the page in view's), then the configuration's.
    fn thread_folders(
        &self,
        driver: Driver,
        dir: Option<String>,
        here: Option<String>,
        profile_id: Option<&str>,
    ) -> ThreadFolders {
        let config = self.config.borrow();
        let chosen = profile_id
            .and_then(|id| config.profile_by_id(id))
            .filter(|p| profile_driver(p) == Some(driver));
        ThreadFolders {
            explicit: dir,
            chosen_profile: chosen.and_then(|p| p.dir.clone()),
            current: here,
            agent_profile: config.agent_profile(driver).and_then(|p| p.dir.clone()),
            starting: config.starting_directory.clone(),
            home: env::var("HOME").unwrap_or_else(|_| "/".to_string()),
        }
    }

    /// Resolves `folders` off the main thread (it stats the disk, which can hang on a network
    /// mount), says what it must (a picked folder gone, a missing starting directory), then runs
    /// `then` with the folder, and returns `None`. Only a test without a store writer resolves in
    /// place, so its page comes back at once: `then`'s result. A production store without a
    /// writer (it failed to open or start) still resolves off the main thread.
    fn with_new_thread_dir<R: 'static>(
        &self,
        folders: ThreadFolders,
        what: &'static str,
        then: impl FnOnce(&Self, String) -> Option<R> + 'static,
    ) -> Option<R> {
        if cfg!(test) && app_handle().writer().is_none() {
            let cwd = self.use_new_thread_dir(folders.resolve(), what)?;
            return then(self, cwd);
        }
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            let resolved = gtk4::gio::spawn_blocking(move || folders.resolve()).await;
            let Some(obj) = obj.upgrade() else { return };
            let imp = obj.imp();
            if imp.is_closing() {
                return;
            }
            let Ok(resolved) = resolved else {
                present_message(&obj, what, "Checking the folder failed.");
                return;
            };
            if let Some(cwd) = imp.use_new_thread_dir(resolved, what) {
                then(imp, cwd);
            }
        });
        None
    }

    /// The folder a new thread starts in, once resolved: a picked folder that is gone is
    /// refused with a dialog, and a missing starting directory is said in a toast.
    fn use_new_thread_dir(
        &self,
        resolved: Result<crate::utils::NewThreadDir, String>,
        what: &str,
    ) -> Option<String> {
        match resolved {
            Ok(found) => {
                if let Some(missing) = &found.missing_start {
                    self.show_toast(&format!("Started in Home: {missing} is missing"));
                }
                Some(found.dir)
            }
            Err(gone) => {
                warn!("New thread refused: {gone} no longer exists");
                present_message(
                    &self.obj(),
                    what,
                    &format!("Folder {gone} no longer exists."),
                );
                None
            }
        }
    }

    /// Creates a thread, opens and shows it. Whatever `new` leaves unset comes from its profile
    /// (the one chosen, else its agent's). Returns its page when it opens at once (a test with
    /// no store writer); otherwise it opens once its folder is checked and it is stored.
    pub(super) fn create_chat_thread(&self, mut new: NewThread) -> Option<adw::TabPage> {
        if self.is_closing() {
            return None;
        }
        // A profile is the thread's only while it runs the thread's agent.
        if let Some(id) = new.profile_id.as_deref() {
            let fits = self
                .config
                .borrow()
                .profile_by_id(id)
                .is_some_and(|p| profile_driver(p) == Some(new.driver));
            if !fits {
                warn!("Profile {id} does not run this thread's agent; using the agent's own");
                new.profile_id = None;
            }
        }
        let folders = self.thread_folders(
            new.driver,
            new.dir.take(),
            new.here.take(),
            new.profile_id.as_deref(),
        );
        self.with_new_thread_dir(folders, "Cannot Create a Thread", move |imp, cwd| {
            imp.store_new_thread(new, cwd)
        })
    }

    /// Stores a thread in `cwd` (resolved) and opens it once stored. Returns its page when that
    /// is at once (memory-only store, tests).
    fn store_new_thread(&self, new: NewThread, cwd: String) -> Option<adw::TabPage> {
        if self.is_closing() {
            return None;
        }
        let NewThread {
            notice,
            driver,
            prompt,
            model,
            effort,
            title,
            handoff,
            mode,
            profile_id,
            worktree,
            ..
        } = new;
        let store = app_handle();
        // The agent is recorded now, so the sidebar and a later reopen know it before the
        // session starts. The session adopts this provider thread.
        let model = model.or_else(|| {
            self.config
                .borrow()
                .thread_profile(profile_id.as_deref(), driver)
                .and_then(|p| p.effective_default_model(None))
        });
        // A profile default the agent has retired is never started: the suggestion is used.
        let (model, effort, _) = live_model(driver, model, effort);
        let ids = match store.new_id().and_then(|thread| {
            Ok(NewThreadIds {
                thread,
                provider: store.new_id()?,
            })
        }) {
            Ok(ids) => ids,
            Err(e) => {
                present_message(&self.obj(), "Cannot Create a Thread", &e.to_string());
                return None;
            }
        };
        let thread = ids.thread.clone();
        let carries_handoff = handoff.as_ref().is_some_and(|(h, _)| h.carried > 0);
        let spec = NewThreadSpec {
            title: title.clone(),
            model,
            mode,
            handoff,
            attributes: agent_kit::store::ThreadAttributes {
                profile_id: profile_id.clone(),
                worktree: worktree.clone(),
            },
            ..NewThreadSpec::new(&cwd, driver)
        };
        let pending = prompt.clone();
        let record = move |s: &Store| {
            record_new_thread_as(s, &ids, &spec)?;
            if let Some(prompt) = pending {
                save_pending_prompt(s, &ids.thread, &prompt)?;
            }
            Ok(())
        };
        let created = CreatedThread {
            notice,
            driver,
            prompt,
            effort,
            handoff: carries_handoff,
            title,
            profile_id,
            worktree,
            mode,
        };
        if store.writer().is_none() {
            // In place (memory-only store, tests): the page opens at once.
            if let Err(e) = store.reader().atomically(record) {
                present_message(&self.obj(), "Cannot Create a Thread", &e.to_string());
                return None;
            }
            info!("New {} thread in {cwd}", driver_label(driver));
            return self.open_created(&thread, created);
        }
        // Stored by the writer first, and the page opens only once it is: a thread that could
        // not be stored gets the dialog, never a page whose every write would fail.
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            let reload = obj.clone();
            let stored = write_job_or_late("create a thread", record, move || {
                // Landed after all: the list shows it now.
                if let Some(obj) = reload.upgrade() {
                    obj.imp().reload_summaries();
                }
            })
            .await;
            let Some(obj) = obj.upgrade() else { return };
            match stored {
                Some(Ok(())) => {
                    info!("New {} thread in {cwd}", driver_label(driver));
                    obj.imp().open_created(&thread, created);
                }
                Some(Err(e)) => present_message(&obj, "Cannot Create a Thread", &e.to_string()),
                None => {
                    present_message(
                        &obj,
                        "The Thread Is Not Ready Yet",
                        "The thread store is far behind. The thread may still be saved: it \
                         will show in the list once it is.",
                    );
                }
            }
        });
        None
    }

    /// Opens and shows a thread just stored by [`Self::create_chat_thread`], with what it
    /// carries for its first start. Returns its page.
    fn open_created(&self, thread: &str, created: CreatedThread) -> Option<adw::TabPage> {
        let CreatedThread {
            notice,
            driver,
            prompt,
            effort,
            handoff,
            title,
            profile_id,
            worktree,
            mode,
        } = created;
        self.reload_summaries();
        let page = self.open_thread(thread, false)?;
        let branch = worktree.as_ref().map(|w| w.branch.clone());
        if let Some(tab) = self.tabs.borrow_mut().iter_mut().find(|t| t.page == page) {
            if worktree.is_some() {
                tab.worktree = worktree;
            }
            if let Some(chat) = tab.chat.as_mut() {
                chat.driver = driver;
                chat.pending_prompt = prompt;
                chat.pending_effort = effort;
                chat.pending_handoff = handoff;
                chat.profile_id = profile_id;
                // Before the build reads it from history: a page closed sooner still knows it.
                chat.chosen_mode = mode.map(Mode::session_safe);
                if let Some(title) = title {
                    chat.title = title;
                }
            }
        }
        if let Some(branch) = branch {
            page.set_tooltip(&tab_tooltip(Some(&branch), None));
        }
        self.open_thread(thread, true);
        match notice {
            Some(CreationNotice::New) => {
                self.show_toast("New thread: kept the model and safe mode")
            }
            Some(CreationNotice::Fork) => {
                self.show_toast("Forked thread: kept the model, effort and safe mode")
            }
            Some(CreationNotice::Compact(source)) => {
                let toast = adw::Toast::builder()
                    .title("Compacted into a new thread; kept the model, effort and safe mode")
                    .use_markup(false)
                    .button_label("Archive the original")
                    .timeout(15)
                    .build();
                let obj = self.obj().downgrade();
                toast.connect_button_clicked(move |_| {
                    if let Some(obj) = obj.upgrade() {
                        obj.imp().set_thread_archived(&source, true);
                    }
                });
                self.add_toast(toast);
            }
            None => {}
        }
        Some(page)
    }

    /// Resumes a native Claude or agy session as a chat thread: a new thread whose provider
    /// thread carries that native id, opened once stored. A thread that already holds that
    /// session is opened instead (never a duplicate), and a delegated worker's session is
    /// refused. `dir` is the session's folder, passed over if it is gone.
    pub(super) fn resume_as_thread(&self, driver: Driver, native_id: &str, dir: Option<String>) {
        let folders = self.thread_folders(driver, None, dir, None);
        let native = native_id.to_owned();
        self.with_new_thread_dir(folders, "Cannot Resume the Session", move |imp, cwd| {
            imp.store_resumed_thread(driver, native, cwd);
            Some(())
        });
    }

    /// [`Self::resume_as_thread`] once its folder is known: checked and created in one unit on
    /// the store writer ([`Store::create_native_thread`]).
    fn store_resumed_thread(&self, driver: Driver, native: String, cwd: String) {
        if self.is_closing() {
            return;
        }
        let store = app_handle();
        let thread = match store.new_id() {
            Ok(id) => id,
            Err(e) => {
                present_message(&self.obj(), "Cannot Resume the Session", &e.to_string());
                return;
            }
        };
        let record = {
            let (thread, native) = (thread.clone(), native.clone());
            move |s: &Store| {
                let key = driver_key(driver);
                let record = agent_kit::store::ThreadRecord {
                    provider: Some(agent_kit::store::ProviderRecord {
                        id: None,
                        driver: key,
                        model: None,
                        native_id: Some(&native),
                    }),
                    ..agent_kit::store::ThreadRecord::new(&thread, &cwd)
                };
                s.create_native_thread(&record, key, &native)
            }
        };
        let opened = move |imp: &Self, made: agent_kit::store::NativeThread| match made {
            agent_kit::store::NativeThread::Created => {
                info!(
                    "Resuming {} session {native} as a thread",
                    driver_label(driver)
                );
                imp.open_thread(&thread, true);
            }
            agent_kit::store::NativeThread::Existing(existing) => {
                info!(
                    "{} session {native} is already thread {existing}: opening it",
                    driver_label(driver)
                );
                imp.open_thread(&existing, true);
            }
            agent_kit::store::NativeThread::Supervised => {
                warn!(
                    "{} session {native} belongs to a delegated worker: not resumed",
                    driver_label(driver)
                );
                present_message(
                    &imp.obj(),
                    "Cannot Resume the Session",
                    "This session belongs to a worker another thread delegated to. Open that \
                     thread to follow it.",
                );
            }
        };
        if store.writer().is_none() {
            match store.reader().atomically(record) {
                Ok(made) => opened(self, made),
                Err(e) => present_message(&self.obj(), "Cannot Resume the Session", &e.to_string()),
            }
            return;
        }
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            let reload = obj.clone();
            let stored = write_job_or_late("resume a session as a thread", record, move || {
                if let Some(obj) = reload.upgrade() {
                    obj.imp().reload_summaries();
                }
            })
            .await;
            let Some(obj) = obj.upgrade() else { return };
            match stored {
                Some(Ok(made)) => opened(obj.imp(), made),
                Some(Err(e)) => present_message(&obj, "Cannot Resume the Session", &e.to_string()),
                None => {
                    present_message(
                        &obj,
                        "The Session Is Not Ready Yet",
                        "The thread store is far behind. The session may still be saved as a \
                         thread: it will show in the list once it is.",
                    );
                }
            }
        });
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
    /// their binaries; checks again every ten minutes while the window is focused, probing only
    /// what is due.
    fn watch_account_status(&self) {
        // The start-up round: nothing was fetched in this run yet, so every ready agent is due.
        self.refresh_agent_data(Probes::IfDue);
        let obj = self.obj();
        // An agent found signed out (or signed in again) re-shows its threads' banners. The
        // listener goes with the window (the status is app-wide).
        let listener = AccountStatus::shared().connect_changed(glib::clone!(
            #[weak]
            obj,
            move || obj.imp().update_sign_in_banners()
        ));
        obj.connect_destroy(move |_| AccountStatus::shared().disconnect(listener));
        glib::timeout_add_local(
            ACCOUNT_REFRESH,
            glib::clone!(
                #[weak]
                obj,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    if obj.is_active() {
                        obj.imp().refresh_agent_data(Probes::IfDue);
                    }
                    glib::ControlFlow::Continue
                }
            ),
        );
    }

    /// Scans for the agents and refreshes everything that depends on what is there: each
    /// driver's [`Availability`] (the one source for every menu and picker), then the model
    /// catalogue and the account status for the agents that turned out Ready. A switched-off
    /// agent is known at once; the others keep their last state until the scan answers.
    ///
    /// The scan itself spawns no agent (cached resolutions, a stat per binary). With
    /// [`Probes::IfDue`] the catalogue and the account status probe only what their clocks say
    /// is due (an agent back from missing always is), so regaining focus with fresh data spawns
    /// nothing; [`Probes::Force`] probes every ready agent (Settings changed it, or the user
    /// asked to check again).
    pub(super) fn refresh_agent_data(&self, probes: Probes) {
        if self.is_closing() {
            return;
        }
        let generation = SCAN_GENERATION.with(|current| {
            let next = current.get().wrapping_add(1);
            current.set(next);
            next
        });
        // Also what a switch to agy needs: its hook verdict, read off the main thread.
        let sources = RefCell::new((Vec::new(), Vec::new()));
        self.for_each_window(|window| {
            let mut sources = sources.borrow_mut();
            for tab in window.tabs.borrow().iter().filter(|tab| tab.chat.is_some()) {
                sources.0.push(std::path::PathBuf::from(&tab.dir));
                if let Some(session) = tab.chat.as_ref().and_then(|chat| chat.slot.get()) {
                    sources.1.push((tab.dir.clone(), session));
                }
            }
        });
        let (directories, sessions) = sources.into_inner();
        // Session reads happen after every tab borrow has ended; a switch replaces its env.
        let active = sessions
            .into_iter()
            .filter_map(|(cwd, session)| {
                (session.status().driver == Driver::Agy).then(|| {
                    crate::hook_config::HookTarget::new(
                        &session.launch_environment(),
                        Some(cwd.into()),
                    )
                })
            })
            .collect();
        refresh_hook_verdict(directories, active);
        let availability = AgentAvailability::shared();
        availability.mark_scan_started();
        let (profiles, clear): (Vec<(Driver, Profile)>, Vec<String>) = {
            let config = self.config.borrow();
            (
                Driver::ALL
                    .into_iter()
                    .map(|d| {
                        let profile = config
                            .agent_profile(d)
                            .cloned()
                            .unwrap_or_else(|| crate::config::new_agent_profile(d));
                        (d, profile)
                    })
                    .collect(),
                config.clear_env.clone(),
            )
        };
        for (driver, profile) in &profiles {
            if !scan_is_current(generation) {
                return;
            }
            if profile.disabled {
                availability.set(*driver, Availability::Disabled);
            } else if availability.get(*driver) == Availability::Disabled {
                availability.set(*driver, Availability::Detecting);
            }
        }
        // Tests never probe this machine's agents (nor so start one): every agent reads as
        // missing. The window scans as it is built, so every window test would otherwise.
        if cfg!(test) {
            for (driver, profile) in &profiles {
                if !scan_is_current(generation) {
                    return;
                }
                if !profile.disabled {
                    availability.set(*driver, Availability::Missing);
                }
            }
            return;
        }
        // Agents that were missing, or whose binary has gone, are looked for again.
        forget_stale_resolutions();
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            // The shell's PATH, read once before any agent turns Ready: every agent spawn then
            // runs with it (`agent_proc::use_shell_path`), never with the desktop session's.
            let _ = gtk4::gio::spawn_blocking(|| {
                crate::utils::shell_path();
            })
            .await;
            if !scan_is_current(generation) {
                return;
            }
            let availability = AgentAvailability::shared();
            let mut envs: Vec<(Driver, AgentEnv)> = Vec::new();
            for (driver, profile) in profiles {
                if profile.disabled {
                    continue;
                }
                let resolved = resolve_agent(profile).await;
                if !scan_is_current(generation) {
                    return;
                }
                // Probes run where the threads run: the profile's env file and `clear_env`.
                envs.push((
                    driver,
                    AgentEnv {
                        env: resolved.env.clone(),
                        unset: unset_list(&clear),
                    },
                ));
                let state = classify(false, Some(resolved.program.as_deref()));
                // Back from missing (installed, or switched on again): its list may be new.
                if state.is_ready()
                    && matches!(
                        availability.get(driver),
                        Availability::Missing | Availability::Disabled
                    )
                {
                    ModelCatalog::shared().mark_due(driver);
                }
                if !scan_is_current(generation) {
                    return;
                }
                availability.set(driver, state);
                if !scan_is_current(generation) {
                    return;
                }
            }
            // Only what is Ready is probed. The same gate holds for a thread's session
            // (`build_thread`, `start_session`) and a cross-agent switch (`agent_launch`): a
            // missing, switched-off or still-detecting agent is never spawned.
            if !scan_is_current(generation) {
                return;
            }
            let targets = probe_targets(&availability.all(), |d| {
                envs.iter()
                    .find(|(driver, _)| *driver == d)
                    .map(|(_, env)| env.clone())
                    .unwrap_or_default()
            });
            match probes {
                Probes::IfDue => {
                    let now = crate::freshness::now_epoch();
                    ModelCatalog::shared().refresh_if_due(&targets, now);
                    if !scan_is_current(generation) {
                        return;
                    }
                    AccountStatus::shared().refresh_if_due(&targets, now);
                }
                Probes::Force => {
                    ModelCatalog::shared().refresh(&targets);
                    if !scan_is_current(generation) {
                        return;
                    }
                    AccountStatus::shared().refresh(&targets);
                }
            }
            if let Some(obj) = obj.upgrade() {
                obj.imp().on_availability_changed();
            }
        });
    }

    /// Connects the window to [`AgentAvailability`] and re-scans when the window regains focus
    /// and the last scan is over a minute old (a binary installed or removed meanwhile).
    fn watch_availability(&self) {
        let obj = self.obj();
        AgentAvailability::shared().connect_changed(glib::clone!(
            #[weak]
            obj,
            move || obj.imp().on_availability_changed()
        ));
        // A fresh model list may retire a thread's model (or bring it back).
        ModelCatalog::shared().connect_changed(glib::clone!(
            #[weak]
            obj,
            move || obj.imp().evaluate_models()
        ));
        obj.connect_is_active_notify(|window| {
            // Only the cheap rescan, and the probes whose clocks are due: focus alone is not a
            // reason to spawn agents (it used to cost a probe round on nearly every focus).
            if window.is_active() && AgentAvailability::shared().scan_is_stale() {
                window.imp().refresh_agent_data(Probes::IfDue);
            }
            // The thread in view is looked at again: its unread mark and notifications go.
            if window.is_active() {
                window.imp().acknowledge_shown_thread();
            }
            // A thread waiting for approval glows while you are away from it.
            window.imp().refresh_sidebar();
        });
    }

    /// An agent's availability changed (or a scan ended): everything that offers agents follows.
    pub(super) fn on_availability_changed(&self) {
        self.fill_new_with_menu();
        // The picker hides agents that are not ready; an open one redraws.
        ModelCatalog::shared().notify();
        self.update_unavailable_banners();
        self.update_sign_in_banners();
        self.start_pending_sessions();
        self.evaluate_models();
        // A "new thread" asked for while the agents were still being detected.
        if !AgentAvailability::shared().any_detecting() {
            let pending = self.pending_new_thread.borrow_mut().take();
            if let Some(make) = pending {
                self.new_chat_thread_with(None, make);
            }
        }
    }

    /// Judges every built thread's model against its agent's CURRENT catalogue: one the agent no
    /// longer offers gets a banner (with "Choose…") and, when there is a replacement, the session
    /// is told to move to it before the next turn. A list that is only cached, or still loading,
    /// is never grounds for calling a model retired.
    pub(super) fn evaluate_models(&self) {
        let catalog = ModelCatalog::shared();
        let availability = AgentAvailability::shared();
        let built: Vec<(adw::Banner, Rc<ChatSession>)> = self
            .tabs
            .borrow()
            .iter()
            .filter_map(|t| t.chat.as_ref())
            .filter_map(|c| Some((c.model_banner.clone(), c.slot.get()?)))
            .collect();
        for (banner, session) in built {
            let status = session.status();
            let driver = status.driver;
            let notice = if availability.is_ready(driver) {
                model_notice(
                    &catalog.models_of(driver),
                    catalog.is_fresh(driver),
                    session.selected_model().as_deref(),
                    status.effort.as_deref(),
                )
            } else {
                ModelNotice::None
            };
            match retirement_banner(driver_label(driver), &notice) {
                Some(text) => {
                    banner.set_title(&text);
                    banner.set_revealed(true);
                }
                None => banner.set_revealed(false),
            }
            session.set_retired(match notice {
                ModelNotice::Retired {
                    model,
                    replacement: Some(replacement),
                } => Some(RetiredModel { model, replacement }),
                _ => None,
            });
        }
    }

    /// The model banner's "Choose…": the thread's own model picker.
    fn choose_model(&self, thread: &str) {
        let view = self
            .tabs
            .borrow()
            .iter()
            .filter_map(|t| t.chat.as_ref())
            .find(|c| c.thread == thread)
            .and_then(|c| c.view.clone());
        if let Some(view) = view {
            view.open_model_picker();
        }
    }

    /// Every open thread whose agent is installed and switched on, but has no one signed in,
    /// says so, with the button to sign in (or, once a sign-in tab was opened, to check again).
    /// Only the threads of that agent: an agent the user does not use never nags.
    pub(super) fn update_sign_in_banners(&self) {
        let (availability, accounts) = (AgentAvailability::shared(), AccountStatus::shared());
        // Signed in since: the next sign-out starts again from "Sign In…".
        self.sign_in_started
            .borrow_mut()
            .retain(|d| accounts.is_signed_out(*d));
        let tabs = self.tabs.borrow();
        for chat in tabs.iter().filter_map(|t| t.chat.as_ref()) {
            let driver = chat.slot.driver();
            if !(availability.is_ready(driver) && accounts.is_signed_out(driver)) {
                chat.sign_in_banner.set_revealed(false);
                continue;
            }
            let info = driver.info();
            chat.sign_in_banner.set_title(&format!(
                "{} is not signed in. {}",
                info.label, info.sign_in_hint
            ));
            let started = self.sign_in_started.borrow().contains(&driver);
            chat.sign_in_banner.set_button_label(Some(if started {
                "Check Again"
            } else {
                "Sign In…"
            }));
            chat.sign_in_banner.set_revealed(true);
        }
    }

    /// The sign-in banner's button for `thread`'s agent: the first press opens a terminal tab
    /// running the agent's sign-in; after that it checks again (the agent is probed afresh).
    fn sign_in_from_thread(&self, thread: &str) {
        if let Some(driver) = self.slot_of(thread).map(|s| s.driver()) {
            self.sign_in_or_check(driver);
        }
    }

    /// Opens a terminal tab running `driver`'s sign-in (its own command and environment file,
    /// with the sign-in arguments), in the home folder.
    pub(super) fn sign_in(&self, driver: Driver) {
        let info = driver.info();
        let base = self
            .config
            .borrow()
            .agent_profile(driver)
            .cloned()
            .unwrap_or_else(|| crate::config::new_agent_profile(driver));
        let profile = crate::config::Profile {
            name: format!("Sign in to {}", info.label),
            command: base.command,
            args: info.sign_in_args.iter().map(|a| (*a).to_owned()).collect(),
            env_file: base.env_file,
            ..crate::config::Profile::default()
        };
        info!("Opening a sign-in tab for {}", info.label);
        let home = env::var("HOME").ok();
        self.add_terminal_tab(Some(&profile), home.as_deref());
        self.sign_in_started.borrow_mut().insert(driver);
        self.update_sign_in_banners();
    }

    /// Probes `driver` again (agy too, which a refresh otherwise skips while it is signed out).
    /// The banner stays until the probe answers.
    pub(super) fn check_sign_in(&self, driver: Driver) {
        AccountStatus::shared().recheck(driver);
        self.refresh_agent_data(Probes::Force);
        self.show_toast(&format!("Checking {}…", driver.info().label));
    }

    /// Sign In, or Check Again once a sign-in tab was opened: the banner's and Settings' button.
    pub(super) fn sign_in_or_check(&self, driver: Driver) {
        if self.sign_in_started.borrow().contains(&driver) {
            self.check_sign_in(driver);
        } else {
            self.sign_in(driver);
        }
    }

    /// Whether `driver`'s sign-in tab was opened, so its button now checks again.
    pub(super) fn sign_in_was_started(&self, driver: Driver) -> bool {
        self.sign_in_started.borrow().contains(&driver)
    }

    /// An open thread whose agent is missing or switched off says so, and offers to continue in
    /// another agent; the banner goes away again when the agent is back.
    fn update_unavailable_banners(&self) {
        let availability = AgentAvailability::shared();
        let hook_target = cached_agy_hook_target(None);
        let mut rebinds = Vec::new();
        let mut tabs = self.tabs.borrow_mut();
        for tab in tabs.iter_mut() {
            let Some(chat) = tab.chat.as_mut() else {
                continue;
            };
            let hook_unproven = hook_target.clone().is_none_or(|mut target| {
                target.cwd = Some(tab.dir.clone().into());
                cached_hook_verdict(&target).is_err()
            });
            let driver = chat.slot.driver();
            let state = availability.get(driver);
            // As agy_cannot_ask_for (inline: the tabs are borrowed here).
            let wanted = chat.slot.get().map(|s| s.wanted_mode());
            let cannot_ask = forbids_unasked_edits(wanted) && hook_unproven;
            let other = safe_handoff_target(driver, |d| availability.is_ready(d), cannot_ask);
            match unavailable_banner(driver, &state, other.map(|(d, _)| d)) {
                Some((title, button)) => {
                    // A scan that answered Missing/Disabled settles a parked switch: the thread
                    // moves to the agent the user picked now (a build in flight does it once it
                    // lands), so installing or enabling its old agent later applies nothing.
                    if chat.slot.get().is_none() && !chat.building && !chat.starting {
                        if let Some(parked) = chat.pending_switch.take() {
                            rebinds.push((chat.thread.clone(), parked));
                        }
                    }
                    // An agy that would edit unasked says so on the button.
                    let button = match other {
                        Some((target, true)) => Some(continue_label(target, true)),
                        _ => button,
                    };
                    chat.rate_banner.set_title(&title);
                    chat.rate_banner.set_button_label(button.as_deref());
                    chat.rate_banner.set_revealed(true);
                    chat.unavailable = true;
                }
                None if chat.unavailable && state.is_ready() => {
                    chat.rate_banner.set_revealed(false);
                    chat.unavailable = false;
                }
                None => {}
            }
        }
        drop(tabs);
        for (thread, (target, model, effort)) in rebinds {
            self.switch_in_place_with(&thread, target, model, effort);
        }
    }

    /// Starts the session of every open thread that was built without one because its agent was
    /// not Ready and now is. Each start re-resolves the agent and reads the hook verdict and the
    /// provider thread off the main thread, then waits briefly for a fresh model list, exactly
    /// like a first build.
    fn start_pending_sessions(&self) {
        if self.is_closing() {
            return;
        }
        let availability = AgentAvailability::shared();
        let waiting: Vec<(adw::TabPage, String, Driver, String)> = self
            .tabs
            .borrow_mut()
            .iter_mut()
            .filter_map(|t| {
                let chat = t.chat.as_mut()?;
                let driver = chat.slot.driver();
                if chat.view.is_none()
                    || chat.starting
                    || chat.rebinding
                    || chat.slot.get().is_some()
                    || !availability.is_ready(driver)
                {
                    return None;
                }
                chat.starting = true;
                Some((t.page.clone(), chat.thread.clone(), driver, t.dir.clone()))
            })
            .collect();
        for (page, thread, driver, dir) in waiting {
            // Built, so its profile was read with its history.
            let profile = self.thread_profile(&thread, driver);
            let obj = self.obj().downgrade();
            let hook_clear = self.config.borrow().clear_env.clone();
            glib::MainContext::default().spawn_local(async move {
                let resolved = resolve_agent(profile.clone()).await;
                if !obj
                    .upgrade()
                    .is_some_and(|w| w.imp().session_start_allowed())
                {
                    return;
                }
                // A store that cannot be read starts nothing: a session started without the
                // thread's provider would re-point the thread at a fresh session.
                let provider = match store_job({
                    let thread = thread.clone();
                    move |store| load_provider(store, &thread)
                })
                .await
                {
                    Some(Ok(provider)) => Some(provider),
                    Some(Err(e)) => {
                        warn!("Cannot read thread {thread} to start its session: {e}");
                        None
                    }
                    None => None,
                };
                if !obj
                    .upgrade()
                    .is_some_and(|w| w.imp().session_start_allowed())
                {
                    return;
                }
                let hook = match driver {
                    Driver::Agy => {
                        let hook_env = AgentEnv {
                            env: resolved.env.clone(),
                            unset: unset_list(&hook_clear),
                        };
                        check_hook(crate::hook_config::HookTarget::new(
                            &hook_env,
                            Some(dir.clone().into()),
                        ))
                        .await
                    }
                    Driver::Claude | Driver::Codex => Ok(()),
                };
                if !obj
                    .upgrade()
                    .is_some_and(|w| w.imp().session_start_allowed())
                {
                    return;
                }
                if AgentAvailability::shared().is_ready(driver) {
                    wait_for_fresh_catalog(driver).await;
                }
                let Some(obj) = obj.upgrade() else { return };
                if !obj.imp().session_start_allowed() {
                    return;
                }
                let own = AgentAvailability::shared().get(driver);
                let read_ok = provider.is_some();
                let keep_switch = keep_pending_switch(read_ok, &own);
                let ready = own.is_ready() && resolved.program.is_some();
                let Some(provider) = provider.filter(|_| ready) else {
                    // Gone again while resolving, or the store failed: wait for the next change.
                    let parked = if let Some(chat) = obj
                        .imp()
                        .tabs
                        .borrow_mut()
                        .iter_mut()
                        .find(|t| t.page == page)
                        .and_then(|t| t.chat.as_mut())
                    {
                        chat.starting = false;
                        if !read_ok {
                            chat.pending_switch = None;
                            None
                        } else if !keep_switch {
                            chat.pending_switch.take()
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    if let Some((target, model, effort)) = parked {
                        obj.imp()
                            .switch_in_place_with(&thread, target, model, effort);
                    }
                    obj.imp().update_unavailable_banners();
                    return;
                };
                obj.imp().start_session(
                    &page,
                    StartJob {
                        thread,
                        driver,
                        dir,
                        profile,
                        resolved,
                        provider,
                        hook,
                    },
                );
            });
        }
    }
}

/// Forgets resolutions that found nothing or whose binary is gone, so the next scan looks again
/// (the shell probe is costly, so a binary that is still there is kept).
fn forget_stale_resolutions() {
    RESOLVED.with(|r| {
        r.borrow_mut().retain(|_, agent| {
            agent
                .program
                .as_deref()
                .is_some_and(|p| std::path::Path::new(p).exists())
        });
    });
}

/// Whether an event waits on the user: an approval, or a question the agent asked. A question
/// the app found in a reply ([`agent_core::event::QuestionSource::Reply`]) waits for nothing. A
/// form waits only if the card can answer it (the same check the transcript makes).
fn is_interruption(event: &Event) -> bool {
    use agent_core::event::QuestionSource;
    match event {
        Event::ApprovalRequested { .. } => true,
        Event::QuestionRequested { source, .. } => *source != QuestionSource::Reply,
        Event::FormRequested { form, .. } => crate::chat::view::model::form_answerable(form),
        _ => false,
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[test]
    #[ignore = "needs a display; run alone in an isolated display"]
    fn a_failed_custom_profile_does_not_strand_a_switch_while_its_driver_is_ready() {
        gtk4::init().unwrap();
        adw::init().unwrap();
        let config_home = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", config_home.path());
        let app = adw::Application::builder()
            .application_id("org.test.SwitchFailedProfile")
            .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk4::gio::Cancellable>).unwrap();
        let window = crate::window::AgentTerminalWindow::new(&app);
        let ctx = glib::MainContext::default();
        assert!(crate::testutil::pump_until(&ctx, 5, || window
            .imp()
            .tab_view
            .borrow()
            .is_some()));
        CHAT_RESTORED.with(|done| done.set(true));
        let availability = AgentAvailability::shared();
        for driver in Driver::ALL {
            availability.set(driver, Availability::Missing);
        }
        let mut profile = crate::config::new_agent_profile(Driver::Claude);
        profile.id = "unavailable-custom-claude".into();
        profile.command = "/nonexistent/custom-claude".into();
        RESOLVED.with(|r| {
            r.borrow_mut().insert(
                (profile.command.clone(), profile.env_file.clone()),
                ResolvedAgent::default(),
            )
        });
        window
            .imp()
            .config
            .borrow_mut()
            .profiles
            .push(profile.clone());
        let folder = tempfile::tempdir().unwrap();
        window
            .imp()
            .create_chat_thread(NewThread {
                dir: Some(folder.path().to_string_lossy().into_owned()),
                profile_id: Some(profile.id.clone()),
                ..NewThread::new(Driver::Claude)
            })
            .unwrap();
        let thread = window.imp().current_thread().unwrap();
        let built = || {
            window
                .imp()
                .tabs
                .borrow()
                .iter()
                .filter_map(|t| t.chat.as_ref())
                .any(|c| c.thread == thread && c.view.is_some() && !c.building && !c.starting)
        };
        assert!(crate::testutil::pump_until(&ctx, 5, built));
        assert!(window.imp().slot_of(&thread).unwrap().get().is_none());
        let before = app_store().list_threads(false).unwrap().len();
        availability.set(
            Driver::Claude,
            Availability::Ready("/fixture/default-claude".into()),
        );
        window.imp().switch_in_place(&thread, Driver::Codex);
        assert!(crate::testutil::pump_until(&ctx, 5, || window
            .imp()
            .slot_of(&thread)
            .unwrap()
            .driver()
            == Driver::Codex));
        assert_eq!(app_store().list_threads(false).unwrap().len(), before);
        assert_eq!(
            load_provider(&app_store(), &thread)
                .unwrap()
                .unwrap()
                .driver,
            "codex"
        );
        assert_eq!(
            app_store().thread_attributes(&thread).unwrap().profile_id,
            None
        );
        window.destroy();
        crate::testutil::settle(&ctx);
    }

    #[test]
    fn switch_routes_serialize_builds_and_never_fork_missing_agents() {
        for own in [
            Availability::Detecting,
            Availability::Ready("fixture".into()),
            Availability::Missing,
            Availability::Disabled,
        ] {
            for building in [false, true] {
                assert_eq!(switch_route(true, building, &own), SwitchRoute::Live);
                let expected = if building || matches!(own, Availability::Detecting) {
                    SwitchRoute::Park
                } else {
                    SwitchRoute::Rebind
                };
                assert_eq!(switch_route(false, building, &own), expected);
            }
        }
    }

    #[test]
    fn carried_settings_keep_model_effort_and_only_safe_modes() {
        for mode in [Mode::Ask, Mode::Plan, Mode::AcceptEdits, Mode::Bypass] {
            let carried =
                carried_settings(Some("chosen-model".into()), Some("high".into()), Some(mode));
            assert_eq!(carried.model.as_deref(), Some("chosen-model"));
            assert_eq!(carried.effort.as_deref(), Some("high"));
            assert_eq!(carried.mode, Some(mode.session_safe()));
            assert_ne!(carried.mode, Some(Mode::Bypass));
        }
        assert_eq!(
            carried_settings(None, None, None),
            CarriedSettings {
                model: None,
                effort: None,
                mode: None
            }
        );
    }

    #[test]
    fn an_unstarted_switch_keeps_the_thread_and_clears_foreign_resume_and_profile() {
        let store = Store::open_in_memory().unwrap();
        let thread = store.create_thread("/workspace", Some("source")).unwrap();
        let old = store
            .add_provider_thread(&thread, "claude", "old-model")
            .unwrap();
        store.set_native_id(&old, "native-claude").unwrap();
        store.set_active_provider_thread(&thread, &old).unwrap();
        store
            .set_thread_profile(&thread, Some("custom-claude"))
            .unwrap();
        let before = store.list_threads(false).unwrap().len();
        let handoff = rebind_thread(
            &store,
            &thread,
            Driver::Claude,
            Driver::Codex,
            Some("new-model"),
        )
        .unwrap();
        assert_eq!(handoff.carried, 0);
        assert_eq!(store.list_threads(false).unwrap().len(), before);
        let provider = load_provider(&store, &thread).unwrap().unwrap();
        assert_eq!(provider.driver, "codex");
        assert_eq!(provider.model, "new-model");
        assert_eq!(provider.native_id, None);
        assert_eq!(store.thread_attributes(&thread).unwrap().profile_id, None);
        assert!(store.events(&thread, None, 100).unwrap().iter().any(
            |(_, env)| matches!(&env.event, Event::Notice { text } if text == "Switched to Codex")
        ));
    }

    #[test]
    fn a_historical_switch_stores_one_recoverable_handoff_on_the_new_provider() {
        let store = Store::open_in_memory().unwrap();
        let thread = store.create_thread("/workspace", Some("history")).unwrap();
        let old = store
            .add_provider_thread(&thread, "claude", "old-model")
            .unwrap();
        store.set_native_id(&old, "native-claude").unwrap();
        store.set_active_provider_thread(&thread, &old).unwrap();
        store
            .append_user_message(&thread, Some(&old), "Continue the original task")
            .unwrap();
        let handoff = rebind_thread(&store, &thread, Driver::Claude, Driver::Codex, None).unwrap();
        assert_eq!(handoff.carried, 1);
        let history: Vec<_> = store
            .events(&thread, None, 100)
            .unwrap()
            .into_iter()
            .map(|(_, e)| e)
            .collect();
        assert_eq!(
            history
                .iter()
                .filter(|e| matches!(e.event, Event::HandoffSeeded { .. }))
                .count(),
            1
        );
        let owed = crate::chat::session::undelivered_handoff(&history).unwrap();
        assert!(owed.summary.contains("Continue the original task"));
        let provider = load_provider(&store, &thread).unwrap().unwrap();
        assert_eq!(provider.native_id, None);
        assert_eq!(provider.model, "default");
        assert!(store
            .events_by_agent(&thread, None, 100)
            .unwrap()
            .iter()
            .filter(|e| matches!(e.envelope.event, Event::HandoffSeeded { .. }))
            .all(|e| e.driver.as_deref() == Some("codex")));
    }

    /// The idle sweep stops an agent only with suspension on, the thread out of sight, nothing
    /// holding it, and at least the configured minutes unseen.
    #[test]
    fn an_agent_is_suspended_only_when_out_of_sight_idle_and_long_enough() {
        use std::time::Duration;
        let min = |m: u64| Duration::from_secs(m * 60);
        assert!(suspension_due(15, min(15), false, false));
        assert!(suspension_due(15, min(90), false, false));
        assert!(!suspension_due(15, min(14), false, false), "not yet");
        assert!(!suspension_due(15, min(90), true, false), "on show");
        assert!(
            !suspension_due(15, min(90), false, true),
            "something holds it"
        );
        assert!(!suspension_due(0, min(9999), false, false), "0 is never");
    }

    #[test]
    fn the_switch_warning_names_the_agent_and_promises_other_threads_are_untouched() {
        let (heading, body) = switch_warning("Fix the build", Driver::Agy);
        assert_eq!(heading, "Stop the Running Work and Switch to Antigravity?");
        assert!(body.starts_with("“Fix the build” is working"), "{body}");
        assert!(body.contains("every sub-agent of this thread"));
        assert!(body.contains("other threads are not touched"));
        let (_, untitled) = switch_warning("", Driver::Codex);
        assert!(untitled.starts_with("This thread is working"), "{untitled}");
        assert!(untitled.contains("Switching to Codex"));
    }

    #[test]
    fn layout_bursts_save_the_latest_value_before_a_close_barrier() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("threads.db");
        let store = Store::open(&path).unwrap();
        let writer = StoreWriter::spawn(&path).unwrap();
        let writes = LayoutWrites::default();
        for burst in 0..2 {
            let (entered, held) = std::sync::mpsc::channel();
            let (release, wait) = std::sync::mpsc::channel();
            writer
                .write("hold the writer", None, move |_| {
                    entered.send(()).unwrap();
                    wait.recv().unwrap();
                    Ok(())
                })
                .unwrap();
            held.recv_timeout(WORKER_FLUSH_WAIT).unwrap();
            for n in 0..20 {
                queue_open_layout(&writer, &writes, format!("window-{burst}-{n}")).unwrap();
            }
            assert_eq!(
                writer.pending(),
                2,
                "the held write and one coalesced placeholder"
            );
            let barrier = writer.barrier();
            release.send(()).unwrap();
            barrier.recv_timeout(WORKER_FLUSH_WAIT).unwrap();
            assert_eq!(
                store.meta(OPEN_THREADS_KEY).unwrap().as_deref(),
                Some(format!("window-{burst}-19").as_str())
            );
        }
    }

    #[test]
    fn a_discarded_layout_placeholder_allows_the_next_write() {
        let writes = LayoutWrites::default();
        assert!(writes.lock().unwrap().offer("first".into()));
        drop(LayoutTicket {
            writes: writes.clone(),
            captured: false,
        });
        assert!(writes.lock().unwrap().offer("after batch failure".into()));
        let mut captured = LayoutTicket {
            writes: writes.clone(),
            captured: false,
        };
        captured.captured = true;
        assert_eq!(
            writes.lock().unwrap().take().as_deref(),
            Some("after batch failure")
        );
        assert!(writes.lock().unwrap().offer("after capture".into()));
        drop(captured);
        let pending = writes.lock().unwrap();
        assert!(
            pending.queued,
            "the captured ticket must not cancel the next placeholder"
        );
        assert_eq!(pending.pending.as_deref(), Some("after capture"));
    }

    #[test]
    fn closing_windows_and_their_selections_are_not_saved() {
        let surviving =
            window_layout(false, true, vec!["open".into()], Some("open".into())).unwrap();
        assert!(
            window_layout(true, true, vec!["closing".into()], Some("closing".into())).is_none()
        );
        let saved = saved_open_threads(&[surviving], Some("closing".into()));
        assert_eq!(saved["open"], serde_json::json!(["open"]));
        assert!(saved["selected"].is_null());
        assert_eq!(
            saved_open_threads(&[], Some("closing".into()))["open"],
            serde_json::json!([])
        );
    }

    #[test]
    fn the_last_close_keeps_the_reopen_snapshot() {
        for (closing, has_threads, expected) in [
            (vec![true, false], vec![true, true], vec!["window-1"]),
            (vec![true], vec![true], vec!["window-0"]),
            (
                vec![true, true],
                vec![true, true],
                vec!["window-0", "window-1"],
            ),
            (vec![true, false], vec![true, false], vec![]),
        ] {
            let any_live = closing.iter().any(|closing| !closing);
            let layouts: Vec<_> = closing
                .iter()
                .zip(has_threads)
                .enumerate()
                .filter_map(|(i, (closing, has_threads))| {
                    let id = format!("window-{i}");
                    let open = if has_threads {
                        vec![id.clone()]
                    } else {
                        vec![]
                    };
                    window_layout(*closing, any_live, open, has_threads.then_some(id))
                })
                .collect();
            let saved = saved_open_threads(&layouts, expected.first().map(|id| (*id).to_owned()));
            assert_eq!(
                saved["open"],
                serde_json::json!(expected),
                "closing windows: {closing:?}"
            );
            assert_eq!(
                saved["windows"].as_array().unwrap().len(),
                if any_live {
                    closing.iter().filter(|closing| !**closing).count()
                } else {
                    closing.len()
                }
            );
        }
    }

    #[test]
    fn the_final_close_snapshot_is_saved_before_the_barrier() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("threads.db");
        let store = Store::open(&path).unwrap();
        let writer = StoreWriter::spawn(&path).unwrap();
        let writes = LayoutWrites::default();
        let (entered, held) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        writer
            .write("hold the writer", None, move |_| {
                entered.send(()).unwrap();
                wait.recv().unwrap();
                Ok(())
            })
            .unwrap();
        held.recv_timeout(WORKER_FLUSH_WAIT).unwrap();
        // The first close has left one live window. Before either window goes, both become
        // closing; the final snapshot must replace the intermediate one on the same writer.
        let remaining = window_layout(false, true, vec!["b".into()], Some("b".into())).unwrap();
        queue_open_layout(
            &writer,
            &writes,
            saved_open_threads(&[remaining], Some("b".into())).to_string(),
        )
        .unwrap();
        let at_quit: Vec<_> = ["a", "b"]
            .into_iter()
            .filter_map(|id| window_layout(true, false, vec![id.into()], Some(id.into())))
            .collect();
        let final_value = saved_open_threads(&at_quit, Some("a".into()));
        queue_open_layout(&writer, &writes, final_value.to_string()).unwrap();
        let barrier = writer.barrier();
        release.send(()).unwrap();
        barrier.recv_timeout(WORKER_FLUSH_WAIT).unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&store.meta(OPEN_THREADS_KEY).unwrap().unwrap()).unwrap();
        assert_eq!(saved, final_value);
        assert_eq!(saved["open"], serde_json::json!(["a", "b"]));
    }

    #[test]
    #[ignore = "needs a display; run alone in an isolated display"]
    fn the_last_window_keeps_its_snapshot_when_pages_detach() {
        gtk4::init().unwrap();
        adw::init().unwrap();
        crate::icons::register();
        let config_home = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", config_home.path());
        let app = adw::Application::builder()
            .application_id("org.test.ClosingLayout")
            .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk4::gio::Cancellable>).unwrap();
        let _hold = app.hold();
        let window = crate::window::AgentTerminalWindow::new(&app);
        let ctx = glib::MainContext::default();
        assert!(crate::testutil::pump_until(&ctx, 5, || window
            .imp()
            .tab_view
            .borrow()
            .is_some()));
        CHAT_RESTORED.with(|done| done.set(true));
        // Titled: a blank thread left for the next one would be discarded.
        let ids: Vec<_> = (0..2)
            .map(|i| {
                window
                    .imp()
                    .create_chat_thread(NewThread {
                        title: Some(format!("t{i}")),
                        ..NewThread::new(Driver::Claude)
                    })
                    .unwrap();
                window.imp().current_thread().unwrap()
            })
            .collect();
        window.present();
        assert!(crate::testutil::pump_until(&ctx, 5, || window.is_realized()));
        let store = app_store();
        let read_layout = || -> serde_json::Value {
            serde_json::from_str(&store.meta(OPEN_THREADS_KEY).unwrap().unwrap()).unwrap()
        };
        let before = read_layout();
        assert_eq!(before["open"], serde_json::json!(ids));
        let view = window.imp().tab_view.borrow().clone().unwrap();
        let detached = Rc::new(std::cell::Cell::new(0));
        let count = detached.clone();
        view.connect_page_detached(move |_, _, _| count.set(count.get() + 1));

        window.close();
        assert!(window.imp().is_closing(), "the real close path must run");
        assert_eq!(
            read_layout(),
            before,
            "close must take a populated snapshot"
        );
        // Keep the widgets alive and detach explicitly: GTK disposal timing must not make
        // this pass without exercising the selection and detach callbacks after closing.
        while view.n_pages() > 0 {
            view.close_page(&view.nth_page(0));
            assert_eq!(read_layout(), before, "detach must not shrink the snapshot");
        }
        assert_eq!(detached.get(), 2);
        assert!(window.imp().tabs.borrow().is_empty());
        window.imp().save_open_threads();
        window.destroy();
        crate::testutil::settle(&ctx);
        assert_eq!(read_layout(), before);
    }

    /// Only the selected page's content is visible, so window-wide restyles (focus, a monitor
    /// change while resizing) skip every other open thread; switching flips it.
    #[test]
    #[ignore = "needs a display; run alone in an isolated display"]
    fn only_the_selected_page_is_visible() {
        gtk4::init().unwrap();
        adw::init().unwrap();
        crate::icons::register();
        let config_home = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", config_home.path());
        let app = adw::Application::builder()
            .application_id("org.test.OnlySelectedVisible")
            .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk4::gio::Cancellable>).unwrap();
        let _hold = app.hold();
        CHAT_RESTORED.with(|done| done.set(true));
        let window = crate::window::AgentTerminalWindow::new(&app);
        let ctx = glib::MainContext::default();
        assert!(crate::testutil::pump_until(&ctx, 5, || window
            .imp()
            .tab_view
            .borrow()
            .is_some()));
        window.present();
        let imp = window.imp();
        for i in 0..3 {
            imp.create_chat_thread(NewThread {
                title: Some(format!("t{i}")),
                ..NewThread::new(Driver::Claude)
            })
            .unwrap();
        }
        let view = imp.tab_view.borrow().clone().unwrap();
        let visible = || -> Vec<bool> {
            (0..view.n_pages())
                .map(|i| view.nth_page(i).child().is_visible())
                .collect()
        };
        let selected = |i: i32| view.selected_page() == Some(view.nth_page(i));
        assert_eq!(view.n_pages(), 3);
        assert_eq!(visible(), (0..3).map(selected).collect::<Vec<_>>());
        assert_eq!(visible().iter().filter(|v| **v).count(), 1);
        view.set_selected_page(&view.nth_page(0));
        assert_eq!(visible(), vec![true, false, false]);
        view.set_selected_page(&view.nth_page(2));
        assert_eq!(visible(), vec![false, false, true]);
        // Closing the shown page shows the one selected next.
        view.close_page(&view.nth_page(2));
        assert!(crate::testutil::pump_until(&ctx, 5, || view.n_pages() == 2));
        assert_eq!(visible().iter().filter(|v| **v).count(), 1);
        assert!(view.selected_page().unwrap().child().is_visible());
        window.destroy();
        crate::testutil::settle(&ctx);
    }

    /// Focus follows a page switch into the page shown, and back to the widget a page had:
    /// hiding the page left drops its focus, and GTK would move it to the window's first
    /// focusable widget (here an entry outside the pages, standing in for the header and
    /// sidebar). A terminal page has nothing else to put focus back (a thread refocuses its
    /// composer itself).
    #[test]
    #[ignore = "needs a display; run alone in an isolated display"]
    fn focus_follows_a_page_switch_into_the_shown_page() {
        use adw::prelude::AdwWindowExt;
        gtk4::init().unwrap();
        adw::init().unwrap();
        let ctx = glib::MainContext::default();
        let view = adw::TabView::new();
        view.set_vexpand(true);
        wire_page_visibility(&view);
        // Two entries a page: the second is where focus was, and must come back to.
        let pages: Vec<[gtk4::Entry; 2]> = (0..2)
            .map(|_| [gtk4::Entry::new(), gtk4::Entry::new()])
            .collect();
        for [a, b] in &pages {
            let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            content.append(a);
            content.append(b);
            view.append(&content);
        }
        let outside = gtk4::Entry::new();
        let layout = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        layout.append(&outside);
        layout.append(&view);
        let window = adw::Window::new();
        window.set_default_size(400, 300);
        window.set_content(Some(&layout));
        window.present();
        assert!(crate::testutil::pump_until(&ctx, 5, || pages[0][1].is_mapped()));
        assert!(
            !pages[1][0].upcast_ref::<gtk4::Widget>().is_mapped(),
            "the unselected page is hidden"
        );
        let focused = |entry: &gtk4::Entry| {
            gtk4::prelude::RootExt::focus(&window)
                .is_some_and(|f| &f == entry.upcast_ref::<gtk4::Widget>() || f.is_ancestor(entry))
        };
        assert!(pages[0][1].grab_focus());
        view.set_selected_page(&view.nth_page(1));
        assert!(
            crate::testutil::pump_until(&ctx, 5, || focused(&pages[1][0])),
            "focus left the pages on a switch"
        );
        view.set_selected_page(&view.nth_page(0));
        assert!(
            crate::testutil::pump_until(&ctx, 5, || focused(&pages[0][1])),
            "the page did not get back the widget it last had"
        );
        // Focus outside the pages (a page picked from the sidebar) stays there.
        assert!(outside.grab_focus());
        view.set_selected_page(&view.nth_page(1));
        crate::testutil::settle(&ctx);
        assert!(focused(&outside), "focus outside the pages was moved");
        window.destroy();
        crate::testutil::settle(&ctx);
    }

    /// A thread page not yet shown holds a still skeleton: an infinite pulse on a hidden page
    /// made GTK walk the window's every style node each frame. Showing it starts the pulse
    /// while it loads.
    #[test]
    #[ignore = "needs a display; run alone in an isolated display"]
    fn an_unshown_thread_page_keeps_its_skeleton_still() {
        gtk4::init().unwrap();
        adw::init().unwrap();
        crate::icons::register();
        let config_home = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", config_home.path());
        let app = adw::Application::builder()
            .application_id("org.test.StillSkeleton")
            .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk4::gio::Cancellable>).unwrap();
        let _hold = app.hold();
        CHAT_RESTORED.with(|done| done.set(true));
        let window = crate::window::AgentTerminalWindow::new(&app);
        let ctx = glib::MainContext::default();
        assert!(crate::testutil::pump_until(&ctx, 5, || window
            .imp()
            .tab_view
            .borrow()
            .is_some()));
        window.present();
        let imp = window.imp();
        let ids: Vec<_> = (0..2)
            .map(|i| {
                imp.create_chat_thread(NewThread {
                    title: Some(format!("t{i}")),
                    ..NewThread::new(Driver::Claude)
                })
                .unwrap();
                imp.current_thread().unwrap()
            })
            .collect();
        let view = imp.tab_view.borrow().clone().unwrap();
        let page_of = |id: &str| {
            imp.tabs.borrow().iter().find_map(|t| {
                t.chat
                    .as_ref()
                    .filter(|c| c.thread == id)
                    .map(|_| t.page.clone())
            })
        };
        // Closed, then reopened without being shown: a page that was never built.
        view.close_page(&page_of(&ids[1]).unwrap());
        imp.reload_summaries();
        assert!(crate::testutil::pump_until(&ctx, 5, || page_of(&ids[1])
            .is_none()
            && imp.summary_of(&ids[1]).is_some()));
        imp.open_thread(&ids[1], false).expect("reopened");
        let skeleton = || {
            imp.tabs.borrow().iter().find_map(|t| {
                t.chat
                    .as_ref()
                    .filter(|c| c.thread == ids[1])
                    .and_then(|c| c.holder.first_child())
            })
        };
        let first = skeleton().expect("an unbuilt page holds its skeleton");
        assert!(
            first.has_css_class("skeleton-idle"),
            "a page not shown must not pulse"
        );
        imp.open_thread(&ids[1], true).expect("shown");
        assert!(
            skeleton().is_none_or(|w| !w.has_css_class("skeleton-idle")),
            "loading a shown page pulses"
        );
        window.destroy();
        crate::testutil::settle(&ctx);
    }

    #[test]
    #[ignore = "needs a display; run alone in an isolated display"]
    fn leaving_a_blank_thread_discards_it_and_a_titled_one_stays() {
        gtk4::init().unwrap();
        adw::init().unwrap();
        crate::icons::register();
        let config_home = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", config_home.path());
        let app = adw::Application::builder()
            .application_id("org.test.BlankThreads")
            .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk4::gio::Cancellable>).unwrap();
        let _hold = app.hold();
        // Before the window: one built while the last session is restored discards nothing.
        CHAT_RESTORED.with(|done| done.set(true));
        let window = crate::window::AgentTerminalWindow::new(&app);
        let ctx = glib::MainContext::default();
        assert!(crate::testutil::pump_until(&ctx, 5, || window
            .imp()
            .tab_view
            .borrow()
            .is_some()));
        window.present();
        assert!(crate::testutil::pump_until(&ctx, 5, || window.is_realized()));
        let imp = window.imp();
        let store = app_store();
        let exists = |id: &str| store.thread_summary(id).ok().flatten().is_some();

        imp.create_chat_thread(NewThread::new(Driver::Claude))
            .unwrap();
        let blank = imp.current_thread().unwrap();
        imp.create_chat_thread(NewThread {
            title: Some("named".into()),
            ..NewThread::new(Driver::Claude)
        })
        .unwrap();
        let named = imp.current_thread().unwrap();
        assert!(
            crate::testutil::pump_until(&ctx, 5, || !exists(&blank)),
            "the blank thread left behind is deleted"
        );
        assert_eq!(imp.thread_pages(), 1, "and its page closed");

        // Leaving a titled thread keeps it, page and all.
        imp.create_chat_thread(NewThread::new(Driver::Claude))
            .unwrap();
        crate::testutil::settle(&ctx);
        assert!(exists(&named));
        assert_eq!(imp.thread_pages(), 2);
        window.destroy();
        crate::testutil::settle(&ctx);
    }

    #[test]
    fn a_parked_switch_survives_a_view_build_while_detecting() {
        for (own, keep) in [
            (Availability::Detecting, true),
            (Availability::Ready("/bin/claude".into()), false),
            (Availability::Missing, false),
            (Availability::Disabled, false),
        ] {
            assert_eq!(keep_pending_switch(true, &own), keep, "{own:?}");
            assert!(!keep_pending_switch(false, &own), "a failed load: {own:?}");
        }
    }

    /// The last session's reopen offer is app-wide: every window's empty state shows it, and
    /// taking it in one window spends it in all of them.
    #[test]
    #[ignore = "needs a display; run alone in an isolated display"]
    fn the_reopen_offer_shows_in_every_window_and_is_spent_once() {
        gtk4::init().unwrap();
        adw::init().unwrap();
        crate::icons::register();
        let config_home = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", config_home.path());
        let app = adw::Application::builder()
            .application_id("org.test.ReopenOffer")
            .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk4::gio::Cancellable>).unwrap();
        let _hold = app.hold();
        CHAT_RESTORED.with(|done| done.set(true));
        let first = crate::window::AgentTerminalWindow::new(&app);
        let second = crate::window::AgentTerminalWindow::new(&app);
        let ctx = glib::MainContext::default();
        let built =
            |w: &crate::window::AgentTerminalWindow| w.imp().reopen_button.borrow().is_some();
        assert!(crate::testutil::pump_until(&ctx, 5, || built(&first) && built(&second)));
        let shown = |w: &crate::window::AgentTerminalWindow| {
            w.imp()
                .reopen_button
                .borrow()
                .as_ref()
                // Its own `visible`: the windows are never presented, so nothing is drawn.
                .filter(|b| b.property::<bool>("visible"))
                .and_then(|b| b.label())
                .map(|l| l.to_string())
        };
        assert_eq!(shown(&first), None, "nothing to offer yet");

        // A layout whose threads are gone by the time it is taken: still spent everywhere.
        LAST_SESSION.with(|s| {
            *s.borrow_mut() = Some(LastSession {
                saved: serde_json::json!({"open": ["gone-1", "gone-2"]}).to_string(),
                count: 2,
            })
        });
        first.imp().update_reopen_offer();
        second.imp().update_reopen_offer();
        let offer = Some("Reopen 2 threads from your last session".to_owned());
        assert_eq!(shown(&first), offer);
        assert_eq!(shown(&second), offer);

        second.imp().reopen_last_session();
        assert!(LAST_SESSION.with(|s| s.borrow().is_none()), "spent");
        assert_eq!(shown(&first), None, "gone from the other window too");
        assert_eq!(shown(&second), None);
        // A second click, in either window, does nothing.
        first.imp().reopen_last_session();
        crate::testutil::pump_until(&ctx, 1, || false);
        assert_eq!(first.imp().thread_pages() + second.imp().thread_pages(), 0);
        first.close();
        second.close();
    }

    #[test]
    #[ignore = "needs a display; run alone in an isolated display"]
    fn opening_a_thread_in_two_windows_keeps_one_page_and_remote_actions_reach_it() {
        gtk4::init().unwrap();
        adw::init().unwrap();
        let config_home = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", config_home.path());
        let app = adw::Application::builder()
            .application_id("org.test.ThreadOwners")
            .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk4::gio::Cancellable>).unwrap();
        let first = crate::window::AgentTerminalWindow::new(&app);
        let second = crate::window::AgentTerminalWindow::new(&app);
        let ctx = glib::MainContext::default();
        assert!(crate::testutil::pump_until(&ctx, 5, || first
            .imp()
            .tab_view
            .borrow()
            .is_some()
            && second.imp().tab_view.borrow().is_some()));
        first.present();
        assert!(crate::testutil::pump_until(&ctx, 5, || first.is_mapped()));
        CHAT_RESTORED.with(|done| done.set(true));
        let page = first
            .imp()
            .create_chat_thread(NewThread::new(Driver::Claude))
            .unwrap();
        let id = first.imp().current_thread().unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&app_store().meta(OPEN_THREADS_KEY).unwrap().unwrap()).unwrap();
        assert_eq!(saved["open"], serde_json::json!([id]));
        second.imp().reload_summaries();
        assert_eq!(second.imp().open_thread(&id, true), Some(page.clone()));
        assert_eq!(first.imp().thread_pages() + second.imp().thread_pages(), 1);
        assert_eq!(
            second
                .imp()
                .sidebar_rows()
                .iter()
                .find(|r| r.key == RowKey::Thread(id.clone()))
                .unwrap()
                .open,
            OpenState::Elsewhere
        );
        assert_eq!(
            first
                .imp()
                .tab_view
                .borrow()
                .as_ref()
                .unwrap()
                .selected_page(),
            Some(page)
        );

        // Background-only work must ask on a switch, and cancelling leaves it untouched.
        {
            let mut tabs = first.imp().tabs.borrow_mut();
            let chat = tabs.iter_mut().find_map(|t| t.chat.as_mut()).unwrap();
            chat.background = vec![BackgroundTask {
                id: "worker".into(),
                kind: Default::default(),
                tool_use_id: None,
                description: Some("Review".into()),
            }];
        }
        let switched = Rc::new(std::cell::Cell::new(false));
        let seen = switched.clone();
        assert!(!first.imp().unasked_prompt_open.get());
        first.imp().when_not_busy(&id, move |_| seen.set(true));
        assert!(!switched.get());
        // choose_future presents on its first poll, after the scheduled local future runs.
        assert!(crate::testutil::pump_until(&ctx, 5, || first
            .visible_dialog()
            .is_some_and(|dialog| dialog.is_mapped())));
        let dialog = first
            .visible_dialog()
            .unwrap()
            .downcast::<adw::AlertDialog>()
            .unwrap();
        assert!(dialog.body().contains("1 background task will stop"));
        let cancel = crate::testutil::descendants(dialog.upcast_ref())
            .into_iter()
            .filter_map(|widget| widget.downcast::<gtk4::Button>().ok())
            .find(|button| {
                button.label().as_deref() == Some("Keep Waiting")
                    || crate::testutil::descendants(button.upcast_ref())
                        .iter()
                        .any(|widget| {
                            widget
                                .downcast_ref::<gtk4::Label>()
                                .is_some_and(|label| label.text() == "Keep Waiting")
                        })
            })
            .unwrap();
        assert!(
            crate::testutil::pump_until(&ctx, 5, || cancel.is_mapped() && cancel.is_sensitive())
        );
        crate::testutil::settle(&ctx);
        // Activate the real cancel response: choose_future waits on AlertDialog::response.
        cancel.emit_clicked();
        assert!(crate::testutil::pump_until(&ctx, 5, || !first
            .imp()
            .unasked_prompt_open
            .get()));
        assert!(crate::testutil::pump_until(&ctx, 5, || first
            .visible_dialog()
            .is_none_or(
                |open| open != dialog.clone().upcast::<adw::Dialog>()
            )));
        // A mapped dismissal must also deliver the configured close response on 1.5.
        let seen = switched.clone();
        first.imp().when_not_busy(&id, move |_| seen.set(true));
        assert!(crate::testutil::pump_until(&ctx, 5, || first
            .visible_dialog()
            .is_some_and(|dialog| dialog.is_mapped())));
        let dismissed = first
            .visible_dialog()
            .unwrap()
            .downcast::<adw::AlertDialog>()
            .unwrap();
        assert_eq!(dismissed.close_response(), "cancel");
        crate::testutil::settle(&ctx);
        assert!(dismissed.close(), "the mapped question must close");
        assert!(crate::testutil::pump_until(&ctx, 5, || !first
            .imp()
            .unasked_prompt_open
            .get()));
        assert!(crate::testutil::pump_until(&ctx, 5, || first
            .visible_dialog()
            .is_none_or(
                |open| open != dismissed.clone().upcast::<adw::Dialog>()
            )));
        assert!(!switched.get(), "cancelling must not run the switch");
        assert_eq!(
            first
                .imp()
                .tabs
                .borrow()
                .iter()
                .find_map(|t| t.chat.as_ref())
                .unwrap()
                .background
                .len(),
            1
        );
        first.imp().closing_started.set(true);
        second.present();
        assert!(second.imp().open_thread(&id, true).is_none());
        assert!(crate::testutil::pump_until(&ctx, 5, || {
            crate::testutil::descendants(second.upcast_ref())
                .iter()
                .any(|w| {
                    w.downcast_ref::<gtk4::Label>()
                        .is_some_and(|l| l.text() == "This thread is closing in another window")
                })
        }));
        second.imp().save_open_threads();
        let saved: serde_json::Value =
            serde_json::from_str(&app_store().meta(OPEN_THREADS_KEY).unwrap().unwrap()).unwrap();
        assert_eq!(saved["open"], serde_json::json!([]));
        second.imp().close_row_now(&RowKey::Thread(id.clone()));
        assert!(first.imp().page_of_thread(&id).is_none());
        assert!(thread_owner(&id).is_none());
        first.destroy();
        second.destroy();
    }

    #[test]
    fn registry_returns_the_window_already_showing_a_thread() {
        let first = Rc::new("first window");
        let second = Rc::new("second window");
        let mut registry = HashMap::new();
        registry.insert("t".into(), Rc::downgrade(&first));
        let found = registry_owner(&mut registry, "t", std::rc::Weak::upgrade).unwrap();
        assert!(Rc::ptr_eq(&found, &first));
        assert!(!Rc::ptr_eq(&found, &second));
        drop(found);
        drop(first);
        assert!(registry_owner(&mut registry, "t", std::rc::Weak::upgrade).is_none());
        assert!(!registry.contains_key("t"));
    }

    #[test]
    fn open_threads_from_two_windows_are_both_saved() {
        let layouts = vec![
            serde_json::json!({"open": ["a"], "selected": "a"}),
            serde_json::json!({"open": ["b", "a"], "selected": "b"}),
        ];
        let saved = saved_open_threads(&layouts, Some("b".into()));
        assert_eq!(saved["windows"], serde_json::json!(layouts));
        assert_eq!(saved["open"], serde_json::json!(["a", "b"]));
        assert_eq!(saved["selected"], "b");
        let known = vec!["a".into(), "b".into()];
        let new_only = serde_json::json!({"windows": layouts});
        assert_eq!(restorable(&new_only, &known, 5), known);
        assert_eq!(restorable(&new_only, &known, 1), ["a"]);
        assert_eq!(selected_to_restore(&saved, &known), Some("b".into()));
    }

    #[test]
    fn the_old_open_threads_shape_still_restores() {
        let saved = serde_json::json!({"open": ["gone", "b", "a", "b"], "selected": "b"});
        let known = vec!["a".into(), "b".into()];
        let open = restorable(&saved, &known, 10);
        assert_eq!(open, ["b", "a"]);
        assert_eq!(selected_to_restore(&saved, &open), Some("b".into()));
    }

    #[test]
    fn closing_prevents_session_start() {
        let window = AgentTerminalWindow::default();
        assert!(window.session_start_allowed());
        window.closing_started.set(true);
        assert!(!window.session_start_allowed());
    }

    #[test]
    fn a_stale_scan_does_not_overwrite_a_newer_one() {
        SCAN_GENERATION.with(|current| {
            current.set(1);
            assert!(scan_is_current(1));
            current.set(2); // a scan requested by another window supersedes it too
            assert!(!scan_is_current(1));
            assert!(scan_is_current(2));
        });
    }

    #[test]
    fn a_pending_first_prompt_is_restored_without_being_stored_as_sent() {
        let store = Store::open_in_memory().unwrap();
        let ids = NewThreadIds {
            thread: "thread".into(),
            provider: "provider".into(),
        };
        record_new_thread_as(&store, &ids, &NewThreadSpec::new("/tmp", Driver::Claude)).unwrap();
        let text = "Please continue\nPASSWORD=private-value";
        let prompt = agent_core::redact::redact(text);
        save_pending_prompt(&store, &ids.thread, text).unwrap();
        let loaded = load_thread(&store, &ids.thread, None).unwrap();
        assert_eq!(loaded.pending_prompt, Some(prompt));
        assert!(loaded.history.is_empty());
        assert!(!store.has_messages(&ids.thread).unwrap());
        assert!(!loaded.pending_prompt.unwrap().contains("private-value"));
        store
            .append_user_message(&ids.thread, Some(&ids.provider), "Please continue")
            .unwrap();
        assert!(load_thread(&store, &ids.thread, None)
            .unwrap()
            .pending_prompt
            .is_none());
        assert_eq!(
            store
                .meta(&pending_prompt_key(&ids.thread))
                .unwrap()
                .as_deref(),
            None
        );
        save_pending_prompt(&store, &ids.thread, text).unwrap();
        store.delete_thread(&ids.thread).unwrap();
        assert_eq!(
            store
                .meta(&pending_prompt_key(&ids.thread))
                .unwrap()
                .as_deref(),
            None
        );
    }

    #[test]
    fn only_agent_questions_and_approvals_are_interruptions() {
        use agent_core::event::QuestionSource;
        let question = |source| Event::QuestionRequested {
            questions: Vec::new(),
            source,
        };
        assert!(!is_interruption(&question(QuestionSource::Reply)));
        assert!(is_interruption(&question(QuestionSource::Agent)));
        assert!(is_interruption(&Event::ApprovalRequested {
            tool: "Bash".into(),
            title: None,
            input: serde_json::Value::Null,
            reason: None,
            options: Vec::new(),
            response: agent_core::event::ResponseCapability::Live,
            remembers: None,
        }));
        assert!(!is_interruption(&Event::ApprovalExpired));
    }

    pub(crate) fn sidebar_focus_survives_refresh(window: &crate::window::AgentTerminalWindow) {
        let imp = window.imp();
        let sidebar = imp.sidebar.borrow().clone().expect("sidebar");
        let key = sidebar
            .keys
            .borrow()
            .iter()
            .find_map(Clone::clone)
            .expect("a thread row");
        let close_for_key = || {
            let index = sidebar
                .keys
                .borrow()
                .iter()
                .position(|row| row.as_ref() == Some(&key))?;
            let row = sidebar.list.row_at_index(i32::try_from(index).ok()?)?;
            row.child()?
                .last_child()
                .filter(|button| button.has_css_class("thread-close"))
        };
        window.set_default_size(950, 650);
        window.present();
        if !imp
            .split_view
            .borrow()
            .as_ref()
            .is_some_and(|split| split.shows_sidebar())
        {
            imp.toggle_sidebar();
        }
        let ctx = glib::MainContext::default();
        let accepted_focus = crate::testutil::pump_until(&ctx, 5, || {
            close_for_key().is_some_and(|button| {
                button.is_mapped() && button.is_sensitive() && button.grab_focus()
            })
        });
        assert!(
            accepted_focus,
            "the shown sidebar's current close button must accept focus after revealing"
        );
        // Mapping happens before the split-view reveal finishes accepting focus. State and
        // time callbacks can also rebuild rows; each attempt resolves the current control.
        let focus = gtk4::prelude::GtkWindowExt::focus(window).expect("focused control");
        assert_eq!(
            sidebar_focus_target(&sidebar, &focus),
            Some((key.clone(), Some("thread-close")))
        );
        imp.refresh_sidebar();
        let focus = gtk4::prelude::GtkWindowExt::focus(window).expect("focus after refresh");
        assert_eq!(
            sidebar_focus_target(&sidebar, &focus),
            Some((key, Some("thread-close")))
        );
    }

    /// A thread page drives a real agent process end to end: the deterministic Codex fixture
    /// (`tests/support/scripted_codex.py`; no provider, tool or network). Its session starts,
    /// a turn runs and finishes, an approval marks the thread busy and needing the user in the
    /// sidebar, and Stop clears both. Run from the window walkthrough (GTK has one thread).
    pub(crate) fn a_thread_page_runs_a_real_session(window: &crate::window::AgentTerminalWindow) {
        use crate::chat::ChatBackend;
        use crate::testutil::pump_until;
        // The app's own log lines (why a thread opened with or without a session) go to the
        // test's output, so a failure here explains itself.
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_max_level(tracing::Level::DEBUG)
            .try_init();
        let imp = window.imp();
        let ctx = glib::MainContext::default();
        let dir = tempfile::tempdir().expect("tmp");
        let folder = dir.path().to_string_lossy().into_owned();
        let frames =
            || std::fs::read_to_string(dir.path().join("frames.ndjson")).unwrap_or_default();

        // Codex resolves to python3 running the fixture, which keeps its frames in `dir`. The
        // profile gets a command of its own: the resolution cache is keyed by command, and a
        // Codex page an earlier check opened may still be resolving the old one, in the
        // background, and would overwrite a seeded entry under it ("not found").
        let (old_command, env_file) = {
            let config = imp.config.borrow();
            let codex = config
                .agent_profile(Driver::Codex)
                .expect("a Codex profile");
            (codex.command.clone(), codex.env_file.clone())
        };
        // Still named `codex`: the window tells a profile's agent by its command's file name, and
        // a profile it cannot place falls back to the default one, whose plain `codex` would
        // resolve to a real install on PATH.
        let command = "/nonexistent/scripted-fixture/codex".to_owned();
        for profile in imp.config.borrow_mut().profiles.iter_mut() {
            if profile.command == old_command {
                profile.command = command.clone();
                profile.args = vec![
                    "-I".into(),
                    format!(
                        "{}/tests/support/scripted_codex.py",
                        env!("CARGO_MANIFEST_DIR")
                    ),
                    folder.clone(),
                ];
            }
        }
        RESOLVED.with(|r| {
            r.borrow_mut().insert(
                (command.clone(), env_file.clone()),
                ResolvedAgent {
                    program: Some("/usr/bin/python3".into()),
                    env: Vec::new(),
                    env_failed: false,
                },
            )
        });
        // Never a real agent: the profile the window will use for Codex must be this one, and
        // resolve to python3 running the fixture, before anything is opened.
        {
            let config = imp.config.borrow();
            let used = config
                .agent_profile(Driver::Codex)
                .expect("a Codex profile");
            assert_eq!(
                used.command, command,
                "the window would use another profile"
            );
            assert_eq!(
                cached_agent(used).and_then(|r| r.program).as_deref(),
                Some("/usr/bin/python3"),
                "the fixture's profile must resolve to python3"
            );
        }
        let availability = AgentAvailability::shared();
        let before = availability.get(Driver::Codex);
        availability.set(
            Driver::Codex,
            Availability::Ready("/usr/bin/python3".into()),
        );

        imp.new_chat_thread(Some(Driver::Codex), Some(folder.clone()), None);
        let thread = imp
            .tabs
            .borrow()
            .last()
            .and_then(|t| t.chat.as_ref().map(|c| c.thread.clone()))
            .expect("a thread page");
        let state = |f: &dyn Fn(&ChatTab) -> bool| {
            imp.tabs
                .borrow()
                .iter()
                .filter_map(|t| t.chat.as_ref())
                .find(|c| c.thread == thread)
                .is_some_and(f)
        };
        let slot = imp.slot_of(&thread).expect("the page's slot");
        let page_state = || {
            imp.tabs
                .borrow()
                .iter()
                .filter_map(|t| t.chat.as_ref())
                .find(|c| c.thread == thread)
                .map(|c| {
                    format!(
                        "view built {}, building {}, starting {}, unavailable {}",
                        c.view.is_some(),
                        c.building,
                        c.starting,
                        c.unavailable
                    )
                })
        };
        assert!(
            pump_until(&ctx, 15, || slot.get().is_some()),
            "the session starts; Codex is {:?}; resolved now: {:?}; page: {:?}; fixture frames: {}",
            availability.get(Driver::Codex),
            imp.config
                .borrow()
                .agent_profile(Driver::Codex)
                .and_then(cached_agent)
                .map(|r| r.program),
            page_state(),
            frames()
        );
        // Nothing is sent until the fixture itself has answered: it counts its connections
        // into `dir` the moment it starts. Anything else that started fails here, unprompted.
        assert!(
            pump_until(&ctx, 15, || dir.path().join("connections").exists()),
            "the process that started is the fixture"
        );

        // A plain turn: sent to the agent, finished, and nothing left needing the user.
        slot.send_prompt("success");
        assert!(
            pump_until(&ctx, 15, || frames().contains("turn/completed")
                && state(&|c| !c.running)),
            "the turn finishes; fixture frames: {}",
            frames()
        );
        assert!(
            app_store().has_messages(&thread).expect("read"),
            "the prompt is stored"
        );
        assert!(!imp.row_is_busy(&RowKey::Thread(thread.clone())));

        delegation::tests::cancelled_start_can_retry(window);
        delegation::tests::transfer_keeps_live_view_routes(window, &thread, &slot);

        // An approval: the thread is busy and asks for the user, in its page and its row.
        slot.send_prompt("approve this");
        assert!(
            pump_until(&ctx, 15, || state(&|c| c.approval)),
            "the approval reaches the page; fixture frames: {}",
            frames()
        );
        assert!(
            imp.row_is_busy(&RowKey::Thread(thread.clone())),
            "closing would cut work short"
        );
        imp.reload_summaries();
        let badge = || {
            imp.sidebar_rows()
                .into_iter()
                .find(|r| r.key == RowKey::Thread(thread.clone()))
                .and_then(|r| r.badge)
        };
        assert_eq!(badge(), Some(Badge::NeedsApproval));

        // Stop: the agent is told, the turn ends, and nothing is left waiting.
        slot.interrupt();
        assert!(
            pump_until(&ctx, 15, || state(&|c| !c.approval && !c.running)),
            "stop ends the turn; fixture frames: {}",
            frames()
        );
        assert!(frames().contains("turn/interrupt"));
        assert!(!imp.row_is_busy(&RowKey::Thread(thread.clone())));
        assert_ne!(badge(), Some(Badge::NeedsApproval));

        // Busy again, for what must ask before it cuts the turn short.
        slot.send_prompt("approve again");
        assert!(
            pump_until(&ctx, 15, || state(&|c| c.approval)),
            "a second approval"
        );
        let key = RowKey::Thread(thread.clone());
        // Answers the open dialog with `heading` by clicking its `button`, as a user does; the
        // dialog closes itself.
        let answer = |heading: &str, button: &str| {
            let open = || {
                adw::prelude::AdwApplicationWindowExt::visible_dialog(window)
                    .and_then(|d| d.downcast::<adw::AlertDialog>().ok())
                    .filter(|d| d.heading().as_deref() == Some(heading))
            };
            assert!(pump_until(&ctx, 5, || open().is_some()), "asks: {heading}");
            let dialog = open().expect("the dialog");
            let target = crate::testutil::descendants(dialog.upcast_ref())
                .into_iter()
                .filter_map(|w| w.downcast::<gtk4::Button>().ok())
                .find(|b| {
                    b.label().as_deref() == Some(button)
                        || crate::testutil::descendants(b.upcast_ref())
                            .iter()
                            .any(|w| {
                                w.downcast_ref::<gtk4::Label>()
                                    .is_some_and(|l| l.text() == button)
                            })
                })
                .unwrap_or_else(|| panic!("{heading} has a {button} button"));
            // A dialog still animating in ignores its buttons: click only once it is shown.
            assert!(
                pump_until(&ctx, 10, || target.is_mapped() && target.is_sensitive()),
                "{heading}'s {button} button is shown"
            );
            crate::testutil::settle(&ctx);
            target.emit_clicked();
            // Closing is animated: wait for it to go (bounded), never a fixed number of turns.
            assert!(
                pump_until(&ctx, 10, || {
                    adw::prelude::AdwApplicationWindowExt::visible_dialog(window)
                        .is_none_or(|d| d != dialog.clone().upcast::<adw::Dialog>())
                }),
                "{heading} closes once answered"
            );
        };

        // Fork while busy: a second thread on the same agent and folder; this one is untouched.
        let pages = imp.tabs.borrow().len();
        imp.view_action(&thread, &ViewAction::Fork);
        assert!(
            pump_until(&ctx, 15, || imp.tabs.borrow().len() == pages + 1),
            "a fork opens"
        );
        let fork = {
            let tabs = imp.tabs.borrow();
            let new = tabs.last().expect("the fork's page");
            let chat = new.chat.as_ref().expect("a thread");
            assert_ne!(chat.thread, thread);
            assert_eq!(
                (chat.driver, new.dir.as_str()),
                (Driver::Codex, folder.as_str())
            );
            chat.thread.clone()
        };
        assert!(state(&|c| c.approval), "forking leaves the source waiting");
        imp.close_row_now(&RowKey::Thread(fork));

        // Closing a busy thread asks once (a second close while it asks adds nothing); Cancel
        // keeps the thread and its turn.
        imp.close_row(&key);
        imp.close_row(&key);
        answer("Stop the Running Turn and Close?", "Cancel");
        // A second question would be the next dialog up, and it would not go away by itself.
        crate::testutil::settle(&ctx);
        assert!(
            adw::prelude::AdwApplicationWindowExt::visible_dialog(window).is_none(),
            "the second close asked nothing more"
        );
        assert!(state(&|c| c.approval), "cancel keeps the thread waiting");

        // Switching agents mid-turn asks too; Keep Waiting changes nothing.
        imp.view_action(
            &thread,
            &ViewAction::Handoff {
                target: Some(Driver::Claude),
            },
        );
        answer("Switch agents now?", "Keep Waiting");
        assert_eq!(slot.get().map(|s| s.status().driver), Some(Driver::Codex));
        assert!(state(&|c| c.approval));

        // Stop and Close: the page goes, and the thread with it from the open pages.
        imp.close_row(&key);
        answer("Stop the Running Turn and Close?", "Stop and Close");
        assert!(
            pump_until(&ctx, 5, || imp.page_of_thread(&thread).is_none()),
            "stop and close closes the page"
        );
        availability.set(Driver::Codex, before);
        RESOLVED.with(|r| r.borrow_mut().remove(&(command.clone(), env_file)));
        for profile in imp.config.borrow_mut().profiles.iter_mut() {
            if profile.command == command {
                profile.command = old_command.clone();
            }
        }
    }

    /// A refresh with the same rows, an aged clock, or only the focus changed keeps the row
    /// widgets (and so focus, an open menu and a rename in them); only the age and the glow move.
    pub(crate) fn sidebar_updates_in_place(window: &crate::window::AgentTerminalWindow) {
        let imp = window.imp();
        let sidebar = imp.sidebar.borrow().clone().expect("sidebar");
        if let Some(tab) = imp.tabs.borrow_mut().get_mut(0) {
            if let Some(chat) = tab.chat.as_mut() {
                chat.approval = true;
            }
        }
        imp.refresh_sidebar();
        let model = sidebar.drawn.borrow().clone().expect("drawn model");
        let (index, waiting) = model
            .rows()
            .enumerate()
            .find(|(_, r)| r.badge == Some(Badge::NeedsApproval))
            .expect("a row waiting for approval");
        let widget = || sidebar.drawn_rows.borrow()[index].row.clone();
        let before = widget();

        // The same rows, a clock three hours on: the widget stays, its age moves.
        CLOCK_SKEW_MS.with(|s| s.set(3 * 3_600_000));
        imp.refresh_sidebar();
        CLOCK_SKEW_MS.with(|s| s.set(0));
        assert_eq!(widget(), before, "an age tick keeps the row widgets");
        let age = sidebar.drawn_rows.borrow()[index].age.label();
        let expected = relative_time(now_ms() + 3 * 3_600_000, waiting.updated_ms);
        assert_eq!(age, expected, "the age label moved on");

        // Focus and the shown row decide the glow and nothing else.
        let now = sidebar_now();
        let restyle = |selected: Option<RowKey>, focused: bool| {
            let next = SidebarModel {
                selected,
                focused,
                ..model.clone()
            };
            imp.update_sidebar_in_place(&sidebar, &next, Redraw::Restyle, now);
        };
        restyle(None, false);
        assert!(widget().has_css_class("needs-attention"));
        restyle(Some(waiting.key.clone()), true);
        assert!(
            !widget().has_css_class("needs-attention"),
            "looking at it clears the glow"
        );
        assert_eq!(sidebar.list.selected_row(), Some(widget()));
        restyle(None, true);
        assert!(widget().has_css_class("needs-attention"));
        assert_eq!(widget(), before, "restyling keeps the row widgets");
        imp.refresh_sidebar();
    }

    /// Times `refresh_sidebar` over a realistic list (75 stored threads) for each trigger the
    /// plan names: a repeat with nothing changed, the 60 s age tick, and a focus change.
    /// Prints the numbers (`cargo test -- --nocapture`).
    pub(crate) fn sidebar_refresh_cost(window: &crate::window::AgentTerminalWindow) {
        use agent_kit::store::ThreadSummary;
        let imp = window.imp();
        let saved = imp.summaries.borrow().clone();
        let now = now_ms();
        let list: Vec<ThreadSummary> = (0..75)
            .map(|i| ThreadSummary {
                id: format!("bench-{i}"),
                title: format!("Bench thread {i}"),
                cwd: format!("/tmp/bench-{}", i % 9),
                updated_at: now - i64::from(i) * 1_700_000,
                driver: Some(if i % 2 == 0 { "claude" } else { "agy" }.to_owned()),
                model: None,
                unread: i % 7 == 0,
                archived: false,
            })
            .collect();
        *imp.summaries.borrow_mut() = Some(list);
        imp.refresh_sidebar();
        let time = |what: &str, runs: u32, step: &dyn Fn(u32)| {
            let started = std::time::Instant::now();
            for n in 0..runs {
                step(n);
            }
            eprintln!(
                "sidebar bench: {what}: {:.3} ms per call over {runs}",
                started.elapsed().as_secs_f64() * 1000.0 / f64::from(runs)
            );
        };
        time("repeat, nothing changed", 20, &|_| imp.refresh_sidebar());
        time("age tick (clock 2 min on)", 20, &|n| {
            CLOCK_SKEW_MS.with(|s| s.set(i64::from(n + 1) * 120_000));
            imp.refresh_sidebar();
        });
        CLOCK_SKEW_MS.with(|s| s.set(0));
        *imp.summaries.borrow_mut() = saved;
        imp.refresh_sidebar();
    }

    /// A thread menu hangs off the sidebar list, so a rebuild while it is open must clear the
    /// rows, return, and leave the menu where it was.
    #[test]
    #[ignore = "needs a display; run alone: cargo test clear_rows -- --ignored"]
    fn clear_rows_ends_and_keeps_an_open_menu() {
        gtk4::init().expect("GTK init");
        let list = gtk4::ListBox::new();
        for _ in 0..3 {
            list.append(&gtk4::Label::new(Some("row")));
        }
        let menu = gtk4::PopoverMenu::from_model(None::<&gtk4::gio::MenuModel>);
        menu.set_parent(&list);

        clear_rows(&list);

        assert!(list.row_at_index(0).is_none(), "every row is gone");
        assert_eq!(
            menu.parent().as_ref(),
            Some(list.upcast_ref::<gtk4::Widget>()),
            "the menu survives the rebuild"
        );
        menu.unparent();
    }

    fn native(
        driver: Driver,
        id: &str,
        cwd: Option<&str>,
        modified: i64,
    ) -> agent_kit::native::NativeSession {
        agent_kit::native::NativeSession {
            driver,
            native_id: id.into(),
            cwd: cwd.map(str::to_owned),
            title: Some(format!("session {id}")),
            modified,
        }
    }

    #[test]
    fn opening_a_thread_on_a_native_session_imports_its_history_once() {
        const TRANSCRIPT: &str = include_str!(
            "../../../crates/agent-core/tests/fixtures/claude-transcript-synthetic.jsonl"
        );
        let home = tempfile::tempdir().expect("home");
        let project = home.path().join(".claude/projects/-repo");
        std::fs::create_dir_all(&project).expect("projects");
        let id = "0b3c6c1e-5d2a-4f0e-9a1b-2c3d4e5f6a7b";
        std::fs::write(project.join(format!("{id}.jsonl")), TRANSCRIPT).expect("transcript");

        let store = Store::open_in_memory().expect("store");
        let thread = store
            .link_native_thread("/repo", None, 1_000, "claude", id)
            .expect("link");
        let load = |thread: &str, home: Option<&std::path::Path>| {
            load_thread(&store, thread, home).expect("load")
        };
        let first = load(&thread, Some(home.path()));
        assert!(
            first.history.iter().any(|(_, e)| matches!(
                e.event,
                Event::ItemStarted {
                    kind: ItemKind::UserMessage,
                    ..
                }
            )),
            "the conversation shows"
        );
        assert!(
            first
                .history
                .iter()
                .all(|(driver, _)| *driver == Some(Driver::Claude)),
            "imported events are credited to the session's agent"
        );
        // Opened again: read from the store, never imported twice.
        let again = load(&thread, Some(home.path()));
        assert_eq!(again.history.len(), first.history.len());
        // No home (the memory-only store, tests): nothing is read from disk.
        let other = store
            .link_native_thread(
                "/repo",
                None,
                1_000,
                "claude",
                "1b3c6c1e-5d2a-4f0e-9a1b-2c3d4e5f6a7b",
            )
            .expect("link");
        assert!(load(&other, None).history.is_empty());

        // A transcript that is not there: one notice, stored, so it is not retried every open.
        let missing = load(&other, Some(home.path()));
        assert!(matches!(
            missing.history.as_slice(),
            [(_, e)] if matches!(&e.event, Event::Notice { text } if text.contains("could not be loaded"))
        ));
        assert_eq!(load(&other, Some(home.path())).history.len(), 1);
    }

    #[test]
    fn recent_sessions_become_threads_once_in_session_order_and_never_return_after_a_delete() {
        let store = Store::open_in_memory().expect("store");
        let sessions = [
            native(Driver::Claude, "c-new", Some("/w/new"), 2_000),
            native(Driver::Agy, "a-old", None, 1_000),
        ];
        assert_eq!(
            link_recent_sessions(&store, &sessions, "/home/u"),
            [Driver::Claude, Driver::Agy],
            "each one added, by its agent (the toast names them)"
        );
        let threads = store.list_threads(false).expect("list");
        let titles: Vec<_> = threads.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(
            titles,
            ["session c-new", "session a-old"],
            "newest session first"
        );
        assert_eq!(threads[0].cwd, "/w/new");
        assert_eq!(
            threads[1].cwd, "/home/u",
            "no recorded folder: the fallback"
        );
        assert_eq!(threads[1].driver.as_deref(), Some("agy"));
        // The thread resumes the session: its provider thread carries the native id.
        let pt = load_provider(&store, &threads[0].id)
            .expect("read")
            .expect("provider");
        assert_eq!(pt.native_id.as_deref(), Some("c-new"));

        // A second scan adds nothing.
        assert!(link_recent_sessions(&store, &sessions, "/home/u").is_empty());
        // A deleted one stays gone.
        store.delete_thread(&threads[1].id).expect("delete");
        assert!(link_recent_sessions(&store, &sessions, "/home/u").is_empty());
        assert_eq!(store.list_threads(true).expect("list").len(), 1);
    }

    #[test]
    fn a_rescan_keeps_found_binaries_that_still_exist_and_looks_again_for_the_rest() {
        let dir = tempfile::tempdir().expect("tmp");
        let present = dir.path().join("claude");
        std::fs::write(&present, "").expect("file");
        let agent = |program: Option<String>| ResolvedAgent {
            program,
            env: Vec::new(),
            env_failed: false,
        };
        RESOLVED.with(|r| {
            let mut r = r.borrow_mut();
            r.clear();
            r.insert(
                ("claude".into(), None),
                agent(Some(present.display().to_string())),
            );
            r.insert(("agy".into(), None), agent(None));
            r.insert(
                ("codex".into(), None),
                agent(Some("/nonexistent/codex".into())),
            );
        });
        forget_stale_resolutions();
        let kept: Vec<String> = RESOLVED.with(|r| r.borrow().keys().map(|k| k.0.clone()).collect());
        assert_eq!(
            kept,
            ["claude"],
            "missing and vanished entries are probed again"
        );
        RESOLVED.with(|r| r.borrow_mut().clear());
    }

    #[test]
    fn an_agent_that_is_not_ready_is_never_launched() {
        for (state, text) in [
            (Availability::Missing, "Claude is not available."),
            (Availability::Disabled, "Claude is switched off."),
            (Availability::Detecting, "Claude is still being detected."),
        ] {
            let refused = agent_launch_with(&state, Driver::Claude, "t", "/w", None);
            assert_eq!(refused.err().as_deref(), Some(text));
        }
    }

    #[test]
    fn the_catalogue_wait_ends_at_the_limit_or_as_soon_as_the_list_is_fresh() {
        use std::time::Duration;
        let ctx = glib::MainContext::new();
        let step = Duration::from_millis(5);
        // Never fresh: gives up at the limit (and does not hang).
        let started = std::time::Instant::now();
        let fresh = ctx.block_on(wait_until(Duration::from_millis(40), step, || false));
        assert!(!fresh);
        assert!(started.elapsed() < Duration::from_secs(2));
        // Fresh after a few polls: returns true well before the limit.
        let polls = std::cell::Cell::new(0);
        let fresh = ctx.block_on(wait_until(Duration::from_secs(5), step, || {
            polls.set(polls.get() + 1);
            polls.get() > 3
        }));
        assert!(fresh);
        // Already fresh: no waiting at all.
        assert!(ctx.block_on(wait_until(Duration::ZERO, step, || true)));
    }

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
            thread: None,
            switch_serial: Rc::new(std::cell::Cell::new(0)),
            start_attempt: std::cell::Cell::new(0),
            worker_subscriber: RefCell::new(None),
            worker_diffs: RefCell::new(HashMap::new()),
            session: RefCell::new(None),
            view: RefCell::new(None),
            driver: std::cell::Cell::new(Driver::Agy),
            model: Some("gemini".into()),
            mode: Mode::Plan,
        };
        let status = slot.status();
        assert_eq!(status.driver, Driver::Agy);
        assert!(!status.alive);
        assert_eq!(slot.control(Control::Usage), "ctl-unstarted");
        let codex = SessionSlot {
            thread: None,
            switch_serial: Rc::new(std::cell::Cell::new(0)),
            start_attempt: std::cell::Cell::new(0),
            worker_subscriber: RefCell::new(None),
            worker_diffs: RefCell::new(HashMap::new()),
            session: RefCell::new(None),
            view: RefCell::new(None),
            driver: std::cell::Cell::new(Driver::Codex),
            model: None,
            mode: Mode::Ask,
        };
        assert_eq!(codex.status().driver, Driver::Codex);
        assert!(codex.status().capabilities.live_approvals);
    }

    #[test]
    fn a_thread_starts_in_the_chosen_mode_before_any_default() {
        use Mode::{AcceptEdits, Ask, Plan};
        assert_eq!(start_mode(Some(Ask), Some(Plan), Some(AcceptEdits)), Ask);
        assert_eq!(start_mode(None, Some(Plan), Some(AcceptEdits)), Plan);
        assert_eq!(start_mode(None, None, Some(AcceptEdits)), AcceptEdits);
        assert_eq!(start_mode(None, None, None), Ask);
        // A mode carried to a new thread (fork, continue) is stored as the user's choice, so it
        // beats the profile's default on the new thread's first start.
        let carried = [Envelope::new(Event::ModeChosen { mode: Ask })];
        let chosen = crate::chat::session::chosen_mode(carried.iter());
        assert_eq!(start_mode(chosen, None, Some(Plan)), Ask);
    }

    #[test]
    fn a_retired_model_is_replaced_only_on_a_fresh_list_and_the_move_is_recorded() {
        use agent_core::catalog::CatalogModel;
        let model = |id: &str| CatalogModel {
            driver: Driver::Claude,
            id: id.into(),
            display: id.to_uppercase(),
            description: None,
            efforts: vec!["low".into(), "high".into()],
            default_effort: None,
            via: None,
        };
        let list = vec![model("default"), model("sonnet")];
        let run =
            |fresh, m: &str| decide_live_model(&list, fresh, Some(m.into()), Some("high".into()));
        // Cached or loading: left alone, even if it looks gone.
        assert_eq!(
            run(false, "claude-sonnet-3-7").0.as_deref(),
            Some("claude-sonnet-3-7")
        );
        // A listed model is left alone.
        assert_eq!(run(true, "sonnet").0.as_deref(), Some("sonnet"));
        assert!(run(true, "sonnet").2.is_none());
        // A retired one is replaced, keeping the effort the target offers.
        let (m, e, moved) = run(true, "claude-sonnet-3-7");
        assert_eq!((m.as_deref(), e.as_deref()), (Some("sonnet"), Some("high")));
        assert_eq!(moved.expect("recorded").model, "claude-sonnet-3-7");
        // No model at all is the agent's own default: nothing to port.
        assert_eq!(
            decide_live_model(&list, true, None, None),
            (None, None, None)
        );
    }

    fn sonnet() -> agent_core::catalog::CatalogModel {
        agent_core::catalog::CatalogModel {
            driver: Driver::Claude,
            id: "sonnet".into(),
            display: "Sonnet".into(),
            description: None,
            efforts: vec!["low".into(), "high".into()],
            default_effort: None,
            via: None,
        }
    }

    /// A catalogue holding only a recent cached Claude list (fetched a minute ago, last run).
    fn cached_catalog(dir: &std::path::Path) -> Rc<ModelCatalog> {
        let path = dir.join("models.json");
        let cache = serde_json::json!({
            "claude": [sonnet()],
            "read_at": {"claude": crate::freshness::now_epoch() - 60},
        });
        std::fs::write(&path, cache.to_string()).unwrap();
        Rc::new(ModelCatalog::with_cache(Some(path)))
    }

    /// A start from a list fetched in this run does not wait.
    #[test]
    fn a_thread_started_after_this_runs_fetch_does_not_wait() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = cached_catalog(dir.path());
        catalog.land_fetch(Driver::Claude, vec![sonnet()]);
        let ctx = glib::MainContext::new();
        let started = std::time::Instant::now();
        assert!(ctx.block_on(wait_for_catalog(&catalog, Driver::Claude, CATALOG_WAIT)));
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
    }

    /// A cached list, however recent, is neither a reason to skip the wait nor to port a model
    /// in argv: the start waits (at most CATALOG_WAIT) for this run's fetch, which then ports a
    /// retired model before it reaches argv.
    #[test]
    fn a_thread_started_from_only_a_cached_list_waits_then_ports_a_retired_model() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = cached_catalog(dir.path());
        assert_eq!(catalog.models_of(Driver::Claude).len(), 1, "cache read");
        assert!(
            !catalog.is_settled(Driver::Claude),
            "cache alone does not settle"
        );
        // On the cached list alone, nothing is ported.
        let retired = || {
            (
                Some("claude-sonnet-3-7".to_owned()),
                Some("high".to_owned()),
            )
        };
        let decide = |c: &ModelCatalog| {
            let (m, e) = retired();
            decide_live_model(
                &c.models_of(Driver::Claude),
                c.is_fresh(Driver::Claude),
                m,
                e,
            )
        };
        assert_eq!(decide(&catalog).0.as_deref(), Some("claude-sonnet-3-7"));
        crate::testutil::in_loop(|ctx| {
            // This run's fetch lands while the start waits.
            let landing = catalog.clone();
            ctx.spawn_local(async move {
                glib::timeout_future(std::time::Duration::from_millis(150)).await;
                landing.land_fetch(Driver::Claude, vec![sonnet()]);
            });
            let started = std::time::Instant::now();
            let settled = ctx.block_on(wait_for_catalog(&catalog, Driver::Claude, CATALOG_WAIT));
            assert!(settled);
            let waited = started.elapsed();
            assert!(waited >= std::time::Duration::from_millis(100), "it waited");
            assert!(waited <= CATALOG_WAIT);
        });
        let (model, effort, ported) = decide(&catalog);
        assert_eq!(
            (model.as_deref(), effort.as_deref()),
            (Some("sonnet"), Some("high"))
        );
        assert_eq!(ported.expect("recorded").model, "claude-sonnet-3-7");
    }

    fn hook_target(
        home: Option<&std::path::Path>,
        cwd: Option<&std::path::Path>,
    ) -> crate::hook_config::HookTarget {
        let mut env = AgentEnv {
            env: home
                .map(|home| vec![("HOME".into(), home.to_string_lossy().into_owned())])
                .unwrap_or_default(),
            unset: crate::hook_config::CONFIG_REDIRECT_VARS
                .into_iter()
                .map(str::to_owned)
                .collect(),
        };
        env.unset.push("HOME".into());
        crate::hook_config::HookTarget::new(&env, cwd.map(std::path::Path::to_path_buf))
    }

    #[test]
    fn an_unchecked_hook_fails_closed_and_says_it_is_not_checked_yet() {
        let home = std::path::Path::new("/test/home");
        let target = hook_target(Some(home), Some(std::path::Path::new("/tmp")));
        HOOK_VERDICTS.with(|cache| *cache.borrow_mut() = crate::hook_config::HookCache::default());
        let before = cached_hook_verdict(&target);
        assert!(before
            .as_ref()
            .is_err_and(|e| e.contains("not been checked")));
        // It stays closed through the binding: no socket, so agy runs without the skip flag.
        // But the notice says the hook is not checked yet, not that it is missing.
        assert!(matches!(
            bind_approval(before, "t", "/tmp", Mode::Ask),
            Err(Some(reason)) if reason == crate::chat::session::HOOK_NOT_CHECKED
        ));
        // Checked and missing: the install instructions.
        assert!(matches!(
            bind_approval(Err("no entry".into()), "t", "/tmp", Mode::Ask),
            Err(None)
        ));
        // Installed, but the socket cannot be used (a relative workspace is refused before
        // anything is created): the notice carries that reason instead.
        assert!(matches!(
            bind_approval(Ok(()), "t", "relative", Mode::Ask),
            Err(Some(reason)) if !reason.is_empty()
        ));
        remember_hook_probe(
            target.clone(),
            &crate::hook_config::HookProbe::Missing("not installed".into()),
        );
        assert_eq!(
            cached_hook_verdict(&target),
            Err("not installed".to_owned())
        );
        remember_hook_probe(target.clone(), &crate::hook_config::HookProbe::Installed);
        assert_eq!(cached_hook_verdict(&target), Ok(()));
        HOOK_VERDICTS.with(|cache| *cache.borrow_mut() = crate::hook_config::HookCache::default());
    }

    #[test]
    fn hook_check_uses_the_profile_home() {
        crate::testutil::in_loop(|ctx| {
            let app = tempfile::tempdir().expect("app home");
            let profile = tempfile::tempdir().expect("profile home");
            let dir = app.path().join(".gemini/config");
            std::fs::create_dir_all(&dir).expect("mkdir");
            std::fs::write(
                dir.join("hooks.json"),
                crate::hook_config::install_entry_json(),
            )
            .expect("hook");
            assert!(ctx
                .block_on(check_hook(hook_target(Some(app.path()), Some(app.path()))))
                .is_ok());
            let env = AgentEnv {
                env: vec![("HOME".into(), profile.path().to_string_lossy().into_owned())],
                unset: crate::hook_config::CONFIG_REDIRECT_VARS
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            };
            let home = crate::agent_home::effective_home(&env);
            assert_eq!(home.as_deref(), Some(profile.path()));
            let target = crate::hook_config::HookTarget::new(&env, Some(app.path().into()));
            assert!(ctx.block_on(check_hook(target.clone())).is_err());
            assert!(cached_hook_verdict(&target).is_err());
            HOOK_VERDICTS
                .with(|cache| *cache.borrow_mut() = crate::hook_config::HookCache::default());
        });
    }

    #[test]
    fn no_home_fails_closed() {
        crate::testutil::in_loop(|ctx| {
            let cwd = tempfile::tempdir().expect("thread directory");
            let env = AgentEnv {
                env: vec![],
                unset: std::iter::once("HOME")
                    .chain(crate::hook_config::CONFIG_REDIRECT_VARS)
                    .map(str::to_owned)
                    .collect(),
            };
            let home = crate::agent_home::effective_home(&env);
            assert_eq!(home, None);
            let target = crate::hook_config::HookTarget::new(&env, Some(cwd.path().into()));
            assert!(ctx.block_on(check_hook(target.clone())).is_err());
            // Even an installed Settings probe with no resolvable home cannot prove a hook.
            remember_hook_probe(target.clone(), &crate::hook_config::HookProbe::Installed);
            assert_eq!(
                cached_hook_verdict(&target),
                Err(crate::chat::session::HOOK_NOT_CHECKED.into())
            );
            HOOK_VERDICTS
                .with(|cache| *cache.borrow_mut() = crate::hook_config::HookCache::default());
        });
    }

    #[test]
    fn a_verdict_for_another_home_is_not_reused() {
        let app = std::path::Path::new("/app/home");
        let profile = std::path::Path::new("/profile/home");
        let cwd = std::path::Path::new("/tmp");
        let app = hook_target(Some(app), Some(cwd));
        let profile = hook_target(Some(profile), Some(cwd));
        remember_hook_probe(app.clone(), &crate::hook_config::HookProbe::Installed);
        assert_eq!(cached_hook_verdict(&app), Ok(()));
        assert_eq!(
            cached_hook_verdict(&profile),
            Err(crate::hook_config::DIFFERENT_HOME.into())
        );
        assert!(
            cached_hook_verdict(&hook_target(None, Some(cwd))).is_err(),
            "clear_env changed before launch"
        );
        remember_hook_probe(
            profile.clone(),
            &crate::hook_config::HookProbe::Missing("no entry".into()),
        );
        assert_eq!(cached_hook_verdict(&profile), Err("no entry".into()));
        assert_eq!(
            cached_hook_verdict(&app),
            Ok(()),
            "another home's check cannot erase this proof"
        );
        HOOK_VERDICTS.with(|cache| *cache.borrow_mut() = crate::hook_config::HookCache::default());
    }

    #[test]
    fn a_launch_without_home_rejects_an_earlier_hook_proof() {
        let home = std::path::Path::new("/app/home");
        let target = hook_target(Some(home), Some(std::path::Path::new("/tmp")));
        remember_hook_probe(target.clone(), &crate::hook_config::HookProbe::Installed);
        let checked = cached_hook_verdict(&target);
        assert!(checked.is_ok());
        let cleared = AgentEnv {
            env: vec![],
            unset: std::iter::once("HOME")
                .chain(crate::hook_config::CONFIG_REDIRECT_VARS)
                .map(str::to_owned)
                .collect(),
        };
        assert!(matches!(
            bind_approval(hook_for_launch(checked, &cleared, "/tmp"), "t", "/tmp", Mode::Ask),
            Err(Some(reason)) if reason == crate::chat::session::HOOK_NOT_CHECKED
        ));
        HOOK_VERDICTS.with(|cache| *cache.borrow_mut() = crate::hook_config::HookCache::default());
    }

    #[test]
    fn a_redirect_added_before_launch_invalidates_the_hook_proof() {
        let home = std::path::Path::new("/app/home");
        let target = hook_target(Some(home), Some(std::path::Path::new("/tmp")));
        remember_hook_probe(target.clone(), &crate::hook_config::HookProbe::Installed);
        assert!(cached_hook_verdict(&target).is_ok());
        let env = AgentEnv {
            env: vec![
                ("HOME".into(), "/app/home".into()),
                (
                    "ANTIGRAVITY_APP_DATA_DIR".into(),
                    "private-config-value".into(),
                ),
            ],
            unset: crate::hook_config::CONFIG_REDIRECT_VARS
                .into_iter()
                .map(str::to_owned)
                .collect(),
        };
        let refusal = hook_for_launch(Ok(()), &env, "/tmp");
        assert!(matches!(bind_approval(refusal, "t", "/tmp", Mode::Ask),
            Err(Some(why)) if why.contains("ANTIGRAVITY_APP_DATA_DIR") && !why.contains("private-config-value")));
        HOOK_VERDICTS.with(|cache| *cache.borrow_mut() = crate::hook_config::HookCache::default());
    }

    #[test]
    fn a_verdict_for_another_directory_is_not_reused() {
        let home = std::path::Path::new("/app/home");
        let first = hook_target(Some(home), Some(std::path::Path::new("/first")));
        let second = hook_target(Some(home), Some(std::path::Path::new("/second")));
        remember_hook_probe(first.clone(), &crate::hook_config::HookProbe::Installed);
        assert_eq!(cached_hook_verdict(&first), Ok(()));
        assert_eq!(
            cached_hook_verdict(&second),
            Err(crate::hook_config::DIFFERENT_DIRECTORY.into())
        );
        HOOK_VERDICTS.with(|cache| *cache.borrow_mut() = crate::hook_config::HookCache::default());
    }

    #[test]
    fn two_thread_proofs_coexist_and_refresh_independently() {
        crate::testutil::in_loop(|ctx| {
            let home = tempfile::tempdir().expect("home");
            let hooks = home.path().join(".gemini/config/hooks.json");
            std::fs::create_dir_all(hooks.parent().expect("parent")).expect("mkdir");
            std::fs::write(hooks, crate::hook_config::install_entry_json()).expect("home hook");
            let a = tempfile::tempdir().expect("directory A");
            let b = tempfile::tempdir().expect("directory B");
            let target_a = hook_target(Some(home.path()), Some(a.path()));
            let target_b = hook_target(Some(home.path()), Some(b.path()));
            assert!(ctx.block_on(check_hook(target_a.clone())).is_ok());
            assert!(ctx.block_on(check_hook(target_b.clone())).is_ok());
            let env = AgentEnv {
                env: vec![("HOME".into(), home.path().to_string_lossy().into_owned())],
                unset: crate::hook_config::CONFIG_REDIRECT_VARS
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            };
            for cwd in [a.path(), b.path()] {
                assert!(
                    hook_for_launch(Ok(()), &env, cwd.to_str().expect("cwd")).is_ok(),
                    "checking B must not remove A's skip-flag proof"
                );
            }
            for (changed, other) in [(&target_a, &target_b), (&target_b, &target_a)] {
                let local = changed
                    .cwd
                    .as_ref()
                    .expect("cwd")
                    .join(".agents/hooks.json");
                std::fs::create_dir_all(local.parent().expect("parent")).expect("mkdir");
                std::fs::write(&local, "{}").expect("changed hook");
                assert!(ctx.block_on(check_hook(changed.clone())).is_err());
                assert!(cached_hook_verdict(changed).is_err());
                assert_eq!(cached_hook_verdict(other), Ok(()));
                std::fs::remove_file(local).expect("remove");
                assert!(ctx.block_on(check_hook(changed.clone())).is_ok());
            }
            let changed_home =
                hook_target(Some(std::path::Path::new("/another/home")), Some(a.path()));
            assert!(
                cached_hook_verdict(&changed_home).is_err(),
                "a new HOME requires its own proof"
            );
            HOOK_VERDICTS
                .with(|cache| *cache.borrow_mut() = crate::hook_config::HookCache::default());
        });
    }

    #[test]
    fn refresh_checks_all_directories_and_active_session_homes() {
        let a = std::path::Path::new("/directory/a");
        let b = std::path::Path::new("/directory/b");
        let active = hook_target(Some(std::path::Path::new("/active/home")), Some(a));
        let profile = hook_target(Some(std::path::Path::new("/profile/home")), None);
        let targets = hook_refresh_targets(
            vec![a.into(), b.into(), a.into()],
            vec![active.clone()],
            profile,
        );
        assert_eq!(targets.len(), 3, "duplicates are checked once");
        assert!(targets.contains(&active));
        for cwd in [a, b] {
            assert!(
                targets.contains(&hook_target(
                    Some(std::path::Path::new("/profile/home")),
                    Some(cwd)
                )),
                "every open thread can switch to the current profile"
            );
        }
    }

    #[test]
    fn a_refresh_keeps_all_twenty_open_directory_proofs() {
        let directories: Vec<_> = (0..20)
            .map(|i| std::path::PathBuf::from(format!("/directory/{i}")))
            .collect();
        let profile = hook_target(Some(std::path::Path::new("/profile/home")), None);
        let active = hook_target(
            Some(std::path::Path::new("/active/home")),
            Some(&directories[0]),
        );
        let targets = hook_refresh_targets(directories, vec![active], profile);
        assert_eq!(
            targets.len(),
            21,
            "active and configured homes both count toward capacity"
        );
        let mut cache = crate::hook_config::HookCache::default();
        cache.set_refresh_size(targets.len());
        for target in &targets {
            cache.remember(target, Ok(()));
        }
        for target in &targets {
            assert_eq!(
                cache.lookup(target),
                Ok(()),
                "refresh must not evict its own proofs"
            );
        }
        cache.remember(&targets[0], Err("hook changed".into()));
        assert_eq!(cache.lookup(&targets[0]), Err("hook changed".into()));
        assert_eq!(cache.lookup(&targets[1]), Ok(()));
        cache.set_refresh_size(0);
        assert!(
            cache.lookup(&targets[2]).is_err(),
            "closed sources do not leave an unbounded cache"
        );
        assert_eq!(
            cache.lookup(&targets[0]),
            Err("hook changed".into()),
            "the most recent failed proof remains a refusal"
        );
    }

    #[test]
    fn an_unscoped_probe_cannot_replace_a_launch_proof() {
        crate::testutil::in_loop(|ctx| {
            let home = std::path::Path::new("/app/home");
            let scoped = hook_target(Some(home), Some(std::path::Path::new("/tmp")));
            let unscoped = hook_target(Some(home), None);
            remember_hook_probe(scoped.clone(), &crate::hook_config::HookProbe::Installed);
            assert!(ctx.block_on(check_hook(unscoped.clone())).is_err());
            remember_hook_probe(unscoped.clone(), &crate::hook_config::HookProbe::Installed);
            assert!(cached_hook_verdict(&unscoped).is_err());
            assert_eq!(cached_hook_verdict(&scoped), Ok(()));
            HOOK_VERDICTS
                .with(|cache| *cache.borrow_mut() = crate::hook_config::HookCache::default());
        });
    }

    #[test]
    fn a_cache_mismatch_names_the_home_and_rejects_the_launch() {
        let cached = hook_target(
            Some(std::path::Path::new("/cached")),
            Some(std::path::Path::new("/tmp")),
        );
        remember_hook_probe(cached, &crate::hook_config::HookProbe::Installed);
        let env = AgentEnv {
            env: vec![("HOME".into(), "/actual".into())],
            unset: crate::hook_config::CONFIG_REDIRECT_VARS
                .into_iter()
                .map(str::to_owned)
                .collect(),
        };
        let refusal = hook_for_launch(Ok(()), &env, "/tmp");
        assert_eq!(refusal, Err(crate::hook_config::DIFFERENT_HOME.into()));
        assert!(matches!(bind_approval(refusal, "t", "/tmp", Mode::Ask),
            Err(Some(why)) if why == crate::hook_config::DIFFERENT_HOME));
        HOOK_VERDICTS.with(|cache| *cache.borrow_mut() = crate::hook_config::HookCache::default());
    }

    #[test]
    fn a_missing_agy_profile_uses_the_launch_fallback() {
        crate::testutil::in_loop(|ctx| {
            let config = SharedConfig::default();
            let saved = config.borrow().clone();
            config.borrow_mut().profiles.clear();
            config.borrow_mut().clear_env = crate::hook_config::CONFIG_REDIRECT_VARS
                .into_iter()
                .map(str::to_owned)
                .collect();
            let profile = crate::config::new_agent_profile(Driver::Agy);
            let key = (profile.command.clone(), profile.env_file.clone());
            let old = RESOLVED.with(|r| {
                r.borrow_mut().insert(
                    key.clone(),
                    ResolvedAgent {
                        program: Some("/fixture/agy".into()),
                        env: vec![("HOME".into(), "/fallback/home".into())],
                        env_failed: false,
                    },
                )
            });
            HOOK_VERDICTS
                .with(|cache| *cache.borrow_mut() = crate::hook_config::HookCache::default());
            let cached = cached_agy_hook_target(Some("/tmp".into())).expect("fallback resolved");
            let refreshed = ctx.block_on(agy_hook_target(Some("/tmp".into())));
            let launch = agent_launch_with(
                &Availability::Ready("/fixture/agy".into()),
                Driver::Agy,
                "t",
                "/tmp",
                None,
            )
            .expect("launch description");
            assert_eq!(
                cached.home.as_deref(),
                Some(std::path::Path::new("/fallback/home"))
            );
            assert_eq!(cached.home, refreshed.home);
            assert_eq!(cached.home, crate::agent_home::effective_home(&launch.env));
            *config.borrow_mut() = saved;
            RESOLVED.with(|r| {
                let mut r = r.borrow_mut();
                if let Some(old) = old {
                    r.insert(key, old);
                } else {
                    r.remove(&key);
                }
            });
        });
    }

    #[test]
    fn a_fork_makes_a_second_thread_and_leaves_the_original_exactly_as_it_was() {
        for driver in [Driver::Claude, Driver::Agy, Driver::Codex] {
            let store = Store::open_in_memory().expect("store");
            let source = store.create_thread("/w", Some("source")).expect("thread");
            let pt = store
                .add_provider_thread(&source, driver_key(driver), "m1")
                .expect("pt");
            store.set_active_provider_thread(&source, &pt).expect("set");
            let say = |text: &str| Envelope::new(Event::Notice { text: text.into() });
            store
                .append_event(&source, Some(pt.as_str()), &say("hello"))
                .expect("event");
            let before = (
                store.events(&source, None, 100).expect("events").len(),
                store.active_provider_thread(&source).expect("active"),
                store.thread_summary(&source).expect("summary"),
            );

            // What `fork_thread` hands to `create_chat_thread`: the same agent, the source's
            // model and mode, in the source's folder.
            let fork = record_new_thread(&store, "/w", None, driver, Some("m1"), Some(Mode::Plan))
                .expect("fork");

            assert_ne!(fork, source, "{driver:?}: a second thread");
            let ids: Vec<_> = store
                .list_threads(true)
                .expect("list")
                .into_iter()
                .map(|s| s.id)
                .collect();
            assert_eq!(ids.len(), 2);
            assert!(ids.contains(&source) && ids.contains(&fork));
            // The fork is on the same agent and starts in the carried mode.
            assert!(store
                .active_provider_thread(&fork)
                .expect("active")
                .is_some());
            let fork_events = store.events(&fork, None, 100).expect("events");
            assert!(fork_events
                .iter()
                .any(|(_, e)| matches!(e.event, Event::ModeChosen { mode: Mode::Plan })));
            // The original: same events, same active provider thread, same summary.
            let after = (
                store.events(&source, None, 100).expect("events").len(),
                store.active_provider_thread(&source).expect("active"),
                store.thread_summary(&source).expect("summary"),
            );
            assert_eq!(before, after, "{driver:?}: the source is untouched");
        }
    }

    #[test]
    fn saved_open_threads_restore_only_what_still_exists_in_order_and_capped() {
        let saved = serde_json::json!({"open": ["a", "gone", "b", "c"], "selected": "b"});
        let known: Vec<String> = ["a", "b", "c"].map(str::to_owned).to_vec();
        let open = restorable(&saved, &known, 2);
        assert_eq!(open, ["a", "b"]);
        assert_eq!(selected_to_restore(&saved, &open).as_deref(), Some("b"));
        // A selection that was not reopened falls back to the first.
        let saved = serde_json::json!({"open": ["a", "b"], "selected": "c"});
        assert_eq!(selected_to_restore(&saved, &open).as_deref(), Some("a"));
        assert!(restorable(&serde_json::json!({}), &known, 5).is_empty());
        assert_eq!(selected_to_restore(&serde_json::json!({}), &[]), None);
    }

    /// A scan still reading the agents' files when the setting goes off (or off and on again)
    /// links nothing when it returns; only the newest scan, with the setting on, may.
    #[test]
    fn a_delayed_session_scan_is_dropped_once_the_setting_changes() {
        let first = begin_session_scan();
        assert!(session_scan_wanted(first, true));
        assert!(!session_scan_wanted(first, false), "off when it returns");
        // Turned off while it read: dropped, even if the setting is read as on later.
        cancel_session_scans();
        assert!(!session_scan_wanted(first, true));
        assert!(
            !std::thread::spawn(move || session_scan_wanted(first, true))
                .join()
                .expect("writer check"),
            "a result already queued on the writer is dropped too"
        );
        // Turned on again: a new scan; the old one, returning late, still links nothing.
        let second = begin_session_scan();
        assert!(!session_scan_wanted(first, true));
        assert!(session_scan_wanted(second, true));
        // A scan begun after another supersedes it: no two link at once.
        let third = begin_session_scan();
        assert!(!session_scan_wanted(second, true));
        assert!(session_scan_wanted(third, true));
    }

    #[test]
    fn the_reopen_offer_counts_the_last_sessions_threads_that_still_exist() {
        let known: Vec<String> = ["a", "b", "c"].map(str::to_owned).to_vec();
        // Every window's layout counts, each thread once; deleted or archived ones drop out.
        let saved = serde_json::json!({
            "windows": [{"open": ["a", "gone"]}, {"open": ["b", "a"]}],
            "selected": "a"
        })
        .to_string();
        assert_eq!(last_session_count(Some(&saved), &known), 2);
        // The legacy single-window shape.
        let legacy = serde_json::json!({"open": ["c"]}).to_string();
        assert_eq!(last_session_count(Some(&legacy), &known), 1);
        // Nothing saved, nothing readable, or nothing left: no offer.
        assert_eq!(last_session_count(None, &known), 0);
        assert_eq!(last_session_count(Some("not json"), &known), 0);
        assert_eq!(last_session_count(Some(&saved), &[]), 0);
        // Capped like the restore itself.
        let many: Vec<String> = (0..50).map(|i| format!("t{i}")).collect();
        let all = serde_json::json!({ "open": many }).to_string();
        assert_eq!(
            last_session_count(Some(&all), &many),
            crate::config::SessionState::MAX_TABS
        );
    }

    #[test]
    fn history_is_read_in_batches_up_to_the_replay_limit() {
        let store = Store::open_in_memory().expect("store");
        let thread = store.create_thread("/w", Some("t")).expect("thread");
        for _ in 0..2_500 {
            store
                .append_event(
                    &thread,
                    None,
                    &Envelope::new(Event::Notice { text: "x".into() }),
                )
                .expect("event");
        }
        // More than one batch of 2,000, in order, nothing lost.
        let history = read_history(&store, &thread).expect("read");
        assert_eq!(history.len(), 2_500);
        assert!(read_history(&store, "nope").expect("read").is_empty());
    }

    #[test]
    fn history_credits_each_event_to_the_agent_it_was_stored_under() {
        let store = Store::open_in_memory().expect("store");
        let thread = store.create_thread("/w", Some("t")).expect("thread");
        let claude = store
            .add_provider_thread(&thread, "claude", "opus")
            .expect("pt");
        let agy = store
            .add_provider_thread(&thread, "agy", "gemini")
            .expect("pt");
        let note = |text: &str| Envelope::new(Event::Notice { text: text.into() });
        for (pt, text) in [(Some(&claude), "a"), (Some(&agy), "b"), (None, "c")] {
            store
                .append_event(&thread, pt.map(String::as_str), &note(text))
                .expect("event");
        }
        let drivers: Vec<_> = read_history(&store, &thread)
            .expect("read")
            .into_iter()
            .map(|(d, _)| d)
            .collect();
        assert_eq!(drivers, [Some(Driver::Claude), Some(Driver::Agy), None]);
    }

    #[test]
    fn a_long_thread_replays_its_newest_turns_behind_a_notice() {
        let store = Store::open_in_memory().expect("store");
        let thread = store.create_thread("/w", Some("t")).expect("thread");
        let note = |text: String| Envelope::new(Event::Notice { text });
        // Each turn as a session stores it: the prompt first, then the agent's turn start.
        for turn in 0..10 {
            store
                .append_event(
                    &thread,
                    None,
                    &Envelope::new(Event::ItemStarted {
                        kind: ItemKind::UserMessage,
                        title: format!("p{turn}"),
                        input: None,
                        parent: None,
                    })
                    .item(format!("u{turn}")),
                )
                .expect("event");
            store
                .append_event(
                    &thread,
                    None,
                    &Envelope::new(Event::TurnStarted {
                        model: Some(format!("t{turn}")),
                    }),
                )
                .expect("event");
            for step in 0..3 {
                store
                    .append_event(&thread, None, &note(format!("{turn}.{step}")))
                    .expect("event");
            }
        }
        let text = |(_, e): &(Option<Driver>, Envelope)| match &e.event {
            Event::Notice { text } => text.clone(),
            Event::TurnStarted { model } => model.clone().unwrap_or_default(),
            Event::ItemStarted { title, .. } => title.clone(),
            _ => String::new(),
        };
        // 50 events, limit 12: the newest 12 start mid-turn 7, so the cut moves on to turn 8,
        // starting at its prompt.
        let history = read_history_within(&store, &thread, 12).expect("read");
        let got: Vec<String> = history.iter().map(text).collect();
        assert!(got[0].contains("only its most recent part"), "{got:?}");
        assert_eq!(got[1..3], ["p8", "t8"], "{got:?}");
        assert_eq!(got.last().map(String::as_str), Some("9.2"));
        assert_eq!(got.len(), 1 + 10);
        // One turn longer than the window: no turn start in it, so it is shown from its first
        // prompt, or as it is.
        let long = store.create_thread("/w", Some("long")).expect("thread");
        for step in 0..10 {
            if step == 7 {
                // A queued follow-up prompt inside the long turn: the window starts there.
                store
                    .append_event(
                        &long,
                        None,
                        &Envelope::new(Event::ItemStarted {
                            kind: ItemKind::UserMessage,
                            title: "follow-up".into(),
                            input: None,
                            parent: None,
                        })
                        .item("u-long"),
                    )
                    .expect("event");
            }
            store
                .append_event(&long, None, &note(format!("s{step}")))
                .expect("event");
        }
        // Newest 5: s6, follow-up, s7, s8, s9; the cut drops s6.
        let got: Vec<String> = read_history_within(&store, &long, 5)
            .expect("read")
            .iter()
            .map(text)
            .collect();
        assert_eq!(got[1..], ["follow-up", "s7", "s8", "s9"], "{got:?}");
        // A thread that fits is replayed whole, with no notice.
        let whole = read_history_within(&store, &thread, 50).expect("read");
        assert_eq!(whole.len(), 50);
    }

    #[test]
    fn a_store_job_runs_off_the_main_thread_on_a_connection_of_its_own() {
        use crate::testutil::{in_loop, pump_until};
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("threads.db");
        let main_store = Store::open(&path).expect("open");
        let thread = main_store.create_thread("/w", Some("t")).expect("thread");
        STORE_PATH.with(|p| *p.borrow_mut() = Some(path));
        assert!(store_is_async());

        let main_thread = std::thread::current().id();
        let got = in_loop(|ctx| {
            let out = Rc::new(RefCell::new(None));
            let o = out.clone();
            let id = thread.clone();
            glib::spawn_future_local(async move {
                let result = store_job(move |store| {
                    (
                        std::thread::current().id(),
                        store.thread_summary(&id).map(|s| s.map(|s| s.title)),
                    )
                })
                .await;
                *o.borrow_mut() = Some(result);
            });
            assert!(pump_until(ctx, 15, || out.borrow().is_some()), "no result");
            let got = out.borrow_mut().take();
            got
        });
        STORE_PATH.with(|p| *p.borrow_mut() = None);
        let (worker, title) = got.flatten().expect("the job ran");
        assert_ne!(worker, main_thread, "the job ran on the main thread");
        assert_eq!(title.expect("query").as_deref(), Some("t"));
        // A write from the worker is visible to the main connection (WAL).
        assert!(main_store.thread_summary(&thread).expect("read").is_some());
    }

    /// With a file store, the main thread's writes go to the store writer (none runs here),
    /// and a store job queued right after them reads them back: it waits on the writer's
    /// barrier before it reads on its own connection. A write job runs on the writer, in order.
    #[test]
    fn main_thread_writes_are_queued_and_store_jobs_read_them_back() {
        use crate::testutil::{in_loop, pump_until};
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("threads.db");
        let reader = Rc::new(Store::open(&path).expect("open"));
        let writer = StoreWriter::spawn(&path).expect("writer");
        STORE.with(|s| *s.borrow_mut() = Some(reader.clone()));
        STORE_PATH.with(|p| *p.borrow_mut() = Some(path));
        WRITER.with(|w| *w.borrow_mut() = Some(writer.clone()));

        let handle = app_handle();
        assert!(handle.writer().is_some());
        let thread = handle.new_id().expect("id");
        let ids = NewThreadIds {
            thread: thread.clone(),
            provider: handle.new_id().expect("id"),
        };
        // A write ahead of everything holds the writer until the main thread has checked,
        // so the check cannot race the writer however loaded the machine is.
        let (release, held) = std::sync::mpsc::channel::<()>();
        handle
            .write("wait", None, move |_| {
                let _ = held.recv();
                Ok(())
            })
            .expect("queued");
        handle
            .write("create a thread", Some(&thread), move |s| {
                let spec = NewThreadSpec {
                    title: Some("t".into()),
                    ..NewThreadSpec::new("/w", Driver::Claude)
                };
                record_new_thread_as(s, &ids, &spec)
            })
            .expect("queued");
        let say = |n: usize| {
            Envelope::new(Event::Notice {
                text: format!("n{n}"),
            })
        };
        for n in 0..300 {
            handle.append_event(&thread, None, &say(n)).expect("queued");
        }
        assert!(
            reader.thread_summary(&thread).expect("read").is_none(),
            "nothing was written on the main thread"
        );
        release.send(()).expect("the writer is waiting");

        let got = in_loop(|ctx| {
            let out = Rc::new(RefCell::new(None));
            let o = out.clone();
            let id = thread.clone();
            glib::spawn_future_local(async move {
                let read = store_job({
                    let id = id.clone();
                    move |s| s.events(&id, None, 1000).map(|e| e.len())
                })
                .await;
                let renamed = write_job("rename", move |s| {
                    s.rename_thread(&id, "renamed")?;
                    s.thread_summary(&id).map(|s| s.map(|s| s.title))
                })
                .await;
                *o.borrow_mut() = Some((read, renamed));
            });
            assert!(pump_until(ctx, 15, || out.borrow().is_some()), "no result");
            let got = out.borrow_mut().take();
            got
        });
        WRITER.with(|w| *w.borrow_mut() = None);
        STORE_PATH.with(|p| *p.borrow_mut() = None);
        STORE.with(|s| *s.borrow_mut() = None);
        let (read, renamed) = got.expect("ran");
        assert_eq!(read.expect("job").expect("events"), 300);
        assert_eq!(
            renamed.expect("job").expect("rename").as_deref(),
            Some("renamed")
        );
        assert_eq!(writer.failures().0, 0);
    }

    fn handoff(carried: usize) -> crate::chat::session::Handoff {
        crate::chat::session::Handoff {
            summary: "earlier context".into(),
            fence: "0123456789abcdef0123456789abcdef".into(),
            carried,
            total: 2,
        }
    }

    fn seeded_count(store: &Store, thread: &str) -> usize {
        store
            .events(thread, None, 1000)
            .unwrap()
            .into_iter()
            .filter(|(_, e)| matches!(e.event, Event::HandoffSeeded { .. }))
            .count()
    }

    /// A fork's handoff, mode and profile are stored with the thread itself: a page closed
    /// before its session ever started reopens owing the same handoff.
    #[test]
    fn a_new_threads_handoff_survives_a_page_closed_before_its_session_starts() {
        let store = Store::open_in_memory().unwrap();
        let ids = NewThreadIds {
            thread: "fork".into(),
            provider: "provider".into(),
        };
        let spec = NewThreadSpec {
            mode: Some(Mode::Plan),
            handoff: Some((handoff(2), "“source”".into())),
            attributes: agent_kit::store::ThreadAttributes {
                profile_id: Some("claude-work".into()),
                worktree: None,
            },
            ..NewThreadSpec::new("/w", Driver::Claude)
        };
        record_new_thread_as(&store, &ids, &spec).unwrap();
        let loaded = load_thread(&store, "fork", None).unwrap();
        assert_eq!(
            crate::chat::session::undelivered_handoff(loaded.history.iter().map(|(_, e)| e)),
            Some(handoff(2)),
            "re-armed from history whenever its session starts"
        );
        assert_eq!(
            crate::chat::session::chosen_mode(loaded.history.iter().map(|(_, e)| e)),
            Some(Mode::Plan)
        );
        assert_eq!(loaded.attributes.profile_id.as_deref(), Some("claude-work"));
        assert_eq!(seeded_count(&store, "fork"), 1);
        assert!(
            !store.is_blank("fork").unwrap(),
            "an unstarted fork is never discarded as blank"
        );
    }

    #[test]
    fn a_handoff_carrying_nothing_leaves_the_new_thread_blank() {
        let store = Store::open_in_memory().unwrap();
        let ids = NewThreadIds {
            thread: "t".into(),
            provider: "p".into(),
        };
        let spec = NewThreadSpec {
            handoff: Some((handoff(0), "“source”".into())),
            ..NewThreadSpec::new("/w", Driver::Claude)
        };
        record_new_thread_as(&store, &ids, &spec).unwrap();
        assert_eq!(seeded_count(&store, "t"), 0);
        assert!(store.events("t", None, 10).unwrap().is_empty());
        assert!(store.is_blank("t").unwrap());
    }

    #[test]
    fn a_fork_keeps_the_profile_only_on_the_same_agent() {
        let work = || Some("claude-work".to_owned());
        assert_eq!(
            carried_profile(Some(Driver::Claude), work(), Driver::Claude),
            work()
        );
        assert_eq!(
            carried_profile(Some(Driver::Claude), work(), Driver::Codex),
            None,
            "continued in another agent: that agent's own"
        );
        assert_eq!(carried_profile(None, work(), Driver::Claude), None);
        assert_eq!(
            carried_profile(Some(Driver::Claude), None, Driver::Claude),
            None
        );
    }

    #[test]
    fn an_unopened_source_carries_its_stored_profile_only_to_the_same_agent() {
        let store = Store::open_in_memory().unwrap();
        let source = store.create_thread("/w", Some("source")).unwrap();
        let provider = store
            .add_provider_thread(&source, "claude", "source-model")
            .unwrap();
        store
            .set_active_provider_thread(&source, &provider)
            .unwrap();
        store
            .set_thread_profile(&source, Some("claude-work"))
            .unwrap();
        let loaded = load_thread(&store, &source, None).unwrap();
        for (target, expected) in [(Driver::Claude, Some("claude-work")), (Driver::Codex, None)] {
            let ids = NewThreadIds {
                thread: store.new_id().unwrap(),
                provider: store.new_id().unwrap(),
            };
            record_new_thread_as(
                &store,
                &ids,
                &NewThreadSpec {
                    attributes: agent_kit::store::ThreadAttributes {
                        profile_id: carried_stored_profile(&loaded, target),
                        ..Default::default()
                    },
                    ..NewThreadSpec::new("/w", target)
                },
            )
            .unwrap();
            assert_eq!(
                store
                    .thread_attributes(&ids.thread)
                    .unwrap()
                    .profile_id
                    .as_deref(),
                expected
            );
            assert_eq!(
                load_provider(&store, &ids.thread).unwrap().unwrap().driver,
                driver_key(target)
            );
        }
        assert_eq!(
            store
                .thread_attributes(&source)
                .unwrap()
                .profile_id
                .as_deref(),
            Some("claude-work")
        );
        let unbound = store.create_thread("/w", Some("no provider")).unwrap();
        store
            .set_thread_profile(&unbound, Some("claude-work"))
            .unwrap();
        assert_eq!(
            carried_stored_profile(
                &load_thread(&store, &unbound, None).unwrap(),
                Driver::Claude
            ),
            None
        );
    }

    #[test]
    fn a_profile_ends_only_when_another_agent_has_started() {
        let started = Event::SessionStarted {
            native_id: "n".into(),
            model: None,
            cwd: None,
        };
        let early = Event::ModelChanged { model: "m".into() };
        let claude = Some(Some(Driver::Claude));
        assert!(profile_outlived(claude, Driver::Codex, &started));
        assert!(
            !profile_outlived(claude, Driver::Codex, &early),
            "a switch's events before its agent starts keep it (it may roll back)"
        );
        assert!(
            !profile_outlived(claude, Driver::Claude, &started),
            "its own agent (re)started, a roll back included"
        );
        assert!(!profile_outlived(None, Driver::Codex, &started));
        assert!(!profile_outlived(Some(None), Driver::Codex, &started));
    }

    /// A thread on the second of two Claude profiles is switched to Codex, which fails before it
    /// starts: the events the provisional switch emits keep the thread's profile, so the
    /// roll back relaunches Claude with it (its program and arguments), not the default's. Only
    /// a replacement that reports its session ends it.
    #[test]
    #[ignore = "needs a display; run alone in an isolated display"]
    fn a_failed_switch_rolls_back_onto_the_threads_own_profile() {
        use crate::testutil::pump_until;
        gtk4::init().unwrap();
        adw::init().unwrap();
        crate::icons::register();
        let config_home = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", config_home.path());
        let app = adw::Application::builder()
            .application_id("org.test.FailedSwitchKeepsProfile")
            .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk4::gio::Cancellable>).unwrap();
        let _hold = app.hold();
        CHAT_RESTORED.with(|done| done.set(true));
        let window = crate::window::AgentTerminalWindow::new(&app);
        let ctx = glib::MainContext::default();
        assert!(pump_until(&ctx, 5, || window
            .imp()
            .tab_view
            .borrow()
            .is_some()));
        let imp = window.imp();
        let default = Profile {
            id: "claude-default".into(),
            name: "Claude".into(),
            command: "/opt/default/claude".into(),
            args: vec!["--default".into()],
            ..crate::config::new_agent_profile(Driver::Claude)
        };
        let work = Profile {
            id: "claude-work".into(),
            name: "Claude (work)".into(),
            command: "/opt/work/claude".into(),
            args: vec!["--work".into()],
            ..crate::config::new_agent_profile(Driver::Claude)
        };
        assert_eq!(profile_driver(&work), Some(Driver::Claude));
        {
            let mut config = imp.config.borrow_mut();
            config
                .profiles
                .retain(|p| profile_driver(p) != Some(Driver::Claude));
            config.profiles.insert(0, default.clone());
            config.profiles.insert(1, work.clone());
        }
        for p in [&default, &work] {
            RESOLVED.with(|r| {
                r.borrow_mut().insert(
                    (p.command.clone(), p.env_file.clone()),
                    ResolvedAgent {
                        program: Some(p.command.clone()),
                        env: Vec::new(),
                        env_failed: false,
                    },
                )
            });
        }
        AgentAvailability::shared().set(
            Driver::Claude,
            Availability::Ready("/opt/default/claude".into()),
        );
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().into_owned();
        imp.create_chat_thread(NewThread {
            dir: Some(cwd.clone()),
            title: Some("work".into()),
            profile_id: Some("claude-work".into()),
            ..NewThread::new(Driver::Claude)
        })
        .expect("opened in place");
        let thread = imp.current_thread().expect("its page");
        let relaunch = || agent_launch(Driver::Claude, &thread, &cwd).expect("launch");

        // The switch to Codex emits its events before Codex starts; Codex then fails, and the
        // session rolls back to Claude, reporting the failure on Claude.
        imp.on_thread_envelope(
            &thread,
            Driver::Codex,
            &Envelope::new(Event::ModelChanged {
                model: "gpt".into(),
            }),
        );
        imp.on_thread_envelope(
            &thread,
            Driver::Claude,
            &Envelope::new(Event::Error {
                message: "Codex did not start. Back on Claude.".into(),
            }),
        );
        let launch = relaunch();
        assert_eq!(
            launch.program, "/opt/work/claude",
            "the thread's own profile"
        );
        assert_eq!(launch.extra_args, vec!["--work".to_owned()]);
        // Its own agent restarting keeps it too.
        imp.on_thread_envelope(
            &thread,
            Driver::Claude,
            &Envelope::new(Event::SessionStarted {
                native_id: "claude-1".into(),
                model: None,
                cwd: None,
            }),
        );
        assert_eq!(relaunch().program, "/opt/work/claude");

        // A switch whose agent started: the thread follows that agent's default from then on.
        imp.on_thread_envelope(
            &thread,
            Driver::Codex,
            &Envelope::new(Event::SessionStarted {
                native_id: "codex-1".into(),
                model: None,
                cwd: None,
            }),
        );
        assert_eq!(
            relaunch().program,
            "/opt/default/claude",
            "back on Claude later, it is Claude's default"
        );
        crate::testutil::settle(&ctx);
        assert_eq!(
            app_store().thread_attributes(&thread).unwrap().profile_id,
            None,
            "cleared in the store too"
        );
        window.destroy();
        crate::testutil::settle(&ctx);
    }

    /// A worktree thread made through the store writer keeps its worktree and profile: on its
    /// page at once, and again when it is closed and reopened (read back with its history). A
    /// second resume of one native session opens the thread that holds it.
    #[test]
    #[ignore = "needs a display; run alone in an isolated display"]
    fn a_worktree_thread_keeps_its_worktree_and_profile_through_the_writer_and_a_reopen() {
        use crate::testutil::pump_until;
        gtk4::init().unwrap();
        adw::init().unwrap();
        crate::icons::register();
        let config_home = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", config_home.path());
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("threads.db");
        let reader = Rc::new(Store::open(&path).expect("open"));
        let writer = StoreWriter::spawn(&path).expect("writer");
        STORE.with(|s| *s.borrow_mut() = Some(reader.clone()));
        STORE_PATH.with(|p| *p.borrow_mut() = Some(path));
        WRITER.with(|w| *w.borrow_mut() = Some(writer.clone()));
        let app = adw::Application::builder()
            .application_id("org.test.WorktreeThreadReopen")
            .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk4::gio::Cancellable>).unwrap();
        let _hold = app.hold();
        CHAT_RESTORED.with(|done| done.set(true));
        let window = crate::window::AgentTerminalWindow::new(&app);
        let ctx = glib::MainContext::default();
        assert!(pump_until(&ctx, 5, || window
            .imp()
            .tab_view
            .borrow()
            .is_some()));
        let imp = window.imp();
        imp.config.borrow_mut().profiles.push(Profile {
            id: "claude-work".into(),
            name: "Claude (work)".into(),
            ..crate::config::new_agent_profile(Driver::Claude)
        });
        let tree = tempfile::tempdir().unwrap();
        let info = crate::worktree::WorktreeInfo {
            path: tree.path().to_path_buf(),
            branch: "feat/x".into(),
            main_toplevel: tree.path().join("main"),
        };
        let dir = tree.path().to_string_lossy().into_owned();
        assert!(
            imp.create_chat_thread(NewThread {
                dir: Some(dir.clone()),
                title: Some("wt".into()),
                profile_id: Some("claude-work".into()),
                worktree: Some(info.clone()),
                ..NewThread::new(Driver::Claude)
            })
            .is_none(),
            "through the writer, the page opens once the thread is stored"
        );
        let attributes = || {
            imp.tabs.borrow().iter().find_map(|t| {
                let chat = t.chat.as_ref()?;
                Some((
                    chat.thread.clone(),
                    t.worktree.clone(),
                    chat.profile_id.clone(),
                    t.page.tooltip(),
                ))
            })
        };
        assert!(pump_until(&ctx, 10, || attributes().is_some()));
        let (thread, worktree, profile, tooltip) = attributes().unwrap();
        assert_eq!(worktree.as_ref(), Some(&info));
        assert_eq!(profile.as_deref(), Some("claude-work"));
        assert!(tooltip.is_some_and(|t| t.contains("feat/x")));

        // Closed and reopened: the page starts without them and gets them from the store.
        let view = imp.tab_view.borrow().clone().unwrap();
        view.close_page(&view.nth_page(0));
        assert!(pump_until(&ctx, 5, || imp.tabs.borrow().is_empty()));
        imp.open_thread(&thread, true);
        assert!(pump_until(&ctx, 10, || attributes().is_some_and(
            |(_, w, p, _)| w.as_ref() == Some(&info) && p.as_deref() == Some("claude-work")
        )));

        // Two resumes of one native session: one thread, the one already holding it.
        let threads = || reader.list_threads(true).unwrap().len();
        let before = threads();
        imp.resume_as_thread(Driver::Claude, "native-1", Some(dir.clone()));
        assert!(pump_until(&ctx, 10, || threads() == before + 1));
        imp.resume_as_thread(Driver::Claude, "native-1", Some(dir));
        crate::testutil::settle(&ctx);
        assert_eq!(threads(), before + 1, "never a duplicate");

        window.destroy();
        crate::testutil::settle(&ctx);
        WRITER.with(|w| *w.borrow_mut() = None);
        STORE_PATH.with(|p| *p.borrow_mut() = None);
        STORE.with(|s| *s.borrow_mut() = None);
        assert_eq!(writer.failures().0, 0);
    }

    #[test]
    fn an_unsent_message_is_a_close_reason_of_its_own_beside_busy() {
        let only = |busy, draft| CloseReasons { busy, draft };
        assert_eq!(close_reasons(false, false), None, "nothing to lose");
        assert_eq!(close_reasons(true, false), Some(only(true, false)));
        assert_eq!(close_reasons(false, true), Some(only(false, true)));
        assert_eq!(close_reasons(true, true), Some(only(true, true)));

        let (heading, body, confirm) = close_question("Fix the build", only(false, true));
        assert_eq!(heading, "Discard the Unsent Message?");
        assert_eq!(
            body,
            "“Fix the build” has an unsent message. Closing the thread discards it."
        );
        assert_eq!(confirm, "Discard and Close");
        let (_, untitled, _) = close_question("", only(false, true));
        assert!(untitled.starts_with("This thread has an unsent message"));

        // Busy alone reads as it always did.
        let (heading, body, confirm) = close_question("Fix the build", only(true, false));
        assert_eq!(heading, "Stop the Running Turn and Close?");
        assert!(body.starts_with("“Fix the build” is working or waiting for you."));
        assert!(!body.contains("unsent"));
        assert_eq!(confirm, "Stop and Close");
        let (_, both, confirm) = close_question("Fix the build", only(true, true));
        assert!(both.contains("has an unsent message"), "{both}");
        assert!(both.contains("stops the agent"), "{both}");
        assert_eq!(confirm, "Stop and Close");
    }

    #[test]
    fn the_window_close_question_lists_running_threads_and_unsent_messages() {
        let r = |busy, draft| CloseReasons { busy, draft };
        assert_eq!(window_close_question(&[]), None);
        let (heading, body, confirm) =
            window_close_question(&[("Notes".into(), r(false, true))]).unwrap();
        assert_eq!(heading, "Discard the Unsent Message?");
        assert!(body.contains("• Notes has an unsent message"), "{body}");
        assert_eq!(confirm, "Discard and Close");
        let (heading, _, _) = window_close_question(&[
            ("Notes".into(), r(false, true)),
            (String::new(), r(false, true)),
        ])
        .unwrap();
        assert_eq!(heading, "Discard the Unsent Messages?");
        // Any running thread makes it the stop question, counting only the running ones.
        let (heading, body, confirm) = window_close_question(&[
            ("Build".into(), r(true, false)),
            (String::new(), r(false, true)),
        ])
        .unwrap();
        assert_eq!(heading, stop_heading(1));
        assert!(body.contains("• Build\n"), "{body}");
        assert!(
            body.contains("• New thread has an unsent message"),
            "{body}"
        );
        assert_eq!(confirm, "Stop and Close");
    }

    #[test]
    fn archive_and_delete_say_what_happened_only_from_the_writers_answer() {
        let toast = |text: &str, undo| WriteNotice::Toast {
            text: text.into(),
            undo,
        };
        assert_eq!(
            archive_notice(true, false, Some(Ok(()))),
            toast("Thread archived", true)
        );
        assert_eq!(
            archive_notice(true, true, Some(Ok(()))),
            toast("Archived; its running turn was stopped", true)
        );
        assert_eq!(
            archive_notice(false, true, Some(Ok(()))),
            toast("Thread restored", false),
            "Undo brings the thread back, and says nothing of resuming its turn"
        );
        let WriteNotice::Failure { body, .. } =
            archive_notice(true, true, Some(Err("disk".into())))
        else {
            panic!("a failure");
        };
        assert_eq!(
            body,
            "The agent was stopped, but the thread could not be archived: disk"
        );
        let WriteNotice::Failure { body, .. } = archive_notice(true, false, None) else {
            panic!("no answer is a failure");
        };
        assert!(body.starts_with("The thread could not be archived: the thread store"));

        assert_eq!(
            delete_notice(true, Some(Ok(()))),
            toast("Thread deleted", false)
        );
        let WriteNotice::Failure { heading, body } =
            delete_notice(true, Some(Err("locked".into())))
        else {
            panic!("a failure");
        };
        assert_eq!(heading, "Could Not Delete the Thread");
        assert_eq!(
            body,
            "The agent was stopped, but the thread could not be deleted: locked"
        );
        let WriteNotice::Failure { body, .. } = delete_notice(false, Some(Err("locked".into())))
        else {
            panic!("a failure");
        };
        assert_eq!(body, "The thread could not be deleted: locked");
    }

    #[test]
    fn a_discarded_thread_comes_back_with_its_choices_and_never_in_bypass() {
        let worktree = crate::worktree::WorktreeInfo {
            path: "/w/tree".into(),
            branch: "feat/x".into(),
            main_toplevel: "/w/main".into(),
        };
        let spec = DiscardedThread {
            driver: Driver::Codex,
            dir: "/w/tree".into(),
            profile_id: Some("codex-work".into()),
            worktree: Some(worktree.clone()),
            model: Some("gpt-5".into()),
            effort: Some("high".into()),
            mode: Some(Mode::Bypass),
        };
        let new = spec.new_thread();
        assert_eq!(new.driver, Driver::Codex);
        assert_eq!(
            new.dir.as_deref(),
            Some("/w/tree"),
            "the folder is explicit"
        );
        assert_eq!(new.profile_id.as_deref(), Some("codex-work"));
        assert_eq!(new.worktree.as_ref(), Some(&worktree));
        assert_eq!(new.model.as_deref(), Some("gpt-5"));
        assert_eq!(new.effort.as_deref(), Some("high"));
        assert_eq!(new.mode, Some(Mode::Ask), "never dangerous permissive mode");
        assert!(new.prompt.is_none() && new.handoff.is_none() && new.title.is_none());
        let plan = DiscardedThread {
            mode: Some(Mode::Plan),
            ..spec.clone()
        };
        assert_eq!(plan.new_thread().mode, Some(Mode::Plan));

        // A page closed before it was built knows only its agent and folder: the store fills in
        // the rest. A page with a session keeps its own, the session's model included.
        let unbuilt = DiscardedThread {
            profile_id: None,
            worktree: None,
            model: None,
            effort: None,
            mode: None,
            ..spec.clone()
        };
        let stored = StoredChoices {
            model: Some("default".into()),
            attributes: agent_kit::store::ThreadAttributes {
                profile_id: Some("codex-work".into()),
                worktree: Some(worktree.clone()),
            },
            mode: Some(Mode::Bypass),
        };
        let filled = unbuilt.clone().with_stored(stored.clone(), false);
        assert_eq!(filled.model, None, "the stored sentinel is the agent's own");
        assert_eq!(filled.profile_id.as_deref(), Some("codex-work"));
        assert_eq!(filled.worktree, Some(worktree));
        assert_eq!(
            filled.mode,
            Some(Mode::Ask),
            "never dangerous permissive mode"
        );
        let stored_opus = StoredChoices {
            model: Some("opus".into()),
            ..stored.clone()
        };
        assert_eq!(
            unbuilt
                .clone()
                .with_stored(stored_opus, false)
                .model
                .as_deref(),
            Some("opus")
        );
        assert_eq!(unbuilt.clone().with_stored(stored, true), unbuilt);
    }

    /// Through the store writer: an unsent message makes closing ask; a blank thread left is
    /// discarded with a toast whose Undo makes it again as it was (two toasts at once each undo
    /// their own); an archive's toast comes after the write and its Undo brings the thread back;
    /// with the writer gone, an Undo and a delete say they failed.
    #[test]
    #[ignore = "needs a display; run alone in an isolated display"]
    fn close_guards_ask_for_drafts_and_discards_and_archives_can_be_undone() {
        use crate::testutil::{pump_until, settle};
        gtk4::init().unwrap();
        adw::init().unwrap();
        crate::icons::register();
        let config_home = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", config_home.path());
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("threads.db");
        let reader = Rc::new(Store::open(&path).expect("open"));
        let writer = StoreWriter::spawn(&path).expect("writer");
        STORE.with(|s| *s.borrow_mut() = Some(reader.clone()));
        STORE_PATH.with(|p| *p.borrow_mut() = Some(path));
        WRITER.with(|w| *w.borrow_mut() = Some(writer.clone()));
        let app = adw::Application::builder()
            .application_id("org.test.CloseGuards")
            .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk4::gio::Cancellable>).unwrap();
        let _hold = app.hold();
        CHAT_RESTORED.with(|done| done.set(true));
        let window = crate::window::AgentTerminalWindow::new(&app);
        let ctx = glib::MainContext::default();
        assert!(pump_until(&ctx, 5, || window
            .imp()
            .tab_view
            .borrow()
            .is_some()));
        window.present();
        assert!(pump_until(&ctx, 5, || window.is_realized()));
        let imp = window.imp();
        imp.config.borrow_mut().profiles.push(Profile {
            id: "claude-work".into(),
            name: "Claude (work)".into(),
            ..crate::config::new_agent_profile(Driver::Claude)
        });
        let folder = tempfile::tempdir().unwrap();
        let dir = folder.path().to_string_lossy().into_owned();
        let other = tempfile::tempdir().unwrap();
        let other_dir = other.path().to_string_lossy().into_owned();
        let exists = |id: &str| reader.thread_summary(id).ok().flatten().is_some();
        let toasts = || -> Vec<adw::Toast> { imp.test_toasts.borrow().clone() };
        let titles = || -> Vec<String> {
            toasts()
                .iter()
                .map(|t| t.title().map(|s| s.to_string()).unwrap_or_default())
                .collect()
        };
        // The thread whose page is shown once a creation has opened it.
        let create = |new: NewThread| -> String {
            let before = imp.current_thread();
            assert!(imp.create_chat_thread(new).is_none(), "through the writer");
            assert!(pump_until(&ctx, 10, || imp
                .current_thread()
                .is_some_and(|t| Some(&t) != before.as_ref())));
            imp.current_thread().unwrap()
        };
        let built = |id: &str| {
            imp.tabs
                .borrow()
                .iter()
                .filter_map(|t| t.chat.as_ref())
                .find(|c| c.thread == id)
                .and_then(|c| c.view.clone())
        };
        // Answers the open dialog with `heading` by clicking `button`, as a user does.
        let answer = |heading: &str, button: &str| {
            let open = || {
                adw::prelude::AdwApplicationWindowExt::visible_dialog(&window)
                    .and_then(|d| d.downcast::<adw::AlertDialog>().ok())
                    .filter(|d| d.heading().as_deref() == Some(heading))
            };
            assert!(pump_until(&ctx, 5, || open().is_some()), "asks: {heading}");
            let dialog = open().expect("the dialog");
            let target = crate::testutil::descendants(dialog.upcast_ref())
                .into_iter()
                .filter_map(|w| w.downcast::<gtk4::Button>().ok())
                .find(|b| {
                    b.label().as_deref() == Some(button)
                        || crate::testutil::descendants(b.upcast_ref())
                            .iter()
                            .any(|w| {
                                w.downcast_ref::<gtk4::Label>()
                                    .is_some_and(|l| l.text() == button)
                            })
                })
                .unwrap_or_else(|| panic!("{heading} has a {button} button"));
            assert!(pump_until(&ctx, 10, || target.is_mapped() && target.is_sensitive()));
            settle(&ctx);
            target.emit_clicked();
            assert!(pump_until(&ctx, 10, || {
                adw::prelude::AdwApplicationWindowExt::visible_dialog(&window)
                    .is_none_or(|d| d != dialog.clone().upcast::<adw::Dialog>())
            }));
        };

        // A blank thread with every choice a user can make before typing anything.
        let first = create(NewThread {
            dir: Some(dir.clone()),
            profile_id: Some("claude-work".into()),
            model: Some("opus".into()),
            effort: Some("high".into()),
            mode: Some(Mode::Plan),
            ..NewThread::new(Driver::Claude)
        });
        assert!(pump_until(&ctx, 10, || built(&first).is_some()));
        let view = built(&first).unwrap();

        // An unsent message is a reason to ask, on its own; Cancel keeps the page.
        let key = RowKey::Thread(first.clone());
        assert_eq!(imp.row_close_reasons(&key), None);
        view.set_draft("half a thought");
        assert_eq!(
            imp.row_close_reasons(&key),
            Some(CloseReasons {
                busy: false,
                draft: true
            })
        );
        assert!(!imp.row_is_busy(&key), "a draft is not work");
        imp.close_row(&key);
        answer("Discard the Unsent Message?", "Cancel");
        assert!(
            imp.page_of_thread(&first).is_some(),
            "cancel keeps the page"
        );
        assert!(imp.confirm_close_busy(), "the window asks too");
        answer("Discard the Unsent Message?", "Cancel");
        view.set_draft("");

        // Left blank: discarded once the store agrees, then the toast; a second one while the
        // first toast is still up.
        let second = create(NewThread {
            dir: Some(other_dir.clone()),
            ..NewThread::new(Driver::Claude)
        });
        assert!(pump_until(&ctx, 10, || !exists(&first)
            && titles() == ["Empty thread discarded"]));
        let named = create(NewThread {
            title: Some("named".into()),
            ..NewThread::new(Driver::Claude)
        });
        assert!(pump_until(&ctx, 10, || !exists(&second)
            && titles()
                == ["Empty thread discarded", "Empty thread discarded"]));
        let undos = toasts();

        // Each Undo makes its own thread again, as it was.
        let threads_in = |d: &str| -> Vec<String> {
            reader
                .list_threads(true)
                .unwrap()
                .into_iter()
                .filter(|t| t.cwd == d)
                .map(|t| t.id)
                .collect()
        };
        undos[0].emit_by_name::<()>("button-clicked", &[]);
        assert!(pump_until(&ctx, 10, || threads_in(&dir).len() == 1));
        let again = threads_in(&dir).remove(0);
        assert_ne!(again, first, "a new thread");
        let summary = reader.thread_summary(&again).unwrap().unwrap();
        assert_eq!(summary.model.as_deref(), Some("opus"));
        let modes: Vec<Mode> = reader
            .events(&again, None, 100)
            .unwrap()
            .into_iter()
            .filter_map(|(_, e)| match e.event {
                Event::ModeChosen { mode } => Some(mode),
                _ => None,
            })
            .collect();
        assert_eq!(modes, [Mode::Plan]);
        assert!(pump_until(&ctx, 10, || imp.tabs.borrow().iter().any(|t| {
            t.chat.as_ref().is_some_and(|c| {
                c.thread == again && c.profile_id.as_deref() == Some("claude-work")
            }) && t.dir == dir
        })));

        // Archive: its toast comes once written, and Undo brings the thread back.
        let archived = || reader.thread_summary(&named).unwrap().unwrap().archived;
        imp.set_thread_archived(&named, true);
        assert!(pump_until(&ctx, 10, || archived()
            && titles().last().map(String::as_str)
                == Some("Thread archived")));
        assert!(imp.page_of_thread(&named).is_none(), "its page closed");
        toasts()
            .last()
            .unwrap()
            .emit_by_name::<()>("button-clicked", &[]);
        assert!(pump_until(&ctx, 10, || !archived()
            && titles().last().map(String::as_str)
                == Some("Thread restored")));

        // Deleting a blank thread is a delete: no discard, so nothing to undo.
        let discards = || {
            titles()
                .iter()
                .filter(|t| *t == "Empty thread discarded")
                .count()
        };
        let doomed = create(NewThread::new(Driver::Claude));
        assert!(
            pump_until(&ctx, 10, || !exists(&again) && discards() == 3),
            "the recreated thread, left blank, is discarded again"
        );
        imp.delete_thread_now(&doomed);
        assert!(pump_until(&ctx, 10, || !exists(&doomed)
            && titles().last().map(String::as_str)
                == Some("Thread deleted")));
        settle(&ctx);
        assert_eq!(discards(), 3, "a deleted thread offers no Undo");

        // Archiving a blank thread keeps it, archived: its page's close discards nothing.
        let shelved = create(NewThread::new(Driver::Claude));
        imp.set_thread_archived(&shelved, true);
        assert!(pump_until(&ctx, 10, || reader
            .thread_summary(&shelved)
            .unwrap()
            .is_some_and(|t| t.archived)
            && titles().last().map(String::as_str)
                == Some("Thread archived")));
        settle(&ctx);
        assert!(
            exists(&shelved),
            "an archived blank thread is never deleted"
        );
        assert_eq!(discards(), 3, "and offers no discard Undo");
        // The guard itself, with no archive write to save the thread: a blank page closed this
        // way queues no discard at all, so no write order can delete it.
        let kept = create(NewThread::new(Driver::Claude));
        imp.close_row_without_discard(&kept);
        assert!(pump_until(&ctx, 5, || imp.page_of_thread(&kept).is_none()));
        writer.flush(WORKER_FLUSH_WAIT);
        settle(&ctx);
        assert!(exists(&kept), "no discard was queued");
        assert_eq!(discards(), 3);

        // A page closed before it was ever built: Undo still gets back what the store held.
        let tree = tempfile::tempdir().unwrap();
        let tree_dir = tree.path().to_string_lossy().into_owned();
        let info = crate::worktree::WorktreeInfo {
            path: tree.path().to_path_buf(),
            branch: "feat/y".into(),
            main_toplevel: tree.path().join("main"),
        };
        let handle = app_handle();
        let ids = NewThreadIds {
            thread: handle.new_id().unwrap(),
            provider: handle.new_id().unwrap(),
        };
        let unbuilt = ids.thread.clone();
        let spec = NewThreadSpec {
            model: Some("sonnet".into()),
            mode: Some(Mode::Plan),
            attributes: agent_kit::store::ThreadAttributes {
                profile_id: Some("claude-work".into()),
                worktree: Some(info.clone()),
            },
            ..NewThreadSpec::new(&tree_dir, Driver::Claude)
        };
        handle
            .write("create a thread", Some(&unbuilt), move |s| {
                record_new_thread_as(s, &ids, &spec)
            })
            .unwrap();
        assert!(pump_until(&ctx, 10, || exists(&unbuilt)));
        imp.reload_summaries();
        assert!(imp.open_thread(&unbuilt, false).is_some());
        assert!(built(&unbuilt).is_none(), "never shown, never built");
        imp.close_row_now(&RowKey::Thread(unbuilt.clone()));
        assert!(pump_until(&ctx, 10, || !exists(&unbuilt) && discards() == 4));
        toasts()
            .last()
            .unwrap()
            .emit_by_name::<()>("button-clicked", &[]);
        assert!(pump_until(&ctx, 10, || threads_in(&tree_dir).len() == 1));
        let back = threads_in(&tree_dir).remove(0);
        assert!(pump_until(&ctx, 10, || imp.page_of_thread(&back).is_some()));
        assert_eq!(
            reader
                .thread_summary(&back)
                .unwrap()
                .unwrap()
                .model
                .as_deref(),
            Some("sonnet")
        );
        let modes: Vec<Mode> = reader
            .events(&back, None, 100)
            .unwrap()
            .into_iter()
            .filter_map(|(_, e)| match e.event {
                Event::ModeChosen { mode } => Some(mode),
                _ => None,
            })
            .collect();
        assert_eq!(modes, [Mode::Plan]);
        let attributes = reader.thread_attributes(&back).unwrap();
        assert_eq!(attributes.profile_id.as_deref(), Some("claude-work"));
        assert_eq!(attributes.worktree.as_ref(), Some(&info));

        // The writer gone: the second Undo says it could not make the thread, and a delete
        // says it could not delete.
        let pages = imp.thread_pages();
        writer.shutdown(WORKER_FLUSH_WAIT);
        undos[1].emit_by_name::<()>("button-clicked", &[]);
        answer("The Thread Is Not Ready Yet", "OK");
        assert!(threads_in(&other_dir).is_empty());
        assert_eq!(imp.thread_pages(), pages, "no page for a thread not stored");
        imp.delete_thread_now(&named);
        answer("Could Not Delete the Thread", "OK");
        assert!(exists(&named), "still there");

        window.destroy();
        settle(&ctx);
        WRITER.with(|w| *w.borrow_mut() = None);
        STORE_PATH.with(|p| *p.borrow_mut() = None);
        STORE.with(|s| *s.borrow_mut() = None);
    }
}
