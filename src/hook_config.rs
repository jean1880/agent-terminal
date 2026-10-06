//! Is agy's approval hook actually installed? agy runs with `--dangerously-skip-permissions`
//! only when the answer is yes, so this is checked before an [`crate::approval_server`] socket
//! is bound for it: a missing hook means plan mode, never an ungated agent.
//!
//! The file is `$HOME/.gemini/config/hooks.json`. Its shape (named groups, each mapping an
//! event to entries that are either a bare command or a `matcher` with nested `hooks`) is the
//! one agy and agent-sync write. Parsing is pure; only [`check_installed`] reads the file.

use std::path::PathBuf;

use serde_json::Value;

/// What the hook command line must contain.
const HOOK_FLAG: &str = "--approval-hook";

/// `$HOME/.gemini/config/hooks.json`; takes the value so tests never read the real home.
pub fn hooks_path(home: Option<&str>) -> Option<PathBuf> {
    let home = home.filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".gemini/config/hooks.json"))
}

/// True when some `PreToolUse` entry runs `--approval-hook` and matches every tool: a matcher
/// of `.*`, or none. A narrower matcher leaves tools ungated, so it does not count.
pub fn approval_hook_installed(json: &str) -> bool {
    let Ok(root) = serde_json::from_str::<Value>(json) else {
        return false;
    };
    let Some(groups) = root.as_object() else {
        return false;
    };
    groups
        .values()
        .filter_map(|g| g.get("PreToolUse").and_then(Value::as_array))
        .flatten()
        .any(entry_gates_everything)
}

fn entry_gates_everything(entry: &Value) -> bool {
    let matcher_ok = match entry.get("matcher") {
        None | Some(Value::Null) => true,
        Some(Value::String(m)) => m == ".*",
        Some(_) => false,
    };
    if !matcher_ok {
        return false;
    }
    let runs_hook = |v: &Value| {
        v.get("command")
            .and_then(Value::as_str)
            .is_some_and(|c| c.contains(HOOK_FLAG))
    };
    runs_hook(entry)
        || entry
            .get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|hooks| hooks.iter().any(runs_hook))
}

/// Reads the real file. `Err` carries the reason to show the user.
pub fn check_installed(home: Option<&str>) -> Result<(), String> {
    let path = hooks_path(home).ok_or_else(|| "HOME is not set".to_owned())?;
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if approval_hook_installed(&text) {
        Ok(())
    } else {
        Err(format!(
            "{} has no PreToolUse entry running `agent-terminal --approval-hook` with matcher `.*`",
            path.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"{
      "other": {"PostToolUse": [{"matcher": "run_command", "hooks": [{"type": "command", "command": "x"}]}]},
      "agent-terminal-approval": {"PreToolUse": [{"matcher": ".*", "hooks": [
        {"type": "command", "timeout": 600,
         "command": "bash -c '[ -n \"$AGENT_TERMINAL_APPROVAL_SOCKET\" ] || exit 0; exec \"${AGENT_TERMINAL_HOOK_BIN:-agent-terminal}\" --approval-hook'"}]}]}
    }"#;

    #[test]
    fn accepts_the_staged_entry_and_a_matcherless_one() {
        assert!(approval_hook_installed(GOOD));
        let bare = r#"{"g":{"PreToolUse":[{"type":"command","command":"agent-terminal --approval-hook"}]}}"#;
        assert!(approval_hook_installed(bare));
    }

    #[test]
    fn rejects_everything_that_leaves_tools_ungated() {
        // A narrow matcher.
        let narrow = GOOD.replace("\".*\"", "\"run_command|write_to_file\"");
        assert!(!approval_hook_installed(&narrow));
        // The right command under the wrong event.
        let post = GOOD.replace("PreToolUse", "PostToolUse");
        assert!(!approval_hook_installed(&post));
        // No approval hook at all (the user's own guards only).
        let guards = r#"{"g":{"PreToolUse":[{"matcher":".*","hooks":[{"type":"command","command":"bash guard.sh"}]}]}}"#;
        assert!(!approval_hook_installed(guards));
        for bad in ["", "not json", "[]", "{}", r#"{"g":{"PreToolUse":"x"}}"#] {
            assert!(!approval_hook_installed(bad), "{bad}");
        }
        assert!(!approval_hook_installed(
            r#"{"g":{"PreToolUse":[{"matcher":7,"command":"--approval-hook"}]}}"#
        ));
    }

    #[test]
    fn check_installed_reads_the_file_under_a_given_home() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().to_string_lossy().into_owned();
        assert!(check_installed(Some(&home)).is_err(), "no file");
        assert!(check_installed(None).is_err());
        let dir = tmp.path().join(".gemini/config");
        std::fs::create_dir_all(&dir).expect("mk");
        std::fs::write(dir.join("hooks.json"), "{}").expect("write");
        assert!(check_installed(Some(&home)).is_err(), "no entry");
        std::fs::write(dir.join("hooks.json"), GOOD).expect("write");
        assert!(check_installed(Some(&home)).is_ok());
    }
}
