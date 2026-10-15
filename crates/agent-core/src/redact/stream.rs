//! Publishing streamed text without ever publishing a secret [`redact`] would mask.
//!
//! A provider streams an item's text in deltas, and a credential can be split across any two
//! of them: scrubbing each delta alone lets `ghp_abc` + `def…` through. [`StreamSanitiser`]
//! keeps per-stream state and publishes only text whose redaction can no longer change, under
//! the same rules as [`redact`] (it calls the same rule functions; no rule is copied here).
//!
//! # How
//!
//! [`redact`] masks PEM blocks over the whole text, then applies its per-line rules to each
//! `\n`-separated line. The sanitiser runs the same two stages incrementally:
//!
//! 1. **PEM**: a small state machine finds `-----BEGIN ` / `-----END ` / `-----` exactly as
//!    `redact` does, holding back only a trailing partial marker. Block contents are dropped as
//!    they arrive (never buffered), so a huge key cannot overflow anything.
//! 2. **Lines**: the PEM output is collected into the current line. A complete line is
//!    published as `redact` would write it. Before the newline, a prefix of the line ending in a
//!    blank (space or tab) is published once no rule's decision reaches its end (the open flag
//!    of the crate-private `redact_line`): no token, value, quote, header or option argument is
//!    still open there, and no `Basic`/`Token` is waiting on a later `authorization`. Text
//!    appended after such a prefix cannot change how the prefix is redacted, so what was
//!    published stays a prefix of the final redaction. The whole line is kept as context until
//!    its newline.
//!
//! Input is consumed in pieces of at most [`PIECE_BYTES`], so however large one chunk is, the
//! working copies are bounded by the piece size and the limits, not by the chunk.
//!
//! # The invariant
//!
//! For any input and any chunking, the concatenation of everything published is always a
//! prefix of `redact(concat(input))`, and after [`StreamSanitiser::finish`] it is equal to it.
//! Deliberate, conservative divergences, all of which withhold *more* than `redact`:
//!
//! - **Line overflow**: a line (published context plus uncertain bytes) that would not fit the
//!   stream's budget (its per-stream bound, or what the aggregate bound leaves it) is cut: its
//!   unpublished bytes and everything up to its newline are dropped and [`WITHHELD_NOTICE`] is
//!   published in their place. No uncertain byte is ever flushed.
//! - **Sealing**: if even the stream's few remaining bytes (a partial PEM marker, a partial
//!   UTF-8 character) do not fit its budget, the stream is sealed: the notice is published and
//!   everything until [`StreamSanitiser::finish`], [`StreamSanitiser::cancel`] or
//!   [`StreamSanitiser::replace_snapshot`] is withheld.
//! - **Too many streams**: a stream that needs state when [`Limits::max_streams`] streams
//!   already hold some is refused the same way (notice, then withheld until finish, cancel or a
//!   snapshot). Up to `max_streams` refused keys are remembered; past that the sanitiser is
//!   saturated ([`StreamSanitiser::saturated`]): every stream without state is withheld for the
//!   rest of its life, since a refused stream it could not remember must never resume without
//!   its context. Saturation is noticed once; after that the withholding is silent. Streams
//!   that already have state carry on.
//! - **Cancel**: [`StreamSanitiser::cancel`] drops everything not yet published.
//!
//! Holding back is also conservative in timing only: a line with `Basic`/`Token` and no
//! `authorization` yet is held to its newline, and `curl -u ` waits for its value word, though
//! the latter could not change the prefix. Neither changes what is finally published.
//!
//! Byte input ([`StreamSanitiser::push_bytes`]) is decoded like `String::from_utf8_lossy` of
//! the whole byte stream, holding an incomplete trailing character until its next bytes come.
//!
//! # Memory
//!
//! A stream's retained bytes are the *capacities* of its line, partial-marker and
//! partial-character buffers; a line buffer grows only up to its budget, and one larger than
//! [`KEEP_CAPACITY`] is freed at its newline. A stream with nothing pending is dropped, so a
//! finished line costs no entry. Entries holding state are bounded by `max_streams`, and the
//! refused-key set by `max_streams` too. Keys are never cloned or stored, only a 16-byte keyed
//! digest of each, so a key's own size costs nothing past the call. Published text is the caller's output and is not
//! bounded here.
//!
//! The detection limits are `redact`'s own: a credential with no recognisable shape, not
//! assigned to a sensitive-looking name, gets through here exactly as it does there.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hash, RandomState};
use std::marker::PhantomData;

use super::{redact_line, PEM_BEGIN, PEM_CLOSE, PEM_END, PEM_MASK};

#[cfg(doc)]
use super::redact;

/// The default bound on one stream's retained bytes (its line, published context plus
/// uncertain bytes, and a partial marker or character).
pub const DEFAULT_PER_STREAM_BYTES: usize = 64 * 1024;

/// The default bound on what all streams together retain.
pub const DEFAULT_AGGREGATE_BYTES: usize = 4 * 1024 * 1024;

/// The default bound on streams holding state at once.
pub const DEFAULT_MAX_STREAMS: usize = 1024;

/// Published in place of text the sanitiser had to drop rather than check.
pub const WITHHELD_NOTICE: &str = "[withheld: text too long to check for secrets]";

/// Input is consumed in pieces of at most this many bytes.
pub const PIECE_BYTES: usize = 4096;

/// A line buffer with more capacity than this is freed, not kept, when its line ends.
pub const KEEP_CAPACITY: usize = 4096;

/// Past this many bytes in a line, a prefix is judged again only once the line has grown by an
/// eighth since the last judgement, so a long line costs amortised linear time, not quadratic.
const JUDGE_EVERY_PUSH_UP_TO: usize = 4096;

/// What the sanitiser may hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Bytes one stream may retain: its current line (published context and uncertain bytes)
    /// plus a partial PEM marker and a partial UTF-8 character, counted by capacity.
    pub per_stream_bytes: usize,
    /// Bytes all streams together may retain.
    pub aggregate_bytes: usize,
    /// Streams that may hold state at once.
    pub max_streams: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            per_stream_bytes: DEFAULT_PER_STREAM_BYTES,
            aggregate_bytes: DEFAULT_AGGREGATE_BYTES,
            max_streams: DEFAULT_MAX_STREAMS,
        }
    }
}

/// A placeholder stream identity: worker, process generation, item and stream. Any
/// `Hash` key works (it is reduced to a fixed-size digest, never stored); this one is the
/// default until the runtime defines its own.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct StreamKey {
    pub worker: String,
    pub generation: u64,
    pub item: String,
    pub stream: String,
}

