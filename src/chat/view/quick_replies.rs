//! Quick-reply chips: the choices a Codex or agy reply ended with, offered under that reply.
//!
//! Not an interruption. The agent asked nothing through the protocol and waits for nothing, so
//! the chips take no shelf, no sidebar state and no composer hint. A chip only fills the
//! composer; the user reads it, edits it if they like, and presses Enter.

use adw::prelude::*;

use super::cards::{label, option_content, RowEvent, RowSink};

/// The chips for `options` (rendered plain text, exactly what a chip puts in the composer).
pub(super) fn row(options: &[String], sink: &RowSink) -> gtk4::Box {
    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    root.add_css_class("quick-replies");
    root.set_accessible_role(gtk4::AccessibleRole::Group);
    root.update_property(&[gtk4::accessible::Property::Label("Quick replies")]);

    let heading = label("Quick replies", &["quick-replies-label"]);
    heading.set_halign(gtk4::Align::Start);
    root.append(&heading);

    let flow = gtk4::FlowBox::new();
    flow.set_selection_mode(gtk4::SelectionMode::None);
    flow.set_max_children_per_line(4);
    flow.set_column_spacing(8);
    flow.set_row_spacing(8);
    flow.set_homogeneous(false);
    // Chips take the width of their text, not an equal share of the row.
    flow.set_halign(gtk4::Align::Start);
    flow.set_hexpand(false);
    for text in options {
        let chip = gtk4::Button::new();
        chip.add_css_class("option-button");
        chip.add_css_class("quick-reply");
        chip.set_child(Some(&option_content(text, None)));
        chip.set_tooltip_text(Some(text));
        let sink = sink.clone();
        let text = text.clone();
        chip.connect_clicked(move |_| sink(RowEvent::QuickReply { text: text.clone() }));
        flow.insert(&chip, -1);
    }
    root.append(&flow);

    let actions = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
    for (title, tip, event) in [
        (
            "Dismiss",
            "Hide these suggestions",
            (|| RowEvent::DismissQuickReplies) as fn() -> RowEvent,
        ),
        (
            "Don\u{2019}t suggest these",
            "Stop offering quick replies under replies that end with a question",
            || RowEvent::StopQuickReplies,
        ),
    ] {
        let button = gtk4::Button::with_label(title);
        button.add_css_class("flat");
        button.add_css_class("quick-reply-action");
        button.set_tooltip_text(Some(tip));
        let sink = sink.clone();
        button.connect_clicked(move |_| sink(event()));
        actions.append(&button);
    }
    root.append(&actions);
    root
}
