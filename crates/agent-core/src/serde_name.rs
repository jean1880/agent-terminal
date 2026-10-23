//! A unit enum value by its serde name, the way the store and the wire spell it.

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

/// The serde name of `v` (a unit enum value), or `None` if it does not serialise to a string.
pub fn name_of<T: Serialize + ?Sized>(v: &T) -> Option<String> {
    match serde_json::to_value(v) {
        Ok(Value::String(s)) => Some(s),
        _ => None,
    }
}

/// The value named `name`; a name this build does not know is an error.
pub fn parse_strict<T: DeserializeOwned>(name: &str) -> Result<T, serde_json::Error> {
    serde_json::from_value(Value::String(name.to_owned()))
}

/// The value named `name`, or `None` for a name this build does not know.
pub fn parse_lenient<T: DeserializeOwned>(name: &str) -> Option<T> {
    parse_strict(name).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::Driver;
    use crate::event::TurnState;

    #[test]
    fn names_round_trip_and_unknown_names_are_refused() {
        assert_eq!(
            name_of(&TurnState::Interrupted).as_deref(),
            Some("interrupted")
        );
        assert_eq!(name_of(&1u8), None);
        assert_eq!(
            parse_strict::<TurnState>("failed").ok(),
            Some(TurnState::Failed)
        );
        assert!(parse_strict::<TurnState>("later").is_err());
        assert_eq!(parse_lenient::<TurnState>("later"), None);
        let claude = name_of(&Driver::Claude).unwrap_or_default();
        assert_eq!(parse_lenient::<Driver>(&claude), Some(Driver::Claude));
    }
}
