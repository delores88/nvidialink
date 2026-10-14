//! Agent IPC: the only API apps use (design 3, 6; items 2, 4, 8, 9, 10).
//!
//! ## Transport
//! Windows named pipe / Unix domain socket; path and `server_key` come from
//! `endpoint.json` (see [`crate::paths`]). Frames per [`crate::frame`] with
//! max [`IPC_FRAME_MAX`]. There is no HTTP on the IPC and no generic
//! method+path passthrough: every operation is a typed [`Request`].
//!
//! ## Handshake (item 4, normative)
//! ```text
//! client -> {"t":"hello","v":1,"nonce":"<32 random bytes, b64url>"}
//! agent  -> {"t":"hello_ok","v":1,"proof":"<b64url>","agent":{AgentHello}}
//!           proof = HMAC-SHA256(server_key, "spark-link/ipc/v1" || nonce(32 raw bytes) || UTF-8(endpoint.path))
//! client: verify proof in constant time; on failure close and send NOTHING else
//! client -> {"t":"identify","app_id":"phaseforge","app_version":"0.21.0"}
//! agent  -> {"t":"identify_ok","caller":{CallerInfo}}    (pid + exe path from the OS, never from the client)
//! ```
//! The agent rejects callers in an AppContainer/LPAC, below Medium integrity, or
//! (Unix) with a different uid, before `hello_ok`.
//!
//! ## Requests
//! ```text
//! client -> {"t":"req","id":<u32>,"op":"<op>","params":{...}}      params is REQUIRED ({} when empty)
//! agent  -> {"t":"event","id":<u32>,"event":{...}}                  zero or more, streaming ops only
//! agent  -> {"t":"res","id":<u32>,"ok":true,"result":{...}}         exactly one, ends the request
//!         | {"t":"res","id":<u32>,"ok":false,"error":{code,message,detail?}}
//! client -> {"t":"cancel","id":<u32>}                               agent answers res error `cancelled`
//! agent  -> {"t":"fatal","error":{...}}                             then closes the connection
//! ```
//! Up to [`IPC_MAX_INFLIGHT`] requests may be outstanding per connection; ids
//! are chosen by the client and unique per connection.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::api::{DeviceRecord, Grant, PairingOutcome, PendingPairing, RoleState, TicketSummary};
use crate::errors::{ApiError, ErrorBody};
use crate::ids::{AppId, DeviceKind, EndpointId, Key32, Mac32, Nonce32, PairingId, TicketId};
use crate::node_cli::UnitKind;
use crate::release::SignedManifest;
use crate::roles::{Role, RoleSwitchMode};
use crate::scopes::{Scope, ScopeSet};
use crate::secret::RedactedString;
use crate::version::ProtocolRange;
use crate::words::Words;

pub const IPC_VERSION: u16 = 1;
pub const IPC_LABEL: &[u8] = b"spark-link/ipc/v1";
pub const IPC_FRAME_MAX: usize = 8 * 1024 * 1024;
pub const IPC_HELLO_TIMEOUT_MS: u64 = 5_000;
pub const IPC_MAX_INFLIGHT: usize = 16;

// ---------------------------------------------------------------------------
// endpoint.json and handshake
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcTransport {
    NamedPipe,
    UnixSocket,
}

/// `endpoint.json`, written atomically by the agent (temp file + rename) only
/// while it holds the per-user lock and the pipe/socket.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EndpointFile {
    pub v: u16,
    pub transport: IpcTransport,
    /// Pipe name (`\\.\pipe\delores-spark-link-<SID>-<hex>`) or socket path.
    pub path: String,
    /// 32 random bytes, regenerated on every agent start.
    pub server_key: Key32,
    pub pid: u32,
    pub agent_version: String,
    /// hex SHA-256 of the running agent executable (S6).
    pub agent_sha256: String,
    pub started_at: u64,
}

fn proof_mac(server_key: &Key32, nonce: &Nonce32, endpoint_path: &str) -> Hmac<Sha256> {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(server_key.as_bytes()).expect("32-byte key");
    m.update(IPC_LABEL);
    m.update(nonce.as_bytes());
    m.update(endpoint_path.as_bytes());
    m
}

pub fn handshake_proof(server_key: &Key32, nonce: &Nonce32, endpoint_path: &str) -> Mac32 {
    Mac32::from_bytes(proof_mac(server_key, nonce, endpoint_path).finalize().into_bytes().into())
}

