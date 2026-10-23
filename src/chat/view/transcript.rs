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
//!
//! # Filling the window in steps
//!
//! A [`TranscriptView::reset`] builds only the newest [`FIRST_PAINT`] rows, so the thread shows
//! at once; the rest of the window is prepended in idle steps of about [`FILL_BUDGET`], below
//! layout and paint in priority, through the same anchored prepend as loading older rows. A jump
//! to a row the fill has not built yet builds the rest at once and scrolls once it is laid out.
//! Measured on real threads of 42k–47k envelopes (release build), building the whole window
//! was 70–80% of the 36–64 ms a thread open held the main thread.
//!
//! # Bounding a live session
//!
//! Every built row is re-wrapped on a width change, so what a resize costs follows the rows
//! built, nested ones included. A card builds only its newest [`CHILD_CAP`] children, under a
//! button that builds the rest; while following, the oldest top-level rows go once the window
//! passes [`WINDOW`] or all rows pass [`ROW_BUDGET`]. Away from the bottom (a jump, a Tab, a
//! scroll up), rows arriving once the budget is reached are not built: they wait as the window's
//! tail, built a [`PAGE`] at a time when the user scrolls down to them, or at once when the view
//! returns to the end. A jump to a row not built (an evicted card, a card's earlier step) builds
//! what it needs first. A row holding focus is never evicted (it stays, pinned above the
//! window), nor is a pending approval, question or form.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use gtk4::glib;
use gtk4::prelude::*;

use super::cards::{Row, RowSink};
use super::model::{ApprovalState, Body, FormState, ItemId, QuestionState, Transcript};

/// How long a live row keeps its entrance class: longer than its animation, so a row moved
/// later (a trim, a reload) does not play it again.
const ENTRANCE_MS: u64 = 600;

/// The entrance a row arriving live makes, by what it is and what comes before it: a message of
/// yours, or the first row of the reply after it, fades in (`row-enter`); an agent switch's
/// divider settles into place (`divider-settle`). Everything else, and every replayed row,
/// simply appears: in a fast stream more motion would only flicker.
fn entrance(this: &Body, before: Option<&Body>) -> Option<&'static str> {
    match (this, before) {
        (Body::Switch { .. }, _) => Some("divider-settle"),
        // A queued message waits for the turn running now: what follows it is not its reply.
        (Body::User { .. }, _) | (_, Some(Body::User { queued: false, .. })) => Some("row-enter"),
        _ => None,
    }
}

/// Top-level rows kept as widgets when the user is at the bottom.
pub const WINDOW: usize = 150;
/// Rows materialised per scroll-up step.
pub const PAGE: usize = 60;
/// Top-level rows a reset builds before it returns: more than a tall window shows.
pub const FIRST_PAINT: usize = 30;
/// Rows the window keeps as widgets, nested ones included. A width change re-wraps every one of
/// them, so a live session's resize grew with every sub-agent step it had built
/// (`resize_tests::resize_cost_of_live_sessions`); this many is still several screens of the
/// densest thread.
pub const ROW_BUDGET: usize = 600;
/// Children a card builds: its newest ones. A button above them builds the rest on request.
pub const CHILD_CAP: usize = 40;
/// Time after which a fill step stops starting new rows and yields to the next frame. Checked
/// between top-level rows, so one large row (a sub-agent card and its children) can overrun it.
const FILL_BUDGET: Duration = Duration::from_millis(6);
/// Distance from the end (px) that counts as reaching the bottom when scrolling down.
const STICK_SLOP: f64 = 48.0;

/// The scroll position and the two heights it depends on, at one moment.
#[derive(Debug, Clone, Copy)]
pub struct Look {
    pub value: f64,
    /// Content height.
    pub upper: f64,
    /// Visible height.
    pub page: f64,
}

pub struct TranscriptView {
    root: gtk4::Overlay,
    scroller: gtk4::ScrolledWindow,
    list: gtk4::Box,
    jump: gtk4::Button,
    rows: RefCell<HashMap<ItemId, Row>>,
    /// Index (into `Transcript::order`) of the first materialised top-level item.
    first: Cell<usize>,
    /// Top-level items after the materialised ones, not built: rows arrived while the view was
    /// away from the bottom with the window at its [`ROW_BUDGET`]. 0 while the window reaches
    /// the end, which it always does while following.
    tail: Cell<usize>,
    /// A load of the next [`PAGE`] of the tail is queued (the user scrolled down to it).
    newer_queued: Cell<bool>,
    /// Per card building only its newest [`CHILD_CAP`] children: the index (into
    /// `Item::children`) of its first built child, and the button that builds the rest.
    earlier: RefCell<HashMap<ItemId, (usize, gtk4::Button)>>,
    /// Cards whose earlier children the user asked for: not capped again while they are built.
    uncapped: RefCell<HashSet<ItemId>>,
    /// A top-level row the trim passed while it held keyboard focus: kept, first in the list,
    /// though older than the window, until focus leaves it or the window grows back to it.
    pinned: RefCell<Option<ItemId>>,
    /// The view itself, for the handlers of the buttons it builds.
    this: Weak<Self>,
    stick: Cell<bool>,
    /// Set while the view moves itself (see `move_to`).
    own_move: Cell<bool>,
    /// The value and content height at the last look, to tell a user scroll from a relayout.
    last_value: Cell<f64>,
    last_upper: Cell<f64>,
    last_page: Cell<f64>,
    /// Distance from the end to restore after prepending older rows.
    anchor: Cell<Option<f64>>,
    /// The value that distance came to once the older rows were measured; applied by the tick.
    restore_to: Cell<Option<f64>>,
    /// A [`Self::settle`] is queued for after the current layout.
    settle_queued: Cell<bool>,
    /// The mapped frame clock's post-layout handler; disconnected on unmap.
    layout_handler: RefCell<Option<(gtk4::gdk::FrameClock, glib::SignalHandlerId)>>,
    /// The per-frame backstop tick, only while mapped: GTK runs a tick callback for as long as
    /// its widget is realised, and a thread's hidden page stays realised, so every open thread
    /// ticked at the display's refresh rate (measured: `idle_cost_of_open_threads`).
    tick: RefCell<Option<gtk4::TickCallbackId>>,
    /// Frames the tick callback has seen (tests read it).
    #[cfg(test)]
    ticks: Cell<u64>,
    /// Times `settle` and `move_to` ran (measurements read them).
    #[cfg(test)]
    settles: Cell<u64>,
    #[cfg(test)]
    moves: Cell<u64>,
    sink: RowSink,
    empty: gtk4::Box,
    /// The app icon's dots over the empty transcript, the thread's agent lit (`set_agent`).
    empty_dots: gtk4::Box,
    /// The model the rows are built from (set by `connect_load_older`), for the fill steps.
    model: RefCell<Weak<RefCell<Transcript>>>,
    /// The pending fill of the window after a reset (see [`FIRST_PAINT`]).
    fill: RefCell<Option<glib::SourceId>>,
    /// A card a jump built just now: it has no position until the next layout, so
    /// [`Self::settle`] scrolls to it then.
    jump_to: RefCell<Option<String>>,
    /// The pending jump also brings the card's focused control into view (an interruption
    /// shelf jump focuses the card's first answer).
    reveal_after_jump: Cell<bool>,
}

