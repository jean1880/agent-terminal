//! A read-only unified diff: a `GtkSourceView` with the `diff` language, monospace, in a frame
//! capped in height, with a line of notes under it (where the diff came from, whether it was cut).
//!
//! Used by the file-change cards ("View diff") and by file-edit approvals, so both show the same
//! thing the same way. Line numbers are deliberately off: a diff's own line numbers are the
//! numbers of the diff text, not of the file, and would mislead.

use gtk4::prelude::*;
use sourceview5::prelude::*;

/// The tallest the diff frame grows before it scrolls.
const MAX_HEIGHT: i32 = 320;

pub struct DiffText {
    root: gtk4::Box,
    buffer: sourceview5::Buffer,
    notes: gtk4::Label,
}

impl DiffText {
    pub fn new() -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        root.add_css_class("diff-text");

        let buffer = sourceview5::Buffer::new(None);
        buffer.set_highlight_matching_brackets(false);
        if let Some(lang) = sourceview5::LanguageManager::default().language("diff") {
            buffer.set_language(Some(&lang));
            buffer.set_highlight_syntax(true);
        }
        super::cards::apply_scheme(&buffer);

        let view = sourceview5::View::with_buffer(&buffer);
        view.set_editable(false);
        view.set_cursor_visible(false);
        view.set_monospace(true);
        view.set_wrap_mode(gtk4::WrapMode::None);
        view.set_show_line_numbers(false);
        view.add_css_class("code-view");
        view.add_css_class("diff-view");
        view.set_top_margin(8);
        view.set_bottom_margin(8);
        view.set_left_margin(10);
        view.set_right_margin(10);

        let scroller = gtk4::ScrolledWindow::new();
        scroller.set_policy(gtk4::PolicyType::Automatic, gtk4::PolicyType::Automatic);
        scroller.set_propagate_natural_height(true);
        scroller.set_max_content_height(MAX_HEIGHT);
        scroller.add_css_class("code-block");
        scroller.set_child(Some(&view));
        root.append(&scroller);

        let notes = super::cards::label("", &["diff-notes", "dim-label"]);
        notes.set_visible(false);
        root.append(&notes);

        Self {
            root,
            buffer,
            notes,
        }
    }

    pub fn widget(&self) -> &gtk4::Box {
        &self.root
    }

    /// Shows `text`, with `notes` under it (empty: no note line).
    pub fn set(&self, text: &str, notes: &[String]) {
        self.buffer.set_text(text);
        let line = notes.join("  ·  ");
        self.notes.set_text(&line);
        self.notes.set_visible(!line.is_empty());
    }

    /// What is shown (tests).
    #[cfg(test)]
    pub fn text(&self) -> String {
        let (start, end) = self.buffer.bounds();
        self.buffer.text(&start, &end, false).to_string()
    }

    /// The notes line (tests).
    #[cfg(test)]
    pub fn notes(&self) -> String {
        self.notes.text().to_string()
    }

    /// Whether the buffer is highlighted as a diff (tests).
    #[cfg(test)]
    pub fn language(&self) -> Option<String> {
        self.buffer.language().map(|l| l.id().to_string())
    }
}

/// The notes under a diff: where it came from, and that it was cut.
pub fn notes_for(origin: agent_kit::filediff::Origin, omitted_lines: usize) -> Vec<String> {
    use agent_kit::filediff::Origin;
    let mut notes = Vec::new();
    match origin {
        Origin::Checkpoint => notes.push("Changes this turn".to_owned()),
        Origin::AgentEdit => notes.push("from the agent's edit".to_owned()),
    }
    if omitted_lines > 0 {
        notes.push(format!(
            "Diff truncated, {omitted_lines} more lines — open externally"
        ));
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_kit::filediff::Origin;

    #[test]
    fn the_notes_say_where_the_diff_came_from_and_when_it_was_cut() {
        assert_eq!(notes_for(Origin::Checkpoint, 0), ["Changes this turn"]);
        assert_eq!(notes_for(Origin::AgentEdit, 0), ["from the agent's edit"]);
        let cut = notes_for(Origin::Checkpoint, 12);
        assert_eq!(cut.len(), 2);
        assert!(cut[1].starts_with("Diff truncated"), "{cut:?}");
        assert!(cut[1].ends_with("open externally"));
    }
}
