//! Native tests for the cross-agent gaps the app polyfills: `@` files for every agent and the
//! context gauge. They build real views, so they run under a display
//! (`tests/qa/native-tests.json`).

use super::tests::status_of;
use super::*;
use agent_core::event::{Decision, ItemKind};
use std::time::{Duration, Instant};

/// A backend that records the controls it is asked for and hands out distinct request ids.
struct RecordingBackend {
    status: SessionStatus,
    controls: RefCell<Vec<(String, Control)>>,
}

impl RecordingBackend {
    fn new(driver: Driver) -> Rc<Self> {
        Rc::new(Self {
            status: status_of(driver),
            controls: RefCell::new(Vec::new()),
        })
    }

    fn asked(&self, wanted: impl Fn(&Control) -> bool) -> Option<String> {
        self.controls
            .borrow()
            .iter()
            .find(|(_, c)| wanted(c))
            .map(|(id, _)| id.clone())
    }
}

impl ChatBackend for RecordingBackend {
    fn send_prompt(&self, _: &str) {}
    fn interrupt(&self) {}
    fn respond_approval(&self, _: &str, _: Decision) {}
    fn answer_questions(&self, _: &str, _: serde_json::Value) {}
    fn switch(&self, _: Driver, _: Option<String>, _: Option<String>) {}
    fn reload_session(&self) {}
    fn set_mode(&self, _: Mode) {}
    fn control(&self, control: Control) -> String {
        let id = format!("ctl-{}", self.controls.borrow().len() + 1);
        self.controls.borrow_mut().push((id.clone(), control));
        id
    }
    fn status(&self) -> SessionStatus {
        self.status.clone()
    }
}

fn init() -> glib::MainContext {
    gtk4::init().expect("GTK must be available for the native gate");
    adw::init().expect("libadwaita init");
    crate::icons::register();
    crate::load_css();
    glib::MainContext::default()
}

/// Iterates the main loop until `cond` holds; false on timeout.
fn pump_until(ctx: &glib::MainContext, cond: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        if Instant::now() > deadline {
            return false;
        }
        if !ctx.iteration(false) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    true
}

#[test]
#[ignore = "native GTK; run alone in an isolated display"]
fn native_at_popover_lists_files_for_all_three_drivers() {
    let ctx = init();
    let tmp = tempfile::tempdir().expect("tmp");
    std::fs::create_dir_all(tmp.path().join("src")).expect("mkdir");
    for file in ["src/alpha.rs", "beta.md"] {
        std::fs::write(tmp.path().join(file), "x").expect("write");
    }
    for driver in Driver::ALL {
        let backend = RecordingBackend::new(driver);
        let view = ChatView::new(backend.clone());
        // The popup needs a toplevel to open on.
        let window = gtk4::Window::new();
        window.set_default_size(560, 700);
        window.set_child(Some(&view));
        window.present();
        assert!(pump_until(&ctx, || view.width() > 0), "{driver:?}: shown");
        view.set_workspace(tmp.path());
        let inner = view.inner().expect("built view");
        inner.composer.set_text("look at @alp");

        if backend.status.capabilities.file_suggestions {
            // The agent answers (Claude): hand the view its reply.
            assert!(
                pump_until(&ctx, || backend
                    .asked(|c| matches!(c, Control::FileSuggestions { .. }))
                    .is_some()),
                "{driver:?}: no file request reached the agent"
            );
            let id = backend
                .asked(|c| matches!(c, Control::FileSuggestions { .. }))
                .expect("request");
            view.apply(
                &Envelope::new(Event::ControlResult {
                    ok: Some(serde_json::json!({"suggestions": ["src/alpha.rs"]})),
                    error: None,
                })
                .request(&id),
            );
        } else {
            assert!(
                backend
                    .asked(|c| matches!(c, Control::FileSuggestions { .. }))
                    .is_none(),
                "{driver:?}: the app's index answers, not the agent"
            );
        }
        assert!(
            pump_until(&ctx, || inner
                .composer
                .file_entries()
                .contains(&"src/alpha.rs".to_owned())),
            "{driver:?}: the @ popover does not list src/alpha.rs ({:?})",
            inner.composer.file_entries()
        );
        assert!(!inner
            .composer
            .file_entries()
            .contains(&"beta.md".to_owned()));
        assert!(
            inner.composer.placeholder_text().contains("@ files"),
            "{driver:?}: the hint names @"
        );
    }
}

#[test]
#[ignore = "native GTK; run alone in an isolated display"]
fn native_the_context_gauge_is_polled_after_a_turn_for_agents_that_do_not_push_it() {
    let ctx = init();
    for driver in Driver::ALL {
        let backend = RecordingBackend::new(driver);
        let view = ChatView::new(backend.clone());
        view.apply(&Envelope::new(Event::TurnStarted { model: None }));
        view.apply(&Envelope::new(Event::ItemStarted {
            kind: ItemKind::AssistantMessage,
            title: String::new(),
            input: None,
            parent: None,
        }));
        view.apply(&Envelope::new(Event::TurnCompleted {
            state: agent_core::event::TurnState::Completed,
            usage: None,
            cost_usd: None,
            error: None,
        }));
        let asked = || {
            backend
                .asked(|c| matches!(c, Control::ContextUsage))
                .is_some()
        };
        if driver == Driver::Codex {
            // Codex pushes `thread/tokenUsage/updated` itself.
            for _ in 0..50 {
                ctx.iteration(false);
            }
            assert!(!asked(), "Codex is not polled");
        } else {
            assert!(
                pump_until(&ctx, asked),
                "{driver:?}: the gauge is never requested"
            );
        }
    }
}
