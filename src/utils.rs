//! Utility functions for Agent Terminal.

use crate::config::Profile;
use std::cell::RefCell;
use std::collections::HashMap;
use tracing::{debug, info, warn};

thread_local! {
    /// Which commands were found, keyed by command name.
    ///
    /// Resolution shells out — including `$SHELL -ic`, which sources the user's
    /// rc file — so it can take seconds on a heavy interactive shell. Caching it
    /// means a second window, or reopening Settings, never pays that cost again.
    /// GTK confines the application to one thread, so a `thread_local` is
    /// process-wide in practice; resolution itself runs on a worker thread and
    /// only the result is recorded here, on the main thread.
    static COMMAND_CACHE: RefCell<HashMap<String, bool>> = RefCell::new(HashMap::new());
}

/// Whether `command` was previously found. `None` means "never looked".
pub fn cached_command_available(command: &str) -> Option<bool> {
    COMMAND_CACHE.with(|cache| cache.borrow().get(command).copied())
}

/// Records a resolution result so later lookups skip the subprocesses.
pub fn cache_command_available(command: &str, available: bool) {
    COMMAND_CACHE.with(|cache| {
        cache.borrow_mut().insert(command.to_string(), available);
    });
}

/// Forgets cached resolution results.
#[cfg(test)]
pub fn clear_command_cache() {
    COMMAND_CACHE.with(|cache| cache.borrow_mut().clear());
}

/// The real command probe: `which`, then common install directories, then
/// `$SHELL -ic`.
///
/// Every filesystem and subprocess touch lives here so that [`resolve_profile`]
/// — the part with the actual decision logic — can be tested without spawning
/// anything or depending on what happens to be installed on the test machine.
pub struct SystemProbe {
    path: Option<String>,
    home: Option<String>,
    shell: Option<String>,
}

impl SystemProbe {
    pub fn new(path: Option<String>, home: Option<String>, shell: Option<String>) -> Self {
        Self { path, home, shell }
    }

    fn on_path(&self, command: &str) -> bool {
        let mut cmd = std::process::Command::new("which");
        cmd.arg(command);
        if let Some(path) = self.path.as_deref().filter(|p| !p.is_empty()) {
            cmd.env("PATH", path);
        }
        match cmd.output() {
            Ok(output) if output.status.success() => {
                let found = String::from_utf8_lossy(&output.stdout).trim().to_string();
                info!("{command} found via 'which' at {found}");
                true
            }
            _ => false,
        }
    }

    /// Absolute locations checked before falling back to the interactive shell.
    fn candidate_paths(&self, command: &str) -> Vec<String> {
        let home = self.home.as_deref().unwrap_or_default();
        vec![
            format!("/usr/bin/{command}"),
            format!("/usr/local/bin/{command}"),
            format!("{home}/.local/bin/{command}"),
            format!("{home}/.npm-global/bin/{command}"),
            format!("{home}/bin/{command}"),
        ]
    }

    /// Does the user's interactive shell know `command`?
    ///
    /// The expensive check, and the last resort: it sources the user's rc file,
    /// which is how nvm- and asdf-managed commands are found at all.
    fn known_to_interactive_shell(&self, command: &str) -> bool {
        let shell = self.shell.as_deref().unwrap_or("/bin/sh");
        let mut cmd = std::process::Command::new(shell);
        cmd.args(["-ic", &format!("command -v {command}")]);
        if let Some(path) = self.path.as_deref().filter(|p| !p.is_empty()) {
            cmd.env("PATH", path);
        }
        match cmd.output() {
            Ok(output) if output.status.success() => {
                let found = String::from_utf8_lossy(&output.stdout).trim().to_string();
                info!("{command} found via shell -ic at {found}");
                true
            }
            _ => false,
        }
    }

    /// Whether `command` can be launched, checked in increasing order of cost.
    pub fn command_available(&self, command: &str) -> bool {
        debug!("Resolving command {command}");

        if self.on_path(command) {
            return true;
        }

        for path in self.candidate_paths(command) {
            if !path.is_empty() && std::path::Path::new(&path).exists() {
                debug!("{command} found at {path}");
                return true;
            }
        }

        if self.known_to_interactive_shell(command) {
            return true;
        }

        warn!("{command} not found after all checks");
        false
    }
}

