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

/// agy's models from `agy models` output: `id<TAB>Display` per line. Blank lines, lines without
/// a tab or with an empty id, and ids with whitespace are ignored.
pub fn parse_agy_models(tsv: &str) -> Vec<CatalogModel> {
    tsv.lines()
        .filter_map(|line| {
            let (id, display) = line.trim_end_matches('\r').split_once('\t')?;
            let (id, display) = (id.trim(), display.trim());
            if id.is_empty() || id.contains(char::is_whitespace) {
                return None;
            }
            let via = (id.starts_with("claude-") || id.starts_with("gpt-"))
                .then(|| VIA_ANTIGRAVITY.to_owned());
            Some(CatalogModel {
                driver: Driver::Agy,
                id: id.to_owned(),
                display: if display.is_empty() {
                    id.to_owned()
                } else {
                    display.to_owned()
                },
                description: None,
                efforts: Vec::new(),
                via,
            })
        })
        .collect()
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
    fn agy_tsv_parses_and_tags_third_party_routes() {
        let m = parse_agy_models(TSV);
        assert_eq!(m.len(), 12);
        assert_eq!(m[3].id, "gemini-3.1-pro-high");
        assert_eq!(m[3].display, "Gemini 3.1 Pro (High)");
        assert_eq!(m[3].driver, Driver::Agy);
        assert_eq!(m[3].via, None);
        assert_eq!(m[5].via.as_deref(), Some("Antigravity"));
        assert_eq!(m[11].via.as_deref(), Some("Antigravity"));
        assert!(m.iter().all(|m| m.efforts.is_empty()));
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
        assert_eq!(g[1].1.len(), 12);
        assert_eq!(g[1].1[0].id, "gemini-3.8-flash-high", "stable order");

        // Case-insensitive across id, display and description; all words must match.
        let g = group_for_picker(&all, "SONNET");
        assert_eq!(g[0].1.len(), 1);
        assert_eq!(g[1].1.len(), 3);
        let g = group_for_picker(&all, "pro high");
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].0, Driver::Agy);
        assert_eq!(g[0].1[0].id, "gemini-3.1-pro-high");
        let g = group_for_picker(&all, "efficient");
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].0, Driver::Claude);
        assert!(group_for_picker(&all, "zzz").is_empty());
    }
}
