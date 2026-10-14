//! Audit log (design 11, item 1, S4).
//!
//! File: `<state>/audit.jsonl`, one [`AuditEntry`] per line, append-only.
//!
//! Hash chain (normative):
//! ```text
//! entry.hash = SHA-256( prev_hash(32) || canonical(entry without "hash") )
//! canonical  = serde_json::to_vec of [`AuditEntryUnhashed`] (fixed field order,
//!              `detail` is a BTreeMap<String,String> so keys are sorted)
//! genesis prev_hash = 32 zero bytes
//! ```
//! Every [`CHECKPOINT_EVERY`] entries the node appends a `checkpoint` entry whose
//! `detail.sig` is an Ed25519 signature by the node key over
//! `"spark-link/audit-checkpoint/v1" || seq(u64 BE) || hash_of_previous_entry(32)`.
//! Admin agents remember the last checkpoint they saw and alert if the chain
//! rewinds or breaks. Unauthenticated failures are aggregated into one line per
//! minute (`pairing_failures_aggregate`). Rotation carries `prev_hash` across files.

use std::collections::BTreeMap;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ids::{EndpointId, hex_lower};

pub const CHECKPOINT_EVERY: u64 = 256;
pub const CHECKPOINT_DOMAIN: &[u8] = b"spark-link/audit-checkpoint/v1";
pub const GENESIS_PREV_HASH: [u8; 32] = [0; 32];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditKind {
    NodeInstalled,
    /// SSH trust-on-first-use decision recorded at install (item 5).
    HostKeyTofu,
    OwnerSet,
    /// `admin reset --owner <id> --revoke-admins` (S3).
    AdminReset,
    TicketCreated,
    TicketCancelled,
    TicketExpired,
    TicketBurned,
    TicketPoisoned,
    PairingPending,
    PairingApproved,
    PairingDenied,
    PairingWrongCode,
    PairingExpired,
    /// Aggregated unauthenticated failures, at most one line per minute (S4).
    PairingFailuresAggregate,
    LateMac,
    DevicePaired,
    DeviceRevoked,
    DevicePromoted,
    DeviceDemoted,
    GrantChanged,
    GrantDeleted,
    RoleSwitchRequested,
    RoleChanged,
    EngineAdopted,
    EngineExposed,
    /// Item 7: the owner approved closing an exposed engine (UV), before the re-probe.
    EngineExposureFixApproved,
    /// Item 7: a re-probe found the engine no longer exposed (before/after listen addresses).
    EngineExposureFixed,
    ModelAllowed,
    ModelDisallowed,
    /// Any action taken with the local `spark-link-node` CLI (S4).
    CliAction,
    RequestRejectedAggregate,
    Checkpoint,
    /// Amendment A1: a device paused / resumed its own link.
    DevicePaused,
    DeviceResumed,
    /// Amendment A1: a wrong approval token on the LAN page (at most
    /// `owner_link::LAN_MAX_BAD_TOKENS` per pairing, then `ticket_burned`).
    LanBadToken,
    /// Amendment A1: a device removed ITSELF ("Remove" in the app, `DELETE
    /// /link/v1/devices/self`, or `devices revoke <id> --forget` over SSH). The
    /// node forgets it (row, grants, pause flag) WITHOUT denylisting its key;
    /// an owner removing itself leaves the node without an owner.
    DeviceRemovedSelf,
    /// 0.25: a compute job was admitted (subject: job id; detail: runtime, owner, input bytes).
    JobCreated,
    /// 0.25: a compute job's container was launched (detail: runtime, image digest, devices).
    JobStarted,
    /// 0.25: a compute job finished (detail: state, error code, output bytes, cleanup).
    JobFinished,
    /// 0.25: the owner deleted a finished job, or retention pruned an acknowledged one.
    JobDeleted,
    /// 0.25: an unclaimed job passed its grace period and was removed (detail: reason
    /// `unacknowledged` or `abandoned_staging`, idle seconds, grace seconds, bytes).
    JobExpired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "snake_case")]
