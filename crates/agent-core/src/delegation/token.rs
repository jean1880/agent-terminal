//! The delegation capability token and the app-instance nonce (contract §5).
//!
//! The app draws the random bytes; these types only encode, parse and compare them. The token
//! is a secret: [`CapabilityToken::expose_secret`] is its only accessor, its `Debug` prints
//! `atdc_****`, and it has no `Display`.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::ids::IdError;

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Unpadded base64url (RFC 4648 §5).
pub(crate) fn b64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = chunk.len() + 1;
        for i in 0..chars {
            let idx = (n >> (18 - 6 * i)) & 0x3f;
            out.push(char::from(B64URL[idx as usize]));
        }
    }
    out
}

fn b64url_value(c: u8) -> Option<u32> {
    let v = match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'-' => 62,
        b'_' => 63,
        _ => return None,
    };
    Some(u32::from(v))
}

/// Decodes unpadded base64url; `None` on any other character, an impossible length, or
/// non-zero trailing bits (only the canonical encoding is accepted).
pub(crate) fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if bytes.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= b64url_value(c)? << (18 - 6 * i);
        }
        let produced = chunk.len() - 1;
        let unused_bits = 24 - 8 * produced;
        if n & ((1 << unused_bits) - 1) != 0 && chunk.len() < 4 {
            return None;
        }
        for i in 0..produced {
            out.push(((n >> (16 - 8 * i)) & 0xff) as u8);
        }
    }
    Some(out)
}

/// Equal-length comparison whose time depends only on the length.
fn ct_eq_bytes(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// `atdc_` + base64url(32 random bytes), no padding: 48 characters. One per principal and
/// process generation.
#[derive(Clone)]
pub struct CapabilityToken(String);

impl CapabilityToken {
    pub const PREFIX: &'static str = "atdc_";
    /// Base64url characters after the prefix.
    pub const SECRET_CHARS: usize = 43;
    /// Total length.
    pub const LEN: usize = Self::PREFIX.len() + Self::SECRET_CHARS;

    pub fn from_random(bytes: [u8; 32]) -> Self {
        Self(format!("{}{}", Self::PREFIX, b64url_encode(&bytes)))
    }

    pub fn parse(s: &str) -> Result<Self, IdError> {
        if s.is_empty() {
            return Err(IdError::Empty);
        }
        let Some(rest) = s.strip_prefix(Self::PREFIX) else {
            return Err(IdError::BadPrefix);
        };
        if rest.len() > Self::SECRET_CHARS {
            return Err(IdError::TooLong);
        }
        if rest.len() < Self::SECRET_CHARS {
            return Err(IdError::Empty);
        }
        match b64url_decode(rest) {
            Some(bytes) if bytes.len() == 32 => Ok(Self(s.to_owned())),
            _ => Err(IdError::BadChar),
        }
    }

    /// The token itself: only for the spawn environment and the constant-time check.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    /// Constant time over equal-length input; a length mismatch is `false` at once (the
    /// length is public).
    pub fn ct_eq(&self, other: &str) -> bool {
        ct_eq_bytes(self.0.as_bytes(), other.as_bytes())
    }
}

impl PartialEq for CapabilityToken {
    fn eq(&self, other: &Self) -> bool {
        self.ct_eq(&other.0)
    }
}

impl Eq for CapabilityToken {}

impl fmt::Debug for CapabilityToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}****", Self::PREFIX)
    }
}

impl Serialize for CapabilityToken {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for CapabilityToken {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        // Never echo the value in the error.
        CapabilityToken::parse(&s)
            .map_err(|e| serde::de::Error::custom(format!("invalid capability token: {e}")))
    }
}

/// The 128-bit app-instance nonce: 32 lowercase hex characters. Not a secret, never stored.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct InstanceNonce(String);

impl InstanceNonce {
    pub const LEN: usize = 32;

    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        let mut s = String::with_capacity(Self::LEN);
        for b in bytes {
            s.push(char::from(b"0123456789abcdef"[usize::from(b >> 4)]));
            s.push(char::from(b"0123456789abcdef"[usize::from(b & 0xf)]));
        }
        Self(s)
    }

    pub fn parse(s: &str) -> Result<Self, IdError> {
        if s.is_empty() {
            return Err(IdError::Empty);
        }
        if s.len() > Self::LEN {
            return Err(IdError::TooLong);
        }
        if s.len() < Self::LEN {
            return Err(IdError::Empty);
        }
        if !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(IdError::BadChar);
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn ct_eq(&self, other: &str) -> bool {
        ct_eq_bytes(self.0.as_bytes(), other.as_bytes())
    }
}

impl TryFrom<String> for InstanceNonce {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, IdError> {
        InstanceNonce::parse(&s)
    }
}

