//! Pairing (design 5b as amended by 15.2 items 1, 3, 6, 9, 11 and S1/S2).
//!
//! ## Keys and transcript (item 11, normative)
//! ```text
//! k_req = HKDF-SHA256(ikm = secret(16), salt = ticket_id(16), info = "spark-link/pair/v1/req")[..32]
//! k_acc = HKDF-SHA256(ikm = secret(16), salt = ticket_id(16), info = "spark-link/pair/v1/acc")[..32]
//! exporter = TLS exporter of THIS QUIC connection:
//!            conn.export_keying_material(out[32], label = "spark-link/pair/v1", context = ticket_id(16))
//! T   = node_id(32) || joiner_id(32) || ticket_id(16) || expires_at(u64 BE) || exporter(32)   (120 bytes)
//! mac  = HMAC-SHA256(k_req, T)
//! mac2 = HMAC-SHA256(k_acc, T || SHA-256(body_json))      body_json = exact bytes of PairAccept.body_json
//! sas  = words(HKDF-SHA256(ikm = node_id || joiner_id || ticket_id || exporter || n_s, salt = none,
//!                          info = "spark-link/pair/v1/sas")[..6])
//! node fingerprint words = words(HKDF-SHA256(ikm = node_id, salt = none, info = "spark-link/node-fp/v1")[..6])
//! ```
//! `node_id` and `joiner_id` are ALWAYS the TLS-authenticated endpoint keys of
//! the connection (`Connection::remote_id()` / own id); `PairRequest` has no
//! joiner_id field. Verification is constant-time. Keys live in memory only,
//! so tickets die on node restart. The secret itself is never transmitted.
//!
//! ## Message flow on ALPN `delores/spark-link/pair/1` (one bidi stream, frames per `frame`)
//! 1. joiner -> node `pair_request` within [`PAIR_REQUEST_DEADLINE_S`] of connecting.
//! 2. node: unknown/expired/consumed ticket or bad mac -> `pair_reject{pairing_failed}` (one generic
//!    error). Bad macs count toward [`TICKET_MAX_FAILURES`] across all peers, then the ticket burns.
//!    A valid mac from a *second* key for a ticket that already has a pending pairing poisons the
//!    ticket: nobody is added, both joiners get `pair_reject{pairing_rejected, poisoned}`, admins get
//!    `ticket_poisoned`. A valid mac after the ticket was consumed raises `late_mac_alert`.
//! 3. node -> joiner `pair_pending{pairing_id, n_s}` (n_s sent only after the mac verified).
//!    Joiner shows the 4 sas words. Node pushes `pairing_pending` with 3 candidate codes to admins.
//! 4. An admin agent approves with OS user verification: `POST /link/v1/pairings/{id}/approve
//!    {choice}`. Wrong choice -> ticket burned, joiner gets `pair_reject{wrong_code}`. TTL passes ->
//!    `pair_reject{expired}`. The node writes the approval audit entry BEFORE inserting the device.
//! 5. node -> joiner `pair_accept{body_json, mac2}`. The joiner verifies mac2, then persists the
//!    node's relay URL and addresses only from `body_json` (S2), never from the link's r=/a= hints.

use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::errors::ErrorCode;
use crate::ids::{DeviceKind, EndpointId, Mac32, Nonce16, PairingId, TicketId};
use crate::roles::Role;
use crate::scopes::ScopeSet;
use crate::secret::PairingSecret;
use crate::version::ProtocolRange;
use crate::words::{Words, words_from_bytes};

type HmacSha256 = Hmac<Sha256>;

/// Maximum ticket lifetime (design 4).
pub const TICKET_TTL_MAX_S: u32 = 300;
/// Bad-mac failures per ticket, counted across all peers, before it burns (item 6, S1).
pub const TICKET_MAX_FAILURES: u32 = 5;
/// An unpaired pairing connection must send a valid `pair_request` within this many seconds (S1).
pub const PAIR_REQUEST_DEADLINE_S: u64 = 10;
/// Max frame size on the pairing ALPN.
pub const PAIR_FRAME_MAX: usize = 64 * 1024;
/// Message version inside pairing frames.
pub const PAIR_MSG_VERSION: u16 = 1;

