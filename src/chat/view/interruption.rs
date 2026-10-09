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

pub struct InterruptionShelf {
    pub revealer: gtk4::Revealer,
    card: gtk4::Box,
    icon: gtk4::Image,
    title: gtk4::Label,
    preview: gtk4::Label,
    allow_btn: gtk4::Button,
    deny_btn: gtk4::Button,
    pub(crate) more_btn: gtk4::MenuButton,
    pub(crate) more_box: gtk4::Box,
    pub(crate) more_popover: gtk4::Popover,
    pub(crate) jump_btn: gtk4::Button,
    active: Rc<RefCell<Option<PendingInterruption>>>,
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
        title.set_xalign(0.0);
        title.set_ellipsize(gtk4::pango::EllipsizeMode::End);

        let preview = label("", &["interruption-preview"]);
        preview.set_xalign(0.0);
        preview.set_ellipsize(gtk4::pango::EllipsizeMode::End);

        info.append(&title);
        info.append(&preview);
        card.append(&info);

        let actions = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        actions.set_valign(gtk4::Align::Center);

        let deny_btn = gtk4::Button::with_label("Deny");
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
        more_btn.set_visible(false);

        let jump_btn = gtk4::Button::from_icon_name("at-go-bottom-symbolic");
        jump_btn.add_css_class("flat");
        jump_btn.set_tooltip_text(Some("Jump to card in transcript"));

        actions.append(&deny_btn);
        actions.append(&allow_btn);
        actions.append(&more_btn);
        actions.append(&jump_btn);
        card.append(&actions);

        let revealer = gtk4::Revealer::new();
        revealer.set_transition_type(gtk4::RevealerTransitionType::SlideDown);
        revealer.set_child(Some(&card));
        revealer.set_reveal_child(false);

        let active: Rc<RefCell<Option<PendingInterruption>>> = Rc::new(RefCell::new(None));

        {
            let active = active.clone();
            let sink = sink.clone();
            allow_btn.connect_clicked(move |_| {
                // Copied out before the sink runs: answering resolves the approval at once,
                // which updates this shelf (`update` replaces `active`) while still in here.
                let request = match active.borrow().as_ref() {
                    Some(PendingInterruption::Approval { request, .. }) => Some(request.clone()),
                    _ => None,
                };
                if let Some(request) = request {
                    sink(RowEvent::Approve {
                        request,
                        decision: Decision::Allow,
                    });
                }
            });
        }

        {
            let active = active.clone();
            let sink = sink.clone();
            deny_btn.connect_clicked(move |_| {
                // Copied out before the sink runs (see the Allow button).
                let answer = match active.borrow().as_ref() {
                    Some(PendingInterruption::Approval {
                        request, options, ..
                    }) => {
                        let decision = if options.contains(&Decision::Deny) {
                            Decision::Deny
                        } else {
                            Decision::Cancel
                        };
                        Some((request.clone(), decision))
                    }
                    _ => None,
                };
                if let Some((request, decision)) = answer {
                    sink(RowEvent::Approve { request, decision });
                }
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
                        PendingInterruption::Approval { item_id, .. } => item_id.clone(),
                        PendingInterruption::Question { item_id, .. } => item_id.clone(),
                    });
                if let Some(item_id) = item_id {
                    jump(item_id);
                }
            });
        }

        Self {
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
            sink,
        }
    }

    pub fn update(&self, transcript: &Transcript) {
        let interruption = transcript.pending_interruption();
        if interruption == *self.active.borrow() {
            return;
        }
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

                let kind = if tool.eq_ignore_ascii_case("bash") || tool == "run_command" {
                    ItemKind::Command
                } else {
                    ItemKind::Tool
                };
                let prev = if let Some(summary) = payload::tool_summary(kind, Some(&input), "") {
                    if kind == ItemKind::Command {
                        format!("$ {summary}")
                    } else {
                        summary
                    }
                } else {
                    let edits = agent_kit::editdiff::preview_from_input(&input);
                    if !edits.is_empty() {
                        format!("Edit {}", edits[0].path)
                    } else {
                        String::new()
                    }
                };
                self.preview.set_text(&prev);
                self.preview.set_visible(!prev.is_empty());

                let has_allow = options.contains(&Decision::Allow);
                let has_deny =
                    options.contains(&Decision::Deny) || options.contains(&Decision::Cancel);
                self.allow_btn.set_visible(has_allow);
                self.deny_btn.set_visible(has_deny);
                self.allow_btn.set_label("Allow");
                self.deny_btn
                    .set_label(if options.contains(&Decision::Deny) {
                        "Deny"
                    } else {
                        "Cancel"
                    });

                // Clear previous extra options in the dropdown popover
                while let Some(child) = self.more_box.first_child() {
                    self.more_box.remove(&child);
                }

                let extra_decisions: Vec<Decision> =
                    [Decision::AllowForSession, Decision::AllowAlways]
                        .into_iter()
                        .filter(|d| options.contains(d))
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
                        let request = match active.borrow().as_ref() {
                            Some(PendingInterruption::Approval { request, .. }) => {
                                Some(request.clone())
                            }
                            _ => None,
                        };
                        if let Some(request) = request {
                            sink(RowEvent::Approve { request, decision });
                        }
                    });
                    self.more_box.append(&b);
                }
                self.more_btn.set_visible(!extra_decisions.is_empty());

                self.jump_btn.set_label("");
                self.jump_btn.set_icon_name("at-go-bottom-symbolic");
                self.jump_btn.remove_css_class("suggested-action");
                self.jump_btn.remove_css_class("pill");
                self.jump_btn.add_css_class("flat");
                self.jump_btn
                    .set_tooltip_text(Some("Jump to card in transcript"));
                self.jump_btn.set_visible(true);

                self.revealer.set_reveal_child(true);
            }
            Some(PendingInterruption::Question { questions, .. }) => {
                self.more_btn.set_visible(false);
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
                self.preview.set_visible(!first_text.is_empty());

                self.allow_btn.set_visible(false);
                self.deny_btn.set_visible(false);
                // The label alone: a button holds a label or an icon, and setting an (empty)
                // icon after it replaced the text with nothing, a blank pill.
                self.jump_btn.set_icon_name("");
                self.jump_btn.set_label("Answer in transcript ↓");
                self.jump_btn.remove_css_class("flat");
                self.jump_btn.add_css_class("pill");
                self.jump_btn.add_css_class("suggested-action");
                self.jump_btn
                    .set_tooltip_text(Some("Jump to question in transcript"));
                self.jump_btn.set_visible(true);

                self.revealer.set_reveal_child(true);
            }
            None => {
                self.more_btn.set_visible(false);
                self.revealer.set_reveal_child(false);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use agent_core::adapter::Driver;
    use agent_core::event::{Envelope, Event, ResponseCapability};
    use std::cell::Cell;

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
        assert!(
            shelf2.more_btn.is_visible(),
            "More dropdown button is visible"
        );
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
}
