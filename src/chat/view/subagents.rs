//! The sub-agent explorer: a header button listing every sub-agent the thread started (nested ones
//! included) with its live status, and a dialog that shows one of them on its own: what it was
//! asked, every step it took (the same cards as the transcript) and what it reported.
//!
//! Both update in place, like the transcript: the list rebuilds its rows only when the set of
//! sub-agents changes and otherwise just sets their labels; the panel keeps one widget per step,
//! adds the steps that are new and updates the ones that changed, so scrolling, selection and
//! an expanded card survive a running sub-agent. Its cards send their events (expand, approve,
//! load a diff) through the view's own row sink, exactly like the transcript's.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::AdwDialogExt;
use gtk4::prelude::*;

use super::cards::{label, wrapping, Row, RowSink};
use super::model::{Body, SubagentSummary, ToolStatus, Transcript};

/// `running`, `done · 12 steps`.
pub fn status_text(s: &SubagentSummary) -> String {
    let state = match s.status {
        ToolStatus::Running => "running",
        ToolStatus::Completed => "done",
        ToolStatus::Failed => "failed",
        ToolStatus::Declined => "declined",
        ToolStatus::Interrupted => "stopped",
    };
    match s.steps {
        0 => state.to_owned(),
        1 => format!("{state} · 1 step"),
        n => format!("{state} · {n} steps"),
    }
}

/// The labels of one list row, set in place.
struct ListRow {
    name: gtk4::Label,
    status: gtk4::Label,
    task: gtk4::Label,
}

impl ListRow {
    fn new() -> (gtk4::ListBoxRow, Self) {
        let b = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        b.add_css_class("subagent-row");
        b.set_margin_top(6);
        b.set_margin_bottom(6);
        b.set_margin_start(10);
        b.set_margin_end(10);
        let top = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let name = label("", &["subagent-name"]);
        name.set_hexpand(true);
        name.set_xalign(0.0);
        top.append(&name);
        let status = label("", &["subagent-status", "dim-label"]);
        top.append(&status);
        b.append(&top);
        let task = label("", &["subagent-task", "dim-label"]);
        task.set_xalign(0.0);
        task.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        b.append(&task);
        let row = gtk4::ListBoxRow::new();
        row.set_child(Some(&b));
        (row, Self { name, status, task })
    }

    fn set(&self, agent: &SubagentSummary) {
        set_if_changed(&self.name, &agent.name);
        set_if_changed(&self.status, &status_text(agent));
        set_if_changed(&self.task, &agent.task);
        self.task.set_visible(!agent.task.is_empty());
    }
}

/// Leaves a label alone when its text is already right (no relayout, no lost selection).
fn set_if_changed(l: &gtk4::Label, text: &str) {
    if l.text() != text {
        l.set_text(text);
    }
}

/// The header's "Sub-agents" button and its list.
pub struct SubagentButton {
    root: gtk4::MenuButton,
    count: gtk4::Label,
    list: gtk4::ListBox,
    popover: gtk4::Popover,
    /// The ids behind the list's rows, in order, and their labels.
    rows: RefCell<Vec<(String, ListRow)>>,
}

impl SubagentButton {
    /// `open` is called with a sub-agent's item id when its row is chosen.
    pub fn new(open: impl Fn(&str) + 'static) -> Rc<Self> {
        let root = gtk4::MenuButton::new();
        root.add_css_class("flat");
        root.add_css_class("subagent-button");
        root.set_tooltip_text(Some("Sub-agents this thread started"));
        let inner = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        inner.append(&gtk4::Image::from_icon_name(crate::icons::SUBAGENT_ICON));
        let count = label("", &["subagent-count"]);
        inner.append(&count);
        root.set_child(Some(&inner));
        root.set_visible(false);

        let list = gtk4::ListBox::new();
        list.add_css_class("subagent-list");
        list.set_selection_mode(gtk4::SelectionMode::None);
        let scroller = gtk4::ScrolledWindow::new();
        scroller.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        scroller.set_propagate_natural_height(true);
        scroller.set_max_content_height(420);
        scroller.set_min_content_width(340);
        scroller.set_child(Some(&list));
        let popover = gtk4::Popover::new();
        popover.add_css_class("subagent-popover");
        popover.set_child(Some(&scroller));
        root.set_popover(Some(&popover));

        let this = Rc::new(Self {
            root,
            count,
            list,
            popover,
            rows: RefCell::new(Vec::new()),
        });
        let weak = Rc::downgrade(&this);
        this.list.connect_row_activated(move |_, row| {
            let Some(this) = weak.upgrade() else { return };
            let id = usize::try_from(row.index())
                .ok()
                .and_then(|i| this.rows.borrow().get(i).map(|(id, _)| id.clone()));
            if let Some(id) = id {
                this.popover.popdown();
                open(&id);
            }
        });
        this
    }