impl Drop for TranscriptView {
    fn drop(&mut self) {
        if let Some((clock, handler)) = self.layout_handler.get_mut().take() {
            clock.disconnect(handler);
        }
        if let Some(fill) = self.fill.get_mut().take() {
            fill.remove();
        }
    }
}

impl TranscriptView {
    pub fn new(sink: RowSink) -> Rc<Self> {
        let list = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
        list.add_css_class("transcript");
        list.set_valign(gtk4::Align::End);
        // A readable measure: centred column, capped width.
        let clamp = adw::Clamp::new();
        // This list owns 24px padding on each edge; the bottom puts the same padding outside
        // its 860px clamp. Include it here so cards and composer share an outer grid.
        clamp.set_maximum_size(908);
        clamp.set_tightening_threshold(688);
        clamp.set_child(Some(&list));

        let scroller = gtk4::ScrolledWindow::new();
        scroller.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        scroller.set_vexpand(true);
        // The viewport must size its content by *natural* height: the default (minimum) lets
        // nested propagate-natural-height scrollers (tool output) shrink, so the end of the
        // transcript fell outside the scrollable range.
        let viewport = gtk4::Viewport::new(None::<&gtk4::Adjustment>, None::<&gtk4::Adjustment>);
        viewport.set_vscroll_policy(gtk4::ScrollablePolicy::Natural);
        // Never chase keyboard focus. A clicked card button keeps focus; when its card resolves,
        // GTK moves focus to another widget in the card and a focus-scrolling viewport jumps up
        // to it. That reads as the user scrolling up, so stick-to-bottom let go and every later
        // card (approvals above all) landed below the fold.
        viewport.set_scroll_to_focus(false);
        viewport.set_child(Some(&clamp));
        scroller.set_child(Some(&viewport));
        scroller.add_css_class("transcript-scroller");

        let empty = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        empty.add_css_class("transcript-empty");
        empty.set_valign(gtk4::Align::Center);
        empty.set_halign(gtk4::Align::Center);
        let title = gtk4::Label::new(Some("Start a conversation"));
        title.add_css_class("empty-title");
        title.set_selectable(true);
        let hint = gtk4::Label::new(Some(
            "Type a message below. / for commands, @ for files, $ for skills.",
        ));
        hint.add_css_class("empty-hint");
        hint.set_selectable(true);
        let empty_dots = crate::brand::dots();
        empty_dots.set_halign(gtk4::Align::Center);
        empty.append(&empty_dots);
        empty.append(&title);
        empty.append(&hint);
        empty.set_can_target(false);

        let jump = gtk4::Button::from_icon_name("at-go-bottom-symbolic");
        jump.add_css_class("circular");
        jump.add_css_class("osd");
        jump.add_css_class("jump-button");
        jump.set_halign(gtk4::Align::End);
        jump.set_valign(gtk4::Align::End);
        jump.set_margin_end(24);
        jump.set_margin_bottom(16);
        jump.set_tooltip_text(Some("Jump to the latest message"));
        jump.update_property(&[gtk4::accessible::Property::Label(
            "Jump to the latest message",
        )]);
        jump.set_visible(false);

        let root = gtk4::Overlay::new();
        root.set_child(Some(&scroller));
        root.add_overlay(&empty);
        root.add_overlay(&jump);

        let view = Rc::new_cyclic(|this| Self {
            root,
            scroller,
            list,
            jump,
            rows: RefCell::new(HashMap::new()),
            first: Cell::new(0),
            tail: Cell::new(0),
            newer_queued: Cell::new(false),
            earlier: RefCell::new(HashMap::new()),
            uncapped: RefCell::new(HashSet::new()),
            pinned: RefCell::new(None),
            this: this.clone(),
            stick: Cell::new(true),
            own_move: Cell::new(false),
            last_value: Cell::new(0.0),
            last_upper: Cell::new(0.0),
            last_page: Cell::new(0.0),
            anchor: Cell::new(None),
            restore_to: Cell::new(None),
            settle_queued: Cell::new(false),
            layout_handler: RefCell::new(None),
            tick: RefCell::new(None),
            #[cfg(test)]
            ticks: Cell::new(0),
            #[cfg(test)]
            settles: Cell::new(0),
            #[cfg(test)]
            moves: Cell::new(0),
            sink,
            empty,
            empty_dots,
            model: RefCell::new(Weak::new()),
            fill: RefCell::new(None),
            jump_to: RefCell::new(None),
            reveal_after_jump: Cell::new(false),
        });
        view.connect_scrolling();
        view
    }

    /// Lights `driver`'s dot over the empty transcript. A switch of agent before the first
    /// prompt fades the light across; the dots never loop (nothing is at work yet).
    pub fn set_agent(&self, driver: agent_core::adapter::Driver) {
        crate::brand::set_lead(&self.empty_dots, Some(driver));
    }

    /// Which dot over the empty transcript is lit (tests).
    #[cfg(test)]
    pub fn empty_lead(&self) -> Option<String> {
        let mut dot = self.empty_dots.first_child();
        while let Some(d) = dot {
            if d.has_css_class("lead") {
                return d
                    .css_classes()
                    .iter()
                    .find_map(|c| c.strip_prefix("dot-").map(str::to_owned));
            }
            dot = d.next_sibling();
        }
        None
    }

    /// Whether the viewport scrolls to keyboard focus (tests: it must not).
    #[cfg(test)]
    pub fn scrolls_to_focus(&self) -> bool {
        self.scroller
            .child()
            .and_then(|c| c.downcast::<gtk4::Viewport>().ok())
            .is_none_or(|v| v.is_scroll_to_focus())
    }

    /// The jump button and the scroll position, for realised-window tests.
    #[cfg(test)]
    pub fn scroll_parts(&self) -> (gtk4::Button, gtk4::Adjustment) {
        (self.jump.clone(), self.scroller.vadjustment())
    }

    /// A materialised row's bounds in the actual viewport, and its allocated width.
    #[cfg(test)]
    pub fn row_bounds_in_viewport(&self, id: &str) -> Option<(gtk4::graphene::Rect, i32)> {
        let viewport = self.scroller.child()?;
        self.with_row(id, |row| {
            row.widget()
                .compute_bounds(&viewport)
                .map(|bounds| (bounds, viewport.width()))
        })
        .flatten()
    }

    /// How far (px) the last row's bottom edge is below the visible area (negative: inside it),
    /// and whether the view is following the bottom. For realised-window tests: this is what the
    /// user sees, where the adjustment alone can be at its end while the content is taller.
    #[cfg(test)]
    pub fn last_row_overflow(&self) -> Option<(f64, bool)> {
        let last = self.list.last_child()?;
        let bounds = last.compute_bounds(&self.scroller)?;
        let bottom = f64::from(bounds.y() + bounds.height());
        Some((bottom - f64::from(self.scroller.height()), self.stick.get()))
    }

    /// Frames the window's frame clock has drawn. A headless display runs it at about one frame
    /// a second, so tests measure in frames, not milliseconds.
    #[cfg(test)]
    pub fn frames(&self) -> u64 {
        self.scroller
            .frame_clock()
            .map_or(0, |c| u64::try_from(c.frame_counter()).unwrap_or(0))
    }

    /// Times the frame tick has run, for tests and measurements.
    #[cfg(test)]
    pub fn tick_count(&self) -> u64 {
        self.ticks.get()
    }

    /// Times `settle` and `move_to` have run, for measurements.
    #[cfg(test)]
    pub fn settle_counts(&self) -> (u64, u64) {
        (self.settles.get(), self.moves.get())
    }

