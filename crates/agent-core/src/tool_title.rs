//! One title grammar for every agent's tool cards.
//!
//! `Read <path>`, `Search <pattern>`, `Run <command>`, `Edit <path>`, `Create <path>`,
//! `Fetch <url>` and `<server>/<tool>`, whichever agent ran the tool and whatever it named it.
//! The raw argv or JSON is not part of the title: the card puts it under Details.
//!
//! Pure: [`classify`] works from what an `ItemStarted` carries (its kind, its title, which is the
//! native tool name for Claude and the most telling parameter for agy, and its input), so stored
//! history from any build gets the same titles. agy's transcript stores each argument as a
//! JSON-encoded string with its quotes ([`decode_agy_args`]); [`classify`] reads such values
//! decoded too. Codex runs every command as `/usr/bin/zsh -lc '…'`: [`wrapper_body_for_display`]
//! takes the one strict wrapper shape apart for display, and [`from_command_actions`] reads
//! Codex's `commandActions` once its adapter passes them in the item's input.

use serde_json::{Map, Value};

use crate::event::ItemKind;

/// What a tool call does, whichever agent names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Read,
    Search,
    Run,
    Edit,
    Create,
    Fetch,
    /// An MCP tool: the title is `<server>/<tool>`.
    Mcp,
    Subagent,
    /// Anything else: the title is the tool's own name.
    Other,
}

impl ToolKind {
    /// The title's verb (an MCP or unknown tool has none: its name is the title).
    pub fn verb(self) -> Option<&'static str> {
        match self {
            Self::Read => Some("Read"),
            Self::Search => Some("Search"),
            Self::Run => Some("Run"),
            Self::Edit => Some("Edit"),
            Self::Create => Some("Create"),
            Self::Fetch => Some("Fetch"),
            Self::Subagent => Some("Subagent"),
            Self::Mcp | Self::Other => None,
        }
    }
}

/// A card's title: its action and what it acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolTitle {
    pub kind: ToolKind,
    /// The verb (`Read`, `Run`, …), or `<server>/<tool>`, or the tool's own name.
    pub action: String,
    /// One line, at most [`TARGET_MAX`] characters: a path, pattern, command or URL; may be empty.
    pub target: String,
}

impl ToolTitle {
    /// The whole title: `Read src/lib.rs`, `unraid/unraid_containers`.
    pub fn text(&self) -> String {
        if self.target.is_empty() {
            self.action.clone()
        } else {
            format!("{} {}", self.action, self.target)
        }
    }
}

/// Characters of a target shown before it is cut with `…`.
pub const TARGET_MAX: usize = 140;

const PATH_KEYS: &[&str] = &[
    "file_path",
    "TargetFile",
    "AbsolutePath",
    "path",
    "notebook_path",
    "filePath",
    "filepath",
    "DirectoryPath",
];
const COMMAND_KEYS: &[&str] = &["command", "CommandLine", "cmd"];
const PATTERN_KEYS: &[&str] = &["pattern", "Pattern", "query", "Query", "SearchPattern"];
const SEARCH_DIR_KEYS: &[&str] = &["SearchPath", "SearchDirectory", "path", "DirectoryPath"];
const URL_KEYS: &[&str] = &["url", "Url", "URL"];

/// The kind a native tool name stands for (Claude, agy and Codex names), if the name says.
fn kind_of_name(name: &str) -> Option<ToolKind> {
    Some(match name {
        "Bash" | "bash" | "run_command" | "send_command_input" | "command_execution"
        | "commandExecution" | "shell" | "local_shell" => ToolKind::Run,
        "Read" | "NotebookRead" | "LS" | "view_file" | "list_dir" | "view_code_item"
        | "view_file_outline" => ToolKind::Read,
        "Grep" | "Glob" | "grep_search" | "find_by_name" | "codebase_search" | "WebSearch"
        | "search_web" | "web_search" | "webSearch" => ToolKind::Search,
        "Edit"
        | "MultiEdit"
        | "NotebookEdit"
        | "replace_file_content"
        | "multi_replace_file_content"
        | "sed_file"
        | "notebook_edit"
        | "apply_patch"
        | "file_change"
        | "fileChange" => ToolKind::Edit,
        "Write" | "write_to_file" => ToolKind::Create,
        "WebFetch" | "read_url_content" => ToolKind::Fetch,
        "Task" | "Agent" | "start_subagent" | "invoke_subagent" => ToolKind::Subagent,
        "call_mcp_tool" => ToolKind::Mcp,
        n if n.starts_with("mcp__") => ToolKind::Mcp,
        _ => return None,
    })
}

