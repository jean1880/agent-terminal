//! Codex's command rules (`<codex_home>/rules/*.rules`): a fail-closed reader for the one
//! Starlark shape agent-terminal understands, and the writer for the file its export owns.
//!
//! Codex rules are Starlark calls, `prefix_rule(pattern=[...], decision="allow", ...)`, where a
//! pattern position may be a list of alternatives and a match is always a prefix match. The
//! reader understands exactly that: string literals (either quote, standard escapes), lists of
//! strings, keyword arguments in any order (the pattern may also come first, positionally), and
//! comments, plus `host_executable(name=..., paths=[...])`. A rule's inline `match` and
//! `not_match` examples are checked under prefix semantics, as Codex checks them when it loads
//! the file. Anything more (a variable, concatenation, another call, a raw or triple-quoted
//! string as a value, an unknown keyword) makes that call unreadable. A file the reader cannot
//! vouch for as a whole (an unterminated string, unbalanced brackets, an escape outside the
//! standard set, a character outside strings that is not Starlark, or one that could rebind
//! `prefix_rule`) is broken: nothing in it is used. This is conservative: Codex keeps an unknown
//! escape such as `\q` literally, the reader does not take the file. The reader parses text; it
//! never evaluates anything.
//!
//! Codex discards its whole rules policy when any file fails to load, so the caller treats an
//! unreadable call, an unread statement or a broken file as a reason to import nothing.
//!
//! The writer emits only printable-ASCII tokens through a dedicated escaper: an escape Starlark
//! rejects (`{:?}` writes `\u{…}`) would make Codex refuse every rule, the user's own included.
//! Every exported program is pinned with `host_executable`, so a program of the same name
//! elsewhere (a model-written `./cat`) never matches when Codex resolves host executables.

/// The one file export writes, beside the user's own rules.
pub const EXPORT_FILE: &str = "agent-terminal.rules";
/// The first line of [`EXPORT_FILE`].
pub const HEADER: &str = "# Written by agent-terminal. Edits here are replaced on the next export.";
/// The justification every exported rule carries.
pub const JUSTIFICATION: &str = "agent-terminal";
/// The most patterns one rule's alternatives may expand to; a rule beyond it is skipped.
pub const MAX_COMBINATIONS: usize = 16;

/// What a Codex rule decides for a command it matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Prompt,
    Forbidden,
}

/// One `prefix_rule` call, its alternatives expanded: every pattern is a plain argv prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixRule {
    pub patterns: Vec<Vec<String>>,
    pub decision: Decision,
}

/// One `host_executable` call: the absolute paths a bare program name may resolve to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostExecutable {
    pub name: String,
    pub paths: Vec<String>,
}

/// What a rules file holds, as far as the reader understands it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parsed {
    /// The rules read in full.
    pub rules: Vec<PrefixRule>,
    /// The `host_executable` calls read in full.
    pub hosts: Vec<HostExecutable>,
    /// `prefix_rule` calls that use more than the subset (or expand past
    /// [`MAX_COMBINATIONS`]), skipped.
    pub unreadable: usize,
    /// Other top-level statements, skipped.
    pub other: usize,
    /// The file cannot be read as a whole: `rules` is empty and `unreadable` counts its
    /// `prefix_rule` mentions (at least one).
    pub broken: bool,
}

