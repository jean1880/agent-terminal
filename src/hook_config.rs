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
#[cfg(test)]
pub fn approval_hook_installed(json: &str) -> bool {
    serde_json::from_str::<Value>(json).is_ok_and(|root| value_installed(&root))
}

/// [`approval_hook_installed`] on an already parsed file.
fn value_installed(root: &Value) -> bool {
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

/// What reading `hooks.json` found. "Could not tell" is its own case, never folded into
/// [`HookProbe::Missing`] or [`HookProbe::Installed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookProbe {
    Installed,
    /// The file is absent or readable but has no gating approval entry.
    Missing(String),
    /// `HOME` unset, the file unreadable (permissions, I/O), or not valid JSON.
    Unreadable(String),
}

/// Reads the real file and classifies what is there.
pub fn probe(home: Option<&str>) -> HookProbe {
    let Some(path) = hooks_path(home) else {
        return HookProbe::Unreadable("HOME is not set".to_owned());
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return HookProbe::Missing(missing_message(&path));
        }
        Err(e) => {
            return HookProbe::Unreadable(format!("cannot read {}: {e}", path.display()));
        }
    };
    let Ok(root) = serde_json::from_str::<Value>(&text) else {
        return HookProbe::Unreadable(format!("{} is not valid JSON", path.display()));
    };
    if value_installed(&root) {
        HookProbe::Installed
    } else {
        HookProbe::Missing(missing_message(&path))
    }
}

fn missing_message(path: &std::path::Path) -> String {
    format!(
        "{} has no approval hook entry. Add this top-level key:\n{}",
        path.display(),
        install_entry_json()
    )
}

/// Reads the real file. `Err` carries the reason to show the user.
pub fn check_installed(home: Option<&str>) -> Result<(), String> {
    match probe(home) {
        HookProbe::Installed => Ok(()),
        HookProbe::Missing(why) | HookProbe::Unreadable(why) => Err(why),
    }
}

/// What the Settings row shows for agy's hook: three states, per the repo's indicator rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookBadge {
    /// Hook entry present and the runtime directory available. (Not a live-gate proof: that is
    /// the per-thread socket bind and the session canary.)
    Ready,
    /// No gating entry in `hooks.json`.
    Missing,
    /// Could not tell: unreadable file, bad JSON, or no socket to bind.
    Unreadable,
}

/// `socket_bound`: whether the approval socket is (or can be) bound. An installed hook with no
/// socket is not ready, so it is `Unreadable`, never `Ready`.
pub fn badge(probe: &HookProbe, socket_bound: bool) -> HookBadge {
    match probe {
        HookProbe::Installed if socket_bound => HookBadge::Ready,
        HookProbe::Installed | HookProbe::Unreadable(_) => HookBadge::Unreadable,
        HookProbe::Missing(_) => HookBadge::Missing,
    }
}

/// Writes the verdict to the journal (`info` when ready, `warn` otherwise).
pub fn log_verdict(badge: HookBadge, probe: &HookProbe, socket_bound: bool) {
    match badge {
        HookBadge::Ready => {
            tracing::info!("agy approval hook ready (entry present, runtime dir ok)")
        }
        HookBadge::Missing => {
            tracing::warn!("agy approval hook missing: agy cannot ask before acting")
        }
        HookBadge::Unreadable => {
            tracing::warn!(socket_bound, probe = ?probe, "agy approval hook could not be verified");
        }
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
    fn probe_separates_missing_from_unreadable() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().to_string_lossy().into_owned();
        assert!(
            matches!(probe(Some(&home)), HookProbe::Missing(_)),
            "no file"
        );
        assert!(matches!(probe(None), HookProbe::Unreadable(_)), "no HOME");
        let dir = tmp.path().join(".gemini/config");
        std::fs::create_dir_all(&dir).expect("mk");
        std::fs::write(dir.join("hooks.json"), "not json").expect("write");
        assert!(
            matches!(probe(Some(&home)), HookProbe::Unreadable(_)),
            "bad json"
        );
        std::fs::write(dir.join("hooks.json"), "{}").expect("write");
        assert!(
            matches!(probe(Some(&home)), HookProbe::Missing(_)),
            "no entry"
        );
        std::fs::write(dir.join("hooks.json"), GOOD).expect("write");
        assert_eq!(probe(Some(&home)), HookProbe::Installed);
        // A directory where the file should be is a read error, not "missing".
        std::fs::remove_file(dir.join("hooks.json")).expect("rm");
        std::fs::create_dir(dir.join("hooks.json")).expect("mkdir");
        assert!(
            matches!(probe(Some(&home)), HookProbe::Unreadable(_)),
            "dir"
        );
    }

    #[test]
    fn badge_has_three_states_and_unreadable_is_never_healthy() {
        let installed = HookProbe::Installed;
        let missing = HookProbe::Missing("m".into());
        let unreadable = HookProbe::Unreadable("u".into());
        assert_eq!(badge(&installed, true), HookBadge::Ready);
        assert_eq!(badge(&installed, false), HookBadge::Unreadable);
        assert_eq!(badge(&missing, true), HookBadge::Missing);
        assert_eq!(badge(&missing, false), HookBadge::Missing);
        assert_eq!(badge(&unreadable, true), HookBadge::Unreadable);
        assert_eq!(badge(&unreadable, false), HookBadge::Unreadable);
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
