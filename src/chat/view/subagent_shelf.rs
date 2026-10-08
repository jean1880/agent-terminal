//! The sticky "sub-agents at work" card above the composer. While any sub-agent runs it shows a
//! one-line summary ("2 sub-agents running"), collapsed by default; opened, one row per running
//! sub-agent with what it is running right now (`$ cargo test`, `Read src/config.rs`). A row
//! opens that sub-agent on its own (the explorer's dialog). It slides away once none runs; the
//! header's sub-agent button still lists the finished ones.

use std::cell::RefCell;
use std::rc::Rc;

use agent_core::event::ItemKind;
use gtk4::prelude::*;

use super::cards::label;
use super::model::{Body, SubagentSummary, ToolStatus, Transcript};
use super::payload;

/// The card's one-line summary.
pub fn title_text(running: usize) -> String {
    match running {
        1 => "1 sub-agent running".to_owned(),
        n => format!("{n} sub-agents running"),
    }
}

/// What sub-agent `id` is doing now: its running step, else its latest one (`None`: no step
/// yet). A command reads `$ <command>`; another tool its title and target.
pub fn current_step(model: &Transcript, id: &str) -> Option<String> {
    let agent = model.get(id)?;
    // Newest first: the first running step wins; failing that, the newest step of all.
    let mut latest = None;
    let mut running = None;
    for step in agent.children.iter().rev().filter_map(|c| model.get(c)) {
        let Body::Tool(tool) = &step.body else {
            continue;
        };
        latest.get_or_insert(tool);
        if tool.status == ToolStatus::Running {
            running = Some(tool);
            break;
        }
    }
    let step = running.or(latest)?;
    let target = payload::tool_summary(step.kind, step.input.as_ref(), &step.input_text);
    Some(match (step.kind, target) {
        (ItemKind::Command, Some(command)) => format!("$ {command}"),
        (_, Some(target)) => format!("{} {target}", step.title),
        (_, None) => step.title.clone(),
    })
}

/// One running sub-agent's labels, set in place.
struct Row {
    row: gtk4::ListBoxRow,
    name: gtk4::Label,
    task: gtk4::Label,
    now: gtk4::Label,
}

impl Row {
    fn new() -> (gtk4::ListBoxRow, Self) {
        let b = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        b.add_css_class("subagent-shelf-row");
        let top = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let spinner = gtk4::Spinner::builder().spinning(true).build();
        spinner.add_css_class("thread-background");
        top.append(&spinner);
        let name = label("", &["subagent-name"]);
        name.set_xalign(0.0);
        top.append(&name);
        let task = label("", &["subagent-task", "dim-label"]);
        task.set_xalign(0.0);
        task.set_hexpand(true);
        task.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        top.append(&task);
        b.append(&top);
        let now = label("", &["subagent-now"]);
        now.set_xalign(0.0);
        now.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        b.append(&now);
        let row = gtk4::ListBoxRow::new();
        row.set_child(Some(&b));
        row.set_tooltip_text(Some("Open this sub-agent"));
        (
            row.clone(),
            Self {
                row,
                name,
                task,
                now,
            },
        )
    }

    fn set(&self, agent: &SubagentSummary, now: Option<&str>) {
        if self.name.text() != agent.name {
            // What a screen reader says for the row (the tooltip is not announced).
            self.row
                .update_property(&[gtk4::accessible::Property::Description(&format!(
                    "Open the {} sub-agent",
                    agent.name
                ))]);
        }
        set_if_changed(&self.name, &agent.name);
        set_if_changed(&self.task, &agent.task);
        self.task.set_visible(!agent.task.is_empty());
        set_if_changed(&self.now, now.unwrap_or("Starting…"));
    }
}

/// Leaves a label alone when its text is already right (no relayout while it streams).
fn set_if_changed(l: &gtk4::Label, text: &str) {
    if l.text() != text {
        l.set_text(text);
    }
}

pub struct SubagentShelf {
    pub revealer: gtk4::Revealer,
    title: gtk4::Label,
    /// The card itself; it opens and closes on its own toggle (read back by tests).
    #[cfg_attr(not(test), allow(dead_code))]
    fold: super::cards::Collapsible,
    list: gtk4::ListBox,
    /// The running sub-agents behind the rows, in order, and their labels.
    rows: RefCell<Vec<(String, Row)>>,
}

