//! Amendment A1 (owner direction 2026-10-11): the owner links a device by
//! scanning a QR with a phone camera and tapping Approve on a plain web page
//! that the Spark serves on the local network. See docs/PROTOCOL.md section
//! 5.4 and docs/contract-requests/pair-owner-flow.md.
//!
//! ## Flow (normative)
//! 1. The device's agent runs, over its SSH bootstrap channel,
//!    `spark-link-node owner-ticket --device-name <name> --ttl <1..=300> --json`
//!    and receives an [`OwnerTicket`]. Only local or SSH access to the Spark can
//!    mint one (the running node's local control channel; no network route).
//! 2. The agent pairs exactly as in PROTOCOL 5.2 (ALPN `delores/spark-link/pair/1`,
//!    `pair_request` with `mac`, `pair_pending`). For an owner ticket the node does
//!    NOT push `pairing_pending` to admin agents and admin agents cannot approve it
//!    over the API; it waits for the LAN page (or the Spark CLI as a local fallback).
//! 3. While the owner pairing is pending, and only then, the node serves plain HTTP
//!    on `lan_port` (default [`DEFAULT_LAN_PORT`]) bound to private LAN addresses
//!    only ([`is_lan_page_addr`]).
//! 4. The agent shows the 4 SAS words and a QR of [`lan_approve_url`]:
//!    `http://<lan_ip>:<lan_port>/pair/<pairing_id>#<approve_token>` where
//!    ```text
//!    approve_token = base64url(HKDF-SHA256(ikm = secret, salt = ticket_id,
//!                                          info = "spark-link/pair/v1/lan-approve")[..16])
//!    ```
//! 5. The phone opens `GET /pair/<pairing_id>` (the fragment never leaves the phone),
//!    shows "Link <device_name> to your Spark?" with the same 4 SAS words, and on a tap
//!    POSTs `/pair/<pairing_id>/decision` [`LanDecisionRequest`]. The node compares the
//!    token in constant time; [`LAN_MAX_BAD_TOKENS`] bad tokens burn the pairing.
//!    Approve sends `pair_accept` (the device gets `ScopeSet::owner()` as max_scopes,
//!    `approved_by` = the node id, audit `approved_by = "lan-page"`); deny sends
//!    `pair_reject{pairing_rejected, denied}`.
//!
//! ## Device self-control (authenticated device API, agent-internal)
//! [`DeviceSelfRoute`]: `POST /link/v1/devices/self/pause`, `POST .../self/resume`,
//! `DELETE /link/v1/devices/self`. While paused the node answers every other request
//! of that device with `423 device_paused`.

use std::net::IpAddr;

use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::ProtoError;
use crate::ids::{EndpointId, PairingId, TicketId, b64url, b64url_decode_exact};
use crate::secret::PairingSecret;

/// HKDF info for the LAN approval token.
pub const INFO_LAN_APPROVE: &[u8] = b"spark-link/pair/v1/lan-approve";
/// Length of the approval token in bytes (22 base64url characters).
pub const APPROVE_TOKEN_LEN: usize = 16;
/// Length of the approval token's text form.
pub const APPROVE_TOKEN_B64_LEN: usize = 22;
/// Default TCP port of the LAN approval page.
pub const DEFAULT_LAN_PORT: u16 = 7427;
/// Bad approval tokens per owner pairing, counted across all peers, before it burns.
pub const LAN_MAX_BAD_TOKENS: u32 = 5;
/// Value of the audit detail `approved_by` (and the `uv` name `lan_page`) for LAN-page approvals.
pub const APPROVED_BY_LAN_PAGE: &str = "lan-page";
pub const UV_LAN_PAGE: &str = "lan_page";
/// Largest decision body the LAN page accepts.
pub const LAN_DECISION_MAX_BODY: usize = 1024;
/// Path prefix of the LAN page.
pub const LAN_PAGE_PREFIX: &str = "/pair/";

/// The 16-byte LAN approval token. Zeroized on drop; redacted in Debug.
#[derive(Clone, PartialEq, Eq)]
pub struct ApproveToken(Zeroizing<[u8; APPROVE_TOKEN_LEN]>);

impl std::fmt::Debug for ApproveToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApproveToken([redacted])")
    }
}

