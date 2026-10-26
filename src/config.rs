// How a profile's `session_store` is laid out; defined with the readers in
// `agent-kit` and re-exported so config and the app keep their old path.
pub use agent_kit::difftool::DiffTool;
pub use agent_kit::sessions::SessionFormat;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::warn;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "lowercase")]
pub enum CliClient {
    #[default]
    Auto,
    Gemini,
    Agy,
    Claude,
}

impl std::fmt::Display for CliClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliClient::Auto => write!(f, "Auto-detect"),
            CliClient::Gemini => write!(f, "Gemini"),
            CliClient::Agy => write!(f, "Antigravity"),
            CliClient::Claude => write!(f, "Claude"),
        }
    }
}

impl CliClient {
    /// All choices in display order; the index matches the settings dropdown.
    /// Built from this rather than a hand-written index match in both directions,
    /// which is what let the dropdown and the enum drift apart.
    pub const ALL: [CliClient; 4] = [
        CliClient::Auto,
        CliClient::Gemini,
        CliClient::Agy,
        CliClient::Claude,
    ];
}

/// A launchable session: a name, the command to run, and where to run it.
///
/// This replaces the closed `CliClient` enum. Adding a fourth CLI used to mean
/// editing the enum, its `Display`, four arms of the detection function and two
/// hand-maintained index↔variant mappings in the settings dialog; it is now a
/// `config.json` edit with no recompile.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// Stable opaque identity, `^[a-z0-9-]{1,64}$` (see [`new_profile_id`]). Unlike `name` it
    /// survives a rename and a reorder, so anything that must keep pointing at this profile
    /// refers to it by id. Absent in older files: migration assigns one, once, and persists it.
    /// This is configuration identity, not a secret or a token.
    #[serde(default)]
    pub id: String,
    /// Shown in the settings dropdown and the new-tab menu. Also the key that
    /// `default_profile` refers to, so it must be unique.
    pub name: String,
    /// The command to exec. Resolved against PATH, common install directories,
    /// and the user's interactive shell.
    pub command: String,
    /// Extra arguments appended to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Where to root sessions for this profile. `None` falls back to the global
    /// starting directory, which itself falls back to `$HOME`.
    #[serde(default)]
    pub dir: Option<String>,
    /// A shell file sourced in a subshell whose exported environment is merged
    /// into the session's. This is the only safe shape for the old
    /// "startup script" idea: the file must contribute environment, never bytes
    /// written to the TTY, because output before `exec` breaks the CLI's
    /// terminal handshake.
    #[serde(default)]
    pub env_file: Option<String>,
    /// Arguments that resume a specific session, appended after `args`, with
    /// `{id}` standing for the session ID — `["--resume", "{id}"]` for Claude.
    /// `None` or empty means this profile cannot resume a session.
    #[serde(default)]
    pub resume_args: Option<Vec<String>>,
    /// Where this CLI keeps its session transcripts, as `<id>.jsonl` directly
    /// inside it or one directory down. Used to find the directory a session
    /// must be resumed from: a CLI that scopes sessions per project cannot see
    /// one from anywhere else. Each record may carry a `cwd`; the first wins.
    #[serde(default)]
    pub session_store: Option<String>,
    /// A JSON pointer (RFC 6901) to a session's title inside a transcript
    /// record, e.g. `/aiTitle`. Used by the session browser; the latest match
    /// wins, because a CLI may retitle a session as it goes. `None` lists
    /// sessions by ID.
    #[serde(default)]
    pub session_title: Option<String>,
    /// How `session_store` is laid out. See [`SessionFormat`].
    #[serde(default)]
    pub session_format: SessionFormat,
    /// Arguments that start a *new* session under an ID the terminal chooses,
    /// with `{id}` standing for it — `["--session-id", "{id}"]` for Claude.
    /// Knowing the ID up front is what lets the terminal find the transcript of
    /// a session it started, for quota detection and hand-off briefs.
    #[serde(default)]
    pub session_id_args: Option<Vec<String>>,
    /// Arguments that start an interactive session with an initial prompt, with
    /// `{prompt}` standing for it — `["{prompt}"]` for Claude. Used to hand a
    /// task over from another profile. `None` or empty: cannot take a hand-off.
    #[serde(default)]
    pub prompt_args: Option<Vec<String>>,
    /// Case-insensitive text that, seen near the bottom of the screen, means the
    /// CLI has run out of quota. The fallback for a CLI whose transcript carries
    /// no structured signal; Claude's is read from the transcript instead.
    #[serde(default)]
    pub limit_markers: Option<Vec<String>>,
    /// 3.0 chat threads: the model a new thread on this agent starts with.
    /// `None` leaves it to the CLI's own default. Set from Preferences → Agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// 3.0 chat threads: the interaction mode a new thread starts in.
    /// `None` means ask before edits and commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_mode: Option<agent_core::adapter::Mode>,
    /// 3.0 chat threads: the reasoning effort a new thread asks for, where the
    /// agent supports one. `None` leaves it to the CLI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    /// Hides this agent from the new-thread choices. Stored as `disabled`, not
    /// `enabled`, so a profile written by 2.x (which has neither) stays usable.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    /// Opt in to supervised cross-agent delegation, as a parent and as a child target.
    /// Existing profiles remain unavailable until the user enables them in Settings.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub available_for_delegation: bool,
    /// The chat agent this profile was made for, recorded by Preferences → Agents (and
    /// [`new_agent_profile`]) so a command whose file name is not the agent's (a half-typed
    /// name, `claude-nightly`) keeps the profile that agent's. `None` (hand-written and older
    /// profiles): judged by the command's file name alone. See [`profile_driver`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<agent_core::adapter::Driver>,
}

/// A blank profile with a fresh id, so the invariant holds for `..Profile::default()` too.
/// (Serde's default for an absent `id` is the empty string, not this: migration relies on
/// seeing it empty.)
impl Default for Profile {
    fn default() -> Self {
        Profile {
            id: new_profile_id(),
            name: String::new(),
            command: String::new(),
            args: Vec::new(),
            dir: None,
            env_file: None,
            resume_args: None,
            session_store: None,
            session_title: None,
            session_format: SessionFormat::default(),
            session_id_args: None,
            prompt_args: None,
            limit_markers: None,
            default_model: None,
            default_mode: None,
            default_effort: None,
            disabled: false,
            available_for_delegation: false,
            driver: None,
        }
    }
}

/// Whether `id` has the profile-id shape: `^[a-z0-9-]{1,64}$`.
pub fn is_valid_profile_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A fresh profile id: `p-` and 16 lowercase hex characters.
///
/// Only uniqueness matters, not secrecy: an id names a profile in the config and is never a
/// credential. The bits come from `RandomState` (randomly keyed per process by the OS), mixed
/// with the clock, the process id and a counter, so two ids made in one nanosecond still
/// differ. The app has no RNG dependency and this adds none.
pub fn new_profile_id() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    hasher.write_u32(std::process::id());
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    format!("p-{:016x}", hasher.finish())
}

/// The chat driver a profile's command runs, judged by the command's file
/// name: `claude`, `agy` or `codex`, wherever it is installed. A file name that
/// names no agent falls back to the agent the profile records ([`Profile::driver`]),
/// so `claude-nightly` set in Preferences → Agents stays Claude's. Anything else
/// (Gemini, a hand-written wrapper script) has no adapter and runs as a Terminal
/// thread only. A file name that names another agent wins over the record, so a
/// hand edit to another CLI never runs it under the wrong adapter.
pub fn profile_driver(profile: &Profile) -> Option<agent_core::adapter::Driver> {
    command_driver(&profile.command).or(profile.driver)
}

/// The driver whose default command is `command`'s file name.
fn command_driver(command: &str) -> Option<agent_core::adapter::Driver> {
    use agent_core::adapter::Driver;
    let name = Path::new(command.trim())
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())?;
    Driver::ALL
        .into_iter()
        .find(|d| d.info().default_command == name)
}

/// A fresh profile for `driver`'s chat agent, with the known CLI settings filled in. Used when
/// Preferences → Agents edits an agent the profile list has no entry for.
pub fn new_agent_profile(driver: agent_core::adapter::Driver) -> Profile {
    let info = driver.info();
    let mut profile = Profile {
        name: info.profile_name.to_string(),
        command: info.default_command.to_string(),
        driver: Some(driver),
        ..Profile::default()
    };
    apply_known_settings(&mut profile);
    profile
}

/// Picks the agent for a new chat thread.
///
/// In order: the explicit "default agent" choice, the default profile's agent
/// (when it has a chat driver), then every driver in registry order. Only an agent
/// `usable` accepts (detected and enabled: see `availability`) is picked; `None` when
/// none is.
pub fn choose_default_agent(
    explicit: Option<agent_core::adapter::Driver>,
    default_profile: Option<agent_core::adapter::Driver>,
    usable: impl Fn(agent_core::adapter::Driver) -> bool,
) -> Option<agent_core::adapter::Driver> {
    [explicit, default_profile]
        .into_iter()
        .flatten()
        .chain(agent_core::adapter::Driver::ALL)
        .find(|d| usable(*d))
}

/// The model configured in `driver`'s own settings file (`settings.json`, `config.toml`).
pub fn agent_settings_model(
    driver: agent_core::adapter::Driver,
    home: Option<&Path>,
) -> Option<String> {
    let home_buf = if cfg!(test) {
        home.map(PathBuf::from)
    } else {
        home.map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
    };
    let home = home_buf.as_deref()?;
    match driver {
        agent_core::adapter::Driver::Claude => claude_settings_model(home),
        agent_core::adapter::Driver::Agy => agy_settings_model(home),
        agent_core::adapter::Driver::Codex => codex_settings_model(home),
    }
}

/// The model configured in the settings file of the agent running `command`.
pub fn agent_settings_model_for_command(command: &str, home: Option<&Path>) -> Option<String> {
    agent_settings_model(command_driver(command)?, home)
}

