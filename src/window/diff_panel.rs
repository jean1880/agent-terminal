//! The per-tab diff panel: a read-only view of what changed in the tab's
//! repository — uncommitted, over the last turn, or since the tab opened.
//!
//! This module only builds and fills widgets. The diff itself comes from
//! [`crate::git::tab_diff`], run off the main thread by the window, and the
//! text logic is in [`crate::diff`].

use crate::diff::{DiffBase, LineKind};
use crate::git::{DiffOutcome, TabDiff};
use crate::theme::DiffColours;
use adw::prelude::*;
use gtk4::glib;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Narrowest the panel may be dragged.
pub const MIN_WIDTH: i32 = 220;

const TAG_ADDED: &str = "added";
const TAG_REMOVED: &str = "removed";
const TAG_HUNK: &str = "hunk";
const TAG_META: &str = "meta";
const TAG_FILE: &str = "file";

/// CSS class of every diff view, styled by [`apply_view_colours`].
const VIEW_CLASS: &str = "agent-diff-view";

thread_local! {
    /// One provider for every panel: the theme is global, so they all share
    /// its colours. GTK objects stay on the main thread, hence thread-local.
    static VIEW_CSS: (gtk4::CssProvider, Cell<bool>) =
        (gtk4::CssProvider::new(), Cell::new(false));
}

/// Paints diff views with the terminal's background and text colours, so
/// the panel reads as part of the terminal whatever the desktop's own light
/// or dark style.
fn apply_view_colours(colours: &DiffColours) {
    let css = format!(
        ".{VIEW_CLASS}, .{VIEW_CLASS} text {{ background-color: {}; color: {}; }}",
        colours.background, colours.text
    );
    VIEW_CSS.with(|(provider, installed)| {
        provider.load_from_data(&css);
        if !installed.get() {
            if let Some(display) = gtk4::gdk::Display::default() {
                gtk4::style_context_add_provider_for_display(
                    &display,
                    provider,
                    gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
                );
                installed.set(true);
            }
        }
    });
}

/// The panel's widgets. Cheap to clone: every field is a reference.
#[derive(Clone)]
pub struct DiffPanel {
    /// The panel itself, placed beside the terminal.
    pub root: gtk4::Box,
    base: gtk4::DropDown,
    summary: gtk4::Label,
    refresh: gtk4::Button,
    undo: gtk4::Button,
    /// What the Undo button restores to, for the diff on show.
    undo_to: Rc<RefCell<Option<String>>>,
    stack: gtk4::Stack,
    status: adw::StatusPage,
    retry: gtk4::Button,
    files: gtk4::ListBox,
    view: gtk4::TextView,
    /// The buffer line on which each listed file's diff starts, by row.
    file_lines: Rc<RefCell<Vec<i32>>>,
    /// Bumped by every refresh, so a slow one that finishes after a newer one
    /// cannot overwrite it.
    generation: Rc<Cell<u64>>,
    /// Whether the saved width has been applied since the panel was last
    /// shown. Until it has, the divider is where GTK put it, not where the
    /// user did, and must not be saved as their choice.
    placed: Rc<Cell<bool>>,
    /// Set while code moves the divider, so that move is not taken for a drag.
    placing: Rc<Cell<bool>>,
}

impl DiffPanel {
    pub fn new(colours: &DiffColours) -> Self {
        let labels: Vec<&str> = DiffBase::ALL.iter().map(|b| b.label()).collect();
        let base = gtk4::DropDown::from_strings(&labels);
        base.set_tooltip_text(Some("Compare against"));

        let summary = gtk4::Label::builder()
            .hexpand(true)
            .xalign(0.0)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(["dim-label"])
            .build();
        let refresh = gtk4::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Refresh")
            .css_classes(["flat"])
            .build();
        let header = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(6)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        // Offered only for a checkpoint base with something to undo; see
        // `show_diff`.
        let undo = gtk4::Button::builder()
            .icon_name("edit-undo-symbolic")
            .tooltip_text("Undo these changes…")
            .css_classes(["flat"])
            .visible(false)
            .build();
        header.append(&base);
        header.append(&summary);
        header.append(&undo);
        header.append(&refresh);

        let retry = gtk4::Button::builder()
            .label("Retry")
            .halign(gtk4::Align::Center)
            .css_classes(["pill"])
            .visible(false)
            .build();
        let status = adw::StatusPage::builder()
            .child(&retry)
            .vexpand(true)
            .build();
        status.add_css_class("compact");

        let files = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::None)
            .activate_on_single_click(true)
            .css_classes(["navigation-sidebar"])
            .build();
        let files_scroll = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .max_content_height(160)
            .propagate_natural_height(true)
            .child(&files)
            .build();