impl ApproveToken {
    pub fn from_bytes(b: [u8; APPROVE_TOKEN_LEN]) -> Self {
        Self(Zeroizing::new(b))
    }
    pub fn expose(&self) -> &[u8; APPROVE_TOKEN_LEN] {
        &self.0
    }
    /// base64url without padding (22 chars). Treat the result as secret.
    pub fn to_b64(&self) -> String {
        b64url(self.0.as_ref())
    }
    pub fn from_b64(s: &str) -> Result<Self, ProtoError> {
        b64url_decode_exact::<APPROVE_TOKEN_LEN>(s, "approve token").map(Self::from_bytes)
    }
    /// Constant-time comparison with a candidate token's bytes.
    pub fn ct_eq(&self, candidate: &[u8]) -> bool {
        if candidate.len() != APPROVE_TOKEN_LEN {
            return false;
        }
        let mut d = 0u8;
        for (x, y) in self.0.iter().zip(candidate) {
            d |= x ^ y;
        }
        d == 0
    }
}

/// `approve_token = HKDF-SHA256(ikm = secret, salt = ticket_id, info = INFO_LAN_APPROVE)[..16]`.
pub fn approve_token(secret: &PairingSecret, ticket_id: &TicketId) -> ApproveToken {
    let hk = Hkdf::<Sha256>::new(Some(ticket_id.as_bytes()), secret.expose());
    let mut out = Zeroizing::new([0u8; APPROVE_TOKEN_LEN]);
    hk.expand(INFO_LAN_APPROVE, out.as_mut()).expect("16 bytes is a valid HKDF length");
    ApproveToken(out)
}

/// The QR content: `http://<lan_ip>:<lan_port>/pair/<pairing_id>#<approve_token>`.
/// IPv6 addresses are bracketed. Prefer an IPv4 RFC 1918 address: phone browsers
/// cannot open IPv6 link-local URLs (they need a zone id).
pub fn lan_approve_url(lan_ip: IpAddr, lan_port: u16, pairing_id: &PairingId, token: &ApproveToken) -> String {
    let host = match lan_ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    };
    format!("http://{host}:{lan_port}{LAN_PAGE_PREFIX}{pairing_id}#{}", token.to_b64())
}

/// Addresses the LAN page may bind: RFC 1918, IPv4 link-local (169.254/16), IPv6 ULA
/// (fc00::/7) and IPv6 link-local (fe80::/10). Never loopback, unspecified, multicast,
/// CGNAT (100.64/10), public or relay addresses.
pub fn is_lan_page_addr(ip: &IpAddr) -> bool {
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return false;
    }
    if let IpAddr::V4(v4) = ip
        && v4.is_broadcast()
    {
        return false;
    }
    crate::mdns::is_private_lan(ip)
}

/// One route of the LAN page server. Anything else is 404.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LanPageRoute {
    /// `GET /pair/<pairing_id>`: the approval page (HTML).
    Page(PairingId),
    /// `POST /pair/<pairing_id>/decision` [`LanDecisionRequest`] -> [`LanDecisionResponse`].
    Decision(PairingId),
}

impl LanPageRoute {
    /// Route a LAN page request. `path` excludes any query (requests with a query are 404).
    pub fn parse(method: &str, path: &str) -> Option<LanPageRoute> {
        let rest = path.strip_prefix(LAN_PAGE_PREFIX)?;
        let (pid, tail) = match rest.split_once('/') {
            Some((p, t)) => (p, Some(t)),
            None => (rest, None),
        };
        let pid = PairingId::from_b64(pid).ok()?;
        match (method, tail) {
            ("GET", None) => Some(LanPageRoute::Page(pid)),
            ("POST", Some("decision")) => Some(LanPageRoute::Decision(pid)),
            _ => None,
        }
    }

    pub fn path(&self) -> String {
        match self {
            LanPageRoute::Page(p) => format!("{LAN_PAGE_PREFIX}{p}"),
            LanPageRoute::Decision(p) => format!("{LAN_PAGE_PREFIX}{p}/decision"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LanChoice {
    Approve,
    Deny,
}

/// `POST /pair/<pairing_id>/decision` body (`Content-Type: application/json`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanDecisionRequest {
    /// The approval token from the URL fragment (base64url, 22 chars).
    pub token: String,
    pub choice: LanChoice,
    /// Six digits shown only on the laptop. Required to approve, never to deny.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_code: Option<String>,
}

impl std::fmt::Debug for LanDecisionRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LanDecisionRequest").field("token", &"[redacted]").field("choice", &self.choice).finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LanOutcome {
    Approved,
    Denied,
}

/// 200 response of the decision route. Failures use the usual `{"error": ...}` body:
/// `403 pairing_failed` (bad token), `404 not_found` (no such pending owner pairing:
/// unknown, expired, decided or burned), `400 bad_request`, `429 too_many_requests`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanDecisionResponse {
    pub ok: bool,
    pub outcome: LanOutcome,
}

/// `spark-link-node owner-ticket --device-name <name> --ttl <s> --json` output
/// (the CLI adds `"ok": true`). The secret is shown once and never stored by the
/// node (it keeps only HKDF keys and the approval token, in memory).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OwnerTicket {
    pub ticket_id: TicketId,
    pub secret: PairingSecret,
    pub expires_at: u64,
    pub node_id: EndpointId,
    /// Private LAN addresses the approval page will listen on (IPv4 first, then IPv6;
    /// IPv6 without zone ids).
    pub lan_addrs: Vec<String>,
    pub lan_port: u16,
}

