//! The background task explorer: a button on the thinking strip listing what runs in the
//! background (sub-agents, shell commands), and a dialog that follows one shell task's output.
//!
//! The list opens a sub-agent in the sub-agent panel; a shell task opens [`TaskOutputPanel`],
//! which the view fills by polling the agent (`Control::TaskOutput`) while the dialog is open.
//! The agent hands back only the end of the output (Claude: the last 8 KiB), so the panel says
//! when it shows part of it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::AdwDialogExt;
use agent_core::event::{BackgroundTask, BackgroundTaskKind};
use gtk4::prelude::*;

use super::cards::label;
use super::model::task_name;
use super::payload::TaskOutput;

/// What a task is, as its row says.
pub fn kind_text(kind: BackgroundTaskKind) -> &'static str {
    match kind {
        BackgroundTaskKind::Agent => "Sub-agent",
        BackgroundTaskKind::Shell => "Shell command",
        BackgroundTaskKind::Other => "Task",
    }
}

fn kind_icon(kind: BackgroundTaskKind) -> &'static str {
    match kind {
        BackgroundTaskKind::Agent => "agent-subagent-symbolic",
        BackgroundTaskKind::Shell | BackgroundTaskKind::Other => "at-utilities-terminal-symbolic",
    }
}

/// One task as the list shows it; `openable`: selecting it shows something.
#[derive(Debug, Clone, PartialEq)]
pub struct ListedTask {
    pub task: BackgroundTask,
    pub openable: bool,
}

/// The strip's button and its popover list of background tasks.
pub struct BackgroundButton {
    button: gtk4::MenuButton,
    list: gtk4::ListBox,
    popover: gtk4::Popover,
    tasks: RefCell<Vec<ListedTask>>,
}

impl BackgroundButton {
    /// `open` is called with the task the user picked.
    pub fn new(open: impl Fn(&BackgroundTask) + 'static) -> Rc<Self> {
        let list = gtk4::ListBox::new();
        list.add_css_class("subagent-list");
        list.set_selection_mode(gtk4::SelectionMode::None);
        let scroller = gtk4::ScrolledWindow::new();
        scroller.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        scroller.set_propagate_natural_height(true);
        scroller.set_propagate_natural_width(true);
        scroller.set_max_content_height(360);
        scroller.set_child(Some(&list));
        let popover = gtk4::Popover::new();
        popover.set_child(Some(&scroller));
        let button = gtk4::MenuButton::new();
        button.set_label("Show tasks");
        button.add_css_class("flat");
        button.set_valign(gtk4::Align::Center);
        button.set_tooltip_text(Some("What runs in the background, and its output"));
        button.set_popover(Some(&popover));
        button.set_visible(false);
        let this = Rc::new(Self {
            button,
            list,
            popover,
            tasks: RefCell::new(Vec::new()),
        });
        let weak = Rc::downgrade(&this);
        this.list.connect_row_activated(move |_, row| {
            let Some(this) = weak.upgrade() else { return };
            let picked = usize::try_from(row.index())
                .ok()
                .and_then(|i| this.tasks.borrow().get(i).cloned())
                .filter(|t| t.openable);
            if let Some(picked) = picked {
                this.popover.popdown();
                open(&picked.task);
            }
        });
        this
    }

    pub fn widget(&self) -> &gtk4::MenuButton {
        &self.button
    }

    /// Shows `tasks`, or hides the button when there are none. Rows are rebuilt only when the
    /// list changed.
    pub fn set(&self, tasks: Vec<ListedTask>) {
        self.button.set_visible(!tasks.is_empty());
        if tasks.is_empty() {
            self.popover.popdown();
        }
        self.button
            .update_property(&[gtk4::accessible::Property::Label(&match tasks.len() {
                1 => "Show the background task".to_owned(),
                n => format!("Show the {n} background tasks"),
            })]);
        if *self.tasks.borrow() == tasks {
            return;
        }
        while let Some(row) = self.list.first_child() {
            self.list.remove(&row);
        }
        for listed in &tasks {
            self.list.append(&row(listed));
        }
        *self.tasks.borrow_mut() = tasks;
    }

    /// The listed task names and whether each opens, in order (tests).
    #[cfg(test)]
    pub fn listed(&self) -> Vec<(String, bool)> {
        self.tasks
            .borrow()
            .iter()
            .map(|t| (task_name(&t.task), t.openable))
            .collect()
    }
}