/// Picks the profile to launch.
///
/// `preferred` names an explicit choice; when it is absent, or names a profile
/// whose command is not installed, the first installed profile wins — the
/// behaviour the old `CliClient::Auto` had. Returns `None` when nothing at all
/// is available, which is what drives the welcome screen.
///
/// `available` is passed in rather than called directly so this can be tested
/// against a fake without touching the filesystem or spawning a shell.
pub fn resolve_profile<'a, F>(
    profiles: &'a [Profile],
    preferred: Option<&str>,
    mut available: F,
) -> Option<&'a Profile>
where
    F: FnMut(&str) -> bool,
{
    if let Some(name) = preferred {
        match profiles.iter().find(|p| p.name == name) {
            Some(profile) if available(&profile.command) => return Some(profile),
            Some(profile) => warn!(
                "Preferred profile '{}' is selected but '{}' is not installed; falling back",
                profile.name, profile.command
            ),
            None => warn!("Preferred profile '{name}' is not in the profile list"),
        }
    }

    profiles.iter().find(|p| available(&p.command))
}

/// Reads the exported environment produced by sourcing `path`.
///
/// The old "startup script" setting fed its output into the TTY, which is why it
/// had to be removed: anything written before `exec` disrupts the CLI's terminal
/// handshake. Sourcing in a detached subshell and harvesting only the resulting
/// environment gets the useful half of that idea without the harmful half.
///
/// Failures are logged and yield nothing rather than blocking the session.
pub fn load_env_file(path: &str) -> Vec<(String, String)> {
    // Expanded here: the path is passed to the shell quoted, so a leading `~`
    // would otherwise be taken literally.
    let path = expand_tilde(path);
    let path = path.as_str();

    // `env -0` so values containing newlines survive; `set -a` so assignments
    // without an explicit `export` are still exported.
    let output = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(r#"set -a; . "$1" >/dev/null 2>&1 || exit 1; env -0"#)
        .arg("sh")
        .arg(path)
        .output();

    let output = match output {
        Ok(output) if output.status.success() => output,
        Ok(_) => {
            warn!("Env file {path} could not be sourced; ignoring it");
            return Vec::new();
        }
        Err(err) => {
            warn!("Failed to run a shell to read env file {path}: {err}");
            return Vec::new();
        }
    };

    // `env` dumps the WHOLE environment, most of which the subshell simply
    // inherited from us. Returning all of it would append a hundred redundant
    // entries to every session's environment block and, since these are logged,
    // would name every variable the user happens to have set — secrets included.
    // Only what the file actually added or changed is of any interest.
    let inherited: std::collections::HashMap<String, String> = std::env::vars().collect();

    String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| {
            let (key, value) = entry.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .filter(|(key, value)| {
            // Bookkeeping the shell itself rewrites; never meaningful here.
            const SHELL_NOISE: [&str; 4] = ["_", "SHLVL", "PWD", "OLDPWD"];
            !SHELL_NOISE.contains(&key.as_str())
                && inherited.get(key).map(String::as_str) != Some(value.as_str())
        })
        .collect()
}

/// Whether `name` matches a `clear_env` pattern.
///
/// Exact match, or prefix match when the pattern ends in `*`. Nothing fancier:
/// environment variable names are simple, and a real glob would invite patterns
/// whose blast radius is hard to reason about in a config file.
fn matches_clear_pattern(name: &str, pattern: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == pattern,
    }
}

/// Removes the configured variables from a `KEY=VALUE` environment block.
///
/// Returns the names removed, so the caller can log what it did — silently
/// dropping a variable someone deliberately exported would be its own kind of
/// mystery.
pub fn strip_env(env: &mut Vec<String>, patterns: &[String]) -> Vec<String> {
    if patterns.is_empty() {
        return Vec::new();
    }

    let mut removed = Vec::new();
    env.retain(|entry| {
        // An entry without '=' is malformed; leave it rather than guess.
        let Some((name, _)) = entry.split_once('=') else {
            return true;
        };
        if patterns.iter().any(|p| matches_clear_pattern(name, p)) {
            removed.push(name.to_string());
            false
        } else {
            true
        }
    });
    removed
}

/// Expands a leading `~` to `$HOME`.
///
/// Config values are hand-written, so people write `~/...` and expect it to work.
/// Nothing else does this for us: `read_to_string` takes the tilde literally, and
/// a path passed to a shell in quotes is not expanded either.
pub fn expand_tilde(path: &str) -> String {
    let trimmed = path.trim();
    let Some(rest) = trimmed.strip_prefix('~') else {
        return trimmed.to_string();
    };
    // Only a bare `~` or `~/`; `~user` is someone else's home and not ours to guess.
    if !rest.is_empty() && !rest.starts_with('/') {
        return trimmed.to_string();
    }
    match std::env::var("HOME") {
        Ok(home) => format!("{home}{rest}"),
        Err(_) => trimmed.to_string(),
    }
}

/// What an indicator found.
///
/// Three states, not two. The predecessor had only ok/warn, so a source it could
/// not read fell into "ok" and the header reported a healthy system it had never
/// actually checked. `Unknown` exists so that failure can never again be
/// mistaken for health.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndicatorState {
    Ok,
    Warn { detail: String },
    Unknown { reason: String },
}