fn claude_settings_model(home: &Path) -> Option<String> {
    let settings_path = home.join(".claude/settings.json");
    let content = std::fs::read_to_string(settings_path).ok()?;
    let val: serde_json::Value = serde_json::from_str(&content).ok()?;
    val.get("model")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn agy_settings_model(home: &Path) -> Option<String> {
    let cli_path = home.join(".gemini/antigravity-cli/settings.json");
    let gemini_path = home.join(".gemini/settings.json");
    let content = std::fs::read_to_string(&cli_path)
        .or_else(|_| std::fs::read_to_string(&gemini_path))
        .ok()?;
    let val: serde_json::Value = serde_json::from_str(&content).ok()?;
    let raw = val.get("model")?.as_str()?.trim();
    if raw.is_empty() {
        return None;
    }
    Some(normalize_agy_model_setting(raw))
}

fn normalize_agy_model_setting(model: &str) -> String {
    let trimmed = model.trim();
    if let Some(open) = trimmed.rfind('(') {
        if let Some(close) = trimmed[open..].find(')') {
            let effort = trimmed[open + 1..open + close].trim().to_lowercase();
            let base_display = trimmed[..open].trim();
            let base_slug = base_display.to_lowercase().replace(' ', "-");
            return format!("{base_slug}-{effort}");
        }
    }
    if trimmed.contains(' ') {
        trimmed.to_lowercase().replace(' ', "-")
    } else {
        trimmed.to_string()
    }
}

fn codex_settings_model(home: &Path) -> Option<String> {
    let config_path = home.join(".codex/config.toml");
    if let Ok(content) = std::fs::read_to_string(&config_path) {
        if let Some(model) = parse_toml_top_level_model(&content) {
            return Some(model);
        }
    }
    let cache_path = home.join(".codex/models_cache.json");
    if let Ok(content) = std::fs::read_to_string(&cache_path) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(models) = val.get("models").and_then(serde_json::Value::as_array) {
                if let Some(first) = models.first() {
                    if let Some(slug) = first.get("slug").and_then(serde_json::Value::as_str) {
                        let trimmed = slug.trim();
                        if !trimmed.is_empty() {
                            return Some(trimmed.to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

fn parse_toml_top_level_model(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('[') {
            break;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            if k.trim() == "model" {
                let v = v.trim();
                let stripped = v.trim_matches(|c| c == '"' || c == '\'');
                if !stripped.is_empty() {
                    return Some(stripped.to_string());
                }
            }
        }
    }
    None
}

impl Profile {
    /// A copy of this profile with a new id and `name`, for "duplicate". The copy must never
    /// share the original's id, or the two would be one profile to anything that refers to ids.
    pub fn duplicate(&self, name: impl Into<String>) -> Profile {
        Profile {
            id: new_profile_id(),
            name: name.into(),
            ..self.clone()
        }
    }

    /// The full argv to exec, command first.
    pub fn argv(&self) -> Vec<String> {
        let mut argv = Vec::with_capacity(1 + self.args.len());
        argv.push(self.command.clone());
        argv.extend(self.args.iter().cloned());
        argv
    }

    /// Whether this profile knows how to resume a session.
    pub fn can_resume(&self) -> bool {
        self.resume_args.as_ref().is_some_and(|a| !a.is_empty())
    }

    /// Whether this profile can start a session under a chosen ID.
    pub fn can_pin_session_id(&self) -> bool {
        self.session_id_args.as_ref().is_some_and(|a| !a.is_empty())
    }

    /// Whether this profile can start with an initial prompt, and so take a
    /// hand-off.
    pub fn can_take_prompt(&self) -> bool {
        self.prompt_args.as_ref().is_some_and(|a| !a.is_empty())
    }

    /// The default model configured for this profile: either explicitly chosen on the profile,
    /// or pulled from the agent's settings file when left on the CLI's default.
    pub fn effective_default_model(&self, home: Option<&Path>) -> Option<String> {
        self.default_model
            .clone()
            .or_else(|| agent_settings_model_for_command(&self.command, home))
    }
}

/// Settings for a CLI whose conventions are known.
struct KnownProfile {
    resume_args: Vec<String>,
    store: String,
    title: Option<String>,
    format: SessionFormat,
    session_id_args: Option<Vec<String>>,
    prompt_args: Vec<String>,
    limit_markers: Option<Vec<String>>,
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

/// The settings for CLIs whose conventions are known, keyed by command.
///
/// Claude: `--resume`/`--session-id`, the `~/.claude/projects/<dir>/<id>.jsonl`
/// layout and `aiTitle` records were checked against the installed CLI and its
/// transcripts; its quota signal is structured, so it needs no markers.
///
/// Agy: `--conversation <id>` and `--prompt-interactive` were read from the
/// binary's own flag text, and `history.jsonl` from a real log. Whether
/// `--conversation` accepts an ID it has never seen is unknown, so it gets no
/// `session_id_args`. Its quota message has not been captured yet; the markers
/// are the Google API's error names.
///
/// Another CLI gets these by declaring the fields in config, not by an entry here.
fn known_profile_settings(command: &str) -> Option<KnownProfile> {
    match command {
        "claude" => Some(KnownProfile {
            resume_args: strings(&["--resume", "{id}"]),
            store: "~/.claude/projects".to_string(),
            title: Some("/aiTitle".to_string()),
            format: SessionFormat::Jsonl,
            session_id_args: Some(strings(&["--session-id", "{id}"])),
            prompt_args: strings(&["{prompt}"]),
            limit_markers: None,
        }),
        "agy" => Some(KnownProfile {
            resume_args: strings(&["--conversation", "{id}"]),
            store: "~/.gemini/antigravity-cli/history.jsonl".to_string(),
            title: None,
            format: SessionFormat::AgyHistory,
            session_id_args: None,
            prompt_args: strings(&["--prompt-interactive", "{prompt}"]),
            limit_markers: Some(strings(&["RESOURCE_EXHAUSTED", "quota exceeded"])),
        }),
        "codex" => Some(KnownProfile {
            resume_args: strings(&["app-server"]),
            store: "~/.codex/sessions".to_string(),
            title: None,
            format: SessionFormat::Jsonl,
            session_id_args: None,
            prompt_args: strings(&["{prompt}"]),
            limit_markers: None,
        }),
        _ => None,
    }
}

/// Fills in whatever a known CLI's profile has not set.
///
/// Only an absent field is filled: an explicit empty list (`"resume_args": []`)
/// is an opt-out and stays one. Resume and store go together, because a store
/// without the arguments, or the reverse, is a half-configured profile the user
/// chose. The title pointer and format follow the store only when it is the
/// known one — a custom store's layout is not ours to guess.
fn apply_known_settings(profile: &mut Profile) {
    let Some(known) = known_profile_settings(&profile.command) else {
        return;
    };
    if profile.resume_args.is_none() && profile.session_store.is_none() {
        profile.resume_args = Some(known.resume_args);
        profile.session_store = Some(known.store.clone());
        profile.session_format = known.format;
    }
    if profile.session_title.is_none()
        && profile.session_store.as_deref() == Some(known.store.as_str())
    {
        profile.session_title = known.title;
    }
    if profile.session_id_args.is_none() {
        profile.session_id_args = known.session_id_args;
    }
    if profile.prompt_args.is_none() {
        profile.prompt_args = Some(known.prompt_args);
    }
    if profile.limit_markers.is_none() {
        profile.limit_markers = known.limit_markers;
    }
}

/// The profiles a fresh install starts with: the chat agents, in preference order. The
/// standalone Gemini CLI is not one (Gemini is reached through agy).
fn default_profiles() -> Vec<Profile> {
    ["Claude", "Antigravity", "Codex"]
        .iter()
        .map(|name| {
            let cmd = if *name == "Antigravity" {
                "agy".to_string()
            } else {
                name.to_lowercase()
            };
            let mut profile = Profile {
                name: (*name).to_string(),
                command: cmd,
                ..Profile::default()
            };
            apply_known_settings(&mut profile);
            profile
        })
        .collect()
}

/// Where an indicator gets its state from.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum IndicatorSource {
    /// Non-empty file contents mean "warn"; the contents are the detail.
    File { path: String },
    /// A non-zero exit means "warn"; stdout is the detail.
    Command {
        argv: Vec<String>,
        #[serde(default = "default_indicator_timeout")]
        timeout_secs: u64,
    },
}

fn default_indicator_timeout() -> u64 {
    10
}

/// What clicking an indicator does.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum IndicatorAction {
    /// Show the detail in a dialog.
    #[default]
    ShowOutput,
    /// Show the detail, with an explicit button to type it into the session.
    ///
    /// Deliberately still a preview: this writes unreviewed content into a live
    /// agent's stdin, which is tolerable for a source you hard-coded yourself and
    /// not for one anybody can configure.
    SendToTerminal,
}

/// A header-bar status light driven by a file or a command.
///
/// Replaces a single hard-coded Ansible-drift button that read a path which no
/// longer existed — and, because a missing file read as "no drift", reported
/// itself permanently green. Nothing here can do that: an unreadable source is
/// its own state with its own icon.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Indicator {
    #[serde(alias = "name")]
    pub label: String,
    pub source: IndicatorSource,
    /// How often to re-check. `None` checks once at startup.
    #[serde(default)]
    pub refresh_secs: Option<u64>,
    #[serde(default = "default_icon_ok", alias = "icon")]
    pub icon_ok: String,
    #[serde(default = "default_icon_warn")]
    pub icon_warn: String,
    #[serde(default = "default_icon_unknown")]
    pub icon_unknown: String,
    #[serde(default)]
    pub action: IndicatorAction,
}

fn default_icon_ok() -> String {
    "at-security-high-symbolic".to_string()
}
fn default_icon_warn() -> String {
    "at-dialog-warning-symbolic".to_string()
}
fn default_icon_unknown() -> String {
    "at-dialog-question-symbolic".to_string()
}

/// Ceiling on header indicators, so a config edit cannot fill the header bar.
pub const MAX_INDICATORS: usize = 5;

/// The app's colours: the desktop's own, or one of the bundled palettes.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeChoice {
    /// No colours of its own: everything comes from the desktop's GTK theme (light or dark, its
    /// accent, any `gtk.css`), and follows it when it changes. See `crate::palette`.
    System,
    /// The app's own palette. Was named after Antigravity, the CLI the app first drove; the old
    /// name still loads.
    #[default]
    #[serde(alias = "antigravity")]
    AgentTerminal,
    Dracula,
    Nord,
    GruvboxDark,
    SolarizedDark,
    OneDark,
    Monokai,
}

impl std::fmt::Display for ThemeChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            ThemeChoice::System => "Use System Theme",
            ThemeChoice::AgentTerminal => "Agent Terminal",
            ThemeChoice::Dracula => "Dracula",
            ThemeChoice::Nord => "Nord",
            ThemeChoice::GruvboxDark => "Gruvbox Dark",
            ThemeChoice::SolarizedDark => "Solarized Dark",
            ThemeChoice::OneDark => "One Dark",
            ThemeChoice::Monokai => "Monokai",
        };
        f.write_str(name)
    }
}

impl ThemeChoice {
    /// All choices in display order; the index matches the settings dropdown.
    pub const ALL: [ThemeChoice; 8] = [
        ThemeChoice::System,
        ThemeChoice::AgentTerminal,
        ThemeChoice::Dracula,
        ThemeChoice::Nord,
        ThemeChoice::GruvboxDark,
        ThemeChoice::SolarizedDark,
        ThemeChoice::OneDark,
        ThemeChoice::Monokai,
    ];
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(default)]
/// Persisted settings.
///
/// `#[serde(default)]` is a container attribute here, so any field missing from
/// the file falls back to that field's value in [`TerminalConfig::default`] — not
/// to the field type's own default, which would silently turn an absent
/// `scrollback_lines` into 0 rather than 10000. This is what lets settings be
/// added and retired without invalidating existing config files.
pub struct TerminalConfig {
    /// Retained so existing config files keep round-tripping, but no longer
    /// surfaced in Settings: nothing ever consumed it. Sourcing a script into the
    /// TTY before `exec` breaks the CLI's terminal handshake, so when this returns
    /// it will be as a per-profile environment file merged into the spawn
    /// environment rather than fed to the terminal.
    pub startup_script: String,
    pub scrollback_lines: u32,
    pub font_scale: f64,
    /// Superseded by `profiles`/`default_profile` in 2.0. Still parsed so a 1.x
    /// config keeps its chosen client across the upgrade; see [`Self::normalize`].
    pub cli_client: CliClient,
    pub starting_directory: String,
    pub theme: ThemeChoice,
    /// The sessions offered in Settings and the new-tab menu.
    pub profiles: Vec<Profile>,
    /// Which profile to launch. `None` means "use the first one whose command is
    /// actually installed", the behaviour the old `CliClient::Auto` had.
    pub default_profile: Option<String>,
    /// Pango font description for the terminal. Was a hard-coded constant while
    /// the *scale* was configurable, which is an odd place to draw the line.
    pub font: String,
    pub cursor_shape: CursorShapeChoice,
    pub cursor_blink: bool,
    /// Send a desktop notification when a background tab rings the bell — which
    /// is how a CLI announces it has finished and wants attention.
    pub notify_on_bell: bool,
    /// Send a desktop notification, with a hand-off button, when a session
    /// runs out of quota. On by default, unlike the bell: it is rare, and it
    /// means work has stopped.
    pub notify_on_quota: bool,
    /// Ask GitHub, in the background a few seconds after startup, whether a newer release is
    /// out; one found marks the window menu. On by default (an older config opts in); off, the
    /// app connects to nothing unless "Check for Updates…" is picked.
    pub check_updates_on_startup: bool,
    /// Minutes a thread out of sight may sit idle before its agent process is stopped to free
    /// its memory (an agent holds hundreds of MB); its conversation resumes with the next
    /// prompt. 0 never stops one.
    pub idle_suspend_minutes: u32,
    /// Reopen the previous window's tabs on launch. Off by default: a restored
    /// tab gets its directory and profile back but starts a fresh conversation,
    /// so by default it only multiplies blank sessions.
    pub restore_session: bool,
    /// Snapshot a tab's git working tree into a hidden ref when a turn ends,
    /// so each turn can be diffed. On by default: a history only helps if it
    /// was already being kept when something went wrong. See `crate::git`.
    pub checkpoints: bool,
    /// Whether new tabs open with the diff panel showing. Set by toggling it.
    pub diff_panel_visible: bool,
    /// The diff panel's width in pixels, as last dragged.
    pub diff_panel_width: i32,
    /// Where New Tab in Worktree puts worktrees, as `<root>/<repo>/<branch>`.
    /// Blank means a hidden sibling of the repository,
    /// `<parent>/.<repo>.worktrees/<branch>`.
    pub worktree_root: String,
    /// Header-bar status lights. Empty by default: this is an extension point,
    /// not a feature every user wants.
    pub indicators: Vec<Indicator>,
    /// Optional command executed when an agent turn ends (on output quiescence).
    /// Executed asynchronously in the background. `{id}` expands to the session ID if known,
    /// `{dir}` expands to the tab's working directory.
    #[serde(default)]
    pub turn_command: Option<Vec<String>>,
    /// 3.0: the external diff tool the file-change cards' and the diff panel's "Open in …"
    /// buttons launch. `None` (the default, and what 2.x and early 3.0 configs read as) hides
    /// the buttons. See [`DiffTool`] for the argv template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_tool: Option<DiffTool>,
    /// 3.0: a file-change card opens its diff as soon as the edit completes, instead of
    /// waiting for "View diff". Off by default; cards replayed from history stay collapsed.
    pub diffs_expanded: bool,
    /// Environment variables removed from a spawned session's environment.
    /// A trailing `*` matches by prefix. See [`default_clear_env`].
    pub clear_env: Vec<String>,
    /// 3.0: the agent new chat threads start on. `None` follows the default
    /// profile when it is Claude or agy, else the first installed of the two.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_agent: Option<agent_core::adapter::Driver>,
    /// 3.0: whether the thread sidebar is collapsed. Follows its toggle.
    pub sidebar_collapsed: bool,
    /// Open the window as soon as it is ready, without the logo intro. Off by default: the
    /// splash then plays in full (at least `loading::SPLASH_MIN_MS`) even when everything
    /// loaded at once. On: a skeleton of the window shows only while it really is loading.
    pub skip_load_animation: bool,
    /// Reopen the chat threads open at the last close, showing the one that was in view. Off
    /// by default: the window starts with no thread open, the sidebar listing them all.
    pub reopen_last_thread: bool,
    /// List the agents' own recent sessions (started in a terminal, or by another tool) as
    /// threads in the sidebar, with a toast saying how many were added. On by default (an older
    /// config opts in); off, the startup scan of the agents' session files does not run.
    #[serde(default = "default_true")]
    pub list_outside_sessions: bool,
    /// Automatically approve tools matching pattern rules (wildcards/prefixes).
    #[serde(default = "default_true")]
    pub pattern_auto_approval: bool,
    /// When adding an approval rule, prompt to confirm or loosen the pattern.
    #[serde(default = "default_true")]
    pub confirm_rule_modification: bool,
    /// Offer quick replies under a Codex or agy reply that ends with a question and a short
    /// list of choices (`agent_core::question_extract`). On by default; "Don't suggest these"
    /// on the chips turns it off.
    #[serde(default = "default_true")]
    pub suggest_quick_replies: bool,
    /// Extra files in `permissions.toml` form that "Import Agent Permissions" reads and "Export"
    /// writes, beside the agents' own settings files. Empty by default: the app names no
    /// project's layout. Paths are absolute or start with `~/`; see
    /// [`parse_permission_sources`].
    pub permission_sources: Vec<String>,
    /// The model tag file `cross_agent_capabilities` reads (what each model is good for; format in
    /// `agent_core::model_tags`). `None` (the default, and what older configs read as) is
    /// `<config dir>/model-tags.json`; a set path is absolute or starts with `~/`. A missing file
    /// only means models carry no tags.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_tags_path: Option<String>,
    /// Set when this process found no settings file at all (none from a pre-rename release
    /// either): a first start, which offers the setup walkthrough. Not persisted.
    #[serde(skip)]
    first_run: std::cell::Cell<bool>,
    /// The file as this process last read or wrote it, so a hand edit made in
    /// between can be told apart from its own writes. Not persisted.
    #[serde(skip)]
    disk_stamp: std::cell::Cell<Option<DiskStamp>>,
    /// Set while writing would destroy something the user has not got another
    /// copy of: an unusable file that could not be copied aside, or a hand
    /// edit in progress that does not parse yet. Cleared by loading a good
    /// file. Not persisted.
    #[serde(skip)]
    save_blocked: std::cell::Cell<bool>,
    /// Set when [`Self::normalize`] gave a profile an id (a migration, or a repair), so the
    /// loader persists the result once. Not persisted.
    #[serde(skip)]
    ids_assigned: std::cell::Cell<bool>,
}

