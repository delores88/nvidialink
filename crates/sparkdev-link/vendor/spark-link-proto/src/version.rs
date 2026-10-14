//! Versioning (design section 10 "Version policy").
//!
//! - ALPNs carry the protocol major: `delores/spark-link/pair/1`, `delores/spark-link/http/1`.
//! - HTTP routes carry the API major: `/link/v1/...`.
//! - The IPC carries `spark-link/ipc/v1` in the handshake.
//! - `ProtocolRange` (major.minor) is exchanged in `NodeInfo` and the IPC hello.
//!   A node supports the current and the previous minor; an agent refuses a
//!   node whose range does not overlap its own and reports `version_unsupported`.

use serde::{Deserialize, Serialize};

/// ALPN for unauthenticated pairing (5b). Accepted only while an unexpired
/// ticket exists (S1). No 0-RTT on any ALPN (item 11).
pub const ALPN_PAIR: &[u8] = b"delores/spark-link/pair/1";
/// ALPN for the node HTTP/1.1 API; paired, non-denylisted devices only.
pub const ALPN_HTTP: &[u8] = b"delores/spark-link/http/1";

/// URL prefix of the node control API.
pub const API_PREFIX: &str = "/link/v1";

/// A protocol version: major must match exactly; minor is additive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProtoVersion {
    pub major: u16,
    pub minor: u16,
}

/// Inclusive supported range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProtocolRange {
    pub min: ProtoVersion,
    pub max: ProtoVersion,
}

/// This build's protocol: 1.0. M1 + M2-lite ship 1.0.
pub const PROTOCOL: ProtocolRange = ProtocolRange {
    min: ProtoVersion { major: 1, minor: 0 },
    max: ProtoVersion { major: 1, minor: 0 },
};

impl ProtocolRange {
    /// Highest version both sides support, if any.
    pub fn negotiate(&self, other: &ProtocolRange) -> Option<ProtoVersion> {
        let lo = self.min.max(other.min);
        let hi = self.max.min(other.max);
        if lo <= hi && lo.major == hi.major { Some(hi) } else { None }
    }
}

/// Crate version of this build, e.g. for `NodeInfo.node_version`.
pub const CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Parse `MAJOR.MINOR.PATCH` (pre-release/build suffixes are rejected: release
/// manifests must use plain versions).
pub fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    let mut it = s.split('.');
    let a = it.next()?.parse().ok()?;
    let b = it.next()?.parse().ok()?;
    let c = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((a, b, c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiate() {
        let v = |a, b| ProtoVersion { major: a, minor: b };
        let node = ProtocolRange { min: v(1, 0), max: v(1, 1) };
        let agent = ProtocolRange { min: v(1, 1), max: v(1, 2) };
        assert_eq!(node.negotiate(&agent), Some(v(1, 1)));
        let old = ProtocolRange { min: v(0, 9), max: v(0, 9) };
        assert_eq!(node.negotiate(&old), None);
        assert_eq!(PROTOCOL.negotiate(&PROTOCOL), Some(v(1, 0)));
        assert_eq!(parse_semver("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_semver("1.2.3-rc1"), None);
    }
}
