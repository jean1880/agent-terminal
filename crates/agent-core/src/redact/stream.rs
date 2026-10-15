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
//!    blank (space or tab) is published once no rule's decision reaches its end
//!    (the open flag of the crate-private `redact_line`): no token, value, quote, header or option argument is still
//!    open there, and no `Basic`/`Token` is waiting on a later `authorization`. Text appended
//!    after such a prefix cannot change how the prefix is redacted, so what was published stays
//!    a prefix of the final redaction. The whole line is kept as context until its newline.
//!
//! # The invariant
//!
//! For any input and any chunking, the concatenation of everything published is always a
//! prefix of `redact(concat(input))`, and after [`StreamSanitiser::finish`] it is equal to it.
//! Two deliberate, conservative divergences, both of which mask *more* than `redact`:
//!
//! - **Overflow**: a line (published context plus uncertain bytes) longer than the per-stream
//!   bound, or one that would take the sanitiser past its aggregate bound, is cut: its
//!   unpublished bytes and everything up to its newline are dropped and [`WITHHELD_NOTICE`] is
//!   published in their place. No uncertain byte is ever flushed.
//! - **Cancel**: [`StreamSanitiser::cancel`] drops everything not yet published.
//!
//! Holding back is also conservative in timing only: a line with `Basic`/`Token` and no
//! `authorization` yet is held to its newline, and `curl -u ` waits for its value word, though
//! the latter could not change the prefix. Neither changes what is finally published.
//!
//! Byte input ([`StreamSanitiser::push_bytes`]) is decoded like `String::from_utf8_lossy` of
//! the whole byte stream, holding an incomplete trailing character until its next bytes come.
//!
//! The detection limits are `redact`'s own: a credential with no recognisable shape, not
//! assigned to a sensitive-looking name, gets through here exactly as it does there.

use std::collections::HashMap;
use std::hash::Hash;

use super::{redact_line, PEM_BEGIN, PEM_CLOSE, PEM_END, PEM_MASK};

#[cfg(doc)]
use super::redact;

/// The default bound on one stream's retained line (published context plus uncertain bytes).
pub const DEFAULT_PER_STREAM_BYTES: usize = 64 * 1024;

/// The default bound on what all streams together retain.
pub const DEFAULT_AGGREGATE_BYTES: usize = 4 * 1024 * 1024;

/// Published in place of a line the sanitiser had to drop rather than check.
pub const WITHHELD_NOTICE: &str = "[withheld: line too long to check for secrets]";

/// Past this many bytes in a line, a prefix is judged again only once the line has grown by an
/// eighth since the last judgement, so a long line costs amortised linear time, not quadratic.
const JUDGE_EVERY_PUSH_UP_TO: usize = 4096;

/// What the sanitiser may hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Bytes one stream may retain: its current line (published context and uncertain bytes)
    /// plus a partial PEM marker and a partial UTF-8 character.
    pub per_stream_bytes: usize,
    /// Bytes all streams together may retain.
    pub aggregate_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            per_stream_bytes: DEFAULT_PER_STREAM_BYTES,
            aggregate_bytes: DEFAULT_AGGREGATE_BYTES,
        }
    }
}

/// A placeholder stream identity: worker, process generation, item and stream. Any
/// `Eq + Hash + Clone` key works; this one is the default until the runtime defines its own.
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
    /// Bytes dropped rather than published: an overflowing line, or a cancelled stream's
    /// uncertain tail.
    pub withheld_bytes: usize,
    /// A line overflowed and [`WITHHELD_NOTICE`] is in `text`.
    pub overflowed: bool,
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