/// What one call made safe to show.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Published {
    /// Text to append to what this stream has published. With [`Published::replace`], the
    /// whole of what it has published instead.
    pub text: String,
    /// `text` replaces everything published before (a snapshot replaced the deltas).
    pub replace: bool,
    /// Bytes dropped rather than published: an overflowing line, a sealed or refused stream,
    /// or a cancelled stream's uncertain tail.
    pub withheld_bytes: usize,
    /// Text was withheld for want of room (overflow, sealing, refusal or saturation), and
    /// [`WITHHELD_NOTICE`] marks the spot when it first happened.
    pub overflowed: bool,
}

/// Marks a withholding for want of room, once per spot.
fn notice(p: &mut Published) {
    if !p.text.ends_with(WITHHELD_NOTICE) {
        p.text.push_str(WITHHELD_NOTICE);
    }
    p.overflowed = true;
}

/// Where the PEM stage is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Pem {
    /// Outside a block, looking for [`PEM_BEGIN`].
    #[default]
    Outside,
    /// Inside a block, looking for [`PEM_END`].
    Block,
    /// Past [`PEM_END`], looking for [`PEM_CLOSE`].
    BlockEnd,
}

/// One stream's state.
#[derive(Debug, Default)]
struct Stream {
    pem: Pem,
    /// A trailing partial marker the PEM stage cannot judge yet.
    pem_tail: String,
    /// The current line of PEM output: published context, then uncertain bytes.
    line: String,
    /// `line[..cut]` has been published, redacted, as `out_len` bytes.
    cut: usize,
    out_len: usize,
    /// The furthest cut already judged.
    judged: usize,
    /// The current line overflowed: drop everything up to its newline.
    discarding: bool,
    /// Out of room: withhold everything until the stream ends.
    sealed: bool,
    /// An incomplete UTF-8 character from [`StreamSanitiser::push_bytes`].
    carry: Vec<u8>,
}

/// The length of the longest proper prefix of `marker` that `text` ends with. `marker` is
/// ASCII, so the split is a char boundary.
fn partial_marker(text: &str, marker: &str) -> usize {
    (1..marker.len())
        .rev()
        .find(|&k| text.ends_with(&marker[..k]))
        .unwrap_or(0)
}

/// `text` in pieces of at most [`PIECE_BYTES`], split on char boundaries.
fn pieces(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let mut end = rest.len().min(PIECE_BYTES);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (piece, tail) = rest.split_at(end);
        rest = tail;
        Some(piece)
    })
}

impl Stream {
    /// What this stream holds, by capacity.
    fn retained(&self) -> usize {
        self.pem_tail.capacity() + self.line.capacity() + self.carry.capacity()
    }

    /// Input bytes held and not yet published.
    fn uncertain(&self) -> usize {
        self.pem_tail.len() + self.line.len() - self.cut + self.carry.len()
    }

    /// Nothing pending: a fresh stream would behave the same, so the entry can go.
    fn is_idle(&self) -> bool {
        self.pem == Pem::Outside
            && !self.discarding
            && !self.sealed
            && self.pem_tail.is_empty()
            && self.line.is_empty()
            && self.carry.is_empty()
    }

    fn feed_str(&mut self, text: &str, budget: usize, p: &mut Published) {
        for piece in pieces(text) {
            if self.sealed {
                p.withheld_bytes += piece.len();
                continue;
            }
            let out = self.pem_pass(piece);
            self.lines(&out, budget, p);
            self.enforce(budget, p);
        }
    }

    fn feed_bytes(&mut self, bytes: &[u8], budget: usize, p: &mut Published) {
        for piece in bytes.chunks(PIECE_BYTES) {
            if self.sealed {
                p.withheld_bytes += piece.len();
                continue;
            }
            let (text, carry) = decode(std::mem::take(&mut self.carry), piece);
            self.carry = carry;
            let out = self.pem_pass(&text);
            self.lines(&out, budget, p);
            self.enforce(budget, p);
        }
    }

    /// Keeps every buffer within `budget`: the line goes first, then the stream is sealed.
    fn enforce(&mut self, budget: usize, p: &mut Published) {
        if self.retained() <= budget {
            return;
        }
        if !self.line.is_empty() {
            self.overflow_line(0, p);
        }
        if self.retained() > budget {
            p.withheld_bytes += self.uncertain();
            notice(p);
            *self = Stream {
                sealed: true,
                ..Stream::default()
            };
        }
    }

    /// Drops the current line's unpublished bytes (and `extra` more) and the rest of the line.
    fn overflow_line(&mut self, extra: usize, p: &mut Published) {
        p.withheld_bytes += self.line.len() - self.cut + extra;
        notice(p);
        self.reset_line();
        self.discarding = true;
    }

    /// The PEM stage: `chunk`'s contribution to `redact_pem_blocks` of the whole stream.
    fn pem_pass(&mut self, chunk: &str) -> String {
        let mut buf = std::mem::take(&mut self.pem_tail);
        buf.push_str(chunk);
        let mut out = String::new();
        let mut rest = buf.as_str();
        loop {
            let marker = match self.pem {
                Pem::Outside => PEM_BEGIN,
                Pem::Block => PEM_END,
                Pem::BlockEnd => PEM_CLOSE,
            };
            match rest.find(marker) {
                Some(at) => {
                    if self.pem == Pem::Outside {
                        out.push_str(&rest[..at]);
                        out.push_str(PEM_MASK);
                    }
                    rest = &rest[at + marker.len()..];
                    self.pem = match self.pem {
                        Pem::Outside => Pem::Block,
                        Pem::Block => Pem::BlockEnd,
                        Pem::BlockEnd => Pem::Outside,
                    };
                }
                None => {
                    let keep = rest.len() - partial_marker(rest, marker);
                    if self.pem == Pem::Outside {
                        out.push_str(&rest[..keep]);
                    }
                    self.pem_tail = rest[keep..].to_owned();
                    return out;
                }
            }
        }
    }

    /// The line stage: takes PEM output, publishes what is safe.
    fn lines(&mut self, text: &str, budget: usize, p: &mut Published) {
        for (i, segment) in text.split('\n').enumerate() {
            if i > 0 {
                self.end_line(p);
                p.text.push('\n');
            }
            self.append(segment, budget, p);
        }
        self.publish_safe_prefix(p);
    }

    /// Adds `segment` to the line, growing its buffer no further than the budget allows.
    fn append(&mut self, segment: &str, budget: usize, p: &mut Published) {
        if segment.is_empty() {
            return;
        }
        if self.discarding {
            p.withheld_bytes += segment.len();
            return;
        }
        let room = budget.saturating_sub(self.pem_tail.capacity() + self.carry.capacity());
        let needed = self.line.len() + segment.len();
        if needed > room {
            self.overflow_line(segment.len(), p);
            return;
        }
        if needed > self.line.capacity() {
            let target = needed.max(self.line.capacity().saturating_mul(2)).min(room);
            self.line.reserve_exact(target - self.line.len());
        }
        self.line.push_str(segment);
    }

