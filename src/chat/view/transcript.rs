//! The scrolling transcript.
//!
//! # Spike verdict (2026-10-06): a windowed `GtkBox`, not `GtkListView`
//!
//! The plan made the transcript container the highest-risk choice and asked for a spike against
//! four criteria: 5k mixed items, streaming into the last row, expand/collapse surviving, and
//! stick-to-bottom unless the user scrolled up. `GtkListView` over a `gio::ListStore` of item
//! objects was evaluated against the fallback, a `GtkBox` in a `GtkScrolledWindow` that holds
//! only a window of the newest items and loads older ones on scroll-up. The fallback won:
//!
//! - **Stick-to-bottom.** `ListView::scroll_to` needs GTK 4.12 and the floor is 4.10. Without
//!   it the only lever is the vertical adjustment, and a list view's `upper` is an *estimate*
//!   built from the rows it has measured: it jumps while a variable-height row streams and as
//!   unmeasured rows scroll in, so "pin to the end" fights the estimate. In a box `upper` is
//!   exact, so pinning is one assignment on `notify::upper`.
//! - **Streaming into the last row.** A list view recycles row widgets and rebinds them, so a
//!   markdown row would be rebuilt (labels, source views) on every rebind. Here each row keeps its
//!   widgets and a streamed delta re-renders only the trailing markdown block.
//! - **Nesting.** Subagent cards hold their children inline. A flat list model would need a
//!   `TreeListModel` and per-depth indentation; in a box the child rows simply live inside the
//!   card.
//! - **Expand/collapse.** Kept on the model item (`Item::expanded`) either way; in a box the
//!   widget also simply keeps it while materialised.
//! - **5k items.** The reducer holds all 5k (it reduces 5k items in a few ms; see
//!   `model::tests::five_thousand_mixed_items_reduce_quickly`). Only the newest
//!   [`WINDOW`] top-level rows are widgets; scrolling to the top materialises [`PAGE`] more
//!   while keeping the viewport anchored, and returning to the bottom (or following a stream)
//!   trims back to the window. Measured with
//!   `AGENT_TERMINAL_DEMO_STRESS=5000 agent-terminal --chat-demo` (debug build, Broadway):
//!   `ChatView::replay` of 5k items / 12.5k envelopes took 16 ms; appending the same 5k one
//!   envelope at a time (the live path, every row built) took 478 ms before trimming was added.
//!   Thread-open history must therefore go through `replay`, not `apply`.
//!
//! The cost of the box is memory proportional to how far up the user has scrolled, which the
//! trim on return bounds.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;

use super::cards::{Row, RowSink};
use super::model::{ItemId, Transcript};

/// Top-level rows kept as widgets when the user is at the bottom.
pub const WINDOW: usize = 150;
/// Rows materialised per scroll-up step.
pub const PAGE: usize = 60;
/// Distance from the end (px) that still counts as "at the bottom".
const STICK_SLOP: f64 = 48.0;

pub struct TranscriptView {
    root: gtk4::Overlay,
    scroller: gtk4::ScrolledWindow,
    list: gtk4::Box,
    jump: gtk4::Button,
    rows: RefCell<HashMap<ItemId, Row>>,
    /// Index (into `Transcript::order`) of the first materialised top-level item.
    first: Cell<usize>,
    stick: Cell<bool>,
    /// Distance from the end to restore after prepending older rows.
    anchor: Cell<Option<f64>>,
    sink: RowSink,
    empty: gtk4::Box,
}