    /// The numbers behind [`Self::last_row_overflow`], for a test to print.
    #[cfg(test)]
    pub fn scroll_debug(&self) -> String {
        let adj = self.scroller.vadjustment();
        let content = self
            .scroller
            .child()
            .and_then(|v| v.first_child())
            .map(|c| c.height())
            .unwrap_or(-1);
        let rect = |w: &gtk4::Widget| {
            w.compute_bounds(&self.scroller)
                .map_or("-".to_owned(), |b| {
                    format!("y {:.0} h {:.0}", b.y(), b.height())
                })
        };
        let mut rows = Vec::new();
        let mut child = self.list.last_child();
        while let Some(c) = child.filter(|_| rows.len() < 3) {
            let (_, natural, _, _) = c.measure(gtk4::Orientation::Vertical, self.list.width());
            rows.push(format!(
                "{} vis {} [{}] alloc h {} nat {}",
                c.css_classes().join("."),
                c.is_visible(),
                rect(&c),
                c.height(),
                natural
            ));
            child = c.prev_sibling();
        }
        let pair = |a: &gtk4::Widget, b: &gtk4::Widget| {
            a.compute_bounds(b)
                .map_or("-".to_owned(), |r| format!("{:.0}", r.y()))
        };
        let drawn = match self.scroller.child() {
            Some(viewport) => viewport
                .first_child()
                .map_or("-".to_owned(), |clamp| pair(&clamp, &viewport)),
            None => "-".to_owned(),
        };
        let offsets = format!("ticks {} | content drawn at y {drawn}", self.ticks.get());
        format!(
            "{offsets} | value {:.0} page {:.0} upper {:.0} end {:.0} | scroller {} list {} [{}] clamp {} | {}",
            adj.value(),
            adj.page_size(),
            adj.upper(),
            adj.upper() - adj.page_size(),
            self.scroller.height(),
            self.list.height(),
            rect(self.list.upcast_ref()),
            content,
            rows.join(" ; ")
        )
    }

    pub fn widget(&self) -> &gtk4::Overlay {
        &self.root
    }

    /// Moves the view itself (pinning, anchoring). Marked as its own, so it is never read as the
    /// user scrolling.
    fn move_to(&self, value: f64) {
        #[cfg(test)]
        self.moves.set(self.moves.get() + 1);
        let adj = self.scroller.vadjustment();
        self.own_move.set(true);
        adj.set_value(value);
        self.own_move.set(false);
        self.last_value.set(adj.value());
        self.last_upper.set(adj.upper());
        self.last_page.set(adj.page_size());
    }

    /// Moves the value where it belongs: the bottom while following, or the place to restore
    /// after older rows were prepended. The only place (with `scroll_to_end` and the jump) that
    /// moves the view, after a viewport allocation rather than inside its size notifications.
    ///
    /// Not from the size notifications: GTK emits them while the viewport lays out its child,
    /// and a value set then is not applied to the child until something else lays it out
    /// again. The content stayed drawn at the previous value, so a sent message or a turn's
    /// summary sat below the fold until the user scrolled up and back down (measured on
    /// Broadway: value 3133 with the list drawn at -2911, the previous value, for seconds).
    fn settle(&self) {
        #[cfg(test)]
        self.settles.set(self.settles.get() + 1);
        let adj = self.scroller.vadjustment();
        let pending = self.jump_to.borrow().clone();
        if let Some(id) = pending {
            // Gone meanwhile, or hidden (never allocated): nothing to jump to.
            let row = self
                .rows
                .borrow()
                .get(&id)
                .map(Row::widget)
                .filter(WidgetExt::is_mapped);
            match row {
                // Built since the last layout: no allocation yet (the frame tick runs before
                // layout), and its bounds would be a stale zero-sized rectangle. A later
                // settle, after the layout, places it.
                Some(w) if w.height() == 0 => return,
                Some(w) => {
                    self.jump_to.take();
                    let reveal = self.reveal_after_jump.replace(false);
                    if let Some(bounds) = w.compute_bounds(&self.scroller) {
                        self.move_to(self.jump_target(&adj, bounds.y(), reveal));
                        return;
                    }
                }
                None => {
                    self.jump_to.take();
                    self.reveal_after_jump.set(false);
                }
            }
        }
        let end = adj.upper() - adj.page_size();
        if self.stick.get() {
            self.restore_to.set(None);
            if adj.value() < end - 0.5 {
                self.move_to(end);
            }
        } else if let Some(target) = self.restore_to.take() {
            self.move_to(target);
        }
    }

    /// [`Self::settle`] right after the current layout (one idle, however many sizes changed).
    /// A frame tick alone is not enough: the frame clock can run layouts without ticking for a
    /// while (seen on a headless display; a throttled window is the same).
    fn queue_settle(self: &Rc<Self>) {
        if self.settle_queued.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::idle_add_local_full(glib::Priority::HIGH_IDLE, move || {
            if let Some(view) = weak.upgrade() {
                view.settle_queued.set(false);
                view.settle();
            }
            glib::ControlFlow::Break
        });
    }

    /// Whether a value move the view did not make itself is the user scrolling up. Any device
    /// (wheel, touchpad, keys, touch, scrollbar) counts, by any amount. A move with the content
    /// height or the visible height changed since the last look is a relayout (GTK clamping a
    /// shrunk transcript, the composer growing or shrinking around a send), never the user.
    pub fn user_moved_up(now: Look, last: Look) -> bool {
        now.value < last.value - 0.5 && Self::user_moved(now, last)
    }

    /// Whether a value move the view did not make itself is the user scrolling, either way: the
    /// value moved while neither height did (see [`Self::user_moved_up`]).
    pub fn user_moved(now: Look, last: Look) -> bool {
        (now.value - last.value).abs() > 0.5
            && (now.upper - last.upper).abs() < 0.5
            && (now.page - last.page).abs() < 0.5
    }

