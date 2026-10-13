//! Closing the window while agents are still on their way out.
//!
//! Stopping a thread sends its agent SIGTERM, and an agent takes a moment to leave (Claude about
//! 0.4 s, measured). The app used to spend that moment blocked in `agent_proc::terminate_all` at
//! quit, its window frozen on screen. Now [`AgentTerminalWindow::begin_closing`] keeps the window
//! alive and responsive instead: it stops taking input, waits for each agent's real exit
//! (`ExitWatch`), and then closes, so `terminate_all` finds nothing left and returns at once.
//!
//! If the wait outlasts [`SHOW_AFTER`], the whole window becomes the closing screen: the packing
//! animation ([`super::pack_animation`]) with one line below it naming the current step, and,
//! after [`QUIT_NOW_AFTER`], a quiet "Quit Now" that kills what is left. A fast close never shows
//! it. An agent that ignores SIGTERM is killed after `agent_proc`'s grace, as before; nothing
//! waits longer than [`CLOSE_CAP`].
//!
//! Once the agents are gone, the window also waits (off the main thread, same cap) for the store
//! writer to commit what is queued, their closing events included, so the app's own shutdown
//! finds the queue empty.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk4::glib;
use tracing::{debug, error, info, warn};

use super::AgentTerminalWindow;
use crate::agent_proc::{ExitWatch, StopPhase};

/// A wait shorter than this closes without showing the closing screen (no flash).
pub(super) const SHOW_AFTER: Duration = Duration::from_millis(150);
/// When "Quit Now" is offered.
pub(super) const QUIT_NOW_AFTER: Duration = Duration::from_millis(1500);
/// The longest the window waits for its agents; whatever is left is killed.
pub(super) const CLOSE_CAP: Duration = Duration::from_secs(5);
/// How long the closing screen waits for the store before it says it is not keeping up and
/// makes Quit Now the way out.
pub(super) const SAVE_DEADLINE: Duration = Duration::from_secs(10);
/// The longest thread title the step line quotes in full.
const TITLE_CHARS: usize = 40;

/// One agent the window waits for: whose it is (for the step line) and its process.
pub(super) struct StopTarget {
    /// The agent's display name ("Claude").
    pub(super) agent: String,
    /// The thread's title; may be empty.
    pub(super) title: String,
    pub(super) watch: ExitWatch,
}

/// The step line for agents in the given phases: what the window is waiting on, in a few calm
/// words. `(agent, thread title, phase)`.
pub(super) fn step_line(targets: &[(&str, &str, StopPhase)]) -> String {
    let waiting: Vec<_> = targets
        .iter()
        .filter(|(_, _, phase)| *phase != StopPhase::Exited)
        .collect();
    let killing: Vec<_> = waiting
        .iter()
        .filter(|(_, _, phase)| *phase == StopPhase::Killing)
        .collect();
    match (waiting.as_slice(), killing.as_slice()) {
        ([], _) => "Closing…".to_owned(),
        (_, [(agent, _, _)]) => format!("{agent} is not responding, stopping it now…"),
        (_, [_, ..]) => format!(
            "{} agents are not responding, stopping them now…",
            killing.len()
        ),
        ([(agent, title, _)], []) if title.trim().is_empty() => format!("Stopping {agent}…"),
        ([(agent, title, _)], []) => format!("Stopping {agent} in {}…", short_title(title)),
        (_, []) => format!("Stopping {} agents…", waiting.len()),
    }
}

/// The step line while the store saves: how many writes are left, and past the deadline, what
/// Quit Now would leave unsaved.
pub(super) fn saving_line(left: usize, late: bool) -> String {
    let writes = if left == 1 { "write" } else { "writes" };
    match (left, late) {
        (0, _) => "Saving threads…".to_owned(),
        (_, false) => format!("Saving threads… ({left} {writes} left)"),
        (_, true) => format!(
            "The thread store is not keeping up: {left} {writes} still waiting. \
             Quit Now leaves them unsaved."
        ),
    }
}

/// `title`, cut to [`TITLE_CHARS`]. No ellipsis of its own: the line's closing one serves.
fn short_title(title: &str) -> String {
    let title = title.trim();
    if title.chars().count() <= TITLE_CHARS {
        return title.to_owned();
    }
    let mut cut: String = title.chars().take(TITLE_CHARS).collect();
    cut.truncate(cut.trim_end().len());
    cut
}

