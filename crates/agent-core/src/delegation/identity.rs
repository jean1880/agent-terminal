//! A worker's model and effort, and how sure we are of each (contract §2 `Provenance`, §5
//! provenance rows).

use serde::{Deserialize, Serialize};

use super::ids::ProfileId;
use crate::adapter::Driver;

/// Where an identity value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// The provider named the effective value (`effective` is set).
    ProviderReported,
    /// The provider ran a turn with the requested value without refusing it; `effective` stays
    /// `None`. Never an identity attestation.
    ProviderAccepted,
    /// We passed it; nothing came back.
    LaunchConfigured,
    /// The provider refused the value.
    Unsupported,
    /// Nothing is known, or a value from a newer build.
    #[serde(other)]
    Unknown,
}

/// One identity value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValueProvenance {
    pub requested: Option<String>,
    pub effective: Option<String>,
    pub source: Provenance,
}

impl ValueProvenance {
    pub fn unknown() -> Self {
        Self {
            requested: None,
            effective: None,
            source: Provenance::Unknown,
        }
    }

    fn launched(value: Option<String>) -> Self {
        match value {
            Some(v) => Self {
                requested: Some(v),
                effective: None,
                source: Provenance::LaunchConfigured,
            },
            None => Self::unknown(),
        }
    }
}

/// A worker's identity (contract §2 `Identity`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub driver: Driver,
    pub profile_id: ProfileId,
    pub model: ValueProvenance,
    pub effort: ValueProvenance,
}

impl Identity {
    pub fn new(driver: Driver, profile_id: ProfileId) -> Self {
        Self {
            driver,
            profile_id,
            model: ValueProvenance::unknown(),
            effort: ValueProvenance::unknown(),
        }
    }
}

/// What the supervisor saw that bears on identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityObservation {
    /// The process was launched with these values.
    Launched {
        model: Option<String>,
        effort: Option<String>,
    },
    /// Claude `assistant.message.model` / `modelUsage` key, per turn.
    ReportedModel(String),
    /// Claude result ok / Codex turn completed / agy result ok.
    TurnSucceeded,
    TurnFailed {
        unsupported_model: bool,
        unsupported_effort: bool,
    },
}

/// How one value's provenance can advance for a driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvenanceRule {
    /// The provider names the effective value on every turn (it may change between turns).
    ReportedPerTurn,
    /// A successful turn makes it `ProviderAccepted`, with `effective` still `None`.
    AcceptedOnSuccess,
    /// It stays `LaunchConfigured`.
    ConfiguredOnly,
}

/// Claude: reported per turn; Codex and agy: accepted on success.
pub fn model_rule(d: Driver) -> ProvenanceRule {
    match d {
        Driver::Claude => ProvenanceRule::ReportedPerTurn,
        Driver::Codex | Driver::Agy => ProvenanceRule::AcceptedOnSuccess,
    }
}

/// Codex: accepted on success; Claude and agy: configured only.
pub fn effort_rule(d: Driver) -> ProvenanceRule {
    match d {
        Driver::Codex => ProvenanceRule::AcceptedOnSuccess,
        Driver::Claude | Driver::Agy => ProvenanceRule::ConfiguredOnly,
    }
}

fn accept_on_success(v: &mut ValueProvenance, rule: ProvenanceRule) {
    if rule == ProvenanceRule::AcceptedOnSuccess
        && v.requested.is_some()
        && matches!(
            v.source,
            Provenance::LaunchConfigured | Provenance::ProviderAccepted
        )
    {
        v.source = Provenance::ProviderAccepted;
        v.effective = None;
    }
}

fn refuse(v: &mut ValueProvenance) {
    v.source = Provenance::Unsupported;
    v.effective = None;
}