    fn reset_line(&mut self) {
        if self.line.capacity() > KEEP_CAPACITY {
            self.line = String::new();
        } else {
            self.line.clear();
        }
        self.cut = 0;
        self.out_len = 0;
        self.judged = 0;
        self.discarding = false;
    }

    /// Publishes the rest of the current line, which is complete, and starts the next.
    fn end_line(&mut self, p: &mut Published) {
        if !self.discarding {
            let (full, _) = redact_line(&self.line);
            #[cfg(debug_assertions)]
            {
                let published = redact_line(&self.line[..self.cut]).0;
                debug_assert!(
                    full.starts_with(&published),
                    "a published prefix changed: {published:?} vs {full:?}"
                );
            }
            // Everything in `full` is redacted, so even an (impossible) mismatch publishes
            // nothing `redact` would not.
            p.text.push_str(full.get(self.out_len..).unwrap_or(&full));
        }
        self.reset_line();
    }

    /// Publishes the line up to its last blank if no rule's decision reaches that blank.
    fn publish_safe_prefix(&mut self, p: &mut Published) {
        if self.discarding {
            return;
        }
        let Some(blank) = self.line[self.cut..].rfind([' ', '\t']) else {
            return;
        };
        let cut = self.cut + blank + 1;
        if cut <= self.judged {
            return;
        }
        if self.line.len() > JUDGE_EVERY_PUSH_UP_TO && cut - self.judged < self.line.len() / 8 {
            return;
        }
        self.judged = cut;
        let prefix = &self.line[..cut];
        let (out, open) = redact_line(prefix);
        // A rule that kept the trailing blank left nothing open at it; checked, not assumed.
        let blank_kept = out.ends_with(&prefix[cut - 1..]);
        if open || !blank_kept || out.len() < self.out_len || !out.is_char_boundary(self.out_len) {
            return;
        }
        #[cfg(debug_assertions)]
        {
            let published = redact_line(&self.line[..self.cut]).0;
            debug_assert!(out.starts_with(&published), "{published:?} vs {out:?}");
        }
        p.text.push_str(&out[self.out_len..]);
        self.out_len = out.len();
        self.cut = cut;
    }

    /// Ends the stream: a partial character becomes U+FFFD, a partial PEM marker outside a
    /// block is plain text, and the last line is published whole. A sealed stream publishes
    /// nothing more.
    fn finish(mut self, p: &mut Published) {
        if self.sealed {
            return;
        }
        if !self.carry.is_empty() {
            self.carry = Vec::new();
            let text = self.pem_pass("\u{FFFD}");
            self.lines(&text, usize::MAX, p);
        }
        if self.pem == Pem::Outside {
            let tail = std::mem::take(&mut self.pem_tail);
            self.lines(&tail, usize::MAX, p);
        }
        self.end_line(p);
    }
}

/// Decodes `bytes` after `carry` like `String::from_utf8_lossy` would decode the whole byte
/// stream, returning the text and the incomplete character left at the end.
fn decode(mut carry: Vec<u8>, bytes: &[u8]) -> (String, Vec<u8>) {
    carry.extend_from_slice(bytes);
    let mut out = String::new();
    let mut rest = carry.as_slice();
    loop {
        match std::str::from_utf8(rest) {
            Ok(text) => {
                out.push_str(text);
                return (out, Vec::new());
            }
            Err(e) => {
                let (valid, after) = rest.split_at(e.valid_up_to());
                out.push_str(&String::from_utf8_lossy(valid));
                match e.error_len() {
                    Some(bad) => {
                        out.push('\u{FFFD}');
                        rest = &after[bad..];
                    }
                    None => return (out, after.to_vec()),
                }
            }
        }
    }
}

/// Publishes streamed text per stream, never ahead of what [`redact`] would mask. See the
/// module documentation for the invariant and its conservative divergences.
///
/// Keys are never stored: each is reduced to a [`KeyId`], two 64-bit SipHashes under random
/// per-sanitiser seeds, so a key of any size costs 16 bytes in either table. Two keys
/// colliding on all 128 bits would share a stream; with seeds no caller sees, that is out of
/// reach by construction rather than by chance alone.
#[derive(Debug)]
pub struct StreamSanitiser<K = StreamKey> {
    streams: HashMap<KeyId, Stream>,
    /// Streams refused for want of an entry: withheld until finished, cancelled or replaced.
    refused: HashSet<KeyId>,
    /// A refused stream could not be remembered: every stream without state is withheld.
    saturated: bool,
    seeds: (RandomState, RandomState),
    limits: Limits,
    retained: usize,
    key: PhantomData<fn(&K)>,
}

/// A fixed-size stand-in for a key: what the sanitiser stores instead of the key itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct KeyId(u64, u64);

impl<K: Hash> Default for StreamSanitiser<K> {
    fn default() -> Self {
        Self::new(Limits::default())
    }
}

impl<K: Hash> StreamSanitiser<K> {
    pub fn new(limits: Limits) -> Self {
        Self {
            streams: HashMap::new(),
            refused: HashSet::new(),
            saturated: false,
            seeds: (RandomState::new(), RandomState::new()),
            limits,
            retained: 0,
            key: PhantomData,
        }
    }

    fn id(&self, key: &K) -> KeyId {
        KeyId(self.seeds.0.hash_one(key), self.seeds.1.hash_one(key))
    }

    /// Bytes all streams retain now, by buffer capacity.
    pub fn retained_bytes(&self) -> usize {
        self.retained
    }

    /// Streams holding state (something pending, overflowed or sealed).
    pub fn open_streams(&self) -> usize {
        self.streams.len()
    }

    /// More streams were refused than could be remembered: from now on every stream without
    /// state is withheld. Surface it as an error and start a new sanitiser for new work.
    pub fn saturated(&self) -> bool {
        self.saturated
    }

    /// Runs `f` on `key`'s stream with its byte budget; keeps the entry only if it holds
    /// something, and refuses a new stream that needs an entry when none is free.
    fn feed(
        &mut self,
        key: &K,
        input_len: usize,
        p: &mut Published,
        f: impl FnOnce(&mut Stream, usize, &mut Published),
    ) {
        let key = self.id(key);
        if self.refused.contains(&key) {
            p.withheld_bytes += input_len;
            return;
        }
        let existing = self.streams.remove(&key);
        let had = existing.is_some();
        if !had && self.saturated {
            // Noticed once, when it saturated; from then on the withholding is silent.
            p.withheld_bytes += input_len;
            return;
        }
        let mut stream = existing.unwrap_or_default();
        let others = self.retained - stream.retained();
        let budget = self
            .limits
            .per_stream_bytes
            .min(self.limits.aggregate_bytes.saturating_sub(others));
        f(&mut stream, budget, p);
        self.retained = others;
        if stream.is_idle() {
            return;
        }
        if !had && self.streams.len() >= self.limits.max_streams {
            p.withheld_bytes += stream.uncertain();
            // The one notice for this key; saturating happens once, so its notice is once too.
            notice(p);
            if self.refused.len() < self.limits.max_streams {
                self.refused.insert(key);
            } else {
                self.saturated = true;
            }
            return;
        }
        self.retained += stream.retained();
        self.streams.insert(key, stream);
    }

