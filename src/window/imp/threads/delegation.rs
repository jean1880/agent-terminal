//! The narrow application services used by the app-owned delegation supervisor.

use super::*;
use crate::chat::WorkerAction;
use crate::delegation::seam::WorkerSubscriber;
use crate::delegation::seam::{ChildStop, ResolvedChild};
use agent_core::delegation::scope::{Scope, ScopedEnvelope};
use agent_core::delegation::wire::WorkerSummary;

/// The thread's notice when an opted-in Antigravity root starts unbound because its managed
/// registration is unusable (`Supervisor::agy_root_problem`).
pub(super) fn unbound_notice(reason: &str) -> String {
    format!(
        "Antigravity started without cross-agent delegation: {reason}. Set it up in Settings → \
         Agents → Cross-agent delegation (Configure / Repair), then reopen this thread."
    )
}

/// A preparation continuation belongs to one slot and one start attempt. Stop, transfer and
/// a newer attempt invalidate it; clearing an old attempt must not clear a newer spinner.
pub(super) struct PendingStart {
    slot: std::rc::Weak<SessionSlot>,
    serial: u64,
    attempt: u64,
}

impl PendingStart {
    pub(super) fn new(slot: &Rc<SessionSlot>) -> Self {
        let attempt = slot.start_attempt.get().wrapping_add(1);
        slot.start_attempt.set(attempt);
        Self {
            slot: Rc::downgrade(slot),
            serial: slot.switch_serial.get(),
            attempt,
        }
    }

    pub(super) fn current(&self, window: &AgentTerminalWindow, thread: &str) -> bool {
        self.slot.upgrade().is_some_and(|expected| {
            expected.switch_serial.get() == self.serial
                && expected.start_attempt.get() == self.attempt
                && window.session_start_allowed()
                && thread_owner(thread).as_ref() == Some(&*window.obj())
                && window
                    .slot_of(thread)
                    .is_some_and(|live| Rc::ptr_eq(&live, &expected))
        })
    }

    pub(super) fn clear_cancelled(&self, window: &AgentTerminalWindow, thread: &str) {
        let Some(expected) = self.slot.upgrade() else {
            return;
        };
        if expected.start_attempt.get() != self.attempt {
            return;
        }
        if let Some(chat) = window
            .tabs
            .borrow_mut()
            .iter_mut()
            .filter_map(|tab| tab.chat.as_mut())
            .find(|chat| chat.thread == thread && Rc::ptr_eq(&chat.slot, &expected))
        {
            chat.starting = false;
        }
    }
}

#[cfg(test)]
#[path = "delegation_tests.rs"]
pub(super) mod tests;

/// One weak UI subscriber also records diffs in each child's actual working directory.
struct ThreadWorkerSubscriber {
    slot: std::rc::Weak<SessionSlot>,
    view: Rc<dyn WorkerSubscriber>,
}

impl WorkerSubscriber for ThreadWorkerSubscriber {
    fn scoped(&self, event: &ScopedEnvelope) {
        if let Some(slot) = self.slot.upgrade() {
            let existing = slot.worker_diffs.borrow().get(&event.scope).cloned();
            let diffs = existing.or_else(|| {
                let supervisor = crate::delegation::supervisor()?;
                let directory = supervisor.child_workspace(&event.scope.worker)?;
                let directory = directory.to_str()?;
                let view = slot.view.borrow().clone();
                let source = ThreadDiffs::new(
                    directory,
                    next_tab_key(),
                    "delegated agent",
                    Rc::new(move |text| {
                        if let Some(window) = view
                            .as_ref()
                            .and_then(|v| v.upgrade())
                            .as_ref()
                            .and_then(window_of)
                        {
                            window.imp().show_toast(text);
                        }
                    }),
                );
                slot.worker_diffs
                    .borrow_mut()
                    .insert(event.scope.clone(), source.clone());
                Some(source)
            });
            if let Some(diffs) = diffs {
                match &event.envelope.event {
                    Event::TurnStarted { .. } => {
                        diffs.turn_started(SharedConfig::default().borrow().checkpoints)
                    }
                    Event::ItemStarted {
                        kind: ItemKind::FileChange,
                        ..
                    } => {
                        if let Some(item) = &event.envelope.item {
                            diffs.item_started(item);
                        }
                    }
                    Event::TurnCompleted { .. } | Event::SessionExited { .. } => diffs.turn_ended(),
                    _ => (),
                }
            }
        }
        self.view.scoped(event);
    }

    fn replayed(&self, event: &ScopedEnvelope) {
        self.view.replayed(event);
    }
    fn registry(&self, root: &str, workers: &[WorkerSummary]) {
        self.view.registry(root, workers);
        if let Some(window) = thread_owner(root) {
            // A child finishing may release a root's held notification; defer to avoid
            // re-entering the supervisor while it publishes the registry.
            let window = window.downgrade();
            let root = root.to_owned();
            glib::idle_add_local_once(move || {
                if let Some(window) = window.upgrade() {
                    window.imp().refresh_sidebar();
                    window.imp().release_held_finish(&root);
                }
            });
        }
    }
}

