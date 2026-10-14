//! # spark-link-proto: the frozen Spark Link contract
//!
//! This crate is the single source of truth shared by `spark-link-node`,
//! `spark-link-agent`, `spark-link-client` and (mirrored by hand) the
//! TypeScript package `@delores/spark-link`. `docs/PROTOCOL.md` is the
//! normative prose; where the two disagree, file a contract change instead of
//! working around it in one crate.
//!
//! Layout:
//! - [`ids`], [`secret`]: identities (EndpointId z-base-32), ticket/pairing ids,
//!   redacting secret types.
//! - [`scopes`], [`roles`]: per-app scopes, ticketability, the exclusive Spark
//!   role (LLM host or compute host) and placement.
//! - [`pairing`], [`link`], [`words`]: QR/link payload, HKDF/HMAC pairing
//!   transcript (design 15.2 item 11), short-authentication words (item 1).
//! - [`api`]: node HTTP API over the `delores/spark-link/http/1` ALPN.
//! - [`ipc`], [`frame`], [`paths`]: agent IPC (endpoint.json, HMAC handshake,
//!   typed operations) used by every app-side client.
//! - [`jobs`]: the closed compute-job IPC operations; [`job_contract`]: what a node admits as a
//!   compute job and how long it retains it; [`job_transfer`]: chunk-hashed resumable transfers,
//!   output manifests and job events (both shared by the node and PhaseForge's backend).
//! - [`audit`]: hash-chained audit entries with signed checkpoints.
//! - [`mdns`], [`relay`], [`node_cli`], [`release`], [`version`], [`errors`].
//! - [`owner_link`]: Amendment A1 (owner QR + LAN approval page, device pause/resume/self-revoke).
//!
//! Everything here is sans-IO except the optional `io` feature's async frame
//! helpers.

#![forbid(unsafe_code)]

pub mod api;
pub mod audit;
pub mod errors;
pub mod frame;
pub mod ids;
pub mod ipc;
pub mod job_contract;
pub mod job_transfer;
pub mod jobs;
pub mod link;
pub mod mdns;
pub mod node_cli;
pub mod owner_link;
pub mod pairing;
pub mod paths;
pub mod relay;
pub mod release;
pub mod roles;
pub mod scopes;
pub mod secret;
pub mod version;
pub mod words;

pub use errors::{ApiError, ErrorBody, ErrorCode};
pub use ids::{AppId, DeviceKind, EndpointId, Mac32, Nonce16, Nonce32, PairingId, TicketId};
pub use roles::{Placement, Role, RoleClass, RoleSwitchMode};
pub use scopes::{Scope, ScopeSet};
pub use secret::{PairingSecret, RedactedString};
pub use version::{ProtocolRange, PROTOCOL};

/// Seconds since the Unix epoch (UTC). All protocol timestamps use this unit.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Fill `buf` from the OS CSPRNG. Panics only if the OS RNG is unavailable,
/// which is unrecoverable for a security protocol.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).expect("OS random number generator unavailable");
    buf
}

/// Errors produced by parsing or validating contract values.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtoError {
    #[error("invalid encoding for {0}")]
    Encoding(&'static str),
    #[error("invalid length for {0}")]
    Length(&'static str),
    #[error("invalid value for {field}: {reason}")]
    Invalid { field: &'static str, reason: &'static str },
}