/// The permission sources typed in Settings, `:`-separated like `PATH`: each an absolute path or
/// one starting `~/`, naming a file that exists. Blank is no sources. The error names the first
/// entry that cannot be used, so the previous setting stays in force until it is fixed.
pub fn parse_permission_sources(text: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for entry in text.split(':').map(str::trim).filter(|e| !e.is_empty()) {
        if !(entry.starts_with('/') || entry.starts_with("~/")) {
            return Err(format!("{entry} is not an absolute path (or ~/…)"));
        }
        let path = PathBuf::from(crate::utils::expand_tilde(entry));
        if !path.is_file() {
            return Err(format!("{entry} is not an existing file"));
        }
        if !out.iter().any(|e| e == entry) {
            out.push(entry.to_owned());
        }
    }
    Ok(out)
}

/// What a free-text settings row does with its text ([`field_change`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldChange<T> {
    /// The text does not parse: show why. The saved value stays in use.
    Invalid(String),
    /// Valid, and nothing is to be written now.
    Keep,
    /// Valid and different from the saved value: write it.
    Persist(T),
}

/// The rule for a free-text settings row: validate on every change, persist only on commit.
///
/// `parsed` is the row's text as parsed. `saved` is the value in use, and `commit` says
/// whether the user committed the text (Enter, the apply button, focus leaving the row, or the
/// dialog closing) rather than still typing. A valid draft is never written while it is being
/// typed, so a half-typed value never reaches the config (or restarts an agent). An invalid
/// text is reported both while typing and on commit, and the saved value is kept.
pub fn field_change<T: PartialEq>(
    parsed: Result<T, String>,
    saved: &T,
    commit: bool,
) -> FieldChange<T> {
    match parsed {
        Err(reason) => FieldChange::Invalid(reason),
        Ok(value) if commit && value != *saved => FieldChange::Persist(value),
        Ok(_) => FieldChange::Keep,
    }
}

impl TerminalConfig {
    /// [`Self::permission_sources`] as paths (`~/` expanded), for the import and export code.
    pub fn permission_source_paths(&self) -> Vec<PathBuf> {
        self.permission_sources
            .iter()
            .map(|s| PathBuf::from(crate::utils::expand_tilde(s)))
            .collect()
    }
}

/// Identifies one version of a file on disk. The inode catches an editor
/// that saves by renaming a new file into place; the length catches a write
/// within one coarse timestamp tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DiskStamp {
    modified: std::time::SystemTime,
    len: u64,
    inode: u64,
}

fn disk_stamp(path: &Path) -> Option<DiskStamp> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(path).ok()?;
    Some(DiskStamp {
        modified: meta.modified().ok()?,
        len: meta.len(),
        inode: meta.ino(),
    })
}

/// What [`TerminalConfig::check_disk`] found.
pub enum DiskChange {
    /// The file is as this process last saw it (or is absent).
    Unchanged,
    /// Changed elsewhere, and valid: the settings as they now stand.
    Updated(Box<TerminalConfig>),
    /// Changed elsewhere, and unusable. Saving is suspended until it is fixed,
    /// so an edit in progress is not overwritten; the text says so.
    Invalid(String),
}

/// Why the last load fell back to defaults, for the UI to report once.
#[cfg(not(test))]
static LOAD_PROBLEM: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

#[cfg(test)]
thread_local! {
    // Parallel config fixtures must not open a modal in the unrelated GTK window test.
    static LOAD_PROBLEM: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Takes the reason the settings could not be loaded, if they could not.
pub fn take_load_problem() -> Option<String> {
    #[cfg(not(test))]
    {
        LOAD_PROBLEM.lock().ok()?.take()
    }
    #[cfg(test)]
    LOAD_PROBLEM.with(|slot| slot.borrow_mut().take())
}

fn report_load_problem(problem: String) {
    warn!("{problem}");
    #[cfg(not(test))]
    if let Ok(mut slot) = LOAD_PROBLEM.lock() {
        *slot = Some(problem);
    }
    #[cfg(test)]
    LOAD_PROBLEM.with(|slot| *slot.borrow_mut() = Some(problem));
}

/// Copies `path` to `<name>.<tag>-<nanos>` beside it, so a file about to be
/// replaced is kept rather than lost. `None` if the copy failed.
///
/// An identical copy already kept is reused, so a file that stays broken
/// across launches leaves one copy rather than one per launch. The copy is
/// written `0600`, like the config itself.
fn keep_copy(path: &Path, tag: &str) -> Option<PathBuf> {
    let content = match fs::read(path) {
        Ok(content) => content,
        Err(err) => {
            warn!("Could not keep a copy of {}: {err}", path.display());
            return None;
        }
    };
    let prefix = format!("{}.{tag}-", path.file_name()?.to_string_lossy());
    let existing = path.parent().and_then(|dir| {
        fs::read_dir(dir)
            .ok()?
            .flatten()
            .map(|entry| entry.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
            })
            .filter(|p| reusable_copy(p))
            .find(|p| fs::read(p).is_ok_and(|kept| kept == content))
    });
    if let Some(existing) = existing {
        // An older build wrote its copies with the umask (0644): narrow one before reusing it.
        // A copy that cannot be narrowed is not reused; a fresh private one is written.
        if make_private(&existing) {
            return Some(existing);
        }
    }

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let copy = path.with_file_name(format!("{prefix}{nanos}"));
    // The config can hold tokens (profile env, indicator commands): the copy is as private as
    // the file it keeps, never left to the umask.
    match agent_kit::fsutil::write_private_atomic(&copy, &content) {
        Ok(()) => Some(copy),
        Err(err) => {
            warn!("Could not keep a copy of {}: {err}", path.display());
            None
        }
    }
}

/// Whether a file named as a kept copy may be reused: a regular file with one link (not a
/// symlink or a hard link, so neither its contents nor a mode change reach another path) owned
/// by this user.
fn reusable_copy(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(path).is_ok_and(|m| {
        m.file_type().is_file() && m.uid() == agent_kit::fsutil::effective_uid() && m.nlink() == 1
    })
}

/// Narrows a kept copy to `0600` when it is wider, through a handle opened without following a
/// symlink, so the mode change lands on the very file checked. `false` when it cannot be done.
fn make_private(path: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let Ok(file) = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    else {
        return false;
    };
    let Ok(meta) = file.metadata() else {
        return false;
    };
    // A second link is another path to the same inode: its mode is not this copy's to change.
    if !meta.file_type().is_file()
        || meta.uid() != agent_kit::fsutil::effective_uid()
        || meta.nlink() != 1
    {
        return false;
    }
    meta.permissions().mode() & 0o077 == 0
        || file
            .set_permissions(fs::Permissions::from_mode(0o600))
            .is_ok()
}

thread_local! {
    /// [`TerminalConfig::suggest_quick_replies`] as the running app has it, for the chat session
    /// that decides whether to offer chips (it has no window to ask). The window keeps it in
    /// step with its settings; on until told otherwise, so a test sees the default.
    static QUICK_REPLIES: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// Whether quick replies are on right now (see [`TerminalConfig::suggest_quick_replies`]).
pub fn quick_replies_on() -> bool {
    QUICK_REPLIES.with(std::cell::Cell::get)
}

/// Records the setting where [`quick_replies_on`] reads it.
pub fn mirror_quick_replies(on: bool) {
    QUICK_REPLIES.with(|c| c.set(on));
}

/// Hands the approval switches to the gates that read them without a window.
///
/// The policy is process-wide and last-writer-wins: each window sets it from its own config
/// on load and on change, and the settings file monitor keeps the windows in step.
pub fn mirror_approval_policy(config: &TerminalConfig) {
    crate::always_allow::set_saved_rules_enabled(config.pattern_auto_approval);
    crate::always_allow::set_confirm_rule_modification(config.confirm_rule_modification);
}

const fn default_true() -> bool {
    true
}

/// Variables stripped from a session's inherited environment by default.
///
/// Sessions inherit the terminal's whole environment on purpose — that is how
/// nvm- and asdf-managed CLIs stay reachable. But when the terminal is itself
/// launched from inside an agent session, the *parent session's* identity comes
/// along with it, and the CLI we spawn concludes it is a nested child of that
/// session. The visible symptom is Claude Code disabling transcript saving with
/// "inherited CLAUDE_CODE_CHILD_SESSION marker".
///
/// That is not an exotic case for this application: launching it from a terminal
/// where you are already running an agent, or developing it, does exactly this.
///
/// The list is deliberately specific rather than a blanket `CLAUDE_CODE_*` sweep.
/// That prefix also covers authentication (`CLAUDE_CODE_OAUTH_TOKEN`), provider
/// selection (`CLAUDE_CODE_USE_BEDROCK`) and user preferences
/// (`CLAUDE_CODE_ENABLE_TELEMETRY`) — settings people deliberately export and
/// would be baffled to lose. Every name here was taken from the installed CLI's
/// own string table, and each identifies a session or connects to one.
pub fn default_clear_env() -> Vec<String> {
    [
        // "I am running inside an agent session", and which one.
        "CLAUDECODE",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_BRIDGE_SESSION_ID",
        "CLAUDE_CODE_CLOUD_SESSION_ID",
        "CLAUDE_CODE_REMOTE_SESSION_ID",
        "CLAUDE_CODE_REMOTE_SESSION_UUID",
        "CLAUDE_SESSION_ID",
        "CLAUDE_PID",
        // Handles onto the parent session — a socket and its token. Inheriting
        // these points the new session's IPC at the old session.
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CLAUDE_CODE_MESSAGING_TOKEN",
        "CLAUDE_CODE_SESSION_ACCESS_TOKEN",
        // How the parent was started, and what it was doing.
        "CLAUDE_CODE_EXECPATH",
        "CLAUDE_CODE_PROCESS_WRAPPER",
        "CLAUDE_CODE_SPAWN_TIMESTAMP_MS",
        "CLAUDE_CODE_AGENT",
        "CLAUDE_CODE_SUPERVISED",
        "CLAUDE_CODE_TASK_LIST_ID",
        "CLAUDE_CODE_TRIGGER_ID",
        "CLAUDE_CODE_WORKER_EPOCH",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

/// The terminal cursor shape.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum CursorShapeChoice {
    #[default]
    Block,
    Ibeam,
    Underline,
}

impl std::fmt::Display for CursorShapeChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CursorShapeChoice::Block => "Block",
            CursorShapeChoice::Ibeam => "I-beam",
            CursorShapeChoice::Underline => "Underline",
        })
    }
}

impl CursorShapeChoice {
    /// All choices in display order; the index matches the settings dropdown.
    pub const ALL: [CursorShapeChoice; 3] = [
        CursorShapeChoice::Block,
        CursorShapeChoice::Ibeam,
        CursorShapeChoice::Underline,
    ];
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            startup_script: "~/.config/agent-terminal/startup.sh".to_string(),
            scrollback_lines: 10000,
            font_scale: 1.0,
            cli_client: CliClient::default(),
            starting_directory: String::new(),
            theme: ThemeChoice::default(),
            profiles: default_profiles(),
            default_profile: None,
            font: "JetBrains Mono, Fira Code, Monospace 11".to_string(),
            cursor_shape: CursorShapeChoice::default(),
            cursor_blink: true,
            notify_on_bell: false,
            notify_on_quota: true,
            check_updates_on_startup: true,
            idle_suspend_minutes: 15,
            restore_session: false,
            checkpoints: true,
            diff_panel_visible: false,
            diff_panel_width: 520,
            worktree_root: String::new(),
            indicators: Vec::new(),
            turn_command: None,
            diff_tool: None,
            diffs_expanded: false,
            clear_env: default_clear_env(),
            default_agent: None,
            sidebar_collapsed: false,
            skip_load_animation: false,
            reopen_last_thread: false,
            list_outside_sessions: true,
            pattern_auto_approval: true,
            confirm_rule_modification: true,
            suggest_quick_replies: true,
            permission_sources: Vec::new(),
            model_tags_path: None,
            first_run: std::cell::Cell::default(),
            disk_stamp: std::cell::Cell::default(),
            save_blocked: std::cell::Cell::default(),
            ids_assigned: std::cell::Cell::default(),
        }
    }
}

/// One restored tab: which profile it ran and where.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SessionTab {
    /// `None` means the tab ran whatever auto-detection resolved.
    #[serde(default)]
    pub profile: Option<String>,
    pub dir: String,
}

/// The tabs that were open when the window last closed.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(default)]
pub struct SessionState {
    pub tabs: Vec<SessionTab>,
    pub selected: usize,
}

impl SessionState {
    /// Ceiling on restored tabs. A corrupt or hand-edited session file must not
    /// be able to spawn an unbounded number of PTYs at launch.
    pub const MAX_TABS: usize = 20;

    fn path() -> PathBuf {
        TerminalConfig::config_dir().join("session.json")
    }

    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    /// Reads the session file. Anything unreadable or unparseable yields an empty
    /// session — starting fresh is always safe, so a bad file must never be fatal.
    fn load_from(path: &Path) -> Self {
        let Ok(content) = fs::read_to_string(path) else {
            return Self::default();
        };
        match serde_json::from_str::<Self>(&content) {
            Ok(mut state) => {
                if state.tabs.len() > Self::MAX_TABS {
                    warn!(
                        "Session file lists {} tabs; restoring the first {}",
                        state.tabs.len(),
                        Self::MAX_TABS
                    );
                    state.tabs.truncate(Self::MAX_TABS);
                }
                if state.selected >= state.tabs.len() {
                    state.selected = 0;
                }
                state
            }
            Err(e) => {
                warn!("Ignoring unreadable session file: {e}");
                Self::default()
            }
        }
    }

    pub fn save(&self) {
        self.save_to(&Self::path());
    }