pub const EXPORTER_LABEL: &[u8] = b"spark-link/pair/v1";
pub const EXPORTER_LEN: usize = 32;
pub const INFO_REQ: &[u8] = b"spark-link/pair/v1/req";
pub const INFO_ACC: &[u8] = b"spark-link/pair/v1/acc";
pub const INFO_SAS: &[u8] = b"spark-link/pair/v1/sas";
pub const INFO_NODE_FP: &[u8] = b"spark-link/node-fp/v1";
/// Number of candidate codes on an admin approval card (1 correct + 2 decoys).
pub const APPROVAL_CANDIDATES: usize = 3;

/// The exporter context for a ticket (the 16 ticket_id bytes).
pub fn exporter_context(ticket_id: &TicketId) -> &[u8; 16] {
    ticket_id.as_bytes()
}

/// HKDF-derived per-ticket keys; zeroized on drop.
pub struct PairingKeys {
    k_req: Zeroizing<[u8; 32]>,
    k_acc: Zeroizing<[u8; 32]>,
}

impl std::fmt::Debug for PairingKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairingKeys([redacted])")
    }
}

/// The 120-byte fixed-width transcript prefix `T`.
#[derive(Clone, PartialEq, Eq)]
pub struct Transcript([u8; 120]);

impl Transcript {
    pub fn new(node_id: &EndpointId, joiner_id: &EndpointId, ticket_id: &TicketId, expires_at: u64, exporter: &[u8; EXPORTER_LEN]) -> Self {
        let mut t = [0u8; 120];
        t[0..32].copy_from_slice(node_id.as_bytes());
        t[32..64].copy_from_slice(joiner_id.as_bytes());
        t[64..80].copy_from_slice(ticket_id.as_bytes());
        t[80..88].copy_from_slice(&expires_at.to_be_bytes());
        t[88..120].copy_from_slice(exporter);
        Self(t)
    }
    pub fn as_bytes(&self) -> &[u8; 120] {
        &self.0
    }
}

fn hmac(key: &[u8; 32], parts: &[&[u8]]) -> HmacSha256 {
    let mut m = <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC accepts 32-byte keys");
    for p in parts {
        m.update(p);
    }
    m
}

impl PairingKeys {
    pub fn derive(secret: &PairingSecret, ticket_id: &TicketId) -> Self {
        let hk = Hkdf::<Sha256>::new(Some(ticket_id.as_bytes()), secret.expose());
        let mut k_req = Zeroizing::new([0u8; 32]);
        let mut k_acc = Zeroizing::new([0u8; 32]);
        hk.expand(INFO_REQ, k_req.as_mut()).expect("32 bytes is a valid HKDF length");
        hk.expand(INFO_ACC, k_acc.as_mut()).expect("32 bytes is a valid HKDF length");
        Self { k_req, k_acc }
    }

    pub fn request_mac(&self, t: &Transcript) -> Mac32 {
        Mac32::from_bytes(hmac(&self.k_req, &[t.as_bytes()]).finalize().into_bytes().into())
    }

    /// Constant-time check of the joiner's `mac`.
    pub fn verify_request_mac(&self, t: &Transcript, mac: &Mac32) -> bool {
        hmac(&self.k_req, &[t.as_bytes()]).verify_slice(mac.as_bytes()).is_ok()
    }

    pub fn accept_mac(&self, t: &Transcript, body_json: &str) -> Mac32 {
        let digest = Sha256::digest(body_json.as_bytes());
        Mac32::from_bytes(hmac(&self.k_acc, &[t.as_bytes(), &digest]).finalize().into_bytes().into())
    }

    /// Constant-time check of the node's `mac2` over the exact `body_json` bytes.
    pub fn verify_accept_mac(&self, t: &Transcript, body_json: &str, mac2: &Mac32) -> bool {
        let digest = Sha256::digest(body_json.as_bytes());
        hmac(&self.k_acc, &[t.as_bytes(), &digest]).verify_slice(mac2.as_bytes()).is_ok()
    }
}