    pub fn widget(&self) -> &gtk4::MenuButton {
        &self.root
    }

    /// Shows the sub-agents it is given (the view passes the running ones); hidden while there
    /// are none. Rows are rebuilt only when the set of sub-agents changes; otherwise their labels
    /// are set in place.
    pub fn set(&self, agents: &[SubagentSummary]) {
        self.root.set_visible(!agents.is_empty());
        set_if_changed(
            &self.count,
            &match agents.len() {
                1 => "1 sub-agent".to_owned(),
                n => format!("{n} sub-agents"),
            },
        );
        let same = {
            let rows = self.rows.borrow();
            rows.len() == agents.len() && rows.iter().zip(agents).all(|((id, _), a)| *id == a.id)
        };
        if !same {
            while let Some(child) = self.list.first_child() {
                self.list.remove(&child);
            }
            let mut rows = Vec::with_capacity(agents.len());
            for agent in agents {
                let (row, labels) = ListRow::new();
                self.list.append(&row);
                rows.push((agent.id.clone(), labels));
            }
            *self.rows.borrow_mut() = rows;
        }
        for ((_, labels), agent) in self.rows.borrow().iter().zip(agents) {
            labels.set(agent);
        }
    }

    /// Whether it shows, its count text and the listed ids (tests).
    #[cfg(test)]
    pub fn state(&self) -> (bool, String, Vec<String>) {
        (
            self.root.is_visible(),
            self.count.text().to_string(),
            self.rows
                .borrow()
                .iter()
                .map(|(id, _)| id.clone())
                .collect(),
        )
    }
}

/// One sub-agent on its own, in a dialog that follows it until closed.
pub struct SubagentPanel {
    dialog: adw::Dialog,
    id: String,
    sink: RowSink,
    title: gtk4::Label,
    status: gtk4::Label,
    asked: gtk4::Box,
    asked_text: gtk4::Label,
    /// Where the sub-agent's own steps go, in order.
    steps: gtk4::Box,
    empty: gtk4::Label,
    reported: gtk4::Box,
    reported_text: gtk4::Label,
    error: gtk4::Box,
    error_text: gtk4::Label,
    /// One row per step (nested steps included), built once and then updated.
    rows: RefCell<HashMap<String, Row>>,
}

