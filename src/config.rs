// How a profile's `session_store` is laid out; defined with the readers in
// `agent-kit` and re-exported so config and the app keep their old path.
pub use agent_kit::sessions::SessionFormat;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
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
            CliClient::Agy => write!(f, "Agy"),
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
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct Profile {
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
}

impl Profile {
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

/// The profiles a fresh install starts with — the three clients the old
/// `CliClient` enum hard-coded, in the same preference order.
fn default_profiles() -> Vec<Profile> {
    ["Claude", "Agy", "Gemini"]
        .iter()
        .map(|name| {
            let mut profile = Profile {
                name: (*name).to_string(),
                command: name.to_lowercase(),
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
    "security-high-symbolic".to_string()
}
fn default_icon_warn() -> String {
    "dialog-warning-symbolic".to_string()
}
fn default_icon_unknown() -> String {
    "dialog-question-symbolic".to_string()
}

/// Ceiling on header indicators, so a config edit cannot fill the header bar.
pub const MAX_INDICATORS: usize = 5;

/// The terminal color scheme.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeChoice {
    #[default]
    Antigravity,
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
            ThemeChoice::Antigravity => "Antigravity",
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
    pub const ALL: [ThemeChoice; 7] = [
        ThemeChoice::Antigravity,
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
    /// Environment variables removed from a spawned session's environment.
    /// A trailing `*` matches by prefix. See [`default_clear_env`].
    pub clear_env: Vec<String>,
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
static LOAD_PROBLEM: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Takes the reason the settings could not be loaded, if they could not.
pub fn take_load_problem() -> Option<String> {
    LOAD_PROBLEM.lock().ok()?.take()
}

fn report_load_problem(problem: String) {
    warn!("{problem}");
    if let Ok(mut slot) = LOAD_PROBLEM.lock() {
        *slot = Some(problem);
    }
}

/// Copies `path` to `<name>.<tag>-<nanos>` beside it, so a file about to be
/// replaced is kept rather than lost. `None` if the copy failed.
///
/// An identical copy already kept is reused, so a file that stays broken
/// across launches leaves one copy rather than one per launch.
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
            .find(|p| fs::read(p).is_ok_and(|kept| kept == content))
    });
    if existing.is_some() {
        return existing;
    }

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let copy = path.with_file_name(format!("{prefix}{nanos}"));
    match fs::write(&copy, &content) {
        Ok(()) => Some(copy),
        Err(err) => {
            warn!("Could not keep a copy of {}: {err}", path.display());
            None
        }
    }
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
            restore_session: false,
            checkpoints: true,
            diff_panel_visible: false,
            diff_panel_width: 520,
            worktree_root: String::new(),
            indicators: Vec::new(),
            turn_command: None,
            clear_env: default_clear_env(),
            disk_stamp: std::cell::Cell::default(),
            save_blocked: std::cell::Cell::default(),
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

    /// Serializes and writes the config to an explicit path, logging on failure.
    ///
    /// The write goes to a sibling temporary file which is then renamed over the
    /// target. Rename within a directory is atomic on Linux, so an interrupted
    /// save leaves either the previous config or the new one — never the
    /// half-written file a plain `fs::write` would produce.
    fn save_to(&self, path: &Path) {
        if self.save_blocked.get() {
            warn!(
                "Not saving settings to {}: the file there is unusable and has no other copy",
                path.display()
            );
            return;
        }
        let content = match serde_json::to_string_pretty(self) {
            Ok(content) => content,
            Err(e) => {
                warn!("Failed to serialize config: {}", e);
                return;
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
            return;
        }
        self.disk_stamp.set(disk_stamp(path));
    }

    /// Writes `bytes` to a sibling temporary file, then renames it over `path`.
    /// Rename within a directory is atomic, so a crash leaves the old file or
    /// the new one, never a half-written one.
    fn replace_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
        tmp_name.push(".tmp");
        let tmp = path.with_file_name(tmp_name);
        let result = Self::write_all_synced(&tmp, bytes).and_then(|()| fs::rename(&tmp, path));
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    /// Writes `bytes` to `path`, flushing them to disk before returning. The
    /// `sync_all` matters: without it the rename can land before the contents do,
    /// which on a crash yields an empty config rather than an intact old one.
    fn write_all_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let mut file = fs::File::create(path)?;
        file.write_all(bytes)?;
        file.sync_all()
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
                ThemeChoice::Antigravity => 0,
                ThemeChoice::Dracula => 1,
                ThemeChoice::Nord => 2,
                ThemeChoice::GruvboxDark => 3,
                ThemeChoice::SolarizedDark => 4,
                ThemeChoice::OneDark => 5,
                ThemeChoice::Monokai => 6,
            }
        }

        assert_eq!(ThemeChoice::ALL.len(), 7);
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
        // pinned Gemini back to auto-detection.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"cli_client":"gemini","scrollback_lines":700}"#)
            .unwrap();

        let loaded = TerminalConfig::load_from(&path);
        assert_eq!(loaded.default_profile.as_deref(), Some("Gemini"));
        assert_eq!(loaded.scrollback_lines, 700);
        // And the default profile list is materialised for it.
        assert!(loaded.profiles.iter().any(|p| p.command == "gemini"));
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
    fn indicators_load_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"indicators":[{"label":"LiteLLM","icon_ok":"network-server-symbolic","source":{"type":"command","argv":["litellm-sync","--indicator"],"timeout_secs":3}}]}"#)
            .unwrap();
        let config = TerminalConfig::load_from(&path);
        assert_eq!(config.indicators.len(), 1);
        assert_eq!(config.indicators[0].label, "LiteLLM");
        assert_eq!(config.indicators[0].icon_ok, "network-server-symbolic");
    }

    #[test]
    fn indicators_accept_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"indicators":[{"name":"LiteLLM","icon":"network-server-symbolic","source":{"type":"command","argv":["litellm-sync","--indicator"],"timeout_secs":3}}]}"#)
            .unwrap();
        let config = TerminalConfig::load_from(&path);
        assert_eq!(config.indicators.len(), 1);
        assert_eq!(config.indicators[0].label, "LiteLLM");
        assert_eq!(config.indicators[0].icon_ok, "network-server-symbolic");
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
}
