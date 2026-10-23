//! The strip just above the composer that shows the agent is at work: the app icon's three
//! cursor dots bouncing in turn (the startup splash's wave, see `window/loading.css`), every
//! agent at work in the thread (its own and its sub-agents') lit, beside a short status. It slides in when a turn starts and out when the thread settles, so the
//! composer moves smoothly rather than jumping.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

use gtk4::glib;
use gtk4::prelude::*;

use agent_core::adapter::Driver;

use super::model::Activity;

struct StripInner {
    revealer: gtk4::Revealer,
    label: gtk4::Label,
    started_at: Cell<Option<Instant>>,
    timer_source: RefCell<Option<glib::SourceId>>,
    current_activity: RefCell<Activity>,
    current_agent: RefCell<String>,
}

impl StripInner {
    fn stop_timer(&self) {
        if let Some(source) = self.timer_source.take() {
            source.remove();
        }
        self.started_at.set(None);
    }
}

impl Drop for StripInner {
    fn drop(&mut self) {
        if let Some(source) = self.timer_source.take() {
            source.remove();
        }
    }
}

pub struct ThinkingStrip {
    pub revealer: gtk4::Revealer,
    row: gtk4::Box,
    /// The icon's dots, the agent at work lit and leading the wave (`brand::set_lead`).
    dots: gtk4::Box,
    inner: Rc<StripInner>,
}

impl ThinkingStrip {
    pub fn new() -> Self {
        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
        row.add_css_class("thinking-strip");
        let dots = crate::brand::dots();
        dots.add_css_class("thinking-dots");
        let mut dot = dots.first_child();
        while let Some(d) = dot {
            d.add_css_class("thinking-dot");
            dot = d.next_sibling();
        }
        row.append(&dots);
        let label = gtk4::Label::new(None);
        label.add_css_class("thinking-text");
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        row.append(&label);

        let revealer = gtk4::Revealer::builder()
            .transition_type(gtk4::RevealerTransitionType::SlideUp)
            .transition_duration(180)
            .reveal_child(false)
            .child(&row)
            .build();

        let inner = Rc::new(StripInner {
            revealer: revealer.clone(),
            label,
            started_at: Cell::new(None),
            timer_source: RefCell::new(None),
            current_activity: RefCell::new(Activity::Idle),
            current_agent: RefCell::new(String::new()),
        });

        Self {
            revealer,
            row,
            dots,
            inner,
        }
    }

    /// Puts `widget` at the end of the strip, after its status (the background task button).
    pub fn append(&self, widget: &impl IsA<gtk4::Widget>) {
        self.row.append(widget);
    }

    /// Follows the thread: shown while its turn runs or background work it waits on does,
    /// hidden once it settles. `driver`: the thread's agent, whose dot leads; `sub_agents`: the
    /// agents its running sub-agents are on (any route), whose dots are lit beside it.
    pub fn set(&self, activity: &Activity, driver: Driver, sub_agents: &[Driver]) {
        let agent = driver.info().label;
        crate::brand::set_working(&self.dots, driver, sub_agents);
        *self.inner.current_activity.borrow_mut() = activity.clone();
        *self.inner.current_agent.borrow_mut() = agent.to_owned();

        let elapsed = self.inner.started_at.get().map(|t| t.elapsed().as_secs());
        match status_text(activity, agent, elapsed) {
            Some(text) => {
                if self.inner.started_at.get().is_none() {
                    self.inner.started_at.set(Some(Instant::now()));
                }
                self.inner.label.set_text(&text);
                self.revealer.set_reveal_child(true);

                if self.inner.timer_source.borrow().is_none() {
                    let weak = Rc::downgrade(&self.inner);
                    let source =
                        glib::timeout_add_local(std::time::Duration::from_secs(1), move || {
                            if let Some(inner) = weak.upgrade() {
                                let elapsed = inner.started_at.get().map(|t| t.elapsed().as_secs());
                                let act = inner.current_activity.borrow().clone();
                                let ag = inner.current_agent.borrow().clone();
                                match status_text(&act, &ag, elapsed) {
                                    Some(t) => {
                                        inner.label.set_text(&t);
                                        glib::ControlFlow::Continue
                                    }
                                    None => {
                                        inner.revealer.set_reveal_child(false);
                                        inner.timer_source.take();
                                        glib::ControlFlow::Break
                                    }
                                }
                            } else {
                                glib::ControlFlow::Break
                            }
                        });
                    *self.inner.timer_source.borrow_mut() = Some(source);
                }
            }
            None => {
                self.revealer.set_reveal_child(false);
                self.inner.stop_timer();
            }
        }
    }

