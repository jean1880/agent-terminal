//! Reading CLI session stores: validating IDs, locating transcripts and listing
//! sessions. Blocking file I/O only; nothing here touches GTK.

use crate::paths::expand_tilde;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{debug, warn};

/// How a profile's `session_store` records sessions.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SessionFormat {
    /// A directory of `<id>.jsonl` transcripts, directly inside it or one
    /// directory down, whose records may carry a `cwd` (Claude).
    #[default]
    Jsonl,
    /// A single JSONL log, one row per prompt, carrying `conversationId`,
    /// `workspace`, `display` and a millisecond `timestamp` (AGY's
    /// `history.jsonl`).
    AgyHistory,
}

/// Longest session ID accepted. A UUID is 36; this leaves room for other CLIs'
/// formats without letting an arbitrary blob into an argv or a filename.
const MAX_SESSION_ID_LEN: usize = 128;

/// Checks a session ID before it goes anywhere near an argv or a path.
///
/// It arrives from the command line or a paste, and is then used both as a
/// command argument and as a filename inside the session store. So: ASCII
/// letters, digits, `-` and `_` only (no `/` or `.`, so no path traversal), and
/// no leading `-` (so it cannot be read as a flag). Surrounding whitespace from
/// a paste is trimmed rather than rejected.
pub fn validate_session_id(id: &str) -> Result<&str, String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("Session ID is empty".to_string());
    }
    if id.len() > MAX_SESSION_ID_LEN {
        return Err(format!(
            "Session ID is longer than {MAX_SESSION_ID_LEN} characters"
        ));
    }
    if id.starts_with('-') {
        return Err("Session ID cannot start with '-'".to_string());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("Session ID may only contain letters, digits, '-' and '_'".to_string());
    }
    Ok(id)
}
/// Finds the directory `session_id` was recorded in, using a session store.
///
/// Looks for `<id>.jsonl` directly in `store` and one level down (Claude keeps
/// one subdirectory per project), then takes the first record carrying a string
/// `cwd`. Returns `None` if there is no transcript, no `cwd`, or the directory
/// has since been removed. Resuming from the wrong directory fails in a way the
/// user can't see from the tab, so it's better to find out here.
///
/// Blocking filesystem I/O: call it off the main thread.
pub fn find_session_dir(store: &str, session_id: &str) -> Option<String> {
    use std::io::BufRead;

    let transcript = find_transcript(store, session_id)?;
    let file = match std::fs::File::open(&transcript) {
        Ok(file) => file,
        Err(err) => {
            warn!("Could not open {}: {err}", transcript.display());
            return None;
        }
    };

    let dir = std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .find_map(|line| {
            let record: serde_json::Value = serde_json::from_str(&line).ok()?;
            record.get("cwd")?.as_str().map(str::to_string)
        });

    match dir {
        Some(dir) if std::path::Path::new(&dir).is_dir() => Some(dir),
        Some(dir) => {
            warn!("Session {session_id} was recorded in {dir}, which no longer exists");
            None
        }
        None => {
            warn!("Session {session_id} transcript records no working directory");
            None
        }
    }
}

/// Finds the `<id>.jsonl` transcript for `session_id` in a
/// [`SessionFormat::Jsonl`] store: directly inside it or one level down.
///
/// Blocking filesystem I/O: call it off the main thread.
pub fn find_transcript(store: &str, session_id: &str) -> Option<std::path::PathBuf> {
    let store = std::path::PathBuf::from(expand_tilde(store));
    let file_name = format!("{session_id}.jsonl");

    // One level of subdirectories. Bounded on purpose: the store is user-named
    // and a recursive walk of the wrong directory could be enormous.
    let candidates = std::iter::once(store.join(&file_name)).chain(
        std::fs::read_dir(&store)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .map(|dir| dir.join(&file_name)),
    );

    let transcript = candidates.into_iter().find(|path| path.is_file())?;
    debug!(
        "Session {session_id} transcript found at {}",
        transcript.display()
    );
    Some(transcript)
}

