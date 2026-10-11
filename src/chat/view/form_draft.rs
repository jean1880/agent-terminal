//! What the user has put into a form so far, and what that lets them do (pure: no GTK).
//!
//! A field the user has not touched and that has no default holds nothing (`Null`), so it is
//! never sent as if it were an answer: a number is not `0`, a yes/no is not "no". The card
//! judges the draft with [`form::answer`], the same check the adapter makes before sending.

use agent_core::form::{self, FieldError, FieldInput, FormField};
use serde_json::{Map, Value};

#[derive(Debug, Clone)]
pub struct Draft {
    fields: Vec<FormField>,
    values: Vec<Value>,
}

/// A number field's text as a JSON value: a number when it reads as one, else the text itself
/// (which the check then refuses as "Enter a number."). Blank is nothing.
pub fn number_value(text: &str) -> Value {
    let text = text.trim();
    if text.is_empty() {
        return Value::Null;
    }
    if let Ok(n) = text.parse::<i64>() {
        return Value::from(n);
    }
    match text.parse::<f64>() {
        Ok(n) if n.is_finite() => Value::from(n),
        _ => Value::String(text.to_owned()),
    }
}

/// The value a field starts with: its default, or nothing.
fn initial(field: &FormField) -> Value {
    match &field.input {
        FieldInput::Text { default, .. } => default.clone().map_or(Value::Null, Value::String),
        FieldInput::Number { default, .. } => default.map_or(Value::Null, |n| {
            if n.fract() == 0.0 && n.abs() < 1e15 {
                Value::from(n as i64)
            } else {
                Value::from(n)
            }
        }),
        FieldInput::Boolean { default } => default.map_or(Value::Null, Value::Bool),
        FieldInput::Choice {
            multi,
            options,
            default,
            ..
        } => {
            let known: Vec<&String> = default
                .iter()
                .filter(|d| options.iter().any(|o| &o.value == *d))
                .collect();
            if *multi {
                if known.is_empty() {
                    Value::Null
                } else {
                    Value::Array(
                        known
                            .into_iter()
                            .map(|d| Value::String(d.clone()))
                            .collect(),
                    )
                }
            } else {
                known
                    .first()
                    .map_or(Value::Null, |d| Value::String((*d).clone()))
            }
        }
        FieldInput::Unknown => Value::Null,
    }
}

impl Draft {
    pub fn new(fields: &[FormField]) -> Self {
        Self {
            values: fields.iter().map(initial).collect(),
            fields: fields.to_vec(),
        }
    }

    /// The value field `index` starts with (what the widget shows first).
    pub fn initial(&self, index: usize) -> Value {
        self.fields.get(index).map_or(Value::Null, initial)
    }

    /// Records what field `index` now holds (`Null`: nothing).
    pub fn set(&mut self, index: usize, value: Value) {
        if let Some(slot) = self.values.get_mut(index) {
            *slot = value;
        }
    }

    /// Everything filled in or defaulted, keyed by field name.
    pub fn content(&self) -> Value {
        let mut map = Map::new();
        for (field, value) in self.fields.iter().zip(&self.values) {
            if !value.is_null() {
                map.insert(field.name.clone(), value.clone());
            }
        }
        Value::Object(map)
    }

    /// Exactly what Submit would send, or every problem with it.
    pub fn check(&self) -> Result<Map<String, Value>, Vec<FieldError>> {
        form::answer(&self.fields, &self.content())
    }

    /// The problem with field `index`, if it has one.
    pub fn error(&self, index: usize) -> Option<String> {
        let name = &self.fields.get(index)?.name;
        self.check()
            .err()?
            .into_iter()
            .find(|e| &e.name == name)
            .map(|e| e.message)
    }

