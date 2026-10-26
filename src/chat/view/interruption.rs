//! The sticky interruption shelf docked above the composer.
//!
//! Reveals automatically when an approval or question is pending, even if the user
//! has scrolled up or subsequent streaming output pushed the card out of view.

use std::cell::RefCell;
use std::rc::Rc;

use agent_core::event::{Decision, ItemKind};
use gtk4::prelude::*;

use super::cards::{decision_label, label, RowEvent, RowSink};
use super::model::{PendingInterruption, Transcript};
use super::payload;
use super::permission;

pub struct InterruptionShelf {
    pub revealer: gtk4::Revealer,
    card: gtk4::Box,
    icon: gtk4::Image,
    title: gtk4::Label,
    preview: gtk4::Label,
    pub(crate) allow_btn: gtk4::Button,
    pub(crate) deny_btn: gtk4::Button,
    pub(crate) more_btn: gtk4::MenuButton,
    pub(crate) more_box: gtk4::Box,
    pub(crate) more_popover: gtk4::Popover,
    pub(crate) jump_btn: gtk4::Button,
    active: Rc<RefCell<Option<PendingInterruption>>>,
    /// "1 of 3": the shelf shows one of several things waiting, and says so.
    count: gtk4::Label,
    shown_count: std::cell::Cell<usize>,
    source: RefCell<Option<String>>,
    sink: RowSink,
}

impl InterruptionShelf {
    pub fn new(sink: RowSink, jump: impl Fn(String) + 'static) -> Self {
        let card = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        card.add_css_class("interruption-shelf");

        let icon = gtk4::Image::from_icon_name("at-security-medium-symbolic");
        icon.add_css_class("interruption-icon");
        icon.set_valign(gtk4::Align::Center);
        card.append(&icon);

        let info = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        info.set_hexpand(true);
        info.set_valign(gtk4::Align::Center);

        let title = label("", &["interruption-title"]);
        title.set_ellipsize(gtk4::pango::EllipsizeMode::End);

        // Always shown, even empty: a line that came and went would change the shelf's height
        // and move its buttons up and down between requests.
        let preview = label("", &["interruption-preview"]);
        preview.set_ellipsize(gtk4::pango::EllipsizeMode::End);

        info.append(&title);
        info.append(&preview);
        card.append(&info);

        let actions = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        actions.set_valign(gtk4::Align::Center);

        // Deny and Cancel share one width, so the label switching never moves Allow.
        let deny_text = gtk4::Stack::new();
        deny_text.set_hhomogeneous(true);
        deny_text.add_named(&gtk4::Label::new(Some("Deny")), Some("Deny"));
        deny_text.add_named(&gtk4::Label::new(Some("Cancel")), Some("Cancel"));
        let deny_btn = gtk4::Button::new();
        deny_btn.set_child(Some(&deny_text));
        deny_btn.update_property(&[gtk4::accessible::Property::Label("Deny")]);
        deny_btn.add_css_class("pill");
        deny_btn.add_css_class("destructive-action");

        let allow_btn = gtk4::Button::with_label("Allow");
        allow_btn.add_css_class("pill");
        allow_btn.add_css_class("suggested-action");

        let more_box = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        more_box.set_margin_top(6);
        more_box.set_margin_bottom(6);
        more_box.set_margin_start(6);
        more_box.set_margin_end(6);

        let more_popover = gtk4::Popover::new();
        more_popover.set_child(Some(&more_box));

        let more_btn = gtk4::MenuButton::new();
        more_btn.set_icon_name("at-pan-down-symbolic");
        more_btn.set_popover(Some(&more_popover));
        more_btn.add_css_class("flat");
        more_btn.set_tooltip_text(Some("More permission options"));
        more_btn.update_property(&[gtk4::accessible::Property::Label("More permission options")]);

        let jump_btn = gtk4::Button::from_icon_name("at-go-bottom-symbolic");
        jump_btn.add_css_class("flat");
        jump_btn.set_tooltip_text(Some("Jump to card in transcript"));
        jump_btn.update_property(&[gtk4::accessible::Property::Label(
            "Jump to card in transcript",
        )]);

        let count = label("", &["interruption-count", "dim-label"]);
        count.set_valign(gtk4::Align::Center);
        count.set_visible(false);
        // Deny, Allow and the menu sit last, at the shelf's right edge, each in a slot that
        // keeps its space when its button is not offered: they never move between requests,
        // so a click aimed at one never lands on another. Anything that changes width (the
        // count, the jump button turning into a labelled pill) goes before them.
        actions.append(&count);
        actions.append(&jump_btn);
        actions.append(&slot(&deny_btn));
        actions.append(&slot(&allow_btn));
        actions.append(&slot(&more_btn));
        card.append(&actions);
        place(&more_btn, false);

        let revealer = gtk4::Revealer::new();
        revealer.set_transition_type(gtk4::RevealerTransitionType::SlideDown);
        revealer.set_child(Some(&card));
        revealer.set_reveal_child(false);

        let active: Rc<RefCell<Option<PendingInterruption>>> = Rc::new(RefCell::new(None));

        {
            let active = active.clone();
            let sink = sink.clone();
            allow_btn.connect_clicked(move |_| {
                answer_active(&active, &sink, Decision::Allow);
            });
        }

        {
            let active = active.clone();
            let sink = sink.clone();
            deny_btn.connect_clicked(move |_| {
                answer_active(&active, &sink, Decision::Deny);
            });
        }

        {
            let active = active.clone();
            let jump = Rc::new(jump);
            jump_btn.connect_clicked(move |_| {
                // Copied out before jumping: scrolling to the card can update this shelf.
                let item_id = active
                    .borrow()
                    .as_ref()
                    .map(|interruption| match interruption {
                        PendingInterruption::Approval { item_id, .. }
                        | PendingInterruption::Question { item_id, .. }
                        | PendingInterruption::Form { item_id, .. } => item_id.clone(),
                    });
                if let Some(item_id) = item_id {
                    jump(item_id);
                }
            });
        }

        let shelf = Self {
            revealer,
            card,
            icon,
            title,
            preview,
            allow_btn,
            deny_btn,
            more_btn,
            more_box,
            more_popover,
            jump_btn,
            active,
            count,
            shown_count: std::cell::Cell::new(0),
            source: RefCell::new(None),
            sink,
        };
        // A hidden shelf still counts towards the window's minimum width: the slots take their
        // room only while an approval is shown, not with nothing waiting.
        shelf.show_slots(false);
        shelf
    }