/// The kind an item kind implies when the name says nothing.
fn kind_of_item(kind: ItemKind, input: Option<&Value>) -> ToolKind {
    match kind {
        ItemKind::Command => ToolKind::Run,
        ItemKind::FileRead if input.is_some_and(|i| str_field(i, PATTERN_KEYS).is_some()) => {
            ToolKind::Search
        }
        ItemKind::FileRead => ToolKind::Read,
        ItemKind::FileChange => ToolKind::Edit,
        ItemKind::WebSearch
            if input.is_some_and(|i| str_field(i, &["query", "Query"]).is_none())
                && input.is_some_and(|i| str_field(i, URL_KEYS).is_some()) =>
        {
            ToolKind::Fetch
        }
        ItemKind::WebSearch => ToolKind::Search,
        ItemKind::McpTool => ToolKind::Mcp,
        ItemKind::Subagent => ToolKind::Subagent,
        _ => ToolKind::Other,
    }
}

/// The title of a tool card. `name` is the item's title (a native tool name, or for agy the
/// most telling parameter, with the tool name in `input.tool_name`); `input` its structured
/// input, if any.
pub fn classify(item: ItemKind, name: &str, input: Option<&Value>) -> ToolTitle {
    let input = input.filter(|v| !v.is_null());
    // Codex's own reading of a shell command, when the adapter passes it on.
    if let Some(title) = input
        .and_then(|i| i.get("commandActions"))
        .and_then(from_command_actions)
    {
        return title;
    }
    let native = input
        .and_then(|i| str_field(i, &["tool_name"]))
        .unwrap_or_else(|| name.to_owned());
    let kind = kind_of_name(&native)
        .or_else(|| kind_of_name(name))
        .unwrap_or_else(|| kind_of_item(item, input));
    let empty = Value::Object(Map::new());
    let args = input.unwrap_or(&empty);
    let kind = if kind == ToolKind::Edit && only_adds(args) {
        ToolKind::Create
    } else {
        kind
    };
    let target = match kind {
        ToolKind::Run => str_field(args, COMMAND_KEYS)
            .or_else(|| argv_field(args, COMMAND_KEYS))
            .map(|c| display_command(&c))
            .unwrap_or_else(|| name_as_target(name, &["command", "Bash", "bash"])),
        ToolKind::Read => str_field(args, PATH_KEYS).unwrap_or_else(|| path_like(name)),
        ToolKind::Search => match (
            str_field(args, PATTERN_KEYS),
            str_field(args, SEARCH_DIR_KEYS),
        ) {
            (Some(pat), Some(dir)) if !dir.is_empty() && dir != "." => {
                format!("{} in {dir}", first_line(&pat))
            }
            (Some(pat), _) => pat,
            (None, _) => String::new(),
        },
        ToolKind::Edit | ToolKind::Create => {
            let paths = edit_paths(args);
            if paths.is_empty() {
                path_like(name)
            } else {
                paths.join(", ")
            }
        }
        ToolKind::Fetch => str_field(args, URL_KEYS).unwrap_or_default(),
        ToolKind::Subagent => {
            str_field(args, &["description", "subagent_type", "prompt", "Prompt"])
                .unwrap_or_default()
        }
        ToolKind::Mcp => {
            return ToolTitle {
                kind,
                action: mcp_name(&native, args),
                target: String::new(),
            };
        }
        ToolKind::Other => {
            let target = str_field(
                args,
                &[
                    "command",
                    "file_path",
                    "path",
                    "query",
                    "url",
                    "pattern",
                    "description",
                ],
            )
            .unwrap_or_default();
            let action = if native.is_empty() || looks_like_path(&native) {
                "Tool".to_owned()
            } else {
                native
            };
            return ToolTitle {
                kind,
                action,
                target: one_line(&target),
            };
        }
    };
    ToolTitle {
        kind,
        action: kind.verb().unwrap_or("Tool").to_owned(),
        target: one_line(&target),
    }
}