/// Constant-time verification of the agent's `proof`.
pub fn verify_handshake_proof(server_key: &Key32, nonce: &Nonce32, endpoint_path: &str, proof: &Mac32) -> bool {
    proof_mac(server_key, nonce, endpoint_path).verify_slice(proof.as_bytes()).is_ok()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentHello {
    pub agent_version: String,
    pub agent_sha256: String,
    pub protocol: ProtocolRange,
    pub ipc_version: u16,
    /// 0.25 (additive): optional features of this agent, e.g. [`AGENT_FEATURE_JOBS_V2`]. Older
    /// agents omit it; apps then use only what 0.21 offered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
}

/// The agent forwards the `jobs.v2` operations (chunk hashes, `manifest`, `events`, 4 MiB
/// chunks). An app uses them only when both this feature and the node's `jobs.v2` are present.
pub const AGENT_FEATURE_JOBS_V2: &str = "jobs.v2";

/// Caller identity as determined by the agent from the OS
/// (GetNamedPipeClientProcessId + QueryFullProcessImageNameW; SO_PEERCRED / LOCAL_PEERTOKEN).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CallerInfo {
    pub pid: u32,
    pub exe_path: String,
    pub app_id: AppId,
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
// A frame carries one bounded request; boxing would add an allocation to every
// control request without reducing the already bounded wire buffer.
#[allow(clippy::large_enum_variant)]
pub enum ClientFrame {
    Hello {
        v: u16,
        nonce: Nonce32,
    },
    Identify {
        app_id: AppId,
        app_version: String,
    },
    Req {
        id: u32,
        #[serde(flatten)]
        req: Request,
    },
    Cancel {
        id: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerFrame {
    HelloOk {
        v: u16,
        proof: Mac32,
        agent: AgentHello,
    },
    IdentifyOk {
        caller: CallerInfo,
    },
    Event {
        id: u32,
        event: serde_json::Value,
    },
    Res {
        id: u32,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ErrorBody>,
    },
    Fatal {
        error: ErrorBody,
    },
}

impl ServerFrame {
    pub fn ok<T: Serialize>(id: u32, result: &T) -> Self {
        ServerFrame::Res { id, ok: true, result: Some(serde_json::to_value(result).unwrap_or(serde_json::Value::Null)), error: None }
    }
    pub fn err(id: u32, e: ApiError) -> Self {
        ServerFrame::Res { id, ok: false, result: None, error: Some(e.error) }
    }
    pub fn event<T: Serialize>(id: u32, event: &T) -> Self {
        ServerFrame::Event { id, event: serde_json::to_value(event).unwrap_or(serde_json::Value::Null) }
    }
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// Every IPC operation. Wire: `"op": "<name>", "params": {...}`.
///
/// | op | params | events | result | agent UI |
/// |---|---|---|---|---|
/// | `agent.info` | `{}` | - | [`AgentInfo`] | - |
/// | `sparks.list` | `{}` | - | [`SparksList`] | - |
/// | `sparks.status` | [`NodeRef`] | - | `api::NodeInfo` (refreshed) | - |
/// | `grants.request` | [`GrantRequestParams`] | - | [`GrantResult`] | consent dialog with caller exe path |
/// | `infer.models` | [`NodeRef`] | - | OpenAI `/v1/models` JSON | - |
/// | `infer.chat` | [`InferParams`] | [`InferStreamEvent`] if `body.stream` | OpenAI JSON, or `{"done":true}` when streamed | - |
/// | `infer.completions` | [`InferParams`] | as above | as above | - |
/// | `infer.embeddings` | [`InferParams`] | - | OpenAI JSON | - |
/// | `engines.ensure` | [`EnsureParams`] | - | `api::EnsureResponse` | - |
/// | `discovery.lan` | [`DiscoveryParams`] | - | [`DiscoveryResult`] | - |
/// | `bootstrap.ssh` | [`BootstrapSshParams`] | [`BootstrapEvent`] (+ `setup.progress` in A1 setup) | [`BootstrapResult`] | consent + host-key (TOFU) dialogs; none in A1 setup (first-party app + pinned host key) |
/// | `pairing.share` | [`PairingShareParams`] | [`PairingShareEvent`] | [`PairingShareResult`] | consent + OS user verification before minting |
/// | `pairing.redeem` | [`PairingRedeemParams`] | [`PairingRedeemEvent`] | [`PairingRedeemResult`] | confirmation dialog (item 9) |
/// | `role.request_switch` | [`RoleSwitchParams`] | - | `api::RoleState` | owner confirmation + OS user verification |
/// | `events.subscribe` | `{}` | [`AgentEvent`] | (only on cancel) | - |
/// | `agent.upgrade` | [`AgentUpgradeParams`] | - | [`UpgradeResult`] | - |
///
/// Amendment A1 (owner direction 2026-10-11), the laptop side of the owner link
/// (PROTOCOL 5.4 and 6.3; `docs/contract-requests/pair-owner-flow.md` section 7):
///
/// | op | params | events | result | agent UI |
/// |---|---|---|---|---|
/// | `link.owner_pair.start` | [`OwnerPairStartParams`] | [`OwnerPairEvent`] (`setup.progress`) | [`OwnerPairStartResult`] | none for a first-party app with a pinned host key; otherwise the bootstrap consent and host-key (TOFU) dialogs |
/// | `link.owner_pair.cancel` | [`OwnerPairCancelParams`] | - | `{"cancelled": bool}` | - |
/// | `link.status` | `{}` | - | a bare array of [`LinkStatusRow`] | - |
/// | `link.set_enabled` | [`LinkSetEnabledParams`] | - | [`LinkSetEnabledResult`] | - (first-party apps only) |
/// | `link.remove` | [`LinkRemoveParams`] | - | [`LinkRemoveResult`] | - (first-party apps only) |
/// | `link.role.set` | [`RoleSwitchParams`] | - | `api::RoleState` | owner device; confirmation + OS user verification |
///
/// After `link.owner_pair.start` returns, the pairing continues inside the
/// agent; its outcome arrives on `events.subscribe` as
/// [`AgentEvent::OwnerPairStatus`] (`"kind": "owner_pair.status"`), for the
/// calling app only. The bridge passes its saved SSH access again when starting
/// pairing; the agent does not retain SSH credentials or sessions. Without
/// `ssh`, the operation fails with `ssh_required`. On approval a
/// first-party caller is granted `infer`, `metrics` and `models.read` on the
/// Spark with no further prompt (the phone approval is the consent).
///
/// Admin operations (security review: device list/revoke on the device side).
/// Only the agent's OWN executable may call them, with the reserved app id
/// `spark-link-agent` (the `spark-link-agent devices|grants|tickets|engines|
/// recover-ownership` commands); any app gets `forbidden`. Mutations need OS
/// user verification shown by the agent.
///
/// | op | params | result | agent UI |
/// |---|---|---|---|
/// | `admin.devices.list` | [`NodeRef`] | [`AdminDevices`] | - |
/// | `admin.devices.revoke` | [`AdminRevokeParams`] | `api::RevokeResponse` | OS user verification |
/// | `admin.grants.list` | [`AdminGrantsListParams`] | [`AdminGrants`] | - |
/// | `admin.grants.revoke` | [`AdminGrantRevokeParams`] | `{"revoked":true}` | OS user verification |
/// | `admin.tickets.list` | [`NodeRef`] | [`AdminTickets`] | - |
/// | `admin.tickets.cancel` | [`AdminTicketCancelParams`] | `{"cancelled":true}` | - |
/// | `admin.engines.fix_exposure` | [`AdminFixExposureParams`] | `api::ExposureFixResponse` | commands + "I ran them", OS user verification |
/// | `admin.recover_ownership` | [`RecoverOwnershipParams`] | [`RecoverOwnershipResult`] | consent, host key (TOFU), OS user verification |
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", content = "params")]
pub enum Request {
    #[serde(rename = "jobs.request")]
    JobsRequest(crate::jobs::JobRequest),
    #[serde(rename = "agent.info")]
    AgentInfo(Empty),
    #[serde(rename = "sparks.list")]
    SparksList(Empty),
    #[serde(rename = "sparks.status")]
    SparksStatus(NodeRef),
    #[serde(rename = "grants.request")]
    GrantsRequest(GrantRequestParams),
    #[serde(rename = "infer.models")]
    InferModels(NodeRef),
    #[serde(rename = "infer.chat")]
    InferChat(InferParams),
    #[serde(rename = "infer.completions")]
    InferCompletions(InferParams),
    #[serde(rename = "infer.embeddings")]
    InferEmbeddings(InferParams),
    #[serde(rename = "engines.ensure")]
    EnginesEnsure(EnsureParams),
    #[serde(rename = "discovery.lan")]
    DiscoveryLan(DiscoveryParams),
    #[serde(rename = "bootstrap.ssh")]
    BootstrapSsh(BootstrapSshParams),
    #[serde(rename = "pairing.share")]
    PairingShare(PairingShareParams),
    #[serde(rename = "pairing.redeem")]
    PairingRedeem(PairingRedeemParams),
    #[serde(rename = "role.request_switch")]
    RoleRequestSwitch(RoleSwitchParams),
    #[serde(rename = "events.subscribe")]
    EventsSubscribe(Empty),
    #[serde(rename = "agent.upgrade")]
    AgentUpgrade(AgentUpgradeParams),
    #[serde(rename = "admin.devices.list")]
    AdminDevicesList(NodeRef),
    #[serde(rename = "admin.devices.revoke")]
    AdminDevicesRevoke(AdminRevokeParams),
    #[serde(rename = "admin.grants.list")]
    AdminGrantsList(AdminGrantsListParams),
    #[serde(rename = "admin.grants.revoke")]
    AdminGrantsRevoke(AdminGrantRevokeParams),
    #[serde(rename = "admin.tickets.list")]
    AdminTicketsList(NodeRef),
    #[serde(rename = "admin.tickets.cancel")]
    AdminTicketsCancel(AdminTicketCancelParams),
    #[serde(rename = "admin.engines.fix_exposure")]
    AdminFixExposure(AdminFixExposureParams),
    #[serde(rename = "admin.recover_ownership")]
    AdminRecoverOwnership(RecoverOwnershipParams),
    // ---- Amendment A1: owner link (laptop side) ----
    #[serde(rename = "link.owner_pair.start")]
    LinkOwnerPairStart(OwnerPairStartParams),
    #[serde(rename = "link.owner_pair.cancel")]
    LinkOwnerPairCancel(OwnerPairCancelParams),
    #[serde(rename = "link.status")]
    LinkStatus(Empty),
    #[serde(rename = "link.set_enabled")]
    LinkSetEnabled(LinkSetEnabledParams),
    #[serde(rename = "link.remove")]
    LinkRemove(LinkRemoveParams),
    #[serde(rename = "link.role.set")]
    LinkRoleSet(LinkRoleSetParams),
}

impl Request {
    pub fn op(&self) -> &'static str {
        match self {
            Request::AgentInfo(_) => "agent.info",
            Request::JobsRequest(_) => "jobs.request",
            Request::SparksList(_) => "sparks.list",
            Request::SparksStatus(_) => "sparks.status",
            Request::GrantsRequest(_) => "grants.request",
            Request::InferModels(_) => "infer.models",
            Request::InferChat(_) => "infer.chat",
            Request::InferCompletions(_) => "infer.completions",
            Request::InferEmbeddings(_) => "infer.embeddings",
            Request::EnginesEnsure(_) => "engines.ensure",
            Request::DiscoveryLan(_) => "discovery.lan",
            Request::BootstrapSsh(_) => "bootstrap.ssh",
            Request::PairingShare(_) => "pairing.share",
            Request::PairingRedeem(_) => "pairing.redeem",
            Request::RoleRequestSwitch(_) => "role.request_switch",
            Request::EventsSubscribe(_) => "events.subscribe",
            Request::AgentUpgrade(_) => "agent.upgrade",
            Request::AdminDevicesList(_) => "admin.devices.list",
            Request::AdminDevicesRevoke(_) => "admin.devices.revoke",
            Request::AdminGrantsList(_) => "admin.grants.list",
            Request::AdminGrantsRevoke(_) => "admin.grants.revoke",
            Request::AdminTicketsList(_) => "admin.tickets.list",
            Request::AdminTicketsCancel(_) => "admin.tickets.cancel",
            Request::AdminFixExposure(_) => "admin.engines.fix_exposure",
            Request::AdminRecoverOwnership(_) => "admin.recover_ownership",
            Request::LinkOwnerPairStart(_) => "link.owner_pair.start",
            Request::LinkOwnerPairCancel(_) => "link.owner_pair.cancel",
            Request::LinkStatus(_) => "link.status",
            Request::LinkSetEnabled(_) => "link.set_enabled",
            Request::LinkRemove(_) => "link.remove",
            Request::LinkRoleSet(_) => "link.role.set",
        }
    }

    /// Admin operations: only the agent's own executable (reserved app id).
    pub fn is_admin(&self) -> bool {
        matches!(
            self,
            Request::AdminDevicesList(_)
                | Request::AdminDevicesRevoke(_)
                | Request::AdminGrantsList(_)
                | Request::AdminGrantsRevoke(_)
                | Request::AdminTicketsList(_)
                | Request::AdminTicketsCancel(_)
                | Request::AdminFixExposure(_)
                | Request::AdminRecoverOwnership(_)
        )
    }

    /// The grant scope the calling app must hold on the target Spark before the
    /// agent forwards this op (else `grant_required`, which the app resolves
    /// with `grants.request`). `None` = no per-app grant needed.
    pub fn required_scope(&self) -> Option<Scope> {
        match self {
            Request::JobsRequest(_) => Some(Scope::Compute),
            Request::InferModels(_) | Request::InferChat(_) | Request::InferCompletions(_) | Request::InferEmbeddings(_) | Request::EnginesEnsure(_) => Some(Scope::Infer),
            _ => None,
        }
    }

    /// Ops whose effect is gated by UI the agent renders itself (items 2, 9).
    pub fn needs_agent_ui(&self) -> bool {
        matches!(
            self,
            Request::GrantsRequest(_)
                | Request::PairingShare(_)
                | Request::PairingRedeem(_)
                | Request::RoleRequestSwitch(_)
                | Request::BootstrapSsh(_)
                | Request::AdminDevicesRevoke(_)
                | Request::AdminGrantsRevoke(_)
                | Request::AdminFixExposure(_)
                | Request::AdminRecoverOwnership(_)
                | Request::LinkRoleSet(_)
        )
    }

    /// Ops that never log their params (secrets in logs). A1: the owner-link
    /// ops carry SSH credentials.
    pub fn params_are_secret(&self) -> bool {
        matches!(
            self,
            Request::BootstrapSsh(_) | Request::PairingRedeem(_) | Request::AdminRecoverOwnership(_) | Request::LinkOwnerPairStart(_) | Request::LinkRemove(_) | Request::LinkRoleSet(_) | Request::JobsRequest(_)
        )
    }
}

/// `{}`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Empty {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRef {
    pub node_id: EndpointId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrantRequestParams {
    pub node_id: EndpointId,
    /// Must pass `scopes::validate_app_grant` for the caller's app id.
    pub scopes: ScopeSet,
    /// Optional text shown in the consent dialog.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InferParams {
    pub node_id: EndpointId,
    /// OpenAI-compatible request body, forwarded as-is to the node's `/v1` route.
    /// `body.stream == true` switches to streaming events.
    pub body: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnsureParams {
    pub node_id: EndpointId,
    pub model: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryParams {
    /// 100..=5000
    pub timeout_ms: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SshAuth {
    Password { password: RedactedString },
    /// Use the user's running ssh-agent / Pageant.
    Agent,
    KeyFile {
        path: String,
        #[serde(default)]
        passphrase: Option<RedactedString>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BootstrapSshParams {
    pub host: String,
    /// Explicit remote opt-in. Missing/false means no public relay contact.
    #[serde(default)]
    pub remote_access: bool,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
    /// Spark label; default = remote hostname.
    #[serde(default)]
    pub label: Option<String>,
    /// Third-party apps: IGNORED (security review, item 5): only the agent's
    /// own trust-on-first-use dialog can accept an unknown host key.
    ///
    /// Amendment A1 (owner direction 2026-10-11): from a FIRST-PARTY app
    /// (`phaseforge`, `tuningspork`) this is a PIN of the host key the app
    /// already trusts for its saved SSH connection. The agent then runs the
    /// owner-link setup: no consent or host-key dialog, any other key fails
    /// closed with `host_key_mismatch` before authentication, Spark Link is
    /// installed WITHOUT an owner (re-running is safe; an existing install is
    /// kept), nothing is paired or stored, and the authenticated SSH session is
    /// kept for that app for [`SETUP_SESSION_TTL_S`] so `link.owner_pair.start
    /// {host}` can mint the owner ticket without the credentials again. Events:
    /// [`BootstrapEvent`] stages and `setup.progress` ([`OwnerPairEvent::SetupProgress`]).
    #[serde(default)]
    pub accept_host_key_sha256: Option<String>,
    /// 5a step 4: remove the bootstrap key afterwards (only if another SSH path is confirmed).
    #[serde(default)]
    pub remove_bootstrap_key: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminRevokeParams {
    pub node_id: EndpointId,
    pub device_id: EndpointId,
    /// Also revoke the devices it paired or approved (S3).
    #[serde(default)]
    pub cascade: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminGrantsListParams {
    pub node_id: EndpointId,
    /// One device, or every device when absent.
    #[serde(default)]
    pub device_id: Option<EndpointId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminGrantRevokeParams {
    pub node_id: EndpointId,
    pub device_id: EndpointId,
    pub app_id: AppId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminTicketCancelParams {
    pub node_id: EndpointId,
    pub ticket_id: TicketId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminFixExposureParams {
    pub node_id: EndpointId,
    pub engine_id: String,
}

/// S3 "Recover ownership": SSH to the Spark (item 5 host-key rules) and run
/// `spark-link-node admin reset --owner <this device> --revoke-admins`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecoverOwnershipParams {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecoverOwnershipResult {
    pub node_id: EndpointId,
    pub label: String,
    /// Admin keys the reset denylisted.
    pub revoked: Vec<EndpointId>,
    pub tickets_cancelled: u64,
    pub host_key_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AdminDevices {
    pub node_id: EndpointId,
    pub devices: Vec<DeviceRecord>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AdminGrants {
    pub node_id: EndpointId,
    pub grants: Vec<Grant>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AdminTickets {
    pub node_id: EndpointId,
    pub tickets: Vec<TicketSummary>,
    /// Joiners waiting for an admin to tap their code.
    pub pairings: Vec<PendingPairing>,
}

/// `detail` of `host_verification_required` / `host_key_mismatch`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostVerificationDetail {
    pub host: String,
    pub port: u16,
    pub key_type: String,
    /// `SHA256:<base64>` as printed by ssh-keygen -l
    pub fingerprint_sha256: String,
    /// What to run on the Spark's console to compare.
    pub compare_command: String,
}

pub const HOST_KEY_COMPARE_COMMAND: &str = "ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapStage {
    Connecting,
    HostKeyVerified,
    Authenticated,
    Uploading,
    Verifying,
    Installing,
    Starting,
    Dialing,
    Paired,
    RemovingBootstrapKey,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapEvent {
    pub stage: BootstrapStage,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BootstrapResult {
    pub node_id: EndpointId,
    pub label: String,
    pub node_fingerprint_words: Words,
    pub host_key_sha256: String,
    pub unit: UnitKind,
    pub removed_bootstrap_key: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PairingShareParams {
    pub node_id: EndpointId,
    /// Ticketable scopes only (item 3).
    pub scopes: ScopeSet,
    /// 30..=300
    pub ttl_s: u32,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PairingShareEvent {
    /// Render this QR / offer "Copy link". `link`/`https_link` contain the secret.
    Ticket {
        ticket_id: TicketId,
        link: RedactedString,
        https_link: RedactedString,
        /// SVG markup of the QR for `link` (generated by the agent).
        qr_svg: String,
        expires_at: u64,
        node_fingerprint_words: Words,
        scopes: ScopeSet,
    },
    /// A joiner presented a valid mac; the agent is showing its approval card.
    JoinerPending { device_name: String, device_kind: DeviceKind },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PairingShareResult {
    pub outcome: PairingOutcome,
    pub device: Option<EndpointId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PairingRedeemParams {
    /// `delores-link://pair/v1?...` or `https://link.deloresai.com/p/v1#...`
    pub link: RedactedString,
    #[serde(default)]
    pub device_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PairingRedeemEvent {
    /// The agent is showing "New Spark, never paired" (item 9).
    Confirming {
        node_fingerprint_words: Words,
        label_from_code: Option<String>,
        /// The label matches an already-paired Spark with a different EndpointId.
        label_collision: bool,
    },
    Dialing,
    /// Show these 4 words; the admin taps the matching code (item 1).
    Pending { words: Words, expires_at: u64 },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PairingRedeemResult {
    pub node_id: EndpointId,
    pub label: String,
    pub max_scopes: ScopeSet,
    pub node_fingerprint_words: Words,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleSwitchParams {
    pub node_id: EndpointId,
    pub role: Role,
    pub mode: RoleSwitchMode,
}

/// First-party link control may use the user's saved, pinned SSH account to
/// invoke the existing local Spark CLI. This does not change device ownership.
/// The legacy `role.request_switch` remains owner-device-only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinkRoleSetParams {
    #[serde(flatten)]
    pub switch: RoleSwitchParams,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<LinkSsh>,
}

/// Result of `role.request_switch`.
pub type RoleSwitchResult = RoleState;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentUpgradeParams {
    /// Path to the new agent executable staged by the app.
    pub staged_path: String,
    pub manifest: SignedManifest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeResult {
    pub accepted: bool,
    pub version: String,
    /// The agent restarts from the new copy once drained; apps never kill it.
    pub restart_after_drain: bool,
}

// ---------------------------------------------------------------------------
// Amendment A1: owner link, laptop side (PROTOCOL 5.4, 6.3, 11)
// ---------------------------------------------------------------------------

/// The SSH connection the app already trusts (PhaseForge: its saved Spark
/// connection, `{host, port, username, auth | private_key_path | password,
/// host_fingerprint}`; those names are accepted as aliases). The owner ticket
/// is minted over it, and Spark Link is installed over it when needed.
///
/// Credentials: exactly one of `auth`, `identity_file` (a private key file,
/// no passphrase) or `password`. They are used for this request only and never
/// stored, logged or put on a command line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinkSsh {
    /// Default: `host` of `link.owner_pair.start`, or the SSH host stored when
    /// the Spark was linked (`link.remove`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    #[serde(alias = "username")]
    pub user: String,
    /// Password, ssh-agent/Pageant, or a key file with an optional passphrase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<SshAuth>,
    /// Shorthand for `auth: {kind: "key_file", path}`.
    #[serde(default, alias = "private_key_path", skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<String>,
    /// Shorthand for `auth: {kind: "password", password}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<RedactedString>,
    /// `SHA256:<base64>` (as `ssh-keygen -l` prints) of the host key the app
    /// already trusts for this connection. From a first-party app this PINS the
    /// key: no trust-on-first-use dialog, and any other key fails closed with
    /// `host_key_mismatch` before authentication.
    #[serde(default, alias = "host_fingerprint", skip_serializing_if = "Option::is_none")]
    pub host_key_sha256: Option<String>,
}

impl LinkSsh {
    /// The one credential this request carries (`auth`, `identity_file` or `password`).
    pub fn credential(&self) -> Result<SshAuth, &'static str> {
        match (&self.auth, &self.identity_file, &self.password) {
            (Some(a), None, None) => Ok(a.clone()),
            (None, Some(path), None) if !path.is_empty() => Ok(SshAuth::KeyFile { path: path.clone(), passphrase: None }),
            (None, None, Some(pw)) if !pw.is_empty() => Ok(SshAuth::Password { password: pw.clone() }),
            (None, None, None) => Err("the SSH connection has no credential (auth, identity_file or password)"),
            _ => Err("give exactly one SSH credential: auth, identity_file or password"),
        }
    }
}

fn default_ssh_port() -> u16 {
    22
}

/// A1: how long the agent keeps the authenticated SSH session of a first-party
/// owner-link setup (`bootstrap.ssh` with a pin, or `link.owner_pair.start`
/// with `ssh`) for later `link.owner_pair.start {host}` calls of the same app
/// and executable. Credentials themselves are never kept.
pub const SETUP_SESSION_TTL_S: u64 = 600;

/// Default and maximum TTL of an owner ticket (`owner-ticket --ttl`).
pub const OWNER_TICKET_TTL_S: u32 = 300;

/// `link.owner_pair.start`: install Spark Link over SSH if needed (without an
/// owner), mint an owner ticket over SSH, pair over the pair ALPN and return
/// the QR of the Spark's LAN approval page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OwnerPairStartParams {
    #[serde(default)]
    pub remote_access: bool,
    /// The Spark's address (the app's SSH host). The QR uses this address when
    /// the Spark's approval page listens on it.
    pub host: String,
    /// Required to mint the owner ticket (`ssh_required` without it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<LinkSsh>,
    /// Spark label for a fresh install; default: the Spark's hostname.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Name the phone page shows ("Link <device_name> to your Spark?");
    /// default: this computer's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    /// Seconds the QR stays valid, 5..=300 (default 300).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_s: Option<u32>,
}

/// Result of `link.owner_pair.start`. `qr_url` carries the approval token in
/// its fragment: show it as a QR, never log it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OwnerPairStartResult {
    pub pairing_id: PairingId,
    /// `http://<lan_ip>:<lan_port>/pair/<pairing_id>#<approve_token>`.
    pub qr_url: RedactedString,
    /// SVG markup of the QR for `qr_url` (rendered by the agent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qr_svg: Option<String>,
    /// Original 44-bit SAS retained for compatibility.
    pub sas_words: Words,
    /// Six-digit human-entry confirmation (SAS indices modulo 1,000,000),
    /// not a lossless SAS encoding or standalone credential; absent on older agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_code: Option<String>,
    pub device_name: String,
    pub expires_at: u64,
    pub node_id: EndpointId,
    pub label: String,
    /// The Spark has no owner yet, so approving makes this computer its owner.
    /// `false`: the Spark already has an owner; this computer links with every
    /// scope but is not the owner.
    pub becomes_owner: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupStep {
    Connecting,
    Copying,
    Verifying,
    Starting,
    Ready,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerPairState {
    Waiting,
    Approved,
    Denied,
    Expired,
    Failed,
}

/// `owner_pair.status`. `reason` when failed: `cancelled` (cancelled on this
/// computer), `page_closed` (the Spark's approval page closed: too many wrong
/// codes, or it could not start), `rejected`, `node_unreachable` (the
/// connection to the Spark was lost), `not_stored`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OwnerPairStatus {
    pub pairing_id: PairingId,
    pub state: OwnerPairState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Set when approved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<EndpointId>,
    /// Set when approved: this computer is the Spark's owner (`GET /link/v1/node`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_owner: Option<bool>,
}

/// Events of `link.owner_pair.start` (request events; the same kinds are also
/// published on `events.subscribe` for the calling app).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum OwnerPairEvent {
    #[serde(rename = "setup.progress")]
    SetupProgress {
        step: SetupStep,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    #[serde(rename = "owner_pair.status")]
    Status(OwnerPairStatus),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerPairCancelParams {
    pub pairing_id: PairingId,
}

/// How a linked Spark is reached: `lan` = a direct path to a private LAN
/// address, `direct` = a direct (hole-punched) path elsewhere, `relay` = through a relay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkPath {
    Direct,
    Lan,
    Relay,
}

/// One row of `link.status`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinkStatusRow {
    pub node_id: EndpointId,
    pub label: String,
    pub role: Option<Role>,
    /// `false` after `link.set_enabled {enabled: false}` (paused).
    pub enabled: bool,
    pub reachable: bool,
    /// `null` while not reachable.
    pub path: Option<LinkPath>,
    pub is_owner: bool,
    pub admin: bool,
    pub last_seen: Option<u64>,
    /// Optional features of the Spark (`GET /link/v1/node`). 0.21 nodes
    /// advertise none; a role switch to `compute` is useful only once a node
    /// advertises `jobs` (the compute plane).
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkSetEnabledParams {
    pub node_id: EndpointId,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkSetEnabledResult {
    pub node_id: EndpointId,
    pub enabled: bool,
    /// The Spark recorded the pause/resume now. `false`: it was unreachable;
    /// the agent applied it locally at once and resumes on the Spark on the next contact.
    pub applied_on_node: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinkRemoveParams {
    pub node_id: EndpointId,
    /// SSH fallback (`spark-link-node devices revoke <id> --forget`) when the
    /// Spark cannot be reached over Spark Link. Without it removal remains
    /// pending until the node is reachable again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<LinkSsh>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevokeVia {
    /// `DELETE /link/v1/devices/self` over Spark Link.
    Spark,
    /// `spark-link-node devices revoke <id> --forget` over SSH.
    Ssh,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkRemoveResult {
    /// The Spark removed this computer: it forgot this device (row, grants,
    /// pause flag; an owner also releases ownership) without denylisting its
    /// key, so the same computer can link again later by QR (PROTOCOL 11, A1).
    pub revoked_on_node: bool,
    /// The stored Spark record and this computer's consents for it are gone.
    pub local_deleted: bool,
    /// The Spark could not be reached (neither over Spark Link nor over SSH):
    /// the agent keeps the record (marked removed and unusable), removes this
    /// computer on the Spark at the next contact, then deletes the record.
    pub pending_revocation: bool,
    /// The device key was deleted because no other Spark record needs it; the
    /// agent now holds a fresh key. Re-linking always needs a new owner ticket
    /// and QR approval on the Spark's network.
    pub key_deleted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<RevokeVia>,
}

// ---------------------------------------------------------------------------
// Results and events
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub agent_version: String,
    pub agent_sha256: String,
    pub protocol: ProtocolRange,
    pub ipc_version: u16,
    pub platform: String,
    pub device_name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathKind {
    Direct,
    Relay,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LabelSource {
    /// From a pairing link (unverified display name).
    FromCode,
    /// From the node's mac2-covered PairAccept / NodeInfo.
    FromNode,
    User,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SparkSummary {
    pub node_id: EndpointId,
    pub label: String,
    pub label_source: LabelSource,
    pub node_fingerprint_words: Words,
    pub role: Option<Role>,
    pub online: bool,
    pub path: PathKind,
    pub relay_url: Option<String>,
    /// S2: stored relay host is not one the agent's RelayConfig expects.
    pub unexpected_relay: bool,
    pub last_seen: Option<u64>,
    /// Scopes the CALLING app currently holds on this Spark.
    pub caller_scopes: ScopeSet,
    /// This device is an admin of that Spark.
    pub admin: bool,
    /// Item 7: adopted engines exposed on the LAN (admin devices only; empty otherwise).
    pub exposure_warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SparksList {
    pub sparks: Vec<SparkSummary>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrantResult {
    pub node_id: EndpointId,
    pub app_id: AppId,
    pub scopes: ScopeSet,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InferStreamEvent {
    /// One parsed OpenAI SSE `data:` payload. `[DONE]` is not forwarded; the final `res` ends the stream.
    Chunk { chunk: serde_json::Value },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LanCandidateKind {
    SparkLinkNode,
    SshHost,
    Workstation,
}

/// A discovery HINT. Never identity (5c, item 5).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LanCandidate {
    pub kind: LanCandidateKind,
    pub name: String,
    pub host: Option<String>,
    pub addrs: Vec<String>,
    pub port: u16,
    /// Set when an mDNS tag matched a paired node.
    pub paired_node: Option<EndpointId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiscoveryResult {
    pub candidates: Vec<LanCandidate>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Subscription is registered; subsequent operations cannot outrun it.
    Subscribed,
    SparkOnline { node_id: EndpointId, path: PathKind },
    SparkOffline { node_id: EndpointId },
    RoleChanged { node_id: EndpointId, role: Role },
    /// The calling app's grant on that Spark changed.
    GrantChanged { node_id: EndpointId, scopes: ScopeSet },
    /// This device was revoked by that Spark.
    Revoked { node_id: EndpointId },
    Paired { node_id: EndpointId, label: String },
    AgentUpgrading { version: String },
    /// Amendment A1: progress of an owner pairing this app started.
    #[serde(rename = "owner_pair.status")]
    OwnerPairStatus(OwnerPairStatus),
    /// Amendment A1: setting Spark Link up on a Spark (`link.owner_pair.start`).
    #[serde(rename = "setup.progress")]
    SetupProgress {
        step: SetupStep,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_round_trip() {
        let k = Key32::from_bytes([1; 32]);
        let n = Nonce32::from_bytes([2; 32]);
        let p = handshake_proof(&k, &n, r"\\.\pipe\x");
        assert!(verify_handshake_proof(&k, &n, r"\\.\pipe\x", &p));
        assert!(!verify_handshake_proof(&k, &n, r"\\.\pipe\y", &p));
        assert!(!verify_handshake_proof(&Key32::from_bytes([3; 32]), &n, r"\\.\pipe\x", &p));
    }

    #[test]
    fn request_frame_wire_shape() {
        let f = ClientFrame::Req { id: 7, req: Request::SparksList(Empty {}) };
        let j = serde_json::to_string(&f).unwrap();
        assert_eq!(j, r#"{"t":"req","id":7,"op":"sparks.list","params":{}}"#);
        let back: ClientFrame = serde_json::from_str(&j).unwrap();
        assert_eq!(back, f);

        let node = EndpointId::from_bytes([9; 32]).to_z32();
        let chat = format!(r#"{{"t":"req","id":1,"op":"infer.chat","params":{{"node_id":"{node}","body":{{"model":"m","stream":true}}}}}}"#);
        let parsed: ClientFrame = serde_json::from_str(&chat).unwrap();
        match parsed {
            ClientFrame::Req { id: 1, req: Request::InferChat(p) } => assert_eq!(p.body["stream"], true),
            other => panic!("{other:?}"),
        }
        assert!(serde_json::from_str::<ClientFrame>(r#"{"t":"req","id":1,"op":"shell.exec","params":{}}"#).is_err());
        assert!(serde_json::from_str::<ClientFrame>(r#"{"t":"req","id":1,"op":"sparks.list"}"#).is_err(), "params is required");
    }

    #[test]
    fn server_frames() {
        let r = ServerFrame::err(3, ApiError::new(crate::errors::ErrorCode::GrantRequired, "x"));
        assert_eq!(serde_json::to_string(&r).unwrap(), r#"{"t":"res","id":3,"ok":false,"error":{"code":"grant_required","message":"x"}}"#);
        let ok = ServerFrame::ok(4, &serde_json::json!({"done": true}));
        assert_eq!(serde_json::to_string(&ok).unwrap(), r#"{"t":"res","id":4,"ok":true,"result":{"done":true}}"#);
        let ev = ServerFrame::event(5, &PairingRedeemEvent::Dialing);
        assert_eq!(serde_json::to_string(&ev).unwrap(), r#"{"t":"event","id":5,"event":{"kind":"dialing"}}"#);
    }

    #[test]
    fn a1_link_ssh_accepts_phaseforge_names() {
        // PhaseForge's saved connection: {host, port, username, private_key_path | password, host_fingerprint}.
        let v = serde_json::json!({"host":"spark.local","port":2222,"username":"ben","private_key_path":"C:/id","host_fingerprint":"SHA256:fp"});
        let s: LinkSsh = serde_json::from_value(v).unwrap();
        assert_eq!((s.host.as_deref(), s.port, s.user.as_str(), s.host_key_sha256.as_deref()), (Some("spark.local"), 2222, "ben", Some("SHA256:fp")));
        assert_eq!(s.credential(), Ok(SshAuth::KeyFile { path: "C:/id".into(), passphrase: None }));
        let s: LinkSsh = serde_json::from_value(serde_json::json!({"user":"ben","password":"pw"})).unwrap();
        assert!(matches!(s.credential(), Ok(SshAuth::Password { password }) if password.expose() == "pw"));
        assert!(!format!("{s:?}").contains("pw\""), "the password is redacted in Debug");
        let s: LinkSsh = serde_json::from_value(serde_json::json!({"user":"ben"})).unwrap();
        assert!(s.credential().is_err(), "no credential");
        let s: LinkSsh = serde_json::from_value(serde_json::json!({"user":"ben","password":"pw","identity_file":"C:/id"})).unwrap();
        assert!(s.credential().is_err(), "two credentials");
        let s: LinkSsh = serde_json::from_value(serde_json::json!({"user":"ben","auth":{"kind":"agent"}})).unwrap();
        assert_eq!(s.credential(), Ok(SshAuth::Agent));
        let wire = serde_json::to_value(&s).unwrap();
        assert_eq!(wire, serde_json::json!({"port":22,"user":"ben","auth":{"kind":"agent"}}));
    }

    #[test]
    fn a1_link_wire_shapes() {
        let node = EndpointId::from_bytes([9; 32]).to_z32();
        let start = r#"{"t":"req","id":2,"op":"link.owner_pair.start","params":{"host":"192.168.0.20","ssh":{"user":"ben","auth":{"kind":"key_file","path":"C:/k"},"host_key_sha256":"SHA256:abc"}}}"#;
        match serde_json::from_str::<ClientFrame>(start).unwrap() {
            ClientFrame::Req { req: Request::LinkOwnerPairStart(p), .. } => {
                let ssh = p.ssh.as_ref().unwrap();
                assert_eq!((ssh.port, ssh.host.as_deref(), ssh.host_key_sha256.as_deref()), (22, None, Some("SHA256:abc")));
                assert_eq!(ssh.credential(), Ok(SshAuth::KeyFile { path: "C:/k".into(), passphrase: None }));
                assert_eq!(p.ttl_s, None);
                let r = Request::LinkOwnerPairStart(p.clone());
                assert!(r.params_are_secret() && !r.is_admin());
                assert_eq!(r.op(), "link.owner_pair.start");
            }
            other => panic!("{other:?}"),
        }
        for (op, params) in [
            ("link.status", "{}".to_string()),
            ("link.set_enabled", format!(r#"{{"node_id":"{node}","enabled":false}}"#)),
            ("link.remove", format!(r#"{{"node_id":"{node}"}}"#)),
            ("link.role.set", format!(r#"{{"node_id":"{node}","role":"compute","mode":"drain"}}"#)),
            ("link.owner_pair.cancel", format!(r#"{{"pairing_id":"{}"}}"#, PairingId::from_bytes([1; 16]))),
        ] {
            let f: ClientFrame = serde_json::from_str(&format!(r#"{{"t":"req","id":1,"op":"{op}","params":{params}}}"#)).unwrap_or_else(|e| panic!("{op}: {e}"));
            let ClientFrame::Req { req, .. } = f else { panic!("not a request") };
            assert_eq!(req.op(), op);
        }
        assert!(Request::LinkRoleSet(LinkRoleSetParams { switch: RoleSwitchParams { node_id: EndpointId::from_bytes([1; 32]), role: Role::Llm, mode: RoleSwitchMode::Stop }, ssh: None }).needs_agent_ui());
        assert!(Request::LinkRemove(LinkRemoveParams { node_id: EndpointId::from_bytes([1; 32]), ssh: None }).params_are_secret());
        let st = AgentEvent::OwnerPairStatus(OwnerPairStatus {
            pairing_id: PairingId::from_bytes([2; 16]),
            state: OwnerPairState::Approved,
            reason: None,
            node_id: None,
            is_owner: Some(true),
        });
        let j = serde_json::to_value(&st).unwrap();
        assert_eq!(j["kind"], "owner_pair.status");
        assert_eq!(j["state"], "approved");
        assert_eq!(j["is_owner"], true);
        assert!(j.get("reason").is_none());
        assert_eq!(serde_json::from_value::<AgentEvent>(j).unwrap(), st);
        let sp = serde_json::to_value(OwnerPairEvent::SetupProgress { step: SetupStep::Copying, reason: None }).unwrap();
        assert_eq!(sp, serde_json::json!({"kind":"setup.progress","step":"copying"}));
        let ev = serde_json::to_value(AgentEvent::SetupProgress { step: SetupStep::Failed, reason: Some("ssh_auth_failed".into()) }).unwrap();
        assert_eq!(ev, serde_json::json!({"kind":"setup.progress","step":"failed","reason":"ssh_auth_failed"}));
        let row = LinkStatusRow {
            node_id: EndpointId::from_bytes([3; 32]),
            label: "Spark".into(),
            role: Some(Role::Llm),
            enabled: true,
            reachable: false,
            path: None,
            is_owner: true,
            admin: true,
            last_seen: None,
            capabilities: vec![],
        };
        let v = serde_json::to_value(vec![row]).unwrap();
        assert!(v[0]["path"].is_null() && v[0]["is_owner"] == true && v[0]["role"] == "llm");
        let rr = LinkRemoveResult { revoked_on_node: true, local_deleted: true, pending_revocation: false, key_deleted: true, via: Some(RevokeVia::Ssh) };
        assert_eq!(serde_json::to_value(rr).unwrap()["via"], "ssh");
        let res = OwnerPairStartResult {
            pairing_id: PairingId::from_bytes([4; 16]),
            qr_url: RedactedString::new("http://192.168.0.20:7427/pair/x#tok"),
            qr_svg: None,
            sas_words: crate::words::words_from_bytes(&[0; 6]),
            verification_code: Some("000000".into()),
            device_name: "Laptop".into(),
            expires_at: 9,
            node_id: EndpointId::from_bytes([5; 32]),
            label: "Spark".into(),
            becomes_owner: true,
        };
        assert!(!format!("{res:?}").contains("#tok"), "qr_url is redacted in Debug");
        assert_eq!(serde_json::to_value(&res).unwrap()["qr_url"], "http://192.168.0.20:7427/pair/x#tok");
        let mut wire = serde_json::to_value(&res).unwrap();
        assert_eq!(wire["verification_code"], "000000");
        wire.as_object_mut().unwrap().remove("verification_code");
        assert_eq!(serde_json::from_value::<OwnerPairStartResult>(wire).unwrap().verification_code, None);
    }

    #[test]
    fn secret_params_flagged() {
        let p = Request::BootstrapSsh(BootstrapSshParams {
            remote_access: false,
            host: "spark.local".into(),
            port: 22,
            user: "u".into(),
            auth: SshAuth::Password { password: RedactedString::new("pw") },
            label: None,
            accept_host_key_sha256: None,
            remove_bootstrap_key: false,
        });
        assert!(p.params_are_secret());
        assert!(!format!("{p:?}").contains("\"pw\""));
        assert_eq!(Request::InferChat(InferParams { node_id: EndpointId::from_bytes([0; 32]), body: serde_json::json!({}) }).required_scope(), Some(Scope::Infer));
    }
}
