//! What each model is good for, and how much of an agent's plan is left: the pure half of
//! `cross_agent_capabilities`' `models[].tags` and `usage.headroom`.
//!
//! The tag file (`model-tags.json`, contract v1) is authored elsewhere and only read here. Reading
//! is lenient: unknown fields are ignored, unknown tags kept, an entry that cannot match (no
//! `match`, a driver this build does not know) skipped. Only a file that is not a v1 object with
//! a `models` array is refused, and the caller then lists models with no tags and says why.
//! Every string is bounded here, so a hand-edited file cannot blow the tool's output budget.
//!
//! No I/O and no clock: the caller reads the file and passes the reading's age.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::adapter::Driver;
use crate::catalog::CatalogModel;
use crate::delegation::error::truncate_bytes;
use crate::event::QuotaWindow;

/// A window at or above this share used makes the agent's headroom `low`.
pub const LOW: f64 = 0.80;
/// A window at or above this share used makes the agent's headroom `exhausted`.
pub const EXHAUSTED: f64 = 0.98;

/// The only tag-file version this build reads.
pub const VERSION: u64 = 1;
/// Vocabulary entries kept (the rest are dropped).
pub const MAX_VOCABULARY: usize = 32;
/// A tag name, in bytes.
pub const MAX_TAG_BYTES: usize = 32;
/// A tag's meaning, in bytes.
pub const MAX_MEANING_BYTES: usize = 256;
/// Tags kept per entry.
pub const MAX_TAGS: usize = 16;
/// An entry's `best_for` note, in bytes.
pub const MAX_BEST_FOR_BYTES: usize = 256;
/// An entry's `tier`, in bytes.
pub const MAX_TIER_BYTES: usize = 16;
/// A `match` pattern, in bytes; a longer one is skipped rather than cut (a cut pattern would
/// match something else).
pub const MAX_MATCH_BYTES: usize = 128;
/// Entries kept, in file order.
pub const MAX_ENTRIES: usize = 512;

/// How much of an agent's plan is left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Headroom {
    Ok,
    Low,
    Exhausted,
    Unknown,
}

impl Headroom {
    pub fn as_str(self) -> &'static str {
        match self {
            Headroom::Ok => "ok",
            Headroom::Low => "low",
            Headroom::Exhausted => "exhausted",
            Headroom::Unknown => "unknown",
        }
    }
}

/// The headroom `windows` leave, read `age_s` seconds ago: `exhausted` when any window is at
/// [`EXHAUSTED`] or more, `low` at [`LOW`] or more, else `ok`. `unknown` when there is no
/// window, no age (never read, or a timestamp in the future), or the reading is older than
/// `max_age_s`. Non-finite shares are ignored.
pub fn headroom(windows: &[QuotaWindow], age_s: Option<i64>, max_age_s: i64) -> Headroom {
    match load(windows, age_s, max_age_s) {
        None => Headroom::Unknown,
        Some(u) if u >= EXHAUSTED => Headroom::Exhausted,
        Some(u) if u >= LOW => Headroom::Low,
        Some(_) => Headroom::Ok,
    }
}

/// How loaded the agent's plan is, to rank agents by: the fullest window's share used, clamped to
/// 0.0–1.0. `None` exactly when [`headroom`] is `unknown` (no finite window, no age, or a reading
/// older than `max_age_s`).
pub fn load(windows: &[QuotaWindow], age_s: Option<i64>, max_age_s: i64) -> Option<f64> {
    match age_s {
        Some(age) if (0..=max_age_s).contains(&age) => {}
        _ => return None,
    }
    windows
        .iter()
        .map(|w| w.used)
        .filter(|u| u.is_finite())
        .fold(None, |acc: Option<f64>, u| {
            Some(acc.map_or(u, |a| a.max(u)))
        })
        .map(|u| u.clamp(0.0, 1.0))
}

/// A `match` pattern: case-insensitive, `*` matches any run of characters (none included).
/// Contract v1 names the forms `id`, `pre*`, `*suf` and `*infix*`; a `*` anywhere else works the
/// same way (`gpt-6*-sol`). No other character is special.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    /// The lowercased pattern split on `*`: one piece means no wildcard.
    pieces: Vec<String>,
}

