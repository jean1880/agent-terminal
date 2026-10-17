//! The part of an approval card that says what a `git` or `gh` command does: the operation in
//! words, where its effects land, the details worth reading, and a warning for one that can lose
//! work or rewrite history.
//!
//! The reading is [`agent_core::git_command`]'s. It is a reading aid: the card still shows the
//! command line as sent beneath it, and a reading the parser is unsure of says so.

use adw::prelude::*;
use agent_core::git_command::{self, GitAction, Kind, Reach, Tool};
use serde_json::Value;

use super::cards::{content_label, label, wrapping};

/// Characters of a detail shown before it is cut (the command beneath has all of it).
const DETAIL_CHARS: usize = 600;

/// The reading of an approval for `tool` with `input`, when it is a shell command that runs git
/// or gh.
pub(super) fn reading(tool: &str, input: &Value) -> Option<GitAction> {
    match crate::always_allow::Action::of(tool, input)? {
        crate::always_allow::Action::Shell { command } => git_command::describe_command(&command),
        _ => None,
    }
}

fn icon(action: &GitAction) -> &'static str {
    if action.destructive() {
        return "at-dialog-warning-symbolic";
    }
    match (action.tool, action.kind) {
        (_, Kind::PullRequest) => crate::icons::PULL_REQUEST_ICON,
        (Tool::Gh, _) => crate::icons::EXTERNAL_ICON,
        (_, Kind::Branch | Kind::Switch) => crate::icons::WORKTREE_ICON,
        (_, Kind::Push | Kind::Pull | Kind::Fetch | Kind::Clone | Kind::Remote) => {
            "at-network-server-symbolic"
        }
        (_, Kind::Inspect) => "at-system-search-symbolic",
        _ => crate::icons::GIT_ICON,
    }
}

fn reach_class(reach: Reach) -> &'static str {
    match reach {
        Reach::ReadOnly => "git-reach-read",
        Reach::Local => "git-reach-local",
        Reach::Remote => "git-reach-remote",
    }
}

fn cut(value: &str) -> String {
    match value.char_indices().nth(DETAIL_CHARS) {
        Some((at, _)) => format!("{}… (the rest is in the command below)", &value[..at]),
        None => value.to_owned(),
    }
}

/// What [`GitSummary::shown`] reads back: title, badges, `(label, value)` details, warning.
#[cfg(test)]
pub(super) type Shown = (String, String, Vec<(String, String)>, String);

/// The git section of an approval card.
pub(super) struct GitSummary {
    pub root: gtk4::Box,
    icon: gtk4::Image,
    title: gtk4::Label,
    tool: gtk4::Label,
    reach: gtk4::Label,
    details: gtk4::Grid,
    warning: gtk4::Label,
    uncertain: gtk4::Label,
}

impl GitSummary {
    pub fn new() -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        root.add_css_class("git-summary");
        root.set_visible(false);

        let head = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let icon = gtk4::Image::from_icon_name(crate::icons::GIT_ICON);
        icon.add_css_class("git-op-icon");
        icon.set_valign(gtk4::Align::Start);
        head.append(&icon);
        let title = content_label("", &["git-op-title"]);
        wrapping(&title);
        head.append(&title);
        root.append(&head);

        let badges = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let tool = label("", &["git-badge", "git-tool"]);
        let reach = label("", &["git-badge"]);
        badges.append(&tool);
        badges.append(&reach);
        root.append(&badges);

        let details = gtk4::Grid::builder()
            .column_spacing(12)
            .row_spacing(4)
            .build();
        details.add_css_class("git-details");
        root.append(&details);

        // A warning in words and a tinted box, never by colour alone.
        let warning = content_label("", &["approval-warning"]);
        wrapping(&warning);
        warning.set_visible(false);
        root.append(&warning);

        let uncertain = label(
            "Some options here were not recognised, so this summary may be incomplete. The \
             command below is what runs.",
            &["dim-label", "git-uncertain"],
        );
        wrapping(&uncertain);
        uncertain.set_visible(false);
        root.append(&uncertain);