/// Amendment A1: `spark-link-node install` WITHOUT `--owner` prints this (with
/// `--owner` the output is unchanged: `node_cli::InstallReport`). `owner` is
/// always null and `owner_pending` true: the first approved owner-ticket pairing
/// becomes the owner (audited `owner_set`, actor of the approval, `via = owner_ticket`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OwnerlessInstallReport {
    pub ok: bool,
    pub node_id: EndpointId,
    pub owner: Option<EndpointId>,
    pub owner_pending: bool,
    pub label: String,
    pub version: String,
    pub state_dir: String,
    pub unit: crate::node_cli::UnitKind,
    pub linger: Option<bool>,
    pub udp_port: u16,
}

/// Device self-control routes on the node HTTP API (agent-internal: the node
/// refuses them unless `X-Delores-App: spark-link-agent`). They are matched before
/// [`crate::api::Route::parse`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceSelfRoute {
    /// `POST /link/v1/devices/self/pause` -> [`DevicePauseState`].
    Pause,
    /// `POST /link/v1/devices/self/resume` -> [`DevicePauseState`].
    Resume,
    /// `DELETE /link/v1/devices/self` -> [`crate::api::RevokeResponse`]. Revokes and
    /// denylists the calling device key; the owner device gets `409 conflict`.
    Revoke,
}

impl DeviceSelfRoute {
    pub const PAUSE_PATH: &'static str = "/link/v1/devices/self/pause";
    pub const RESUME_PATH: &'static str = "/link/v1/devices/self/resume";
    pub const SELF_PATH: &'static str = "/link/v1/devices/self";

    /// `None`: not a self route (continue with `Route::parse`). `Some(Err(()))`:
    /// a self path with the wrong method (405).
    #[allow(clippy::result_unit_err)]
    pub fn parse(method: &str, path: &str) -> Option<Result<DeviceSelfRoute, ()>> {
        let (want, r) = match path {
            Self::PAUSE_PATH => ("POST", DeviceSelfRoute::Pause),
            Self::RESUME_PATH => ("POST", DeviceSelfRoute::Resume),
            Self::SELF_PATH => ("DELETE", DeviceSelfRoute::Revoke),
            _ => return None,
        };
        Some(if method == want { Ok(r) } else { Err(()) })
    }

    pub fn method(&self) -> &'static str {
        match self {
            DeviceSelfRoute::Pause | DeviceSelfRoute::Resume => "POST",
            DeviceSelfRoute::Revoke => "DELETE",
        }
    }

    pub fn path(&self) -> &'static str {
        match self {
            DeviceSelfRoute::Pause => Self::PAUSE_PATH,
            DeviceSelfRoute::Resume => Self::RESUME_PATH,
            DeviceSelfRoute::Revoke => Self::SELF_PATH,
        }
    }

    /// Requests a paused device may still make.
    pub fn allowed_while_paused(&self) -> bool {
        matches!(self, DeviceSelfRoute::Resume | DeviceSelfRoute::Revoke)
    }
}

