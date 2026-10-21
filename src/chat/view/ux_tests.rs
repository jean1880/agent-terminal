//! Native layout matrix using synthetic transcripts and observable frame conditions.
use super::tests::{status_of, SwitchableBackend};
use super::*;
use agent_core::event::{
    Decision, ItemKind, PlanStep, Question, QuestionOption, ResponseCapability, StepStatus,
    StreamKind,
};

#[test]
#[ignore = "native GTK layout; run alone in an isolated display"]
fn native_ux_matrix() {
    gtk4::init().expect("GTK must be available for the native gate");
    adw::init().expect("libadwaita init");
    crate::icons::register();
    crate::load_css();
    let ctx = glib::MainContext::default();
    for (width, scale, pane_open) in [
        (360, 1.0, false),
        (560, 1.0, true),
        (950, 1.0, true),
        (360, 2.0, false),
        (560, 2.0, false),
        (950, 2.0, true),
    ] {
        let view = ChatView::new(Rc::new(SwitchableBackend {
            status: RefCell::new(status_of(Driver::Codex)),
        }));
        view.set_text_scale(scale);
        let mut history = demo::DemoBackend::stress_history(220);
        for (id, kind, text) in [
            (
                "command",
                ItemKind::Command,
                format!("git show {}", "a".repeat(900)),
            ),
            (
                "search",
                ItemKind::WebSearch,
                format!("https://example.invalid/{}", "b".repeat(900)),
            ),
            (
                "file",
                ItemKind::FileRead,
                format!("src/{}.rs", "c".repeat(900)),
            ),
        ] {
            history.push(
                Envelope::new(Event::ItemStarted {
                    kind,
                    title: text.clone(),
                    input: Some(serde_json::json!({"command": text, "file_path": text})),
                    parent: None,
                })
                .item(id),
            );
        }
        // Text that cannot be broken on its own must wrap or ellipsize, never widen the thread
        // A switch divider with a 900-char model, a question with a 300-char
        // option and chip, a plan with a 300-char step.
        history.extend([
            Envelope::new(Event::ModelChanged {
                model: "first-model".into(),
            }),
            Envelope::new(Event::ModelChanged {
                model: "m".repeat(900),
            }),
            Envelope::new(Event::TurnStarted { model: None }),
            Envelope::new(Event::PlanUpdated {
                steps: vec![
                    PlanStep {
                        text: "p".repeat(300),
                        status: StepStatus::InProgress,
                    },
                    PlanStep {
                        text: "Second step".into(),
                        status: StepStatus::Pending,
                    },
                ],
            }),
            Envelope::new(Event::QuestionRequested {
                source: Default::default(),
                questions: vec![Question {
                    id: "q1".into(),
                    header: "h".repeat(300),
                    question: format!("Pick one {}", "q".repeat(300)),
                    options: vec![
                        QuestionOption {
                            label: "o".repeat(300),
                            description: Some("d".repeat(300)),
                        },
                        QuestionOption {
                            label: "Short".into(),
                            description: None,
                        },
                    ],
                    multi_select: false,
                }],
            })
            .request("wide-question"),
        ]);
        history.push(
            Envelope::new(Event::ItemStarted {
                kind: ItemKind::AssistantMessage,
                title: String::new(),
                input: None,
                parent: None,
            })
            .item("markdown"),
        );
        history.push(
            Envelope::new(Event::ContentSnapshot {
                stream: StreamKind::Assistant,
                text: format!(
                    "A link: https://example.invalid/{}\n\n```rust\n{}\n```",
                    "d".repeat(900),
                    "e".repeat(900)
                ),
            })
            .item("markdown"),
        );
        history.push(
            Envelope::new(Event::ItemStarted {
                kind: ItemKind::UserMessage,
                title: String::new(),
                input: None,
                parent: None,
            })
            .item("user"),
        );
        history.push(
            Envelope::new(Event::ContentSnapshot {
                stream: StreamKind::Assistant,
                text: format!("Visible message {}", "f".repeat(900)),
            })
            .item("user"),
        );
        view.replay(&history);
        let side = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        side.set_size_request(140, -1);
        side.set_visible(pane_open);
        let pane = gtk4::Paned::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .start_child(&view)
            .end_child(&side)
            .shrink_start_child(true)
            .shrink_end_child(true)
            .build();
        pane.set_position(width - 140);
        let window = gtk4::Window::new();
        window.set_default_size(width, 560);
        window.set_child(Some(&pane));
        window.present();
        let inner = view.inner().expect("view");
        let settled = crate::testutil::pump_until(&ctx, 8, || {
            inner.transcript.frames() >= 3
                && inner
                    .transcript
                    .last_row_overflow()
                    .is_some_and(|(over, following)| following && over <= 2.0)
        });
        crate::testutil::capture_window(
            &window,
            &format!(
                "matrix-{width}-scale-{}-pane-{pane_open}",
                (scale * 100.0) as u32
            ),
        );
        assert!(
            settled,
            "following failed at {width}px scale {scale}: {}",
            inner.transcript.scroll_debug()
        );
        assert!(
            window.width() <= width,
            "requested {width}px scale {scale}, window expanded to {}px: {}",
            window.width(),
            crate::testutil::min_size_report(
                view.upcast_ref(),
                gtk4::Orientation::Horizontal,
                width
            )
        );
        for id in ["command", "search", "file", "markdown", "user"] {
            let (bounds, viewport_width) = inner
                .transcript
                .row_bounds_in_viewport(id)
                .expect("mapped row in viewport");
            assert!(
                bounds.x() >= 0.0 && bounds.x() + bounds.width() <= viewport_width as f32 + 1.0,
                "{id} clipped at {width}px scale {scale}, viewport {viewport_width}px: {bounds:?}\n{}",
                crate::testutil::min_size_report(
                    view.upcast_ref(),
                    gtk4::Orientation::Horizontal,
                    width
                )
            );
        }
        // 4.1: the header's status ("Waiting for approval") must not push its last item out of
        // a narrow window; the spinner stays, the words go, and they go before the usage meter.
        inner
            .header
            .set_activity(&model::Activity::WaitingForWorkers {
                names: vec!["a-very-long-worker-name".into(); 3],
            });
        crate::testutil::settle(&ctx);
        let (edge, available) = inner.header.right_edge();
        if width == 360 {
            assert!(
                !inner.header.activity_words_shown(),
                "the compact header keeps the spinner and drops the words (scale {scale})"
            );
        } else if width == 950 && scale == 1.0 {
            assert!(
                !inner.header.activity_words_shown(),
                "below the usage meter's width the words go first, the meter stays"
            );
        }
        assert!(
            edge <= available + 1.0,
            "header clipped at {width}px scale {scale}: last item ends at {edge}, strip is {available}"
        );
        let overflow = crate::testutil::overflowing_bins(view.upcast_ref());
        assert!(
            overflow.is_empty(),
            "overflow at {width}px scale {scale}: {overflow:?}"
        );
        window.destroy();
        while ctx.pending() {
            ctx.iteration(false);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Pressing things
// ---------------------------------------------------------------------------------------------

/// A window of `width` px holding a demo-backed view that has replayed `history`.
fn shown_view(
    width: i32,
    history: &[Envelope],
    ctx: &glib::MainContext,
) -> (gtk4::Window, ChatView) {
    // The usage meter lists agents that are ready.
    for driver in [Driver::Claude, Driver::Codex] {
        crate::availability::AgentAvailability::shared().set(
            driver,
            crate::availability::Availability::Ready("/usr/bin/agent".into()),
        );
    }
    let backend = demo::demo_backend();
    let view = ChatView::new(backend.clone());
    backend.connect(view.sink());
    view.set_account_status(demo::demo_account_status());
    view.set_model_source(Rc::new(demo::DemoModels));
    view.replay(history);
    let window = gtk4::Window::new();
    window.set_default_size(width, 760);
    window.set_child(Some(&view));
    window.present();
    settle(ctx);
    (window, view)
}

/// Lets layout, scrolling and idle work run.
fn settle(ctx: &glib::MainContext) {
    crate::testutil::settle(ctx);
}

/// Where a press at the centre of `w` lands in `window`. A selectable label there would claim
/// the press (a caret), so the press must pass through to `target`: the button, row or toggle
/// that owns it. Returns what was hit when it is not.
fn press_reaches(
    window: &gtk4::Window,
    w: &impl IsA<gtk4::Widget>,
    target: &impl IsA<gtk4::Widget>,
) -> Result<(), String> {
    let w = w.upcast_ref::<gtk4::Widget>();
    let bounds = w
        .compute_bounds(window)
        .ok_or_else(|| format!("{} has no bounds", w.type_().name()))?;
    if bounds.width() < 1.0 || bounds.height() < 1.0 {
        return Err(format!("{} is not laid out", w.type_().name()));
    }
    let (x, y) = (
        f64::from(bounds.x() + bounds.width() / 2.0),
        f64::from(bounds.y() + bounds.height() / 2.0),
    );
    let picked = window
        .pick(x, y, gtk4::PickFlags::DEFAULT)
        .ok_or("nothing under the press")?;
    if picked
        .downcast_ref::<gtk4::Label>()
        .is_some_and(|l| l.is_selectable())
    {
        return Err(format!(
            "a selectable label {:?} claims the press",
            picked.downcast_ref::<gtk4::Label>().map(|l| l.text())
        ));
    }
    let target = target.upcast_ref::<gtk4::Widget>();
    let mut at = Some(picked.clone());
    while let Some(widget) = at {
        if &widget == target {
            return Ok(());
        }
        at = widget.parent();
    }
    Err(format!(
        "the press landed on {} ({}), outside {}",
        picked.type_().name(),
        picked.css_classes().join("."),
        target.type_().name()
    ))
}

/// The first widget under `root` of type `T` with CSS class `class`.
fn find_with_class<T: IsA<gtk4::Widget>>(root: &impl IsA<gtk4::Widget>, class: &str) -> T {
    crate::testutil::descendants(root.upcast_ref())
        .into_iter()
        .filter(|w| w.has_css_class(class) && w.is_mapped())
        .find_map(|w| w.downcast::<T>().ok())
        .unwrap_or_else(|| panic!("no {} with class {class}", std::any::type_name::<T>()))
}

fn ancestor<T: IsA<gtk4::Widget>>(w: &impl IsA<gtk4::Widget>) -> T {
    let mut at = w.parent();
    while let Some(widget) = at {
        if let Ok(found) = widget.clone().downcast::<T>() {
            return found;
        }
        at = widget.parent();
    }
    panic!("no {} above the widget", std::any::type_name::<T>())
}

fn item_started(id: &str, kind: ItemKind, title: &str, input: serde_json::Value) -> Envelope {
    Envelope::new(Event::ItemStarted {
        kind,
        title: title.into(),
        input: Some(input),
        parent: None,
    })
    .item(id)
}

/// Presses the visible text of every click target of the view and asserts its effect.
/// A selectable label inside a button must not swallow the press.
/// GTK has no way to inject a pointer event, so a press is a pick at the text's centre (which
/// must pass through to the button) followed by the button's own activation.
#[test]
#[ignore = "native GTK layout; run alone in an isolated display"]
fn native_click_matrix() {
    gtk4::init().expect("GTK must be available for the native gate");
    adw::init().expect("libadwaita init");
    crate::icons::register();
    crate::load_css();
    let ctx = glib::MainContext::default();

    // A tool card, a thought, a plan and a question, fresh in one thread.
    let tools = vec![
        item_started(
            "cmd",
            ItemKind::Command,
            "Bash",
            serde_json::json!({"command": "cargo test --workspace"}),
        ),
        Envelope::new(Event::ItemCompleted {
            status: agent_core::event::ItemStatus::Completed,
            output: Some("ok".into()),
            error: None,
        })
        .item("cmd"),
        Envelope::new(Event::ItemStarted {
            kind: ItemKind::Reasoning,
            title: String::new(),
            input: None,
            parent: None,
        })
        .item("think"),
        Envelope::new(Event::ContentSnapshot {
            stream: StreamKind::Reasoning,
            text: "Weighing the options.".into(),
        })
        .item("think"),
        Envelope::new(Event::ItemCompleted {
            status: agent_core::event::ItemStatus::Completed,
            output: None,
            error: None,
        })
        .item("think"),
    ];
    // A plan and a running sub-agent: the header's counter and a row on the shelf.
    let mut working = vec![
        Envelope::new(Event::TurnStarted { model: None }),
        Envelope::new(Event::PlanUpdated {
            steps: vec![PlanStep {
                text: "Do the thing".into(),
                status: StepStatus::InProgress,
            }],
        }),
    ];
    working.push(item_started(
        "worker",
        ItemKind::Subagent,
        "Explore",
        serde_json::json!({"description": "Find the writers", "subagent_type": "Explore"}),
    ));
    let asking = vec![Envelope::new(Event::QuestionRequested {
        source: Default::default(),
        questions: vec![Question {
            id: "q1".into(),
            header: "Approach".into(),
            question: "Which one?".into(),
            options: vec![
                QuestionOption {
                    label: "First".into(),
                    description: Some("The quick one".into()),
                },
                QuestionOption {
                    label: "Second".into(),
                    description: None,
                },
            ],
            multi_select: false,
        }],
    })
    .request("q")];
    let (window, view) = shown_view(1100, &tools, &ctx);
    let revealed = |root: &gtk4::Widget| {
        root.last_child()
            .and_downcast::<gtk4::Revealer>()
            .expect("the card's revealer")
            .reveals_child()
    };

    // Tool card: the title text opens the card.
    let title: gtk4::Label = find_with_class(&view, "card-title");
    let header: gtk4::Button = ancestor(&title);
    press_reaches(&window, &title, &header).unwrap_or_else(|e| panic!("tool-card title: {e}"));
    let card = header.parent().expect("card");
    let before = revealed(card.upcast_ref());
    header.emit_clicked();
    settle(&ctx);
    assert_ne!(
        revealed(card.upcast_ref()),
        before,
        "the card did not toggle"
    );

    // "Thought process" opens the thought.
    let thought: gtk4::Label = find_with_class(&view, "reasoning-title");
    let toggle: gtk4::Button = ancestor(&thought);
    press_reaches(&window, &thought, &toggle).unwrap_or_else(|e| panic!("thought: {e}"));
    let root = toggle.parent().expect("reasoning row");
    let before = revealed(root.upcast_ref());
    toggle.emit_clicked();
    settle(&ctx);
    assert_ne!(
        revealed(root.upcast_ref()),
        before,
        "the thought did not toggle"
    );

    window.destroy();

    // The plan card's title.
    let (window, view) = shown_view(1100, &working, &ctx);
    let inner = view.inner().expect("view");
    let plan: gtk4::Label = find_with_class(&inner.plan.revealer, "plan-title");
    let plan_toggle: gtk4::Button = ancestor(&plan);
    press_reaches(&window, &plan, &plan_toggle).unwrap_or_else(|e| panic!("plan title: {e}"));
    let body = plan_toggle
        .parent()
        .and_then(|card| card.last_child())
        .and_downcast::<gtk4::Revealer>()
        .expect("plan body");
    let before = body.reveals_child();
    plan_toggle.emit_clicked();
    assert_ne!(body.reveals_child(), before, "the plan did not toggle");

    // The usage meter and the sub-agents counter open their popovers.
    for class in ["usage-text", "subagent-count"] {
        let text: gtk4::Label = find_with_class(&view, class);
        let menu: gtk4::MenuButton = ancestor(&text);
        press_reaches(&window, &text, &menu).unwrap_or_else(|e| panic!("{class}: {e}"));
        menu.popup();
        settle(&ctx);
        assert!(
            menu.popover().is_some_and(|p| p.is_visible()),
            "{class} did not open its popover"
        );
        menu.popdown();
        settle(&ctx);
    }

    // A shelf row: the sub-agent's name opens it.
    inner.subagent_shelf.set_expanded(true);
    settle(&ctx);
    let name: gtk4::Label = find_with_class(&inner.subagent_shelf.revealer, "subagent-name");
    let row: gtk4::ListBoxRow = ancestor(&name);
    press_reaches(&window, &name, &row).unwrap_or_else(|e| panic!("shelf row: {e}"));
    row.activate();
    settle(&ctx);
    assert!(
        inner.subagent_panel.borrow().is_some(),
        "the shelf row did not open the sub-agent"
    );
    window.destroy();

    // A question option: the option's text chooses it.
    let (window, view) = shown_view(1100, &asking, &ctx);
    let option: gtk4::Label = find_with_class(&view, "option-label");
    let choice: gtk4::ToggleButton = ancestor(&option);
    press_reaches(&window, &option, &choice).unwrap_or_else(|e| panic!("question option: {e}"));
    assert!(!choice.is_active());
    choice.emit_clicked();
    assert!(choice.is_active(), "the option was not chosen");
    window.destroy();

    // Allow and Deny on the shelf answer the approval; the model moves past Pending.
    let approval_state = |inner: &Inner| match &inner
        .model
        .borrow()
        .get("approval:r1")
        .expect("the approval card")
        .body
    {
        model::Body::Approval(a) => a.state,
        other => panic!("not an approval: {other:?}"),
    };
    for pick_deny in [false, true] {
        let history = vec![
            item_started(
                "t1",
                ItemKind::Command,
                "Bash",
                serde_json::json!({"command": "cargo test"}),
            ),
            Envelope::new(Event::ApprovalRequested {
                tool: "Bash".into(),
                title: None,
                input: serde_json::json!({"command": "cargo test"}),
                reason: None,
                options: vec![Decision::Allow, Decision::Deny],
                response: ResponseCapability::Live,
                remembers: None,
            })
            .item("t1")
            .request("r1"),
        ];
        let (window, view) = shown_view(1100, &history, &ctx);
        let inner = view.inner().expect("view");
        assert_eq!(approval_state(&inner), model::ApprovalState::Pending);
        let button = if pick_deny {
            inner.interruption.deny_btn.clone()
        } else {
            inner.interruption.allow_btn.clone()
        };
        press_reaches(&window, &button, &button)
            .unwrap_or_else(|e| panic!("shelf approval button: {e}"));
        button.emit_clicked();
        settle(&ctx);
        let expected = if pick_deny {
            Decision::Deny
        } else {
            Decision::Allow
        };
        assert!(
            matches!(
                approval_state(&inner),
                model::ApprovalState::Sent(d) | model::ApprovalState::Resolved(d) if d == expected
            ),
            "the shelf button did not answer the approval: {:?}",
            approval_state(&inner)
        );
        window.destroy();
    }

    // Deny and Allow on the shelf stay put from one request to the next, whatever else it
    // offers: the menu of session grants, a Cancel in place of Deny, a permission request
    // with no Allow and a wide Review button. A click aimed at one never lands on the other.
    let slots = |tool: &str, options: Vec<Decision>| {
        let input = if tool == super::permission::TOOL {
            serde_json::json!({
                "cwd": "/w", "reason": null, "renderable": true,
                "entries": [{"access": "write", "target": {"kind": "path", "path": "/w/out"}}],
                "network": null, "session_allowed": true, "warning": null, "requested": {}
            })
        } else {
            serde_json::json!({"command": "cargo test"})
        };
        let history = vec![Envelope::new(Event::ApprovalRequested {
            tool: tool.into(),
            title: None,
            input,
            reason: None,
            options,
            response: ResponseCapability::Live,
            remembers: None,
        })
        .request("r1")];
        let (window, view) = shown_view(1100, &history, &ctx);
        let inner = view.inner().expect("view");
        let at = |button: &gtk4::Widget| {
            let slot = button.parent().expect("the button's slot");
            let b = slot.compute_bounds(&window).expect("the slot is laid out");
            (b.x(), b.y(), b.width(), b.height())
        };
        let placed = (
            at(inner.interruption.deny_btn.upcast_ref()),
            at(inner.interruption.allow_btn.upcast_ref()),
        );
        window.destroy();
        placed
    };
    let plain = slots("Bash", vec![Decision::Allow, Decision::Deny]);
    for (case, options, tool) in [
        (
            "with the grants menu",
            vec![
                Decision::Allow,
                Decision::AllowForSession,
                Decision::AllowAlways,
                Decision::Deny,
            ],
            "Bash",
        ),
        (
            "with Cancel for Deny",
            vec![Decision::Allow, Decision::Cancel],
            "Bash",
        ),
        (
            "for a permission request",
            vec![Decision::Allow, Decision::AllowForSession, Decision::Deny],
            super::permission::TOOL,
        ),
    ] {
        assert_eq!(
            slots(tool, options),
            plain,
            "the shelf buttons moved {case}"
        );
    }

    // Send answers sends the chosen answers.
    let (window, view) = shown_view(1100, &asking, &ctx);
    let inner = view.inner().expect("view");
    let option: gtk4::Label = find_with_class(&view, "option-label");
    ancestor::<gtk4::ToggleButton>(&option).emit_clicked();
    let submit = crate::testutil::descendants(view.upcast_ref())
        .into_iter()
        .filter_map(|w| w.downcast::<gtk4::Button>().ok())
        .find(|b| b.label().as_deref() == Some("Send answers"))
        .expect("the Send answers button");
    press_reaches(&window, &submit, &submit).unwrap_or_else(|e| panic!("Send answers: {e}"));
    submit.emit_clicked();
    settle(&ctx);
    assert!(
        matches!(
            &inner.model.borrow().get("question:q").expect("question").body,
            model::Body::Question(c) if c.state != model::QuestionState::Pending
        ),
        "Send answers did not send"
    );
    window.destroy();

    // The composer's send button sends the draft; a code block's Copy button confirms.
    let code = vec![
        Envelope::new(Event::ItemStarted {
            kind: ItemKind::AssistantMessage,
            title: String::new(),
            input: None,
            parent: None,
        })
        .item("answer"),
        Envelope::new(Event::ContentSnapshot {
            stream: StreamKind::Assistant,
            text: "Here:\n\n```rust\nfn main() {}\n```".into(),
        })
        .item("answer"),
    ];
    let (window, view) = shown_view(1100, &code, &ctx);
    let inner = view.inner().expect("view");
    let copy: gtk4::Button = find_with_class(&view, "code-copy");
    press_reaches(&window, &copy, &copy).unwrap_or_else(|e| panic!("code Copy: {e}"));
    copy.emit_clicked();
    assert_eq!(
        copy.icon_name().as_deref(),
        Some("at-object-select-symbolic"),
        "Copy did not confirm"
    );
    inner.composer.set_text("hello from the click test");
    let send: gtk4::Button = find_with_class(&view, "send-button");
    press_reaches(&window, &send, &send).unwrap_or_else(|e| panic!("send: {e}"));
    send.emit_clicked();
    settle(&ctx);
    assert!(
        inner
            .model
            .borrow()
            .user_messages()
            .iter()
            .any(|m| m.contains("hello from the click test")),
        "the draft was not sent"
    );
    assert!(
        inner.composer.text().is_empty(),
        "the draft was not cleared"
    );
    window.destroy();

    // The always-allow dialog's suggestions: the text of one fills the pattern field.
    let entry = gtk4::Entry::new();
    // A read-only command: a runner gets no patterns to choose between.
    let flow = cards::suggestion_flow(&entry, "Bash", "git log --oneline").expect("flow");
    let holder = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    holder.append(&entry);
    holder.append(&flow);
    let dialog = gtk4::Window::new();
    dialog.set_default_size(700, 300);
    dialog.set_child(Some(&holder));
    dialog.present();
    settle(&ctx);
    let labels: Vec<gtk4::Label> = crate::testutil::descendants(flow.upcast_ref())
        .into_iter()
        .filter(|w| w.has_css_class("option-label"))
        .filter_map(|w| w.downcast::<gtk4::Label>().ok())
        .collect();
    assert!(labels.len() >= 3, "{} suggestions", labels.len());
    // One that is not the chosen one already.
    let (last, toggle) = labels
        .iter()
        .map(|l| (l, ancestor::<gtk4::ToggleButton>(l)))
        .find(|(_, t)| !t.is_active())
        .expect("a suggestion that is not chosen");
    press_reaches(&dialog, last, &toggle).unwrap_or_else(|e| panic!("rule suggestion: {e}"));
    let before = entry.text();
    toggle.emit_clicked();
    assert_ne!(
        entry.text(),
        before,
        "the suggestion did not fill the field"
    );
    dialog.destroy();
}

/// A toast covers the transcript, never the shelves or the composer.
#[test]
#[ignore = "native GTK layout; run alone in an isolated display"]
fn native_toasts_clear_the_composer() {
    gtk4::init().expect("GTK must be available for the native gate");
    adw::init().expect("libadwaita init");
    crate::icons::register();
    crate::load_css();
    let ctx = glib::MainContext::default();
    // A plan, a running sub-agent and an open question: the shelves are all up.
    let mut history = vec![
        Envelope::new(Event::TurnStarted { model: None }),
        Envelope::new(Event::PlanUpdated {
            steps: vec![PlanStep {
                text: "Do the thing".into(),
                status: StepStatus::InProgress,
            }],
        }),
    ];
    history.push(item_started(
        "worker",
        ItemKind::Subagent,
        "Explore",
        serde_json::json!({"description": "Find the writers", "subagent_type": "Explore"}),
    ));
    history.push(
        Envelope::new(Event::QuestionRequested {
            source: Default::default(),
            questions: vec![Question {
                id: "q1".into(),
                header: "Approach".into(),
                question: "Which one?".into(),
                options: vec![],
                multi_select: false,
            }],
        })
        .request("q"),
    );
    for width in [360, 900] {
        let (window, view) = shown_view(width, &history, &ctx);
        let overlay = view
            .toast_overlay()
            .expect("the transcript's toast overlay");
        overlay.add_toast(
            adw::Toast::builder()
                .title("Copied the whole thing")
                .build(),
        );
        let on_screen = |overlay: &adw::ToastOverlay| {
            crate::testutil::descendants(overlay.upcast_ref())
                .into_iter()
                .any(|w| w.type_().name() == "AdwToastWidget" && w.is_mapped())
        };
        assert!(
            crate::testutil::pump_until(&ctx, 5, || on_screen(&overlay)),
            "the toast never appeared"
        );
        crate::testutil::settle(&ctx);
        let toast = crate::testutil::descendants(overlay.upcast_ref())
            .into_iter()
            .find(|w| w.type_().name() == "AdwToastWidget" && w.is_mapped())
            .expect("the toast is on screen");
        let toast_bounds = toast.compute_bounds(&window).expect("toast bounds");
        let bottom: gtk4::Box = find_with_class(&view, "chat-bottom");
        let bottom_bounds = bottom.compute_bounds(&window).expect("bottom bounds");
        assert!(
            bottom_bounds.height() > 40.0,
            "shelves and composer are up at {width}px"
        );
        assert!(
            toast_bounds.y() + toast_bounds.height() <= bottom_bounds.y() + 1.0,
            "the toast {toast_bounds:?} covers the shelves or composer {bottom_bounds:?} at {width}px"
        );
        crate::testutil::capture_window(&window, &format!("toast-{width}"));
        window.destroy();
    }
}

/// The queue scene: prompts waiting behind a running
/// turn stay pinned under the growing reply, can be selected for copying, and stay inside the
/// viewport at every width.
#[test]
#[ignore = "native GTK layout; run alone in an isolated display"]
fn native_queue_stays_pinned_and_contained() {
    gtk4::init().expect("GTK must be available for the native gate");
    adw::init().expect("libadwaita init");
    crate::icons::register();
    crate::load_css();
    let ctx = glib::MainContext::default();
    for width in [360, 900] {
        let (window, view) = shown_view(width, &demo::queue_envelopes(), &ctx);
        let inner = view.inner().expect("view");
        {
            let model = inner.model.borrow();
            let order = model.order();
            assert_eq!(
                &order[order.len() - 2..],
                ["queue-1", "queue-2"],
                "the queued prompts are pinned at the tail, after the reply"
            );
        }
        assert!(window.width() <= width, "the queue widened the window");
        for id in ["queue-1", "queue-2"] {
            let (bounds, viewport) = inner
                .transcript
                .row_bounds_in_viewport(id)
                .unwrap_or_else(|| panic!("{id} is not in view at {width}px"));
            assert!(
                bounds.x() >= 0.0 && bounds.x() + bounds.width() <= viewport as f32 + 1.0,
                "{id} clipped at {width}px: {bounds:?} in {viewport}"
            );
            assert!(bounds.height() > 10.0, "{id} has a body");
        }
        // Queued prompts are text a person may want back: selectable, and not in a button.
        let queued: Vec<gtk4::Label> = crate::testutil::descendants(view.upcast_ref())
            .into_iter()
            .filter(|w| w.has_css_class("queued"))
            .flat_map(|row| crate::testutil::descendants(&row))
            .filter_map(|w| w.downcast::<gtk4::Label>().ok())
            .collect();
        assert_eq!(queued.len(), 2, "two queued rows at {width}px");
        assert!(queued.iter().all(|l| l.is_selectable()));
        // A new reply lands above the queue, not below it.
        view.apply(
            &Envelope::new(Event::ItemStarted {
                kind: ItemKind::AssistantMessage,
                title: String::new(),
                input: None,
                parent: None,
            })
            .item("late-reply"),
        );
        let model = inner.model.borrow();
        let order = model.order();
        assert_eq!(
            &order[order.len() - 3..],
            ["late-reply", "queue-1", "queue-2"],
            "a later reply stays above the queue"
        );
        drop(model);
        crate::testutil::capture_window(&window, &format!("queue-{width}"));
        window.destroy();
    }
}

/// A backend that remembers every prompt sent to it.
struct PromptLog {
    status: SessionStatus,
    sent: RefCell<Vec<String>>,
}

impl ChatBackend for PromptLog {
    fn send_prompt(&self, text: &str) {
        self.sent.borrow_mut().push(text.to_owned());
    }
    fn queue_prompt(&self, text: &str) {
        self.sent.borrow_mut().push(text.to_owned());
    }
    fn interrupt(&self) {}
    fn respond_approval(&self, _: &str, _: Decision) {}
    fn answer_questions(&self, _: &str, _: serde_json::Value) {
        panic!("a reply question is answered by a prompt, never over the protocol");
    }
    fn switch(&self, _: Driver, _: Option<String>, _: Option<String>) {}
    fn reload_session(&self) {}
    fn set_mode(&self, _: Mode) {}
    fn control(&self, _: Control) -> String {
        String::new()
    }
    fn status(&self) -> SessionStatus {
        self.status.clone()
    }
}

/// Quick-reply chips under a finished reply: a click puts the exact option in the composer, with
/// the cursor after it and the focus there, and sends nothing; the chips are no interruption;
/// Dismiss and "Don't suggest these" both take them away, the latter telling the window.
#[test]
#[ignore = "native GTK layout; run alone in an isolated display"]
fn native_quick_reply_chips_fill_the_composer_and_send_nothing() {
    gtk4::init().expect("GTK must be available for the native gate");
    adw::init().expect("libadwaita init");
    crate::icons::register();
    crate::load_css();
    let ctx = glib::MainContext::default();
    let backend = Rc::new(PromptLog {
        status: status_of(Driver::Codex),
        sent: RefCell::default(),
    });
    let view = ChatView::new(backend.clone());
    let actions: Rc<RefCell<Vec<String>>> = Rc::default();
    let log = actions.clone();
    view.connect_action(move |a| log.borrow_mut().push(format!("{a:?}")));
    let window = gtk4::Window::new();
    window.set_default_size(620, 560);
    window.set_child(Some(&view));
    window.present();
    crate::testutil::settle(&ctx);
    let inner = view.inner().expect("built view");

    let offer = |request: &str| {
        for event in [
            Event::TurnStarted { model: None },
            Event::ItemStarted {
                kind: ItemKind::AssistantMessage,
                title: String::new(),
                input: None,
                parent: None,
            },
        ] {
            view.apply(&Envelope::new(event).item("a1"));
        }
        view.apply(
            &Envelope::new(Event::ContentSnapshot {
                stream: StreamKind::Assistant,
                text:
                    "Two layouts fit.\n\nWhich do you prefer?\n\n- Tabs on top\n- Tabs on the side"
                        .into(),
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
                questions: vec![Question {
                    id: "reply".into(),
                    header: "Quick replies".into(),
                    question: "Which do you prefer?".into(),
                    options: ["Tabs on top", "Tabs on the side"]
                        .iter()
                        .map(|o| QuestionOption {
                            label: (*o).into(),
                            description: None,
                        })
                        .collect(),
                    multi_select: false,
                }],
            })
            .request(request),
        );
        crate::testutil::settle(&ctx);
    };
    let chips = || -> Vec<gtk4::Button> {
        crate::testutil::descendants(view.upcast_ref())
            .into_iter()
            .filter(|w| w.has_css_class("quick-reply") && w.is_mapped())
            .filter_map(|w| w.downcast::<gtk4::Button>().ok())
            .collect()
    };
    let action = |title: &str| -> gtk4::Button {
        crate::testutil::descendants(view.upcast_ref())
            .into_iter()
            .filter(|w| w.has_css_class("quick-reply-action") && w.is_mapped())
            .filter_map(|w| w.downcast::<gtk4::Button>().ok())
            .find(|b| b.label().as_deref() == Some(title))
            .unwrap_or_else(|| panic!("no {title} button"))
    };

    offer("reply:a1");
    assert_eq!(chips().len(), 2, "one chip per option");
    assert!(
        !inner.interruption.revealer.reveals_child(),
        "a reply question never takes the shelf"
    );
    assert!(
        !matches!(
            inner.model.borrow().activity(false),
            model::Activity::NeedsAnswer
        ),
        "a reply question is not a needed answer"
    );
    crate::testutil::capture_window(&window, "quick-replies");

    chips()
        .into_iter()
        .find(|c| c.tooltip_text().as_deref() == Some("Tabs on the side"))
        .expect("the second option's chip")
        .emit_clicked();
    crate::testutil::settle(&ctx);
    assert_eq!(inner.composer.text(), "Tabs on the side");
    assert!(
        inner.composer.cursor_at_end(),
        "the cursor follows the text"
    );
    assert!(backend.sent.borrow().is_empty(), "a click sends nothing");
    assert_eq!(
        chips().len(),
        2,
        "the chips stay until a prompt, a turn or Dismiss"
    );

    action("Dismiss").emit_clicked();
    crate::testutil::settle(&ctx);
    assert!(chips().is_empty());
    assert!(backend.sent.borrow().is_empty());
    assert!(actions.borrow().is_empty(), "Dismiss changes no setting");

    offer("reply:a2");
    action("Don\u{2019}t suggest these").emit_clicked();
    crate::testutil::settle(&ctx);
    assert!(chips().is_empty());
    assert_eq!(&*actions.borrow(), &["StopQuickReplies".to_owned()]);

    // A new turn takes chips away without a click.
    offer("reply:a3");
    view.apply(&Envelope::new(Event::TurnStarted { model: None }));
    crate::testutil::settle(&ctx);
    assert!(chips().is_empty());
    window.destroy();
}
