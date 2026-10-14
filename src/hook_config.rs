//! Is agy's approval hook actually installed? agy runs with `--dangerously-skip-permissions`
//! only when the answer is yes, so this is checked before an [`crate::approval_server`] socket
//! is bound for it. Without a proven hook, agy uses its native mode and cannot ask before acting.
//!
//! The primary file is `$HOME/.gemini/config/hooks.json`. Its shape (named groups, each mapping an
//! event to entries that are either a bare command or a `matcher` with nested `hooks`) is the
//! one agy and agent-sync write. Alternate sources must be covered too before a launch is proven.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// These agy-specific names may redirect its configuration. Their precedence is unproven, so
/// any set value (even blank) prevents a proof. XDG_CONFIG_HOME does not move agy's hook files.
pub const CONFIG_REDIRECT_VARS: [&str; 3] = [
    "ANTIGRAVITY_APP_DATA_DIR",
    "JETSKI_APP_DATA_DIR",
    "CASCADE_GLOBAL_CONFIG_OVERRIDE",
];

pub const DIFFERENT_HOME: &str = "the cached approval hook was checked for a different home folder";
pub const DIFFERENT_DIRECTORY: &str =
    "the cached approval hook was checked for a different thread directory";
const UNVERIFIED: &str = "cannot verify agy's approval hook: ";

enum HomeHookSource {
    File(&'static str),
    Plugins(&'static str),
}

// A real agy strace (2026-10-14, agy-hooks-strace.txt) showed these home sources in
// addition to the primary .gemini/config/hooks.json. Their precedence is unproven.
const ALTERNATE_HOME_HOOK_SOURCES: [HomeHookSource; 5] = [
    HomeHookSource::File(".gemini/antigravity-cli/hooks.json"),
    HomeHookSource::File(".gemini/hooks.json"),
    HomeHookSource::Plugins(".gemini/config/plugins"),
    HomeHookSource::Plugins(".gemini/antigravity-cli/plugins"),
    HomeHookSource::Plugins(".gemini/plugins"),
];

/// Only names and paths are kept with a proof, never environment values.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HookTarget {
    pub home: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    redirect: Option<&'static str>,
}

impl HookTarget {
    pub fn new(env: &crate::agent_proc::AgentEnv, cwd: Option<PathBuf>) -> Self {
        Self::from_child(&crate::agent_home::child_env(env), cwd)
    }

    fn from_child(child: &[(String, String)], cwd: Option<PathBuf>) -> Self {
        Self {
            home: crate::agent_home::home_in_child(child),
            cwd,
            redirect: CONFIG_REDIRECT_VARS
                .into_iter()
                .find(|name| child.iter().any(|(key, _)| key == name)),
        }
    }

    fn environment_verdict(&self) -> Result<(), String> {
        match self.redirect {
            Some(name) => Err(format!(
                "{UNVERIFIED}{name} is set and may redirect its configuration"
            )),
            None => Ok(()),
        }
    }

    pub fn context_verdict(&self) -> Result<(), String> {
        self.environment_verdict()?;
        if self.home.is_none() {
            return Err(crate::chat::session::HOOK_NOT_CHECKED.to_owned());
        }
        if !self
            .cwd
            .as_ref()
            .is_some_and(|cwd| agent_core::codex::is_plain_absolute(cwd))
        {
            return Err(format!(
                "{UNVERIFIED}the thread directory is unknown or not a plain absolute path"
            ));
        }
        Ok(())
    }

