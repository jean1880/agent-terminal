//! Finds a question with choices at the end of a finished reply, so the view can offer the
//! choices as quick replies.
//!
//! Agents that have no question tool of their own (Codex, agy) end a reply with "Which do you
//! prefer?" and a short list. The reader here is deliberately conservative: a false negative
//! costs the user nothing (they type), a false positive is noise on every reply. So a reply
//! qualifies only when its tail is exactly
//!
//! - a list of 2 to 6 single-line items, and
//! - a paragraph ending in `?` (or the full-width `？`) either directly before the list or
//!   directly after it, whose last sentence reads as a choice ("which", "prefer", "or", …),
//!
//! and nothing about the list says it is something else: a procedure (imperative steps, commands
//! in code spans), a summary of what was done, a list of questions, or anything holding a link,
//! inline HTML, an image or a character the user cannot see. Option text is the item as rendered
//! plain text, which is exactly what the composer will hold.
//!
//! Pure: no I/O, and no message text in any error (callers log the reason, never the reply).

use std::collections::HashSet;
use std::fmt;

use pulldown_cmark::{Event, Options, Parser, Tag};

use crate::event::{Question, QuestionOption};

/// Fewest and most choices offered.
pub const MIN_OPTIONS: usize = 2;
pub const MAX_OPTIONS: usize = 6;
/// Longest option, in characters of rendered text.
pub const MAX_OPTION_CHARS: usize = 120;
/// Longest question paragraph, in characters.
const MAX_QUESTION_CHARS: usize = 300;
/// Replies longer than this are not read at all (parse cost, and no real question is that long).
const MAX_REPLY_BYTES: usize = 200_000;

/// Why a reply was not read as a question with choices. Carries no text from the reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    Empty,
    TooLong,
    /// The reply does not end in a list (with a question beside it).
    NoList,
    /// The list is not directly before or after a paragraph ending in `?`.
    NoQuestion,
    /// The question does not ask for a choice.
    NotAChoice,
    /// Fewer than [`MIN_OPTIONS`] or more than [`MAX_OPTIONS`] items.
    ItemCount(usize),
    /// An item spans lines, nests a block, or is too long.
    ItemShape,
    /// An item holds a link, inline HTML or an image.
    ItemMarkup,
    /// An item holds an invisible, zero-width, bidi or control character.
    ItemHiddenChar,
    /// The items are steps or commands, not alternatives.
    Procedure,
    /// The items report what was done, or are themselves questions.
    NotAlternatives,
    /// Two items read the same.
    Duplicate,
}

impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Empty => "empty reply",
            Self::TooLong => "reply too long",
            Self::NoList => "reply does not end in a list",
            Self::NoQuestion => "no question beside the list",
            Self::NotAChoice => "the question does not ask for a choice",
            Self::ItemCount(_) => "item count outside 2 to 6",
            Self::ItemShape => "an item is multi-line, nested or too long",
            Self::ItemMarkup => "an item holds a link, HTML or an image",
            Self::ItemHiddenChar => "an item holds an invisible character",
            Self::Procedure => "the items are steps or commands",
            Self::NotAlternatives => "the items are not alternatives",
            Self::Duplicate => "two items are the same",
        };
        match self {
            Self::ItemCount(n) => write!(f, "{text} ({n} items)"),
            _ => f.write_str(text),
        }
    }
}

/// The question with choices at the end of `text`, if it has one.
pub fn extract(text: &str) -> Option<Question> {
    extract_or_why(text).ok()
}

/// [`extract`], saying why not when there is none (for one log line per decision).
pub fn extract_or_why(text: &str) -> Result<Question, Skip> {
    if text.trim().is_empty() {
        return Err(Skip::Empty);
    }
    if text.len() > MAX_REPLY_BYTES {
        return Err(Skip::TooLong);
    }
    let events: Vec<Event<'_>> =
        Parser::new_ext(text, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH).collect();
    let blocks = top_level_blocks(&events);
    let (list, question) = tail_shape(&events, &blocks)?;
    let options = read_items(&events[list.0..list.1])?;
    let question = question_text(&events[question.0..question.1])?;
    let strong_cue = judge_question(&question)?;
    let ordered = matches!(events[list.0], Event::Start(Tag::List(Some(_))));
    judge_options(&options, strong_cue, ordered)?;
    Ok(Question {
        id: "reply".into(),
        header: "Quick replies".into(),
        question,
        options: options
            .into_iter()
            .map(|label| QuestionOption {
                label,
                description: None,
            })
            .collect(),
        multi_select: false,
    })
}

