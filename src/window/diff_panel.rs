//! The per-tab diff panel: a read-only view of what changed in the tab's
//! repository — uncommitted, over the last turn, or since the tab opened.
//!
//! The changed files are listed under a header per folder, each file a
//! collapsed row (name and `+3 −1`) that expands to its own diff, so the diffs
//! are separate and explored one at a time. A file's diff view is built the
//! first time it is expanded: a large change set lists instantly.
//!
//! This module only builds and fills widgets. The diff itself comes from
//! [`crate::git::tab_diff`], run off the main thread by the window, and the
//! text logic (classifying, splitting by file, grouping by folder) is in
//! [`crate::diff`].

use crate::diff::{DiffBase, LineKind};
use crate::diff_tool::DiffTools;
use crate::git::{DiffOutcome, TabDiff};
use crate::theme::DiffColours;
use adw::prelude::*;
use gtk4::glib;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Narrowest width the panel remembers being dragged to.
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

/// Gives `view`'s line tags the diff colours.
fn colour_tags(view: &gtk4::TextView, colours: &DiffColours) {
    let table = view.buffer().tag_table();
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

/// A read-only view of one file's diff, each line coloured by its kind.
fn file_view(text: &str, colours: &DiffColours) -> gtk4::TextView {
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
    colour_tags(&view, colours);
    buffer.set_text(text.strip_suffix('\n').unwrap_or(text));
    for (line, kind) in crate::diff::classify(text).into_iter().enumerate() {
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
        if let Some(start) = buffer.iter_at_line(line) {
            let mut end = start;
            end.forward_to_line_end();
            buffer.apply_tag_by_name(tag, &start, &end);
        }
    }
    view
}

/// A file the user asked to open in the external diff tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenFile {
    /// Repository-relative, as git named it.
    pub path: String,
    /// The commit or tree the old side is read from.
    pub from: String,
    /// The new side when it is a checkpoint; `None`: the working file.
    pub to_checkpoint: Option<String>,
}

type OpenHandler = Rc<dyn Fn(OpenFile)>;