/// The closing screen's timers, each `Some` while pending (a fired one is spent and must not be
/// removed again).
#[derive(Default)]
struct Timers {
    /// Puts the closing screen up ([`SHOW_AFTER`]).
    show: Option<glib::SourceId>,
    /// Offers Quit Now ([`QUIT_NOW_AFTER`]).
    quit_now: Option<glib::SourceId>,
    /// Gives up waiting ([`CLOSE_CAP`]).
    cap: Option<glib::SourceId>,
}

impl Timers {
    /// The slot of the timer `pick` names.
    fn slot(&mut self, pick: TimerKind) -> &mut Option<glib::SourceId> {
        match pick {
            TimerKind::Show => &mut self.show,
            TimerKind::QuitNow => &mut self.quit_now,
            TimerKind::Cap => &mut self.cap,
        }
    }
}

#[derive(Clone, Copy)]
enum TimerKind {
    Show,
    QuitNow,
    Cap,
}

/// The closing screen and what it waits on. Held by the window until it goes.
pub(super) struct Closing {
    targets: Vec<StopTarget>,
    started: Instant,
    /// The whole window while closing: the animation, the step line and "Quit Now".
    page: gtk4::WindowHandle,
    animation: super::pack_animation::PackAnimation,
    /// The step line. It is the live region itself (role Status): screen readers hear its text
    /// change, with no separate property to keep in step.
    step: gtk4::Label,
    quit_now: gtk4::Revealer,
    /// The window's own content, kept alive (not destroyed) while the closing screen is shown.
    content: RefCell<Option<gtk4::Widget>>,
    /// Swallows keys while the old content is still on screen; removed with it, so the closing
    /// screen's own button can be reached from the keyboard.
    key_block: RefCell<Option<gtk4::EventControllerKey>>,
    timers: RefCell<Timers>,
    /// Waiting for the store writer to commit the agents' last events.
    saving: Cell<bool>,
    finished: Cell<bool>,
    window: glib::WeakRef<super::super::AgentTerminalWindow>,
}

impl Closing {
    fn new(window: &super::super::AgentTerminalWindow, targets: Vec<StopTarget>) -> Rc<Self> {
        let animation = super::pack_animation::PackAnimation::new();
        let step = gtk4::Label::builder()
            .wrap(true)
            .justify(gtk4::Justification::Center)
            .max_width_chars(48)
            .css_classes(["closing-step"])
            .accessible_role(gtk4::AccessibleRole::Status)
            .build();
        let quit_button = gtk4::Button::builder()
            .label("Quit Now")
            .halign(gtk4::Align::Center)
            .css_classes(["flat", "closing-quit"])
            .tooltip_text("Stop the remaining agents at once and close")
            .build();
        let quit_now = gtk4::Revealer::builder()
            .transition_type(gtk4::RevealerTransitionType::Crossfade)
            .transition_duration(250)
            .child(&quit_button)
            .build();
        // A crossfading revealer keeps its size while hidden: the line above never moves.
        quit_button.set_can_target(false);
        quit_button.set_can_focus(false);
        let status = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(14)
            .halign(gtk4::Align::Center)
            .valign(gtk4::Align::Center)
            .build();
        status.append(animation.widget());
        status.append(&step);
        status.append(&quit_now);
        let page = gtk4::WindowHandle::builder()
            .child(&status)
            .hexpand(true)
            .vexpand(true)
            .css_classes(["closing-screen"])
            .build();
        let closing = Rc::new(Self {
            targets,
            started: Instant::now(),
            page,
            animation,
            step,
            quit_now,
            content: RefCell::new(None),
            key_block: RefCell::new(None),
            timers: RefCell::new(Timers::default()),
            saving: Cell::new(false),
            finished: Cell::new(false),
            window: window.downgrade(),
        });
        let weak = Rc::downgrade(&closing);
        quit_button.connect_clicked(move |_| {
            if let Some(closing) = weak.upgrade() {
                let pending = super::threads::store_writes_pending();
                if pending > 0 {
                    warn!(pending, "close: Quit Now with store writes still queued");
                } else {
                    info!("close: Quit Now");
                }
                closing.kill_all();
                closing.finish();
            }
        });
        closing
    }

