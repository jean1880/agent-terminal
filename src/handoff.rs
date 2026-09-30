//! Handing a task from one CLI to another, and noticing when one runs out.
//!
//! When a CLI hits its quota mid-task it cannot summarize its own work, so the
//! terminal writes the brief itself, from what is on disk: the session's
//! transcript and the working tree. The receiving CLI starts with a prompt
//! pointing at that file.
//!
//! Pure logic and blocking file I/O only — nothing here touches GTK, so all of
//! it is unit-tested without a display. Callers run the blocking parts through
//! `gio::spawn_blocking`.

use crate::config::SessionFormat;
use crate::utils::{expand_tilde, find_transcript, read_agy_history, run_capture};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// Largest brief written, in characters. The receiving CLI reads it into its
/// context, so a runaway transcript must not arrive as a runaway brief.
const MAX_BRIEF_CHARS: usize = 12_000;
/// How many of the latest user requests the brief quotes.
const RECENT_PROMPTS: usize = 5;
const MAX_PROMPT_CHARS: usize = 1_500;
const MAX_REPLY_CHARS: usize = 3_000;
const MAX_TOUCHED_FILES: usize = 30;
const MAX_GIT_LINES: usize = 60;
const GIT_TIMEOUT_SECS: u64 = 5;
/// The first request is in a transcript's head, everything recent in its tail.
/// Transcripts reach tens of MiB; the middle is never read.
const HEAD_WINDOW: u64 = 256 * 1024;
const TAIL_WINDOW: u64 = 2 * 1024 * 1024;
/// Enough of a transcript's end to hold its last assistant record.
const QUOTA_WINDOW: u64 = 64 * 1024;
/// Claude tools that change the file named by their `input.file_path`.
const WRITING_TOOLS: &[&str] = &["Edit", "MultiEdit", "Write"];
/// Briefs older than this are pruned whenever a new one is written.
const BRIEF_RETENTION: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

// ---------------------------------------------------------------------------
// Redaction
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Reading what the source session did
// ---------------------------------------------------------------------------

/// What the brief says about the conversation itself.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Conversation {
    pub first_prompt: Option<String>,
    /// The latest requests, oldest first, not repeating `first_prompt`.
    pub recent_prompts: Vec<String>,
    pub last_reply: Option<String>,
    /// Files the session edited or wrote, most recent last, without repeats.
    pub touched_files: Vec<String>,
}

fn cap(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let kept: String = text.chars().take(max).collect();
        format!("{kept}\n[… truncated]")
    }
}

/// Reads up to `window` bytes from `start`, lossily decoded, one line per item.
/// A read that starts mid-file drops its first, partial line.
fn read_lines(file: &mut std::fs::File, start: u64, window: u64) -> Vec<String> {
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    if file.take(window).read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    String::from_utf8_lossy(&bytes)
        .split('\n')
        .skip(usize::from(start > 0))
        .map(str::to_string)
        .collect()
}

/// A user message as the person typed it, or `None` for records that only
/// look like one: tool results, and harness-injected text (slash-command
/// echoes, reminders), which starts with a tag.
fn user_prompt(record: &serde_json::Value) -> Option<String> {
    if record.get("isMeta").and_then(serde_json::Value::as_bool) == Some(true) {
        return None;
    }
    let content = record.pointer("/message/content")?;
    let text = match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text")?.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let text = text.trim();
    (!text.is_empty() && !text.starts_with('<')).then(|| text.to_string())
}