    /// Whether the strip is shown, and what it says (tests).
    #[cfg(test)]
    pub fn shown(&self) -> Option<String> {
        self.revealer
            .reveals_child()
            .then(|| self.inner.label.text().to_string())
    }

    /// Each dot's classes beyond its colour: `lead`, `working`, `dimmed` or none (tests).
    #[cfg(test)]
    pub fn dot_states(&self) -> Vec<&'static str> {
        let mut states = Vec::new();
        let mut dot = self.dots.first_child();
        while let Some(d) = dot {
            states.push(if d.has_css_class("lead") {
                "lead"
            } else if d.has_css_class("working") {
                "working"
            } else if d.has_css_class("dimmed") {
                "dimmed"
            } else {
                ""
            });
            dot = d.next_sibling();
        }
        states
    }
}

/// What the strip says for `activity`; `None` hides it.
fn status_text(activity: &Activity, agent: &str, elapsed: Option<u64>) -> Option<String> {
    match activity {
        Activity::Working { background: 0 } => match elapsed.filter(|&s| s > 0) {
            Some(secs) => Some(format!("{agent} is working ({secs}s)…")),
            None => Some(format!("{agent} is working…")),
        },
        Activity::Working { background } => match elapsed.filter(|&s| s > 0) {
            Some(secs) => Some(format!(
                "{agent} is working ({secs}s) · {background} tasks active"
            )),
            None => Some(format!("{agent} is working · {background} tasks active")),
        },
        Activity::Thinking => match elapsed.filter(|&s| s > 0) {
            Some(secs) => Some(format!("{agent} is thinking ({secs}s)…")),
            None => Some(format!("{agent} is thinking…")),
        },
        Activity::Tool { .. }
        | Activity::WaitingForWorkers { .. }
        | Activity::NeedsApproval
        | Activity::NeedsAnswer => activity.text(),
        Activity::Waiting { tasks } => Some(match tasks.len() {
            1 => "Waiting on 1 background task…".to_owned(),
            n => format!("Waiting on {n} background tasks…"),
        }),
        // The agent's own start, resume and pause are the header's to say.
        Activity::Idle
        | Activity::Finished
        | Activity::Failed
        | Activity::Stopped
        | Activity::Starting
        | Activity::Resuming
        | Activity::Paused => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_who_is_working_and_hides_when_settled() {
        assert_eq!(
            status_text(&Activity::Working { background: 2 }, "Claude", None).as_deref(),
            Some("Claude is working · 2 tasks active")
        );
        assert_eq!(
            status_text(&Activity::Working { background: 2 }, "Claude", Some(12)).as_deref(),
            Some("Claude is working (12s) · 2 tasks active")
        );
        assert_eq!(
            status_text(&Activity::Working { background: 0 }, "Claude", Some(5)).as_deref(),
            Some("Claude is working (5s)…")
        );
        assert_eq!(
            status_text(&Activity::Thinking, "Claude", Some(3)).as_deref(),
            Some("Claude is thinking (3s)…")
        );
        assert_eq!(
            status_text(
                &Activity::Waiting {
                    tasks: vec!["Explore".into()]
                },
                "Claude",
                None,
            )
            .as_deref(),
            Some("Waiting on 1 background task…")
        );
        assert_eq!(
            status_text(
                &Activity::Waiting {
                    tasks: vec!["a".into(), "b".into()]
                },
                "agy",
                None,
            )
            .as_deref(),
            Some("Waiting on 2 background tasks…")
        );
        assert_eq!(status_text(&Activity::Finished, "Claude", None), None);
        assert_eq!(status_text(&Activity::Idle, "Claude", None), None);
    }
}