/// `<server>/<tool>`: Claude `mcp__server__tool`, agy `call_mcp_tool` {ServerName, ToolName},
/// Codex `server.tool`.
fn mcp_name(name: &str, args: &Value) -> String {
    if let Some((server, tool)) = name
        .strip_prefix("mcp__")
        .and_then(|rest| rest.split_once("__"))
    {
        return format!("{server}/{tool}");
    }
    let server = str_field(args, &["ServerName", "server_name", "server", "Server"]);
    let tool = str_field(args, &["ToolName", "tool_name_mcp", "tool", "Name"]);
    match (server, tool) {
        (Some(s), Some(t)) => format!("{s}/{t}"),
        _ => match name.split_once('.') {
            Some((s, t)) if !s.is_empty() && !t.is_empty() && !name.contains(' ') => {
                format!("{s}/{t}")
            }
            _ if !name.is_empty() && name != "call_mcp_tool" => name.to_owned(),
            _ => "MCP tool".to_owned(),
        },
    }
}

/// Every path an edit names: one (`file_path`, `TargetFile`, …), Codex's `changes[].path`, or an
/// array of changes as the input itself.
fn edit_paths(args: &Value) -> Vec<String> {
    let changes = args
        .as_array()
        .or_else(|| args.get("changes").and_then(Value::as_array));
    if let Some(changes) = changes {
        let paths: Vec<String> = changes
            .iter()
            .filter_map(|c| c.get("path").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();
        if !paths.is_empty() {
            return paths;
        }
    }
    str_field(args, PATH_KEYS).into_iter().collect()
}

/// Whether a Codex patch (`changes`, or the changes as the input itself) only creates files:
/// every change is `kind.type == "add"`. A patch that also updates, deletes or moves stays an
/// edit.
fn only_adds(args: &Value) -> bool {
    let changes = args
        .as_array()
        .or_else(|| args.get("changes").and_then(Value::as_array));
    changes.is_some_and(|changes| {
        !changes.is_empty()
            && changes
                .iter()
                .all(|c| c.pointer("/kind/type").and_then(Value::as_str) == Some("add"))
    })
}

/// The command a Codex `commandActions` list describes, in the grammar: a single `read` is
/// `Read <path>`, a `search` is `Search <query>`, a `listFiles` with a path is `Read <path>`.
/// Anything else (several actions, `unknown`, no path) is `None`: the card titles the command
/// itself (`Run <command>`).
pub fn from_command_actions(actions: &Value) -> Option<ToolTitle> {
    let [action] = actions.as_array()?.as_slice() else {
        return None;
    };
    let path = str_field(action, &["path"]);
    let (kind, target) = match action.get("type").and_then(Value::as_str)? {
        "read" => (ToolKind::Read, path?),
        "listFiles" => (ToolKind::Read, path?),
        "search" => {
            let query = str_field(action, &["query"])?;
            match path {
                Some(p) if !p.is_empty() && p != "." => {
                    (ToolKind::Search, format!("{} in {p}", first_line(&query)))
                }
                _ => (ToolKind::Search, query),
            }
        }
        _ => return None,
    };
    Some(ToolTitle {
        kind,
        action: kind.verb().unwrap_or("Tool").to_owned(),
        target: one_line(&target),
    })
}

/// A command as the user would type it: the body of the one strict wrapper shape
/// ([`wrapper_body_for_display`]), else the command itself.
pub fn display_command(command: &str) -> String {
    wrapper_body_for_display(command).unwrap_or_else(|| command.trim().to_owned())
}

/// The system shell-wrapper shape: `sh|bash|zsh` (bare, `/bin/` or `/usr/bin/`) then `-c` or
/// `-lc` (Codex runs every command as `/usr/bin/zsh -lc '…'`). [`wrapper_body_for_display`]
/// (titles) judges by it alone; the app's fail-closed `always_allow::unwrap_shell_wrapper`
/// (rules) also accepts the account's own login shell by its exact path, which this crate,
/// doing no I/O, cannot know.
pub fn is_strict_wrapper(shell: &str, flag: &str) -> bool {
    let known = ["", "/bin/", "/usr/bin/"].iter().any(|dir| {
        shell
            .strip_prefix(dir)
            .is_some_and(|name| ["sh", "bash", "zsh"].contains(&name))
    });
    known && (flag == "-c" || flag == "-lc")
}

/// The command line a strict wrapper ([`is_strict_wrapper`] plus exactly one argument) runs,
/// verbatim, for display: `/usr/bin/zsh -lc 'ls; id'` titles as `ls; id`. Unlike the app's
/// `always_allow::unwrap_shell_wrapper`, which refuses shell syntax anywhere because a rule is
/// made from its result, this keeps any body. Any other shape (options, several arguments,
/// unquoted shell syntax, unbalanced quotes) is `None`.
pub fn wrapper_body_for_display(command: &str) -> Option<String> {
    let words = shell_words(command)?;
    let [shell, flag, body] = words.as_slice() else {
        return None;
    };
    is_strict_wrapper(shell, flag).then(|| body.clone())
}

/// POSIX-style words: single quotes literal, double quotes with `\` escaping `"`, `\`, `$`,
/// backtick and newline, `\` outside quotes escaping the next character. `None` for unbalanced
/// quotes or any unquoted operator, substitution or redirection (then the line is not one
/// simple command).
// Not the app's `always_allow::simple_command_tokens`: that one refuses shell syntax even inside
// quotes (a rule is made from its result), while a title keeps a quoted `ls; id` body as typed.
fn shell_words(line: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.trim().chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        ch => word.push(ch),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            esc @ ('"' | '\\' | '$' | '`') => word.push(esc),
                            '\n' => {}
                            other => {
                                word.push('\\');
                                word.push(other);
                            }
                        },
                        ch => word.push(ch),
                    }
                }
            }
            '\\' => {
                in_word = true;
                word.push(chars.next()?);
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            ';' | '&' | '|' | '<' | '>' | '(' | ')' | '`' | '$' => return None,
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Some(words)
}

