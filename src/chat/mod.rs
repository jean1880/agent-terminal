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

/// What the view can ask of a thread. Results come back as envelopes through the sink given
/// to the backend at construction (including `ControlResult` for `control`).
pub trait ChatBackend {
    fn send_prompt(&self, text: &str);
    fn queue_prompt(&self, text: &str) {
        self.send_prompt(text);
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
    /// Switch model and/or agent. Same agent: in-session or restart per capabilities;
    /// other agent: budgeted, redacted handoff into a new provider thread.
    /// `effort: None` keeps the current effort (same agent) or the agent's own (other agent).
    /// For agy the effort is already part of `model` (the composed id); Claude takes it
    /// separately, and a changed effort restarts its process.
    fn switch(&self, driver: Driver, model: Option<String>, effort: Option<String>);
    /// Restarts the agent process while keeping this thread's provider conversation and settings.
    /// A running turn, if any, is deliberately interrupted first.
    fn reload_session(&self);
    fn set_mode(&self, mode: Mode);
    /// Returns the request id the `ControlResult` will carry.
    fn control(&self, control: Control) -> String;
    /// Current agent, model, mode and capabilities for the header and typeahead.
    fn status(&self) -> SessionStatus;
    /// Pops the most recent queued user prompt if sitting in queue, returning (item_id, prompt_text).
    fn pop_queued_prompt(&self) -> Option<(String, String)> {
        None
    }
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
