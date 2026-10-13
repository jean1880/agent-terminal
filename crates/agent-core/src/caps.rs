//! Per-agent capability matrix. The UI greys out what an agent cannot do instead of failing.
//! Values are the verified Phase 0 behaviour (plans/2026-10-06_v3-structured-agents.md).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// `SetModel` applies to the next turn without a restart (Claude `set_model`, Codex
    /// `turn/start`). False (agy) means the session restarts and resumes with the new model.
    pub model_switch_in_session: bool,
    /// Approvals can be answered live (Claude control protocol; agy via the approval hook).
    pub live_approvals: bool,
    /// Interrupt keeps the process (Claude); false means the process exits (agy SIGINT).
    pub interrupt_keeps_process: bool,
    /// `Control::FileSuggestions` is answered by the agent. False means the app's own workspace
    /// index serves the `@` popover instead.
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
    /// Passes forms from its tool servers (MCP elicitation) to the UI to fill in or decline.
    #[serde(default)]
    pub forms: bool,
    /// Takes a message while a turn runs ([`crate::adapter::Command::Steer`]): Claude folds it
    /// into the running turn, Codex through `turn/steer`. False (agy) keeps the app's queue: agy
    /// runs a message written mid-turn as the next turn anyway (verified 2026-10-11).
    #[serde(default)]
    pub live_input: bool,
    /// Reads a background shell task's output while it runs (`Control::TaskOutput`, Claude
    /// `get_task_output`).
    #[serde(default)]
    pub task_output: bool,
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
            // The stream-json control protocol raises no MCP elicitation the adapter maps.
            forms: false,
            // A user line written mid-turn is folded into that turn: one `result`, no new
            // `init` (probe, 2026-10-11).
            live_input: true,
            // `get_task_output`: the last 8 KiB of a shell or Monitor task (CLI 2.1.296 schema).
            task_output: true,
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
            context_usage: true,
            usage: true,
            // No compact command: `/compact` continues in a new thread with a budgeted summary.
            compact_command: None,
            streams_text: true,
            // One whole thought block per step, read from agy's on-disk transcript,
            // never token by token.
            streams_reasoning: true,
            questions: false,
            plan_mode: true,
            // Headless agy has no channel for a tool server's form.
            forms: false,
            // A message written mid-turn waits and runs as the next turn (probe, 2026-10-11).
            live_input: false,
            // agy reports no background tasks.
            task_output: false,
        }
    }

    /// Verified against the upstream `app-server-protocol` source (see `codex.rs`) and live
    /// against codex-cli 0.161.0.
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
            // No `Control::Usage` (the /usage panel). Its quota still shows in the header meter,
            // fed by `account/rateLimits/updated`, which no flag gates.
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
            // `mcpServer/elicitation/request` is a live server request answered by the form card.
            forms: true,
            // `turn/steer` (codex-cli 0.161.0's own schema).
            live_input: true,
            // Its workers are sub-agents, shown in the sub-agent panel; no shell task to read.
            task_output: false,
        }
    }
}

/// One capability fact, named so the UI can render it: a control when the agent has it, a greyed
/// control or a notice (with [`Capabilities::unsupported_note`]) when it does not. There is one
/// variant per [`Capabilities`] field; `supports` and the exhaustive-destructure test keep the
/// two in step, so a field nothing reads cannot be added unnoticed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Feature {
    ModelSwitch,
    LiveApprovals,
    InterruptKeepsProcess,
    FileSuggestions,
    McpPanel,
    SettingsPanel,
    ModelList,
    ContextUsage,
    Usage,
    Compact,
    StreamsText,
    StreamsReasoning,
    Questions,
    PlanMode,
    Forms,
    LiveInput,
    TaskOutput,
}

impl Feature {
    pub const ALL: [Feature; 17] = [
        Feature::ModelSwitch,
        Feature::LiveApprovals,
        Feature::InterruptKeepsProcess,
        Feature::FileSuggestions,
        Feature::McpPanel,
        Feature::SettingsPanel,
        Feature::ModelList,
        Feature::ContextUsage,
        Feature::Usage,
        Feature::Compact,
        Feature::StreamsText,
        Feature::StreamsReasoning,
        Feature::Questions,
        Feature::PlanMode,
        Feature::Forms,
        Feature::LiveInput,
        Feature::TaskOutput,
    ];

