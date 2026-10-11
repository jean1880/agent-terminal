//! Forms an agent's tool server asks the user to fill in (MCP elicitation), and the checks an
//! answer must pass before it is sent.
//!
//! A [`Form`] is either typed fields (MCP `requestedSchema`: text, number, true/false and choice
//! fields) or a link the user is asked to open. An adapter builds one only if [`check_form`]
//! accepts it. The view renders it and the adapter answers it with
//! [`crate::adapter::Command::SubmitForm`]; both judge an answer with [`answer`], so the card
//! never offers Submit for content the adapter would refuse to send.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// What the user is asked for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Form {
    /// Typed fields, in the order the server listed them.
    Fields { fields: Vec<FormField> },
    /// Open `url` (it passed [`openable_link`]) and say when done.
    Link { url: String },
    /// A form kind from a newer build, read from stored history: shown as expired, never
    /// answered.
    #[serde(other)]
    Unknown,
}

/// One field of a [`Form::Fields`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormField {
    /// The key the answer uses.
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
    pub input: FieldInput,
}

impl FormField {
    /// The label shown for the field: its title, else its key read as words (`repo_name` and
    /// `repoName` are "Repo name").
    pub fn label(&self) -> String {
        if let Some(title) = self.title.as_deref().filter(|t| !t.trim().is_empty()) {
            return title.trim().to_owned();
        }
        let mut words = String::new();
        let mut prev_lower = false;
        for c in self.name.chars() {
            if c == '_' || c == '-' || c == ' ' {
                if !words.ends_with(' ') && !words.is_empty() {
                    words.push(' ');
                }
                prev_lower = false;
                continue;
            }
            if c.is_uppercase() && prev_lower {
                words.push(' ');
            }
            prev_lower = c.is_lowercase() || c.is_ascii_digit();
            words.extend(c.to_lowercase());
        }
        let words = words.trim();
        let mut chars = words.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().chain(chars).collect(),
            None => self.name.clone(),
        }
    }
}

/// What a field takes, with its limits and its prefilled value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FieldInput {
    /// A string; lengths count characters, not bytes.
    Text {
        #[serde(default)]
        format: Option<TextFormat>,
        #[serde(default)]
        min_length: Option<u32>,
        #[serde(default)]
        max_length: Option<u32>,
        #[serde(default)]
        default: Option<String>,
    },
    /// A number (a JSON number answer).
    Number {
        /// Whole numbers only (`"type": "integer"`).
        #[serde(default)]
        integer: bool,
        #[serde(default)]
        minimum: Option<f64>,
        #[serde(default)]
        maximum: Option<f64>,
        #[serde(default)]
        default: Option<f64>,
    },
    /// Yes or no (a JSON boolean answer).
    Boolean {
        #[serde(default)]
        default: Option<bool>,
    },
    /// One or several of a fixed list of values.
    Choice {
        /// Never empty, values unique ([`check_form`]).
        options: Vec<ChoiceOption>,
        /// Several may be picked (an array answer); otherwise exactly one (a string answer).
        #[serde(default)]
        multi: bool,
        #[serde(default)]
        min_items: Option<u64>,
        #[serde(default)]
        max_items: Option<u64>,
        /// The values picked at first. A single choice uses only the first.
        #[serde(default)]
        default: Vec<String>,
    },
    /// A field kind from a newer build: a form holding one cannot be answered.
    #[serde(other)]
    Unknown,
}

/// One option of a [`FieldInput::Choice`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChoiceOption {
    /// What the answer carries.
    pub value: String,
    /// What the user sees.
    pub label: String,
}

/// A shape a text answer must have (MCP string `format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextFormat {
    Email,
    Uri,
    Date,
    DateTime,
    #[serde(other)]
    Unknown,
}

/// How the user answered a form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormAction {
    /// Submit the content (for a link: the user says they are done).
    Accept,
    /// Refuse to give it; the agent carries on without it.
    Decline,
    /// Dismiss it and stop the turn.
    Cancel,
}

/// One field's problem with an answer, worded for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    /// The field it concerns; empty when the answer as a whole is wrong (not an object).
    pub name: String,
    pub message: String,
}