/// Whether `token` may appear in an exported (or imported) rule: non-empty, printable ASCII
/// with no space (a word holding a space reads as two words to anyone checking the rule by
/// eye, and agent-terminal never relies on one).
pub fn plain_token(token: &str) -> bool {
    !token.is_empty() && token.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// Printable ASCII, spaces included (an example's words, which are never exported).
fn printable(text: &str) -> bool {
    text.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// `token` as a double-quoted Starlark string literal, or `None` when it is not a
/// [`plain_token`] (nothing else is ever written).
pub fn escape(token: &str) -> Option<String> {
    if !plain_token(token) {
        return None;
    }
    let mut out = String::with_capacity(token.len() + 2);
    out.push('"');
    for c in token.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    Some(out)
}

/// Whether `host` is one Codex accepts: a bare name, and absolute paths whose basename is that
/// name, all plain tokens.
fn valid_host(host: &HostExecutable) -> bool {
    plain_token(&host.name)
        && !host.name.contains('/')
        && !host.paths.is_empty()
        && host.paths.iter().all(|p| {
            plain_token(p) && p.starts_with('/') && p.rsplit('/').next() == Some(host.name.as_str())
        })
}

/// The whole of [`EXPORT_FILE`] for these argv prefixes: [`HEADER`], one
/// `host_executable(name=..., paths=[...])` per pinned program, then one
/// `prefix_rule(pattern=[...], decision="allow", justification="agent-terminal")` per rule. No
/// rules gives the header alone (so removed rules stop applying). `None` when a rule is empty,
/// holds a token [`escape`] refuses, or a host is not one Codex accepts.
pub fn render(rules: &[Vec<String>], hosts: &[HostExecutable]) -> Option<String> {
    let mut out = format!("{HEADER}\n");
    for host in hosts {
        if !valid_host(host) {
            return None;
        }
        let paths = host
            .paths
            .iter()
            .map(|p| escape(p))
            .collect::<Option<Vec<_>>>()?;
        out.push_str(&format!(
            "host_executable(name={}, paths=[{}])\n",
            escape(&host.name)?,
            paths.join(", ")
        ));
    }
    for rule in rules {
        if rule.is_empty() {
            return None;
        }
        let tokens = rule.iter().map(|t| escape(t)).collect::<Option<Vec<_>>>()?;
        out.push_str(&format!(
            "prefix_rule(pattern=[{}], decision=\"allow\", justification=\"{JUSTIFICATION}\")\n",
            tokens.join(", ")
        ));
    }
    Some(out)
}

// ---- the lexer ----

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Ident(String),
    Str(String),
    /// A raw, bytes or triple-quoted string: valid Starlark, never a value the reader takes.
    OtherStr,
    Open(char),
    Close(char),
    Comma,
    Eq,
    /// Any other operator character.
    Op,
}

/// One statement's tokens and whether it starts at column 0.
type Statement = (Vec<Tok>, bool);

/// The file's statements (split at newlines and `;` outside brackets), or `None` when Codex
/// could not parse it either.
fn statements(text: &str) -> Option<Vec<Statement>> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<Statement> = Vec::new();
    let mut current: Vec<Tok> = Vec::new();
    let mut current_col0 = false;
    let mut stack: Vec<char> = Vec::new();
    let mut i = 0;
    let mut at_line_start = true;
    let mut col0 = true;
    let flush = |current: &mut Vec<Tok>, col0: bool, out: &mut Vec<Statement>| {
        if !current.is_empty() {
            out.push((std::mem::take(current), col0));
        }
    };
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\n' => {
                if stack.is_empty() {
                    flush(&mut current, current_col0, &mut out);
                }
                at_line_start = true;
                col0 = true;
                i += 1;
                continue;
            }
            ' ' | '\t' | '\r' => {
                col0 = false;
                i += 1;
                continue;
            }
            '#' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            _ => {}
        }
        let line_start = at_line_start && col0;
        at_line_start = false;
        col0 = false;
        let lexed = match c {
            '"' | '\'' => {
                let (tok, next) = string(&chars, i, false)?;
                i = next;
                tok
            }
            '(' | '[' | '{' => {
                stack.push(c);
                i += 1;
                Tok::Open(c)
            }
            ')' | ']' | '}' => {
                let want = match c {
                    ')' => '(',
                    ']' => '[',
                    _ => '{',
                };
                if stack.pop() != Some(want) {
                    return None;
                }
                i += 1;
                Tok::Close(c)
            }
            ',' => {
                i += 1;
                Tok::Comma
            }
            '=' if chars.get(i + 1) != Some(&'=') => {
                i += 1;
                Tok::Eq
            }
            ';' if stack.is_empty() => {
                i += 1;
                flush(&mut current, current_col0, &mut out);
                // What follows on this line is a statement that does not start a line.
                continue;
            }
            c if c.is_ascii_alphanumeric() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let prefix = matches!(word.to_ascii_lowercase().as_str(), "r" | "b" | "rb" | "br");
                if prefix && matches!(chars.get(i), Some('"' | '\'')) {
                    let (_, next) = string(&chars, i, true)?;
                    i = next;
                    Tok::OtherStr
                } else {
                    Tok::Ident(word)
                }
            }
            c if c.is_ascii_punctuation() && c != '\\' => {
                i += 1;
                Tok::Op
            }
            // A backslash outside a string, a control or non-ASCII character: not Starlark the
            // reader can vouch for.
            _ => return None,
        };
        if current.is_empty() {
            current_col0 = line_start;
        }
        current.push(lexed);
    }
    if !stack.is_empty() {
        return None;
    }
    flush(&mut current, current_col0, &mut out);
    Some(out)
}

