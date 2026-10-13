//! In-memory tails of an item's streamed text, so the store can re-scrub a secret split across
//! deltas (`Store::heal`) without scanning the events table.
//!
//! A tail is a cache of rows the store itself wrote. It is trusted only when it is known to hold
//! every one of the item's delta rows, so that healing from it writes exactly what the SQL scan
//! would. Anything less makes it untrusted, and an untrusted or absent tail means "use the SQL
//! scan", which is always correct. The rules:
//!
//! - A tail starts at the item's `ItemStarted` in this process, and only if the store holds no
//!   row of that item yet (within the scan's window): a reused id (a second start after the
//!   first was completed or cleared, a reopened thread, another process) heals by the scan.
//!   Deltas of an item with no started tail leave an untrusted marker.
//! - A second `ItemStarted` for an item already tailed makes it untrusted.
//! - It overflows (per-item or global cap, row overhead counted): untrusted, text dropped.
//! - Any append that does not succeed (an error, a panic), a batch that does not commit, or a
//!   transaction SQLite rolled back on its own: every tail of the thread becomes untrusted
//!   (the DB may not hold what the tail does, or the reverse).
//! - A tail heal that does not match the store (a row gone or changed) is undone and the
//!   scan runs instead.
//! - `ItemCompleted` consumes it; `ContentSnapshot` keeps it (with the healed text);
//!   `TurnCompleted`, `SessionExited`, a deleted or closed thread clear the thread's tails.
//!
//! The text kept is the scrubbed text of each row as stored, never the raw input.

use std::collections::HashMap;

use agent_core::event::StreamKind;

/// Most text one item's tail holds before it overflows (untrusted, text dropped).
pub(crate) const ITEM_CAP: usize = 1 << 20;
/// Most text all tails together hold; an append past it overflows the item being appended.
pub(crate) const TOTAL_CAP: usize = 16 << 20;
/// Most items tailed at once (trusted or not); past it new items get no tail (SQL heal).
pub(crate) const MAX_ITEMS: usize = 4096;
/// What one kept row costs beyond its text: its `seq` and `String` header, and its slot.
pub(crate) const ROW_OVERHEAD: usize = std::mem::size_of::<(i64, String)>();

/// One stream's rows: `(seq, scrubbed text)` in append order.
pub(crate) type StreamRows = Vec<(i64, String)>;

#[derive(Debug, Default)]
struct ItemTail {
    trusted: bool,
    bytes: usize,
    streams: Vec<(StreamKind, StreamRows)>,
}

impl ItemTail {
    fn distrust(&mut self) -> usize {
        self.trusted = false;
        self.streams.clear();
        std::mem::take(&mut self.bytes)
    }
}

/// Every tail of one store connection.
#[derive(Debug)]
pub(crate) struct HealTails {
    items: HashMap<(String, String), ItemTail>,
    total: usize,
    item_cap: usize,
    total_cap: usize,
    max_items: usize,
    enabled: bool,
}

impl Default for HealTails {
    fn default() -> Self {
        Self {
            items: HashMap::new(),
            total: 0,
            item_cap: ITEM_CAP,
            total_cap: TOTAL_CAP,
            max_items: MAX_ITEMS,
            enabled: true,
        }
    }
}

fn key(thread: &str, item: &str) -> (String, String) {
    (thread.to_owned(), item.to_owned())
}