    fn take(&mut self, key: &K) -> Option<Stream> {
        let stream = self.streams.remove(&self.id(key))?;
        self.retained -= stream.retained();
        Some(stream)
    }

    /// Adds `chunk` to `key`'s stream; returns what became safe to publish.
    pub fn push(&mut self, key: &K, chunk: &str) -> Published {
        let mut p = Published::default();
        self.feed(key, chunk.len(), &mut p, |s, budget, p| {
            s.feed_str(chunk, budget, p)
        });
        p
    }

    /// [`StreamSanitiser::push`] for raw bytes, which may split a character anywhere or not
    /// be UTF-8 at all (decoded as `String::from_utf8_lossy` would decode the whole stream).
    pub fn push_bytes(&mut self, key: &K, chunk: &[u8]) -> Published {
        let mut p = Published::default();
        self.feed(key, chunk.len(), &mut p, |s, budget, p| {
            s.feed_bytes(chunk, budget, p)
        });
        p
    }

    /// Ends `key`'s stream: publishes the rest of its last line, redacted, and forgets it.
    pub fn finish(&mut self, key: &K) -> Published {
        let mut p = Published::default();
        if self.refused.remove(&self.id(key)) {
            return p;
        }
        if let Some(stream) = self.take(key) {
            stream.finish(&mut p);
        }
        p
    }

    /// Abandons `key`'s stream: nothing not yet published ever is. `withheld_bytes` counts
    /// what was dropped.
    pub fn cancel(&mut self, key: &K) -> Published {
        let mut p = Published::default();
        self.refused.remove(&self.id(key));
        if let Some(stream) = self.take(key) {
            p.withheld_bytes = stream.uncertain();
        }
        p
    }

    /// Replaces `key`'s stream with a provider's snapshot of the whole item so far: the state
    /// built from earlier deltas is dropped and `full_text` is pushed into a fresh stream. The
    /// result has `replace` set; its text is everything now published for the stream.
    pub fn replace_snapshot(&mut self, key: &K, full_text: &str) -> Published {
        self.refused.remove(&self.id(key));
        self.take(key);
        let mut p = self.push(key, full_text);
        p.replace = true;
        p
    }
}

#[cfg(test)]
mod tests {
    use super::super::redact;
    use super::*;

    /// A fake key, not a credential.
    const PEM: &str =
        "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAAB3NzaC1yc2E\n-----END OPENSSH PRIVATE KEY-----"; // gitleaks:allow

    /// Representative text for every rule family, and a fragment each must not leak. Fake
    /// values in real shapes; none is a live credential.
    fn fixtures() -> Vec<(String, &'static str)> {
        let aws = "AKIAIOSFODNN7EXAMPLE"; // gitleaks:allow
        vec![
            (
                "export GH=ghp_abcdefghijklmnopqrstuvwxyz0123 and".into(),
                "ghp_abcdefghij",
            ),
            ("key AIzaSyA1234567890abcdefgh end".into(), "AIzaSyA123"),
            (format!("aws {aws} here"), "IOSFODNN7"),
            (
                "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NSJ9.c2lnbmF0dXJl done".into(),
                "eyJzdWIi",
            ),
            (
                "Authorization: Bearer abcdefghijklmnop rest".into(),
                "abcdefghijkl",
            ),
            (
                "Basic dXNlcjpodW50ZXIyaHVudGVy is the authorization".into(),
                "dXNlcjpodW50",
            ),
            (
                "token abcdefgh12345678 then authorization: x".into(),
                "abcdefgh1234",
            ),
            (
                "clone https://deploy:s3cretPassw0rd@git.example.com/r.git now".into(),
                "s3cretPassw0rd",
            ),
            (
                "tool --password correcthorse --verbose".into(),
                "correcthorse",
            ),
            (
                "run --api-token 'staple staple staple' next".into(),
                "staple staple",
            ),
            ("curl -u alice:s3cr3tpass https://x".into(), "s3cr3tpass"),
            (
                "curl -s --user 'alice:two words here' https://x".into(),
                "two words",
            ),
            (
                "curl -H 'Cookie: session=abcdef123456' https://x".into(),
                "abcdef123456",
            ),
            ("X-Api-Key: k1234567890 to the end".into(), "k1234567890"),
            ("mysql -u root -pHunter2pass db".into(), "Hunter2pass"),
            (
                "sshpass -p hunter22secret ssh host".into(),
                "hunter22secret",
            ),
            ("wget --password topsecret1 https://x".into(), "topsecret1"),
            ("API_KEY=supersecretvalue1234 next".into(), "supersecret"),
            (
                "\"db_password\": \"hunter2 hunter2\", \"x\": 1".into(),
                "hunter2 hunter2",
            ),
            (format!("before {PEM} after\nnext"), "AAAAB3"),
            (format!("key: {PEM}"), "AAAAB3"),
            (
                "-----BEGIN RSA PRIVATE KEY-----\nMIIEunterminated".into(),
                "MIIEun",
            ),
            (
                "café TOKEN=ééééééééé€ ghp_ßßßßßßßßßßßß 日本語 password: ünïcödé!!".into(),
                "ééééééééé€",
            ),
            (
                "{\"api_key\": \"abcdefgh12345678, \"secret\":\"open value with spaces".into(),
                "abcdefgh1234",
            ),
        ]
    }

    /// CRLF and malformed UTF-8 around PEM markers and rule delimiters, as raw bytes.
    const EXTRAS: &[&[u8]] = &[
        b"API_KEY=supersecretvalue1234\r\nnext line\r\n",
        b"-----BEGIN RSA PRIVATE KEY-----\r\nMIIEsecretbody\r\n-----END RSA PRIVATE KEY-----\r\nafter\r\n", // gitleaks:allow
        b"Authorization: Bearer abcdefghijklmnop\r\nBasic dXNlcjpodW50ZXIyaHVudGVy\r\n",
        b"curl -u alice:s3cr3tpass\r\n https://x",
        b"\"api_key\":\"abc\r\ndefghijklmnop\"\r\n",
        b"url https://user:pa\r\nss@host/x and https://u:pw123456@h/\r\n",
        b"pre \xff-----BEGIN KEY-----\nMIIE\xfe\n-----END KEY-----\xc3 post",
        b"password: \xe2\x82hunter2hunter2 \xff token=\xf0\x9f\x92supersecret99",
        b"ghp_abcdefghijkl\xffmnopqrstuvwxyz0123 and Bearer abc\xc0defghijklmn",
        b"-----BEG\xffIN KEY----- not a block; -----BEGIN \xffKEY\n-----END\xff -----END x----- tail",
        b"Cookie: sid=\xff\xfeabcdef123456\r\nX-Api-Key:\t\xe6\x97k1234567890",
    ];

