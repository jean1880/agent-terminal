//! Transcript row widgets: bubbles, markdown, reasoning, tool cards, approvals, questions and
//! the inline dividers. Each row is built once from its [`Item`] and then updated in place, so
//! a streaming message only touches the widgets of the blocks that changed.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use agent_core::adapter::Driver;
use agent_core::event::{Decision, ItemKind};
use gtk4::glib;
use serde_json::Value;
use sourceview5::prelude::*;

use super::difftext::{self, DiffText};
use super::form_card::FormCardWidget;
use super::git_card::{self, GitSummary};
use super::markdown::{self, Block, TextKind};
use super::model::{format_tokens, ApprovalState, Body, Item, QuestionState, Tone, ToolStatus};
use super::payload;
use super::permission;
use crate::chat::DiffReply;
use crate::diff_tool::{self, DiffTools};

/// What a row asks of the view (the view owns the model and the backend).
pub enum RowEvent {
    Toggle {
        id: String,
        expanded: bool,
    },
    Approve {
        request: String,
        decision: Decision,
        custom_rule: Option<crate::always_allow::Rule>,
    },
    Answer {
        request: String,
        answers: Value,
    },
    /// A form card was submitted (`content` is what the form's check returned) or declined.
    Form {
        request: String,
        action: agent_core::form::FormAction,
        content: Option<Value>,
    },
    /// A quick-reply chip was chosen: its text goes in the composer, nothing is sent.
    QuickReply {
        text: String,
    },
    /// "Dismiss" on the quick-reply chips.
    DismissQuickReplies,
    /// "Don't suggest these" on the quick-reply chips.
    StopQuickReplies,
    /// A file-change card was asked to show its diff.
    LoadDiff {
        id: String,
    },
    /// A file-change card's "Open in ‹tool›".
    OpenDiff {
        id: String,
    },
}

pub type RowSink = Rc<dyn Fn(RowEvent)>;

/// The CSS class carrying an agent's accent colour.
pub fn accent_class(driver: Driver) -> &'static str {
    driver.info().accent_class
}

/// An agent's brand mark, tinted by the enclosing `accent-*` class (`.accent-dot`). Codex
/// honours the user's override file (`icons::driver_icon`).
pub fn brand_image(driver: Driver) -> gtk4::Image {
    let image = gtk4::Image::from_gicon(&crate::icons::driver_icon(driver));
    image.set_pixel_size(14);
    image.add_css_class("accent-dot");
    image
}

/// A plain label. NOT selectable: a selectable label claims the press (it places a caret), so a
/// label inside a button or any click target would swallow the click. Content that a person
/// may want to copy opts in with [`content_label`] or an explicit `set_selectable(true)`.
pub fn label(text: &str, classes: &[&str]) -> gtk4::Label {
    let l = gtk4::Label::new(Some(text));
    l.set_xalign(0.0);
    l.set_selectable(false);
    for c in classes {
        l.add_css_class(c);
    }
    l
}

/// A label for content (never inside a button or click target): selectable, so it can be copied.
pub fn content_label(text: &str, classes: &[&str]) -> gtk4::Label {
    let l = label(text, classes);
    l.set_selectable(true);
    l
}

/// A card above the composer whose header opens and closes its body: the plan, the running
/// sub-agents. Built by [`collapsible`].
pub struct Collapsible {
    pub card: gtk4::Box,
    /// The header button and the body it opens: wired up already; kept for tests to drive.
    #[cfg_attr(not(test), allow(dead_code))]
    pub toggle: gtk4::Button,
    #[cfg_attr(not(test), allow(dead_code))]
    pub body: gtk4::Revealer,
}

/// A collapsible card: a header button (`icon`, `title`, a chevron) over `content`, open or
/// closed to start with. The chevron, the tooltip ("Hide …"/"Show …" + `what`) and the
/// accessible expanded state follow every toggle.
pub fn collapsible(
    icon: &str,
    title: &gtk4::Label,
    content: &impl IsA<gtk4::Widget>,
    open: bool,
    what: &'static str,
) -> Collapsible {
    let card = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    card.add_css_class("plan-card");
    let toggle = gtk4::Button::new();
    toggle.add_css_class("flat");
    toggle.add_css_class("plan-toggle");
    let head = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    head.append(&gtk4::Image::from_icon_name(icon));
    title.set_hexpand(true);
    head.append(title);
    let chevron = gtk4::Image::new();
    head.append(&chevron);
    toggle.set_child(Some(&head));
    card.append(&toggle);
    let body = gtk4::Revealer::builder()
        .reveal_child(open)
        .child(content)
        .build();
    card.append(&body);

    let show = move |toggle: &gtk4::Button, chevron: &gtk4::Image, open: bool| {
        chevron.set_icon_name(Some(if open {
            "at-pan-down-symbolic"
        } else {
            "at-pan-up-symbolic"
        }));
        toggle.set_tooltip_text(Some(&format!(
            "{} {what}",
            if open { "Hide" } else { "Show" }
        )));
        toggle.update_state(&[gtk4::accessible::State::Expanded(Some(open))]);
        toggle.update_property(&[gtk4::accessible::Property::Label(&format!(
            "{} {what}",
            if open { "Hide" } else { "Show" }
        ))]);
    };
    show(&toggle, &chevron, open);
    toggle.connect_clicked({
        let body = body.downgrade();
        move |toggle| {
            let Some(body) = body.upgrade() else { return };
            let open = !body.reveals_child();
            body.set_reveal_child(open);
            show(toggle, &chevron, open);
        }
    });
    Collapsible { card, toggle, body }
}

/// Hides `revealer`'s child once it has closed and shows it again as it starts to open. GTK
/// measures a closed revealer's child on every size request (`tests::
/// a_closed_revealer_measures_its_child_unless_hidden`), so every collapsed card's body was
/// re-wrapped on each width change; a hidden child is skipped. Nothing shows differently: a
/// closed revealer draws nothing, and the child is shown before the opening transition starts.
pub(super) fn measure_only_when_open(revealer: &gtk4::Revealer) {
    let sync = |r: &gtk4::Revealer| {
        if let Some(child) = r.child() {
            let open = r.reveals_child() || r.is_child_revealed();
            if child.is_visible() != open {
                child.set_visible(open);
            }
        }
    };
    sync(revealer);
    revealer.connect_reveal_child_notify(sync);
    revealer.connect_child_revealed_notify(sync);
}

pub(super) fn wrapping(l: &gtk4::Label) {
    wrap_words(l);
    l.set_hexpand(true);
}

/// Wraps `l` between words, and inside a word too long for a line, without making it expand.
pub(super) fn wrap_words(l: &gtk4::Label) {
    l.set_wrap(true);
    l.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
}

/// A [`label`] that wraps between words.
pub(super) fn wrapped_text(text: &str, classes: &[&str]) -> gtk4::Label {
    let l = label(text, classes);
    l.set_wrap(true);
    l
}

/// A card's head: the `icon` beside its wrapping `heading` (a question card's look). Returns the
/// head and its icon.
pub(super) fn card_head(icon: &str, heading: &str) -> (gtk4::Box, gtk4::Image) {
    let head = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    let image = gtk4::Image::from_icon_name(icon);
    head.append(&image);
    let heading = label(heading, &["question-heading"]);
    wrapping(&heading);
    head.append(&heading);
    (head, image)
}

/// A one-line chip naming whose words follow, cut at `max_chars` with the whole text as its
/// tooltip.
pub(super) fn chip(text: &str, max_chars: i32) -> gtk4::Label {
    let chip = label(text, &["question-chip"]);
    chip.set_halign(gtk4::Align::Start);
    chip.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    chip.set_max_width_chars(max_chars);
    chip.set_tooltip_text(Some(text));
    chip
}

/// A selectable, wrapping monospace label for tool input and output.
pub(super) fn mono(text: &str) -> gtk4::Label {
    let l = label(text, &["mono-text"]);
    wrapping(&l);
    l.set_selectable(true);
    l.set_valign(gtk4::Align::Start);
    l
}

// ---------------------------------------------------------------------------------------------
// Markdown blocks
// ---------------------------------------------------------------------------------------------

enum BlockWidget {
    Text(gtk4::Label),
    Code {
        root: gtk4::Box,
        buffer: sourceview5::Buffer,
        lang_label: gtk4::Label,
    },
    Rule(gtk4::Separator),
}

impl BlockWidget {
    fn widget(&self) -> gtk4::Widget {
        match self {
            Self::Text(l) => l.clone().upcast(),
            Self::Code { root, .. } => root.clone().upcast(),
            Self::Rule(s) => s.clone().upcast(),
        }
    }

    fn build(block: &Block) -> Self {
        match block {
            Block::Text { markup, kind } => {
                let l = gtk4::Label::new(None);
                l.set_xalign(0.0);
                wrapping(&l);
                l.set_selectable(true);
                l.add_css_class("md-text");
                l.add_css_class(match kind {
                    TextKind::Paragraph => "md-paragraph",
                    TextKind::Heading(_) => "md-heading",
                    TextKind::Quote => "md-quote",
                    TextKind::List => "md-list",
                });
                l.set_markup(markup);
                Self::Text(l)
            }
            Block::Code { lang, text } => code_block(lang.as_deref(), text),
            Block::Rule => {
                let s = gtk4::Separator::new(gtk4::Orientation::Horizontal);
                s.add_css_class("md-rule");
                Self::Rule(s)
            }
        }
    }

    /// Updates in place when the kind matches; returns false when it must be rebuilt.
    fn update(&self, old: &Block, new: &Block) -> bool {
        match (self, old, new) {
            (Self::Text(l), Block::Text { kind: a, .. }, Block::Text { kind: b, markup })
                if a == b =>
            {
                l.set_markup(markup);
                true
            }
            (
                Self::Code { buffer, .. },
                Block::Code { lang: a, .. },
                Block::Code { lang: b, text },
            ) if a == b => {
                buffer.set_text(text);
                true
            }
            (Self::Rule(_), Block::Rule, Block::Rule) => true,
            _ => false,
        }
    }
}

thread_local! {
    static SCHEME: Option<sourceview5::StyleScheme> = {
        let manager = sourceview5::StyleSchemeManager::default();
        ["Adwaita-dark", "oblivion", "cobalt", "classic-dark"]
            .iter()
            .find_map(|id| manager.scheme(id))
    };
}

/// Gives `buffer` the app's dark source-view scheme (when GtkSourceView has one of them).
pub(super) fn apply_scheme(buffer: &sourceview5::Buffer) {
    SCHEME.with(|s| {
        if let Some(scheme) = s {
            buffer.set_style_scheme(Some(scheme));
        }
    });
}

/// A flat "Copy" icon button, named for what it copies (`what`: "Copy output"). Give it its text
/// with [`wire_copy`].
pub(super) fn copy_button(what: &str) -> gtk4::Button {
    let copy = gtk4::Button::from_icon_name("at-edit-copy-symbolic");
    copy.add_css_class("flat");
    copy.add_css_class("code-copy");
    copy.set_valign(gtk4::Align::Center);
    copy.set_tooltip_text(Some(what));
    copy.update_property(&[gtk4::accessible::Property::Label(what)]);
    copy
}

/// Makes `copy` put `text()` on the clipboard, and shows a tick for a moment.
pub(super) fn wire_copy(copy: &gtk4::Button, text: impl Fn() -> String + 'static) {
    copy.connect_clicked(move |copy| {
        copy.clipboard().set_text(&text());
        copy.set_icon_name("at-object-select-symbolic");
        glib::timeout_add_local_once(
            std::time::Duration::from_millis(1200),
            glib::clone!(
                #[weak]
                copy,
                move || copy.set_icon_name("at-edit-copy-symbolic")
            ),
        );
    });
}

/// A code block: language label + copy button over a read-only, highlighted source view.
fn code_block(lang: Option<&str>, text: &str) -> BlockWidget {
    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    root.add_css_class("code-block");

    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    header.add_css_class("code-header");
    let lang_label = label(lang.unwrap_or("text"), &["code-lang"]);
    lang_label.set_hexpand(true);
    header.append(&lang_label);
    let copy = copy_button("Copy code");
    header.append(&copy);
    root.append(&header);

    let buffer = sourceview5::Buffer::new(None);
    buffer.set_highlight_matching_brackets(false);
    if let Some(lang) =
        lang.and_then(|l| sourceview5::LanguageManager::default().language(&normalise_lang(l)))
    {
        buffer.set_language(Some(&lang));
        buffer.set_highlight_syntax(true);
    }
    apply_scheme(&buffer);
    buffer.set_text(text);
    let view = sourceview5::View::with_buffer(&buffer);
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_monospace(true);
    view.set_wrap_mode(gtk4::WrapMode::None);
    view.add_css_class("code-view");
    view.set_top_margin(10);
    view.set_bottom_margin(10);
    view.set_left_margin(12);
    view.set_right_margin(12);

    let scroller = gtk4::ScrolledWindow::new();
    scroller.set_policy(gtk4::PolicyType::Automatic, gtk4::PolicyType::Never);
    scroller.set_propagate_natural_height(true);
    scroller.set_child(Some(&view));
    root.append(&scroller);

    wire_copy(&copy, {
        let buffer = buffer.downgrade();
        move || {
            buffer.upgrade().map_or_else(String::new, |buffer| {
                let (start, end) = buffer.bounds();
                buffer.text(&start, &end, false).to_string()
            })
        }
    });

    BlockWidget::Code {
        root,
        buffer,
        lang_label,
    }
}

/// Markdown fence names → GtkSourceView language ids.
fn normalise_lang(lang: &str) -> String {
    match lang.to_lowercase().as_str() {
        "rs" => "rust".into(),
        "py" | "python3" => "python3".into(),
        "sh" | "bash" | "zsh" | "shell" | "console" => "sh".into(),
        "js" | "javascript" => "js".into(),
        "ts" | "typescript" => "typescript".into(),
        "yml" => "yaml".into(),
        "md" => "markdown".into(),
        "diff" | "patch" => "diff".into(),
        other => other.to_owned(),
    }
}

/// A container of markdown blocks that re-renders only the blocks that changed.
pub struct MarkdownView {
    root: gtk4::Box,
    blocks: RefCell<Vec<(Block, BlockWidget)>>,
    source: RefCell<String>,
}