impl HealTails {
    /// Off: nothing is tailed and every heal takes the SQL scan.
    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.items.clear();
            self.total = 0;
        }
    }

    #[cfg(test)]
    pub(crate) fn set_caps(&mut self, item_cap: usize, total_cap: usize) {
        self.item_cap = item_cap;
        self.total_cap = total_cap;
    }

    #[cfg(test)]
    pub(crate) fn set_max_items(&mut self, max_items: usize) {
        self.max_items = max_items;
    }

    /// Whether [`HealTails::arm`] would start a new trusted tail for the item: the store then
    /// first checks the item has no rows yet ([`HealTails::refuse`] if it has).
    pub(crate) fn would_arm(&self, thread: &str, item: &str) -> bool {
        self.enabled
            && self.items.len() < self.max_items
            && !self.items.contains_key(&key(thread, item))
    }

    /// The item started, but already has rows in the store (a reused id): never trusted.
    pub(crate) fn refuse(&mut self, thread: &str, item: &str) {
        if !self.enabled {
            return;
        }
        let full = self.items.len() >= self.max_items;
        match self.items.get_mut(&key(thread, item)) {
            Some(tail) => self.total -= tail.distrust(),
            None if !full => {
                self.items.insert(key(thread, item), ItemTail::default());
            }
            None => {}
        }
    }

    /// The item started (its `ItemStarted` is stored).
    pub(crate) fn arm(&mut self, thread: &str, item: &str) {
        if !self.enabled {
            return;
        }
        let full = self.items.len() >= self.max_items;
        match self.items.get_mut(&key(thread, item)) {
            // Started twice, or deltas came first: the tail cannot know it has every row.
            Some(tail) => self.total -= tail.distrust(),
            None if !full => {
                self.items.insert(
                    key(thread, item),
                    ItemTail {
                        trusted: true,
                        ..ItemTail::default()
                    },
                );
            }
            None => {}
        }
    }

    /// A delta row of the item was stored as `seq` with this (scrubbed) text.
    pub(crate) fn record(
        &mut self,
        thread: &str,
        item: &str,
        stream: StreamKind,
        seq: i64,
        text: &str,
    ) {
        if !self.enabled {
            return;
        }
        let Some(tail) = self.items.get_mut(&key(thread, item)) else {
            // Deltas before any `ItemStarted`: remember that this item cannot be trusted.
            if self.items.len() < self.max_items {
                self.items.insert(key(thread, item), ItemTail::default());
            }
            return;
        };
        if !tail.trusted {
            return;
        }
        // A row costs its text plus what keeping it costs (its seq, the String, the Vec slot).
        let cost = text.len() + ROW_OVERHEAD;
        if tail.bytes + cost > self.item_cap || self.total + cost > self.total_cap {
            self.total -= tail.distrust();
            tracing::debug!("a heal tail overflowed; its item will heal by the SQL scan");
            return;
        }
        tail.bytes += cost;
        self.total += cost;
        let row = (seq, text.to_owned());
        match tail.streams.iter_mut().find(|(s, _)| *s == stream) {
            Some((_, rows)) => rows.push(row),
            None => tail.streams.push((stream, vec![row])),
        }
    }

    /// The item's rows by stream, when its tail is trusted.
    pub(crate) fn trusted(
        &self,
        thread: &str,
        item: &str,
    ) -> Option<Vec<(StreamKind, StreamRows)>> {
        self.items
            .get(&key(thread, item))
            .filter(|t| t.trusted)
            .map(|t| t.streams.clone())
    }

    /// The stream's rows were healed: the first holds `whole`, the rest are empty.
    pub(crate) fn healed(&mut self, thread: &str, item: &str, stream: StreamKind, whole: &str) {
        let Some(tail) = self.items.get_mut(&key(thread, item)) else {
            return;
        };
        let Some((_, rows)) = tail.streams.iter_mut().find(|(s, _)| *s == stream) else {
            return;
        };
        let before: usize = rows.iter().map(|(_, t)| t.len()).sum();
        for (i, (_, text)) in rows.iter_mut().enumerate() {
            *text = if i == 0 {
                whole.to_owned()
            } else {
                String::new()
            };
        }
        // Redaction only shortens or keeps the text in practice; account for either way.
        tail.bytes = tail.bytes - before + whole.len();
        self.total = self.total - before + whole.len();
    }

    /// The item completed: its tail is spent.
    pub(crate) fn consume(&mut self, thread: &str, item: &str) {
        if let Some(tail) = self.items.remove(&key(thread, item)) {
            self.total -= tail.bytes;
        }
    }

    /// The thread's turn or session ended, or it was closed or deleted.
    pub(crate) fn clear_thread(&mut self, thread: &str) {
        let mut freed = 0;
        self.items.retain(|(t, _), tail| {
            let keep = t != thread;
            if !keep {
                freed += tail.bytes;
            }
            keep
        });
        self.total -= freed;
    }

    /// Something on the thread failed: none of its tails can vouch for the DB any more.
    pub(crate) fn distrust_thread(&mut self, thread: &str) {
        let mut freed = 0;
        for ((t, _), tail) in self.items.iter_mut() {
            if t == thread {
                freed += tail.distrust();
            }
        }
        self.total -= freed;
    }

    #[cfg(test)]
    pub(crate) fn total_bytes(&self) -> usize {
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: StreamKind = StreamKind::Assistant;

    #[test]
    fn a_tail_is_trusted_only_from_its_start_and_until_something_goes_wrong() {
        let mut t = HealTails::default();
        // Deltas before the start: never trusted, even once it starts.
        t.record("t", "early", A, 1, "x");
        t.arm("t", "early");
        assert!(t.trusted("t", "early").is_none());

        t.arm("t", "i");
        t.record("t", "i", A, 2, "he");
        t.record("t", "i", A, 3, "llo");
        assert_eq!(
            t.trusted("t", "i"),
            Some(vec![(A, vec![(2, "he".into()), (3, "llo".into())])])
        );
        t.healed("t", "i", A, "hello");
        assert_eq!(
            t.trusted("t", "i"),
            Some(vec![(A, vec![(2, "hello".into()), (3, String::new())])])
        );
        // Started twice: untrusted.
        t.arm("t", "i");
        assert!(t.trusted("t", "i").is_none());

        t.arm("t", "j");
        t.record("t", "j", A, 4, "a");
        t.distrust_thread("t");
        assert!(t.trusted("t", "j").is_none());
        t.clear_thread("t");
        assert_eq!(t.total_bytes(), 0);
    }

    #[test]
    fn overflow_drops_the_text_and_distrusts() {
        let o = ROW_OVERHEAD;
        let mut t = HealTails::default();
        t.set_caps(o + 4, 1000);
        t.arm("t", "i");
        t.record("t", "i", A, 1, "abc");
        t.record("t", "i", A, 2, "de");
        assert!(t.trusted("t", "i").is_none());
        assert_eq!(t.total_bytes(), 0);
        // Row overhead counts: many empty rows overflow too.
        t.set_caps(3 * o, 1000);
        t.arm("t", "e");
        for seq in 0..4 {
            t.record("t", "e", A, seq, "");
        }
        assert!(t.trusted("t", "e").is_none());
        // The global cap overflows the item being appended.
        t.set_caps(1000, o + 5);
        t.arm("t", "k");
        t.record("t", "k", A, 3, "abcd");
        t.arm("t", "m");
        t.record("t", "m", A, 4, "xy");
        assert!(t.trusted("t", "k").is_some() && t.trusted("t", "m").is_none());
        t.consume("t", "k");
        assert_eq!(t.total_bytes(), 0);
    }

    #[test]
    fn disabled_tails_hold_nothing() {
        let mut t = HealTails::default();
        t.set_enabled(false);
        t.arm("t", "i");
        t.record("t", "i", A, 1, "a");
        assert!(t.trusted("t", "i").is_none());
    }
}