impl From<InstanceNonce> for String {
    fn from(n: InstanceNonce) -> String {
        n.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redact::redact;
    use crate::redact::stream::{StreamKey, StreamSanitiser};

    fn token() -> CapabilityToken {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        CapabilityToken::from_random(bytes)
    }

    #[test]
    fn base64url_round_trips_every_length() {
        for len in 0..40 {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 53 + 7) as u8).collect();
            let enc = b64url_encode(&bytes);
            assert!(!enc.contains('='));
            assert_eq!(b64url_decode(&enc), Some(bytes));
        }
        assert_eq!(b64url_encode(b"\xfb\xff"), "-_8");
        assert_eq!(b64url_decode("a"), None);
        assert_eq!(b64url_decode("ab+c"), None);
        // Non-canonical trailing bits.
        assert_eq!(b64url_decode("-_9"), None);
    }

    #[test]
    fn from_random_has_the_prefix_and_48_chars_and_parses_back() {
        let t = token();
        assert!(t.expose_secret().starts_with("atdc_"));
        assert_eq!(t.expose_secret().len(), 48);
        assert_eq!(CapabilityToken::parse(t.expose_secret()), Ok(t.clone()));
        let zeros = CapabilityToken::from_random([0; 32]);
        assert_eq!(zeros.expose_secret(), format!("atdc_{}", "A".repeat(43)));
    }

    #[test]
    fn parse_refuses_malformed_tokens() {
        let t = token();
        let s = t.expose_secret();
        assert_eq!(CapabilityToken::parse(""), Err(IdError::Empty));
        assert_eq!(
            CapabilityToken::parse(&s.replace("atdc_", "atdx_")),
            Err(IdError::BadPrefix)
        );
        assert_eq!(
            CapabilityToken::parse(&format!("{s}A")),
            Err(IdError::TooLong)
        );
        assert_eq!(CapabilityToken::parse(&s[..47]), Err(IdError::Empty));
        let mut bad = s.to_owned();
        bad.replace_range(10..11, "+");
        assert_eq!(CapabilityToken::parse(&bad), Err(IdError::BadChar));
    }

    #[test]
    fn debug_and_serde_never_leak_into_logs() {
        let t = token();
        let dbg = format!("{t:?}");
        assert_eq!(dbg, "atdc_****");
        assert!(!dbg.contains(&t.expose_secret()[5..]));
        let json = serde_json::to_string(&t).expect("ser");
        assert_eq!(json, format!("\"{}\"", t.expose_secret()));
        let back: CapabilityToken = serde_json::from_str(&json).expect("de");
        assert_eq!(back, t);
        let err = serde_json::from_str::<CapabilityToken>("\"atdc_short_secretvalue\"")
            .expect_err("refused");
        assert!(!err.to_string().contains("short_secretvalue"));
    }

    #[test]
    fn ct_eq_compares_correctly() {
        let t = token();
        let s = t.expose_secret().to_owned();
        assert!(t.ct_eq(&s));
        let mut other = s.clone();
        other.replace_range(47..48, if s.ends_with('A') { "B" } else { "A" });
        assert!(!t.ct_eq(&other));
        assert!(!t.ct_eq(&s[..47]));
        assert!(!t.ct_eq(""));
        assert!(!t.ct_eq(&format!("{s}x")));
        assert_ne!(t, CapabilityToken::from_random([9; 32]));
    }

    #[test]
    fn redact_masks_the_token() {
        let t = token();
        let secret = t.expose_secret();
        let text = format!("export AGENT_X={secret} and plain {secret}.");
        let out = redact(&text);
        assert!(!out.contains(&secret[..40]), "{out}");
        let bare = redact(&format!("cap {secret} end"));
        assert!(!bare.contains(&secret[..40]), "{bare}");
        assert!(bare.contains("****"));
    }

    #[test]
    fn stream_sanitiser_masks_the_token_at_every_split() {
        let t = token();
        let secret = t.expose_secret();
        let text = format!("token is {secret} ok\n");
        let whole = redact(&text);
        let key = StreamKey {
            worker: "w_abcdefgh".into(),
            generation: 1,
            item: "i".into(),
            stream: "assistant".into(),
        };
        for split in 0..=text.len() {
            if !text.is_char_boundary(split) {
                continue;
            }
            let mut s = StreamSanitiser::default();
            let mut out = s.push(&key, &text[..split]).text;
            out.push_str(&s.push(&key, &text[split..]).text);
            out.push_str(&s.finish(&key).text);
            assert_eq!(out, whole, "split at {split}");
            assert!(!out.contains(&secret[..40]));
        }
    }

    #[test]
    fn instance_nonce_is_32_lowercase_hex() {
        let n = InstanceNonce::from_bytes([0xab; 16]);
        assert_eq!(n.as_str(), "ab".repeat(16));
        assert_eq!(InstanceNonce::parse(n.as_str()), Ok(n.clone()));
        assert_eq!(
            InstanceNonce::parse(&"AB".repeat(16)),
            Err(IdError::BadChar)
        );
        assert_eq!(InstanceNonce::parse("ab"), Err(IdError::Empty));
        assert_eq!(InstanceNonce::parse(&"a".repeat(33)), Err(IdError::TooLong));
        assert!(n.ct_eq(&"ab".repeat(16)));
        assert!(!n.ct_eq(&"ac".repeat(16)));
        assert!(!n.ct_eq("ab"));
        let json = serde_json::to_string(&n).expect("ser");
        assert_eq!(serde_json::from_str::<InstanceNonce>(&json).expect("de"), n);
    }
}