impl MarkdownView {
    pub fn new() -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
        root.add_css_class("markdown");
        Self {
            root,
            blocks: RefCell::new(Vec::new()),
            source: RefCell::new(String::new()),
        }
    }

    pub fn widget(&self) -> &gtk4::Box {
        &self.root
    }

    pub fn set_markdown(&self, text: &str) {
        if *self.source.borrow() == text {
            return;
        }
        text.clone_into(&mut self.source.borrow_mut());
        let new = markdown::parse(text);
        let mut blocks = self.blocks.borrow_mut();
        let common = blocks
            .iter()
            .zip(&new)
            .take_while(|((old, _), new)| old == *new)
            .count();
        // Update the remaining blocks in place where the kind still matches, else rebuild.
        let mut i = common;
        while i < new.len() {
            if let Some((old, widget)) = blocks.get(i) {
                if widget.update(old, &new[i]) {
                    blocks[i].0 = new[i].clone();
                    i += 1;
                    continue;
                }
                let (_, widget) = blocks.remove(i);
                self.root.remove(&widget.widget());
            }
            let widget = BlockWidget::build(&new[i]);
            let w = widget.widget();
            match i.checked_sub(1).and_then(|p| blocks.get(p)) {
                Some((_, prev)) => self.root.insert_child_after(&w, Some(&prev.widget())),
                None => self.root.prepend(&w),
            }
            blocks.insert(i, (new[i].clone(), widget));
            i += 1;
        }
        while blocks.len() > new.len() {
            if let Some((_, widget)) = blocks.pop() {
                self.root.remove(&widget.widget());
            }
        }
        if let Some((Block::Code { lang, .. }, BlockWidget::Code { lang_label, .. })) =
            blocks.last()
        {
            lang_label.set_text(lang.as_deref().unwrap_or("text"));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------------------------

pub enum Row {
    User {
        root: gtk4::Box,
        text: gtk4::Label,
    },
    Assistant {
        root: gtk4::Box,
        md: MarkdownView,
        caret: gtk4::Label,
    },
    Reasoning(ReasoningRow),
    Tool(ToolCard),
    Static(gtk4::Widget),
    /// An agent or model switch divider: its words change when the new agent names its model.
    Switch {
        root: gtk4::Widget,
        words: gtk4::Label,
    },
    Approval(ApprovalCard),
    Question(QuestionCardWidget),
    Form(FormCardWidget),
    Plan {
        root: gtk4::Expander,
        steps: gtk4::Box,
        updating: Rc<Cell<bool>>,
    },
}

impl Row {
    pub fn build(item: &Item, sink: &RowSink) -> Self {
        let row = match &item.body {
            Body::User { queued, .. } => {
                let root = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
                root.add_css_class("user-row");
                root.set_halign(gtk4::Align::End);
                if *queued {
                    root.add_css_class("queued");
                }
                let text = label("", &["user-bubble"]);
                wrapping(&text);
                text.set_hexpand(false);
                text.set_selectable(true);
                text.set_max_width_chars(72);
                root.append(&text);
                Self::User { root, text }
            }
            Body::Assistant { driver, .. } => {
                let root = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
                root.add_css_class("assistant-row");
                root.add_css_class(accent_class(*driver));
                let head = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
                head.add_css_class("assistant-head");
                head.append(&brand_image(*driver));
                head.append(&label(driver.info().label, &["agent-name"]));
                root.append(&head);
                let md = MarkdownView::new();
                root.append(md.widget());
                let caret = label("▍", &["stream-caret"]);
                caret.set_selectable(false);
                root.append(&caret);
                Self::Assistant { root, md, caret }
            }
            Body::Reasoning { .. } => Self::Reasoning(ReasoningRow::new(&item.id, sink)),
            Body::Tool(_) => Self::Tool(ToolCard::new(&item.id, sink)),
            Body::Notice { text, tone } => Self::Static(notice(text, *tone).upcast()),
            Body::Compaction {
                manual,
                before,
                after,
            } => Self::Static(divider(
                &compaction_text(*manual, *before, *after),
                "compaction-divider",
                None,
            )),
            Body::TurnError { message } => Self::Static(error_row(message).upcast()),
            Body::Switch {
                driver,
                model,
                agent_changed,
                returned,
            } => {
                let text = switch_text(*driver, model.as_deref(), *agent_changed, *returned);
                let (root, words) = divider_parts(&text, "switch-divider", Some(*driver));
                Self::Switch { root, words }
            }
            Body::Approval(_) => Self::Approval(ApprovalCard::new(sink)),
            Body::Question(q) => Self::Question(QuestionCardWidget::new(q, sink)),
            Body::Form(f) => Self::Form(FormCardWidget::new(f, sink)),
            Body::QuickReplies(q) => {
                Self::Static(super::quick_replies::row(&q.options, sink).upcast())
            }
            Body::Plan { .. } => {
                let root = gtk4::Expander::new(None);
                let steps = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
                steps.set_margin_start(12);
                steps.set_margin_top(6);
                steps.set_margin_bottom(6);
                root.set_child(Some(&steps));
                let id = item.id.clone();
                let sink = sink.clone();
                let updating = Rc::new(Cell::new(false));
                let guard = updating.clone();
                root.connect_expanded_notify(move |root| {
                    if guard.get() {
                        return;
                    }
                    sink(RowEvent::Toggle {
                        id: id.clone(),
                        expanded: root.is_expanded(),
                    });
                });
                Self::Plan {
                    root,
                    steps,
                    updating,
                }
            }
        };
        row.update(item);
        row
    }

    pub fn widget(&self) -> gtk4::Widget {
        match self {
            Self::User { root, .. } | Self::Assistant { root, .. } => root.clone().upcast(),
            Self::Reasoning(r) => r.root.clone().upcast(),
            Self::Tool(t) => t.root.clone().upcast(),
            Self::Static(w) | Self::Switch { root: w, .. } => w.clone(),
            Self::Approval(a) => a.root.clone().upcast(),
            Self::Question(q) => q.root.clone().upcast(),
            Self::Form(f) => f.root().clone().upcast(),
            Self::Plan { root, .. } => root.clone().upcast(),
        }
    }

    /// Focuses a pending answer after an explicit user jump. Historical/expired cards never
    /// take focus; approval buttons are ordered with Deny/Cancel before permission grants.
    pub fn focus_pending_action(&self) -> bool {
        let area = match self {
            Self::Approval(a) if a.buttons.is_visible() && a.buttons.is_sensitive() => {
                a.buttons.clone().upcast::<gtk4::Widget>()
            }
            Self::Question(q) if q.submit.is_visible() && q.options.is_sensitive() => {
                q.options.clone().upcast::<gtk4::Widget>()
            }
            // The first field, else the first button: the user asked to be taken here.
            Self::Form(f) => return f.focus_first(),
            _ => return false,
        };
        fn focus_button(widget: &gtk4::Widget) -> bool {
            if !widget.is_visible() || !widget.is_sensitive() {
                return false;
            }
            if widget.is::<gtk4::Button>() && widget.is_focusable() {
                return widget.grab_focus();
            }
            let mut child = widget.first_child();
            while let Some(widget) = child {
                child = widget.next_sibling();
                if focus_button(&widget) {
                    return true;
                }
            }
            false
        }
        focus_button(&area)
    }

    /// Where nested (subagent) rows go.
    pub fn children_box(&self) -> Option<&gtk4::Box> {
        match self {
            Self::Tool(t) => Some(&t.children),
            _ => None,
        }
    }

    pub fn update(&self, item: &Item) {
        match (self, &item.body) {
            (Self::User { root, text: l }, Body::User { text, queued }) => {
                // The prompt a busy switch sent for the user is a marker, not their bubble; the
                // tooltip keeps what the agent was told.
                let continuation = super::model::is_switch_continuation(text);
                l.set_text(if continuation {
                    CONTINUATION_MARKER
                } else {
                    text
                });
                l.set_tooltip_text(
                    continuation
                        .then_some(SENT_FOR_YOU)
                        .map(|why| format!("{why}\n\n{}", crate::chat::SWITCH_CONTINUATION))
                        .as_deref(),
                );
                for (widget, class) in [
                    (root.upcast_ref::<gtk4::Widget>(), "switch-continuation"),
                    (l.upcast_ref(), "continuation-marker"),
                ] {
                    if continuation {
                        widget.add_css_class(class);
                    } else {
                        widget.remove_css_class(class);
                    }
                }
                if continuation {
                    l.remove_css_class("user-bubble");
                } else {
                    l.add_css_class("user-bubble");
                }
                root.set_halign(if continuation {
                    gtk4::Align::Center
                } else {
                    gtk4::Align::End
                });
                if *queued {
                    root.add_css_class("queued");
                } else {
                    root.remove_css_class("queued");
                }
            }
            (
                Self::Assistant { md, caret, .. },
                Body::Assistant {
                    text, streaming, ..
                },
            ) => {
                md.set_markdown(text);
                caret.set_visible(*streaming);
            }
            (
                Self::Reasoning(r),
                Body::Reasoning {
                    text,
                    streaming,
                    state,
                    label,
                },
            ) => {
                let view = super::model::reasoning_view(text, *streaming, *state, *label);
                r.update(text, &view, item.expanded);
            }
            (
                Self::Switch { words, .. },
                Body::Switch {
                    driver,
                    model,
                    agent_changed,
                    returned,
                },
            ) => words.set_text(&switch_text(
                *driver,
                model.as_deref(),
                *agent_changed,
                *returned,
            )),
            (Self::Tool(t), Body::Tool(_)) => t.update(item),
            (Self::Approval(a), Body::Approval(_)) => a.update(item),
            (Self::Question(q), Body::Question(card)) => q.update(card.state),
            (Self::Form(f), Body::Form(card)) => f.update(card),
            (
                Self::Plan {
                    root,
                    steps: list,
                    updating,
                },
                Body::Plan { steps, outcome },
            ) => {
                use agent_core::event::{StepStatus, TurnState};
                let complete =
                    !steps.is_empty() && steps.iter().all(|s| s.status == StepStatus::Completed);
                let title = if complete {
                    "Completed plan"
                } else {
                    match outcome {
                        Some(TurnState::Interrupted) => "Plan interrupted",
                        Some(TurnState::Failed) => "Plan failed",
                        Some(TurnState::Completed) => "Plan ended with unfinished steps",
                        None => "Plan",
                    }
                };
                root.set_label(Some(title));
                if root.is_expanded() != item.expanded {
                    updating.set(true);
                    root.set_expanded(item.expanded);
                    updating.set(false);
                }
                root.update_property(&[gtk4::accessible::Property::Label(title)]);
                while let Some(child) = list.first_child() {
                    list.remove(&child);
                }
                for step in steps {
                    let state = match step.status {
                        StepStatus::Pending => "Pending",
                        StepStatus::InProgress => "In progress",
                        StepStatus::Completed => "Completed",
                    };
                    let row = label(&format!("{state}: {}", step.text), &[]);
                    wrapping(&row);
                    row.set_selectable(true);
                    list.append(&row);
                }
            }
            _ => {}
        }
    }
}

pub fn compaction_text(manual: bool, before: u64, after: Option<u64>) -> String {
    let what = if manual {
        "Context compacted"
    } else {
        "Context auto-compacted"
    };
    match after {
        Some(a) => format!("{what} {} → {}", format_tokens(before), format_tokens(a)),
        None => format!("{what} (was {})", format_tokens(before)),
    }
}

/// The divider's text. Back on an agent the thread used before, it says the session is new (from
/// a summary): the earlier session is not resumed, and "Continued in" read as if it were.
/// What the transcript shows in place of [`crate::chat::SWITCH_CONTINUATION`].
pub const CONTINUATION_MARKER: &str = "↻ Continuing the interrupted task after the switch";
const SENT_FOR_YOU: &str = "Sent for you after the switch. The agent was told:";

pub fn switch_text(
    driver: Driver,
    model: Option<&str>,
    agent_changed: bool,
    returned: bool,
) -> String {
    match (agent_changed, model) {
        (true, _) if returned => format!(
            "Back in {} · new session from a summary",
            driver.info().label
        ),
        (true, Some(m)) => format!("Continued in {} · {m}", driver.info().label),
        (true, None) => format!("Continued in {}", driver.info().label),
        (false, Some(m)) => format!("Model switched to {m}"),
        (false, None) => "Model switched".to_owned(),
    }
}

fn notice(text: &str, tone: Tone) -> gtk4::Box {
    let root = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    root.add_css_class("notice-row");
    if tone == Tone::Warning {
        root.add_css_class("warning");
    }
    let icon = gtk4::Image::from_icon_name(match tone {
        Tone::Info => "at-dialog-information-symbolic",
        Tone::Warning => "at-dialog-warning-symbolic",
    });
    icon.set_valign(gtk4::Align::Start);
    icon.add_css_class("notice-icon");
    root.append(&icon);
    let l = label(text, &["notice-text"]);
    wrapping(&l);
    l.set_selectable(true);
    root.append(&l);
    root
}

fn error_row(message: &str) -> gtk4::Box {
    let root = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    root.add_css_class("error-row");
    let icon = gtk4::Image::from_icon_name("at-dialog-error-symbolic");
    icon.set_valign(gtk4::Align::Start);
    root.append(&icon);
    let col = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    col.set_hexpand(true);
    col.append(&content_label("Turn failed", &["error-title"]));
    let l = label(message, &["error-text"]);
    wrapping(&l);
    l.set_selectable(true);
    col.append(&l);
    root.append(&col);
    let copy = copy_button("Copy error");
    copy.set_valign(gtk4::Align::Start);
    let message = message.to_owned();
    wire_copy(&copy, move || message.clone());
    root.append(&copy);
    root
}

/// A centred label between two rules.
fn divider(text: &str, class: &str, driver: Option<Driver>) -> gtk4::Widget {
    divider_parts(text, class, driver).0
}

/// [`divider`] and its words, for a divider whose text can change (a switch named later).
fn divider_parts(text: &str, class: &str, driver: Option<Driver>) -> (gtk4::Widget, gtk4::Label) {
    let root = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    root.add_css_class("divider");
    root.add_css_class(class);
    if let Some(d) = driver {
        root.add_css_class(accent_class(d));
    }
    let left = gtk4::Separator::new(gtk4::Orientation::Horizontal);
    left.set_hexpand(true);
    left.set_valign(gtk4::Align::Center);
    let right = gtk4::Separator::new(gtk4::Orientation::Horizontal);
    right.set_hexpand(true);
    right.set_valign(gtk4::Align::Center);
    let pill = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    pill.add_css_class("divider-pill");
    if let Some(d) = driver {
        pill.append(&brand_image(d));
    }
    // Wraps: a long model name must not set the transcript's minimum width.
    let words = content_label(text, &["divider-text"]);
    wrap_words(&words);
    words.set_justify(gtk4::Justification::Center);
    words.set_xalign(0.5);
    pill.append(&words);
    root.append(&left);
    root.append(&pill);
    root.append(&right);
    (root.upcast(), words)
}

// ---------------------------------------------------------------------------------------------
// Reasoning
// ---------------------------------------------------------------------------------------------

pub struct ReasoningRow {
    root: gtk4::Box,
    toggle: gtk4::Button,
    title: gtk4::Label,
    chevron: gtk4::Image,
    revealer: gtk4::Revealer,
    /// The text, one wrapping label per paragraph (see [`Self::show_text`]).
    body: gtk4::Box,
    paragraphs: RefCell<Vec<gtk4::Label>>,
    /// The markdown source each paragraph label shows, to re-render only what changed.
    sources: RefCell<Vec<String>>,
}

impl ReasoningRow {
    fn new(id: &str, sink: &RowSink) -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        root.add_css_class("reasoning-row");
        let toggle = gtk4::Button::new();
        toggle.add_css_class("flat");
        toggle.add_css_class("reasoning-toggle");
        toggle.set_halign(gtk4::Align::Start);
        let head = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let chevron = gtk4::Image::from_icon_name("at-pan-end-symbolic");
        let title = label("Thinking…", &["reasoning-title"]);
        head.append(&chevron);
        head.append(&title);
        toggle.set_child(Some(&head));
        root.append(&toggle);
        let body = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        body.add_css_class("reasoning-text");
        let revealer = gtk4::Revealer::new();
        revealer.set_transition_type(gtk4::RevealerTransitionType::SlideDown);
        revealer.set_child(Some(&body));
        measure_only_when_open(&revealer);
        root.append(&revealer);

        let id = id.to_owned();
        let sink = sink.clone();
        toggle.connect_clicked(glib::clone!(
            #[weak]
            revealer,
            move |_| {
                sink(RowEvent::Toggle {
                    id: id.clone(),
                    expanded: !revealer.reveals_child(),
                });
            }
        ));
        Self {
            root,
            toggle,
            title,
            chevron,
            revealer,
            body,
            paragraphs: RefCell::new(Vec::new()),
            sources: RefCell::new(Vec::new()),
        }
    }

    /// Puts `text` in the paragraph labels, touching only those whose text changed.
    ///
    /// One label for the whole block re-laid out every character of it on each frame while it
    /// streamed: its size request alone took 19 ms per frame at 6k characters and 106 ms at 30k
    /// (release build), the stutter while thinking. A streamed delta now changes only the last
    /// paragraph, so a frame costs what that paragraph does (under 1 ms at any length).
    /// Measured by `tests::stream_cost_of_a_long_thinking_block` in `view.rs`.
    ///
    /// Ceiling: a single paragraph with no blank line in it still costs its whole length per
    /// frame, and a selection cannot span two paragraphs. Upgrade path: an append-only
    /// `TextView`, which lays out only the lines that changed.
    fn show_text(&self, text: &str) {
        let parts: Vec<&str> = if text.is_empty() {
            Vec::new()
        } else {
            text.split("\n\n").map(str::trim).collect()
        };
        let mut labels = self.paragraphs.borrow_mut();
        while labels.len() > parts.len() {
            if let Some(l) = labels.pop() {
                self.body.remove(&l);
                self.sources.borrow_mut().truncate(labels.len());
            }
        }
        while labels.len() < parts.len() {
            let l = label("", &["reasoning-paragraph"]);
            wrapping(&l);
            l.set_selectable(true);
            self.body.append(&l);
            labels.push(l);
        }
        // Each paragraph is markdown (Codex summaries bold their headings), rendered by the
        // transcript's own renderer, which escapes every piece of source text. Only a paragraph
        // whose source changed is parsed again.
        let mut sources = self.sources.borrow_mut();
        sources.resize(labels.len(), String::new());
        for ((l, source), part) in labels.iter().zip(sources.iter_mut()).zip(parts) {
            if source != part {
                part.clone_into(source);
                let markup = super::markdown::passage_markup(part);
                if gtk4::pango::parse_markup(&markup, '\0').is_ok() {
                    l.set_markup(&markup);
                } else {
                    l.set_text(part);
                }
            }
        }
    }

    /// One widget, four states ([`super::model::reasoning_view`]): streaming, complete, replayed
    /// from history, and not shared by the model (a dim, closed line with nothing to open).
    fn update(&self, text: &str, view: &super::model::ReasoningView, expanded: bool) {
        use agent_core::event::ReasoningState;
        let text = text.trim();
        // A block with no words yet takes no space at all, so it cannot make the transcript
        // jump; the header's "Working…" already says the agent is busy.
        self.root.set_visible(view.visible);
        self.title.set_text(view.title);
        self.toggle.set_tooltip_text(view.hint);
        self.toggle.set_sensitive(view.expandable);
        for (state, class) in [
            (ReasoningState::Streaming, "reasoning-streaming"),
            (ReasoningState::Replayed, "reasoning-replayed"),
            (ReasoningState::Unavailable, "reasoning-unavailable"),
        ] {
            if view.state == state {
                self.root.add_css_class(class);
            } else {
                self.root.remove_css_class(class);
            }
        }
        let expanded = expanded && view.expandable;
        self.chevron.set_visible(view.expandable);
        // A collapsed card's text is not drawn, but a label in a closed revealer is still
        // measured whenever it changes: streaming into it cost as much as into an open one. Its
        // text is put in only while open (opening it updates the row, with all of it).
        if expanded {
            self.show_text(text);
        }
        self.revealer.set_reveal_child(expanded);
        self.chevron.set_icon_name(Some(if expanded {
            "at-pan-down-symbolic"
        } else {
            "at-pan-end-symbolic"
        }));
    }

    /// The card's title and whether it shows at all (tests).
    #[cfg(test)]
    pub(super) fn reasoning_state(&self) -> (String, bool) {
        (self.title.text().to_string(), self.root.is_visible())
    }

    /// The paragraph labels' texts (tests).
    #[cfg(test)]
    pub(super) fn reasoning_paragraphs(&self) -> Vec<String> {
        self.paragraphs
            .borrow()
            .iter()
            .map(|l| l.text().to_string())
            .collect()
    }

    /// The paragraph labels themselves, to tell a kept label from a replaced one (tests).
    #[cfg(test)]
    pub(super) fn reasoning_labels(&self) -> Vec<gtk4::Label> {
        self.paragraphs.borrow().clone()
    }
}

// ---------------------------------------------------------------------------------------------
// Tool card
// ---------------------------------------------------------------------------------------------

pub struct ToolCard {
    root: gtk4::Box,
    header: gtk4::Button,
    is_edit: Rc<Cell<bool>>,
    details_guard: Rc<Cell<bool>>,
    status: gtk4::Stack,
    spinner: gtk4::Spinner,
    status_icon: gtk4::Image,
    kind_icon: gtk4::Image,
    title: gtk4::Label,
    summary: gtk4::Label,
    badge: gtk4::Label,
    chevron: gtk4::Image,
    revealer: gtk4::Revealer,
    input_box: Section,
    output_box: Section,
    /// Kept for the tests that read its selection.
    #[cfg_attr(not(test), allow(dead_code))]
    error: gtk4::Label,
    error_box: Section,
    children: gtk4::Box,
    last_status: Cell<Option<ToolStatus>>,
    /// File-change cards only: "View diff" and "Open in ‹tool›", and the diff they reveal.
    diff: FileDiffUi,
}

/// The diff part of a file-change card.
struct FileDiffUi {
    bar: gtk4::Box,
    toggle: gtk4::ToggleButton,
    details: gtk4::ToggleButton,
    // Held so the widgets live as long as the card; read by the GTK checks.
    #[cfg_attr(not(test), allow(dead_code))]
    open: gtk4::Button,
    #[cfg_attr(not(test), allow(dead_code))]
    open_label: gtk4::Label,
    revealer: gtk4::Revealer,
    status: gtk4::Label,
    text: DiffText,
}

/// Shows or hides "Open in ‹tool›" and words the "View diff" tooltip to match the configured
/// tool. Cheap; run on build and whenever the tool changes.
fn refresh_diff_tool(open: &gtk4::Button, label: &gtk4::Label, toggle: &gtk4::ToggleButton) {
    let tool = DiffTools::shared().get();
    open.set_visible(tool.is_some());
    match &tool {
        Some(t) => {
            label.set_text(&diff_tool::open_label(t));
            open.set_tooltip_text(Some(&format!("Open this file in {}", t.name)));
            toggle.set_tooltip_text(Some("Show this file's diff"));
        }
        None => {
            // The button is hidden, so the hint lives where the user is looking.
            toggle.set_tooltip_text(Some(&format!(
                "Show this file's diff. {} to open it in an external viewer.",
                diff_tool::NO_TOOL_HINT
            )));
        }
    }
}

fn kind_icon(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Command => "at-utilities-terminal-symbolic",
        ItemKind::FileChange => "at-document-edit-symbolic",
        ItemKind::FileRead => "at-x-office-document-symbolic",
        ItemKind::McpTool => crate::icons::MCP_ICON,
        ItemKind::WebSearch => "at-system-search-symbolic",
        ItemKind::Subagent => crate::icons::SUBAGENT_ICON,
        ItemKind::Reasoning => crate::icons::THINKING_ICON,
        _ => "at-applications-engineering-symbolic",
    }
}

