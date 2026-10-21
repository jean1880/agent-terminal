//! Chat threads: the primary 3.0 surface.
//!
//! Layering (no cycles):
//! - `agent_core` adapters translate one agent CLI ⇄ canonical [`Envelope`]s (pure);
//! - [`crate::agent_proc`] runs the agent process on the GTK main loop (gio);
//! - [`session::ChatSession`] owns one thread: adapter + process + store + switching/handoff,
//!   and implements [`ChatBackend`];
//! - [`view::ChatView`] renders a thread from [`Envelope`]s and calls [`ChatBackend`] for every
//!   user action. It never touches a process, the store or an adapter directly.
//!
//! Both sides run on the GTK main thread, so the backend is shared as `Rc<dyn ChatBackend>`.

use agent_core::adapter::{Control, Driver, Mode};
use agent_core::event::{Decision, Envelope};

pub mod session;
pub mod view;
pub mod workspace_files;

/// The prompt a confirmed busy switch sends for the user. Stored as a user message (it is part
/// of the conversation), but the view shows it as a marker, never as words the user wrote.
pub const SWITCH_CONTINUATION: &str = "Continue the unfinished task after the model switch. \
    Review the existing context and current workspace state, then proceed from where the \
    interrupted turn left off. Do not repeat completed actions. Any unanswered approval or \
    question was cancelled by the switch; ask again if it is still needed.";

/// What the view can ask of a thread. Results come back as envelopes through the sink given
/// to the backend at construction (including `ControlResult` for `control`).
pub trait ChatBackend {
    /// Current canonical scope, including a queued worker before its first process event.
    /// The view must not guess a process generation from a registry row.
    fn worker_scope(
        &self,
        _worker: &agent_core::delegation::WorkerId,
    ) -> Option<agent_core::delegation::scope::Scope> {
        None
    }
    /// An operator action on a supervised worker. Backends must recheck the exact generation
    /// and run before dispatch; a stale inspector never acts on a replacement process.
    fn worker_action(&self, _action: WorkerAction) -> bool {
        false
    }
    /// The child's own workspace and turn baseline. Never fall back to the root's diff source.
    fn worker_diff_source(
        &self,
        _scope: &agent_core::delegation::scope::Scope,
    ) -> Option<std::rc::Rc<dyn DiffSource>> {
        None
    }
    fn send_prompt(&self, text: &str);
    fn queue_prompt(&self, text: &str) {
        self.send_prompt(text);
    }
    /// A message written while a turn runs, for an agent that takes it at once
    /// (`Capabilities::live_input`). A backend that cannot queues it as before.
    fn steer_prompt(&self, text: &str) {
        self.queue_prompt(text);
    }
    fn interrupt(&self);
    fn respond_approval(&self, request: &str, decision: Decision);
    fn respond_approval_with_rule(
        &self,
        request: &str,
        decision: Decision,
        custom_rule: Option<crate::always_allow::Rule>,
    ) {
        let _ = custom_rule;
        self.respond_approval(request, decision);
    }
    fn answer_questions(&self, request: &str, answers: serde_json::Value);
    /// Answers a form ([`agent_core::event::Event::FormRequested`]). `content` is what
    /// [`agent_core::form::answer`] returned, for an accepted field form only. Returns whether
    /// the adapter accepted the answer; `false` (refused, or no session) leaves the card to
    /// reopen. A backend that never raises forms refuses.
    fn respond_form(
        &self,
        request: &str,
        action: agent_core::form::FormAction,
        content: Option<serde_json::Value>,
    ) -> bool {
        let _ = (request, action, content);
        false
    }
    /// Switch model and/or agent. Same agent: in-session or restart per capabilities;
    /// other agent: budgeted, redacted handoff into a new provider thread.
    /// `effort: None` keeps the current effort (same agent) or the agent's own (other agent).
    /// For agy the effort is already part of `model` (the composed id); Claude takes it
    /// separately, and a changed effort restarts its process.
    fn switch(&self, driver: Driver, model: Option<String>, effort: Option<String>);
    /// What switching to `to` would cost, one sentence each, for the picker to say before the
    /// user chooses (a summary handoff, approvals that end, an agy that cannot ask). Empty for
    /// the current agent, and for a backend that cannot tell.
    fn switch_caveats(&self, _to: Driver) -> Vec<String> {
        Vec::new()
    }
    /// Restarts the agent process while keeping this thread's provider conversation and settings.
    /// A running turn, if any, is deliberately interrupted first.
    fn reload_session(&self);
    fn set_mode(&self, mode: Mode);
    /// Returns the request id the `ControlResult` will carry.
    fn control(&self, control: Control) -> String;
    /// Current agent, model, mode and capabilities for the header and typeahead.
    fn status(&self) -> SessionStatus;
    /// Where the agent process is in its life (starting, resuming, paused), for the header. A
    /// backend with no process to start says nothing.
    fn lifecycle(&self) -> Lifecycle {
        Lifecycle::default()
    }
    /// Pops the most recent queued user prompt if sitting in queue, returning (item_id, prompt_text).
    fn pop_queued_prompt(&self) -> Option<(String, String)> {
        None
    }
}

