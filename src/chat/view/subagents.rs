//! The sub-agent explorer: a header button listing every sub-agent the thread started (nested ones
//! included; running ones first) with its live status, and a dialog that shows one of them on its own: what it was
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
#[cfg(test)]
use super::model::ToolStatus;
use super::model::{Body, SubagentSummary, Transcript};

/// `running`, `done · 12 steps`.
pub fn status_text(s: &SubagentSummary) -> String {
    let state = s.state_label();
    match s.steps {
        0 => state.to_owned(),
        1 => format!("{state} · 1 step"),
        n => format!("{state} · {n} steps"),
    }
}

/// The explorer's order: running sub-agents first, then the finished ones, each in the order they
/// began.
pub fn running_first(agents: &[SubagentSummary]) -> Vec<SubagentSummary> {
    let mut ordered = agents.to_vec();
    ordered.sort_by_key(|a| {
        if a.active() {
            0
        } else if a.finished() {
            2
        } else {
            1
        }
    });
    ordered
}

/// The header button's text: how many sub-agents, and how many of them still run.
pub fn count_text(agents: &[SubagentSummary]) -> String {
    let active = agents.iter().filter(|a| a.active()).count();
    let finished = agents.iter().filter(|a| a.finished()).count();
    let unknown = agents.len().saturating_sub(active + finished);
    let known = format!("{active} active · {finished} finished");
    if unknown == 0 {
        known
    } else {
        format!("{known} · {unknown} unknown")
    }
}