/// [`find_session_dir`] for either store format.
pub fn find_session_dir_in(format: SessionFormat, store: &str, session_id: &str) -> Option<String> {
    match format {
        SessionFormat::Jsonl => find_session_dir(store, session_id),
        SessionFormat::AgyHistory => {
            let rows = match read_agy_history(store) {
                Ok(rows) => rows,
                Err(reason) => {
                    warn!("{reason}");
                    return None;
                }
            };
            let dir = rows
                .into_iter()
                .filter(|row| row.id == session_id)
                .max_by_key(|row| row.timestamp_ms)?
                .workspace?;
            if std::path::Path::new(&dir).is_dir() {
                Some(dir)
            } else {
                warn!("Session {session_id} was recorded in {dir}, which no longer exists");
                None
            }
        }
    }
}

/// [`list_sessions`] for either store format.
pub fn list_sessions_in(
    format: SessionFormat,
    store: &str,
    title_pointer: Option<&str>,
) -> Result<Vec<SessionSummary>, String> {
    match format {
        SessionFormat::Jsonl => list_sessions(store, title_pointer),
        SessionFormat::AgyHistory => list_agy_sessions(store),
    }
}

/// How much of the end of AGY's `history.jsonl` is read. It is one log that
/// only grows, so the tail holds the recent sessions; one row per prompt runs
/// to a few hundred bytes, so this covers thousands of prompts. Ceiling: a
/// session whose prompts all predate the window is not listed. Upgrade path:
/// an index keyed by `conversationId`, kept by the terminal.
const AGY_HISTORY_WINDOW: u64 = 4 * 1024 * 1024;

/// One prompt row from AGY's history log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgyRow {
    pub id: String,
    pub workspace: Option<String>,
    pub display: Option<String>,
    pub timestamp_ms: u64,
}

/// Reads the rows in the tail of an AGY history log, oldest first.
///
/// Rows without a valid `conversationId` or a `timestamp` are skipped; a
/// corrupt line does not hide the ones after it. `Err` means the log itself
/// could not be read. Blocking: call it off the main thread.
pub fn read_agy_history(path: &str) -> Result<Vec<AgyRow>, String> {
    use std::io::{Read, Seek, SeekFrom};

    let path = expand_tilde(path);
    let mut file =
        std::fs::File::open(&path).map_err(|err| format!("Could not read {path}: {err}"))?;
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    // As in summarize_session: start one byte early so the first piece is
    // always a partial (or empty) line that is safe to drop.
    let start = len.saturating_sub(AGY_HISTORY_WINDOW + 1);
    file.seek(SeekFrom::Start(start))
        .map_err(|err| format!("Could not read {path}: {err}"))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|err| format!("Could not read {path}: {err}"))?;
    let text = String::from_utf8_lossy(&bytes);

    let mut rows: Vec<AgyRow> = text
        .split('\n')
        .skip(usize::from(start > 0))
        .filter_map(|line| {
            let record: serde_json::Value = serde_json::from_str(line).ok()?;
            let id = record.get("conversationId")?.as_str()?;
            let id = validate_session_id(id).ok()?.to_string();
            let text_field = |key: &str| {
                record
                    .get(key)
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .filter(|s| !s.is_empty())
            };
            Some(AgyRow {
                id,
                workspace: text_field("workspace"),
                display: text_field("display"),
                timestamp_ms: record.get("timestamp")?.as_u64()?,
            })
        })
        .collect();
    rows.sort_by_key(|row| row.timestamp_ms);
    Ok(rows)
}

/// Lists AGY sessions from its history log, newest first.
///
/// The title is a session's first prompt in the window — AGY records no title
/// of its own — and the directory its latest `workspace`.
pub(crate) fn list_agy_sessions(path: &str) -> Result<Vec<SessionSummary>, String> {
    let rows = read_agy_history(path)?;

    // Rows are oldest first, so the first row seen for an ID holds its
    // opening prompt and each later one moves its last-active time on.
    let mut sessions: Vec<SessionSummary> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for row in rows {
        let modified = std::time::UNIX_EPOCH + std::time::Duration::from_millis(row.timestamp_ms);
        match index.get(&row.id) {
            Some(&i) => {
                let session = &mut sessions[i];
                session.modified = modified;
                if row.workspace.is_some() {
                    session.dir = row.workspace;
                }
                if session.title.is_none() {
                    session.title = row.display.as_deref().and_then(one_line_title);
                }
            }
            None => {
                index.insert(row.id.clone(), sessions.len());
                sessions.push(SessionSummary {
                    title: row.display.as_deref().and_then(one_line_title),
                    id: row.id,
                    dir: row.workspace,
                    modified,
                });
            }
        }
    }

    sessions.sort_by_key(|s| std::cmp::Reverse(s.modified));
    sessions.truncate(MAX_LISTED_SESSIONS);
    for session in &mut sessions {
        session.dir = session
            .dir
            .take()
            .filter(|dir| std::path::Path::new(dir).is_dir());
    }
    Ok(sessions)
}

