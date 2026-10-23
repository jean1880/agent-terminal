//! Masking secrets in text headed for a file another program will read.
//!
//! Pure string logic: no I/O, no GTK. Every hand-off brief goes through
//! [`redact`] before it is written. Streamed text is published through
//! [`stream::StreamSanitiser`], which applies these same rules incrementally.

pub mod stream;

/// Prefixes of well-known credential formats. A token starting with one, and
/// long enough to be a real credential, is masked wherever it appears.
const TOKEN_PREFIXES: &[&str] = &[
    "sk-ant-",
    "sk-",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "github_pat_",
    "glpat-",
    "AIza",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xapp-",
    "hf_",
    // AWS access key IDs, long-term and temporary.
    "AKIA",
    "ASIA",
    // agent-terminal's own delegation capability (`delegation::token::CapabilityToken`).
    "atdc_",
];

/// Authorization schemes whose next word is a credential.
const AUTH_SCHEMES: &[&str] = &["Bearer ", "Basic ", "Token "];

/// Parts of a variable or key name that mark its value as secret.
const SENSITIVE_NAME_PARTS: &[&str] = &[
    "KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "PRIVATE",
    "CREDENTIAL",
];

/// A value shorter than this is left alone: real credentials are longer, and
/// masking `KEY=1` would only make the brief harder to read.
const MIN_SECRET_CHARS: usize = 8;

fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.'
}

/// `****` plus the last four characters, the masking convention.
fn mask(secret: &str) -> String {
    let tail: Vec<char> = secret.chars().rev().take(4).collect();
    format!("****{}", tail.into_iter().rev().collect::<String>())
}

/// Masks secrets in text headed for a file another program will read.
///
/// Pattern based, so its ceiling is known: a credential with no recognizable
/// prefix, not assigned to a sensitive-looking name, gets through. The brief
/// says so in its header. Upgrade path: entropy scoring of long tokens.
pub fn redact(text: &str) -> String {
    let without_pem = redact_pem_blocks(text);
    let mut out = String::with_capacity(without_pem.len());
    for (i, line) in without_pem.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&redact_line(line).0);
    }
    out
}

/// The per-line rules [`redact`] applies after PEM blocks are gone, in order. The flag is
/// `true` when some rule's decision reached the end of `line` (a token, value, quote, header
/// or option argument still open there), so text appended to the line could change the
/// output for bytes already in it. [`stream`] publishes a line's prefix only when it is
/// `false`; [`redact`] ignores it.
pub(crate) fn redact_line(line: &str) -> (String, bool) {
    let (line, mut open) = redact_prefixed_tokens(line);
    let (line, o) = redact_auth_schemes(&line);
    open |= o;
    let (line, o) = redact_url_credentials(&line);
    open |= o;
    let (line, o) = redact_flag_values(&line);
    open |= o;
    let (line, o) = redact_credential_options(&line);
    open |= o;
    let (line, o) = redact_assignments(&line);
    (line, open | o)
}

/// `mask`, but never revealing a short value whole: four characters or fewer become `****`.
fn mask_any(secret: &str) -> String {
    if secret.chars().count() <= 4 {
        "****".to_owned()
    } else {
        mask(secret)
    }
}

/// Header names whose value is a credential (compared lowercased). `authorization:` is left to
/// [`redact_auth_schemes`] in free text, and handled as a header in a command's arguments.
const CREDENTIAL_HEADERS: &[&str] = &["cookie:", "x-api-key:", "proxy-authorization:"];

/// Header names a command's header argument is judged by (lowercased).
const COMMAND_HEADERS: &[&str] = &[
    "authorization:",
    "proxy-authorization:",
    "cookie:",
    "x-api-key:",
];

/// Applies `(start, end, replacement)` edits (byte ranges in `line`) in order; overlapping
/// ones after the first are dropped.
fn apply_edits(line: &str, mut edits: Vec<(usize, usize, String)>) -> String {
    if edits.is_empty() {
        return line.to_owned();
    }
    edits.sort_by_key(|(s, _, _)| *s);
    let mut out = String::with_capacity(line.len());
    let mut copied = 0;
    for (s, e, replacement) in edits {
        if s < copied {
            continue;
        }
        out.push_str(&line[copied..s]);
        out.push_str(&replacement);
        copied = e;
    }
    out.push_str(&line[copied..]);
    out
}

/// The quote character open at byte `pos` of `line` (no escapes: `'` and `"` toggle).
fn open_quote_at(line: &str, pos: usize) -> Option<char> {
    let mut open: Option<char> = None;
    for c in line[..pos].chars() {
        match (open, c) {
            (None, '\'' | '"') => open = Some(c),
            (Some(q), c) if c == q => open = None,
            _ => {}
        }
    }
    open
}