fn hkdf6(ikm: &[u8], info: &[u8]) -> [u8; 6] {
    let hk = Hkdf::<Sha256>::new(None, ikm);
    let mut out = [0u8; 6];
    hk.expand(info, &mut out).expect("6 bytes is a valid HKDF length");
    out
}

/// The 4 short-authentication words both the joiner and the node compute after
/// `pair_pending` (item 1). They change with ticket_id, exporter and n_s.
pub fn sas_words(node_id: &EndpointId, joiner_id: &EndpointId, ticket_id: &TicketId, exporter: &[u8; EXPORTER_LEN], n_s: &Nonce16) -> Words {
    let mut ikm = Vec::with_capacity(128);
    ikm.extend_from_slice(node_id.as_bytes());
    ikm.extend_from_slice(joiner_id.as_bytes());
    ikm.extend_from_slice(ticket_id.as_bytes());
    ikm.extend_from_slice(exporter);
    ikm.extend_from_slice(n_s.as_bytes());
    words_from_bytes(&hkdf6(&ikm, INFO_SAS))
}

/// Node fingerprint words (item 9): shown on the admin QR screen and on the
/// joiner's "New Spark, never paired" confirmation dialog.
pub fn node_fingerprint_words(node_id: &EndpointId) -> Words {
    words_from_bytes(&hkdf6(node_id.as_bytes(), INFO_NODE_FP))
}

/// Build the admin approval card: the correct code plus random decoys, in
/// random order. Returns (candidates, index of the correct one). Decoys are
/// distinct from the correct code and from each other.
pub fn approval_candidates(correct: &Words) -> (Vec<Words>, usize) {
    let mut out: Vec<Words> = vec![correct.clone()];
    while out.len() < APPROVAL_CANDIDATES {
        let w = words_from_bytes(&crate::random_bytes::<6>());
        if !out.contains(&w) {
            out.push(w);
        }
    }
    let idx = (crate::random_bytes::<1>()[0] as usize) % APPROVAL_CANDIDATES;
    out.swap(0, idx);
    (out, idx)
}

// ---------------------------------------------------------------------------
// Messages on ALPN delores/spark-link/pair/1
// ---------------------------------------------------------------------------

/// Joiner -> node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum PairClientMsg {
    PairRequest(PairRequest),
    /// The joiner's user cancelled while pending.
    PairCancel,
}

/// `{"t":"pair_request","v":1,"ticket_id","device_name","device_kind","mac"}`.
/// There is deliberately no joiner_id field (item 11).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PairRequest {
    pub v: u16,
    pub ticket_id: TicketId,
    /// Sanitized with `ids::sanitize_label`; shown on the admin card.
    pub device_name: String,
    pub device_kind: DeviceKind,
    pub mac: Mac32,
}

/// Node -> joiner.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum PairServerMsg {
    PairPending(PairPending),
    PairAccept(PairAccept),
    PairReject(PairReject),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PairPending {
    pub v: u16,
    pub pairing_id: PairingId,
    pub n_s: Nonce16,
    /// When the pending pairing (= the ticket) expires.
    pub expires_at: u64,
}

/// `body_json` is the exact JSON text of a [`PairAcceptBody`]; `mac2` covers
/// its bytes, so the joiner verifies before parsing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PairAccept {
    pub body_json: String,
    pub mac2: Mac32,
}

