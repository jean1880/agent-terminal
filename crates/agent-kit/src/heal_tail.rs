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
//!
//! Above all, the tails are transactional ([`HealTails::begin`] / `release` / `rollback`, tied
//! to every transaction and savepoint the store opens): a change made by a write that is then
//! rolled back, for whatever reason, is taken back with it. Outside a transaction, a trusted tail
//! holds exactly the item's committed delta rows; the store's tests check that after every step.

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

#[derive(Debug, Default, Clone)]
struct ItemTail {
    trusted: bool,
    bytes: usize,
    streams: Vec<(StreamKind, StreamRows)>,
}

type Key = (String, String);

/// How to take one change back (see the transaction notes on [`HealTails`]).
#[derive(Debug)]
enum Undo {
    /// Put the key back as it was (`None`: it was absent).
    Restore(Key, Option<ItemTail>),
    /// Take back the last row appended to stream `stream` of the key (and the stream itself
    /// when the row created it).
    PopRow {
        key: Key,
        stream: usize,
        created_stream: bool,
        cost: usize,
    },
}

/// Every tail of one store connection.
///
/// **Transactional.** The tails follow the store's transactions, so they only ever vouch for
/// rows the database really holds: [`HealTails::begin`] opens a level (a batch, a job's or an
/// append's savepoint), and every change made inside it is journalled. [`HealTails::release`]
/// keeps the level's changes (folded into the level around it; at the outermost level they are
/// committed), [`HealTails::rollback`] takes them all back. A level is released only when its
/// SQL savepoint or transaction was released or committed, and rolled back whenever that was
/// rolled back (an error, an `Err` job, a panic, a lost transaction, a failed COMMIT). Changes
/// made outside any level are already committed.
#[derive(Debug)]
pub(crate) struct HealTails {
    items: HashMap<Key, ItemTail>,
    total: usize,
    item_cap: usize,
    total_cap: usize,
    max_items: usize,
    enabled: bool,
    /// Changes made inside open levels, oldest first.
    journal: Vec<Undo>,
    /// Where each open level's changes start in `journal`.
    levels: Vec<usize>,
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
            journal: Vec::new(),
            levels: Vec::new(),
        }
    }
}

fn key(thread: &str, item: &str) -> Key {
    (thread.to_owned(), item.to_owned())
}

impl HealTails {
    /// Off: nothing is tailed and every heal takes the SQL scan.
    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.items.clear();
            self.total = 0;
            self.journal.clear();
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

    // ---- transactions ----

    /// Opens a level: what changes from here is taken back if it is rolled back.
    pub(crate) fn begin(&mut self) {
        self.levels.push(self.journal.len());
    }

    /// Keeps the innermost level's changes. At the outermost level they are committed.
    pub(crate) fn release(&mut self) {
        self.levels.pop();
        if self.levels.is_empty() {
            self.journal.clear();
        }
    }

    /// Takes the innermost level's changes back.
    pub(crate) fn rollback(&mut self) {
        let Some(start) = self.levels.pop() else {
            return;
        };
        while self.journal.len() > start {
            let Some(undo) = self.journal.pop() else {
                break;
            };
            match undo {
                Undo::Restore(key, old) => {
                    let gone = match old {
                        Some(tail) => {
                            self.total += tail.bytes;
                            self.items.insert(key, tail)
                        }
                        None => self.items.remove(&key),
                    };
                    if let Some(gone) = gone {
                        self.total -= gone.bytes;
                    }
                }
                Undo::PopRow {
                    key,
                    stream,
                    created_stream,
                    cost,
                } => {
                    if let Some(tail) = self.items.get_mut(&key) {
                        if let Some((_, rows)) = tail.streams.get_mut(stream) {
                            rows.pop();
                        }
                        if created_stream {
                            tail.streams.truncate(stream);
                        }
                        tail.bytes -= cost;
                        self.total -= cost;
                    }
                }
            }
        }
    }

    /// Whether a level is open (changes are journalled).
    #[cfg(test)]
    pub(crate) fn in_level(&self) -> bool {
        !self.levels.is_empty()
    }

    fn journal(&mut self, undo: Undo) {
        if !self.levels.is_empty() {
            self.journal.push(undo);
        }
    }

    /// Replaces the key's tail (`None`: removes it), journalled; returns nothing, keeps `total`.
    fn set(&mut self, key: Key, new: Option<ItemTail>) {
        let old = match new {
            Some(tail) => {
                self.total += tail.bytes;
                self.items.insert(key.clone(), tail)
            }
            None => self.items.remove(&key),
        };
        if let Some(old) = &old {
            self.total -= old.bytes;
        }
        self.journal(Undo::Restore(key, old));
    }

    // ---- changes ----

    /// Whether [`HealTails::arm`] would start a new trusted tail for the item: the store then
    /// first checks the item has no rows yet ([`HealTails::refuse`] if it has).
    pub(crate) fn would_arm(&self, thread: &str, item: &str) -> bool {
        self.enabled
            && self.items.len() < self.max_items
            && !self.items.contains_key(&key(thread, item))
    }