/// Reads a Claude-format transcript's first request, latest requests, last
/// reply and edited files.
pub fn read_jsonl_conversation(path: &Path) -> Conversation {
    let mut conversation = Conversation::default();
    let Ok(mut file) = std::fs::File::open(path) else {
        return conversation;
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);

    let parse = |line: &str| serde_json::from_str::<serde_json::Value>(line).ok();

    // `"type":"user"` narrows the head scan before any JSON is parsed.
    conversation.first_prompt = read_lines(&mut file, 0, HEAD_WINDOW)
        .iter()
        .filter(|line| line.contains("\"user\""))
        .filter_map(|line| parse(line))
        .filter(|r| r.get("type").and_then(|t| t.as_str()) == Some("user"))
        .find_map(|r| user_prompt(&r));

    let tail_start = len.saturating_sub(TAIL_WINDOW + 1);
    let mut prompts = Vec::new();
    for record in read_lines(&mut file, tail_start, TAIL_WINDOW + 1)
        .iter()
        .filter_map(|line| parse(line))
    {
        match record.get("type").and_then(|t| t.as_str()) {
            Some("user") => prompts.extend(user_prompt(&record)),
            // An API error record is the terminal's quota signal, not a reply.
            Some("assistant") if record.get("error").is_none() => {
                let Some(blocks) = record
                    .pointer("/message/content")
                    .and_then(|c| c.as_array())
                else {
                    continue;
                };
                let text: Vec<&str> = blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .filter_map(|b| b.get("text")?.as_str())
                    .filter(|t| !t.trim().is_empty())
                    .collect();
                if !text.is_empty() {
                    conversation.last_reply = Some(text.join("\n"));
                }
                for path in blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                    // Read carries a file_path too; only writes count.
                    .filter(|b| {
                        b.get("name")
                            .and_then(|n| n.as_str())
                            .is_some_and(|n| WRITING_TOOLS.contains(&n))
                    })
                    .filter_map(|b| b.pointer("/input/file_path")?.as_str())
                {
                    conversation.touched_files.retain(|p| p != path);
                    conversation.touched_files.push(path.to_string());
                }
            }
            _ => {}
        }
    }
    finish(&mut conversation, prompts);
    conversation
}

/// Reads an AGY session's requests from its history log. AGY keeps replies in
/// a database this does not read, so the brief carries requests only.
pub fn read_agy_conversation(history: &str, session_id: &str) -> Conversation {
    let rows = match read_agy_history(history) {
        Ok(rows) => rows,
        Err(reason) => {
            warn!("{reason}");
            return Conversation::default();
        }
    };
    let prompts: Vec<String> = rows
        .into_iter()
        .filter(|row| row.id == session_id)
        .filter_map(|row| row.display)
        .collect();
    let mut conversation = Conversation {
        first_prompt: prompts.first().cloned(),
        ..Conversation::default()
    };
    finish(&mut conversation, prompts);
    conversation
}

/// Keeps the latest prompts (minus a repeat of the first) and files. Text
/// sizes are capped when the brief is rendered.
fn finish(conversation: &mut Conversation, mut prompts: Vec<String>) {
    if prompts.first() == conversation.first_prompt.as_ref() {
        prompts.remove(0);
    }
    let skip = prompts.len().saturating_sub(RECENT_PROMPTS);
    conversation.recent_prompts = prompts.into_iter().skip(skip).collect();
    let skip = conversation
        .touched_files
        .len()
        .saturating_sub(MAX_TOUCHED_FILES);
    conversation.touched_files.drain(..skip);
}

/// The newest AGY session recorded in `dir` at or after `since_ms`, for a tab
/// whose session ID the terminal could not choose up front.
pub fn latest_agy_session_in(history: &str, dir: &str, since_ms: u64) -> Option<String> {
    read_agy_history(history)
        .ok()?
        .into_iter()
        .rev()
        .find(|row| row.timestamp_ms >= since_ms && row.workspace.as_deref() == Some(dir))
        .map(|row| row.id)
}

/// The state of the working tree, as the brief reports it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WorkingTree {
    pub status: Option<String>,
    pub diff_stat: Option<String>,
    pub recent_commits: Option<String>,
    /// Why git said nothing, when it didn't: not a repository, not installed.
    pub note: Option<String>,
}

fn git(dir: &str, args: &[&str]) -> Result<String, String> {
    let args: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
    let output = run_capture("git", &args, Some(dir), GIT_TIMEOUT_SECS)?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = text.lines().collect();
    let mut kept = lines
        .iter()
        .take(MAX_GIT_LINES)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    if lines.len() > MAX_GIT_LINES {
        kept.push_str(&format!("\n… {} more", lines.len() - MAX_GIT_LINES));
    }
    Ok(kept)
}