    /// Answers the approval on show with `decision`, as its buttons would: a permission request
    /// (a list of places) is never allowed from here, only denied or stopped, since the shelf
    /// cuts its list. Deny falls back to Cancel where the approval offers no Deny, as the Deny
    /// button does. False when no approval is pending or it does not offer `decision`: the
    /// keyboard shortcut then lets its key through.
    pub fn answer(&self, decision: Decision) -> bool {
        answer_active(&self.active, &self.sink, decision)
    }

    pub fn update(&self, transcript: &Transcript) {
        self.update_pending(
            transcript.pending_interruption(),
            transcript.pending_count(),
            None,
        );
    }

    #[cfg(test)]
    pub(super) fn shown(&self) -> (String, usize) {
        (self.title.text().to_string(), self.shown_count.get())
    }

    pub(super) fn update_pending(
        &self,
        interruption: Option<PendingInterruption>,
        waiting: usize,
        source: Option<&str>,
    ) {
        if self.shown_count.replace(waiting) != waiting {
            self.count.set_visible(waiting > 1);
            self.count.set_text(&format!("1 of {waiting}"));
            self.count.set_tooltip_text(Some(
                "Requests waiting for you in this thread and its workers",
            ));
        }
        let source_changed = self.source.borrow().as_deref() != source;
        if interruption == *self.active.borrow() && !source_changed {
            return;
        }
        *self.source.borrow_mut() = source.map(str::to_owned);
        *self.active.borrow_mut() = interruption.clone();

        match interruption {
            Some(PendingInterruption::Approval {
                tool,
                title,
                input,
                options,
                ..
            }) => {
                self.card.remove_css_class("is-question");
                self.icon.set_icon_name(Some("at-security-medium-symbolic"));
                let heading = title.unwrap_or_else(|| format!("Approval required: {tool}"));
                self.title.set_text(&heading);

                let prev = approval_preview(&tool, &input);
                self.preview.set_text(&prev);

                // A permission grant is a list of places: the shelf cuts lists, so it never
                // offers a one-click Allow for one. It points at the card instead.
                let permission = tool == permission::TOOL;
                let has_allow = options.contains(&Decision::Allow) && !permission;
                let has_deny =
                    options.contains(&Decision::Deny) || options.contains(&Decision::Cancel);
                self.show_slots(true);
                place(&self.allow_btn, has_allow);
                place(&self.deny_btn, has_deny);
                let deny = if options.contains(&Decision::Deny) {
                    "Deny"
                } else {
                    "Cancel"
                };
                if let Some(text) = self.deny_btn.child().and_downcast::<gtk4::Stack>() {
                    text.set_visible_child_name(deny);
                }
                self.deny_btn
                    .update_property(&[gtk4::accessible::Property::Label(deny)]);

                // Clear previous extra options in the dropdown popover
                while let Some(child) = self.more_box.first_child() {
                    self.more_box.remove(&child);
                }

                let extra_decisions: Vec<Decision> =
                    [Decision::AllowForSession, Decision::AllowAlways]
                        .into_iter()
                        .filter(|d| options.contains(d) && !permission)
                        .collect();

                for d in &extra_decisions {
                    let b = gtk4::Button::with_label(decision_label(*d));
                    b.add_css_class("pill");
                    let sink = self.sink.clone();
                    let active = self.active.clone();
                    let popover = self.more_popover.clone();
                    let decision = *d;
                    b.connect_clicked(move |_| {
                        popover.popdown();
                        answer_active(&active, &sink, decision);
                    });
                    self.more_box.append(&b);
                }
                place(&self.more_btn, !extra_decisions.is_empty());

                if permission {
                    self.jump_as_action(
                        "Review in transcript ↓",
                        "Read the whole request in the transcript",
                        "Review the permission request",
                    );
                } else {
                    self.jump_as_icon();
                }

                self.revealer.set_reveal_child(true);
            }
            Some(PendingInterruption::Form {
                server,
                message,
                link,
                ..
            }) => {
                place(&self.more_btn, false);
                self.card.add_css_class("is-question");
                self.icon.set_icon_name(Some("at-dialog-question-symbolic"));
                self.title.set_text(if link {
                    "A tool server wants you to open a link"
                } else {
                    "A tool server wants information"
                });
                // Whose words they are comes first: the server's, not the app's or the agent's.
                let first = message.lines().map(str::trim).find(|l| !l.is_empty());
                let preview = match first {
                    Some(line) => format!("{server}: {line}"),
                    None => server,
                };
                self.preview.set_text(&preview);
                self.show_slots(false);
                self.jump_as_action(
                    if link {
                        "Review in transcript ↓"
                    } else {
                        "Answer in transcript ↓"
                    },
                    "Jump to the form in the transcript",
                    if link {
                        "Review the link in the transcript"
                    } else {
                        "Answer in the transcript"
                    },
                );
                self.revealer.set_reveal_child(true);
            }
            Some(PendingInterruption::Question { questions, .. }) => {
                place(&self.more_btn, false);
                self.card.add_css_class("is-question");
                self.icon.set_icon_name(Some("at-dialog-question-symbolic"));
                let count = questions.len();
                let heading = if count > 1 {
                    format!("Agent question ({count} items)")
                } else {
                    "Agent question".to_owned()
                };
                self.title.set_text(&heading);

                let first_text = questions
                    .first()
                    .map(|q| q.question.trim().replace('\n', " "))
                    .unwrap_or_default();
                self.preview.set_text(&first_text);

                self.show_slots(false);
                self.jump_as_action(
                    "Answer in transcript ↓",
                    "Jump to question in transcript",
                    "Answer in transcript",
                );

                self.revealer.set_reveal_child(true);
            }
            None => {
                place(&self.more_btn, false);
                self.show_slots(false);
                self.revealer.set_reveal_child(false);
            }
        }
        if let Some(source) = source {
            self.title
                .set_text(&format!("{source}: {}", self.title.text()));
            self.jump_as_action(
                "Review worker…",
                "Open the worker's request",
                "Review worker request",
            );
        }
    }

