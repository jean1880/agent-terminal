//! Handles and small identifiers of the delegation contract.
//!
//! Handles are opaque ASCII, `^[a-z]{1,3}_[A-Za-z0-9]{8,40}$`, each with its own prefix. The app
//! supplies the random suffix ([`WorkerId::from_suffix`] and friends); nothing here draws
//! randomness. Handles are not secrets, so their `Debug` prints the value.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Why a handle or identifier was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdError {
    /// The value is empty, or a handle's suffix is shorter than its minimum.
    Empty,
    /// Longer than its bound.
    TooLong,
    /// A handle without its prefix (or a token without `atdc_`).
    BadPrefix,
    /// A character outside the allowed set.
    BadChar,
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            IdError::Empty => "empty or too short",
            IdError::TooLong => "too long",
            IdError::BadPrefix => "wrong prefix",
            IdError::BadChar => "invalid character",
        })
    }
}

impl std::error::Error for IdError {}

/// Shortest and longest handle suffix.
pub const HANDLE_SUFFIX_MIN: usize = 8;
pub const HANDLE_SUFFIX_MAX: usize = 40;

fn parse_handle(s: &str, prefix: &str) -> Result<(), IdError> {
    if s.is_empty() {
        return Err(IdError::Empty);
    }
    if s.len() > prefix.len() + HANDLE_SUFFIX_MAX {
        return Err(IdError::TooLong);
    }
    let Some(suffix) = s.strip_prefix(prefix) else {
        return Err(IdError::BadPrefix);
    };
    if !suffix.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(IdError::BadChar);
    }
    if suffix.len() < HANDLE_SUFFIX_MIN {
        return Err(IdError::Empty);
    }
    Ok(())
}

