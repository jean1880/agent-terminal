//! Transcript row widgets: bubbles, markdown, reasoning, tool cards, approvals, questions and
//! the inline dividers. Each row is built once from its [`Item`] and then updated in place, so
//! a streaming message only touches the widgets of the blocks that changed.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use agent_core::adapter::Driver;
use agent_core::event::{Decision, ItemKind};
use gtk4::glib;
use gtk4::prelude::*;
use serde_json::Value;
use sourceview5::prelude::*;

use super::difftext::{self, DiffText};
use super::markdown::{self, Block, TextKind};
use super::model::{format_tokens, ApprovalState, Body, Item, QuestionState, Tone, ToolStatus};
use super::payload;
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
    },
    Answer {
        request: String,
        answers: Value,
    },
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

pub fn driver_name(driver: Driver) -> &'static str {
    driver.info().label
}

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

pub fn label(text: &str, classes: &[&str]) -> gtk4::Label {
    let l = gtk4::Label::new(Some(text));
    l.set_xalign(0.0);
    for c in classes {
        l.add_css_class(c);
    }
    l
}

fn wrapping(l: &gtk4::Label) {
    l.set_wrap(true);
    l.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
    l.set_hexpand(true);
}

/// A selectable, wrapping monospace label for tool input and output.
fn mono(text: &str) -> gtk4::Label {
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

/// A code block: language label + copy button over a read-only, highlighted source view.
fn code_block(lang: Option<&str>, text: &str) -> BlockWidget {
    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    root.add_css_class("code-block");

    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    header.add_css_class("code-header");
    let lang_label = label(lang.unwrap_or("text"), &["code-lang"]);
    lang_label.set_hexpand(true);
    header.append(&lang_label);
    let copy = gtk4::Button::from_icon_name("at-edit-copy-symbolic");
    copy.add_css_class("flat");
    copy.add_css_class("code-copy");
    copy.set_tooltip_text(Some("Copy"));
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

    copy.connect_clicked(glib::clone!(
        #[weak]
        buffer,
        #[weak]
        copy,
        move |_| {
            let (start, end) = buffer.bounds();
            copy.clipboard().set_text(&buffer.text(&start, &end, false));
            copy.set_icon_name("at-object-select-symbolic");
            glib::timeout_add_local_once(
                std::time::Duration::from_millis(1200),
                glib::clone!(
                    #[weak]
                    copy,
                    move || copy.set_icon_name("at-edit-copy-symbolic")
                ),
            );
        }
    ));

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
    Approval(ApprovalCard),
    Question(QuestionCardWidget),
}

impl Row {
    pub fn build(item: &Item, sink: &RowSink) -> Self {
        let row = match &item.body {
            Body::User { .. } => {
                let root = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
                root.add_css_class("user-row");
                root.set_halign(gtk4::Align::End);
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
                head.append(&label(driver_name(*driver), &["agent-name"]));
                root.append(&head);
                let md = MarkdownView::new();
                root.append(md.widget());
                let caret = label("▍", &["stream-caret"]);
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
            } => {
                let text = switch_text(*driver, model.as_deref(), *agent_changed);
                Self::Static(divider(&text, "switch-divider", Some(*driver)))
            }
            Body::Approval(_) => Self::Approval(ApprovalCard::new(sink)),
            Body::Question(q) => Self::Question(QuestionCardWidget::new(q, sink)),
        };
        row.update(item);
        row
    }

    pub fn widget(&self) -> gtk4::Widget {
        match self {
            Self::User { root, .. } | Self::Assistant { root, .. } => root.clone().upcast(),
            Self::Reasoning(r) => r.root.clone().upcast(),
            Self::Tool(t) => t.root.clone().upcast(),
            Self::Static(w) => w.clone(),
            Self::Approval(a) => a.root.clone().upcast(),
            Self::Question(q) => q.root.clone().upcast(),
        }
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
            (Self::User { text: l, .. }, Body::User { text }) => l.set_text(text),
            (
                Self::Assistant { md, caret, .. },
                Body::Assistant {
                    text, streaming, ..
                },
            ) => {
                md.set_markdown(text);
                caret.set_visible(*streaming);
            }
            (Self::Reasoning(r), Body::Reasoning { text, streaming }) => {
                r.update(text, *streaming, item.expanded);
            }
            (Self::Tool(t), Body::Tool(_)) => t.update(item),
            (Self::Approval(a), Body::Approval(_)) => a.update(item),
            (Self::Question(q), Body::Question(card)) => q.update(card.state),
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

pub fn switch_text(driver: Driver, model: Option<&str>, agent_changed: bool) -> String {
    match (agent_changed, model) {
        (true, Some(m)) => format!("Continued in {} · {m}", driver_name(driver)),
        (true, None) => format!("Continued in {}", driver_name(driver)),
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
    col.append(&label("Turn failed", &["error-title"]));
    let l = label(message, &["error-text"]);
    wrapping(&l);
    l.set_selectable(true);
    col.append(&l);
    root.append(&col);
    root
}

/// A centred label between two rules.
fn divider(text: &str, class: &str, driver: Option<Driver>) -> gtk4::Widget {
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
    pill.append(&label(text, &["divider-text"]));
    root.append(&left);
    root.append(&pill);
    root.append(&right);
    root.upcast()
}

// ---------------------------------------------------------------------------------------------
// Reasoning
// ---------------------------------------------------------------------------------------------

pub struct ReasoningRow {
    root: gtk4::Box,
    title: gtk4::Label,
    chevron: gtk4::Image,
    revealer: gtk4::Revealer,
    text: gtk4::Label,
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
        let text = label("", &["reasoning-text"]);
        wrapping(&text);
        text.set_selectable(true);
        let revealer = gtk4::Revealer::new();
        revealer.set_transition_type(gtk4::RevealerTransitionType::SlideDown);
        revealer.set_child(Some(&text));
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
            title,
            chevron,
            revealer,
            text,
        }
    }

    fn update(&self, text: &str, streaming: bool, expanded: bool) {
        let text = text.trim();
        // Claude often withholds its reasoning: the thinking block arrives (signature only) but
        // no text ever streams. Say so rather than open onto nothing.
        let withheld = !streaming && text.is_empty();
        self.title.set_text(if streaming {
            "Thinking…"
        } else if withheld {
            "Thought process (not shared by the model)"
        } else {
            "Thought process"
        });
        self.text.set_text(text);
        self.revealer.set_reveal_child(expanded && !withheld);
        self.chevron.set_visible(!withheld);
        if let Some(toggle) = self.chevron.parent().and_then(|head| head.parent()) {
            toggle.set_sensitive(!withheld);
        }
        self.chevron.set_icon_name(Some(if expanded {
            "at-pan-down-symbolic"
        } else {
            "at-pan-end-symbolic"
        }));
    }

    /// The card's title and whether it can be opened (tests).
    #[cfg(test)]
    pub(super) fn reasoning_state(&self) -> (String, bool) {
        (self.title.text().to_string(), self.chevron.is_visible())
    }
}

// ---------------------------------------------------------------------------------------------
// Tool card
// ---------------------------------------------------------------------------------------------

pub struct ToolCard {
    root: gtk4::Box,
    status: gtk4::Stack,
    spinner: gtk4::Spinner,
    status_icon: gtk4::Image,
    kind_icon: gtk4::Image,
    title: gtk4::Label,
    summary: gtk4::Label,
    badge: gtk4::Label,
    chevron: gtk4::Image,
    revealer: gtk4::Revealer,
    input_box: gtk4::Box,
    input: gtk4::Label,
    output_box: gtk4::Box,
    output: gtk4::Label,
    error: gtk4::Label,
    children: gtk4::Box,
    last_status: Cell<Option<ToolStatus>>,
    /// File-change cards only: "View diff" and "Open in ‹tool›", and the diff they reveal.
    diff: FileDiffUi,
}

/// The diff part of a file-change card.
struct FileDiffUi {
    bar: gtk4::Box,
    toggle: gtk4::ToggleButton,
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

fn section(title: &str, body: &gtk4::Label) -> gtk4::Box {
    let b = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    b.add_css_class("card-section");
    b.append(&label(title, &["section-title"]));
    // No nested scroller: its minimum height padded one-line outputs to three lines, and a
    // scroller inside the transcript steals wheel events. Long text is cut by lines instead
    // (`payload::cap_lines`).
    let frame = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    frame.add_css_class("mono-scroller");
    frame.append(body);
    b.append(&frame);
    b
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
        let spinner = gtk4::Spinner::new();
        let status_icon = gtk4::Image::new();
        status_icon.add_css_class("status-icon");
        status.add_named(&spinner, Some("spinner"));
        status.add_named(&status_icon, Some("icon"));
        let kind_icon = gtk4::Image::new();
        kind_icon.add_css_class("kind-icon");
        let title = label("", &["card-title"]);
        let summary = label("", &["card-summary"]);
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

        let body = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
        body.add_css_class("card-body");
        let input = mono("");
        let input_box = section("Input", &input);
        let output = mono("");
        let output_box = section("Output", &output);
        let error = label("", &["card-error"]);
        wrapping(&error);
        error.set_selectable(true);
        let children = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        children.add_css_class("card-children");
        body.append(&input_box);
        body.append(&children);
        body.append(&output_box);
        body.append(&error);
        let revealer = gtk4::Revealer::new();
        revealer.set_transition_type(gtk4::RevealerTransitionType::SlideDown);
        revealer.set_child(Some(&body));
        root.append(&revealer);

        let id = id.to_owned();
        let sink = sink.clone();
        header.connect_clicked(glib::clone!(
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
            input,
            output_box,
            output,
            error,
            children,
            last_status: Cell::new(None),
            diff,
        }
    }

    fn build_diff_ui(id: &str, sink: &RowSink) -> FileDiffUi {
        let bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        bar.add_css_class("diff-bar");
        bar.set_visible(false);
        let toggle = gtk4::ToggleButton::with_label("View diff");
        toggle.add_css_class("flat");
        toggle.add_css_class("diff-toggle");
        let open = gtk4::Button::new();
        open.add_css_class("flat");
        open.add_css_class("diff-open");
        let open_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        open_row.append(&gtk4::Image::from_icon_name(crate::icons::EXTERNAL_ICON));
        let open_label = label("", &[]);
        open_row.append(&open_label);
        open.set_child(Some(&open_row));
        bar.append(&toggle);
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
            open,
            open_label,
            revealer,
            status,
            text,
        }
    }

    /// Shows the diff the host computed for this card.
    pub fn show_diff(&self, reply: &DiffReply) {
        let ui = &self.diff;
        if !ui.toggle.is_active() {
            return; // closed again before the answer arrived
        }
        match reply {
            Ok(shown) if shown.text.trim().is_empty() => {
                ui.status.set_text("No changes");
                ui.status.set_visible(true);
                ui.text.widget().set_visible(false);
            }
            Ok(shown) => {
                ui.text.set(
                    &shown.text,
                    &difftext::notes_for(shown.origin, shown.omitted_lines),
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
    #[cfg(test)]
    pub(super) fn click_view_diff(&self) {
        self.diff.toggle.set_active(true);
    }

    fn update(&self, item: &Item) {
        let Body::Tool(tool) = &item.body else {
            return;
        };
        let is_edit = tool.kind == ItemKind::FileChange;
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
        self.title.set_text(&tool.title);
        let summary = payload::tool_summary(tool.kind, tool.input.as_ref(), &tool.input_text);
        self.summary.set_text(summary.as_deref().unwrap_or(""));
        let input = payload::tool_input_text(tool.kind, tool.input.as_ref(), &tool.input_text);
        self.input_box.set_visible(!input.trim().is_empty());
        self.input
            .set_text(&payload::cap_lines(input.trim_end(), MAX_CARD_LINES));
        self.output_box.set_visible(!tool.output.trim().is_empty());
        self.output
            .set_text(&payload::cap_lines(tool.output.trim_end(), MAX_CARD_LINES));
        match &tool.error {
            Some(e) if !e.trim().is_empty() => {
                self.error.set_text(e.trim());
                self.error.set_visible(true);
            }
            _ => self.error.set_visible(false),
        }
        self.children.set_visible(!item.children.is_empty());
        self.revealer.set_reveal_child(item.expanded);
        self.chevron.set_icon_name(Some(if item.expanded {
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
    input: gtk4::Label,
    /// A file-edit request's proposed change, as a diff (instead of the tool-input JSON).
    diff: DiffText,
    /// The JSON behind that diff, one click away.
    raw: gtk4::Expander,
    raw_text: gtk4::Label,
    buttons: gtk4::Box,
    outcome: gtk4::Label,
    request: RefCell<String>,
    built_buttons: Cell<bool>,
    sink: RowSink,
}

pub fn decision_label(d: Decision) -> &'static str {
    match d {
        Decision::Allow => "Allow",
        Decision::AllowForSession => "Allow for session",
        Decision::Deny => "Deny",
        Decision::Cancel => "Cancel",
    }
}

fn decision_outcome(d: Decision) -> &'static str {
    match d {
        Decision::Allow => "Allowed once",
        Decision::AllowForSession => "Allowed for this session",
        Decision::Deny => "Denied",
        Decision::Cancel => "Cancelled",
    }
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
        let title = label("", &["approval-title"]);
        wrapping(&title);
        let reason = label("", &["approval-reason"]);
        wrapping(&reason);
        col.append(&title);
        col.append(&reason);
        head.append(&col);
        root.append(&head);
        let input = mono("");
        input.add_css_class("approval-input");
        root.append(&input);
        let diff = DiffText::new();
        diff.widget().set_visible(false);
        root.append(diff.widget());
        let raw_text = mono("");
        let raw = gtk4::Expander::new(Some("Show raw"));
        raw.add_css_class("approval-raw");
        raw.set_child(Some(&raw_text));
        raw.set_visible(false);
        root.append(&raw);
        let footer = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let outcome = label("", &["approval-outcome"]);
        outcome.set_hexpand(true);
        let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        buttons.set_halign(gtk4::Align::End);
        footer.append(&outcome);
        footer.append(&buttons);
        root.append(&footer);
        Self {
            root,
            title,
            reason,
            input,
            diff,
            raw,
            raw_text,
            buttons,
            outcome,
            request: RefCell::new(String::new()),
            built_buttons: Cell::new(false),
            sink: sink.clone(),
        }
    }

    /// What the card shows of a file edit (tests): the files line, whether the diff and the "Show
    /// raw" expander are visible, and the diff's text.
    #[cfg(test)]
    pub(super) fn diff_view(&self) -> (String, bool, bool, String) {
        (
            self.input.text().to_string(),
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
        let title = a
            .title
            .clone()
            .unwrap_or_else(|| format!("Allow {}?", a.tool));
        self.title.set_text(&title);
        self.reason.set_text(a.reason.as_deref().unwrap_or(""));
        self.reason.set_visible(a.reason.is_some());
        // A file edit is shown as the diff it proposes (computed from the request alone), with
        // the JSON behind it one click away. Commands and everything else keep their input text.
        let preview = agent_kit::editdiff::preview_from_input(&a.input);
        let input = if preview.is_empty() {
            match &a.input {
                Value::Null => String::new(),
                v => {
                    let kind = if a.tool.eq_ignore_ascii_case("bash") || a.tool == "run_command" {
                        ItemKind::Command
                    } else {
                        ItemKind::Tool
                    };
                    payload::tool_input_text(kind, Some(v), "")
                }
            }
        } else {
            preview
                .iter()
                .map(|e| format!("{}  +{} −{}", e.path, e.added, e.removed))
                .collect::<Vec<_>>()
                .join("\n")
        };
        self.input.set_visible(!input.is_empty());
        self.input.set_text(&input);
        if preview.is_empty() {
            self.diff.widget().set_visible(false);
            self.raw.set_visible(false);
        } else {
            let text: String = preview.iter().map(|e| e.diff.as_str()).collect();
            let (kept, omitted) = agent_kit::diff::truncate(
                &text,
                agent_kit::diff::MAX_DIFF_BYTES,
                agent_kit::diff::MAX_DIFF_LINES,
            );
            self.diff.set(
                kept,
                &difftext::notes_for(agent_kit::filediff::Origin::AgentEdit, omitted)
                    .into_iter()
                    .map(|n| n.replace("from the agent's edit", "Proposed change"))
                    .collect::<Vec<_>>(),
            );
            self.diff.widget().set_visible(true);
            let raw = serde_json::to_string_pretty(&a.input).unwrap_or_default();
            self.raw_text
                .set_text(&payload::cap_lines(&raw, MAX_CARD_LINES));
            self.raw.set_visible(true);
        }

        if !self.built_buttons.get() {
            self.built_buttons.set(true);
            // Deny first, the safest default on the right.
            let mut order = a.options.clone();
            order.sort_by_key(|d| match d {
                Decision::Cancel => 0,
                Decision::Deny => 1,
                Decision::AllowForSession => 2,
                Decision::Allow => 3,
            });
            for d in order {
                let b = gtk4::Button::with_label(decision_label(d));
                b.add_css_class("pill");
                match d {
                    Decision::Allow => b.add_css_class("suggested-action"),
                    Decision::Deny => b.add_css_class("destructive-action"),
                    _ => {}
                }
                let sink = self.sink.clone();
                let request = a.request.clone();
                b.connect_clicked(move |_| {
                    sink(RowEvent::Approve {
                        request: request.clone(),
                        decision: d,
                    });
                });
                self.buttons.append(&b);
            }
        }

        for class in ["pending", "resolved", "expired"] {
            self.root.remove_css_class(class);
        }
        let (class, outcome, sensitive, show_buttons) = match a.state {
            ApprovalState::Pending => ("pending", String::new(), true, true),
            ApprovalState::Sent(d) => ("pending", format!("{}…", decision_label(d)), false, true),
            ApprovalState::Resolved(d) => {
                ("resolved", decision_outcome(d).to_owned(), false, false)
            }
            ApprovalState::Expired => (
                "expired",
                "Expired: the agent that asked has exited".to_owned(),
                false,
                false,
            ),
        };
        self.root.add_css_class(class);
        self.outcome.set_text(&outcome);
        self.buttons.set_sensitive(sensitive);
        self.buttons.set_visible(show_buttons);
    }
}

// ---------------------------------------------------------------------------------------------
// Question card
// ---------------------------------------------------------------------------------------------

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
        let head = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
        head.append(&gtk4::Image::from_icon_name("at-dialog-question-symbolic"));
        head.append(&label("The agent has a question", &["question-heading"]));
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
                let chip = label(&q.header, &["question-chip"]);
                chip.set_halign(gtk4::Align::Start);
                qbox.append(&chip);
            }
            let text = label(&q.question, &["question-text"]);
            wrapping(&text);
            qbox.append(&text);
            if q.multi_select {
                qbox.append(&label("Choose any", &["question-hint"]));
            }
            let flow = gtk4::FlowBox::new();
            flow.set_selection_mode(gtk4::SelectionMode::None);
            flow.set_max_children_per_line(4);
            flow.set_column_spacing(8);
            flow.set_row_spacing(8);
            flow.set_homogeneous(false);
            let mut group: Option<gtk4::ToggleButton> = None;
            for (oi, opt) in q.options.iter().enumerate() {
                let b = gtk4::ToggleButton::new();
                b.add_css_class("option-button");
                let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
                inner.append(&label(&opt.label, &["option-label"]));
                if let Some(d) = &opt.description {
                    let dl = label(d, &["option-desc"]);
                    wrapping(&dl);
                    dl.set_max_width_chars(36);
                    inner.append(&dl);
                }
                b.set_child(Some(&inner));
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
        };
        self.status.set_text(text);
        self.submit.set_visible(open);
        self.options.set_sensitive(open);
        if !open {
            self.root.add_css_class("resolved");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            switch_text(Driver::Agy, Some("gemini-3.1-pro"), true),
            "Continued in Antigravity · gemini-3.1-pro"
        );
        assert_eq!(
            switch_text(Driver::Claude, Some("sonnet"), false),
            "Model switched to sonnet"
        );
    }

    #[test]
    fn languages_normalise() {
        assert_eq!(normalise_lang("RS"), "rust");
        assert_eq!(normalise_lang("bash"), "sh");
        assert_eq!(normalise_lang("toml"), "toml");
    }
}
