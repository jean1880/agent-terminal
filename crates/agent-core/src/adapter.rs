//! The adapter contract: a pure, sans-I/O translator between one agent CLI and the canonical
//! [`Event`](crate::event::Event) stream.
//!
//! An adapter never spawns a process, reads a file or touches the network. The app's transport
//! (`src/agent_proc.rs`, gio) owns the process: it calls [`Adapter::argv`] to spawn, writes the
//! lines [`Adapter::encode`] returns, feeds every stdout/stderr line to [`Adapter::feed`] /
//! [`Adapter::feed_stderr`], and reports the exit through [`Adapter::on_exit`]. Replay tests feed
//! recorded fixture lines straight into `feed`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::caps::Capabilities;
use crate::event::{Decision, Envelope};

/// Which agent CLI an adapter drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Driver {
    Claude,
    Agy,
    Codex,
}

/// Everything the app knows about a driver that is not behaviour: its names, colour and the
/// command it is detected by. The one place a new driver is described; menus, probes, labels and
/// Preferences iterate [`Driver::ALL`] and read this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverInfo {
    pub driver: Driver,
    /// Stable id: the store's `provider_threads.driver`, menu action targets, config.
    pub key: &'static str,
    /// Short name for menus, rows and the composer hint (`Antigravity`).
    pub label: &'static str,
    /// Name with the command where it helps (`Antigravity (agy)`).
    pub long_label: &'static str,
    /// The CSS class carrying the agent's accent colour.
    pub accent_class: &'static str,
    /// The accent colour, `#rrggbb` (the CSS classes use the same values).
    pub accent_hex: &'static str,
    /// The bundled symbolic icon name of the agent's brand mark (`assets/icons`). Codex's mark
    /// honours a user override in the app (`icons::driver_icon`).
    pub brand_icon: &'static str,
    /// The command a fresh profile runs and detection looks for.
    pub default_command: &'static str,
    /// The name of the profile created for it.
    pub profile_name: &'static str,
    /// What to tell the user when the binary is missing.
    pub install_hint: &'static str,
    /// Arguments after the agent's command that sign in, run in a terminal tab: an installed
    /// agent nobody has signed in to (setup skipped, or the login expired) cannot run a turn.
    /// Empty: the bare command starts its own sign-in on a fresh install.
    pub sign_in_args: &'static [&'static str],
    /// What signing in means for it, for the banner and Settings.
    pub sign_in_hint: &'static str,
}

const REGISTRY: [DriverInfo; 3] = [
    DriverInfo {
        driver: Driver::Claude,
        key: "claude",
        label: "Claude",
        long_label: "Claude",
        accent_class: "accent-claude",
        accent_hex: "#e8846b",
        brand_icon: "agent-claude-symbolic",
        default_command: "claude",
        profile_name: "Claude",
        install_hint: "Install Claude Code (npm install -g @anthropic-ai/claude-code).",
        sign_in_args: &[],
        sign_in_hint: "Run claude once and sign in to your Anthropic account (or /login).",
    },
    DriverInfo {
        driver: Driver::Agy,
        key: "agy",
        label: "Antigravity",
        long_label: "Antigravity (agy)",
        accent_class: "accent-agy",
        accent_hex: "#5b9cf6",
        brand_icon: "agent-agy-symbolic",
        default_command: "agy",
        profile_name: "Agy",
        install_hint: "Install the Antigravity CLI (agy).",
        sign_in_args: &[],
        sign_in_hint: "Run agy once and sign in with your Google account in the browser it opens.",
    },
    DriverInfo {
        driver: Driver::Codex,
        key: "codex",
        label: "Codex",
        long_label: "Codex",
        accent_class: "accent-codex",
        accent_hex: "#4cc38a",
        brand_icon: "agent-codex-symbolic",
        default_command: "codex",
        profile_name: "Codex",
        install_hint: "Install the Codex CLI (npm install -g @openai/codex).",
        sign_in_args: &["login"],
        sign_in_hint: "Run codex login and sign in with ChatGPT (or an API key).",
    },
];