    /// The jump button as a small icon beside the approval's own buttons.
    /// Whether the approval slots take their space. A question or a form answers nothing from
    /// the shelf, so it gives the room to its title; an approval always has all three, so
    /// Deny and Allow land in the same place for every approval.
    fn show_slots(&self, shown: bool) {
        for button in [
            self.deny_btn.upcast_ref::<gtk4::Widget>(),
            self.allow_btn.upcast_ref(),
            self.more_btn.upcast_ref(),
        ] {
            if let Some(slot) = button.parent() {
                slot.set_visible(shown);
            }
        }
    }

    fn jump_as_icon(&self) {
        self.jump_btn.set_label("");
        self.jump_btn.set_icon_name("at-go-bottom-symbolic");
        self.jump_btn.remove_css_class("suggested-action");
        self.jump_btn.remove_css_class("pill");
        self.jump_btn.add_css_class("flat");
        self.jump_btn
            .set_tooltip_text(Some("Jump to card in transcript"));
        self.jump_btn
            .update_property(&[gtk4::accessible::Property::Label(
                "Jump to card in transcript",
            )]);
        self.jump_btn.set_visible(true);
    }

    /// The jump button as the shelf's one suggested action, labelled with what it does.
    fn jump_as_action(&self, text: &str, tooltip: &str, accessible: &str) {
        // The label alone: a button holds a label or an icon, and setting an (empty)
        // icon after it replaced the text with nothing, a blank pill.
        self.jump_btn.set_icon_name("");
        self.jump_btn.set_label(text);
        // Ellipsizes rather than widening the shelf at a large text scale.
        if let Some(text) = self.jump_btn.child().and_downcast::<gtk4::Label>() {
            text.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        }
        self.jump_btn.remove_css_class("flat");
        self.jump_btn.add_css_class("pill");
        self.jump_btn.add_css_class("suggested-action");
        self.jump_btn.set_tooltip_text(Some(tooltip));
        self.jump_btn
            .update_property(&[gtk4::accessible::Property::Label(accessible)]);
        self.jump_btn.set_visible(true);
    }
}

