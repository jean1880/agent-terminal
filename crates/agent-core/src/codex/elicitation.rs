//! Codex MCP forms (`mcpServer/elicitation/request`), read into a [`Form`] or into the reason
//! the adapter declines them.
//!
//! `mode: "form"` maps the MCP 2025-11-25 `requestedSchema` to [`Form::Fields`]; `mode: "url"`
//! with a link [`form::link_host`] accepts becomes [`Form::Link`]. Everything else, and every
//! form that is too large, unusable or asks for a secret, is declined with a reason the notice
//! says in full. A field whose kind is not known becomes [`FieldInput::Unknown`]: the form still
//! renders, and cannot be submitted while that field is required.

use serde::de::{Deserializer, IgnoredAny, MapAccess, Visitor};
use serde::Deserialize;
use serde_json::{Map, Value};

use super::permissions::{clip, visible};
use crate::form::{self, ChoiceOption, FieldInput, Form, FormField, TextFormat};

/// A form with more fields than this is declined.
pub(super) const MAX_FIELDS: usize = 20;
/// A choice with more options than this is declined.
pub(super) const MAX_OPTIONS: usize = 50;
/// A message longer than this (in characters) is declined.
pub(super) const MAX_MESSAGE: usize = 4_000;
/// A field name or choice value longer than this is declined (the answer carries it whole).
const MAX_KEY: usize = 200;
/// Titles and option labels are cut to this many characters for display.
const MAX_LABEL: usize = 200;
/// Descriptions are cut to this many characters for display.
const MAX_DESCRIPTION: usize = 1_000;
/// Server names are cut to this many characters.
const MAX_SERVER: usize = 64;

/// Why a form that asks for a secret is declined.
const SECRET: &str =
    "asks for a password or other secret; agent-terminal never passes secrets to tool servers";

/// A string `format` that names a secret: `password` (JSON Schema's own) or any unknown format
/// whose name reads as one.
fn secret_format(format: &str) -> bool {
    let f = format.to_ascii_lowercase();
    [
        "pass",
        "secret",
        "token",
        "key",
        "credential",
        "pin",
        "otp",
        "totp",
        "mfa",
    ]
    .iter()
    .any(|w| f.contains(w))
}

/// What a request is read into.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Elicitation {
    /// Show it.
    Show { message: String, form: Form },
    /// Decline it; `reason` reads after "The <server> tool server". `link` is a refused link,
    /// for the notice to show as text.
    Decline {
        reason: String,
        link: Option<String>,
    },
}

/// The server's name, as the notice and card say it: hidden characters removed, at most
/// [`MAX_SERVER`] characters.
pub(super) fn server_name(params: &Value) -> String {
    let name = params
        .get("serverName")
        .and_then(Value::as_str)
        .map(visible)
        .unwrap_or_default();
    match name.trim() {
        "" => "unnamed".to_owned(),
        name => clip(name, MAX_SERVER),
    }
}