    fn key(&self) -> Result<(PathBuf, PathBuf), String> {
        self.context_verdict()?;
        match (&self.home, &self.cwd) {
            (Some(home), Some(cwd)) => Ok((home.clone(), cwd.clone())),
            _ => Err(crate::chat::session::HOOK_NOT_CHECKED.to_owned()),
        }
    }
}

/// These failures must reach the session notice instead of being called a missing installation.
pub fn verification_refused(reason: &str) -> bool {
    reason.starts_with(UNVERIFIED) || matches!(reason, DIFFERENT_HOME | DIFFERENT_DIRECTORY)
}

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
pub fn hooks_path(home: Option<&Path>) -> Option<PathBuf> {
    let home = home.filter(|h| agent_core::codex::is_plain_absolute(h))?;
    Some(home.join(".gemini/config/hooks.json"))
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
pub fn probe(home: Option<&Path>) -> HookProbe {
    let (Some(home), Some(path)) = (home, hooks_path(home)) else {
        return HookProbe::Unreadable("HOME is not an absolute path".to_owned());
    };
    let primary = probe_path(&path);
    if primary != HookProbe::Installed {
        return primary;
    }
    match check_home_sources(home) {
        Ok(()) => HookProbe::Installed,
        Err(why) => HookProbe::Unreadable(why),
    }
}

fn check_home_sources(home: &Path) -> Result<(), String> {
    for source in ALTERNATE_HOME_HOOK_SOURCES {
        match source {
            HomeHookSource::File(relative) => check_optional_hook(&home.join(relative))?,
            HomeHookSource::Plugins(relative) => {
                let root = home.join(relative);
                if !present(&root)? {
                    continue;
                }
                let listing_error = |e| format!("{UNVERIFIED}cannot list {}: {e}", root.display());
                for entry in std::fs::read_dir(&root).map_err(listing_error)? {
                    let path = entry.map_err(listing_error)?.path();
                    // Follow linked plugin directories; metadata errors must never hide a source.
                    let metadata = std::fs::metadata(&path).map_err(|e| {
                        format!("{UNVERIFIED}cannot inspect {}: {e}", path.display())
                    })?;
                    if metadata.is_dir() {
                        check_optional_hook(&path.join("hooks.json"))?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn check_optional_hook(path: &Path) -> Result<(), String> {
    if present(path)? {
        probe_verdict(&probe_path(path)).map_err(|why| format!("{UNVERIFIED}{why}"))?;
    }
    Ok(())
}

fn probe_path(path: &Path) -> HookProbe {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return HookProbe::Missing(missing_message(path));
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
        HookProbe::Missing(missing_message(path))
    }
}

/// `exists` would hide permission errors and dangling links. Both must fail closed.
fn present(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!(
            "{UNVERIFIED}cannot inspect {}: {e}",
            path.display()
        )),
    }
}

fn reject_workspace_hook(path: &Path) -> Result<(), String> {
    if present(path)? {
        return Err(format!(
            "{UNVERIFIED}{} exists and may override the approval hook",
            path.display()
        ));
    }
    Ok(())
}

fn hook_directories(cwd: &Path, toplevel: Option<&Path>) -> Vec<PathBuf> {
    match toplevel.filter(|root| cwd.starts_with(root)) {
        Some(root) => cwd
            .ancestors()
            .take_while(|dir| dir.starts_with(root))
            .map(Path::to_path_buf)
            .collect(),
        None => vec![cwd.to_path_buf()],
    }
}

fn repository_toplevel(
    discovered: Result<Option<PathBuf>, String>,
) -> Result<Option<PathBuf>, String> {
    discovered.map_err(|_| format!(
        "{UNVERIFIED}could not tell which repository this thread is in, so its workspace hook files could not be checked"
    ))
}

/// A whole launch proof, including the sources whose precedence this app cannot assume.
fn target_sources(target: &HookTarget) -> Result<(), String> {
    target.environment_verdict()?;
    let Some(cwd) = target.cwd.as_deref() else {
        return Err(format!(
            "{UNVERIFIED}the thread directory is unknown, so workspace hook files could not be checked"
        ));
    };
    if !agent_core::codex::is_plain_absolute(cwd) {
        return Err(format!(
            "{UNVERIFIED}the thread directory is not a plain absolute path"
        ));
    }
    reject_workspace_hook(&cwd.join(".agents/hooks.json"))?;
    reject_workspace_hook(&cwd.join("hooks/hooks.json"))?;
    // git returns the physical toplevel; resolve aliases before comparing the boundary. These
    // read-only, timed queries use agent-kit's cleaned git environment, on the check's worker.
    let physical = std::fs::canonicalize(cwd).map_err(|e| {
        format!(
        "{UNVERIFIED}could not resolve the thread directory to check its workspace hook files: {e}"
    )
    })?;
    let toplevel = repository_toplevel(
        agent_kit::git::discover(&physical).map(|repo| repo.map(|repo| repo.toplevel)),
    )?;
    for dir in hook_directories(&physical, toplevel.as_deref())
        .into_iter()
        .skip(1)
    {
        reject_workspace_hook(&dir.join(".agents/hooks.json"))?;
    }
    Ok(())
}

/// Settings proves only the home-level installation; each launch checks its own directory.
pub fn probe_settings(target: &HookTarget) -> HookProbe {
    match target.environment_verdict() {
        Ok(()) => probe(target.home.as_deref()),
        Err(why) => HookProbe::Unreadable(why),
    }
}

pub fn check_target(target: &HookTarget) -> Result<(), String> {
    target_sources(target)?;
    check_installed(target.home.as_deref())
}

fn missing_message(path: &std::path::Path) -> String {
    format!(
        "{} has no approval hook entry. Add this top-level key:\n{}",
        path.display(),
        install_entry_json()
    )
}

/// Reads the real file. `Err` carries the reason to show the user.
pub fn check_installed(home: Option<&Path>) -> Result<(), String> {
    probe_verdict(&probe(home))
}

pub fn probe_verdict(probe: &HookProbe) -> Result<(), String> {
    match probe {
        HookProbe::Installed => Ok(()),
        HookProbe::Missing(why) | HookProbe::Unreadable(why) => Err(why.clone()),
    }
}

/// A hook proof belongs only to the home and thread directory checked.
#[derive(Clone)]
pub struct HookVerdict {
    pub home: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub verdict: Result<(), String>,
}

impl HookVerdict {
    pub fn for_target(&self, target: &HookTarget) -> Result<(), String> {
        target.context_verdict()?;
        if self.home != target.home {
            return Err(DIFFERENT_HOME.to_owned());
        }
        if self.cwd != target.cwd {
            return Err(DIFFERENT_DIRECTORY.to_owned());
        }
        self.verdict.clone()
    }
}

const HOOK_CACHE_LIMIT: usize = 16;

/// Bounded by the most recently checked sources, never by lookup side effects. A failed check
/// replaces only its own entry; Settings and unresolved contexts cannot create launch proofs.
pub struct HookCache {
    entries: HashMap<(PathBuf, PathBuf), HookVerdict>,
    checked: VecDeque<(PathBuf, PathBuf)>,
    capacity: usize,
}

impl Default for HookCache {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            checked: VecDeque::new(),
            capacity: HOOK_CACHE_LIMIT,
        }
    }
}

impl HookCache {
    /// A refresh can retain every open source while inactive entries remain bounded.
    pub fn set_refresh_size(&mut self, sources: usize) {
        self.capacity = HOOK_CACHE_LIMIT.max(sources);
        self.trim();
    }

    pub fn remember(&mut self, target: &HookTarget, verdict: Result<(), String>) {
        let Ok(key) = target.key() else { return };
        self.checked.retain(|old| old != &key);
        self.checked.push_back(key.clone());
        self.entries.insert(
            key,
            HookVerdict {
                home: target.home.clone(),
                cwd: target.cwd.clone(),
                verdict,
            },
        );
        self.trim();
    }

    fn trim(&mut self) {
        while self.checked.len() > self.capacity {
            if let Some(oldest) = self.checked.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }

    pub fn lookup(&self, target: &HookTarget) -> Result<(), String> {
        let key = target.key()?;
        if let Some(verdict) = self.entries.get(&key) {
            return verdict.for_target(target);
        }
        if self.entries.keys().any(|(_, cwd)| cwd == &key.1) {
            return Err(DIFFERENT_HOME.to_owned());
        }
        if self.entries.keys().any(|(home, _)| home == &key.0) {
            return Err(DIFFERENT_DIRECTORY.to_owned());
        }
        Err(crate::chat::session::HOOK_NOT_CHECKED.to_owned())
    }

    pub fn needs_check(&self, target: &HookTarget) -> bool {
        target
            .key()
            .is_ok_and(|key| !self.entries.contains_key(&key))
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

    fn installed_home() -> tempfile::TempDir {
        let home = tempfile::tempdir().expect("home");
        let dir = home.path().join(".gemini/config");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("hooks.json"), GOOD).expect("hook");
        home
    }

    fn target(home: &Path, cwd: Option<&Path>) -> HookTarget {
        HookTarget::from_child(
            &[("HOME".into(), home.to_string_lossy().into_owned())],
            cwd.map(Path::to_path_buf),
        )
    }

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
        let home = tmp.path();
        std::fs::create_dir_all(tmp.path().join(".gemini/config")).expect("mk");
        std::fs::write(tmp.path().join(".gemini/config/hooks.json"), "{}").expect("write");
        check_installed(Some(home))
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
        let home = tmp.path();
        assert!(
            matches!(probe(Some(home)), HookProbe::Missing(_)),
            "no file"
        );
        assert!(matches!(probe(None), HookProbe::Unreadable(_)), "no HOME");
        let dir = tmp.path().join(".gemini/config");
        std::fs::create_dir_all(&dir).expect("mk");
        std::fs::write(dir.join("hooks.json"), "not json").expect("write");
        assert!(
            matches!(probe(Some(home)), HookProbe::Unreadable(_)),
            "bad json"
        );
        std::fs::write(dir.join("hooks.json"), "{}").expect("write");
        assert!(
            matches!(probe(Some(home)), HookProbe::Missing(_)),
            "no entry"
        );
        std::fs::write(dir.join("hooks.json"), GOOD).expect("write");
        assert_eq!(probe(Some(home)), HookProbe::Installed);
        // A directory where the file should be is a read error, not "missing".
        std::fs::remove_file(dir.join("hooks.json")).expect("rm");
        std::fs::create_dir(dir.join("hooks.json")).expect("mkdir");
        assert!(matches!(probe(Some(home)), HookProbe::Unreadable(_)), "dir");
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
        let home = tmp.path();
        assert!(check_installed(Some(home)).is_err(), "no file");
        assert!(check_installed(None).is_err());
        let dir = tmp.path().join(".gemini/config");
        std::fs::create_dir_all(&dir).expect("mk");
        std::fs::write(dir.join("hooks.json"), "{}").expect("write");
        assert!(check_installed(Some(home)).is_err(), "no entry");
        std::fs::write(dir.join("hooks.json"), GOOD).expect("write");
        assert!(check_installed(Some(home)).is_ok());
    }

    #[test]
    fn redirect_vars_fail_closed_in_the_launch_environment() {
        let home = installed_home();
        let cwd = tempfile::tempdir().expect("cwd");
        let proof = HookVerdict {
            home: Some(home.path().into()),
            cwd: Some(cwd.path().into()),
            verdict: Ok(()),
        };
        for name in CONFIG_REDIRECT_VARS {
            let inherited = vec![
                ("HOME".into(), home.path().to_string_lossy().into_owned()),
                (name.into(), "private-config-value".into()),
            ];
            let mut inputs = crate::codex_home::Inputs {
                inherited,
                ..crate::codex_home::Inputs::default()
            };
            for from_profile in [false, true] {
                if from_profile {
                    inputs.inherited.retain(|(key, _)| key != name);
                    inputs.env_file = vec![(name.into(), "private-config-value".into())];
                }
                let target = HookTarget::from_child(
                    &crate::codex_home::child_env(&inputs),
                    Some(cwd.path().into()),
                );
                let why = check_target(&target).expect_err("redirect cannot be vouched for");
                assert!(why.contains(name), "{why}");
                assert!(
                    !why.contains("private-config-value"),
                    "never expose the value"
                );
                assert!(
                    proof.for_target(&target).is_err(),
                    "a cached proof cannot bypass the guard"
                );
            }
            inputs.env_file.clear();
            inputs
                .inherited
                .push((name.into(), "private-config-value".into()));
            inputs.clear = vec![name.into()];
            let cleared = HookTarget::from_child(
                &crate::codex_home::child_env(&inputs),
                Some(cwd.path().into()),
            );
            assert!(
                check_target(&cleared).is_ok(),
                "an unset inherited variable does not reach agy"
            );
            inputs.env_file = vec![(name.into(), "".into())];
            let blank = HookTarget::from_child(
                &crate::codex_home::child_env(&inputs),
                Some(cwd.path().into()),
            );
            assert!(
                check_target(&blank).is_err(),
                "a blank override is still set after unset"
            );
        }
    }

    #[test]
    fn an_alternate_home_hooks_file_must_also_gate_every_tool() {
        let home = installed_home();
        assert!(check_installed(Some(home.path())).is_ok());
        let alternate = home.path().join(".gemini/antigravity-cli/hooks.json");
        std::fs::create_dir_all(alternate.parent().expect("parent")).expect("mkdir");
        let narrow = GOOD.replace("\".*\"", "\"read_file\"");
        for text in ["{}", "not json", narrow.as_str()] {
            std::fs::write(&alternate, text).expect("hook");
            let why = check_installed(Some(home.path())).expect_err("alternate hook must gate too");
            assert!(why.contains("antigravity-cli/hooks.json"), "{why}");
        }
        std::fs::write(&alternate, GOOD).expect("hook");
        assert!(check_installed(Some(home.path())).is_ok());
        std::fs::remove_file(&alternate).expect("remove");
        std::os::unix::fs::symlink("absent.json", &alternate).expect("dangling link");
        assert!(
            check_installed(Some(home.path())).is_err(),
            "a dangling hook is not absent"
        );
    }

    fn assert_home_source_refused(home: &Path, path: &Path) {
        let why = check_installed(Some(home)).expect_err("every present source must gate tools");
        assert!(why.contains(&path.to_string_lossy().into_owned()), "{why}");
        assert!(verification_refused(&why), "{why}");
        let cwd = tempfile::tempdir().expect("cwd");
        assert!(check_target(&target(home, Some(cwd.path()))).is_err());
        assert!(matches!(
            probe_settings(&target(home, None)),
            HookProbe::Unreadable(_)
        ));
    }

    #[test]
    fn a_home_root_hooks_file_must_gate_every_tool() {
        let home = installed_home();
        let path = home.path().join(".gemini/hooks.json");
        let narrow = GOOD.replace("\".*\"", "\"read_file\"");
        for text in ["{}", "not json", narrow.as_str()] {
            std::fs::write(&path, text).expect("hook");
            assert_home_source_refused(home.path(), &path);
        }
        std::fs::write(&path, GOOD).expect("hook");
        assert!(check_installed(Some(home.path())).is_ok());
        assert_eq!(
            probe_settings(&target(home.path(), None)),
            HookProbe::Installed
        );
        std::fs::remove_file(&path).expect("remove");
        std::fs::create_dir(&path).expect("unreadable hook");
        assert_home_source_refused(home.path(), &path);
        std::fs::remove_dir(&path).expect("remove directory");
        std::os::unix::fs::symlink("absent.json", &path).expect("dangling hook");
        assert_home_source_refused(home.path(), &path);
    }

    #[test]
    fn a_plugin_hooks_file_must_gate_every_tool() {
        for root in [
            ".gemini/config/plugins",
            ".gemini/antigravity-cli/plugins",
            ".gemini/plugins",
        ] {
            let home = installed_home();
            let plugins = home.path().join(root);
            let path = plugins.join("first/hooks.json");
            std::fs::create_dir_all(path.parent().expect("parent")).expect("plugin");
            let narrow = GOOD.replace("\".*\"", "\"read_file\"");
            for text in ["{}", "not json", narrow.as_str()] {
                std::fs::write(&path, text).expect("hook");
                assert_home_source_refused(home.path(), &path);
            }
            std::fs::write(&path, GOOD).expect("hook");
            assert!(check_installed(Some(home.path())).is_ok());
            let second = plugins.join("second/hooks.json");
            std::fs::create_dir_all(second.parent().expect("parent")).expect("second plugin");
            std::fs::write(&second, "{}").expect("second hook");
            assert_home_source_refused(home.path(), &second);
            std::fs::write(&second, GOOD).expect("second hook");
            assert!(check_installed(Some(home.path())).is_ok());
            std::fs::remove_file(&path).expect("remove");
            std::fs::create_dir(&path).expect("unreadable hook");
            assert_home_source_refused(home.path(), &path);
            std::fs::remove_dir(&path).expect("remove directory");
            std::os::unix::fs::symlink("absent.json", &path).expect("dangling hook");
            assert_home_source_refused(home.path(), &path);
            std::fs::remove_file(&path).expect("remove link");
            std::fs::write(&path, GOOD).expect("restore hook");
            let linked = tempfile::tempdir().expect("linked plugin");
            std::fs::write(linked.path().join("hooks.json"), "{}").expect("linked hook");
            let alias = plugins.join("linked");
            std::os::unix::fs::symlink(linked.path(), &alias).expect("linked directory");
            assert_home_source_refused(home.path(), &alias.join("hooks.json"));
            std::fs::write(linked.path().join("hooks.json"), GOOD).expect("linked hook");
            assert!(check_installed(Some(home.path())).is_ok());
            let unknown = plugins.join("unknown");
            std::os::unix::fs::symlink("absent-plugin", &unknown).expect("dangling plugin");
            assert_home_source_refused(home.path(), &unknown);
        }
    }

    #[test]
    fn an_unlistable_plugins_dir_fails_closed() {
        use std::os::unix::fs::PermissionsExt;

        for root in [
            ".gemini/config/plugins",
            ".gemini/antigravity-cli/plugins",
            ".gemini/plugins",
        ] {
            let home = installed_home();
            let plugins = home.path().join(root);
            std::fs::create_dir_all(plugins.parent().expect("parent")).expect("parent");
            // Not a directory: a deterministic listing error, even for a privileged test runner.
            std::fs::write(&plugins, "not a directory").expect("block listing");
            assert_home_source_refused(home.path(), &plugins);
            std::fs::remove_file(&plugins).expect("remove");
            std::os::unix::fs::symlink("absent-plugins", &plugins).expect("dangling root");
            assert_home_source_refused(home.path(), &plugins);
            std::fs::remove_file(&plugins).expect("remove link");
            std::fs::create_dir(&plugins).expect("plugins");
            std::fs::set_permissions(&plugins, std::fs::Permissions::from_mode(0o000))
                .expect("deny listing");
            let cannot_list = std::fs::read_dir(&plugins).is_err();
            let verdict = check_installed(Some(home.path()));
            std::fs::set_permissions(&plugins, std::fs::Permissions::from_mode(0o700))
                .expect("restore permissions");
            if cannot_list {
                let why = verdict.expect_err("permission errors must fail closed");
                assert!(why.contains(root), "{why}");
            }
        }
    }

    #[test]
    fn absent_plugin_hooks_do_not_block() {
        let home = installed_home();
        assert!(check_installed(Some(home.path())).is_ok(), "absent roots");
        for root in [
            ".gemini/config/plugins",
            ".gemini/antigravity-cli/plugins",
            ".gemini/plugins",
        ] {
            let plugins = home.path().join(root);
            std::fs::create_dir_all(&plugins).expect("empty root");
            assert!(check_installed(Some(home.path())).is_ok(), "empty root");
            std::fs::create_dir(plugins.join("without-hooks")).expect("plugin");
            std::fs::write(plugins.join("README.md"), "regular entry").expect("non-plugin");
            assert!(check_installed(Some(home.path())).is_ok(), "no hook file");
        }
        assert_eq!(
            probe_settings(&target(home.path(), None)),
            HookProbe::Installed
        );
        let cwd = tempfile::tempdir().expect("cwd");
        assert!(check_target(&target(home.path(), Some(cwd.path()))).is_ok());
    }

    #[test]
    fn workspace_hooks_are_not_covered_by_a_home_proof() {
        let home = installed_home();
        let cwd = tempfile::tempdir().expect("cwd");
        let target = target(home.path(), Some(cwd.path()));
        assert!(check_target(&target).is_ok());
        for relative in [".agents/hooks.json", "hooks/hooks.json"] {
            let path = cwd.path().join(relative);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(&path, GOOD).expect("workspace hook");
            let why = check_target(&target).expect_err("local precedence is unproven");
            assert!(why.contains(relative), "{why}");
            std::fs::remove_file(&path).expect("remove");
            std::os::unix::fs::symlink("absent.json", &path).expect("dangling link");
            assert!(
                check_target(&target).is_err(),
                "a dangling local hook is not absent"
            );
            std::fs::remove_file(&path).expect("remove link");
        }
        let blocked = tempfile::tempdir().expect("blocked cwd");
        std::fs::write(blocked.path().join(".agents"), "a file, not a directory").expect("blocker");
        assert!(
            check_target(&self::target(home.path(), Some(blocked.path()))).is_err(),
            "metadata errors must not be mistaken for an absent hook"
        );
    }

    #[test]
    fn a_hook_proof_without_a_thread_directory_fails_closed() {
        let home = installed_home();
        let target = target(home.path(), None);
        let why = check_target(&target).expect_err("no directory means no local-source coverage");
        assert!(why.contains("thread directory is unknown"), "{why}");
        assert_eq!(probe_settings(&target), HookProbe::Installed);
        assert_eq!(badge(&probe_settings(&target), true), HookBadge::Ready);
        let mut cache = HookCache::default();
        cache.remember(&target, probe_verdict(&probe_settings(&target)));
        assert!(
            cache.lookup(&target).is_err(),
            "Settings cannot authorise a launch without its directory"
        );
        std::fs::remove_file(home.path().join(".gemini/config/hooks.json")).expect("remove hook");
        assert!(matches!(probe_settings(&target), HookProbe::Missing(_)));
        assert_eq!(badge(&probe_settings(&target), true), HookBadge::Missing);
    }

    #[test]
    fn settings_checks_home_sources_without_authorising_workspace_hooks() {
        let home = installed_home();
        let cwd = tempfile::tempdir().expect("cwd");
        let target = target(home.path(), Some(cwd.path()));
        let local = cwd.path().join(".agents/hooks.json");
        std::fs::create_dir_all(local.parent().expect("parent")).expect("mkdir");
        std::fs::write(local, GOOD).expect("local hook");
        assert_eq!(probe_settings(&target), HookProbe::Installed);
        assert!(
            check_target(&target).is_err(),
            "the home badge is not a launch proof"
        );
        let alternate = home.path().join(".gemini/antigravity-cli/hooks.json");
        std::fs::create_dir_all(alternate.parent().expect("parent")).expect("mkdir");
        std::fs::write(alternate, "{}").expect("alternate hook");
        assert!(
            matches!(probe_settings(&target), HookProbe::Unreadable(_)),
            "Settings still checks both home sources"
        );
    }

    #[test]
    fn xdg_config_home_does_not_redirect_agy_hooks() {
        let home = installed_home();
        let cwd = tempfile::tempdir().expect("cwd");
        for value in ["/another/config", ""] {
            let target = HookTarget::from_child(
                &[
                    ("HOME".into(), home.path().to_string_lossy().into_owned()),
                    ("XDG_CONFIG_HOME".into(), value.into()),
                ],
                Some(cwd.path().into()),
            );
            assert_eq!(probe_settings(&target), HookProbe::Installed);
            assert!(check_target(&target).is_ok());
            let mut cache = HookCache::default();
            cache.remember(&target, Ok(()));
            assert_eq!(cache.lookup(&target), Ok(()));
        }
    }

    #[test]
    fn hook_cache_is_bounded_and_lookup_does_not_change_recency() {
        let home = Path::new("/profile/home");
        let mut cache = HookCache::default();
        let targets: Vec<_> = (0..=HOOK_CACHE_LIMIT)
            .map(|i| target(home, Some(&PathBuf::from(format!("/thread/{i}")))))
            .collect();
        for target in &targets {
            cache.remember(target, Ok(()));
        }
        assert_eq!(cache.entries.len(), HOOK_CACHE_LIMIT);
        assert!(
            cache.lookup(&targets[0]).is_err(),
            "oldest proof is evicted, never guessed"
        );
        let order = cache.checked.clone();
        for target in &targets[1..] {
            assert_eq!(cache.lookup(target), Ok(()));
        }
        assert_eq!(cache.checked, order, "lookup is read-only");
        cache.remember(&targets[1], Err("hook changed".into()));
        assert_eq!(cache.entries.len(), HOOK_CACHE_LIMIT);
        assert_eq!(cache.lookup(&targets[1]), Err("hook changed".into()));
        assert_eq!(
            cache.lookup(&targets[2]),
            Ok(()),
            "an update only touches its own key"
        );
    }

    #[test]
    fn ancestor_hook_checks_stop_at_the_repository_boundary() {
        if !agent_kit::git::git_installed() {
            return;
        }
        let outer = tempfile::tempdir().expect("outer directory");
        let repo = outer.path().join("repo");
        let cwd = repo.join("sub/deeper");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        let initialized = agent_kit::git::git_output(
            &repo,
            ["-c", "init.templateDir=", "init", "--quiet"],
            &[
                ("GIT_CONFIG_GLOBAL", std::ffi::OsStr::new("/dev/null")),
                ("GIT_CONFIG_SYSTEM", std::ffi::OsStr::new("/dev/null")),
            ],
            5,
        )
        .expect("git init");
        assert!(initialized.status.success());
        let home = installed_home();
        let target = target(home.path(), Some(&cwd));
        // A hook above the repository is outside the search boundary.
        let above = outer.path().join(".agents/hooks.json");
        std::fs::create_dir_all(above.parent().expect("parent")).expect("mkdir");
        std::fs::write(above, GOOD).expect("above hook");
        assert!(check_target(&target).is_ok());
        let ancestor = repo.join(".agents/hooks.json");
        std::fs::create_dir_all(ancestor.parent().expect("parent")).expect("mkdir");
        std::fs::write(&ancestor, GOOD).expect("ancestor hook");
        let why = check_target(&target).expect_err("repository ancestor hook must fail closed");
        assert!(
            why.contains(&ancestor.to_string_lossy().into_owned()),
            "{why}"
        );
        let alias = outer.path().join("alias");
        std::os::unix::fs::symlink(&cwd, &alias).expect("directory alias");
        assert!(
            check_target(&self::target(home.path(), Some(&alias))).is_err(),
            "aliases cannot skip ancestors"
        );
        let outside = outer.path().join("outside/deeper");
        std::fs::create_dir_all(&outside).expect("outside");
        assert!(
            check_target(&self::target(home.path(), Some(&outside))).is_ok(),
            "without a repository only the thread directory is checked"
        );
    }

    #[test]
    fn hook_search_without_a_known_ancestor_stays_in_the_thread_directory() {
        let cwd = Path::new("/repo/sub/deeper");
        assert_eq!(hook_directories(cwd, None), vec![cwd.to_path_buf()]);
        assert_eq!(
            hook_directories(cwd, Some(Path::new("/another"))),
            vec![cwd.to_path_buf()]
        );
        assert_eq!(
            hook_directories(cwd, Some(Path::new("/repo"))),
            vec![cwd.to_path_buf(), "/repo/sub".into(), "/repo".into()]
        );
    }

    #[test]
    fn a_failed_repository_discovery_refuses_the_hook_proof() {
        for error in ["git query timed out", "git query failed"] {
            let why = repository_toplevel(Err(error.into()))
                .expect_err("a failure is not a non-repository");
            assert!(
                why.contains("workspace hook files could not be checked"),
                "{why}"
            );
            assert!(
                verification_refused(&why),
                "the refusal must reach the session notice"
            );
        }
        assert_eq!(
            repository_toplevel(Ok(None)),
            Ok(None),
            "only a known non-repository falls back"
        );
        let root = PathBuf::from("/repo");
        assert_eq!(repository_toplevel(Ok(Some(root.clone()))), Ok(Some(root)));
    }
}