        let view = gtk4::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk4::WrapMode::None)
            .left_margin(6)
            .right_margin(6)
            .top_margin(4)
            .bottom_margin(4)
            .build();
        // Added, not set through the builder: `css_classes` replaces the list,
        // and with it the `monospace` class that `.monospace(true)` put there.
        view.add_css_class(VIEW_CLASS);
        let buffer = view.buffer();
        for name in [TAG_ADDED, TAG_REMOVED, TAG_HUNK, TAG_META, TAG_FILE] {
            buffer.create_tag(Some(name), &[]);
        }
        if let Some(tag) = buffer.tag_table().lookup(TAG_FILE) {
            tag.set_weight(700);
        }
        let view_scroll = gtk4::ScrolledWindow::builder()
            .vexpand(true)
            .child(&view)
            .build();

        let diff_page = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .build();
        diff_page.append(&files_scroll);
        diff_page.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
        diff_page.append(&view_scroll);

        let stack = gtk4::Stack::new();
        stack.add_named(&status, Some("status"));
        stack.add_named(&diff_page, Some("diff"));

        let root = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .width_request(MIN_WIDTH)
            .build();
        root.append(&header);
        root.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
        root.append(&stack);

        let panel = Self {
            root,
            base,
            summary,
            refresh,
            undo,
            undo_to: Rc::new(RefCell::new(None)),
            stack,
            status,
            retry,
            files,
            view,
            file_lines: Rc::new(RefCell::new(Vec::new())),
            generation: Rc::new(Cell::new(0)),
            placed: Rc::new(Cell::new(false)),
            placing: Rc::new(Cell::new(false)),
        };
        panel.set_colours(colours);
        panel.wire_file_jumps();
        panel
    }

    /// Shows or hides the panel. A shown panel gets its saved width again at
    /// the next [`Self::place`], once the paned has measured the new layout.
    pub fn set_shown(&self, shown: bool) {
        self.placed.set(false);
        self.root.set_visible(shown);
    }

    /// Gives the panel `width` pixels of `paned`, if that has not been done
    /// since it was shown and `paned` has a size to take them from. Called
    /// from the paned's layout notifications until it succeeds.
    pub fn place(&self, paned: &gtk4::Paned, width: i32) {
        // `placing`: set_position re-enters through the position handler.
        if self.placed.get() || self.placing.get() || !self.root.is_visible() || paned.width() <= 0
        {
            return;
        }
        self.placing.set(true);
        paned.set_position((paned.width() - width).max(0));
        self.placing.set(false);
        self.placed.set(true);
    }

    /// The width the user has dragged the panel to, when a position change
    /// is that: not a placement, not a hidden or unplaced panel.
    pub fn dragged_width(&self, paned: &gtk4::Paned) -> Option<i32> {
        if !self.placed.get() || self.placing.get() || !self.root.is_visible() {
            return None;
        }
        let width = paned.width() - paned.position();
        (paned.width() > 0 && width >= MIN_WIDTH).then_some(width)
    }

    /// The base currently chosen in the dropdown.
    pub fn base(&self) -> DiffBase {
        DiffBase::ALL
            .get(self.base.selected() as usize)
            .copied()
            .unwrap_or_default()
    }

    /// Calls `refresh` when the user asks for one: the refresh button, the
    /// retry button, or a change of base.
    pub fn connect_refresh(&self, refresh: impl Fn() + Clone + 'static) {
        let on_click = refresh.clone();
        self.refresh.connect_clicked(move |_| on_click());
        let on_retry = refresh.clone();
        self.retry.connect_clicked(move |_| on_retry());
        self.base.connect_selected_notify(move |_| refresh());
    }

    /// Recolours the diff text, as when the terminal theme changes.
    pub fn set_colours(&self, colours: &DiffColours) {
        apply_view_colours(colours);
        let table = self.view.buffer().tag_table();
        for (name, colour) in [
            (TAG_ADDED, &colours.added),
            (TAG_REMOVED, &colours.removed),
            (TAG_HUNK, &colours.hunk),
            (TAG_META, &colours.meta),
            (TAG_FILE, &colours.file),
        ] {
            if let Some(tag) = table.lookup(name) {
                tag.set_foreground_rgba(Some(colour));
            }
        }
    }

    /// What the panel shows: the stack page, the status title, the summary,
    /// the diff text and how many files are listed.
    #[cfg(test)]
    pub fn visible_state(&self) -> (String, String, String, String, usize) {
        let buffer = self.view.buffer();
        let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
        let mut rows = 0;
        while self.files.row_at_index(rows).is_some() {
            rows += 1;
        }
        (
            self.stack
                .visible_child_name()
                .map(|s| s.to_string())
                .unwrap_or_default(),
            self.status.title().to_string(),
            self.summary.text().to_string(),
            text.to_string(),
            usize::try_from(rows).unwrap_or(0),
        )
    }

    /// Marks a refresh as started and returns its generation, which
    /// [`Self::show`] must be given back.
    pub fn begin(&self) -> u64 {
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        self.summary.set_text("Loading…");
        generation
    }

    /// Shows a finished refresh, unless a newer one has started since.
    pub fn show(&self, generation: u64, result: Result<DiffOutcome, String>) {
        if generation != self.generation.get() {
            return;
        }
        match result {
            Err(err) => self.show_status("dialog-warning-symbolic", "Git failed", &err, true),
            Ok(DiffOutcome::NotRepo) => self.show_status(
                "folder-symbolic",
                "Not a git repository",
                "This tab's folder is not inside a repository.",
                false,
            ),
            Ok(DiffOutcome::Unavailable(reason)) => self.show_status(
                "document-open-recent-symbolic",
                "Nothing to show yet",
                &reason,
                false,
            ),
            Ok(DiffOutcome::Ready(diff)) if diff.stats.is_empty() => {
                self.show_status("object-select-symbolic", "No changes", "", false)
            }
            Ok(DiffOutcome::Ready(diff)) => self.show_diff(&diff),
        }
    }

    fn show_status(&self, icon: &str, title: &str, description: &str, retry: bool) {
        self.set_undo(None);
        self.summary.set_text("");
        self.status.set_icon_name(Some(icon));
        self.status.set_title(title);
        // The description is markup; git's messages are not.
        self.status
            .set_description(Some(&glib::markup_escape_text(description)));
        self.retry.set_visible(retry);
        self.stack.set_visible_child_name("status");
    }

    fn set_undo(&self, to: Option<String>) {
        self.undo.set_visible(to.is_some());
        *self.undo_to.borrow_mut() = to;
    }

    /// Calls `undo` with the revision to restore to when Undo is clicked.
    pub fn connect_undo(&self, undo: impl Fn(String) + 'static) {
        let undo_to = Rc::clone(&self.undo_to);
        self.undo.connect_clicked(move |_| {
            let target = undo_to.borrow().clone();
            if let Some(target) = target {
                undo(target);
            }
        });
    }

    /// Whether Undo is on offer.
    #[cfg(test)]
    pub fn undo_offered(&self) -> bool {
        self.undo.is_visible() && self.undo_to.borrow().is_some()
    }

    fn show_diff(&self, diff: &TabDiff) {
        self.set_undo(diff.undo_to.clone());
        self.summary.set_text(&crate::diff::summary(&diff.stats));

        while let Some(row) = self.files.row_at_index(0) {
            self.files.remove(&row);
        }
        for stat in &diff.stats {
            let label = gtk4::Label::builder()
                .label(crate::diff::file_row(stat))
                .xalign(0.0)
                .ellipsize(gtk4::pango::EllipsizeMode::Middle)
                .tooltip_text(stat.path.as_str())
                .build();
            self.files.append(&label);
        }

        let buffer = self.view.buffer();
        let mut file_lines = Vec::new();
        if diff.too_large {
            let (_, added, deleted) = crate::diff::totals(&diff.stats);
            buffer.set_text(&format!(
                "This diff is too large to show ({} changed lines). The file list above is complete.",
                added + deleted
            ));
        } else {
            buffer.set_text(&diff.text);
            for (line, kind) in crate::diff::classify(&diff.text).into_iter().enumerate() {
                let tag = match kind {
                    LineKind::Added => TAG_ADDED,
                    LineKind::Removed => TAG_REMOVED,
                    LineKind::Hunk => TAG_HUNK,
                    LineKind::Meta => TAG_META,
                    LineKind::File => TAG_FILE,
                    LineKind::Context => continue,
                };
                let Ok(line) = i32::try_from(line) else {
                    break;
                };
                if kind == LineKind::File {
                    file_lines.push(line);
                }
                if let Some(start) = buffer.iter_at_line(line) {
                    let mut end = start;
                    end.forward_to_line_end();
                    buffer.apply_tag_by_name(tag, &start, &end);
                }
            }
            if diff.omitted_lines > 0 {
                let mut end = buffer.end_iter();
                let note = format!(
                    "\n… diff truncated: {} more lines not shown",
                    diff.omitted_lines
                );
                buffer.insert_with_tags_by_name(&mut end, &note, &[TAG_META]);
            }
        }
        *self.file_lines.borrow_mut() = file_lines;
        self.stack.set_visible_child_name("diff");
    }

    /// Clicking a file scrolls the diff to where it starts. Files and
    /// `diff --git` headers come out of git in the same order.
    fn wire_file_jumps(&self) {
        let view = self.view.clone();
        let file_lines = Rc::clone(&self.file_lines);
        self.files.connect_row_activated(move |_, row| {
            let Ok(index) = usize::try_from(row.index()) else {
                return;
            };
            let Some(line) = file_lines.borrow().get(index).copied() else {
                return;
            };
            if let Some(mut iter) = view.buffer().iter_at_line(line) {
                view.scroll_to_iter(&mut iter, 0.0, true, 0.0, 0.0);
            }
        });
    }
}