    fn save_to(&self, path: &Path) {
        match serde_json::to_string_pretty(self) {
            Ok(content) => {
                if let Err(e) = TerminalConfig::replace_atomically(path, content.as_bytes()) {
                    warn!("Failed to write session to {}: {}", path.display(), e);
                }
            }
            Err(e) => warn!("Failed to serialize session: {e}"),
        }
    }
}

impl TerminalConfig {
    /// The base directory for user configuration.
    ///
    /// Honours `XDG_CONFIG_HOME` per the XDG Base Directory specification, which
    /// this previously ignored in favour of a hardcoded `~/.config` — so anyone
    /// who had relocated their config directory silently got a second one. It is
    /// also what makes an isolated instance possible:
    ///
    /// ```text
    /// XDG_CONFIG_HOME=/tmp/scratch agent-terminal
    /// ```
    ///
    /// Both the current and the pre-2.0 directory hang off this, so an override
    /// moves the migration source with it rather than leaving it pointed at the
    /// real home directory.
    fn config_home() -> PathBuf {
        Self::config_home_from(
            std::env::var("XDG_CONFIG_HOME").ok(),
            std::env::var("HOME").ok(),
        )
    }

    /// The resolution itself, with the environment passed in.
    ///
    /// Injected rather than read here so it can be tested without mutating
    /// process-global state, the same way CLI detection takes its environment as
    /// parameters.
    fn config_home_from(xdg_config_home: Option<String>, home: Option<String>) -> PathBuf {
        // An empty value counts as unset, per the specification.
        if let Some(dir) = xdg_config_home.filter(|d| !d.trim().is_empty()) {
            return PathBuf::from(dir);
        }
        PathBuf::from(home.unwrap_or_else(|| "/".to_string())).join(".config")
    }

    pub fn config_dir() -> PathBuf {
        let dir = Self::config_home().join("agent-terminal");
        if !dir.exists() {
            let _ = fs::create_dir_all(&dir);
        }
        dir
    }

    pub fn config_path() -> PathBuf {
        Self::config_dir().join("config.json")
    }

    /// The model tag file: [`Self::model_tags_path`] with `~/` expanded, or
    /// `<config dir>/model-tags.json` when it is unset or blank.
    pub fn model_tags_file(&self) -> PathBuf {
        Self::model_tags_file_in(
            self.model_tags_path.as_deref(),
            &Self::config_home().join("agent-terminal"),
        )
    }

    /// [`Self::model_tags_file`] with the config directory passed in (tests).
    fn model_tags_file_in(set: Option<&str>, config_dir: &Path) -> PathBuf {
        match set.map(str::trim).filter(|s| !s.is_empty()) {
            Some(p) => PathBuf::from(crate::utils::expand_tilde(p)),
            None => config_dir.join("model-tags.json"),
        }
    }

    /// The pre-2.0 configuration directory, kept only so settings can be carried
    /// across the rename.
    fn legacy_config_path() -> PathBuf {
        Self::config_home()
            .join("antigravity-terminal")
            .join("config.json")
    }

    pub fn load() -> Self {
        Self::load_or_migrate(&Self::config_path(), &Self::legacy_config_path())
    }

    /// Loads the config, adopting a pre-rename one the first time if present.
    ///
    /// The old file is deliberately left in place rather than moved. Deleting it
    /// would make a rollback to v1.x — which is `apt install antigravity-terminal`,
    /// a *different package* — come up with no settings at all. Leaving it costs
    /// one stale file and keeps the downgrade path intact.
    fn load_or_migrate(path: &Path, legacy_path: &Path) -> Self {
        if !path.exists() && !legacy_path.exists() {
            let config = Self::default();
            config.first_run.set(true);
            return config;
        }
        if !path.exists() && legacy_path.exists() {
            // Only a legacy file that actually parses is adopted. Writing
            // defaults to the new path in its place would make fixing the old
            // file afterwards pointless, since the new one wins from then on.
            match Self::read_config(legacy_path) {
                Ok(Some(migrated)) => {
                    warn!(
                        "Adopting settings from {} into {} (the original is left in place)",
                        legacy_path.display(),
                        path.display()
                    );
                    migrated.save_to(path);
                    return migrated;
                }
                Ok(None) => {}
                Err(problem) => {
                    // Saving now would create the new file and end the chance
                    // to adopt the old one, so nothing is saved this run.
                    report_load_problem(format!(
                        "Your previous settings at {} {problem}, so defaults are in use and \
                         nothing will be saved this session. Fix that file and relaunch to \
                         carry it over.",
                        legacy_path.display()
                    ));
                    let config = Self::load_from(path);
                    config.save_blocked.set(true);
                    return config;
                }
            }
        }
        Self::load_from(path)
    }

    /// Fills in anything a pre-2.0 config could not have carried.
    ///
    /// A 1.x file has no `profiles` and no `default_profile`, but it does have a
    /// `cli_client`. Dropping that on the floor would silently move a user who had
    /// pinned Gemini back to auto-detection, so it is translated into the
    /// equivalent profile selection instead.
    fn normalize(mut self) -> Self {
        if self.profiles.is_empty() {
            self.profiles = default_profiles();
        }

        // Migrate "Agy" profile name to "Antigravity" if "Antigravity" is not already defined.
        if !self.profiles.iter().any(|p| p.name == "Antigravity") {
            for profile in &mut self.profiles {
                if profile.name == "Agy" {
                    profile.name = "Antigravity".to_string();
                }
            }
            if self.default_profile.as_deref() == Some("Agy") {
                self.default_profile = Some("Antigravity".to_string());
            }
        }

        // Names are the key every menu and action looks a profile up by, so a
        // second "Claude" would be unreachable. Renamed rather than dropped:
        // the next save writes this list back, and dropping would delete it.
        // A new name must not collide with one a later profile already has.
        let mut taken: std::collections::HashSet<String> =
            self.profiles.iter().map(|p| p.name.clone()).collect();
        let mut seen = std::collections::HashSet::new();
        for profile in &mut self.profiles {
            if seen.insert(profile.name.clone()) {
                continue;
            }
            let original = profile.name.clone();
            let unique = (2..)
                .map(|n| format!("{original} ({n})"))
                .find(|name| !taken.contains(name))
                .unwrap_or_else(|| original.clone());
            warn!("Profile name '{original}' is used twice; the second is now '{unique}'");
            taken.insert(unique.clone());
            seen.insert(unique.clone());
            profile.name = unique;
        }

        self.repair_profile_ids();

        // Profiles saved by an older release lack whatever came after it —
        // resume, the browser's title pointer, hand-off — so a persisted
        // Claude or Agy profile would otherwise never gain them.
        for profile in &mut self.profiles {
            apply_known_settings(profile);
        }

        if self.default_profile.is_none() && self.cli_client != CliClient::Auto {
            let carried = self.cli_client.to_string();
            if self.profiles.iter().any(|p| p.name == carried) {
                warn!("Carrying the 1.x '{carried}' client selection over to profiles");
                self.default_profile = Some(carried);
            }
        }

        // A default_profile naming something that no longer exists would silently
        // fall through to auto-detection; say so rather than leaving the user to
        // wonder why their choice was ignored.
        if let Some(name) = &self.default_profile {
            if !self.profiles.iter().any(|p| &p.name == name) {
                warn!("Configured default profile '{name}' does not exist; using auto-detection");
                self.default_profile = None;
            }
        }

        self
    }

    /// Gives every profile a valid, unique id. A missing (or malformed) id is assigned once; of
    /// several profiles sharing an id the first keeps it and later ones get fresh ids. Never
    /// fails the load. Marks `ids_assigned` when anything changed, so the loader saves it.
    fn repair_profile_ids(&mut self) {
        let mut seen = std::collections::HashSet::new();
        for profile in &mut self.profiles {
            // An empty id is a migration, not a fault: older files carry none.
            if !profile.id.is_empty() {
                if !is_valid_profile_id(&profile.id) {
                    warn!(
                        "Profile '{}' has an invalid id '{}'; assigning a new one",
                        profile.name, profile.id
                    );
                } else if seen.insert(profile.id.clone()) {
                    continue;
                } else {
                    warn!(
                        "Profile '{}' repeats id '{}'; assigning a new one",
                        profile.name, profile.id
                    );
                }
            }
            let mut fresh = new_profile_id();
            while seen.contains(&fresh) {
                fresh = new_profile_id();
            }
            seen.insert(fresh.clone());
            profile.id = fresh;
            self.ids_assigned.set(true);
        }
    }

    /// Writes the config once if loading assigned ids, so a migrated id is the id from then on
    /// rather than a new one every launch. Goes through the same atomic save as any other write.
    fn persist_assigned_ids(&self, path: &Path) {
        if self.ids_assigned.replace(false) {
            self.save_to(path);
        }
    }

