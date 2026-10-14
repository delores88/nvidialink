//! The QR / link payload (design 5b step 2, items 6 and 9, S2).
//!
//! ```text
//! delores-link://pair/v1?n=<node z32>&r=<relay URL>&a=<ip:port,...>&t=<ticket_id b64url>&s=<secret b64url>&e=<expires unix>&l=<label>
//! https://link.deloresai.com/p/v1#<same parameters>      (secret in the fragment, never sent to a server)
//! ```
//! - Values are `application/x-www-form-urlencoded`.
//! - Required: n, t, s, e. Optional: r, a, l. Unknown keys are ignored; a
//!   repeated key is an error.
//! - `r` and `a` are dial hints for the pairing connection only (S2). The
//!   joiner persists addresses from `PairAccept.body_json` instead.
//! - `l` is "name from the code" (item 9): display only, sanitized.
//! - There is no typed short code in M1 (item 6). The fallback is copying the
//!   full link.

use std::net::SocketAddr;

use url::form_urlencoded;

use crate::ids::{EndpointId, TicketId, sanitize_label};
use crate::secret::PairingSecret;

pub const LINK_SCHEME_PREFIX: &str = "delores-link://pair/v1?";
pub const HTTPS_LINK_PREFIX: &str = "https://link.deloresai.com/p/v1#";
pub const LINK_MAX_LEN: usize = 2048;
pub const LINK_MAX_ADDRS: usize = 8;
pub const RELAY_URL_MAX_LEN: usize = 256;

#[derive(Clone, PartialEq, Eq)]
pub struct PairingLink {
    pub node: EndpointId,
    pub relay: Option<String>,
    pub addrs: Vec<SocketAddr>,
    pub ticket_id: TicketId,
    pub secret: PairingSecret,
    pub expires_at: u64,
    pub label: Option<String>,
}

