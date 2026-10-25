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

/// Authorization schemes whose next word, after one or more blanks, is a credential.
const AUTH_SCHEMES: &[&str] = &["Bearer", "Basic", "Token"];

/// Parts of a variable or key name that mark its value as secret.
const SENSITIVE_NAME_PARTS: &[&str] = &[
    "KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "PASSPHRASE",
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
///
/// Known limits, each a credential this does not mask:
/// - a value under a name with no sensitive part (`DB_PASS=`, `pw:`, `auth=`);
/// - a value shorter than eight characters after an assignment or flag (`NAME: 1234567`);
/// - an unquoted `NAME=value` cut by a blank the shell would not see as one (`NAME=a b`,
///   unless escaped as `a\ b`), and a value that continues on the next line (YAML block
///   scalars `|`/`>`, a trailing `\`, JSON with an embedded newline): rules apply line by line;
/// - command-line credentials of programs other than curl, wget, mysql, mariadb, mysqldump and
///   sshpass, and of those programs behind an alias or a variable (`$CURL -u u:pw`);
/// - a secret split by a separator inside its own word (`-u u:a;b` masks only to the `;`);
/// - an authorization scheme other than Bearer, Basic and Token, and `Basic`/`Token` on a line
///   that never says "authorization";
/// - a YAML or INI value on a line that reads as code (it holds ` = ` or ends with `;`, `{` or
///   `,`), which is cut at its first blank like an `=` value (`NAME: two words,`);
/// - a flow mapping judged by brackets: an unmatched `{` or `[` in plain text before a key
///   (outside quotes) makes its value end at the next `,`, `}` or `]`.
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
            if !value.is_empty() && !is_masked(value) {
                edits.push((value_start, value_end, mask_any(value)));
            }
            from = value_end.max(start);
        }
    }
    (apply_edits(line, edits), open)
}

/// The shell words of `line` as byte spans, quote-aware (`'a b'` and `"a b"` stay one word, and
/// so do adjacent parts such as `'a'b"c"`). A backslash outside quotes or inside double quotes
/// escapes the next character (`a\ b` is one word); inside single quotes it is literal. The flag
/// is `true` when a quote or an escape is still open at the end of the line.
fn shell_word_spans(line: &str) -> (Vec<(usize, usize)>, bool) {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' && quote != Some('\'') {
            escaped = true;
            start.get_or_insert(i);
            continue;
        }
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
    (spans, quote.is_some() || escaped)
}