    /// Where the app renders this fact (for people reading the contract test; the test checks
    /// only that it is named).
    pub fn surface(self) -> &'static str {
        match self {
            Feature::ModelSwitch => "model picker: applies at once, or restarts and resumes",
            Feature::LiveApprovals => "approval cards (Ask mode)",
            Feature::InterruptKeepsProcess => "stop button tooltip",
            Feature::FileSuggestions => {
                "@ popover (the agent's list, else the app's workspace index)"
            }
            Feature::McpPanel => "/mcp panel",
            Feature::SettingsPanel => "/config panel",
            Feature::ModelList => "/model picker",
            Feature::ContextUsage => "header gauge and /context",
            Feature::Usage => "/usage panel",
            Feature::Compact => "/compact",
            Feature::StreamsText => "assistant message rows",
            Feature::StreamsReasoning => "thought-process rows",
            Feature::Questions => "question cards",
            Feature::PlanMode => "mode picker: Plan",
            Feature::Forms => "form cards (a tool server's form or link)",
            Feature::LiveInput => "composer while a turn runs: sent at once, or queued",
            Feature::TaskOutput => "background task list: a shell task's live output",
        }
    }
}

impl Capabilities {
    /// Whether this agent has `feature`.
    pub fn supports(&self, feature: Feature) -> bool {
        match feature {
            Feature::ModelSwitch => self.model_switch_in_session,
            Feature::LiveApprovals => self.live_approvals,
            Feature::InterruptKeepsProcess => self.interrupt_keeps_process,
            Feature::FileSuggestions => self.file_suggestions,
            Feature::McpPanel => self.mcp_panel,
            Feature::SettingsPanel => self.settings_panel,
            Feature::ModelList => self.model_list,
            Feature::ContextUsage => self.context_usage,
            Feature::Usage => self.usage,
            Feature::Compact => self.compact_command.is_some(),
            Feature::StreamsText => self.streams_text,
            Feature::StreamsReasoning => self.streams_reasoning,
            Feature::Questions => self.questions,
            Feature::PlanMode => self.plan_mode,
            Feature::Forms => self.forms,
            Feature::LiveInput => self.live_input,
            Feature::TaskOutput => self.task_output,
        }
    }