/// Response of pause/resume (both idempotent only in the sense that resume on an
/// active device is a 200 no-op; pause on a paused device is `423 device_paused`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevicePauseState {
    pub device: EndpointId,
    pub paused: bool,
    pub paused_at: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_bound_to_secret_and_ticket() {
        let s = PairingSecret::from_bytes([5; 16]);
        let t = TicketId::from_bytes([3; 16]);
        let a = approve_token(&s, &t);
        assert_eq!(a.to_b64().len(), APPROVE_TOKEN_B64_LEN);
        assert_eq!(ApproveToken::from_b64(&a.to_b64()).unwrap(), a);
        assert_ne!(approve_token(&PairingSecret::from_bytes([6; 16]), &t), a);
        assert_ne!(approve_token(&s, &TicketId::from_bytes([4; 16])), a);
        // Not one of the pairing keys (different info label).
        let keys_debug = format!("{:?}", a);
        assert_eq!(keys_debug, "ApproveToken([redacted])");
        assert!(a.ct_eq(a.expose()));
        assert!(!a.ct_eq(&[0; 16]));
        assert!(!a.ct_eq(&a.expose()[..15]));
    }

    #[test]
    fn url_shape() {
        let tok = ApproveToken::from_bytes([0xfb; 16]);
        let pid = PairingId::from_bytes([1; 16]);
        let u = lan_approve_url("192.168.1.20".parse().unwrap(), DEFAULT_LAN_PORT, &pid, &tok);
        assert_eq!(u, format!("http://192.168.1.20:7427/pair/{pid}#{}", tok.to_b64()));
        let u6 = lan_approve_url("fd00::5".parse().unwrap(), 7427, &pid, &tok);
        assert!(u6.starts_with("http://[fd00::5]:7427/pair/"));
    }

    #[test]
    fn lan_page_addr_filter() {
        for ok in ["192.168.1.20", "10.1.2.3", "172.16.0.9", "172.31.255.254", "169.254.7.7", "fd12:3456::1", "fc00::1", "fe80::1"] {
            assert!(is_lan_page_addr(&ok.parse().unwrap()), "{ok}");
        }
        for bad in [
            "0.0.0.0", "127.0.0.1", "8.8.8.8", "100.64.1.1", "172.32.0.1", "224.0.0.251", "255.255.255.255", "::", "::1", "2001:db8::1", "2606:4700::1111",
            "ff02::fb", "::ffff:192.168.0.1",
        ] {
            assert!(!is_lan_page_addr(&bad.parse().unwrap()), "{bad}");
        }
    }

    #[test]
    fn lan_routes() {
        let pid = PairingId::from_bytes([9; 16]);
        assert_eq!(LanPageRoute::parse("GET", &format!("/pair/{pid}")), Some(LanPageRoute::Page(pid)));
        assert_eq!(LanPageRoute::parse("POST", &format!("/pair/{pid}/decision")), Some(LanPageRoute::Decision(pid)));
        assert_eq!(LanPageRoute::parse("POST", &format!("/pair/{pid}")), None);
        assert_eq!(LanPageRoute::parse("GET", &format!("/pair/{pid}/decision")), None);
        assert_eq!(LanPageRoute::parse("GET", "/pair/not-an-id"), None);
        assert_eq!(LanPageRoute::parse("GET", &format!("/pair/{pid}/")), None);
        assert_eq!(LanPageRoute::parse("GET", "/favicon.ico"), None);
        assert_eq!(LanPageRoute::parse("GET", &LanPageRoute::Page(pid).path()), Some(LanPageRoute::Page(pid)));
    }

    #[test]
    fn self_routes() {
        assert_eq!(DeviceSelfRoute::parse("POST", "/link/v1/devices/self/pause"), Some(Ok(DeviceSelfRoute::Pause)));
        assert_eq!(DeviceSelfRoute::parse("POST", "/link/v1/devices/self/resume"), Some(Ok(DeviceSelfRoute::Resume)));
        assert_eq!(DeviceSelfRoute::parse("DELETE", "/link/v1/devices/self"), Some(Ok(DeviceSelfRoute::Revoke)));
        assert_eq!(DeviceSelfRoute::parse("GET", "/link/v1/devices/self"), Some(Err(())));
        assert_eq!(DeviceSelfRoute::parse("DELETE", "/link/v1/devices/self/pause"), Some(Err(())));
        assert_eq!(DeviceSelfRoute::parse("GET", "/link/v1/devices"), None);
        for r in [DeviceSelfRoute::Pause, DeviceSelfRoute::Resume, DeviceSelfRoute::Revoke] {
            assert_eq!(DeviceSelfRoute::parse(r.method(), r.path()), Some(Ok(r)));
            assert!(crate::api::path_is_canonical(r.path()));
        }
        assert!(!DeviceSelfRoute::Pause.allowed_while_paused());
        assert!(DeviceSelfRoute::Resume.allowed_while_paused() && DeviceSelfRoute::Revoke.allowed_while_paused());
    }

    #[test]
    fn wire_shapes() {
        let r: LanDecisionRequest = serde_json::from_str(r#"{"token":"abc","choice":"approve"}"#).unwrap();
        assert_eq!(r.choice, LanChoice::Approve);
        assert!(!format!("{r:?}").contains("abc"));
        assert_eq!(serde_json::to_string(&LanDecisionResponse { ok: true, outcome: LanOutcome::Denied }).unwrap(), r#"{"ok":true,"outcome":"denied"}"#);
        let t = OwnerTicket {
            ticket_id: TicketId::from_bytes([1; 16]),
            secret: PairingSecret::from_bytes([2; 16]),
            expires_at: 9,
            node_id: EndpointId::from_bytes([3; 32]),
            lan_addrs: vec!["192.168.1.20".into()],
            lan_port: 7427,
        };
        let v = serde_json::to_value(&t).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(keys, ["expires_at", "lan_addrs", "lan_port", "node_id", "secret", "ticket_id"]);
        assert_eq!(serde_json::from_value::<OwnerTicket>(v).unwrap(), t);
    }
}
