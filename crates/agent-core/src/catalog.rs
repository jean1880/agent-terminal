//! The model catalog: every model either agent offers, in one provider-neutral list.
//!
//! Claude reports its models in the `initialize` control response (`models[]`); agy prints them
//! with `agy models` as `id<TAB>Display` lines. Both parsers are pure and tolerant: unknown
//! shapes and garbage lines yield fewer entries, never an error. The picker groups the merged
//! list with [`group_for_picker`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::adapter::Driver;

/// The route label for third-party models that agy lists (`claude-*`, `gpt-*`).
pub const VIA_ANTIGRAVITY: &str = "Antigravity";

/// One selectable model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogModel {
    /// The agent that serves it (the one `switch` must target).
    pub driver: Driver,
    /// The value passed to `--model` / `set_model`.
    pub id: String,
    pub display: String,
    pub description: Option<String>,
    /// Effort levels the model supports; empty when it has none.
    pub efforts: Vec<String>,
    /// The effort the agent itself starts the model on (Codex reports one); `None` leaves the
    /// choice to [`CatalogModel::default_effort`]'s own rule.
    #[serde(default)]
    pub default_effort: Option<String>,
    /// Set when the model is reached through another vendor's route, e.g. `Antigravity` for a
    /// `claude-*` model listed by agy.
    pub via: Option<String>,
}

/// Finds the `models` array in an `initialize` control response, whether given as the bare
/// payload, the `{response: {...}}` envelope, or the whole recorded frame.
fn models_array(v: &Value) -> Option<&Vec<Value>> {
    let mut cur = v;
    for _ in 0..4 {
        if let Some(a) = cur.get("models").and_then(Value::as_array) {
            return Some(a);
        }
        cur = cur.get("response")?;
    }
    None
}

/// Claude's models from the `initialize` control response payload.
pub fn parse_claude_initialize(response_json: &Value) -> Vec<CatalogModel> {
    let Some(models) = models_array(response_json) else {
        return Vec::new();
    };
    models
        .iter()
        .filter_map(|m| {
            let id = m.get("value")?.as_str()?.trim();
            if id.is_empty() {
                return None;
            }
            let text = |k: &str| {
                m.get(k)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            };
            let efforts = m
                .get("supportedEffortLevels")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            Some(CatalogModel {
                driver: Driver::Claude,
                id: id.to_owned(),
                display: text("displayName").unwrap_or_else(|| id.to_owned()),
                description: text("description"),
                efforts,
                default_effort: None,
                via: None,
            })
        })
        .collect()
}

/// Codex's models from the `model/list` result (`{data: [...]}`; hidden models are left out).
/// Efforts are `supportedReasoningEfforts` in the server's order and `default_effort` is the one
/// the model starts on. The id is the `model` slug `turn/start` takes.
pub fn parse_codex_models(result: &Value) -> Vec<CatalogModel> {
    let mut models = crate::codex::parse_codex_models(result);
    // The server's default first (stable otherwise), so "the agent's default model" is the head
    // of the list for [`suggest_replacement`].
    models.sort_by_key(|m| !m.is_default);
    models
        .into_iter()
        .map(|m| CatalogModel {
            driver: Driver::Codex,
            display: if m.display.trim().is_empty() {
                m.id.clone()
            } else {
                m.display
            },
            id: m.id,
            description: Some(m.description).filter(|d| !d.trim().is_empty()),
            efforts: m.efforts,
            default_effort: m.default_effort,
            via: None,
        })
        .collect()
}

/// Effort levels from lowest to highest; the order dropdowns list them in.
pub const EFFORT_ORDER: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

fn effort_rank(effort: &str) -> usize {
    EFFORT_ORDER
        .iter()
        .position(|e| *e == effort)
        .unwrap_or(EFFORT_ORDER.len())
}