    /// `redact` of each of `fixtures()` then `EXTRAS` (lossily decoded), produced by the
    /// pre-refactor `redact.rs` (master at cf63cc31, compiled standalone) and frozen here: an
    /// independent check that neither `redact` nor the sanitiser drifted. Some entries record
    /// `redact`'s known limits as they stand (a `\r` kept inside a masked value, `Basic` on a
    /// line without "authorization", a token cut by an invalid byte).
    const BASELINE: &[&str] = &[
        "export GH=****0123 and",
        "key ****efgh end",
        "aws ****MPLE here",
        "jwt ****dXJl done",
        "Authorization: Bearer ****mnop rest",
        "Basic ****dGVy is the authorization",
        "token ****5678 then authorization: x",
        "clone https://deploy:****w0rd@git.example.com/r.git now",
        "tool --password ****orse --verbose",
        "run --api-token '****aple' next",
        "curl -u alice:****pass https://x",
        "curl -s --user 'alice:****here' https://x",
        "curl -H 'Cookie: ****3456' https://x",
        "X-Api-Key: **** end",
        "mysql -u root -p****pass db",
        "sshpass -p ****cret ssh host",
        "wget --password ****ret1 https://x",
        "API_KEY=****1234 next",
        "\"db_password\": \"****ter2\", \"x\": 1",
        "before [REDACTED PEM BLOCK] after\nnext",
        "key: ****CTED PEM BLOCK]",
        "[REDACTED PEM BLOCK]",
        "café TOKEN=****ééé€ ghp_ßßßßßßßßßßßß 日本語 password: ****dé!!",
        "{\"api_key\": \"****78, \"secret\":\"****aces",
        "API_KEY=****234\r\nnext line\r\n",
        "[REDACTED PEM BLOCK]\r\nafter\r\n",
        "Authorization: Bearer ****mnop\r\nBasic dXNlcjpodW50ZXIyaHVudGVy\r\n",
        "curl -u alice:****ass\r\n https://x",
        "\"api_key\":\"abc\r\ndefghijklmnop\"\r\n",
        "url https://user:pa\r\nss@host/x and https://u:****3456@h/\r\n",
        "pre \u{FFFD}[REDACTED PEM BLOCK]\u{FFFD} post",
        "password: ****ter2 \u{FFFD} token=****et99",
        "****ijkl\u{FFFD}mnopqrstuvwxyz0123 and Bearer abc\u{FFFD}defghijklmn",
        "-----BEG\u{FFFD}IN KEY----- not a block; [REDACTED PEM BLOCK] tail",
        "Cookie: ****456\r\nX-Api-Key:\t****7890",
    ];

    fn baseline_inputs() -> Vec<Vec<u8>> {
        fixtures()
            .into_iter()
            .map(|(t, _)| t.into_bytes())
            .chain(EXTRAS.iter().map(|b| b.to_vec()))
            .collect()
    }

    /// Pushes `chunks` into one stream, checking after every call that what is published is
    /// a prefix of `redact` of the whole, and returns everything published.
    fn run(chunks: &[&str]) -> String {
        let whole: String = chunks.concat();
        let expected = redact(&whole);
        let mut s: StreamSanitiser<u8> = StreamSanitiser::default();
        let mut published = String::new();
        for chunk in chunks {
            let p = s.push(&0, chunk);
            assert!(!p.replace && !p.overflowed);
            published.push_str(&p.text);
            assert!(
                expected.starts_with(&published),
                "published ahead of redact for {chunks:?}:\n{published:?}\n{expected:?}"
            );
        }
        published.push_str(&s.finish(&0).text);
        assert_eq!(s.retained_bytes(), 0);
        assert_eq!(s.open_streams(), 0);
        published
    }

    fn boundaries(text: &str) -> Vec<usize> {
        (0..=text.len())
            .filter(|&i| text.is_char_boundary(i))
            .collect()
    }

    fn limits(per_stream_bytes: usize, aggregate_bytes: usize) -> Limits {
        Limits {
            per_stream_bytes,
            aggregate_bytes,
            ..Limits::default()
        }
    }

    #[test]
    fn every_fixture_is_masked_by_redact_itself() {
        for (text, leaked) in fixtures() {
            assert!(!redact(&text).contains(leaked), "{text}: {}", redact(&text));
        }
    }

    #[test]
    fn redact_and_the_stream_match_the_frozen_pre_refactor_baseline() {
        let inputs = baseline_inputs();
        assert_eq!(inputs.len(), BASELINE.len());
        for (bytes, expected) in inputs.iter().zip(BASELINE) {
            assert_eq!(
                redact(&String::from_utf8_lossy(bytes)),
                *expected,
                "{bytes:?}"
            );
            // Every single byte split, through the byte path.
            for i in 0..=bytes.len() {
                let mut s: StreamSanitiser<u8> = StreamSanitiser::default();
                let mut out = s.push_bytes(&0, &bytes[..i]).text;
                assert!(expected.starts_with(&out), "{i}: {out:?}");
                out.push_str(&s.push_bytes(&0, &bytes[i..]).text);
                assert!(expected.starts_with(&out), "{i}: {out:?}");
                out.push_str(&s.finish(&0).text);
                assert_eq!(out, *expected, "split at {i}: {bytes:?}");
            }
            // Byte by byte.
            let mut s: StreamSanitiser<u8> = StreamSanitiser::default();
            let mut out = String::new();
            for b in bytes {
                out.push_str(&s.push_bytes(&0, std::slice::from_ref(b)).text);
            }
            out.push_str(&s.finish(&0).text);
            assert_eq!(out, *expected, "byte by byte: {bytes:?}");
        }
    }

    #[test]
    fn every_single_and_double_split_publishes_exactly_redact() {
        for (text, leaked) in fixtures() {
            let expected = redact(&text);
            let cuts = boundaries(&text);
            for &i in &cuts {
                for &j in cuts.iter().filter(|&&j| j >= i) {
                    let out = run(&[&text[..i], &text[i..j], &text[j..]]);
                    assert_eq!(out, expected, "split at {i},{j}: {text:?}");
                    assert!(!out.contains(leaked));
                }
            }
        }
    }