impl SubagentShelf {
    /// `open` is called with a sub-agent's item id when its row is chosen.
    pub fn new(open: impl Fn(&str) + 'static) -> Rc<Self> {
        let title = label("", &["plan-title"]);
        let list = gtk4::ListBox::new();
        list.add_css_class("subagent-list");
        list.set_selection_mode(gtk4::SelectionMode::None);
        // A long list scrolls inside the card rather than pushing the composer away.
        let scroller = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .propagate_natural_height(true)
            .max_content_height(220)
            .child(&list)
            .build();
        // Collapsed by default: one line until it is asked for.
        let fold = super::cards::collapsible(
            crate::icons::SUBAGENT_ICON,
            &title,
            &scroller,
            false,
            "what the sub-agents are doing",
        );
        fold.card.add_css_class("subagent-shelf");

        let revealer = gtk4::Revealer::builder()
            .transition_type(gtk4::RevealerTransitionType::SlideUp)
            .reveal_child(false)
            .child(&fold.card)
            .build();
        let this = Rc::new(Self {
            revealer,
            title,
            fold,
            list,
            rows: RefCell::new(Vec::new()),
        });
        let weak = Rc::downgrade(&this);
        this.list.connect_row_activated(move |_, row| {
            let Some(this) = weak.upgrade() else { return };
            let id = usize::try_from(row.index())
                .ok()
                .and_then(|i| this.rows.borrow().get(i).map(|(id, _)| id.clone()));
            if let Some(id) = id {
                open(&id);
            }
        });
        this
    }

    /// Follows the thread's sub-agents: shown while any runs, one row per running one with its
    /// current step. Rows are rebuilt only when the set of running sub-agents changes.
    pub fn set(&self, model: &Transcript, agents: &[SubagentSummary]) {
        let running: Vec<&SubagentSummary> = agents
            .iter()
            .filter(|a| a.status == ToolStatus::Running)
            .collect();
        self.revealer.set_reveal_child(!running.is_empty());
        if running.is_empty() {
            return;
        }
        set_if_changed(&self.title, &title_text(running.len()));
        let same = {
            let rows = self.rows.borrow();
            rows.len() == running.len() && rows.iter().zip(&running).all(|((id, _), a)| *id == a.id)
        };
        if !same {
            while let Some(child) = self.list.first_child() {
                self.list.remove(&child);
            }
            let mut rows = Vec::with_capacity(running.len());
            for agent in &running {
                let (row, labels) = Row::new();
                self.list.append(&row);
                rows.push((agent.id.clone(), labels));
            }
            *self.rows.borrow_mut() = rows;
        }
        for ((id, labels), agent) in self.rows.borrow().iter().zip(&running) {
            labels.set(agent, current_step(model, id).as_deref());
        }
    }

