//! Relay configuration (design 2, 9; M2-lite for 0.21; S2).
//!
//! 0.21 dials off-LAN through n0's PUBLIC iroh relays (`iroh::RelayMode::Default`,
//! hosts `*.relay.n0.iroh.link`). No self-hosted relay, no spend. Production
//! builds still use `iroh::endpoint::presets::Minimal` (crypto provider only):
//! NO n0 DNS / pkarr address lookup. Paired devices dial by EndpointId with the
//! relay URL and direct addresses they stored from `PairAccept` and refresh from
//! `GET /link/v1/node`.
//!
//! [`RelayConfig::Custom`] is the hook for a future self-hosted
//! `relay.deloresai.com` (needs owner spend approval; see deploy/relay). Its
//! token (abuse control only, not a security boundary) goes in the
//! `Authorization` header, never `?token=`.

use serde::{Deserialize, Serialize};

use crate::secret::RedactedString;

/// Future self-hosted relay (not deployed; spend not approved).
pub const FUTURE_DELORES_RELAY_URL: &str = "https://relay.deloresai.com";
/// Hostname suffix of n0's public relays (iroh 1.3.0 `defaults::prod`).
pub const N0_RELAY_HOST_SUFFIX: &str = ".relay.n0.iroh.link";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum RelayConfig {
    /// LAN only: `iroh::RelayMode::Disabled`. No relay connection.
    #[default]
    Disabled,
    /// Explicit remote opt-in using n0's public development relays (beta).
    N0Public,
    /// Explicit relay map (future relay.deloresai.com or a user-configured relay).
    Custom {
        urls: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<RedactedString>,
    },
}

impl RelayConfig {
    /// Hosts this configuration considers expected. S2: warn about a stored or
    /// hinted relay host outside this set.
    pub fn is_expected_relay(&self, url: &str) -> bool {
        let host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(|h| h.trim_end_matches('.').to_ascii_lowercase()));
        let Some(host) = host else { return false };
        match self {
            RelayConfig::Disabled => false,
            RelayConfig::N0Public => host.ends_with(N0_RELAY_HOST_SUFFIX),
            RelayConfig::Custom { urls, .. } => urls.iter().any(|u| {
                url::Url::parse(u).ok().and_then(|u| u.host_str().map(|h| h.trim_end_matches('.').to_ascii_lowercase())).as_deref() == Some(host.as_str())
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_relays() {
        assert!(RelayConfig::N0Public.is_expected_relay("https://use1-1.relay.n0.iroh.link./"));
        assert!(!RelayConfig::N0Public.is_expected_relay("https://evil.example/"));
        let c = RelayConfig::Custom { urls: vec![FUTURE_DELORES_RELAY_URL.into()], token: None };
        assert!(c.is_expected_relay("https://relay.deloresai.com/"));
        assert!(!c.is_expected_relay("https://use1-1.relay.n0.iroh.link/"));
        assert_eq!(serde_json::to_string(&RelayConfig::N0Public).unwrap(), r#"{"mode":"n0_public"}"#);
    }
}