fn status_class(s: ToolStatus) -> &'static str {
    match s {
        ToolStatus::Running => "status-running",
        ToolStatus::Completed => "status-completed",
        ToolStatus::Failed => "status-failed",
        ToolStatus::Declined => "status-declined",
        ToolStatus::Interrupted => "status-interrupted",
    }
}

/// Lines of tool input/output shown in a card.
const MAX_CARD_LINES: usize = 40;

/// A titled block of tool text (input, output, error) with a Copy button that copies all of it,
/// and "Show full" when the card shows only the last [`MAX_CARD_LINES`] lines.
struct Section {
    root: gtk4::Box,
    body: gtk4::Label,
    full: Rc<RefCell<String>>,
    expanded: Rc<Cell<bool>>,
    more: gtk4::Button,
}

impl Section {
    fn new(title: &str, body: &gtk4::Label) -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        root.add_css_class("card-section");
        let head = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        let name = label(title, &["section-title"]);
        name.set_hexpand(true);
        head.append(&name);
        let more = gtk4::Button::with_label("Show full");
        more.add_css_class("flat");
        more.add_css_class("section-more");
        more.set_visible(false);
        head.append(&more);
        let copy = copy_button(&format!("Copy {}", title.to_lowercase()));
        head.append(&copy);
        root.append(&head);
        // No nested scroller: its minimum height padded one-line outputs to three lines, and a
        // scroller inside the transcript steals wheel events. Long text is cut by lines instead
        // (`payload::cap_lines`).
        let frame = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        frame.add_css_class("mono-scroller");
        frame.append(body);
        root.append(&frame);

        let full = Rc::new(RefCell::new(String::new()));
        let expanded = Rc::new(Cell::new(false));
        wire_copy(&copy, {
            let full = full.clone();
            move || full.borrow().clone()
        });
        let section = Self {
            root,
            body: body.clone(),
            full,
            expanded,
            more,
        };
        section.more.connect_clicked({
            let (body, full, expanded) = (
                section.body.downgrade(),
                section.full.clone(),
                section.expanded.clone(),
            );
            move |more| {
                let Some(body) = body.upgrade() else { return };
                expanded.set(!expanded.get());
                Self::show(&body, more, &full.borrow(), expanded.get());
            }
        });
        section
    }

    /// Shows `text` (all of it to Copy), cut to the last lines unless the person asked for it all.
    fn set_text(&self, text: &str) {
        text.clone_into(&mut self.full.borrow_mut());
        Self::show(&self.body, &self.more, text, self.expanded.get());
    }

    fn show(body: &gtk4::Label, more: &gtk4::Button, text: &str, expanded: bool) {
        let capped = text.lines().count() > MAX_CARD_LINES;
        more.set_visible(capped);
        more.set_label(if expanded { "Show less" } else { "Show full" });
        body.set_text(&if expanded {
            text.to_owned()
        } else {
            payload::cap_lines(text, MAX_CARD_LINES)
        });
    }

    fn set_visible(&self, visible: bool) {
        self.root.set_visible(visible);
    }

    fn widget(&self) -> &gtk4::Box {
        &self.root
    }
}

impl ToolCard {
    fn new(id: &str, sink: &RowSink) -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        root.add_css_class("tool-card");
        root.add_css_class("card");

        let header = gtk4::Button::new();
        header.add_css_class("flat");
        header.add_css_class("card-header");
        let head = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
        let status = gtk4::Stack::new();
        // A finished step's icon fades in over its spinner rather than flicking.
        status.set_transition_type(gtk4::StackTransitionType::Crossfade);
        status.set_transition_duration(crate::brand::FAST_MS);
        let spinner = gtk4::Spinner::new();
        let status_icon = gtk4::Image::new();
        status_icon.add_css_class("status-icon");
        status.add_named(&spinner, Some("spinner"));
        status.add_named(&status_icon, Some("icon"));
        let kind_icon = gtk4::Image::new();
        kind_icon.add_css_class("kind-icon");
        let title = label("", &["card-title"]);
        // Codex command titles contain the whole command. An unconstrained title widens
        // the viewport's content, placing right-aligned user bubbles outside the window.
        title.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        title.set_max_width_chars(32);
        let summary = label("", &["card-summary"]);
        summary.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
        summary.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        summary.set_hexpand(true);
        let badge = label("", &["status-badge"]);
        let chevron = gtk4::Image::from_icon_name("at-pan-end-symbolic");
        chevron.add_css_class("chevron");
        head.append(&status);
        head.append(&kind_icon);
        head.append(&title);
        head.append(&summary);
        head.append(&badge);
        head.append(&chevron);
        header.set_child(Some(&head));
        root.append(&header);

        // The diff bar sits under the header so it is there while the card is collapsed.
        let diff = Self::build_diff_ui(id, sink);
        root.append(&diff.bar);
        root.append(&diff.revealer);
        header.update_relation(&[gtk4::accessible::Relation::Controls(&[diff
            .revealer
            .upcast_ref()])]);
        let is_edit = Rc::new(Cell::new(false));
        let details_guard = Rc::new(Cell::new(false));

        let body = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
        body.add_css_class("card-body");
        let input_box = Section::new("Input", &mono(""));
        let output_box = Section::new("Output", &mono(""));
        let error = label("", &["card-error"]);
        wrapping(&error);
        error.set_selectable(true);
        let error_box = Section::new("Error", &error);
        let children = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        children.add_css_class("card-children");
        body.append(input_box.widget());
        body.append(&children);
        body.append(output_box.widget());
        body.append(error_box.widget());
        let revealer = gtk4::Revealer::new();
        revealer.set_transition_type(gtk4::RevealerTransitionType::SlideDown);
        revealer.set_child(Some(&body));
        measure_only_when_open(&revealer);
        root.append(&revealer);

        let id = id.to_owned();
        let sink = sink.clone();
        let (detail_id, detail_sink) = (id.to_owned(), sink.clone());
        let edit = is_edit.clone();
        let diff_toggle = diff.toggle.clone();
        header.connect_clicked(glib::clone!(
            #[weak]
            revealer,
            move |_| {
                if edit.get() {
                    diff_toggle.set_active(!diff_toggle.is_active());
                    return;
                }
                sink(RowEvent::Toggle {
                    id: id.clone(),
                    expanded: !revealer.reveals_child(),
                });
            }
        ));
        let guard = details_guard.clone();
        diff.details.connect_toggled(move |button| {
            if guard.get() {
                return;
            }
            detail_sink(RowEvent::Toggle {
                id: detail_id.clone(),
                expanded: button.is_active(),
            });
        });
        diff.toggle.connect_toggled(glib::clone!(
            #[weak]
            header,
            #[weak]
            chevron,
            move |toggle| {
                let open = toggle.is_active();
                header.update_state(&[gtk4::accessible::State::Expanded(Some(open))]);
                chevron.set_icon_name(Some(if open {
                    "at-pan-down-symbolic"
                } else {
                    "at-pan-end-symbolic"
                }));
            }
        ));
        Self {
            root,
            header,
            is_edit,
            details_guard,
            status,
            spinner,
            status_icon,
            kind_icon,
            title,
            summary,
            badge,
            chevron,
            revealer,
            input_box,
            output_box,
            error,
            error_box,
            children,
            last_status: Cell::new(None),
            diff,
        }
    }

    fn build_diff_ui(id: &str, sink: &RowSink) -> FileDiffUi {
        let bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        bar.add_css_class("diff-bar");
        bar.set_visible(false);
        // The file header is the primary changes disclosure. This toggle holds its state and
        // loading signal; raw protocol details are a separate, explicitly named action.
        let toggle = gtk4::ToggleButton::with_label("Show changes");
        toggle.add_css_class("flat");
        toggle.add_css_class("diff-toggle");
        let open = gtk4::Button::new();
        open.add_css_class("flat");
        open.add_css_class("diff-open");
        let open_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        open_row.append(&gtk4::Image::from_icon_name(crate::icons::EXTERNAL_ICON));
        open.update_property(&[gtk4::accessible::Property::Label(
            "Open the changes in the external diff tool",
        )]);
        let open_label = label("", &[]);
        open_row.append(&open_label);
        open.set_child(Some(&open_row));
        let details = gtk4::ToggleButton::with_label("Raw details");
        details.add_css_class("flat");
        details.set_visible(false);
        bar.append(&details);
        bar.append(&open);

        let status = label("", &["diff-status", "dim-label"]);
        let text = DiffText::new();
        text.widget().set_visible(false);
        let column = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        column.add_css_class("diff-section");
        column.append(&status);
        column.append(text.widget());
        let revealer = gtk4::Revealer::new();
        revealer.set_transition_type(gtk4::RevealerTransitionType::SlideDown);
        revealer.set_child(Some(&column));
        measure_only_when_open(&revealer);

        refresh_diff_tool(&open, &open_label, &toggle);
        let conn = DiffTools::shared().connect_changed(glib::clone!(
            #[weak]
            open,
            #[weak]
            open_label,
            #[weak]
            toggle,
            move || refresh_diff_tool(&open, &open_label, &toggle)
        ));
        bar.connect_destroy(move |_| DiffTools::shared().disconnect(conn));

        let (item, load_sink) = (id.to_owned(), sink.clone());
        toggle.connect_toggled(glib::clone!(
            #[weak]
            revealer,
            #[weak]
            status,
            move |toggle| {
                revealer.set_reveal_child(toggle.is_active());
                if toggle.is_active() {
                    status.set_text("Loading diff…");
                    status.set_visible(true);
                    load_sink(RowEvent::LoadDiff { id: item.clone() });
                }
            }
        ));
        let (item, open_sink) = (id.to_owned(), sink.clone());
        open.connect_clicked(move |_| open_sink(RowEvent::OpenDiff { id: item.clone() }));

        FileDiffUi {
            bar,
            toggle,
            details,
            open,
            open_label,
            revealer,
            status,
            text,
        }
    }

    /// Shows the diff the host computed for this card.
    pub(crate) fn set_changes_expanded(&self, expanded: bool) {
        self.diff.toggle.set_active(expanded);
    }

    pub fn show_diff(&self, reply: &DiffReply) {
        let ui = &self.diff;
        if !ui.toggle.is_active() {
            return; // closed again before the answer arrived
        }
        match reply {
            Ok(shown) if shown.text.trim().is_empty() && !shown.clipped => {
                ui.status.set_text("No changes");
                ui.status.set_visible(true);
                ui.text.widget().set_visible(false);
            }
            Ok(shown) => {
                ui.text.set(
                    &shown.text,
                    &difftext::notes_for(shown.origin, shown.omitted_lines, shown.clipped),
                );
                ui.status.set_visible(false);
                ui.text.widget().set_visible(true);
            }
            Err(why) => {
                ui.status.set_text(why);
                ui.status.set_visible(true);
                ui.text.widget().set_visible(false);
            }
        }
    }

    /// The diff section's state (tests): toggle on, status text, diff text.
    #[cfg(test)]
    pub(super) fn diff_state(&self) -> (bool, String, String, String, Option<String>) {
        (
            self.diff.toggle.is_active(),
            self.diff.status.text().to_string(),
            self.diff.text.text(),
            self.diff.text.notes(),
            self.diff.text.language(),
        )
    }

    /// Whether the card offers a diff at all, and whether it offers "Open in" (tests).
    #[cfg(test)]
    pub(super) fn diff_buttons(&self) -> (bool, bool, String) {
        (
            self.diff.bar.is_visible(),
            self.diff.open.is_visible(),
            self.diff.open_label.text().to_string(),
        )
    }

    /// Clicks "View diff" (tests).
    /// Whether the card shows its spinner (tests).
    #[cfg(test)]
    pub(super) fn spinning(&self) -> bool {
        self.status.visible_child_name().as_deref() == Some("spinner")
    }

    #[cfg(test)]
    pub(super) fn click_view_diff(&self) {
        self.diff.toggle.set_active(true);
    }

    fn update(&self, item: &Item) {
        let Body::Tool(tool) = &item.body else {
            return;
        };
        let is_edit = tool.kind == ItemKind::FileChange;
        self.is_edit.set(is_edit);
        self.diff.bar.set_visible(is_edit);
        if !is_edit {
            self.diff.revealer.set_reveal_child(false);
        }
        if self.last_status.get() != Some(tool.status) {
            let prev = self.last_status.get();
            if let Some(prev) = prev {
                self.root.remove_css_class(status_class(prev));
            }
            self.root.add_css_class(status_class(tool.status));
            self.last_status.set(Some(tool.status));
            // Live edits only: a card replayed from history is built already completed, so
            // reopening a thread never loads a diff for every old edit.
            if is_edit
                && prev == Some(ToolStatus::Running)
                && tool.status == ToolStatus::Completed
                && DiffTools::shared().expand_by_default()
            {
                self.diff.toggle.set_active(true);
            }
        }
        let running = tool.status == ToolStatus::Running;
        self.spinner.set_spinning(running);
        self.status
            .set_visible_child_name(if running { "spinner" } else { "icon" });
        self.status_icon.set_icon_name(Some(match tool.status {
            ToolStatus::Running | ToolStatus::Completed => "at-object-select-symbolic",
            ToolStatus::Failed => "at-dialog-error-symbolic",
            ToolStatus::Declined => "at-action-unavailable-symbolic",
            ToolStatus::Interrupted => "at-media-playback-stop-symbolic",
        }));
        self.badge.set_text(match tool.status {
            ToolStatus::Failed => "Failed",
            ToolStatus::Declined => "Declined",
            ToolStatus::Interrupted => "Interrupted",
            ToolStatus::Running | ToolStatus::Completed => "",
        });
        self.badge.set_visible(!self.badge.text().is_empty());
        self.kind_icon.set_icon_name(Some(kind_icon(tool.kind)));
        let (action_title, target_summary) = payload::card_title_and_summary(
            tool.kind,
            &tool.title,
            tool.input.as_ref(),
            &tool.input_text,
        );
        self.title.set_text(&action_title);
        self.title
            .set_tooltip_text(Some(&format!("{action_title}: {target_summary}")));
        let mut paths = tool
            .input
            .as_ref()
            .map(agent_kit::editdiff::paths_from_input)
            .unwrap_or_default();
        if paths.is_empty() && !tool.input_text.is_empty() {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&tool.input_text) {
                paths = agent_kit::editdiff::paths_from_input(&val);
            }
        }
        let files = if paths.is_empty() {
            if !target_summary.is_empty() {
                target_summary.clone()
            } else {
                tool.title.clone()
            }
        } else {
            paths.join(", ")
        };
        self.summary
            .set_text(if is_edit { &files } else { &target_summary });
        self.summary
            .set_tooltip_text(Some(if is_edit { &files } else { &target_summary }));
        self.summary.set_wrap(is_edit);
        self.summary.set_ellipsize(if is_edit {
            gtk4::pango::EllipsizeMode::None
        } else {
            gtk4::pango::EllipsizeMode::End
        });
        self.header
            .update_property(&[gtk4::accessible::Property::Label(&if is_edit {
                format!("{action_title} {files}")
            } else {
                format!("{action_title} {target_summary}")
            })]);
        let input = payload::tool_input_text(tool.kind, tool.input.as_ref(), &tool.input_text);
        self.input_box.set_visible(!input.trim().is_empty());
        self.input_box.set_text(input.trim_end());
        self.output_box.set_visible(!tool.output.trim().is_empty());
        self.output_box.set_text(tool.output.trim_end());
        match &tool.error {
            Some(e) if !e.trim().is_empty() => {
                self.error_box.set_text(e.trim());
                self.error_box.set_visible(true);
            }
            _ => self.error_box.set_visible(false),
        }
        self.children.set_visible(!item.children.is_empty());
        let useful_details = !input.trim().is_empty()
            || !tool.output.trim().is_empty()
            || tool.error.as_ref().is_some_and(|e| !e.trim().is_empty())
            || !item.children.is_empty();
        self.diff.details.set_visible(is_edit && useful_details);
        self.header.set_sensitive(is_edit || useful_details);
        self.chevron.set_visible(is_edit || useful_details);
        self.diff
            .details
            .update_property(&[gtk4::accessible::Property::Label(&format!(
                "Raw details for {files}"
            ))]);
        self.details_guard.set(true);
        self.diff.details.set_active(item.expanded);
        self.details_guard.set(false);
        self.diff
            .details
            .update_state(&[gtk4::accessible::State::Expanded(Some(item.expanded))]);
        self.diff
            .details
            .update_relation(&[gtk4::accessible::Relation::Controls(&[self
                .revealer
                .upcast_ref()])]);
        self.header
            .update_relation(&[gtk4::accessible::Relation::Controls(&[if is_edit {
                self.diff.revealer.upcast_ref()
            } else {
                self.revealer.upcast_ref()
            }])]);
        self.revealer
            .set_reveal_child(item.expanded && useful_details);
        let expanded = if is_edit {
            self.diff.toggle.is_active()
        } else {
            item.expanded
        };
        self.header
            .update_state(&[gtk4::accessible::State::Expanded(Some(expanded))]);
        self.chevron.set_icon_name(Some(if expanded {
            "at-pan-down-symbolic"
        } else {
            "at-pan-end-symbolic"
        }));
    }
}