    /// The item started, but already has rows in the store (a reused id): never trusted.
    pub(crate) fn refuse(&mut self, thread: &str, item: &str) {
        let k = key(thread, item);
        if !self.enabled || (!self.items.contains_key(&k) && self.items.len() >= self.max_items) {
            return;
        }
        self.set(k, Some(ItemTail::default()));
    }

    /// The item started (its `ItemStarted` is stored).
    pub(crate) fn arm(&mut self, thread: &str, item: &str) {
        if !self.enabled {
            return;
        }
        let k = key(thread, item);
        if self.items.contains_key(&k) {
            // Started twice, or deltas came first: the tail cannot know it has every row.
            self.set(k, Some(ItemTail::default()));
        } else if self.items.len() < self.max_items {
            self.set(
                k,
                Some(ItemTail {
                    trusted: true,
                    ..ItemTail::default()
                }),
            );
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
        let k = key(thread, item);
        let Some(tail) = self.items.get(&k) else {
            // Deltas before any `ItemStarted`: remember that this item cannot be trusted.
            if self.items.len() < self.max_items {
                self.set(k, Some(ItemTail::default()));
            }
            return;
        };
        if !tail.trusted {
            return;
        }
        // A row costs its text plus what keeping it costs (its seq, the String, the Vec slot).
        let cost = text.len() + ROW_OVERHEAD;
        if tail.bytes + cost > self.item_cap || self.total + cost > self.total_cap {
            tracing::debug!("a heal tail overflowed; its item will heal by the SQL scan");
            self.set(k, Some(ItemTail::default()));
            return;
        }
        let Some(tail) = self.items.get_mut(&k) else {
            return;
        };
        let (index, created_stream) = match tail.streams.iter().position(|(s, _)| *s == stream) {
            Some(i) => (i, false),
            None => {
                tail.streams.push((stream, Vec::new()));
                (tail.streams.len() - 1, true)
            }
        };
        tail.streams[index].1.push((seq, text.to_owned()));
        tail.bytes += cost;
        self.total += cost;
        self.journal(Undo::PopRow {
            key: k,
            stream: index,
            created_stream,
            cost,
        });
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

    /// Every trusted tail: `((thread, item), rows by stream)`. For the invariant test.
    #[cfg(test)]
    pub(crate) fn all_trusted(&self) -> Vec<(Key, Vec<(StreamKind, StreamRows)>)> {
        self.items
            .iter()
            .filter(|(_, t)| t.trusted)
            .map(|(k, t)| (k.clone(), t.streams.clone()))
            .collect()
    }

    /// The stream's rows were healed: the first holds `whole`, the rest are empty.
    pub(crate) fn healed(&mut self, thread: &str, item: &str, stream: StreamKind, whole: &str) {
        let k = key(thread, item);
        let Some(mut tail) = self.items.get(&k).cloned() else {
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
        tail.bytes = tail.bytes - before + whole.len();
        self.set(k, Some(tail));
    }

    /// The item completed: its tail is spent.
    pub(crate) fn consume(&mut self, thread: &str, item: &str) {
        let k = key(thread, item);
        if self.items.contains_key(&k) {
            self.set(k, None);
        }
    }

    fn keys_of(&self, thread: &str) -> Vec<Key> {
        self.items
            .keys()
            .filter(|(t, _)| t == thread)
            .cloned()
            .collect()
    }

    /// The thread's turn or session ended, or it was closed or deleted.
    pub(crate) fn clear_thread(&mut self, thread: &str) {
        for k in self.keys_of(thread) {
            self.set(k, None);
        }
    }

    /// Something on the thread failed: none of its tails can vouch for the DB any more.
    pub(crate) fn distrust_thread(&mut self, thread: &str) {
        for k in self.keys_of(thread) {
            if self.items.get(&k).is_some_and(|t| t.trusted) {
                self.set(k, Some(ItemTail::default()));
            }
        }
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
    fn a_rolled_back_level_takes_every_change_back_and_a_released_one_keeps_them() {
        let mut t = HealTails::default();
        t.arm("t", "i");
        t.record("t", "i", A, 1, "he");
        let before = (t.all_trusted(), t.total_bytes());
        // A batch, with a job that rows, heals, starts and consumes; rolled back.
        t.begin();
        t.begin();
        t.record("t", "i", A, 2, "llo");
        t.record("t", "i", StreamKind::Reasoning, 3, "x");
        t.healed("t", "i", A, "hello");
        t.arm("t", "new");
        t.consume("t", "i");
        t.rollback();
        t.release();
        // Only "i" was trusted before: compare it alone (and that "new" is gone).
        assert_eq!(before.0.len(), 1);
        assert_eq!(
            (t.all_trusted(), t.total_bytes()),
            (before.0.clone(), before.1)
        );
        // A job released inside a batch the batch then rolls back: also gone.
        t.begin();
        t.begin();
        t.record("t", "i", A, 4, "llo");
        t.release();
        t.distrust_thread("t");
        t.rollback();
        assert_eq!(t.trusted("t", "i"), Some(vec![(A, vec![(1, "he".into())])]));
        // Released at the outermost level: committed.
        t.begin();
        t.record("t", "i", A, 5, "y");
        t.release();
        assert!(!t.in_level());
        assert_eq!(
            t.trusted("t", "i"),
            Some(vec![(A, vec![(1, "he".into()), (5, "y".into())])])
        );
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
