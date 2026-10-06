//! Sessions the agents saved themselves: listing them for the thread list, and reading one back as
//! canonical envelopes so the chat view can show the old conversation. Blocking file I/O; call it
//! off the main thread.
//!
//! Claude keeps `~/.claude/projects/<dir>/<session>.jsonl` (subagent transcripts live one level
//! deeper, in `<session>/subagents/`, so a one-level listing never sees them). agy keeps only a
//! prompt log, `~/.gemini/antigravity-cli/history.jsonl`; its replies sit in a protobuf store we do
//! not parse, so an agy session imports as its prompts plus a notice.
//!
//! TODO(codex): Codex sessions (`~/.codex/sessions/**/rollout-*.jsonl`) are not listed. Their
//! on-disk format is not documented anywhere this crate can check, Codex is not signed in on the
//! development machine, and a guess would show wrong history. Upgrade path: record a real rollout
//! file, scrub it into `agent-core/tests/fixtures/`, and add a `codex_transcript` importer.

use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_core::adapter::Driver;
use agent_core::event::Envelope;
use agent_core::import::{agy_prompts, ClaudeImporter};

use crate::sessions::{
    list_agy_sessions, one_line_title, read_agy_history, summarize_session, validate_session_id,
    SUMMARY_WINDOW,
};

/// Longest Claude transcript read back. Larger files are refused rather than slowly imported.
pub const MAX_TRANSCRIPT_BYTES: u64 = 200 * 1024 * 1024;

/// Longest list title, in characters.
const MAX_NATIVE_TITLE_CHARS: usize = 80;

/// One session an agent saved on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSession {
    pub driver: Driver,
    /// The id the agent resumes it by.
    pub native_id: String,
    /// The recorded working directory, only if it still exists.
    pub cwd: Option<String>,
    /// One line, at most 80 characters.
    pub title: Option<String>,
    /// Last activity, epoch seconds.
    pub modified: i64,
}

fn claude_projects(home: &Path) -> PathBuf {
    home.join(".claude").join("projects")
}

fn agy_history_path(home: &Path) -> PathBuf {
    home.join(".gemini")
        .join("antigravity-cli")
        .join("history.jsonl")
}

fn epoch_secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

fn short_title(title: &str) -> Option<String> {
    one_line_title(title).map(|t| t.chars().take(MAX_NATIVE_TITLE_CHARS).collect())
}

/// The newest `limit` sessions of every agent, newest first.
///
/// An agent whose store is missing or unreadable contributes nothing; an empty list is not an
/// error here, because a fresh machine has no history. Claude sessions with no conversation in them
/// (only housekeeping records) are left out.
pub fn recent_native_sessions(home: &Path, limit: usize) -> Vec<NativeSession> {
    let mut all = claude_sessions(home, limit);
    all.extend(agy_sessions(home, limit));
    all.sort_by_key(|s| std::cmp::Reverse(s.modified));
    all.truncate(limit);
    all
}

fn claude_sessions(home: &Path, limit: usize) -> Vec<NativeSession> {
    let mut transcripts: Vec<(PathBuf, String, SystemTime)> = Vec::new();
    for project in std::fs::read_dir(claude_projects(home))
        .into_iter()
        .flatten()
        .flatten()
    {
        let dir = project.path();
        if !dir.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| validate_session_id(s).ok())
                .map(str::to_owned)
            else {
                continue;
            };
            if let Ok(modified) = path.metadata().and_then(|m| m.modified()) {
                transcripts.push((path, id, modified));
            }
        }
    }
    transcripts.sort_by_key(|(_, _, modified)| std::cmp::Reverse(*modified));

    let mut out = Vec::new();
    for (path, id, modified) in transcripts {
        if out.len() >= limit {
            break;
        }
        let head = head_info(&path);
        if !head.has_message {
            continue;
        }
        let summary = summarize_session(&path, id.clone(), modified, Some("/aiTitle"));
        let title = summary
            .title
            .as_deref()
            .and_then(short_title)
            .or_else(|| head.first_prompt.as_deref().and_then(short_title));
        out.push(NativeSession {
            driver: Driver::Claude,
            native_id: id,
            cwd: summary.dir,
            title,
            modified: epoch_secs(modified),
        });
    }
    out
}

