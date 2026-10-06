//! Masking secrets in text headed for a file another program will read.
//!
//! Pure string logic: no I/O, no GTK. Every hand-off brief goes through
//! [`redact`] before it is written.

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
        let line = redact_prefixed_tokens(line);
        let line = redact_auth_schemes(&line);
        let line = redact_url_credentials(&line);
        let line = redact_flag_values(&line);
        out.push_str(&redact_assignments(&line));
    }
    out
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

fn redact_pem_blocks(text: &str) -> String {
    const BEGIN: &str = "-----BEGIN ";
    const END: &str = "-----END ";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(BEGIN) {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        // The block runs to the end of the END line's closing dashes; an
        // unterminated block is masked to the end of the text.
        let end = after
            .find(END)
            .and_then(|e| {
                let closing = after[e + END.len()..].find("-----")?;
                Some(e + END.len() + closing + "-----".len())
            })
            .unwrap_or(after.len());
        out.push_str("[REDACTED PEM BLOCK]");
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// Masks tokens that start with a known credential prefix. Every index used
/// to slice sits next to an ASCII byte, so it is always a char boundary.
fn redact_prefixed_tokens(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut copied = 0;
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
    out
}

/// Masks the credential after an authorization scheme, in any letter case.
///
/// `Bearer` is specific enough to act on anywhere. `Basic` and `Token` are
/// ordinary words ("Basic functionality"), so they only count on a line that
/// is about authorization.
fn redact_auth_schemes(line: &str) -> String {
    let about_auth = line.to_ascii_lowercase().contains("authorization");
    AUTH_SCHEMES
        .iter()
        .filter(|scheme| about_auth || scheme.eq_ignore_ascii_case("bearer "))
        .fold(line.to_string(), |text, scheme| redact_after(&text, scheme))
}

/// Masks the word following each occurrence of `marker`, ignoring ASCII case.
fn redact_after(line: &str, marker: &str) -> String {
    let marker = marker.to_ascii_lowercase();
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
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
    out
}

/// Masks the password in `scheme://user:password@host`.
fn redact_url_credentials(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(pos) = rest.find("://") {
        let authority_start = pos + "://".len();
        let authority = &rest[authority_start..];
        // The userinfo ends at '@', and only counts if that comes before the
        // path, query or the end of the word.
        let limit = authority
            .find(|c: char| c == '/' || c == '?' || c == '#' || c.is_whitespace())
            .unwrap_or(authority.len());
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
    out
}

/// Masks the value in `--password value` / `--api-token value`: the
/// space-separated flag form, which `redact_assignments` cannot see.
fn redact_flag_values(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut copied = 0;
    let mut i = 0;
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
        if name_end >= bytes.len()
            || !blank(bytes[name_end])
            || !is_sensitive_name(&line[name_start..name_end])
        {
            i = name_end.max(i + 1);
            continue;
        }
        let mut value_start = name_end;
        while value_start < bytes.len() && blank(bytes[value_start]) {
            value_start += 1;
        }
        // A quoted value runs to its closing quote, spaces and all.
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
                None => blank(b),
            };
            if stop {
                break;
            }
            value_end += 1;
        }
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
    out
}

fn is_sensitive_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SENSITIVE_NAME_PARTS.iter().any(|part| upper.contains(part))
}

/// Masks the value in `NAME=value`, `NAME: value` and `"name": "value"` when
/// the name looks like it holds a secret.
fn redact_assignments(line: &str) -> String {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(line.len());
    let mut copied = 0;
    let mut i = 0;
    while i < len {
        if bytes[i] != b'=' && bytes[i] != b':' {
            i += 1;
            continue;
        }
        let mut name_end = i;
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
        while value_start < len && bytes[value_start] == b' ' {
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
        while value_end < len {
            let b = bytes[value_end];
            let stop = match quote {
                Some(q) => b == q,
                None => matches!(b, b' ' | b'\t' | b',' | b';' | b'&' | b'"' | b'\''),
            };
            if stop {
                break;
            }
            value_end += 1;
        }
        let value = &line[value_start..value_end];
        if value.chars().count() >= MIN_SECRET_CHARS && !value.starts_with("****") {
            out.push_str(&line[copied..value_start]);
            out.push_str(&mask(value));
            copied = value_end;
        }
        i = value_end.max(i + 1);
    }
    out.push_str(&line[copied..]);
    out
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

    #[test]
    fn harmless_assignments_survive() {
        let text = "keyboard=us\nPATH=/usr/bin:/bin\nname: agent-terminal";
        assert_eq!(redact(text), text);
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