/// A fixed place on the shelf for `button`: a stack of the button and an empty page, the same
/// size, so a button that is not offered leaves its space empty rather than letting its
/// neighbours slide into it. Shown or emptied with [`place`].
/// Answers the approval in `active` with `decision` through `sink`: a permission request (a list
/// of places) is never allowed from the shelf, only denied or stopped, since the shelf cuts its
/// list, and Deny falls back to Cancel where the approval offers no Deny. False when no approval
/// is pending or it does not offer `decision`.
fn answer_active(
    active: &RefCell<Option<PendingInterruption>>,
    sink: &RowSink,
    decision: Decision,
) -> bool {
    // Copied out before the sink runs: answering resolves the approval at once, which updates
    // the shelf (`update` replaces `active`) while still in here.
    let answer = match active.borrow().as_ref() {
        Some(PendingInterruption::Approval {
            request,
            tool,
            options,
            ..
        }) => {
            let decision = match decision {
                Decision::Deny if !options.contains(&Decision::Deny) => Decision::Cancel,
                d => d,
            };
            let refusal = matches!(decision, Decision::Deny | Decision::Cancel);
            (options.contains(&decision) && (refusal || tool != permission::TOOL))
                .then(|| (request.clone(), decision))
        }
        _ => None,
    };
    let Some((request, decision)) = answer else {
        return false;
    };
    sink(RowEvent::Approve {
        request,
        decision,
        custom_rule: None,
    });
    true
}

fn slot(button: &impl IsA<gtk4::Widget>) -> gtk4::Stack {
    let stack = gtk4::Stack::new();
    stack.set_hhomogeneous(true);
    stack.set_vhomogeneous(true);
    stack.set_valign(gtk4::Align::Center);
    stack.add_named(button, Some("button"));
    stack.add_named(
        &gtk4::Box::new(gtk4::Orientation::Horizontal, 0),
        Some("empty"),
    );
    stack
}

/// Shows `button` in its [`slot`], or leaves the slot empty (same size, nothing to click).
fn place(button: &impl IsA<gtk4::Widget>, shown: bool) {
    if let Some(stack) = button.parent().and_downcast::<gtk4::Stack>() {
        stack.set_visible_child_name(if shown { "button" } else { "empty" });
    }
}

/// Whether `button` is the one showing in its [`slot`], and the slot is shown.
#[cfg(test)]
pub(crate) fn placed(button: &impl IsA<gtk4::Widget>) -> bool {
    button
        .parent()
        .and_downcast::<gtk4::Stack>()
        .filter(|stack| stack.is_visible())
        .and_then(|stack| stack.visible_child())
        .is_some_and(|child| &child == button.upcast_ref())
}