impl Driver {
    /// Every driver, in the order menus and handoffs offer them.
    pub const ALL: [Driver; 3] = [Driver::Claude, Driver::Agy, Driver::Codex];

    /// The driver's description. Exhaustive: a new variant fails to compile here until it has one.
    pub fn info(self) -> &'static DriverInfo {
        match self {
            Driver::Claude => &REGISTRY[0],
            Driver::Agy => &REGISTRY[1],
            Driver::Codex => &REGISTRY[2],
        }
    }

    /// The driver with this [`DriverInfo::key`].
    pub fn from_key(key: &str) -> Option<Driver> {
        Self::ALL.into_iter().find(|d| d.info().key == key)
    }
}

/// Approval / edit policy for a session (mapped per agent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Ask before edits and commands (Claude `default`; agy: hook asks for everything gated).
    #[default]
    Ask,
    /// Edits allowed, commands ask (Claude `acceptEdits`; agy `--mode accept-edits` + hook).
    AcceptEdits,
    /// Read-only planning (Claude `plan`; agy `--mode plan`).
    Plan,
}

/// Everything needed to start (or restart) an agent process for one provider thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenSession {
    /// Absolute path of the binary to run (resolved by the app from the profile).
    pub program: String,
    /// Extra profile arguments, appended before the adapter's own.
    #[serde(default)]
    pub extra_args: Vec<String>,
    pub cwd: String,
    #[serde(default)]
    pub model: Option<String>,
    /// Reasoning effort for the whole session (Claude `--effort`). agy folds effort into its
    /// model id (`gemini-3.1-pro-high`), so its adapter ignores this.
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub mode: Mode,
    /// Native id to resume (Claude `--resume=`, agy `--conversation`).
    #[serde(default)]
    pub resume: Option<String>,
    /// Native id to create with (Claude `--session-id=`); agy assigns its own.
    #[serde(default)]
    pub new_session_id: Option<String>,
    /// The app's approval hook is installed and its socket is exported to this process.
    /// agy only: without it the adapter must not use `--dangerously-skip-permissions`.
    #[serde(default)]
    pub approval_hook: bool,
}

/// What the app asks an adapter to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    Prompt {
        text: String,
    },
    Interrupt,
    /// Answer an approval the adapter raised itself (Claude `can_use_tool`).
    Approve {
        request: String,
        decision: Decision,
        #[serde(default)]
        updated_input: Option<Value>,
        #[serde(default)]
        message: Option<String>,
    },
    /// Answer structured questions (Claude AskUserQuestion via `can_use_tool`).
    Answer {
        request: String,
        answers: Value,
    },
    /// `effort: None` leaves the effort as it is. Claude has no in-session effort control, so
    /// an effort different from the session's is a respawn; agy's effort is part of `model`.
    SetModel {
        model: String,
        #[serde(default)]
        effort: Option<String>,
    },
    SetMode {
        mode: Mode,
    },
    /// A control request; the reply arrives as `Event::ControlResult` with `request == id`.
    Control {
        id: String,
        control: Control,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Control {
    McpStatus,
    McpToggle { server: String, enabled: bool },
    McpReconnect { server: String },
    GetSettings,
    ListModels,
    FileSuggestions { query: String },
    ContextUsage,
    Usage,
}

/// How the transport must carry out an encoded command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Write these lines (each without a trailing newline) to the agent's stdin.
    Write(Vec<String>),
    /// Send SIGINT to the agent process.
    Interrupt,
    /// Stop this process and start a new one with the given session (model switch, resume).
    Respawn(OpenSessionDelta),
    /// Run a separate one-shot process (agy `-p /model --output-format json`); its stdout goes
    /// to [`Adapter::feed_side`] with the same request id.
    SideProcess { id: String, argv: Vec<String> },
}

/// What an adapter has to say outside a call's return value (see [`Adapter::drain_outbox`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outbox {
    pub actions: Vec<Action>,
    pub events: Vec<Envelope>,
}

impl Outbox {
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty() && self.events.is_empty()
    }
}

