//! The `spark-link-node` command line and on-Spark layout (design 5a, 10, 11;
//! items 5, 7; S3, S4). The agent's SSH bootstrap drives these commands, so
//! their arguments and JSON output are part of the frozen contract.
//!
//! Layout on the Spark (`$HOME` of the node user):
//! - state:   `~/.local/state/delores/spark-link/` (0700): `node.key` (0600), `state.json`,
//!   `audit.jsonl`, `status.json` (written by `serve`)
//! - install: `~/.local/share/delores/spark-link/versions/<ver>/spark-link-node`, `bin/spark-link-node`
//!   -> symlink to the current version
//! - config:  `~/.config/delores/spark-link/node.toml`
//! - unit:    `~/.config/systemd/user/spark-link-node.service` (+ `loginctl enable-linger`)
//!
//! Commands (all accept `--state-dir <path>` and `--json`; with `--json` exactly
//! one JSON object is printed to stdout, also on failure:
//! `{"ok":false,"error":{"code","message"}}`):
//! ```text
//! spark-link-node install --owner <z32> --label <text> [--owner-name <text>]
//!                         [--host-key-sha256 <SHA256:...>] [--tofu-confirmed]
//!                         [--udp-port <n>] [--no-systemd] [--ollama-url <url>]
//!     -> InstallReport. Refuses (exit 3, already_owned) if an owner exists. Order:
//!        node.key -> owner + audit -> unit file -> `systemctl --user enable --now` LAST,
//!        so an interrupted install leaves the service disabled and the pairing ALPN closed.
//!     Amendment A1: `--owner` (and `--owner-name`) may be omitted; the node is then
//!     installed with no owner (output `owner_link::OwnerlessInstallReport`, `owner_pending`)
//!     and the first approved owner-ticket pairing becomes the owner.
//! spark-link-node serve                      run the node (systemd ExecStart)
//! spark-link-node status                     -> StatusReport
//! spark-link-node version                    -> {"ok":true,"version":"x.y.z","protocol":{..}}
//! spark-link-node devices list               -> {"ok":true,"devices":[DeviceRecord]}
//! spark-link-node devices revoke <z32> [--cascade]
//! spark-link-node pairings list              -> {"ok":true,"pairings":[PendingPairing]}
//! spark-link-node pairings approve <pairing_id> --code "<w1 w2 w3 w4>"   (UV = spark_cli)
//! spark-link-node pairings deny <pairing_id>
//! spark-link-node owner-ticket --device-name <text> [--ttl <1..=300, default 300>]
//!     -> owner_link::OwnerTicket (Amendment A1). Needs the running node (exit 8 NOT_RUNNING).
//! spark-link-node admin reset --owner <z32> [--revoke-admins]
//! spark-link-node audit verify               exit 0 if the chain verifies, 7 otherwise
//! spark-link-node role show | role set <llm|compute> [--mode drain|stop]
//! spark-link-node engines list | engines adopt-ollama [--url http://127.0.0.1:11434]
//! spark-link-node engines ollama-dropin      print the owner-approvable systemd drop-in
//!                                            (OLLAMA_HOST=127.0.0.1:11434, no ORIGINS=*); never applies it
//! spark-link-node models allow <model> | models disallow <model>
//! ```
//! Every CLI mutation writes an audit entry with actor `cli` (S4).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::errors::ErrorBody;
use crate::ids::EndpointId;
use crate::roles::Role;
use crate::version::ProtocolRange;

pub const NODE_BIN: &str = "spark-link-node";
/// Default UDP port the node's iroh endpoint binds (IPv4 and IPv6).
pub const DEFAULT_UDP_PORT: u16 = 47821;
pub const SYSTEMD_UNIT: &str = "spark-link-node.service";
pub const NODE_KEY_FILE: &str = "node.key";
pub const STATE_FILE: &str = "state.json";
pub const AUDIT_FILE: &str = "audit.jsonl";
pub const STATUS_FILE: &str = "status.json";
/// Default adopted-Ollama base URL probed at install/serve.
pub const DEFAULT_OLLAMA_URL: &str = "http://127.0.0.1:11434";

pub mod exit {
    pub const OK: i32 = 0;
    pub const USAGE: i32 = 2;
    pub const ALREADY_OWNED: i32 = 3;
    pub const SYSTEMD_UNAVAILABLE: i32 = 4;
    pub const IO: i32 = 5;
    pub const NOT_INSTALLED: i32 = 6;
    pub const VERIFY_FAILED: i32 = 7;
    /// Amendment A1: the command needs the running node (`owner-ticket`) and `serve` is not running.
    pub const NOT_RUNNING: i32 = 8;
    pub const INTERNAL: i32 = 70;
}

pub fn node_state_dir(home: &Path) -> PathBuf {
    home.join(".local").join("state").join("delores").join("spark-link")
}
pub fn node_install_root(home: &Path) -> PathBuf {
    home.join(".local").join("share").join("delores").join("spark-link")
}
pub fn node_config_file(home: &Path) -> PathBuf {
    home.join(".config").join("delores").join("spark-link").join("node.toml")
}
pub fn systemd_user_unit_file(home: &Path) -> PathBuf {
    home.join(".config").join("systemd").join("user").join(SYSTEMD_UNIT)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitKind {
    /// `systemd --user` + linger
    User,
    /// system unit installed through `sudo -A` (L3 path; not in M1 unless needed)
    System,
    /// `--no-systemd` (tests)
    None,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InstallReport {
    pub ok: bool,
    pub node_id: EndpointId,
    pub owner: EndpointId,
    pub label: String,
    pub version: String,
    pub state_dir: String,
    pub unit: UnitKind,
    pub linger: Option<bool>,
    pub udp_port: u16,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatusReport {
    pub ok: bool,
    pub installed: bool,
    pub running: bool,
    pub node_id: Option<EndpointId>,
    pub label: Option<String>,
    pub version: Option<String>,
    pub protocol: Option<ProtocolRange>,
    pub role: Option<Role>,
    pub relay_url: Option<String>,
    pub direct_addrs: Vec<String>,
    pub udp_port: Option<u16>,
    pub pid: Option<u32>,
    /// 0.25 (additive): the compute job store, written by the running node. Absent from older
    /// nodes and while no node is running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jobs: Option<JobsReport>,
}

/// The compute job store in `status --json` (0.25): how much is retained, what is unclaimed and
/// when it expires, and what expiry has removed since the node started.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobsReport {
    /// Retained job records, in any state.
    pub retained: usize,
    /// Finished, cleanup confirmed, not acknowledged: unclaimed results.
    pub unacknowledged: usize,
    pub acknowledged: usize,
    /// Created and not started.
    pub staging: usize,
    /// Queued, running or cancelling.
    pub active: usize,
    /// Finished records whose container cleanup is unconfirmed (they block admission).
    pub cleanup_unconfirmed: usize,
    /// Accounted bytes (declared inputs plus verified outputs).
    pub bytes: u64,
    /// Grace period of unclaimed jobs, seconds.
    pub unclaimed_grace_s: u64,
    /// When the next unclaimed job expires (Unix seconds), if any is pending.
    pub next_expiry_at: Option<u64>,
    /// Unclaimed jobs removed by expiry since this node process started.
    pub expired_since_start: u64,
    /// Time of the last expiry (Unix seconds).
    pub last_expired_at: Option<u64>,
}

/// `{"ok":false,"error":{...}}`
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CliFailure {
    pub ok: bool,
    pub error: ErrorBody,
}