    /// Why Submit is disabled, in one line; `None` when it may be pressed. A missing required
    /// field is "Fill in Repo name"; anything else wrong is "Fix Repo name".
    pub fn submit_hint(&self) -> Option<String> {
        let errors = self.check().err()?;
        let label_of = |name: &str| {
            self.fields
                .iter()
                .find(|f| f.name == name)
                .map(FormField::label)
        };
        // A required field of a kind this build cannot show is the one thing that is no use
        // filling in: say so, instead of asking for what cannot be given.
        if let Some(field) = self
            .fields
            .iter()
            .find(|f| f.required && matches!(f.input, FieldInput::Unknown))
        {
            return Some(format!("Cannot fill in {}", field.label()));
        }
        let missing: Vec<String> = self
            .fields
            .iter()
            .zip(&self.values)
            .filter(|(f, v)| f.required && blank(v) && errors.iter().any(|e| e.name == f.name))
            .map(|(f, _)| f.label())
            .collect();
        let (verb, names) = if missing.is_empty() {
            let wrong: Vec<String> = errors.iter().filter_map(|e| label_of(&e.name)).collect();
            if wrong.is_empty() {
                return Some("Check the form".to_owned());
            }
            ("Fix", wrong)
        } else {
            ("Fill in", missing)
        };
        Some(match names.as_slice() {
            [one] => format!("{verb} {one}"),
            [one, two] => format!("{verb} {one} and {two}"),
            [one, rest @ ..] => format!("{verb} {one} and {} more", rest.len()),
            [] => format!("{verb} the form"),
        })
    }

    /// Whether any field has an optional kind this build cannot fill in: it is left out of
    /// the answer, and the card says so.
    pub fn left_out_unknown(&self, index: usize) -> bool {
        self.fields
            .get(index)
            .is_some_and(|f| !f.required && matches!(f.input, FieldInput::Unknown))
    }
}

fn blank(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        _ => false,
    }
}