fn agy_sessions(home: &Path, limit: usize) -> Vec<NativeSession> {
    let path = agy_history_path(home);
    let Some(path) = path.to_str() else {
        return Vec::new();
    };
    list_agy_sessions(path)
        .unwrap_or_default()
        .into_iter()
        .take(limit)
        .map(|s| NativeSession {
            driver: Driver::Agy,
            native_id: s.id,
            cwd: s.dir,
            title: s.title.as_deref().and_then(short_title),
            modified: epoch_secs(s.modified),
        })
        .collect()
}

/// What the head of a Claude transcript says about its content.
struct HeadInfo {
    /// A user or assistant record exists. When the head window is full and shows none, the file is
    /// assumed to have one further in (a huge attachment can push it past the window).
    has_message: bool,
    first_prompt: Option<String>,
}

fn head_info(path: &Path) -> HeadInfo {
    let Ok(file) = std::fs::File::open(path) else {
        return HeadInfo {
            has_message: false,
            first_prompt: None,
        };
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut info = HeadInfo {
        has_message: len > SUMMARY_WINDOW,
        first_prompt: None,
    };
    for line in std::io::BufReader::new(file.take(SUMMARY_WINDOW))
        .split(b'\n')
        .map_while(Result::ok)
    {
        let line = String::from_utf8_lossy(&line);
        let is_user = line.contains("\"type\":\"user\"");
        if !is_user && !line.contains("\"type\":\"assistant\"") {
            continue;
        }
        info.has_message = true;
        if !is_user {
            continue;
        }
        let Ok(record) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if ["isMeta", "isSidechain", "isCompactSummary"]
            .iter()
            .any(|k| record.get(k).and_then(|v| v.as_bool()) == Some(true))
        {
            continue;
        }
        let content = record.pointer("/message/content");
        let text = match content {
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(serde_json::Value::Array(blocks)) => blocks.iter().find_map(|b| {
                (b.get("type")?.as_str()? == "text")
                    .then(|| b.get("text")?.as_str().map(str::to_owned))
                    .flatten()
            }),
            _ => None,
        };
        // Tag-led text is a slash command, command output or a reminder, not something typed.
        if let Some(text) = text.filter(|t| {
            let t = t.trim_start();
            !t.starts_with('<') && !t.starts_with("[Request interrupted")
        }) {
            info.first_prompt = Some(text);
            break;
        }
    }
    info
}

/// Reads a saved session back as envelopes for the chat view.
///
/// Claude: finds `<native_id>.jsonl` under `~/.claude/projects`, refuses a file over
/// [`MAX_TRANSCRIPT_BYTES`], and streams it line by line through the importer (which keeps only
/// the newest items). agy: the session's prompts from `history.jsonl` (only the tail of that log is
/// read, as in listing) plus a notice that its replies are not shown. Codex is not supported.
pub fn read_native_history(home: &Path, s: &NativeSession) -> Result<Vec<Envelope>, String> {
    let id = validate_session_id(&s.native_id)?;
    match s.driver {
        Driver::Claude => read_claude(home, id),
        Driver::Agy => read_agy(home, id),
        Driver::Codex => Err("Codex history cannot be imported yet".to_owned()),
    }
}

fn read_claude(home: &Path, id: &str) -> Result<Vec<Envelope>, String> {
    let store = claude_projects(home);
    let store = store
        .to_str()
        .ok_or_else(|| "The Claude project directory is not valid UTF-8".to_owned())?;
    let path = crate::sessions::find_transcript(store, id)
        .ok_or_else(|| format!("No Claude transcript found for session {id}"))?;
    let file = std::fs::File::open(&path)
        .map_err(|e| format!("Could not read {}: {e}", path.display()))?;
    let len = file
        .metadata()
        .map_err(|e| format!("Could not read {}: {e}", path.display()))?
        .len();
    if len > MAX_TRANSCRIPT_BYTES {
        return Err(format!(
            "{} is {} MB, over the {} MB import limit",
            path.display(),
            len / (1024 * 1024),
            MAX_TRANSCRIPT_BYTES / (1024 * 1024)
        ));
    }
    let mut importer = ClaudeImporter::new();
    for line in std::io::BufReader::new(file).split(b'\n') {
        let line = line.map_err(|e| format!("Could not read {}: {e}", path.display()))?;
        importer.push_line(&String::from_utf8_lossy(&line));
    }
    Ok(importer.finish())
}

fn read_agy(home: &Path, id: &str) -> Result<Vec<Envelope>, String> {
    let path = agy_history_path(home);
    let path = path
        .to_str()
        .ok_or_else(|| "The agy history path is not valid UTF-8".to_owned())?;
    let rows: Vec<(String, Option<i64>)> = read_agy_history(path)?
        .into_iter()
        .filter(|row| row.id == id)
        .filter_map(|row| {
            let secs = i64::try_from(row.timestamp_ms / 1000).ok();
            Some((row.display?, secs))
        })
        .collect();
    Ok(agy_prompts(&rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::event::{Event, ItemKind};
    use tempfile::tempdir;

    fn write(path: &Path, body: &str, age_secs: u64) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
        let when = SystemTime::now() - std::time::Duration::from_secs(age_secs);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    fn claude_file(home: &Path, project: &str, id: &str) -> PathBuf {
        claude_projects(home)
            .join(project)
            .join(format!("{id}.jsonl"))
    }

    const CONVERSATION: &str = concat!(
        "{\"type\":\"file-history-snapshot\"}\n",
        "{\"type\":\"user\",\"uuid\":\"u1\",\"cwd\":\"/nonexistent/w\",\"message\":{\"role\":\"user\",\"content\":\"Tidy the readme\"}}\n",
        "{\"type\":\"assistant\",\"uuid\":\"a1\",\"message\":{\"id\":\"m1\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"Done.\"}]}}\n",
    );

    #[test]
    fn claude_sessions_list_newest_first_with_titles_and_skip_empty_and_subagents() {
        let home = tempdir().unwrap();
        let cwd = tempdir().unwrap();
        let cwd_str = cwd.path().to_str().unwrap();

        // Titled by the transcript's own ai-title.
        write(
            &claude_file(home.path(), "-w", "new-1"),
            &format!(
                "{{\"type\":\"user\",\"cwd\":\"{cwd_str}\",\"message\":{{\"content\":\"hi\"}}}}\n\
                 {{\"type\":\"ai-title\",\"aiTitle\":\"A titled chat\"}}\n"
            ),
            10,
        );
        // No title record: the first typed prompt (the command line before it is not a prompt).
        write(&claude_file(home.path(), "-w", "old-1"), &format!(
            "{{\"type\":\"user\",\"message\":{{\"content\":\"<command-name>/x</command-name>\"}}}}\n\
             {{\"type\":\"user\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{}\"}}]}}}}\n",
            "word ".repeat(40)
        ), 500);
        // Housekeeping only: nothing to show.
        write(
            &claude_file(home.path(), "-w", "empty-1"),
            "{\"type\":\"file-history-snapshot\"}\n{\"type\":\"attachment\"}\n",
            1,
        );
        // A subagent transcript sits two levels down and is never listed.
        write(
            &claude_projects(home.path())
                .join("-w")
                .join("new-1")
                .join("subagents")
                .join("agent-x.jsonl"),
            CONVERSATION,
            1,
        );

        let sessions = recent_native_sessions(home.path(), 10);
        let ids: Vec<&str> = sessions.iter().map(|s| s.native_id.as_str()).collect();
        assert_eq!(ids, ["new-1", "old-1"]);
        assert_eq!(sessions[0].driver, Driver::Claude);
        assert_eq!(sessions[0].title.as_deref(), Some("A titled chat"));
        assert_eq!(sessions[0].cwd.as_deref(), Some(cwd_str));
        let title = sessions[1].title.as_deref().unwrap();
        assert!(title.starts_with("word word"));
        assert!(title.chars().count() <= MAX_NATIVE_TITLE_CHARS);
        assert!(sessions[0].modified > sessions[1].modified);
    }

    fn agy_history(home: &Path) {
        write(
            &agy_history_path(home),
            "{\"display\":\"Port the settings\",\"timestamp\":5000000,\"workspace\":\"/nonexistent/a\",\"conversationId\":\"agy-1\"}\n\
             {\"display\":\"And the tests\",\"timestamp\":6000000,\"workspace\":\"/nonexistent/a\",\"conversationId\":\"agy-1\"}\n\
             {\"display\":\"Other chat\",\"timestamp\":4000000,\"conversationId\":\"agy-2\"}\n",
            0,
        );
    }

    #[test]
    fn agy_and_claude_sessions_merge_by_recency_and_respect_the_limit() {
        let home = tempdir().unwrap();
        agy_history(home.path());
        write(&claude_file(home.path(), "-w", "c-1"), CONVERSATION, 0);

        let all = recent_native_sessions(home.path(), 10);
        // The agy rows date from 1970, so Claude is newest.
        let order: Vec<(Driver, &str)> = all
            .iter()
            .map(|s| (s.driver, s.native_id.as_str()))
            .collect();
        assert_eq!(
            order,
            [
                (Driver::Claude, "c-1"),
                (Driver::Agy, "agy-1"),
                (Driver::Agy, "agy-2")
            ]
        );
        let agy = &all[1];
        assert_eq!(agy.title.as_deref(), Some("Port the settings"));
        assert_eq!(agy.modified, 6000);
        assert_eq!(recent_native_sessions(home.path(), 2).len(), 2);
    }

    #[test]
    fn a_machine_with_no_history_lists_nothing() {
        let home = tempdir().unwrap();
        assert!(recent_native_sessions(home.path(), 10).is_empty());
    }

    #[test]
    fn a_claude_session_reads_back_as_its_conversation() {
        let home = tempdir().unwrap();
        write(&claude_file(home.path(), "-w", "c-1"), CONVERSATION, 0);
        let s = recent_native_sessions(home.path(), 5).remove(0);
        let out = read_native_history(home.path(), &s).unwrap();
        assert!(out.iter().any(|e| matches!(
            e.event,
            Event::ItemStarted {
                kind: ItemKind::UserMessage,
                ..
            }
        )));
        assert!(out.iter().any(
            |e| matches!(&e.event, Event::ContentSnapshot { text, .. } if text == "Tidy the readme")
        ));
        assert!(out
            .iter()
            .any(|e| matches!(&e.event, Event::ContentSnapshot { text, .. } if text == "Done.")));
    }

    #[test]
    fn an_agy_session_reads_back_as_its_prompts_and_a_notice() {
        let home = tempdir().unwrap();
        agy_history(home.path());
        let s = recent_native_sessions(home.path(), 5)
            .into_iter()
            .find(|s| s.native_id == "agy-1")
            .unwrap();
        let out = read_native_history(home.path(), &s).unwrap();
        let texts: Vec<&str> = out
            .iter()
            .filter_map(|e| match &e.event {
                Event::ContentSnapshot { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["Port the settings", "And the tests"]);
        assert!(matches!(
            out.last().map(|e| &e.event),
            Some(Event::Notice { text }) if text.starts_with("Antigravity keeps its replies")
        ));
    }

    #[test]
    fn unsafe_missing_oversized_and_codex_sessions_are_errors() {
        let home = tempdir().unwrap();
        let mut s = NativeSession {
            driver: Driver::Claude,
            native_id: "../../etc/passwd".to_owned(),
            cwd: None,
            title: None,
            modified: 0,
        };
        assert!(read_native_history(home.path(), &s).is_err());
        s.native_id = "absent".to_owned();
        assert!(read_native_history(home.path(), &s).is_err());

        // A sparse file one byte over the limit: refused from its size, never read.
        let big = claude_file(home.path(), "-w", "big");
        std::fs::create_dir_all(big.parent().unwrap()).unwrap();
        std::fs::File::create(&big)
            .unwrap()
            .set_len(MAX_TRANSCRIPT_BYTES + 1)
            .unwrap();
        s.native_id = "big".to_owned();
        let err = read_native_history(home.path(), &s).unwrap_err();
        assert!(err.contains("import limit"), "{err}");

        s.driver = Driver::Codex;
        assert!(read_native_history(home.path(), &s).is_err());
    }
}