impl Pattern {
    pub fn new(pattern: &str) -> Self {
        Self {
            pieces: pattern
                .to_lowercase()
                .split('*')
                .map(str::to_owned)
                .collect(),
        }
    }

    pub fn matches(&self, id: &str) -> bool {
        let id = id.to_lowercase();
        let Some((first, rest)) = self.pieces.split_first() else {
            return false;
        };
        let Some(mut tail) = id.strip_prefix(first.as_str()) else {
            return false;
        };
        let Some((last, middle)) = rest.split_last() else {
            return tail.is_empty(); // no `*`: exact
        };
        for piece in middle {
            match tail.find(piece.as_str()) {
                Some(at) => tail = &tail[at + piece.len()..],
                None => return false,
            }
        }
        tail.ends_with(last.as_str())
    }
}

/// One `models[]` entry of the tag file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagEntry {
    pub pattern: Pattern,
    /// `None`: any driver.
    pub driver: Option<Driver>,
    pub tier: Option<String>,
    pub tags: Vec<String>,
    pub best_for: Option<String>,
}

/// A read tag file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelTags {
    /// Tag → what it means, for callers choosing a model.
    pub vocabulary: BTreeMap<String, String>,
    /// In file order: the first match wins.
    pub entries: Vec<TagEntry>,
    /// Which effort each tier should spawn with; `None` when the file has none (or no ladder).
    pub effort_policy: Option<EffortPolicy>,
}

/// Effort names kept in a ladder or the reserved list.
pub const MAX_LADDER: usize = 16;
/// An effort name, in bytes.
pub const MAX_EFFORT_NAME_BYTES: usize = 32;

/// The tag file's optional `effort_policy`: the effort scale from least to most, the effort each
/// tier should start on, and the efforts only chosen on purpose (they multiply quota burn).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffortPolicy {
    pub ladder: Vec<String>,
    pub fast: Option<String>,
    pub standard: Option<String>,
    pub deep: Option<String>,
    pub reserved: Vec<String>,
}

/// One model's effort per tier under the policy ([`effort_for`]); `None` when it has no efforts
/// or the policy names none for that tier.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffortFor {
    pub fast: Option<String>,
    pub standard: Option<String>,
    pub deep: Option<String>,
}

/// `wanted` as `offered` allows it, among the offered efforts that are not reserved: `wanted`
/// itself; else the highest one below it on the ladder; else the lowest one (by the ladder, then
/// the model's own order for efforts off the ladder). `None` when nothing is offered, or only
/// reserved efforts are: a reserved effort is never recommended, even when the policy names one.
pub fn clamp_effort(policy: &EffortPolicy, wanted: &str, offered: &[String]) -> Option<String> {
    let same = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    let usable: Vec<&String> = offered
        .iter()
        .filter(|e| !policy.reserved.iter().any(|r| same(r, e)))
        .collect();
    if let Some(e) = usable.iter().find(|e| same(e, wanted)) {
        return Some((*e).clone());
    }
    let rank = |e: &str| policy.ladder.iter().position(|l| same(l, e));
    if let Some(top) = rank(wanted) {
        let below = usable
            .iter()
            .filter_map(|e| rank(e).filter(|r| *r < top).map(|r| (r, *e)))
            .max_by_key(|(r, _)| *r);
        if let Some((_, e)) = below {
            return Some(e.clone());
        }
    }
    usable
        .iter()
        .filter_map(|e| rank(e).map(|r| (r, *e)))
        .min_by_key(|(r, _)| *r)
        .map(|(_, e)| e.clone())
        .or_else(|| usable.first().map(|e| (*e).clone()))
}