// ---------------------------------------------------------------------------------------------
// Approval card
// ---------------------------------------------------------------------------------------------

pub struct ApprovalCard {
    root: gtk4::Box,
    title: gtk4::Label,
    reason: gtk4::Label,
    /// What is asked, as text, which the section also copies and can show whole.
    input_section: Section,
    /// A file-edit request's proposed change, as a diff (instead of the tool-input JSON).
    diff: DiffText,
    /// A plan to approve (Claude's ExitPlanMode), rendered as markdown.
    plan: MarkdownView,
    /// The request's JSON, capped, one click away.
    raw: gtk4::Expander,
    raw_section: Section,
    /// What "Always allow" would save, when it is offered.
    remembers: gtk4::Label,
    /// A permission request's places, grouped, one per line (hidden for every other request).
    places: Rc<Places>,
    /// A git or gh command's operation in words (hidden for every other request).
    git: GitSummary,
    /// A permission request's warning, or why it can only be refused.
    warning: gtk4::Label,
    /// What the permission buttons last (dim, under the request).
    note: gtk4::Label,
    buttons: gtk4::FlowBox,
    outcome: gtk4::Label,
    request: RefCell<String>,
    built_buttons: Cell<bool>,
    /// The raw request was opened for the user once already (an unreadable permission request).
    raw_forced: Cell<bool>,
    sink: RowSink,
}

/// A flow box of chips or buttons that wraps onto more lines when its card is narrow, with every
/// child at its own natural width.
///
/// Its `halign` stays Fill, never Start or End. A flow box with another alignment is measured
/// on its own natural width by `gtk_widget_allocate`, and a row not yet given a width then has
/// its children measured at 0 px ("Trying to measure GtkFlowBoxChild ... for width of 0"). Left
/// to fill, the children still pack at the start of each line; for the end, see [`button_row`].
pub(super) fn chip_flow(max_per_line: u32) -> gtk4::FlowBox {
    let flow = gtk4::FlowBox::new();
    flow.set_selection_mode(gtk4::SelectionMode::None);
    flow.set_homogeneous(false);
    flow.set_column_spacing(8);
    flow.set_row_spacing(8);
    flow.set_max_children_per_line(max_per_line);
    flow
}

/// A row of buttons, pushed to the end of its card, that wraps onto more lines when the card is
/// narrow, in the order given. Returns the row to put in the card and the flow box to fill; show
/// or hide them together with [`set_row_visible`].
pub(super) fn button_row() -> (gtk4::Box, gtk4::FlowBox) {
    let flow = chip_flow(8);
    flow.set_min_children_per_line(1);
    flow.set_activate_on_single_click(false);
    // A plain spacer takes the free width, so the buttons sit at the end without the flow box
    // itself being aligned (see `chip_flow`).
    let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    row.append(&spacer);
    row.append(&flow);
    (row, flow)
}

/// A card's footer: `outcome` (wrapping, hidden until there is one) above a [`button_row`].
/// Above the buttons, not beside them: the buttons wrap, and a wrapping label beside a wrapping
/// row is where GTK's height-for-width measuring disagrees with itself. Returns the footer and
/// the flow box to fill.
pub(super) fn outcome_footer(outcome: &gtk4::Label) -> (gtk4::Box, gtk4::FlowBox) {
    let footer = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    wrapping(outcome);
    outcome.set_visible(false);
    let (row, flow) = button_row();
    footer.append(outcome);
    footer.append(&row);
    (footer, flow)
}

/// Shows or hides a [`button_row`]'s flow box with the row around it, which would leave a gap.
pub(super) fn set_row_visible(flow: &gtk4::FlowBox, visible: bool) {
    flow.set_visible(visible);
    if let Some(row) = flow.parent() {
        row.set_visible(visible);
    }
}

/// Adds `button` to a [`button_row`]. The flow box's own wrapper takes no focus, so Tab goes
/// from button to button.
pub(super) fn add_button(flow: &gtk4::FlowBox, button: &impl IsA<gtk4::Widget>) {
    flow.insert(button, -1);
    if let Some(wrapper) = button.parent() {
        wrapper.set_focusable(false);
    }
}

/// The buttons of a [`button_row`], in order (tests).
#[cfg(test)]
pub(super) fn row_buttons(flow: &gtk4::FlowBox) -> Vec<gtk4::Widget> {
    let mut out = Vec::new();
    let mut child = flow.first_child();
    while let Some(wrapper) = child {
        out.extend(wrapper.first_child());
        child = wrapper.next_sibling();
    }
    out
}

/// The places a permission request names, as a capped monospace list with "Show all".
struct Places {
    root: gtk4::Box,
    body: gtk4::Label,
    more: gtk4::Button,
    full: RefCell<String>,
    expanded: Cell<bool>,
}

impl Places {
    fn new() -> Rc<Self> {
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        root.add_css_class("places-section");
        let frame = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        frame.add_css_class("mono-scroller");
        let body = mono("");
        body.update_property(&[gtk4::accessible::Property::Label("Places Codex asks for")]);
        frame.append(&body);
        root.append(&frame);
        let more = gtk4::Button::with_label("Show all");
        more.add_css_class("flat");
        more.add_css_class("section-more");
        more.set_halign(gtk4::Align::Start);
        more.set_visible(false);
        root.append(&more);
        root.set_visible(false);
        let places = Rc::new(Self {
            root,
            body,
            more,
            full: RefCell::new(String::new()),
            expanded: Cell::new(false),
        });
        places.more.connect_clicked({
            let places = Rc::downgrade(&places);
            move |_| {
                if let Some(places) = places.upgrade() {
                    places.expanded.set(!places.expanded.get());
                    places.show();
                }
            }
        });
        places
    }

    fn set(&self, lines: &[String]) {
        *self.full.borrow_mut() = lines.join("\n");
        self.show();
    }

    fn show(&self) {
        let full = self.full.borrow();
        let (head, cut) = head_lines_in(
            &full,
            APPROVAL_VISIBLE_LINES,
            APPROVAL_LINE_CHARS,
            "Show all",
        );
        self.more.set_visible(cut || self.expanded.get());
        self.more.set_label(if self.expanded.get() {
            "Show fewer"
        } else {
            "Show all"
        });
        self.body.set_text(&if self.expanded.get() {
            full.clone()
        } else {
            head
        });
    }
}

pub fn decision_label(d: Decision) -> &'static str {
    match d {
        Decision::Allow => "Allow",
        Decision::AllowForSession => "Allow for session",
        Decision::AllowAlways => "Always allow",
        Decision::Deny => "Deny",
        // Ends the whole turn (Codex `cancel`); Deny lets the agent see the refusal and go on.
        Decision::Cancel => "Stop turn",
    }
}

/// The button for `d` on a card for `tool`. A permission request's Allow lasts the turn, not
/// "once", and its session grant is a button, not a menu item.
fn button_label(tool: &str, d: Decision) -> &'static str {
    match (tool == permission::TOOL, d) {
        (true, Decision::Allow) => "Allow for this turn",
        (true, Decision::AllowForSession) => "Allow for this session",
        _ => decision_label(d),
    }
}

fn outcome_text(tool: &str, d: Decision) -> &'static str {
    match (tool == permission::TOOL, d) {
        (true, Decision::Allow) => "Allowed for this turn",
        (true, Decision::AllowForSession) => "Allowed for this Codex session",
        _ => decision_outcome(d),
    }
}

fn decision_outcome(d: Decision) -> &'static str {
    match d {
        Decision::Allow => "Allowed once",
        Decision::AllowForSession => "Allowed for this session",
        Decision::AllowAlways => "Always allowed here",
        Decision::Deny => "Denied",
        Decision::Cancel => "Turn stopped",
    }
}

/// Lines of an approval's input shown on the card itself; the rest is behind "Show raw".
const APPROVAL_VISIBLE_LINES: usize = 12;
/// Characters of one line shown on the card itself; a longer line is cut, the rest in "Show raw".
const APPROVAL_LINE_CHARS: usize = 400;

/// What an approval card shows of a request that is not a file edit or a plan, and whether that
/// was cut. For a shell command: the command line (a strict `zsh -lc '…'` wrapper unwrapped, as
/// rules see it) and its working directory; otherwise the readable input. Either is cut to
/// [`APPROVAL_VISIBLE_LINES`] lines of at most [`APPROVAL_LINE_CHARS`] characters.
fn approval_input_text(tool: &str, input: &Value) -> (String, bool) {
    if input.is_null() {
        return (String::new(), false);
    }
    if let Some(crate::always_allow::Action::Shell { command }) =
        crate::always_allow::Action::of(tool, input)
    {
        let command = crate::always_allow::unwrap_shell_wrapper(&command).unwrap_or(command);
        let mut shown = serde_json::json!({ "command": command });
        for (from, to) in [
            ("cwd", "cwd"),
            ("Cwd", "cwd"),
            ("description", "description"),
        ] {
            if let Some(v) = input.get(from).filter(|v| v.is_string()) {
                shown[to] = v.clone();
            }
        }
        return head_lines(
            &payload::tool_input_text(ItemKind::Command, Some(&shown), ""),
            APPROVAL_VISIBLE_LINES,
            APPROVAL_LINE_CHARS,
        );
    }
    let kind = if tool.starts_with("mcp__") || tool == "call_mcp_tool" {
        ItemKind::McpTool
    } else {
        ItemKind::Tool
    };
    head_lines(
        &payload::tool_input_text(kind, Some(input), ""),
        APPROVAL_VISIBLE_LINES,
        APPROVAL_LINE_CHARS,
    )
}

/// The first `max` lines of `text`, each cut to `width` characters, with a note of what "Show
/// raw" holds; true when anything was cut.
fn head_lines(text: &str, max: usize, width: usize) -> (String, bool) {
    head_lines_in(text, max, width, "Show raw")
}

/// [`head_lines`], naming the control (`rest_in`) that holds what was cut.
fn head_lines_in(text: &str, max: usize, width: usize, rest_in: &str) -> (String, bool) {
    let total = text.lines().count();
    let keep = if total <= max {
        total
    } else {
        max.saturating_sub(1)
    };
    let mut cut = total > keep;
    let mut lines: Vec<String> = text
        .lines()
        .take(keep)
        .map(|line| {
            let chars = line.chars().count();
            if chars <= width {
                return line.to_owned();
            }
            cut = true;
            let head: String = line.chars().take(width).collect();
            format!("{head}… ({} more characters in {rest_in})", chars - width)
        })
        .collect();
    if total > keep {
        lines.push(format!("… {} more lines in {rest_in}", total - keep));
    }
    (lines.join("\n"), cut)
}