/// Where a thread's agent process is in its life. Presentation only: the header's "Starting…",
/// "Resuming…" and "Paused" and the sidebar's `ThreadActivity` are read from it; nothing
/// decides on it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Lifecycle {
    /// A process is running or starting.
    pub alive: bool,
    /// The process is starting and has not answered yet.
    pub starting: bool,
    /// That start resumes an agent paused to save memory.
    pub resuming: bool,
    /// The agent is paused to save memory; the next prompt resumes it.
    pub suspended: bool,
}

/// Actions originating in a supervised worker inspector. Native request ids are recovered
/// from the view's private scoped lookup, never accepted as an unscoped root request.
pub enum WorkerAction {
    Send {
        scope: agent_core::delegation::scope::Scope,
        text: String,
    },
    Cancel {
        scope: agent_core::delegation::scope::Scope,
    },
    Close {
        scope: agent_core::delegation::scope::Scope,
    },
    Approve {
        scope: agent_core::delegation::scope::Scope,
        request: String,
        decision: Decision,
        custom_rule: Option<crate::always_allow::Rule>,
    },
    Answer {
        scope: agent_core::delegation::scope::Scope,
        request: String,
        answers: serde_json::Value,
    },
    Form {
        scope: agent_core::delegation::scope::Scope,
        request: String,
        action: agent_core::form::FormAction,
        content: Option<serde_json::Value>,
    },
}

/// Where the model picker gets its rows: both agents' full model lists in one snapshot.
/// Implemented by [`crate::model_catalog::ModelCatalog`] (live) and the demo backend (static).
/// Without a source the picker falls back to the backend's `Control::ListModels`.
pub trait ModelSource {
    /// The current snapshot, possibly empty (nothing fetched or cached yet).
    fn models(&self) -> Vec<agent_core::catalog::CatalogModel>;
    /// Calls `f` on the main thread each time the snapshot changes; the id disconnects it.
    fn connect_changed(&self, f: Box<dyn Fn()>) -> u64;
    /// Removes a listener added by [`Self::connect_changed`].
    fn disconnect(&self, id: u64);
    /// How old the shown lists are and whether a fetch is running, for the picker's footer.
    /// `None`: the source has no clock (a static one), and the picker shows no footer.
    fn status(&self) -> Option<crate::freshness::CatalogStatus> {
        None
    }
    /// The picker opened (`force`: its Refresh button): fetch in the background what is old.
    /// Returns at once; the lists show from cache meanwhile.
    fn refresh_lists(self: std::rc::Rc<Self>, _force: bool) {}
    /// Calls `f` when [`Self::status`] may have changed; `None` when it never does.
    fn connect_status_changed(&self, _f: Box<dyn Fn()>) -> Option<u64> {
        None
    }
    /// Removes a listener added by [`Self::connect_status_changed`].
    fn disconnect_status(&self, _id: u64) {}
}

/// What a file-change card asks about: the transcript item and its tool input.
#[derive(Debug, Clone, PartialEq)]
pub struct DiffAsk {
    /// The item's id, which names the turn it belongs to.
    pub item: String,
    /// The tool input (`Value::Null` when the agent gave none yet).
    pub input: serde_json::Value,
}

/// A card's diff, or why there is none.
pub type DiffReply = Result<agent_kit::filediff::Shown, String>;

/// Where a file-change card gets its diff and opens it in the external tool. Implemented by the
/// window (it knows the thread's repository and the turns' baselines); the view only asks.
pub trait DiffSource {
    /// Computes the diff off the main thread; `done` runs on the main thread.
    fn load(&self, ask: DiffAsk, done: Box<dyn FnOnce(DiffReply)>);
    /// Opens the item's (first) file in the configured external tool. Failures are reported to
    /// the user by the implementation (a toast).
    fn open_external(&self, ask: DiffAsk);
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionStatus {
    pub driver: Driver,
    pub model: Option<String>,
    /// Requested selection waiting for backend acceptance. `model` remains the last report.
    pub pending_model: Option<String>,
    /// Reasoning effort of the session, when one was asked for.
    pub effort: Option<String>,
    pub mode: Mode,
    pub running_turn: bool,
    pub alive: bool,
    pub capabilities: agent_core::caps::Capabilities,
    pub commands: Vec<agent_core::event::AgentCommand>,
}

/// Where a backend delivers envelopes (the view's `apply`). Called on the main thread.
pub type EnvelopeSink = std::rc::Rc<dyn Fn(&Envelope)>;
