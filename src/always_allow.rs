//! agy's "Always allow": approvals the user chose to remember beyond the session.
//!
//! A rule is exactly what "Allow for session" remembers ([`crate::approval_server::session_key`]):
//! an exact command line in an exact working directory, an exact file path, or one MCP server's
//! tool. Never a prefix or a pattern. Rules are kept per workspace, so allowing `cargo fmt`-like
//! lines in one repository says nothing about another, and the mode policy still runs first (Plan
//! refuses edits whatever is remembered).
//!
//! The file is `$XDG_STATE_HOME/agent-terminal/always-allow.json`, written `0600` through the
//! atomic writer. Settings → Agents lists the rules and removes them. Claude keeps its own
//! "always" rules in the project's `.claude/settings.local.json`; Codex has no such option.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One remembered approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    /// The workspace (agy's working directory) the rule applies in.
    pub workspace: String,
    /// The agy tool, e.g. `run_command`.
    pub tool: String,
    /// The exact detail [`crate::approval_server::session_key`] built (command line, path…).
    pub detail: String,
}

impl Rule {
    /// What Settings shows: the command line or path, without the working-directory prefix.
    pub fn summary(&self) -> String {
        let detail = self
            .detail
            .rsplit_once('\n')
            .map_or(self.detail.as_str(), |(_, line)| line);
        format!("{}: {detail}", self.tool)
    }
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

    /// Adds a rule unless it is already there; whether it was new.
    pub fn add(&mut self, rule: Rule) -> bool {
        if self.rules.contains(&rule) {
            return false;
        }
        self.rules.push(rule);
        true
    }

    /// The `(tool, detail)` keys that apply in `workspace`.
    pub fn keys_for(&self, workspace: &str) -> Vec<(String, String)> {
        self.rules
            .iter()
            .filter(|r| r.workspace == workspace)
            .map(|r| (r.tool.clone(), r.detail.clone()))
            .collect()
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
        Rule {
            workspace: workspace.into(),
            tool: "run_command".into(),
            detail: detail.into(),
        }
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
}