    /// The profile with this id, if any.
    pub fn profile_by_id(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    /// The position of the profile that defines `driver`'s chat agent: the first that records
    /// it as its agent ([`Profile::driver`]) while it still runs it, else the first whose
    /// command runs it. Preferences → Agents edits this profile.
    pub fn agent_profile_index(&self, driver: agent_core::adapter::Driver) -> Option<usize> {
        let runs = |p: &Profile| profile_driver(p) == Some(driver);
        self.profiles
            .iter()
            .position(|p| p.driver == Some(driver) && runs(p))
            .or_else(|| self.profiles.iter().position(runs))
    }

    /// The position of the profile Preferences → Agents edits for `driver`, created when there
    /// is none. The profile found records `driver` as its agent, so an edit that gives its
    /// command another file name (typing, a wrapper) keeps it `driver`'s rather than detaching
    /// it and making the next edit create a duplicate.
    pub fn agent_profile_slot(&mut self, driver: agent_core::adapter::Driver) -> usize {
        let index = match self.agent_profile_index(driver) {
            Some(i) => i,
            None => {
                self.profiles.push(new_agent_profile(driver));
                self.profiles.len() - 1
            }
        };
        self.profiles[index].driver = Some(driver);
        index
    }

    /// The profile that defines `driver`'s chat agent, if one does.
    pub fn agent_profile(&self, driver: agent_core::adapter::Driver) -> Option<&Profile> {
        self.agent_profile_index(driver)
            .and_then(|i| self.profiles.get(i))
    }

    /// The profile a chat thread on `driver` runs with: the one it was opened with (`profile_id`)
    /// while that profile still runs `driver`, else `driver`'s own ([`Self::agent_profile`]). A
    /// thread switched to another agent, or whose profile was deleted or now runs another CLI,
    /// falls back rather than starting the wrong agent.
    pub fn thread_profile(
        &self,
        profile_id: Option<&str>,
        driver: agent_core::adapter::Driver,
    ) -> Option<&Profile> {
        profile_id
            .and_then(|id| self.profile_by_id(id))
            .filter(|p| profile_driver(p) == Some(driver))
            .or_else(|| self.agent_profile(driver))
    }

    /// Whether this is a first start: no settings file existed when these were loaded.
    pub fn is_first_run(&self) -> bool {
        self.first_run.get()
    }

    /// The profile the user explicitly chose, if any.
    pub fn selected_profile(&self) -> Option<&Profile> {
        let name = self.default_profile.as_ref()?;
        self.profiles.iter().find(|p| &p.name == name)
    }

    /// Loads a config from an explicit path, falling back to defaults. A missing
    /// file is expected (first run); a present-but-invalid file is logged so the
    /// user knows their settings were ignored rather than silently discarded.
    ///
    /// Falling back is only safe because the unusable file is copied aside
    /// first: the next save of any setting — even a zoom — writes the defaults
    /// over it, and a typo in a hand edit used to cost every profile and
    /// indicator that way.
    fn load_from(path: &Path) -> Self {
        let stamp = disk_stamp(path);
        let problem = match Self::read_config(path) {
            Ok(None) => return Self::default(),
            Ok(Some(config)) => {
                config.disk_stamp.set(stamp);
                config.persist_assigned_ids(path);
                return config;
            }
            Err(problem) => problem,
        };

        let config = Self::default();
        match keep_copy(path, "invalid") {
            Some(copy) => {
                report_load_problem(format!(
                    "Settings file {} {problem}, so defaults are in use. Your file was kept \
                     as {}.",
                    path.display(),
                    copy.display()
                ));
                // The copy is taken, so the first save may replace the file.
                config.disk_stamp.set(stamp);
            }
            None => {
                report_load_problem(format!(
                    "Settings file {} {problem}, and a copy of it could not be kept, so \
                     defaults are in use and nothing will be saved over it. Fix the file \
                     and relaunch.",
                    path.display()
                ));
                config.save_blocked.set(true);
            }
        }
        config
    }

    /// Reads and parses a config file. `Ok(None)` when it does not exist;
    /// `Err` is a phrase completing "Settings file X …".
    fn read_config(path: &Path) -> Result<Option<Self>, String> {
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("could not be read ({e})")),
        };
        serde_json::from_str::<Self>(&content)
            .map(|config| Some(config.normalize()))
            .map_err(|e| format!("is not valid ({e})"))
    }

    /// Whether the settings file was changed by something other than this
    /// process since it last read or wrote it — a hand edit, typically.
    ///
    /// Profiles and indicators can only be set by editing the file, so this is
    /// how the app keeps up with them: the caller swaps in the settings from an
    /// `Updated` result. An `Invalid` result suspends saving until the file is
    /// fixed, so an edit that is half-done does not get written over.
    pub fn check_disk(&self) -> DiskChange {
        self.check_disk_at(&Self::config_path())
    }

    fn check_disk_at(&self, path: &Path) -> DiskChange {
        let current = disk_stamp(path);
        if current.is_none() {
            // Deleted: nothing is left to protect, so saving may resume and
            // will recreate the file from the settings in memory.
            self.save_blocked.set(false);
            self.disk_stamp.set(None);
            return DiskChange::Unchanged;
        }
        if current == self.disk_stamp.get() {
            return DiskChange::Unchanged;
        }
        match Self::read_config(path) {
            Ok(Some(config)) => {
                config.disk_stamp.set(current);
                config.persist_assigned_ids(path);
                DiskChange::Updated(Box::new(config))
            }
            Ok(None) => DiskChange::Unchanged,
            Err(problem) => {
                // Remembered, so the same broken version is reported once.
                self.disk_stamp.set(current);
                self.save_blocked.set(true);
                DiskChange::Invalid(format!(
                    "Settings file {} {problem}. Changes made in the app will not be saved \
                     until it is fixed.",
                    path.display()
                ))
            }
        }
    }

    pub fn save(&self) {
        self.save_to(&Self::config_path());
    }

    /// Like [`Self::save`], and says whether the settings reached the disk.
    pub fn save_checked(&self) -> bool {
        self.save_to(&Self::config_path())
    }

    /// Serializes and writes the config to an explicit path, logging on failure. Returns whether
    /// the file was written.
    ///
    /// The write goes to a sibling temporary file which is then renamed over the
    /// target. Rename within a directory is atomic on Linux, so an interrupted
    /// save leaves either the previous config or the new one — never the
    /// half-written file a plain `fs::write` would produce.
    fn save_to(&self, path: &Path) -> bool {
        if self.save_blocked.get() {
            warn!(
                "Not saving settings to {}: the file there is unusable and has no other copy",
                path.display()
            );
            return false;
        }
        let content = match serde_json::to_string_pretty(self) {
            Ok(content) => content,
            Err(e) => {
                warn!("Failed to serialize config: {}", e);
                return false;
            }
        };

        // The app reloads hand edits as they happen (check_disk), so reaching
        // here with a changed file means one landed in the moment before this
        // save. Replacing it would discard it; keep it beside the new one.
        let on_disk = disk_stamp(path);
        if on_disk.is_some() && on_disk != self.disk_stamp.get() {
            if let Some(copy) = keep_copy(path, "external") {
                warn!(
                    "{} was changed outside Agent Terminal; that version was kept as {}",
                    path.display(),
                    copy.display()
                );
            }
        }

        if let Err(e) = Self::replace_atomically(path, content.as_bytes()) {
            warn!(
                "Failed to replace config at {}: {}; settings not saved",
                path.display(),
                e
            );
            return false;
        }
        self.disk_stamp.set(disk_stamp(path));
        true
    }

    /// Replaces `path` through the one shared atomic writer: a uniquely named `0600` sibling,
    /// `sync_all`, then a rename, so a crash leaves the old file or the new one, never a
    /// half-written one. `0600` is deliberate for the user's config (it can name env files and
    /// commands).
    fn replace_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        agent_kit::fsutil::write_private_atomic(path, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let cfg = TerminalConfig {
            startup_script: "/tmp/startup.sh".to_string(),
            scrollback_lines: 500,
            font_scale: 1.5,
            cli_client: CliClient::Claude,
            starting_directory: "/tmp/project".to_string(),
            theme: ThemeChoice::Dracula,
            profiles: default_profiles(),
            default_profile: Some("Claude".to_string()),
            ..Default::default()
        };
        cfg.save_to(&path);

        let loaded = TerminalConfig::load_from(&path);
        assert_eq!(loaded.startup_script, cfg.startup_script);
        assert_eq!(loaded.scrollback_lines, cfg.scrollback_lines);
        assert_eq!(loaded.font_scale, cfg.font_scale);
        assert_eq!(loaded.cli_client, cfg.cli_client);
        assert_eq!(loaded.starting_directory, cfg.starting_directory);
        assert_eq!(loaded.theme, cfg.theme);
    }

    #[test]
    fn permission_sources_default_empty_and_round_trip() {
        assert!(TerminalConfig::default().permission_sources.is_empty());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        // A file written before the field existed reads as no sources.
        std::fs::write(&path, br#"{"theme":"dracula"}"#).unwrap();
        assert!(TerminalConfig::load_from(&path)
            .permission_sources
            .is_empty());

        let cfg = TerminalConfig {
            permission_sources: vec!["/srv/a/permissions.toml".to_owned(), "~/b.toml".to_owned()],
            ..Default::default()
        };
        cfg.save_to(&path);
        let loaded = TerminalConfig::load_from(&path);
        assert_eq!(loaded.permission_sources, cfg.permission_sources);
        let paths = loaded.permission_source_paths();
        assert_eq!(paths[0], PathBuf::from("/srv/a/permissions.toml"));
        assert!(
            paths[1].is_absolute() && paths[1].ends_with("b.toml"),
            "{paths:?}"
        );
    }

    #[test]
    fn permission_sources_are_validated_inline() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("permissions.toml");
        std::fs::write(&file, "").unwrap();
        let f = file.to_string_lossy().into_owned();
        assert_eq!(parse_permission_sources("").unwrap(), Vec::<String>::new());
        assert_eq!(parse_permission_sources(&f).unwrap(), vec![f.clone()]);
        assert_eq!(
            parse_permission_sources(&format!(" {f} : {f} ")).unwrap(),
            vec![f.clone()],
            "blanks and duplicates fold"
        );
        assert!(parse_permission_sources("relative/permissions.toml")
            .unwrap_err()
            .contains("absolute"));
        let missing = dir.path().join("nope.toml");
        assert!(
            parse_permission_sources(&format!("{f}:{}", missing.display()))
                .unwrap_err()
                .contains("nope.toml")
        );
        assert!(parse_permission_sources(&dir.path().to_string_lossy())
            .unwrap_err()
            .contains("existing file"));
    }

    /// No project's config path is baked into the binary.
    #[test]
    fn no_agent_config_path_is_hard_coded() {
        fn scan(dir: &Path, hits: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if path
                        .file_name()
                        .is_some_and(|n| n == "tests" || n == "fixtures")
                    {
                        continue;
                    }
                    scan(&path, hits);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    // Everything from the test module on is test code.
                    let code = text.split("#[cfg(test)]\nmod ").next().unwrap_or("");
                    if code.contains(concat!("agent", "-config")) {
                        hits.push(path.display().to_string());
                    }
                }
            }
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut hits = Vec::new();
        scan(&root.join("src"), &mut hits);
        scan(&root.join("crates"), &mut hits);
        assert!(hits.is_empty(), "hard-coded agent-config path in {hits:?}");
    }

    #[test]
    fn missing_file_yields_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        assert_eq!(
            TerminalConfig::load_from(&path).cli_client,
            CliClient::default()
        );
    }

    #[test]
    fn all_lists_every_client_in_dropdown_order() {
        // Same hazard as ThemeChoice::ALL: the client dropdown is built from this
        // array and read back by index, so a variant missing from it mis-maps the
        // picker. Exhaustive on purpose — a new variant must not compile until ALL
        // is updated.
        fn expected_index(client: CliClient) -> usize {
            match client {
                CliClient::Auto => 0,
                CliClient::Gemini => 1,
                CliClient::Agy => 2,
                CliClient::Claude => 3,
            }
        }

        assert_eq!(CliClient::ALL.len(), 4);
        for client in CliClient::ALL {
            let index = expected_index(client);
            assert_eq!(CliClient::ALL[index], client);
            assert_eq!(
                CliClient::ALL.iter().position(|c| *c == client),
                Some(index),
                "{client} is not at its expected position in ALL"
            );
        }
    }

    #[test]
    fn all_lists_every_theme_in_dropdown_order() {
        // ALL is hand-maintained and the settings dropdown maps it by index in
        // BOTH directions: position() to preselect, ALL.get(index) to read back.
        // A variant missing from ALL therefore mis-maps the picker silently — you
        // choose Nord and get Gruvbox, with no error anywhere.
        //
        // The match is exhaustive deliberately: adding a variant fails to compile
        // here until ALL is updated to match.
        fn expected_index(theme: ThemeChoice) -> usize {
            match theme {
                ThemeChoice::System => 0,
                ThemeChoice::AgentTerminal => 1,
                ThemeChoice::Dracula => 2,
                ThemeChoice::Nord => 3,
                ThemeChoice::GruvboxDark => 4,
                ThemeChoice::SolarizedDark => 5,
                ThemeChoice::OneDark => 6,
                ThemeChoice::Monokai => 7,
            }
        }

        assert_eq!(ThemeChoice::ALL.len(), 8);
        for theme in ThemeChoice::ALL {
            let index = expected_index(theme);
            assert_eq!(ThemeChoice::ALL[index], theme);
            assert_eq!(
                ThemeChoice::ALL.iter().position(|t| *t == theme),
                Some(index),
                "{theme} is not at its expected position in ALL"
            );
        }
    }

    /// The default theme's old name (after the CLI the app first drove) still loads, and is
    /// written back under the app's own.
    #[test]
    fn the_old_theme_name_still_loads() {
        let old: ThemeChoice = serde_json::from_str("\"antigravity\"").expect("the old name");
        assert_eq!(old, ThemeChoice::AgentTerminal);
        assert_eq!(
            serde_json::to_string(&old).expect("serialises"),
            "\"agent-terminal\""
        );
    }

    #[test]
    fn theme_display_names_are_unique_and_non_empty() {
        // The dropdown is built from these strings; duplicates or blanks would
        // leave the user unable to tell two entries apart.
        let mut names: Vec<String> = ThemeChoice::ALL.iter().map(ToString::to_string).collect();
        assert!(names.iter().all(|n| !n.trim().is_empty()));
        names.sort();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate theme display names");
    }

    #[test]
    fn every_theme_round_trips_through_json() {
        for theme in ThemeChoice::ALL {
            let encoded = serde_json::to_string(&theme).unwrap();
            let decoded: ThemeChoice = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, theme, "{theme} did not survive a JSON round trip");
        }
    }

    #[test]
    fn overwrites_in_place_without_leaving_a_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");

        let mut cfg = TerminalConfig {
            scrollback_lines: 500,
            ..Default::default()
        };
        cfg.save_to(&path);
        cfg.scrollback_lines = 900;
        cfg.save_to(&path);

        assert_eq!(TerminalConfig::load_from(&path).scrollback_lines, 900);

        // The save goes via config.json.tmp and renames; a leftover temp file
        // would mean the rename never happened.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn partial_file_keeps_defaults_for_missing_fields() {
        // Container-level #[serde(default)] must fall back to TerminalConfig's own
        // defaults, not to each field type's default — otherwise a config written
        // by an older build silently loses its scrollback to 0.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"theme":"dracula"}"#)
            .unwrap();

        let loaded = TerminalConfig::load_from(&path);
        assert_eq!(loaded.theme, ThemeChoice::Dracula);
        assert_eq!(
            loaded.scrollback_lines,
            TerminalConfig::default().scrollback_lines
        );
        assert_eq!(loaded.font_scale, TerminalConfig::default().font_scale);
        // An older config gets idle suspension at its default, not 0 (never).
        assert_eq!(loaded.idle_suspend_minutes, 15);
    }

    #[test]
    fn config_home_prefers_xdg_over_home() {
        // XDG_CONFIG_HOME was ignored entirely before this, so anyone who had
        // relocated their config directory silently got a second one.
        assert_eq!(
            TerminalConfig::config_home_from(
                Some("/xdg/config".to_string()),
                Some("/home/someone".to_string())
            ),
            PathBuf::from("/xdg/config")
        );
    }

    #[test]
    fn config_home_falls_back_to_home_when_xdg_is_unset_or_blank() {
        let expected = PathBuf::from("/home/someone/.config");
        assert_eq!(
            TerminalConfig::config_home_from(None, Some("/home/someone".to_string())),
            expected
        );
        // The specification treats an empty value as unset.
        assert_eq!(
            TerminalConfig::config_home_from(
                Some("   ".to_string()),
                Some("/home/someone".to_string())
            ),
            expected
        );
    }

    #[test]
    fn config_home_survives_a_missing_home() {
        assert_eq!(
            TerminalConfig::config_home_from(None, None),
            PathBuf::from("/.config")
        );
    }

    #[test]
    fn session_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let state = SessionState {
            tabs: vec![
                SessionTab {
                    profile: Some("Claude".to_string()),
                    dir: "/tmp/a".to_string(),
                },
                SessionTab {
                    profile: None,
                    dir: "/tmp/b".to_string(),
                },
            ],
            selected: 1,
        };
        state.save_to(&path);

        let loaded = SessionState::load_from(&path);
        assert_eq!(loaded.tabs, state.tabs);
        assert_eq!(loaded.selected, 1);
    }

    #[test]
    fn a_corrupt_session_file_starts_fresh_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(b"{ this is not json")
            .unwrap();
        assert!(SessionState::load_from(&path).tabs.is_empty());
    }

    #[test]
    fn a_session_file_cannot_spawn_unbounded_tabs() {
        // A hand-edited or corrupt file must not be able to open hundreds of PTYs
        // at launch.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let state = SessionState {
            tabs: (0..500)
                .map(|i| SessionTab {
                    profile: None,
                    dir: format!("/tmp/{i}"),
                })
                .collect(),
            selected: 400,
        };
        state.save_to(&path);

        let loaded = SessionState::load_from(&path);
        assert_eq!(loaded.tabs.len(), SessionState::MAX_TABS);
        // The recorded selection pointed past the truncation, so it must be
        // brought back in range rather than left dangling.
        assert!(loaded.selected < loaded.tabs.len());
    }

    #[test]
    fn a_missing_session_file_is_an_empty_session() {
        let dir = tempfile::tempdir().unwrap();
        assert!(SessionState::load_from(&dir.path().join("none.json"))
            .tabs
            .is_empty());
    }

    #[test]
    fn a_v1_config_keeps_its_pinned_client_as_a_profile() {
        // Dropping cli_client on the floor would silently move a user who had
        // pinned Claude back to auto-detection.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"cli_client":"claude","scrollback_lines":700}"#)
            .unwrap();

        let loaded = TerminalConfig::load_from(&path);
        assert_eq!(loaded.default_profile.as_deref(), Some("Claude"));
        assert_eq!(loaded.scrollback_lines, 700);
        // And the default profile list is materialised for it.
        assert!(loaded.profiles.iter().any(|p| p.command == "claude"));

        // The standalone Gemini CLI is no longer a default profile, so a 1.x Gemini pin falls
        // back to auto-detection instead of naming a profile that does not exist.
        std::fs::write(&path, br#"{"cli_client":"gemini"}"#).unwrap();
        let loaded = TerminalConfig::load_from(&path);
        assert_eq!(loaded.default_profile, None);
        assert!(!loaded.profiles.iter().any(|p| p.command == "gemini"));
    }

    #[test]
    fn a_v1_auto_config_stays_on_auto_detection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"cli_client":"auto"}"#)
            .unwrap();
        assert_eq!(TerminalConfig::load_from(&path).default_profile, None);
    }

    #[test]
    fn a_default_profile_naming_nothing_falls_back_to_auto() {
        // Deleting a profile that default_profile pointed at must not leave the
        // config referring to something that no longer exists.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(
                br#"{"default_profile":"Deleted","profiles":[{"name":"Claude","command":"claude"}]}"#,
            )
            .unwrap();
        assert_eq!(TerminalConfig::load_from(&path).default_profile, None);
    }

    #[test]
    fn custom_profiles_round_trip() {
        // The whole point of profiles: a new CLI is a config edit, not a rebuild.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let cfg = TerminalConfig {
            profiles: vec![Profile {
                id: "p-0000000000000001".to_string(),
                name: "Codex".to_string(),
                command: "codex".to_string(),
                args: vec!["--full-auto".to_string()],
                dir: Some("/tmp/project".to_string()),
                env_file: Some("/tmp/env.sh".to_string()),
                resume_args: Some(vec!["resume".to_string(), "{id}".to_string()]),
                session_format: SessionFormat::AgyHistory,
                prompt_args: Some(vec!["{prompt}".to_string()]),
                limit_markers: Some(vec!["quota".to_string()]),
                ..Profile::default()
            }],
            default_profile: Some("Codex".to_string()),
            ..Default::default()
        };
        cfg.save_to(&path);

        let loaded = TerminalConfig::load_from(&path);
        assert_eq!(loaded.profiles, cfg.profiles);
        assert_eq!(loaded.default_profile.as_deref(), Some("Codex"));
        assert_eq!(
            loaded.selected_profile().map(|p| p.command.as_str()),
            Some("codex")
        );
    }

    #[test]
    fn an_empty_profile_list_is_repopulated() {
        // An empty list would leave the app with nothing to launch and no way to
        // recover through the UI.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"profiles":[]}"#)
            .unwrap();
        assert!(!TerminalConfig::load_from(&path).profiles.is_empty());
    }

    #[test]
    fn profile_argv_puts_the_command_first() {
        let profile = Profile {
            name: "Claude".to_string(),
            command: "claude".to_string(),
            args: vec!["--model".to_string(), "opus".to_string()],
            ..Profile::default()
        };
        assert_eq!(profile.argv(), vec!["claude", "--model", "opus"]);
    }

    #[test]
    fn a_resume_era_profile_gains_only_the_title_pointer() {
        // Saved by the release that added resume: store present, title absent.
        // A profile pointed at its own store is not given Claude's title format.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(
                br#"{"profiles":[
                    {"name":"Claude","command":"claude","resume_args":["--resume","{id}"],
                     "session_store":"~/.claude/projects"},
                    {"name":"Work","command":"claude","resume_args":["--resume","{id}"],
                     "session_store":"/srv/elsewhere"}]}"#,
            )
            .unwrap();

        let loaded = TerminalConfig::load_from(&path);
        assert_eq!(
            loaded.profiles[0].session_title.as_deref(),
            Some("/aiTitle")
        );
        assert_eq!(loaded.profiles[1].session_title, None);
    }

    #[test]
    fn a_pre_resume_claude_profile_gains_resume_settings() {
        // Every config saved before resume existed has a Claude profile with
        // neither field; without the backfill it could never resume anything.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"profiles":[{"name":"Claude","command":"claude"},{"name":"Gemini","command":"gemini"}]}"#)
            .unwrap();

        let loaded = TerminalConfig::load_from(&path);
        assert!(loaded.profiles[0].can_resume());
        assert_eq!(
            loaded.profiles[0].session_store.as_deref(),
            Some("~/.claude/projects")
        );
        assert_eq!(
            loaded.profiles[0].session_title.as_deref(),
            Some("/aiTitle")
        );
        // A CLI with no known convention is left alone rather than guessed at.
        assert!(!loaded.profiles[1].can_resume());
        assert!(!loaded.profiles[1].can_take_prompt());
    }

    #[test]
    fn quota_notifications_are_on_for_a_config_that_predates_them() {
        // Unlike the bell, which a user may find noisy, a stalled session is
        // worth interrupting for, so an older config opts in.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"notify_on_bell":false}"#)
            .unwrap();
        let loaded = TerminalConfig::load_from(&path);
        assert!(loaded.notify_on_quota);
        assert!(!loaded.notify_on_bell);
    }

    #[test]
    fn the_model_tag_file_defaults_to_the_config_dir_and_round_trips_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        // A config from before the field loads, with the default.
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"notify_on_bell":false}"#)
            .unwrap();
        let loaded = TerminalConfig::load_from(&path);
        assert_eq!(loaded.model_tags_path, None);
        let cfg_dir = Path::new("/cfg/agent-terminal");
        assert_eq!(
            TerminalConfig::model_tags_file_in(None, cfg_dir),
            cfg_dir.join("model-tags.json")
        );
        assert_eq!(
            TerminalConfig::model_tags_file_in(Some("  "), cfg_dir),
            cfg_dir.join("model-tags.json"),
            "blank is unset"
        );
        assert_eq!(
            TerminalConfig::model_tags_file_in(Some("/etc/tags.json"), cfg_dir),
            PathBuf::from("/etc/tags.json")
        );
        let home = std::env::var("HOME").unwrap_or_default();
        if !home.is_empty() {
            assert_eq!(
                TerminalConfig::model_tags_file_in(Some("~/t.json"), cfg_dir),
                Path::new(&home).join("t.json")
            );
        }
        // Unset is not written; a set path survives a save and load.
        let unset = serde_json::to_value(&loaded).unwrap();
        assert!(unset.get("model_tags_path").is_none());
        let mut set = loaded.clone();
        set.model_tags_path = Some("~/tags.json".into());
        let text = serde_json::to_string(&set).unwrap();
        std::fs::write(&path, text).unwrap();
        assert_eq!(
            TerminalConfig::load_from(&path).model_tags_path.as_deref(),
            Some("~/tags.json")
        );
    }

    #[test]
    fn the_startup_update_check_is_on_unless_turned_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"notify_on_bell":false}"#)
            .unwrap();
        assert!(TerminalConfig::load_from(&path).check_updates_on_startup);
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"check_updates_on_startup":false}"#)
            .unwrap();
        assert!(!TerminalConfig::load_from(&path).check_updates_on_startup);
    }

    #[test]
    fn session_restore_is_off_unless_chosen() {
        // A restored tab starts a fresh conversation, so restoring by default
        // only opens extra blank sessions. An explicit choice still stands.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"notify_on_bell":false}"#)
            .unwrap();
        assert!(!TerminalConfig::load_from(&path).restore_session);

        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"restore_session":true}"#)
            .unwrap();
        assert!(TerminalConfig::load_from(&path).restore_session);
    }

    #[test]
    fn checkpoints_are_on_unless_turned_off() {
        // A config written before checkpoints existed gets them.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"notify_on_bell":false}"#)
            .unwrap();
        assert!(TerminalConfig::load_from(&path).checkpoints);

        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"checkpoints":false}"#)
            .unwrap();
        assert!(!TerminalConfig::load_from(&path).checkpoints);
    }

    #[test]
    fn turn_command_loads_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"turn_command":["litellm-sync","--session","{id}"]}"#)
            .unwrap();
        let config = TerminalConfig::load_from(&path);
        assert_eq!(
            config.turn_command,
            Some(vec![
                "litellm-sync".to_string(),
                "--session".to_string(),
                "{id}".to_string()
            ])
        );
    }

    #[test]
    fn a_config_from_before_the_diff_tool_loads_without_one_and_one_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        // A 2.x / early-3.0 file: no `diff_tool` key at all.
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"checkpoints":true,"theme":"dracula","default_agent":"claude"}"#)
            .unwrap();
        let old = TerminalConfig::load_from(&path);
        assert_eq!(old.diff_tool, None);
        assert!(old.checkpoints, "the rest of the file still loads");

        // Saving a config without a tool writes no key (older builds read it unchanged).
        old.save_to(&path);
        assert!(!std::fs::read_to_string(&path)
            .unwrap()
            .contains("diff_tool"));

        let mut config = TerminalConfig::load_from(&path);
        config.diff_tool = Some(DiffTool {
            name: "Meld".into(),
            argv: vec!["meld".into(), "{old}".into(), "{new}".into()],
        });
        config.save_to(&path);
        let back = TerminalConfig::load_from(&path);
        assert_eq!(back.diff_tool, config.diff_tool);
    }

    #[test]
    fn indicators_load_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"indicators":[{"label":"LiteLLM","icon_ok":"at-network-server-symbolic","source":{"type":"command","argv":["litellm-sync","--indicator"],"timeout_secs":3}}]}"#)
            .unwrap();
        let config = TerminalConfig::load_from(&path);
        assert_eq!(config.indicators.len(), 1);
        assert_eq!(config.indicators[0].label, "LiteLLM");
        assert_eq!(config.indicators[0].icon_ok, "at-network-server-symbolic");
    }

    #[test]
    fn indicators_accept_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"indicators":[{"name":"LiteLLM","icon":"at-network-server-symbolic","source":{"type":"command","argv":["litellm-sync","--indicator"],"timeout_secs":3}}]}"#)
            .unwrap();
        let config = TerminalConfig::load_from(&path);
        assert_eq!(config.indicators.len(), 1);
        assert_eq!(config.indicators[0].label, "LiteLLM");
        assert_eq!(config.indicators[0].icon_ok, "at-network-server-symbolic");
    }

    #[test]
    fn a_pre_handoff_agy_profile_gains_resume_and_handoff_settings() {
        // Saved before Agy could resume: nothing but name and command.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"profiles":[{"name":"Agy","command":"agy"}]}"#)
            .unwrap();

        let agy = &TerminalConfig::load_from(&path).profiles[0];
        assert_eq!(
            agy.resume_args.as_deref(),
            Some(&["--conversation".to_string(), "{id}".to_string()][..])
        );
        assert_eq!(agy.session_format, SessionFormat::AgyHistory);
        assert_eq!(agy.session_title, None);
        assert!(agy.can_take_prompt());
        // Not known to accept a fresh ID, so never asked to.
        assert!(!agy.can_pin_session_id());
    }

    #[test]
    fn a_pre_resume_codex_profile_gains_resume_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"profiles":[{"name":"Codex","command":"codex"}]}"#)
            .unwrap();

        let codex = &TerminalConfig::load_from(&path).profiles[0];
        assert_eq!(
            codex.resume_args.as_deref(),
            Some(&["app-server".to_string()][..])
        );
        assert_eq!(codex.session_store.as_deref(), Some("~/.codex/sessions"));
        assert_eq!(codex.session_format, SessionFormat::Jsonl);
        assert_eq!(codex.session_title, None);
        assert!(codex.can_take_prompt());
        assert!(!codex.can_pin_session_id());
    }

    #[test]
    fn a_resume_era_claude_profile_gains_handoff_settings_but_keeps_opt_outs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(
                br#"{"profiles":[
                    {"name":"Claude","command":"claude","resume_args":["--resume","{id}"],
                     "session_store":"~/.claude/projects"},
                    {"name":"Quiet","command":"claude","prompt_args":[],"session_id_args":[]}]}"#,
            )
            .unwrap();

        let loaded = TerminalConfig::load_from(&path);
        assert!(loaded.profiles[0].can_pin_session_id());
        assert!(loaded.profiles[0].can_take_prompt());
        assert_eq!(loaded.profiles[0].session_format, SessionFormat::Jsonl);
        assert!(!loaded.profiles[1].can_pin_session_id());
        assert!(!loaded.profiles[1].can_take_prompt());
    }

    #[test]
    fn an_explicit_resume_opt_out_is_respected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"profiles":[{"name":"Claude","command":"claude","resume_args":[]}]}"#)
            .unwrap();

        let loaded = TerminalConfig::load_from(&path);
        assert!(!loaded.profiles[0].can_resume());
        assert!(loaded.profiles[0].session_store.is_none());
    }

    #[test]
    fn adopts_a_pre_rename_config_without_destroying_it() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("antigravity-terminal.json");
        let current = dir.path().join("agent-terminal.json");

        let original = TerminalConfig {
            scrollback_lines: 4242,
            theme: ThemeChoice::Nord,
            ..Default::default()
        };
        original.save_to(&legacy);

        let migrated = TerminalConfig::load_or_migrate(&current, &legacy);
        assert_eq!(migrated.scrollback_lines, 4242);
        assert_eq!(migrated.theme, ThemeChoice::Nord);

        // The settings must now exist under the new name...
        assert!(current.exists(), "migration did not write the new config");
        assert_eq!(TerminalConfig::load_from(&current).scrollback_lines, 4242);

        // ...and the old file must survive, because rolling back to v1.x means
        // reinstalling a differently-named package that reads only the old path.
        assert!(legacy.exists(), "migration must not delete the old config");
    }

    #[test]
    fn migration_does_not_clobber_an_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("antigravity-terminal.json");
        let current = dir.path().join("agent-terminal.json");

        TerminalConfig {
            scrollback_lines: 100,
            ..Default::default()
        }
        .save_to(&legacy);
        TerminalConfig {
            scrollback_lines: 900,
            ..Default::default()
        }
        .save_to(&current);

        // Already migrated once: the new file wins from then on.
        assert_eq!(
            TerminalConfig::load_or_migrate(&current, &legacy).scrollback_lines,
            900
        );
    }

    #[test]
    fn missing_both_configs_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = TerminalConfig::load_or_migrate(
            &dir.path().join("new.json"),
            &dir.path().join("old.json"),
        );
        assert_eq!(
            loaded.scrollback_lines,
            TerminalConfig::default().scrollback_lines
        );
        assert!(
            loaded.is_first_run(),
            "no settings anywhere is a first start"
        );
    }

    /// Only a start with no settings file anywhere offers the setup walkthrough: not one with
    /// a current file, nor one adopting a pre-rename file.
    #[test]
    fn a_first_start_is_one_with_no_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("old.json");
        let current = dir.path().join("new.json");

        TerminalConfig::default().save_to(&legacy);
        assert!(!TerminalConfig::load_or_migrate(&current, &legacy).is_first_run());
        assert!(current.exists(), "adopted");
        assert!(!TerminalConfig::load_or_migrate(&current, &legacy).is_first_run());
    }

    /// The system theme is stored by name and offered first.
    #[test]
    fn the_system_theme_round_trips_and_leads_the_list() {
        assert_eq!(
            serde_json::to_string(&ThemeChoice::System).expect("serialises"),
            "\"system\""
        );
        assert_eq!(ThemeChoice::ALL[0], ThemeChoice::System);
        assert_eq!(ThemeChoice::System.to_string(), "Use System Theme");
    }

    #[test]
    fn legacy_startup_script_still_parses() {
        // The Settings row is gone, but a config file written by v1.x still
        // carries the key and must not be rejected.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"startup_script":"/tmp/old.sh","scrollback_lines":200}"#)
            .unwrap();

        let loaded = TerminalConfig::load_from(&path);
        assert_eq!(loaded.startup_script, "/tmp/old.sh");
        assert_eq!(loaded.scrollback_lines, 200);
    }

    #[test]
    fn malformed_file_yields_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(b"{ not valid json ]")
            .unwrap();
        assert_eq!(
            TerminalConfig::load_from(&path).scrollback_lines,
            TerminalConfig::default().scrollback_lines
        );
    }

    fn kept_copies(dir: &Path, tag: &str) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name.contains(&format!(".{tag}-")))
            .collect()
    }

    #[test]
    fn duplicate_profile_names_are_made_unique_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            br#"{"profiles":[
                {"name":"Claude","command":"claude"},
                {"name":"Claude","command":"claude","args":["--model","opus"]},
                {"name":"Claude (2)","command":"agy"}]}"#,
        )
        .unwrap();
        let names: Vec<String> = TerminalConfig::load_from(&path)
            .profiles
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["Claude", "Claude (3)", "Claude (2)"]);
    }

    #[test]
    fn agy_profile_is_migrated_to_antigravity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            br#"{"default_profile":"Agy","profiles":[{"name":"Agy","command":"agy"}]}"#,
        )
        .unwrap();
        let cfg = TerminalConfig::load_from(&path);
        assert_eq!(cfg.profiles[0].name, "Antigravity");
        assert_eq!(cfg.default_profile.as_deref(), Some("Antigravity"));
    }

    #[test]
    fn load_problems_stay_with_the_parallel_test_that_created_them() {
        let ready = std::sync::Barrier::new(2);
        let checked = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                report_load_problem("synthetic invalid config".into());
                ready.wait();
                checked.wait();
                assert_eq!(
                    take_load_problem().as_deref(),
                    Some("synthetic invalid config")
                );
            });
            ready.wait();
            let problem = take_load_problem();
            checked.wait();
            assert!(
                problem.is_none(),
                "another test's error must not open a modal here"
            );
        });
    }

    #[test]
    fn an_invalid_file_is_kept_before_defaults_can_replace_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let original = br#"{ "scrollback_lines": 777, oops }"#;
        std::fs::File::create(&path)
            .unwrap()
            .write_all(original)
            .unwrap();

        let loaded = TerminalConfig::load_from(&path);
        assert!(
            take_load_problem().is_some(),
            "the fallback must be reported"
        );
        let copies = kept_copies(dir.path(), "invalid");
        assert_eq!(copies.len(), 1, "{copies:?}");
        assert_eq!(
            std::fs::read(dir.path().join(&copies[0])).unwrap(),
            original
        );

        // The first save replaces the file without taking a second copy.
        loaded.save_to(&path);
        assert!(kept_copies(dir.path(), "external").is_empty());
    }

    #[test]
    fn a_threads_profile_is_kept_only_while_it_runs_the_threads_agent() {
        use agent_core::adapter::Driver;
        let mut config = TerminalConfig::default();
        let named = |id: &str, name: &str, driver: Driver| Profile {
            id: id.into(),
            name: name.into(),
            ..new_agent_profile(driver)
        };
        config.profiles = vec![
            named("claude-default", "Claude", Driver::Claude),
            named("claude-work", "Claude (work)", Driver::Claude),
            named("codex-default", "Codex", Driver::Codex),
        ];
        let id = |p: Option<&Profile>| p.map(|p| p.id.clone());
        assert_eq!(
            id(config.thread_profile(Some("claude-work"), Driver::Claude)),
            Some("claude-work".into()),
            "the profile the thread was opened with"
        );
        assert_eq!(
            id(config.thread_profile(Some("claude-work"), Driver::Codex)),
            Some("codex-default".into()),
            "switched to another agent: that agent's own"
        );
        assert_eq!(
            id(config.thread_profile(Some("deleted"), Driver::Claude)),
            Some("claude-default".into()),
            "a deleted profile falls back"
        );
        assert_eq!(
            id(config.thread_profile(None, Driver::Claude)),
            Some("claude-default".into())
        );
        assert_eq!(config.thread_profile(None, Driver::Agy), None);
    }

    #[test]
    fn a_hand_edit_is_picked_up_and_a_broken_one_suspends_saving() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let config = TerminalConfig::default();
        config.save_to(&path);
        assert!(matches!(config.check_disk_at(&path), DiskChange::Unchanged));

        // A valid edit comes back as the new settings.
        std::fs::write(&path, br#"{"scrollback_lines": 4321}"#).unwrap();
        let DiskChange::Updated(edited) = config.check_disk_at(&path) else {
            panic!("a valid edit must be picked up");
        };
        assert_eq!(edited.scrollback_lines, 4321);
        assert!(matches!(edited.check_disk_at(&path), DiskChange::Unchanged));

        // A half-done edit is reported once, and nothing is written over it.
        let broken = br#"{"scrollback_lines": 43"#;
        std::fs::write(&path, broken).unwrap();
        assert!(matches!(
            edited.check_disk_at(&path),
            DiskChange::Invalid(_)
        ));
        assert!(matches!(edited.check_disk_at(&path), DiskChange::Unchanged));
        edited.save_to(&path);
        assert_eq!(std::fs::read(&path).unwrap(), broken);
    }

    #[test]
    fn a_broken_legacy_file_is_not_replaced_by_migrated_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("antigravity-terminal.json");
        let current = dir.path().join("agent-terminal.json");
        std::fs::write(&legacy, b"{ broken").unwrap();

        let loaded = TerminalConfig::load_or_migrate(&current, &legacy);
        loaded.save_to(&current);
        assert!(
            !current.exists(),
            "writing the new file would make the old one unadoptable"
        );
        assert_eq!(std::fs::read(&legacy).unwrap(), b"{ broken");
    }

    #[test]
    fn editing_an_agents_command_to_another_name_keeps_its_profile() {
        use agent_core::adapter::Driver;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut config = TerminalConfig::default();
        let first = config.agent_profile_slot(Driver::Claude);
        let id = config.profiles[first].id.clone();
        let count = config.profiles.len();
        // Typing in Settings → Agents → Claude → Command: a half-typed name, a wrapper with
        // another basename, then back to the plain name. Each debounced save edits the slot.
        for command in ["claud", "/opt/bin/claude-nightly", "claude"] {
            let slot = config.agent_profile_slot(Driver::Claude);
            assert_eq!(config.profiles[slot].id, id, "{command}: another profile");
            config.profiles[slot].command = command.into();
        }
        assert_eq!(config.profiles.len(), count, "a duplicate Claude profile");

        // A non-standard command survives a save and a load as Claude's.
        let slot = config.agent_profile_slot(Driver::Claude);
        config.profiles[slot].command = "/opt/bin/claude-nightly".into();
        config.save_to(&path);
        let loaded = TerminalConfig::load_from(&path);
        let claude = loaded.agent_profile(Driver::Claude).expect("claude");
        assert_eq!(claude.id, id);
        assert_eq!(claude.command, "/opt/bin/claude-nightly");
        assert_eq!(profile_driver(claude), Some(Driver::Claude));
        assert_eq!(
            loaded
                .thread_profile(Some(&id), Driver::Claude)
                .map(|p| &p.id),
            Some(&id)
        );

        // A hand-written wrapper profile that names no agent still runs as a terminal only.
        let wrapper = Profile {
            command: "/opt/bin/claude-wrapper".into(),
            ..Profile::default()
        };
        assert_eq!(profile_driver(&wrapper), None);
    }

    #[test]
    fn a_profiles_recorded_agent_round_trips_and_older_files_need_no_migration() {
        use agent_core::adapter::Driver;
        // A file from before the field: judged by the command's name, exactly as before.
        let old: Profile =
            serde_json::from_str(r#"{"id":"p-1","name":"Claude","command":"claude"}"#).unwrap();
        assert_eq!(old.driver, None);
        assert_eq!(profile_driver(&old), Some(Driver::Claude));
        // An unrecorded profile writes no field, so the file stays what an older build reads.
        assert!(!serde_json::to_string(&old).unwrap().contains("driver"));

        let recorded = Profile {
            command: "claude-nightly".into(),
            driver: Some(Driver::Claude),
            ..Profile::default()
        };
        let json = serde_json::to_string(&recorded).unwrap();
        assert!(json.contains(r#""driver":"claude""#), "{json}");
        let back: Profile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, recorded);
        assert_eq!(profile_driver(&back), Some(Driver::Claude));

        // A command that names another agent wins over the record: never the wrong adapter.
        let moved = Profile {
            command: "/usr/bin/codex".into(),
            ..recorded
        };
        assert_eq!(profile_driver(&moved), Some(Driver::Codex));
        assert_eq!(
            new_agent_profile(Driver::Agy).driver,
            Some(Driver::Agy),
            "a profile made for an agent records it"
        );
    }

    #[test]
    fn a_half_typed_setting_is_validated_but_saved_only_on_commit() {
        let saved = vec!["--verbose".to_string()];
        let parse = |text: &str| -> Result<Vec<String>, String> {
            if text.matches('\'').count() % 2 == 1 {
                Err("Unfinished quote".into())
            } else {
                Ok(text.split_whitespace().map(str::to_owned).collect())
            }
        };
        // Typing `--model opus` into Extra Arguments: every keystroke is a valid draft, and
        // none of them is written while the user types.
        for typed in ["-", "--m", "--model", "--model o", "--model opus"] {
            assert_eq!(
                field_change(parse(typed), &saved, false),
                FieldChange::Keep,
                "{typed:?} was saved while typing"
            );
        }
        // Enter (or focus leaving, or the dialog closing) writes the finished value.
        assert_eq!(
            field_change(parse("--model opus"), &saved, true),
            FieldChange::Persist(vec!["--model".to_string(), "opus".to_string()])
        );
        // An unfinished quote is reported while typing and on commit; the saved value stays.
        for commit in [false, true] {
            assert_eq!(
                field_change(parse("--model 'op"), &saved, commit),
                FieldChange::Invalid("Unfinished quote".into())
            );
        }
        // Committing the value already in use writes nothing.
        assert_eq!(
            field_change(parse("--verbose"), &saved, true),
            FieldChange::Keep
        );
    }

    #[test]
    fn an_older_builds_0644_backup_is_made_private_when_reused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let broken = br#"{ "env": {"TOKEN": "s3cret"}, oops }"#;
        std::fs::write(&path, broken).unwrap();
        // An identical copy an older build left world-readable.
        let old = dir.path().join("config.json.invalid-1");
        std::fs::write(&old, broken).unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o644)).unwrap();
        let kept = keep_copy(&path, "invalid").expect("kept");
        let mode = std::fs::metadata(&kept).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "kept copy is {:o}", mode & 0o777);
        let mode = std::fs::metadata(&old).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the old copy is {:o}", mode & 0o777);

        // A symlink with the copy's name is never reused (nor its target's mode changed).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, broken).unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::write(&elsewhere, broken).unwrap();
        std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o644)).unwrap();
        let link = dir.path().join("config.json.invalid-1");
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        let kept = keep_copy(&path, "invalid").expect("kept");
        assert_ne!(kept, link, "a symlink was reused");
        let meta = std::fs::symlink_metadata(&kept).unwrap();
        assert!(meta.file_type().is_file());
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        let mode = std::fs::metadata(&elsewhere).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644, "the link's target was touched");
    }

    #[test]
    fn a_hard_linked_backup_is_not_chmodded_and_a_fresh_copy_is_kept() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let broken = br#"{ "env": {"TOKEN": "s3cret"}, oops }"#;
        std::fs::write(&path, broken).unwrap();
        // An identical 0644 backup that is also another path (st_nlink 2).
        let other = dir.path().join("someone-elses-file");
        std::fs::write(&other, broken).unwrap();
        std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o644)).unwrap();
        let linked = dir.path().join("config.json.invalid-1");
        std::fs::hard_link(&other, &linked).unwrap();

        let kept = keep_copy(&path, "invalid").expect("kept");
        let mode = std::fs::metadata(&other).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644, "the shared inode was chmodded");
        assert_ne!(kept, linked, "a hard-linked backup was reused");
        let meta = std::fs::symlink_metadata(&kept).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read(&kept).unwrap(), broken);
    }

    #[test]
    fn a_kept_copy_of_a_private_config_is_private_too() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        // A 0600 config holding a token, broken by a hand edit, under a umask that would make a
        // plain write 0644: the copy kept before defaults replace it must stay 0600.
        std::fs::write(&path, br#"{ "env": {"TOKEN": "s3cret"}, oops }"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        TerminalConfig::load_from(&path);
        let _ = take_load_problem();
        let copies = kept_copies(dir.path(), "invalid");
        assert_eq!(copies.len(), 1, "{copies:?}");
        let mode = std::fs::metadata(dir.path().join(&copies[0]))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "kept copy is {:o}", mode & 0o777);

        // The copy kept of an edit made behind a save's back, too.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let config = TerminalConfig::default();
        config.save_to(&path);
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, br#"{"scrollback_lines": 1234}"#).unwrap();
        config.save_to(&path);
        let copies = kept_copies(dir.path(), "external");
        assert_eq!(copies.len(), 1, "{copies:?}");
        let mode = std::fs::metadata(dir.path().join(&copies[0]))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "kept copy is {:o}", mode & 0o777);
    }

    #[test]
    fn a_file_broken_across_launches_is_kept_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, b"{ broken").unwrap();
        TerminalConfig::load_from(&path);
        TerminalConfig::load_from(&path);
        assert_eq!(kept_copies(dir.path(), "invalid").len(), 1);
    }

    #[test]
    fn a_save_keeps_an_edit_made_behind_its_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let config = TerminalConfig::default();
        config.save_to(&path);

        // A hand edit while the app runs; the sleep guarantees a new mtime.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, br#"{"scrollback_lines": 1234}"#).unwrap();

        config.save_to(&path);
        let copies = kept_copies(dir.path(), "external");
        assert_eq!(copies.len(), 1, "{copies:?}");
        assert!(std::fs::read_to_string(dir.path().join(&copies[0]))
            .unwrap()
            .contains("1234"));

        // Nothing changed since that save, so the next one keeps nothing more.
        config.save_to(&path);
        assert_eq!(kept_copies(dir.path(), "external").len(), 1);
    }

    #[test]
    fn a_2x_config_loads_into_3_0_with_the_agent_fields_defaulted() {
        use agent_core::adapter::{Driver, Mode};
        // A config.json as 2.x wrote it: profiles with no 3.0 fields at all.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            br#"{
              "scrollback_lines": 5000,
              "default_profile": "Agy",
              "restore_session": true,
              "profiles": [
                {"name": "Claude", "command": "claude", "args": ["--verbose"]},
                {"name": "Agy", "command": "/opt/agy/bin/agy", "env_file": "~/.agy.env"},
                {"name": "Gemini", "command": "gemini"}
              ]
            }"#,
        )
        .unwrap();
        let config = TerminalConfig::load_from(&path);
        assert_eq!(config.scrollback_lines, 5000);
        assert_eq!(config.default_agent, None);
        assert!(!config.sidebar_collapsed);
        assert!(
            !config.skip_load_animation,
            "an older file plays the splash"
        );
        assert!(
            !config.reopen_last_thread,
            "an older file starts with no thread open"
        );
        assert!(
            config.list_outside_sessions,
            "an older file keeps listing the agents' own sessions"
        );
        let claude = config.agent_profile(Driver::Claude).expect("claude");
        assert_eq!(claude.args, ["--verbose"]);
        assert!(!claude.disabled);
        assert_eq!(claude.default_model, None);
        assert_eq!(claude.default_mode, None);
        let agy = config
            .agent_profile(Driver::Agy)
            .expect("agy by its binary name");
        assert_eq!(agy.env_file.as_deref(), Some("~/.agy.env"));
        assert_eq!(config.agent_profile_index(Driver::Agy), Some(1));
        assert_eq!(
            profile_driver(&config.profiles[2]),
            None,
            "gemini has no adapter"
        );

        // The 3.0 fields round-trip, and an untouched profile writes none of them.
        let mut config = config;
        config.default_agent = Some(Driver::Agy);
        config.sidebar_collapsed = true;
        config.skip_load_animation = true;
        config.reopen_last_thread = true;
        config.list_outside_sessions = false;
        config.profiles[0].default_model = Some("opus".into());
        config.profiles[0].default_mode = Some(Mode::AcceptEdits);
        config.profiles[0].default_effort = Some("high".into());
        config.profiles[1].disabled = true;
        config.profiles[0].available_for_delegation = true;
        config.save_to(&path);
        let text = std::fs::read_to_string(&path).unwrap();
        let gemini_json = text.split("\"Gemini\"").nth(1).unwrap_or_default();
        assert!(!gemini_json.contains("default_model") && !gemini_json.contains("disabled"));
        let back = TerminalConfig::load_from(&path);
        assert_eq!(back.default_agent, Some(Driver::Agy));
        assert!(back.sidebar_collapsed);
        assert!(back.skip_load_animation);
        assert!(back.reopen_last_thread);
        assert!(!back.list_outside_sessions, "turned off, it stays off");
        assert_eq!(back.profiles[0].default_model.as_deref(), Some("opus"));
        assert_eq!(back.profiles[0].default_mode, Some(Mode::AcceptEdits));
        assert_eq!(back.profiles[0].default_effort.as_deref(), Some("high"));
        assert!(back.profiles[1].disabled);
        assert!(back.profiles[0].available_for_delegation);
        assert!(!back.profiles[1].available_for_delegation);
    }

    #[test]
    fn quick_replies_default_on_and_round_trip_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        assert!(TerminalConfig::default().suggest_quick_replies);
        // An older file, with no such key, keeps the default.
        std::fs::write(&path, br#"{"scrollback_lines": 5000}"#).unwrap();
        let mut config = TerminalConfig::load_from(&path);
        assert!(config.suggest_quick_replies, "an older file suggests them");
        config.suggest_quick_replies = false;
        config.save_to(&path);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"suggest_quick_replies\": false"), "{text}");
        assert!(!TerminalConfig::load_from(&path).suggest_quick_replies);
    }

    #[test]
    fn the_default_agent_follows_the_choice_then_the_profile_then_what_is_installed() {
        use agent_core::adapter::Driver::{self, Agy, Claude};
        let all = |_| true;
        assert_eq!(
            choose_default_agent(Some(Agy), Some(Claude), all),
            Some(Agy)
        );
        assert_eq!(choose_default_agent(None, Some(Agy), all), Some(Agy));
        assert_eq!(choose_default_agent(None, None, all), Some(Claude));
        // An unusable choice (disabled, or not installed) falls through.
        let only_agy = |d| d == Agy;
        assert_eq!(
            choose_default_agent(Some(Claude), None, only_agy),
            Some(Agy)
        );
        assert_eq!(choose_default_agent(None, None, |_| false), None);
        // Codex is the last resort, and is picked when it is all that is usable.
        assert_eq!(
            choose_default_agent(None, None, |d| d == Driver::Codex),
            Some(Driver::Codex)
        );
    }

    #[test]
    fn a_codex_profile_is_recognised_by_its_binary_and_created_on_demand() {
        use agent_core::adapter::Driver;
        let profile = new_agent_profile(Driver::Codex);
        assert_eq!(
            (profile.name.as_str(), profile.command.as_str()),
            ("Codex", "codex")
        );
        assert_eq!(profile_driver(&profile), Some(Driver::Codex));
        let wrapped = Profile {
            command: "/opt/bin/Codex".into(),
            ..Profile::default()
        };
        assert_eq!(profile_driver(&wrapped), Some(Driver::Codex));
    }

    #[test]
    fn default_model_is_pulled_from_agent_settings() {
        use agent_core::adapter::Driver;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();

        // 1. Claude: ~/.claude/settings.json
        let claude_dir = home.join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(
            claude_dir.join("settings.json"),
            br#"{"model": "claude-sonnet-4-6"}"#,
        )
        .unwrap();
        assert_eq!(
            agent_settings_model(Driver::Claude, Some(home)),
            Some("claude-sonnet-4-6".to_string())
        );

        // 2. Agy: ~/.gemini/antigravity-cli/settings.json with display label containing effort
        let agy_dir = home.join(".gemini/antigravity-cli");
        std::fs::create_dir_all(&agy_dir).unwrap();
        std::fs::write(
            agy_dir.join("settings.json"),
            br#"{"model": "Gemini 3.8 Flash (High)"}"#,
        )
        .unwrap();
        assert_eq!(
            agent_settings_model(Driver::Agy, Some(home)),
            Some("gemini-3.8-flash-high".to_string())
        );

        // 3. Codex: ~/.codex/config.toml
        let codex_dir = home.join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("config.toml"),
            b"model = \"gpt-5-codex\"\n\n[other]\nfoo = \"bar\"\n",
        )
        .unwrap();
        assert_eq!(
            agent_settings_model(Driver::Codex, Some(home)),
            Some("gpt-5-codex".to_string())
        );

        // 4. Codex fallback to models_cache.json when config.toml has no top-level model
        let dir2 = tempfile::tempdir().unwrap();
        let home2 = dir2.path();
        let codex_dir2 = home2.join(".codex");
        std::fs::create_dir_all(&codex_dir2).unwrap();
        std::fs::write(
            codex_dir2.join("models_cache.json"),
            br#"{"models": [{"slug": "gpt-6.1-sol"}]}"#,
        )
        .unwrap();
        assert_eq!(
            agent_settings_model(Driver::Codex, Some(home2)),
            Some("gpt-6.1-sol".to_string())
        );

        // 5. Profile::effective_default_model
        let mut claude_profile = Profile {
            command: "claude".into(),
            ..Profile::default()
        };
        // Pulls from settings when profile has none
        assert_eq!(
            claude_profile.effective_default_model(Some(home)),
            Some("claude-sonnet-4-6".to_string())
        );
        // Explicit profile setting overrides agent settings
        claude_profile.default_model = Some("opus".into());
        assert_eq!(
            claude_profile.effective_default_model(Some(home)),
            Some("opus".to_string())
        );
    }

    fn write_config(dir: &Path, json: &str) -> PathBuf {
        let path = dir.join("config.json");
        std::fs::write(&path, json).unwrap();
        path
    }

    fn ids(config: &TerminalConfig) -> Vec<String> {
        config.profiles.iter().map(|p| p.id.clone()).collect()
    }

    #[test]
    fn profile_id_format_matches_the_contract() {
        for _ in 0..200 {
            let id = new_profile_id();
            assert!(is_valid_profile_id(&id), "{id}");
            assert_eq!(id.len(), 18);
            assert!(id.starts_with("p-"));
        }
        assert!(!is_valid_profile_id(""));
        assert!(!is_valid_profile_id("Upper"));
        assert!(!is_valid_profile_id("has space"));
        assert!(!is_valid_profile_id("under_score"));
        assert!(!is_valid_profile_id(&"a".repeat(65)));
        assert!(is_valid_profile_id(&"a".repeat(64)));
        let unique: std::collections::HashSet<_> = (0..1000).map(|_| new_profile_id()).collect();
        assert_eq!(unique.len(), 1000);
    }

    #[test]
    fn migration_assigns_each_missing_id_once_and_persists_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            r#"{"profiles":[{"name":"Claude","command":"claude"},{"name":"Tool","command":"tool"}]}"#,
        );
        let first = TerminalConfig::load_from(&path);
        assert!(first.profiles.iter().all(|p| is_valid_profile_id(&p.id)));
        assert_ne!(first.profiles[0].id, first.profiles[1].id);
        // Persisted at load: the file now carries the ids.
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains(&first.profiles[0].id));
        // A second load assigns nothing and does not rewrite the file.
        let second = TerminalConfig::load_from(&path);
        assert_eq!(ids(&first), ids(&second));
        assert!(!second.ids_assigned.get());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), on_disk);
    }

    #[test]
    fn ids_survive_save_load_rename_and_reorder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut config = TerminalConfig::default();
        let before = ids(&config);
        config.profiles[0].name = "Renamed".to_string();
        config.profiles.reverse();
        config.save_to(&path);
        let loaded = TerminalConfig::load_from(&path);
        let mut expected = before.clone();
        expected.reverse();
        assert_eq!(ids(&loaded), expected);
        assert_eq!(loaded.profile_by_id(&before[0]).unwrap().name, "Renamed");
        assert!(loaded.profile_by_id("p-missing").is_none());
    }

    #[test]
    fn duplicate_ids_are_repaired_first_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            r#"{"profiles":[
                {"id":"same","name":"A","command":"a"},
                {"id":"same","name":"B","command":"b"},
                {"id":"same","name":"C","command":"c"},
                {"id":"BAD ID","name":"D","command":"d"}]}"#,
        );
        let config = TerminalConfig::load_from(&path);
        assert_eq!(config.profiles[0].id, "same");
        let all = ids(&config);
        let unique: std::collections::HashSet<_> = all.iter().collect();
        assert_eq!(unique.len(), 4);
        assert!(all.iter().all(|id| is_valid_profile_id(id)));
        // Repaired on disk too, so the repair happens once.
        let again = TerminalConfig::load_from(&path);
        assert_eq!(ids(&again), all);
    }

    #[test]
    fn a_duplicated_profile_gets_a_new_id() {
        let original = new_agent_profile(agent_core::adapter::Driver::Claude);
        let copy = original.duplicate("Claude copy");
        assert_ne!(copy.id, original.id);
        assert!(is_valid_profile_id(&copy.id));
        assert_eq!(copy.name, "Claude copy");
        assert_eq!(copy.command, original.command);
        // The Preferences → Agents path (push a fresh agent profile) also never reuses one.
        assert_ne!(
            new_agent_profile(agent_core::adapter::Driver::Codex).id,
            new_agent_profile(agent_core::adapter::Driver::Codex).id
        );
    }

    #[test]
    fn a_default_profile_has_an_id_but_an_absent_one_reads_as_empty() {
        let profile = Profile::default();
        assert!(is_valid_profile_id(&profile.id));
        assert_ne!(Profile::default().id, profile.id);
        let parsed: Profile = serde_json::from_str(r#"{"name":"x","command":"x"}"#).unwrap();
        assert!(parsed.id.is_empty());
    }

    #[test]
    fn a_hand_edit_without_an_id_is_repaired_and_saved_once_without_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let config = TerminalConfig::default();
        config.save_to(&path);
        let known = ids(&config);

        let mut edited: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        edited["profiles"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"name": "Hand", "command": "hand"}));
        std::fs::write(&path, serde_json::to_string_pretty(&edited).unwrap()).unwrap();

        let DiskChange::Updated(updated) = config.check_disk_at(&path) else {
            panic!("a valid hand edit should be picked up");
        };
        assert_eq!(updated.profiles.len(), known.len() + 1);
        assert_eq!(ids(&updated)[..known.len()], known[..]);
        let new_id = updated.profiles.last().unwrap().id.clone();
        assert!(is_valid_profile_id(&new_id));
        // Saved: the file carries the id, and nothing flags the file as changed since.
        assert!(std::fs::read_to_string(&path).unwrap().contains(&new_id));
        assert!(matches!(
            updated.check_disk_at(&path),
            DiskChange::Unchanged
        ));
        assert!(!updated.ids_assigned.get());
        let backups = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".external-"))
            .count();
        assert_eq!(backups, 0);
    }

    #[test]
    fn profile_ids_round_trip_through_json() {
        let config = TerminalConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: TerminalConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(ids(&back), ids(&config));
        assert!(ids(&back).iter().all(|id| is_valid_profile_id(id)));
    }
}
