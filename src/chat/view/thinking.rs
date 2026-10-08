//! The strip just above the composer that shows the agent is at work: the app icon's three
//! cursor dots bouncing in turn (the startup splash's wave, see `window/loading.css`) beside a
//! short status. It slides in when a turn starts and out when the thread settles, so the
//! composer moves smoothly rather than jumping.

use gtk4::prelude::*;

use super::model::Activity;

/// The agent accents the dots are drawn in, as in the app icon.
const DOTS: [&str; 3] = ["claude", "agy", "codex"];

pub struct ThinkingStrip {
    pub revealer: gtk4::Revealer,
    label: gtk4::Label,
}

impl ThinkingStrip {
    pub fn new() -> Self {
        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
        row.add_css_class("thinking-strip");
        let dots = gtk4::Box::new(gtk4::Orientation::Horizontal, 5);
        dots.set_valign(gtk4::Align::Center);
        dots.add_css_class("thinking-dots");
        for agent in DOTS {
            let dot = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            dot.add_css_class("thinking-dot");
            dot.add_css_class(&format!("dot-{agent}"));
            dot.set_valign(gtk4::Align::Center);
            dots.append(&dot);
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
        Self { revealer, label }
    }

    /// Follows the thread: shown while its turn runs or background work it waits on does,
    /// hidden once it settles. `agent`: the name of the agent at work.
    pub fn set(&self, activity: &Activity, agent: &str) {
        match status_text(activity, agent) {
            Some(text) => {
                self.label.set_text(&text);
                self.revealer.set_reveal_child(true);
            }
            None => self.revealer.set_reveal_child(false),
        }
    }

    /// Whether the strip is shown, and what it says (tests).
    #[cfg(test)]
    pub fn shown(&self) -> Option<String> {
        self.revealer
            .reveals_child()
            .then(|| self.label.text().to_string())
    }
}

/// What the strip says for `activity`; `None` hides it.
fn status_text(activity: &Activity, agent: &str) -> Option<String> {
    match activity {
        Activity::Working { background: 0 } => Some(format!("{agent} is working…")),
        Activity::Working { background } => {
            Some(format!("{agent} is working · {background} tasks active"))
        }
        Activity::Thinking => Some(format!("{agent} is thinking…")),
        Activity::Tool { .. }
        | Activity::WaitingForWorkers { .. }
        | Activity::NeedsApproval
        | Activity::NeedsAnswer => activity.text(),
        Activity::Waiting { tasks } => Some(match tasks.len() {
            1 => "Waiting on 1 background task…".to_owned(),
            n => format!("Waiting on {n} background tasks…"),
        }),
        Activity::Idle | Activity::Finished | Activity::Failed | Activity::Stopped => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_who_is_working_and_hides_when_settled() {
        assert_eq!(
            status_text(&Activity::Working { background: 2 }, "Claude").as_deref(),
            Some("Claude is working · 2 tasks active")
        );
        assert_eq!(
            status_text(
                &Activity::Waiting {
                    tasks: vec!["Explore".into()]
                },
                "Claude"
            )
            .as_deref(),
            Some("Waiting on 1 background task…")
        );
        assert_eq!(
            status_text(
                &Activity::Waiting {
                    tasks: vec!["a".into(), "b".into()]
                },
                "agy"
            )
            .as_deref(),
            Some("Waiting on 2 background tasks…")
        );
        assert_eq!(status_text(&Activity::Finished, "Claude"), None);
        assert_eq!(status_text(&Activity::Idle, "Claude"), None);
    }
}
