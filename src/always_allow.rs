//! In-app permission management: approvals remembered beyond the session.
//!
//! A rule is what "Always allow" remembers: an exact command line, a pattern/wildcard
//! (`cargo *`, `git diff*`), a prefix (`cargo test`), an exact file path, or one MCP server's
//! tool. Rules can apply to a specific workspace or globally (`workspace: "*"`).
//!
//! The file is `$XDG_STATE_HOME/agent-terminal/always-allow.json`, written `0600` through the
//! atomic writer. Settings → Agents lists the rules and removes them.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What kind of matching a rule performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PatternKind {
    #[default]
    Exact,
    Prefix,
    Wildcard,
}

/// One remembered approval rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    /// The workspace (working directory) the rule applies in, or "*" for all workspaces.
    pub workspace: String,
    /// The tool name, e.g. `run_command` or `call_mcp_tool`.
    pub tool: String,
    /// The command line, pattern, or path.
    pub detail: String,
    /// Pattern matching mode.
    #[serde(default)]
    pub kind: PatternKind,
}

impl Rule {
    /// Constructs a new rule with default pattern detection.
    pub fn new(
        workspace: impl Into<String>,
        tool: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        let detail = detail.into();
        let kind = if detail.contains('*') || detail.contains('?') {
            PatternKind::Wildcard
        } else {
            PatternKind::Exact
        };
        Self {
            workspace: workspace.into(),
            tool: tool.into(),
            detail,
            kind,
        }
    }

    /// Constructs a rule with an explicit pattern kind.
    pub fn with_kind(
        workspace: impl Into<String>,
        tool: impl Into<String>,
        detail: impl Into<String>,
        kind: PatternKind,
    ) -> Self {
        Self {
            workspace: workspace.into(),
            tool: tool.into(),
            detail: detail.into(),
            kind,
        }
    }

    /// What Settings shows: the command line or path, without any working-directory prefix.
    pub fn summary(&self) -> String {
        let detail = self
            .detail
            .rsplit_once('\n')
            .map_or(self.detail.as_str(), |(_, line)| line);
        let kind_suffix = match self.kind {
            PatternKind::Exact => "",
            PatternKind::Prefix => " (prefix)",
            PatternKind::Wildcard => " (pattern)",
        };
        let ws_prefix = if self.workspace == "*" {
            "[Global] "
        } else {
            ""
        };
        format!("{ws_prefix}{}: {detail}{kind_suffix}", self.tool)
    }

    /// Whether this rule matches an invocation in `workspace` with `tool` and `target`.
    pub fn matches(&self, workspace: &str, tool: &str, target: &str) -> bool {
        if self.workspace != "*" && self.workspace != workspace {
            return false;
        }
        if self.tool != "*" && self.tool != tool {
            return false;
        }

        let clean_target = target.rsplit_once('\n').map_or(target, |(_, line)| line);
        let clean_rule = self
            .detail
            .rsplit_once('\n')
            .map_or(self.detail.as_str(), |(_, line)| line);

        match self.kind {
            PatternKind::Exact => target == self.detail || clean_target == clean_rule,
            PatternKind::Prefix => clean_target.starts_with(clean_rule),
            PatternKind::Wildcard => matches_wildcard(clean_rule, clean_target),
        }
    }
}

