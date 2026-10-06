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
                via: None,
            })
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

/// The picker's rows: native Claude first, then Antigravity, each in catalog order, filtered by
/// a case-insensitive substring of id, display name or description (every whitespace-separated
/// word of `query` must match somewhere). Empty groups are dropped.
pub fn group_for_picker<'a>(
    models: &'a [CatalogModel],
    query: &str,
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
    [Driver::Claude, Driver::Agy]
        .into_iter()
        .filter_map(|d| {
            let rows: Vec<&CatalogModel> = models
                .iter()
                .filter(|m| m.driver == d && matches(m))
                .collect();
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
    fn picker_groups_claude_first_and_filters() {
        let mut all = parse_agy_models(TSV);
        all.extend(parse_claude_initialize(&init_frame()));
        let g = group_for_picker(&all, "");
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].0, Driver::Claude);
        assert_eq!(g[0].1.len(), 5);
        assert_eq!(g[1].0, Driver::Agy);
        assert_eq!(g[1].1.len(), 5);
        assert_eq!(g[1].1[0].id, "gemini-3.8-flash", "stable order");

        // Case-insensitive across id, display and description; all words must match.
        let g = group_for_picker(&all, "SONNET");
        assert_eq!(g[0].1.len(), 1);
        assert_eq!(g[1].1.len(), 1);
        let g = group_for_picker(&all, "gemini pro");
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].0, Driver::Agy);
        assert_eq!(g[0].1[0].id, "gemini-3.1-pro");
        let g = group_for_picker(&all, "efficient");
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].0, Driver::Claude);
        assert!(group_for_picker(&all, "zzz").is_empty());
    }
}
