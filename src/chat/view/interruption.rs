//! The sticky interruption shelf docked above the composer.
//!
//! Reveals automatically when an approval or question is pending, even if the user
//! has scrolled up or subsequent streaming output pushed the card out of view.

use std::cell::RefCell;
use std::rc::Rc;

use agent_core::event::{Decision, ItemKind};
use gtk4::prelude::*;

use super::cards::{label, RowEvent, RowSink};
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
    jump_btn: gtk4::Button,
    active: Rc<RefCell<Option<PendingInterruption>>>,
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

        let jump_btn = gtk4::Button::from_icon_name("at-pan-down-symbolic");
        jump_btn.add_css_class("flat");
        jump_btn.set_tooltip_text(Some("Jump to card in transcript"));

        actions.append(&deny_btn);
        actions.append(&allow_btn);
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
                if let Some(PendingInterruption::Approval { request, .. }) =
                    active.borrow().as_ref()
                {
                    sink(RowEvent::Approve {
                        request: request.clone(),
                        decision: Decision::Allow,
                    });
                }
            });
        }

        {
            let active = active.clone();
            let sink = sink.clone();
            deny_btn.connect_clicked(move |_| {
                if let Some(PendingInterruption::Approval {
                    request, options, ..
                }) = active.borrow().as_ref()
                {
                    let decision = if options.contains(&Decision::Deny) {
                        Decision::Deny
                    } else {
                        Decision::Cancel
                    };
                    sink(RowEvent::Approve {
                        request: request.clone(),
                        decision,
                    });
                }
            });
        }

        {
            let active = active.clone();
            let jump = Rc::new(jump);
            jump_btn.connect_clicked(move |_| {
                if let Some(interruption) = active.borrow().as_ref() {
                    let item_id = match interruption {
                        PendingInterruption::Approval { item_id, .. } => item_id.clone(),
                        PendingInterruption::Question { item_id, .. } => item_id.clone(),
                    };
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
            jump_btn,
            active,
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
                self.jump_btn.set_label("");
                self.jump_btn.set_icon_name("at-pan-down-symbolic");
                self.jump_btn.remove_css_class("suggested-action");
                self.jump_btn.remove_css_class("pill");
                self.jump_btn.add_css_class("flat");
                self.jump_btn
                    .set_tooltip_text(Some("Jump to card in transcript"));
                self.jump_btn.set_visible(true);

                self.revealer.set_reveal_child(true);
            }
            Some(PendingInterruption::Question { questions, .. }) => {
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
                self.jump_btn.set_label("Answer in transcript ↓");
                self.jump_btn.set_icon_name("");
                self.jump_btn.remove_css_class("flat");
                self.jump_btn.add_css_class("pill");
                self.jump_btn.add_css_class("suggested-action");
                self.jump_btn
                    .set_tooltip_text(Some("Jump to question in transcript"));
                self.jump_btn.set_visible(true);

                self.revealer.set_reveal_child(true);
            }
            None => {
                self.revealer.set_reveal_child(false);
            }
        }
    }
}