/// Applies one observation under the contract §5 provenance rows.
pub fn observe(id: &mut Identity, obs: IdentityObservation) {
    let model_rule = model_rule(id.driver);
    let effort_rule = effort_rule(id.driver);
    match obs {
        IdentityObservation::Launched { model, effort } => {
            id.model = ValueProvenance::launched(model);
            id.effort = ValueProvenance::launched(effort);
        }
        IdentityObservation::ReportedModel(model) => {
            if model_rule == ProvenanceRule::ReportedPerTurn {
                id.model.effective = Some(model);
                id.model.source = Provenance::ProviderReported;
            }
        }
        IdentityObservation::TurnSucceeded => {
            accept_on_success(&mut id.model, model_rule);
            accept_on_success(&mut id.effort, effort_rule);
        }
        IdentityObservation::TurnFailed {
            unsupported_model,
            unsupported_effort,
        } => {
            if unsupported_model {
                refuse(&mut id.model);
            }
            if unsupported_effort {
                refuse(&mut id.effort);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(driver: Driver) -> Identity {
        let mut id = Identity::new(driver, ProfileId::parse("p").expect("valid"));
        observe(
            &mut id,
            IdentityObservation::Launched {
                model: Some("m-1".into()),
                effort: Some("high".into()),
            },
        );
        id
    }

    #[test]
    fn claude_reports_the_model_per_turn_and_keeps_effort_configured() {
        let mut id = identity(Driver::Claude);
        assert_eq!(id.model.source, Provenance::LaunchConfigured);
        observe(
            &mut id,
            IdentityObservation::ReportedModel("claude-a".into()),
        );
        assert_eq!(id.model.effective.as_deref(), Some("claude-a"));
        assert_eq!(id.model.source, Provenance::ProviderReported);
        observe(&mut id, IdentityObservation::TurnSucceeded);
        observe(
            &mut id,
            IdentityObservation::ReportedModel("claude-b".into()),
        );
        assert_eq!(id.model.effective.as_deref(), Some("claude-b"));
        assert_eq!(id.effort.source, Provenance::LaunchConfigured);
        assert_eq!(id.effort.effective, None);
    }

    #[test]
    fn codex_accepts_both_on_success_without_an_effective_value() {
        let mut id = identity(Driver::Codex);
        observe(
            &mut id,
            IdentityObservation::ReportedModel("ignored".into()),
        );
        assert_eq!(id.model.source, Provenance::LaunchConfigured);
        observe(&mut id, IdentityObservation::TurnSucceeded);
        for v in [&id.model, &id.effort] {
            assert_eq!(v.source, Provenance::ProviderAccepted);
            assert_eq!(v.effective, None);
            assert!(v.requested.is_some());
        }
        let mut bad = identity(Driver::Codex);
        observe(
            &mut bad,
            IdentityObservation::TurnFailed {
                unsupported_model: false,
                unsupported_effort: true,
            },
        );
        assert_eq!(bad.effort.source, Provenance::Unsupported);
        assert_eq!(bad.model.source, Provenance::LaunchConfigured);
    }

    #[test]
    fn agy_accepts_the_model_and_keeps_effort_configured() {
        let mut id = identity(Driver::Agy);
        observe(&mut id, IdentityObservation::TurnSucceeded);
        assert_eq!(id.model.source, Provenance::ProviderAccepted);
        assert_eq!(id.model.effective, None);
        assert_eq!(id.effort.source, Provenance::LaunchConfigured);
    }

    #[test]
    fn launched_without_values_is_unknown_and_success_does_not_change_it() {
        for driver in Driver::ALL {
            let mut id = Identity::new(driver, ProfileId::parse("p").expect("valid"));
            observe(
                &mut id,
                IdentityObservation::Launched {
                    model: None,
                    effort: None,
                },
            );
            assert_eq!(id.model, ValueProvenance::unknown());
            assert_eq!(id.effort, ValueProvenance::unknown());
            observe(&mut id, IdentityObservation::TurnSucceeded);
            assert_eq!(id.effort.source, Provenance::Unknown);
        }
    }

    #[test]
    fn provenance_serialises_snake_case_and_tolerates_newer_values() {
        assert_eq!(
            serde_json::to_value(Provenance::ProviderAccepted).expect("ser"),
            serde_json::json!("provider_accepted")
        );
        assert_eq!(
            serde_json::from_str::<Provenance>("\"provider_attested\"").expect("de"),
            Provenance::Unknown
        );
        assert_eq!(model_rule(Driver::Claude), ProvenanceRule::ReportedPerTurn);
        assert_eq!(effort_rule(Driver::Claude), ProvenanceRule::ConfiguredOnly);
        assert_eq!(model_rule(Driver::Agy), ProvenanceRule::AcceptedOnSuccess);
        assert_eq!(
            effort_rule(Driver::Codex),
            ProvenanceRule::AcceptedOnSuccess
        );
    }
}