        Self {
            root,
            icon,
            title,
            tool,
            reach,
            details,
            warning,
            uncertain,
        }
    }

    /// Shows `action`, or hides the section for a request that is not a git or gh command.
    pub fn set(&self, action: Option<&GitAction>) {
        let Some(action) = action else {
            self.root.set_visible(false);
            return;
        };
        self.root.set_visible(true);
        self.icon.set_icon_name(Some(icon(action)));
        self.title.set_text(&action.title);
        self.tool.set_text(match action.tool {
            Tool::Git => "git",
            Tool::Gh => "GitHub CLI",
        });
        for class in ["git-reach-read", "git-reach-local", "git-reach-remote"] {
            self.reach.remove_css_class(class);
        }
        self.reach.add_css_class(reach_class(action.reach));
        self.reach.set_text(action.reach.label());
        self.root
            .update_property(&[gtk4::accessible::Property::Label(&format!(
                "{}: {}",
                action.tool.name(),
                action.title
            ))]);

        while let Some(child) = self.details.first_child() {
            self.details.remove(&child);
        }
        for (row, detail) in action.details.iter().enumerate() {
            let Ok(row) = i32::try_from(row) else { break };
            let name = label(detail.label, &["dim-label", "git-detail-label"]);
            name.set_valign(gtk4::Align::Start);
            let value = content_label(&cut(&detail.value), &["git-detail-value"]);
            wrapping(&value);
            if detail.label == "Message" || detail.label == "Body" {
                value.add_css_class("mono-text");
            }
            self.details.attach(&name, 0, row, 1, 1);
            self.details.attach(&value, 1, row, 1, 1);
        }
        self.details.set_visible(!action.details.is_empty());

        let warning = action.warnings.join("\n");
        self.warning.set_text(&warning);
        self.warning.set_visible(!warning.is_empty());
        self.uncertain.set_visible(action.uncertain);
    }

    /// What the section shows (tests): the title, the badges, each detail and the warning.
    #[cfg(test)]
    pub fn shown(&self) -> Option<Shown> {
        if !self.root.is_visible() {
            return None;
        }
        let mut details = Vec::new();
        let mut row = 0;
        while let (Some(name), Some(value)) = (
            self.details.child_at(0, row).and_downcast::<gtk4::Label>(),
            self.details.child_at(1, row).and_downcast::<gtk4::Label>(),
        ) {
            details.push((name.text().to_string(), value.text().to_string()));
            row += 1;
        }
        Some((
            self.title.text().to_string(),
            format!("{} · {}", self.tool.text(), self.reach.text()),
            details,
            if self.warning.is_visible() {
                self.warning.text().to_string()
            } else {
                String::new()
            },
        ))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_shell_commands_running_git_or_gh_get_a_reading() {
        assert!(reading("Bash", &json!({"command": "git push origin main"})).is_some());
        assert!(reading(
            "run_command",
            &json!({"CommandLine": "gh pr create --fill"})
        )
        .is_some());
        assert!(reading("Bash", &json!({"command": "ls -la"})).is_none());
        assert!(reading("Edit", &json!({"file_path": "/a", "old_string": "git"})).is_none());
        assert!(reading("Bash", &json!({"command": "git status && rm -rf /"})).is_none());
    }

    #[test]
    fn a_long_detail_is_cut_with_a_pointer_to_the_command() {
        let long = "x".repeat(DETAIL_CHARS + 10);
        assert!(cut(&long).ends_with("(the rest is in the command below)"));
        assert_eq!(cut("short"), "short");
    }

    /// GTK: run from `view::tests::ui_checks`.
    pub(crate) fn the_section_shows_the_reading_and_hides_for_anything_else() {
        let summary = GitSummary::new();
        let action = reading(
            "Bash",
            &json!({"command": "git push --force origin feat/x"}),
        );
        summary.set(action.as_ref());
        let (title, badges, details, warning) = summary.shown().expect("shown");
        assert_eq!(title, "Push feat/x to origin");
        assert_eq!(badges, "git · Reaches the remote");
        assert!(details.contains(&("Remote".to_owned(), "origin".to_owned())));
        assert!(warning.starts_with("Force push"));
        summary.set(None);
        assert!(summary.shown().is_none());
    }
}