/// Reads a request's params. `order` is the server's own order of the schema's properties
/// (`serde_json` keeps keys sorted), read from the raw frame by [`property_order`].
pub(super) fn parse(params: &Value, order: Option<&[String]>) -> Elicitation {
    let decline = |reason: &str| Elicitation::Decline {
        reason: reason.to_owned(),
        link: None,
    };
    let message = params
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if message.chars().count() > MAX_MESSAGE {
        return decline("sent a message longer than agent-terminal shows (4 000 characters)");
    }
    match params.get("mode").and_then(Value::as_str) {
        Some("form") => {}
        Some("openai/form" | "openaiForm") => {
            return decline("asked for a form agent-terminal cannot show yet");
        }
        Some("url") => {
            let url = params
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return if form::link_host(url).is_some() {
                Elicitation::Show {
                    message,
                    form: Form::Link {
                        url: url.to_owned(),
                    },
                }
            } else {
                Elicitation::Decline {
                    reason: "asked you to open a link that is not a plain web address".to_owned(),
                    link: Some(url.to_owned()),
                }
            };
        }
        _ => return decline("asked for something agent-terminal does not know how to show"),
    }
    let Some(schema) = params.get("requestedSchema").and_then(Value::as_object) else {
        return decline("sent a form agent-terminal could not read");
    };
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return decline("sent a form agent-terminal could not read");
    };
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return decline("sent a form agent-terminal could not read");
    }
    // Before any per-field work: a hostile frame may carry any number of properties.
    if properties.len() > MAX_FIELDS {
        return decline("sent a form with more fields than agent-terminal shows (20)");
    }
    // A text format that is itself a secret (`"format": "password"`), whatever the field's name.
    if properties.values().any(|p| {
        p.get("format")
            .and_then(Value::as_str)
            .is_some_and(secret_format)
    }) {
        return decline(SECRET);
    }
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut names: Vec<&String> = Vec::new();
    for name in order.into_iter().flatten() {
        if properties.contains_key(name) && !names.contains(&name) {
            names.push(name);
        }
    }
    for name in properties.keys() {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let fields: Vec<FormField> = names
        .into_iter()
        .map(|name| {
            field(
                name,
                &properties[name.as_str()],
                required.contains(&name.as_str()),
            )
        })
        .collect();
    if let Err(why) = form::check_form(&fields) {
        return Elicitation::Decline {
            reason: format!("sent a form agent-terminal cannot use ({why})"),
            link: None,
        };
    }
    if fields.iter().any(form::asks_for_secret) {
        return decline(SECRET);
    }
    if fields.iter().any(
        |f| matches!(&f.input, FieldInput::Choice { options, .. } if options.len() > MAX_OPTIONS),
    ) {
        return decline("offers more choices in one field than agent-terminal shows (50)");
    }
    // Names and choice values are what the answer carries, so they cannot be shortened.
    let too_long = |s: &String| s.chars().count() > MAX_KEY;
    if fields.iter().any(|f| {
        too_long(&f.name)
            || matches!(&f.input, FieldInput::Choice { options, .. }
                if options.iter().any(|o| too_long(&o.value)))
    }) {
        return decline("sent a form with field names or choices too long to show");
    }
    Elicitation::Show {
        message,
        form: Form::Fields { fields },
    }
}

/// A display string, cut to `max` characters with an ellipsis.
fn text(schema: &Map<String, Value>, key: &str, max: usize) -> Option<String> {
    schema
        .get(key)
        .and_then(Value::as_str)
        .map(|s| clip(s, max))
}

fn field(name: &str, schema: &Value, required: bool) -> FormField {
    let empty = Map::new();
    let obj = schema.as_object().unwrap_or(&empty);
    FormField {
        name: name.to_owned(),
        title: text(obj, "title", MAX_LABEL),
        description: text(obj, "description", MAX_DESCRIPTION),
        required,
        input: input(obj).unwrap_or(FieldInput::Unknown),
    }
}