    /// Says the current step, and closes once nothing is left to wait for.
    fn update(&self) {
        let phases: Vec<(&str, &str, StopPhase)> = self
            .targets
            .iter()
            .map(|t| (t.agent.as_str(), t.title.as_str(), t.watch.phase()))
            .collect();
        let line = step_line(&phases);
        // Once saving, the saving line (kept by its own timer) stays.
        if !self.saving.get() && self.step.text().as_str() != line {
            self.step.set_text(&line);
        }
        if phases.iter().all(|(_, _, p)| *p == StopPhase::Exited) {
            self.save_then_finish();
        }
    }

    /// The agents are gone and their last events are queued for the store: the window goes
    /// once the store writer has committed them, waited for off the main thread (the window
    /// stays responsive, the cap still applies).
    fn save_then_finish(&self) {
        let Some(writer) = super::threads::store_writer().filter(|w| w.pending() > 0) else {
            self.finish();
            return;
        };
        if self.saving.replace(true) {
            return;
        }
        debug!(
            pending = writer.pending(),
            "close: waiting for the store to save"
        );
        // Until it has saved (or the user quits now): the window does not go with writes still
        // on their way, the cap included. Past the deadline it says how many are left, and
        // Quit Now becomes the way out.
        self.step.set_text("Saving threads…");
        let started = Instant::now();
        let ticker = self.window.clone();
        glib::timeout_add_local(Duration::from_secs(1), move || {
            let closing = ticker
                .upgrade()
                .and_then(|w| w.imp().closing.borrow().clone());
            let Some(closing) = closing.filter(|c| !c.finished.get()) else {
                return glib::ControlFlow::Break;
            };
            let left = super::threads::store_writes_pending();
            let line = saving_line(left, started.elapsed() >= SAVE_DEADLINE);
            if closing.step.text().as_str() != line {
                closing.step.set_text(&line);
            }
            if started.elapsed() >= SAVE_DEADLINE {
                closing.offer_quit_now();
                if let Some(button) = closing.quit_now.child() {
                    if !button.has_css_class("suggested-action") {
                        warn!(left, "close: the store is not keeping up; Quit Now offered");
                        button.add_css_class("suggested-action");
                        button.grab_focus();
                    }
                }
            }
            glib::ControlFlow::Continue
        });
        let window = self.window.clone();
        let barrier = writer.barrier();
        glib::spawn_future_local(async move {
            let saved = gtk4::gio::spawn_blocking(move || barrier.recv().is_ok())
                .await
                .unwrap_or(false);
            if !saved {
                error!("close: the store writer stopped before it had saved everything");
            }
            let closing = window
                .upgrade()
                .and_then(|w| w.imp().closing.borrow().clone());
            if let Some(closing) = closing {
                closing.finish();
            }
        });
    }

    fn kill_all(&self) {
        for target in &self.targets {
            target.watch.kill_now();
        }
    }

    /// Puts the closing screen up in place of the window's content.
    fn show(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        if self.finished.get() {
            return;
        }
        self.content.replace(window.content());
        window.set_content(Some(&self.page));
        if let Some(keys) = self.key_block.take() {
            window.remove_controller(&keys);
        }
        debug!(
            ms = self.started.elapsed().as_millis(),
            animated = self.animation.is_playing(),
            "close: showing the closing screen"
        );
    }

    fn offer_quit_now(&self) {
        if let Some(button) = self.quit_now.child() {
            button.set_can_target(true);
            button.set_can_focus(true);
        }
        self.quit_now.set_reveal_child(true);
    }

    /// Closes the window for good, once (from an idle: never inside a process callback).
    fn finish(&self) {
        if self.finished.replace(true) {
            return;
        }
        let Timers {
            show,
            quit_now,
            cap,
        } = self.timers.take();
        for timer in [show, quit_now, cap].into_iter().flatten() {
            timer.remove();
        }
        info!(
            ms = self.started.elapsed().as_millis(),
            "close: agents stopped, closing the window"
        );
        let window = self.window.clone();
        glib::idle_add_local_once(move || {
            let Some(window) = window.upgrade() else {
                return;
            };
            window.imp().close_done.set(true);
            window.close();
        });
    }
}

impl AgentTerminalWindow {
    /// The window is waiting for its agents to exit before it goes.
    pub(in crate::window) fn is_closing(&self) -> bool {
        self.closing.borrow().is_some()
    }