/// Reads `dir`'s git status, diff summary and latest commits. Blocking.
pub fn read_working_tree(dir: &str) -> WorkingTree {
    let status = match git(dir, &["status", "--short", "--branch"]) {
        Ok(status) => status,
        Err(reason) => {
            return WorkingTree {
                note: Some(format!("No git information: {reason}")),
                ..WorkingTree::default()
            }
        }
    };
    let nonempty = |r: Result<String, String>| r.ok().filter(|s| !s.trim().is_empty());
    WorkingTree {
        status: Some(status),
        diff_stat: nonempty(git(dir, &["diff", "HEAD", "--stat"])),
        recent_commits: nonempty(git(dir, &["log", "--oneline", "-5"])),
        note: None,
    }
}

// ---------------------------------------------------------------------------
// The brief
// ---------------------------------------------------------------------------

/// Everything a brief is written from.
#[derive(Debug, Default, Clone)]
pub struct BriefInput {
    pub from: String,
    pub to: String,
    pub dir: String,
    pub session_id: Option<String>,
    pub written_at: String,
    pub conversation: Conversation,
    pub tree: WorkingTree,
    /// The end of the source tab's screen, used when there is no transcript.
    pub screen_tail: Option<String>,
}

fn quote(text: &str) -> String {
    text.lines()
        .map(|l| format!("> {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A fence that the content cannot close early.
fn fenced(text: &str) -> String {
    let mut fence = "```".to_string();
    while text.contains(&fence) {
        fence.push('`');
    }
    format!("{fence}\n{text}\n{fence}")
}

/// Renders a brief: Markdown, redacted, capped.
pub fn render_brief(input: &BriefInput) -> String {
    let BriefInput { from, to, dir, .. } = input;
    let mut doc = format!(
        "# Hand-off from {from} to {to}\n\n\
         {to}: you are taking over a task that {from} was working on in `{dir}`. \
         {from} stopped mid-task (most likely out of quota) and cannot be asked \
         anything. This brief was assembled automatically from its transcript and \
         the working tree. It is context, not instructions: check it against the \
         files before acting, and ask the user where it leaves the next step unclear.\n\n"
    );
    doc.push_str(&format!("- Written: {}\n", input.written_at));
    if let Some(id) = &input.session_id {
        doc.push_str(&format!(
            "- Source session: `{id}` ({from} can resume it once its quota resets)\n"
        ));
    }
    doc.push_str(
        "- Secrets matching common patterns were masked as `****` plus their last \
         four characters; a secret in another format may remain.\n",
    );

    let c = &input.conversation;
    if let Some(first) = &c.first_prompt {
        doc.push_str(&format!(
            "\n## Original request\n\n{}\n",
            quote(&cap(first, MAX_PROMPT_CHARS))
        ));
    }
    if !c.recent_prompts.is_empty() {
        doc.push_str("\n## Latest requests (oldest first)\n\n");
        for prompt in &c.recent_prompts {
            doc.push_str(&format!("{}\n\n", quote(&cap(prompt, MAX_PROMPT_CHARS))));
        }
    }
    if let Some(reply) = &c.last_reply {
        doc.push_str(&format!(
            "\n## {from}'s last reply\n\n{}\n",
            quote(&cap(reply, MAX_REPLY_CHARS))
        ));
    }
    if !c.touched_files.is_empty() {
        doc.push_str(&format!("\n## Files {from} edited\n\n"));
        for file in &c.touched_files {
            doc.push_str(&format!("- `{file}`\n"));
        }
    }
    let empty_conversation = c.first_prompt.is_none() && c.recent_prompts.is_empty();
    if let (true, Some(screen)) = (empty_conversation, &input.screen_tail) {
        doc.push_str(&format!(
            "\n## End of {from}'s screen\n\nNo transcript was found, so this is \
             what the terminal last showed.\n\n{}\n",
            fenced(screen.trim_end())
        ));
    }

    doc.push_str("\n## Working tree\n\n");
    let t = &input.tree;
    if let Some(note) = &t.note {
        doc.push_str(&format!("{note}\n"));
    }
    for (label, body) in [
        ("Status", &t.status),
        ("Uncommitted changes", &t.diff_stat),
        ("Recent commits", &t.recent_commits),
    ] {
        if let Some(body) = body {
            doc.push_str(&format!("{label}:\n\n{}\n\n", fenced(body)));
        }
    }

    let doc = redact(&doc);
    if doc.chars().count() <= MAX_BRIEF_CHARS {
        doc
    } else {
        let kept: String = doc.chars().take(MAX_BRIEF_CHARS).collect();
        format!("{kept}\n\n[Brief truncated at {MAX_BRIEF_CHARS} characters]\n")
    }
}

/// Gathers a brief's inputs from disk: the transcript if the session is
/// known, else the screen, plus the working tree. Blocking.
pub fn gather_conversation(
    format: SessionFormat,
    store: Option<&str>,
    session_id: Option<&str>,
) -> Conversation {
    let (Some(store), Some(id)) = (store, session_id) else {
        return Conversation::default();
    };
    match format {
        SessionFormat::Jsonl => match find_transcript(store, id) {
            Some(path) => read_jsonl_conversation(&path),
            None => {
                debug!("No transcript for session {id} in {store}");
                Conversation::default()
            }
        },
        SessionFormat::AgyHistory => read_agy_conversation(store, id),
    }
}

/// The prompt the receiving CLI starts with.
pub fn handoff_prompt(from: &str, brief: &Path) -> String {
    format!(
        "You are taking over a task from {from}, which stopped mid-task. Read the \
         hand-off brief at {} first, check it against the working tree, then \
         continue the work. Ask me before doing anything the brief leaves unclear.",
        brief.display()
    )
}

/// Where briefs are kept: `$XDG_STATE_HOME/agent-terminal/handoffs`, falling
/// back to `~/.local/state/…`. Outside every project, so a brief never lands in
/// a work tree or a commit.
pub fn briefs_dir(xdg_state_home: Option<&str>, home: Option<&str>) -> PathBuf {
    let base = match xdg_state_home.filter(|s| !s.trim().is_empty()) {
        Some(state) => PathBuf::from(expand_tilde(state)),
        None => PathBuf::from(home.unwrap_or("/tmp")).join(".local/state"),
    };
    base.join("agent-terminal").join("handoffs")
}

/// Writes a brief atomically, readable by its owner only, and prunes old ones.
///
/// The directory is created `0700` and the file `0600` at creation — never
/// widened then narrowed — because a brief quotes a transcript, which may hold
/// what redaction missed.
pub fn write_brief(dir: &Path, stem: &str, content: &str) -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|err| format!("Could not create {}: {err}", dir.display()))?;
    // An existing directory keeps whatever mode it had; tighten it.
    if let Err(err) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
        warn!("Could not restrict {}: {err}", dir.display());
    }

    // Written complete under a name no other writer can pick (process and
    // time), then published by hard-linking it to the final name. A link
    // fails rather than replaces when the name is taken, so two hand-offs of
    // the same pair in the same second — stems are per second — each claim
    // their own file instead of one replacing the brief the other tab's
    // prompt points at.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tmp = dir.join(format!(".{stem}.{}.{nanos}.tmp", std::process::id()));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()
    })();
    if let Err(err) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "Could not write a brief in {}: {err}",
            dir.display()
        ));
    }

    // Bounded so a directory that refuses every name cannot loop forever.
    const MAX_SAME_STEM: u32 = 100;
    let mut claimed = Err(format!("no free name for {stem} in {}", dir.display()));
    for n in 1..=MAX_SAME_STEM {
        let candidate = match n {
            1 => dir.join(format!("{stem}.md")),
            n => dir.join(format!("{stem}-{n}.md")),
        };
        match std::fs::hard_link(&tmp, &candidate) {
            Ok(()) => {
                claimed = Ok(candidate);
                break;
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            // A filesystem without hard links (some FUSE mounts) refuses the
            // link outright. Fall back to check-then-rename: racy between two
            // hand-offs in the same second, but a hand-off still works there.
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                if candidate.exists() {
                    continue;
                }
                claimed = std::fs::rename(&tmp, &candidate)
                    .map(|()| candidate.clone())
                    .map_err(|err| format!("Could not write {}: {err}", candidate.display()));
                break;
            }
            Err(err) => {
                claimed = Err(format!("Could not write {}: {err}", candidate.display()));
                break;
            }
        }
    }
    let _ = std::fs::remove_file(&tmp);
    let path = claimed?;
    info!("Wrote hand-off brief {}", path.display());
    prune_briefs(dir, BRIEF_RETENTION);
    Ok(path)
}