/// The field's input, or `None` (shown as [`FieldInput::Unknown`]) for a kind or a shape this
/// build does not know.
fn input(schema: &Map<String, Value>) -> Option<FieldInput> {
    let u32_of = |key| match schema.get(key) {
        None | Some(Value::Null) => Some(None),
        Some(v) => v.as_u64().and_then(|n| u32::try_from(n).ok()).map(Some),
    };
    let u64_of = |key| match schema.get(key) {
        None | Some(Value::Null) => Some(None),
        Some(v) => v.as_u64().map(Some),
    };
    let f64_of = |key| match schema.get(key) {
        None | Some(Value::Null) => Some(None),
        Some(v) => v.as_f64().map(Some),
    };
    match schema.get("type")?.as_str()? {
        "string" => {
            if let Some(values) = schema.get("enum") {
                let values = strings(values)?;
                let labels = match schema.get("enumNames") {
                    None | Some(Value::Null) => values.clone(),
                    Some(names) => {
                        let names = strings(names)?;
                        (names.len() == values.len()).then_some(names)?
                    }
                };
                let options = values
                    .into_iter()
                    .zip(labels)
                    .map(|(value, label)| ChoiceOption {
                        value,
                        label: clip(&label, MAX_LABEL),
                    })
                    .collect();
                return choice(options, false, None, None, single_default(schema));
            }
            if let Some(one_of) = schema.get("oneOf") {
                return choice(titled(one_of)?, false, None, None, single_default(schema));
            }
            let format = match schema.get("format") {
                None | Some(Value::Null) => None,
                Some(f) => Some(match f.as_str()? {
                    "email" => TextFormat::Email,
                    "uri" => TextFormat::Uri,
                    "date" => TextFormat::Date,
                    "date-time" => TextFormat::DateTime,
                    _ => TextFormat::Unknown,
                }),
            };
            Some(FieldInput::Text {
                format,
                min_length: u32_of("minLength")?,
                max_length: u32_of("maxLength")?,
                default: match schema.get("default") {
                    None | Some(Value::Null) => None,
                    Some(d) => Some(d.as_str()?.to_owned()),
                },
            })
        }
        ty @ ("number" | "integer") => Some(FieldInput::Number {
            integer: ty == "integer",
            minimum: f64_of("minimum")?,
            maximum: f64_of("maximum")?,
            default: f64_of("default")?,
        }),
        "boolean" => Some(FieldInput::Boolean {
            default: match schema.get("default") {
                None | Some(Value::Null) => None,
                Some(d) => Some(d.as_bool()?),
            },
        }),
        "array" => {
            let items = schema.get("items")?.as_object()?;
            let options = if let Some(values) = items.get("enum") {
                if items.get("type").and_then(Value::as_str) != Some("string") {
                    return None;
                }
                strings(values)?
                    .into_iter()
                    .map(|v| ChoiceOption {
                        label: v.clone(),
                        value: v,
                    })
                    .collect()
            } else {
                titled(items.get("anyOf")?)?
            };
            let default = match schema.get("default") {
                None | Some(Value::Null) => Vec::new(),
                Some(d) => strings(d)?,
            };
            choice(
                options,
                true,
                u64_of("minItems")?,
                u64_of("maxItems")?,
                default,
            )
        }
        _ => None,
    }
}

fn single_default(schema: &Map<String, Value>) -> Vec<String> {
    schema
        .get("default")
        .and_then(Value::as_str)
        .map(|d| vec![d.to_owned()])
        .unwrap_or_default()
}

/// A choice, keeping only defaults that are among its options.
fn choice(
    options: Vec<ChoiceOption>,
    multi: bool,
    min_items: Option<u64>,
    max_items: Option<u64>,
    default: Vec<String>,
) -> Option<FieldInput> {
    let default = default
        .into_iter()
        .filter(|d| options.iter().any(|o| &o.value == d))
        .collect();
    Some(FieldInput::Choice {
        options,
        multi,
        min_items,
        max_items,
        default,
    })
}

/// An array of strings, or `None`.
fn strings(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|v| v.as_str().map(str::to_owned))
        .collect()
}

/// `[{const, title}]` options.
fn titled(value: &Value) -> Option<Vec<ChoiceOption>> {
    value
        .as_array()?
        .iter()
        .map(|o| {
            Some(ChoiceOption {
                value: o.get("const")?.as_str()?.to_owned(),
                label: clip(o.get("title")?.as_str()?, MAX_LABEL),
            })
        })
        .collect()
}

