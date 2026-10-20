//! `cross_agent_capabilities`' models and usage, from the app's catalogue, tag file and account
//! status. Pure: everything is passed in.

use agent_core::catalog::CatalogModel;
use agent_core::delegation::error::truncate_bytes;
use agent_core::delegation::wire::{
    ModelInfo, UsageInfo, UsageWindow, MAX_EFFORT_BYTES, MAX_MODEL_BYTES, MAX_USAGE_WINDOWS,
};
use agent_core::model_tags::{effort_for, ModelTags};

use crate::account_status::Snapshot;
use crate::freshness::age_of;
use crate::usage_file::{headroom_of, load_of};

/// Models listed per profile (the output schema's bound).
pub(super) const MAX_MODELS: usize = 64;
/// Efforts listed per model (the output schema's bound).
const MAX_EFFORTS: usize = 8;

/// One catalogue model as listed, with its tags when the tag file has an entry for it. A model
/// whose id is too long to spawn is not listed; an effort too long to pass is left out.
pub(super) fn model_info(model: &CatalogModel, tags: Option<&ModelTags>) -> Option<ModelInfo> {
    if model.id.is_empty() || model.id.len() > MAX_MODEL_BYTES {
        return None;
    }
    let entry = tags.and_then(|t| t.lookup_model(model));
    let effort_ok = |e: &&String| !e.is_empty() && e.len() <= MAX_EFFORT_BYTES;
    let efforts: Vec<String> = model
        .efforts
        .iter()
        .filter(effort_ok)
        .take(MAX_EFFORTS)
        .cloned()
        .collect();
    Some(ModelInfo {
        id: model.id.clone(),
        display: truncate_bytes(&model.display, MAX_MODEL_BYTES).to_owned(),
        effort_for: tags
            .and_then(|t| t.effort_policy.as_ref())
            .and_then(|p| effort_for(p, &efforts)),
        efforts,
        default_effort: model.default_effort.as_ref().filter(effort_ok).cloned(),
        tags: entry.map(|e| e.tags.clone()).unwrap_or_default(),
        best_for: entry.and_then(|e| e.best_for.clone()),
        tier: entry.and_then(|e| e.tier.clone()),
    })
}

/// The model id an agy child is launched with: a catalogue model listed with efforts gets its
/// effort composed into the id ([`CatalogModel::model_id_for`]: `gemini-4-argon` at `high` is
/// `gemini-4-argon-high`; an effort it does not offer falls back to its default), since agy
/// refuses a separate `--effort` for such a model. Any other id (already composed, or not in the
/// catalogue) is passed as given.
pub(super) fn agy_launch_model(
    models: &[CatalogModel],
    model: Option<String>,
    effort: Option<&str>,
) -> Option<String> {
    let id = model?;
    Some(
        models
            .iter()
            .find(|m| m.id == id && !m.efforts.is_empty())
            .map_or(id.clone(), |m| m.model_id_for(effort)),
    )
}