/// Linear wildcard matcher supporting `*` (zero or more chars) and `?` (any single char).
pub fn matches_wildcard(pattern: &str, text: &str) -> bool {
    let p_bytes = pattern.as_bytes();
    let t_bytes = text.as_bytes();
    let mut p_idx = 0;
    let mut t_idx = 0;
    let mut star_p = None;
    let mut match_t = 0;

    while t_idx < t_bytes.len() {
        if p_idx < p_bytes.len() && (p_bytes[p_idx] == b'?' || p_bytes[p_idx] == t_bytes[t_idx]) {
            p_idx += 1;
            t_idx += 1;
        } else if p_idx < p_bytes.len() && p_bytes[p_idx] == b'*' {
            star_p = Some(p_idx);
            p_idx += 1;
            match_t = t_idx;
        } else if let Some(sp) = star_p {
            p_idx = sp + 1;
            match_t += 1;
            t_idx = match_t;
        } else {
            return false;
        }
    }

    while p_idx < p_bytes.len() && p_bytes[p_idx] == b'*' {
        p_idx += 1;
    }

    p_idx == p_bytes.len()
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlwaysRules {
    #[serde(default)]
    pub rules: Vec<Rule>,
}

impl AlwaysRules {
    /// The rules on disk; a missing or unreadable file is no rules (it never allows anything
    /// by failing).
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        agent_kit::fsutil::write_private_atomic(path, &bytes)
    }

    /// Adds a rule unless an identical rule already exists; returns whether it was newly added.
    pub fn add(&mut self, rule: Rule) -> bool {
        if self.rules.contains(&rule) {
            return false;
        }
        self.rules.push(rule);
        true
    }

    /// Whether any configured rule matches the action.
    pub fn matches_any(&self, workspace: &str, tool: &str, target: &str) -> bool {
        self.rules
            .iter()
            .any(|r| r.matches(workspace, tool, target))
    }

    /// The `(tool, detail)` keys that apply in `workspace` (including global `*` rules).
    pub fn keys_for(&self, workspace: &str) -> Vec<(String, String)> {
        self.rules
            .iter()
            .filter(|r| r.workspace == workspace || r.workspace == "*")
            .map(|r| (r.tool.clone(), r.detail.clone()))
            .collect()
    }

    /// Imports permission rules from available agent configuration files:
    /// 1. `~/git/agent-config/sync/permissions.toml`
    /// 2. `~/.claude/settings.json`
    /// 3. `~/.gemini/antigravity-cli/settings.json`
    ///
    /// Returns the number of newly added rules.
    pub fn import_agent_permissions(&mut self, home: Option<&Path>) -> usize {
        let mut added = 0;
        let home_buf = if cfg!(test) {
            home.map(PathBuf::from)
        } else {
            home.map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        };
        let Some(home) = home_buf else {
            return 0;
        };

        let perm_toml = home.join("git/agent-config/sync/permissions.toml");
        if let Ok(content) = std::fs::read_to_string(&perm_toml) {
            added += self.import_permissions_toml(&content);
        }

        let claude_settings = home.join(".claude/settings.json");
        if let Ok(content) = std::fs::read_to_string(&claude_settings) {
            added += self.import_claude_settings(&content);
        }

        let agy_settings = home.join(".gemini/antigravity-cli/settings.json");
        if let Ok(content) = std::fs::read_to_string(&agy_settings) {
            added += self.import_agy_settings(&content);
        }

        added
    }

    /// Exports permission rules from `AlwaysRules` to available agent configuration files:
    /// 1. `~/git/agent-config/sync/permissions.toml`
    /// 2. `~/.claude/settings.json`
    /// 3. `~/.gemini/antigravity-cli/settings.json`
    ///
    /// This is strictly a ONE-WAY export: it never alters `always-allow.json`.
    /// Returns the number of newly added rules across all targets.
    pub fn export_agent_permissions(&self, home: Option<&Path>) -> usize {
        let mut added = 0;
        let home_buf = if cfg!(test) {
            home.map(PathBuf::from)
        } else {
            home.map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        };
        let Some(home) = home_buf else {
            return 0;
        };

        let perm_toml = home.join("git/agent-config/sync/permissions.toml");
        if perm_toml.exists() {
            if let Ok(c) = self.export_to_permissions_toml(&perm_toml) {
                added += c;
            }
        }

        let claude_settings = home.join(".claude/settings.json");
        if claude_settings.exists() {
            if let Ok(c) = self.export_to_claude_settings(&claude_settings) {
                added += c;
            }
        }

        let agy_settings = home.join(".gemini/antigravity-cli/settings.json");
        if agy_settings.exists() {
            if let Ok(c) = self.export_to_agy_settings(&agy_settings) {
                added += c;
            }
        }

        added
    }

    /// Exports rules to `permissions.toml`.
    pub fn export_to_permissions_toml(&self, path: &Path) -> Result<usize, std::io::Error> {
        let content = std::fs::read_to_string(path)?;
        let mut new_content = content.clone();
        let mut added = 0;

        for r in &self.rules {
            let (target_array, entry) = match r.tool.as_str() {
                "run_command" | "Bash" | "bash" => {
                    let d = r.detail.trim();
                    match r.kind {
                        PatternKind::Exact => ("shell_exact", d.to_string()),
                        PatternKind::Prefix => ("shell", d.trim_end_matches('*').to_string()),
                        PatternKind::Wildcard => {
                            if d.ends_with('*') && !d[..d.len() - 1].contains('*') {
                                ("shell", d.trim_end_matches('*').to_string())
                            } else {
                                ("shell_glob", d.to_string())
                            }
                        }
                    }
                }
                "call_mcp_tool" => ("mcp", r.detail.trim().to_string()),
                _ => continue,
            };

            if entry.is_empty() || new_content.contains(&format!("\"{entry}\"")) {
                continue;
            }

            let needle = format!("{target_array} = [");
            if let Some(pos) = new_content.find(&needle) {
                let after_needle = pos + needle.len();
                let insert_pos = if let Some(newline_pos) = new_content[after_needle..].find('\n') {
                    after_needle + newline_pos + 1
                } else {
                    after_needle
                };
                new_content.insert_str(insert_pos, &format!("    \"{entry}\",\n"));
                added += 1;
            } else if let Some(allow_pos) = new_content.find("[allow]") {
                let insert_pos = allow_pos + "[allow]\n".len();
                new_content.insert_str(
                    insert_pos,
                    &format!("{target_array} = [\n    \"{entry}\",\n]\n"),
                );
                added += 1;
            }
        }

        if added > 0 {
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, new_content)?;
            std::fs::rename(&tmp, path)?;
        }
        Ok(added)
    }

    /// Exports rules to Claude `settings.json`.
    pub fn export_to_claude_settings(&self, path: &Path) -> Result<usize, std::io::Error> {
        let content = std::fs::read_to_string(path)?;
        let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&content) else {
            return Ok(0);
        };
        let mut added = 0;

        let permissions_obj = val.as_object_mut().and_then(|root| {
            if !root.contains_key("permissions") {
                root.insert("permissions".to_owned(), serde_json::json!({}));
            }
            root.get_mut("permissions").and_then(|p| p.as_object_mut())
        });

        let Some(perms) = permissions_obj else {
            return Ok(0);
        };

        if !perms.contains_key("allow") {
            perms.insert("allow".to_owned(), serde_json::json!([]));
        }
        let Some(allow_list) = perms.get_mut("allow").and_then(|a| a.as_array_mut()) else {
            return Ok(0);
        };

        for r in &self.rules {
            let entry = match r.tool.as_str() {
                "run_command" | "Bash" | "bash" => {
                    let d = r.detail.trim();
                    match r.kind {
                        PatternKind::Exact => format!("Bash({d})"),
                        PatternKind::Prefix => {
                            format!("Bash({}:*)", d.trim_end_matches('*'))
                        }
                        PatternKind::Wildcard => {
                            if d.ends_with('*') && !d[..d.len() - 1].contains('*') {
                                format!("Bash({}:*)", d.trim_end_matches('*'))
                            } else {
                                format!("Bash({d})")
                            }
                        }
                    }
                }
                "call_mcp_tool" => {
                    format!("mcp__{}", r.detail.trim().replace('/', "__"))
                }
                _ => continue,
            };

            let json_str = serde_json::Value::String(entry);
            if !allow_list.contains(&json_str) {
                allow_list.push(json_str);
                added += 1;
            }
        }

        if added > 0 {
            let formatted = serde_json::to_string_pretty(&val).map_err(std::io::Error::other)?;
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, formatted)?;
            std::fs::rename(&tmp, path)?;
        }
        Ok(added)
    }

    /// Exports rules to Antigravity `settings.json`.
    pub fn export_to_agy_settings(&self, path: &Path) -> Result<usize, std::io::Error> {
        let content = std::fs::read_to_string(path)?;
        let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&content) else {
            return Ok(0);
        };
        let mut added = 0;

        let permissions_obj = val.as_object_mut().and_then(|root| {
            if !root.contains_key("permissions") {
                root.insert("permissions".to_owned(), serde_json::json!({}));
            }
            root.get_mut("permissions").and_then(|p| p.as_object_mut())
        });

        let Some(perms) = permissions_obj else {
            return Ok(0);
        };

        if !perms.contains_key("allow") {
            perms.insert("allow".to_owned(), serde_json::json!([]));
        }
        let Some(allow_list) = perms.get_mut("allow").and_then(|a| a.as_array_mut()) else {
            return Ok(0);
        };

        for r in &self.rules {
            let entry = match r.tool.as_str() {
                "run_command" | "Bash" | "bash" => {
                    let d = r.detail.trim();
                    if d.ends_with('*') && !d[..d.len() - 1].contains('*') {
                        format!("command({})", d.trim_end_matches('*'))
                    } else {
                        format!("command({d})")
                    }
                }
                "call_mcp_tool" => {
                    format!("mcp({})", r.detail.trim())
                }
                _ => continue,
            };

            let json_str = serde_json::Value::String(entry);
            if !allow_list.contains(&json_str) {
                allow_list.push(json_str);
                added += 1;
            }
        }

        if added > 0 {
            let formatted = serde_json::to_string_pretty(&val).map_err(std::io::Error::other)?;
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, formatted)?;
            std::fs::rename(&tmp, path)?;
        }
        Ok(added)
    }

    /// Parses allow rules from `permissions.toml`.
    pub fn import_permissions_toml(&mut self, content: &str) -> usize {
        let mut count = 0;
        let mut in_allow_section = false;
        let mut current_array = "";

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') || trimmed.is_empty() {
                continue;
            }
            if trimmed.starts_with('[') {
                in_allow_section = trimmed == "[allow]";
                current_array = "";
                continue;
            }
            if !in_allow_section {
                continue;
            }
            if let Some((key, _)) = trimmed.split_once('=') {
                current_array = key.trim();
            }
            if let Some(inner) = trimmed.strip_prefix('"') {
                let end = inner.find('"').unwrap_or(0);
                if end > 0 {
                    let val = &inner[..end];
                    if current_array == "shell" {
                        let pat = if val.contains('*') {
                            val.to_string()
                        } else {
                            format!("{val}*")
                        };
                        if self.add(Rule::with_kind(
                            "*",
                            "run_command",
                            pat,
                            PatternKind::Wildcard,
                        )) {
                            count += 1;
                        }
                    } else if current_array == "shell_glob" {
                        if self.add(Rule::with_kind(
                            "*",
                            "run_command",
                            val,
                            PatternKind::Wildcard,
                        )) {
                            count += 1;
                        }
                    } else if current_array == "shell_exact" {
                        if self.add(Rule::with_kind("*", "run_command", val, PatternKind::Exact)) {
                            count += 1;
                        }
                    } else if current_array == "mcp"
                        && self.add(Rule::with_kind(
                            "*",
                            "call_mcp_tool",
                            val,
                            PatternKind::Prefix,
                        ))
                    {
                        count += 1;
                    }
                }
            }
        }
        count
    }

    /// Parses allow rules from Claude `settings.json`.
    pub fn import_claude_settings(&mut self, content: &str) -> usize {
        let mut count = 0;
        let Ok(v) = serde_json::from_str::<serde_json::Value>(content) else {
            return 0;
        };
        let Some(allows) = v
            .get("permissions")
            .and_then(|p| p.get("allow"))
            .and_then(|a| a.as_array())
        else {
            return 0;
        };

        for item in allows {
            let Some(s) = item.as_str() else { continue };
            if let Some(rest) = s.strip_prefix("Bash(").and_then(|r| r.strip_suffix(')')) {
                let cmd = rest
                    .strip_suffix(":*")
                    .or_else(|| rest.strip_suffix("*)"))
                    .unwrap_or(rest);
                let pat = if cmd.contains('*') {
                    cmd.to_string()
                } else {
                    format!("{cmd}*")
                };
                if self.add(Rule::with_kind(
                    "*",
                    "run_command",
                    pat,
                    PatternKind::Wildcard,
                )) {
                    count += 1;
                }
            } else if let Some(mcp) = s.strip_prefix("mcp__") {
                let tool_spec = mcp.replace("__", "/");
                if self.add(Rule::with_kind(
                    "*",
                    "call_mcp_tool",
                    tool_spec,
                    PatternKind::Prefix,
                )) {
                    count += 1;
                }
            }
        }
        count
    }

    /// Parses allow rules from Antigravity `settings.json`.
    pub fn import_agy_settings(&mut self, content: &str) -> usize {
        let mut count = 0;
        let Ok(v) = serde_json::from_str::<serde_json::Value>(content) else {
            return 0;
        };
        let Some(allows) = v
            .get("permissions")
            .and_then(|p| p.get("allow"))
            .and_then(|a| a.as_array())
        else {
            return 0;
        };

        for item in allows {
            let Some(s) = item.as_str() else { continue };
            if let Some(cmd) = s.strip_prefix("command(").and_then(|r| r.strip_suffix(')')) {
                let pat = if cmd.contains('*') {
                    cmd.to_string()
                } else {
                    format!("{cmd}*")
                };
                if self.add(Rule::with_kind(
                    "*",
                    "run_command",
                    pat,
                    PatternKind::Wildcard,
                )) {
                    count += 1;
                }
            }
        }
        count
    }
}

