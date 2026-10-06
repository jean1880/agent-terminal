//! Utility functions for Agent Terminal.

use crate::config::Profile;
// Pure session-store, path and process helpers live in `agent-kit`; the app
// keeps reaching them through `crate::utils`.
pub use agent_kit::exec::run_command;
pub use agent_kit::paths::{expand_tilde, expand_tilde_with};
pub use agent_kit::sessions::{
    find_session_dir_in, find_transcript, list_sessions_in, validate_session_id, SessionSummary,
};
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

/// How long sourcing the user's shell rc, or a profile's env file, may take
/// before it is abandoned. Generous: a heavy rc (nvm, conda) takes seconds.
pub const SHELL_PROBE_TIMEOUT_SECS: u64 = 15;

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

    fn on_path(&self, command: &str) -> Option<String> {
        let mut cmd = std::process::Command::new("which");
        cmd.arg(command);
        if let Some(path) = self.path.as_deref().filter(|p| !p.is_empty()) {
            cmd.env("PATH", path);
        }
        match cmd.output() {
            Ok(output) if output.status.success() => {
                let found = String::from_utf8_lossy(&output.stdout).trim().to_string();
                info!("{command} found via 'which' at {found}");
                Some(found).filter(|f| !f.is_empty())
            }
            _ => None,
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
    ///
    /// Bounded by [`SHELL_PROBE_TIMEOUT_SECS`]: an rc that blocks — on the
    /// network, a keychain prompt — would otherwise leave the window on its
    /// "Starting up…" page forever.
    fn known_to_interactive_shell(&self, command: &str) -> Option<String> {
        let shell = self.shell.as_deref().unwrap_or("/bin/sh");
        let mut cmd = std::process::Command::new(shell);
        // Single-quoted, so a name with a space or a `$` is looked up as
        // written. Spliced rather than passed as "$1": fish takes extra -c
        // arguments as $argv and has no $1, and single quotes mean the same
        // thing in sh, bash, zsh and fish.
        cmd.args(["-ic", &format!("command -v {}", shell_quote(command))]);
        if let Some(path) = self.path.as_deref().filter(|p| !p.is_empty()) {
            cmd.env("PATH", path);
        }
        match run_command(cmd, shell, SHELL_PROBE_TIMEOUT_SECS) {
            Ok(output) if output.status.success() => {
                // An rc may print a banner before the answer; the path is the last line.
                let stdout = String::from_utf8_lossy(&output.stdout);
                let found = stdout.lines().last().unwrap_or_default().trim().to_string();
                info!("{command} found via shell -ic at {found}");
                Some(found).filter(|f| !f.is_empty())
            }
            Ok(_) => None,
            Err(reason) => {
                warn!("Interactive shell check for {command} gave up: {reason}");
                None
            }
        }
    }

    /// Whether `command` can be launched, checked in increasing order of cost.
    pub fn command_available(&self, command: &str) -> bool {
        self.locate(command).is_some()
    }

    /// Where `command` runs from, by the same checks as [`Self::command_available`]. A
    /// command found only by the interactive shell (an nvm install) is not on the app's own
    /// PATH, so a process the app spawns directly needs this path rather than the name.
    pub fn locate(&self, command: &str) -> Option<String> {
        debug!("Resolving command {command}");

        if let Some(found) = self.on_path(command) {
            return Some(found);
        }

        for path in self.candidate_paths(command) {
            if !path.is_empty() && std::path::Path::new(&path).exists() {
                debug!("{command} found at {path}");
                return Some(path);
            }
        }

        if let Some(found) = self.known_to_interactive_shell(command) {
            return Some(found);
        }

        warn!("{command} not found after all checks");
        None
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
    // Bounded, like the shell probe: the tab waits on this before spawning,
    // so a file that blocks would leave it on its loading screen for good.
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(r#"set -a; . "$1" >/dev/null 2>&1 || exit 1; env -0"#)
        .arg("sh")
        .arg(path);
    let output = match run_command(cmd, "/bin/sh", SHELL_PROBE_TIMEOUT_SECS) {
        Ok(output) if output.status.success() => output,
        Ok(_) => {
            warn!("Env file {path} could not be sourced; ignoring it");
            return Vec::new();
        }
        Err(reason) => {
            warn!("Reading env file {path} failed: {reason}; ignoring it");
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
    match run_capture(command, args, None, timeout_secs) {
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
        Err(reason) => IndicatorState::Unknown { reason },
    }
}

/// Runs a command, optionally in `cwd`, and collects its output, giving up
/// after `timeout_secs`. `Err` carries a sentence for the user.
///
/// Output is drained on its own threads while the command runs. Waiting first
/// and reading after would deadlock on any command that fills the pipe buffer
/// (64 KiB on Linux) — `git diff` on a large change, for one — which would
/// then be reported as a timeout.
///
/// Blocking: call it off the main thread.
pub fn run_capture(
    command: &str,
    args: &[String],
    cwd: Option<&str>,
    timeout_secs: u64,
) -> Result<std::process::Output, String> {
    let mut cmd = std::process::Command::new(command);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    run_command(cmd, command, timeout_secs)
}

/// Resolves the working directory to use, handling ~ expansion and fallback to home.
///
/// A path that exists but is not a directory falls back too: the spawn would
/// otherwise fail on it.
pub fn resolve_working_directory(starting_dir: &str, home_dir: &str) -> String {
    let work_dir = expand_tilde_with(starting_dir, home_dir);
    if work_dir.is_empty() {
        return home_dir.to_string();
    }
    if std::path::Path::new(&work_dir).is_dir() {
        work_dir
    } else {
        warn!("Starting directory '{work_dir}' is not a directory, falling back to home directory");
        home_dir.to_string()
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

/// The argv that resumes `session_id` with `profile`, or `None` when the
/// profile declares no way to resume.
///
/// The ID is substituted into the profile's `resume_args` rather than simply
/// appended, because CLIs disagree on the shape (`--resume <id>`,
/// `resume <id>`, `--session=<id>`).
pub fn resume_argv(profile: &Profile, session_id: &str) -> Option<Vec<String>> {
    if !profile.can_resume() {
        return None;
    }
    let mut argv = profile.argv();
    argv.extend(fill(profile.resume_args.as_deref()?, "{id}", session_id));
    Some(argv)
}

/// Renders how long ago something happened, for a list row.
pub fn describe_age(elapsed: std::time::Duration) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    match elapsed.as_secs() {
        s if s < MINUTE => "just now".to_string(),
        s if s < HOUR => format!("{} min ago", s / MINUTE),
        s if s < DAY => format!("{} h ago", s / HOUR),
        s if s < 2 * DAY => "yesterday".to_string(),
        s if s < 60 * DAY => format!("{} days ago", s / DAY),
        s => format!("{} months ago", s / (30 * DAY)),
    }
}

/// Shortens a path under `home` to `~/…` for display.
pub fn tildify(path: &str, home: &str) -> String {
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return path.to_string();
    }
    match path.strip_prefix(home) {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// How a session starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Launch<'a> {
    /// A new session, as the profile's plain argv.
    #[default]
    Fresh,
    /// Resume an existing session. Excludes everything else: a resumed
    /// session already has its ID and its conversation.
    Resume(&'a str),
    /// A new session, optionally under an ID the terminal chose (only if the
    /// profile can pin one) and with an initial prompt (only if it can take one).
    New {
        session_id: Option<&'a str>,
        prompt: Option<&'a str>,
    },
}

/// Substitutes `value` for `placeholder` in each template argument.
///
/// Whole-argument values only ever land inside one argv entry, and every entry
/// is shell-quoted afterwards, so a value cannot become a second argument or a
/// shell construct.
fn fill(template: &[String], placeholder: &str, value: &str) -> Vec<String> {
    template
        .iter()
        .map(|arg| arg.replace(placeholder, value))
        .collect()
}

/// The argv that starts `launch` with `profile`, command first.
fn launch_argv(profile: &Profile, launch: Launch<'_>) -> Vec<String> {
    match launch {
        Launch::Fresh => profile.argv(),
        Launch::Resume(id) => resume_argv(profile, id).unwrap_or_else(|| {
            warn!(
                "Profile '{}' cannot resume sessions; starting a new one",
                profile.name
            );
            profile.argv()
        }),
        Launch::New { session_id, prompt } => {
            let mut argv = profile.argv();
            if let (Some(id), Some(args)) = (session_id, profile.session_id_args.as_deref()) {
                argv.extend(fill(args, "{id}", id));
            }
            match (prompt, profile.prompt_args.as_deref()) {
                (Some(prompt), Some(args)) if !args.is_empty() => {
                    argv.extend(fill(args, "{prompt}", prompt));
                }
                (Some(_), _) => warn!(
                    "Profile '{}' cannot take an initial prompt; starting without it",
                    profile.name
                ),
                (None, _) => {}
            }
            argv
        }
    }
}

/// Determines the startup command for a profile.
///
/// A profile that cannot do what `launch` asks — resume, or take a prompt —
/// starts a plain session with a warning. Callers are expected to pick a
/// capable profile first; this is only the safety net.
pub fn get_startup_command(profile: Option<&Profile>, launch: Launch<'_>) -> Vec<String> {
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
            let argv: Vec<String> = launch_argv(profile, launch)
                .iter()
                .map(|a| shell_quote(a))
                .collect();
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
            ..Profile::default()
        }
    }

    fn resumable(name: &str, command: &str) -> Profile {
        let mut p = profile(name, command);
        p.resume_args = Some(vec!["--resume".to_string(), "{id}".to_string()]);
        p
    }

    #[test]
    fn resume_substitutes_the_id_after_the_profile_arguments() {
        let mut p = resumable("Claude", "claude");
        p.args = vec!["--model".to_string(), "opus".to_string()];
        assert_eq!(
            resume_argv(&p, "abc-123"),
            Some(vec![
                "claude".to_string(),
                "--model".to_string(),
                "opus".to_string(),
                "--resume".to_string(),
                "abc-123".to_string(),
            ])
        );
    }

    #[test]
    fn a_profile_without_resume_args_cannot_resume() {
        assert_eq!(resume_argv(&profile("Agy", "agy"), "abc"), None);
        let mut empty = profile("Claude", "claude");
        empty.resume_args = Some(Vec::new());
        assert_eq!(resume_argv(&empty, "abc"), None);
    }

    #[test]
    fn startup_command_resumes_when_asked() {
        let cmd = get_startup_command(
            Some(&resumable("Claude", "claude")),
            Launch::Resume("abc-123"),
        );
        assert_eq!(cmd[1], "exec 'claude' '--resume' 'abc-123'");
    }

    #[test]
    fn startup_command_ignores_resume_for_a_profile_that_cannot() {
        let cmd = get_startup_command(Some(&profile("Agy", "agy")), Launch::Resume("abc-123"));
        assert_eq!(cmd[1], "exec 'agy'");
    }

    fn handoff_capable(name: &str, command: &str) -> Profile {
        let mut p = resumable(name, command);
        p.session_id_args = Some(vec!["--session-id".to_string(), "{id}".to_string()]);
        p.prompt_args = Some(vec![
            "--prompt-interactive".to_string(),
            "{prompt}".to_string(),
        ]);
        p
    }

    #[test]
    fn a_new_session_pins_its_id_then_takes_its_prompt() {
        let launch = Launch::New {
            session_id: Some("abc-123"),
            prompt: Some("Read the brief"),
        };
        let cmd = get_startup_command(Some(&handoff_capable("Claude", "claude")), launch);
        assert_eq!(
            cmd[1],
            "exec 'claude' '--session-id' 'abc-123' '--prompt-interactive' 'Read the brief'"
        );
    }

    #[test]
    fn resume_wins_over_everything_a_new_session_would_add() {
        let cmd = get_startup_command(
            Some(&handoff_capable("Claude", "claude")),
            Launch::Resume("abc-123"),
        );
        assert_eq!(cmd[1], "exec 'claude' '--resume' 'abc-123'");
    }

    #[test]
    fn a_profile_without_the_templates_starts_plain() {
        let launch = Launch::New {
            session_id: Some("abc-123"),
            prompt: Some("Read the brief"),
        };
        let cmd = get_startup_command(Some(&profile("Gemini", "gemini")), launch);
        assert_eq!(cmd[1], "exec 'gemini'");
    }

    #[test]
    fn a_prompt_cannot_break_out_of_its_argument() {
        let launch = Launch::New {
            session_id: None,
            prompt: Some("it's $(reboot); `id` --dangerously-skip-permissions"),
        };
        let cmd = get_startup_command(Some(&handoff_capable("Claude", "claude")), launch);
        assert_eq!(
            cmd[1],
            r"exec 'claude' '--prompt-interactive' 'it'\''s $(reboot); `id` --dangerously-skip-permissions'"
        );
    }

    #[test]
    fn ages_read_naturally() {
        use std::time::Duration;
        assert_eq!(describe_age(Duration::from_secs(5)), "just now");
        assert_eq!(describe_age(Duration::from_secs(300)), "5 min ago");
        assert_eq!(describe_age(Duration::from_secs(2 * 3600)), "2 h ago");
        assert_eq!(describe_age(Duration::from_secs(30 * 3600)), "yesterday");
        assert_eq!(describe_age(Duration::from_secs(5 * 86400)), "5 days ago");
        assert_eq!(
            describe_age(Duration::from_secs(90 * 86400)),
            "3 months ago"
        );
    }

    #[test]
    fn paths_under_home_are_shortened() {
        assert_eq!(tildify("/home/u/git/x", "/home/u"), "~/git/x");
        assert_eq!(tildify("/home/u", "/home/u/"), "~");
        // A sibling that merely shares the prefix is not under home.
        assert_eq!(tildify("/home/user2/x", "/home/u"), "/home/user2/x");
        assert_eq!(tildify("/srv/x", ""), "/srv/x");
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
        let cmd = get_startup_command(Some(&profile("Claude", "claude")), Launch::Fresh);
        assert_eq!(cmd[0], "-ic");
        assert_eq!(cmd[1], "exec 'claude'");
    }

    #[test]
    fn startup_command_includes_profile_arguments() {
        let mut p = profile("Claude", "claude");
        p.args = vec!["--model".to_string(), "opus".to_string()];
        assert_eq!(
            get_startup_command(Some(&p), Launch::Fresh)[1],
            "exec 'claude' '--model' 'opus'"
        );
    }

    #[test]
    fn startup_command_quotes_arguments_against_the_wrapping_shell() {
        // The command string is handed to `$SHELL -ic`, so an unquoted argument
        // with a space would be re-split and one with a $ would be expanded.
        let mut p = profile("Claude", "claude");
        p.args = vec!["a b".to_string(), "$HOME".to_string(), "it's".to_string()];
        let rendered = get_startup_command(Some(&p), Launch::Fresh)[1].clone();
        assert!(rendered.contains("'a b'"), "{rendered}");
        assert!(rendered.contains("'$HOME'"), "{rendered}");
        assert!(rendered.contains(r"'it'\''s'"), "{rendered}");
    }

    #[test]
    fn startup_command_without_a_profile_falls_back_to_the_shell() {
        let cmd = get_startup_command(None, Launch::Fresh);
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
    fn a_background_job_holding_the_pipes_does_not_outlast_the_timeout() {
        // What an rc's `foo &` does to a probe: the shell exits, the job keeps
        // its stdout. The call must come back, with what was printed, shortly
        // after the exit — not at the (here generous) overall timeout.
        let started = std::time::Instant::now();
        let output = run_capture(
            "sh",
            &["-c".to_string(), "echo found; sleep 30 &".to_string()],
            None,
            10,
        )
        .expect("the command itself finished");
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "found");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "waited on the background job: {:?}",
            started.elapsed()
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

        // A file is not somewhere a session can start.
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(
            resolve_working_directory(file.to_str().unwrap(), &home_dir),
            home_dir
        );

        // `~user` is someone else's home: not spliced onto ours.
        assert_eq!(expand_tilde_with("~projects", &home_dir), "~projects");
    }
}