/// Whether a form from an agent can be shown and answered: every field has a non-empty name no
/// other field has, and every choice has at least one option and no two options share a value.
/// An adapter declines a form that fails this, with the reason as a notice.
pub fn check_form(fields: &[FormField]) -> Result<(), String> {
    let mut names = std::collections::HashSet::new();
    for field in fields {
        if field.name.trim().is_empty() {
            return Err("a field has no name".to_owned());
        }
        if !names.insert(field.name.as_str()) {
            return Err(format!("two fields are named {}", field.name));
        }
        if let FieldInput::Choice { options, .. } = &field.input {
            let mut values = std::collections::HashSet::new();
            if options.is_empty() {
                return Err(format!("{} offers no options", field.label()));
            }
            if !options.iter().all(|o| values.insert(o.value.as_str())) {
                return Err(format!("{} offers the same option twice", field.label()));
            }
        }
    }
    Ok(())
}

/// Checks `content` (a JSON object keyed by field name, or `null` for nothing filled in) against
/// `fields` and returns exactly what may be sent: the answered fields, with `null`s dropped.
/// Every problem is returned, in field order, so the card can mark each field at once. A key no
/// field names is a problem too: nothing is sent that the form did not ask for. An empty text or
/// an empty selection counts as not answered, so a required field needs a real answer. The
/// card builds content only from its own fields, so the whole-answer and unknown-key messages
/// reach the adapter's log, never a user.
pub fn answer(
    fields: &[FormField],
    content: &Value,
) -> Result<Map<String, Value>, Vec<FieldError>> {
    let empty = Map::new();
    let Some(answers) = content.as_object().or(content.is_null().then_some(&empty)) else {
        return Err(vec![FieldError {
            name: String::new(),
            message: "The answer is not a set of fields.".to_owned(),
        }]);
    };
    let mut errors = Vec::new();
    let mut sent = Map::new();
    for field in fields {
        let fail = |message: String| FieldError {
            name: field.name.clone(),
            message,
        };
        match answers.get(&field.name).filter(|v| !blank(v)) {
            None if field.required => errors.push(fail(format!("{} is required.", field.label()))),
            None => {}
            Some(value) => match check(&field.input, value) {
                Some(message) => errors.push(fail(message)),
                None => {
                    sent.insert(field.name.clone(), value.clone());
                }
            },
        }
    }
    for key in answers.keys() {
        if !fields.iter().any(|f| &f.name == key) {
            errors.push(FieldError {
                name: key.clone(),
                message: format!("The form has no field named {key}."),
            });
        }
    }
    if errors.is_empty() {
        Ok(sent)
    } else {
        Err(errors)
    }
}

/// Not an answer: `null`, empty text, or an empty selection.
fn blank(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        _ => false,
    }
}

/// The problem with one answered value, if any.
fn check(input: &FieldInput, value: &Value) -> Option<String> {
    match input {
        FieldInput::Text {
            format,
            min_length,
            max_length,
            ..
        } => {
            let Some(text) = value.as_str() else {
                return Some("Enter text.".to_owned());
            };
            let len = text.chars().count();
            if let Some(min) = min_length.filter(|min| len < *min as usize) {
                return Some(format!("Enter at least {min} characters."));
            }
            if let Some(max) = max_length.filter(|max| len > *max as usize) {
                return Some(format!("Enter at most {max} characters."));
            }
            format.and_then(|f| check_format(f, text))
        }
        FieldInput::Number {
            integer,
            minimum,
            maximum,
            ..
        } => {
            let Some(n) = value.as_f64() else {
                return Some("Enter a number.".to_owned());
            };
            if *integer && n.fract() != 0.0 {
                return Some("Enter a whole number.".to_owned());
            }
            if let Some(min) = minimum.filter(|min| n < *min) {
                return Some(format!("Enter {} or more.", number(min)));
            }
            if let Some(max) = maximum.filter(|max| n > *max) {
                return Some(format!("Enter {} or less.", number(max)));
            }
            None
        }
        FieldInput::Boolean { .. } => (!value.is_boolean()).then(|| "Choose yes or no.".to_owned()),
        FieldInput::Choice {
            options,
            multi,
            min_items,
            max_items,
            ..
        } => {
            let known = |v: &Value| {
                v.as_str()
                    .is_some_and(|s| options.iter().any(|o| o.value == s))
            };
            if !*multi {
                return (!known(value)).then(|| "Choose one of the options.".to_owned());
            }
            let Some(picked) = value.as_array() else {
                return Some("Choose from the options.".to_owned());
            };
            if !picked.iter().all(known) {
                return Some("Choose only from the options.".to_owned());
            }
            let mut seen = std::collections::HashSet::new();
            if !picked.iter().all(|v| seen.insert(v.as_str())) {
                return Some("Choose each option once.".to_owned());
            }
            let n = picked.len() as u64;
            if let Some(min) = min_items.filter(|min| n < *min) {
                return Some(format!("Choose at least {min}."));
            }
            if let Some(max) = max_items.filter(|max| n > *max) {
                return Some(format!("Choose at most {max}."));
            }
            None
        }
        FieldInput::Unknown => Some(
            "agent-terminal cannot fill in this kind of field. Decline the form, or ask the agent \
             another way."
                .to_owned(),
        ),
    }
}