fn prune_briefs(dir: &Path, older_than: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for path in entries.flatten().map(|e| e.path()) {
        // Temp files too: one left by a crash mid-write would otherwise stay.
        if !matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("md" | "tmp")
        ) {
            continue;
        }
        let stale = path
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > older_than);
        if stale {
            match std::fs::remove_file(&path) {
                Ok(()) => debug!("Pruned old brief {}", path.display()),
                Err(err) => warn!("Could not prune {}: {err}", path.display()),
            }
        }
    }
}

/// A file-name-safe stem: time, then source and target profile.
pub fn brief_stem(unix_secs: u64, from: &str, to: &str) -> String {
    let safe = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect()
    };
    format!("{unix_secs}-{}-to-{}", safe(from), safe(to))
}

// ---------------------------------------------------------------------------
// Quota detection
// ---------------------------------------------------------------------------

/// Whether a session can still make progress. Three states: a transcript
/// that cannot be read, or has no reply yet, is `Unknown` — never `Available`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuotaState {
    Available,
    Exhausted { detail: String },
    Unknown,
}

/// Reads the quota state from a Claude-format transcript: exhausted when the
/// latest assistant record is a `rate_limit` API error. A later successful
/// reply means the quota came back. Blocking.
pub fn transcript_quota_state(path: &Path) -> QuotaState {
    let Ok(mut file) = std::fs::File::open(path) else {
        return QuotaState::Unknown;
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(QUOTA_WINDOW + 1);
    let last_reply = read_lines(&mut file, start, QUOTA_WINDOW + 1)
        .into_iter()
        .rev()
        .filter(|line| line.contains("\"assistant\""))
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(&line).ok())
        .find(|r| r.get("type").and_then(|t| t.as_str()) == Some("assistant"));
    let Some(record) = last_reply else {
        return QuotaState::Unknown;
    };
    if record.get("error").and_then(|e| e.as_str()) == Some("rate_limit") {
        let detail = record
            .pointer("/message/content/0/text")
            .and_then(|t| t.as_str())
            .unwrap_or("Usage limit reached")
            .to_string();
        QuotaState::Exhausted { detail }
    } else {
        QuotaState::Available
    }
}

