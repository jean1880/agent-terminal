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
    fn interrupt(&self);
    fn respond_approval(&self, request: &str, decision: Decision);
    fn answer_questions(&self, request: &str, answers: serde_json::Value);
    /// Switch model and/or agent. Same agent: in-session or restart per capabilities;
    /// other agent: budgeted, redacted handoff into a new provider thread.
    /// `effort: None` keeps the current effort (same agent) or the agent's own (other agent).
    /// For agy the effort is already part of `model` (the composed id); Claude takes it
    /// separately, and a changed effort restarts its process.
    fn switch(&self, driver: Driver, model: Option<String>, effort: Option<String>);
    fn set_mode(&self, mode: Mode);
    /// Returns the request id the `ControlResult` will carry.
    fn control(&self, control: Control) -> String;
    /// Current agent, model, mode and capabilities for the header and typeahead.
    fn status(&self) -> SessionStatus;
}

/// Where the model picker gets its rows: both agents' full model lists in one snapshot.
/// Implemented by [`crate::model_catalog::ModelCatalog`] (live) and the demo backend (static).
/// Without a source the picker falls back to the backend's `Control::ListModels`.
pub trait ModelSource {
    /// The current snapshot, possibly empty (nothing fetched or cached yet).
    fn models(&self) -> Vec<agent_core::catalog::CatalogModel>;
    /// Calls `f` on the main thread each time the snapshot changes.
    fn connect_changed(&self, f: Box<dyn Fn()>);
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionStatus {
    pub driver: Driver,
    pub model: Option<String>,
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
