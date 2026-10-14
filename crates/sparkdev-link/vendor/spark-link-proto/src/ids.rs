//! Identities and fixed-size byte values.
//!
//! Text encodings (normative, see PROTOCOL.md section 2):
//! - [`EndpointId`]: z-base-32 (alphabet `ybndrfg8ejkmcpqxot1uwisza345h769`,
//!   no padding, 52 chars), identical to `iroh::PublicKey::to_z32`. Parsing
//!   also accepts 64 lowercase hex chars (iroh's `Display`).
//! - [`TicketId`], [`PairingId`], [`Nonce16`], [`Nonce32`], [`Mac32`]:
//!   base64url without padding.

use std::fmt;
use std::str::FromStr;
use std::sync::LazyLock;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use data_encoding::{Encoding, Specification};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ProtoError;

pub(crate) static Z32: LazyLock<Encoding> = LazyLock::new(|| {
    let mut spec = Specification::new();
    spec.symbols.push_str("ybndrfg8ejkmcpqxot1uwisza345h769");
    spec.encoding().expect("valid z-base-32 specification")
});

pub(crate) fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub(crate) fn b64url_decode_exact<const N: usize>(s: &str, what: &'static str) -> Result<[u8; N], ProtoError> {
    // Reject anything that is not canonical base64url-no-pad of exactly N bytes.
    let expected_len = (N * 4).div_ceil(3);
    if s.len() != expected_len {
        return Err(ProtoError::Length(what));
    }
    let v = URL_SAFE_NO_PAD.decode(s).map_err(|_| ProtoError::Encoding(what))?;
    let arr: [u8; N] = v.try_into().map_err(|_| ProtoError::Length(what))?;
    if b64url(&arr) != s {
        return Err(ProtoError::Encoding(what));
    }
    Ok(arr)
}

pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(bytes)
}

/// An iroh endpoint identity: the 32-byte Ed25519 public key. Used for both
/// node (Spark) and device identities. Conversion to `iroh::PublicKey` is done
/// in the node/agent crates with `PublicKey::from_bytes(id.as_bytes())`, which
/// additionally checks the point is valid; treat a failure as `bad_request`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EndpointId([u8; 32]);

impl EndpointId {
    pub const LEN: usize = 32;
    /// Length of the z-base-32 text form.
    pub const Z32_LEN: usize = 52;

    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
    pub fn to_z32(&self) -> String {
        Z32.encode(&self.0)
    }
    pub fn from_z32(s: &str) -> Result<Self, ProtoError> {
        if s.len() != Self::Z32_LEN {
            return Err(ProtoError::Length("endpoint id"));
        }
        let v = Z32.decode(s.as_bytes()).map_err(|_| ProtoError::Encoding("endpoint id"))?;
        let arr: [u8; 32] = v.try_into().map_err(|_| ProtoError::Length("endpoint id"))?;
        Ok(Self(arr))
    }
    /// First 10 z-base-32 characters, for logs and UI. Never used for identity.
    pub fn short(&self) -> String {
        self.to_z32()[..10].to_string()
    }
}

impl fmt::Display for EndpointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_z32())
    }
}
impl fmt::Debug for EndpointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EndpointId({})", self.short())
    }
}
impl FromStr for EndpointId {
    type Err = ProtoError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            let v = data_encoding::HEXLOWER.decode(s.as_bytes()).map_err(|_| ProtoError::Encoding("endpoint id"))?;
            let arr: [u8; 32] = v.try_into().map_err(|_| ProtoError::Length("endpoint id"))?;
            return Ok(Self(arr));
        }
        Self::from_z32(s)
    }
}
impl Serialize for EndpointId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_z32())
    }
}
impl<'de> Deserialize<'de> for EndpointId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        EndpointId::from_str(&s).map_err(serde::de::Error::custom)
    }
}

