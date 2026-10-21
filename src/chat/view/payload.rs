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

/// The end of a background task's output (`get_task_output`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaskOutput {
    pub text: String,
    /// The whole output's size; `None` when the reply does not say.
    pub total_bytes: Option<u64>,
    /// `text` is only the end of a longer output.
    pub truncated: bool,
}

/// `get_task_output` (Claude: `output`, `total_bytes`, `truncated`).
pub fn task_output(v: &Value) -> TaskOutput {
    TaskOutput {
        text: str_field(v, &["output", "text"])
            .unwrap_or_default()
            .to_owned(),
        total_bytes: u64_field(v, &["total_bytes", "totalBytes"]),
        truncated: v.get("truncated").and_then(Value::as_bool) == Some(true),
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

/// A tool call's title in the one grammar every agent shares (`agent_core::tool_title`):
/// `Read <path>`, `Search <pattern>`, `Run <command>`, `Edit <path>`, `Create <path>`,
/// `Fetch <url>`, `<server>/<tool>`. The card header, the approval preview and the sub-agent
/// shelf all take their text from here, so they always agree. Streamed input that is not
/// parsed yet titles by the tool alone; the raw input stays under the card's details.
pub fn tool_title(
    kind: ItemKind,
    raw_title: &str,
    input: Option<&Value>,
    input_text: &str,
) -> agent_core::tool_title::ToolTitle {
    let parsed;
    let val = match input {
        Some(v) if !v.is_null() => Some(v),
        _ => {
            parsed = serde_json::from_str::<Value>(input_text).ok();
            parsed.as_ref()
        }
    };
    agent_core::tool_title::classify(kind, raw_title, val)
}

/// [`tool_title`] as the card header's (action, target) pair.
pub fn card_title_and_summary(
    kind: ItemKind,
    raw_title: &str,
    input: Option<&Value>,
    input_text: &str,
) -> (String, String) {
    let title = tool_title(kind, raw_title, input, input_text);
    (title.action, title.target)
}

/// Pretty, human-readable input for a card body: command lines, clear action/target summaries,
/// or indented JSON.
pub fn tool_input_text(kind: ItemKind, input: Option<&Value>, input_text: &str) -> String {
    match input {
        Some(v) if !v.is_null() => {
            if kind == ItemKind::Command {
                if let Some(cmd) = str_field(v, &["command", "CommandLine", "cmd"]) {
                    let mut s = format!("$ {cmd}");
                    if let Some(cwd) = str_field(v, &["cwd", "Cwd"]) {
                        s.push_str(&format!("\nWorking directory: {cwd}"));
                    }
                    if let Some(desc) = str_field(v, &["description", "Description", "toolAction"])
                    {
                        s.push_str(&format!("\nAction: {desc}"));
                    }
                    return s;
                }
            }
            if matches!(kind, ItemKind::FileRead | ItemKind::FileChange) {
                let mut lines = Vec::new();
                if let Some(action) = str_field(
                    v,
                    &[
                        "toolAction",
                        "toolSummary",
                        "Description",
                        "Instruction",
                        "description",
                        "instruction",
                    ],
                ) {
                    lines.push(format!("Action: {action}"));
                }
                if let Some(pat) = str_field(v, &["pattern", "query"]) {
                    lines.push(format!("Pattern: {pat}"));
                }
                if let Some(path) = str_field(
                    v,
                    &[
                        "file_path",
                        "TargetFile",
                        "path",
                        "AbsolutePath",
                        "notebook_path",
                        "DirectoryPath",
                        "SearchPath",
                    ],
                ) {
                    let label = if v.get("DirectoryPath").is_some() || v.get("SearchPath").is_some()
                    {
                        "Directory"
                    } else {
                        "File"
                    };
                    lines.push(format!("{label}: {path}"));
                }
                if let (Some(start), Some(end)) = (v.get("StartLine"), v.get("EndLine")) {
                    lines.push(format!("Lines: {start} – {end}"));
                }
                if !lines.is_empty()
                    && (v.get("old_string").is_none() && v.get("TargetContent").is_none())
                {
                    return lines.join("\n");
                }
            }
            if kind == ItemKind::WebSearch {
                let mut lines = Vec::new();
                if let Some(action) = str_field(v, &["toolAction", "toolSummary"]) {
                    lines.push(format!("Action: {action}"));
                }
                if let Some(q) = str_field(v, &["query", "q", "search_query"]) {
                    lines.push(format!("Query: {q}"));
                }
                if let Some(u) = str_field(v, &["url", "Url"]) {
                    lines.push(format!("URL: {u}"));
                }
                if !lines.is_empty() {
                    return lines.join("\n");
                }
            }
            if kind == ItemKind::McpTool {
                let mut lines = Vec::new();
                if let (Some(server), Some(tool)) = (
                    str_field(v, &["ServerName", "server"]),
                    str_field(v, &["ToolName", "tool"]),
                ) {
                    lines.push(format!("Tool: {server}.{tool}"));
                }
                if let Some(action) = str_field(
                    v,
                    &["toolAction", "toolSummary", "Description", "description"],
                ) {
                    lines.push(format!("Action: {action}"));
                }
                if let Some(args) = v
                    .get("Arguments")
                    .or_else(|| v.get("arguments"))
                    .or_else(|| v.get("args"))
                {
                    if let Ok(pretty) = serde_json::to_string_pretty(args) {
                        lines.push(format!("Arguments:\n{pretty}"));
                    }
                }
                if !lines.is_empty() {
                    return lines.join("\n");
                }
            }
            if kind == ItemKind::Subagent {
                let mut lines = Vec::new();
                if let Some(role) = str_field(v, &["role", "Role", "subagent_type", "TypeName"]) {
                    lines.push(format!("Subagent: {role}"));
                }
                if let Some(desc) = str_field(
                    v,
                    &["description", "Description", "toolAction", "toolSummary"],
                ) {
                    lines.push(format!("Action: {desc}"));
                }
                if let Some(prompt) = str_field(v, &["prompt", "Prompt"]) {
                    lines.push(format!("Task: {prompt}"));
                }
                if !lines.is_empty() {
                    return lines.join("\n");
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
    fn task_output_reads_claude_fields_and_tolerates_gaps() {
        assert_eq!(
            task_output(&json!({"output": "ok\n", "total_bytes": 9000, "truncated": true})),
            TaskOutput {
                text: "ok\n".to_owned(),
                total_bytes: Some(9000),
                truncated: true,
            }
        );
        assert_eq!(task_output(&json!(null)), TaskOutput::default());
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
            tool_title(ItemKind::Command, "Bash", Some(&cmd), "").text(),
            "Run cargo test…"
        );
        // Streamed input, parsed once whole; a fragment titles by the tool alone.
        assert_eq!(
            tool_title(
                ItemKind::FileRead,
                "Read",
                None,
                r#"{"file_path":"src/a.rs"}"#
            )
            .text(),
            "Read src/a.rs"
        );
        assert_eq!(
            tool_title(ItemKind::FileRead, "Read", None, "{\"file_pa").text(),
            "Read"
        );
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
    fn card_titles_and_summaries_are_distinct_actions_never_bare_file_names() {
        let (action, target) = card_title_and_summary(
            ItemKind::FileRead,
            "/work/repo/calc.py",
            Some(&json!({"AbsolutePath": "/work/repo/calc.py"})),
            "",
        );
        assert_eq!(action, "Read");
        assert_eq!(target, "/work/repo/calc.py");

        let (action, target) = card_title_and_summary(
            ItemKind::FileRead,
            "list_dir",
            Some(&json!({"DirectoryPath": "src/chat/view"})),
            "",
        );
        assert_eq!(action, "Read");
        assert_eq!(target, "src/chat/view");

        let (action, target) = card_title_and_summary(
            ItemKind::FileRead,
            "grep_search",
            Some(&json!({"SearchPath": "src", "pattern": "diff"})),
            "",
        );
        assert_eq!(action, "Search");
        assert_eq!(target, "diff in src");

        let (action, target) = card_title_and_summary(
            ItemKind::FileChange,
            "/work/repo/calc.py",
            Some(&json!({"TargetFile": "/work/repo/calc.py"})),
            "",
        );
        assert_eq!(action, "Edit");
        assert_eq!(target, "/work/repo/calc.py");

        let (action, target) = card_title_and_summary(
            ItemKind::FileChange,
            "write_to_file",
            Some(&json!({"TargetFile": "src/new.rs", "CodeContent": "fn main() {}"})),
            "",
        );
        assert_eq!(action, "Create");
        assert_eq!(target, "src/new.rs");

        let (action, target) = card_title_and_summary(
            ItemKind::Command,
            "false",
            Some(&json!({"CommandLine": "cargo test"})),
            "",
        );
        assert_eq!(action, "Run");
        assert_eq!(target, "cargo test");

        let (action, target) = card_title_and_summary(
            ItemKind::WebSearch,
            "read_url_content",
            Some(&json!({"url": "https://nuvek.ca"})),
            "",
        );
        assert_eq!(action, "Fetch");
        assert_eq!(target, "https://nuvek.ca");

        let (action, target) = card_title_and_summary(
            ItemKind::McpTool,
            "call_mcp_tool",
            Some(&json!({"ServerName": "unraid", "ToolName": "unraid_containers"})),
            "",
        );
        assert_eq!(action, "unraid/unraid_containers");
        assert_eq!(
            target, "",
            "an MCP title is <server>/<tool>; arguments are details"
        );

        // Codex's wrapped command shows as typed.
        let (action, target) = card_title_and_summary(
            ItemKind::Command,
            "/usr/bin/zsh -lc 'ls -la'",
            Some(&json!({"command": "/usr/bin/zsh -lc 'ls -la'", "cwd": "/w"})),
            "",
        );
        assert_eq!((action.as_str(), target.as_str()), ("Run", "ls -la"));
    }

    #[test]
    fn formatted_tool_input_renders_readable_details() {
        let formatted = tool_input_text(
            ItemKind::FileRead,
            Some(&json!({
                "AbsolutePath": "src/cards.rs",
                "toolAction": "Viewing ToolCard",
                "StartLine": 10,
                "EndLine": 50
            })),
            "",
        );
        assert!(formatted.contains("Action: Viewing ToolCard"));
        assert!(formatted.contains("File: src/cards.rs"));
        assert!(formatted.contains("Lines: 10 – 50"));

        let cmd = tool_input_text(
            ItemKind::Command,
            Some(&json!({
                "CommandLine": "git status",
                "Cwd": "/home/user",
                "toolAction": "Checking status"
            })),
            "",
        );
        assert!(cmd.contains("$ git status"));
        assert!(cmd.contains("Working directory: /home/user"));
        assert!(cmd.contains("Action: Checking status"));
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