impl IndicatorState {
    /// The detail text to show when the indicator is clicked.
    pub fn detail(&self) -> &str {
        match self {
            IndicatorState::Ok => "",
            IndicatorState::Warn { detail } => detail,
            IndicatorState::Unknown { reason } => reason,
        }
    }
}

/// Reads an indicator's source. Blocking — callers must run it off the main
/// thread, for the same reason CLI resolution does.
pub fn read_indicator(source: &crate::config::IndicatorSource) -> IndicatorState {
    match source {
        crate::config::IndicatorSource::File { path } => {
            let path = &expand_tilde(path);
            match std::fs::read_to_string(path) {
                Ok(content) => {
                    if content.lines().any(|line| !line.trim().is_empty()) {
                        IndicatorState::Warn { detail: content }
                    } else {
                        IndicatorState::Ok
                    }
                }
                Err(err) => IndicatorState::Unknown {
                    reason: format!("Could not read {path}: {err}"),
                },
            }
        }
        crate::config::IndicatorSource::Command { argv, timeout_secs } => {
            let Some((command, args)) = argv.split_first() else {
                return IndicatorState::Unknown {
                    reason: "Indicator command is empty".to_string(),
                };
            };
            run_with_timeout(command, args, *timeout_secs)
        }
    }
}

/// Runs a command, giving up after `timeout_secs`.
///
/// A configured command is arbitrary and may hang; without a bound it would tie
/// up a worker thread for the life of the process.
fn run_with_timeout(command: &str, args: &[String], timeout_secs: u64) -> IndicatorState {
    use std::time::{Duration, Instant};

    let mut child = match std::process::Command::new(command)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            return IndicatorState::Unknown {
                reason: format!("Could not run {command}: {err}"),
            }
        }
    };

    let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(1));
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return IndicatorState::Unknown {
                    reason: format!("{command} did not finish within {timeout_secs}s"),
                };
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(err) => {
                return IndicatorState::Unknown {
                    reason: format!("Failed while waiting for {command}: {err}"),
                }
            }
        }
    }

    match child.wait_with_output() {
        Ok(output) if output.status.success() => IndicatorState::Ok,
        Ok(output) => {
            let mut detail = String::from_utf8_lossy(&output.stdout).to_string();
            if detail.trim().is_empty() {
                detail = String::from_utf8_lossy(&output.stderr).to_string();
            }
            if detail.trim().is_empty() {
                detail = format!("{command} exited with {}", output.status);
            }
            IndicatorState::Warn { detail }
        }
        Err(err) => IndicatorState::Unknown {
            reason: format!("Failed to collect output from {command}: {err}"),
        },
    }
}