pub enum AuditActor {
    Device(EndpointId),
    Cli,
    Node,
    Unauthenticated,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub seq: u64,
    pub ts: u64,
    /// hex
    pub prev_hash: String,
    pub kind: AuditKind,
    pub actor: AuditActor,
    pub subject: Option<String>,
    pub detail: BTreeMap<String, String>,
    /// hex
    pub hash: String,
}

/// The hashed portion of an entry, in canonical field order.
#[derive(Serialize)]
pub struct AuditEntryUnhashed<'a> {
    pub seq: u64,
    pub ts: u64,
    pub prev_hash: &'a str,
    pub kind: AuditKind,
    pub actor: &'a AuditActor,
    pub subject: &'a Option<String>,
    pub detail: &'a BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditCheckpoint {
    pub seq: u64,
    /// hex hash of the entry before the checkpoint
    pub hash: String,
    /// base64url Ed25519 signature
    pub sig: String,
}

fn decode_hex32(s: &str) -> Option<[u8; 32]> {
    let v = data_encoding::HEXLOWER.decode(s.as_bytes()).ok()?;
    v.try_into().ok()
}

impl AuditEntry {
    /// Build the next entry after `prev` (None = genesis).
    pub fn next(prev: Option<&AuditEntry>, ts: u64, kind: AuditKind, actor: AuditActor, subject: Option<String>, detail: BTreeMap<String, String>) -> AuditEntry {
        let (seq, prev_hash) = match prev {
            Some(p) => (p.seq + 1, p.hash.clone()),
            None => (0, hex_lower(&GENESIS_PREV_HASH)),
        };
        let mut e = AuditEntry { seq, ts, prev_hash, kind, actor, subject, detail, hash: String::new() };
        e.hash = hex_lower(&e.compute_hash());
        e
    }

