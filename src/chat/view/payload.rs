//! Lenient readers for control-reply payloads and tool inputs (pure; no GTK).
//!
//! Agents answer the same control with different shapes (Claude `list_models` objects, agy's
//! `{id, display}` rows from `agy models`), and those shapes drift between CLI versions. Every
//! reader here accepts the known variants, ignores unknown fields and never fails: a payload it
//! cannot read yields an empty list, and the panel falls back to showing the raw JSON.

use agent_core::event::{ItemKind, Question};
use serde_json::{Map, Value};

fn str_field<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .filter(|s| !s.trim().is_empty())
}

fn u64_field(v: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|k| {
        v.get(*k)
            .and_then(|x| x.as_u64().or_else(|| x.as_f64().map(|f| f.max(0.0) as u64)))
    })
}

/// The list in `v` itself, or under the first of `keys` that holds one.
fn list<'a>(v: &'a Value, keys: &[&str]) -> &'a [Value] {
    if let Value::Array(a) = v {
        return a;
    }
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_array))
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntry {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

/// `list_models` (Claude: `{models:[{value, displayName, description}]}` or a bare list;
/// agy: `[{id, display}]`).
pub fn models(v: &Value) -> Vec<ModelEntry> {
    list(v, &["models", "availableModels", "items"])
        .iter()
        .filter_map(|m| {
            if let Some(s) = m.as_str() {
                return Some(ModelEntry {
                    id: s.to_owned(),
                    label: s.to_owned(),
                    description: None,
                });
            }
            let id = str_field(m, &["value", "id", "model", "resolvedModel", "name"])?;
            let label = str_field(m, &["displayName", "display", "label", "name"]).unwrap_or(id);
            Some(ModelEntry {
                id: id.to_owned(),
                label: label.to_owned(),
                description: str_field(m, &["description"]).map(str::to_owned),
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    pub name: String,
    pub status: String,
    pub enabled: bool,
    pub detail: Option<String>,
}

/// `mcp_status` (Claude: `{mcpServers:[{name, status, serverInfo}]}`).
pub fn mcp_servers(v: &Value) -> Vec<McpServer> {
    list(v, &["mcpServers", "servers", "mcp_servers"])
        .iter()
        .filter_map(|s| {
            let name = str_field(s, &["name", "id"])?.to_owned();
            let status = str_field(s, &["status", "state"])
                .unwrap_or("unknown")
                .to_owned();
            let enabled = s
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(status != "disabled");
            let detail = s
                .get("serverInfo")
                .and_then(|i| {
                    let name = str_field(i, &["name"])?;
                    Some(match str_field(i, &["version"]) {
                        Some(ver) => format!("{name} {ver}"),
                        None => name.to_owned(),
                    })
                })
                .or_else(|| str_field(s, &["error", "scope"]).map(str::to_owned));
            Some(McpServer {
                name,
                status,
                enabled,
                detail,
            })
        })
        .collect()
}

/// `file_suggestions`: a list of strings, or objects with a path, at the top or under a key.
pub fn file_suggestions(v: &Value) -> Vec<String> {
    list(v, &["suggestions", "files", "items", "results"])
        .iter()
        .filter_map(|f| match f {
            Value::String(s) => Some(s.clone()),
            other => str_field(other, &["path", "displayPath", "file", "name"]).map(str::to_owned),
        })
        .filter(|s| !s.trim().is_empty())
        .take(50)
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContextBreakdown {
    pub used: Option<u64>,
    pub max: Option<u64>,
    pub auto_compact_at: Option<u64>,
    pub categories: Vec<(String, u64)>,
}

/// `get_context_usage` (Claude: `totalTokens`, `maxTokens`, `autoCompactThreshold`,
/// `categories:[{name, tokens}]`).
pub fn context_breakdown(v: &Value) -> ContextBreakdown {
    let categories = list(v, &["categories"])
        .iter()
        .filter_map(|c| {
            Some((
                str_field(c, &["name", "label"])?.to_owned(),
                u64_field(c, &["tokens", "count"])?,
            ))
        })
        .collect();
    ContextBreakdown {
        used: u64_field(v, &["totalTokens", "used", "tokens"]),
        max: u64_field(v, &["maxTokens", "max", "contextWindow"]),
        auto_compact_at: u64_field(v, &["autoCompactThreshold", "autoCompactAt"]),
        categories,
    }
}

/// Flattens JSON into `(dotted.path, value)` rows for the read-only settings and usage views.
/// Scalars render as JSON; empty containers as `{}` / `[]`. Long values are cut at `max_len`.
pub fn flatten(v: &Value, max_len: usize) -> Vec<(String, String)> {
    fn walk(prefix: &str, v: &Value, max_len: usize, out: &mut Vec<(String, String)>) {
        let key = |k: &str| {
            if prefix.is_empty() {
                k.to_owned()
            } else {
                format!("{prefix}.{k}")
            }
        };
        match v {
            Value::Object(m) if !m.is_empty() => {
                for (k, child) in m {
                    walk(&key(k), child, max_len, out);
                }
            }
            Value::Array(a) if !a.is_empty() && a.iter().any(|x| x.is_object() || x.is_array()) => {
                for (i, child) in a.iter().enumerate() {
                    walk(&format!("{prefix}[{i}]"), child, max_len, out);
                }
            }
            other => {
                let mut s = match other {
                    Value::String(s) => s.clone(),
                    _ => other.to_string(),
                };
                if s.chars().count() > max_len {
                    s = s.chars().take(max_len).collect::<String>() + "…";
                }
                let label = if prefix.is_empty() { "(value)" } else { prefix };
                out.push((label.to_owned(), s));
            }
        }
    }
    let mut out = Vec::new();
    walk("", v, max_len, &mut out);
    out
}

/// The settings view prefers `effective` (Claude `get_settings` returns applied/effective/sources).
pub fn effective_settings(v: &Value) -> &Value {
    v.get("effective").unwrap_or(v)
}

/// One line for a tool card's header: the command, the path, the query.
pub fn tool_summary(kind: ItemKind, input: Option<&Value>, input_text: &str) -> Option<String> {
    let parsed;
    let input = match input {
        Some(v) if !v.is_null() => v,
        _ => {
            parsed = serde_json::from_str::<Value>(input_text).ok()?;
            &parsed
        }
    };
    let keys: &[&str] = match kind {
        ItemKind::Command => &["command", "CommandLine", "cmd"],
        ItemKind::FileChange | ItemKind::FileRead => &[
            "file_path",
            "path",
            "TargetFile",
            "AbsolutePath",
            "notebook_path",
        ],
        ItemKind::WebSearch => &["query", "url"],
        ItemKind::Subagent => &["description", "subagent_type", "prompt"],
        _ => &[
            "command",
            "file_path",
            "path",
            "query",
            "url",
            "pattern",
            "description",
        ],
    };
    let s = str_field(input, keys)?;
    let line = s.lines().next().unwrap_or(s).trim();
    let mut out: String = line.chars().take(140).collect();
    if line.chars().count() > 140 || s.lines().count() > 1 {
        out.push('…');
    }
    Some(out)
}

/// Pretty input for a card body: the command line for commands, otherwise indented JSON.
pub fn tool_input_text(kind: ItemKind, input: Option<&Value>, input_text: &str) -> String {
    match input {
        Some(v) if !v.is_null() => {
            if kind == ItemKind::Command {
                if let Some(cmd) = str_field(v, &["command", "CommandLine", "cmd"]) {
                    return format!("$ {cmd}");
                }
            }
            serde_json::to_string_pretty(v).unwrap_or_default()
        }
        _ => match serde_json::from_str::<Value>(input_text) {
            Ok(v) => tool_input_text(kind, Some(&v), ""),
            Err(_) => input_text.to_owned(),
        },
    }
}

/// The last `max` lines of `text` (tool output matters most at its end), with a note of how
/// many were cut. Short text is returned unchanged.
pub fn cap_lines(text: &str, max: usize) -> String {
    let total = text.lines().count();
    if total <= max {
        return text.to_owned();
    }
    let kept: Vec<&str> = text.lines().skip(total - max).collect();
    format!(
        "… {} earlier lines not shown\n{}",
        total - max,
        kept.join("\n")
    )
}

/// The answers payload for `answer_questions`: `{question text: chosen label(s)}`, multiple
/// labels joined with ", " (the shape Claude's AskUserQuestion takes). `selected[q]` holds the
/// chosen option indices of question `q`. Returns `None` until every question has an answer.
pub fn answers(questions: &[Question], selected: &[Vec<usize>]) -> Option<Value> {
    let mut map = Map::new();
    for (i, q) in questions.iter().enumerate() {
        let picks = selected.get(i)?;
        let labels: Vec<&str> = picks
            .iter()
            .filter_map(|&o| q.options.get(o).map(|opt| opt.label.as_str()))
            .collect();
        if labels.is_empty() {
            return None;
        }
        let labels = if q.multi_select {
            labels
        } else {
            labels.into_iter().take(1).collect()
        };
        map.insert(q.question.clone(), Value::String(labels.join(", ")));
    }
    Some(Value::Object(map))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::event::QuestionOption;
    use serde_json::json;

    #[test]
    fn models_from_claude_and_agy_shapes() {
        let claude = json!({"models": [
            {"value": "opus", "displayName": "Opus 5.5", "description": "Most capable"},
            {"value": "sonnet"}
        ]});
        let m = models(&claude);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].label, "Opus 5.5");
        assert_eq!(m[0].description.as_deref(), Some("Most capable"));
        assert_eq!(m[1].label, "sonnet");
        let agy = json!([{"id": "gemini-3.1-pro", "display": "Gemini 3.1 Pro"}, "flash"]);
        let m = models(&agy);
        assert_eq!(m[0].id, "gemini-3.1-pro");
        assert_eq!(m[1].id, "flash");
        assert!(models(&json!({"weird": 1})).is_empty());
    }

    #[test]
    fn mcp_servers_read_status_and_enabled() {
        let v = json!({"mcpServers": [
            {"name": "git", "status": "connected", "serverInfo": {"name": "git-mcp", "version": "1.2"}},
            {"name": "web", "status": "disabled"},
            {"name": "x", "status": "failed", "error": "spawn ENOENT", "enabled": true},
            {"status": "nameless"}
        ]});
        let s = mcp_servers(&v);
        assert_eq!(s.len(), 3);
        assert!(s[0].enabled);
        assert_eq!(s[0].detail.as_deref(), Some("git-mcp 1.2"));
        assert!(!s[1].enabled);
        assert_eq!(s[2].detail.as_deref(), Some("spawn ENOENT"));
    }

    #[test]
    fn file_suggestions_accept_strings_and_objects() {
        assert_eq!(
            file_suggestions(&json!(["a.rs", {"path": "b/c.rs"}, {"nope": 1}, ""])),
            ["a.rs", "b/c.rs"]
        );
        assert_eq!(
            file_suggestions(&json!({"suggestions": [{"displayPath": "d.md"}]})),
            ["d.md"]
        );
        assert!(file_suggestions(&json!(null)).is_empty());
    }

    #[test]
    fn context_breakdown_reads_claude_fields() {
        let c = context_breakdown(&json!({
            "totalTokens": 21478, "maxTokens": 200000, "autoCompactThreshold": 160000,
            "categories": [{"name": "System prompt", "tokens": 3000}, {"name": "bad"}]
        }));
        assert_eq!(c.used, Some(21_478));
        assert_eq!(c.max, Some(200_000));
        assert_eq!(c.auto_compact_at, Some(160_000));
        assert_eq!(c.categories, [("System prompt".to_owned(), 3_000)]);
    }

    #[test]
    fn flatten_walks_objects_and_object_arrays() {
        let rows = flatten(
            &json!({"a": {"b": 1, "c": "x"}, "list": [1, 2], "objs": [{"k": true}], "e": {}}),
            100,
        );
        assert_eq!(
            rows,
            [
                ("a.b".to_owned(), "1".to_owned()),
                ("a.c".to_owned(), "x".to_owned()),
                ("e".to_owned(), "{}".to_owned()),
                ("list".to_owned(), "[1,2]".to_owned()),
                ("objs[0].k".to_owned(), "true".to_owned()),
            ]
        );
        assert_eq!(
            flatten(&json!("abcdef"), 3),
            [("(value)".to_owned(), "abc…".to_owned())]
        );
        assert_eq!(
            effective_settings(&json!({"effective": {"x": 1}})),
            &json!({"x": 1})
        );
    }

    #[test]
    fn tool_summaries() {
        let cmd = json!({"command": "cargo test\n--all"});
        assert_eq!(
            tool_summary(ItemKind::Command, Some(&cmd), "").as_deref(),
            Some("cargo test…")
        );
        assert_eq!(
            tool_summary(ItemKind::FileRead, None, r#"{"file_path":"src/a.rs"}"#).as_deref(),
            Some("src/a.rs")
        );
        assert_eq!(tool_summary(ItemKind::FileRead, None, "{\"file_pa"), None);
        assert_eq!(
            tool_input_text(ItemKind::Command, Some(&json!({"command": "ls"})), ""),
            "$ ls"
        );
        assert_eq!(
            tool_input_text(ItemKind::Tool, None, "partial {"),
            "partial {"
        );
    }

    #[test]
    fn long_output_keeps_its_tail() {
        assert_eq!(cap_lines("a\nb", 2), "a\nb");
        assert_eq!(
            cap_lines("1\n2\n3\n4", 2),
            "… 2 earlier lines not shown\n3\n4"
        );
    }

    fn q(text: &str, multi: bool) -> Question {
        Question {
            id: text.to_owned(),
            header: "H".into(),
            question: text.to_owned(),
            options: ["A", "B", "C"]
                .iter()
                .map(|l| QuestionOption {
                    label: (*l).to_owned(),
                    description: None,
                })
                .collect(),
            multi_select: multi,
        }
    }

    #[test]
    fn answers_need_every_question_and_honour_multi_select() {
        let qs = [q("One?", false), q("Many?", true)];
        assert_eq!(answers(&qs, &[vec![0], vec![]]), None);
        assert_eq!(answers(&qs, &[vec![0]]), None);
        assert_eq!(
            answers(&qs, &[vec![1, 2], vec![0, 2]]),
            Some(json!({"One?": "B", "Many?": "A, C"}))
        );
    }
}