    fn connect_scrolling(self: &Rc<Self>) {
        // Only deliberate keyboard navigation reveals focus. Automatic focus changes when a
        // resolved card removes buttons must retain the existing stick-to-bottom behaviour.
        let keys = gtk4::EventControllerKey::new();
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, _| {
            if matches!(key, gtk4::gdk::Key::Tab | gtk4::gdk::Key::ISO_Left_Tab) {
                let weak = weak.clone();
                glib::idle_add_local_once(move || {
                    if let Some(view) = weak.upgrade() {
                        if let Some(focus) = view.scroller.root().and_then(|r| r.focus()) {
                            if focus.is_ancestor(&view.list) {
                                view.reveal_widget(&focus);
                            }
                        }
                    }
                });
            }
            glib::Propagation::Proceed
        });
        self.scroller.add_controller(keys);
        // GtkViewport freezes adjustment notifications until after allocating its child.
        // The tick runs before layout, so both tick and idle can be one allocation behind
        // during continuous growth. Settle after GTK's layout handler; adjustment changes
        // request another layout pass in this frame, before painting the newest row.
        let weak = Rc::downgrade(self);
        self.scroller.connect_map(move |_| {
            if let Some(view) = weak.upgrade() {
                view.on_mapped();
            }
        });
        // Wired from `new`, before the scroller has a parent; a scroller already on show would
        // never see `map`, and so never get its tick or layout handler.
        if self.scroller.is_mapped() {
            self.on_mapped();
        }
        let weak = Rc::downgrade(self);
        self.scroller.connect_unmap(move |_| {
            if let Some(view) = weak.upgrade() {
                if let Some((clock, handler)) = view.layout_handler.borrow_mut().take() {
                    clock.disconnect(handler);
                }
                if let Some(tick) = view.tick.borrow_mut().take() {
                    tick.remove();
                }
            }
        });
        let adj = self.scroller.vadjustment();
        // The view leaves the bottom only when the user scrolls up, and comes back when they
        // scroll down into it, jump, or send. Every move it makes itself goes through
        // `move_to`, so anything else that moves the value is the user (or a relayout).
        let weak = Rc::downgrade(self);
        adj.connect_value_changed(move |adj| {
            let Some(view) = weak.upgrade() else {
                return;
            };
            if view.own_move.get() {
                return;
            }
            let now = Look {
                value: adj.value(),
                upper: adj.upper(),
                page: adj.page_size(),
            };
            let last = Look {
                value: view.last_value.replace(now.value),
                upper: view.last_upper.replace(now.upper),
                page: view.last_page.replace(now.page),
            };
            // The user scrolling, either way, takes over from a jump still waiting for layout.
            if Self::user_moved(now, last) {
                view.jump_to.take();
                view.reveal_after_jump.set(false);
            }
            if Self::user_moved_up(now, last) {
                view.stick.set(false);
            } else if now.value > last.value && now.value + now.page >= now.upper - STICK_SLOP {
                if view.tail.get() > 0 {
                    // The end of what is built, not of the thread: build on, follow later.
                    view.queue_load_newer();
                } else {
                    view.stick.set(true);
                    view.jump.set_visible(false);
                }
            }
        });
        let weak = Rc::downgrade(self);
        let on_size = move |adj: &gtk4::Adjustment| {
            let Some(view) = weak.upgrade() else {
                return;
            };
            // Only note what changed; `settle` moves, once this layout is over.
            view.queue_settle();
            if let Some(from_end) = view.anchor.take() {
                // Following the bottom wins over a pending "keep my place" from loading older
                // rows: restoring it after a jump threw the view back to the top.
                if !view.stick.get() {
                    view.restore_to.set(Some((adj.upper() - from_end).max(0.0)));
                }
            }
            // The next value move is judged against these heights.
            view.last_upper.set(adj.upper());
            view.last_page.set(adj.page_size());
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

    /// The scroller came on show: the backstop tick and the post-layout settle, each once.
    fn on_mapped(self: &Rc<Self>) {
        if self.tick.borrow().is_none() {
            *self.tick.borrow_mut() = Some(self.add_tick());
        }
        if self.layout_handler.borrow().is_some() {
            return;
        }
        let Some(clock) = self.scroller.frame_clock() else {
            return;
        };
        let weak = Rc::downgrade(self);
        let handler = clock.connect_local("layout", true, move |_| {
            if let Some(view) = weak.upgrade() {
                view.settle();
            }
            None
        });
        *self.layout_handler.borrow_mut() = Some((clock, handler));
    }

    /// Every frame while mapped, as a backstop for content that settles without a size
    /// notification. Added on map and removed on unmap (see [`Self::tick`]).
    fn add_tick(self: &Rc<Self>) -> gtk4::TickCallbackId {
        let weak = Rc::downgrade(self);
        self.scroller.add_tick_callback(move |_, _| {
            let Some(view) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            #[cfg(test)]
            view.ticks.set(view.ticks.get() + 1);
            view.settle();
            glib::ControlFlow::Continue
        })
    }

    /// Connects the "scrolled to the top" edge to loading older rows. Separate from `new` because
    /// it needs the model, which the view owns.
    pub fn connect_load_older(self: &Rc<Self>, model: Rc<RefCell<Transcript>>) {
        *self.model.borrow_mut() = Rc::downgrade(&model);
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
            glib::idle_add_local_once(move || {
                // Only for a user reading upward: a view following the bottom (a jump or a send
                // since) never loads older rows, which would re-anchor it away from the end.
                if !view.stick.get() {
                    view.load_older(&model.borrow());
                }
            });
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
        self.jump_to.take();
        self.reveal_after_jump.set(false);
        self.stick.set(true);
        self.anchor.set(None);
        self.restore_to.set(None);
        self.jump.set_visible(false);
        if self.tail.get() > 0 {
            self.build_tail();
        }
        let adj = self.scroller.vadjustment();
        self.move_to(adj.upper() - adj.page_size());
    }

    /// Following again with rows not built at the end: builds them (then trims to the window),
    /// or, when there are more than a window of them, starts over from the newest window as a
    /// thread open does. Never the latter while focus is in the transcript, which it would take
    /// away.
    fn build_tail(&self) {
        let Some(model) = self.model.borrow().upgrade() else {
            return;
        };
        // Being reduced into right now: the next row added while following builds it.
        if let Ok(model) = model.try_borrow() {
            self.build_tail_of(&model);
        };
    }

    fn build_tail_of(&self, model: &Transcript) {
        if self.tail.get() > WINDOW && !self.focus_in_list() {
            if let Some(this) = self.this.upgrade() {
                this.reset(model);
                return;
            }
        }
        let order = model.order();
        for id in &order[order.len() - self.tail.get()..] {
            if let Some(w) = self.build_tree(model, id) {
                self.list.append(&w);
            }
        }
        self.tail.set(0);
        self.trim(model);
    }

    /// Whether keyboard focus is on a row of the transcript.
    fn focus_in_list(&self) -> bool {
        self.list
            .root()
            .and_then(|r| r.focus())
            .is_some_and(|f| f.is_ancestor(&self.list))
    }

    /// Builds the next [`PAGE`] rows of the tail on idle: the user scrolled down to the end of
    /// what is built. Appended below the view, they move nothing on screen.
    fn queue_load_newer(&self) {
        if self.newer_queued.replace(true) {
            return;
        }
        let weak = self.this.clone();
        glib::idle_add_local_once(move || {
            let Some(view) = weak.upgrade() else {
                return;
            };
            view.newer_queued.set(false);
            let Some(model) = view.model.borrow().upgrade() else {
                return;
            };
            if let Ok(model) = model.try_borrow() {
                view.load_newer(&model);
            };
        });
    }

    /// Materialises up to [`PAGE`] rows of the tail, below the built ones.
    fn load_newer(&self, model: &Transcript) {
        let tail = self.tail.get();
        if tail == 0 {
            return;
        }
        let order = model.order();
        let start = order.len() - tail;
        let end = (start + PAGE).min(order.len());
        for id in &order[start..end] {
            if let Some(w) = self.build_tree(model, id) {
                self.list.append(&w);
            }
        }
        self.tail.set(order.len() - end);
        tracing::debug!(
            tail = order.len() - end,
            "chat transcript: loaded newer rows"
        );
    }

    /// Scrolls the transcript to the row for `id`, or to the end if not found.
    pub fn scroll_to_card(&self, id: &str) {
        self.jump_to.take();
        self.reveal_after_jump.set(false);
        // It may be in the part of the window still filling in, outside the window, or a
        // card's earlier step. Built now, it has no position until the next layout: `settle`
        // scrolls to it then, and nothing else (the fill's "keep my place" above all) moves the
        // view meanwhile.
        if !self.rows.borrow().contains_key(id) {
            if !(self.finish_fill() && self.rows.borrow().contains_key(id)) {
                self.build_for(id);
            }
            if self.rows.borrow().contains_key(id) {
                self.stick.set(false);
                self.anchor.set(None);
                self.restore_to.set(None);
                *self.jump_to.borrow_mut() = Some(id.to_owned());
                return;
            }
        }
        if let Some(row) = self.rows.borrow().get(id) {
            if let Some(bounds) = row.widget().compute_bounds(&self.scroller) {
                let adj = self.scroller.vadjustment();
                self.stick.set(false);
                self.move_to(Self::card_target(&adj, bounds.y()));
                return;
            }
        }
        self.scroll_to_end();
    }

    /// Where the view goes to show a card whose top is `y` px below the visible area's top: the
    /// card 40 px from the top, within the scrollable range.
    fn card_target(adj: &gtk4::Adjustment, y: f32) -> f64 {
        Self::clamp_value(adj, adj.value() + f64::from(y) - 40.0)
    }

    fn clamp_value(adj: &gtk4::Adjustment, value: f64) -> f64 {
        value.clamp(
            adj.lower(),
            (adj.upper() - adj.page_size()).max(adj.lower()),
        )
    }

    /// [`Self::card_target`] for a pending jump; with `reveal_focus`, moved further down when
    /// the card's focused control (a tall card's first answer) would otherwise be below the fold.
    fn jump_target(&self, adj: &gtk4::Adjustment, y: f32, reveal_focus: bool) -> f64 {
        let target = Self::card_target(adj, y);
        let focus = reveal_focus
            .then(|| self.scroller.root().and_then(|r| r.focus()))
            .flatten()
            .filter(|f| f.is_ancestor(&self.list))
            .and_then(|f| f.compute_bounds(&self.scroller));
        match focus {
            Some(b) => {
                let bottom = adj.value() + f64::from(b.y() + b.height());
                if bottom + 16.0 > target + adj.page_size() {
                    Self::clamp_value(adj, bottom + 16.0 - adj.page_size())
                } else {
                    target
                }
            }
            None => target,
        }
    }

    /// An explicit question/approval jump reveals and focuses its first pending choice.
    /// Returns false when the card has expired, disappeared, or has no actionable answer.
    pub fn focus_pending_card(&self, id: &str) -> bool {
        self.scroll_to_card(id);
        let focused = self
            .rows
            .borrow()
            .get(id)
            .is_some_and(Row::focus_pending_action);
        // A card built just now is revealed by the pending jump once laid out.
        if focused {
            if self.jump_to.borrow().is_some() {
                self.reveal_after_jump.set(true);
            } else if let Some(focus) = self.scroller.root().and_then(|r| r.focus()) {
                self.reveal_widget(&focus);
            }
        }
        focused
    }

    /// Opens the changes region in a deterministic in-memory demo fixture.
    pub(crate) fn expand_demo_changes(&self, id: &str) {
        if let Some(Row::Tool(card)) = self.rows.borrow().get(id) {
            card.set_changes_expanded(true);
        }
    }

    fn reveal_widget(&self, widget: &gtk4::Widget) {
        let Some(bounds) = widget.compute_bounds(&self.scroller) else {
            return;
        };
        let adj = self.scroller.vadjustment();
        let top = f64::from(bounds.y());
        let bottom = top + f64::from(bounds.height());
        let target = if top < 0.0 {
            Some(adj.value() + top)
        } else if bottom > adj.page_size() {
            Some(adj.value() + bottom - adj.page_size())
        } else {
            None
        };
        if let Some(target) = target {
            self.stick.set(false);
            self.move_to(target.clamp(
                adj.lower(),
                (adj.upper() - adj.page_size()).max(adj.lower()),
            ));
        }
    }

    /// Rebuilds everything from the model (initial load, or a thread swap): the newest
    /// [`FIRST_PAINT`] rows now, the rest of the window in idle steps.
    pub fn reset(self: &Rc<Self>, model: &Transcript) {
        if let Some(fill) = self.fill.take() {
            fill.remove();
        }
        self.jump_to.take();
        self.reveal_after_jump.set(false);
        self.clear_rows();
        let order = model.order();
        let first = order.len().saturating_sub(FIRST_PAINT);
        self.first.set(first);
        for id in &order[first..] {
            if let Some(w) = self.build_tree(model, id) {
                self.list.append(&w);
            }
        }
        self.empty.set_visible(order.is_empty());
        self.stick.set(true);
        if first > order.len().saturating_sub(WINDOW) {
            self.queue_fill();
        }
    }

    /// Fills the window up to [`WINDOW`] rows, one [`FILL_BUDGET`] step per idle. Below layout
    /// and paint in priority, so a frame is never held for longer than one step.
    fn queue_fill(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let started = Instant::now();
        let source = glib::idle_add_local_full(glib::Priority::DEFAULT_IDLE, move || {
            let Some(view) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let Some(model) = view.model.borrow().upgrade() else {
                view.fill.take();
                return glib::ControlFlow::Break;
            };
            // Being reduced into right now: try again on the next idle.
            let Ok(model) = model.try_borrow() else {
                return glib::ControlFlow::Continue;
            };
            if !view.fill_step(&model, FILL_BUDGET) {
                return glib::ControlFlow::Continue;
            }
            view.fill.take();
            tracing::debug!(
                ms = started.elapsed().as_millis() as u64,
                "chat transcript: filled the window"
            );
            glib::ControlFlow::Break
        });
        *self.fill.borrow_mut() = Some(source);
    }

    /// Prepends older rows until the window is full or `budget` is spent, keeping the viewport
    /// where it was. Whether the window is full: [`WINDOW`] top-level rows, or [`ROW_BUDGET`]
    /// rows in all.
    fn fill_step(&self, model: &Transcript, budget: Duration) -> bool {
        let order = model.order();
        // Recomputed each step: rows streamed in since the reset move the window's start.
        let target = (order.len() - self.tail.get()).saturating_sub(WINDOW);
        let full =
            |view: &Self| view.first.get() <= target || view.rows.borrow().len() >= ROW_BUDGET;
        if full(self) {
            return true;
        }
        let started = Instant::now();
        self.hold_place();
        let mut pin = self.pin_index(model);
        while !full(self) && started.elapsed() < budget {
            let first = self.first.get() - 1;
            self.prepend_at(model, first, &mut pin);
            self.first.set(first);
        }
        full(self)
    }

    /// Builds the rest of the window now, for a caller that needs one of its rows. Whether a
    /// fill was pending.
    fn finish_fill(&self) -> bool {
        let Some(fill) = self.fill.take() else {
            return false;
        };
        fill.remove();
        if let Some(model) = self.model.borrow().upgrade() {
            if let Ok(model) = model.try_borrow() {
                self.fill_step(&model, Duration::MAX);
            }
        }
        true
    }

    /// Rows are about to be prepended: keep the viewport where it is (unless following the
    /// bottom, which wins; see the size handler).
    fn hold_place(&self) {
        let adj = self.scroller.vadjustment();
        self.anchor.set(Some(adj.upper() - adj.value()));
    }

    /// Builds a row and its nested children, registering every one. A card builds only its
    /// newest [`CHILD_CAP`] children, under a button that builds the rest.
    fn build_tree(&self, model: &Transcript, id: &str) -> Option<gtk4::Widget> {
        let item = model.get(id)?;
        let row = Row::build(item, &self.sink);
        let widget = row.widget();
        if let Some(children) = row.children_box() {
            let from = if self.uncapped.borrow().contains(id) {
                0
            } else {
                item.children.len().saturating_sub(CHILD_CAP)
            };
            if from > 0 {
                let button = self.earlier_button(id, from);
                children.append(&button);
                self.earlier
                    .borrow_mut()
                    .insert(id.to_owned(), (from, button));
            }
            for child in &item.children[from..] {
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
        self.earlier.borrow_mut().remove(id);
        self.uncapped.borrow_mut().remove(id);
        if let Some(item) = model.get(id) {
            for child in &item.children {
                self.unregister_tree(model, child);
            }
        }
    }

    /// Takes every row down (a reset, or a jump that starts the window over).
    fn clear_rows(&self) {
        for (_, row) in self.rows.borrow_mut().drain() {
            if row.widget().parent().as_ref() == Some(self.list.upcast_ref()) {
                self.list.remove(&row.widget());
            }
        }
        self.earlier.borrow_mut().clear();
        self.uncapped.borrow_mut().clear();
        self.pinned.take();
        self.tail.set(0);
    }

    /// The pinned row's index in the order and its widget.
    fn pin_index(&self, model: &Transcript) -> Option<(usize, gtk4::Widget)> {
        let id = self.pinned.borrow().clone()?;
        let widget = self.rows.borrow().get(&id).map(Row::widget)?;
        let at = model.order().iter().position(|o| *o == id)?;
        Some((at, widget))
    }

    /// Builds the top-level row at `index`, just above the window: first in the list, or right
    /// under the pinned row when that is older. Reaching the pinned row's own index takes it
    /// back into the window, built as it is.
    fn prepend_at(
        &self,
        model: &Transcript,
        index: usize,
        pin: &mut Option<(usize, gtk4::Widget)>,
    ) {
        match pin {
            Some((at, _)) if *at == index => {
                self.pinned.take();
                *pin = None;
            }
            Some((at, under)) if *at < index => {
                if let Some(w) = self.build_tree(model, &model.order()[index]) {
                    self.list.insert_child_after(&w, Some(&*under));
                }
            }
            _ => {
                if let Some(w) = self.build_tree(model, &model.order()[index]) {
                    self.list.prepend(&w);
                }
            }
        }
    }

    /// The pinned row goes once focus has left it.
    fn release_pin(&self, model: &Transcript) {
        let Some(id) = self.pinned.borrow().clone() else {
            return;
        };
        if self.holds_focus(&id) {
            return;
        }
        self.pinned.take();
        let widget = self.rows.borrow().get(&id).map(Row::widget);
        if let Some(w) = widget {
            self.list.remove(&w);
        }
        self.unregister_tree(model, &id);
    }

    /// The button above a card's built children that builds the `count` earlier ones.
    fn earlier_button(&self, id: &str, count: usize) -> gtk4::Button {
        let button = gtk4::Button::new();
        button.add_css_class("flat");
        button.add_css_class("earlier-steps");
        button.set_halign(gtk4::Align::Start);
        Self::set_earlier_count(&button, count);
        let weak = self.this.clone();
        let id = id.to_owned();
        button.connect_clicked(move |_| {
            if let Some(view) = weak.upgrade() {
                view.show_earlier(&id);
            }
        });
        button
    }

    fn set_earlier_count(button: &gtk4::Button, count: usize) {
        let steps = if count == 1 { "step" } else { "steps" };
        button.set_label(&format!("{count} earlier {steps}"));
        button.set_tooltip_text(Some("Show the steps before these"));
        button.update_property(&[gtk4::accessible::Property::Label(&format!(
            "Show {count} earlier {steps}"
        ))]);
    }

    /// The user asked for a card's earlier children: builds them all, and keeps focus on the
    /// first of them (the button that had it goes).
    fn show_earlier(&self, id: &str) {
        let Some(model) = self.model.borrow().upgrade() else {
            return;
        };
        let Ok(model) = model.try_borrow() else {
            return;
        };
        self.uncapped.borrow_mut().insert(id.to_owned());
        self.build_earlier(&model, id, 0);
    }

    /// Builds the children of card `id` from index `to` up to its first built one, in place
    /// under its button, which then counts what is left (and goes when nothing is). The first
    /// row built.
    fn build_earlier(&self, model: &Transcript, id: &str, to: usize) -> Option<gtk4::Widget> {
        let (from, button) = self
            .earlier
            .borrow()
            .get(id)
            .map(|(from, button)| (*from, button.clone()))?;
        let item = model.get(id)?;
        let children = self
            .rows
            .borrow()
            .get(id)
            .and_then(|r| r.children_box().cloned())?;
        let from = from.min(item.children.len());
        let mut after: gtk4::Widget = button.clone().upcast();
        let mut first = None;
        for child in &item.children[to.min(from)..from] {
            if let Some(w) = self.build_tree(model, child) {
                children.insert_child_after(&w, Some(&after));
                after = w.clone();
                first.get_or_insert(w);
            }
        }
        if to == 0 {
            if button.is_focus() {
                if let Some(w) = &first {
                    w.child_focus(gtk4::DirectionType::TabForward);
                }
            }
            self.earlier.borrow_mut().remove(id);
            children.remove(&button);
        } else if let Some(entry) = self.earlier.borrow_mut().get_mut(id) {
            entry.0 = to;
            Self::set_earlier_count(&entry.1, to);
        }
        first
    }

    /// Keeps a capped card at its newest [`CHILD_CAP`] built children after one was added:
    /// drops the oldest, unless dropping it would move what the user sees or it holds focus
    /// or a pending request.
    fn cap_children(&self, model: &Transcript, id: &str) {
        if self.uncapped.borrow().contains(id) {
            return;
        }
        let (Some(item), Some(children)) = (
            model.get(id),
            self.rows
                .borrow()
                .get(id)
                .and_then(|r| r.children_box().cloned()),
        ) else {
            return;
        };
        let mut from = self.earlier.borrow().get(id).map_or(0, |(from, _)| *from);
        while item.children.len().saturating_sub(from) > CHILD_CAP {
            let oldest = &item.children[from];
            let widget = self.rows.borrow().get(oldest).map(Row::widget);
            if let Some(w) = &widget {
                if self.moves_view(w) || self.protected(model, oldest) {
                    break;
                }
                children.remove(w);
            }
            self.unregister_tree(model, oldest);
            from += 1;
        }
        let mut earlier = self.earlier.borrow_mut();
        match earlier.get_mut(id) {
            Some(entry) => {
                entry.0 = from;
                Self::set_earlier_count(&entry.1, from);
            }
            None if from > 0 => {
                let button = self.earlier_button(id, from);
                children.prepend(&button);
                earlier.insert(id.to_owned(), (from, button));
            }
            None => {}
        }
    }

    /// Whether taking `widget` out would move what the user sees: the view is not following
    /// and the row starts above the bottom of the visible area. A row not laid out (a collapsed
    /// card's) moves nothing.
    fn moves_view(&self, widget: &gtk4::Widget) -> bool {
        if self.stick.get() || !widget.is_mapped() {
            return false;
        }
        widget
            .compute_bounds(&self.scroller)
            .is_some_and(|b| f64::from(b.y()) < f64::from(self.scroller.height()))
    }

    /// Whether the row for `id` must stay built: it holds keyboard focus, or it (or a row inside
    /// it) is an approval, question or form waiting on the user.
    fn protected(&self, model: &Transcript, id: &str) -> bool {
        self.holds_focus(id) || Self::waits_on_user(model, id)
    }

    /// Whether the row for `id`, or a widget in it, has keyboard focus.
    fn holds_focus(&self, id: &str) -> bool {
        let focus = self.list.root().and_then(|r| r.focus());
        let row = self.rows.borrow().get(id).map(Row::widget);
        matches!((focus, row), (Some(focus), Some(row)) if focus == row || focus.is_ancestor(&row))
    }

    /// Whether `id`, or an item inside it, is an approval, question or form waiting on the user.
    fn waits_on_user(model: &Transcript, id: &str) -> bool {
        fn pending(model: &Transcript, id: &str) -> bool {
            let Some(item) = model.get(id) else {
                return false;
            };
            let waiting = match &item.body {
                Body::Approval(a) => a.state == ApprovalState::Pending,
                Body::Question(q) => q.state == QuestionState::Pending,
                Body::Form(f) => f.state == FormState::Pending,
                _ => false,
            };
            waiting || item.children.iter().any(|c| pending(model, c))
        }
        pending(model, id)
    }

    /// Builds what a jump to `id` needs: its top-level row (and the window up to it), then each
    /// card on the way down to it, from the child on the way to its first built one.
    fn build_for(&self, id: &str) {
        let Some(model) = self.model.borrow().upgrade() else {
            return;
        };
        let Ok(model) = model.try_borrow() else {
            return;
        };
        let mut chain = vec![id.to_owned()];
        let mut item = model.get(id);
        while let Some(parent) = item.and_then(|i| i.parent.clone()) {
            item = model.get(&parent);
            chain.push(parent);
        }
        chain.reverse();
        if !self.rows.borrow().contains_key(&chain[0]) {
            let Some(index) = model.order().iter().position(|o| *o == chain[0]) else {
                return;
            };
            self.build_window_at(&model, index);
        }
        for pair in chain.windows(2) {
            let (card, child) = (&pair[0], &pair[1]);
            if self.rows.borrow().contains_key(child) {
                continue;
            }
            let at = model
                .get(card)
                .and_then(|c| c.children.iter().position(|x| x == child));
            match at {
                Some(at) => {
                    self.build_earlier(&model, card, at);
                }
                None => return,
            }
        }
    }

    /// Makes the top-level item at `index` (not built) part of the window: the rows between it
    /// and the window are built when they are at most a window's worth, or focus is in the
    /// transcript; otherwise the window starts over at it, the rows after it built up to a
    /// window (the rest load as the user scrolls either way).
    fn build_window_at(&self, model: &Transcript, index: usize) {
        let order = model.order();
        let first = self.first.get();
        let end = order.len() - self.tail.get();
        if index >= end && (index - end < WINDOW || self.focus_in_list()) {
            for id in &order[end..=index] {
                if let Some(w) = self.build_tree(model, id) {
                    self.list.append(&w);
                }
            }
            self.tail.set(order.len() - index - 1);
            return;
        }
        if index < first && (first - index <= WINDOW || self.focus_in_list()) {
            let mut pin = self.pin_index(model);
            for at in (index..first).rev() {
                self.prepend_at(model, at, &mut pin);
            }
            self.first.set(index);
            return;
        }
        self.clear_rows();
        let mut end = index;
        while end < order.len()
            && (end - index < FIRST_PAINT
                || (end - index < WINDOW && self.rows.borrow().len() < ROW_BUDGET))
        {
            if let Some(w) = self.build_tree(model, &order[end]) {
                self.list.append(&w);
            }
            end += 1;
        }
        self.first.set(index);
        self.tail.set(order.len() - end);
        self.jump.set_visible(self.tail.get() > 0);
    }

    /// After an item that was not built left the transcript, which side of the window it was
    /// on is unknown: the window's bounds are read back from its first and last rows.
    fn resync_bounds(&self, model: &Transcript) {
        let order = model.order();
        let id_of = |w: Option<gtk4::Widget>| {
            let w = w?;
            self.rows
                .borrow()
                .iter()
                .find(|(_, row)| row.widget() == w)
                .map(|(id, _)| id.clone())
        };
        let position = |id: Option<String>| id.and_then(|id| order.iter().position(|o| *o == id));
        let pinned = self.pin_index(model).map(|(_, w)| w);
        let first = self
            .list
            .first_child()
            .filter(|w| Some(w) != pinned.as_ref())
            .or_else(|| self.list.first_child().and_then(|w| w.next_sibling()));
        match (
            position(id_of(first)),
            position(id_of(self.list.last_child())),
        ) {
            (Some(first), Some(last)) => {
                self.first.set(first);
                self.tail.set(order.len() - last - 1);
            }
            _ => {
                self.first.set(order.len());
                self.tail.set(0);
            }
        }
    }

    /// Puts `widget` (the row for `id`) into `container` where `id` sits among `siblings`: after
    /// the row of the item before it, else first. A new item is usually the last; an approval goes
    /// just before the step it asks about, so its outcome reads before the step's result.
    fn place(&self, container: &gtk4::Box, siblings: &[String], id: &str, widget: &gtk4::Widget) {
        // The usual case, checked first: a long thread's order is thousands of items long.
        if siblings.last().is_some_and(|s| s == id) {
            container.append(widget);
            return;
        }
        let pos = siblings.iter().position(|s| s == id);
        if pos.is_none_or(|p| p + 1 == siblings.len()) {
            container.append(widget);
            return;
        }
        let before = pos
            .and_then(|p| p.checked_sub(1))
            .and_then(|p| siblings.get(p))
            .and_then(|p| self.rows.borrow().get(p).map(Row::widget))
            .filter(|w| w.parent().as_ref() == Some(container.upcast_ref()));
        // First of what is built: under a card's "earlier steps" button, or the transcript's
        // pinned row (older than the window), if there is one.
        let button = container
            .first_child()
            .filter(|c| c.has_css_class("earlier-steps"));
        let pinned = || {
            let id = self.pinned.borrow().clone()?;
            self.rows
                .borrow()
                .get(&id)
                .map(Row::widget)
                .filter(|w| w.parent().as_ref() == Some(container.upcast_ref()))
        };
        match before.or(button).or_else(pinned) {
            Some(before) => container.insert_child_after(widget, Some(&before)),
            None => container.prepend(widget),
        }
    }

    /// A new item was reduced, usually at the end; it goes where the model placed it. Nested
    /// items go into their parent card when that card is materialised (otherwise they appear
    /// when it is).
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
                    let siblings = model.get(parent).map_or(&[][..], |p| &p.children[..]);
                    let pos = if siblings.last().is_some_and(|s| s == id) {
                        siblings.len() - 1
                    } else {
                        siblings
                            .iter()
                            .position(|s| s == id)
                            .unwrap_or(siblings.len())
                    };
                    let from = self.earlier.borrow().get(parent).map(|(from, _)| *from);
                    match from {
                        // Among the children not built: one more for the button to count.
                        Some(from) if pos < from => {
                            if let Some(entry) = self.earlier.borrow_mut().get_mut(parent) {
                                entry.0 = from + 1;
                                Self::set_earlier_count(&entry.1, from + 1);
                            }
                        }
                        _ => {
                            if let Some(w) = self.build_tree(model, id) {
                                self.place(&children, siblings, id, &w);
                            }
                            self.cap_children(model, parent);
                        }
                    }
                    self.updated(model, parent);
                    if self.stick.get() {
                        self.trim(model);
                    }
                }
            }
            None => {
                let order = model.order();
                let pos = if order.last().is_some_and(|o| o == id) {
                    order.len() - 1
                } else {
                    order.iter().position(|o| o == id).unwrap_or(order.len())
                };
                if pos < self.first.get() {
                    // Above the materialised window: it appears when older rows load. The window
                    // still starts at the same item, now one further down.
                    self.first.set(self.first.get() + 1);
                    return;
                }
                // After the materialised window (the tail), or past the budget while the view is
                // away from the bottom: not built until the user comes back down to it.
                let tail = self.tail.get();
                let in_tail = if tail > 0 {
                    pos + 1 + tail >= order.len()
                } else {
                    pos + 1 == order.len()
                        && !self.stick.get()
                        && self.rows.borrow().len() >= ROW_BUDGET
                };
                if in_tail {
                    self.tail.set(tail + 1);
                    if self.stick.get() {
                        // Following, with a tail left from when the model was busy: build it.
                        self.build_tail_of(model);
                    } else {
                        self.jump.set_visible(true);
                    }
                    return;
                }
                if let Some(w) = self.build_tree(model, id) {
                    let before = pos
                        .checked_sub(1)
                        .and_then(|p| order.get(p))
                        .and_then(|p| model.get(p))
                        .map(|p| &p.body);
                    if let Some(class) = entrance(&item.body, before) {
                        w.add_css_class(class);
                        let row = w.downgrade();
                        glib::timeout_add_local_once(
                            Duration::from_millis(ENTRANCE_MS),
                            move || {
                                if let Some(row) = row.upgrade() {
                                    row.remove_css_class(class);
                                }
                            },
                        );
                    }
                    self.place(&self.list, order, id, &w);
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

    /// A materialised row, for GTK checks in tests.
    #[cfg(test)]
    pub fn with_row<R>(&self, id: &str, f: impl FnOnce(&Row) -> R) -> Option<R> {
        self.rows.borrow().get(id).map(f)
    }

    /// Hands a computed diff to the file-change card of item `id`, if that row is still a widget.
    pub fn show_diff(&self, id: &str, reply: &crate::chat::DiffReply) {
        if let Some(Row::Tool(card)) = self.rows.borrow().get(id) {
            card.show_diff(reply);
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

    /// An item was removed from the transcript (e.g. popping a queued user message). A built
    /// row leaves the window's bounds as they were; one that was not built was above or after
    /// the window, which moves.
    pub fn removed(&self, model: &Transcript, id: &str) {
        let row = self.rows.borrow_mut().remove(id);
        self.earlier.borrow_mut().remove(id);
        self.uncapped.borrow_mut().remove(id);
        if self.pinned.borrow().as_deref() == Some(id) {
            self.pinned.take();
        }
        match row {
            Some(row) => row.widget().unparent(),
            None => self.resync_bounds(model),
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
        self.hold_place();
        let mut pin = self.pin_index(model);
        for index in (new_first..first).rev() {
            self.prepend_at(model, index, &mut pin);
        }
        self.first.set(new_first);
        tracing::debug!(first = new_first, "chat transcript: loaded older rows");
    }

    /// Back at the bottom: drop the oldest rows beyond the window, and beyond the
    /// [`ROW_BUDGET`] counting nested ones, so memory and a resize's cost stay bounded. The
    /// newest [`FIRST_PAINT`] rows stay whatever they hold, and so does a pending request (with
    /// everything after it). A row holding focus stays too, pinned above the window.
    fn trim(&self, model: &Transcript) {
        if self.tail.get() > 0 {
            return;
        }
        self.release_pin(model);
        let order = model.order();
        let first = self.first.get();
        // Only trim once the excess is worth a relayout.
        let over_window = order.len().saturating_sub(first) > WINDOW + PAGE;
        let over_budget = self.rows.borrow().len() > ROW_BUDGET;
        if !over_window && !over_budget {
            return;
        }
        let keep = order.len().saturating_sub(FIRST_PAINT);
        let mut at = first;
        while at < keep {
            let needed = (over_window && order.len() - at > WINDOW)
                || (over_budget && self.rows.borrow().len() > ROW_BUDGET - PAGE);
            if !needed || Self::waits_on_user(model, &order[at]) {
                break;
            }
            // Keeps focus where it is: the row stays, pinned above the window.
            if self.holds_focus(&order[at]) {
                if self.pinned.borrow().is_some() {
                    break;
                }
                *self.pinned.borrow_mut() = Some(order[at].clone());
                at += 1;
                continue;
            }
            let widget = self.rows.borrow().get(&order[at]).map(Row::widget);
            if let Some(w) = widget {
                self.list.remove(&w);
            }
            self.unregister_tree(model, &order[at]);
            at += 1;
        }
        self.first.set(at);
    }

    /// Rows built as widgets, nested ones included (tests and measurements).
    #[cfg(test)]
    pub fn row_count(&self) -> usize {
        self.rows.borrow().len()
    }

    /// Top-level items after the built ones, not built (tests).
    #[cfg(test)]
    pub fn tail_len(&self) -> usize {
        self.tail.get()
    }

    /// A card's "earlier steps" button and how many children it stands for (tests).
    #[cfg(test)]
    pub fn earlier_of(&self, id: &str) -> Option<(usize, gtk4::Button)> {
        self.earlier.borrow().get(id).cloned()
    }

    /// Whether the jump-to-latest button shows (tests).
    #[cfg(test)]
    pub fn jump_shown(&self) -> bool {
        self.jump.is_visible()
    }

    /// Top-level rows built as widgets.
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

#[cfg(test)]
mod tests {
    use agent_core::adapter::Driver;

    use super::super::model::{Body, Tone};
    use super::entrance;

    /// A live message of yours and the reply's first row fade in; a switch's divider settles;
    /// the rest of a turn (tool steps, the reply's later rows) just appears.
    #[test]
    fn only_a_turns_first_rows_and_a_switch_make_an_entrance() {
        let user = Body::User {
            text: "hi".into(),
            queued: false,
        };
        let reply = Body::Assistant {
            text: String::new(),
            driver: Driver::Claude,
            streaming: true,
        };
        let notice = Body::Notice {
            text: "note".into(),
            tone: Tone::Info,
        };
        let switch = Body::Switch {
            driver: Driver::Agy,
            model: None,
            agent_changed: true,
            returned: false,
        };
        assert_eq!(entrance(&user, None), Some("row-enter"));
        assert_eq!(entrance(&user, Some(&reply)), Some("row-enter"));
        assert_eq!(entrance(&reply, Some(&user)), Some("row-enter"));
        assert_eq!(entrance(&notice, Some(&user)), Some("row-enter"));
        assert_eq!(entrance(&switch, Some(&reply)), Some("divider-settle"));
        assert_eq!(entrance(&reply, Some(&notice)), None);
        assert_eq!(entrance(&notice, Some(&reply)), None);
        assert_eq!(entrance(&reply, None), None);
        let queued = Body::User {
            text: "next".into(),
            queued: true,
        };
        assert_eq!(entrance(&queued, Some(&reply)), Some("row-enter"));
        assert_eq!(
            entrance(&notice, Some(&queued)),
            None,
            "a queued message is not answered by what follows it"
        );
    }
}
