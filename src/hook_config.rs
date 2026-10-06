//! Is agy's approval hook actually installed? agy runs with `--dangerously-skip-permissions`
//! only when the answer is yes, so this is checked before an [`crate::approval_server`] socket
//! is bound for it: a missing hook means plan mode, never an ungated agent.
//!
//! The file is `$HOME/.gemini/config/hooks.json`. Its shape (named groups, each mapping an
//! event to entries that are either a bare command or a `matcher` with nested `hooks`) is the
//! one agy and agent-sync write. Parsing is pure; only [`check_installed`] reads the file.

use std::path::PathBuf;

use serde_json::Value;

/// The one hook command agent-terminal accepts. Anything else (a substring match, a bare
/// `agent-terminal` found on PATH, an `echo --approval-hook`) proves nothing about the gate.
/// It is a no-op when the socket variable is unset, and runs the exact binary of this app.
pub const CANONICAL_HOOK_COMMAND: &str = r#"bash -c '[ -n "$AGENT_TERMINAL_APPROVAL_SOCKET" ] || exit 0; exec "${AGENT_TERMINAL_HOOK_BIN:-agent-terminal}" --approval-hook'"#;

/// The entry to add to `hooks.json` (as a new top-level key), for the user-facing Notice.
pub fn install_entry_json() -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "agent-terminal-approval": {
            "PreToolUse": [{
                "matcher": ".*",
                "hooks": [{
                    "type": "command",
                    "command": CANONICAL_HOOK_COMMAND,
                    "timeout": 600
                }]
            }]
        }
    }))
    .unwrap_or_default()
}

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
            .is_some_and(|c| c.trim() == CANONICAL_HOOK_COMMAND)
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
            "{} has no approval hook entry. Add this top-level key:\n{}",
            path.display(),
            install_entry_json()
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
        let bare = serde_json::json!({"g": {"PreToolUse": [
            {"type": "command", "command": CANONICAL_HOOK_COMMAND}
        ]}})
        .to_string();
        assert!(approval_hook_installed(&bare));
        // The entry the user is told to add is itself accepted.
        assert!(approval_hook_installed(&install_entry_json()));
        assert!(check_message_names_the_entry());
    }

    fn check_message_names_the_entry() -> bool {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().to_string_lossy().into_owned();
        std::fs::create_dir_all(tmp.path().join(".gemini/config")).expect("mk");
        std::fs::write(tmp.path().join(".gemini/config/hooks.json"), "{}").expect("write");
        check_installed(Some(&home))
            .err()
            .is_some_and(|m| m.contains("PreToolUse") && m.contains("--approval-hook"))
    }

    #[test]
    fn near_misses_do_not_count() {
        let with = |command: &str| {
            serde_json::json!({"g": {"PreToolUse": [{"matcher": ".*", "hooks": [
                {"type": "command", "command": command}
            ]}]}})
            .to_string()
        };
        for bad in [
            "echo --approval-hook",
            "agent-terminal --approval-hook",
            "bash -c 'exec \"${AGENT_TERMINAL_HOOK_BIN:-agent-terminal}\" --approval-hook'",
            "bash -c 'true' # --approval-hook",
        ] {
            assert!(!approval_hook_installed(&with(bad)), "{bad}");
        }
        assert!(approval_hook_installed(&with(&format!(
            "  {CANONICAL_HOOK_COMMAND}\n"
        ))));
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