/// Masks the value of a credential header (`Cookie: …`, `X-Api-Key: …`,
/// `Proxy-Authorization: …`): to the close of the quotes the header sits in (so `Cookie:
/// a="b c"` inside `'…'` is masked whole), else to the end of the line. The line is lowercased
/// once. Open (see [`redact_line`]) when a header's value runs to the end of the line.
fn redact_header_values(line: &str) -> (String, bool) {
    // ASCII lower-casing keeps byte positions, so offsets found there are char boundaries.
    let lower = line.to_ascii_lowercase();
    let mut edits = Vec::new();
    let mut open = false;
    for header in CREDENTIAL_HEADERS {
        let mut from = 0;
        while let Some(pos) = lower[from..].find(header) {
            let marker = from + pos;
            let start = marker + header.len();
            let value_start = start + line[start..].len() - line[start..].trim_start().len();
            let value_end = match open_quote_at(line, marker) {
                Some(q) => line[value_start..]
                    .find(q)
                    .map_or(line.len(), |at| value_start + at),
                None => line.len(),
            };
            open |= value_end == line.len();
            let value = &line[value_start..value_end];
            if !value.is_empty() && !value.starts_with("****") {
                edits.push((value_start, value_end, mask_any(value)));
            }
            from = value_end.max(start);
        }
    }
    (apply_edits(line, edits), open)
}

/// The shell words of `line` as byte spans, quote-aware (`'a b'` and `"a b"` stay one word; no
/// escapes). The flag is `true` when a quote is still open at the end of the line.
fn shell_word_spans(line: &str) -> (Vec<(usize, usize)>, bool) {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    let mut quote: Option<char> = None;
    for (i, c) in line.char_indices() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                ' ' | '\t' => {
                    if let Some(s) = start.take() {
                        spans.push((s, i));
                    }
                }
                '\'' | '"' => {
                    quote = Some(c);
                    start.get_or_insert(i);
                }
                _ => {
                    start.get_or_insert(i);
                }
            },
        }
    }
    if let Some(s) = start {
        spans.push((s, line.len()));
    }
    (spans, quote.is_some())
}

/// The bytes of `word` from `from` with one pair of surrounding quotes left out: the value's
/// range within `word`.
fn value_range(word: &str, from: usize) -> (usize, usize) {
    let rest = &word[from..];
    match rest.chars().next() {
        Some(q @ ('\'' | '"')) => {
            let start = from + 1;
            let end = word[start..].find(q).map_or(word.len(), |at| start + at);
            (start, end)
        }
        _ => (from, word.len()),
    }
}

/// curl's short options that take a value: in a cluster, the first of these takes the rest of
/// the word (or, at the end of the word, the next word).
const CURL_VALUE_LETTERS: &str = "bcCdDeEFHKmoPQrtTuUwxXyYz";

/// Where a credential sits in a command's arguments: for the program `program`, each
/// `(word index, start, end)` byte range (within that word as written, quotes allowed) of a
/// secret in `words`. Covers curl's `-u`/`-U`/`--user`/`--proxy-user` (the password after the
/// colon), `--oauth2-bearer` and a `-H`/`--header` naming Authorization, Proxy-Authorization,
/// Cookie or X-Api-Key (its value), short-option clusters included (`-su alice:pw`, `-vualice:pw`,
/// `-HCookie:…`); wget's password options; `mysql`/`mariadb`/`mysqldump -p<password>` and
/// `--password=`; and `sshpass -p`. The one parser behind both the live masking and the rule
/// engine's refusal of such commands.
pub fn credential_args(program: &str, words: &[&str]) -> Vec<(usize, usize, usize)> {
    credential_args_reach(program, words).0
}