macro_rules! b64_bytes {
    ($(#[$m:meta])* $name:ident, $n:expr, $what:expr, $redact:expr) => {
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name([u8; $n]);
        impl $name {
            pub const LEN: usize = $n;
            pub const fn from_bytes(b: [u8; $n]) -> Self { Self(b) }
            pub const fn as_bytes(&self) -> &[u8; $n] { &self.0 }
            /// Fresh value from the OS CSPRNG.
            pub fn random() -> Self { Self(crate::random_bytes::<$n>()) }
            pub fn to_b64(&self) -> String { b64url(&self.0) }
            pub fn from_b64(s: &str) -> Result<Self, ProtoError> { b64url_decode_exact::<$n>(s, $what).map(Self) }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.to_b64()) }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                if $redact { write!(f, concat!(stringify!($name), "([redacted])")) }
                else { write!(f, concat!(stringify!($name), "({})"), self.to_b64()) }
            }
        }
        impl FromStr for $name {
            type Err = ProtoError;
            fn from_str(s: &str) -> Result<Self, Self::Err> { Self::from_b64(s) }
        }
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> { s.serialize_str(&self.to_b64()) }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                Self::from_b64(&s).map_err(serde::de::Error::custom)
            }
        }
    };
}

b64_bytes!(
    /// 128-bit random ticket identifier (design 15.2 item 11). Logged freely.
    TicketId, 16, "ticket id", false);
b64_bytes!(
    /// 128-bit random identifier of a pending pairing (item 1).
    PairingId, 16, "pairing id", false);
b64_bytes!(
    /// 128-bit nonce; `n_s` in `PairPending` (item 1).
    Nonce16, 16, "nonce16", false);
b64_bytes!(
    /// 256-bit nonce; the IPC handshake client nonce.
    Nonce32, 32, "nonce32", false);
b64_bytes!(
    /// HMAC-SHA256 output (pairing `mac`/`mac2`, IPC handshake `proof`).
    Mac32, 32, "mac", false);
b64_bytes!(
    /// A 32-byte symmetric key, e.g. the IPC `server_key` in endpoint.json.
    /// Debug output is redacted.
    Key32, 32, "key", true);

/// Application identifier carried in `X-Delores-App` and grants.
/// Grammar: `^[a-z][a-z0-9-]{1,31}$`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct AppId(String);

impl AppId {
    pub const PHASEFORGE: &'static str = "phaseforge";
    pub const TUNINGSPORK: &'static str = "tuningspork";
    /// First-party Delores apps. Third-party apps are capped at `infer`
    /// (design 15.2 item 12) by [`crate::scopes::app_scope_cap`].
    pub const FIRST_PARTY: [&'static str; 2] = [Self::PHASEFORGE, Self::TUNINGSPORK];

    pub fn new(s: &str) -> Result<Self, ProtoError> {
        let b = s.as_bytes();
        let ok = (2..=32).contains(&b.len())
            && b[0].is_ascii_lowercase()
            && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-');
        if ok { Ok(Self(s.to_string())) } else { Err(ProtoError::Invalid { field: "app_id", reason: "must match ^[a-z][a-z0-9-]{1,31}$" }) }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn is_first_party(&self) -> bool {
        Self::FIRST_PARTY.contains(&self.0.as_str())
    }
}
impl fmt::Display for AppId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl FromStr for AppId {
    type Err = ProtoError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}
impl Serialize for AppId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for AppId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        AppId::new(&s).map_err(serde::de::Error::custom)
    }
}

/// Kind of a paired device, shown on approval cards.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Desktop,
    Laptop,
    Phone,
    #[default]
    Other,
}

/// Maximum length in characters of a device name or Spark label.
pub const LABEL_MAX_CHARS: usize = 64;

/// Normalise a human label (Spark label, device name): strip control and
/// bidi-override characters, collapse whitespace, trim, cap at
/// [`LABEL_MAX_CHARS`]. Returns `None` if nothing printable remains.
/// Labels are display hints, never identity (item 9: "name from the code").
pub fn sanitize_label(input: &str) -> Option<String> {
    let mut out = String::new();
    let mut last_space = true;
    for ch in input.chars() {
        let bidi = matches!(ch, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}');
        if ch.is_control() || bidi {
            continue;
        }
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
            continue;
        }
        out.push(ch);
        last_space = false;
        if out.chars().count() >= LABEL_MAX_CHARS {
            break;
        }
    }
    let t = out.trim().to_string();
    if t.is_empty() { None } else { Some(t) }
}
