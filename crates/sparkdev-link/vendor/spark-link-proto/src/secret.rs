//! Redacting secret types (design 15.4 "Secrets in logs").
//!
//! `Debug` and `Display` never print the value. `Serialize` *does* write the
//! real value, because these types must cross the wire (the pairing link, the
//! IPC bootstrap password). Never log serialized frames that contain them:
//! log the ticket_id only, and never log `/bootstrap/*` or `/pair/*` bodies.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::ProtoError;
use crate::ids::{b64url, b64url_decode_exact};

/// The 128-bit single-use pairing secret carried in the QR/link `s=` field.
/// The node keeps only the derived HKDF keys, in memory (item 11).
#[derive(Clone, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct PairingSecret([u8; 16]);

impl PairingSecret {
    pub const LEN: usize = 16;
    pub fn random() -> Self {
        Self(crate::random_bytes::<16>())
    }
    pub fn from_bytes(b: [u8; 16]) -> Self {
        Self(b)
    }
    pub fn expose(&self) -> &[u8; 16] {
        &self.0
    }
    /// base64url (no padding) text form used in links. Treat the result as secret.
    pub fn expose_b64(&self) -> String {
        b64url(&self.0)
    }
    pub fn from_b64(s: &str) -> Result<Self, ProtoError> {
        b64url_decode_exact::<16>(s, "pairing secret").map(Self)
    }
}
impl fmt::Debug for PairingSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairingSecret([redacted])")
    }
}
impl Serialize for PairingSecret {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.expose_b64())
    }
}
impl<'de> Deserialize<'de> for PairingSecret {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut s = String::deserialize(d)?;
        let r = PairingSecret::from_b64(&s).map_err(serde::de::Error::custom);
        s.zeroize();
        r
    }
}

/// A secret string (SSH password, key passphrase, a pairing link held as text).
/// Zeroized on drop; redacted in Debug/Display.
#[derive(Clone, PartialEq, Eq, Zeroize, ZeroizeOnDrop, Default)]
pub struct RedactedString(String);

impl RedactedString {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
impl fmt::Debug for RedactedString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}
impl fmt::Display for RedactedString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}
impl Serialize for RedactedString {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for RedactedString {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self(String::deserialize(d)?))
    }
}

/// Replace any pairing link (`delores-link://...` or `https://link.deloresai.com/p/...`)
/// and any `s=`/`t=` query or fragment parameter in `text` with `[redacted]`.
/// Use before logging free text that might contain a link.
pub fn redact_links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    const PREFIXES: [&str; 2] = ["delores-link://", "https://link.deloresai.com/p/"];
    loop {
        let next = PREFIXES.iter().filter_map(|p| rest.find(p).map(|i| (i, *p))).min_by_key(|(i, _)| *i);
        match next {
            None => {
                out.push_str(rest);
                break;
            }
            Some((i, _)) => {
                out.push_str(&rest[..i]);
                out.push_str("[redacted-link]");
                let tail = &rest[i..];
                let end = tail.find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '<' || c == '>').unwrap_or(tail.len());
                rest = &tail[end..];
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_is_redacted() {
        let s = PairingSecret::from_bytes([7; 16]);
        assert_eq!(format!("{s:?}"), "PairingSecret([redacted])");
        let p = RedactedString::new("hunter2");
        assert_eq!(format!("{p:?} {p}"), "[redacted] [redacted]");
    }

    #[test]
    fn redacts_links() {
        let t = "open delores-link://pair/v1?n=abc&s=SECRET now and https://link.deloresai.com/p/v1#s=X\tdone";
        let r = redact_links(t);
        assert!(!r.contains("SECRET") && !r.contains("#s=X"));
        assert!(r.contains("now and") && r.ends_with("\tdone"));
    }
}