impl SubagentPanel {
    pub fn new(id: &str, sink: RowSink) -> Rc<Self> {
        let dialog = adw::Dialog::new();
        dialog.set_content_width(760);
        dialog.set_content_height(640);
        let body = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
        body.add_css_class("transcript");
        body.set_margin_top(12);
        body.set_margin_bottom(18);
        body.set_margin_start(18);
        body.set_margin_end(18);

        let title = label("", &["subagent-panel-title"]);
        title.set_xalign(0.0);
        let status = label("", &["dim-label"]);
        status.set_xalign(0.0);
        let head = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        head.append(&title);
        head.append(&status);
        body.append(&head);
        let (asked, asked_text) = section("Asked to");
        body.append(&asked);
        let steps = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
        body.append(&steps);
        let empty = label("No steps yet.", &["dim-label"]);
        empty.set_xalign(0.0);
        body.append(&empty);
        let (reported, reported_text) = section("Reported");
        body.append(&reported);
        let (error, error_text) = section("Error");
        body.append(&error);

        let scroller = gtk4::ScrolledWindow::new();
        scroller.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        scroller.set_vexpand(true);
        scroller.set_child(Some(&body));
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&scroller));
        dialog.set_child(Some(&toolbar));
        Rc::new(Self {
            dialog,
            id: id.to_owned(),
            sink,
            title,
            status,
            asked,
            asked_text,
            steps,
            empty,
            reported,
            reported_text,
            error,
            error_text,
            rows: RefCell::new(HashMap::new()),
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn dialog(&self) -> &adw::Dialog {
        &self.dialog
    }

    /// Brings the panel up to date: new steps are added, `changed` ones (ids the view flushed)
    /// are updated, and the header and sections are set in place. Never rebuilds what is there.
    pub fn refresh(
        &self,
        model: &Transcript,
        summary: Option<&SubagentSummary>,
        changed: &[String],
    ) {
        let Some(item) = model.get(&self.id) else {
            set_if_changed(&self.status, "This sub-agent is no longer in the thread.");
            return;
        };
        if let Some(s) = summary {
            if self.dialog.title() != s.name {
                self.dialog.set_title(&s.name);
            }
            set_if_changed(&self.title, &s.name);
            set_if_changed(&self.status, &status_text(s));
        }
        let Body::Tool(tool) = &item.body else {
            return;
        };
        let prompt = tool
            .input
            .as_ref()
            .and_then(|i| i.get("prompt"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim();
        set_if_changed(&self.asked_text, prompt);
        self.asked.set_visible(!prompt.is_empty());

        for child in &item.children {
            self.sync(model, child, &self.steps, changed);
        }
        self.empty.set_visible(item.children.is_empty());

        let report = tool.output.trim();
        set_if_changed(&self.reported_text, report);
        self.reported.set_visible(!report.is_empty());
        let error = tool.error.as_deref().unwrap_or("").trim();
        set_if_changed(&self.error_text, error);
        self.error.set_visible(!error.is_empty());
    }

    /// Builds `id` under `parent` the first time it is seen, else updates it when it changed;
    /// then the same for its own children.
    fn sync(&self, model: &Transcript, id: &str, parent: &gtk4::Box, changed: &[String]) {
        let Some(item) = model.get(id) else { return };
        let built = self.rows.borrow().contains_key(id);
        if !built {
            let row = Row::build(item, &self.sink);
            parent.append(&row.widget());
            self.rows.borrow_mut().insert(id.to_owned(), row);
        } else if changed.iter().any(|c| c == id) {
            if let Some(row) = self.rows.borrow().get(id) {
                row.update(item);
            }
        }
        let children_box = self
            .rows
            .borrow()
            .get(id)
            .and_then(|r| r.children_box().cloned());
        if let Some(children) = children_box {
            for child in &item.children {
                self.sync(model, child, &children, changed);
            }
        }
    }

    /// Hands a computed diff to the panel's copy of a file-change card, when it shows one.
    pub fn show_diff(&self, id: &str, reply: &crate::chat::DiffReply) {
        if let Some(Row::Tool(card)) = self.rows.borrow().get(id) {
            card.show_diff(reply);
        }
    }

    /// How many step rows the panel holds, nested ones included (tests).
    #[cfg(test)]
    pub fn steps_shown(&self) -> usize {
        self.rows.borrow().len()
    }

    /// The same widget is kept for a step across refreshes (tests).
    #[cfg(test)]
    pub fn row_widget(&self, id: &str) -> Option<gtk4::Widget> {
        self.rows.borrow().get(id).map(Row::widget)
    }
}

fn section(title: &str) -> (gtk4::Box, gtk4::Label) {
    let b = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    b.add_css_class("card-section");
    b.append(&label(title, &["section-title"]));
    let body = label("", &["subagent-text"]);
    wrapping(&body);
    body.set_selectable(true);
    body.set_xalign(0.0);
    b.append(&body);
    b.set_visible(false);
    (b, body)
}