/// The string literal starting at `chars[start]` (a quote), and the index after it. `raw`: a
/// raw or bytes string, scanned without decoding. A triple-quoted string is [`Tok::OtherStr`].
/// `None` for an unterminated string or an escape Starlark rejects.
fn string(chars: &[char], start: usize, raw: bool) -> Option<(Tok, usize)> {
    let q = chars[start];
    let triple = chars.get(start + 1) == Some(&q) && chars.get(start + 2) == Some(&q);
    let mut i = start + if triple { 3 } else { 1 };
    let mut out = String::new();
    loop {
        let c = *chars.get(i)?;
        if triple {
            if c == q && chars.get(i + 1) == Some(&q) && chars.get(i + 2) == Some(&q) {
                return Some((Tok::OtherStr, i + 3));
            }
        } else if c == q {
            let tok = if raw { Tok::OtherStr } else { Tok::Str(out) };
            return Some((tok, i + 1));
        } else if c == '\n' {
            return None;
        }
        if c == '\\' {
            if raw {
                // The next character never ends the string, and is kept as it is.
                chars.get(i + 1)?;
                i += 2;
                continue;
            }
            let (decoded, next) = escape_at(chars, i + 1)?;
            if let Some(d) = decoded {
                out.push(d);
            }
            i = next;
            continue;
        }
        out.push(c);
        i += 1;
    }
}

/// The escape after a backslash at `chars[i]`: its character (`None` for a line continuation)
/// and the index after it. `None` for an escape Starlark rejects.
fn escape_at(chars: &[char], i: usize) -> Option<(Option<char>, usize)> {
    let c = *chars.get(i)?;
    let simple = match c {
        '\n' => return Some((None, i + 1)),
        'a' => '\x07',
        'b' => '\x08',
        'f' => '\x0c',
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        'v' => '\x0b',
        '\\' | '\'' | '"' => c,
        '0'..='7' => {
            let mut end = i;
            while end < chars.len() && end < i + 3 && ('0'..='7').contains(&chars[end]) {
                end += 1;
            }
            let digits: String = chars[i..end].iter().collect();
            let value = u32::from_str_radix(&digits, 8).ok()?;
            return Some((Some(char::from_u32(value)?), end));
        }
        'x' | 'u' | 'U' => {
            let len = match c {
                'x' => 2,
                'u' => 4,
                _ => 8,
            };
            let digits: String = chars.get(i + 1..i + 1 + len)?.iter().collect();
            if !digits.chars().all(|d| d.is_ascii_hexdigit()) {
                return None;
            }
            let value = u32::from_str_radix(&digits, 16).ok()?;
            return Some((Some(char::from_u32(value)?), i + 1 + len));
        }
        _ => return None,
    };
    Some((Some(simple), i + 1))
}

// ---- the parser ----

#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    Str(String),
    List(Vec<Value>),
}

/// Reads one rules file. See the module documentation for what it understands.
pub fn parse(text: &str) -> Parsed {
    let broken = |text: &str| Parsed {
        broken: true,
        unreadable: text.matches(PREFIX_RULE).count().max(1),
        ..Parsed::default()
    };
    let Some(statements) = statements(text) else {
        return broken(text);
    };
    let mut parsed = Parsed::default();
    for (toks, col0) in statements {
        let callee = match (toks.first(), toks.get(1)) {
            (Some(Tok::Ident(n)), Some(Tok::Open('('))) if col0 => Some(n.as_str()),
            _ => None,
        };
        match callee {
            Some(PREFIX_RULE) => match prefix_rule(&toks) {
                Some(rule) => parsed.rules.push(rule),
                None => parsed.unreadable += 1,
            },
            Some(HOST_EXECUTABLE) => match host_executable(&toks) {
                Some(host) => parsed.hosts.push(host),
                None => parsed.unreadable += 1,
            },
            _ => {
                // Anything else that names either call could rebind it, or call it where the
                // reader cannot see whether it runs (an indented block, a condition).
                let mentions = toks.iter().any(|t| {
                    matches!(t, Tok::Ident(n) | Tok::Str(n)
                        if n == PREFIX_RULE || n == HOST_EXECUTABLE)
                });
                if mentions {
                    return broken(text);
                }
                parsed.other += 1;
            }
        }
    }
    parsed
}

const PREFIX_RULE: &str = "prefix_rule";
const HOST_EXECUTABLE: &str = "host_executable";