    /// Waits for `targets` to exit before the window goes; see the module docs. The window takes
    /// no more input from here on.
    pub(super) fn begin_closing(&self, targets: Vec<StopTarget>) {
        let window = self.obj();
        info!(count = targets.len(), "close: waiting for agents to exit");
        // Nothing new may start: no action (menus, shortcuts), no click, no key.
        for name in window.list_actions() {
            if let Some(action) = window
                .lookup_action(&name)
                .and_downcast::<gtk4::gio::SimpleAction>()
            {
                action.set_enabled(false);
            }
        }
        if let Some(content) = window.content() {
            content.set_can_target(false);
        }
        GtkWindowExt::set_focus(&*window, None::<&gtk4::Widget>);
        let keys = gtk4::EventControllerKey::new();
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        keys.connect_key_pressed(|_, _, _, _| glib::Propagation::Stop);
        window.add_controller(keys.clone());

        let closing = Closing::new(&window, targets);
        closing.key_block.replace(Some(keys));
        for target in &closing.targets {
            let weak = Rc::downgrade(&closing);
            target.watch.connect_phase(move |_| {
                if let Some(closing) = weak.upgrade() {
                    closing.update();
                }
            });
        }
        let timer = |kind: TimerKind, after: Duration, f: fn(&Closing)| {
            let weak = Rc::downgrade(&closing);
            let id = glib::timeout_add_local_once(after, move || {
                if let Some(closing) = weak.upgrade() {
                    // Fired: its id is spent, and must not be removed again.
                    closing.timers.borrow_mut().slot(kind).take();
                    f(&closing);
                }
            });
            *closing.timers.borrow_mut().slot(kind) = Some(id);
        };
        timer(TimerKind::Show, SHOW_AFTER, Closing::show);
        timer(TimerKind::QuitNow, QUIT_NOW_AFTER, Closing::offer_quit_now);
        timer(TimerKind::Cap, CLOSE_CAP, |closing| {
            warn!("close: agents still running at the cap; killing them");
            closing.kill_all();
            // Gone now, unless the store still has writes queued: those it waits for (Quit Now
            // stays on offer).
            closing.save_then_finish();
        });
        self.closing.replace(Some(closing.clone()));
        closing.update();
    }

    /// Agents for the closing demo (`AGENT_TERMINAL_DEMO_THREADS=1` with
    /// `AGENT_TERMINAL_DEMO_SCENE=closing`): one real process per open thread (at most two),
    /// each ignoring SIGTERM, so the closing screen plays through its steps for screenshots.
    pub(super) fn demo_stop_targets(&self) -> Vec<StopTarget> {
        #[cfg(not(test))]
        let wanted = crate::chat::view::demo::demo_threads_requested()
            && std::env::var("AGENT_TERMINAL_DEMO_SCENE").is_ok_and(|s| s.trim() == "closing");
        #[cfg(test)]
        let wanted = false;
        if !wanted {
            return Vec::new();
        }
        let labels: Vec<(String, String)> = self
            .tabs
            .borrow()
            .iter()
            .filter_map(|t| t.chat.as_ref())
            .take(2)
            .map(|c| {
                (
                    crate::window::sidebar_model::driver_label(c.slot.driver()).to_owned(),
                    c.title.clone(),
                )
            })
            .collect();
        labels
            .into_iter()
            .filter_map(|(agent, title)| {
                stubborn_process()
                    .map(|watch| StopTarget {
                        agent,
                        title,
                        watch,
                    })
                    .map_err(|e| warn!(error = %e, "closing demo: no stand-in agent"))
                    .ok()
            })
            .collect()
    }
}