    pub fn compute_hash(&self) -> [u8; 32] {
        let unhashed = AuditEntryUnhashed {
            seq: self.seq,
            ts: self.ts,
            prev_hash: &self.prev_hash,
            kind: self.kind,
            actor: &self.actor,
            subject: &self.subject,
            detail: &self.detail,
        };
        let canonical = serde_json::to_vec(&unhashed).expect("audit entry serializes");
        let prev = decode_hex32(&self.prev_hash).unwrap_or([0xff; 32]);
        let mut h = Sha256::new();
        h.update(prev);
        h.update(&canonical);
        h.finalize().into()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AuditChainError {
    #[error("audit entry {seq} has a bad hash")]
    BadHash { seq: u64 },
    #[error("audit entry {seq} does not link to the previous entry")]
    BrokenLink { seq: u64 },
    #[error("audit sequence jumps at {seq}")]
    SeqGap { seq: u64 },
    #[error("audit checkpoint at {seq} has an invalid signature")]
    BadCheckpoint { seq: u64 },
}

/// Message signed by a checkpoint.
pub fn checkpoint_message(seq: u64, prev_entry_hash: &[u8; 32]) -> Vec<u8> {
    let mut m = Vec::with_capacity(CHECKPOINT_DOMAIN.len() + 40);
    m.extend_from_slice(CHECKPOINT_DOMAIN);
    m.extend_from_slice(&seq.to_be_bytes());
    m.extend_from_slice(prev_entry_hash);
    m
}

pub fn verify_checkpoint(node_id: &EndpointId, cp: &AuditCheckpoint) -> bool {
    let Some(hash) = decode_hex32(&cp.hash) else { return false };
    let Ok(sig_bytes) = crate::ids::b64url_decode_exact::<64>(&cp.sig, "signature") else { return false };
    let Ok(vk) = VerifyingKey::from_bytes(node_id.as_bytes()) else { return false };
    vk.verify(&checkpoint_message(cp.seq, &hash), &Signature::from_bytes(&sig_bytes)).is_ok()
}

/// Verify a contiguous run of entries. `expected_prev` is the hash before the
/// first entry (genesis or the end of the previous rotated file). Checkpoint
/// entries (kind `checkpoint`, detail `sig`) are verified with `node_id`.
pub fn verify_chain(entries: &[AuditEntry], expected_prev: &[u8; 32], node_id: Option<&EndpointId>) -> Result<(), AuditChainError> {
    let mut prev = *expected_prev;
    let mut last_seq: Option<u64> = None;
    for e in entries {
        if let Some(s) = last_seq
            && e.seq != s + 1
        {
            return Err(AuditChainError::SeqGap { seq: e.seq });
        }
        if decode_hex32(&e.prev_hash) != Some(prev) {
            return Err(AuditChainError::BrokenLink { seq: e.seq });
        }
        let h = e.compute_hash();
        if hex_lower(&h) != e.hash {
            return Err(AuditChainError::BadHash { seq: e.seq });
        }
        if e.kind == AuditKind::Checkpoint
            && let Some(nid) = node_id
        {
            let cp = AuditCheckpoint { seq: e.seq, hash: e.prev_hash.clone(), sig: e.detail.get("sig").cloned().unwrap_or_default() };
            if !verify_checkpoint(nid, &cp) {
                return Err(AuditChainError::BadCheckpoint { seq: e.seq });
            }
        }
        prev = h;
        last_seq = Some(e.seq);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn d(k: &str, v: &str) -> BTreeMap<String, String> {
        BTreeMap::from([(k.to_string(), v.to_string())])
    }

    #[test]
    fn chain_detects_edits_and_deletions() {
        let a = AuditEntry::next(None, 1, AuditKind::NodeInstalled, AuditActor::Cli, None, d("label", "spark"));
        let b = AuditEntry::next(Some(&a), 2, AuditKind::TicketCreated, AuditActor::Device(EndpointId::from_bytes([1; 32])), Some("t".into()), BTreeMap::new());
        let c = AuditEntry::next(Some(&b), 3, AuditKind::DeviceRevoked, AuditActor::Node, None, BTreeMap::new());
        let all = vec![a.clone(), b.clone(), c.clone()];
        assert_eq!(verify_chain(&all, &GENESIS_PREV_HASH, None), Ok(()));
        // JSONL round trip keeps hashes valid
        let lines: Vec<String> = all.iter().map(|e| serde_json::to_string(e).unwrap()).collect();
        let back: Vec<AuditEntry> = lines.iter().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(verify_chain(&back, &GENESIS_PREV_HASH, None), Ok(()));
        // edit middle
        let mut edited = all.clone();
        edited[1].detail.insert("x".into(), "y".into());
        assert_eq!(verify_chain(&edited, &GENESIS_PREV_HASH, None), Err(AuditChainError::BadHash { seq: 1 }));
        // delete middle
        let deleted = vec![a, c];
        assert_eq!(verify_chain(&deleted, &GENESIS_PREV_HASH, None), Err(AuditChainError::SeqGap { seq: 2 }));
    }

    #[test]
    fn checkpoint_signature() {
        let sk = SigningKey::from_bytes(&[9; 32]);
        let node = EndpointId::from_bytes(sk.verifying_key().to_bytes());
        let a = AuditEntry::next(None, 1, AuditKind::NodeInstalled, AuditActor::Cli, None, BTreeMap::new());
        let h = a.compute_hash();
        let sig = sk.sign(&checkpoint_message(1, &h));
        let cp_entry = AuditEntry::next(Some(&a), 2, AuditKind::Checkpoint, AuditActor::Node, None, d("sig", &crate::ids::b64url(&sig.to_bytes())));
        assert_eq!(verify_chain(&[a.clone(), cp_entry.clone()], &GENESIS_PREV_HASH, Some(&node)), Ok(()));
        let wrong = EndpointId::from_bytes(SigningKey::from_bytes(&[8; 32]).verifying_key().to_bytes());
        assert_eq!(verify_chain(&[a, cp_entry], &GENESIS_PREV_HASH, Some(&wrong)), Err(AuditChainError::BadCheckpoint { seq: 1 }));
    }
}