/// A call's arguments (its tokens from the name on): keyword arguments named in `keywords`,
/// each at most once, after positional ones that take the names in `positional` in order.
/// `None` for anything else, an unknown keyword included (so a call has at most
/// `keywords.len()` arguments), or for anything after the closing parenthesis.
fn arguments(toks: &[Tok], keywords: &[&str], positional: &[&str]) -> Option<Vec<(String, Value)>> {
    let mut i = 2;
    let mut args: Vec<(String, Value)> = Vec::new();
    let mut by_position = true;
    loop {
        if toks.get(i)? == &Tok::Close(')') {
            i += 1;
            break;
        }
        let name = match (toks.get(i)?, toks.get(i + 1)) {
            (Tok::Ident(name), Some(Tok::Eq)) => {
                i += 2;
                by_position = false;
                keywords.iter().find(|k| **k == name)?.to_string()
            }
            _ if by_position => positional.get(args.len())?.to_string(),
            _ => return None,
        };
        if args.iter().any(|(n, _)| *n == name) {
            return None;
        }
        let (value, next) = value(toks, i, 0)?;
        i = next;
        args.push((name, value));
        match toks.get(i)? {
            Tok::Comma => i += 1,
            Tok::Close(')') => {}
            _ => return None,
        }
    }
    // Nothing may follow the call (`prefix_rule(...) if x else y`, a chained call).
    (i == toks.len()).then_some(args)
}

/// A `prefix_rule(...)` statement, or `None` when it uses more than the subset or one of its
/// examples does not hold.
fn prefix_rule(toks: &[Tok]) -> Option<PrefixRule> {
    let args = arguments(
        toks,
        &["pattern", "decision", "justification", "match", "not_match"],
        &["pattern"],
    )?;
    let mut pattern = None;
    let mut decision = Decision::Allow;
    let mut matches = Vec::new();
    let mut not_matches = Vec::new();
    for (name, value) in args {
        match name.as_str() {
            "pattern" => pattern = Some(positions(&value)?),
            "decision" => {
                decision = match value {
                    Value::Str(s) if s == "allow" => Decision::Allow,
                    Value::Str(s) if s == "prompt" => Decision::Prompt,
                    Value::Str(s) if s == "forbidden" => Decision::Forbidden,
                    _ => return None,
                }
            }
            "justification" => {
                if !matches!(value, Value::Str(_)) {
                    return None;
                }
            }
            "match" => matches = examples(value)?,
            "not_match" => not_matches = examples(value)?,
            _ => return None,
        }
    }
    let patterns = expand(&pattern?)?;
    // Codex refuses the file when an example does not hold; the reader refuses the call.
    let matched = |example: &Vec<String>| patterns.iter().any(|p| example.starts_with(p));
    if !matches.iter().all(matched) || not_matches.iter().any(matched) {
        return None;
    }
    Some(PrefixRule { patterns, decision })
}

