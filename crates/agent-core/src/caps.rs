//! Per-agent capability matrix. The UI greys out what an agent cannot do instead of failing.
//! Values are the verified Phase 0 behaviour (plans/2026-10-06_v3-structured-agents.md).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// `SetModel` applies to the next turn without a restart (Claude `set_model`).
    pub model_switch_in_session: bool,
    /// Approvals can be answered live (Claude control protocol; agy via the approval hook).
    pub live_approvals: bool,
    /// Interrupt keeps the process (Claude); false means the process exits (agy SIGINT).
    pub interrupt_keeps_process: bool,
    /// `Control::FileSuggestions` is answered by the agent.
    pub file_suggestions: bool,
    /// Native-panel controls the agent answers (`/mcp`, `/config`, `/model`, usage, context).
    pub mcp_panel: bool,
    pub settings_panel: bool,
    pub model_list: bool,
    pub context_usage: bool,
    pub usage: bool,
    /// The agent compacts on `/compact` (Claude) or `/compress`-style command (`compact_command`).
    pub compact_command: Option<String>,
    /// Streams assistant text token by token.
    pub streams_text: bool,
    /// Streams reasoning text.
    pub streams_reasoning: bool,
    /// Asks structured questions the UI answers (Claude AskUserQuestion).
    pub questions: bool,
    /// Has a read-only planning mode (`Mode::Plan`).
    pub plan_mode: bool,
}

impl Capabilities {
    /// The capabilities of `driver`'s adapter.
    pub fn of(driver: crate::adapter::Driver) -> Self {
        use crate::adapter::Driver;
        match driver {
            Driver::Claude => Self::claude(),
            Driver::Agy => Self::agy(),
            Driver::Codex => Self::codex(),
        }
    }

    pub fn claude() -> Self {
        Self {
            model_switch_in_session: true,
            live_approvals: true,
            interrupt_keeps_process: true,
            file_suggestions: true,
            mcp_panel: true,
            settings_panel: true,
            model_list: true,
            context_usage: true,
            usage: true,
            compact_command: Some("/compact".to_owned()),
            streams_text: true,
            streams_reasoning: true,
            questions: true,
            plan_mode: true,
        }
    }

    pub fn agy() -> Self {
        Self {
            model_switch_in_session: false,
            live_approvals: true,
            interrupt_keeps_process: false,
            file_suggestions: false,
            mcp_panel: false,
            settings_panel: true,
            model_list: true,
            context_usage: false,
            usage: true,
            compact_command: None,
            streams_text: true,
            streams_reasoning: false,
            questions: false,
            plan_mode: true,
        }
    }

    /// Verified against the upstream `app-server-protocol` source (see `codex.rs`), not a
    /// recording: Codex is not installed on the development machine yet.
    pub fn codex() -> Self {
        Self {
            // `turn/start` takes `model` and applies it to that turn and later ones.
            model_switch_in_session: true,
            // Approvals are server requests answered over the live connection.
            live_approvals: true,
            // `turn/interrupt` ends the turn, not the `app-server` process.
            interrupt_keeps_process: true,
            file_suggestions: false,
            mcp_panel: false,
            settings_panel: false,
            // `model/list`.
            model_list: true,
            // `thread/tokenUsage/updated` carries the window size and occupancy.
            context_usage: true,
            usage: false,
            // `thread/compact/start`: the adapter encodes a `/compact` prompt as that request.
            compact_command: Some("/compact".to_owned()),
            streams_text: true,
            // Reasoning *summaries* (`item/reasoning/summaryTextDelta`), requested per turn.
            streams_reasoning: true,
            // `item/tool/requestUserInput` is a live server request answered by the question card.
            questions: true,
            // Plan = read-only sandbox and never escalate (`codex.rs`, "Modes").
            plan_mode: true,
        }
    }
}