/// A lenient shape check: enough to catch a slip, never a full grammar.
fn check_format(format: TextFormat, text: &str) -> Option<String> {
    let ok = match format {
        TextFormat::Email => text.split_once('@').is_some_and(|(user, host)| {
            !user.is_empty() && host.contains('.') && !text.contains(char::is_whitespace)
        }),
        TextFormat::Uri => text.split_once(':').is_some_and(|(scheme, rest)| {
            !scheme.is_empty()
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
                && !rest.is_empty()
                && !text.contains(char::is_whitespace)
        }),
        TextFormat::Date => is_date(text),
        TextFormat::DateTime => text
            .split_once(['T', 't', ' '])
            .is_some_and(|(date, time)| {
                let hh_mm = time.as_bytes();
                is_date(date)
                    && hh_mm.len() >= 5
                    && hh_mm[..2].iter().all(u8::is_ascii_digit)
                    && hh_mm[2] == b':'
                    && hh_mm[3..5].iter().all(u8::is_ascii_digit)
            }),
        TextFormat::Unknown => true,
    };
    (!ok).then(|| match format {
        TextFormat::Email => "Enter an email address.".to_owned(),
        TextFormat::Uri => "Enter a link, such as https://example.com.".to_owned(),
        TextFormat::Date => "Enter a date as YYYY-MM-DD.".to_owned(),
        TextFormat::DateTime => "Enter a date and time, such as 2026-10-11T09:30.".to_owned(),
        TextFormat::Unknown => String::new(),
    })
}

/// `YYYY-MM-DD` with a real month and a day that month can have.
fn is_date(text: &str) -> bool {
    let parts: Vec<&str> = text.split('-').collect();
    let [y, m, d] = parts.as_slice() else {
        return false;
    };
    if y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return false;
    }
    if !parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit())) {
        return false;
    }
    let (Ok(y), Ok(m), Ok(d)) = (y.parse::<u32>(), m.parse::<u32>(), d.parse::<u32>()) else {
        return false;
    };
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&d)
}

/// A bound as the user would write it: `5`, not `5.0`.
fn number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

/// Whether `url` may be offered as a link to open ([`link_host`] finds its host).
pub fn openable_link(url: &str) -> bool {
    link_host(url).is_some()
}

/// The host an openable link goes to, lowercased and without its port, for the card's
/// "Opens <host>" line. `None` when the link may not be offered at all: not http or https, no
/// host, credentials in it, a `\` anywhere (browsers read it as `/`, so the host a reader sees
/// is not the one that opens), or anything but printable ASCII (no spaces, controls, bidi
/// overrides or look-alike letters; an international host arrives in its `xn--` form).
pub fn link_host(url: &str) -> Option<String> {
    if !url.bytes().all(|b| b.is_ascii_graphic()) || url.contains('\\') {
        return None;
    }
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return None;
    }
    let host = match authority.strip_prefix('[') {
        // An IPv6 literal keeps its brackets; its colons are not a port.
        Some(v6) => format!("[{}]", v6.split(']').next().unwrap_or_default()),
        None => authority.split(':').next().unwrap_or_default().to_owned(),
    };
    let bare = host.trim_matches(['[', ']', '.']);
    (!bare.is_empty()).then_some(host)
}