    #[test]
    fn char_by_char_publishes_exactly_redact() {
        for (text, _) in fixtures() {
            let chars: Vec<String> = text.chars().map(String::from).collect();
            let chunks: Vec<&str> = chars.iter().map(String::as_str).collect();
            assert_eq!(run(&chunks), redact(&text), "{text:?}");
        }
    }

    /// A tiny deterministic generator (no new dependency for property tests).
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, n: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 33) as usize) % n.max(1)
        }
    }

    /// Splits `text` at random char boundaries.
    fn random_chunks<'a>(text: &'a str, rng: &mut Lcg) -> Vec<&'a str> {
        let cuts = boundaries(text);
        let mut at: Vec<usize> = (0..rng.next(8))
            .map(|_| cuts[rng.next(cuts.len())])
            .collect();
        at.push(0);
        at.push(text.len());
        at.sort_unstable();
        at.windows(2).map(|w| &text[w[0]..w[1]]).collect()
    }

    /// Random text from the pieces every rule reacts to, under random chunkings: the published
    /// text is a prefix of `redact` at every step and equal to it at the end. This is the check
    /// on the claim that a closed prefix cannot change.
    #[test]
    fn random_rule_soup_never_publishes_ahead_of_redact() {
        const PIECES: &[&str] = &[
            " ",
            " ",
            " ",
            "\t",
            "\n",
            "\r\n",
            "'",
            "\"",
            "=",
            ":",
            ";",
            "|",
            "&&",
            "@",
            "/",
            "://",
            "Bearer ",
            "basic ",
            "Token ",
            "authorization",
            "Authorization: ",
            "curl",
            "wget",
            "mysql",
            "sshpass",
            "-u",
            "-p",
            "-su",
            "-H",
            "--user",
            "--password",
            "--api-token",
            "--header",
            "--verbose",
            "alice:pw",
            "s3cr3tpass99",
            "abcdefghijklmnop",
            "ghp_",
            "sk-ant-",
            "AKIA",
            "eyJ",
            ".",
            "abc",
            "Cookie:",
            "X-Api-Key:",
            "API_KEY",
            "password",
            "secret",
            "https",
            "-----BEGIN ",
            "-----END ",
            "-----",
            "é",
            "日本",
            "😀",
            "ß",
            "\u{FFFD}",
            "****",
            "x",
            "1234",
        ];
        let mut rng = Lcg(0x5eed);
        for _ in 0..6000 {
            let text: String = (0..rng.next(40))
                .map(|_| PIECES[rng.next(PIECES.len())])
                .collect();
            let chunks = random_chunks(&text, &mut rng);
            assert_eq!(run(&chunks), redact(&text), "{chunks:?}");
        }
    }

    #[test]
    fn random_chunkings_of_all_fixtures_together() {
        let corpus: String = fixtures()
            .into_iter()
            .map(|(t, _)| t)
            .collect::<Vec<_>>()
            .join("\n");
        let mut rng = Lcg(7);
        for _ in 0..300 {
            let chunks = random_chunks(&corpus, &mut rng);
            assert_eq!(run(&chunks), redact(&corpus));
        }
    }

    #[test]
    fn safe_text_is_published_before_its_line_ends() {
        let mut s: StreamSanitiser<u8> = StreamSanitiser::default();
        assert_eq!(s.push(&0, "hello world, this ").text, "hello world, this ");
        assert_eq!(s.push(&0, "is a te").text, "is a ");
        assert_eq!(s.push(&0, "st and ").text, "test and ");
        // A token prefix holds back only from its word on.
        assert_eq!(s.push(&0, "ghp_abcdefghijkl").text, "");
        assert_eq!(s.push(&0, "mnopqrstuvwxyz0123 ok ").text, "****0123 ok ");
        // "Token" waits: a later "authorization" on the line would mask the next word.
        assert_eq!(s.push(&0, "Token abcdefgh12345678 ").text, "");
        assert_eq!(
            s.push(&0, "is fine\nnext ").text,
            "Token abcdefgh12345678 is fine\nnext "
        );
        assert_eq!(s.finish(&0).text, "");
    }

    #[test]
    fn interleaved_streams_keep_their_own_state() {
        let fx = fixtures();
        let mut s: StreamSanitiser<StreamKey> = StreamSanitiser::default();
        let keys: Vec<StreamKey> = (0..fx.len())
            .map(|i| StreamKey {
                worker: format!("w{}", i % 3),
                generation: (i % 2) as u64,
                item: format!("item{i}"),
                stream: "text".into(),
            })
            .collect();
        let mut published = vec![String::new(); fx.len()];
        let longest = fx.iter().map(|(t, _)| t.len()).max().unwrap_or(0);
        for at in 0..longest {
            for (k, (text, _)) in fx.iter().enumerate() {
                if at < text.len() && text.is_char_boundary(at) {
                    let end = (at + 1..=text.len())
                        .find(|&e| text.is_char_boundary(e))
                        .unwrap_or(text.len());
                    published[k].push_str(&s.push(&keys[k], &text[at..end]).text);
                }
            }
        }
        for (k, (text, leaked)) in fx.iter().enumerate() {
            published[k].push_str(&s.finish(&keys[k]).text);
            assert_eq!(published[k], redact(text));
            assert!(!published[k].contains(leaked));
        }
        assert_eq!((s.retained_bytes(), s.open_streams()), (0, 0));
    }

    #[test]
    fn byte_chunks_split_anywhere_decode_like_lossy() {
        let mut bytes = "café ghp_ßßßßßßßßßßßß 日本語 TOKEN=ééééééééé€ 😀 x"
            .as_bytes()
            .to_vec();
        bytes.extend_from_slice(b" bad \xff\xfe and \xe6\x97");
        let expected = redact(&String::from_utf8_lossy(&bytes));
        for i in 0..=bytes.len() {
            for j in i..=bytes.len() {
                let mut s: StreamSanitiser<u8> = StreamSanitiser::default();
                let mut out = String::new();
                for part in [&bytes[..i], &bytes[i..j], &bytes[j..]] {
                    out.push_str(&s.push_bytes(&0, part).text);
                    assert!(expected.starts_with(&out), "{i},{j}: {out:?}");
                }
                out.push_str(&s.finish(&0).text);
                assert_eq!(out, expected, "{i},{j}");
            }
        }
    }

    #[test]
    fn a_snapshot_replaces_the_deltas() {
        let mut s: StreamSanitiser<u8> = StreamSanitiser::default();
        assert_eq!(s.push(&0, "API_KEY=supers").text, "");
        assert_eq!(s.push(&0, "ecretv").text, "");
        let p = s.replace_snapshot(&0, "API_KEY=supersecretvalue1234 done and ");
        assert!(p.replace);
        assert_eq!(p.text, "API_KEY=****1234 done and ");
        // Deltas after the snapshot continue it.
        let mut rest = s.push(&0, "ghp_abcdefghijklmnop").text;
        rest.push_str(&s.finish(&0).text);
        assert_eq!(rest, "****mnop");
        assert_eq!(s.retained_bytes(), 0);
        // A snapshot of a different text starts over from it.
        s.push(&0, "Basic dXNlcjpodW50ZXIyaHVudGVy ");
        let p = s.replace_snapshot(&0, "short\n");
        assert_eq!((p.text.as_str(), p.replace), ("short\n", true));
    }

    #[test]
    fn cancel_never_flushes_uncertain_bytes() {
        let mut s: StreamSanitiser<u8> = StreamSanitiser::default();
        let mut out = s.push(&0, "safe words ").text;
        out.push_str(&s.push(&0, "then ghp_abcdefghij").text);
        let p = s.cancel(&0);
        assert_eq!(p.text, "");
        assert_eq!(p.withheld_bytes, "ghp_abcdefghij".len());
        assert_eq!(out, "safe words then ");
        assert_eq!((s.retained_bytes(), s.open_streams()), (0, 0));
        // A cancelled stream inside a PEM block, or an unknown one, publishes nothing.
        s.push(&1, "x -----BEGIN KEY-----\nMIIE");
        assert_eq!(s.cancel(&1).text, "");
        assert_eq!(s.cancel(&9), Published::default());
        assert_eq!(s.finish(&9), Published::default());
    }

    #[test]
    fn a_huge_unterminated_line_is_withheld_not_flushed() {
        let mut s: StreamSanitiser<u8> = StreamSanitiser::new(limits(256, 1 << 20));
        let line = format!("{}Basic {} ", "word ".repeat(30), "Z".repeat(300));
        let mut out = String::new();
        let mut overflowed = false;
        for chunk in line.as_bytes().chunks(7) {
            let p = s.push_bytes(&0, chunk);
            overflowed |= p.overflowed;
            out.push_str(&p.text);
            assert!(s.retained_bytes() <= 256);
        }
        assert!(overflowed);
        // What was safe went out, then the notice; never the uncertain `Basic` value.
        assert_eq!(out, format!("{}{WITHHELD_NOTICE}", "word ".repeat(30)));
        // The next line is checked normally.
        out.push_str(
            &s.push(
                &0,
                " still dropped\nnext ghp_abcdefghijklmnopqrstuvwxyz0123",
            )
            .text,
        );
        out.push_str(&s.finish(&0).text);
        assert!(
            out.ends_with(&format!("{WITHHELD_NOTICE}\nnext ****0123")),
            "{out}"
        );
        assert!(!out.contains("ZZZZ") && !out.contains("still"));
    }

    #[test]
    fn a_huge_pem_block_is_dropped_as_it_streams() {
        let mut s: StreamSanitiser<u8> = StreamSanitiser::new(limits(128, 128));
        let mut out = s.push(&0, "key -----BEGIN RSA PRIVATE KEY-----\n").text;
        for _ in 0..1000 {
            out.push_str(&s.push(&0, "MIIEowIBAAKCAQEA0123456789\n").text);
            assert!(s.retained_bytes() < 128);
        }
        out.push_str(&s.push(&0, "-----END RSA PRIVATE KEY----- done\n").text);
        out.push_str(&s.finish(&0).text);
        assert_eq!(out, "key [REDACTED PEM BLOCK] done\n");
    }

    #[test]
    fn the_aggregate_bound_withholds_the_stream_that_crosses_it() {
        let mut s: StreamSanitiser<u8> = StreamSanitiser::new(limits(1024, 1500));
        let held = "x".repeat(1000);
        assert!(!s.push(&0, &held).overflowed);
        let p = s.push(&1, &held);
        assert!(p.overflowed);
        assert_eq!(p.text, WITHHELD_NOTICE);
        assert!(s.retained_bytes() >= 1000 && s.retained_bytes() <= 1500);
        assert_eq!(s.finish(&0).text, held);
    }

    /// Review finding: a partial PEM marker or UTF-8 character counted against no limit.
    #[test]
    fn partial_markers_and_characters_count_against_the_limits() {
        let mut s: StreamSanitiser<u8> = StreamSanitiser::new(limits(0, 0));
        let p = s.push(&0, "-----BEGIN");
        assert!(p.overflowed);
        assert_eq!((p.text.as_str(), p.withheld_bytes), (WITHHELD_NOTICE, 10));
        assert_eq!(s.retained_bytes(), 0);
        // Sealed: nothing more is published, key bytes included, until the stream ends.
        let p = s.push(&0, " RSA KEY-----\nMIIE\n-----END RSA KEY-----\nafter\n");
        assert_eq!(p.text, "");
        assert_eq!(s.retained_bytes(), 0);
        assert_eq!(s.finish(&0).text, "");
        let p = s.push_bytes(&1, &[0xF0, 0x9F, 0x92]);
        assert!(p.overflowed);
        assert_eq!((p.text.as_str(), p.withheld_bytes), (WITHHELD_NOTICE, 3));
        assert_eq!(s.retained_bytes(), 0);
        assert_eq!(s.finish(&1).text, "");
        // A fresh stream with room is unaffected.
        let mut s: StreamSanitiser<u8> = StreamSanitiser::default();
        assert_eq!(s.push(&0, "a -----BEGIN").text, "a ");
        assert!(s.retained_bytes() > 0);
    }

    /// Review finding: 64 full streams plus a partial marker on another key went past 4 MiB.
    #[test]
    fn the_aggregate_bound_holds_across_every_buffer() {
        let mut s: StreamSanitiser<u32> = StreamSanitiser::default();
        let full = "x".repeat(DEFAULT_PER_STREAM_BYTES);
        for k in 0..64 {
            assert!(!s.push(&k, &full).overflowed);
            assert!(s.retained_bytes() <= DEFAULT_AGGREGATE_BYTES);
        }
        let p = s.push(&64, "-----BEGIN");
        assert!(p.overflowed && !p.text.contains("-----"));
        let p = s.push_bytes(&65, &[0xE6, 0x97]);
        assert!(p.overflowed);
        assert!(s.retained_bytes() <= DEFAULT_AGGREGATE_BYTES);
        assert_eq!(s.finish(&3).text, full);
    }

    /// Review finding: finished lines kept their buffers and every key kept an entry.
    #[test]
    fn idle_streams_release_their_entry_and_buffer() {
        // Far more keys than `max_streams`: none is refused, none keeps an entry.
        let mut s: StreamSanitiser<u32> = StreamSanitiser::new(Limits {
            max_streams: 4,
            ..Limits::default()
        });
        let line = format!("{}\n", "x".repeat(65_000));
        for k in 0..300 {
            assert_eq!(s.push(&k, &line).text, line);
            assert_eq!((s.retained_bytes(), s.open_streams()), (0, 0));
        }
        // An empty push creates nothing.
        assert_eq!(s.push(&5000, ""), Published::default());
        assert_eq!(s.open_streams(), 0);
        // A long line's buffer is freed at its newline even while the stream stays open.
        s.push(&1, &"y".repeat(60_000));
        assert!(s.retained_bytes() >= 60_000);
        s.push(&1, "\n-----BEGIN");
        assert!(s.retained_bytes() < KEEP_CAPACITY);
        assert_eq!(s.open_streams(), 1);
    }

    #[test]
    fn streams_past_the_limit_are_refused_never_published() {
        let mut s: StreamSanitiser<u32> = StreamSanitiser::new(Limits {
            max_streams: 2,
            ..Limits::default()
        });
        s.push(&0, "ghp_abc");
        s.push(&1, "ghp_def");
        assert_eq!(s.open_streams(), 2);
        // A stream that needs no entry still flows.
        assert_eq!(s.push(&2, "a whole line\n").text, "a whole line\n");
        // One that would: its safe prefix goes out, its uncertain tail does not, ever.
        let p = s.push(&2, "safe words then ghp_abcdefghijkl");
        assert!(p.overflowed);
        assert_eq!(p.text, format!("safe words then {WITHHELD_NOTICE}"));
        assert_eq!(p.withheld_bytes, "ghp_abcdefghijkl".len());
        let p = s.push(&2, "mnopqrstuvwx more\nnext line\n");
        assert_eq!(p.text, "");
        assert!(p.withheld_bytes > 0);
        assert_eq!(s.finish(&2), Published::default());
        // After finishing, the key starts fresh.
        assert_eq!(s.push(&2, "new item\n").text, "new item\n");
        // More refusals than can be remembered saturate: stateless streams are withheld.
        s.push(&3, "token: ");
        s.push(&4, "token: ");
        assert!(!s.saturated());
        s.push(&5, "token: ");
        assert!(s.saturated());
        let p = s.push(&6, "a whole line\n");
        assert_eq!((p.text.as_str(), p.withheld_bytes), ("", 13));
        // Streams with state carry on.
        let mut out = s.push(&0, "defghijklmnopqrstuvwxyz0123\n").text;
        out.push_str(&s.finish(&0).text);
        assert_eq!(out, "****0123\n");
    }

    /// Review finding: refused and open-stream tables cloned keys of any size.
    #[test]
    fn a_huge_key_is_remembered_by_digest_not_copied() {
        let mut s: StreamSanitiser<StreamKey> = StreamSanitiser::new(Limits {
            max_streams: 1,
            ..Limits::default()
        });
        let a = StreamKey {
            item: "a".into(),
            ..StreamKey::default()
        };
        let huge = || StreamKey {
            item: "b".repeat(100_000_000),
            ..StreamKey::default()
        };
        assert_eq!(std::mem::size_of::<KeyId>(), 16);
        s.push(&a, "ghp_abc");
        let b = huge();
        let p = s.push(&b, "ghp_def");
        assert!(p.overflowed);
        drop(b);
        // Only the digest was kept, yet an equal key built afresh is still the refused stream.
        let b = huge();
        let p = s.push(&b, "ghijklmnopqrstuvwxyz0123\n");
        assert_eq!((p.text.as_str(), p.withheld_bytes), ("", 25));
        assert_eq!(s.finish(&b), Published::default());
        assert_eq!(s.push(&b, "free again\n").text, "free again\n");
        assert_eq!(s.open_streams(), 1);
    }

    /// Review finding: saturation republished the notice on every later call.
    #[test]
    fn saturation_is_noticed_once_then_silent() {
        let mut s: StreamSanitiser<u8> = StreamSanitiser::new(Limits {
            max_streams: 0,
            ..Limits::default()
        });
        let p = s.push(&0, "x");
        assert!(s.saturated());
        assert_eq!((p.text.as_str(), p.overflowed), (WITHHELD_NOTICE, true));
        for key in [0, 0, 1, 2] {
            assert_eq!(s.push(&key, ""), Published::default());
            let p = s.push(&key, "more text\n");
            assert_eq!((p.text.as_str(), p.withheld_bytes), ("", 10));
            assert_eq!(s.push_bytes(&key, b"\xff").text, "");
        }
    }

    /// Review finding: one huge chunk was copied whole before the limits applied.
    #[test]
    fn one_huge_chunk_is_consumed_in_bounded_pieces() {
        let mut s: StreamSanitiser<u8> = StreamSanitiser::default();
        // 8 MiB of safe words with no newline: published up to the bound, then withheld.
        let huge = "word ".repeat(8 * 1024 * 1024 / 5);
        let p = s.push(&0, &huge);
        assert!(p.overflowed);
        assert!(p.text.ends_with(WITHHELD_NOTICE));
        assert!(p.text.len() <= DEFAULT_PER_STREAM_BYTES + WITHHELD_NOTICE.len());
        assert!(huge.starts_with(&p.text[..p.text.len() - WITHHELD_NOTICE.len()]));
        assert!(s.retained_bytes() <= DEFAULT_PER_STREAM_BYTES);
        assert_eq!(s.finish(&0).text, "");
        // An 8 MiB PEM body in one push, and through the byte path.
        let pem = format!(
            "k -----BEGIN RSA PRIVATE KEY-----\n{}\n-----END RSA PRIVATE KEY-----\nok\n", // gitleaks:allow
            "A".repeat(8 * 1024 * 1024)
        );
        assert_eq!(s.push(&0, &pem).text, "k [REDACTED PEM BLOCK]\nok\n");
        assert_eq!(
            s.push_bytes(&1, pem.as_bytes()).text,
            "k [REDACTED PEM BLOCK]\nok\n"
        );
        assert_eq!((s.retained_bytes(), s.open_streams()), (0, 0));
        // Many short lines in one push are each published exactly.
        let lines = "API_KEY=supersecretvalue1234 é\n".repeat(100_000);
        let mut out = s.push(&0, &lines).text;
        out.push_str(&s.finish(&0).text);
        assert_eq!(out, redact(&lines));
    }

    #[test]
    fn malformed_json_streams_like_redact() {
        let text = "{\"token\": \"ghp_abcdefghijklmnopqrstuvwxyz0123\", \"nested\": {\"api_key\":\n\"open string with spaces and \\\" quote, \"password\":\"pw\"}]]]";
        let mut rng = Lcg(42);
        for _ in 0..200 {
            let chunks = random_chunks(text, &mut rng);
            assert_eq!(run(&chunks), redact(text));
        }
    }
}