/// The order of `params.requestedSchema.properties` as the server wrote it, read from the raw
/// frame (a `Value` keeps keys sorted). `None` when the frame has no such object.
pub(super) fn property_order(line: &str) -> Option<Vec<String>> {
    #[derive(Deserialize)]
    struct Frame {
        params: Option<Params>,
    }
    #[derive(Deserialize)]
    struct Params {
        #[serde(rename = "requestedSchema")]
        schema: Option<Schema>,
    }
    #[derive(Deserialize)]
    struct Schema {
        properties: Option<Keys>,
    }
    struct Keys(Vec<String>);
    impl<'de> Deserialize<'de> for Keys {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct KeysVisitor;
            impl<'de> Visitor<'de> for KeysVisitor {
                type Value = Keys;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("an object")
                }
                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Keys, A::Error> {
                    let mut keys = Vec::new();
                    while let Some((key, IgnoredAny)) = map.next_entry::<String, IgnoredAny>()? {
                        keys.push(key);
                    }
                    Ok(Keys(keys))
                }
            }
            d.deserialize_map(KeysVisitor)
        }
    }
    let frame: Frame = serde_json::from_str(line).ok()?;
    Some(frame.params?.schema?.properties?.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn form(schema: Value) -> Value {
        json!({"serverName": "github", "threadId": "th", "turnId": "tu",
               "mode": "form", "message": "Pick one", "requestedSchema": schema})
    }

    fn fields(params: &Value) -> Vec<FormField> {
        match parse(params, None) {
            Elicitation::Show {
                form: Form::Fields { fields },
                ..
            } => fields,
            other => panic!("{other:?}"),
        }
    }

    fn declined(params: &Value) -> String {
        match parse(params, None) {
            Elicitation::Decline { reason, .. } => reason,
            other => panic!("expected a decline, got {other:?}"),
        }
    }

    fn opts(pairs: &[(&str, &str)]) -> Vec<ChoiceOption> {
        pairs
            .iter()
            .map(|(v, l)| ChoiceOption {
                value: (*v).to_owned(),
                label: (*l).to_owned(),
            })
            .collect()
    }

    #[test]
    fn primitives_map_with_their_limits_defaults_and_required_marks() {
        let f = fields(&form(
            json!({"type": "object", "required": ["repo", "count"], "properties": {
                "repo": {"type": "string", "title": "Repository", "description": "owner/name",
                         "minLength": 3, "maxLength": 100, "default": "a/b"},
                "mail": {"type": "string", "format": "email"},
                "when": {"type": "string", "format": "date-time"},
                "count": {"type": "integer", "minimum": 1, "maximum": 9, "default": 2},
                "ratio": {"type": "number", "minimum": 0.5},
                "draft": {"type": "boolean", "default": true},
                "colour": {"type": "colour"},
            }}),
        ));
        let by = |n: &str| f.iter().find(|x| x.name == n).expect(n).clone();
        let repo = by("repo");
        assert!(repo.required);
        assert_eq!(repo.title.as_deref(), Some("Repository"));
        assert_eq!(repo.description.as_deref(), Some("owner/name"));
        assert_eq!(
            repo.input,
            FieldInput::Text {
                format: None,
                min_length: Some(3),
                max_length: Some(100),
                default: Some("a/b".into())
            }
        );
        assert!(matches!(
            by("mail").input,
            FieldInput::Text {
                format: Some(TextFormat::Email),
                ..
            }
        ));
        assert!(matches!(
            by("when").input,
            FieldInput::Text {
                format: Some(TextFormat::DateTime),
                ..
            }
        ));
        assert_eq!(
            by("count").input,
            FieldInput::Number {
                integer: true,
                minimum: Some(1.0),
                maximum: Some(9.0),
                default: Some(2.0)
            }
        );
        assert!(by("count").required);
        assert!(matches!(
            by("ratio").input,
            FieldInput::Number {
                integer: false,
                minimum: Some(m),
                ..
            } if m == 0.5
        ));
        assert_eq!(
            by("draft").input,
            FieldInput::Boolean {
                default: Some(true)
            }
        );
        assert_eq!(by("colour").input, FieldInput::Unknown);
        assert!(!by("draft").required);
    }

    #[test]
    fn every_enum_shape_becomes_a_choice() {
        let f = fields(&form(json!({"type": "object", "properties": {
            "a_untitled": {"type": "string", "enum": ["x", "y"], "default": "y"},
            "b_titled": {"type": "string", "oneOf": [{"const": "x", "title": "Ex"}, {"const": "y", "title": "Why"}]},
            "c_legacy": {"type": "string", "enum": ["x", "y"], "enumNames": ["Ex", "Why"]},
            "d_multi": {"type": "array", "items": {"type": "string", "enum": ["x", "y", "z"]},
                        "minItems": 1, "maxItems": 2, "default": ["z", "nope"]},
            "e_multi_titled": {"type": "array", "items": {"anyOf": [{"const": "x", "title": "Ex"}]}},
            "f_bad_names": {"type": "string", "enum": ["x", "y"], "enumNames": ["Ex"]},
        }})));
        assert_eq!(
            f[0].input,
            FieldInput::Choice {
                options: opts(&[("x", "x"), ("y", "y")]),
                multi: false,
                min_items: None,
                max_items: None,
                default: vec!["y".into()]
            }
        );
        let titled = opts(&[("x", "Ex"), ("y", "Why")]);
        assert!(
            matches!(&f[1].input, FieldInput::Choice { options, multi: false, .. } if *options == titled)
        );
        assert!(
            matches!(&f[2].input, FieldInput::Choice { options, multi: false, .. } if *options == titled)
        );
        assert_eq!(
            f[3].input,
            FieldInput::Choice {
                options: opts(&[("x", "x"), ("y", "y"), ("z", "z")]),
                multi: true,
                min_items: Some(1),
                max_items: Some(2),
                default: vec!["z".into()]
            },
            "a default that is not an option is dropped"
        );
        assert!(
            matches!(&f[4].input, FieldInput::Choice { options, multi: true, .. } if *options == opts(&[("x", "Ex")]))
        );
        assert_eq!(
            f[5].input,
            FieldInput::Unknown,
            "names that do not pair with values"
        );
    }

    #[test]
    fn the_servers_field_order_is_kept() {
        let line = json!({"id": 1, "method": "mcpServer/elicitation/request", "params": {}})
            .to_string()
            .replace(
                "\"params\":{}",
                r#""params":{"serverName":"s","threadId":"t","mode":"form","message":"m","requestedSchema":{"type":"object","properties":{"zeta":{"type":"string"},"alpha":{"type":"boolean"},"mid":{"type":"number"}}}}"#,
            );
        let order = property_order(&line).expect("order");
        assert_eq!(order, ["zeta", "alpha", "mid"]);
        let params: Value = serde_json::from_str::<Value>(&line).unwrap()["params"].clone();
        let Elicitation::Show {
            form: Form::Fields { fields },
            ..
        } = parse(&params, Some(&order))
        else {
            panic!()
        };
        let names: Vec<_> = fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["zeta", "alpha", "mid"]);
        assert_eq!(property_order(r#"{"id":1}"#), None);
    }

    #[test]
    fn every_decline_reason() {
        let props = |n: usize| {
            let mut m = Map::new();
            for i in 0..n {
                m.insert(format!("f{i:02}"), json!({"type": "string"}));
            }
            json!({"type": "object", "properties": m})
        };
        assert!(declined(&form(props(21))).contains("more fields"));
        assert!(matches!(
            parse(&form(props(20)), None),
            Elicitation::Show { .. }
        ));
        let many: Vec<String> = (0..51).map(|i| format!("o{i}")).collect();
        assert!(declined(&form(json!({"type": "object", "properties": {
            "pick": {"type": "string", "enum": many}}})))
        .contains("more choices"));
        assert!(declined(&form(json!({"type": "object", "properties": {
            "apiToken": {"type": "string"}}})))
        .contains("never passes secrets"));
        for format in ["password", "x-api-key", "otp-code"] {
            assert!(
                declined(&form(json!({"type": "object", "properties": {
                    "value": {"type": "string", "format": format}}})))
                .contains("never passes secrets"),
                "{format}"
            );
        }
        assert!(matches!(
            parse(
                &form(json!({"type": "object", "properties": {
                    "when": {"type": "string", "format": "date"}}})),
                None
            ),
            Elicitation::Show { .. }
        ));
        assert!(declined(&form(json!({"type": "object", "properties": {
            "c": {"type": "string", "enum": ["x", "x"]}}})))
        .contains("cannot use"));
        let mut long = form(props(1));
        long["message"] = json!("é".repeat(MAX_MESSAGE + 1));
        assert!(declined(&long).contains("4 000"));
        for mode in ["openai/form", "openaiForm"] {
            let mut p = form(props(1));
            p["mode"] = json!(mode);
            assert_eq!(
                declined(&p),
                "asked for a form agent-terminal cannot show yet"
            );
        }
        let mut p = form(props(1));
        p["mode"] = json!("telepathy");
        assert!(declined(&p).contains("does not know"));
        let mut p = form(json!({"type": "array"}));
        assert!(declined(&p).contains("could not read"));
        p["requestedSchema"] = json!({"type": "array", "properties": {}});
        assert!(declined(&p).contains("could not read"));
    }

    #[test]
    fn links_open_only_when_plain() {
        let link = |url: &str| {
            json!({"serverName": "auth", "threadId": "th", "mode": "url", "message": "Sign in",
                   "url": url, "elicitationId": "e1"})
        };
        assert_eq!(
            parse(&link("https://example.com/login"), None),
            Elicitation::Show {
                message: "Sign in".into(),
                form: Form::Link {
                    url: "https://example.com/login".into()
                }
            }
        );
        for bad in [
            "file:///etc/passwd",
            "https://user:pw@example.com",
            "https://ex\u{202e}ample.com",
        ] {
            assert_eq!(
                parse(&link(bad), None),
                Elicitation::Decline {
                    reason: "asked you to open a link that is not a plain web address".into(),
                    link: Some(bad.into())
                }
            );
        }
        assert_eq!(server_name(&json!({"serverName": "  "})), "unnamed");
    }

    #[test]
    fn an_empty_form_is_a_confirmation() {
        assert_eq!(
            fields(&form(json!({"type": "object", "properties": {}}))),
            vec![]
        );
    }

    /// 50 000 properties are refused at once, before any per-field work (it used to order them
    /// with a quadratic scan on the UI thread).
    #[test]
    fn a_huge_form_is_declined_before_it_is_read() {
        let mut props = Map::new();
        for i in 0..50_000 {
            props.insert(format!("f{i}"), json!({"type": "string"}));
        }
        let order: Vec<String> = props.keys().cloned().collect();
        let started = std::time::Instant::now();
        let out = parse(
            &form(json!({"type": "object", "properties": props})),
            Some(&order),
        );
        assert!(
            matches!(out, Elicitation::Decline { ref reason, .. } if reason.contains("more fields"))
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn long_display_text_is_cut_and_long_keys_are_declined() {
        let long = "x".repeat(5_000);
        let f = fields(&form(json!({"type": "object", "properties": {
            "a": {"type": "string", "title": long, "description": long},
            "b": {"type": "string", "oneOf": [{"const": "v", "title": long}]},
        }})));
        assert_eq!(
            f[0].title.as_deref().map(|t| t.chars().count()),
            Some(MAX_LABEL + 1)
        );
        assert!(f[0].title.as_deref().is_some_and(|t| t.ends_with('…')));
        assert_eq!(
            f[0].description.as_deref().map(|t| t.chars().count()),
            Some(MAX_DESCRIPTION + 1)
        );
        assert!(
            matches!(&f[1].input, FieldInput::Choice { options, .. } if options[0].label.chars().count() == MAX_LABEL + 1)
        );
        let mut props = Map::new();
        props.insert(long.clone(), json!({"type": "string"}));
        assert!(
            declined(&form(json!({"type": "object", "properties": props}))).contains("too long")
        );
        assert!(declined(&form(json!({"type": "object", "properties": {
            "c": {"type": "string", "enum": [long]}}})))
        .contains("too long"));
        let name = server_name(&json!({"serverName": format!("evil\u{202e}{}", "s".repeat(100))}));
        assert!(!name.contains('\u{202e}'));
        assert_eq!(name.chars().count(), MAX_SERVER + 1);
    }
}