/// The effort each tier should spawn `offered`'s model with under `policy`; `None` when no tier
/// gets one (the model offers no effort, or none it may be given).
pub fn effort_for(policy: &EffortPolicy, offered: &[String]) -> Option<EffortFor> {
    let tier = |wanted: &Option<String>| {
        wanted
            .as_deref()
            .and_then(|w| clamp_effort(policy, w, offered))
    };
    let f = EffortFor {
        fast: tier(&policy.fast),
        standard: tier(&policy.standard),
        deep: tier(&policy.deep),
    };
    (f.fast.is_some() || f.standard.is_some() || f.deep.is_some()).then_some(f)
}

fn effort_names(v: Option<&Value>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in v
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|e| !e.is_empty() && e.len() <= MAX_EFFORT_NAME_BYTES)
    {
        if !out.iter().any(|o| o == e) && out.len() < MAX_LADDER {
            out.push(e.to_owned());
        }
    }
    out
}

fn effort_policy(v: &Value) -> Option<EffortPolicy> {
    let p = v.get("effort_policy")?;
    let ladder = effort_names(p.get("ladder"));
    if ladder.is_empty() {
        return None;
    }
    let level = |k: &str| {
        p.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|e| !e.is_empty() && e.len() <= MAX_EFFORT_NAME_BYTES)
            .map(str::to_owned)
    };
    Some(EffortPolicy {
        fast: level("fast"),
        standard: level("standard"),
        deep: level("deep"),
        reserved: effort_names(p.get("reserved")),
        ladder,
    })
}

/// `s` cut to at most `max` bytes on a character boundary, owned.
fn cut(s: &str, max: usize) -> String {
    truncate_bytes(s, max).to_owned()
}

fn text(v: &Value, key: &str, max: usize) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| cut(s, max))
}

fn driver_of(name: &str) -> Option<Driver> {
    serde_json::from_value(Value::String(name.to_owned())).ok()
}

fn entry(v: &Value) -> Option<TagEntry> {
    let pattern = v
        .get("match")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty() && p.len() <= MAX_MATCH_BYTES)?;
    let driver = match v.get("driver") {
        None | Some(Value::Null) => None,
        // A driver this build does not know can match nothing here: skip the entry, never widen
        // it to every driver.
        Some(Value::String(d)) => Some(driver_of(d)?),
        Some(_) => return None,
    };
    let mut tags: Vec<String> = Vec::new();
    for t in v
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        let t = cut(t, MAX_TAG_BYTES);
        if !tags.contains(&t) && tags.len() < MAX_TAGS {
            tags.push(t);
        }
    }
    Some(TagEntry {
        pattern: Pattern::new(pattern),
        driver,
        tier: text(v, "tier", MAX_TIER_BYTES),
        tags,
        best_for: text(v, "best_for", MAX_BEST_FOR_BYTES),
    })
}

/// Reads a tag file. `Err` says why it was refused: not JSON, not an object, another version,
/// or no `models` array.
pub fn parse(json: &str) -> Result<ModelTags, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("not valid JSON: {e}"))?;
    if !v.is_object() {
        return Err("not a JSON object".into());
    }
    match v.get("version").and_then(Value::as_u64) {
        Some(VERSION) => {}
        Some(other) => return Err(format!("unsupported version {other} (expected {VERSION})")),
        None => return Err("missing version".into()),
    }
    let models = v
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| String::from("missing models array"))?;
    let vocabulary = v
        .get("vocabulary")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(tag, meaning)| {
            let tag = tag.trim();
            (!tag.is_empty()).then(|| {
                (
                    cut(tag, MAX_TAG_BYTES),
                    cut(
                        meaning.as_str().unwrap_or_default().trim(),
                        MAX_MEANING_BYTES,
                    ),
                )
            })
        })
        .take(MAX_VOCABULARY)
        .collect();
    Ok(ModelTags {
        vocabulary,
        entries: models.iter().take(MAX_ENTRIES).filter_map(entry).collect(),
        effort_policy: effort_policy(&v),
    })
}

impl ModelTags {
    /// The first entry, in file order, whose pattern matches `id` and whose driver (if any) is
    /// `driver`.
    pub fn lookup(&self, driver: Driver, id: &str) -> Option<&TagEntry> {
        self.entries
            .iter()
            .find(|e| e.driver.is_none_or(|d| d == driver) && e.pattern.matches(id))
    }