/// An agent's usage as listed at `now`: windows, headroom, load and age. Never the account.
pub(super) fn usage_info(snap: &Snapshot, now: i64) -> UsageInfo {
    UsageInfo {
        headroom: headroom_of(snap, now),
        load: load_of(snap, now),
        windows: snap
            .windows
            .iter()
            .take(MAX_USAGE_WINDOWS)
            .map(|w| {
                UsageWindow::bounded(w.group.as_deref(), &w.label, w.used, w.resets_at.as_deref())
            })
            .collect(),
        age_s: age_of(snap.read_at, now),
        refresh_failed: snap.refresh_failed,
        signed_out: snap.signed_out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::adapter::Driver;
    use agent_core::event::{Account, QuotaWindow};
    use agent_core::model_tags::{parse, Headroom};

    fn model(driver: Driver, id: &str) -> CatalogModel {
        CatalogModel {
            driver,
            id: id.into(),
            display: format!("{id} display"),
            description: None,
            efforts: vec![
                "low".into(),
                "x".repeat(MAX_EFFORT_BYTES + 1),
                "high".into(),
            ],
            default_effort: Some("high".into()),
            via: None,
        }
    }

    #[test]
    fn a_model_carries_its_tags_or_none() {
        let tags = parse(
            r#"{"version": 1, "models": [
                {"match": "*opus*", "tier": "deep", "tags": ["code", "review"],
                 "best_for": "hard bugs"}],
               "effort_policy": {"ladder": ["low", "medium", "high", "max"],
                                 "fast": "low", "standard": "medium", "deep": "max",
                                 "reserved": ["max"]}}"#,
        )
        .expect("tags");
        let opus = model_info(&model(Driver::Agy, "claude-opus-5-5"), Some(&tags)).expect("ok");
        assert_eq!(opus.tags, ["code", "review"]);
        assert_eq!(opus.tier.as_deref(), Some("deep"));
        assert_eq!(opus.best_for.as_deref(), Some("hard bugs"));
        assert_eq!(opus.display, "claude-opus-5-5 display");
        assert_eq!(
            opus.efforts,
            ["low", "high"],
            "an unpassable effort is left out"
        );
        assert_eq!(opus.default_effort.as_deref(), Some("high"));
        let effort = opus.effort_for.expect("a policy");
        assert_eq!(effort.fast.as_deref(), Some("low"));
        assert_eq!(
            effort.standard.as_deref(),
            Some("low"),
            "medium not offered"
        );
        assert_eq!(effort.deep.as_deref(), Some("high"), "max not offered");
        let other = model_info(&model(Driver::Codex, "gpt-6-luna"), Some(&tags)).expect("ok");
        assert!(other.tags.is_empty() && other.tier.is_none() && other.best_for.is_none());
        let untagged = model_info(&model(Driver::Claude, "opus"), None).expect("ok");
        assert!(untagged.tags.is_empty(), "no tag file: listed, no tags");
        assert_eq!(untagged.effort_for, None);
        // A policy, but a model with no effort: null, not an object of nulls.
        let mut bare = model(Driver::Agy, "claude-opus-5-5");
        bare.efforts.clear();
        let bare = model_info(&bare, Some(&tags)).expect("ok");
        assert_eq!(bare.effort_for, None);
        let v = serde_json::to_value(&bare).expect("ser");
        assert!(v["effort_for"].is_null());
        assert!(model_info(&model(Driver::Claude, &"m".repeat(129)), None).is_none());
    }

    /// agy refuses `--effort` for its catalogue models: the effort goes into the id.
    #[test]
    fn an_agy_spawn_composes_its_effort_into_the_model_id() {
        let argon = CatalogModel {
            driver: Driver::Agy,
            id: "gemini-4-argon".into(),
            display: "Gemini 4 Argon".into(),
            description: None,
            efforts: vec!["low".into(), "high".into()],
            default_effort: None,
            via: None,
        };
        let oss = CatalogModel {
            id: "gpt-oss-120b".into(),
            efforts: vec![],
            ..argon.clone()
        };
        let models = [argon.clone(), oss];
        let launch = |model: Option<&str>, effort: Option<&str>| {
            agy_launch_model(&models, model.map(str::to_owned), effort)
        };
        assert_eq!(
            launch(Some("gemini-4-argon"), Some("high")).as_deref(),
            Some("gemini-4-argon-high")
        );
        assert_eq!(
            launch(Some("gemini-4-argon"), Some("low")).as_deref(),
            Some("gemini-4-argon-low")
        );
        // No effort, or one not offered: the model's own default effort.
        let default = argon.model_id_for(None);
        assert_eq!(launch(Some("gemini-4-argon"), None), Some(default.clone()));
        assert_eq!(launch(Some("gemini-4-argon"), Some("max")), Some(default));
        // Already composed, effortless or unknown: as given.
        assert_eq!(
            launch(Some("gemini-4-argon-high"), Some("low")).as_deref(),
            Some("gemini-4-argon-high")
        );
        assert_eq!(
            launch(Some("gpt-oss-120b"), Some("high")).as_deref(),
            Some("gpt-oss-120b")
        );
        assert_eq!(
            launch(Some("mystery"), Some("high")).as_deref(),
            Some("mystery")
        );
        assert_eq!(launch(None, Some("high")), None);
    }

    #[test]
    fn usage_never_carries_the_account() {
        let now = 1_800_000_000;
        let snap = Snapshot {
            account: Some(Account {
                label: "person@example.test".into(),
                plan: Some("Max".into()),
                provider: None,
            }),
            windows: vec![
                QuotaWindow {
                    group: Some("Gemini".into()),
                    label: "Weekly".into(),
                    used: 0.9,
                    resets_at: None,
                };
                MAX_USAGE_WINDOWS + 3
            ],
            read_at: Some(now - 30),
            ..Snapshot::default()
        };
        let usage = usage_info(&snap, now);
        assert_eq!(usage.headroom, Headroom::Low);
        assert_eq!(usage.load, Some(0.9));
        assert_eq!(usage.age_s, Some(30));
        assert_eq!(usage.windows.len(), MAX_USAGE_WINDOWS);
        let text = serde_json::to_string(&usage).expect("ser");
        assert!(!text.contains("person@example.test") && !text.contains("account"));
        let never = usage_info(&Snapshot::default(), now);
        assert_eq!(never.headroom, Headroom::Unknown);
        assert_eq!(never.load, None);
        assert!(serde_json::to_string(&never)
            .expect("ser")
            .contains(r#""load":null"#));
    }
}