    /// The tooltip or notice for a greyed control: what `agent` (a display name) lacks and what
    /// happens instead. `None` when the agent has the feature.
    pub fn unsupported_note(&self, feature: Feature, agent: &str) -> Option<String> {
        if self.supports(feature) {
            return None;
        }
        Some(match feature {
            Feature::ModelSwitch => {
                format!("{agent} restarts to switch models; the conversation resumes.")
            }
            Feature::LiveApprovals => format!("{agent} cannot ask before acting."),
            Feature::InterruptKeepsProcess => {
                format!("Stopping ends {agent}'s process; the next message restarts it.")
            }
            Feature::FileSuggestions => {
                format!("{agent} does not list files; the app searches the workspace instead.")
            }
            Feature::McpPanel => format!("{agent} has no MCP panel."),
            Feature::SettingsPanel => format!("{agent} has no settings panel."),
            Feature::ModelList => format!("{agent} has no model list."),
            Feature::ContextUsage => format!("{agent} reports no context window."),
            Feature::Usage => format!("{agent} has no usage panel; its quota shows in the header."),
            Feature::Compact => format!(
                "{agent} has no compact command; /compact continues in a new thread with a summary."
            ),
            Feature::StreamsText => format!("{agent} does not stream its replies."),
            Feature::StreamsReasoning => format!("{agent} shows no reasoning."),
            Feature::Questions => format!("{agent} asks no structured questions."),
            Feature::PlanMode => format!("{agent} has no read-only planning mode."),
            Feature::Forms => format!(
                "{agent} does not pass on forms from its tool servers; a tool that needs one \
                 asks in the reply instead."
            ),
            Feature::LiveInput => format!(
                "{agent} cannot take a message while it works; it is queued and sent when the \
                 turn ends."
            ),
            Feature::TaskOutput => format!("{agent} does not share a background task's output."),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::Driver;

    /// Adding a field to `Capabilities` stops this compiling until it is named here; give it a
    /// `Feature` variant, an `ALL` entry and a `supports` arm too.
    #[test]
    fn every_field_has_a_feature() {
        let Capabilities {
            model_switch_in_session: _,
            live_approvals: _,
            interrupt_keeps_process: _,
            file_suggestions: _,
            mcp_panel: _,
            settings_panel: _,
            model_list: _,
            context_usage: _,
            usage: _,
            compact_command: _,
            streams_text: _,
            streams_reasoning: _,
            questions: _,
            plan_mode: _,
            forms: _,
            live_input: _,
            task_output: _,
        } = Capabilities::claude();
        let mut seen = std::collections::HashSet::new();
        assert!(Feature::ALL.iter().all(|f| seen.insert(*f)));
    }

    /// `Feature::ALL` is hand-maintained. `next` has no wildcard, so a new variant does not
    /// compile until it is chained in here; walking the chain must then give exactly `ALL`.
    #[test]
    fn feature_all_lists_every_variant_once_in_order() {
        use Feature::*;
        fn next(f: Feature) -> Option<Feature> {
            match f {
                ModelSwitch => Some(LiveApprovals),
                LiveApprovals => Some(InterruptKeepsProcess),
                InterruptKeepsProcess => Some(FileSuggestions),
                FileSuggestions => Some(McpPanel),
                McpPanel => Some(SettingsPanel),
                SettingsPanel => Some(ModelList),
                ModelList => Some(ContextUsage),
                ContextUsage => Some(Usage),
                Usage => Some(Compact),
                Compact => Some(StreamsText),
                StreamsText => Some(StreamsReasoning),
                StreamsReasoning => Some(Questions),
                Questions => Some(PlanMode),
                PlanMode => Some(Forms),
                Forms => Some(LiveInput),
                LiveInput => Some(TaskOutput),
                TaskOutput => None,
            }
        }
        let mut walked = vec![ModelSwitch];
        while let Some(f) = next(*walked.last().expect("non-empty")) {
            walked.push(f);
        }
        assert_eq!(
            walked,
            Feature::ALL,
            "ALL misses a variant or is out of order"
        );
    }

    #[test]
    fn every_missing_feature_explains_itself_and_every_feature_names_its_control() {
        for driver in Driver::ALL {
            let caps = Capabilities::of(driver);
            let agent = driver.info().long_label;
            for feature in Feature::ALL {
                assert!(!feature.surface().is_empty());
                match caps.unsupported_note(feature, agent) {
                    None => assert!(caps.supports(feature)),
                    Some(note) => {
                        assert!(!caps.supports(feature));
                        assert!(note.contains(agent), "{note}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_matrix_matches_what_each_agent_does() {
        let agy = Capabilities::agy();
        // Reasoning arrives as one whole block per step (from its transcript), not token by token.
        assert!(agy.supports(Feature::StreamsReasoning));
        // Model switches restart and resume; compact continues in a new thread.
        assert!(!agy.supports(Feature::ModelSwitch));
        assert!(!agy.supports(Feature::Compact));
        let codex = Capabilities::codex();
        assert_eq!(
            codex
                .unsupported_note(Feature::McpPanel, "Codex")
                .as_deref(),
            Some("Codex has no MCP panel.")
        );
        // `usage` is the /usage panel; Codex's quota meter does not depend on it.
        assert!(!codex.supports(Feature::Usage));
        // Only Codex passes on its tool servers' forms; the others say why there is no card.
        assert!(codex.supports(Feature::Forms));
        for (caps, agent) in [(Capabilities::claude(), "Claude"), (agy, "Antigravity")] {
            assert!(caps
                .unsupported_note(Feature::Forms, agent)
                .is_some_and(|n| n.contains("forms from its tool servers")));
        }
    }
}