/// "Choose 1 to 3", "Choose at least 2", "Choose up to 3", "Choose any": what a multi-choice
/// field asks, before the user has picked anything.
pub fn choose_hint(min: Option<u64>, max: Option<u64>) -> String {
    match (min.filter(|m| *m > 0), max) {
        (Some(min), Some(max)) if min == max => format!("Choose {min}"),
        (Some(min), Some(max)) => format!("Choose {min} to {max}"),
        (Some(min), None) => format!("Choose at least {min}"),
        (None, Some(max)) => format!("Choose up to {max}"),
        (None, None) => "Choose any".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::form::ChoiceOption;
    use serde_json::json;

    fn field(name: &str, required: bool, input: FieldInput) -> FormField {
        FormField {
            name: name.into(),
            title: None,
            description: None,
            required,
            input,
        }
    }

    fn text(default: Option<&str>) -> FieldInput {
        FieldInput::Text {
            format: None,
            min_length: None,
            max_length: None,
            default: default.map(str::to_owned),
        }
    }

    fn number(minimum: Option<f64>) -> FieldInput {
        FieldInput::Number {
            integer: true,
            minimum,
            maximum: None,
            default: None,
        }
    }

    #[test]
    fn nothing_is_sent_for_a_field_nobody_touched() {
        let fields = [
            field("name", false, text(None)),
            field("count", false, number(Some(1.0))),
            field("on", false, FieldInput::Boolean { default: None }),
            field("kind", false, choice(false, &["a", "b"], &[])),
            field("tags", false, choice(true, &["a", "b"], &[])),
        ];
        let draft = Draft::new(&fields);
        assert_eq!(
            draft.content(),
            json!({}),
            "no 0, no false, no first option"
        );
        assert!(draft
            .check()
            .expect("optional fields may stay empty")
            .is_empty());
        assert_eq!(draft.submit_hint(), None);
    }

    fn choice(multi: bool, values: &[&str], default: &[&str]) -> FieldInput {
        FieldInput::Choice {
            options: values
                .iter()
                .map(|v| ChoiceOption {
                    value: (*v).into(),
                    label: v.to_uppercase(),
                })
                .collect(),
            multi,
            min_items: None,
            max_items: None,
            default: default.iter().map(|d| (*d).to_owned()).collect(),
        }
    }

    #[test]
    fn defaults_are_prefilled_and_sent_untouched() {
        let fields = [
            field("name", false, text(Some("octo"))),
            field(
                "on",
                false,
                FieldInput::Boolean {
                    default: Some(false),
                },
            ),
            field("kind", false, choice(false, &["a", "b"], &["b"])),
            field("tags", false, choice(true, &["a", "b"], &["a", "zzz"])),
        ];
        let draft = Draft::new(&fields);
        assert_eq!(
            draft.content(),
            json!({"name": "octo", "on": false, "kind": "b", "tags": ["a"]}),
            "a default of false is a default; an unknown default value is dropped"
        );
    }

    #[test]
    fn a_missing_required_field_names_itself_on_the_disabled_submit() {
        let fields = [
            field("repo_name", true, text(None)),
            field("branch", true, text(None)),
            field("depth", true, number(None)),
            field("note", false, text(None)),
        ];
        let mut draft = Draft::new(&fields);
        assert_eq!(
            draft.submit_hint().as_deref(),
            Some("Fill in Repo name and 2 more")
        );
        draft.set(0, json!("agent-terminal"));
        assert_eq!(
            draft.submit_hint().as_deref(),
            Some("Fill in Branch and Depth")
        );
        draft.set(1, json!("main"));
        assert_eq!(draft.submit_hint().as_deref(), Some("Fill in Depth"));
        draft.set(2, number_value("abc"));
        assert_eq!(draft.submit_hint().as_deref(), Some("Fix Depth"));
        assert_eq!(draft.error(2).as_deref(), Some("Enter a number."));
        draft.set(2, number_value("3"));
        assert_eq!(draft.submit_hint(), None);
        assert_eq!(
            draft.check().expect("valid"),
            json!({"repo_name": "agent-terminal", "branch": "main", "depth": 3})
                .as_object()
                .cloned()
                .expect("object")
        );
    }

    #[test]
    fn numbers_are_validated_never_defaulted() {
        assert_eq!(number_value(""), Value::Null);
        assert_eq!(number_value("  "), Value::Null);
        assert_eq!(number_value("12"), json!(12));
        assert_eq!(number_value("-3"), json!(-3));
        assert_eq!(number_value("2.5"), json!(2.5));
        assert_eq!(number_value("NaN"), json!("NaN"));
        assert_eq!(number_value("1e999"), json!("1e999"), "no infinity");
        let fields = [field("n", true, number(Some(1.0)))];
        let mut draft = Draft::new(&fields);
        assert_eq!(draft.submit_hint().as_deref(), Some("Fill in N"));
        draft.set(0, number_value("0"));
        assert_eq!(draft.error(0).as_deref(), Some("Enter 1 or more."));
        draft.set(0, number_value("1.5"));
        assert_eq!(draft.error(0).as_deref(), Some("Enter a whole number."));
    }

    #[test]
    fn a_required_field_that_cannot_be_filled_in_blocks_submit() {
        let fields = [
            field("legacy", true, FieldInput::Unknown),
            field("extra", false, FieldInput::Unknown),
        ];
        let draft = Draft::new(&fields);
        assert!(draft.submit_hint().is_some());
        assert!(!draft.left_out_unknown(0));
        assert!(draft.left_out_unknown(1));
        let optional_only = Draft::new(&fields[1..]);
        assert_eq!(optional_only.submit_hint(), None, "left out, not blocked");
    }

    #[test]
    fn choose_hints_read_like_the_limits() {
        assert_eq!(choose_hint(Some(1), Some(3)), "Choose 1 to 3");
        assert_eq!(choose_hint(Some(2), Some(2)), "Choose 2");
        assert_eq!(choose_hint(Some(2), None), "Choose at least 2");
        assert_eq!(choose_hint(None, Some(3)), "Choose up to 3");
        assert_eq!(choose_hint(Some(0), None), "Choose any");
        assert_eq!(choose_hint(None, None), "Choose any");
    }
}
