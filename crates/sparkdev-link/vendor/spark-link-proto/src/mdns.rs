//! LAN discovery (design 5c as amended by 15.4 "mDNS TXT overshares").
//!
//! - The node advertises service [`SERVICE_TYPE`] with a random per-boot
//!   instance name `sl-<8 hex>` and ONE TXT record: `tag=<16 hex>` where
//!   `tag = HMAC-SHA256(key = node_id(32), "spark-link/mdns" || floor(unix/3600) as u64 BE)[..8]`.
//!   No `eid`, `ver` or `paired` records. Advertise only on private-range
//!   interfaces ([`is_private_lan`]). The SRV port is the node's iroh UDP port.
//! - Agents that know a node_id (paired, or holding a link) match the tag for
//!   the current hour +/- 1. mDNS results are dial HINTS, never identity: the
//!   iroh handshake authenticates the node key.
//! - Agents also browse [`SSH_SERVICE_TYPE`] and [`WORKSTATION_SERVICE_TYPE`]
//!   to offer SSH bootstrap (5a) for Sparks that have no node yet.

use std::net::IpAddr;

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::ids::{EndpointId, hex_lower};

pub const SERVICE_TYPE: &str = "_delores-link._udp.local.";
pub const SSH_SERVICE_TYPE: &str = "_ssh._tcp.local.";
pub const WORKSTATION_SERVICE_TYPE: &str = "_workstation._tcp.local.";
pub const TXT_TAG: &str = "tag";
pub const TAG_DOMAIN: &[u8] = b"spark-link/mdns";
pub const TAG_PERIOD_S: u64 = 3600;

/// The rotating tag for `node_id` at `unix_time`.
pub fn tag(node_id: &EndpointId, unix_time: u64) -> String {
    tag_for_period(node_id, unix_time / TAG_PERIOD_S)
}

pub fn tag_for_period(node_id: &EndpointId, period: u64) -> String {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(node_id.as_bytes()).expect("any key length");
    m.update(TAG_DOMAIN);
    m.update(&period.to_be_bytes());
    let out = m.finalize().into_bytes();
    hex_lower(&out[..8])
}

/// Does `tag_value` match `node_id` for the hour of `now` or an adjacent hour?
pub fn tag_matches(node_id: &EndpointId, tag_value: &str, now: u64) -> bool {
    let p = now / TAG_PERIOD_S;
    [p.saturating_sub(1), p, p + 1].iter().any(|q| tag_for_period(node_id, *q) == tag_value)
}

/// Random per-boot instance name.
pub fn instance_name() -> String {
    format!("sl-{}", hex_lower(&crate::random_bytes::<4>()))
}

/// RFC 1918 / RFC 4193 / link-local addresses: the only interfaces the node advertises on.
pub fn is_private_lan(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            let s = v6.segments()[0];
            (s & 0xfe00) == 0xfc00 || (s & 0xffc0) == 0xfe80
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_rotates_and_matches_adjacent_hours() {
        let n = EndpointId::from_bytes([5; 32]);
        let t0 = tag(&n, 7200);
        assert_eq!(t0.len(), 16);
        assert_ne!(t0, tag(&n, 7200 + 3600));
        assert!(tag_matches(&n, &t0, 7200 + 3599));
        assert!(tag_matches(&n, &t0, 7200 + 3600));
        assert!(!tag_matches(&n, &t0, 7200 + 3 * 3600));
        assert!(!tag_matches(&EndpointId::from_bytes([6; 32]), &t0, 7200));
    }

    #[test]
    fn private_ranges() {
        for ok in ["192.168.1.2", "10.0.0.1", "172.16.5.4", "169.254.1.1", "fd00::1", "fe80::1"] {
            assert!(is_private_lan(&ok.parse().unwrap()), "{ok}");
        }
        for bad in ["8.8.8.8", "100.64.0.1", "2001:db8::1", "127.0.0.1"] {
            assert!(!is_private_lan(&bad.parse().unwrap()), "{bad}");
        }
    }
}