/// Splits an agy id into (base, effort) when it ends in `-<effort>`.
fn split_effort(id: &str) -> Option<(&str, &'static str)> {
    EFFORT_ORDER.iter().find_map(|e| {
        let base = id.strip_suffix(e)?.strip_suffix('-')?;
        (!base.is_empty()).then_some((base, *e))
    })
}

/// `Gemini 3.1 Pro (High)` -> `Gemini 3.1 Pro` when the parenthetical is the effort.
fn strip_effort_label(display: &str, effort: &str) -> String {
    let tail = format!("({})", effort);
    let display = display.trim();
    let lower = display.to_lowercase();
    lower
        .strip_suffix(&tail)
        .and_then(|head| display.get(..head.len()))
        .map_or_else(|| display.to_owned(), |head| head.trim_end().to_owned())
}

impl CatalogModel {
    /// The effort a row starts on when nothing says otherwise: `medium` if offered, else the
    /// first (lowest) one; `None` when the model has no efforts.
    pub fn default_effort(&self) -> Option<&str> {
        if let Some(own) = self.default_effort.as_deref() {
            if self.efforts.iter().any(|e| e == own) {
                return Some(own);
            }
        }
        if self.efforts.iter().any(|e| e == "medium") {
            Some("medium")
        } else {
            self.efforts.first().map(String::as_str)
        }
    }

    /// The value to pass as the model for `effort` (see [`Self::default_effort`] when `None`).
    /// agy encodes effort in the id (`<base>-<effort>`); Claude takes it separately, so its id
    /// is returned unchanged. An effort the model does not offer falls back to the default.
    pub fn model_id_for(&self, effort: Option<&str>) -> String {
        if self.driver != Driver::Agy || self.efforts.is_empty() {
            return self.id.clone();
        }
        let chosen = effort
            .filter(|e| self.efforts.iter().any(|x| x == e))
            .or_else(|| self.default_effort());
        match chosen {
            Some(e) => format!("{}-{e}", self.id),
            None => self.id.clone(),
        }
    }

    /// Whether `model_id` (what a thread reports as its model) names this model at any effort.
    pub fn is_model(&self, model_id: &str) -> bool {
        model_id == self.id || self.effort_in(model_id).is_some()
    }

    /// The effort encoded in `model_id` when it is this (agy) model at one of its efforts.
    pub fn effort_in(&self, model_id: &str) -> Option<&str> {
        if self.driver != Driver::Agy {
            return None;
        }
        let (base, effort) = split_effort(model_id)?;
        (base == self.id)
            .then(|| self.efforts.iter().find(|e| *e == effort))
            .flatten()
            .map(String::as_str)
    }
}