impl ApprovalCard {
    fn new(sink: &RowSink) -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
        root.add_css_class("approval-card");
        root.add_css_class("card");
        let head = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
        let icon = gtk4::Image::from_icon_name("at-security-medium-symbolic");
        icon.add_css_class("approval-icon");
        icon.set_valign(gtk4::Align::Start);
        head.append(&icon);
        let col = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        let title = content_label("", &["approval-title"]);
        wrapping(&title);
        let reason = content_label("", &["approval-reason"]);
        wrapping(&reason);
        col.append(&title);
        col.append(&reason);
        head.append(&col);
        root.append(&head);
        // A warning shown prominently, in words, never by colour alone.
        let warning = content_label("", &["approval-warning"]);
        wrapping(&warning);
        warning.set_visible(false);
        root.append(&warning);
        let places = Places::new();
        root.append(&places.root);
        let git = GitSummary::new();
        root.append(&git.root);
        let input = mono("");
        let input_section = Section::new("Request", &input);
        root.append(input_section.widget());
        let diff = DiffText::new();
        diff.widget().set_visible(false);
        root.append(diff.widget());
        let plan = MarkdownView::new();
        plan.widget().add_css_class("approval-plan");
        plan.widget().set_visible(false);
        root.append(plan.widget());
        let raw_text = mono("");
        let raw_section = Section::new("Raw request", &raw_text);
        let raw = gtk4::Expander::new(Some("Show raw"));
        raw.add_css_class("approval-raw");
        raw.set_child(Some(raw_section.widget()));
        raw.set_visible(false);
        root.append(&raw);
        let remembers = label("", &["approval-remembers", "dim-label"]);
        wrapping(&remembers);
        remembers.set_selectable(true);
        remembers.set_visible(false);
        root.append(&remembers);
        let note = label("", &["approval-note", "dim-label"]);
        wrapping(&note);
        note.set_visible(false);
        root.append(&note);
        let outcome = content_label("", &["approval-outcome"]);
        let (footer, buttons) = outcome_footer(&outcome);
        root.append(&footer);
        Self {
            root,
            title,
            reason,
            input_section,
            diff,
            plan,
            raw,
            raw_section,
            remembers,
            places,
            git,
            warning,
            note,
            buttons,
            outcome,
            request: RefCell::new(String::new()),
            built_buttons: Cell::new(false),
            raw_forced: Cell::new(false),
            sink: sink.clone(),
        }
    }

    /// What the card shows of a file edit (tests): the files line, whether the diff and the "Show
    /// raw" expander are visible, and the diff's text.
    #[cfg(test)]
    pub(super) fn diff_view(&self) -> (String, bool, bool, String) {
        (
            self.input_section.full.borrow().clone(),
            self.diff.widget().is_visible(),
            self.raw.is_visible(),
            self.diff.text(),
        )
    }

    fn update(&self, item: &Item) {
        let Body::Approval(a) = &item.body else {
            return;
        };
        a.request.clone_into(&mut self.request.borrow_mut());
        // A Codex permission request is read once, here: everything below follows it.
        let permission = (a.tool == permission::TOOL).then(|| permission::Request::parse(&a.input));
        // A git or gh command is also said in words, above the command line itself.
        let git = permission
            .is_none()
            .then(|| git_card::reading(&a.tool, &a.input))
            .flatten();
        self.git.set(git.as_ref());
        let title = a
            .title
            .clone()
            .filter(|t| !t.trim().is_empty())
            // A permission request's title is shown only if it is plain text.
            .filter(|t| permission.is_none() || permission::plain(t))
            .or_else(|| permission.as_ref().map(permission::Request::title))
            .or_else(|| git.as_ref().map(|g| format!("Allow {}?", g.tool.name())))
            .unwrap_or_else(|| format!("Allow {}?", a.tool));
        self.title.set_text(&title);
        let reason = a
            .reason
            .clone()
            .or_else(|| permission.as_ref().and_then(|p| p.reason.clone()))
            .filter(|r| !r.trim().is_empty());
        self.reason.set_text(reason.as_deref().unwrap_or(""));
        self.reason.set_visible(reason.is_some());
        let pending = a.state == ApprovalState::Pending;
        match &permission {
            Some(request) => self.show_permission(request, pending, &a.options),
            None => {
                self.places.root.set_visible(false);
                self.warning.set_visible(false);
                self.note.set_visible(false);
            }
        }
        // A file edit is shown as the diff it proposes (computed from the request alone), a plan
        // (Claude's ExitPlanMode) as markdown, a command as its bare command line and working
        // directory, anything else as a few lines of its input. The JSON is one click away.
        let preview = agent_kit::editdiff::preview_from_input(&a.input);
        let plan = (a.tool == "ExitPlanMode")
            .then(|| a.input.get("plan").and_then(Value::as_str))
            .flatten()
            .filter(|p| !p.trim().is_empty());
        let (input, cut) = if let Some(plan) = plan {
            self.plan.set_markdown(plan);
            (String::new(), false)
        } else if permission.is_some() {
            // Said by the places list above, not dumped as JSON.
            (String::new(), false)
        } else if preview.is_empty() {
            approval_input_text(&a.tool, &a.input)
        } else {
            let files = preview
                .iter()
                .map(|e| format!("{}  +{} −{}", e.path, e.added, e.removed))
                .collect::<Vec<_>>()
                .join("\n");
            (files, false)
        };
        self.plan.widget().set_visible(plan.is_some());
        self.input_section.set_visible(!input.is_empty());
        self.input_section.set_text(&input);
        // No "Show raw" only when the card already shows all of it, uncut (a bare command and its
        // folder): whatever is cut is never approved unseen.
        let shown_whole = permission.is_none()
            && !cut
            && a.input.as_object().is_some_and(|o| {
                o.keys().all(|k| {
                    ["command", "CommandLine", "cmd", "cwd", "Cwd", "description"]
                        .contains(&k.as_str())
                })
            })
            && preview.is_empty()
            && plan.is_none()
            && ["command", "CommandLine", "cmd"].iter().all(|k| {
                a.input
                    .get(*k)
                    .and_then(Value::as_str)
                    .is_none_or(|c| crate::always_allow::unwrap_shell_wrapper(c).is_none())
            });
        let raw = match &a.input {
            Value::Null => String::new(),
            _ if shown_whole => String::new(),
            v => serde_json::to_string_pretty(v).unwrap_or_default(),
        };
        // The Raw section caps what it shows (Show full, Copy takes all of it).
        self.raw_section.set_text(&raw);
        self.raw.set_visible(!raw.is_empty());
        // What cannot be summarised is shown as sent, open: the user can only refuse it, and
        // refuses (or stops) knowing what it said.
        if pending
            && permission.as_ref().is_some_and(|p| !p.renderable)
            && !self.raw_forced.replace(true)
        {
            self.raw.set_expanded(true);
        }
        if preview.is_empty() || plan.is_some() {
            self.diff.widget().set_visible(false);
        } else {
            let text: String = preview.iter().map(|e| e.diff.as_str()).collect();
            let (kept, omitted) = agent_kit::diff::truncate(
                &text,
                agent_kit::diff::MAX_DIFF_BYTES,
                agent_kit::diff::MAX_DIFF_LINES,
            );
            self.diff.set(
                kept,
                &difftext::notes_for(
                    agent_kit::filediff::Origin::AgentEdit,
                    omitted,
                    preview.iter().any(|e| e.clipped),
                )
                .into_iter()
                .map(|n| n.replace("from the agent's edit", "Proposed change"))
                .collect::<Vec<_>>(),
            );
            self.diff.widget().set_visible(true);
        }

        if !self.built_buttons.get() {
            self.built_buttons.set(true);
            // Deny first, the safest default on the right.
            let mut order = a.options.clone();
            order.sort_by_key(|d| match d {
                Decision::Cancel => 0,
                Decision::Deny => 1,
                Decision::AllowAlways => 2,
                Decision::AllowForSession => 3,
                Decision::Allow => 4,
            });
            // A permission request offers only what its card can honestly show: nothing but a
            // refusal when it cannot be read, and no session grant when it may not last one.
            // Its grants are plain buttons: there is no rule to save, so no menu and no dialog.
            if let Some(request) = &permission {
                order.retain(|d| match d {
                    Decision::Cancel | Decision::Deny => true,
                    Decision::Allow => request.renderable,
                    // A broad grant (anything warned about) is for this turn only.
                    Decision::AllowForSession => {
                        request.renderable
                            && request.session_allowed
                            && request.warnings().is_empty()
                    }
                    Decision::AllowAlways => false,
                });
            }
            let rule_menu = permission.is_none();
            // Allow is not the suggested button for a request warned about: a permission
            // request's broad grant, or a git command that can lose work or rewrite history.
            let calm = permission.as_ref().is_none_or(|p| p.allow_is_calm())
                && git.as_ref().is_none_or(|g| !g.destructive());
            let extra = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
            extra.set_margin_top(6);
            extra.set_margin_bottom(6);
            extra.set_margin_start(6);
            extra.set_margin_end(6);
            let more = gtk4::MenuButton::new();
            more.set_label("More");
            more.set_tooltip_text(Some("More permission options"));
            more.update_property(&[gtk4::accessible::Property::Label("More permission options")]);
            more.add_css_class("pill");
            let popover = gtk4::Popover::new();
            popover.set_child(Some(&extra));
            more.set_popover(Some(&popover));
            let mut added_more = false;
            for d in order {
                let b = gtk4::Button::with_label(button_label(&a.tool, d));
                b.add_css_class("pill");
                match d {
                    Decision::Allow if calm => b.add_css_class("suggested-action"),
                    Decision::Deny => b.add_css_class("destructive-action"),
                    _ => {}
                }
                let sink = self.sink.clone();
                let request = a.request.clone();
                let popover = popover.clone();
                if rule_menu && matches!(d, Decision::AllowAlways | Decision::AllowForSession) {
                    let tool = a.tool.clone();
                    let input_val = a.input.clone();
                    let initial_cmd = crate::always_allow::dialog_prefill(&tool, &input_val);
                    let cwd_str = input_val
                        .get("Cwd")
                        .or_else(|| input_val.get("cwd"))
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let b_clone = b.clone();
                    b.connect_clicked(move |_| {
                        popover.popdown();
                        let confirm =
                            !cfg!(test) && crate::always_allow::confirm_rule_modification();
                        if confirm && !initial_cmd.is_empty() {
                            confirm_approval_rule(
                                &b_clone,
                                request.clone(),
                                tool.clone(),
                                initial_cmd.clone(),
                                cwd_str.clone(),
                                sink.clone(),
                                d,
                            );
                        } else {
                            sink(RowEvent::Approve {
                                request: request.clone(),
                                decision: d,
                                custom_rule: None,
                            });
                        }
                    });
                } else {
                    b.connect_clicked(move |_| {
                        popover.popdown();
                        sink(RowEvent::Approve {
                            request: request.clone(),
                            decision: d,
                            custom_rule: None,
                        });
                    });
                }
                if rule_menu && matches!(d, Decision::AllowAlways | Decision::AllowForSession) {
                    if !added_more {
                        add_button(&self.buttons, &more);
                        added_more = true;
                    }
                    extra.append(&b);
                } else {
                    add_button(&self.buttons, &b);
                }
            }
        }

        for class in ["pending", "resolved", "expired", "denied"] {
            self.root.remove_css_class(class);
        }
        // A refusal is never shown in the "allowed" green.
        if matches!(
            a.state,
            ApprovalState::Resolved(Decision::Deny | Decision::Cancel)
        ) {
            self.root.add_css_class("denied");
        }
        let (class, outcome, sensitive, show_buttons) = match a.state {
            ApprovalState::Pending => ("pending", String::new(), true, true),
            ApprovalState::Sent(d) => (
                "pending",
                format!("{}…", button_label(&a.tool, d)),
                false,
                true,
            ),
            ApprovalState::Resolved(d) => (
                "resolved",
                outcome_text(&a.tool, d).to_owned(),
                false,
                false,
            ),
            ApprovalState::Expired => (
                "expired",
                "Expired: the agent that asked has exited".to_owned(),
                false,
                false,
            ),
        };
        self.root.add_css_class(class);
        self.outcome.set_text(&outcome);
        self.outcome.set_visible(!outcome.is_empty());
        self.buttons.set_sensitive(sensitive);
        set_row_visible(&self.buttons, show_buttons);
        // While the choice is open, say what the remembering buttons keep: the gate wrote it
        // (`approval_server::card_note`), judged where the call runs.
        let remembering = a.options.contains(&Decision::AllowForSession)
            || a.options.contains(&Decision::AllowAlways);
        match a
            .remembers
            .as_deref()
            .filter(|_| a.state == ApprovalState::Pending && remembering)
        {
            Some(text) => {
                self.remembers.set_text(text);
                self.remembers.set_visible(true);
            }
            None => self.remembers.set_visible(false),
        }
    }

    /// A Codex permission request: the places it names, its warning (or why it can only be
    /// refused), and what each grant lasts.
    fn show_permission(&self, request: &permission::Request, pending: bool, options: &[Decision]) {
        self.places.root.set_visible(request.renderable);
        if request.renderable {
            self.places.set(&request.lines());
        }
        let warning = if request.renderable {
            request.warnings().join("\n")
        } else {
            "Some of this request could not be summarised, so it can only be denied. It is \
             shown below exactly as Codex sent it."
                .to_owned()
        };
        self.warning.set_text(&warning);
        self.warning.set_visible(!warning.is_empty());
        let session = options.contains(&Decision::AllowForSession)
            && request.session_allowed
            && request.warnings().is_empty();
        let note = match (request.renderable, session) {
            (false, _) => String::new(),
            (true, true) => "“Allow for this turn” lasts until the turn ends. “Allow for this \
                             session” lasts until this thread's Codex session restarts."
                .to_owned(),
            (true, false) => "“Allow for this turn” lasts until the turn ends.".to_owned(),
        };
        self.note.set_text(&note);
        self.note.set_visible(pending && !note.is_empty());
    }

    /// What the card shows of a permission request (tests): the places text, the warning, the
    /// note, and whether the raw request is open.
    #[cfg(test)]
    pub(super) fn permission_view(&self) -> (String, String, String, bool) {
        (
            if self.places.root.is_visible() {
                self.places.body.text().to_string()
            } else {
                String::new()
            },
            if self.warning.is_visible() {
                self.warning.text().to_string()
            } else {
                String::new()
            },
            if self.note.is_visible() {
                self.note.text().to_string()
            } else {
                String::new()
            },
            self.raw.is_visible() && self.raw.is_expanded(),
        )
    }

    /// The labels of the card's direct buttons, in order (tests).
    #[cfg(test)]
    pub(super) fn button_labels(&self) -> Vec<String> {
        row_buttons(&self.buttons)
            .iter()
            .filter_map(|w| {
                w.downcast_ref::<gtk4::Button>()
                    .and_then(|b| b.label())
                    .or_else(|| w.downcast_ref::<gtk4::MenuButton>().and_then(|b| b.label()))
                    .map(|l| l.to_string())
            })
            .collect()
    }

    /// Presses the direct button labelled `label` (tests); false when there is none.
    #[cfg(test)]
    pub(super) fn press(&self, label: &str) -> bool {
        row_buttons(&self.buttons)
            .iter()
            .filter_map(|w| w.downcast_ref::<gtk4::Button>())
            .find(|b| b.label().as_deref() == Some(label))
            .map(|b| b.emit_clicked())
            .is_some()
    }

    /// Whether the buttons show and can be pressed, and the outcome line (tests).
    #[cfg(test)]
    pub(super) fn actionable(&self) -> (bool, bool, String) {
        (
            self.buttons.is_visible(),
            self.buttons.is_sensitive(),
            self.outcome.text().to_string(),
        )
    }

    /// What the card says "Always allow" saves, when it shows it (tests).
    #[cfg(test)]
    pub(super) fn remembers_text(&self) -> Option<String> {
        self.remembers
            .is_visible()
            .then(|| self.remembers.text().to_string())
    }
}

// ---------------------------------------------------------------------------------------------
// Question card
// ---------------------------------------------------------------------------------------------

/// What an option button holds: its label and, under it, a dim description. Shared by the
/// question card and the quick-reply chips so both read alike. Wraps: a long option must not set
/// the transcript's minimum width.
pub(super) fn option_content(text: &str, description: Option<&str>) -> gtk4::Box {
    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    let ol = label(text, &["option-label"]);
    wrapping(&ol);
    ol.set_hexpand(false);
    ol.set_max_width_chars(36);
    inner.append(&ol);
    if let Some(d) = description {
        let dl = label(d, &["option-desc"]);
        wrapping(&dl);
        dl.set_max_width_chars(36);
        inner.append(&dl);
    }
    inner
}

pub struct QuestionCardWidget {
    root: gtk4::Box,
    submit: gtk4::Button,
    status: gtk4::Label,
    options: gtk4::Box,
}

impl QuestionCardWidget {
    fn new(card: &super::model::QuestionCard, sink: &RowSink) -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        root.add_css_class("question-card");
        root.add_css_class("card");
        let (head, _) = card_head("at-dialog-question-symbolic", "The agent has a question");
        root.append(&head);

        let selected: Rc<RefCell<Vec<Vec<usize>>>> =
            Rc::new(RefCell::new(vec![Vec::new(); card.questions.len()]));
        let submit = gtk4::Button::with_label("Send answers");
        submit.add_css_class("suggested-action");
        submit.add_css_class("pill");
        submit.set_sensitive(false);

        let options = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
        for (qi, q) in card.questions.iter().enumerate() {
            let qbox = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
            if !q.header.is_empty() {
                qbox.append(&chip(&q.header, 40));
            }
            let text = content_label(&q.question, &["question-text"]);
            wrapping(&text);
            qbox.append(&text);
            if q.multi_select {
                qbox.append(&label("Choose any", &["question-hint"]));
            }
            let flow = chip_flow(4);
            let mut group: Option<gtk4::ToggleButton> = None;
            for (oi, opt) in q.options.iter().enumerate() {
                let b = gtk4::ToggleButton::new();
                b.add_css_class("option-button");
                b.set_child(Some(&option_content(
                    &opt.label,
                    opt.description.as_deref(),
                )));
                if !q.multi_select {
                    if let Some(first) = &group {
                        b.set_group(Some(first));
                    } else {
                        group = Some(b.clone());
                    }
                }
                let selected = selected.clone();
                let questions = card.questions.clone();
                let multi = q.multi_select;
                b.connect_toggled(glib::clone!(
                    #[weak]
                    submit,
                    move |b| {
                        {
                            let mut sel = selected.borrow_mut();
                            if let Some(picks) = sel.get_mut(qi) {
                                if b.is_active() {
                                    if !multi {
                                        picks.clear();
                                    }
                                    if !picks.contains(&oi) {
                                        picks.push(oi);
                                    }
                                } else {
                                    picks.retain(|&p| p != oi);
                                }
                            }
                        }
                        submit.set_sensitive(
                            payload::answers(&questions, &selected.borrow()).is_some(),
                        );
                    }
                ));
                flow.insert(&b, -1);
            }
            qbox.append(&flow);
            options.append(&qbox);
        }
        root.append(&options);

        let footer = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let status = label("", &["approval-outcome"]);
        status.set_hexpand(true);
        footer.append(&status);
        footer.append(&submit);
        root.append(&footer);