/// `$XDG_STATE_HOME/agent-terminal/always-allow.json` (else `~/.local/state/…`); takes the values
/// so tests never read the real environment.
pub fn default_path(xdg_state_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    agent_kit::store::default_path(xdg_state_home, home)
        .and_then(|db| db.parent().map(|dir| dir.join("always-allow.json")))
}

/// [`default_path`] from this process's environment; `None` under test, so no test reads or
/// writes the real rules.
pub fn path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    default_path(
        std::env::var("XDG_STATE_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn rule(workspace: &str, detail: &str) -> Rule {
        Rule::new(workspace, "run_command", detail)
    }

    #[test]
    fn rules_are_per_workspace_deduplicated_private_and_survive_a_reload() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("state").join("always-allow.json");
        let mut rules = AlwaysRules::load(&path);
        assert!(rules.rules.is_empty(), "no file is no rules");
        assert!(rules.add(rule("/a", "/a\ncargo fmt")));
        assert!(!rules.add(rule("/a", "/a\ncargo fmt")), "no duplicate");
        assert!(rules.add(rule("/b", "/b\nls")));
        rules.save(&path).expect("save");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        let back = AlwaysRules::load(&path);
        assert_eq!(back, rules);
        assert_eq!(
            back.keys_for("/a"),
            [("run_command".to_owned(), "/a\ncargo fmt".to_owned())]
        );
        assert!(back.keys_for("/elsewhere").is_empty());
        assert_eq!(back.rules[0].summary(), "run_command: cargo fmt");
    }

    #[test]
    fn pattern_matching_supports_wildcards_and_prefixes() {
        let mut rules = AlwaysRules::default();
        rules.add(Rule::with_kind(
            "*",
            "run_command",
            "cargo test*",
            PatternKind::Wildcard,
        ));
        rules.add(Rule::with_kind(
            "/repo",
            "run_command",
            "git status",
            PatternKind::Exact,
        ));
        rules.add(Rule::with_kind(
            "*",
            "call_mcp_tool",
            "hive-board/*",
            PatternKind::Wildcard,
        ));

        assert!(rules.matches_any("/repo", "run_command", "cargo test --workspace"));
        assert!(rules.matches_any("/other", "run_command", "cargo test -p crate"));
        assert!(!rules.matches_any("/other", "run_command", "cargo build"));

        assert!(rules.matches_any("/repo", "run_command", "git status"));
        assert!(!rules.matches_any("/other", "run_command", "git status"));

        assert!(rules.matches_any("/repo", "call_mcp_tool", "hive-board/board_get_messages"));
        assert!(!rules.matches_any("/repo", "call_mcp_tool", "other-server/tool"));
    }

    #[test]
    fn wildcard_matcher_handles_globs() {
        assert!(matches_wildcard("cargo *", "cargo build"));
        assert!(matches_wildcard("cargo *", "cargo test --workspace"));
        assert!(!matches_wildcard("cargo *", "make test"));
        assert!(matches_wildcard("git diff*", "git diff --stat"));
        assert!(matches_wildcard("*.rs", "main.rs"));
        assert!(matches_wildcard("test_?", "test_1"));
        assert!(!matches_wildcard("test_?", "test_12"));
    }

    #[test]
    fn import_permissions_from_toml_and_json() {
        let toml_sample = r#"
[allow]
shell = [
    "cargo test",
    "fdfind",
]
shell_glob = [
    "git *",
]
shell_exact = [
    "env",
]
mcp = [
    "hive-board/get_messages",
]
"#;
        let mut rules = AlwaysRules::default();
        let count = rules.import_permissions_toml(toml_sample);
        assert_eq!(count, 5);
        assert!(rules.matches_any("/any", "run_command", "cargo test --lib"));
        assert!(rules.matches_any("/any", "run_command", "git status"));
        assert!(rules.matches_any("/any", "run_command", "env"));
        assert!(!rules.matches_any("/any", "run_command", "env -i"));

        let claude_sample = r#"{
            "permissions": {
                "allow": [
                    "Bash(cargo clippy:*)",
                    "mcp__hive-board__board_list"
                ]
            }
        }"#;
        let c_count = rules.import_claude_settings(claude_sample);
        assert_eq!(c_count, 2);
        assert!(rules.matches_any("/any", "run_command", "cargo clippy --workspace"));
        assert!(rules.matches_any("/any", "call_mcp_tool", "hive-board/board_list"));
    }

    #[test]
    fn a_corrupt_file_allows_nothing() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("always-allow.json");
        std::fs::write(&path, "{ not json").expect("write");
        assert!(AlwaysRules::load(&path).rules.is_empty());
    }

    #[test]
    fn the_file_sits_beside_the_thread_store() {
        assert_eq!(
            default_path(Some("/s"), Some("/h")),
            Some(PathBuf::from("/s/agent-terminal/always-allow.json"))
        );
        assert_eq!(
            default_path(None, Some("/h")),
            Some(PathBuf::from(
                "/h/.local/state/agent-terminal/always-allow.json"
            ))
        );
    }

    #[test]
    fn one_way_import_and_export() {
        let dir = tempfile::tempdir().expect("tmp");
        let toml_path = dir.path().join("permissions.toml");
        std::fs::write(
            &toml_path,
            "[allow]\nshell = [\n    \"fdfind\",\n]\nshell_exact = [\n    \"env\",\n]\n",
        )
        .expect("write toml");

        let claude_path = dir.path().join("claude_settings.json");
        std::fs::write(
            &claude_path,
            r#"{"permissions":{"allow":["Bash(cargo check:*)"]}}"#,
        )
        .expect("write claude");

        let agy_path = dir.path().join("agy_settings.json");
        std::fs::write(&agy_path, r#"{"permissions":{"allow":["command(date)"]}}"#)
            .expect("write agy");

        // 1. One-way Import: loads external rules into AlwaysRules, external files are untouched.
        let mut rules = AlwaysRules::default();
        let toml_content = std::fs::read_to_string(&toml_path).unwrap();
        assert_eq!(rules.import_permissions_toml(&toml_content), 2);
        let claude_content = std::fs::read_to_string(&claude_path).unwrap();
        assert_eq!(rules.import_claude_settings(&claude_content), 1);
        let agy_content = std::fs::read_to_string(&agy_path).unwrap();
        assert_eq!(rules.import_agy_settings(&agy_content), 1);

        assert_eq!(rules.rules.len(), 4);
        assert!(rules.matches_any("/ws", "run_command", "fdfind foo"));
        assert!(rules.matches_any("/ws", "run_command", "cargo check --quiet"));
        assert!(rules.matches_any("/ws", "run_command", "date"));

        // 2. Add an Agent Terminal-originated rule
        rules.add(Rule::with_kind(
            "*",
            "run_command",
            "make test*",
            PatternKind::Wildcard,
        ));
        rules.add(Rule::with_kind(
            "*",
            "call_mcp_tool",
            "test-mcp/tool",
            PatternKind::Prefix,
        ));

        // 3. One-way Export: pushes rules to external files without altering AlwaysRules.
        let exp_toml = rules
            .export_to_permissions_toml(&toml_path)
            .expect("export toml");
        assert!(exp_toml > 0);
        let updated_toml = std::fs::read_to_string(&toml_path).unwrap();
        assert!(updated_toml.contains("\"make test\""));
        assert!(updated_toml.contains("\"test-mcp/tool\""));

        let exp_claude = rules
            .export_to_claude_settings(&claude_path)
            .expect("export claude");
        assert!(exp_claude > 0);
        let updated_claude = std::fs::read_to_string(&claude_path).unwrap();
        assert!(updated_claude.contains("Bash(make test:*)"));
        assert!(updated_claude.contains("mcp__test-mcp__tool"));

        let exp_agy = rules.export_to_agy_settings(&agy_path).expect("export agy");
        assert!(exp_agy > 0);
        let updated_agy = std::fs::read_to_string(&agy_path).unwrap();
        assert!(updated_agy.contains("command(make test)"));
        assert!(updated_agy.contains("mcp(test-mcp/tool)"));

        // Rules themselves are untouched (strict one-way)
        assert_eq!(rules.rules.len(), 6);
    }
}