impl Stream {
    fn retained(&self) -> usize {
        self.pem_tail.len() + self.line.len() + self.carry.len()
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

    fn append(&mut self, segment: &str, budget: usize, p: &mut Published) {
        if segment.is_empty() {
            return;
        }
        if self.discarding {
            p.withheld_bytes += segment.len();
            return;
        }
        if self.retained() + segment.len() > budget {
            p.withheld_bytes += self.line.len() - self.cut + segment.len();
            p.text.push_str(WITHHELD_NOTICE);
            p.overflowed = true;
            self.reset_line();
            self.discarding = true;
            return;
        }
        self.line.push_str(segment);
    }

    fn reset_line(&mut self) {
        self.line.clear();
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
    /// block is plain text, and the last line is published whole.
    fn finish(mut self, p: &mut Published) {
        if !self.carry.is_empty() {
            self.carry.clear();
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
/// module documentation for the invariant and its two conservative divergences.
#[derive(Debug)]
pub struct StreamSanitiser<K = StreamKey> {
    streams: HashMap<K, Stream>,
    limits: Limits,
    retained: usize,
}

impl<K: Eq + Hash + Clone> Default for StreamSanitiser<K> {
    fn default() -> Self {
        Self::new(Limits::default())
    }
}

impl<K: Eq + Hash + Clone> StreamSanitiser<K> {
    pub fn new(limits: Limits) -> Self {
        Self {
            streams: HashMap::new(),
            limits,
            retained: 0,
        }
    }

    /// Bytes all streams retain now.
    pub fn retained_bytes(&self) -> usize {
        self.retained
    }

    /// Streams with state (pushed to, not yet finished or cancelled).
    pub fn open_streams(&self) -> usize {
        self.streams.len()
    }

    /// Runs `f` on `key`'s stream (created if new) with its byte budget, keeping the
    /// aggregate count right.
    fn with_stream<R>(&mut self, key: &K, f: impl FnOnce(&mut Stream, usize) -> R) -> R {
        let current = self.streams.get(key).map_or(0, Stream::retained);
        let others = self.retained - current;
        let budget = self
            .limits
            .per_stream_bytes
            .min(self.limits.aggregate_bytes.saturating_sub(others));
        let stream = self.streams.entry(key.clone()).or_default();
        let r = f(stream, budget);
        self.retained = others + stream.retained();
        r
    }

    fn take(&mut self, key: &K) -> Option<Stream> {
        let stream = self.streams.remove(key)?;
        self.retained -= stream.retained();
        Some(stream)
    }

    /// Adds `chunk` to `key`'s stream; returns what became safe to publish.
    pub fn push(&mut self, key: &K, chunk: &str) -> Published {
        let mut p = Published::default();
        self.with_stream(key, |s, budget| {
            let text = s.pem_pass(chunk);
            s.lines(&text, budget, &mut p);
        });
        p
    }

    /// [`StreamSanitiser::push`] for raw bytes, which may split a character anywhere or not
    /// be UTF-8 at all (decoded as `String::from_utf8_lossy` would decode the whole stream).
    pub fn push_bytes(&mut self, key: &K, chunk: &[u8]) -> Published {
        let mut p = Published::default();
        self.with_stream(key, |s, budget| {
            let (text, carry) = decode(std::mem::take(&mut s.carry), chunk);
            s.carry = carry;
            let text = s.pem_pass(&text);
            s.lines(&text, budget, &mut p);
        });
        p
    }

    /// Ends `key`'s stream: publishes the rest of its last line, redacted, and forgets it.
    pub fn finish(&mut self, key: &K) -> Published {
        let mut p = Published::default();
        if let Some(stream) = self.take(key) {
            stream.finish(&mut p);
        }
        p
    }

    /// Abandons `key`'s stream: nothing not yet published ever is. `withheld_bytes` counts
    /// what was dropped.
    pub fn cancel(&mut self, key: &K) -> Published {
        let mut p = Published::default();
        if let Some(stream) = self.take(key) {
            p.withheld_bytes = stream.retained() - stream.cut;
        }
        p
    }

    /// Replaces `key`'s stream with a provider's snapshot of the whole item so far: the state
    /// built from earlier deltas is dropped and `full_text` is pushed into a fresh stream. The
    /// result has `replace` set; its text is everything now published for the stream.
    pub fn replace_snapshot(&mut self, key: &K, full_text: &str) -> Published {
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

    #[test]
    fn every_fixture_is_masked_by_redact_itself() {
        for (text, leaked) in fixtures() {
            assert!(!redact(&text).contains(leaked), "{text}: {}", redact(&text));
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
        assert_eq!(s.retained_bytes(), 0);
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
        let limits = Limits {
            per_stream_bytes: 256,
            aggregate_bytes: 1 << 20,
        };
        let mut s: StreamSanitiser<u8> = StreamSanitiser::new(limits);
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
        let limits = Limits {
            per_stream_bytes: 128,
            aggregate_bytes: 128,
        };
        let mut s: StreamSanitiser<u8> = StreamSanitiser::new(limits);
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
        let limits = Limits {
            per_stream_bytes: 1024,
            aggregate_bytes: 1500,
        };
        let mut s: StreamSanitiser<u8> = StreamSanitiser::new(limits);
        let held = "x".repeat(1000);
        assert!(!s.push(&0, &held).overflowed);
        let p = s.push(&1, &held);
        assert!(p.overflowed);
        assert_eq!(p.text, WITHHELD_NOTICE);
        assert!(!p.text.contains('x'));
        assert_eq!(s.retained_bytes(), 1000);
        assert_eq!(s.finish(&0).text, held);
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
