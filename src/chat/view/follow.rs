//! Keeping a dialog's scroller on its newest content while the user watches it live: a
//! sub-agent's steps, a background task's output. It opens at the end and stays there as content
//! arrives, until the user scrolls up; scrolling back down to the end picks the following up
//! again. The thread's own transcript has its own, richer version (`transcript.rs`).

use std::cell::Cell;
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;

/// How close to the end (in pixels) still counts as at it.
const SLOP: f64 = 4.0;

struct Follow {
    /// Following the end: the user has not scrolled up from it.
    stick: Cell<bool>,
    /// The next value change is this module's own move, not the user's.
    own_move: Cell<bool>,
    /// A move to the end waits for the layout in progress to finish.
    queued: Cell<bool>,
}

fn at_end(adj: &gtk4::Adjustment) -> bool {
    adj.value() + adj.page_size() >= adj.upper() - SLOP
}

/// Makes `scroller` follow its end (see the module). Holds only weak references to it.
pub fn follow_end(scroller: &gtk4::ScrolledWindow) {
    let state = Rc::new(Follow {
        stick: Cell::new(true),
        own_move: Cell::new(false),
        queued: Cell::new(false),
    });
    let adj = scroller.vadjustment();
    let watch = Rc::clone(&state);
    adj.connect_value_changed(move |adj| {
        if watch.own_move.get() {
            return;
        }
        // The user (or a clamp as content shrinks, which leaves it at the end anyway).
        watch.stick.set(at_end(adj));
    });
    // Content grew or the dialog resized: back to the end once this layout is done, since moving
    // the value in the middle of one is undone by it.
    let on_size = move |adj: &gtk4::Adjustment| {
        if !state.stick.get() || state.queued.replace(true) {
            return;
        }
        let state = Rc::clone(&state);
        let adj = adj.downgrade();
        glib::idle_add_local_once(move || {
            state.queued.set(false);
            let Some(adj) = adj.upgrade() else { return };
            if state.stick.get() && !at_end(&adj) {
                state.own_move.set(true);
                adj.set_value(adj.upper() - adj.page_size());
                state.own_move.set(false);
            }
        });
    };
    adj.connect_upper_notify(on_size.clone());
    adj.connect_page_size_notify(on_size);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn pump() {
        let context = glib::MainContext::default();
        while context.iteration(false) {}
    }

    /// Opens at the end, follows growth, lets go when the user scrolls up and picks up again at
    /// the end. Called on the display tests' private display.
    pub(crate) fn a_followed_scroller_tracks_its_end_until_the_user_scrolls_up() {
        let scroller = gtk4::ScrolledWindow::new();
        follow_end(&scroller);
        let adj = scroller.vadjustment();
        adj.configure(0.0, 0.0, 100.0, 1.0, 10.0, 100.0);
        adj.set_upper(400.0);
        pump();
        assert_eq!(adj.value(), 300.0, "opens at the end");
        adj.set_upper(600.0);
        pump();
        assert_eq!(adj.value(), 500.0, "follows new content");
        adj.set_value(120.0);
        adj.set_upper(800.0);
        pump();
        assert_eq!(adj.value(), 120.0, "scrolled up: stays put");
        adj.set_value(700.0);
        adj.set_upper(900.0);
        pump();
        assert_eq!(adj.value(), 800.0, "back at the end: follows again");
    }
}