/// The value's range within `word` from `from`: the [`shell_value`] there, to the end of the
/// shell word.
fn value_range(word: &str, from: usize) -> (usize, usize) {
    shell_value(word.as_bytes(), from, |_| false)
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

#[cfg(test)]
thread_local! {
    /// Argument words [`redact_credential_options`] handed to the parser: the linearity check.
    static ARGS_SCANNED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Command words that end one command and start the next on the same line.
const COMMAND_SEPARATORS: &[&str] = &["|", "||", "&&", ";", "&"];

/// Masks credentials that command-line options carry ([`credential_args`]): only in the words
/// after a program that uses them (`curl`, `wget`, `mysql`/`mariadb`/`mysqldump`, `sshpass`),
/// up to the next command separator, so `docker -u 1000:1000` or `mkdir -p dir` are left alone. A
/// separator attached to the program's word (`true;curl`) still reveals the program.
/// Credential headers in any text are masked too ([`redact_header_values`]). Open (see
/// [`redact_line`]) when a header value runs to the end, or a credential program's command does
/// while a quote is open or an option still waits for its value word.
fn redact_credential_options(line: &str) -> (String, bool) {
    let (line, mut open) = redact_header_values(line);
    let (spans, quote_open) = shell_word_spans(&line);
    let words: Vec<&str> = spans.iter().map(|&(s, e)| &line[s..e]).collect();
    // Where the command whose arguments start at word `k` ends, for every `k`, in one backward
    // pass: a separator word ends it before itself, a word ending in one (`db;`) after itself.
    let mut command_end = vec![words.len(); words.len() + 1];
    for k in (0..words.len()).rev() {
        command_end[k] = if COMMAND_SEPARATORS.contains(&words[k]) {
            k
        } else if words[k].ends_with([';', '|', '&']) {
            k + 1
        } else {
            command_end[k + 1]
        };
    }
    // The parser judges each argument word on its own and the next word, so a command read from
    // its first `curl` already covers what a later `curl` in the same command would find: each
    // span is read once per program name, which keeps the line linear (`curl curl curl …`).
    let mut read: Vec<(&str, usize)> = Vec::new();
    let mut edits = Vec::new();
    for (p, program) in words.iter().enumerate() {
        // A separator attached to the program (`true;curl`, `a&&curl`, `$(curl`) starts its
        // command: the name is what follows the last one.
        let program = program
            .rsplit([';', '|', '&', '(', '`'])
            .next()
            .unwrap_or(program);
        let name = program.rsplit('/').next().unwrap_or(program);
        if !matches!(
            name,
            "curl" | "wget" | "mysql" | "mariadb" | "mysqldump" | "sshpass"
        ) {
            continue;
        }
        let end = command_end[p + 1];
        // Words of one command share its end and come together: a new end is a new command.
        if read.last().is_some_and(|&(_, e)| e != end) {
            read.clear();
        }
        if read.contains(&(name, end)) {
            continue;
        }
        read.push((name, end));
        #[cfg(test)]
        ARGS_SCANNED.with(|n| n.set(n.get() + (end - p - 1)));
        let (hits, past_end) = credential_args_reach(name, &words[p + 1..end]);
        open |= end == words.len() && (quote_open || past_end);
        for (i, s, e) in hits {
            let (word_start, _) = spans[p + 1 + i];
            let secret = &line[word_start + s..word_start + e];
            open |= word_start + e == line.len();
            if !is_masked(secret) {
                let (_, word_end) = spans[p + 1 + i];
                let word = &line[word_start..word_end];
                edits.push((word_start + s, word_start + e, shell_mask(word, 0, s, e)));
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
    if is_sensitive_name(key) && value.chars().count() >= MIN_SECRET_CHARS && !is_masked(value) {
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
            .filter(|scheme| !scheme.eq_ignore_ascii_case("bearer"))
            .any(|scheme| {
                let scheme = scheme.to_ascii_lowercase();
                let found = scheme_positions(&lower, &scheme).next().is_some();
                found
            });
    let out = AUTH_SCHEMES
        .iter()
        .filter(|scheme| about_auth || scheme.eq_ignore_ascii_case("bearer"))
        .fold(line.to_string(), |text, scheme| {
            let (text, o) = redact_after(&text, scheme);
            open |= o;
            text
        });
    (out, open)
}

/// Where `scheme` (lowercase) stands in `lower` followed by a blank (space or tab), as the byte
/// offset just past the scheme.
fn scheme_positions<'a>(lower: &'a str, scheme: &'a str) -> impl Iterator<Item = usize> + 'a {
    lower
        .match_indices(scheme)
        .map(move |(pos, _)| pos + scheme.len())
        .filter(|&end| matches!(lower.as_bytes().get(end), Some(b' ' | b'\t')))
}

/// Masks the word following each occurrence of the scheme word `marker` and one or more blanks
/// (spaces or tabs), ignoring ASCII case. Open when a value runs to the end of the line.
fn redact_after(line: &str, marker: &str) -> (String, bool) {
    // ASCII lower-casing keeps every byte where it was, so a position found in the lowered copy
    // is the same position, and a char boundary, in the original.
    let lower = line.to_ascii_lowercase();
    let marker = marker.to_ascii_lowercase();
    let bytes = line.as_bytes();
    let mut edits = Vec::new();
    let mut open = false;
    let mut from = 0;
    for after in scheme_positions(&lower, &marker) {
        if after < from {
            continue;
        }
        let value_start = after
            + bytes[after..]
                .iter()
                .take_while(|b| matches!(b, b' ' | b'\t'))
                .count();
        let value_len = bytes[value_start..]
            .iter()
            .take_while(|&&b| is_token_byte(b) || matches!(b, b'.' | b'=' | b'/' | b'+'))
            .count();
        let value_end = value_start + value_len;
        open |= value_end == line.len();
        if value_len >= MIN_SECRET_CHARS {
            edits.push((value_start, value_end, mask(&line[value_start..value_end])));
        }
        from = value_end;
    }
    (apply_edits(line, edits), open)
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
        let neutral = skip_blanks(bytes, name_end);
        let (value_start, value_end) = value_span(bytes, name_end, blank);
        open |= value_end == bytes.len();
        let value = &line[value_start..value_end];
        // A value starting with '-' is the next flag, not this one's argument.
        if value.chars().count() >= MIN_SECRET_CHARS && !is_masked(value) && !value.starts_with('-')
        {
            out.push_str(&line[copied..value_start]);
            out.push_str(&shell_mask(line, neutral, value_start, value_end));
            copied = value_end;
        }
        i = value_end.max(i + 1);
    }
    out.push_str(&line[copied..]);
    (out, open)
}

/// The value that starts at `start` in `bytes`, read as one shell word, as `(start, end)`. Every
/// path that reads a value goes through this one reader.
///
/// Quoted and unquoted parts that touch are joined (`'ab'cd`, `a"b c"`), as the shell joins them
/// into one argument. A backslash escapes the next byte outside quotes and inside double quotes
/// (`a\ b`, `"a\"b"`, as in the shell and JSON); inside single quotes it is literal, and `''`
/// continues the quote (YAML's escaped quote, the shell's join of two quoted strings). Outside
/// quotes the value ends at the first byte `stop` accepts; a quote in that set ends it too,
/// except as its very first byte, where it opens the value.
///
/// A value that is exactly one quoted part comes back without its quotes (`'a b'` → `a b`), as
/// does an unterminated one without its opening quote. Joined parts come back whole, quotes
/// and all. `end` is the length of `bytes` when the value runs to the end.
fn shell_value(bytes: &[u8], start: usize, stop: impl Fn(u8) -> bool) -> (usize, usize) {
    let starts_quoted = matches!(bytes.get(start), Some(b'"' | b'\''));
    let mut quote: Option<u8> = None;
    let mut first_close: Option<usize> = None;
    let mut at = start;
    while at < bytes.len() {
        let b = bytes[at];
        match quote {
            Some(b'"') if b == b'\\' => {
                // A trailing backslash leaves the value open.
                at = (at + 2).min(bytes.len());
                continue;
            }
            Some(b'\'') if b == b'\'' && bytes.get(at + 1) == Some(&b'\'') => {
                at += 2;
                continue;
            }
            Some(q) => {
                if b == q {
                    quote = None;
                    if starts_quoted && first_close.is_none() {
                        first_close = Some(at);
                    }
                }
                at += 1;
                continue;
            }
            None => {}
        }
        if b == b'\\' {
            at = (at + 2).min(bytes.len());
            continue;
        }
        let is_quote = matches!(b, b'"' | b'\'');
        if is_quote && (at == start || !stop(b)) {
            quote = Some(b);
            at += 1;
            continue;
        }
        if stop(b) {
            break;
        }
        at += 1;
    }
    if !starts_quoted {
        return (start, at);
    }
    match first_close {
        Some(close) if close + 1 == at => (start + 1, close),
        Some(_) => (start, at),
        None => (start + 1, at),
    }
}

/// The quote open after reading `bytes` as shell text from quote state `quote`, with the same
/// rules as [`shell_value`].
fn quote_state(bytes: &[u8], mut quote: Option<u8>) -> Option<u8> {
    let mut at = 0;
    while at < bytes.len() {
        let b = bytes[at];
        match quote {
            Some(b'"') if b == b'\\' => at += 1,
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if b == b'\\' => at += 1,
            None if matches!(b, b'"' | b'\'') => quote = Some(b),
            None => {}
        }
        at += 1;
    }
    quote
}

/// The mask for `text[s..e]`, a secret read with [`shell_value`] from the quote-neutral
/// position `from`. The visible tail never holds a quote or a backslash, and the quote state
/// the secret left is restored after it (`'user:ab'cdefgh` → `'user:****efgh'`), so the masked line
/// splits into the same shell words, reads the same again, and stays masked.
fn shell_mask(text: &str, from: usize, s: usize, e: usize) -> String {
    let bytes = text.as_bytes();
    let before = quote_state(&bytes[from..s], None);
    let after = quote_state(&bytes[s..e], before);
    let kept: String = text[s..e]
        .chars()
        .filter(|c| !matches!(c, '\'' | '"' | '\\'))
        .collect();
    // A secret that opens with its own quote (`"ab"cd`) keeps it, so it reads as quoted again.
    let lead = match (before, bytes.get(s)) {
        (None, Some(&q @ (b'"' | b'\''))) if s < e => Some(q),
        _ => None,
    };
    let inside = lead.or(before);
    let mut out: String = lead.map(char::from).into_iter().collect();
    out.push_str(&mask_any(&kept));
    if inside != after {
        out.extend(inside.into_iter().chain(after).map(char::from));
    }
    out
}

/// `start` moved past spaces and tabs.
fn skip_blanks(bytes: &[u8], mut start: usize) -> usize {
    while start < bytes.len() && matches!(bytes[start], b' ' | b'\t') {
        start += 1;
    }
    start
}

/// Where the value that follows `start` in `bytes` lies: past any spaces and tabs, the
/// [`shell_value`] there, ending (outside quotes) at the first byte `stop_unquoted` accepts.
fn value_span(bytes: &[u8], start: usize, stop_unquoted: impl Fn(u8) -> bool) -> (usize, usize) {
    shell_value(bytes, skip_blanks(bytes, start), stop_unquoted)
}

/// The value after `name:`. A quoted one is a [`value_span`] that ends at a blank, `,`, `}` or
/// `]` (`"k": "v", …`). An unquoted one runs to the end of the line or to a `#` that follows a
/// blank (a YAML/INI comment), trailing whitespace left out; in a flow collection (`flow`:
/// `{…}` or `[…]` open before the key), it also ends at `,`, `}` or `]`, and inside a quoted
/// string (`close`: the quote open at the value), at that string's closing quote.
fn yaml_value_span(bytes: &[u8], start: usize, flow: bool, close: Option<u8>) -> (usize, usize) {
    let value_start = skip_blanks(bytes, start);
    let first = bytes.get(value_start).copied();
    // The closing quote of the string the key sits in is not the start of a quoted value.
    if matches!(first, Some(b'"' | b'\'')) && first != close {
        // A later quote or `:` ends it, so a malformed `"k": "v, "next": "w"` never swallows
        // `next`'s value.
        return shell_value(bytes, value_start, |b| {
            matches!(b, b' ' | b'\t' | b',' | b'}' | b']' | b'"' | b'\'' | b':')
        });
    }
    let value_end = (value_start..bytes.len())
        .find(|&at| Some(bytes[at]) == close || (flow && matches!(bytes[at], b',' | b'}' | b']')))
        .unwrap_or(bytes.len());
    let mut end = (value_start..value_end)
        .find(|&at| bytes[at] == b'#' && matches!(bytes[at - 1], b' ' | b'\t'))
        .unwrap_or(value_end);
    while end > value_start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    (value_start, end)
}

/// `value` is exactly a mask: this module's `****` plus at most four characters (what [`mask`]
/// and [`mask_any`] write), or a PEM block's. Masking it again would only hide what it was. A
/// value that merely starts with one (`****abcdSUFFIX`, `****0123 correct horse`) still has
/// secret text after it and is masked. The one predicate every path uses.
fn is_masked(value: &str) -> bool {
    value == PEM_MASK || (value.starts_with("****") && value.chars().count() <= 8)
}

/// How deep in YAML flow collections (`{…}`, `[…]`) a line is, read left to right once.
/// Brackets inside a quoted span (`"x{password"`) do not count; the quote state is tracked in
/// the same pass (a backslash escapes inside double quotes).
#[derive(Default)]
struct FlowDepth {
    scanned: usize,
    depth: usize,
    quote: Option<u8>,
    escaped: bool,
}

impl FlowDepth {
    /// A flow collection is still open at byte `pos` of `bytes`.
    fn open_at(&mut self, bytes: &[u8], pos: usize) -> bool {
        self.advance(bytes, pos);
        self.depth > 0
    }

    /// The quote open at byte `pos` of `bytes`: the string an assignment sits in, if any.
    fn quote_at(&mut self, bytes: &[u8], pos: usize) -> Option<u8> {
        self.advance(bytes, pos);
        self.quote
    }

    /// Reads `bytes` up to `pos`. Positions are asked in increasing order, so the line is
    /// scanned once; an earlier one rescans from the start.
    fn advance(&mut self, bytes: &[u8], pos: usize) {
        if pos < self.scanned {
            *self = Self::default();
        }
        for &b in &bytes[self.scanned..pos] {
            if self.escaped {
                self.escaped = false;
                continue;
            }
            match (self.quote, b) {
                (Some(b'"'), b'\\') => self.escaped = true,
                (Some(q), b) if b == q => self.quote = None,
                (Some(_), _) => {}
                (None, b'"' | b'\'') => self.quote = Some(b),
                (None, b'{' | b'[') => self.depth += 1,
                (None, b'}' | b']') => self.depth = self.depth.saturating_sub(1),
                (None, _) => {}
            }
        }
        self.scanned = pos;
    }
}

/// The rest of a `name: value` line reads as code, not as a YAML or INI mapping: it holds ` = `
/// (`let name: &str = value;`, `name: str = None`) or ends with `;`, `{` or `,` (a statement, a
/// signature, a struct field).
fn looks_like_code(rest: &str) -> bool {
    let rest = rest.trim_end();
    rest.contains(" = ") || rest.ends_with([';', '{', ','])
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
    let mut flow = FlowDepth::default();
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

        // A path separator (`SecretKey::new`) assigns nothing.
        if bytes[i] == b':' && bytes.get(i + 1) == Some(&b':') {
            i += 2;
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
        // `name: value` (YAML, INI): an unquoted value runs to the end of the line or a ` #`
        // comment, so a passphrase with spaces is masked whole, unless the line reads as code
        // (`let key: &str = name;`). That is known only at the line's end, so until then the
        // line stays open. The `=` form keeps shell semantics, where a blank ends the value.
        let neutral = skip_blanks(bytes, value_start);
        let mut yaml = value_start == i + 1
            && bytes[i] == b':'
            && matches!(bytes.get(value_start), Some(b' ' | b'\t'));
        let in_flow = yaml && flow.open_at(bytes, name_start);
        // Inside a quoted string (`-H 'x-key: v' url`), the value ends at the string's close;
        // the line-long rule, and the code check it needs, apply only outside quotes.
        let outer = if yaml {
            flow.quote_at(bytes, neutral)
        } else {
            None
        };
        if yaml && !in_flow && outer.is_none() {
            open = true;
            yaml = !looks_like_code(&line[neutral..]);
        }
        // An unquoted YAML value is plain text to the line end; every other value is read as a
        // shell word.
        let shell_read = !yaml || matches!(bytes.get(neutral), Some(b'"' | b'\''));
        let (value_start, value_end) = if yaml {
            yaml_value_span(bytes, value_start, in_flow, outer)
        } else {
            // A whole shell word: quoted parts join (`abc"de f"`), and only a blank or a
            // separator ends it, or the closing quote of a string the assignment sits in
            // (`echo "NAME=…"`).
            let outer = flow.quote_at(bytes, neutral);
            value_span(bytes, value_start, |b| {
                matches!(b, b' ' | b'\t' | b',' | b';' | b'&') || Some(b) == outer
            })
        };
        open |= value_end == len || (yaml && line[value_end..].trim().is_empty());
        let value = &line[value_start..value_end];
        if value.chars().count() >= MIN_SECRET_CHARS && !is_masked(value) {
            out.push_str(&line[copied..value_start]);
            if shell_read {
                out.push_str(&shell_mask(line, neutral, value_start, value_end));
            } else {
                out.push_str(&mask(value));
            }
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

    /// Fixture markers and the words they stand for. A fixture is written with markers and
    /// assembled at run time, so no source line holds a credential-shaped assignment for a
    /// secret scanner to flag; the fixtures are fake either way.
    const MARKERS: &[(&str, &str)] = &[
        ("%pp%", concat!("pass", "phrase")),
        ("%PW%", concat!("PASS", "WORD")),
        ("%pw%", concat!("pass", "word")),
        ("%SC%", concat!("SEC", "RET")),
        ("%sc%", concat!("sec", "ret")),
        ("%TK%", concat!("TO", "KEN")),
        ("%tk%", concat!("to", "ken")),
        ("%KY%", concat!("K", "EY")),
        ("%ky%", concat!("k", "ey")),
        ("%BR%", concat!("Bea", "rer")),
        ("%br%", concat!("bea", "rer")),
        ("%cu%", concat!("cu", "rl")),
    ];

    /// `template` with every marker replaced by the word it stands for.
    pub(super) fn fx(template: &str) -> String {
        MARKERS
            .iter()
            .fold(template.to_owned(), |text, (marker, word)| {
                text.replace(marker, word)
            })
    }

    /// `text` with every marked word replaced by its marker: the inverse of [`fx`] on text with
    /// no markers in it.
    fn unfx(text: &str) -> String {
        MARKERS
            .iter()
            .fold(text.to_owned(), |text, (marker, word)| {
                text.replace(word, marker)
            })
    }

    #[test]
    fn markers_round_trip() {
        let text = "%pp% %PW% %pw% %SC%_%sc% %TK% %tk% API_%KY% x-%ky% %BR% %br% %cu%";
        assert_eq!(unfx(&fx(text)), text);
        assert!(!fx(text).contains('%'));
    }

    /// The shapes this module masks, written with [`fx`] markers.
    mod shapes {
        use super::super::*;
        use super::{fx, unfx};

        /// [`super::super::redact`] of a marked fixture, read back with markers.
        fn redact(text: &str) -> String {
            unfx(&super::super::redact(&fx(text)))
        }

        /// The spaced-separator trigger stays masked (`NAME = value`, `"k" : "v"`).
        #[test]
        fn spaced_separator_trigger_is_masked() {
            let out = redact("DB_%PW% = hunter2hunter2xyz");
            assert_eq!(out, "DB_%PW% = ****2xyz");
            let out = redact("\"api_%ky%\" : \"plainvalue123abc\"");
            assert_eq!(out, "\"api_%ky%\" : \"****3abc\"");
        }

        /// An unquoted YAML/INI value after `name: ` runs to the end of the line (or a ` #`
        /// comment), so a passphrase with spaces is masked whole, not cut at its first space.
        #[test]
        fn unquoted_yaml_passphrase_with_spaces_is_masked_whole() {
            for (text, expected) in [
                ("%pp%: correct horse battery staple", "%pp%: ****aple"),
                ("  db_%pw%: two words here  ", "  db_%pw%: ****here  "),
                (
                    "%sc%_%ky%: alpha beta gamma # rotated monthly",
                    "%sc%_%ky%: ****amma # rotated monthly",
                ),
                ("%pw%: hunter2 hunter2\r", "%pw%: ****ter2\r"),
            ] {
                assert_eq!(redact(text), expected, "{text:?}");
            }
            // The `=` form keeps shell semantics: the value ends at a blank.
            assert_eq!(
                redact("%PW%=hunter2hunter2 make deploy"),
                "%PW%=****ter2 make deploy"
            );
            // A path separator is not an assignment.
            let rust = "let k = SecretKey::new(); x";
            assert_eq!(redact(rust), rust);
        }

        /// A backslash-escaped quote inside a double-quoted value does not end it.
        #[test]
        fn escaped_quote_does_not_end_a_json_secret() {
            for (text, leaked) in [
                (
                    "{\"%pw%\": \"abc\\\"defghijklmnop\", \"x\": 1}",
                    "defghijklmnop",
                ),
                ("\"api_%ky%\":\"a\\\\\\\"bcdefghijklmn\"", "bcdefghijklmn"),
                ("tool --%pw% \"pa\\\"ss word here\" next", "ss word"),
            ] {
                let out = redact(text);
                assert!(
                    !out.contains(leaked),
                    "{leaked} leaked from {text:?}: {out}"
                );
            }
            // An escaped backslash before the closing quote ends the value there.
            assert_eq!(
                redact("{\"%pw%\": \"abcdefgh\\\\\", \"x\": 1}"),
                "{\"%pw%\": \"****efgh\", \"x\": 1}"
            );
        }

        /// Two or more blanks after `Bearer` do not hide the token.
        #[test]
        fn bearer_with_extra_spaces_is_masked() {
            for text in [
                "Authorization: %BR%  abcdefghijklmnop",
                "Authorization: %BR%   abcdefghijklmnop",
                "%br% \t abcdefghijklmnop rest",
                "Authorization: Basic   dXNlcjpodW50ZXIyaHVudGVy",
            ] {
                let out = redact(text);
                assert!(
                    !out.contains("abcdefghijkl") && !out.contains("dXNlcjpodW50"),
                    "{text:?}: {out}"
                );
            }
            assert_eq!(
                redact("Authorization: %BR%   abcdefghijklmnop rest"),
                "Authorization: %BR%   ****mnop rest"
            );
        }

        /// A separator attached to the program (`true;curl`) does not hide it.
        #[test]
        fn attached_separator_does_not_hide_a_credential_program() {
            for text in [
                "true;%cu% -u u:s3cr3tpass https://x",
                "true&&%cu% -u u:s3cr3tpass https://x",
                "false||%cu% --user u:s3cr3tpass https://x",
                "ls|%cu% -u u:s3cr3tpass https://x",
                "x=$(%cu% -u u:s3cr3tpass https://x)",
                "(%cu% -u u:s3cr3tpass https://x)",
                "true;/usr/bin/%cu% -u u:s3cr3tpass https://x",
                "true;mysql -pS3cr3tpass db",
            ] {
                let out = redact(text);
                assert!(!out.contains("3cr3tpass"), "{text:?}: {out}");
            }
            assert_eq!(
                redact("true;%cu% -u u:s3cr3tpass https://x"),
                "true;%cu% -u u:****pass https://x"
            );
            // Look-alikes after a separator stay readable.
            for text in ["true;docker run -u 1000:1000 img", "a;sort -u names.txt"] {
                assert_eq!(redact(text), text);
            }
        }

        /// Ordinary prose and short values are not over-masked by the new rules.
        #[test]
        fn new_rules_do_not_over_mask() {
            for text in [
                "%ky% = 1",
                "%ky%: 1",
                "Basic functionality works",
                "%pw%: 123",
                "%pw%: #",
                "%BR%  short",
                "if %pw% == other {",
                "the %ky% matters: a lot",
                "a;b|c&&d echo -u x:y",
            ] {
                assert_eq!(redact(text), text, "{text:?}");
            }
        }

        /// A backslash is literal inside shell single quotes: `'…\'` closes there, so the next
        /// assignment on the line is still found and masked.
        #[test]
        fn backslash_is_literal_in_single_quotes() {
            assert_eq!(
                redact("%PW%='abcdefgh\\' %TK%='hunter2hunter2'"),
                "%PW%='****efgh' %TK%='****ter2'"
            );
        }

        /// In a YAML flow mapping (`{…}` / `[…]`), an unquoted value ends at `,`, `}` or `]`; a
        /// short one stays readable and the next key is judged on its own.
        #[test]
        fn yaml_flow_mapping_value_stops_at_its_delimiter() {
            for text in [
                "{%pw%: 123, host: example.org}",
                "- [%tk%: 42, name: example.org]",
            ] {
                assert_eq!(redact(text), text, "{text:?}");
            }
            assert_eq!(
                redact("{%pw%: hunter2hunter2, host: example.org}"),
                "{%pw%: ****ter2, host: example.org}"
            );
            // A closed mapping before the key leaves block context.
            assert_eq!(
                redact("{a: 1} %pw%: two words here"),
                "{a: 1} %pw%: ****here"
            );
        }

        /// YAML's doubled single quote (`''`) does not end a single-quoted value.
        #[test]
        fn yaml_doubled_single_quote_does_not_end_a_value() {
            let out = redact("%pp%: 'it''s a very long %sc%'");
            assert!(!out.contains("very long"), "{out}");
            assert_eq!(out, "%pp%: '****cret'");
        }

        /// A masked token at the start of a line-long value does not exempt the rest of it.
        #[test]
        fn an_inner_mask_does_not_exempt_the_rest_of_a_line_long_value() {
            let out = redact(concat!(
                "%pw%: ghp_",
                "abcdefghijklmnopqrstuvwxyz0123 correct horse battery staple"
            ));
            assert!(!out.contains("horse"), "{out}");
            assert_eq!(out, "%pw%: ****aple");
        }

        /// A credential option's value is the whole shell word: quoted and unquoted parts joined,
        /// and a backslash-escaped blank kept inside it.
        #[test]
        fn credential_value_is_the_whole_shell_word() {
            for text in [
                "true;%cu% -u 'user:ab'cdefghijklmnop https://x",
                "%cu% -u user:\"ab\"cdefghijklmnop https://x",
                "%cu% -u user:abc\\ defghijklmnop https://x",
                "%cu% --user=user:'ab'cdefghijklmnop https://x",
                "sshpass -p 'ab'cdefghijklmnop ssh host",
            ] {
                let out = redact(text);
                assert!(!out.contains("defghijklm"), "{text:?}: {out}");
                assert!(
                    out.ends_with(" https://x") || out.ends_with(" ssh host"),
                    "{out}"
                );
            }
            // A plain quoted value still keeps its quotes.
            assert_eq!(
                redact("%cu% --user 'alice:two words here' https://x"),
                "%cu% --user 'alice:****here' https://x"
            );
        }

        /// A tab directly after the scheme does not hide the credential.
        #[test]
        fn a_tab_after_the_scheme_does_not_hide_the_credential() {
            for text in [
                "Authorization: %BR%\tabcdefghijklmnop",
                "Authorization: Basic\tabcdefghijklmnop",
                "%br%\t\tabcdefghijklmnop rest",
            ] {
                let out = redact(text);
                assert!(!out.contains("abcdefghijkl"), "{text:?}: {out}");
            }
            assert_eq!(
                redact("Authorization: %BR%\tabcdefghijklmnop rest"),
                "Authorization: %BR%\t****mnop rest"
            );
            // Prose is still left alone.
            let prose = "Basic\tfunctionality works";
            assert_eq!(redact(prose), prose);
        }

        /// Only a value that is exactly a mask (`****` plus at most four characters) is left alone,
        /// in every path: a mask-shaped prefix never exempts what follows it.
        #[test]
        fn a_mask_prefix_does_not_exempt_a_credential() {
            let out = redact("%cu% -u user:****abcdSUFFIX https://x");
            assert!(!out.contains("SUFFIX"), "{out}");
            assert_eq!(out, "%cu% -u user:****FFIX https://x");
            for text in [
                "tool --%pw% ****abcdSUFFIX next",
                "API_%KY%=****abcdSUFFIX next",
                "Cookie: ****abcdSUFFIX",
            ] {
                let out = redact(text);
                assert!(!out.contains("SUFFIX"), "{text:?}: {out}");
            }
            // The documented mask output itself stays as it is.
            for text in [
                "%cu% -u user:****abcd https://x",
                "tool --%pw% ****abcd next",
                "API_%KY%=****abcd next",
            ] {
                assert_eq!(redact(text), text);
            }
        }

        /// Every value is read as one shell word: escapes outside quotes and inside double quotes,
        /// joined quoted and unquoted parts, and literal single quotes.
        #[test]
        fn every_path_reads_a_value_as_one_shell_word() {
            for (text, leaked) in [
                ("%PW%=\"a\\\"bcdefghijklmnop", "bcdefghijklm"),
                ("%PW%=abc\\ defghijklmnop", "defghijklm"),
                ("tool --%pw% \"ab\"cdefghijklmnop next", "cdefghijklm"),
                ("tool --%pw% ab\\ cdefghijklmnop next", "cdefghijklm"),
                ("%PW%=\"ab\"cdefghijklmnop next", "cdefghijklm"),
            ] {
                let out = redact(text);
                assert!(!out.contains(leaked), "{text:?}: {out}");
            }
            assert_eq!(
                redact("tool --%pw% \"ab\"cdefghijklmnop next"),
                "tool --%pw% \"****mnop\" next"
            );
        }

        /// Brackets inside a quoted span do not open a YAML flow collection.
        #[test]
        fn quoted_brackets_do_not_open_a_flow_collection() {
            for text in [
                "\"x{%pw%\": correct,horse,battery,staple",
                "'[a' %pw%: correct,horse,battery,staple",
            ] {
                let out = redact(text);
                assert!(!out.contains("horse"), "{text:?}: {out}");
            }
            assert_eq!(
                redact("\"x{%pw%\": correct,horse,battery,staple"),
                "\"x{%pw%\": ****aple"
            );
            // A real flow mapping after a quoted bracket still is one.
            let text = "\"[\" {%pw%: 123, host: example.org}";
            assert_eq!(redact(text), text);
        }

        /// An `=` value is the whole shell word: a quoted part after unquoted text joins it. A quote
        /// that closes a string the assignment sits in still ends it.
        #[test]
        fn concatenated_quotes_join_an_assignment_value() {
            for (text, expected) in [
                ("%PW%=abcdefgh\"ijklmnop\"", "%PW%=****mnop"),
                ("%PW%=abcd'efgh ijkl'mnop next", "%PW%=****mnop next"),
                (
                    "export %TK%=ab\"cd ef\"ghijklmnop x",
                    "export %TK%=****mnop x",
                ),
            ] {
                assert_eq!(redact(text), expected, "{text:?}");
            }
            // Inside a quoted string, its closing quote ends the value.
            assert_eq!(
                redact("echo \"API_%KY%=super%sc%value1234\" done"),
                "echo \"API_%KY%=****1234\" done"
            );
            assert_eq!(
                redact("%cu% -d '%pw%=hunter2hunter2' https://x"),
                "%cu% -d '%pw%=****ter2' https://x"
            );
        }

        /// A `name: value` inside a quoted string ends at that string's closing quote: what
        /// follows the quote is never taken for the value.
        #[test]
        fn a_quoted_mapping_value_ends_at_its_closing_quote() {
            let text = concat!(
                "%cu% -H 'x-%ky%: sk-",
                "abcdefghijklmnopqrstuvwxyz0123456789' https://example.test"
            );
            assert_eq!(
                redact(text),
                "%cu% -H 'x-%ky%: ****6789' https://example.test"
            );
            assert_eq!(
                redact("echo \"%pw%: hunter2hunter2\" done"),
                "echo \"%pw%: ****ter2\" done"
            );
            // Spaces inside the string are still part of the value.
            assert_eq!(
                redact("echo '%pp%: correct horse battery' done"),
                "echo '%pp%: ****tery' done"
            );
        }

        /// Code is not a YAML mapping: a typed declaration keeps the short-value rule, as before.
        #[test]
        fn code_declarations_are_not_read_as_yaml() {
            for text in [
                "let %ky%: &str = name;",
                "fn f(%tk%: String) -> Result<(), E> {",
                "    let %sc%_%ky%: Vec<u8> = Vec::new();",
                "def login(%pw%: str = None) -> bool:",
            ] {
                assert_eq!(redact(text), text, "{text:?}");
            }
            // A struct field keeps what the short-value rule always did (masking a long type name),
            // never the line-long rule.
            assert_eq!(redact("    %pw%: Option<String>,"), "    %pw%: ****ing>,");
        }

        /// Many credential-program words on one line are parsed in linear time: each command span
        /// is read once per program name, not once per program word.
        #[test]
        fn many_program_words_are_scanned_linearly() {
            let line = "%cu% ".repeat(10_000);
            ARGS_SCANNED.with(|n| n.set(0));
            let started = std::time::Instant::now();
            assert_eq!(redact(&line), line);
            let scanned = ARGS_SCANNED.with(|n| n.get());
            assert!(scanned <= 2 * 10_000, "{scanned} words scanned");
            // Generous: a debug build takes a few milliseconds.
            assert!(started.elapsed() < std::time::Duration::from_secs(2));
            // Each program still sees its own options.
            let out = redact("sshpass -p hunter22%sc% %cu% -u u:s3cr3tpass https://x");
            assert!(
                !out.contains("hunter22") && !out.contains("3cr3tpass"),
                "{out}"
            );
        }

        /// `redact` is idempotent on its own output.
        #[test]
        fn redact_is_idempotent_on_its_output() {
            for text in [
                "%pp%: correct horse battery staple",
                "%sc%_%ky%: alpha beta gamma # rotated monthly",
                "{\"%pw%\": \"abc\\\"defghijklmnop\", \"x\": 1}",
                "Authorization: %BR%   abcdefghijklmnop rest",
                "true;%cu% -u u:s3cr3tpass https://x",
                "DB_%PW% = hunter2hunter2xyz",
                "API_%KY%=super%sc%value1234 next",
                "%cu% -H 'Cookie: session=abcdef123456' https://x",
                "%PW%='abcdefgh\\' %TK%='hunter2hunter2'",
                "{%pw%: hunter2hunter2, host: example.org}",
                "%pp%: 'it''s a very long %sc%'",
                concat!("%pw%: ghp_", "abcdefghijklmnopqrstuvwxyz0123 correct horse"),
                "true;%cu% -u 'user:ab'cdefghijklmnop https://x",
                "Authorization: %BR%\tabcdefghijklmnop rest",
                "%cu% -u user:****abcdSUFFIX https://x",
                "%PW%=\"a\\\"bcdefghijklmnop",
                "%PW%=abc\\ defghijklmnop x",
                "tool --%pw% \"ab\"cdefghijklmnop next",
                "\"x{%pw%\": correct,horse,battery,staple",
                "sshpass -p 'ab'cdefghijklmnop ssh host",
                "%cu% -u user:\"ab\"cdefghijklmnop https://x",
                "{\"api_%ky%\": \"abcdefgh12345678, \"%sc%\":\"open value with spaces",
                "%PW%=abcdefgh\"ijklmnop\"",
                "export %TK%=ab\"cd ef\"ghijklmnop x",
                "echo \"API_%KY%=super%sc%value1234\" done",
                "    %pw%: Option<String>,",
                "echo \"%pw%: hunter2hunter2\" done",
                "echo '%pp%: correct horse battery' done",
            ] {
                let once = redact(text);
                assert_eq!(redact(&once), once, "{text:?}");
            }
        }
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