impl std::fmt::Debug for PairingLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingLink")
            .field("node", &self.node)
            .field("relay", &self.relay)
            .field("addrs", &self.addrs)
            .field("ticket_id", &self.ticket_id)
            .field("secret", &"[redacted]")
            .field("expires_at", &self.expires_at)
            .field("label", &self.label)
            .finish()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LinkError {
    #[error("not a Spark Link pairing link")]
    NotALink,
    #[error("link is too long")]
    TooLong,
    #[error("link parameter {0} appears more than once")]
    Duplicate(&'static str),
    #[error("link parameter {0} is missing")]
    Missing(&'static str),
    #[error("link parameter {0} is invalid")]
    Invalid(&'static str),
}

impl PairingLink {
    fn params(&self) -> String {
        let mut q = form_urlencoded::Serializer::new(String::new());
        q.append_pair("n", &self.node.to_z32());
        if let Some(r) = &self.relay {
            q.append_pair("r", r);
        }
        if !self.addrs.is_empty() {
            let a: Vec<String> = self.addrs.iter().map(|a| a.to_string()).collect();
            q.append_pair("a", &a.join(","));
        }
        q.append_pair("t", &self.ticket_id.to_b64());
        q.append_pair("s", &self.secret.expose_b64());
        q.append_pair("e", &self.expires_at.to_string());
        if let Some(l) = &self.label {
            q.append_pair("l", l);
        }
        q.finish()
    }

    /// `delores-link://pair/v1?...` (QR content; handled only by the agent, item 9).
    pub fn to_uri(&self) -> String {
        format!("{LINK_SCHEME_PREFIX}{}", self.params())
    }

    /// `https://link.deloresai.com/p/v1#...` (camera-friendly form; S5 governs the page).
    pub fn to_https(&self) -> String {
        format!("{HTTPS_LINK_PREFIX}{}", self.params())
    }

    pub fn is_expired(&self, now_unix: u64) -> bool {
        now_unix >= self.expires_at
    }

    pub fn parse(input: &str) -> Result<Self, LinkError> {
        let s = input.trim();
        if s.len() > LINK_MAX_LEN {
            return Err(LinkError::TooLong);
        }
        let params = s
            .strip_prefix(LINK_SCHEME_PREFIX)
            .or_else(|| s.strip_prefix(HTTPS_LINK_PREFIX))
            .ok_or(LinkError::NotALink)?;

        let mut n = None;
        let mut r = None;
        let mut a = None;
        let mut t = None;
        let mut sec = None;
        let mut e = None;
        let mut l = None;
        for (k, v) in form_urlencoded::parse(params.as_bytes()) {
            let (slot, name): (&mut Option<String>, &'static str) = match k.as_ref() {
                "n" => (&mut n, "n"),
                "r" => (&mut r, "r"),
                "a" => (&mut a, "a"),
                "t" => (&mut t, "t"),
                "s" => (&mut sec, "s"),
                "e" => (&mut e, "e"),
                "l" => (&mut l, "l"),
                _ => continue,
            };
            if slot.is_some() {
                return Err(LinkError::Duplicate(name));
            }
            *slot = Some(v.into_owned());
        }

        let node = EndpointId::from_z32(&n.ok_or(LinkError::Missing("n"))?).map_err(|_| LinkError::Invalid("n"))?;
        let ticket_id = TicketId::from_b64(&t.ok_or(LinkError::Missing("t"))?).map_err(|_| LinkError::Invalid("t"))?;
        let secret = PairingSecret::from_b64(&sec.ok_or(LinkError::Missing("s"))?).map_err(|_| LinkError::Invalid("s"))?;
        let expires_at: u64 = e.ok_or(LinkError::Missing("e"))?.parse().map_err(|_| LinkError::Invalid("e"))?;

        let relay = match r {
            None => None,
            Some(r) => {
                if r.len() > RELAY_URL_MAX_LEN {
                    return Err(LinkError::Invalid("r"));
                }
                let u = url::Url::parse(&r).map_err(|_| LinkError::Invalid("r"))?;
                if u.scheme() != "https" || u.host_str().is_none() || !u.username().is_empty() || u.password().is_some() {
                    return Err(LinkError::Invalid("r"));
                }
                Some(r)
            }
        };
        let addrs = match a {
            None => Vec::new(),
            Some(a) if a.is_empty() => Vec::new(),
            Some(a) => {
                let v: Result<Vec<SocketAddr>, _> = a.split(',').map(|x| x.parse::<SocketAddr>()).collect();
                let v = v.map_err(|_| LinkError::Invalid("a"))?;
                if v.len() > LINK_MAX_ADDRS {
                    return Err(LinkError::Invalid("a"));
                }
                v
            }
        };
        let label = l.and_then(|l| sanitize_label(&l));
        Ok(Self { node, relay, addrs, ticket_id, secret, expires_at, label })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PairingLink {
        PairingLink {
            node: EndpointId::from_bytes([1; 32]),
            relay: Some("https://use1-1.relay.n0.iroh.link./".into()),
            addrs: vec!["192.168.1.20:47821".parse().unwrap(), "[fe80::1]:47821".parse().unwrap()],
            ticket_id: TicketId::from_bytes([2; 16]),
            secret: PairingSecret::from_bytes([3; 16]),
            expires_at: 1_900_000_000,
            label: Some("spark-bb3b".into()),
        }
    }

    #[test]
    fn round_trip_both_forms() {
        let l = sample();
        assert_eq!(PairingLink::parse(&l.to_uri()).unwrap(), l);
        assert_eq!(PairingLink::parse(&l.to_https()).unwrap(), l);
        assert!(l.to_https().contains("#n="));
        assert!(!format!("{l:?}").contains(&l.secret.expose_b64()));
    }

    #[test]
    fn rejects_bad_links() {
        let l = sample();
        assert_eq!(PairingLink::parse("https://evil.example/p/v1#n=x"), Err(LinkError::NotALink));
        assert_eq!(PairingLink::parse(&format!("{}&t=AAAAAAAAAAAAAAAAAAAAAA", l.to_uri())), Err(LinkError::Duplicate("t")));
        let no_secret = l.to_uri().replace(&format!("&s={}", l.secret.expose_b64()), "");
        assert_eq!(PairingLink::parse(&no_secret), Err(LinkError::Missing("s")));
        let http_relay = l.to_uri().replace("r=https", "r=http");
        assert_eq!(PairingLink::parse(&http_relay), Err(LinkError::Invalid("r")));
        assert_eq!(PairingLink::parse(&"x".repeat(5000)), Err(LinkError::TooLong));
    }

    #[test]
    fn label_is_sanitized_and_unknown_keys_ignored() {
        let mut l = sample();
        l.label = Some("evil\u{202E}name\n".into());
        let parsed = PairingLink::parse(&format!("{}&zz=1", l.to_uri())).unwrap();
        assert_eq!(parsed.label.as_deref(), Some("evilname"));
    }
}
