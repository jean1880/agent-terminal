//! Antigravity's on-disk transcript, read for what its stdout omits.
//!
//! `agy --output-format stream-json` leaves out the raw thinking string; it writes it (and each
//! tool call's `args`) to `~/.gemini/antigravity-cli/brain/<conv>/.system_generated/logs/transcript.jsonl`.
//! Everything here is plain file I/O: callers run [`read_step`] on a worker thread
//! (`gio::spawn_blocking`), never on the main thread. [`decode_args`] is pure.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::{Map, Value};

/// How much of the transcript's tail is read: recent steps live there.
const TAIL_BYTES: u64 = 65_536;

/// What the transcript holds for one step.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct StepRecord {
    /// The model's thinking text, trimmed; `None` when absent or blank.
    pub thinking: Option<String>,
    /// The step's tool calls with their `args` already decoded ([`decode_args`]).
    pub calls: Vec<ToolCall>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub name: String,
    /// An object of the call's arguments with JSON-encoded string values decoded.
    pub args: Value,
}

impl StepRecord {
    pub fn is_empty(&self) -> bool {
        self.thinking.is_none() && self.calls.is_empty()
    }
}

/// The transcript stores each arg value as a JSON-encoded string (`"\"/etc/hostname\""`, with
/// the quotes inside). This returns `args` with those values decoded to what the agent meant:
/// a string, object or array. Plain strings that are not JSON, numbers, booleans and
/// non-string values are left alone. Titles are built elsewhere; this only exposes the values.
pub fn decode_args(args: &Value) -> Value {
    let Some(map) = args.as_object() else {
        return args.clone();
    };
    let decoded: Map<String, Value> = map
        .iter()
        .map(|(key, value)| (key.clone(), decode_value(value)))
        .collect();
    Value::Object(decoded)
}

fn decode_value(value: &Value) -> Value {
    let Some(text) = value.as_str() else {
        return value.clone();
    };
    match serde_json::from_str::<Value>(text) {
        Ok(inner @ (Value::String(_) | Value::Object(_) | Value::Array(_))) => inner,
        _ => value.clone(),
    }
}

/// The transcript file for `conv_id` under `home`, newest layout first.
fn transcript_path(home: &Path, conv_id: &str) -> Option<std::path::PathBuf> {
    [".gemini/antigravity-cli/brain", ".gemini/brain"]
        .iter()
        .map(|base| {
            home.join(base)
                .join(conv_id)
                .join(".system_generated/logs/transcript.jsonl")
        })
        .find(|p| p.exists())
}

/// Reads what the transcript tail records for `target_index` (or the step before it, where the
/// planner response that carries the thinking sits). Blocking file I/O.
pub fn read_step(home: &Path, conv_id: &str, target_index: u64) -> StepRecord {
    let Some(path) = transcript_path(home, conv_id) else {
        return StepRecord::default();
    };
    read_tail(&path)
        .map(|text| find_step(&text, target_index))
        .unwrap_or_default()
}

fn read_tail(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len == 0 {
        return None;
    }
    let to_read = len.min(TAIL_BYTES) as usize;
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))
        .ok()?;
    let mut buf = vec![0u8; to_read];
    file.read_exact(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Pure: the newest thinking and the newest non-empty tool calls for `target_index`.
fn find_step(text: &str, target_index: u64) -> StepRecord {
    let mut record = StepRecord::default();
    for line in text.lines().rev() {
        if !line.contains("\"thinking\"") && !line.contains("\"tool_calls\"") {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(idx) = val.get("step_index").and_then(Value::as_u64) else {
            continue;
        };
        if idx != target_index && !(idx + 1 == target_index && target_index > 0) {
            continue;
        }
        if record.thinking.is_none() {
            record.thinking = val
                .get("thinking")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_owned);
        }
        if record.calls.is_empty() {
            record.calls = val
                .get("tool_calls")
                .and_then(Value::as_array)
                .map(|calls| {
                    calls
                        .iter()
                        .filter_map(|call| {
                            let name = call.get("name").and_then(Value::as_str)?;
                            let args = call.get("args").map(decode_args).unwrap_or(Value::Null);
                            Some(ToolCall {
                                name: name.to_owned(),
                                args,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
        }
        if record.thinking.is_some() && !record.calls.is_empty() {
            break;
        }
    }
    record
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decode_args_strips_the_extra_json_quotes() {
        let args = json!({
            "AbsolutePath": "\"/etc/hostname\"",
            "toolAction": "\"Viewing hostname file\"",
            "Lines": "[1,2]",
            "Opts": "{\"a\":1}",
            "Plain": "no json here",
            "Count": "42",
            "Flag": true
        });
        let out = decode_args(&args);
        assert_eq!(out["AbsolutePath"], "/etc/hostname");
        assert_eq!(out["toolAction"], "Viewing hostname file");
        assert_eq!(out["Lines"], json!([1, 2]));
        assert_eq!(out["Opts"], json!({"a": 1}));
        assert_eq!(out["Plain"], "no json here");
        // A bare number stays the string it was: it cannot be told from text.
        assert_eq!(out["Count"], "42");
        assert_eq!(out["Flag"], true);
        assert_eq!(decode_args(&json!("x")), json!("x"));
    }

    #[test]
    fn find_step_decodes_the_recorded_fixture_args() {
        let fixture = include_str!("../../../tests/fixtures/real/agy/transcript-view-file.jsonl");
        let record = find_step(fixture, 2);
        assert_eq!(record.thinking, None);
        assert_eq!(record.calls.len(), 1);
        assert_eq!(record.calls[0].name, "view_file");
        assert_eq!(record.calls[0].args["AbsolutePath"], "/etc/hostname");
        assert_eq!(record.calls[0].args["toolSummary"], "View /etc/hostname");
    }

    #[test]
    fn find_step_takes_the_step_or_the_one_before() {
        let text = "{\"step_index\":4,\"thinking\":\" hmm \"}\n{\"step_index\":9,\"thinking\":\"later\"}\n";
        assert_eq!(find_step(text, 5).thinking.as_deref(), Some("hmm"));
        assert_eq!(find_step(text, 4).thinking.as_deref(), Some("hmm"));
        assert!(find_step(text, 7).is_empty());
    }

    #[test]
    fn read_step_reads_the_file_and_is_empty_without_one() {
        let tmp = tempfile::tempdir().expect("tmp");
        assert!(read_step(tmp.path(), "none", 1).is_empty());
        let dir = tmp
            .path()
            .join(".gemini/antigravity-cli/brain/c1/.system_generated/logs");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("transcript.jsonl"),
            "{\"step_index\":1,\"thinking\":\"t\"}\n",
        )
        .expect("write");
        assert_eq!(
            read_step(tmp.path(), "c1", 1).thinking.as_deref(),
            Some("t")
        );
    }
}