/// Fields to change on the current [`OpenSession`] before respawning.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenSessionDelta {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<Mode>,
    /// Always set by the adapter to the current native id so history carries over.
    pub resume: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterError {
    /// The agent cannot do this; the caller follows `decide_transition` (restart or handoff).
    Unsupported(&'static str),
    /// The command is invalid in the current state (e.g. answering an expired request).
    Invalid(String),
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(what) => write!(f, "not supported by this agent: {what}"),
            Self::Invalid(why) => write!(f, "invalid command: {why}"),
        }
    }
}

impl std::error::Error for AdapterError {}

pub trait Adapter {
    fn driver(&self) -> Driver;
    fn capabilities(&self) -> &Capabilities;
    /// Full argv (program first) for this session.
    fn argv(&self, session: &OpenSession) -> Vec<String>;
    /// Lines to write as soon as the process starts (Claude `initialize`); may be empty.
    fn handshake(&mut self) -> Vec<String>;
    /// Whether a prompt starts/queues work in the main session, including custom commands.
    /// Local controls and side processes override this so the host can mark submitted turns
    /// busy before the backend acknowledges them.
    fn prompt_starts_turn(&self, _text: &str) -> bool {
        true
    }
    /// Discard prompts waiting for backend startup when the host deliberately stops work.
    /// A retry after a failed spawn can instead keep them by leaving this uncalled.
    fn discard_queued_prompts(&mut self) {}
    fn encode(&mut self, command: Command) -> Result<Vec<Action>, AdapterError>;
    /// One stdout line → zero or more canonical events. Never panics on bad input: an
    /// unparseable line becomes `Event::Unknown` with the text in `raw`.
    fn feed(&mut self, line: &str) -> Vec<Envelope>;
    fn feed_stderr(&mut self, line: &str) -> Vec<Envelope>;
    /// Whole stdout of a finished [`Action::SideProcess`].
    fn feed_side(&mut self, id: &str, stdout: &str, success: bool) -> Vec<Envelope>;
    /// The process exited: close open items as interrupted, expire live approvals.
    fn on_exit(&mut self, code: Option<i32>) -> Vec<Envelope>;
    /// Output the adapter produced on its own initiative: actions to carry out like
    /// [`Adapter::encode`]'s, and events to dispatch like [`Adapter::feed`]'s. The transport
    /// calls it after every `encode`, `feed` and `feed_side`, and empties it each time. Needed by
    /// agents whose protocol is a two-way RPC (Codex: prompts queued until the thread exists,
    /// replies to server requests the app does not handle, answers to controls the adapter can
    /// give itself); Claude and agy have none.
    fn drain_outbox(&mut self) -> Outbox {
        Outbox::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_driver_is_listed_and_described_once() {
        // Exhaustive: adding a variant fails to compile here until ALL and the registry have it.
        for d in Driver::ALL {
            match d {
                Driver::Claude | Driver::Agy | Driver::Codex => {}
            }
            let info = d.info();
            assert_eq!(info.driver, d, "{} points at another driver", info.key);
            assert_eq!(Driver::from_key(info.key), Some(d));
            assert!(!info.label.is_empty() && !info.default_command.is_empty());
            assert!(info.accent_class.starts_with("accent-"));
            assert!(
                info.brand_icon.starts_with("agent-") && info.brand_icon.ends_with("-symbolic")
            );
            assert!(info.accent_hex.starts_with('#') && info.accent_hex.len() == 7);
            // The capability table covers it too.
            let _ = Capabilities::of(d);
        }
        let keys: std::collections::HashSet<_> = Driver::ALL.iter().map(|d| d.info().key).collect();
        assert_eq!(keys.len(), Driver::ALL.len(), "keys are unique");
        assert_eq!(REGISTRY.len(), Driver::ALL.len());
        assert_eq!(Driver::from_key("nope"), None);
    }

    #[test]
    fn keys_match_the_serde_names_the_store_already_holds() {
        for d in Driver::ALL {
            let json = serde_json::to_value(d).expect("serde");
            assert_eq!(json.as_str(), Some(d.info().key));
        }
    }
}
