//! The sticky "sub-agents at work" card above the composer, so what the workers do never gets lost
//! in the history. It shows a one-line summary ("2 workers active · 1 finished"), collapsed by
//! default; opened, one row per docked sub-agent with what it is running now (`Run cargo test`,
//! `Read src/config.rs`) or how it ended. A row opens that sub-agent on its own (the explorer's
//! dialog).
//!
//! A sub-agent docks when it is seen running and stays docked after it finishes, until the next
//! prompt is sent or "Clear finished" is pressed; then the card slides away once none runs.
//! Sub-agents that finished before the thread was opened (replayed history) never dock: the
//! header's sub-agent button lists those.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use gtk4::prelude::*;

use super::cards::label;
use super::model::{Body, SubagentSummary, ToolStatus, Transcript};
use super::payload;

/// The card's one-line summary: the running ones, then the finished ones still docked.
pub fn title_text(running: usize, finished: usize, unknown: usize) -> String {
    let active = match running {
        1 => "1 worker active".to_owned(),
        n => format!("{n} workers active"),
    };
    let done = match finished {
        1 => "1 worker finished".to_owned(),
        n => format!("{n} workers finished"),
    };
    let known = match (running, finished) {
        (_, 0) => active,
        (0, _) => done,
        (_, n) => format!("{active} · {n} finished"),
    };
    match (running + finished, unknown) {
        (_, 0) => known,
        (0, 1) => "1 worker unknown".to_owned(),
        (0, n) => format!("{n} workers unknown"),
        (_, n) => format!("{known} · {n} unknown"),
    }
}

fn counts(shown: &[&SubagentSummary]) -> (usize, usize, usize) {
    let running = shown.iter().filter(|a| a.active()).count();
    let finished = shown.iter().filter(|a| a.finished()).count();
    (
        running,
        finished,
        shown.len().saturating_sub(running + finished),
    )
}

/// What sub-agent `id` is doing now: its running step, else its latest one (`None`: no step
/// yet), titled as its card is (`Run <command>`, `Read <path>`, …).
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
    // The step card's own title, so the shelf and the card say the same.
    Some(
        payload::tool_title(
            step.kind,
            &step.title,
            step.input.as_ref(),
            &step.input_text,
        )
        .text(),
    )
}

/// One docked sub-agent's labels, set in place.
struct Row {
    row: gtk4::ListBoxRow,
    /// Spins while it runs; hidden once it has finished.
    spinner: gtk4::Spinner,
    /// How it ended (`completed`, `failed`…); empty while it runs.
    state: gtk4::Label,
    name: gtk4::Label,
    task: gtk4::Label,
    now: gtk4::Label,
    brand: gtk4::Image,
}

impl Row {
    fn new() -> (gtk4::ListBoxRow, Self) {
        let b = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        b.add_css_class("subagent-shelf-row");
        let top = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let brand = gtk4::Image::new();
        brand.set_pixel_size(14);
        top.append(&brand);
        let spinner = gtk4::Spinner::builder().spinning(true).build();
        spinner.add_css_class("thread-background");
        top.append(&spinner);
        let state = label("", &["subagent-state", "dim-label"]);
        state.set_visible(false);
        top.append(&state);
        let name = label("", &["subagent-name"]);
        name.set_xalign(0.0);
        name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        name.set_max_width_chars(20);
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
                spinner,
                state,
                name,
                task,
                now,
                brand,
            },
        )
    }

    fn set(&self, agent: &SubagentSummary, now: Option<&str>) {
        super::subagents::set_brand(&self.brand, agent.driver);
        let running = agent.active();
        // The widget's own `visible`, not `is_visible()`: that is false for every row while the
        // card is collapsed, and a finished row would keep spinning once opened.
        if self.spinner.property::<bool>("visible") != running {
            self.spinner.set_visible(running);
            self.spinner.set_spinning(running);
        }
        self.state.set_visible(!running);
        set_if_changed(&self.state, if running { "" } else { agent.state_label() });
        let activity = agent.activity.as_deref().or(now);
        let now = activity
            .map(|s| format!("Last reported activity: {s}"))
            .unwrap_or_else(|| "Activity unavailable".to_owned());
        self.row.set_tooltip_text(Some(&format!(
            "{}\n{}\n{}\n{now}\nOpen this worker",
            agent.name,
            super::subagents::identity_text(agent),
            if agent.task.is_empty() {
                "No task reported"
            } else {
                &agent.task
            }
        )));
        self.row.update_property(&[
            gtk4::accessible::Property::Label(&agent.name),
            gtk4::accessible::Property::Description(&format!(
                "{}; {}; {now}",
                agent.state_label(),
                if agent.task.is_empty() {
                    "No task reported"
                } else {
                    &agent.task
                }
            )),
        ]);
        set_if_changed(&self.name, &agent.name);
        set_if_changed(
            &self.task,
            if agent.task.is_empty() {
                "No task reported"
            } else {
                &agent.task
            },
        );
        set_if_changed(&self.now, &now);
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
    /// The docked sub-agents behind the rows, in order, and their labels.
    rows: RefCell<Vec<(String, Row)>>,
    /// Every sub-agent seen running since the last prompt or "Clear finished": what stays docked
    /// after it finishes.
    docked: RefCell<HashSet<String>>,
    /// "Clear finished": shown while a finished sub-agent is docked.
    clear: gtk4::Button,
}

