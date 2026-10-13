//! Native tests for the cross-agent gaps the app polyfills: `@` files for every agent and the
//! context gauge. They build real views, so they are `#[ignore]`d and run under a display by
//! `make test-display` (part of `make check`).

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

/// The strip lists every background task; a shell task opens its output, read from the agent
/// once a second (escapes stripped) until it ends, then once more, and never again after that.
/// A sub-agent task whose item is not in the thread has nothing to open.
#[test]
#[ignore = "native GTK; run alone in an isolated display"]
fn native_background_tasks_are_listed_and_a_shell_task_shows_its_output() {
    use agent_core::event::{BackgroundTask, BackgroundTaskKind};
    let ctx = init();
    let backend = RecordingBackend::new(Driver::Claude);
    let view = ChatView::new(backend.clone());
    let window = gtk4::Window::new();
    window.set_default_size(560, 700);
    window.set_child(Some(&view));
    window.present();
    assert!(pump_until(&ctx, || view.width() > 0), "shown");
    let inner = view.inner().expect("built view");
    let button = inner.background_button.widget().clone();
    assert!(!button.is_visible(), "nothing in the background yet");

    let tasks = |tasks: Vec<BackgroundTask>| {
        view.apply(&Envelope::new(Event::BackgroundTasks { tasks }));
    };
    let shell = BackgroundTask {
        id: "b1".to_owned(),
        kind: BackgroundTaskKind::Shell,
        description: Some("cargo test".to_owned()),
        tool_use_id: Some("toolu_1".to_owned()),
    };
    tasks(vec![
        shell.clone(),
        BackgroundTask {
            id: "a1".to_owned(),
            kind: BackgroundTaskKind::Agent,
            description: Some("Review".to_owned()),
            tool_use_id: Some("toolu_gone".to_owned()),
        },
    ]);
    assert!(button.is_visible());
    assert_eq!(
        inner.background_button.listed(),
        [
            ("cargo test".to_owned(), true),
            ("Review".to_owned(), false)
        ]
    );

    let reads = || -> Vec<String> {
        backend
            .controls
            .borrow()
            .iter()
            .filter(|(_, c)| matches!(c, Control::TaskOutput { task_id } if task_id == "b1"))
            .map(|(id, _)| id.clone())
            .collect()
    };
    let reply = |id: &str, output: &str| {
        view.apply(
            &Envelope::new(Event::ControlResult {
                ok: Some(serde_json::json!({
                    "output": output, "total_bytes": output.len(), "truncated": false
                })),
                error: None,
            })
            .request(id),
        );
    };
    inner.open_background(&shell);
    assert_eq!(reads().len(), 1, "read at once");
    let panel = inner
        .task_panel
        .borrow()
        .clone()
        .expect("the output dialog");
    reply(&reads()[0], "\u{1b}[32mok\u{1b}[0m\n");
    assert_eq!(
        panel.shown(),
        (
            "ok\n".to_owned(),
            "Running · updates every second".to_owned()
        )
    );
    assert!(pump_until(&ctx, || reads().len() == 2), "polled again");
    reply(&reads()[1], "ok\nmore\n");
    assert_eq!(panel.shown().0, "ok\nmore\n");

    // The task ends: one last read, whose empty answer (the file is gone) keeps what was shown.
    tasks(Vec::new());
    assert!(!button.is_visible());
    assert!(pump_until(&ctx, || reads().len() == 3), "the last read");
    reply(&reads()[2], "");
    assert_eq!(
        panel.shown(),
        ("ok\nmore\n".to_owned(), "Finished".to_owned())
    );
    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline {
        if !ctx.iteration(false) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    assert_eq!(reads().len(), 3, "no read after the last");
}

/// The three request cards (a permission grant, a form, a link) in a real narrow window at a
/// normal and a large text size: they wrap rather than widen the thread, none takes keyboard
/// focus from the composer when it appears, and Enter in the last field sends a valid answer.
#[test]
#[ignore = "native GTK; run alone in an isolated display"]
fn native_request_cards_wrap_and_never_take_focus() {
    use agent_core::form::{FieldInput, Form, FormAction, FormField};
    use serde_json::json;

    let ctx = init();
    let warnings = crate::testutil::GtkWarningCapture::start();
    // A new card is measured before it has a width. Flow boxes of buttons or chips that are
    // aligned (rather than left to fill) then have their children measured at 0 px. Checked
    // once everything in a window has settled, and again at the end.
    let no_flow_warnings = |what: &str| {
        let warned = warnings.take();
        assert!(
            warned.iter().all(|w| !w.contains("GtkFlowBoxChild")),
            "{what}: flow box children measured without a width:\n{}",
            warned.join("\n")
        );
    };
    for (width, scale) in [(360, 1.0), (560, 2.0), (560, 1.0)] {
        let backend = super::tests::AnswerLog::new(Driver::Codex);
        let view = ChatView::new(backend.clone());
        view.set_text_scale(scale);
        // As the layout matrix hosts a view: in a pane that may shrink it, so the assertions are
        // about the rows not being clipped, not about the header's own minimum.
        let side = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        side.set_visible(false);
        let pane = gtk4::Paned::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .start_child(&view)
            .end_child(&side)
            .shrink_start_child(true)
            .shrink_end_child(true)
            .build();
        pane.set_position(width);
        let window = gtk4::Window::new();
        window.set_default_size(width, 700);
        window.set_child(Some(&pane));
        window.present();
        assert!(pump_until(&ctx, || view.width() > 0), "{width}: shown");
        view.focus_composer();
        let inner = view.inner().expect("built view");
        let focused_before = GtkWindowExt::focus(&window);

        view.apply(
            &Envelope::new(Event::ApprovalRequested {
                tool: "codex_permissions".into(),
                title: Some("Codex asks to write to 2 places and use the network".into()),
                input: json!({
                    "cwd": "/w", "reason": "The build keeps its cache outside the project",
                    "renderable": true,
                    "entries": [
                        {"access": "write", "target": {"kind": "path",
                            "path": format!("/home/someone/{}", "long-folder-name/".repeat(8))}},
                        {"access": "write", "target": {"kind": "special", "name": "slash_tmp", "path": null}},
                        {"access": "deny", "target": {"kind": "path", "path": "/w/.git"}}
                    ],
                    "network": true, "session_allowed": true, "warning": null, "requested": {}
                }),
                reason: None,
                options: vec![
                    Decision::Allow,
                    Decision::AllowForSession,
                    Decision::Deny,
                    Decision::Cancel,
                ],
                response: agent_core::event::ResponseCapability::Live,
                remembers: None,
            })
            .request("p"),
        );
        let field = |name: &str, input: FieldInput| FormField {
            name: name.into(),
            title: None,
            description: Some("A long description that has to wrap at a narrow width.".into()),
            required: true,
            input,
        };
        view.apply(
            &Envelope::new(Event::FormRequested {
                server: "a-server-with-a-long-name".into(),
                message: "Please tell me which repository to use for this work.".into(),
                form: Form::Fields {
                    fields: vec![
                        field(
                            "flavour",
                            FieldInput::Choice {
                                options: (0..4)
                                    .map(|i| agent_core::form::ChoiceOption {
                                        value: format!("v{i}"),
                                        label: format!("A fairly long option label {i}"),
                                    })
                                    .collect(),
                                multi: false,
                                min_items: None,
                                max_items: None,
                                default: Vec::new(),
                            },
                        ),
                        field(
                            "repo_name",
                            FieldInput::Text {
                                format: None,
                                min_length: None,
                                max_length: None,
                                default: None,
                            },
                        ),
                    ],
                },
            })
            .request("f"),
        );
        view.apply(
            &Envelope::new(Event::FormRequested {
                server: "github".into(),
                message: "Sign in to continue.".into(),
                form: Form::Link {
                    url: format!("https://github.com/login/device?code={}", "A".repeat(300)),
                },
            })
            .request("l"),
        );
        // The other chip rows: the agent's own question with choices, and quick replies under
        // a finished reply.
        let option = |label: &str| agent_core::event::QuestionOption {
            label: label.into(),
            description: None,
        };
        view.apply(
            &Envelope::new(Event::QuestionRequested {
                source: Default::default(),
                questions: vec![agent_core::event::Question {
                    id: "q".into(),
                    header: "Layout".into(),
                    question: "Which layout?".into(),
                    options: vec![option("Tabs on top"), option("Tabs on the side")],
                    multi_select: false,
                }],
            })
            .request("q"),
        );
        for event in [
            Event::TurnStarted { model: None },
            Event::ItemStarted {
                kind: agent_core::event::ItemKind::AssistantMessage,
                title: String::new(),
                input: None,
                parent: None,
            },
        ] {
            view.apply(&Envelope::new(event).item("a1"));
        }
        view.apply(
            &Envelope::new(Event::ContentSnapshot {
                stream: agent_core::event::StreamKind::Assistant,
                text: "Which do you prefer?".into(),
            })
            .item("a1"),
        );
        view.apply(&Envelope::new(Event::TurnCompleted {
            state: agent_core::event::TurnState::Completed,
            usage: None,
            cost_usd: None,
            error: None,
        }));
        view.apply(
            &Envelope::new(Event::QuestionRequested {
                source: agent_core::event::QuestionSource::Reply,
                questions: vec![agent_core::event::Question {
                    id: "reply".into(),
                    header: "Quick replies".into(),
                    question: "Which do you prefer?".into(),
                    options: vec![option("Tabs on top"), option("Tabs on the side")],
                    multi_select: false,
                }],
            })
            .request("r"),
        );
        assert!(
            pump_until(&ctx, || inner.transcript.frames() >= 3),
            "{width}: drawn"
        );
        crate::testutil::capture_window(
            &window,
            &format!("requests-{width}-scale-{}", (scale * 100.0) as u32),
        );
        assert!(
            window.width() <= width,
            "{width}px scale {scale}: the window grew to {}px: {}",
            window.width(),
            crate::testutil::min_size_report(
                view.upcast_ref(),
                gtk4::Orientation::Horizontal,
                width - 100
            )
        );
        for id in ["approval:p", "form:f", "form:l"] {
            inner.transcript.scroll_to_card(id);
            assert!(
                pump_until(&ctx, || inner
                    .transcript
                    .row_bounds_in_viewport(id)
                    .is_some()),
                "{width}: {id} is on screen"
            );
            let (bounds, viewport) = inner
                .transcript
                .row_bounds_in_viewport(id)
                .expect("mapped row");
            assert!(
                bounds.x() >= 0.0 && bounds.x() + bounds.width() <= viewport as f32 + 1.0,
                "{width}px scale {scale}: {id} is clipped: {bounds:?} in {viewport}px"
            );
        }
        // Appearing took no focus from where the user was typing.
        assert_eq!(GtkWindowExt::focus(&window), focused_before);

        // Enter in the last field of a valid form answers it.
        let entry = inner
            .transcript
            .with_row("form:f", |row| match row {
                super::cards::Row::Form(card) => card.entry(1),
                _ => None,
            })
            .flatten()
            .expect("the form's last field");
        entry.emit_activate();
        assert!(
            backend.forms.borrow().is_empty(),
            "an empty answer is not sent"
        );
        let kind = inner
            .transcript
            .with_row("form:f", |row| match row {
                super::cards::Row::Form(card) => card.marks(0).into_iter().next(),
                _ => None,
            })
            .flatten()
            .and_then(|w| w.downcast::<gtk4::ToggleButton>().ok())
            .expect("first option");
        kind.set_active(true);
        entry.set_text("octo");
        entry.emit_activate();
        assert_eq!(
            *backend.forms.borrow(),
            [(
                "f".to_owned(),
                FormAction::Accept,
                Some(json!({"flavour": "v0", "repo_name": "octo"}))
            )]
        );
        // Let what the interactions set off settle, then close the window with its view.
        crate::testutil::settle(&ctx);
        window.destroy();
        crate::testutil::settle(&ctx);
        no_flow_warnings(&format!("{width}px scale {scale}"));
    }
    no_flow_warnings("after every window");
}
