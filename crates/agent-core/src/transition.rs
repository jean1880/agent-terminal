//! Session transition policy: what switching model, mode, workspace or agent mid-thread requires.
//!
//! Port of T3 Code (MIT, see `THIRD_PARTY.md`) `ProviderSessionTransitionPolicy.ts`
//! (`decideProviderSessionTransition`), `ProviderSelectionTransition.ts` (the selection plan)
//! and the planning step of `ProviderSwitchService.ts`. Pure decisions: nothing here spawns,
//! restarts or hands off; the caller acts on the returned [`Transition`].
//!
//! Differences from T3: we have no provider instances or continuation keys (the [`Driver`] is
//! the whole identity), so T3's instance/continuation checks collapse into a driver comparison,
//! and T3's `runtimeMode` maps to our approval [`Mode`].

use serde::{Deserialize, Serialize};

use crate::adapter::{Driver, Mode};
use crate::caps::Capabilities;

/// One complete model choice: which agent, which model, how hard it should think.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSelection {
    pub driver: Driver,
    pub model: String,
    #[serde(default)]
    pub effort: Option<String>,
}

/// How a same-driver selection change can be applied (T3 `ProviderSelectionTransitionPlan`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionPlan {
    /// The running process takes the new selection at the next turn (Claude `set_model`).
    ApplyOnNextTurn,
    /// The process must be stopped and resumed with the new selection (agy).
    RestartSession,
    /// The native session cannot carry over; start a new one and replay a handoff.
    CreateWithHandoff,
    /// The change cannot be made; the reason is shown to the user.
    Reject(String),
}

/// What the caller must do to reach the target state (T3 `ProviderSessionTransition`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    /// The live session already satisfies the target.
    Reuse,
    /// Apply the new selection to the live session (no restart).
    SwitchModelInSession,
    /// Stop the process and resume the same native session with the new settings.
    RestartAndResume,
    /// Start a fresh native session and carry history over with a handoff.
    CreateWithHandoff,
    /// Refuse, with a reason fit to show the user.
    Reject(String),
}

/// A session as the policy sees it: enough to compare two of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionState {
    pub selection: ModelSelection,
    pub mode: Mode,
    pub workspace: String,
    pub capabilities: Capabilities,
}

/// Classifies a selection change (T3 `turnScopedSelectionTransition` / `acpSelectionTransition`,
/// folded into the verified per-agent behaviour in [`Capabilities`]).
///
/// A different driver always needs a handoff. Within a driver, an agent that switches models
/// inside the session (Claude) applies the change on the next turn; one that cannot (agy) must
/// be restarted and resumed. `caps` are the capabilities of the live (current) session.
pub fn plan_selection(
    caps: &Capabilities,
    current: &ModelSelection,
    target: &ModelSelection,
) -> SelectionPlan {
    if current.driver != target.driver {
        SelectionPlan::CreateWithHandoff
    } else if caps.model_switch_in_session {
        SelectionPlan::ApplyOnNextTurn
    } else {
        SelectionPlan::RestartSession
    }
}