/// Collapses whitespace and caps the length, for a one-line list row.
pub(crate) fn one_line_title(text: &str) -> Option<String> {
    let title = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!title.is_empty()).then(|| title.chars().take(MAX_TITLE_CHARS).collect())
}

/// One session as the browser lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    /// The latest title the transcript records, if the profile declares where
    /// titles live and this session has one.
    pub title: Option<String>,
    /// The recorded working directory, only if it still exists.
    pub dir: Option<String>,
    /// The transcript's modification time — when the session was last active.
    pub modified: std::time::SystemTime,
}

/// Newest sessions listed. Each costs up to two reads of [`SUMMARY_WINDOW`], so
/// this bounds the scan to a few tens of MiB however large the store grows.
/// Upgrade path if older sessions are wanted: page in more on scroll.
pub const MAX_LISTED_SESSIONS: usize = 200;

/// How much of a transcript's head, and of its tail, a summary reads. The
/// working directory is in the first records; the latest title is near the
/// end, since titles are re-recorded as a session goes. Transcripts reach
/// several MiB, so neither end justifies reading the middle.
pub(crate) const SUMMARY_WINDOW: u64 = 256 * 1024;

/// Longest title shown; titles are one line in a list row.
const MAX_TITLE_CHARS: usize = 120;

/// Lists the sessions in a store, newest first.
///
/// Looks at `*.jsonl` directly in `store` and one directory down, the same
/// places [`find_session_dir`] searches. A file whose name is not a valid
/// session ID is skipped: it could not be resumed anyway. `title_pointer` is
/// the profile's `session_title`.
///
/// `Err` means the store itself could not be read — an unreadable store must
/// not look like an empty one. Blocking: call it off the main thread.
pub fn list_sessions(
    store: &str,
    title_pointer: Option<&str>,
) -> Result<Vec<SessionSummary>, String> {
    let store = std::path::PathBuf::from(expand_tilde(store));
    let top = std::fs::read_dir(&store)
        .map_err(|err| format!("Could not read {}: {err}", store.display()))?;

    let mut transcripts: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();
    let mut consider = |path: std::path::PathBuf| {
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            return;
        }
        if let Ok(modified) = path.metadata().and_then(|m| m.modified()) {
            transcripts.push((path, modified));
        }
    };
    for entry in top.flatten() {
        let path = entry.path();
        if path.is_dir() {
            for sub in std::fs::read_dir(&path).into_iter().flatten().flatten() {
                consider(sub.path());
            }
        } else {
            consider(path);
        }
    }

    transcripts.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
    Ok(transcripts
        .into_iter()
        .filter_map(|(path, modified)| {
            let id = path.file_stem()?.to_str()?;
            let id = validate_session_id(id).ok()?.to_string();
            Some((path, id, modified))
        })
        .take(MAX_LISTED_SESSIONS)
        .map(|(path, id, modified)| summarize_session(&path, id, modified, title_pointer))
        .collect())
}