/// agy's models from `agy models` output: `id<TAB>Display` per line, with the effort folded into
/// the id suffix (`gemini-3.1-pro-high`) and the display (`Gemini 3.1 Pro (High)`). Variants of
/// one base model become ONE entry: id = the base, display without the parenthetical, efforts =
/// the offered suffixes in low..max order. An id without an effort suffix stays as it is, with no
/// efforts. Blank lines, lines without a tab or with an empty id, and ids with whitespace are
/// ignored.
pub fn parse_agy_models(tsv: &str) -> Vec<CatalogModel> {
    let mut out: Vec<CatalogModel> = Vec::new();
    for line in tsv.lines() {
        let Some((id, display)) = line.trim_end_matches('\r').split_once('\t') else {
            continue;
        };
        let (id, display) = (id.trim(), display.trim());
        if id.is_empty() || id.contains(char::is_whitespace) {
            continue;
        }
        let (base, effort) = match split_effort(id) {
            Some((b, e)) => (b, Some(e)),
            None => (id, None),
        };
        let display = match effort {
            Some(e) => strip_effort_label(display, e),
            None => display.to_owned(),
        };
        let display = if display.is_empty() {
            base.to_owned()
        } else {
            display
        };
        let at = match out.iter().position(|m| m.id == base) {
            Some(i) => i,
            None => {
                let via = (base.starts_with("claude-") || base.starts_with("gpt-"))
                    .then(|| VIA_ANTIGRAVITY.to_owned());
                out.push(CatalogModel {
                    driver: Driver::Agy,
                    id: base.to_owned(),
                    display,
                    description: None,
                    efforts: Vec::new(),
                    default_effort: None,
                    via,
                });
                out.len() - 1
            }
        };
        if let Some(e) = effort {
            let efforts = &mut out[at].efforts;
            if !efforts.iter().any(|x| x == e) {
                efforts.push(e.to_owned());
                efforts.sort_by_key(|x| effort_rank(x));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Retired models: what a thread on a model the agent no longer offers can move to
// ---------------------------------------------------------------------------------------------

/// The numbers of a model id, in order (`claude-sonnet-5-5` is `[5, 5]`, `gemini-3.1-pro` is
/// `[3, 1]`): newer versions compare greater.
fn version_key(id: &str) -> Vec<u64> {
    let mut out = Vec::new();
    let mut digits = String::new();
    for c in id.chars().chain(std::iter::once(' ')) {
        if c.is_ascii_digit() {
            digits.push(c);
        } else if !digits.is_empty() {
            if let Ok(n) = digits.parse() {
                out.push(n);
            }
            digits.clear();
        }
    }
    out
}

/// A model id without its version, its effort, a `claude` vendor prefix and bracketed suffixes:
/// the family and tier that a newer model of the same line shares (`claude-sonnet-5-5` and
/// `sonnet` are `sonnet`; `gemini-3.1-pro-high` is `gemini-pro`; `gpt-5-codex-mini` is
/// `gpt-codex-mini`).
pub fn family_of(id: &str) -> String {
    let id = id.split('[').next().unwrap_or(id).to_lowercase();
    id.split(['-', '.', '_', ' '])
        .filter(|t| !t.is_empty())
        .filter(|t| !t.chars().any(|c| c.is_ascii_digit()))
        .filter(|t| !EFFORT_ORDER.contains(t) && !matches!(*t, "claude" | "latest" | "preview"))
        .collect::<Vec<_>>()
        .join("-")
}

impl CatalogModel {
    /// An alias that tracks the newest model of its line by itself (Claude's `opus`, `sonnet`,
    /// `haiku`, `default`): no version in the id. Threads store the alias, so they follow the
    /// agent's updates without being ported.
    pub fn is_alias(&self) -> bool {
        self.driver == Driver::Claude && !self.id.chars().any(|c| c.is_ascii_digit())
    }
}

/// Where a thread on a retired model should go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replacement {
    /// The model value to hand to a switch (agy: the composed `<base>-<effort>` id).
    pub model: String,
    pub display: String,
    /// The effort to keep or start on (`None` when the model has none).
    pub effort: Option<String>,
}

/// The model a thread on `retired` should move to, among `models` (one agent's current list).
///
/// The same family and tier first (the newest of `sonnet`-line models for a retired
/// `claude-sonnet-*`), preferring an alias when there is one; else the agent's default model (the
/// `default` alias, else the head of the list). The thread's `effort` is kept when the target
/// offers it, else the target's own default. `None` only when the list is empty.
pub fn suggest_replacement(
    models: &[CatalogModel],
    retired: &str,
    effort: Option<&str>,
) -> Option<Replacement> {
    let family = family_of(retired);
    let same_line = models
        .iter()
        .filter(|m| !family.is_empty() && family_of(&m.id) == family)
        // An alias first, then the newest version.
        .max_by(|a, b| (a.is_alias(), version_key(&a.id)).cmp(&(b.is_alias(), version_key(&b.id))));
    let target = same_line
        .or_else(|| models.iter().find(|m| m.id == "default"))
        .or_else(|| models.first())?;
    let effort = effort
        .filter(|e| target.efforts.iter().any(|x| x == e))
        .or_else(|| target.default_effort())
        .filter(|_| !target.efforts.is_empty());
    Some(Replacement {
        model: target.model_id_for(effort),
        display: target.display.clone(),
        effort: effort.map(str::to_owned),
    })
}

/// What a thread's stored model amounts to against its agent's catalogue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelNotice {
    /// Nothing to say: the model is current, is the agent's own default, or the list is not
    /// known well enough to tell.
    None,
    /// The agent no longer offers `model`; `replacement` is where it should go.
    Retired {
        model: String,
        replacement: Option<Replacement>,
    },
}

/// Decides [`ModelNotice`] for a thread on `stored` (the model it was last set to; `None` or
/// `default` is the agent's own). `models` is the agent's list and `fresh` says it came from a
/// fetch in this run: a cached list, or one still loading, is never grounds to call a model
/// retired (`ModelNotice::None`), only a fresh one is.
pub fn model_notice(
    models: &[CatalogModel],
    fresh: bool,
    stored: Option<&str>,
    effort: Option<&str>,
) -> ModelNotice {
    let Some(stored) = stored.filter(|m| !m.is_empty() && *m != "default") else {
        return ModelNotice::None;
    };
    if !fresh || models.is_empty() {
        return ModelNotice::None;
    }
    if models.iter().any(|m| m.is_model(stored)) {
        return ModelNotice::None;
    }
    ModelNotice::Retired {
        model: stored.to_owned(),
        replacement: suggest_replacement(models, stored, effort),
    }
}

/// The picker's rows: when `current` is provided, that agent's models float to the top; otherwise
/// native Claude first, then Antigravity, then Codex, each in catalog order, filtered by
/// a case-insensitive substring of id, display name or description (every whitespace-separated
/// word of `query` must match somewhere). Empty groups are dropped.
pub fn group_for_picker<'a>(
    models: &'a [CatalogModel],
    query: &str,
    current: Option<Driver>,
) -> Vec<(Driver, Vec<&'a CatalogModel>)> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let matches = |m: &CatalogModel| {
        let hay = format!(
            "{} {} {} {}",
            m.id,
            m.display,
            m.description.as_deref().unwrap_or(""),
            m.via.as_deref().unwrap_or("")
        )
        .to_lowercase();
        words.iter().all(|w| hay.contains(w))
    };
    let mut drivers = Vec::with_capacity(Driver::ALL.len());
    if let Some(cur) = current {
        drivers.push(cur);
    }
    for d in Driver::ALL {
        if Some(d) != current {
            drivers.push(d);
        }
    }
    drivers
        .into_iter()
        .filter_map(|d| {
            let mut rows: Vec<&CatalogModel> = models
                .iter()
                .filter(|m| m.driver == d && matches(m))
                .collect();
            // Aliases (they follow the agent's newest model) before pinned versions.
            rows.sort_by_key(|m| !m.is_alias());
            (!rows.is_empty()).then_some((d, rows))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTROLS: &str = include_str!("../tests/fixtures/claude-controls-slash.ndjson");

    /// The recorded `init-1` frame (`dir: out`), whole.
    fn init_frame() -> Value {
        CONTROLS
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|r| r.pointer("/frame/response/request_id") == Some(&Value::from("init-1")))
            .expect("init-1 response in the fixture")["frame"]
            .clone()
    }

    const TSV: &str = "gemini-3.8-flash-high\tGemini 3.8 Flash (High)\n\
gemini-3.8-flash-medium\tGemini 3.8 Flash (Medium)\n\
gemini-3.8-flash-low\tGemini 3.8 Flash (Low)\n\
gemini-3.1-pro-high\tGemini 3.1 Pro (High)\n\
gemini-3.1-pro-low\tGemini 3.1 Pro (Low)\n\
claude-opus-5-5-low\tClaude Opus 5.5 (Low)\n\
claude-opus-5-5-medium\tClaude Opus 5.5 (Medium)\n\
claude-opus-5-5-high\tClaude Opus 5.5 (High)\n\
claude-sonnet-5-5-low\tClaude Sonnet 5.5 (Low)\n\
claude-sonnet-5-5-medium\tClaude Sonnet 5.5 (Medium)\n\
claude-sonnet-5-5-high\tClaude Sonnet 5.5 (High)\n\
gpt-oss-120b-medium\tGPT-OSS 120B (Medium)\n";

    #[test]
    fn claude_models_from_the_recorded_initialize() {
        let m = parse_claude_initialize(&init_frame());
        let ids: Vec<&str> = m.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            ["default", "opus", "claude-fable-5-1[1m]", "sonnet", "haiku"]
        );
        let sonnet = &m[3];
        assert_eq!(sonnet.driver, Driver::Claude);
        assert_eq!(sonnet.display, "Sonnet");
        assert_eq!(
            sonnet.description.as_deref(),
            Some("Sonnet 5.5 · Efficient for routine tasks")
        );
        assert_eq!(sonnet.efforts, ["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(sonnet.via, None);
        assert!(m[4].efforts.is_empty(), "haiku has no effort levels");
    }

    #[test]
    fn claude_payload_shapes_are_all_accepted() {
        let frame = init_frame();
        let inner = frame["response"]["response"].clone();
        let envelope = frame["response"].clone();
        let n = parse_claude_initialize(&frame).len();
        assert_eq!(n, 5);
        assert_eq!(parse_claude_initialize(&inner).len(), n);
        assert_eq!(parse_claude_initialize(&envelope).len(), n);
        assert!(parse_claude_initialize(&serde_json::json!({"weird": 1})).is_empty());
        assert!(
            parse_claude_initialize(&serde_json::json!({"models": [{"nope": 1}, 3]})).is_empty()
        );
    }

    #[test]
    fn agy_variants_fold_into_one_model_per_base() {
        let m = parse_agy_models(TSV);
        let ids: Vec<&str> = m.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "gemini-3.8-flash",
                "gemini-3.1-pro",
                "claude-opus-5-5",
                "claude-sonnet-5-5",
                "gpt-oss-120b"
            ]
        );
        let flash = &m[0];
        assert_eq!(flash.display, "Gemini 3.8 Flash");
        assert_eq!(flash.driver, Driver::Agy);
        // The TSV lists high, medium, low; the dropdown order is low < medium < high.
        assert_eq!(flash.efforts, ["low", "medium", "high"]);
        assert_eq!(m[1].efforts, ["low", "high"]);
        assert_eq!(m[1].display, "Gemini 3.1 Pro");
        assert_eq!(m[0].via, None);
        assert_eq!(m[2].via.as_deref(), Some("Antigravity"));
        assert_eq!(m[4].via.as_deref(), Some("Antigravity"));
        // A single variant still gets its one effort.
        assert_eq!(m[4].efforts, ["medium"]);
        assert_eq!(m[4].display, "GPT-OSS 120B");
    }

    #[test]
    fn agy_effort_order_covers_xhigh_and_max_and_unsuffixed_ids() {
        let m = parse_agy_models(
            "m-max\tM (Max)\nm-low\tM (Low)\nm-xhigh\tM (XHigh)\nplain\tPlain One\nodd-high\tOdd name\n",
        );
        assert_eq!(m[0].id, "m");
        assert_eq!(m[0].efforts, ["low", "xhigh", "max"]);
        assert_eq!(m[0].display, "M");
        assert_eq!((m[1].id.as_str(), m[1].efforts.len()), ("plain", 0));
        // A display without the parenthetical is kept whole.
        assert_eq!(m[2].display, "Odd name");
    }

    #[test]
    fn model_ids_compose_per_agent() {
        let agy = parse_agy_models(TSV);
        let (flash, pro, gpt) = (&agy[0], &agy[1], &agy[4]);
        assert_eq!(flash.model_id_for(Some("low")), "gemini-3.8-flash-low");
        assert_eq!(flash.model_id_for(None), "gemini-3.8-flash-medium");
        // No medium: the first (lowest) effort is the default.
        assert_eq!(pro.default_effort(), Some("low"));
        assert_eq!(pro.model_id_for(None), "gemini-3.1-pro-low");
        // An effort the model does not offer falls back to the default.
        assert_eq!(pro.model_id_for(Some("max")), "gemini-3.1-pro-low");
        assert_eq!(gpt.model_id_for(Some("high")), "gpt-oss-120b-medium");
        // Claude's id never changes.
        let claude = parse_claude_initialize(&init_frame());
        assert_eq!(claude[3].model_id_for(Some("high")), "sonnet");
        // No efforts at all: the id as listed.
        let bare = parse_agy_models("plain\tPlain\n");
        assert_eq!(bare[0].model_id_for(Some("high")), "plain");
        assert_eq!(bare[0].default_effort(), None);
    }

    #[test]
    fn a_threads_model_id_maps_back_to_its_row_and_effort() {
        let agy = parse_agy_models(TSV);
        let flash = &agy[0];
        assert!(flash.is_model("gemini-3.8-flash-high"));
        assert!(flash.is_model("gemini-3.8-flash"));
        assert!(!flash.is_model("gemini-3.1-pro-high"));
        assert!(
            !flash.is_model("gemini-3.8-flash-max"),
            "not an offered effort"
        );
        assert_eq!(flash.effort_in("gemini-3.8-flash-low"), Some("low"));
        assert_eq!(flash.effort_in("gemini-3.8-flash"), None);
        let claude = parse_claude_initialize(&init_frame());
        assert_eq!(claude[3].effort_in("sonnet-high"), None);
        assert!(claude[3].is_model("sonnet"));
    }

    #[test]
    fn agy_garbage_is_ignored() {
        let m = parse_agy_models(
            "\n   \nnot a model line\n\tNo id\nbad id\tx\nok-1\tOK One\r\nbare\t\nWarning: foo\n",
        );
        let got: Vec<(&str, &str)> = m
            .iter()
            .map(|m| (m.id.as_str(), m.display.as_str()))
            .collect();
        assert_eq!(got, [("ok-1", "OK One"), ("bare", "bare")]);
    }

    #[test]
    fn codex_models_map_onto_the_catalog_with_their_default_effort() {
        let result = serde_json::json!({"data": [
            {"id": "a", "model": "gpt-5-codex", "displayName": "GPT-5 Codex", "description": "d",
             "supportedReasoningEfforts": [{"reasoningEffort": "low"}, {"reasoningEffort": "high"}],
             "defaultReasoningEffort": "high", "hidden": false, "isDefault": true},
            {"id": "b", "model": "old", "hidden": true},
            {"id": "c", "displayName": " "}
        ]});
        let m = parse_codex_models(&result);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].driver, Driver::Codex);
        assert_eq!(m[0].id, "gpt-5-codex");
        assert_eq!(m[0].efforts, ["low", "high"]);
        assert_eq!(m[0].default_effort(), Some("high"));
        assert_eq!(m[0].model_id_for(Some("low")), "gpt-5-codex");
        assert_eq!((m[1].id.as_str(), m[1].display.as_str()), ("c", "c"));
        assert_eq!(m[1].description, None);
        assert!(parse_codex_models(&serde_json::json!({"x": 1})).is_empty());

        let mut all = vec![m[0].clone()];
        all.extend(parse_agy_models(TSV));
        let g = group_for_picker(&all, "", None);
        assert_eq!(
            g.iter().map(|(d, _)| *d).collect::<Vec<_>>(),
            [Driver::Agy, Driver::Codex]
        );
    }

    fn m(driver: Driver, id: &str, efforts: &[&str]) -> CatalogModel {
        CatalogModel {
            driver,
            id: id.into(),
            display: id.to_uppercase(),
            description: None,
            efforts: efforts.iter().map(|e| (*e).to_owned()).collect(),
            default_effort: None,
            via: None,
        }
    }

    #[test]
    fn families_ignore_versions_efforts_and_the_vendor_prefix() {
        for (id, family) in [
            ("claude-sonnet-5-5", "sonnet"),
            ("sonnet", "sonnet"),
            ("claude-fable-5-1[1m]", "fable"),
            ("gemini-3.1-pro-high", "gemini-pro"),
            ("gemini-3.8-flash", "gemini-flash"),
            ("gpt-5-codex", "gpt-codex"),
            ("gpt-5-codex-mini", "gpt-codex-mini"),
            ("gpt-5.5", "gpt"),
            ("", ""),
        ] {
            assert_eq!(family_of(id), family, "{id}");
        }
        assert_eq!(version_key("claude-sonnet-5-5"), [5, 5]);
        assert_eq!(version_key("gemini-3.1-pro"), [3, 1]);
        assert!(version_key("gemini-3.8-flash") > version_key("gemini-3.1-flash"));
        assert!(version_key("sonnet").is_empty());
    }

    #[test]
    fn a_retired_model_moves_to_the_newest_of_its_family_keeping_the_effort() {
        let agy = vec![
            m(Driver::Agy, "gemini-3.1-pro", &["low", "high"]),
            m(Driver::Agy, "gemini-3.8-flash", &["low", "medium", "high"]),
            m(Driver::Agy, "gemini-4-pro", &["low", "high"]),
        ];
        // gemini-2-pro -> the newest pro; its "high" is offered, so it is kept (and composed).
        let r = suggest_replacement(&agy, "gemini-2-pro-high", Some("high")).expect("a suggestion");
        assert_eq!(r.model, "gemini-4-pro-high");
        assert_eq!(r.effort.as_deref(), Some("high"));
        // An effort the target lacks falls back to the target's default.
        let r =
            suggest_replacement(&agy, "gemini-2-pro-high", Some("xhigh")).expect("a suggestion");
        assert_eq!(r.effort.as_deref(), Some("low"));
        assert_eq!(r.model, "gemini-4-pro-low");
        // A flash stays a flash.
        let r =
            suggest_replacement(&agy, "gemini-2.5-flash-medium", Some("medium")).expect("flash");
        assert_eq!(r.model, "gemini-3.8-flash-medium");
    }

    #[test]
    fn claude_prefers_the_alias_of_the_same_line_then_the_default() {
        let claude = vec![
            m(Driver::Claude, "default", &[]),
            m(Driver::Claude, "opus", &["low", "high"]),
            m(Driver::Claude, "claude-sonnet-5-5", &["low", "high"]),
            m(Driver::Claude, "sonnet", &["low", "high"]),
        ];
        // A pinned retired sonnet goes to the alias, which tracks the newest from now on.
        let r = suggest_replacement(&claude, "claude-sonnet-4-1", Some("high")).expect("alias");
        assert_eq!(r.model, "sonnet");
        assert_eq!(r.effort.as_deref(), Some("high"));
        // No family match: the agent's default model.
        let r = suggest_replacement(&claude, "claude-haiku-3", None).expect("default");
        assert_eq!(r.model, "default");
        assert_eq!(r.effort, None, "the default has no effort levels");
        // No `default` row: the head of the list.
        let r = suggest_replacement(&claude[1..], "claude-haiku-3", None).expect("head");
        assert_eq!(r.model, "opus");
        assert!(suggest_replacement(&[], "x", None).is_none());
    }

    #[test]
    fn codex_lists_its_default_first_so_it_is_the_fallback() {
        let result = serde_json::json!({"data": [
            {"id": "a", "model": "gpt-5-mini", "isDefault": false},
            {"id": "b", "model": "gpt-5-codex", "isDefault": true,
             "supportedReasoningEfforts": [{"reasoningEffort": "low"}, {"reasoningEffort": "high"}],
             "defaultReasoningEffort": "high"}
        ]});
        let models = parse_codex_models(&result);
        assert_eq!(models[0].id, "gpt-5-codex");
        let r = suggest_replacement(&models, "o3-pro", Some("low")).expect("default");
        assert_eq!(
            (r.model.as_str(), r.effort.as_deref()),
            ("gpt-5-codex", Some("low"))
        );
    }

    #[test]
    fn only_a_fresh_list_can_call_a_model_retired() {
        let list = vec![
            m(Driver::Claude, "default", &[]),
            m(Driver::Claude, "sonnet", &[]),
        ];
        let notice = |fresh, stored| model_notice(&list, fresh, stored, None);
        // Still loading, or only the cache: unknown, never "retired".
        assert_eq!(notice(false, Some("claude-opus-3")), ModelNotice::None);
        assert_eq!(model_notice(&[], true, Some("x"), None), ModelNotice::None);
        // The agent's own default and listed models are fine.
        assert_eq!(notice(true, None), ModelNotice::None);
        assert_eq!(notice(true, Some("default")), ModelNotice::None);
        assert_eq!(notice(true, Some("sonnet")), ModelNotice::None);
        // Gone from a fresh list: retired, with where to go.
        match notice(true, Some("claude-sonnet-3-7")) {
            ModelNotice::Retired { model, replacement } => {
                assert_eq!(model, "claude-sonnet-3-7");
                assert_eq!(replacement.expect("replacement").model, "sonnet");
            }
            other => panic!("{other:?}"),
        }
        // An agy id at one of its efforts is the listed base model.
        let agy = vec![m(Driver::Agy, "gemini-3.1-pro", &["low", "high"])];
        assert_eq!(
            model_notice(&agy, true, Some("gemini-3.1-pro-high"), None),
            ModelNotice::None
        );
    }

    #[test]
    fn the_picker_lists_aliases_before_pinned_versions() {
        let all = vec![
            m(Driver::Claude, "claude-fable-5-1[1m]", &[]),
            m(Driver::Claude, "opus", &[]),
            m(Driver::Claude, "default", &[]),
        ];
        let g = group_for_picker(&all, "", None);
        let ids: Vec<&str> = g[0].1.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["opus", "default", "claude-fable-5-1[1m]"]);
    }

    #[test]
    fn picker_groups_claude_first_and_filters() {
        let mut all = parse_agy_models(TSV);
        all.extend(parse_claude_initialize(&init_frame()));
        let g = group_for_picker(&all, "", None);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].0, Driver::Claude);
        assert_eq!(g[0].1.len(), 5);
        assert_eq!(g[1].0, Driver::Agy);
        assert_eq!(g[1].1.len(), 5);
        assert_eq!(g[1].1[0].id, "gemini-3.8-flash", "stable order");

        // Case-insensitive across id, display and description; all words must match.
        let g = group_for_picker(&all, "SONNET", None);
        assert_eq!(g[0].1.len(), 1);
        assert_eq!(g[1].1.len(), 1);
        let g = group_for_picker(&all, "gemini pro", None);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].0, Driver::Agy);
        assert_eq!(g[0].1[0].id, "gemini-3.1-pro");
        let g = group_for_picker(&all, "efficient", None);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].0, Driver::Claude);
        assert!(group_for_picker(&all, "zzz", None).is_empty());
    }

    #[test]
    fn picker_floats_current_agent_to_top() {
        let mut all = parse_agy_models(TSV);
        all.extend(parse_claude_initialize(&init_frame()));

        // In an Antigravity thread, Agy models float to the top.
        let g = group_for_picker(&all, "", Some(Driver::Agy));
        assert_eq!(g[0].0, Driver::Agy);
        assert_eq!(g[1].0, Driver::Claude);

        // In a Claude thread, Claude models float to the top.
        let g = group_for_picker(&all, "", Some(Driver::Claude));
        assert_eq!(g[0].0, Driver::Claude);
        assert_eq!(g[1].0, Driver::Agy);

        // Without current, default order is preserved.
        let g = group_for_picker(&all, "", None);
        assert_eq!(g[0].0, Driver::Claude);
        assert_eq!(g[1].0, Driver::Agy);
    }
}
