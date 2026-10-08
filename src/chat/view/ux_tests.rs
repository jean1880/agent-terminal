//! Native layout matrix using synthetic transcripts and observable frame conditions.
use super::tests::{status_of, SwitchableBackend};
use super::*;
use agent_core::event::{ItemKind, StreamKind};

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
                "{id} clipped at {width}px scale {scale}, viewport {viewport_width}px: {bounds:?}"
            );
        }
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