/// The labels of one list row, set in place.
struct ListRow {
    row: gtk4::ListBoxRow,
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
        name.set_wrap(true);
        name.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
        name.set_max_width_chars(30);
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
        (
            row.clone(),
            Self {
                row,
                name,
                status,
                task,
            },
        )
    }

    fn set(&self, agent: &SubagentSummary) {
        set_if_changed(&self.name, &agent.name);
        set_if_changed(&self.status, &status_text(agent));
        set_if_changed(
            &self.task,
            if agent.task.is_empty() {
                "No task reported"
            } else {
                &agent.task
            },
        );
        let short_id: String = agent
            .id
            .chars()
            .rev()
            .take(8)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        self.row.update_property(&[
            gtk4::accessible::Property::Label(&format!("{}; worker {short_id}", agent.name)),
            gtk4::accessible::Property::Description(&format!(
                "{}; {}",
                status_text(agent),
                if agent.task.is_empty() {
                    "No task reported"
                } else {
                    &agent.task
                }
            )),
        ]);
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
        count.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        count.set_max_width_chars(22);
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

    /// Shows the sub-agents it is given, in that order (the view passes all of them, running
    /// first); hidden while there are none. Rows are rebuilt only when the set or order of
    /// sub-agents changes; otherwise their labels are set in place.
    pub fn set(&self, agents: &[SubagentSummary]) {
        self.root.set_visible(!agents.is_empty());
        let counts = count_text(agents);
        set_if_changed(&self.count, &counts);
        self.root
            .set_tooltip_text(Some(&format!("Workers: {counts}; show all workers")));
        self.root
            .update_property(&[gtk4::accessible::Property::Label(&format!(
                "Workers: {counts}; show all workers"
            ))]);
        let same = {
            let rows = self.rows.borrow();
            rows.len() == agents.len() && rows.iter().zip(agents).all(|((id, _), a)| *id == a.id)
        };
        if !same {
            let focused = self.list.root().and_then(|r| r.focus()).and_then(|focus| {
                self.rows
                    .borrow()
                    .iter()
                    .find(|(_, labels)| {
                        focus.is_ancestor(&labels.row)
                            || focus == labels.row.clone().upcast::<gtk4::Widget>()
                    })
                    .map(|(id, _)| id.clone())
            });
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
            if let Some(id) = focused {
                if let Some((_, row)) = self.rows.borrow().iter().find(|(key, _)| *key == id) {
                    row.row.grab_focus();
                } else {
                    self.root.grab_focus();
                }
            }
        }
        let mut previous_group = None;
        for ((_, labels), agent) in self.rows.borrow().iter().zip(agents) {
            labels.set(agent);
            let group = if agent.active() {
                "Active"
            } else if agent.finished() {
                "History"
            } else {
                "Status unknown"
            };
            if previous_group != Some(group) {
                if let Some(heading) = labels
                    .row
                    .header()
                    .and_then(|w| w.downcast::<gtk4::Label>().ok())
                {
                    set_if_changed(&heading, group);
                } else {
                    let heading = gtk4::Label::builder()
                        .label(group)
                        .accessible_role(gtk4::AccessibleRole::Heading)
                        .xalign(0.0)
                        .css_classes(["heading"])
                        .margin_top(10)
                        .margin_bottom(6)
                        .margin_start(10)
                        .build();
                    labels.row.set_header(Some(&heading));
                }
            } else {
                labels.row.set_header(None::<&gtk4::Widget>);
            }
            previous_group = Some(group);
        }
    }

    /// Each listed row's status text, in order (tests).
    #[cfg(test)]
    pub fn statuses(&self) -> Vec<String> {
        self.rows
            .borrow()
            .iter()
            .map(|(_, r)| r.status.text().to_string())
            .collect()
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
        self.empty
            .set_text(if summary.is_some_and(|s| s.lifecycle.is_some()) {
                "Step telemetry unavailable."
            } else {
                "No steps yet."
            });
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn worker_group_updates_without_rebuilding_its_row() {
        let button = SubagentButton::new(|_| {});
        let mut worker = agent("same-worker", ToolStatus::Running);
        button.set(&[worker.clone()]);
        let first = button.list.row_at_index(0).expect("worker row");
        let header = || {
            first
                .header()
                .expect("group heading")
                .downcast::<gtk4::Label>()
                .expect("heading")
                .text()
                .to_string()
        };
        assert_eq!(header(), "Active");
        worker.lifecycle = Some(agent_core::event::WorkerState::Completed);
        button.set(&[worker]);
        assert_eq!(
            button.list.row_at_index(0).as_ref(),
            Some(&first),
            "the same worker retains its widget"
        );
        assert_eq!(
            header(),
            "History",
            "completion updates grouping even when order is unchanged"
        );
    }

    fn agent(id: &str, status: ToolStatus) -> SubagentSummary {
        SubagentSummary {
            id: id.to_owned(),
            name: "Explore".to_owned(),
            task: String::new(),
            status,
            steps: 0,
            lifecycle: None,
            activity: None,
        }
    }

    #[test]
    fn running_sub_agents_come_first_and_finished_ones_stay_listed() {
        let all = [
            agent("a", ToolStatus::Completed),
            agent("b", ToolStatus::Running),
            agent("c", ToolStatus::Failed),
            agent("d", ToolStatus::Running),
        ];
        let ids: Vec<_> = running_first(&all).into_iter().map(|a| a.id).collect();
        assert_eq!(ids, ["b", "d", "a", "c"]);
        assert_eq!(count_text(&all), "2 active · 2 finished");
        assert_eq!(count_text(&all[..1]), "0 active · 1 finished");
        assert_eq!(count_text(&all[2..3]), "0 active · 1 finished");
        assert_eq!(count_text(&all[1..2]), "1 active · 0 finished");
        assert_eq!(status_text(&all[0]), "completed");
        assert_eq!(status_text(&all[1]), "running");
    }

    #[test]
    fn counts_follow_worker_lifecycle_instead_of_a_completed_collaboration_call() {
        use agent_core::event::WorkerState;
        let mut worker = agent("worker", ToolStatus::Completed);
        worker.lifecycle = Some(WorkerState::Waiting);
        assert_eq!(count_text(&[worker.clone()]), "1 active · 0 finished");
        worker.lifecycle = Some(WorkerState::Unknown);
        assert_eq!(
            count_text(&[worker.clone()]),
            "0 active · 0 finished · 1 unknown"
        );
        worker.lifecycle = Some(WorkerState::Completed);
        assert_eq!(status_text(&worker), "completed");
        assert_eq!(count_text(&[worker.clone()]), "0 active · 1 finished");
        worker.lifecycle = Some(WorkerState::Closed);
        assert_eq!(
            status_text(&worker),
            "closed",
            "completion and closure stay distinct"
        );
    }
}