fn scope_matches_slot(slot: &SessionSlot, scope: &Scope) -> bool {
    // The supervisor owns worker→root membership; the view cannot nominate another open root.
    slot.thread.as_deref().is_some_and(|thread| {
        crate::delegation::supervisor()
            .is_some_and(|s| s.scope_belongs_to_thread(scope, thread) && s.scope_is_current(scope))
    })
}

pub(super) fn worker_action(slot: &SessionSlot, action: WorkerAction) -> bool {
    let scope = match &action {
        WorkerAction::Send { scope, .. }
        | WorkerAction::Cancel { scope }
        | WorkerAction::Close { scope }
        | WorkerAction::Approve { scope, .. }
        | WorkerAction::Answer { scope, .. }
        | WorkerAction::Form { scope, .. } => scope,
    };
    if !scope_matches_slot(slot, scope) {
        return false;
    }
    let Some(supervisor) = crate::delegation::supervisor() else {
        return false;
    };
    match action {
        WorkerAction::Send { scope, text } => supervisor.submit_human(&scope.worker, &text),
        WorkerAction::Cancel { scope } => supervisor.cancel_human(&scope),
        WorkerAction::Close { scope } => supervisor.close_human(&scope),
        WorkerAction::Approve {
            scope,
            request,
            decision,
            custom_rule,
        } => {
            let Some(session) = supervisor.checked_child_session(&scope) else {
                return false;
            };
            session.respond_approval_with_rule(&request, decision, custom_rule);
        }
        WorkerAction::Answer {
            scope,
            request,
            answers,
        } => {
            let Some(session) = supervisor.checked_child_session(&scope) else {
                return false;
            };
            session.answer_questions(&request, answers);
        }
        WorkerAction::Form {
            scope,
            request,
            action,
            content,
        } => {
            let Some(session) = supervisor.checked_child_session(&scope) else {
                return false;
            };
            return session.respond_form(&request, action, content);
        }
    }
    true
}

pub(super) fn switch(
    slot: &SessionSlot,
    driver: Driver,
    model: Option<String>,
    effort: Option<String>,
) {
    let Some(session) = slot.get() else {
        return;
    };
    let Some((supervisor, thread)) = crate::delegation::supervisor().zip(slot.thread.clone())
    else {
        session.switch(driver, model, effort);
        session.rebind_start();
        return;
    };
    // The thread's own profile on its own agent; another agent's default on a switch.
    let profile = match super::thread_owner(&thread) {
        Some(window) => window.imp().thread_profile(&thread, driver),
        None => SharedConfig::default()
            .borrow()
            .agent_profile(driver)
            .cloned()
            .unwrap_or_else(|| crate::config::new_agent_profile(driver)),
    };
    let serial = slot.switch_serial.clone();
    let chosen = serial.get().wrapping_add(1);
    serial.set(chosen);
    glib::MainContext::default().spawn_local(async move {
        let problem = supervisor.agy_root_problem(&profile, driver).await;
        if serial.get() != chosen
            || !supervisor
                .prepare_root_switch(&thread, &profile, driver, problem.is_none())
                .await
            || serial.get() != chosen
        {
            return;
        }
        session.switch(driver, model, effort);
        if let Some(reason) = problem {
            session.note(unbound_notice(&reason));
        }
        session.rebind_start();
    });
}

pub(super) fn stop_target(stop: ChildStop) -> super::super::closing::StopTarget {
    super::super::closing::StopTarget {
        agent: stop.agent,
        title: stop.title,
        watch: stop.watch,
    }
}

impl AgentTerminalWindow {
    pub(in crate::window::imp) fn start_transferred_threads(&self) {
        let pages: Vec<_> = self
            .tabs
            .borrow()
            .iter()
            .filter(|tab| tab.chat.as_ref().is_some_and(|chat| chat.view.is_none()))
            .map(|tab| tab.page.clone())
            .collect();
        for page in pages {
            self.ensure_built(&page);
        }
        self.start_pending_sessions();
    }

    pub(in crate::window::imp) fn resume_transferred_thread(
        &self,
        thread: &str,
        slot: &Rc<SessionSlot>,
    ) {
        let Some(session) = slot.get() else {
            return;
        };
        let driver = slot.driver();
        let profile = self.thread_profile(thread, driver);
        let thread = thread.to_owned();
        let serial = slot.switch_serial.clone();
        let chosen = serial.get();
        glib::MainContext::default().spawn_local(async move {
            let (prepared, problem) = match crate::delegation::supervisor() {
                Some(supervisor) => {
                    let problem = supervisor.agy_root_problem(&profile, driver).await;
                    let prepared = supervisor
                        .root_transfer_resume(&thread, &profile, driver, problem.is_none())
                        .await;
                    (prepared, problem)
                }
                None => (true, None),
            };
            // Closing, Stop or a newer transfer invalidates this continuation.
            if prepared && serial.get() == chosen && thread_owner(&thread).is_some() {
                if let Some(reason) = problem {
                    session.note(unbound_notice(&reason));
                }
                session.rebind_start();
            }
        });
    }