fn row(listed: &ListedTask) -> gtk4::ListBoxRow {
    let task = &listed.task;
    let b = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    b.set_margin_top(6);
    b.set_margin_bottom(6);
    b.set_margin_start(8);
    b.set_margin_end(8);
    let icon = gtk4::Image::from_icon_name(kind_icon(task.kind));
    icon.set_valign(gtk4::Align::Start);
    b.append(&icon);
    let text = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    let name = label(&task_name(task), &["subagent-name"]);
    name.set_xalign(0.0);
    name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    name.set_width_chars(16);
    name.set_max_width_chars(48);
    text.append(&name);
    let kind = if listed.openable {
        kind_text(task.kind).to_owned()
    } else {
        format!("{} · nothing to show", kind_text(task.kind))
    };
    let kind = label(&kind, &["subagent-status", "dim-label"]);
    kind.set_xalign(0.0);
    text.append(&kind);
    b.append(&text);
    let row = gtk4::ListBoxRow::new();
    row.set_child(Some(&b));
    row.set_activatable(listed.openable);
    row
}

/// One background shell task's output, in a dialog that follows it until closed.
pub struct TaskOutputPanel {
    dialog: adw::Dialog,
    task_id: String,
    status: gtk4::Label,
    note: gtk4::Label,
    buffer: gtk4::TextBuffer,
    scroller: gtk4::ScrolledWindow,
    /// A read is out: the next poll waits for it.
    in_flight: Cell<bool>,
    /// The task has left the list; one last read, then the poll stops.
    ended: Cell<bool>,
    closed: Cell<bool>,
}

impl TaskOutputPanel {
    pub fn new(task: &BackgroundTask) -> Rc<Self> {
        let dialog = adw::Dialog::new();
        dialog.set_content_width(760);
        dialog.set_content_height(560);
        dialog.set_title(&task_name(task));
        let body = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
        body.set_margin_top(12);
        body.set_margin_bottom(18);
        body.set_margin_start(18);
        body.set_margin_end(18);
        let title = label(&task_name(task), &["subagent-panel-title"]);
        title.set_xalign(0.0);
        title.set_wrap(true);
        title.set_selectable(true);
        let status = label("Reading output…", &["dim-label"]);
        status.set_xalign(0.0);
        body.append(&title);
        body.append(&status);
        let buffer = gtk4::TextBuffer::new(None);
        let view = gtk4::TextView::with_buffer(&buffer);
        view.set_editable(false);
        view.set_cursor_visible(false);
        view.set_monospace(true);
        view.set_wrap_mode(gtk4::WrapMode::WordChar);
        view.add_css_class("code-view");
        view.set_top_margin(10);
        view.set_bottom_margin(10);
        view.set_left_margin(12);
        view.set_right_margin(12);
        let scroller = gtk4::ScrolledWindow::new();
        scroller.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        scroller.set_vexpand(true);
        scroller.add_css_class("code-block");
        scroller.set_child(Some(&view));
        body.append(&scroller);
        let note = label("", &["dim-label", "caption"]);
        note.set_xalign(0.0);
        note.set_visible(false);
        body.append(&note);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&body));
        dialog.set_child(Some(&toolbar));
        let this = Rc::new(Self {
            dialog,
            task_id: task.id.clone(),
            status,
            note,
            buffer,
            scroller,
            in_flight: Cell::new(false),
            ended: Cell::new(false),
            closed: Cell::new(false),
        });
        let weak = Rc::downgrade(&this);
        this.dialog.connect_closed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.closed.set(true);
            }
        });
        this
    }

    pub fn dialog(&self) -> &adw::Dialog {
        &self.dialog
    }

    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub fn closed(&self) -> bool {
        self.closed.get()
    }

    /// Starts a read unless one is out; false when it should not (one is out, or the panel is
    /// done).
    pub fn begin_read(&self) -> bool {
        if self.closed.get() || self.in_flight.get() {
            return false;
        }
        self.in_flight.set(true);
        true
    }

    /// The task left the background list: the next read is the last.
    pub fn mark_ended(&self) {
        self.ended.set(true);
    }

    pub fn ended(&self) -> bool {
        self.ended.get()
    }

    /// Shows a read's result: the output (following its end unless the user scrolled up), or
    /// why it could not be read.
    pub fn show(&self, result: Result<TaskOutput, String>) {
        self.in_flight.set(false);
        let state = if self.ended.get() {
            "Finished"
        } else {
            "Running · updates every second"
        };
        match result {
            Ok(out) => {
                let shown = {
                    let (start, end) = self.buffer.bounds();
                    self.buffer.text(&start, &end, false).to_string()
                };
                // An ended task whose output file is gone reads as empty: keep what was shown.
                if out.text.is_empty() && !shown.is_empty() {
                    self.status.set_text(state);
                    return;
                }
                let text = plain_text(&out.text);
                if shown != text {
                    let adj = self.scroller.vadjustment();
                    let following = adj.value() + adj.page_size() >= adj.upper() - 4.0;
                    self.buffer.set_text(&text);
                    if following {
                        let mut end = self.buffer.end_iter();
                        if let Some(view) = self.scroller.child().and_downcast::<gtk4::TextView>() {
                            view.scroll_to_iter(&mut end, 0.0, false, 0.0, 1.0);
                        }
                    }
                }
                self.status.set_text(if text.is_empty() {
                    if self.ended.get() {
                        "Finished without output"
                    } else {
                        "Running · no output yet"
                    }
                } else {
                    state
                });
                let note = size_note(&out);
                self.note.set_visible(note.is_some());
                self.note.set_text(note.as_deref().unwrap_or_default());
            }
            Err(error) => {
                self.status
                    .set_text(&format!("Could not read the output: {error}"));
            }
        }
    }

    /// The shown text and status (tests).
    #[cfg(test)]
    pub fn shown(&self) -> (String, String) {
        let (start, end) = self.buffer.bounds();
        (
            self.buffer.text(&start, &end, false).to_string(),
            self.status.text().to_string(),
        )
    }
}