/// Decides how to move from `current` (the live session, if any) to `target`.
///
/// Branch order follows T3 `decideProviderSessionTransition`:
/// 1. an unavailable target is rejected, and no live session means a fresh one with a handoff;
/// 2. a driver change is a handoff;
/// 3. a same-driver selection change takes the plan's `CreateWithHandoff`/`Reject` outcome
///    first, even when the workspace or mode also changed, and a missing plan is rejected;
/// 4. a workspace or mode change restarts and resumes;
/// 5. otherwise the plan decides (`ApplyOnNextTurn` switches in session, `RestartSession`
///    restarts), and an unchanged selection reuses the session.
pub fn decide_transition(
    current: Option<&SessionState>,
    target: &SessionState,
    target_available: bool,
    selection_plan: Option<&SelectionPlan>,
) -> Transition {
    if !target_available {
        return Transition::Reject("The target agent is unavailable.".to_owned());
    }
    let Some(current) = current else {
        return Transition::CreateWithHandoff;
    };
    if current.selection.driver != target.selection.driver {
        return Transition::CreateWithHandoff;
    }

    let mode_changed = current.mode != target.mode;
    let workspace_changed = current.workspace != target.workspace;
    let selection_changed = current.selection != target.selection;

    if selection_changed {
        match selection_plan {
            None => {
                return Transition::Reject(
                    "The agent adapter did not classify the selection change.".to_owned(),
                );
            }
            Some(SelectionPlan::CreateWithHandoff) => return Transition::CreateWithHandoff,
            Some(SelectionPlan::Reject(reason)) => return Transition::Reject(reason.clone()),
            Some(SelectionPlan::ApplyOnNextTurn | SelectionPlan::RestartSession) => {}
        }
    }
    if mode_changed || workspace_changed {
        return Transition::RestartAndResume;
    }
    if selection_changed {
        return match selection_plan {
            Some(SelectionPlan::ApplyOnNextTurn) => Transition::SwitchModelInSession,
            // `RestartSession`; the other plans returned above and `None` is unreachable here.
            _ => Transition::RestartAndResume,
        };
    }
    Transition::Reuse
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection(driver: Driver, model: &str, effort: Option<&str>) -> ModelSelection {
        ModelSelection {
            driver,
            model: model.to_owned(),
            effort: effort.map(str::to_owned),
        }
    }

    fn claude_state() -> SessionState {
        SessionState {
            selection: selection(Driver::Claude, "opus", Some("medium")),
            mode: Mode::Ask,
            workspace: "/repo".to_owned(),
            capabilities: Capabilities::claude(),
        }
    }

    fn agy_state() -> SessionState {
        SessionState {
            selection: selection(Driver::Agy, "gemini-pro", None),
            mode: Mode::Ask,
            workspace: "/repo".to_owned(),
            capabilities: Capabilities::agy(),
        }
    }

    fn other_model(state: &SessionState, model: &str) -> SessionState {
        SessionState {
            selection: ModelSelection {
                model: model.to_owned(),
                ..state.selection.clone()
            },
            ..state.clone()
        }
    }

    #[test]
    fn unavailable_target_is_rejected_before_anything_else() {
        let base = claude_state();
        let out = decide_transition(Some(&base), &base, false, None);
        assert!(matches!(out, Transition::Reject(_)), "{out:?}");
        let out = decide_transition(None, &base, false, None);
        assert!(matches!(out, Transition::Reject(_)), "{out:?}");
    }

    #[test]
    fn no_live_session_creates_with_handoff() {
        let base = claude_state();
        assert_eq!(
            decide_transition(None, &base, true, None),
            Transition::CreateWithHandoff
        );
    }

    #[test]
    fn identical_state_is_reused() {
        let base = claude_state();
        assert_eq!(
            decide_transition(Some(&base), &base, true, None),
            Transition::Reuse
        );
    }

    #[test]
    fn cross_driver_uses_handoff_even_with_a_plan() {
        let from = claude_state();
        let to = agy_state();
        assert_eq!(
            decide_transition(Some(&from), &to, true, None),
            Transition::CreateWithHandoff
        );
        assert_eq!(
            decide_transition(
                Some(&from),
                &to,
                true,
                Some(&SelectionPlan::ApplyOnNextTurn)
            ),
            Transition::CreateWithHandoff
        );
    }

    #[test]
    fn same_driver_model_change_follows_the_plan() {
        let base = claude_state();
        let target = other_model(&base, "sonnet");
        assert_eq!(
            decide_transition(
                Some(&base),
                &target,
                true,
                Some(&SelectionPlan::ApplyOnNextTurn)
            ),
            Transition::SwitchModelInSession
        );
        assert_eq!(
            decide_transition(
                Some(&base),
                &target,
                true,
                Some(&SelectionPlan::RestartSession)
            ),
            Transition::RestartAndResume
        );
        assert_eq!(
            decide_transition(
                Some(&base),
                &target,
                true,
                Some(&SelectionPlan::CreateWithHandoff)
            ),
            Transition::CreateWithHandoff
        );
        assert_eq!(
            decide_transition(
                Some(&base),
                &target,
                true,
                Some(&SelectionPlan::Reject("nope".to_owned()))
            ),
            Transition::Reject("nope".to_owned())
        );
    }

    #[test]
    fn effort_only_change_is_a_selection_change() {
        let base = claude_state();
        let mut target = base.clone();
        target.selection.effort = Some("high".to_owned());
        assert_eq!(
            decide_transition(
                Some(&base),
                &target,
                true,
                Some(&SelectionPlan::ApplyOnNextTurn)
            ),
            Transition::SwitchModelInSession
        );
    }

    #[test]
    fn workspace_or_mode_change_restarts_and_resumes() {
        let base = claude_state();
        let mut moved = base.clone();
        moved.workspace = "/other".to_owned();
        assert_eq!(
            decide_transition(Some(&base), &moved, true, None),
            Transition::RestartAndResume
        );
        let mut planning = base.clone();
        planning.mode = Mode::Plan;
        assert_eq!(
            decide_transition(Some(&base), &planning, true, None),
            Transition::RestartAndResume
        );
    }

    #[test]
    fn a_rejected_selection_survives_a_workspace_change() {
        let base = claude_state();
        let mut target = other_model(&base, "sonnet");
        target.workspace = "/other".to_owned();
        let reason = "The active session cannot apply that model.".to_owned();
        assert_eq!(
            decide_transition(
                Some(&base),
                &target,
                true,
                Some(&SelectionPlan::Reject(reason.clone()))
            ),
            Transition::Reject(reason)
        );
        assert_eq!(
            decide_transition(
                Some(&base),
                &target,
                true,
                Some(&SelectionPlan::CreateWithHandoff)
            ),
            Transition::CreateWithHandoff
        );
    }

    #[test]
    fn a_missing_plan_for_a_selection_change_is_rejected() {
        let base = claude_state();
        let mut target = other_model(&base, "sonnet");
        target.workspace = "/other".to_owned();
        let out = decide_transition(Some(&base), &target, true, None);
        assert!(matches!(out, Transition::Reject(_)), "{out:?}");
    }

    #[test]
    fn a_workspace_change_alongside_a_switchable_model_restarts() {
        let base = claude_state();
        let mut target = other_model(&base, "sonnet");
        target.workspace = "/other".to_owned();
        assert_eq!(
            decide_transition(
                Some(&base),
                &target,
                true,
                Some(&SelectionPlan::ApplyOnNextTurn)
            ),
            Transition::RestartAndResume
        );
    }

    #[test]
    fn plan_selection_encodes_the_verified_agent_behaviour() {
        let claude = claude_state();
        let agy = agy_state();
        let claude_sonnet = other_model(&claude, "sonnet");
        let agy_flash = other_model(&agy, "gemini-flash");
        assert_eq!(
            plan_selection(
                &claude.capabilities,
                &claude.selection,
                &claude_sonnet.selection
            ),
            SelectionPlan::ApplyOnNextTurn
        );
        assert_eq!(
            plan_selection(&agy.capabilities, &agy.selection, &agy_flash.selection),
            SelectionPlan::RestartSession
        );
        assert_eq!(
            plan_selection(&claude.capabilities, &claude.selection, &agy.selection),
            SelectionPlan::CreateWithHandoff
        );
        assert_eq!(
            plan_selection(&agy.capabilities, &agy.selection, &claude.selection),
            SelectionPlan::CreateWithHandoff
        );
    }

    #[test]
    fn plan_and_decision_compose_for_both_agents() {
        let claude = claude_state();
        let to = other_model(&claude, "sonnet");
        let plan = plan_selection(&claude.capabilities, &claude.selection, &to.selection);
        assert_eq!(
            decide_transition(Some(&claude), &to, true, Some(&plan)),
            Transition::SwitchModelInSession
        );
        let agy = agy_state();
        let to = other_model(&agy, "gemini-flash");
        let plan = plan_selection(&agy.capabilities, &agy.selection, &to.selection);
        assert_eq!(
            decide_transition(Some(&agy), &to, true, Some(&plan)),
            Transition::RestartAndResume
        );
    }
}