    pub(super) fn attach_worker_view(&self, thread: &str, slot: &Rc<SessionSlot>, view: &ChatView) {
        if cfg!(test) && crate::delegation::supervisor().is_none() {
            return;
        }
        let thread = thread.to_owned();
        let weak_slot = Rc::downgrade(slot);
        let weak_view = view.downgrade();
        glib::MainContext::default().spawn_local(async move {
            let Some(supervisor) = crate::delegation::ready().await else {
                return;
            };
            let (Some(slot), Some(view)) = (weak_slot.upgrade(), weak_view.upgrade()) else {
                return;
            };
            let subscriber: Rc<dyn WorkerSubscriber> = Rc::new(ThreadWorkerSubscriber {
                slot: Rc::downgrade(&slot),
                view: view.delegation_subscriber(&thread),
            });
            *slot.worker_subscriber.borrow_mut() = Some(subscriber.clone());
            supervisor.subscribe(&thread, Rc::downgrade(&subscriber));
        });
    }

    pub(super) fn start_delegation(&self) {
        // Construction tests never bind sockets or inspect the user's agent configuration.
        if cfg!(test) {
            return;
        }
        let command = match std::env::current_exe() {
            Ok(command) if command.is_absolute() => command,
            _ => {
                warn!("Delegation unavailable: cannot locate this executable");
                return;
            }
        };
        let config = self.config.clone();
        let profiles = config.clone();
        let tags_config = config.clone();
        let weak = self.obj().downgrade();
        // usage.json for tools outside the app; once per process.
        crate::usage_file::install();
        let tags_cache = Rc::new(crate::model_tags_file::TagsCache::default());
        let catalog = crate::delegation::services::CatalogServices {
            models: Rc::new(|driver| {
                let availability = AgentAvailability::shared();
                crate::model_catalog::only_ready(
                    crate::model_catalog::ModelCatalog::shared().models_of(driver),
                    |d| availability.is_ready(d),
                )
            }),
            usage: Rc::new(|driver| {
                Some(crate::account_status::AccountStatus::shared().snapshot(driver))
            }),
            tags: Rc::new(move || -> crate::delegation::seam::LocalFuture<crate::model_tags_file::TagsState> {
                let (cache, path) = (tags_cache.clone(), tags_config.borrow().model_tags_file());
                std::boxed::Box::pin(async move { cache.current(path).await })
            }),
            refresh_usage: Rc::new(|wait: std::time::Duration| -> crate::delegation::seam::LocalFuture<()> {
                std::boxed::Box::pin(async move {
                    // The targets of the window's last availability scan: only agents that are
                    // ready, in the environment their threads get.
                    let targets = crate::model_catalog::ModelCatalog::shared().last_targets();
                    crate::account_status::AccountStatus::shared()
                        .refresh_and_wait(
                            &targets,
                            crate::delegation::services::USAGE_REFRESH_FLOOR,
                            crate::freshness::now_epoch(),
                            wait,
                        )
                        .await;
                })
            }),
        };
        crate::delegation::start(crate::delegation::AppServices {
            handle: app_handle(),
            store_path: STORE_PATH.with(|p| p.borrow().clone()),
            app_pid: std::process::id(),
            bridge_command: command,
            profiles: Rc::new(move || profiles.borrow().profiles.clone()),
            resolve: Rc::new(move |profile| {
                let driver = profile_driver(profile)?;
                if profile.disabled || !AgentAvailability::shared().is_ready(driver) {
                    return None;
                }
                let resolved = cached_agent(profile)?;
                if resolved.env_failed {
                    return None;
                }
                Some(ResolvedChild {
                    program: resolved.program?,
                    extra_args: profile.args.clone(),
                    env: AgentEnv {
                        env: resolved.env,
                        unset: unset_list(&config.borrow().clear_env),
                    },
                    default_model: profile.default_model.clone(),
                    default_effort: profile.default_effort.clone(),
                })
            }),
            make_adapter: Rc::new(make_adapter),
            hook_check: Rc::new(|target| std::boxed::Box::pin(check_hook(target))),
            tab_open_in: Rc::new(|path| {
                // No disk access on the UI path: workspace removal also proves containment
                // and ownership on its worker before invoking git without --force.
                let windows: Vec<_> = OPEN_THREADS.with(|registry| {
                    registry
                        .borrow()
                        .values()
                        .filter_map(|w| w.upgrade())
                        .collect()
                });
                windows.iter().any(|window| {
                    window
                        .imp()
                        .tabs
                        .borrow()
                        .iter()
                        .any(|tab| std::path::Path::new(&tab.dir).starts_with(path))
                })
            }),
            report: Rc::new(move |text| {
                warn!(message = %agent_core::redact::redact(text), "Delegation");
                if let Some(obj) = weak.upgrade() {
                    obj.imp().show_toast(text);
                }
            }),
            catalog,
        });
    }
}