/// "Showing the last 8.0 KiB of 120.4 KiB" when the reply holds only the end of the output.
pub fn size_note(out: &TaskOutput) -> Option<String> {
    if !out.truncated {
        return None;
    }
    let kib = |b: usize| format!("{:.1} KiB", b as f64 / 1024.0);
    Some(match out.total_bytes {
        Some(total) => format!(
            "Showing the last {} of {}",
            kib(out.text.len()),
            kib(usize::try_from(total).unwrap_or(usize::MAX))
        ),
        None => format!("Showing the last {}", kib(out.text.len())),
    })
}

/// Terminal output as plain text: escape sequences (colours, cursor moves, titles) dropped, and
/// a line redrawn with carriage returns (a progress bar) kept as its last state.
pub fn plain_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameters and intermediates, then one final byte in @..~.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // OSC (titles, hyperlinks): up to BEL or ST (ESC \).
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // A two-byte sequence (charset selection, keypad mode, ...).
            _ => {}
        }
    }
    out.split('\n')
        .map(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            line.rsplit('\r').next().unwrap_or(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_drops_escapes_and_keeps_a_redrawn_line_last_state() {
        assert_eq!(
            plain_text("\u{1b}[1;32mok\u{1b}[0m done\r\n"),
            "ok done\n",
            "colours go, CRLF is a newline"
        );
        assert_eq!(
            plain_text("\u{1b}]0;title\u{7}a\u{1b}]8;;http://x\u{1b}\\b"),
            "ab",
            "titles and hyperlinks go"
        );
        assert_eq!(plain_text(" 10%\r 50%\r100%\nnext"), "100%\nnext");
        assert_eq!(plain_text("é ✓ plain"), "é ✓ plain");
        assert_eq!(
            plain_text("cut \u{1b}["),
            "cut ",
            "a cut-off sequence ends the text"
        );
    }

    #[test]
    fn size_note_speaks_only_for_a_cut_output() {
        let mut out = TaskOutput {
            text: "x".repeat(8192),
            total_bytes: Some(123_290),
            truncated: false,
        };
        assert_eq!(size_note(&out), None);
        out.truncated = true;
        assert_eq!(
            size_note(&out).as_deref(),
            Some("Showing the last 8.0 KiB of 120.4 KiB")
        );
        out.total_bytes = None;
        assert_eq!(size_note(&out).as_deref(), Some("Showing the last 8.0 KiB"));
    }
}