/// Reads the working directory and latest title from a transcript's two ends.
pub(crate) fn summarize_session(
    path: &std::path::Path,
    id: String,
    modified: std::time::SystemTime,
    title_pointer: Option<&str>,
) -> SessionSummary {
    use std::io::{BufRead, Read, Seek, SeekFrom};

    // A cheap substring test before parsing: most lines are large tool output
    // that mentions neither key, and parsing them all would dominate the scan.
    let title_key = title_pointer
        .and_then(|p| p.rsplit('/').next())
        .filter(|k| !k.is_empty())
        .map(|k| format!("\"{k}\""));
    let title_of = |line: &str| -> Option<String> {
        let (pointer, key) = (title_pointer?, title_key.as_deref()?);
        if !line.contains(key) {
            return None;
        }
        let record: serde_json::Value = serde_json::from_str(line).ok()?;
        one_line_title(record.pointer(pointer)?.as_str()?)
    };

    let mut summary = SessionSummary {
        id,
        title: None,
        dir: None,
        modified,
    };
    let Ok(mut file) = std::fs::File::open(path) else {
        return summary;
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);

    // Split on raw bytes and decode each line lossily. `lines()` would stop at
    // the first line that isn't valid UTF-8, so one corrupt record would hide
    // every record after it. `map_while` still stops on a real I/O error,
    // which would only repeat.
    let mut head_title = None;
    for line in std::io::BufReader::new((&mut file).take(SUMMARY_WINDOW))
        .split(b'\n')
        .map_while(Result::ok)
    {
        let line = String::from_utf8_lossy(&line);
        if summary.dir.is_none() && line.contains("\"cwd\"") {
            summary.dir = serde_json::from_str::<serde_json::Value>(&line)
                .ok()
                .and_then(|r| r.get("cwd")?.as_str().map(str::to_string));
        }
        if let Some(title) = title_of(&line) {
            head_title = Some(title);
        }
    }

    // The tail, when the head did not already cover the whole file. Reading
    // starts one byte before the window, so the first piece `split` yields is
    // always safe to drop: it is empty if the window starts on a line
    // boundary, and a partial line otherwise. Starting exactly at the window
    // would instead discard a complete first line whenever the boundary fell
    // on a newline.
    let mut tail_title = None;
    let tail_start = len.saturating_sub(SUMMARY_WINDOW + 1);
    if len > SUMMARY_WINDOW && file.seek(SeekFrom::Start(tail_start)).is_ok() {
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_ok() {
            let text = String::from_utf8_lossy(&bytes);
            tail_title = text.split('\n').skip(1).filter_map(title_of).last();
        }
    }

    summary.title = tail_title.or(head_title);
    summary.dir = summary.dir.filter(|dir| std::path::Path::new(dir).is_dir());
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn session_ids_that_could_escape_are_rejected() {
        // The ID becomes both an argv element and a filename in the store.
        assert!(validate_session_id("../../etc/passwd").is_err());
        assert!(validate_session_id("a/b").is_err());
        assert!(validate_session_id("--dangerously-skip-permissions").is_err());
        assert!(validate_session_id("a b").is_err());
        assert!(validate_session_id("$(reboot)").is_err());
        assert!(validate_session_id("").is_err());
        assert!(validate_session_id(&"a".repeat(129)).is_err());
    }

    #[test]
    fn a_pasted_uuid_is_accepted_and_trimmed() {
        assert_eq!(
            validate_session_id("  215bf7f7-88e4-4070-b41b-332500303534\n"),
            Ok("215bf7f7-88e4-4070-b41b-332500303534")
        );
    }

    /// A captured-shape AGY history log: two sessions, interleaved, plus noise.
    fn agy_history(dir: &std::path::Path, workspace: &std::path::Path) -> String {
        let ws = workspace.display();
        let body = format!(
            "{{\"display\":\"Port my settings\",\"timestamp\":1000,\"workspace\":\"{ws}\",\"conversationId\":\"aaa-1\"}}\n\
             {{\"display\":\"Second task\",\"timestamp\":2000,\"workspace\":\"/nowhere/gone\",\"conversationId\":\"bbb-2\"}}\n\
             not json at all\n\
             {{\"display\":\"No id\",\"timestamp\":2500}}\n\
             {{\"display\":\"../escape\",\"timestamp\":2600,\"conversationId\":\"../../etc\"}}\n\
             {{\"display\":\"Follow-up\",\"timestamp\":3000,\"workspace\":\"{ws}\",\"conversationId\":\"aaa-1\"}}\n"
        );
        let path = dir.join("history.jsonl");
        std::fs::write(&path, body).unwrap();
        path.display().to_string()
    }

    #[test]
    fn agy_sessions_are_grouped_titled_by_first_prompt_and_newest_first() {
        let dir = tempdir().unwrap();
        let path = agy_history(dir.path(), dir.path());
        let sessions = list_sessions_in(SessionFormat::AgyHistory, &path, None).unwrap();

        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["aaa-1", "bbb-2"]);
        assert_eq!(sessions[0].title.as_deref(), Some("Port my settings"));
        assert_eq!(
            sessions[0].modified,
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(3000)
        );
        assert_eq!(
            sessions[0].dir.as_deref(),
            Some(dir.path().to_str().unwrap())
        );
        // A workspace that no longer exists is not offered as a place to resume.
        assert_eq!(sessions[1].dir, None);
    }

    #[test]
    fn agy_session_dir_is_its_latest_workspace() {
        let dir = tempdir().unwrap();
        let path = agy_history(dir.path(), dir.path());
        assert_eq!(
            find_session_dir_in(SessionFormat::AgyHistory, &path, "aaa-1").as_deref(),
            dir.path().to_str()
        );
        assert_eq!(
            find_session_dir_in(SessionFormat::AgyHistory, &path, "bbb-2"),
            None
        );
        assert_eq!(
            find_session_dir_in(SessionFormat::AgyHistory, &path, "missing"),
            None
        );
    }

    #[test]
    fn a_missing_agy_history_is_an_error_not_an_empty_list() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("absent.jsonl");
        assert!(list_sessions_in(SessionFormat::AgyHistory, path.to_str().unwrap(), None).is_err());
    }

    #[test]
    fn agy_history_reads_only_its_tail_and_drops_the_partial_first_line() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let old = "{\"display\":\"old\",\"timestamp\":1,\"conversationId\":\"old-1\"}\n";
        let filler = "x".repeat(usize::try_from(AGY_HISTORY_WINDOW).unwrap());
        let recent = "{\"display\":\"new\",\"timestamp\":2,\"conversationId\":\"new-1\"}\n";
        std::fs::write(&path, format!("{old}{filler}\n{recent}")).unwrap();

        let rows = read_agy_history(path.to_str().unwrap()).unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["new-1"]);
    }

    #[test]
    fn session_dir_is_read_from_a_transcript_one_level_down() {
        let store = tempdir().unwrap();
        let project = tempdir().unwrap();
        let project_dir = project.path().to_str().unwrap();
        let sub = store.path().join("-some-project");
        std::fs::create_dir_all(&sub).unwrap();
        // The first record has no cwd, as Claude's leading summary lines don't.
        std::fs::write(
            sub.join("abc-123.jsonl"),
            format!(
                "{{\"type\":\"summary\"}}\nnot json\n{{\"type\":\"user\",\"cwd\":\"{project_dir}\"}}\n"
            ),
        )
        .unwrap();

        assert_eq!(
            find_session_dir(store.path().to_str().unwrap(), "abc-123").as_deref(),
            Some(project_dir)
        );
    }

    #[test]
    fn session_dir_is_none_when_unknown_or_gone() {
        let store = tempdir().unwrap();
        let store_path = store.path().to_str().unwrap();
        assert_eq!(find_session_dir(store_path, "missing"), None);

        // A recorded directory that has since been deleted is no better than
        // none: spawning there would fall back to $HOME and fail to resume.
        std::fs::write(
            store.path().join("gone.jsonl"),
            "{\"cwd\":\"/nonexistent/project/dir\"}\n",
        )
        .unwrap();
        assert_eq!(find_session_dir(store_path, "gone"), None);

        assert_eq!(find_session_dir("/nonexistent/store", "abc"), None);
    }

    /// Writes a transcript and backdates it, so ordering is deterministic.
    fn transcript(dir: &std::path::Path, id: &str, body: &str, age_secs: u64) {
        let path = dir.join(format!("{id}.jsonl"));
        std::fs::write(&path, body).unwrap();
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(age_secs);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    #[test]
    fn sessions_are_listed_newest_first_with_their_latest_title() {
        let store = tempdir().unwrap();
        let project = tempdir().unwrap();
        let project_dir = project.path().to_str().unwrap();
        let sub = store.path().join("-proj");
        std::fs::create_dir_all(&sub).unwrap();

        // Retitled part-way through: the later title must win.
        transcript(
            &sub,
            "new-1",
            &format!(
                "{{\"cwd\":\"{project_dir}\"}}\n{{\"aiTitle\":\"First draft\"}}\n\
                 {{\"aiTitle\":\"Final  title\\nsecond line\"}}\n"
            ),
            10,
        );
        transcript(
            store.path(),
            "old-1",
            "{\"cwd\":\"/nonexistent/gone\"}\n",
            5000,
        );
        // Not a valid session ID, so not resumable, so not listed.
        transcript(&sub, "not.an.id", "{}\n", 1);

        let sessions = list_sessions(store.path().to_str().unwrap(), Some("/aiTitle")).unwrap();
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["new-1", "old-1"]);

        // Whitespace, newlines included, is collapsed onto one line.
        assert_eq!(
            sessions[0].title.as_deref(),
            Some("Final title second line")
        );
        assert_eq!(sessions[0].dir.as_deref(), Some(project_dir));
        // A recorded directory that no longer exists is not offered.
        assert_eq!(sessions[1].title, None);
        assert_eq!(sessions[1].dir, None);
    }

    #[test]
    fn the_title_is_found_in_the_tail_of_a_large_transcript() {
        // Only the two ends are read, so a title recorded near the end of a
        // file much larger than the window must still be found there.
        let store = tempdir().unwrap();
        let filler = format!("{{\"blob\":\"{}\"}}\n", "x".repeat(1024));
        let body = format!(
            "{{\"aiTitle\":\"Early\"}}\n{}{{\"aiTitle\":\"Late\"}}\n",
            filler.repeat(600)
        );
        assert!(body.len() as u64 > 2 * SUMMARY_WINDOW);
        transcript(store.path(), "big", &body, 1);

        let sessions = list_sessions(store.path().to_str().unwrap(), Some("/aiTitle")).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("Late"));
    }

    #[test]
    fn a_title_line_starting_exactly_at_the_tail_window_is_kept() {
        // The tail window begins on a line boundary here, so its first line is
        // complete. Always skipping the first line would drop the only title.
        let store = tempdir().unwrap();
        let head = format!("{{\"blob\":\"{}\"}}\n", "x".repeat(SUMMARY_WINDOW as usize));
        let prefix = "{\"aiTitle\":\"Edge\",\"pad\":\"";
        let suffix = "\"}\n";
        let pad = "p".repeat(SUMMARY_WINDOW as usize - prefix.len() - suffix.len());
        let tail = format!("{prefix}{pad}{suffix}");
        assert_eq!(tail.len() as u64, SUMMARY_WINDOW);
        transcript(store.path(), "edge", &format!("{head}{tail}"), 1);

        let sessions = list_sessions(store.path().to_str().unwrap(), Some("/aiTitle")).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("Edge"));
    }

    #[test]
    fn a_corrupt_line_does_not_hide_the_records_after_it() {
        let store = tempdir().unwrap();
        let project = tempdir().unwrap();
        let project_dir = project.path().to_str().unwrap();
        let mut body = b"{\"broken\":\"\xff\xfe\"}\n".to_vec();
        body.extend_from_slice(format!("{{\"cwd\":\"{project_dir}\"}}\n").as_bytes());
        std::fs::write(store.path().join("bad.jsonl"), body).unwrap();

        let sessions = list_sessions(store.path().to_str().unwrap(), None).unwrap();
        assert_eq!(sessions[0].dir.as_deref(), Some(project_dir));
    }

    #[test]
    fn without_a_title_pointer_sessions_are_untitled() {
        let store = tempdir().unwrap();
        transcript(store.path(), "s1", "{\"aiTitle\":\"Ignored\"}\n", 1);
        let sessions = list_sessions(store.path().to_str().unwrap(), None).unwrap();
        assert_eq!(sessions[0].title, None);
    }

    #[test]
    fn an_unreadable_store_is_an_error_not_an_empty_list() {
        // The indicator lesson: failure to read must never pass for "nothing there".
        assert!(list_sessions("/nonexistent/store", Some("/aiTitle")).is_err());
    }
}