/// [`credential_args`], plus whether an option wanted a value word past the last of `words`
/// (`curl -u` at the end): a word appended later would be a credential.
fn credential_args_reach(program: &str, words: &[&str]) -> (Vec<(usize, usize, usize)>, bool) {
    let program = program.rsplit('/').next().unwrap_or(program);
    let mut hits = Vec::new();
    let past_end = std::cell::Cell::new(false);
    let word = |i: usize| {
        let w = words.get(i);
        if w.is_none() {
            past_end.set(true);
        }
        w
    };
    // The password after the first ':' of the value at (word, from).
    let after_colon = |i: usize, from: usize, hits: &mut Vec<(usize, usize, usize)>| {
        let Some(w) = word(i) else { return };
        let (s, e) = value_range(w, from);
        if let Some(colon) = w[s..e].find(':') {
            let start = s + colon + 1;
            if start < e {
                hits.push((i, start, e));
            }
        }
    };
    let whole = |i: usize, from: usize, hits: &mut Vec<(usize, usize, usize)>| {
        let Some(w) = word(i) else { return };
        let (s, e) = value_range(w, from);
        if s < e {
            hits.push((i, s, e));
        }
    };
    let header = |i: usize, from: usize, hits: &mut Vec<(usize, usize, usize)>| {
        let Some(w) = word(i) else { return };
        let (s, e) = value_range(w, from);
        let value = &w[s..e];
        let lower = value.trim_start().to_ascii_lowercase();
        if COMMAND_HEADERS.iter().any(|h| lower.starts_with(h)) {
            if let Some(colon) = value.find(':') {
                let rest = &value[colon + 1..];
                let start = s + colon + 1 + (rest.len() - rest.trim_start().len());
                if start < e {
                    hits.push((i, start, e));
                }
            }
        }
    };
    for (i, w) in words.iter().enumerate() {
        match program {
            "curl" => match *w {
                "--user" | "--proxy-user" => after_colon(i + 1, 0, &mut hits),
                "--oauth2-bearer" => whole(i + 1, 0, &mut hits),
                "--header" => header(i + 1, 0, &mut hits),
                _ if w.starts_with("--user=") => after_colon(i, "--user=".len(), &mut hits),
                _ if w.starts_with("--proxy-user=") => {
                    after_colon(i, "--proxy-user=".len(), &mut hits)
                }
                _ if w.starts_with("--oauth2-bearer=") => {
                    whole(i, "--oauth2-bearer=".len(), &mut hits)
                }
                _ if w.starts_with("--header=") => header(i, "--header=".len(), &mut hits),
                _ if w.starts_with('-') && !w.starts_with("--") && w.len() > 1 => {
                    for (at, c) in w.char_indices().skip(1) {
                        if !CURL_VALUE_LETTERS.contains(c) {
                            continue;
                        }
                        let rest = at + c.len_utf8();
                        let (word, from) = if rest < w.len() {
                            (i, rest)
                        } else {
                            (i + 1, 0)
                        };
                        match c {
                            'u' | 'U' => after_colon(word, from, &mut hits),
                            'H' => header(word, from, &mut hits),
                            _ => {}
                        }
                        break;
                    }
                }
                _ => {}
            },
            "wget" => {
                for opt in [
                    "--password",
                    "--http-password",
                    "--ftp-password",
                    "--proxy-password",
                ] {
                    if *w == opt {
                        whole(i + 1, 0, &mut hits);
                    } else if let Some(rest) = w.strip_prefix(opt) {
                        if rest.starts_with('=') {
                            whole(i, opt.len() + 1, &mut hits);
                        }
                    }
                }
            }
            "mysql" | "mariadb" | "mysqldump" => {
                if w.starts_with("--password=") {
                    whole(i, "--password=".len(), &mut hits);
                } else if w.starts_with("-p") && w.len() > 2 {
                    whole(i, 2, &mut hits);
                }
            }
            "sshpass" => {
                if *w == "-p" {
                    whole(i + 1, 0, &mut hits);
                } else if w.starts_with("-p") && w.len() > 2 {
                    whole(i, 2, &mut hits);
                }
            }
            _ => {}
        }
    }
    (hits, past_end.get())
}

/// Command words that end one command and start the next on the same line.
const COMMAND_SEPARATORS: &[&str] = &["|", "||", "&&", ";", "&"];

/// Masks credentials that command-line options carry ([`credential_args`]): only in the words
/// after a program that uses them (`curl`, `wget`, `mysql`/`mariadb`/`mysqldump`, `sshpass`),
/// up to the next command separator, so `docker -u 1000:1000` or `mkdir -p dir` are left alone.
/// Credential headers in any text are masked too ([`redact_header_values`]). Open (see
/// [`redact_line`]) when a header value runs to the end, or a credential program's command does
/// while a quote is open or an option still waits for its value word.
fn redact_credential_options(line: &str) -> (String, bool) {
    let (line, mut open) = redact_header_values(line);
    let (spans, quote_open) = shell_word_spans(&line);
    let words: Vec<&str> = spans.iter().map(|&(s, e)| &line[s..e]).collect();
    let mut edits = Vec::new();
    for (p, program) in words.iter().enumerate() {
        let name = program.rsplit('/').next().unwrap_or(program);
        if !matches!(
            name,
            "curl" | "wget" | "mysql" | "mariadb" | "mysqldump" | "sshpass"
        ) {
            continue;
        }
        // A separator word ends the command; a word ending in one (`db;`) is its last word.
        let end = words[p + 1..]
            .iter()
            .enumerate()
            .find_map(|(at, w)| {
                if COMMAND_SEPARATORS.contains(w) {
                    Some(p + 1 + at)
                } else if w.ends_with([';', '|', '&']) {
                    Some(p + 2 + at)
                } else {
                    None
                }
            })
            .unwrap_or(words.len());
        let (hits, past_end) = credential_args_reach(name, &words[p + 1..end]);
        open |= end == words.len() && (quote_open || past_end);
        for (i, s, e) in hits {
            let (word_start, _) = spans[p + 1 + i];
            let secret = &line[word_start + s..word_start + e];
            open |= word_start + e == line.len();
            if !secret.starts_with("****") {
                edits.push((word_start + s, word_start + e, mask_any(secret)));
            }
        }
    }
    (apply_edits(&line, edits), open)
}