/// What the user should know before opening an openable link, if anything: it is not
/// encrypted, or it goes to this computer, the local network or a bare IP address (a local tool
/// server's sign-in page can do that legitimately, so it is a warning, not a refusal).
pub fn link_concern(url: &str) -> Option<&'static str> {
    let host = link_host(url)?;
    let local = host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host.ends_with(".lan")
        || host.ends_with(".home.arpa");
    let ip = host.starts_with('[')
        || host.split('.').all(|part| {
            !part.is_empty()
                && (part.bytes().all(|b| b.is_ascii_digit())
                    || part
                        .strip_prefix("0x")
                        .is_some_and(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_hexdigit())))
        });
    if local {
        Some("This link goes to this computer or your local network.")
    } else if ip {
        Some("This link goes to a bare IP address, not a named site.")
    } else if url
        .get(..7)
        .is_some_and(|s| s.eq_ignore_ascii_case("http://"))
    {
        Some("This link is not encrypted (http, not https).")
    } else {
        None
    }
}

/// Whether a field asks for a password or another secret, judged from its key and title. A
/// tool server must never ask for one in a form (MCP elicitation forbids it), and the answer
/// would end up in the agent's context and history, so an adapter declines such a form.
pub fn asks_for_secret(field: &FormField) -> bool {
    const SECRET: &[&str] = &[
        "password",
        "passwd",
        "passphrase",
        "secret",
        "token",
        "apikey",
        "credential",
        "credentials",
        "privatekey",
        "otp",
        "totp",
        "mfa",
        "2fa",
    ];
    let mut words: Vec<String> = Vec::new();
    for text in [Some(field.name.as_str()), field.title.as_deref()]
        .into_iter()
        .flatten()
    {
        let mut word = String::new();
        let mut prev_lower = false;
        for c in text.chars() {
            let boundary = !c.is_alphanumeric() || (c.is_uppercase() && prev_lower);
            if boundary && !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            if c.is_alphanumeric() {
                word.extend(c.to_lowercase());
            }
            prev_lower = c.is_lowercase();
        }
        if !word.is_empty() {
            words.push(word);
        }
    }
    let joined: Vec<String> = words.windows(2).map(|w| w.concat()).collect();
    words
        .iter()
        .chain(&joined)
        .any(|w| SECRET.contains(&w.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn field(name: &str, required: bool, input: FieldInput) -> FormField {
        FormField {
            name: name.to_owned(),
            title: None,
            description: None,
            required,
            input,
        }
    }

    fn text(min: Option<u32>, max: Option<u32>, format: Option<TextFormat>) -> FieldInput {
        FieldInput::Text {
            format,
            min_length: min,
            max_length: max,
            default: None,
        }
    }

    fn choice(multi: bool, min: Option<u64>, max: Option<u64>) -> FieldInput {
        FieldInput::Choice {
            options: ["a", "b", "c"]
                .iter()
                .map(|v| ChoiceOption {
                    value: (*v).to_owned(),
                    label: v.to_uppercase(),
                })
                .collect(),
            multi,
            min_items: min,
            max_items: max,
            default: Vec::new(),
        }
    }

    /// [`answer`] when only the verdict matters.
    fn validate(fields: &[FormField], content: &Value) -> Result<(), Vec<FieldError>> {
        answer(fields, content).map(drop)
    }

    fn names(r: Result<(), Vec<FieldError>>) -> Vec<String> {
        r.err()
            .unwrap_or_default()
            .into_iter()
            .map(|e| e.name)
            .collect()
    }

    #[test]
    fn required_fields_and_unknown_keys_are_reported_in_order() {
        let fields = [
            field("name", true, text(None, None, None)),
            field("note", false, text(None, None, None)),
        ];
        assert_eq!(validate(&fields, &json!({"name": "x"})), Ok(()));
        assert_eq!(
            validate(&fields, &json!({"name": "x", "note": null})),
            Ok(())
        );
        assert_eq!(names(validate(&fields, &json!({}))), ["name"]);
        assert_eq!(names(validate(&fields, &Value::Null)), ["name"]);
        assert_eq!(
            names(validate(&fields, &json!({"extra": 1}))),
            ["name", "extra"]
        );
        assert!(validate(&fields, &json!([1])).is_err());
        let err = validate(&fields, &json!({})).unwrap_err();
        assert_eq!(err[0].message, "Name is required.");
    }

    #[test]
    fn a_field_without_a_title_reads_its_key_as_words() {
        let label = |name: &str, title: Option<&str>| {
            FormField {
                title: title.map(str::to_owned),
                ..field(name, false, FieldInput::Boolean { default: None })
            }
            .label()
        };
        assert_eq!(label("repo_name", None), "Repo name");
        assert_eq!(label("repoName", None), "Repo name");
        assert_eq!(label("api-key2", None), "Api key2");
        assert_eq!(label("x", Some("  Pick a branch ")), "Pick a branch");
        assert_eq!(label("x", Some(" ")), "X");
        assert_eq!(
            label("__", None),
            "__",
            "nothing to read falls back to the key"
        );
    }

    #[test]
    fn text_lengths_count_characters_and_formats_are_checked() {
        let f = [field("t", true, text(Some(2), Some(3), None))];
        assert!(validate(&f, &json!({"t": "é"})).is_err());
        assert_eq!(validate(&f, &json!({"t": "éé"})), Ok(()));
        assert!(validate(&f, &json!({"t": "abcd"})).is_err());
        assert!(validate(&f, &json!({"t": 3})).is_err());

        let ok = |format, v: &str| {
            validate(
                &[field("t", true, text(None, None, Some(format)))],
                &json!({"t": v}),
            )
            .is_ok()
        };
        assert!(ok(TextFormat::Email, "a@b.ca"));
        assert!(!ok(TextFormat::Email, "a b@c.ca"));
        assert!(!ok(TextFormat::Email, "@b.ca"));
        assert!(ok(TextFormat::Uri, "https://example.com/x"));
        assert!(ok(TextFormat::Uri, "mailto:a@b.ca"));
        assert!(!ok(TextFormat::Uri, "not a link"));
        assert!(ok(TextFormat::Date, "2024-02-29"));
        assert!(!ok(TextFormat::Date, "2023-02-29"));
        assert!(!ok(TextFormat::Date, "2026-13-01"));
        assert!(!ok(TextFormat::Date, "26-10-11"));
        assert!(ok(TextFormat::DateTime, "2026-10-11T09:30"));
        assert!(ok(TextFormat::DateTime, "2026-10-11 09:30:00Z"));
        assert!(!ok(TextFormat::DateTime, "2026-10-11"));
        assert!(
            !ok(TextFormat::DateTime, "2026-10-11Taé123"),
            "multibyte time, no panic"
        );
        assert!(!ok(TextFormat::DateTime, "2026-10-11T0930x"));
        assert!(!ok(TextFormat::Date, "+123-01-01"));
        assert!(!ok(TextFormat::Date, "2026-+1-01"));
        assert!(ok(TextFormat::Unknown, "anything"));
    }

    #[test]
    fn the_answer_is_exactly_what_is_sent_and_blank_is_unanswered() {
        let fields = [
            field("name", true, text(None, None, None)),
            field("note", false, text(None, None, None)),
            field("tags", true, choice(true, None, None)),
        ];
        let sent = answer(
            &fields,
            &json!({"name": "Ada", "note": null, "tags": ["a"]}),
        )
        .expect("valid");
        assert_eq!(Value::Object(sent), json!({"name": "Ada", "tags": ["a"]}));
        let sent = answer(&fields, &json!({"name": "Ada", "note": "", "tags": ["a"]})).unwrap();
        assert!(
            !sent.contains_key("note"),
            "an empty optional text is not sent"
        );
        assert_eq!(
            names(validate(&fields, &json!({"name": "", "tags": []}))),
            ["name", "tags"],
            "empty text and an empty selection do not satisfy required"
        );
    }

    #[test]
    fn a_form_needs_unique_names_and_real_options() {
        let ok = [
            field("a", true, text(None, None, None)),
            field("b", false, choice(false, None, None)),
        ];
        assert_eq!(check_form(&ok), Ok(()));
        assert!(check_form(&[field(" ", false, FieldInput::Boolean { default: None })]).is_err());
        assert!(check_form(&[
            field("a", false, FieldInput::Boolean { default: None }),
            field("a", false, FieldInput::Boolean { default: None }),
        ])
        .is_err());
        let no_options = FieldInput::Choice {
            options: vec![],
            multi: false,
            min_items: None,
            max_items: None,
            default: vec![],
        };
        assert!(check_form(&[field("c", true, no_options)]).is_err());
        let twice = FieldInput::Choice {
            options: ["x", "x"]
                .iter()
                .map(|v| ChoiceOption {
                    value: (*v).to_owned(),
                    label: (*v).to_owned(),
                })
                .collect(),
            multi: false,
            min_items: None,
            max_items: None,
            default: vec![],
        };
        assert!(check_form(&[field("c", true, twice)]).is_err());
    }

    #[test]
    fn numbers_honour_integer_and_bounds() {
        let f = [field(
            "n",
            true,
            FieldInput::Number {
                integer: true,
                minimum: Some(1.0),
                maximum: Some(10.0),
                default: None,
            },
        )];
        assert_eq!(validate(&f, &json!({"n": 5})), Ok(()));
        let err = validate(&f, &json!({"n": 0})).unwrap_err();
        assert_eq!(err[0].message, "Enter 1 or more.");
        assert!(validate(&f, &json!({"n": 11})).is_err());
        assert!(validate(&f, &json!({"n": 2.5})).is_err());
        assert!(validate(&f, &json!({"n": "5"})).is_err());
    }

    #[test]
    fn booleans_and_choices_take_only_their_values() {
        let b = [field("b", true, FieldInput::Boolean { default: None })];
        assert_eq!(validate(&b, &json!({"b": false})), Ok(()));
        assert!(validate(&b, &json!({"b": "false"})).is_err());

        let one = [field("c", true, choice(false, None, None))];
        assert_eq!(validate(&one, &json!({"c": "b"})), Ok(()));
        assert!(
            validate(&one, &json!({"c": "B"})).is_err(),
            "the label is not the value"
        );
        assert!(validate(&one, &json!({"c": ["b"]})).is_err());

        let many = [field("c", true, choice(true, Some(1), Some(2)))];
        assert_eq!(validate(&many, &json!({"c": ["a", "c"]})), Ok(()));
        assert!(validate(&many, &json!({"c": []})).is_err());
        assert!(validate(&many, &json!({"c": ["a", "b", "c"]})).is_err());
        assert!(validate(&many, &json!({"c": ["a", "a"]})).is_err());
        assert!(validate(&many, &json!({"c": ["z"]})).is_err());
    }

    #[test]
    fn an_unknown_field_kind_cannot_be_answered_and_round_trips() {
        let parsed: FieldInput = serde_json::from_value(json!({"type": "colour"})).unwrap();
        assert_eq!(parsed, FieldInput::Unknown);
        let f = [field("x", false, FieldInput::Unknown)];
        assert!(validate(&f, &json!({"x": "red"})).is_err());
        assert_eq!(validate(&f, &json!({})), Ok(()));

        let form = Form::Fields {
            fields: vec![field("c", true, choice(true, None, None))],
        };
        let back: Form = serde_json::from_value(serde_json::to_value(&form).unwrap()).unwrap();
        assert_eq!(back, form);
        let newer: Form = serde_json::from_value(json!({"kind": "wizard", "steps": 3})).unwrap();
        assert_eq!(newer, Form::Unknown);
    }

    /// The store keeps events as JSON: old rows, new rows and rows from a newer build all read.
    #[test]
    fn form_and_question_events_survive_the_store() {
        use crate::adapter::Command;
        use crate::event::{Envelope, Event, QuestionSource};
        let old: Event =
            serde_json::from_value(json!({"type": "question_requested", "questions": []})).unwrap();
        assert_eq!(
            old,
            Event::QuestionRequested {
                questions: vec![],
                source: QuestionSource::Agent
            },
            "a question stored before `source` existed was the agent's"
        );
        let newer: Event = serde_json::from_value(
            json!({"type": "question_requested", "questions": [], "source": "telepathy"}),
        )
        .unwrap();
        assert!(matches!(
            newer,
            Event::QuestionRequested {
                source: QuestionSource::Agent,
                ..
            }
        ));
        for event in [
            Event::QuestionRequested {
                questions: vec![],
                source: QuestionSource::Reply,
            },
            Event::FormRequested {
                server: "github".into(),
                message: "Pick a repo".into(),
                form: Form::Fields {
                    fields: vec![field("repo", true, choice(false, None, None))],
                },
            },
            Event::FormRequested {
                server: "github".into(),
                message: "Sign in".into(),
                form: Form::Link {
                    url: "https://example.com/login".into(),
                },
            },
            Event::FormResolved {
                action: FormAction::Decline,
            },
        ] {
            let env = Envelope::new(event).request("r1");
            let back: Envelope =
                serde_json::from_value(serde_json::to_value(&env).unwrap()).unwrap();
            assert_eq!(back.event, env.event);
        }
        let submit = Command::SubmitForm {
            request: "r1".into(),
            action: FormAction::Accept,
            content: Some(json!({"repo": "a"})),
        };
        let back: Command = serde_json::from_value(serde_json::to_value(&submit).unwrap()).unwrap();
        assert_eq!(back, submit);
    }

    #[test]
    fn only_plain_http_links_are_openable() {
        assert!(openable_link("https://example.com/auth?x=1"));
        assert!(openable_link("HTTP://example.com"));
        assert!(!openable_link("file:///etc/passwd"));
        assert!(!openable_link("javascript:alert(1)"));
        assert!(!openable_link("https://"));
        assert!(!openable_link("https://user:pw@example.com/"));
        assert!(!openable_link("https://example.com/a b"));
        assert!(!openable_link("https://example.com/\u{202e}x"));
        assert!(!openable_link("https://exаmple.com/"), "Cyrillic а");
        assert!(openable_link("https://xn--exmple-4nf.com/"));
        assert!(!openable_link("https://\\@example.com"));
        assert!(
            !openable_link("https://evil.example\\github.com/"),
            "a `\\` makes the shown host differ from the opened one"
        );
        assert!(!openable_link("https://[]/"));
    }

    #[test]
    fn the_card_names_the_host_and_warns_about_local_and_plain_links() {
        assert_eq!(
            link_host("https://Accounts.Example.com:8443/x?y#z").as_deref(),
            Some("accounts.example.com")
        );
        assert_eq!(link_host("http://[::1]:8080/cb").as_deref(), Some("[::1]"));
        assert_eq!(link_concern("https://github.com/login"), None);
        for local in [
            "http://localhost:3000/callback",
            "https://printer.local/",
            "https://nas.lan/",
        ] {
            assert_eq!(
                link_concern(local),
                Some("This link goes to this computer or your local network."),
                "{local}"
            );
        }
        for ip in [
            "https://192.168.1.1/",
            "http://0x7f.1/",
            "https://[::1]/",
            "https://10.0.0.1:8443/",
        ] {
            assert_eq!(
                link_concern(ip),
                Some("This link goes to a bare IP address, not a named site."),
                "{ip}"
            );
        }
        assert_eq!(
            link_concern("http://example.com/"),
            Some("This link is not encrypted (http, not https).")
        );
        assert_eq!(
            link_concern("file:///etc/passwd"),
            None,
            "not openable at all"
        );
    }

    #[test]
    fn fields_that_ask_for_secrets_are_recognised() {
        let named = |name: &str, title: Option<&str>| FormField {
            title: title.map(str::to_owned),
            ..field(name, true, text(None, None, None))
        };
        for (name, title) in [
            ("password", None),
            ("apiKey", None),
            ("api_key", None),
            ("x", Some("Your GitHub token")),
            ("privateKey", None),
            ("mfa_code", None),
            ("dbPassphrase", None),
        ] {
            assert!(asks_for_secret(&named(name, title)), "{name} {title:?}");
        }
        for (name, title) in [
            ("repo", None),
            ("tokenizer_model", None),
            ("keyboard", None),
            ("x", Some("Primary key column")),
            ("branch", Some("Which branch?")),
        ] {
            assert!(!asks_for_secret(&named(name, title)), "{name} {title:?}");
        }
    }
}