        let sink = sink.clone();
        let request = card.request.clone();
        let questions = card.questions.clone();
        submit.connect_clicked(move |_| {
            if let Some(answers) = payload::answers(&questions, &selected.borrow()) {
                sink(RowEvent::Answer {
                    request: request.clone(),
                    answers,
                });
            }
        });
        Self {
            root,
            submit,
            status,
            options,
        }
    }

    fn update(&self, state: QuestionState) {
        let (text, open) = match state {
            QuestionState::Pending => ("", true),
            QuestionState::Sent => ("Sending…", false),
            QuestionState::Answered => ("Answered", false),
            QuestionState::Withdrawn => ("Withdrawn by the agent", false),
            QuestionState::Expired => ("Expired: the agent that asked has exited", false),
        };
        self.status.set_text(text);
        self.submit.set_visible(open);
        self.options.set_sensitive(open);
        if !open {
            self.root.add_css_class("resolved");
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApprovalSuggestion {
    pub(crate) pattern: String,
    pub(crate) label: String,
    pub(crate) description: Option<String>,
}

pub(crate) fn build_approval_suggestions(
    tool: &str,
    initial_pattern: &str,
) -> Vec<ApprovalSuggestion> {
    let initial = initial_pattern.trim();
    if initial.is_empty() {
        return Vec::new();
    }

    let mut suggestions: Vec<ApprovalSuggestion> = Vec::new();
    let is_shell_cmd = matches!(tool, "run_command" | "Bash" | "bash" | "command_execution");

    let exact_desc = if is_shell_cmd {
        "Exact command"
    } else if tool == "call_mcp_tool" {
        "Exact tool"
    } else {
        "Exact pattern"
    };
    suggestions.push(ApprovalSuggestion {
        pattern: initial.to_string(),
        label: initial.to_string(),
        description: Some(exact_desc.to_string()),
    });

    if is_shell_cmd {
        let clean_words: Vec<&str> = initial
            .split_whitespace()
            .map(|w| w.trim_matches(|c| c == '*' || c == '?'))
            .filter(|w| !w.is_empty())
            .collect();
        if clean_words.len() > 1 {
            let p2 = format!("{} {} *", clean_words[0], clean_words[1]);
            if !suggestions.iter().any(|s| s.pattern == p2) {
                suggestions.push(ApprovalSuggestion {
                    pattern: p2.clone(),
                    label: p2,
                    description: Some(format!(
                        "Permit any '{} {}' command",
                        clean_words[0], clean_words[1]
                    )),
                });
            }
        }
        if let Some(first) = clean_words.first() {
            let p1 = format!("{first} *");
            if !suggestions.iter().any(|s| s.pattern == p1) {
                suggestions.push(ApprovalSuggestion {
                    pattern: p1.clone(),
                    label: p1,
                    description: Some(format!("Permit any '{first}' command")),
                });
            }
        }
    } else if crate::approval_server::classify(tool) == crate::approval_server::ToolClass::Edit {
        let p = std::path::Path::new(initial);
        if let Some(parent) = p.parent() {
            let p_str = parent.to_string_lossy();
            if !p_str.is_empty() {
                let p_dir = format!("{p_str}/*");
                if !suggestions.iter().any(|s| s.pattern == p_dir) {
                    suggestions.push(ApprovalSuggestion {
                        pattern: p_dir.clone(),
                        label: p_dir,
                        description: Some("Permit all edits in this directory".to_string()),
                    });
                }
            }
        }
        if let Some(ext) = p.extension() {
            let p_ext = format!("*.{}", ext.to_string_lossy());
            if !suggestions.iter().any(|s| s.pattern == p_ext) {
                suggestions.push(ApprovalSuggestion {
                    pattern: p_ext.clone(),
                    label: p_ext,
                    description: Some(format!("Permit all .{} file edits", ext.to_string_lossy())),
                });
            }
        }
        let p_all = "*".to_string();
        if !suggestions.iter().any(|s| s.pattern == p_all) {
            suggestions.push(ApprovalSuggestion {
                pattern: p_all.clone(),
                label: p_all,
                description: Some("Permit all file edits for this tool".to_string()),
            });
        }
    } else if tool == "call_mcp_tool" || initial.contains('/') {
        if let Some((server, _tool_name)) = initial.split_once('/') {
            let clean_server = server.trim_matches(|c| c == '*' || c == '?');
            if !clean_server.is_empty() {
                let p_server = format!("{clean_server}/*");
                if !suggestions.iter().any(|s| s.pattern == p_server) {
                    suggestions.push(ApprovalSuggestion {
                        pattern: p_server.clone(),
                        label: p_server,
                        description: Some(format!("Permit all tools on '{clean_server}'")),
                    });
                }
            }
        }
    }

    // Never offer what the rule checks would refuse (a glob path, a runner's pattern, `*`).
    suggestions.retain(|s| {
        pattern_error(tool, &s.pattern, "", crate::always_allow::Scope::Session).is_none()
    });
    suggestions
}

/// The always-allow dialog's pattern suggestions: toggle buttons that keep `entry` in step.
/// None when there is nothing to choose between.
pub(super) fn suggestion_flow(
    entry: &gtk4::Entry,
    tool: &str,
    initial_pattern: &str,
) -> Option<gtk4::FlowBox> {
    let suggestions = build_approval_suggestions(tool, initial_pattern);
    if suggestions.len() <= 1 {
        return None;
    }
    let flow = chip_flow(3);

    let in_sync = Rc::new(Cell::new(false));
    let mut toggle_buttons: Vec<(gtk4::ToggleButton, String)> = Vec::new();

    for opt in &suggestions {
        let b = gtk4::ToggleButton::new();
        b.add_css_class("option-button");

        let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        let ol = label(&opt.label, &["option-label"]);
        wrapping(&ol);
        ol.set_max_width_chars(32);
        ol.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
        inner.append(&ol);

        if let Some(desc) = &opt.description {
            let dl = label(desc, &["option-desc"]);
            wrapping(&dl);
            dl.set_max_width_chars(36);
            inner.append(&dl);
        }

        b.set_child(Some(&inner));
        b.set_tooltip_text(Some(&opt.pattern));

        if opt.pattern == initial_pattern {
            b.set_active(true);
        }

        toggle_buttons.push((b.clone(), opt.pattern.clone()));
        flow.insert(&b, -1);
    }

    // Handlers hold the widgets weakly: the buttons and the entry each own the other's
    // handler, so strong references would keep them all alive for good.
    let weak_buttons: Rc<Vec<(glib::WeakRef<gtk4::ToggleButton>, String)>> = Rc::new(
        toggle_buttons
            .iter()
            .map(|(b, pat)| (b.downgrade(), pat.clone()))
            .collect(),
    );
    for (b, pat) in &toggle_buttons {
        let entry = entry.downgrade();
        let others = weak_buttons.clone();
        let in_sync = in_sync.clone();
        let pattern = pat.clone();

        b.connect_toggled(move |btn| {
            let Some(entry) = entry.upgrade() else { return };
            if in_sync.get() {
                return;
            }
            in_sync.set(true);
            if btn.is_active() {
                entry.set_text(&pattern);
                for (other, other_pat) in others.iter() {
                    if let (Some(other), true) = (other.upgrade(), other_pat != &pattern) {
                        other.set_active(false);
                    }
                }
            } else if entry.text().trim() == pattern {
                btn.set_active(true);
            }
            in_sync.set(false);
        });
    }

    {
        let buttons = weak_buttons.clone();
        let in_sync = in_sync.clone();
        entry.connect_changed(move |ent| {
            if in_sync.get() {
                return;
            }
            in_sync.set(true);
            let current = ent.text().trim().to_string();
            for (btn, pat) in buttons.iter() {
                if let Some(btn) = btn.upgrade() {
                    btn.set_active(pat == &current);
                }
            }
            in_sync.set(false);
        });
    }

    Some(flow)
}

fn confirm_approval_rule(
    parent: &impl IsA<gtk4::Widget>,
    request: String,
    tool: String,
    initial_pattern: String,
    cwd: Option<String>,
    sink: Rc<dyn Fn(RowEvent)>,
    decision: Decision,
) {
    let (dialog, entry, global_check) = rule_dialog(&tool, &initial_pattern, decision);
    let root = parent.root();
    let req = request;
    let t = tool;
    let c = cwd;

    glib::MainContext::default().spawn_local(async move {
        let response = dialog.choose_future(root.as_ref()).await;
        let chosen = match response.as_str() {
            "save" => Some(decision),
            "save_session" => Some(Decision::AllowForSession),
            _ => None,
        };
        match response.as_str() {
            "save" | "save_session" => {
                let user_pattern = entry.text().trim().to_string();
                let pattern = if user_pattern.is_empty() {
                    initial_pattern
                } else {
                    user_pattern
                };
                let workspace = if global_check.is_active() {
                    "*".to_string()
                } else {
                    c.unwrap_or_default()
                };
                // Save is disabled while the pattern is invalid (inline validation), so an
                // `Err` here is only a race; it never becomes a different rule: allow once.
                let (decision, custom_rule) =
                    match crate::always_allow::Rule::new(workspace, t, pattern) {
                        Ok(rule) => (chosen.unwrap_or(decision), Some(rule)),
                        Err(_) => (Decision::Allow, None),
                    };
                sink(RowEvent::Approve {
                    request: req,
                    decision,
                    custom_rule,
                });
            }
            "allow_once" => {
                sink(RowEvent::Approve {
                    request: req,
                    decision: Decision::Allow,
                    custom_rule: None,
                });
            }
            _ => {
                // Cancelled: leave pending.
            }
        }
    });
}

/// The rule dialog for `decision`, its pattern entry prefilled with `initial_pattern`, and the
/// "all workspaces" check.
fn rule_dialog(
    tool: &str,
    initial_pattern: &str,
    decision: Decision,
) -> (adw::AlertDialog, gtk4::Entry, gtk4::CheckButton) {
    let (title, subtitle, save_label) = if decision == Decision::AllowForSession {
        (
            "Confirm Session Approval Rule",
            "Modify the pattern below to loosen or tighten what will be approved for the \
             remainder of this session. A final * covers any further arguments.",
            "Save for Session & Allow",
        )
    } else {
        (
            "Confirm Auto-Approval Rule",
            "Modify the pattern below to loosen or tighten what will be automatically approved \
             in future sessions. A final * covers any further arguments.",
            "Save Always & Allow",
        )
    };

    let dialog = adw::AlertDialog::new(Some(title), Some(subtitle));

    let content_box = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
    content_box.set_margin_top(8);
    content_box.set_margin_bottom(8);
    content_box.set_margin_start(12);
    content_box.set_margin_end(12);

    let entry = gtk4::Entry::builder()
        .text(initial_pattern)
        .placeholder_text("e.g. ls -la or git status *")
        .activates_default(true)
        .hexpand(true)
        .build();
    content_box.append(&entry);
    // Inline validation: a pattern no rule can hold says why, and cannot be saved.
    let error = gtk4::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .css_classes(["error", "caption"])
        .visible(false)
        .build();
    content_box.append(&error);

    if let Some(flow) = suggestion_flow(&entry, tool, initial_pattern) {
        content_box.append(&flow);
    }

    let global_check = gtk4::CheckButton::builder()
        .label("Apply across all workspaces")
        .active(false)
        .build();
    content_box.append(&global_check);

    let caption = gtk4::Label::builder()
        .label(
            "Caution: Overly broad patterns will permit arbitrary agent commands without review.",
        )
        .wrap(true)
        .css_classes(["dim-label", "caption"])
        .build();
    content_box.append(&caption);

    dialog.set_extra_child(Some(&content_box));
    if decision == Decision::AllowAlways {
        dialog.add_responses(&[
            ("cancel", "Cancel"),
            ("allow_once", "Allow Once"),
            ("save_session", "Save for Session"),
            ("save", save_label),
        ]);
    } else {
        dialog.add_responses(&[
            ("cancel", "Cancel"),
            ("allow_once", "Allow Once"),
            ("save", save_label),
        ]);
    }
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("save"));
    dialog.set_close_response("cancel");

    let validate = {
        let dialog = dialog.downgrade();
        let error = error.downgrade();
        let tool = tool.to_owned();
        let initial = initial_pattern.to_owned();
        move |entry: &gtk4::Entry| {
            let (Some(dialog), Some(error)) = (dialog.upgrade(), error.upgrade()) else {
                return;
            };
            use crate::always_allow::Scope;
            let save_scope = if decision == Decision::AllowAlways {
                Scope::Always
            } else {
                Scope::Session
            };
            let text = entry.text();
            let save_why = pattern_error(&tool, &text, &initial, save_scope);
            let session_why = pattern_error(&tool, &text, &initial, Scope::Session);
            if dialog.has_response("save") {
                dialog.set_response_enabled("save", save_why.is_none());
            }
            if dialog.has_response("save_session") {
                dialog.set_response_enabled("save_session", session_why.is_none());
            }
            match save_why.or(session_why) {
                Some(why) => {
                    error.set_text(&why);
                    error.set_visible(true);
                    entry.add_css_class("error");
                }
                None => {
                    error.set_visible(false);
                    entry.remove_css_class("error");
                }
            }
        }
    };
    validate(&entry);
    entry.connect_changed(validate);
    (dialog, entry, global_check)
}

/// Why the dialog's pattern cannot be a rule for `tool` kept for `scope` (an empty entry means
/// the prefilled pattern), or `None` when it can: [`crate::always_allow::Rule::new`] then
/// [`crate::always_allow::Rule::check`], the checks the session applies when it is saved.
pub(super) fn pattern_error(
    tool: &str,
    text: &str,
    initial: &str,
    scope: crate::always_allow::Scope,
) -> Option<String> {
    let pattern = if text.trim().is_empty() {
        initial
    } else {
        text
    };
    // Judged as a global rule: the working directory is the request's, filled in when saved.
    crate::always_allow::Rule::new("*", tool, pattern)
        .and_then(|rule| rule.check(scope))
        .err()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A widget that counts the times GTK measures it.
    mod counting {
        use gtk4::glib;
        use gtk4::subclass::prelude::*;
        use std::cell::Cell;

        #[derive(Default)]
        pub struct Imp {
            pub measures: Cell<u32>,
        }

        #[glib::object_subclass]
        impl ObjectSubclass for Imp {
            const NAME: &'static str = "AtMeasureCounter";
            type Type = Counter;
            type ParentType = gtk4::Widget;
        }

        impl ObjectImpl for Imp {}

        impl WidgetImpl for Imp {
            fn measure(&self, _: gtk4::Orientation, _: i32) -> (i32, i32, i32, i32) {
                self.measures.set(self.measures.get() + 1);
                (10, 400, -1, -1)
            }
        }

        glib::wrapper! {
            pub struct Counter(ObjectSubclass<Imp>) @extends gtk4::Widget,
                @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
        }
    }

    /// GTK measures a closed revealer's child on every size request (GTK 4.22 included), so a
    /// collapsed card's body cost a resize what an open one did. [`measure_only_when_open`]
    /// hides it once closed and shows it again before it opens.
    pub(crate) fn a_closed_revealer_measures_its_child_unless_hidden() {
        use gtk4::subclass::prelude::ObjectSubclassIsExt;
        let measured = |revealer: &gtk4::Revealer, child: &counting::Counter, width: i32| {
            let before = child.imp().measures.get();
            let _ = revealer.measure(gtk4::Orientation::Vertical, width);
            child.imp().measures.get() - before
        };
        let child: counting::Counter = glib::Object::new();
        let plain = gtk4::Revealer::new();
        plain.set_child(Some(&child));
        assert!(!plain.reveals_child() && !plain.is_child_revealed());
        assert!(
            measured(&plain, &child, 500) > 0,
            "GTK no longer measures a closed revealer's child: the hiding can go"
        );

        let child: counting::Counter = glib::Object::new();
        let lazy = gtk4::Revealer::new();
        lazy.set_transition_duration(0);
        lazy.set_child(Some(&child));
        measure_only_when_open(&lazy);
        assert_eq!(measured(&lazy, &child, 501), 0, "closed: never measured");
        lazy.set_reveal_child(true);
        assert!(child.is_visible(), "shown before it opens");
        assert!(measured(&lazy, &child, 502) > 0);
        lazy.set_reveal_child(false);
        assert!(!lazy.is_child_revealed() && !child.is_visible());
        assert_eq!(measured(&lazy, &child, 503), 0);

        // With a transition, it stays shown until the close is over.
        let child: counting::Counter = glib::Object::new();
        let sliding = gtk4::Revealer::new();
        sliding.set_child(Some(&child));
        measure_only_when_open(&sliding);
        sliding.set_reveal_child(true);
        sliding.set_reveal_child(false);
        assert_eq!(child.is_visible(), sliding.is_child_revealed());
    }

    #[test]
    #[ignore = "native GTK; run alone in an isolated display"]
    fn native_clipping_warning_reaches_both_diff_surfaces() {
        gtk4::init().expect("GTK init");
        let warning =
            "Only the first 512 KiB of each side were compared; the change may lie beyond.";
        let sink: RowSink = Rc::new(|_| {});
        let card = ToolCard::new("edit", &sink);
        card.diff.toggle.set_active(true);
        card.show_diff(&Ok(agent_kit::filediff::Shown {
            text: "@@ -1 +1 @@\n-old\n+new\n".into(),
            origin: agent_kit::filediff::Origin::Checkpoint,
            omitted_lines: 12,
            clipped: true,
            files: vec!["file.txt".into()],
        }));
        assert!(card.diff.text.notes().contains(warning));
        let prefix = "same\n".repeat(120_000);
        let (approval, _) = codex_card(
            "Edit",
            serde_json::json!({
                "file_path": "/w/file.txt",
                "old_string": format!("old\n{prefix}"),
                "new_string": format!("new\n{prefix}"),
            }),
            vec![Decision::Allow, Decision::Deny],
            None,
        );
        assert!(approval.diff.notes().contains(warning));
        assert!(
            !approval.diff.text().contains(warning),
            "the trailer was cut from the display"
        );
    }

    /// Every `.card-section` and `.code-block` has a Copy button, and output cut to the last
    /// 40 lines can be shown whole (and copied whole).
    pub(crate) fn blocks_copy_and_long_output_shows_full() {
        let sink: RowSink = Rc::new(|_| {});
        let output: String = (1..=100).map(|n| format!("line {n}\n")).collect();
        let item = Item {
            id: "long".into(),
            body: Body::Tool(super::super::model::Tool {
                kind: ItemKind::Command,
                title: "Bash".into(),
                input: Some(serde_json::json!({"command": "seq 100"})),
                input_text: String::new(),
                output: output.clone(),
                error: Some("exit status 1".into()),
                status: ToolStatus::Failed,
                reported_model: None,
            }),
            expanded: true,
            parent: None,
            children: Vec::new(),
        };
        let card = ToolCard::new(&item.id, &sink);
        card.update(&item);
        let missing = crate::testutil::blocks_without_copy(&card.root.clone().upcast());
        assert!(missing.is_empty(), "{missing:?}");

        let shown = |card: &ToolCard| card.output_box.body.text().lines().count();
        assert_eq!(
            shown(&card),
            MAX_CARD_LINES + 1,
            "the last lines and a note"
        );
        assert!(card.output_box.more.is_visible(), "offers the rest");
        assert_eq!(card.output_box.full.borrow().lines().count(), 100);
        card.output_box.more.emit_clicked();
        assert_eq!(shown(&card), 100, "Show full shows everything");
        assert_eq!(card.output_box.more.label().as_deref(), Some("Show less"));
        card.output_box.more.emit_clicked();
        assert_eq!(shown(&card), MAX_CARD_LINES + 1);
        // Short text offers nothing more.
        assert!(!card.input_box.more.is_visible());

        // The approval card's details, raw request and diff, and the error row.
        let approval = ApprovalCard::new(&sink);
        let missing = crate::testutil::blocks_without_copy(&approval.root.clone().upcast());
        assert!(missing.is_empty(), "{missing:?}");
        let failed = error_row("boom");
        let copies = crate::testutil::descendants(failed.upcast_ref())
            .iter()
            .filter(|w| w.tooltip_text().is_some_and(|t| t == "Copy error"))
            .count();
        assert_eq!(copies, 1, "an error can be copied whole");
    }

    pub(crate) fn file_changes_and_permission_details_have_distinct_actions() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let received = events.clone();
        let sink: RowSink = Rc::new(move |event| received.borrow_mut().push(event));
        let item = Item {
            id: "edit".into(),
            body: Body::Tool(super::super::model::Tool {
                kind: ItemKind::FileChange,
                title: "a.rs (+1 more)".into(),
                input: Some(serde_json::json!([
                    {"path":"a.rs","kind":{"type":"update"},"diff":"@@ -1 +1 @@\n-old\n+new\n"},
                    {"path":"b.rs","kind":{"type":"update"},"diff":"@@ -1 +1 @@\n-before\n+after\n"}
                ])),
                input_text: String::new(),
                output: String::new(),
                error: None,
                status: ToolStatus::Completed,
                reported_model: None,
            }),
            expanded: false,
            parent: None,
            children: Vec::new(),
        };
        let card = ToolCard::new(&item.id, &sink);
        card.update(&item);
        assert!(card.summary.text().contains("a.rs") && card.summary.text().contains("b.rs"));
        card.header.emit_clicked();
        assert!(card.diff.revealer.reveals_child());
        assert!(matches!(events.borrow().last(), Some(RowEvent::LoadDiff { id }) if id == "edit"));
        assert!(
            !card.revealer.reveals_child(),
            "showing changes does not expand raw details"
        );
        card.diff.details.set_active(true);
        assert!(
            matches!(events.borrow().last(), Some(RowEvent::Toggle { id, expanded: true }) if id == "edit")
        );

        let mut transcript = super::super::model::Transcript::new();
        transcript.apply(
            &agent_core::event::Envelope::new(agent_core::event::Event::ApprovalRequested {
                tool: "Bash".into(),
                title: None,
                input: serde_json::json!({"command":"cargo test"}),
                reason: None,
                options: vec![
                    Decision::Allow,
                    Decision::AllowAlways,
                    Decision::AllowForSession,
                    Decision::Deny,
                ],
                response: agent_core::event::ResponseCapability::Live,
                remembers: Some("cargo test in this folder".into()),
            })
            .request("approval"),
            Driver::Claude,
        );
        let approval = ApprovalCard::new(&sink);
        approval.update(transcript.get("approval:approval").expect("approval card"));
        let direct = row_buttons(&approval.buttons);
        assert_eq!(
            direct.len(),
            3,
            "long-lived grants do not widen direct controls"
        );
        let deny = direct[0]
            .clone()
            .downcast::<gtk4::Button>()
            .expect("direct deny");
        assert_eq!(deny.label().as_deref(), Some("Deny"));
        let more = direct[1]
            .clone()
            .downcast::<gtk4::MenuButton>()
            .expect("advanced choices");
        let allow = direct[2]
            .clone()
            .downcast::<gtk4::Button>()
            .expect("direct allow");
        assert_eq!(allow.label().as_deref(), Some("Allow"));
        let extra = more
            .popover()
            .expect("advanced menu")
            .child()
            .expect("choices");
        let always = extra
            .first_child()
            .expect("always allow")
            .downcast::<gtk4::Button>()
            .expect("grant");
        always.emit_clicked();
        assert!(
            matches!(events.borrow().last(), Some(RowEvent::Approve { request, decision: Decision::AllowAlways, .. }) if request == "approval")
        );
        let session_btn = always
            .next_sibling()
            .expect("session allow")
            .downcast::<gtk4::Button>()
            .expect("grant");
        session_btn.emit_clicked();
        assert!(
            matches!(events.borrow().last(), Some(RowEvent::Approve { request, decision: Decision::AllowForSession, .. }) if request == "approval")
        );
        assert!(
            approval
                .remembers
                .text()
                .contains("cargo test in this folder"),
            "scope explanation is retained"
        );
        chat_labels_are_selectable();
        codex_approval_card_checks();
        permission_cards_say_what_is_asked();
        approval_buttons_wrap();
    }

    /// A Codex-shaped approval: one card, `transcript` resolved by `resolution` (or pending).
    fn codex_card(
        tool: &str,
        input: Value,
        options: Vec<Decision>,
        resolution: Option<Decision>,
    ) -> (ApprovalCard, Rc<RefCell<Vec<RowEvent>>>) {
        let events = Rc::new(RefCell::new(Vec::new()));
        let received = events.clone();
        let sink: RowSink = Rc::new(move |event| received.borrow_mut().push(event));
        let mut transcript = super::super::model::Transcript::new();
        transcript.apply(
            &agent_core::event::Envelope::new(agent_core::event::Event::ApprovalRequested {
                tool: tool.into(),
                title: None,
                input,
                reason: None,
                options,
                response: agent_core::event::ResponseCapability::Live,
                remembers: None,
            })
            .request("c"),
            Driver::Codex,
        );
        if let Some(decision) = resolution {
            transcript.apply(
                &agent_core::event::Envelope::new(agent_core::event::Event::ApprovalResolved {
                    decision,
                })
                .request("c"),
                Driver::Codex,
            );
        }
        let card = ApprovalCard::new(&sink);
        card.update(transcript.get("approval:c").expect("approval card"));
        (card, events)
    }

    /// A git command's approval says the operation in words above the command, keeps the
    /// command line itself, and does not suggest Allow for one that can lose work.
    pub(crate) fn a_git_approval_reads_as_its_operation() {
        let allow = vec![Decision::Allow, Decision::Deny];
        let suggested = |card: &ApprovalCard| {
            row_buttons(&card.buttons)
                .iter()
                .filter_map(|w| w.downcast_ref::<gtk4::Button>())
                .any(|b| b.has_css_class("suggested-action"))
        };
        let (card, _) = codex_card(
            "command_execution",
            serde_json::json!({"command": "git commit -m \"feat(ui): add a PR button\"",
                               "cwd": "/w"}),
            allow.clone(),
            None,
        );
        let (title, badges, details, warning) = card.git.shown().expect("a git reading");
        assert_eq!(
            title,
            "Commit the staged changes: “feat(ui): add a PR button”"
        );
        assert_eq!(badges, "git · Changes this repository");
        assert_eq!(details[0].1, "feat(ui): add a PR button");
        assert!(warning.is_empty());
        assert_eq!(card.title.text(), "Allow git?");
        assert!(
            card.input_section.full.borrow().contains("git commit"),
            "the command line is still shown"
        );
        assert!(suggested(&card), "a plain commit keeps Allow suggested");

        let (card, _) = codex_card(
            "command_execution",
            serde_json::json!({"command": "git reset --hard HEAD~3"}),
            allow.clone(),
            None,
        );
        assert!(card
            .git
            .shown()
            .expect("reading")
            .3
            .starts_with("Hard reset"));
        assert!(!suggested(&card), "a hard reset never suggests Allow");

        let (card, _) = codex_card(
            "command_execution",
            serde_json::json!({"command": "cargo test"}),
            allow,
            None,
        );
        assert!(card.git.shown().is_none());
    }

    fn direct_labels(card: &ApprovalCard) -> Vec<String> {
        row_buttons(&card.buttons)
            .iter()
            .filter_map(|w| w.downcast_ref::<gtk4::Button>())
            .map(|b| b.label().map(|l| l.to_string()).unwrap_or_default())
            .collect()
    }

    /// Codex cards carry Deny and a separate Stop turn; a refusal is styled as one.
    fn codex_approval_card_checks() {
        let all = vec![
            Decision::Allow,
            Decision::AllowAlways,
            Decision::AllowForSession,
            Decision::Deny,
            Decision::Cancel,
        ];
        let shell = serde_json::json!({"command": "/usr/bin/zsh -lc 'ls -la'", "cwd": "/w"});
        let (card, events) = codex_card("command_execution", shell.clone(), all.clone(), None);
        assert_eq!(direct_labels(&card), ["Stop turn", "Deny", "Allow"]);
        let stop = row_buttons(&card.buttons)
            .first()
            .and_then(|w| w.clone().downcast::<gtk4::Button>().ok())
            .expect("stop turn");
        stop.emit_clicked();
        assert!(matches!(
            events.borrow().last(),
            Some(RowEvent::Approve {
                decision: Decision::Cancel,
                ..
            })
        ));
        for (decision, denied, outcome) in [
            (Decision::Deny, true, "Denied"),
            (Decision::Cancel, true, "Turn stopped"),
            (Decision::Allow, false, "Allowed once"),
        ] {
            let (card, _) = codex_card(
                "command_execution",
                shell.clone(),
                all.clone(),
                Some(decision),
            );
            assert_eq!(card.root.has_css_class("denied"), denied, "{outcome}");
            assert_eq!(card.outcome.text(), outcome);
        }
        // A patch approval carries the change its item started with: shown as a diff.
        let edit = serde_json::json!({"itemId": "e", "changes": [
            {"path": "/w/fixture.txt", "kind": {"type": "update"},
             "diff": "@@ -1 +1 @@\n-hello world\n+hello there\n"}]});
        let (card, _) = codex_card("file_change", edit, all, None);
        let (files, diff_shown, _, diff) = card.diff_view();
        assert!(files.contains("/w/fixture.txt"), "{files}");
        assert!(diff_shown && diff.contains("hello there"), "{diff}");
        assert_eq!(direct_labels(&card), ["Stop turn", "Deny", "Allow"]);
        rule_dialog_prefills_the_bare_w0_command();
        rule_dialog_validates_the_pattern_inline();
        approval_cards_show_the_command_and_cap_the_rest();
    }

    /// `Rule::new` is a `Result`: the rule dialog shows why a pattern cannot be a rule as it is
    /// typed, and Save (and Save for Session) stay disabled until it can.
    fn rule_dialog_validates_the_pattern_inline() {
        let (dialog, entry, _) = rule_dialog("command_execution", "ls -la", Decision::AllowAlways);
        let enabled = |d: &adw::AlertDialog| {
            (
                d.is_response_enabled("save"),
                d.is_response_enabled("save_session"),
            )
        };
        assert_eq!(enabled(&dialog), (true, true));
        assert!(!entry.has_css_class("error"));
        for bad in ["ls; id", "FOO=1 ls", "printf %s =sh", "ls\u{00A0}-la", "*"] {
            entry.set_text(bad);
            assert_eq!(enabled(&dialog), (false, false), "{bad:?}");
            assert!(entry.has_css_class("error"), "{bad:?}");
        }
        entry.set_text("ls *");
        assert_eq!(enabled(&dialog), (true, true));
        assert!(!entry.has_css_class("error"));
        entry.set_text("");
        assert_eq!(enabled(&dialog), (true, true), "empty means the prefill");
        // Only what no rule can hold is refused inline. A runner's command and its pattern
        // are the user's to keep, for the session or for good.
        entry.set_text("cargo test");
        assert_eq!(enabled(&dialog), (true, true));
        assert!(!entry.has_css_class("error"));
        entry.set_text("cargo test *");
        assert_eq!(enabled(&dialog), (true, true));

        use crate::always_allow::Scope::{Always, Session};
        assert!(pattern_error("command_execution", "git status *", "", Always).is_none());
        assert!(pattern_error("command_execution", "cargo test", "", Session).is_none());
        assert!(pattern_error("command_execution", "cargo test", "", Always).is_none());
        assert!(pattern_error("write_to_file", "*.rs", "", Session).is_some());
        assert!(pattern_error("call_mcp_tool", "fs/read", "", Always).is_none());
        assert!(pattern_error("WebFetch", "x", "", Session).is_some());
        // No suggestion the checks would refuse.
        for (tool, initial) in [
            ("write_to_file", "/w/src/a.rs"),
            ("command_execution", "cargo test --lib"),
            ("command_execution", "ls -la"),
            ("call_mcp_tool", "fs/read"),
        ] {
            for s in build_approval_suggestions(tool, initial) {
                assert!(
                    pattern_error(tool, &s.pattern, "", Session).is_none(),
                    "{tool}: {}",
                    s.pattern
                );
            }
        }
    }

    /// A Codex command card shows the bare command and its folder,
    /// with the request's JSON behind a capped "Show raw"; a large input shows at most the cap;
    /// Claude's ExitPlanMode shows its plan as markdown, not JSON.
    fn approval_cards_show_the_command_and_cap_the_rest() {
        let w0 = include_str!("../../../tests/fixtures/real/codex/ask-shell-decline.jsonl")
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|r| r["frame"]["method"] == "item/commandExecution/requestApproval")
            .expect("W0 frame")["frame"]["params"]
            .clone();
        let options = vec![Decision::Allow, Decision::Deny, Decision::Cancel];
        let (card, _) = codex_card("command_execution", w0, options.clone(), None);
        assert_eq!(
            card.input_section.body.text(),
            "$ ls -la\nWorking directory: /tmp/w0-sandbox"
        );
        assert!(
            card.raw.is_visible(),
            "the request's JSON is one click away"
        );
        assert!(card.raw_section.body.text().contains("availableDecisions"));
        assert!(!card.plan.widget().is_visible());

        let big: serde_json::Map<String, Value> = (0..200)
            .map(|i| (format!("key{i:03}"), Value::from(format!("value {i}"))))
            .collect();
        let (card, _) = codex_card("WebFetch", Value::Object(big), options.clone(), None);
        let visible = card.input_section.body.text().lines().count();
        assert!(visible <= APPROVAL_VISIBLE_LINES, "{visible} lines shown");
        assert!(card
            .input_section
            .body
            .text()
            .ends_with("more lines in Show raw"));
        assert!(card.raw_section.body.text().lines().count() <= MAX_CARD_LINES + 1);

        // A command the card cuts (a 30-line heredoc, one 5000-character line) always keeps its
        // whole text one click away, though it has only the keys a plain command has.
        let heredoc = format!(
            "cat > notes.txt <<'EOF'\n{}EOF",
            (1..=30).map(|i| format!("line {i}\n")).collect::<String>()
        );
        let long = format!("echo {}", "x".repeat(5000));
        for command in [heredoc, long] {
            let (card, _) = codex_card(
                "Bash",
                serde_json::json!({ "command": command }),
                options.clone(),
                None,
            );
            let shown = card.input_section.body.text().to_string();
            assert!(shown.lines().count() <= APPROVAL_VISIBLE_LINES, "{shown}");
            assert!(
                shown
                    .lines()
                    .all(|l| l.chars().count() <= APPROVAL_LINE_CHARS + 60),
                "a single enormous line is cut"
            );
            assert!(shown.contains("in Show raw"), "{shown}");
            assert!(card.raw.is_visible(), "cut text is never approved unseen");
            let raw: Value = serde_json::from_str(&card.raw_section.full.borrow()).expect("raw");
            assert_eq!(raw["command"], command, "Show raw holds the whole command");
        }
        // A short plain command is shown whole: no raw needed.
        let (card, _) = codex_card(
            "Bash",
            serde_json::json!({ "command": "ls -la" }),
            options.clone(),
            None,
        );
        assert!(!card.raw.is_visible());

        let plan = "# Ship the parity wave\n\n1. Deny sends decline\n2. Stop turn cancels\n";
        let (card, _) = codex_card(
            "ExitPlanMode",
            serde_json::json!({ "plan": plan }),
            options,
            None,
        );
        assert!(card.plan.widget().is_visible());
        assert!(
            !card.input_section.root.is_visible(),
            "no JSON dump of the plan"
        );
        let blocks = card.plan.blocks.borrow();
        assert!(
            matches!(blocks.first(), Some((Block::Text { kind: TextKind::Heading(_), markup }, _))
                if markup.contains("Ship the parity wave")),
            "the plan renders as markdown"
        );
        assert!(blocks.len() >= 2);
    }

    /// From the Codex frame: the rule dialog's entry holds the bare command,
    /// not the wrapper and not the card's footer sentence.
    fn rule_dialog_prefills_the_bare_w0_command() {
        let params = include_str!("../../../tests/fixtures/real/codex/ask-shell-decline.jsonl")
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|r| r["frame"]["method"] == "item/commandExecution/requestApproval")
            .expect("W0 frame")["frame"]["params"]
            .clone();
        assert_eq!(params["command"], "/usr/bin/zsh -lc 'ls -la'");
        let prefill = crate::always_allow::dialog_prefill("command_execution", &params);
        assert_eq!(prefill, "ls -la");
        for decision in [Decision::AllowAlways, Decision::AllowForSession] {
            let (_, entry, global) = rule_dialog("command_execution", &prefill, decision);
            assert_eq!(entry.text(), "ls -la");
            assert!(!global.is_active());
        }
        let s = build_approval_suggestions("command_execution", &prefill);
        assert_eq!(s[0].pattern, "ls -la");
        assert_eq!(s[0].description.as_deref(), Some("Exact command"));
    }

    #[test]
    fn divider_texts() {
        assert_eq!(
            compaction_text(true, 21_478, Some(2_082)),
            "Context compacted 21k → 2.1k"
        );
        assert_eq!(
            compaction_text(false, 150_000, None),
            "Context auto-compacted (was 150k)"
        );
        assert_eq!(
            switch_text(Driver::Agy, Some("gemini-3.1-pro"), true, false),
            "Continued in Antigravity · gemini-3.1-pro"
        );
        assert_eq!(
            switch_text(Driver::Claude, Some("opus"), true, true),
            "Back in Claude · new session from a summary"
        );
        assert_eq!(
            switch_text(Driver::Claude, Some("sonnet"), false, false),
            "Model switched to sonnet"
        );
    }

    #[test]
    fn languages_normalise() {
        assert_eq!(normalise_lang("RS"), "rust");
        assert_eq!(normalise_lang("bash"), "sh");
        assert_eq!(normalise_lang("toml"), "toml");
    }

    #[test]
    fn approval_suggestions_for_shell_and_mcp() {
        // A runner gets the same choices as any command: the user decides (a warning says what
        // a pattern for it allows when it is kept).
        let s = build_approval_suggestions("run_command", "cargo test --workspace");
        let patterns: Vec<&str> = s.iter().map(|s| s.pattern.as_str()).collect();
        assert_eq!(
            patterns,
            ["cargo test --workspace", "cargo test *", "cargo *"]
        );
        assert_eq!(s[0].description.as_deref(), Some("Exact command"));

        // A read-only git command widens.
        let s = build_approval_suggestions("Bash", "git status");
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].pattern, "git status");
        assert_eq!(s[1].pattern, "git status *");
        assert_eq!(
            s[1].description.as_deref(),
            Some("Permit any 'git status' command")
        );
        assert_eq!(s[2].pattern, "git *");

        // Single-word commands: exact, then the program with any arguments, whatever it is.
        let s = build_approval_suggestions("bash", "ls");
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].pattern, "ls");
        assert_eq!(s[1].pattern, "ls *");
        let s = build_approval_suggestions("bash", "htop");
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].pattern, "htop");
        assert_eq!(s[1].pattern, "htop *");

        // MCP tool
        let s = build_approval_suggestions("call_mcp_tool", "ansible/deploy_apply");
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].pattern, "ansible/deploy_apply");
        assert_eq!(s[0].description.as_deref(), Some("Exact tool"));
        assert_eq!(s[1].pattern, "ansible/*");
        assert_eq!(
            s[1].description.as_deref(),
            Some("Permit all tools on 'ansible'")
        );

        // Deduplication when the pattern already has its wildcard, a runner's included.
        let s = build_approval_suggestions("run_command", "rg *");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].pattern, "rg *");
        let s = build_approval_suggestions("run_command", "cargo *");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].pattern, "cargo *");

        // Empty pattern
        let s = build_approval_suggestions("run_command", "");
        assert!(s.is_empty());
    }

    fn chat_labels_are_selectable() {
        // A plain label is inert; content opts in.
        let l = label("sample text", &["custom-class"]);
        assert!(!l.is_selectable());
        assert!(content_label("sample text", &[]).is_selectable());

        let err = error_row("something went wrong");
        let col = err
            .first_child()
            .and_then(|icon| icon.next_sibling())
            .unwrap()
            .downcast::<gtk4::Box>()
            .unwrap();
        let title_lbl = col
            .first_child()
            .unwrap()
            .downcast::<gtk4::Label>()
            .unwrap();
        let text_lbl = col.last_child().unwrap().downcast::<gtk4::Label>().unwrap();
        assert!(title_lbl.is_selectable());
        assert!(text_lbl.is_selectable());

        let not = notice("Heads up", Tone::Info);
        let not_lbl = not.last_child().unwrap().downcast::<gtk4::Label>().unwrap();
        assert!(not_lbl.is_selectable());

        let div = divider("Context compacted", "compaction-divider", None);
        let pill = div
            .downcast::<gtk4::Box>()
            .unwrap()
            .first_child()
            .unwrap()
            .next_sibling()
            .unwrap()
            .downcast::<gtk4::Box>()
            .unwrap();
        let div_lbl = pill
            .first_child()
            .unwrap()
            .downcast::<gtk4::Label>()
            .unwrap();
        assert!(div_lbl.is_selectable());

        let sink: RowSink = Rc::new(|_| {});
        let app = ApprovalCard::new(&sink);
        assert!(app.title.is_selectable());
        assert!(app.reason.is_selectable());
        assert!(app.remembers.is_selectable());
        assert!(app.outcome.is_selectable());

        // The card header is a button: its labels must not eat the click.
        let tc = ToolCard::new("t1", &sink);
        assert!(!tc.title.is_selectable());
        assert!(!tc.summary.is_selectable());
        assert!(!tc.badge.is_selectable());
        assert!(tc.error.is_selectable());
    }

    /// Widget-tree walk: no selectable label has a button, list row or click gesture above it.
    /// Builds every card kind that carries a click target, plus the always-allow dialog's
    /// suggestion buttons.
    pub(crate) fn click_targets_hold_no_selectable_labels() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let received = events.clone();
        let sink: RowSink = Rc::new(move |event| received.borrow_mut().push(event));
        let mut transcript = super::super::model::Transcript::new();
        for env in super::super::demo::DemoBackend::script_envelopes() {
            transcript.apply(&env, Driver::Claude);
        }
        let items: Vec<&Item> = transcript
            .order()
            .iter()
            .filter_map(|id| transcript.get(id))
            .collect();
        let mut rows = 0;
        for item in items.iter().copied() {
            let row = Row::build(item, &sink);
            row.update(item);
            let problems = crate::testutil::selectable_labels_in_click_targets(&row.widget());
            assert!(problems.is_empty(), "{}: {problems:?}", item.id);
            rows += 1;
        }
        assert!(
            rows > 10,
            "the demo script builds a full transcript: {rows}"
        );

        // Content labels stay selectable (the widgets this walk must not strip).
        let mut selectable = 0;
        for item in items.iter().copied() {
            let row = Row::build(item, &sink);
            row.update(item);
            selectable += crate::testutil::descendants(&row.widget())
                .iter()
                .filter(|w| {
                    w.downcast_ref::<gtk4::Label>()
                        .is_some_and(|l| l.is_selectable())
                })
                .count();
        }
        assert!(
            selectable > 10,
            "content labels remain selectable: {selectable}"
        );

        // The always-allow dialog's suggestions are toggle buttons around option labels.
        let entry = gtk4::Entry::new();
        // A read-only command: a runner gets no patterns to choose between.
        let flow = suggestion_flow(&entry, "Bash", "git log --oneline").expect("suggestions");
        let toggles = crate::testutil::descendants(flow.upcast_ref())
            .iter()
            .filter(|w| w.is::<gtk4::ToggleButton>())
            .count();
        assert!(toggles >= 3, "one toggle per suggestion: {toggles}");
        assert_eq!(
            crate::testutil::selectable_labels_in_click_targets(flow.upcast_ref()),
            Vec::<String>::new()
        );
    }

    use super::super::permission::tests::{path as entry, request_input as permission_input};

    const ALL_FOUR: [Decision; 4] = [
        Decision::Allow,
        Decision::AllowForSession,
        Decision::Deny,
        Decision::Cancel,
    ];

    /// A Codex permission request is the same approval card with a summary of what it asks:
    /// the places grouped one per line, what each button lasts, and no rule machinery.
    pub(crate) fn permission_cards_say_what_is_asked() {
        let input = permission_input(
            serde_json::json!([
                entry("write", "/w/out"),
                entry("read", "/data"),
                entry("deny", "/w/.git"),
                {"access": "read", "target": {"kind": "special", "name": "project", "path": null}},
            ]),
            serde_json::json!(true),
            true,
        );
        let (card, events) = codex_card(permission::TOOL, input.clone(), ALL_FOUR.to_vec(), None);
        assert_eq!(
            card.title.text(),
            "Codex asks to write to 1 place, read 2 places and use the network"
        );
        assert_eq!(card.reason.text(), "The build writes its cache");
        let (places, warning, note, raw_open) = card.permission_view();
        assert_eq!(
            places,
            "Read\n  /data\n  the project\nWrite\n  /w/out\nStill blocked\n  /w/.git\nUse the network"
        );
        assert_eq!(warning, "");
        assert!(note.contains("Allow for this turn") && note.contains("session restarts"));
        assert!(!raw_open, "a request it can say is not dumped open");
        assert!(!card.input_section.root.is_visible(), "no JSON dump");

        // Every grant is a plain button: no hidden More menu, and Allow for this session is
        // one click away, not a menu item.
        assert_eq!(
            card.button_labels(),
            [
                "Stop turn",
                "Deny",
                "Allow for this session",
                "Allow for this turn"
            ]
        );
        assert!(
            row_buttons(&card.buttons)
                .iter()
                .all(|w| !w.is::<gtk4::MenuButton>()),
            "nothing is behind a More menu"
        );
        let suggested: Vec<String> = row_buttons(&card.buttons)
            .iter()
            .filter(|w| w.has_css_class("suggested-action"))
            .filter_map(|w| w.downcast_ref::<gtk4::Button>()?.label())
            .map(|l| l.to_string())
            .collect();
        assert_eq!(suggested, ["Allow for this turn"]);

        // No rule: the dialog has nothing to prefill, and the session grant is sent straight.
        assert_eq!(
            crate::always_allow::dialog_prefill(permission::TOOL, &input),
            ""
        );
        assert!(card.press("Allow for this session"));
        assert!(matches!(
            events.borrow().last(),
            Some(RowEvent::Approve {
                decision: Decision::AllowForSession,
                custom_rule: None,
                ..
            })
        ));
        assert!(card.press("Allow for this turn"));
        assert!(matches!(
            events.borrow().last(),
            Some(RowEvent::Approve {
                decision: Decision::Allow,
                custom_rule: None,
                ..
            })
        ));
        assert!(card.press("Stop turn"));
        assert!(matches!(
            events.borrow().last(),
            Some(RowEvent::Approve {
                decision: Decision::Cancel,
                ..
            })
        ));

        // The outcome lines say how long the grant lasts.
        for (decision, outcome) in [
            (Decision::Allow, "Allowed for this turn"),
            (Decision::AllowForSession, "Allowed for this Codex session"),
            (Decision::Deny, "Denied"),
            (Decision::Cancel, "Turn stopped"),
        ] {
            let (card, _) = codex_card(
                permission::TOOL,
                input.clone(),
                ALL_FOUR.to_vec(),
                Some(decision),
            );
            assert_eq!(card.outcome.text(), outcome);
            assert_eq!(
                card.root.has_css_class("denied"),
                matches!(decision, Decision::Deny | Decision::Cancel)
            );
        }
        // The command card's wording is untouched.
        assert_eq!(button_label("Bash", Decision::Allow), "Allow");
        assert_eq!(outcome_text("Bash", Decision::Allow), "Allowed once");

        // A request that may not last the session offers no session button, even if listed.
        let turn_only = permission_input(
            serde_json::json!([entry("write", "/w/out")]),
            serde_json::json!(null),
            false,
        );
        let (card, _) = codex_card(permission::TOOL, turn_only, ALL_FOUR.to_vec(), None);
        assert_eq!(
            card.button_labels(),
            ["Stop turn", "Deny", "Allow for this turn"]
        );
        assert!(!card.permission_view().2.contains("session"));

        permission_cards_fail_closed();
        permission_lists_are_bounded();
    }

    /// Anything the card cannot say exactly, or that reaches everything, is not made easy to
    /// grant: the raw request opens, only a refusal is offered, or Allow loses its emphasis.
    fn permission_cards_fail_closed() {
        let mut unreadable = permission_input(
            serde_json::json!([{"access": "read", "target": {"kind": "mystery"}}]),
            serde_json::json!(null),
            true,
        );
        unreadable["renderable"] = serde_json::json!(false);
        let (card, _) = codex_card(
            permission::TOOL,
            unreadable.clone(),
            ALL_FOUR.to_vec(),
            None,
        );
        assert_eq!(
            card.button_labels(),
            ["Stop turn", "Deny"],
            "an unreadable request can only be refused, whatever the adapter listed"
        );
        let (places, warning, note, raw_open) = card.permission_view();
        assert_eq!(places, "", "no partial summary");
        assert!(warning.contains("can only be denied"), "{warning}");
        assert_eq!(note, "");
        assert!(raw_open, "the raw request opens by default");
        assert!(card
            .raw_section
            .full
            .borrow()
            .contains("\"kind\": \"mystery\""));
        // The adapter's own refusal list is honoured too.
        let (card, _) = codex_card(
            permission::TOOL,
            unreadable,
            vec![Decision::Deny, Decision::Cancel],
            None,
        );
        assert_eq!(card.button_labels(), ["Stop turn", "Deny"]);

        // The whole filesystem: a plain warning, Allow not suggested, Deny still first.
        let root = permission_input(
            serde_json::json!([{"access": "write", "target": {"kind": "special", "name": "root", "path": null}}]),
            serde_json::json!(null),
            true,
        );
        let (card, _) = codex_card(permission::TOOL, root, ALL_FOUR.to_vec(), None);
        let (places, warning, note, _) = card.permission_view();
        assert!(places.contains("the whole filesystem"), "{places}");
        assert!(warning.contains("whole filesystem"), "{warning}");
        assert!(
            !note.contains("session"),
            "a broad grant is for this turn only: {note}"
        );
        assert!(
            row_buttons(&card.buttons)
                .iter()
                .all(|w| !w.has_css_class("suggested-action")),
            "Allow is not the suggested action"
        );
        assert_eq!(card.button_labels()[1], "Deny");

        // The adapter's own warning is shown as written.
        let mut warned = permission_input(
            serde_json::json!([entry("write", "/home/me")]),
            serde_json::json!(null),
            false,
        );
        warned["warning"] = serde_json::json!("This includes your home folder.");
        let (card, _) = codex_card(permission::TOOL, warned, ALL_FOUR.to_vec(), None);
        assert!(card.permission_view().1.contains("home folder"));
    }

    /// Two hundred paths do not make a two-hundred-line card: twelve lines, then Show all.
    fn permission_lists_are_bounded() {
        let many: Vec<Value> = (0..200)
            .map(|i| entry("write", &format!("/w/out/{i:03}")))
            .collect();
        let input = permission_input(Value::Array(many), serde_json::json!(null), true);
        let (card, _) = codex_card(permission::TOOL, input, ALL_FOUR.to_vec(), None);
        assert_eq!(
            card.title.text(),
            "Codex asks to write to 200 places",
            "the count is in the title"
        );
        let shown = card.places.body.text().to_string();
        assert!(shown.lines().count() <= APPROVAL_VISIBLE_LINES, "{shown}");
        assert!(shown.ends_with("more lines in Show all"), "{shown}");
        assert!(card.places.more.is_visible());
        card.places.more.emit_clicked();
        assert_eq!(card.places.body.text().lines().count(), 201);
        assert_eq!(card.places.more.label().as_deref(), Some("Show fewer"));
        card.places.more.emit_clicked();
        assert!(card.places.body.text().lines().count() <= APPROVAL_VISIBLE_LINES);
    }

    /// Buttons wrap instead of setting the transcript's minimum width.
    pub(crate) fn approval_buttons_wrap() {
        let (card, _) = codex_card(
            "command_execution",
            serde_json::json!({"command": "ls"}),
            vec![
                Decision::Allow,
                Decision::AllowAlways,
                Decision::AllowForSession,
                Decision::Deny,
                Decision::Cancel,
            ],
            None,
        );
        let width = card.root.measure(gtk4::Orientation::Horizontal, -1).0;
        // The buttons side by side, from each one's own natural width: a flow box's own
        // natural width differs between GTK versions (CI's GTK reports one row's worth less).
        let mut in_a_row = 0;
        let mut child = card.buttons.first_child();
        while let Some(c) = child {
            in_a_row += c.measure(gtk4::Orientation::Horizontal, -1).1;
            child = c.next_sibling();
        }
        assert!(
            width < in_a_row,
            "the card's minimum width ({width}) must be below the buttons in one row ({in_a_row})"
        );
    }
}