/// A stand-in agent that ignores SIGTERM (for the demo and the tests). It is told to stop, as a
/// thread's shutdown does (its process dropped: SIGTERM, then the kill after the grace), as soon
/// as it says it is ready: a SIGTERM sent before its shell has set the trap would end it at once.
pub(super) fn stubborn_process() -> Result<ExitWatch, String> {
    let spec = crate::agent_proc::SpawnSpec {
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "trap '' TERM; echo ready; exec sleep 30".into(),
        ],
        ..Default::default()
    };
    let held: Rc<RefCell<Option<crate::agent_proc::AgentProcess>>> = Rc::default();
    let on_ready = held.clone();
    let process = crate::agent_proc::AgentProcess::spawn(
        &spec,
        move |_| drop(on_ready.borrow_mut().take()),
        |_| {},
        |_| {},
    )?;
    let watch = process.exit_watch();
    held.replace(Some(process));
    Ok(watch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use StopPhase::{Exited, Killing, Running, Stopping};

    #[test]
    fn the_step_line_names_the_one_agent_and_thread_it_waits_on() {
        assert_eq!(
            step_line(&[("Codex", "Atomic config writes", Stopping)]),
            "Stopping Codex in Atomic config writes…"
        );
        assert_eq!(step_line(&[("Claude", "  ", Running)]), "Stopping Claude…");
        // Those already gone are not waited on.
        assert_eq!(
            step_line(&[
                ("Claude", "Done", Exited),
                ("Codex", "Atomic config writes", Stopping)
            ]),
            "Stopping Codex in Atomic config writes…"
        );
    }

    #[test]
    fn the_step_line_counts_several_and_says_when_one_is_killed() {
        assert_eq!(
            step_line(&[("Claude", "a", Stopping), ("Codex", "b", Stopping)]),
            "Stopping 2 agents…"
        );
        assert_eq!(
            step_line(&[("Claude", "a", Exited), ("Codex", "b", Killing)]),
            "Codex is not responding, stopping it now…"
        );
        assert_eq!(
            step_line(&[("Claude", "a", Killing), ("Codex", "b", Killing)]),
            "2 agents are not responding, stopping them now…"
        );
        assert_eq!(step_line(&[("Claude", "a", Exited)]), "Closing…");
        assert_eq!(step_line(&[]), "Closing…");
    }

    type Window = super::super::super::AgentTerminalWindow;

    fn new_window(app: &adw::Application, ctx: &glib::MainContext) -> Window {
        let window = Window::new(app);
        window.set_default_size(720, 520);
        window.present();
        crate::testutil::settle(ctx);
        window
    }

    fn is_open(app: &adw::Application, window: &Window) -> bool {
        app.windows()
            .iter()
            .any(|w| w == window.upcast_ref::<gtk4::Window>())
    }

    /// Queues a stand-in agent (ignoring SIGTERM, already told to stop) for the next close.
    fn add_stubborn_agent(window: &Window, agent: &str, title: &str) -> ExitWatch {
        let watch = stubborn_process().expect("a stand-in agent");
        window.imp().test_stops.borrow_mut().push(StopTarget {
            agent: agent.into(),
            title: title.into(),
            watch: watch.clone(),
        });
        watch
    }

    fn step_text(window: &Window) -> String {
        window
            .imp()
            .closing
            .borrow()
            .as_ref()
            .map(|c| c.step.text().to_string())
            .unwrap_or_default()
    }

    /// The close path end to end, on a real window: with nothing to wait for it closes at once;
    /// with an agent still leaving it shows the closing screen, says what it waits on, takes no
    /// more input, escalates to SIGKILL for an agent that ignores SIGTERM, and closes when it
    /// has exited; Quit Now kills at once. (Before: the window always closed at once and the app
    /// blocked in `terminate_all`.)
    #[test]
    #[ignore = "native GTK window; run alone in an isolated display"]
    fn native_closing_waits_for_agents_then_closes() {
        gtk4::init().expect("GTK must be available for the native gate");
        adw::init().expect("libadwaita init");
        crate::icons::register();
        crate::load_css();
        let config_home = tempfile::tempdir().expect("tmp");
        std::env::set_var("XDG_CONFIG_HOME", config_home.path());
        let app = adw::Application::builder()
            .application_id("org.test.Closing")
            .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk4::gio::Cancellable>)
            .expect("registering a non-unique application");
        // Keep the app (and so the windows' lifecycle) independent of the window count.
        let _hold = app.hold();
        let ctx = glib::MainContext::default();

        // Nothing alive: the window goes at once, with no closing screen.
        let window = new_window(&app, &ctx);
        window.close();
        assert!(window.imp().closing.borrow().is_none());
        assert!(!is_open(&app, &window), "closed at once");

        // A stubborn agent: the window stays, waiting, and says on what.
        let window = new_window(&app, &ctx);
        let watch = add_stubborn_agent(&window, "Codex", "Atomic config writes");
        let started = Instant::now();
        window.close();
        assert!(is_open(&app, &window), "waits for the agent");
        assert_eq!(
            step_text(&window),
            "Stopping Codex in Atomic config writes…"
        );
        // The changing line is itself the live region a screen reader follows.
        assert!(window
            .imp()
            .closing
            .borrow()
            .as_ref()
            .is_some_and(|c| c.step.accessible_role() == gtk4::AccessibleRole::Status));
        assert!(
            window
                .lookup_action("new-tab")
                .is_some_and(|a| !a.is_enabled()),
            "no new thread can start while closing"
        );
        // A second close while waiting does not pile up or skip the wait.
        window.close();
        assert!(is_open(&app, &window));
        // Past the flicker threshold, the closing screen is the whole window.
        let shown = crate::testutil::pump_until(&ctx, 2, || {
            window
                .content()
                .is_some_and(|c| c.has_css_class("closing-screen"))
        });
        assert!(
            shown,
            "content: {:?}, closing: {}",
            window.content().map(|c| c.type_().name()),
            window.imp().closing.borrow().is_some()
        );
        let playing = |window: &Window| {
            window
                .imp()
                .closing
                .borrow()
                .as_ref()
                .is_some_and(|c| c.animation.is_playing())
        };
        assert!(playing(&window), "the icon packs itself up while it waits");
        crate::testutil::settle(&ctx);
        crate::testutil::capture_window(window.upcast_ref(), "closing-stopping");
        // It ignores SIGTERM: after the grace it is killed, and the line says so.
        assert!(
            crate::testutil::pump_until(&ctx, 5, || step_text(&window).contains("not responding")
                || !is_open(&app, &window)),
            "never said it was killing the agent"
        );
        assert!(crate::testutil::pump_until(&ctx, 5, || !is_open(
            &app, &window
        )));
        assert_eq!(watch.phase(), StopPhase::Exited);
        let waited = started.elapsed();
        assert!(
            waited < CLOSE_CAP + Duration::from_secs(1),
            "waited {waited:?}"
        );

        // Quit Now: offered after a moment, kills what is left at once. With animations off
        // (reduced motion), the icon stands still and the step line still says what is going on.
        let settings = gtk4::Settings::default().expect("GTK settings");
        settings.set_gtk_enable_animations(false);
        let window = new_window(&app, &ctx);
        let first = add_stubborn_agent(&window, "Claude", "Closing animation");
        let second = add_stubborn_agent(&window, "Codex", "");
        window.close();
        assert_eq!(step_text(&window), "Stopping 2 agents…");
        let revealed = || {
            window
                .imp()
                .closing
                .borrow()
                .as_ref()
                .is_some_and(|c| c.quit_now.reveals_child())
        };
        assert!(!revealed(), "Quit Now waits a moment");
        assert!(crate::testutil::pump_until(&ctx, 3, revealed));
        assert!(!playing(&window), "no motion when animations are off");
        assert_eq!(step_text(&window), "Stopping 2 agents…");
        crate::testutil::settle(&ctx);
        crate::testutil::capture_window(window.upcast_ref(), "closing-quit-now");
        let button = window
            .imp()
            .closing
            .borrow()
            .as_ref()
            .and_then(|c| c.quit_now.child())
            .and_downcast::<gtk4::Button>()
            .expect("the Quit Now button");
        let clicked = Instant::now();
        button.emit_clicked();
        assert!(crate::testutil::pump_until(&ctx, 2, || !is_open(
            &app, &window
        )));
        assert!(crate::testutil::pump_until(&ctx, 2, || {
            first.phase() == StopPhase::Exited && second.phase() == StopPhase::Exited
        }));
        assert!(
            clicked.elapsed() < Duration::from_secs(2),
            "Quit Now waited out the grace"
        );
        // Nothing is left for the app's own quit to wait on.
        assert_eq!(crate::agent_proc::live_agents(), 0);
        let quit = Instant::now();
        crate::agent_proc::terminate_all();
        assert!(quit.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn the_saving_line_counts_what_is_left_and_says_when_it_is_late() {
        assert_eq!(saving_line(0, false), "Saving threads…");
        assert_eq!(saving_line(1, false), "Saving threads… (1 write left)");
        assert_eq!(saving_line(12, false), "Saving threads… (12 writes left)");
        assert!(saving_line(12, true).contains("12 writes still waiting"));
        assert!(saving_line(12, true).contains("Quit Now leaves them unsaved"));
    }

    #[test]
    fn a_long_title_is_cut_short() {
        let line = step_line(&[("Claude", &"word ".repeat(30), Stopping)]);
        assert!(line.starts_with("Stopping Claude in word word"));
        assert!(line.chars().count() < 70, "{line}");
        assert!(line.ends_with("word…") && !line.ends_with("……"), "{line}");
        assert_eq!(short_title("Short"), "Short");
    }
}