impl TranscriptView {
    pub fn new(sink: RowSink) -> Rc<Self> {
        let list = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
        list.add_css_class("transcript");
        list.set_valign(gtk4::Align::End);
        // A readable measure: centred column, capped width.
        let clamp = adw::Clamp::new();
        clamp.set_maximum_size(860);
        clamp.set_tightening_threshold(640);
        clamp.set_child(Some(&list));

        let scroller = gtk4::ScrolledWindow::new();
        scroller.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        scroller.set_vexpand(true);
        // The viewport must size its content by *natural* height: the default (minimum) lets
        // nested propagate-natural-height scrollers (tool output) shrink, so the end of the
        // transcript fell outside the scrollable range.
        let viewport = gtk4::Viewport::new(None::<&gtk4::Adjustment>, None::<&gtk4::Adjustment>);
        viewport.set_vscroll_policy(gtk4::ScrollablePolicy::Natural);
        viewport.set_child(Some(&clamp));
        scroller.set_child(Some(&viewport));
        scroller.add_css_class("transcript-scroller");

        let empty = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        empty.add_css_class("transcript-empty");
        empty.set_valign(gtk4::Align::Center);
        empty.set_halign(gtk4::Align::Center);
        let title = gtk4::Label::new(Some("Start a conversation"));
        title.add_css_class("empty-title");
        let hint = gtk4::Label::new(Some(
            "Type a message below. / for commands, @ for files, $ for skills.",
        ));
        hint.add_css_class("empty-hint");
        empty.append(&title);
        empty.append(&hint);
        empty.set_can_target(false);

        let jump = gtk4::Button::from_icon_name("go-bottom-symbolic");
        jump.add_css_class("circular");
        jump.add_css_class("osd");
        jump.add_css_class("jump-button");
        jump.set_halign(gtk4::Align::End);
        jump.set_valign(gtk4::Align::End);
        jump.set_margin_end(24);
        jump.set_margin_bottom(16);
        jump.set_tooltip_text(Some("Jump to the latest message"));
        jump.set_visible(false);

        let root = gtk4::Overlay::new();
        root.set_child(Some(&scroller));
        root.add_overlay(&empty);
        root.add_overlay(&jump);

        let view = Rc::new(Self {
            root,
            scroller,
            list,
            jump,
            rows: RefCell::new(HashMap::new()),
            first: Cell::new(0),
            stick: Cell::new(true),
            anchor: Cell::new(None),
            sink,
            empty,
        });
        view.connect_scrolling();
        view
    }

    pub fn widget(&self) -> &gtk4::Overlay {
        &self.root
    }

    fn connect_scrolling(self: &Rc<Self>) {
        let adj = self.scroller.vadjustment();
        // Only the user scrolling *up* unsticks: content growing, page-size changes and GTK's own
        // clamping never move the value up with an unchanged upper bound.
        let weak = Rc::downgrade(self);
        let last = Cell::new((0.0_f64, 0.0_f64));
        adj.connect_value_changed(move |adj| {
            let Some(view) = weak.upgrade() else {
                return;
            };
            let (last_value, last_upper) = last.replace((adj.value(), adj.upper()));
            let at_bottom = adj.value() + adj.page_size() >= adj.upper() - STICK_SLOP;
            if at_bottom {
                view.stick.set(true);
                view.jump.set_visible(false);
            } else if adj.value() < last_value - 0.5 && (adj.upper() - last_upper).abs() < 0.5 {
                view.stick.set(false);
            }
        });
        let weak = Rc::downgrade(self);
        let on_size = move |adj: &gtk4::Adjustment| {
            let Some(view) = weak.upgrade() else {
                return;
            };
            if let Some(from_end) = view.anchor.take() {
                adj.set_value((adj.upper() - from_end).max(0.0));
            } else if view.stick.get() {
                adj.set_value(adj.upper() - adj.page_size());
            }
        };
        adj.connect_upper_notify(on_size.clone());
        adj.connect_page_size_notify(on_size);
        let weak = Rc::downgrade(self);
        self.jump.connect_clicked(move |_| {
            if let Some(view) = weak.upgrade() {
                view.scroll_to_end();
            }
        });
    }

    /// Connects the "scrolled to the top" edge to loading older rows. Separate from `new` because
    /// it needs the model, which the view owns.
    pub fn connect_load_older(self: &Rc<Self>, model: Rc<RefCell<Transcript>>) {
        let weak = Rc::downgrade(self);
        let weak_model = Rc::downgrade(&model);
        self.scroller.connect_edge_reached(move |_, pos| {
            if pos != gtk4::PositionType::Top {
                return;
            }
            let (Some(view), Some(model)) = (weak.upgrade(), weak_model.upgrade()) else {
                return;
            };
            // Defer: changing children inside the scroll handler re-enters allocation.
            glib::idle_add_local_once(move || view.load_older(&model.borrow()));
        });
        let weak = Rc::downgrade(self);
        let weak_model = Rc::downgrade(&model);
        self.scroller
            .vadjustment()
            .connect_value_changed(move |adj| {
                let at_bottom = adj.value() + adj.page_size() >= adj.upper() - 1.0;
                if !at_bottom {
                    return;
                }
                if let (Some(view), Some(model)) = (weak.upgrade(), weak_model.upgrade()) {
                    if let Ok(m) = model.try_borrow() {
                        view.trim(&m);
                    }
                }
            });
    }