impl SubagentShelf {
    /// `open` is called with a sub-agent's item id when its row is chosen; `cleared` after
    /// "Clear finished" emptied the dock, to redraw it.
    pub fn new(open: impl Fn(&str) + 'static, cleared: impl Fn() + 'static) -> Rc<Self> {
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
        let clear = gtk4::Button::builder()
            .label("Clear finished")
            .halign(gtk4::Align::End)
            .css_classes(["flat"])
            .tooltip_text("Take the finished workers off the dock")
            .visible(false)
            .build();
        let body = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        body.append(&scroller);
        body.append(&clear);
        // Collapsed by default: one line until it is asked for.
        let fold = super::cards::collapsible(
            crate::icons::SUBAGENT_ICON,
            &title,
            &body,
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
            docked: RefCell::new(HashSet::new()),
            clear,
        });
        let weak = Rc::downgrade(&this);
        this.clear.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.clear_finished();
                cleared();
            }
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

    /// Follows the thread's sub-agents: a row per docked one (every one seen running since the
    /// last prompt or "Clear finished"), with its current step or how it ended; shown while any
    /// is docked. Rows are rebuilt only when the set of docked sub-agents changes.
    pub fn set(&self, model: &Transcript, agents: &[SubagentSummary]) {
        let shown: Vec<&SubagentSummary> = {
            let mut docked = self.docked.borrow_mut();
            docked.extend(agents.iter().filter(|a| a.active()).map(|a| a.id.clone()));
            agents.iter().filter(|a| docked.contains(&a.id)).collect()
        };
        self.revealer.set_reveal_child(!shown.is_empty());
        if shown.is_empty() {
            return;
        }
        let (running, finished, unknown) = counts(&shown);
        set_if_changed(&self.title, &title_text(running, finished, unknown));
        self.clear.set_visible(finished > 0);
        let same = {
            let rows = self.rows.borrow();
            rows.len() == shown.len() && rows.iter().zip(&shown).all(|((id, _), a)| *id == a.id)
        };
        if !same {
            while let Some(child) = self.list.first_child() {
                self.list.remove(&child);
            }
            let mut rows = Vec::with_capacity(shown.len());
            for agent in &shown {
                let (row, labels) = Row::new();
                self.list.append(&row);
                rows.push((agent.id.clone(), labels));
            }
            *self.rows.borrow_mut() = rows;
        }
        for ((id, labels), agent) in self.rows.borrow().iter().zip(&shown) {
            labels.set(agent, current_step(model, id).as_deref());
        }
    }

    /// Takes the finished sub-agents off the dock (a new prompt, or "Clear finished"). Running
    /// ones dock again at the next [`Self::set`]; the caller redraws.
    pub fn clear_finished(&self) {
        self.docked.borrow_mut().clear();
    }

    /// Opens the worker activity disclosure, including deterministic demo scenes.
    pub(crate) fn set_expanded(&self, expanded: bool) {
        if self.fold.body.reveals_child() != expanded {
            self.fold.toggle.emit_clicked();
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
    use agent_core::event::{Envelope, Event, ItemKind};

    #[test]
    #[ignore = "native GTK; run alone in an isolated display"]
    fn native_unknown_worker_has_no_clear_finished_button() {
        gtk4::init().expect("GTK init");
        let t = transcript(&[subagent("s1", "Explore")]);
        let shelf = SubagentShelf::new(|_| {}, || {});
        let mut workers = t.subagents();
        shelf.set(&t, &workers);
        workers[0].lifecycle = Some(agent_core::event::WorkerState::Unknown);
        shelf.set(&t, &workers);
        assert_eq!(shelf.title.text(), "1 worker unknown");
        assert!(!shelf.clear.property::<bool>("visible"));
        assert!(shelf.state().0);
    }

    #[test]
    fn titles_count_the_running_ones() {
        assert_eq!(title_text(1, 0, 0), "1 worker active");
        assert_eq!(title_text(3, 0, 0), "3 workers active");
        assert_eq!(title_text(0, 1, 0), "1 worker finished");
        assert_eq!(title_text(0, 2, 0), "2 workers finished");
        assert_eq!(title_text(1, 2, 0), "1 worker active · 2 finished");
    }

    #[test]
    fn an_unknown_worker_is_not_called_finished() {
        let t = transcript(&[subagent("s1", "Explore")]);
        let mut worker = t.subagents().remove(0);
        worker.lifecycle = Some(agent_core::event::WorkerState::Unknown);
        let (running, finished, unknown) = counts(&[&worker]);
        assert_eq!((running, finished, unknown), (0, 0, 1));
        assert_eq!(title_text(running, finished, unknown), "1 worker unknown");
        assert_eq!(
            title_text(2, 1, 3),
            "2 workers active · 1 finished · 3 unknown"
        );
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
        assert_eq!(current_step(&t, "s1").as_deref(), Some("Run cargo test"));

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

        // The shelf line is exactly the step card's title (one grammar).
        let step = match &t.get("r1").expect("step").body {
            Body::Tool(tool) => tool.clone(),
            other => panic!("{other:?}"),
        };
        let (action, target) = payload::card_title_and_summary(
            step.kind,
            &step.title,
            step.input.as_ref(),
            &step.input_text,
        );
        assert_eq!(current_step(&t, "s1"), Some(format!("{action} {target}")));

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
        let cleared = Rc::new(std::cell::Cell::new(0));
        let shelf = SubagentShelf::new(
            {
                let (opened, shelf_slot) = (opened.clone(), shelf_slot.clone());
                let t = transcript(&[subagent("s1", "Explore")]);
                move |id| {
                    opened.borrow_mut().push(id.to_owned());
                    // Opening runs the view's refresh, which updates the shelf from in here.
                    if let Some(shelf) = shelf_slot.borrow().as_ref() {
                        shelf.set(&t, &t.subagents());
                    }
                }
            },
            {
                let cleared = cleared.clone();
                move || cleared.set(cleared.get() + 1)
            },
        );
        *shelf_slot.borrow_mut() = Some(shelf.clone());
        // In a window, as in the app: activating a row moves focus, which needs a root.
        let window = gtk4::Window::new();
        window.set_child(Some(&shelf.revealer));

        shelf.set(&t, &t.subagents());
        assert_eq!(
            shelf.state(),
            (
                true,
                false,
                vec!["Last reported activity: Run rg write_atomic".to_owned()]
            ),
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
        assert_eq!(shelf.title.text(), "2 workers active");
        assert_eq!(
            shelf.state().2,
            [
                "Last reported activity: Run rg write_atomic",
                "Activity unavailable"
            ],
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
        // Finished, they stay docked (not lost in the history), with how they ended, and the
        // card offers to clear them.
        shelf.set(&two, &finished);
        assert!(shelf.state().0, "still docked once they finish");
        assert_eq!(shelf.title.text(), "2 workers finished");
        // The `visible` property itself: `is_visible()` also needs every parent shown, and the
        // card is collapsed here.
        assert!(shelf.clear.property::<bool>("visible"));
        let ended: Vec<String> = shelf
            .rows
            .borrow()
            .iter()
            .map(|(_, r)| r.state.text().to_string())
            .collect();
        assert_eq!(ended, ["completed", "completed"]);
        assert!(shelf
            .rows
            .borrow()
            .iter()
            .all(|(_, r)| !r.spinner.property::<bool>("visible")
                && r.state.property::<bool>("visible")));

        // "Clear finished" takes them off and asks the view to redraw; then the card goes.
        shelf.clear.emit_clicked();
        assert_eq!(cleared.get(), 1);
        shelf.set(&two, &finished);
        assert!(!shelf.state().0, "gone once cleared and none runs");

        // A sub-agent that finished before the thread was shown (replayed history) never docks.
        let fresh = SubagentShelf::new(|_| {}, || {});
        fresh.set(&two, &finished);
        assert!(!fresh.state().0, "replayed finished workers are not docked");

        // Clearing keeps a running one: it docks again at the next update.
        shelf.set(&two, &two.subagents());
        shelf.clear_finished();
        shelf.set(&two, &two.subagents());
        assert_eq!(shelf.title.text(), "2 workers active");
        shelf_slot.borrow_mut().take();
        window.destroy();
    }
}