/// Resolves the working directory to use, handling ~ expansion and fallback to home.
pub fn resolve_working_directory(starting_dir: &str, home_dir: &str) -> String {
    let mut work_dir = starting_dir.trim().to_string();
    if work_dir.is_empty() {
        home_dir.to_string()
    } else {
        if work_dir.starts_with('~') {
            work_dir = work_dir.replacen('~', home_dir, 1);
        }
        if std::path::Path::new(&work_dir).exists() {
            work_dir
        } else {
            warn!(
                "Configured starting directory '{}' does not exist, falling back to home directory",
                work_dir
            );
            home_dir.to_string()
        }
    }
}

/// Quotes an argument for safe inclusion in the single shell command string.
///
/// The command is handed to `$SHELL -ic`, so a profile carrying an argument with
/// a space, a quote, or a `$` would otherwise be re-split or expanded by that
/// shell. Single quotes suppress all expansion; an embedded single quote is
/// closed, escaped and reopened.
fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// Determines the startup command for a profile.
pub fn get_startup_command(profile: Option<&Profile>) -> Vec<String> {
    match profile {
        Some(profile) => {
            // `exec` is critical: it replaces the wrapping interactive shell with the
            // CLI so the CLI directly owns the controlling terminal (session leader).
            // Without it, `zsh -ic "claude"` runs claude as a *child job* of the shell,
            // and Claude Code comes up degraded — no status line, CLAUDE.md not loaded,
            // settings/folder-trust not persisted.
            //
            // Nothing is emitted before the exec: banner or `clear` output written to
            // the TTY at this point disrupts the CLI's initial terminal handshake.
            // A profile's env_file contributes environment instead, merged into the
            // spawn environment rather than sourced into the terminal.
            let argv: Vec<String> = profile.argv().iter().map(|a| shell_quote(a)).collect();
            vec!["-ic".to_string(), format!("exec {}", argv.join(" "))]
        }
        None => vec!["-ic".to_string(), "exec $SHELL".to_string()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn profile(name: &str, command: &str) -> Profile {
        Profile {
            name: name.to_string(),
            command: command.to_string(),
            args: Vec::new(),
            dir: None,
            env_file: None,
        }
    }

    /// A stand-in for command lookup. The real probe spawns `which` and an
    /// interactive shell, which made the old tests depend on what happened to be
    /// installed on the machine running them.
    fn only<'a>(installed: &'a [&'a str]) -> impl FnMut(&str) -> bool + 'a {
        move |command| installed.contains(&command)
    }

    #[test]
    fn resolve_prefers_the_selected_profile() {
        let profiles = vec![profile("Claude", "claude"), profile("Gemini", "gemini")];
        let chosen = resolve_profile(&profiles, Some("Gemini"), only(&["claude", "gemini"]));
        assert_eq!(chosen.map(|p| p.name.as_str()), Some("Gemini"));
    }

    #[test]
    fn resolve_falls_back_when_the_selected_command_is_missing() {
        // Selecting a client you have not installed should still give you a
        // working terminal rather than the welcome screen.
        let profiles = vec![profile("Claude", "claude"), profile("Gemini", "gemini")];
        let chosen = resolve_profile(&profiles, Some("Gemini"), only(&["claude"]));
        assert_eq!(chosen.map(|p| p.name.as_str()), Some("Claude"));
    }

    #[test]
    fn resolve_falls_back_when_the_selected_profile_no_longer_exists() {
        let profiles = vec![profile("Claude", "claude")];
        let chosen = resolve_profile(&profiles, Some("Deleted"), only(&["claude"]));
        assert_eq!(chosen.map(|p| p.name.as_str()), Some("Claude"));
    }

    #[test]
    fn resolve_without_a_preference_takes_the_first_installed() {
        // List order is the preference order, as CliClient::Auto used to encode.
        let profiles = vec![
            profile("Claude", "claude"),
            profile("Agy", "agy"),
            profile("Gemini", "gemini"),
        ];
        let chosen = resolve_profile(&profiles, None, only(&["agy", "gemini"]));
        assert_eq!(chosen.map(|p| p.name.as_str()), Some("Agy"));
    }

    #[test]
    fn resolve_yields_nothing_when_no_command_is_installed() {
        let profiles = vec![profile("Claude", "claude")];
        assert!(resolve_profile(&profiles, None, only(&[])).is_none());
    }

    #[test]
    fn startup_command_execs_the_profile_command() {
        // exec so the CLI owns the TTY; without it the CLI runs as a child job
        // and comes up degraded.
        let cmd = get_startup_command(Some(&profile("Claude", "claude")));
        assert_eq!(cmd[0], "-ic");
        assert_eq!(cmd[1], "exec 'claude'");
    }

    #[test]
    fn startup_command_includes_profile_arguments() {
        let mut p = profile("Claude", "claude");
        p.args = vec!["--model".to_string(), "opus".to_string()];
        assert_eq!(
            get_startup_command(Some(&p))[1],
            "exec 'claude' '--model' 'opus'"
        );
    }

    #[test]
    fn startup_command_quotes_arguments_against_the_wrapping_shell() {
        // The command string is handed to `$SHELL -ic`, so an unquoted argument
        // with a space would be re-split and one with a $ would be expanded.
        let mut p = profile("Claude", "claude");
        p.args = vec!["a b".to_string(), "$HOME".to_string(), "it's".to_string()];
        let rendered = get_startup_command(Some(&p))[1].clone();
        assert!(rendered.contains("'a b'"), "{rendered}");
        assert!(rendered.contains("'$HOME'"), "{rendered}");
        assert!(rendered.contains(r"'it'\''s'"), "{rendered}");
    }

    #[test]
    fn startup_command_without_a_profile_falls_back_to_the_shell() {
        let cmd = get_startup_command(None);
        assert_eq!(cmd[0], "-ic");
        assert_eq!(cmd[1], "exec $SHELL");
    }

    #[test]
    fn command_cache_records_negatives_as_well_as_hits() {
        // A negative is the expensive case worth remembering: it costs a `which`,
        // five path checks and an interactive shell to establish.
        clear_command_cache();
        assert_eq!(cached_command_available("claude"), None);
        cache_command_available("claude", false);
        assert_eq!(cached_command_available("claude"), Some(false));
        cache_command_available("agy", true);
        assert_eq!(cached_command_available("agy"), Some(true));
        assert_eq!(cached_command_available("claude"), Some(false));
        clear_command_cache();
        assert_eq!(cached_command_available("agy"), None);
    }

    #[test]
    fn env_file_contributes_exported_variables() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("env.sh");
        // No `export` on the second one: `set -a` must still pick it up.
        std::fs::write(&path, "export FOO=bar\nBAZ=qux\n").unwrap();

        let env = load_env_file(path.to_str().unwrap());
        let get = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
        assert_eq!(get("FOO"), Some("bar"));
        assert_eq!(get("BAZ"), Some("qux"));
    }

    #[test]
    fn a_missing_env_file_is_ignored_rather_than_fatal() {
        assert!(load_env_file("/nonexistent/env.sh").is_empty());
    }

    #[test]
    fn env_file_returns_only_what_it_changed() {
        // `env` prints the whole environment, nearly all of it inherited. Passing
        // that through would append ~100 redundant entries per session and name
        // every variable the user has set — secrets included — in the logs.
        let dir = tempdir().unwrap();
        let path = dir.path().join("env.sh");
        std::fs::write(&path, "export AGENT_TERMINAL_TEST_ONLY=yes\n").unwrap();

        let env = load_env_file(path.to_str().unwrap());
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();

        assert!(keys.contains(&"AGENT_TERMINAL_TEST_ONLY"));
        // PATH and HOME are inherited unchanged, so they must not come back.
        assert!(
            !keys.contains(&"PATH"),
            "inherited PATH leaked through: {keys:?}"
        );
        assert!(
            !keys.contains(&"HOME"),
            "inherited HOME leaked through: {keys:?}"
        );
        assert!(!keys.contains(&"SHLVL"), "shell bookkeeping leaked through");
    }

    #[test]
    fn env_file_can_override_an_inherited_value() {
        // Overriding is the point of the feature, so a variable that exists but
        // differs must still come through.
        let dir = tempdir().unwrap();
        let path = dir.path().join("env.sh");
        std::fs::write(&path, "export TERM=dumb-for-test\n").unwrap();

        let env = load_env_file(path.to_str().unwrap());
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "TERM")
                .map(|(_, v)| v.as_str()),
            Some("dumb-for-test")
        );
    }

    #[test]
    fn strip_env_removes_the_parent_session_markers() {
        // The bug: a terminal launched from inside an agent session passed that
        // session's identity to the CLI it spawned, which then treated itself as
        // a nested child and turned off transcript saving.
        let mut env = vec![
            "PATH=/usr/bin".to_string(),
            "CLAUDE_CODE_CHILD_SESSION=1".to_string(),
            "CLAUDECODE=1".to_string(),
            "CLAUDE_CODE_SESSION_ID=abc123".to_string(),
            "HOME=/home/someone".to_string(),
        ];
        let removed = strip_env(&mut env, &crate::config::default_clear_env());

        assert!(env.contains(&"PATH=/usr/bin".to_string()));
        assert!(env.contains(&"HOME=/home/someone".to_string()));
        assert!(!env.iter().any(|e| e.starts_with("CLAUDE")));
        assert!(removed.contains(&"CLAUDE_CODE_CHILD_SESSION".to_string()));
        assert_eq!(removed.len(), 3);
    }

    #[test]
    fn strip_env_leaves_settings_that_share_the_prefix() {
        // Why the default list is specific rather than a CLAUDE_CODE_* sweep:
        // authentication, provider selection and user preferences share that
        // prefix, and silently dropping them would break a working setup.
        let mut env = vec![
            "CLAUDE_CODE_OAUTH_TOKEN=secret".to_string(),
            "CLAUDE_CODE_USE_BEDROCK=1".to_string(),
            "CLAUDE_CODE_ENABLE_TELEMETRY=1".to_string(),
            "CLAUDE_CONFIG_DIR=/home/someone/.claude".to_string(),
        ];
        let removed = strip_env(&mut env, &crate::config::default_clear_env());

        assert!(removed.is_empty(), "unexpectedly removed {removed:?}");
        assert_eq!(env.len(), 4);
    }

    #[test]
    fn strip_env_supports_a_trailing_wildcard() {
        let mut env = vec![
            "MYAPP_SESSION=1".to_string(),
            "MYAPP_TOKEN=x".to_string(),
            "MYAPPLE=keep".to_string(),
            "OTHER=keep".to_string(),
        ];
        let removed = strip_env(&mut env, &["MYAPP_*".to_string()]);

        assert_eq!(removed.len(), 2);
        assert!(env.contains(&"OTHER=keep".to_string()));
        // The underscore is part of the prefix, so MYAPPLE is untouched.
        assert!(env.contains(&"MYAPPLE=keep".to_string()));
    }

    #[test]
    fn strip_env_with_no_patterns_changes_nothing() {
        let mut env = vec!["A=1".to_string(), "B=2".to_string()];
        assert!(strip_env(&mut env, &[]).is_empty());
        assert_eq!(env.len(), 2);
    }

    #[test]
    fn strip_env_leaves_malformed_entries_alone() {
        // An entry with no '=' is not ours to interpret.
        let mut env = vec!["NOT_AN_ASSIGNMENT".to_string(), "A=1".to_string()];
        strip_env(&mut env, &["NOT_AN_ASSIGNMENT".to_string()]);
        assert_eq!(env.len(), 2);
    }

    #[test]
    fn tilde_paths_are_expanded_for_config_supplied_files() {
        // People write ~/... in config; nothing else expands it for us, and a
        // path handed to a shell in quotes stays literal.
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            expand_tilde("~/reports/drift.txt"),
            format!("{home}/reports/drift.txt")
        );
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("/absolute/path"), "/absolute/path");
        assert_eq!(expand_tilde("  ~/spaced  "), format!("{home}/spaced"));
        // ~otheruser is someone else's home and not ours to guess at.
        assert_eq!(expand_tilde("~root/x"), "~root/x");
    }

    #[test]
    fn an_indicator_file_path_may_use_a_tilde() {
        let home = std::env::var("HOME").unwrap();
        let state = read_indicator(&crate::config::IndicatorSource::File {
            path: "~/definitely-not-a-real-report-xyz.txt".to_string(),
        });
        match state {
            // The reported path proves the tilde was expanded before the read.
            IndicatorState::Unknown { reason } => assert!(reason.contains(&home), "{reason}"),
            other => panic!("expected unknown, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_indicator_file_is_unknown_not_ok() {
        // The bug this whole feature exists to prevent: the predecessor read a
        // path that no longer existed, treated the failure as "no drift", and
        // rendered a green shield for a system it had never checked.
        let state = read_indicator(&crate::config::IndicatorSource::File {
            path: "/nonexistent/drift_report.txt".to_string(),
        });
        assert!(
            matches!(state, IndicatorState::Unknown { .. }),
            "an unreadable source must not read as healthy, got {state:?}"
        );
    }

    #[test]
    fn an_empty_indicator_file_is_ok_and_a_populated_one_warns() {
        let dir = tempdir().unwrap();

        let empty = dir.path().join("empty.txt");
        std::fs::write(&empty, "\n   \n").unwrap();
        assert_eq!(
            read_indicator(&crate::config::IndicatorSource::File {
                path: empty.to_string_lossy().to_string(),
            }),
            IndicatorState::Ok
        );

        let populated = dir.path().join("drift.txt");
        std::fs::write(&populated, "role x drifted\n").unwrap();
        let state = read_indicator(&crate::config::IndicatorSource::File {
            path: populated.to_string_lossy().to_string(),
        });
        match state {
            IndicatorState::Warn { detail } => assert!(detail.contains("role x drifted")),
            other => panic!("expected a warning, got {other:?}"),
        }
    }

    #[test]
    fn indicator_command_exit_status_selects_the_state() {
        assert_eq!(
            read_indicator(&crate::config::IndicatorSource::Command {
                argv: vec!["true".to_string()],
                timeout_secs: 5,
            }),
            IndicatorState::Ok
        );

        let state = read_indicator(&crate::config::IndicatorSource::Command {
            argv: vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo bad; exit 1".to_string(),
            ],
            timeout_secs: 5,
        });
        match state {
            IndicatorState::Warn { detail } => assert!(detail.contains("bad")),
            other => panic!("expected a warning, got {other:?}"),
        }
    }

    #[test]
    fn an_unrunnable_indicator_command_is_unknown() {
        let state = read_indicator(&crate::config::IndicatorSource::Command {
            argv: vec!["definitely-not-a-real-command-xyz".to_string()],
            timeout_secs: 5,
        });
        assert!(matches!(state, IndicatorState::Unknown { .. }), "{state:?}");
    }

    #[test]
    fn an_empty_indicator_command_is_unknown() {
        let state = read_indicator(&crate::config::IndicatorSource::Command {
            argv: Vec::new(),
            timeout_secs: 5,
        });
        assert!(matches!(state, IndicatorState::Unknown { .. }), "{state:?}");
    }

    #[test]
    fn a_hanging_indicator_command_times_out() {
        // A configured command is arbitrary; without a bound it would hold a
        // worker thread for the life of the process.
        let state = read_indicator(&crate::config::IndicatorSource::Command {
            argv: vec!["sleep".to_string(), "30".to_string()],
            timeout_secs: 1,
        });
        match state {
            IndicatorState::Unknown { reason } => assert!(reason.contains("did not finish")),
            other => panic!("expected a timeout, got {other:?}"),
        }
    }

    #[test]
    fn test_resolve_working_directory() {
        let dir = tempdir().unwrap();
        let home_dir = dir.path().to_str().unwrap().to_string();

        // Empty and whitespace-only both mean "use home".
        assert_eq!(resolve_working_directory("", &home_dir), home_dir);
        assert_eq!(resolve_working_directory("  ", &home_dir), home_dir);

        // A path that does not exist falls back rather than failing to spawn.
        assert_eq!(
            resolve_working_directory("/non/existent/path", &home_dir),
            home_dir
        );

        assert_eq!(resolve_working_directory("~", &home_dir), home_dir);

        let sub_dir = dir.path().join("projects");
        std::fs::create_dir_all(&sub_dir).unwrap();
        let sub_dir_str = sub_dir.to_str().unwrap();

        assert_eq!(
            resolve_working_directory(sub_dir_str, &home_dir),
            sub_dir_str
        );
        assert_eq!(
            resolve_working_directory("~/projects", &home_dir),
            sub_dir_str
        );
    }
}