/// Masks a structured value whose key is known: a string under a sensitive
/// name (`api_key`, `client_secret`, …) is masked whole, whatever it looks like;
/// any other value goes through [`redact`]. Short values stay readable, as in
/// [`redact`].
///
/// Ceiling: only the key's own name is judged, so a secret under an innocuous
/// key still needs a recognizable shape. Upgrade path: entropy scoring.
pub fn redact_keyed(key: &str, value: &str) -> String {
    if is_sensitive_name(key)
        && value.chars().count() >= MIN_SECRET_CHARS
        && !value.starts_with("****")
    {
        mask(value)
    } else {
        redact(value)
    }
}

/// Opens a PEM block.
pub(crate) const PEM_BEGIN: &str = "-----BEGIN ";
/// Starts the line that closes a PEM block.
pub(crate) const PEM_END: &str = "-----END ";
/// Closes a PEM block: the first of these after [`PEM_END`].
pub(crate) const PEM_CLOSE: &str = "-----";
/// What a PEM block becomes.
pub(crate) const PEM_MASK: &str = "[REDACTED PEM BLOCK]";

fn redact_pem_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(PEM_BEGIN) {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        // The block runs to the end of the END line's closing dashes; an
        // unterminated block is masked to the end of the text.
        let end = after
            .find(PEM_END)
            .and_then(|e| {
                let closing = after[e + PEM_END.len()..].find(PEM_CLOSE)?;
                Some(e + PEM_END.len() + closing + PEM_CLOSE.len())
            })
            .unwrap_or(after.len());
        out.push_str(PEM_MASK);
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// Masks tokens that start with a known credential prefix. Every index used
/// to slice sits next to an ASCII byte, so it is always a char boundary. Open (see
/// [`redact_line`]) when a token runs to the end of the line.
fn redact_prefixed_tokens(line: &str) -> (String, bool) {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut copied = 0;
    let mut open = false;
    let mut i = 0;
    while i < bytes.len() {
        let at_token_start = is_token_byte(bytes[i]) && (i == 0 || !is_token_byte(bytes[i - 1]));
        if at_token_start {
            let mut end = i;
            while end < bytes.len() && is_token_byte(bytes[end]) {
                end += 1;
            }
            // A JWT is three dot-joined base64url parts; masking only the
            // first would leave its claims and signature in the brief.
            let is_jwt = line[i..end].starts_with("eyJ") && bytes.get(end) == Some(&b'.');
            if is_jwt {
                while end < bytes.len() && (is_token_byte(bytes[end]) || bytes[end] == b'.') {
                    end += 1;
                }
            }
            open |= end == bytes.len();
            let token = &line[i..end];
            let is_secret = (is_jwt && token.len() >= 20)
                || TOKEN_PREFIXES
                    .iter()
                    .any(|p| token.starts_with(p) && token.len() >= p.len() + 12);
            if is_secret {
                out.push_str(&line[copied..i]);
                out.push_str(&mask(token));
                copied = end;
            }
            i = end;
        } else {
            i += 1;
        }
    }
    out.push_str(&line[copied..]);
    (out, open)
}

/// Masks the credential after an authorization scheme, in any letter case.
///
/// `Bearer` is specific enough to act on anywhere. `Basic` and `Token` are
/// ordinary words ("Basic functionality"), so they only count on a line that
/// is about authorization. Open (see [`redact_line`]) when a scheme's value runs to the end,
/// or when `Basic`/`Token` appear on a line that does not mention authorization yet: the word
/// arriving later would bring them in.
fn redact_auth_schemes(line: &str) -> (String, bool) {
    let lower = line.to_ascii_lowercase();
    let about_auth = lower.contains("authorization");
    let mut open = !about_auth
        && AUTH_SCHEMES
            .iter()
            .filter(|scheme| !scheme.eq_ignore_ascii_case("bearer "))
            .any(|scheme| lower.contains(&scheme.to_ascii_lowercase()));
    let out = AUTH_SCHEMES
        .iter()
        .filter(|scheme| about_auth || scheme.eq_ignore_ascii_case("bearer "))
        .fold(line.to_string(), |text, scheme| {
            let (text, o) = redact_after(&text, scheme);
            open |= o;
            text
        });
    (out, open)
}

/// Masks the word following each occurrence of `marker`, ignoring ASCII case. Open when a
/// value runs to the end of the line.
fn redact_after(line: &str, marker: &str) -> (String, bool) {
    let marker = marker.to_ascii_lowercase();
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    let mut open = false;
    // ASCII lower-casing keeps every byte where it was, so a position found
    // in the lowered copy is the same position — and a char boundary — in
    // the original.
    while let Some(pos) = rest.to_ascii_lowercase().find(&marker) {
        let value_start = pos + marker.len();
        let value_len = rest[value_start..]
            .bytes()
            .take_while(|b| {
                is_token_byte(*b) || *b == b'.' || *b == b'=' || *b == b'/' || *b == b'+'
            })
            .count();
        open |= value_start + value_len == rest.len();
        let value = &rest[value_start..value_start + value_len];
        out.push_str(&rest[..value_start]);
        if value.len() >= MIN_SECRET_CHARS {
            out.push_str(&mask(value));
        } else {
            out.push_str(value);
        }
        rest = &rest[value_start + value_len..];
    }
    out.push_str(rest);
    (out, open)
}

/// Masks the password in `scheme://user:password@host`. Open (see [`redact_line`]) when an
/// authority runs to the end of the line.
fn redact_url_credentials(line: &str) -> (String, bool) {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    let mut open = false;
    while let Some(pos) = rest.find("://") {
        let authority_start = pos + "://".len();
        let authority = &rest[authority_start..];
        // The userinfo ends at '@', and only counts if that comes before the
        // path, query or the end of the word.
        let limit = authority
            .find(|c: char| c == '/' || c == '?' || c == '#' || c.is_whitespace())
            .unwrap_or(authority.len());
        open |= limit == authority.len();
        let masked = authority[..limit].rfind('@').and_then(|at| {
            let colon = authority[..at].find(':')?;
            let password = &authority[colon + 1..at];
            (!password.is_empty()).then(|| (colon + 1, at, mask(password)))
        });
        match masked {
            Some((start, end, replacement)) => {
                out.push_str(&rest[..authority_start + start]);
                out.push_str(&replacement);
                rest = &rest[authority_start + end..];
            }
            None => {
                out.push_str(&rest[..authority_start]);
                rest = &rest[authority_start..];
            }
        }
    }
    out.push_str(rest);
    (out, open)
}

/// Masks the value in `--password value` / `--api-token value`: the
/// space-separated flag form, which `redact_assignments` cannot see. Open (see
/// [`redact_line`]) when a flag's name, or a sensitive flag's value, runs to the end.
fn redact_flag_values(line: &str) -> (String, bool) {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut copied = 0;
    let mut i = 0;
    let mut open = false;
    let blank = |b: u8| b == b' ' || b == b'\t';
    while i + 2 < bytes.len() {
        let at_flag = bytes[i] == b'-' && bytes[i + 1] == b'-' && (i == 0 || blank(bytes[i - 1]));
        if !at_flag {
            i += 1;
            continue;
        }
        let name_start = i + 2;
        let mut name_end = name_start;
        while name_end < bytes.len() && is_name_byte(bytes[name_end]) {
            name_end += 1;
        }
        open |= name_end >= bytes.len();
        if name_end >= bytes.len()
            || !blank(bytes[name_end])
            || !is_sensitive_name(&line[name_start..name_end])
        {
            i = name_end.max(i + 1);
            continue;
        }
        let (value_start, value_end) = value_span(bytes, name_end, blank);
        open |= value_end == bytes.len();
        let value = &line[value_start..value_end];
        // A value starting with '-' is the next flag, not this one's argument.
        if value.chars().count() >= MIN_SECRET_CHARS
            && !value.starts_with("****")
            && !value.starts_with('-')
        {
            out.push_str(&line[copied..value_start]);
            out.push_str(&mask(value));
            copied = value_end;
        }
        i = value_end.max(i + 1);
    }
    out.push_str(&line[copied..]);
    (out, open)
}

/// Where the value that follows `start` in `bytes` lies, as `(start, end)`: past any spaces and
/// tabs, a quoted value runs to its closing quote (spaces and all, the quotes left out), an
/// unquoted one to the first byte `stop_unquoted` accepts. `end` is the line's length when the
/// value runs to the end.
fn value_span(bytes: &[u8], start: usize, stop_unquoted: impl Fn(u8) -> bool) -> (usize, usize) {
    let mut value_start = start;
    while value_start < bytes.len() && matches!(bytes[value_start], b' ' | b'\t') {
        value_start += 1;
    }
    let quote = match bytes.get(value_start) {
        Some(q @ (b'"' | b'\'')) => {
            value_start += 1;
            Some(*q)
        }
        _ => None,
    };
    let mut value_end = value_start;
    while value_end < bytes.len() {
        let b = bytes[value_end];
        let stop = match quote {
            Some(q) => b == q,
            None => stop_unquoted(b),
        };
        if stop {
            break;
        }
        value_end += 1;
    }
    (value_start, value_end)
}

fn is_sensitive_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SENSITIVE_NAME_PARTS.iter().any(|part| upper.contains(part))
}