/// A rule's examples: lists of words, or strings of plain words split at spaces (a string
/// that needs shell quoting to split is not one the reader can vouch for).
fn examples(value: Value) -> Option<Vec<Vec<String>>> {
    let Value::List(items) = value else {
        return None;
    };
    items
        .into_iter()
        .map(|item| match item {
            Value::Str(s) => {
                let plain = printable(&s) && !s.contains(['"', '\'', '\\']);
                let words: Vec<String> = s
                    .split(' ')
                    .filter(|w| !w.is_empty())
                    .map(str::to_owned)
                    .collect();
                (plain && !words.is_empty()).then_some(words)
            }
            Value::List(words) => words
                .into_iter()
                .map(|w| match w {
                    Value::Str(s) if !s.is_empty() && printable(&s) => Some(s),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
                .filter(|w| !w.is_empty()),
        })
        .collect()
}

/// A `host_executable(name, paths)` statement, or `None` unless Codex would accept it.
fn host_executable(toks: &[Tok]) -> Option<HostExecutable> {
    let args = arguments(toks, &["name", "paths"], &["name", "paths"])?;
    let mut name = None;
    let mut paths = None;
    for (key, value) in args {
        match (key.as_str(), value) {
            ("name", Value::Str(s)) => name = Some(s),
            ("paths", Value::List(items)) => {
                paths = Some(
                    items
                        .into_iter()
                        .map(|p| match p {
                            Value::Str(s) => Some(s),
                            Value::List(_) => None,
                        })
                        .collect::<Option<Vec<_>>>()?,
                )
            }
            _ => return None,
        }
    }
    let host = HostExecutable {
        name: name?,
        paths: paths?,
    };
    valid_host(&host).then_some(host)
}

/// The value starting at `toks[i]`: a string, or a list of values (lists nest at most twice).
fn value(toks: &[Tok], mut i: usize, depth: usize) -> Option<(Value, usize)> {
    match toks.get(i)? {
        Tok::Str(s) => Some((Value::Str(s.clone()), i + 1)),
        Tok::Open('[') if depth < 2 => {
            i += 1;
            let mut items = Vec::new();
            loop {
                if toks.get(i)? == &Tok::Close(']') {
                    return Some((Value::List(items), i + 1));
                }
                let (item, next) = value(toks, i, depth + 1)?;
                items.push(item);
                i = next;
                match toks.get(i)? {
                    Tok::Comma => i += 1,
                    Tok::Close(']') => {}
                    _ => return None,
                }
            }
        }
        _ => None,
    }
}

/// A pattern's positions, each a non-empty list of plain-token alternatives.
fn positions(pattern: &Value) -> Option<Vec<Vec<String>>> {
    let Value::List(items) = pattern else {
        return None;
    };
    if items.is_empty() {
        return None;
    }
    items
        .iter()
        .map(|item| {
            let alternatives = match item {
                Value::Str(s) => vec![s.clone()],
                Value::List(alts) if !alts.is_empty() => alts
                    .iter()
                    .map(|a| match a {
                        Value::Str(s) => Some(s.clone()),
                        Value::List(_) => None,
                    })
                    .collect::<Option<Vec<_>>>()?,
                Value::List(_) => return None,
            };
            alternatives
                .iter()
                .all(|t| plain_token(t))
                .then_some(alternatives)
        })
        .collect()
}

/// Every pattern the alternatives describe, or `None` past [`MAX_COMBINATIONS`].
fn expand(positions: &[Vec<String>]) -> Option<Vec<Vec<String>>> {
    let total = positions
        .iter()
        .try_fold(1usize, |n, alts| n.checked_mul(alts.len()))?;
    if total > MAX_COMBINATIONS {
        return None;
    }
    let mut out: Vec<Vec<String>> = vec![Vec::new()];
    for alts in positions {
        out = out
            .iter()
            .flat_map(|prefix| {
                alts.iter().map(move |a| {
                    let mut next = prefix.clone();
                    next.push(a.clone());
                    next
                })
            })
            .collect();
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(w: &[&str]) -> Vec<String> {
        w.iter().map(|s| (*s).to_owned()).collect()
    }

    fn allow(patterns: &[&[&str]]) -> PrefixRule {
        PrefixRule {
            patterns: patterns.iter().map(|p| words(p)).collect(),
            decision: Decision::Allow,
        }
    }

    #[test]
    fn good_rules_read_in_any_argument_order() {
        let text = r#"
# The user's own rules.
prefix_rule(pattern=["ls"], decision="allow", justification="read only")
prefix_rule(
    decision = "forbidden",   # never
    pattern = ['rm', "-rf"],
    match = [["rm", "-rf", "x"], "rm -rf y"],
    not_match = ["rm x"],
)
prefix_rule(["git", "status"])
prefix_rule(pattern=["cat"],)
prefix_rule(pattern=["head"], decision="prompt")
"#;
        let parsed = parse(text);
        assert!(!parsed.broken);
        assert_eq!(parsed.unreadable, 0);
        assert_eq!(parsed.other, 0);
        assert_eq!(
            parsed.rules,
            vec![
                allow(&[&["ls"]]),
                PrefixRule {
                    patterns: vec![words(&["rm", "-rf"])],
                    decision: Decision::Forbidden
                },
                allow(&[&["git", "status"]]),
                allow(&[&["cat"]]),
                PrefixRule {
                    patterns: vec![words(&["head"])],
                    decision: Decision::Prompt
                },
            ]
        );
    }

    #[test]
    fn alternatives_expand_up_to_the_cap() {
        let parsed = parse(r#"prefix_rule(pattern=["git", ["log", "show"], ["-p", "--stat"]])"#);
        assert_eq!(
            parsed.rules,
            vec![allow(&[
                &["git", "log", "-p"],
                &["git", "log", "--stat"],
                &["git", "show", "-p"],
                &["git", "show", "--stat"],
            ])]
        );
        // 4 × 4 = 16 is the cap; 17 or more is skipped.
        let at_cap = r#"prefix_rule(pattern=[["a","b","c","d"], ["e","f","g","h"]])"#;
        assert_eq!(parse(at_cap).rules[0].patterns.len(), 16);
        let over = r#"prefix_rule(pattern=[["a","b","c","d","x"], ["e","f","g","h"]])"#;
        assert_eq!(parse(over).unreadable, 1);
        assert!(parse(over).rules.is_empty());
        // A product that would overflow is refused without allocating it.
        let alts = format!("[{}]", vec!["\"a\""; 60].join(","));
        let huge = format!("prefix_rule(pattern=[{}])", vec![alts; 40].join(","));
        let parsed = parse(&huge);
        assert_eq!((parsed.rules.len(), parsed.unreadable), (0, 1));
    }

    #[test]
    fn every_unsupported_shape_skips_only_that_rule() {
        let skipped = [
            r#"prefix_rule(pattern=CMD)"#,
            r#"prefix_rule(pattern=["a" + "b"])"#,
            r#"prefix_rule(pattern=["a"] + ["b"])"#,
            r#"prefix_rule(pattern=[f"{x}"])"#,
            r#"prefix_rule(pattern=[r"raw"])"#,
            r#"prefix_rule(pattern=["""long"""])"#,
            r#"prefix_rule(pattern=[str(1)])"#,
            r#"prefix_rule(pattern=[])"#,
            r#"prefix_rule(pattern=[[]])"#,
            r#"prefix_rule(pattern=[""])"#,
            r#"prefix_rule(pattern=[["a", ["b"]]])"#,
            r#"prefix_rule(pattern=["a" "b"])"#,
            r#"prefix_rule(pattern=["ls"], decision="allowed")"#,
            r#"prefix_rule(pattern=["ls"], decision=ALLOW)"#,
            r#"prefix_rule(pattern=["ls"], pattern=["cat"])"#,
            r#"prefix_rule(["ls"], ["cat"])"#,
            r#"prefix_rule(decision="allow", ["ls"])"#,
            r#"prefix_rule(pattern=["ls"], host="x")"#,
            r#"prefix_rule(pattern=["ls"], justification=1)"#,
            r#"prefix_rule(pattern=["ls"], match="ls")"#,
            r#"prefix_rule(pattern=["ls"]) if True else None"#,
            r#"prefix_rule(pattern=["ls"]).x"#,
            r#"prefix_rule(pattern=["ls"]"#,
            r#"prefix_rule(pattern={"a": "b"})"#,
            r#"prefix_rule(pattern=["café"])"#,
            r#"prefix_rule(pattern=["a\tb"])"#,
            "prefix_rule(pattern=[\"\u{202e}\"])",
            "prefix_rule(pattern=[\"\\U0000202E\"])",
            r#"prefix_rule(pattern=[1])"#,
            // Examples Codex checks at load time, under prefix semantics.
            r#"prefix_rule(["cat"], not_match=["cat /etc/shadow"])"#,
            r#"prefix_rule(["cat"], match=["ls x"])"#,
            r#"prefix_rule(["cat"], match=[["cat"], ["ls"]])"#,
            r#"prefix_rule(["echo"], match=["echo \"a b\""])"#,
            r#"prefix_rule(["echo"], match=[""])"#,
            // Unknown keywords end the call at once.
            r#"prefix_rule(["ls"], x1=[], x2=[])"#,
            // host_executable as Codex would refuse it.
            r#"host_executable(name="cat", paths=["/usr/bin/ls"])"#,
            r#"host_executable(name="/usr/bin/cat", paths=["/usr/bin/cat"])"#,
            r#"host_executable(name="cat", paths=["cat"])"#,
            r#"host_executable(name="cat", paths=[])"#,
            r#"host_executable(name="cat")"#,
            r#"host_executable(name="cat", paths=["/bin/cat"], extra=1)"#,
        ];
        for rule in skipped {
            let text = format!("{rule}\nprefix_rule(pattern=[\"ok\"])\n");
            let parsed = parse(&text);
            if parsed.broken {
                // Unbalanced brackets make the whole file unreadable, which is stricter still.
                assert!(parsed.rules.is_empty(), "{rule}");
                continue;
            }
            assert_eq!(parsed.unreadable, 1, "{rule}: {parsed:?}");
            assert_eq!(parsed.rules, vec![allow(&[&["ok"]])], "{rule}");
        }
    }

    #[test]
    fn other_statements_are_counted_not_read() {
        let text = r#"
"""A docstring."""
X = ["ls"]
load("other.star", "helper")
host_executable(name="git", paths=["/usr/bin/git", "/usr/local/bin/git"])
host_executable("ls", ["/bin/ls"])
prefix_rule(pattern=["ls"], match=["ls -la", ["ls", "a b"]], not_match=["lsof"])
"#;
        let parsed = parse(text);
        assert!(!parsed.broken);
        assert_eq!(parsed.other, 3);
        assert_eq!(parsed.rules, vec![allow(&[&["ls"]])]);
        assert_eq!(
            parsed.hosts,
            vec![
                HostExecutable {
                    name: "git".into(),
                    paths: words(&["/usr/bin/git", "/usr/local/bin/git"])
                },
                HostExecutable {
                    name: "ls".into(),
                    paths: words(&["/bin/ls"])
                }
            ]
        );
        assert_eq!(parsed.unreadable, 0);
    }

    /// Hostile shapes: no panic, no quadratic blow-up, nothing read.
    #[test]
    fn hostile_shapes_never_panic() {
        let many_args = format!(
            "prefix_rule([\"ls\"], {})",
            (0..50_000)
                .map(|i| format!("x{i}=[]"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let started = std::time::Instant::now();
        let parsed = parse(&many_args);
        assert_eq!((parsed.rules.len(), parsed.unreadable), (0, 1));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        for text in [
            format!(
                "prefix_rule(pattern={}\"a\"{})",
                "[".repeat(100_000),
                "]".repeat(100_000)
            ),
            format!("{}prefix_rule([\"ls\"])", "(".repeat(10_000)),
            "prefix_rule([\"ls\"]) # ünïcödé comment is fine\n".to_owned(),
            "ünïcödé\nprefix_rule([\"ls\"])".to_owned(),
            "prefix_rule([\"ls\"], ✓=1)".to_owned(),
            "\0\0\0".to_owned(),
            "]]]]".to_owned(),
            "prefix_rule(".repeat(1000),
            "host_executable(name=\"a\", paths=[\"/a\"] * 100000)".to_owned(),
        ] {
            let parsed = parse(&text);
            assert!(parsed.rules.len() <= 1, "{parsed:?}");
        }
        assert_eq!(
            parse("prefix_rule([\"ls\"]) # ünïcödé comment is fine\n").rules,
            vec![allow(&[&["ls"]])]
        );
    }

    #[test]
    fn exported_hosts_render_and_read_back() {
        let hosts = vec![HostExecutable {
            name: "cat".into(),
            paths: words(&["/usr/bin/cat", "/usr/lib/cargo/bin/coreutils/cat"]),
        }];
        let text = render(&[words(&["cat"])], &hosts).expect("renders");
        assert!(text.contains(
            "host_executable(name=\"cat\", paths=[\"/usr/bin/cat\", \"/usr/lib/cargo/bin/coreutils/cat\"])"
        ));
        let parsed = parse(&text);
        assert_eq!(parsed.hosts, hosts);
        assert_eq!(parsed.rules, vec![allow(&[&["cat"]])]);
        for bad in [
            HostExecutable {
                name: "cat".into(),
                paths: words(&["/usr/bin/ls"]),
            },
            HostExecutable {
                name: "cat".into(),
                paths: words(&["relative/cat"]),
            },
            HostExecutable {
                name: "c/at".into(),
                paths: words(&["/c/at"]),
            },
            HostExecutable {
                name: "cat".into(),
                paths: vec![],
            },
        ] {
            assert_eq!(render(&[], std::slice::from_ref(&bad)), None, "{bad:?}");
        }
    }

    #[test]
    fn a_file_the_reader_cannot_vouch_for_or_that_rebinds_the_call_is_broken() {
        for text in [
            "prefix_rule(pattern=[\"a\\u{202e}\"])\nprefix_rule([\"ls\"])",
            "prefix_rule(pattern=[\"a\\q\"])\nprefix_rule([\"ls\"])",
            "prefix_rule(pattern=[\"unterminated])\nprefix_rule([\"ls\"])",
            "prefix_rule(pattern=[\"ls\")]\n",
            "prefix_rule(pattern=[\"ls\"]\n\nprefix_rule([\"cat\"])",
            "prefix_rule([\"ls\"]))\n",
            "x = 1 \\\n + 2\nprefix_rule([\"ls\"])",
            "prefix_rule = lambda **k: None\nprefix_rule([\"ls\"])",
            "def prefix_rule(**k):\n    pass\nprefix_rule([\"ls\"])",
            "if True:\n    prefix_rule([\"bash\"])\nprefix_rule([\"ls\"])",
            "  prefix_rule([\"bash\"])\nprefix_rule([\"ls\"])",
            "f = prefix_rule\nf([\"bash\"])",
            "h = host_executable\nprefix_rule([\"ls\"])",
            "  host_executable(name=\"ls\", paths=[\"/bin/ls\"])\nprefix_rule([\"ls\"])",
            // After `;` a call may sit in a one-line block (`if x: a(); prefix_rule(…)`).
            "prefix_rule([\"ls\"]); prefix_rule([\"cat\"])",
            "if False: pass; prefix_rule([\"bash\"])",
            "x = \"\u{7}\"; prefix_rule([\"ls\"]) ; y = [\nprefix_rule([\"ls\"])",
            "é = 1\nprefix_rule([\"ls\"])",
        ] {
            let parsed = parse(text);
            assert!(parsed.broken, "{text:?}: {parsed:?}");
            assert!(parsed.rules.is_empty(), "{text:?}");
            assert!(parsed.unreadable >= 1, "{text:?}");
        }
        assert!(!parse("").broken);
        assert_eq!(parse("").rules, vec![]);
        assert_eq!(parse("# only a comment\n").other, 0);
    }

    #[test]
    fn escapes_decode_like_starlark() {
        let parsed = parse(
            r#"prefix_rule(pattern=["a\"b", 'c\'d', "e\\f", "\x41\101B", 'it"s', "x\
y"])"#,
        );
        assert_eq!(
            parsed.rules,
            vec![allow(&[&["a\"b", "c'd", "e\\f", "AAB", "it\"s", "xy"]])],
            "{parsed:?}"
        );
        // Hostile input: huge, deeply nested, embedded NULs: no panic, nothing read.
        let deep = format!(
            "prefix_rule(pattern={}\"a\"{})",
            "[".repeat(5000),
            "]".repeat(5000)
        );
        let parsed = parse(&deep);
        assert!(parsed.rules.is_empty());
        let big = "prefix_rule(pattern=[\"ls\"])\n".repeat(20_000);
        assert_eq!(parse(&big).rules.len(), 20_000);
        assert!(parse("prefix_rule(pattern=[\"a\0b\"])").rules.is_empty());
        assert!(parse("\u{feff}prefix_rule([\"ls\"])").broken);
    }

    #[test]
    fn the_escaper_round_trips_and_refuses_what_starlark_cannot_hold() {
        let tokens = words(&[
            "plain",
            "a\"q\"b",
            "back\\slash",
            "'single'",
            "-x=1",
            "~/$HOME",
        ]);
        let quoted: Vec<String> = tokens.iter().map(|t| escape(t).expect(t)).collect();
        assert_eq!(quoted[1], r#""a\"q\"b""#);
        assert_eq!(quoted[2], r#""back\\slash""#);
        let text = render(std::slice::from_ref(&tokens), &[]).expect("renders");
        assert_eq!(
            parse(&text).rules,
            vec![allow(&[&tokens
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()])]
        );
        // SEC-09: a space never goes into a word, imported or exported.
        assert!(!plain_token("a b"));
        assert!(!plain_token(" "));
        assert!(parse(r#"prefix_rule(pattern=["echo", "a b"])"#)
            .rules
            .is_empty());
        for refused in [
            "",
            "a b",
            " lead",
            "new\nline",
            "tab\t",
            "bidi\u{202e}",
            "é",
            "\u{0}",
            "del\u{7f}",
        ] {
            assert_eq!(escape(refused), None, "{refused:?}");
            assert_eq!(
                render(&[words(&["echo", refused])], &[]),
                None,
                "{refused:?}"
            );
        }
        assert_eq!(render(&[vec![]], &[]), None);
    }

    #[test]
    fn an_empty_export_is_the_header_alone() {
        let text = render(&[], &[]).expect("renders");
        assert_eq!(text, format!("{HEADER}\n"));
        let parsed = parse(&text);
        assert_eq!(parsed, Parsed::default());
        let two = render(&[words(&["ls"]), words(&["git", "status"])], &[]).expect("renders");
        assert_eq!(
            two,
            format!(
                "{HEADER}\nprefix_rule(pattern=[\"ls\"], decision=\"allow\", justification=\"agent-terminal\")\n\
                 prefix_rule(pattern=[\"git\", \"status\"], decision=\"allow\", justification=\"agent-terminal\")\n"
            )
        );
    }
}