/// agy's transcript arguments with their extra quotes taken off: every string value that is
/// itself a JSON string (`"\"/etc/hostname\""`) becomes that string (`"/etc/hostname"`); other
/// values are kept as they are. Non-objects are returned unchanged.
pub fn decode_agy_args(args: &Value) -> Value {
    match args {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let v = match v {
                        Value::String(s) => Value::String(unquote(s).unwrap_or_else(|| s.clone())),
                        other => other.clone(),
                    };
                    (k.clone(), v)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// `"\"text\""` → `text`; `None` for a value that is not a JSON-encoded string.
fn unquote(s: &str) -> Option<String> {
    let t = s.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        serde_json::from_str::<String>(t).ok()
    } else {
        None
    }
}

/// The first non-empty string under `keys`, with agy's extra quotes taken off.
fn str_field(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| {
        let s = v.get(*k)?.as_str()?;
        let s = unquote(s).unwrap_or_else(|| s.to_owned());
        (!s.trim().is_empty()).then_some(s)
    })
}

/// A command given as an argv array (`["ls", "-la"]`), joined with spaces.
fn argv_field(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| {
        let parts: Vec<&str> = v
            .get(*k)?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .collect();
        (!parts.is_empty()).then(|| parts.join(" "))
    })
}

/// The item title as the target, when it is not just the tool's own name.
fn name_as_target(name: &str, generic: &[&str]) -> String {
    if name.is_empty() || generic.contains(&name) || kind_of_name(name).is_some() {
        String::new()
    } else {
        name.to_owned()
    }
}

/// The item title as a path target (agy titles a step by its path), else nothing.
fn path_like(name: &str) -> String {
    if looks_like_path(name) {
        name.to_owned()
    } else {
        String::new()
    }
}

fn looks_like_path(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty()
        && !s.contains(' ')
        && (s.contains('/')
            || s.contains('\\')
            || s.rsplit_once('.').is_some_and(|(stem, ext)| {
                !stem.is_empty()
                    && (1..=5).contains(&ext.len())
                    && ext.chars().all(char::is_alphanumeric)
            }))
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s).trim()
}