    /// [`Self::lookup`] for a catalogue model. An alias with no entry of its own (Claude's
    /// `default`) is looked up as the model its description names first ("Sonnet 5.5 · …").
    pub fn lookup_model(&self, model: &CatalogModel) -> Option<&TagEntry> {
        self.lookup(model.driver, &model.id).or_else(|| {
            if model.id != "default" {
                return None;
            }
            let head = model.description.as_deref()?.split('·').next()?.trim();
            (!head.is_empty())
                .then(|| self.lookup(model.driver, head))
                .flatten()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(used: f64) -> QuotaWindow {
        QuotaWindow {
            group: None,
            label: "5-hour".into(),
            used,
            resets_at: None,
        }
    }

    const MAX_AGE: i64 = 1200;

    #[test]
    fn headroom_follows_the_fullest_window_and_its_thresholds() {
        let at = |used: &[f64]| {
            let windows: Vec<_> = used.iter().map(|u| win(*u)).collect();
            headroom(&windows, Some(0), MAX_AGE)
        };
        assert_eq!(at(&[0.0]), Headroom::Ok);
        assert_eq!(at(&[0.79]), Headroom::Ok);
        assert_eq!(at(&[0.80]), Headroom::Low);
        assert_eq!(at(&[0.2, 0.97]), Headroom::Low);
        assert_eq!(at(&[0.98]), Headroom::Exhausted);
        assert_eq!(at(&[0.1, 1.0]), Headroom::Exhausted);
        assert_eq!(at(&[]), Headroom::Unknown, "no window");
        assert_eq!(at(&[f64::NAN]), Headroom::Unknown, "nothing finite");
        assert_eq!(at(&[f64::NAN, 0.5]), Headroom::Ok);
    }

    #[test]
    fn load_is_the_fullest_window_and_none_when_headroom_is_unknown() {
        let ws = |used: &[f64]| used.iter().map(|u| win(*u)).collect::<Vec<_>>();
        assert_eq!(load(&ws(&[0.2, 0.55, 0.1]), Some(5), MAX_AGE), Some(0.55));
        assert_eq!(load(&ws(&[1.7]), Some(5), MAX_AGE), Some(1.0), "clamped");
        assert_eq!(load(&ws(&[-0.5]), Some(5), MAX_AGE), Some(0.0), "clamped");
        assert_eq!(load(&ws(&[]), Some(5), MAX_AGE), None);
        assert_eq!(load(&ws(&[f64::INFINITY]), Some(5), MAX_AGE), None);
        assert_eq!(load(&ws(&[0.3]), Some(MAX_AGE + 1), MAX_AGE), None, "stale");
        assert_eq!(load(&ws(&[0.3]), None, MAX_AGE), None);
        assert_eq!(load(&ws(&[0.3]), Some(-1), MAX_AGE), None, "negative age");
        assert_eq!(load(&ws(&[0.3]), Some(0), MAX_AGE), Some(0.3));
        assert_eq!(headroom(&ws(&[0.3]), Some(-5), MAX_AGE), Headroom::Unknown);
        // Ranks agents where headroom cannot: both are `ok`.
        let (a, b) = (ws(&[0.10]), ws(&[0.60]));
        assert_eq!(
            headroom(&a, Some(0), MAX_AGE),
            headroom(&b, Some(0), MAX_AGE)
        );
        assert!(load(&a, Some(0), MAX_AGE) < load(&b, Some(0), MAX_AGE));
        for used in [0.0, 0.5, 0.8, 0.99] {
            let w = ws(&[used]);
            assert_eq!(
                load(&w, Some(0), MAX_AGE).is_none(),
                headroom(&w, Some(0), MAX_AGE) == Headroom::Unknown
            );
        }
    }

    #[test]
    fn an_old_or_missing_reading_is_unknown() {
        let w = [win(0.99)];
        assert_eq!(headroom(&w, Some(MAX_AGE), MAX_AGE), Headroom::Exhausted);
        assert_eq!(headroom(&w, Some(MAX_AGE + 1), MAX_AGE), Headroom::Unknown);
        assert_eq!(headroom(&w, None, MAX_AGE), Headroom::Unknown);
        assert_eq!(
            serde_json::to_value(Headroom::Exhausted).expect("ser"),
            "exhausted"
        );
        for h in [
            Headroom::Ok,
            Headroom::Low,
            Headroom::Exhausted,
            Headroom::Unknown,
        ] {
            assert_eq!(serde_json::to_value(h).expect("ser"), h.as_str());
        }
    }

    #[test]
    fn patterns_cover_the_contract_forms_case_insensitively() {
        let m = |p: &str, id: &str| Pattern::new(p).matches(id);
        assert!(m("gemini-4-argon", "Gemini-4-Argon"));
        assert!(!m("gemini-4-argon", "gemini-4-argon-x"), "exact is exact");
        assert!(m("gpt-oss-*", "gpt-oss-120b"));
        assert!(!m("gpt-oss-*", "x-gpt-oss-120b"));
        assert!(m("*-sol", "gpt-6.1-SOL"));
        assert!(!m("*-sol", "gpt-6-sol-x"));
        assert!(m("*opus*", "claude-opus-5-5"));
        assert!(m("*opus*", "opus"), "an empty run matches");
        assert!(!m("*opus*", "sonnet"));
        assert!(m("gpt-6*-sol", "gpt-6.1-sol"));
        assert!(m("gpt-6*-sol", "gpt-6-sol"));
        assert!(!m("gpt-6*-sol", "gpt-6-astra"));
        assert!(m("gemini-*flash*", "gemini-3.8-flash"));
        assert!(!m("a*a", "a"), "pieces never overlap");
        assert!(m("a*a", "aa"));
        assert!(m("*", "anything"));
        assert!(m("é*", "É-model"), "multi-byte lowercase");
        assert!(m("ab?", "ab?"), "only * is special");
        assert!(!m("ab?", "abc"));
    }

    fn file() -> String {
        serde_json::json!({
            "version": 1,
            "vocabulary": {"code": "writes code", "fast": "low latency", "": "dropped"},
            "future_field": true,
            "models": [
                {"match": "*opus*", "driver": "agy", "tier": "standard", "tags": ["code"]},
                {"match": "*opus*", "driver": null, "tier": "deep",
                 "tags": ["deep-reasoning", "code", "not-in-vocabulary", "code", 7],
                 "best_for": "design, hard bugs"},
                {"match": "*sonnet*", "tags": ["code"]},
                {"match": "gpt-*", "driver": "some-future-agent", "tags": ["x"]},
                {"tags": ["no-match-field"]},
                {"match": "*luna*", "driver": "codex", "tags": ["fast"], "extra": {}}
            ]
        })
        .to_string()
    }

    #[test]
    fn the_first_match_wins_and_a_driver_narrows_an_entry() {
        let tags = parse(&file()).expect("parse");
        let opus_agy = tags.lookup(Driver::Agy, "claude-opus-5-5").expect("agy");
        assert_eq!(opus_agy.tier.as_deref(), Some("standard"));
        let opus = tags.lookup(Driver::Claude, "opus").expect("claude");
        assert_eq!(opus.tier.as_deref(), Some("deep"));
        assert_eq!(
            opus.tags,
            ["deep-reasoning", "code", "not-in-vocabulary"],
            "unknown tags kept, duplicates and non-strings dropped"
        );
        assert_eq!(opus.best_for.as_deref(), Some("design, hard bugs"));
        assert!(tags.lookup(Driver::Claude, "gpt-6-luna").is_none());
        assert!(tags.lookup(Driver::Codex, "gpt-6-luna").is_some());
        assert!(
            tags.lookup(Driver::Codex, "gpt-5").is_none(),
            "an unknown driver never widens to any"
        );
        assert_eq!(tags.entries.len(), 4, "unmatchable entries skipped");
        assert_eq!(tags.vocabulary.len(), 2);
        assert_eq!(tags.vocabulary["code"], "writes code");
    }

    #[test]
    fn the_default_alias_is_looked_up_as_the_model_it_names() {
        let tags = parse(&file()).expect("parse");
        let model = |id: &str, description: Option<&str>| CatalogModel {
            driver: Driver::Claude,
            id: id.into(),
            display: "Default (recommended)".into(),
            description: description.map(str::to_owned),
            efforts: vec![],
            default_effort: None,
            via: None,
        };
        let default = model("default", Some("Sonnet 5.5 · Efficient for routine tasks"));
        assert_eq!(
            tags.lookup_model(&default).map(|e| e.tags.clone()),
            Some(vec!["code".to_owned()])
        );
        assert!(tags.lookup_model(&model("default", None)).is_none());
        // Only the alias: another model's description never decides its tags.
        assert!(tags
            .lookup_model(&model("mystery", Some("Opus-like")))
            .is_none());
    }

    fn policy() -> EffortPolicy {
        let s = |v: &[&str]| v.iter().map(|e| (*e).to_owned()).collect::<Vec<_>>();
        EffortPolicy {
            ladder: s(&["minimal", "low", "medium", "high", "xhigh", "max", "ultra"]),
            fast: Some("low".into()),
            standard: Some("medium".into()),
            deep: Some("high".into()),
            reserved: s(&["xhigh", "max", "ultra"]),
        }
    }

    fn efforts(v: &[&str]) -> Vec<String> {
        v.iter().map(|e| (*e).to_owned()).collect()
    }

    fn tiers(offered: &[&str]) -> [Option<String>; 3] {
        let f = effort_for(&policy(), &efforts(offered)).unwrap_or_default();
        [f.fast, f.standard, f.deep]
    }

    fn some(v: &str) -> Option<String> {
        Some(v.to_owned())
    }

    #[test]
    fn each_tier_gets_its_effort_clamped_to_what_the_model_offers() {
        // Codex, whose default_effort is "low": every tier is offered as is.
        assert_eq!(
            tiers(&["low", "medium", "high", "xhigh"]),
            [some("low"), some("medium"), some("high")]
        );
        // A Codex list that goes up to "ultra": reserved efforts are offered but never picked.
        assert_eq!(
            tiers(&["medium", "high", "xhigh", "max", "ultra"]),
            [some("medium"), some("medium"), some("high")],
            "no effort below low: the lowest offered, not a reserved one"
        );
        // agy: low..high.
        assert_eq!(
            tiers(&["low", "medium", "high"]),
            [some("low"), some("medium"), some("high")]
        );
        // gpt-oss: medium only.
        assert_eq!(
            tiers(&["medium"]),
            [some("medium"), some("medium"), some("medium")]
        );
        // Nothing offered: nothing to pass.
        assert_eq!(tiers(&[]), [None, None, None]);
        // Below the wanted step: the highest offered under it.
        assert_eq!(
            tiers(&["minimal", "low", "max"]),
            [some("low"), some("low"), some("low")]
        );
        // Only reserved efforts: clamping never reaches for one.
        assert_eq!(tiers(&["xhigh", "max"]), [None, None, None]);
        // Off the ladder: the model's own first (non-reserved) effort as the last resort.
        assert_eq!(
            tiers(&["turbo"]),
            [some("turbo"), some("turbo"), some("turbo")]
        );
        // Case-insensitive, the model's spelling kept.
        assert_eq!(
            clamp_effort(&policy(), "HIGH", &efforts(&["High"])),
            some("High")
        );
    }

    /// No effort for any tier is `None`, not an object of nulls.
    #[test]
    fn a_model_with_no_eligible_effort_has_no_effort_for() {
        assert_eq!(effort_for(&policy(), &[]), None);
        assert_eq!(effort_for(&policy(), &efforts(&["xhigh", "ultra"])), None);
        let no_tiers = EffortPolicy {
            fast: None,
            standard: None,
            deep: None,
            ..policy()
        };
        assert_eq!(effort_for(&no_tiers, &efforts(&["low"])), None);
        let one = EffortPolicy {
            fast: None,
            standard: None,
            ..policy()
        };
        assert_eq!(
            effort_for(&one, &efforts(&["high"])),
            Some(EffortFor {
                fast: None,
                standard: None,
                deep: some("high"),
            })
        );
    }

    /// A reserved effort is never recommended, even when the policy names it for a tier and the
    /// model offers it exactly.
    #[test]
    fn a_reserved_effort_named_by_the_policy_is_still_never_recommended() {
        let greedy = EffortPolicy {
            deep: some("xhigh"),
            ..policy()
        };
        assert_eq!(
            clamp_effort(&greedy, "xhigh", &efforts(&["low", "high", "xhigh"])),
            some("high")
        );
        assert_eq!(
            clamp_effort(&greedy, "XHIGH", &efforts(&["XHigh"])),
            None,
            "case does not get one past"
        );
        let f =
            effort_for(&greedy, &efforts(&["medium", "high", "xhigh", "max"])).expect("some tier");
        assert_eq!(f.deep, some("high"));
    }

    #[test]
    fn the_effort_policy_is_read_leniently() {
        let tags = parse(
            r#"{"version": 1, "models": [], "effort_policy": {
                "ladder": ["minimal", "low", "medium", "high", "xhigh", "max", "ultra", "low", 3],
                "fast": "low", "standard": "medium", "deep": "high",
                "reserved": ["xhigh", "max", "ultra"], "future": 1}}"#,
        )
        .expect("parse");
        assert_eq!(
            tags.effort_policy,
            Some(policy()),
            "duplicates and non-strings dropped"
        );
        let none = parse(r#"{"version": 1, "models": [], "effort_policy": {"fast": "low"}}"#)
            .expect("parse");
        assert_eq!(none.effort_policy, None, "no ladder, no policy");
        assert_eq!(parse(&file()).expect("parse").effort_policy, None);
    }

    #[test]
    fn a_file_that_is_not_v1_is_refused_with_a_reason() {
        assert!(parse("not json").unwrap_err().starts_with("not valid JSON"));
        assert_eq!(parse("[]").unwrap_err(), "not a JSON object");
        assert_eq!(parse(r#"{"models": []}"#).unwrap_err(), "missing version");
        assert_eq!(
            parse(r#"{"version": 2, "models": []}"#).unwrap_err(),
            "unsupported version 2 (expected 1)"
        );
        assert_eq!(
            parse(r#"{"version": 1}"#).unwrap_err(),
            "missing models array"
        );
        let empty = parse(r#"{"version": 1, "models": []}"#).expect("empty is fine");
        assert!(empty.vocabulary.is_empty() && empty.entries.is_empty());
    }

    #[test]
    fn every_string_is_bounded_on_a_character_boundary() {
        let long = "é".repeat(1000);
        let json = serde_json::json!({
            "version": 1,
            "vocabulary": (0..100).map(|i| (format!("{i}{long}"), Value::String(long.clone())))
                .collect::<serde_json::Map<_, _>>(),
            "models": [
                {"match": "x".repeat(MAX_MATCH_BYTES + 1), "tags": ["a"]},
                {"match": "*", "tier": long, "best_for": long,
                 "tags": (0..100).map(|i| format!("{i}{long}")).collect::<Vec<_>>()}
            ]
        })
        .to_string();
        let tags = parse(&json).expect("parse");
        assert_eq!(tags.vocabulary.len(), MAX_VOCABULARY);
        for (k, v) in &tags.vocabulary {
            assert!(k.len() <= MAX_TAG_BYTES && v.len() <= MAX_MEANING_BYTES);
        }
        assert_eq!(tags.entries.len(), 1, "an over-long pattern is skipped");
        let e = &tags.entries[0];
        assert_eq!(e.tags.len(), MAX_TAGS);
        assert!(e.tags.iter().all(|t| t.len() <= MAX_TAG_BYTES));
        assert!(e.tier.as_ref().is_some_and(|t| t.len() <= MAX_TIER_BYTES));
        assert!(e
            .best_for
            .as_ref()
            .is_some_and(|t| t.len() <= MAX_BEST_FOR_BYTES));
    }
}
