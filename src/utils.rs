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
        assert!(!keys.contains(&"PATH"), "inherited PATH leaked through: {keys:?}");
        assert!(!keys.contains(&"HOME"), "inherited HOME leaked through: {keys:?}");
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