/// The shelf's one line for a pending approval: the call's card title (`Run <command>`,
/// `Edit <path>`, …), else the first file an edit names, else nothing.
fn approval_preview(tool: &str, input: &serde_json::Value) -> String {
    if tool == permission::TOOL {
        let request = permission::Request::parse(input);
        return if request.renderable {
            request.one_line()
        } else {
            "Part of this request could not be summarised. Review it before answering.".to_owned()
        };
    }
    let called = payload::tool_title(ItemKind::Tool, tool, Some(input), "");
    if !called.target.is_empty() || called.kind == agent_core::tool_title::ToolKind::Mcp {
        return called.text();
    }
    agent_kit::editdiff::preview_from_input(input)
        .first()
        .map(|edit| format!("Edit {}", edit.path))
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use agent_core::adapter::Driver;
    use agent_core::event::{Envelope, Event, ResponseCapability};
    use std::cell::Cell;

    /// The approval line says what the call's card will: one title grammar for every agent.
    #[test]
    fn approval_preview_is_the_cards_title() {
        let cases = [
            ("Bash", serde_json::json!({"command": "touch x.tmp"})),
            (
                "run_command",
                serde_json::json!({"CommandLine": "cargo test"}),
            ),
            (
                "command_execution",
                serde_json::json!({"command": "/usr/bin/zsh -lc 'ls -la'"}),
            ),
            ("Edit", serde_json::json!({"file_path": "/w/a.rs"})),
            ("mcp__unraid__unraid_containers", serde_json::json!({})),
        ];
        let want = [
            "Run touch x.tmp",
            "Run cargo test",
            "Run ls -la",
            "Edit /w/a.rs",
            "unraid/unraid_containers",
        ];
        for ((tool, input), want) in cases.iter().zip(want) {
            assert_eq!(approval_preview(tool, input), want, "{tool}");
            let title = payload::tool_title(ItemKind::Tool, tool, Some(input), "");
            assert_eq!(want, title.text(), "{tool}: same as the card");
        }
    }

    /// Regression: answering from the shelf resolves the approval at once, which updates the
    /// shelf again from inside its own button handler. That panicked (RefCell already borrowed)
    /// in a callback that cannot unwind, and so aborted the app. Needs GTK (the window smoke test
    /// runs it, through `chat::view::tests::ui_checks`).
    pub(crate) fn answering_from_the_shelf_reenters_safely() {
        let shelf: Rc<RefCell<Option<Rc<InterruptionShelf>>>> = Rc::default();
        let answered = Rc::new(Cell::new(false));
        let sink: RowSink = {
            let (shelf, answered) = (shelf.clone(), answered.clone());
            Rc::new(move |event| {
                if matches!(event, RowEvent::Approve { .. }) {
                    answered.set(true);
                    // What the view does on an answer: the approval is resolved, so the
                    // transcript has nothing pending and the shelf updates.
                    if let Some(shelf) = shelf.borrow().as_ref() {
                        shelf.update(&Transcript::new());
                    }
                }
            })
        };
        let built = Rc::new(InterruptionShelf::new(sink, |_| {}));
        *shelf.borrow_mut() = Some(built.clone());

        let mut transcript = Transcript::new();
        transcript.apply(
            &Envelope::new(Event::ApprovalRequested {
                tool: "Edit".into(),
                title: Some("Edit src/config.rs".into()),
                input: serde_json::json!({"file_path": "src/config.rs"}),
                reason: None,
                options: vec![Decision::Allow, Decision::Deny],
                response: ResponseCapability::Live,
                remembers: None,
            })
            .request("r1"),
            Driver::Claude,
        );
        built.update(&transcript);
        assert!(
            built.active.borrow().is_some(),
            "the approval is on the shelf"
        );

        built.allow_btn.emit_clicked();
        assert!(answered.get(), "Allow answered the approval");
        assert!(
            built.active.borrow().is_none(),
            "the shelf followed the answer"
        );
        // Break the test's own cycle (shelf → sink → shelf).
        shelf.borrow_mut().take();

        // A pending question: the shelf's one button says what it does (it rendered as a blank
        // pill when an empty icon was set over its label).
        let mut asked = Transcript::new();
        asked.apply(
            &Envelope::new(Event::QuestionRequested {
                source: Default::default(),
                questions: vec![agent_core::event::Question {
                    id: "q1".into(),
                    header: "Approach".into(),
                    question: "How should the crash be simulated?".into(),
                    options: vec![],
                    multi_select: false,
                }],
            })
            .request("q"),
            Driver::Claude,
        );
        built.update(&asked);
        assert_eq!(
            built.jump_btn.label().as_deref(),
            Some("Answer in transcript ↓")
        );

        // An approval with session and always allow: more_btn is visible, populated,
        // and clicking an item issues the approval.
        let session_approved = Rc::new(Cell::new(false));
        let sink2: RowSink = {
            let session_approved = session_approved.clone();
            Rc::new(move |event| {
                if let RowEvent::Approve { decision, .. } = event {
                    if decision == Decision::AllowForSession {
                        session_approved.set(true);
                    }
                }
            })
        };
        let shelf2 = InterruptionShelf::new(sink2, |_| {});
        let mut transcript2 = Transcript::new();
        transcript2.apply(
            &Envelope::new(Event::ApprovalRequested {
                tool: "Bash".into(),
                title: Some("Run cargo test".into()),
                input: serde_json::json!({"command": "cargo test"}),
                reason: None,
                options: vec![
                    Decision::Allow,
                    Decision::AllowForSession,
                    Decision::AllowAlways,
                    Decision::Deny,
                ],
                response: ResponseCapability::Live,
                remembers: None,
            })
            .request("r2"),
            Driver::Claude,
        );
        shelf2.update(&transcript2);
        assert!(placed(&shelf2.more_btn), "More dropdown button is visible");
        assert_eq!(
            shelf2.jump_btn.icon_name().as_deref(),
            Some("at-go-bottom-symbolic")
        );
        let first_child = shelf2
            .more_box
            .first_child()
            .expect("more_box has children");
        let btn = first_child.downcast::<gtk4::Button>().expect("is button");
        assert_eq!(btn.label().as_deref(), Some("Allow for session"));
        btn.emit_clicked();
        assert!(
            session_approved.get(),
            "Allow for session answered the approval"
        );
    }

    /// The shelf never grants a permission request unseen, and points at the form it cannot fill.
    #[test]
    fn a_permission_request_is_summarised_in_one_line() {
        let input = serde_json::json!({
            "cwd": "/w", "reason": null, "renderable": true,
            "entries": [
                {"access": "write", "target": {"kind": "path", "path": "/w/out"}},
                {"access": "read", "target": {"kind": "path", "path": "/data"}}
            ],
            "network": true, "session_allowed": true, "warning": null, "requested": {}
        });
        assert_eq!(
            approval_preview(permission::TOOL, &input),
            "Write /w/out; Read /data; network"
        );
        assert!(
            approval_preview(permission::TOOL, &serde_json::json!({"renderable": false}))
                .contains("could not be summarised")
        );
    }

    /// GTK: the shelf for a permission request has no one-click Allow (and no menu with a
    /// session grant); it offers Deny and a Review button. A form is a shelf entry too.
    pub(crate) fn the_shelf_holds_requests_it_must_not_grant_unseen() {
        let answered: Rc<RefCell<Vec<Decision>>> = Rc::default();
        let sink: RowSink = {
            let answered = answered.clone();
            Rc::new(move |event| {
                if let RowEvent::Approve { decision, .. } = event {
                    answered.borrow_mut().push(decision);
                }
            })
        };
        let shelf = InterruptionShelf::new(sink, |_| {});
        let mut transcript = Transcript::new();
        transcript.apply(
            &Envelope::new(Event::ApprovalRequested {
                tool: permission::TOOL.into(),
                title: Some("Codex asks to write to 1 place".into()),
                input: serde_json::json!({
                    "cwd": "/w", "reason": null, "renderable": true,
                    "entries": [{"access": "write", "target": {"kind": "path", "path": "/w/out"}}],
                    "network": null, "session_allowed": true, "warning": null, "requested": {}
                }),
                reason: None,
                options: vec![
                    Decision::Allow,
                    Decision::AllowForSession,
                    Decision::Deny,
                    Decision::Cancel,
                ],
                response: ResponseCapability::Live,
                remembers: None,
            })
            .request("p"),
            Driver::Codex,
        );
        shelf.update(&transcript);
        assert!(shelf.revealer.reveals_child());
        assert!(!placed(&shelf.allow_btn), "no one-click Allow");
        assert!(!placed(&shelf.more_btn), "no session grant in a menu");
        assert!(placed(&shelf.deny_btn));
        assert_eq!(shelf.preview.text(), "Write /w/out");
        assert_eq!(
            shelf.jump_btn.label().as_deref(),
            Some("Review in transcript ↓")
        );
        shelf.deny_btn.emit_clicked();
        assert_eq!(*answered.borrow(), [Decision::Deny]);
        shelf.allow_btn.emit_clicked();
        assert_eq!(
            *answered.borrow(),
            [Decision::Deny],
            "even a stray click on the hidden Allow grants nothing"
        );
        // The keyboard follows the same rules: no Allow or session grant for a permission
        // request (the key goes through), Deny still works.
        assert!(!shelf.answer(Decision::Allow));
        assert!(!shelf.answer(Decision::AllowForSession));
        assert!(shelf.answer(Decision::Deny));
        assert_eq!(*answered.borrow(), [Decision::Deny, Decision::Deny]);

        // An ordinary approval keeps its Allow.
        let mut command = Transcript::new();
        command.apply(
            &Envelope::new(Event::ApprovalRequested {
                tool: "Bash".into(),
                title: None,
                input: serde_json::json!({"command": "ls"}),
                reason: None,
                options: vec![Decision::Allow, Decision::Deny],
                response: ResponseCapability::Live,
                remembers: None,
            })
            .request("b"),
            Driver::Codex,
        );
        shelf.update(&command);
        assert!(placed(&shelf.allow_btn));
        assert!(shelf.jump_btn.icon_name().is_some());
        // Keys: what the approval does not offer lets the key through; Allow answers it.
        assert!(!shelf.answer(Decision::AllowAlways), "not offered");
        assert!(!shelf.answer(Decision::Cancel), "not offered");
        assert!(shelf.answer(Decision::Allow));
        assert_eq!(answered.borrow().last(), Some(&Decision::Allow));

        // Deny falls back to Stop turn where the approval offers no Deny, as the button does.
        let mut no_deny = Transcript::new();
        no_deny.apply(
            &Envelope::new(Event::ApprovalRequested {
                tool: "Bash".into(),
                title: None,
                input: serde_json::json!({"command": "ls"}),
                reason: None,
                options: vec![Decision::Allow, Decision::Cancel],
                response: ResponseCapability::Live,
                remembers: None,
            })
            .request("c"),
            Driver::Claude,
        );
        shelf.update(&no_deny);
        assert!(shelf.answer(Decision::Deny));
        assert_eq!(answered.borrow().last(), Some(&Decision::Cancel));
        // Nothing pending: every key goes through.
        shelf.update(&Transcript::new());
        assert!(!shelf.answer(Decision::Allow));
        assert!(!shelf.answer(Decision::Deny));

        // A form: whose words, and the way to the card.
        let mut form = Transcript::new();
        form.apply(
            &Envelope::new(Event::FormRequested {
                server: "github".into(),
                message: "Which repo?\nSecond line".into(),
                form: agent_core::form::Form::Link {
                    url: "https://example.com/a".into(),
                },
            })
            .request("f"),
            Driver::Codex,
        );
        shelf.update(&form);
        assert!(shelf.revealer.reveals_child());
        assert_eq!(shelf.title.text(), "A tool server wants you to open a link");
        assert_eq!(shelf.preview.text(), "github: Which repo?");
        assert!(!placed(&shelf.allow_btn) && !placed(&shelf.deny_btn));
        assert_eq!(
            shelf.jump_btn.label().as_deref(),
            Some("Review in transcript ↓")
        );
        let mut fields = Transcript::new();
        fields.apply(
            &Envelope::new(Event::FormRequested {
                server: "github".into(),
                message: String::new(),
                form: agent_core::form::Form::Fields { fields: vec![] },
            })
            .request("g"),
            Driver::Codex,
        );
        shelf.update(&fields);
        assert_eq!(shelf.title.text(), "A tool server wants information");
        assert_eq!(shelf.preview.text(), "github", "no empty quote");
        assert_eq!(
            shelf.jump_btn.label().as_deref(),
            Some("Answer in transcript ↓")
        );
        // Several waiting: the shelf shows the newest and says there are more.
        assert!(
            !shelf.count.is_visible(),
            "one thing waiting needs no count"
        );
        transcript.apply(
            &Envelope::new(Event::FormRequested {
                server: "github".into(),
                message: String::new(),
                form: agent_core::form::Form::Fields { fields: vec![] },
            })
            .request("g2"),
            Driver::Codex,
        );
        shelf.update(&transcript);
        assert!(shelf.count.is_visible());
        assert_eq!(shelf.count.text(), "1 of 2");
        shelf.update(&Transcript::new());
        assert!(!shelf.revealer.reveals_child());
        assert!(!shelf.count.is_visible());
    }
}