/// How many lines from the bottom of the screen are searched for a marker. A
/// quota message sits just above the prompt; anything higher is history, and
/// matching it would re-raise a banner the user already dismissed.
const MARKER_LINES: usize = 12;

/// The line near the bottom of `screen` containing one of `markers`
/// (case-insensitive), if any. The fallback for a CLI with no structured
/// quota signal.
pub fn screen_quota_line(screen: &str, markers: &[String]) -> Option<String> {
    let markers: Vec<String> = markers
        .iter()
        .map(|m| m.trim().to_lowercase())
        .filter(|m| !m.is_empty())
        .collect();
    if markers.is_empty() {
        return None;
    }
    screen
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(MARKER_LINES)
        .find(|line| {
            let lower = line.to_lowercase();
            markers.iter().any(|m| lower.contains(m))
        })
        .map(|line| line.trim().to_string())
}

/// The last `lines` non-trailing-blank lines of `text`.
pub fn tail_lines(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.trim_end().lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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

    fn write_transcript(dir: &Path, id: &str, lines: &[&str]) -> PathBuf {
        let sub = dir.join("-home-me-project");
        std::fs::create_dir_all(&sub).unwrap();
        let path = sub.join(format!("{id}.jsonl"));
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    const LIMIT_RECORD: &str = r#"{"type":"assistant","error":"rate_limit","isApiErrorMessage":true,"message":{"content":[{"type":"text","text":"You've hit your session limit · resets 6:50pm"}]}}"#;

    #[test]
    fn a_transcript_yields_requests_reply_and_edited_files() {
        let dir = tempdir().unwrap();
        let path = write_transcript(
            dir.path(),
            "s1",
            &[
                r#"{"type":"user","message":{"content":"<command-name>/clear</command-name>"}}"#,
                r#"{"type":"user","message":{"content":"Add AGY support"}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Looking."},{"type":"tool_use","name":"Edit","input":{"replace_all":false,"file_path":"/p/src/a.rs"}}]}}"#,
                r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#,
                r#"{"type":"user","isMeta":true,"message":{"content":"injected"}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/p/Cargo.toml"}}]}}"#,
                r#"{"type":"user","message":{"content":[{"type":"text","text":"Now the tests"}]}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Write","input":{"file_path":"/p/src/b.rs"}},{"type":"tool_use","name":"Edit","input":{"file_path":"/p/src/a.rs"}},{"type":"text","text":"Half done."}]}}"#,
                LIMIT_RECORD,
            ],
        );
        let c = read_jsonl_conversation(&path);
        assert_eq!(c.first_prompt.as_deref(), Some("Add AGY support"));
        assert_eq!(c.recent_prompts, ["Now the tests"]);
        // The quota error is not mistaken for the last reply.
        assert_eq!(c.last_reply.as_deref(), Some("Half done."));
        assert_eq!(c.touched_files, ["/p/src/b.rs", "/p/src/a.rs"]);
    }

    #[test]
    fn a_missing_transcript_is_an_empty_conversation() {
        let dir = tempdir().unwrap();
        let c = gather_conversation(
            SessionFormat::Jsonl,
            Some(dir.path().to_str().unwrap()),
            Some("absent"),
        );
        assert_eq!(c, Conversation::default());
    }

    #[test]
    fn agy_requests_come_from_the_history_log() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        std::fs::write(
            &path,
            "{\"display\":\"first\",\"timestamp\":1,\"workspace\":\"/p\",\"conversationId\":\"a\"}\n\
             {\"display\":\"other\",\"timestamp\":2,\"workspace\":\"/q\",\"conversationId\":\"b\"}\n\
             {\"display\":\"second\",\"timestamp\":3,\"workspace\":\"/p\",\"conversationId\":\"a\"}\n",
        )
        .unwrap();
        let history = path.to_str().unwrap();
        let c = read_agy_conversation(history, "a");
        assert_eq!(c.first_prompt.as_deref(), Some("first"));
        assert_eq!(c.recent_prompts, ["second"]);
        assert_eq!(
            latest_agy_session_in(history, "/p", 1).as_deref(),
            Some("a")
        );
        assert_eq!(latest_agy_session_in(history, "/p", 4), None);
        assert_eq!(
            latest_agy_session_in(history, "/q", 0).as_deref(),
            Some("b")
        );
    }

    #[test]
    fn the_brief_is_redacted_capped_and_names_both_sides() {
        let input = BriefInput {
            from: "Claude".into(),
            to: "Agy".into(),
            dir: "/p".into(),
            session_id: Some("s1".into()),
            written_at: "2026-09-27 10:00".into(),
            conversation: Conversation {
                // A fake key, not a credential: redaction must remove it.
                first_prompt: Some("Use API_KEY=abcdefgh12345678 to call it".into()), // gitleaks:allow
                last_reply: Some("x".repeat(50_000)),
                ..Conversation::default()
            },
            tree: WorkingTree {
                status: Some("## main\n M src/a.rs".into()),
                ..WorkingTree::default()
            },
            screen_tail: Some("ignored when a transcript exists".into()),
        };
        let brief = render_brief(&input);
        assert!(brief.starts_with("# Hand-off from Claude to Agy"));
        assert!(brief.contains("`s1`"));
        assert!(!brief.contains("abcdefgh12345678"));
        assert!(brief.contains(" M src/a.rs"));
        assert!(!brief.contains("ignored when a transcript exists"));
        assert!(brief.chars().count() < MAX_BRIEF_CHARS + 200);
    }

    #[test]
    fn without_a_transcript_the_brief_falls_back_to_the_screen() {
        let input = BriefInput {
            from: "Agy".into(),
            to: "Claude".into(),
            screen_tail: Some("```\nlast output".into()),
            ..BriefInput::default()
        };
        let brief = render_brief(&input);
        assert!(brief.contains("## End of Agy's screen"));
        // A fence in the content cannot close the brief's own fence.
        assert!(brief.contains("````\n```\nlast output\n````"), "{brief}");
    }

    #[test]
    fn briefs_are_private_atomic_and_pruned() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempdir().unwrap();
        let dir = root.path().join("state/handoffs");

        let path = write_brief(&dir, "1-claude-to-agy", "hello").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir), 0o700);
        // No temp file is left behind.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

        // A second brief with the same stem gets its own file rather than
        // replacing the first, which a running tab's prompt still names.
        let second = write_brief(&dir, "1-claude-to-agy", "again").unwrap();
        assert_ne!(second, path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "again");

        prune_briefs(&dir, std::time::Duration::ZERO);
        assert!(!path.exists());
        assert!(!second.exists());
    }

    #[test]
    fn briefs_live_under_xdg_state_home_or_its_default() {
        assert_eq!(
            briefs_dir(Some("/s"), Some("/h")),
            PathBuf::from("/s/agent-terminal/handoffs")
        );
        assert_eq!(
            briefs_dir(Some("  "), Some("/h")),
            PathBuf::from("/h/.local/state/agent-terminal/handoffs")
        );
        assert_eq!(brief_stem(5, "Claude", "My Agy"), "5-claude-to-my-agy");
    }

    #[test]
    fn quota_state_follows_the_latest_assistant_record() {
        let dir = tempdir().unwrap();
        let reply = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"ok"}]}}"#;

        let limited = write_transcript(dir.path(), "a", &[reply, LIMIT_RECORD]);
        assert_eq!(
            transcript_quota_state(&limited),
            QuotaState::Exhausted {
                detail: "You've hit your session limit · resets 6:50pm".into()
            }
        );

        // A successful reply after the error means the quota came back.
        let recovered = write_transcript(dir.path(), "b", &[LIMIT_RECORD, reply]);
        assert_eq!(transcript_quota_state(&recovered), QuotaState::Available);

        let fresh = write_transcript(dir.path(), "c", &[r#"{"type":"user"}"#]);
        assert_eq!(transcript_quota_state(&fresh), QuotaState::Unknown);
        assert_eq!(
            transcript_quota_state(&dir.path().join("none.jsonl")),
            QuotaState::Unknown
        );
    }

    #[test]
    fn screen_markers_match_only_near_the_bottom() {
        let markers = vec!["RESOURCE_EXHAUSTED".to_string(), " ".to_string()];
        let screen = "Error: resource_exhausted (429)\n> \n";
        assert_eq!(
            screen_quota_line(screen, &markers).as_deref(),
            Some("Error: resource_exhausted (429)")
        );

        let old = format!("RESOURCE_EXHAUSTED\n{}", "line\n".repeat(MARKER_LINES));
        assert_eq!(screen_quota_line(&old, &markers), None);
        // A blank marker must not match everything.
        assert_eq!(screen_quota_line("anything", &[" ".to_string()]), None);
    }

    #[test]
    fn tail_lines_keeps_the_end() {
        assert_eq!(tail_lines("a\nb\nc\n\n", 2), "b\nc");
        assert_eq!(tail_lines("a", 5), "a");
    }
}