impl PairAccept {
    pub fn body(&self) -> Result<PairAcceptBody, serde_json::Error> {
        serde_json::from_str(&self.body_json)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PairAcceptBody {
    pub v: u16,
    pub pairing_id: PairingId,
    pub node: PairedNodeInfo,
    /// The joiner's own EndpointId as the node saw it (TLS remote_id).
    pub device_id: EndpointId,
    pub max_scopes: ScopeSet,
    pub paired_at: u64,
    pub approved_by: EndpointId,
}

/// What a joiner persists about the node (S2: only from mac2-covered data,
/// refreshed from `GET /link/v1/node` on every connect).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PairedNodeInfo {
    pub node_id: EndpointId,
    pub label: String,
    pub relay_url: Option<String>,
    /// "ip:port" strings.
    pub direct_addrs: Vec<String>,
    pub protocol: ProtocolRange,
    pub role: Role,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PairReject {
    /// `pairing_failed` (generic) or `pairing_rejected`.
    pub code: ErrorCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<RejectReason>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    Denied,
    WrongCode,
    Expired,
    Poisoned,
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (EndpointId, EndpointId, TicketId, [u8; 32], PairingSecret, u64) {
        (
            EndpointId::from_bytes([1; 32]),
            EndpointId::from_bytes([2; 32]),
            TicketId::from_bytes([3; 16]),
            [4; 32],
            PairingSecret::from_bytes([5; 16]),
            1_900_000_000,
        )
    }

    #[test]
    fn mac_binds_everything() {
        let (n, j, t, x, s, e) = fixture();
        let keys = PairingKeys::derive(&s, &t);
        let tr = Transcript::new(&n, &j, &t, e, &x);
        let mac = keys.request_mac(&tr);
        assert!(keys.verify_request_mac(&tr, &mac));
        // joiner key swap (mac computed for X, sent from Y)
        let other = Transcript::new(&n, &EndpointId::from_bytes([9; 32]), &t, e, &x);
        assert!(!keys.verify_request_mac(&other, &mac));
        // different connection (exporter) = replay on a second connection
        let replay = Transcript::new(&n, &j, &t, e, &[8; 32]);
        assert!(!keys.verify_request_mac(&replay, &mac));
        // swapping k_req and k_acc fails
        let as_acc = keys.accept_mac(&tr, "");
        assert_ne!(as_acc, mac);
        // wrong secret
        let k2 = PairingKeys::derive(&PairingSecret::from_bytes([6; 16]), &t);
        assert!(!k2.verify_request_mac(&tr, &mac));
    }

    #[test]
    fn mac2_covers_exact_body_bytes() {
        let (n, j, t, x, s, e) = fixture();
        let keys = PairingKeys::derive(&s, &t);
        let tr = Transcript::new(&n, &j, &t, e, &x);
        let body = r#"{"v":1}"#;
        let m2 = keys.accept_mac(&tr, body);
        assert!(keys.verify_accept_mac(&tr, body, &m2));
        assert!(!keys.verify_accept_mac(&tr, r#"{"v": 1}"#, &m2));
    }

    #[test]
    fn sas_changes_with_ticket_and_nonce() {
        let (n, j, t, x, _, _) = fixture();
        let ns = Nonce16::from_bytes([7; 16]);
        let a = sas_words(&n, &j, &t, &x, &ns);
        assert_ne!(a, sas_words(&n, &j, &TicketId::from_bytes([30; 16]), &x, &ns));
        assert_ne!(a, sas_words(&n, &j, &t, &x, &Nonce16::from_bytes([70; 16])));
        assert_ne!(a, sas_words(&n, &j, &t, &[40; 32], &ns));
    }

    #[test]
    fn candidates_contain_correct_once() {
        let (n, ..) = fixture();
        let w = node_fingerprint_words(&n);
        for _ in 0..50 {
            let (c, idx) = approval_candidates(&w);
            assert_eq!(c.len(), APPROVAL_CANDIDATES);
            assert_eq!(c[idx], w);
            assert_eq!(c.iter().filter(|x| **x == w).count(), 1);
        }
    }

    #[test]
    fn message_shapes() {
        let m = PairClientMsg::PairRequest(PairRequest {
            v: 1,
            ticket_id: TicketId::from_bytes([0; 16]),
            device_name: "Ben's laptop".into(),
            device_kind: DeviceKind::Laptop,
            mac: Mac32::from_bytes([0; 32]),
        });
        let j = serde_json::to_value(&m).unwrap();
        assert_eq!(j["t"], "pair_request");
        assert!(j.get("joiner_id").is_none());
        let r = PairServerMsg::PairReject(PairReject { code: ErrorCode::PairingRejected, reason: Some(RejectReason::Poisoned) });
        assert_eq!(serde_json::to_string(&r).unwrap(), r#"{"t":"pair_reject","code":"pairing_rejected","reason":"poisoned"}"#);
    }
}