/// The diff on show: its left side, and its right side when that is a checkpoint.
type Sides = (String, Option<String>);

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
    /// The folder headers and file rows of the diff on show.
    files: gtk4::Box,
    /// Under the files: why some diffs are not shown (too large, cut short).
    note: gtk4::Label,
    /// Each listed file's diff text, by `stats` index (empty: not shown).
    chunks: Rc<RefCell<Vec<String>>>,
    /// How many file rows are listed.
    file_rows: Rc<Cell<usize>>,
    /// Every file view built so far, recoloured with the theme.
    views: Rc<RefCell<Vec<gtk4::TextView>>>,
    /// The diff colours, for views built after a theme change.
    colours: Rc<RefCell<DiffColours>>,
    /// Bumped by every refresh, so a slow one that finishes after a newer one
    /// cannot overwrite it.
    generation: Rc<Cell<u64>>,
    /// Called with the file to open when a row's "Open in …" is clicked.
    open_handler: Rc<RefCell<Option<OpenHandler>>>,
    /// The diff on show, for the rows' open buttons.
    shown_sides: Rc<RefCell<Option<Sides>>>,
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
            .icon_name("at-view-refresh-symbolic")
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
            .icon_name("at-edit-undo-symbolic")
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

        let files = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(2)
            .margin_start(6)
            .margin_end(6)
            .margin_top(4)
            .margin_bottom(6)
            .build();
        let note = gtk4::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .visible(false)
            .margin_top(6)
            .css_classes(["dim-label"])
            .build();
        let column = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        column.append(&files);
        column.append(&note);
        let diff_page = gtk4::ScrolledWindow::builder()
            .vexpand(true)
            .child(&column)
            .build();

        let stack = gtk4::Stack::new();
        stack.add_named(&status, Some("status"));
        stack.add_named(&diff_page, Some("diff"));

        // No width request: the paned lets the panel narrow in a small window. MIN_WIDTH is only
        // the floor for a width the user drags it to.
        let root = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
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
            note,
            chunks: Rc::new(RefCell::new(Vec::new())),
            file_rows: Rc::new(Cell::new(0)),
            views: Rc::new(RefCell::new(Vec::new())),
            colours: Rc::new(RefCell::new(colours.clone())),
            generation: Rc::new(Cell::new(0)),
            open_handler: Rc::new(RefCell::new(None)),
            shown_sides: Rc::new(RefCell::new(None)),
            placed: Rc::new(Cell::new(false)),
            placing: Rc::new(Cell::new(false)),
        };
        panel.set_colours(colours);
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

    /// Recolours the diff text, as when the terminal theme changes: every file
    /// view built so far, and those built from now on.
    pub fn set_colours(&self, colours: &DiffColours) {
        apply_view_colours(colours);
        *self.colours.borrow_mut() = colours.clone();
        for view in self.views.borrow().iter() {
            colour_tags(view, colours);
        }
    }

    /// What the panel shows: the stack page, the status title, the summary,
    /// the diff text (every file's, expanded or not, then the note) and how
    /// many files are listed.
    #[cfg(test)]
    pub fn visible_state(&self) -> (String, String, String, String, usize) {
        let mut text = self.chunks.borrow().concat();
        if self.note.is_visible() {
            text.push('\n');
            text.push_str(&self.note.text());
        }
        (
            self.stack
                .visible_child_name()
                .map(|s| s.to_string())
                .unwrap_or_default(),
            self.status.title().to_string(),
            self.summary.text().to_string(),
            text,
            self.file_rows.get(),
        )
    }

    /// The folder headers on show, in order (tests).
    #[cfg(test)]
    pub fn folder_headers(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut child = self.files.first_child();
        while let Some(widget) = child {
            if let Some(label) = widget.downcast_ref::<gtk4::Label>() {
                out.push(label.text().to_string());
            }
            child = widget.next_sibling();
        }
        out
    }

    /// The file rows' expanders, in order (tests).
    #[cfg(test)]
    pub fn file_expanders(&self) -> Vec<gtk4::Expander> {
        let mut out = Vec::new();
        let mut child = self.files.first_child();
        while let Some(widget) = child {
            if let Ok(expander) = widget.clone().downcast::<gtk4::Expander>() {
                out.push(expander);
            }
            child = widget.next_sibling();
        }
        out
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
            Err(err) => self.show_status("at-dialog-warning-symbolic", "Git failed", &err, true),
            Ok(DiffOutcome::NotRepo) => self.show_status(
                "at-folder-symbolic",
                "Not a git repository",
                "This tab's folder is not inside a repository.",
                false,
            ),
            Ok(DiffOutcome::Unavailable(reason)) => self.show_status(
                "at-document-open-recent-symbolic",
                "Nothing to show yet",
                &reason,
                false,
            ),
            Ok(DiffOutcome::Ready(diff)) if diff.stats.is_empty() => {
                self.show_status("at-object-select-symbolic", "No changes", "", false)
            }
            Ok(DiffOutcome::Ready(diff)) => self.show_diff(&diff),
        }
    }

    fn show_status(&self, icon: &str, title: &str, description: &str, retry: bool) {
        self.set_undo(None);
        self.summary.set_text("");
        crate::icons::set_status_icon(&self.status, icon);
        self.status.set_title(title);
        // The description is markup; git's messages are not.
        self.status
            .set_description(Some(&glib::markup_escape_text(description)));
        self.retry.set_visible(retry);
        self.stack.set_visible_child_name("status");
    }

    /// Calls `open` when a file row's "Open in …" is clicked.
    pub fn connect_open(&self, open: impl Fn(OpenFile) + 'static) {
        *self.open_handler.borrow_mut() = Some(Rc::new(open));
    }

    /// A row's "Open in ‹tool›" button: hidden while no tool is configured, and following the
    /// setting when it changes. Renamed files open as the new path (the old side reads empty).
    fn open_button(&self, path: String) -> gtk4::Button {
        let button = gtk4::Button::builder()
            .icon_name(crate::icons::EXTERNAL_ICON)
            .css_classes(["flat", "diff-open"])
            .valign(gtk4::Align::Center)
            .build();
        let refresh = |button: &gtk4::Button| match DiffTools::shared().get() {
            Some(tool) => {
                button.set_visible(true);
                button.set_tooltip_text(Some(&crate::diff_tool::open_label(&tool)));
            }
            None => {
                button.set_visible(false);
                button.set_tooltip_text(Some(crate::diff_tool::NO_TOOL_HINT));
            }
        };
        refresh(&button);
        let conn = DiffTools::shared().connect_changed(glib::clone!(
            #[weak]
            button,
            move || refresh(&button)
        ));
        button.connect_destroy(move |_| DiffTools::shared().disconnect(conn));

        let handler = Rc::clone(&self.open_handler);
        let sides = Rc::clone(&self.shown_sides);
        button.connect_clicked(move |_| {
            let (Some(handler), Some((from, to_checkpoint))) =
                (handler.borrow().clone(), sides.borrow().clone())
            else {
                return;
            };
            handler(OpenFile {
                path: path.clone(),
                from,
                to_checkpoint,
            });
        });
        button
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
        *self.shown_sides.borrow_mut() = Some((diff.from.clone(), diff.to_checkpoint.clone()));

        while let Some(child) = self.files.first_child() {
            self.files.remove(&child);
        }
        self.views.borrow_mut().clear();

        // Chunk `i` is `stats[i]`'s: git writes `--numstat` and the patch in the same order. A
        // diff cut short has fewer chunks than files; a too-large one has none.
        let chunks: Vec<String> = if diff.too_large {
            Vec::new()
        } else {
            crate::diff::split_by_file(&diff.text)
                .into_iter()
                .map(str::to_owned)
                .collect()
        };
        let mut by_stat = vec![String::new(); diff.stats.len()];
        for (slot, chunk) in by_stat.iter_mut().zip(chunks) {
            *slot = chunk;
        }
        let cut_short = !diff.too_large && diff.omitted_lines > 0;

        let mut rows = 0;
        for (folder, files) in crate::diff::group_by_folder(&diff.stats) {
            let heading = if folder.is_empty() {
                "Repository root".to_owned()
            } else {
                format!("{folder}/")
            };
            let header = gtk4::Label::builder()
                .label(heading)
                .xalign(0.0)
                .ellipsize(gtk4::pango::EllipsizeMode::Start)
                .tooltip_text(folder.as_str())
                .margin_top(if rows == 0 { 0 } else { 8 })
                .css_classes(["heading", "dim-label"])
                .build();
            self.files.append(&header);
            for index in files {
                let stat = &diff.stats[index];
                let text = by_stat[index].clone();
                let why_hidden = if diff.too_large {
                    Some("Not shown: the diff is too large. Open it in your diff tool.")
                } else if text.is_empty() && cut_short {
                    Some("Not shown: the diff was cut short before this file.")
                } else if text.is_empty() {
                    Some("No text diff for this file.")
                } else {
                    None
                };
                self.files.append(&self.file_row(stat, text, why_hidden));
                rows += 1;
            }
        }
        self.file_rows.set(rows);
        *self.chunks.borrow_mut() = by_stat;

        let note = if diff.too_large {
            let (_, added, deleted) = crate::diff::totals(&diff.stats);
            Some(format!(
                "This diff is too large to show ({} changed lines). The file list above is complete.",
                added + deleted
            ))
        } else if cut_short {
            Some(format!(
                "… diff truncated: {} more lines not shown",
                diff.omitted_lines
            ))
        } else {
            None
        };
        self.note.set_visible(note.is_some());
        self.note.set_text(note.as_deref().unwrap_or_default());
        self.stack.set_visible_child_name("diff");
    }

    /// One changed file: a collapsed row (name, `+3 −1`, "Open in …") that expands to its own
    /// diff, built on first expand. `why_hidden`: shown instead of a diff there is no text for.
    fn file_row(
        &self,
        stat: &crate::diff::FileStat,
        text: String,
        why_hidden: Option<&'static str>,
    ) -> gtk4::Expander {
        let label = gtk4::Label::builder()
            .label(crate::diff::file_label(stat))
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk4::pango::EllipsizeMode::Middle)
            .tooltip_text(stat.path.as_str())
            .build();
        let title = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        title.append(&label);
        // Binary files open fine in a tool that knows them; the button is for every row.
        title.append(&self.open_button(stat.path.clone()));
        let expander = gtk4::Expander::builder()
            .label_widget(&title)
            .margin_start(8)
            .build();
        expander.add_css_class("diff-file");

        let views = Rc::clone(&self.views);
        let colours = Rc::clone(&self.colours);
        expander.connect_expanded_notify(move |expander| {
            if !expander.is_expanded() || expander.child().is_some() {
                return;
            }
            let child: gtk4::Widget = match why_hidden {
                Some(why) => gtk4::Label::builder()
                    .label(why)
                    .xalign(0.0)
                    .wrap(true)
                    .margin_start(6)
                    .css_classes(["dim-label"])
                    .build()
                    .upcast(),
                None => {
                    let view = file_view(&text, &colours.borrow());
                    views.borrow_mut().push(view.clone());
                    view.upcast()
                }
            };
            expander.set_child(Some(&child));
        });
        expander
    }
}