    /// Whether the card shows, whether it is open, and each row's current step (tests).
    #[cfg(test)]
    pub fn state(&self) -> (bool, bool, Vec<String>) {
        (
            self.revealer.reveals_child(),
            self.fold.body.reveals_child(),
            self.rows
                .borrow()
                .iter()
                .map(|(_, r)| r.now.text().to_string())
                .collect(),
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use agent_core::adapter::Driver;
    use agent_core::event::{Envelope, Event};

    #[test]
    fn titles_count_the_running_ones() {
        assert_eq!(title_text(1), "1 sub-agent running");
        assert_eq!(title_text(3), "3 sub-agents running");
    }

    fn start(
        id: &str,
        kind: ItemKind,
        title: &str,
        input: serde_json::Value,
        parent: Option<&str>,
    ) -> Envelope {
        Envelope::new(Event::ItemStarted {
            kind,
            title: title.into(),
            input: Some(input),
            parent: parent.map(str::to_owned),
        })
        .item(id)
    }

    fn done(id: &str) -> Envelope {
        Envelope::new(Event::ItemCompleted {
            status: agent_core::event::ItemStatus::Completed,
            output: None,
            error: None,
        })
        .item(id)
    }

    fn subagent(id: &str, name: &str) -> Envelope {
        start(
            id,
            ItemKind::Subagent,
            "Task",
            serde_json::json!({"subagent_type": name, "description": "Find the writers"}),
            None,
        )
    }

    fn command(id: &str, parent: &str, cmd: &str) -> Envelope {
        start(
            id,
            ItemKind::Command,
            "Bash",
            serde_json::json!({ "command": cmd }),
            Some(parent),
        )
    }

    fn read(id: &str, parent: &str, path: &str) -> Envelope {
        start(
            id,
            ItemKind::FileRead,
            "Read",
            serde_json::json!({ "file_path": path }),
            Some(parent),
        )
    }

    fn transcript(envs: &[Envelope]) -> Transcript {
        let mut t = Transcript::new();
        for env in envs {
            t.apply(env, Driver::Claude);
        }
        t
    }

    #[test]
    fn current_step_prefers_the_running_one_then_the_latest() {
        // A command still running, then a read that already finished: the command is what the
        // sub-agent is doing.
        let t = transcript(&[
            subagent("s1", "Explore"),
            command("c1", "s1", "cargo test"),
            read("r1", "s1", "src/config.rs"),
            done("r1"),
        ]);
        assert_eq!(current_step(&t, "s1").as_deref(), Some("$ cargo test"));

        // Everything finished: the newest step, a tool other than a command by title + target.
        let t = transcript(&[
            subagent("s1", "Explore"),
            command("c1", "s1", "cargo test"),
            done("c1"),
            read("r1", "s1", "src/config.rs"),
            done("r1"),
        ]);
        assert_eq!(
            current_step(&t, "s1").as_deref(),
            Some("Read src/config.rs")
        );

        // No step yet, or no such sub-agent.
        let t = transcript(&[subagent("s1", "Explore")]);
        assert_eq!(current_step(&t, "s1"), None);
        assert_eq!(current_step(&t, "nope"), None);
    }

    /// Shown, collapsed, while a sub-agent runs; a row per running one with its current step,
    /// rebuilt as the set changes; a row opens its sub-agent (and that may update the shelf
    /// re-entrantly); gone once none runs. Needs GTK (run from `chat::view::tests::ui_checks`).
    pub(crate) fn shelf_follows_running_subagents() {
        let opened = Rc::new(RefCell::new(Vec::<String>::new()));
        let shelf_slot: Rc<RefCell<Option<Rc<SubagentShelf>>>> = Rc::default();
        let t = transcript(&[
            subagent("s1", "Explore"),
            command("c1", "s1", "rg write_atomic"),
        ]);
        let shelf = SubagentShelf::new({
            let (opened, shelf_slot) = (opened.clone(), shelf_slot.clone());
            let t = transcript(&[subagent("s1", "Explore")]);
            move |id| {
                opened.borrow_mut().push(id.to_owned());
                // Opening runs the view's refresh, which updates the shelf from in here.
                if let Some(shelf) = shelf_slot.borrow().as_ref() {
                    shelf.set(&t, &t.subagents());
                }
            }
        });
        *shelf_slot.borrow_mut() = Some(shelf.clone());
        // In a window, as in the app: activating a row moves focus, which needs a root.
        let window = gtk4::Window::new();
        window.set_child(Some(&shelf.revealer));

        shelf.set(&t, &t.subagents());
        assert_eq!(
            shelf.state(),
            (true, false, vec!["$ rg write_atomic".to_owned()]),
            "shown, collapsed by default, with the running command"
        );
        shelf.fold.toggle.emit_clicked();
        assert!(shelf.state().1, "one click opens it");
        shelf.fold.toggle.emit_clicked();
        assert!(!shelf.state().1, "and another closes it");

        let row = shelf.list.row_at_index(0).expect("a row");
        row.emit_by_name::<()>("activate", &[]);
        assert_eq!(*opened.borrow(), ["s1"], "the row opened its sub-agent");

        let two = transcript(&[
            subagent("s1", "Explore"),
            command("c1", "s1", "rg write_atomic"),
            subagent("s2", "general-purpose"),
        ]);
        shelf.set(&two, &two.subagents());
        assert_eq!(shelf.title.text(), "2 sub-agents running");
        assert_eq!(
            shelf.state().2,
            ["$ rg write_atomic", "Starting…"],
            "a row each, in order"
        );

        let finished: Vec<SubagentSummary> = two
            .subagents()
            .into_iter()
            .map(|mut a| {
                a.status = ToolStatus::Completed;
                a
            })
            .collect();
        shelf.set(&two, &finished);
        assert!(!shelf.state().0, "hidden once no sub-agent runs");
        shelf_slot.borrow_mut().take();
        window.destroy();
    }
}