/// Masks the value in `NAME=value`, `NAME: value` and `"name": "value"` when
/// the name looks like it holds a secret. Open (see [`redact_line`]) when such a value runs to
/// the end of the line.
fn redact_assignments(line: &str) -> (String, bool) {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(line.len());
    let mut copied = 0;
    let mut i = 0;
    let mut open = false;
    while i < len {
        if bytes[i] != b'=' && bytes[i] != b':' {
            i += 1;
            continue;
        }
        // Blanks may stand on either side of the separator (`NAME = value`, `"name" : "value"`):
        // a name is never missed for them, or its value would be kept whole.
        let mut name_end = i;
        while name_end > 0 && matches!(bytes[name_end - 1], b' ' | b'\t') {
            name_end -= 1;
        }
        if name_end > 0 && matches!(bytes[name_end - 1], b'"' | b'\'') {
            name_end -= 1;
        }
        let mut name_start = name_end;
        while name_start > 0 && is_name_byte(bytes[name_start - 1]) {
            name_start -= 1;
        }
        if !is_sensitive_name(&line[name_start..name_end]) {
            i += 1;
            continue;
        }

        let mut value_start = i + 1;
        // Two-byte separators: `NAME := value` (Go, make) and `'name' => 'value'` (PHP, Ruby,
        // Perl). Not `==`: a comparison assigns nothing.
        if matches!(
            (bytes[i], bytes.get(value_start)),
            (b':', Some(b'=')) | (b'=', Some(b'>'))
        ) {
            value_start += 1;
        }
        let (value_start, value_end) = value_span(bytes, value_start, |b| {
            matches!(b, b' ' | b'\t' | b',' | b';' | b'&' | b'"' | b'\'')
        });
        open |= value_end == len;
        let value = &line[value_start..value_end];
        if value.chars().count() >= MIN_SECRET_CHARS && !value.starts_with("****") {
            out.push_str(&line[copied..value_start]);
            out.push_str(&mask(value));
            copied = value_end;
        }
        i = value_end.max(i + 1);
    }
    out.push_str(&line[copied..]);
    (out, open)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixed_tokens_are_masked_to_their_last_four() {
        let text = "export GH=ghp_abcdefghijklmnopqrstuvwxyz0123 and key AIzaSyA1234567890abcdefgh";
        let out = redact(text);
        assert!(!out.contains("ghp_abcdefghij"), "{out}");
        assert!(out.contains("****0123"), "{out}");
        assert!(!out.contains("AIzaSyA123"), "{out}");
        assert!(out.contains("****efgh"), "{out}");
    }

    /// Credentials carried by command-line options and headers are masked in live text.
    #[test]
    fn credential_options_and_headers_are_masked() {
        for (text, leaked) in [
            ("curl -u alice:s3cr3tpass https://x", "s3cr3tpass"),
            ("curl --user alice:s3cr3tpass https://x", "s3cr3tpass"),
            ("curl --user=alice:s3cr3tpass https://x", "s3cr3tpass"),
            ("curl -ualice:s3cr3tpass https://x", "s3cr3tpass"),
            ("curl --proxy-user bob:pr0xypass1 https://x", "pr0xypass1"),
            (
                "curl --oauth2-bearer tok123456789abc https://x",
                "tok123456789abc",
            ),
            (
                "curl -H 'Cookie: session=abcdef123456' https://x",
                "abcdef123456",
            ),
            (
                "curl -H \"X-Api-Key: k1234567890\" https://x",
                "k1234567890",
            ),
            (
                "curl --header 'Proxy-Authorization: Basic Zm9vOmJhcg==' https://x",
                "Zm9vOmJhcg",
            ),
            ("mysql -u root -pHunter2pass db", "Hunter2pass"),
            ("mariadb -pHunter2pass", "Hunter2pass"),
            ("sshpass -p hunter22secret ssh host", "hunter22secret"),
            ("wget --http-password=topsecret1 https://x", "topsecret1"),
            ("wget --password topsecret1 https://x", "topsecret1"),
            ("psql postgres://app:dbpass99@db/app", "dbpass99"),
        ] {
            let out = redact(text);
            assert!(!out.contains(leaked), "{text} leaked: {out}");
            assert!(out.contains("****"), "{out}");
        }
        // Short passwords are never shown whole.
        assert!(!redact("curl -u a:pw1 https://x").contains("pw1"));
        // The offset comes from the colon, so a password equal to the user name goes too.
        for (text, kept) in [
            ("curl -u admin:admin https://x", "curl -u admin:****"),
            ("curl -uroot:root https://x", "curl -uroot:****"),
            ("curl --user=a:a https://x", "curl --user=a:****"),
        ] {
            let out = redact(text);
            assert!(out.starts_with(kept), "{text}: {out}");
        }
        // Quoted values with spaces are masked whole.
        let out = redact("curl --user 'alice:two words here' https://x");
        assert!(!out.contains("two") && !out.contains("words"), "{out}");
        let out = redact("curl -H 'Cookie: a=\"b c\"; sid=SESSIONVALUE99' https://x");
        assert!(!out.contains("b c") && !out.contains("SESSIONVAL"), "{out}");
        // Curl short-option clusters.
        for text in [
            "curl -su alice:DEMOPASS1 https://x",
            "curl -vualice:DEMOPASS1 https://x",
            "curl -sHCookie:sid=DEMOPASS1 https://x",
        ] {
            assert!(!redact(text).contains("DEMOPASS1"), "{text}");
        }
        // Look-alikes stay readable.
        // `-u` and `-p` mean credentials only for the programs that use them that way.
        for text in [
            "sort -u names.txt",
            "mkdir -p build/out",
            "git log -p",
            "grep -u: x",
            "curl -H 'Accept: text/html' https://x",
            "docker run -u 1000:1000 img",
            "id -u root:root",
            "mysql -u root db; mkdir -pall",
            "curl -s https://x | grep -u root:root",
        ] {
            assert_eq!(redact(text), text);
        }
        // The shared parser, as the rule engine sees an argv.
        assert_eq!(credential_args("curl", &["-su", "alice:pw"]), [(1, 6, 8)]);
        assert!(credential_args("docker", &["-u", "1000:1000"]).is_empty());
        assert_eq!(
            credential_args("/usr/bin/sshpass", &["-p", "pw"]),
            [(1, 0, 2)]
        );
    }

    #[test]
    fn short_or_unprefixed_words_are_left_alone() {
        let text = "sk-learn is a library; ghp_ is a prefix; the task-list works";
        assert_eq!(redact(text), text);
    }

    #[test]
    fn sensitive_assignments_are_masked_in_every_common_shape() {
        let out = redact(
            "API_KEY=supersecretvalue1234\n\
             \"db_password\": \"hunter2hunter2\"\n\
             client_secret: abcdefgh5678\n\
             https://x.test/cb?token=zzzzzzzzqqqq&state=ok",
        );
        for leaked in [
            "supersecret",
            "hunter2hunter2",
            "abcdefgh5678",
            "zzzzzzzzqqqq",
        ] {
            assert!(!out.contains(leaked), "{leaked} leaked: {out}");
        }
        assert!(out.contains("API_KEY=****1234"), "{out}");
        assert!(out.contains("\"db_password\": \"****ter2\""), "{out}");
        assert!(out.contains("&state=ok"), "{out}");
    }

    #[test]
    fn keyed_values_are_masked_by_their_key_alone() {
        assert_eq!(redact_keyed("api_key", "plain-secret-9981"), "****9981");
        assert_eq!(redact_keyed("Password", "hunter2hunter2"), "****ter2");
        // Short values, harmless keys and prior masks are left alone.
        assert_eq!(redact_keyed("api_key", "short"), "short");
        assert_eq!(redact_keyed("name", "plain-value-123"), "plain-value-123");
        assert_eq!(redact_keyed("api_key", "****9981"), "****9981");
        // A harmless key still gets pattern redaction.
        assert!(redact_keyed("note", "ghp_abcdefghijklmnopqrstuvwxyz0123").starts_with("****"));
    }

    /// Blanks around the separator never hide a secret: each of these leaked whole before.
    #[test]
    fn spaced_assignments_are_masked_too() {
        for (text, leaked, kept) in [
            (
                "DB_PASSWORD = hunter2hunter2xyz",
                "hunter2hunter2",
                "DB_PASSWORD = ****2xyz",
            ),
            (
                "DB_PASSWORD =hunter2hunter2xyz",
                "hunter2hunter2",
                "DB_PASSWORD =****2xyz",
            ),
            (
                "DB_PASSWORD\t=\thunter2hunter2xyz",
                "hunter2hunter2",
                "DB_PASSWORD\t=\t****2xyz",
            ),
            (
                "api_key : \"plainvalue123abc\"",
                "plainvalue123",
                "api_key : \"****3abc\"",
            ),
            (
                "\"api_key\" : \"plainvalue123abc\"",
                "plainvalue123",
                "\"api_key\" : \"****3abc\"",
            ),
            // Low-entropy values: a detector-shaped fixture trips the secret scanners.
            (
                "export SECRET_TOKEN = tokentokentoken1",
                "tokentokentoken",
                "SECRET_TOKEN = ****ken1",
            ),
            (
                "client_secret  :  secretsecret5678",
                "secretsecret",
                "client_secret  :  ****5678",
            ),
            (
                "DB_PASSWORD := hunter2hunter2xyz",
                "hunter2hunter2",
                "DB_PASSWORD := ****2xyz",
            ),
            (
                "'password' => 'hunter2hunter2xyz'",
                "hunter2hunter2",
                "'password' => '****2xyz'",
            ),
            (
                "password=>hunter2hunter2xyz",
                "hunter2hunter2",
                "password=>****2xyz",
            ),
        ] {
            let out = redact(text);
            assert!(
                !out.contains(leaked),
                "{leaked} leaked from {text:?}: {out}"
            );
            assert!(out.contains(kept), "{text:?} -> {out}");
        }
    }

    #[test]
    fn harmless_assignments_survive() {
        let text = "keyboard=us\nPATH=/usr/bin:/bin\nname: agent-terminal";
        assert_eq!(redact(text), text);
        // Spaced, a harmless name or a comparison is still left alone.
        let spaced = "keyboard = us\nname : agent-terminal\nif password == other {";
        assert_eq!(redact(spaced), spaced);
    }

    #[test]
    fn bearer_tokens_and_pem_blocks_are_masked() {
        // A fake key, not a credential: the fixture the masking is tested on.
        let pem = "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAAB3NzaC1yc2E\n-----END OPENSSH PRIVATE KEY-----"; // gitleaks:allow
        let out = redact(&format!(
            "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.payload.sig\n{pem}\nafter"
        ));
        assert!(!out.contains("eyJhbGci"), "{out}");
        assert!(!out.contains("AAAAB3"), "{out}");
        assert!(out.contains("[REDACTED PEM BLOCK]\nafter"), "{out}");
    }

    #[test]
    fn the_formats_the_first_pass_missed_are_masked() {
        // Fake values in real shapes; none is a live credential. The AWS key
        // is AWS's own documentation example, held apart so the scanner's
        // allow marker can sit on its line.
        let aws_example = "AKIAIOSFODNN7EXAMPLE"; // gitleaks:allow
        let out = redact(&format!(
            "aws {aws_example} here\n\
             jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NSJ9.c2lnbmF0dXJlLXZhbHVl done\n\
             Authorization: Basic dXNlcjpodW50ZXIyaHVudGVy\n\
             git clone https://deploy:s3cretPassw0rd@git.example.com/repo.git\n\
             tool --password correcthorse --verbose --token --dry-run",
        ));
        for leaked in [
            "IOSFODNN7",
            "eyJzdWIi",
            "c2lnbmF0dXJl",
            "dXNlcjpodW50",
            "s3cretPassw0rd",
            "correcthorse",
        ] {
            assert!(!out.contains(leaked), "{leaked} leaked: {out}");
        }
        // What surrounds a secret is left readable.
        assert!(out.contains("https://deploy:****"), "{out}");
        assert!(out.contains("@git.example.com/repo.git"), "{out}");
        assert!(out.contains("--verbose --token --dry-run"), "{out}");
        assert!(out.contains(" done"), "{out}");
    }

    #[test]
    fn scheme_case_quotes_and_tabs_do_not_hide_a_secret() {
        let out = redact(
            "authorization: bearer abcdefghijklmnop\n\
             run\t--password \"correct horse battery\" next\n\
             run --api-token 'staple-staple-staple'",
        );
        for leaked in ["abcdefghijkl", "correct horse", "staple-staple"] {
            assert!(!out.contains(leaked), "{leaked} leaked: {out}");
        }
        assert!(out.contains("\" next"), "{out}");
    }

    #[test]
    fn scheme_words_in_ordinary_prose_are_left_alone() {
        let text = "Basic functionality works. Token budgeting matters.";
        assert_eq!(redact(text), text);
    }

    #[test]
    fn urls_without_credentials_are_untouched() {
        let text = "see https://example.com/a:b@c and mailto:user@example.com";
        assert_eq!(redact(text), text);
    }

    #[test]
    fn redaction_never_splits_a_multibyte_character() {
        let text = "café TOKEN=ééééééééé€ ghp_ßßßßßßßßßßßß 日本語 password: ünïcödé!!";
        let out = redact(text);
        assert!(out.contains("café"));
        assert!(out.contains("日本語"));
        assert!(!out.contains("ééééééééé€"));
    }
}