/// The first non-blank line, at most [`TARGET_MAX`] characters, `…` when anything was cut.
fn one_line(s: &str) -> String {
    let line = first_line(s.trim());
    let mut out: String = line.chars().take(TARGET_MAX).collect();
    if line.chars().count() > TARGET_MAX || s.trim().lines().count() > 1 {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn title(item: ItemKind, name: &str, input: Value) -> String {
        classify(item, name, Some(&input)).text()
    }

    /// The frames of one agent, as recorded.
    fn frames(fixture: &str) -> Vec<Value> {
        fixture
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .collect()
    }

    /// Golden titles from the Claude recording: its `Bash touch x.tmp` call.
    #[test]
    fn golden_titles_claude_w0() {
        let recorded = frames(include_str!(
            "../../../tests/fixtures/real/claude/thinking-and-touch.jsonl"
        ));
        let call = recorded
            .iter()
            .filter(|r| r["dir"] == "out" && r["frame"]["type"] == "assistant")
            .flat_map(|r| {
                r["frame"]["message"]["content"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .find(|b| b["type"] == "tool_use")
            .expect("W0 tool_use");
        assert_eq!(
            title(
                ItemKind::Command,
                call["name"].as_str().unwrap(),
                call["input"].clone()
            ),
            "Run touch x.tmp"
        );
        // The rest of Claude's tools, as its adapter starts them (title = tool name).
        let cases = [
            (
                ItemKind::FileRead,
                "Read",
                json!({"file_path": "/w/src/lib.rs"}),
                "Read /w/src/lib.rs",
            ),
            (
                ItemKind::FileRead,
                "Grep",
                json!({"pattern": "fn main", "path": "src"}),
                "Search fn main in src",
            ),
            (
                ItemKind::FileRead,
                "Glob",
                json!({"pattern": "**/*.rs"}),
                "Search **/*.rs",
            ),
            (
                ItemKind::FileChange,
                "Edit",
                json!({"file_path": "a.rs", "old_string": "x", "new_string": "y"}),
                "Edit a.rs",
            ),
            (
                ItemKind::FileChange,
                "MultiEdit",
                json!({"file_path": "a.rs", "edits": []}),
                "Edit a.rs",
            ),
            (
                ItemKind::FileChange,
                "Write",
                json!({"file_path": "new.rs", "content": "x"}),
                "Create new.rs",
            ),
            (
                ItemKind::WebSearch,
                "WebFetch",
                json!({"url": "https://example.com", "prompt": "p"}),
                "Fetch https://example.com",
            ),
            (
                ItemKind::WebSearch,
                "WebSearch",
                json!({"query": "rust gtk"}),
                "Search rust gtk",
            ),
            (
                ItemKind::McpTool,
                "mcp__unraid__unraid_containers",
                json!({}),
                "unraid/unraid_containers",
            ),
            (
                ItemKind::McpTool,
                "mcp__claude_ai_Claude_Docs__batch",
                json!({}),
                "claude_ai_Claude_Docs/batch",
            ),
            (
                ItemKind::Subagent,
                "Agent",
                json!({"description": "Find callers", "prompt": "…"}),
                "Subagent Find callers",
            ),
            (
                ItemKind::Tool,
                "TodoWrite",
                json!({"todos": []}),
                "TodoWrite",
            ),
        ];
        for (kind, name, input, want) in cases {
            assert_eq!(title(kind, name, input), want, "{name}");
        }
    }

    /// Golden titles from the agy recordings: the live `view_file` step (titled by its path,
    /// tool name in the input, as the adapter starts it) and the transcript's quoted args.
    #[test]
    fn golden_titles_agy_w0() {
        let recorded = frames(include_str!(
            "../../../tests/fixtures/real/agy/read-outside-project.jsonl"
        ));
        let step = recorded
            .iter()
            .find_map(|r| {
                r.get("step_update")
                    .filter(|s| s["tool_name"] == "view_file")
            })
            .expect("W0 view_file step");
        let mut params = step["tool_info"]["parameters"].clone();
        params["tool_name"] = json!("view_file");
        assert_eq!(
            title(ItemKind::FileRead, "/etc/hostname", params),
            "Read /etc/hostname"
        );

        let transcript = frames(include_str!(
            "../../../tests/fixtures/real/agy/transcript-view-file.jsonl"
        ));
        let call = &transcript[0]["tool_calls"][0];
        let args = &call["args"];
        assert_eq!(
            args["AbsolutePath"], "\"/etc/hostname\"",
            "W0: quoted values"
        );
        let decoded = decode_agy_args(args);
        assert_eq!(decoded["AbsolutePath"], "/etc/hostname");
        assert_eq!(decoded["toolSummary"], "View /etc/hostname");
        // Titled the same whether the reader decoded them or not.
        let name = call["name"].as_str().unwrap();
        assert_eq!(
            title(ItemKind::FileRead, name, decoded),
            "Read /etc/hostname"
        );
        assert_eq!(
            title(ItemKind::FileRead, name, args.clone()),
            "Read /etc/hostname"
        );

        let cases = [
            (
                ItemKind::Command,
                "cargo test",
                json!({"CommandLine": "cargo test", "tool_name": "run_command"}),
                "Run cargo test",
            ),
            (
                ItemKind::FileRead,
                "src",
                json!({"DirectoryPath": "src", "tool_name": "list_dir"}),
                "Read src",
            ),
            (
                ItemKind::FileRead,
                "src",
                json!({"SearchPath": "src", "Query": "diff", "tool_name": "grep_search"}),
                "Search diff in src",
            ),
            (
                ItemKind::FileRead,
                "find_by_name",
                json!({"Pattern": "*.rs", "tool_name": "find_by_name"}),
                "Search *.rs",
            ),
            (
                ItemKind::FileChange,
                "replace_file_content",
                json!({"TargetFile": "/w/a.py", "tool_name": "replace_file_content"}),
                "Edit /w/a.py",
            ),
            (
                ItemKind::FileChange,
                "write_to_file",
                json!({"TargetFile": "src/new.rs", "CodeContent": "x", "tool_name": "write_to_file"}),
                "Create src/new.rs",
            ),
            (
                ItemKind::WebSearch,
                "https://nuvek.ca",
                json!({"Url": "https://nuvek.ca", "tool_name": "read_url_content"}),
                "Fetch https://nuvek.ca",
            ),
            (
                ItemKind::WebSearch,
                "gtk4",
                json!({"query": "gtk4", "tool_name": "search_web"}),
                "Search gtk4",
            ),
            (
                ItemKind::McpTool,
                "call_mcp_tool",
                json!({"ServerName": "unraid", "ToolName": "unraid_containers", "tool_name": "call_mcp_tool"}),
                "unraid/unraid_containers",
            ),
        ];
        for (kind, name, input, want) in cases {
            assert_eq!(title(kind, name, input), want, "{name}");
        }
    }

    /// Golden titles from the Codex recordings, through the shared helpers its adapter will
    /// use: the wrapped command, its `commandActions`, a patch's paths, an MCP call.
    #[test]
    fn golden_titles_codex_w0() {
        let recorded = frames(include_str!(
            "../../../tests/fixtures/real/codex/ask-shell-decline.jsonl"
        ));
        let item = recorded
            .iter()
            .find_map(|r| {
                let params = &r["frame"]["params"];
                (params["item"]["type"] == "commandExecution").then(|| params["item"].clone())
            })
            .expect("W0 commandExecution item");
        let command = item["command"].as_str().expect("command");
        assert_eq!(command, "/usr/bin/zsh -lc 'ls -la'");
        // As codex.rs starts it today (title = command, input = {command, cwd}).
        let input = json!({"command": command, "cwd": item["cwd"]});
        assert_eq!(title(ItemKind::Command, command, input), "Run ls -la");
        // With its commandActions passed on (the Codex hook point).
        assert_eq!(
            from_command_actions(&item["commandActions"]),
            None,
            "listFiles with no path titles the command"
        );
        let read =
            json!([{"type": "read", "command": "cat a.rs", "name": "a.rs", "path": "/w/a.rs"}]);
        assert_eq!(
            classify(
                ItemKind::Command,
                command,
                Some(&json!({"command": command, "commandActions": read}))
            )
            .text(),
            "Read /w/a.rs"
        );
        let search =
            json!([{"type": "search", "command": "rg diff src", "query": "diff", "path": "src"}]);
        assert_eq!(
            from_command_actions(&search).unwrap().text(),
            "Search diff in src"
        );

        let edit = frames(include_str!(
            "../../../tests/fixtures/real/codex/ask-edit-decline.jsonl"
        ));
        let changes = edit
            .iter()
            .find_map(|r| {
                let item = &r["frame"]["params"]["item"];
                (item["type"] == "fileChange").then(|| item["changes"].clone())
            })
            .expect("W0 fileChange item");
        let path = changes[0]["path"].as_str().expect("path").to_owned();
        assert_eq!(changes[0]["kind"]["type"], "update");
        assert_eq!(
            title(ItemKind::FileChange, &path, changes.clone()),
            format!("Edit {path}")
        );
        // The same patch as a new file is a Create; mixed with an update it stays an Edit.
        let mut created = changes.clone();
        created[0]["kind"] = json!({"type": "add"});
        assert_eq!(
            title(ItemKind::FileChange, &path, created.clone()),
            format!("Create {path}")
        );
        assert_eq!(
            title(
                ItemKind::FileChange,
                &path,
                json!({"changes": created.clone()})
            ),
            format!("Create {path}"),
            "the changes under `changes` too"
        );
        let mut mixed = created.as_array().expect("changes").clone();
        mixed.extend(changes.as_array().expect("changes").iter().cloned());
        assert_eq!(
            title(ItemKind::FileChange, &path, Value::Array(mixed)),
            format!("Edit {path}, {path}")
        );
        let mut deleted = changes.clone();
        deleted[0]["kind"] = json!({"type": "delete"});
        assert_eq!(
            title(ItemKind::FileChange, &path, deleted),
            format!("Edit {path}")
        );
        assert_eq!(
            title(ItemKind::FileChange, "file change", json!([])),
            "Edit",
            "no changes is not a Create"
        );
        assert_eq!(
            title(ItemKind::McpTool, "unraid.unraid_containers", json!({})),
            "unraid/unraid_containers"
        );
    }

    #[test]
    fn only_the_strict_wrapper_shape_is_unwrapped() {
        for (line, body) in [
            ("/usr/bin/zsh -lc 'ls -la'", "ls -la"),
            ("bash -c 'cargo test; echo done'", "cargo test; echo done"),
            ("/bin/sh -c \"echo \\\"hi\\\"\"", "echo \"hi\""),
            ("zsh -lc 'echo '\\''quoted'\\'''", "echo 'quoted'"),
        ] {
            assert_eq!(
                wrapper_body_for_display(line).as_deref(),
                Some(body),
                "{line}"
            );
        }
        for line in [
            "ls -la",
            "zsh -lc 'a' extra",
            "zsh -x -c 'a'",
            "fish -c 'a'",
            "/opt/bin/zsh -lc 'a'",
            "zsh -lc 'a",
            "zsh -lc a; id",
            "zsh -lc $(id)",
        ] {
            assert_eq!(wrapper_body_for_display(line), None, "{line}");
        }
        assert_eq!(display_command("  git status "), "git status");
        assert_eq!(
            display_command("/usr/bin/zsh -lc 'git status'"),
            "git status"
        );
    }

    #[test]
    fn targets_are_one_line_and_bounded() {
        let long = "x".repeat(200);
        let t = classify(ItemKind::Command, "Bash", Some(&json!({"command": long})));
        assert_eq!(t.target.chars().count(), TARGET_MAX + 1);
        assert!(t.target.ends_with('…'));
        let t = classify(ItemKind::Command, "Bash", Some(&json!({"command": "a\nb"})));
        assert_eq!(t.target, "a…");
        // A leading blank line is not the command: "\nls\n" titles `Run ls`; with more after it,
        // the cut is still marked.
        let t = classify(
            ItemKind::Command,
            "Bash",
            Some(&json!({"command": "\nls\n"})),
        );
        assert_eq!(t.text(), "Run ls");
        let t = classify(
            ItemKind::Command,
            "Bash",
            Some(&json!({"command": "\nls\nx"})),
        );
        assert_eq!(t.text(), "Run ls…");
        // No input at all: the verb alone, never a bare tool name as a target.
        assert_eq!(classify(ItemKind::Command, "Bash", None).text(), "Run");
        assert_eq!(classify(ItemKind::FileRead, "Read", None).text(), "Read");
        assert_eq!(classify(ItemKind::Tool, "", None).text(), "Tool");
    }
}