    pub fn scroll_to_end(&self) {
        self.stick.set(true);
        self.jump.set_visible(false);
        let adj = self.scroller.vadjustment();
        adj.set_value(adj.upper() - adj.page_size());
    }

    /// Rebuilds everything from the model (initial load, or a thread swap).
    pub fn reset(&self, model: &Transcript) {
        for (_, row) in self.rows.borrow_mut().drain() {
            if row.widget().parent().as_ref() == Some(self.list.upcast_ref()) {
                self.list.remove(&row.widget());
            }
        }
        let order = model.order();
        let first = order.len().saturating_sub(WINDOW);
        self.first.set(first);
        for id in &order[first..] {
            if let Some(w) = self.build_tree(model, id) {
                self.list.append(&w);
            }
        }
        self.empty.set_visible(order.is_empty());
        self.stick.set(true);
    }

    /// Builds a row and its nested children, registering every one.
    fn build_tree(&self, model: &Transcript, id: &str) -> Option<gtk4::Widget> {
        let item = model.get(id)?;
        let row = Row::build(item, &self.sink);
        let widget = row.widget();
        if let Some(children) = row.children_box() {
            for child in &item.children {
                if let Some(w) = self.build_tree(model, child) {
                    children.append(&w);
                }
            }
        }
        self.rows.borrow_mut().insert(id.to_owned(), row);
        Some(widget)
    }

    fn unregister_tree(&self, model: &Transcript, id: &str) {
        self.rows.borrow_mut().remove(id);
        if let Some(item) = model.get(id) {
            for child in &item.children {
                self.unregister_tree(model, child);
            }
        }
    }

    /// A new item was reduced. Top-level items are appended; nested ones go into their parent
    /// card when that card is materialised (otherwise they appear when it is).
    pub fn added(&self, model: &Transcript, id: &str) {
        let Some(item) = model.get(id) else {
            return;
        };
        self.empty.set_visible(false);
        match &item.parent {
            Some(parent) => {
                let parent_box = self
                    .rows
                    .borrow()
                    .get(parent)
                    .and_then(|r| r.children_box().cloned());
                if let Some(children) = parent_box {
                    if let Some(w) = self.build_tree(model, id) {
                        children.append(&w);
                    }
                    self.updated(model, parent);
                }
            }
            None => {
                if let Some(w) = self.build_tree(model, id) {
                    self.list.append(&w);
                }
                if self.stick.get() {
                    // Keep the window bounded while following the stream.
                    self.trim(model);
                } else {
                    self.jump.set_visible(true);
                }
            }
        }
    }

    pub fn updated(&self, model: &Transcript, id: &str) {
        if let (Some(item), Some(row)) = (model.get(id), self.rows.borrow().get(id)) {
            row.update(item);
        }
        if !self.stick.get() && model.order().last().map(String::as_str) == Some(id) {
            self.jump.set_visible(true);
        }
    }

    /// Materialises up to [`PAGE`] older top-level rows above the current window, keeping the
    /// viewport where it was.
    pub fn load_older(&self, model: &Transcript) {
        let first = self.first.get();
        if first == 0 {
            return;
        }
        let new_first = first.saturating_sub(PAGE);
        let adj = self.scroller.vadjustment();
        self.anchor.set(Some(adj.upper() - adj.value()));
        for id in model.order()[new_first..first].iter().rev() {
            if let Some(w) = self.build_tree(model, id) {
                self.list.prepend(&w);
            }
        }
        self.first.set(new_first);
        tracing::debug!(first = new_first, "chat transcript: loaded older rows");
    }

    /// Back at the bottom: drop rows beyond the window so memory stays bounded.
    fn trim(&self, model: &Transcript) {
        let order = model.order();
        let keep_from = order.len().saturating_sub(WINDOW);
        let first = self.first.get();
        // Only trim once the excess is worth a relayout.
        if keep_from <= first + PAGE {
            return;
        }
        for id in &order[first..keep_from] {
            let widget = self.rows.borrow().get(id).map(Row::widget);
            if let Some(w) = widget {
                self.list.remove(&w);
            }
            self.unregister_tree(model, id);
        }
        self.first.set(keep_from);
    }

    #[cfg(test)]
    pub fn materialised(&self) -> usize {
        let mut n = 0;
        let mut child = self.list.first_child();
        while let Some(c) = child {
            n += 1;
            child = c.next_sibling();
        }
        n
    }
}