/// A half-open range of parser events: one top-level block.
type Span = (usize, usize);

/// Half-open event ranges of the document's top-level blocks.
fn top_level_blocks(events: &[Event<'_>]) -> Vec<Span> {
    let mut blocks = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, event) in events.iter().enumerate() {
        match event {
            Event::Start(_) => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    blocks.push((start, i + 1));
                }
            }
            _ if depth == 0 => blocks.push((i, i + 1)),
            _ => {}
        }
    }
    blocks
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Paragraph,
    List,
    Other,
}

fn kind(events: &[Event<'_>], block: Span) -> Kind {
    match events.get(block.0) {
        Some(Event::Start(Tag::Paragraph)) => Kind::Paragraph,
        Some(Event::Start(Tag::List(_))) => Kind::List,
        _ => Kind::Other,
    }
}

/// The list and its question paragraph: `[question, list]` or `[list, question]` at the tail.
fn tail_shape(events: &[Event<'_>], blocks: &[Span]) -> Result<(Span, Span), Skip> {
    let n = blocks.len();
    let kinds: Vec<Kind> = blocks.iter().map(|b| kind(events, *b)).collect();
    // A list last: its question is the paragraph before it.
    if n >= 1 && kinds[n - 1] == Kind::List {
        if n >= 2 && kinds[n - 2] == Kind::Paragraph {
            return Ok((blocks[n - 1], blocks[n - 2]));
        }
        return Err(Skip::NoQuestion);
    }
    // A paragraph last, the list just before it.
    if n >= 2 && kinds[n - 1] == Kind::Paragraph && kinds[n - 2] == Kind::List {
        return Ok((blocks[n - 2], blocks[n - 1]));
    }
    Err(Skip::NoList)
}

/// The plain text of a paragraph, markdown stripped.
fn question_text(events: &[Event<'_>]) -> Result<String, Skip> {
    let mut out = String::new();
    for event in events {
        match event {
            Event::Text(t) | Event::Code(t) => out.push_str(t),
            Event::SoftBreak | Event::HardBreak => out.push(' '),
            _ => {}
        }
    }
    let out = out.trim().to_owned();
    if !ends_with_question(&out) {
        return Err(Skip::NoQuestion);
    }
    if out.chars().count() > MAX_QUESTION_CHARS || out.chars().any(is_hidden) {
        return Err(Skip::NoQuestion);
    }
    Ok(out)
}

fn ends_with_question(text: &str) -> bool {
    text.trim_end_matches(|c: char| {
        c.is_whitespace() || matches!(c, ')' | ']' | '"' | '\'' | '”' | '’' | '」' | '*' | '_')
    })
    .ends_with(['?', '？'])
}

/// The question's last sentence reads as a choice rather than a yes/no about something else.
/// Returns whether the cue is a strong one ("which", "prefer", "pick", "choose", "rather"; a bare "or" is not): the question names
/// alternatives, which is what lets short imperative items ("Use A", "Use B") pass as choices.
fn judge_question(question: &str) -> Result<bool, Skip> {
    let body = question.trim_end_matches(|c: char| !c.is_alphanumeric());
    let sentence = body
        .rsplit(['.', '!', '?', '？', ':', '\n'])
        .next()
        .unwrap_or(body)
        .to_lowercase();
    const PHRASES: &[&str] = &[
        "how would you like",
        "how should",
        "how do you want",
        "what would you like",
        "what should",
        "what do you want",
        "where should",
    ];
    const WORDS: &[&str] = &[
        "which",
        "prefer",
        "choose",
        "pick",
        "select",
        "option",
        "options",
        "approach",
        "approaches",
        "alternative",
        "alternatives",
        "or",
    ];
    let words: Vec<&str> = sentence
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
        .collect();
    let strong = words
        .iter()
        .any(|w| matches!(*w, "which" | "prefer" | "pick" | "choose" | "rather"));
    if strong
        || PHRASES.iter().any(|p| sentence.contains(p))
        || words.iter().any(|w| WORDS.contains(w))
    {
        Ok(strong)
    } else {
        Err(Skip::NotAChoice)
    }
}

/// Each item's rendered plain text, or why the list is not a plain list of short single-line
/// items.
fn read_items(list: &[Event<'_>]) -> Result<Vec<String>, Skip> {
    let mut items = Vec::new();
    let mut cur: Option<ItemReader> = None;
    for event in list {
        let Some(reader) = cur.as_mut() else {
            // Between items: only the list's own items open a reader.
            if matches!(event, Event::Start(Tag::Item)) {
                cur = Some(ItemReader::default());
            }
            continue;
        };
        match event {
            Event::Start(_) => {
                reader.nest += 1;
                reader.feed(event);
            }
            Event::End(_) if reader.nest == 0 => {
                if let Some(done) = cur.take() {
                    items.push(done.finish()?);
                }
            }
            Event::End(_) => reader.nest -= 1,
            _ => reader.feed(event),
        }
    }
    // Markup and hidden characters disqualify the whole list whichever item holds them, and
    // before the count says anything about it.
    if !(MIN_OPTIONS..=MAX_OPTIONS).contains(&items.len()) {
        return Err(Skip::ItemCount(items.len()));
    }
    Ok(items)
}

#[derive(Default)]
struct ItemReader {
    /// Open tags inside the item (a paragraph, emphasis, a nested list's items).
    nest: usize,
    text: String,
    code_spans: Vec<String>,
    paragraphs: usize,
    shape: bool,
    markup: bool,
}

impl ItemReader {
    fn feed(&mut self, event: &Event<'_>) {
        match event {
            Event::Start(Tag::Paragraph) => {
                self.paragraphs += 1;
                if self.paragraphs > 1 {
                    self.shape = true;
                }
            }
            Event::Start(Tag::Emphasis | Tag::Strong | Tag::Strikethrough) => {}
            Event::Start(Tag::Link { .. } | Tag::Image { .. }) => self.markup = true,
            Event::Start(_) => self.shape = true,
            Event::Text(t) => self.text.push_str(t),
            Event::Code(c) => {
                self.text.push_str(c);
                self.code_spans.push(c.to_string());
            }
            Event::SoftBreak | Event::HardBreak => self.shape = true,
            Event::Html(_) | Event::InlineHtml(_) => self.markup = true,
            _ => self.shape = true,
        }
    }

    fn finish(self) -> Result<String, Skip> {
        // Hidden characters and markup first: they disqualify however the rest looks.
        if self.markup || has_bare_link(&self.text) {
            return Err(Skip::ItemMarkup);
        }
        if self.text.chars().any(is_hidden) {
            return Err(Skip::ItemHiddenChar);
        }
        let text = self.text.trim().to_owned();
        if self.shape || text.is_empty() || text.chars().count() > MAX_OPTION_CHARS {
            return Err(Skip::ItemShape);
        }
        if self.code_spans.iter().any(|c| looks_like_command(c)) {
            return Err(Skip::Procedure);
        }
        Ok(text)
    }
}

/// A web address typed as plain text is a link as far as the user is concerned.
fn has_bare_link(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("://") || lower.contains("www.") || lower.contains("mailto:")
}

/// Characters the user cannot see (or that reorder what they see), and any control or unusual
/// whitespace. A reply that carries one in an option could hide what a click puts in the
/// composer.
fn is_hidden(c: char) -> bool {
    matches!(c,
        '\u{00A0}' | '\u{00AD}' | '\u{034F}' | '\u{061C}' | '\u{115F}' | '\u{1160}'
        | '\u{17B4}' | '\u{17B5}' | '\u{180B}'..='\u{180F}'
        | '\u{1680}' | '\u{2800}' | '\u{2000}'..='\u{200F}' | '\u{2028}'..='\u{202F}'
        | '\u{205F}'..='\u{206F}' | '\u{3000}' | '\u{3164}'
        | '\u{FE00}'..='\u{FE0E}' | '\u{FEFF}' | '\u{FFA0}' | '\u{FFF9}'..='\u{FFFC}'
        | '\u{E0000}'..='\u{E007F}' | '\u{E0100}'..='\u{E01EF}'
    ) || c.is_control()
}

/// A code span that is something to run rather than a name: several words, a shell sigil, a
/// flag, a pipe or a redirect.
fn looks_like_command(span: &str) -> bool {
    let s = span.trim();
    s.starts_with(['$', '>'])
        || s.split_whitespace().count() >= 2
        || s.contains("--")
        || s.contains(['|', ';', '&'])
}

/// Imperative verbs a step starts with.
const IMPERATIVES: &[&str] = &[
    "run",
    "open",
    "install",
    "add",
    "click",
    "create",
    "delete",
    "set",
    "update",
    "use",
    "go",
    "navigate",
    "check",
    "restart",
    "edit",
    "remove",
    "copy",
    "paste",
    "enter",
    "press",
    "type",
    "execute",
    "configure",
    "make",
    "change",
    "save",
    "download",
    "clone",
    "build",
    "push",
    "commit",
    "start",
    "stop",
    "enable",
    "disable",
    "replace",
    "move",
    "rename",
    "import",
    "verify",
    "ensure",
    "launch",
    "write",
    "select",
    "choose",
    "log",
    "sign",
    "visit",
    "extract",
    "unzip",
    "reboot",
    "reload",
    "apply",
    "append",
    "insert",
    "uncomment",
    "comment",
];

fn first_word(item: &str) -> String {
    item.split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
}

/// The list is a set of alternatives, not steps, a report of what was done, or questions.
fn judge_options(options: &[String], strong_cue: bool, ordered: bool) -> Result<(), Skip> {
    let n = options.len();
    let firsts: Vec<String> = options.iter().map(|o| first_word(o)).collect();
    // "Most" is more than half: two of two, two of three.
    let most = |count: usize| count * 2 > n;
    if most(
        firsts
            .iter()
            .filter(|w| IMPERATIVES.contains(&w.as_str()))
            .count(),
    ) {
        // Steps run in sequence. Alternatives ("Use SQLite", "Use Postgres") pass only when the
        // question names alternatives and either every item opens with the same verb, or the
        // list is a bullet list of short items. A numbered list of different verbs is a procedure.
        let same_verb = firsts.iter().all(|w| *w == firsts[0]);
        let short = options.iter().all(|o| o.split_whitespace().count() <= 8);
        if !(strong_cue && (same_verb || (!ordered && short))) {
            return Err(Skip::Procedure);
        }
    }
    // A report of what was done. Known past-tense verbs always count; any other "-ed" word
    // ("Embedded", "Shared") only when the question names no alternatives.
    const PAST: &[&str] = &[
        "fixed",
        "added",
        "removed",
        "updated",
        "changed",
        "renamed",
        "reworked",
        "moved",
        "deleted",
        "created",
        "implemented",
        "refactored",
        "merged",
        "replaced",
        "ran",
        "wrote",
    ];
    let known_past = firsts.iter().filter(|w| PAST.contains(&w.as_str())).count();
    let any_ed = firsts
        .iter()
        .filter(|w| PAST.contains(&w.as_str()) || (w.chars().count() > 4 && w.ends_with("ed")))
        .count();
    if most(known_past) || (!strong_cue && most(any_ed)) {
        return Err(Skip::NotAlternatives);
    }
    if options.iter().any(|o| ends_with_question(o)) {
        return Err(Skip::NotAlternatives);
    }
    let mut seen = HashSet::new();
    if !options.iter().all(|o| seen.insert(o.to_lowercase())) {
        return Err(Skip::Duplicate);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reply shapes a user would want chips for. `(name, reply, options)`.
    const TRUE_CASES: &[(&str, &str, &[&str])] = &[
        (
            "question first, bullets after",
            "I found two ways to do this.\n\nWhich approach do you prefer?\n\n- Keep the current API and add a shim\n- Break the API and migrate callers",
            &["Keep the current API and add a shim", "Break the API and migrate callers"],
        ),
        (
            "numbered options then the question",
            "There are three places the cache could live.\n\n1. In memory\n2. On disk\n3. In SQLite\n\nWhich one should I go with?",
            &["In memory", "On disk", "In SQLite"],
        ),
        (
            "what would you like next",
            "Parser is done and the tests pass.\n\nWhat would you like to do next?\n\n- Add tests for the lexer\n- Refactor the store module\n- Ship it as is",
            &["Add tests for the lexer", "Refactor the store module", "Ship it as is"],
        ),
        (
            "bold labels are stripped",
            "Two designs fit.\n\n- **Option A**: keep one process\n- **Option B**: one process per thread\n\nWhich do you want?",
            &["Option A: keep one process", "Option B: one process per thread"],
        ),
        (
            "codex style, colon intro then question",
            "I can go two ways:\n\n- Patch the adapter only\n- Patch the adapter and the view\n\nWhich do you want me to take?",
            &["Patch the adapter only", "Patch the adapter and the view"],
        ),
        (
            "full-width question mark",
            "Two layouts are possible.\n\n- Tabs on top\n- Tabs on the side\n\nWhich do you prefer？",
            &["Tabs on top", "Tabs on the side"],
        ),
        (
            "six items",
            "Which language for the helper?\n\n- Rust\n- Go\n- Python\n- Shell\n- Zig\n- Lua",
            &["Rust", "Go", "Python", "Shell", "Zig", "Lua"],
        ),
        (
            "two items",
            "Should I keep the old flag or drop it?\n\n- Keep it\n- Drop it",
            &["Keep it", "Drop it"],
        ),
        (
            "identifiers in code spans are fine",
            "Which name do you prefer?\n\n- `parse_config`\n- `load_settings`\n- `read_options`",
            &["parse_config", "load_settings", "read_options"],
        ),
        (
            "bold question after the list",
            "Candidates:\n\n- SQLite (one file, no server)\n- Postgres (needs a server)\n\n**Which one do you want?**",
            &["SQLite (one file, no server)", "Postgres (needs a server)"],
        ),
        (
            "question wrapped in parentheses",
            "Plan B costs a day.\n\n(Which do you prefer?)\n\n* Plan A, ship today\n* Plan B, ship tomorrow",
            &["Plan A, ship today", "Plan B, ship tomorrow"],
        ),
        (
            "loose list",
            "Which of these should I do first?\n\n- Migrate the config\n\n- Rewrite the importer\n\n- Update the docs later",
            &["Migrate the config", "Rewrite the importer", "Update the docs later"],
        ),
        (
            "agy style, how would you like to proceed",
            "The branch is ready.\n\nHow would you like to proceed?\n\n1. Merge now\n2. Wait for CI\n3. Open a draft PR",
            &["Merge now", "Wait for CI", "Open a draft PR"],
        ),
        (
            "same leading verb across alternatives",
            "Which database should I use?\n\n- Use SQLite\n- Use Postgres",
            &["Use SQLite", "Use Postgres"],
        ),
        (
            "short bullet alternatives with different verbs",
            "Which do you prefer?\n\n- Remove the old flag\n- Replace it with a setting",
            &["Remove the old flag", "Replace it with a setting"],
        ),
        (
            "words that merely end in -ed are not past tense",
            "Which do you prefer?\n\n- Shared cache across threads\n- Embedded cache per thread",
            &["Shared cache across threads", "Embedded cache per thread"],
        ),
        (
            "entities decode to plain text",
            "Which do you prefer?\n\n- Fish &amp; chips\n- Soup &lt;hot&gt;",
            &["Fish & chips", "Soup <hot>"],
        ),
    ];

    /// Replies that must offer nothing. `(name, reply)`.
    const FALSE_CASES: &[(&str, &str)] = &[
        (
            "procedure, imperative steps",
            "To fix it, do this.\n\n1. Run the installer\n2. Open the settings page\n3. Set the port to 8080\n\nDoes that work?",
        ),
        (
            "procedure whose question sounds like a choice",
            "To fix it, do this.\n\n1. Run the installer\n2. Open the settings page\n3. Set the port to 8080\n\nWhich step is unclear?",
        ),
        (
            "procedure with a choice-sounding question",
            "Here is what to do.\n\n1. Install the package\n2. Update the lockfile\n\nWould you rather do this yourself or have me apply it?",
        ),
        (
            "procedure of commands in code spans",
            "Which of these do you want to run?\n\n- `cargo test --workspace`\n- `cargo clippy -- -D warnings`",
        ),
        (
            "one command in a code span",
            "Run one of these. Which do you prefer?\n\n- `git stash`\n- `git reset --hard`",
        ),
        (
            "review findings, no question",
            "Review notes:\n\n- The retry loop never backs off\n- The lock is held across the await\n- The error is dropped",
        ),
        (
            "findings then an unrelated yes/no",
            "I changed three things.\n\n- The retry loop backs off now\n- The lock is released before the await\n\nWant me to run the tests?",
        ),
        (
            "summary of what was done, then a choice-like question",
            "Done.\n\n- Fixed the parser\n- Added two regression tests\n- Removed the dead branch\n\nWhich tests should run next?",
        ),
        ("question without a list", "Which approach do you prefer, the shim or the break?"),
        ("question then prose", "Which do you prefer?\n\nI think the shim is simpler, but it is your call."),
        (
            "list in a code fence",
            "Which do you prefer?\n\n```\n- Keep it\n- Drop it\n```",
        ),
        (
            "list in a tilde fence, question before",
            "Pick one.\n\n~~~md\n1. First\n2. Second\n~~~\n\nWhich one?",
        ),
        (
            "list in an indented code block",
            "Which do you prefer?\n\n    - Keep it\n    - Drop it",
        ),
        (
            "list in a block quote",
            "Which do you prefer?\n\n> - Keep it\n> - Drop it",
        ),
        (
            "nested list",
            "Which do you prefer?\n\n- Keep it\n  - for a month\n- Drop it",
        ),
        (
            "table of options",
            "Which do you prefer?\n\n| Option | Cost |\n|---|---|\n| Keep | low |\n| Drop | none |",
        ),
        (
            "zero-width space in an item",
            "Which do you prefer?\n\n- Keep it\n- Drop\u{200B} it",
        ),
        (
            "numbered steps with different verbs and an either-or question",
            "Here is the fix.\n\n1. Install the package\n2. Open the settings page\n3. Restart it\n\nShall I do it or will you?",
        ),
        (
            "bullet steps with a bare either-or question",
            "Here's the fix:\n- Install the package\n- Restart it\n\nShall I do it, or will you?",
        ),
        (
            "braille blank in an item",
            "Which do you prefer?\n\n- Keep it\n- Drop\u{2800} it",
        ),
        (
            "ogham space mark in an item",
            "Which do you prefer?\n\n- Keep\u{1680}it\n- Drop it",
        ),
        (
            "bidi override in an item",
            "Which do you prefer?\n\n- Keep it\n- \u{202E}ti pord",
        ),
        (
            "unicode tag characters hiding a payload",
            "Which do you prefer?\n\n- Keep it\u{E0049}\u{E0067}\n- Drop it",
        ),
        (
            "word joiner and BOM",
            "Which do you prefer?\n\n- Keep\u{2060} it\n- \u{FEFF}Drop it",
        ),
        (
            "zero-width entity",
            "Which do you prefer?\n\n- Keep it\n- Drop&#8203; it",
        ),
        (
            "markdown link in an item",
            "Which do you prefer?\n\n- [Keep it](https://example.com/keep)\n- Drop it",
        ),
        (
            "bare url in an item",
            "Which do you prefer?\n\n- Keep it, see https://example.com/keep\n- Drop it",
        ),
        (
            "autolink in an item",
            "Which do you prefer?\n\n- Keep it <https://example.com>\n- Drop it",
        ),
        (
            "inline html in an item",
            "Which do you prefer?\n\n- Keep <b>it</b>\n- Drop it",
        ),
        (
            "image in an item",
            "Which do you prefer?\n\n- ![diagram](x.png)\n- Drop it",
        ),
        (
            "one item is too long",
            "Which do you prefer?\n\n- Keep it\n- This option goes on and on about the migration, the shim, the callers and the release plan until it passes the limit of a single short line for a chip",
        ),
        ("one item", "Which do you prefer?\n\n- Keep it"),
        (
            "seven items",
            "Which do you prefer?\n\n- A\n- B\n- C\n- D\n- E\n- F\n- G",
        ),
        (
            "item spans two lines",
            "Which do you prefer?\n\n- Keep it\n  and tidy it up\n- Drop it",
        ),
        (
            "item holds two paragraphs",
            "Which do you prefer?\n\n- Keep it\n\n  Because it is old\n- Drop it",
        ),
        (
            "list is not last, prose follows",
            "Which do you prefer?\n\n- Keep it\n- Drop it\n\nLet me know when you have decided.",
        ),
        (
            "a question in the middle, list at the very end without one",
            "Which do you prefer? I will wait.\n\nHere is the background:\n\n- Keep it\n- Drop it",
        ),
        (
            "list ends the reply with no question",
            "The options are:\n\n- Keep it\n- Drop it",
        ),
        (
            "question ends with a colon",
            "Which do you prefer:\n\n- Keep it\n- Drop it",
        ),
        (
            "question is not adjacent",
            "Which do you prefer?\n\nHere is some background.\n\n- Keep it\n- Drop it",
        ),
        (
            "items are questions",
            "I need to know a few things. Which comes first?\n\n- What is the port?\n- Where is the config?",
        ),
        (
            "duplicate items",
            "Which do you prefer?\n\n- Keep it\n- keep it",
        ),
        (
            "yes/no with no choice cue",
            "Here are the files.\n\n- src/main.rs\n- src/lib.rs\n\nIs that everything you needed?",
        ),
        ("empty", ""),
        ("whitespace only", "  \n\n "),
        (
            "heading then list",
            "## Which do you prefer?\n\n- Keep it\n- Drop it",
        ),
        (
            "past-tense report with a choice-like question",
            "Which tests should run next?\n\n- Updated the fixtures\n- Reworked the helper",
        ),
    ];

    #[test]
    fn corpus_counts_hold() {
        assert!(TRUE_CASES.len() >= 12, "{}", TRUE_CASES.len());
        assert!(FALSE_CASES.len() >= 24, "{}", FALSE_CASES.len());
    }

    #[test]
    fn every_true_case_offers_exactly_its_options() {
        for (name, reply, want) in TRUE_CASES {
            let got = extract_or_why(reply)
                .unwrap_or_else(|why| panic!("{name}: expected chips, got skip: {why}"));
            let labels: Vec<&str> = got.options.iter().map(|o| o.label.as_str()).collect();
            assert_eq!(&labels, want, "{name}");
            assert!(!got.multi_select, "{name}");
        }
    }

    #[test]
    fn every_false_case_offers_nothing() {
        for (name, reply) in FALSE_CASES {
            if let Ok(q) = extract_or_why(reply) {
                let labels: Vec<_> = q.options.iter().map(|o| &o.label).collect();
                panic!("{name}: expected no chips, got {labels:?}");
            }
        }
    }

    #[test]
    fn skips_name_the_reason_without_the_text() {
        let why = |t: &str| extract_or_why(t).unwrap_err();
        assert_eq!(why("hello"), Skip::NoList);
        assert_eq!(why(""), Skip::Empty);
        assert_eq!(why("Which?\n\n- a"), Skip::ItemCount(1));
        assert_eq!(why("Which?\n\n- [a](http://x)\n- b"), Skip::ItemMarkup);
        assert_eq!(why("Which?\n\n- a\u{200B}\n- b"), Skip::ItemHiddenChar);
        assert_eq!(why("Run these.\n\n- a\n- b"), Skip::NoQuestion);
        assert_eq!(why("Which?\n\n1. Run it\n2. Open it"), Skip::Procedure);
        for skip in [Skip::ItemCount(7), Skip::NotAChoice, Skip::Duplicate] {
            assert!(!skip.to_string().is_empty());
        }
    }

    #[test]
    fn the_length_limit_is_120_characters_of_rendered_text() {
        let at_limit = "é".repeat(MAX_OPTION_CHARS);
        let over = "é".repeat(MAX_OPTION_CHARS + 1);
        let reply = |item: &str| format!("Which one?\n\n- {item}\n- b");
        assert_eq!(
            extract(&reply(&at_limit)).expect("at the limit").options[0].label,
            at_limit
        );
        assert_eq!(extract_or_why(&reply(&over)).unwrap_err(), Skip::ItemShape);
        // Markup does not count: the limit is on what the composer will hold.
        let bold = format!("**{at_limit}**");
        assert!(extract(&reply(&bold)).is_some());
    }

    #[test]
    fn huge_replies_are_not_read() {
        let big = format!("{}\n\nWhich?\n\n- a\n- b", "x ".repeat(MAX_REPLY_BYTES));
        assert_eq!(extract_or_why(&big).unwrap_err(), Skip::TooLong);
    }

    #[test]
    fn the_question_is_the_paragraph_and_the_id_is_stable() {
        let q = extract("Which one do you prefer?\n\n- a1\n- b2").expect("chips");
        assert_eq!(q.question, "Which one do you prefer?");
        assert_eq!(q.id, "reply");
    }
}
