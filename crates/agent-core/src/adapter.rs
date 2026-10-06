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
    SetModel {
        model: String,
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

/// Fields to change on the current [`OpenSession`] before respawning.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenSessionDelta {
    pub model: Option<String>,
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
    fn encode(&mut self, command: Command) -> Result<Vec<Action>, AdapterError>;
    /// One stdout line → zero or more canonical events. Never panics on bad input: an
    /// unparseable line becomes `Event::Unknown` with the text in `raw`.
    fn feed(&mut self, line: &str) -> Vec<Envelope>;
    fn feed_stderr(&mut self, line: &str) -> Vec<Envelope>;
    /// Whole stdout of a finished [`Action::SideProcess`].
    fn feed_side(&mut self, id: &str, stdout: &str, success: bool) -> Vec<Envelope>;
    /// The process exited: close open items as interrupted, expire live approvals.
    fn on_exit(&mut self, code: Option<i32>) -> Vec<Envelope>;
}