macro_rules! handle {
    ($(#[$doc:meta])* $name:ident, $prefix:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub const PREFIX: &'static str = $prefix;

            pub fn parse(s: &str) -> Result<Self, IdError> {
                parse_handle(s, Self::PREFIX)?;
                Ok(Self(s.to_owned()))
            }

            /// The handle with this random suffix (the app draws it; 8..=40 alphanumerics).
            pub fn from_suffix(suffix: &str) -> Result<Self, IdError> {
                Self::parse(&format!("{}{}", Self::PREFIX, suffix))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;
            fn try_from(s: String) -> Result<Self, IdError> {
                parse_handle(&s, Self::PREFIX)?;
                Ok(Self(s))
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> String {
                id.0
            }
        }
    };
}

handle!(
    /// A worker: a root session or a supervised child (`w_…`).
    WorkerId,
    "w_"
);
handle!(
    /// One run (turn) of a worker (`r_…`).
    RunId,
    "r_"
);
handle!(
    /// One mailbox message (`m_…`).
    MessageId,
    "m_"
);
handle!(
    /// One accepted mutation (`op_…`).
    OperationId,
    "op_"
);

macro_rules! checked_string {
    ($(#[$doc:meta])* $name:ident, $check:expr) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn parse(s: &str) -> Result<Self, IdError> {
                let check: fn(&str) -> Result<(), IdError> = $check;
                check(s)?;
                Ok(Self(s.to_owned()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;
            fn try_from(s: String) -> Result<Self, IdError> {
                let check: fn(&str) -> Result<(), IdError> = $check;
                check(&s)?;
                Ok(Self(s))
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> String {
                id.0
            }
        }
    };
}

/// Longest cursor, in bytes.
pub const MAX_CURSOR_BYTES: usize = 256;
/// Longest idempotency key.
pub const MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;
/// Longest profile id.
pub const MAX_PROFILE_ID_BYTES: usize = 64;

fn bounded(s: &str, max: usize, ok: impl Fn(u8) -> bool) -> Result<(), IdError> {
    if s.is_empty() {
        return Err(IdError::Empty);
    }
    if s.len() > max {
        return Err(IdError::TooLong);
    }
    if !s.bytes().all(ok) {
        return Err(IdError::BadChar);
    }
    Ok(())
}

checked_string!(
    /// An opaque page cursor, 1..=256 bytes. Its content is checked by `page::decode_cursor`.
    Cursor,
    |s| bounded(s, MAX_CURSOR_BYTES, |b| (0x21..0x7f).contains(&b))
);
impl Cursor {
    /// A cursor `page::encode_cursor` built: base64url, well under 256 bytes.
    pub(crate) fn from_encoded(s: String) -> Self {
        Self(s)
    }
}

checked_string!(
    /// `^[A-Za-z0-9._:-]{1,128}$`.
    IdempotencyKey,
    |s| bounded(s, MAX_IDEMPOTENCY_KEY_BYTES, |b| b.is_ascii_alphanumeric()
        || matches!(b, b'.' | b'_' | b':' | b'-'))
);
checked_string!(
    /// `^[a-z0-9-]{1,64}$`.
    ProfileId,
    |s| bounded(s, MAX_PROFILE_ID_BYTES, |b| b.is_ascii_lowercase()
        || b.is_ascii_digit()
        || b == b'-')
);

/// A root tree's authority epoch. Every revocation bumps it once.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct AuthorityEpoch(pub u64);

/// A worker's process generation: one OS process for Claude and agy, one
/// `thread/start` or `thread/resume` for Codex.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct ProcessGeneration(pub u64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_accept_the_documented_shapes() {
        assert!(WorkerId::parse("w_abcdEFGH").is_ok());
        assert!(WorkerId::parse(&format!("w_{}", "a".repeat(40))).is_ok());
        assert!(RunId::parse("r_12345678").is_ok());
        assert!(MessageId::parse("m_A1b2C3d4").is_ok());
        assert!(OperationId::parse("op_A1b2C3d4").is_ok());
        assert_eq!(
            WorkerId::from_suffix("abcdefgh").map(|w| w.as_str().to_owned()),
            Ok("w_abcdefgh".to_owned())
        );
        assert_eq!(
            OperationId::from_suffix("x1234567").map(|o| o.to_string()),
            Ok("op_x1234567".to_owned())
        );
    }

    #[test]
    fn handles_refuse_empty_long_prefix_and_chars() {
        assert_eq!(WorkerId::parse(""), Err(IdError::Empty));
        assert_eq!(WorkerId::parse("w_abc"), Err(IdError::Empty));
        assert_eq!(
            WorkerId::parse(&format!("w_{}", "a".repeat(41))),
            Err(IdError::TooLong)
        );
        assert_eq!(WorkerId::parse("r_abcdefgh"), Err(IdError::BadPrefix));
        assert_eq!(RunId::parse("w_abcdefgh"), Err(IdError::BadPrefix));
        assert_eq!(WorkerId::parse("w_abcd-fgh"), Err(IdError::BadChar));
        assert_eq!(WorkerId::parse("w_abcdéfgh"), Err(IdError::BadChar));
        assert_eq!(WorkerId::from_suffix("abc_defgh"), Err(IdError::BadChar));
    }

    #[test]
    fn keys_cursors_and_profiles_follow_their_patterns() {
        assert!(IdempotencyKey::parse("a.b_c:d-9").is_ok());
        assert!(IdempotencyKey::parse(&"k".repeat(128)).is_ok());
        assert_eq!(
            IdempotencyKey::parse(&"k".repeat(129)),
            Err(IdError::TooLong)
        );
        assert_eq!(IdempotencyKey::parse(""), Err(IdError::Empty));
        assert_eq!(IdempotencyKey::parse("a b"), Err(IdError::BadChar));
        assert!(ProfileId::parse("codex-2").is_ok());
        assert_eq!(ProfileId::parse("Codex"), Err(IdError::BadChar));
        assert_eq!(ProfileId::parse(&"p".repeat(65)), Err(IdError::TooLong));
        assert!(Cursor::parse(&"c".repeat(256)).is_ok());
        assert_eq!(Cursor::parse(&"c".repeat(257)), Err(IdError::TooLong));
        assert_eq!(Cursor::parse(""), Err(IdError::Empty));
        assert_eq!(Cursor::parse("a\u{1}"), Err(IdError::BadChar));
    }

    #[test]
    fn serde_is_transparent_and_validates() {
        let w = WorkerId::parse("w_abcdefgh").expect("valid");
        assert_eq!(
            serde_json::to_value(&w).expect("ser"),
            serde_json::json!("w_abcdefgh")
        );
        let back: WorkerId = serde_json::from_str("\"w_abcdefgh\"").expect("de");
        assert_eq!(back, w);
        assert!(serde_json::from_str::<WorkerId>("\"r_abcdefgh\"").is_err());
        let key: IdempotencyKey = serde_json::from_str("\"k1\"").expect("de");
        assert_eq!(serde_json::to_string(&key).expect("ser"), "\"k1\"");
        assert_eq!(serde_json::to_string(&AuthorityEpoch(4)).expect("ser"), "4");
        assert_eq!(
            serde_json::from_str::<ProcessGeneration>("7").expect("de"),
            ProcessGeneration(7)
        );
    }
}
