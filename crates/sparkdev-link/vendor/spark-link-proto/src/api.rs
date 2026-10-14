//! Node HTTP API over ALPN `delores/spark-link/http/1` (design 7, 15.2 items 1-3, 7, 16).
//!
//! Transport rules (normative; item 2, S1, S3):
//! - HTTP/1.1, exactly ONE request per iroh bidi stream; the node closes the
//!   stream after the response. A pipelined second request is `request_rejected`.
//! - Before accepting any stream the node checks the connection's `remote_id()`
//!   against the device list and the denylist. Authorization is re-checked on
//!   every request (S3), so revocation takes effect immediately.
//! - Every request carries exactly one `X-Delores-App` header, set by the agent
//!   (it strips any client-supplied `X-Delores-*`). Zero or several -> 400
//!   `request_rejected`. Agent-internal calls use app id [`AGENT_APP_ID`].
//! - CL+TE together, duplicate Content-Length, obs-fold, absolute-form or
//!   CONNECT targets, `..`, `//`, `\`, `%2f`, `%5c`, `%00` in the path -> 400
//!   `request_rejected`.
//! - Bodies are JSON (`Content-Type: application/json`) unless noted; errors use
//!   [`crate::errors::ApiError`].
//! - Request bodies are delimited by `Content-Length` only (any `Transfer-Encoding`
//!   is rejected); the agent sends `Host: spark-link` ([`HOST`]). Fixed responses
//!   carry `Content-Length`; streaming responses (SSE events, streamed `/v1`) are
//!   close-delimited: the body ends when the node finishes the iroh send stream.

use serde::{Deserialize, Serialize};

use crate::audit::{AuditCheckpoint, AuditEntry};
use crate::errors::ErrorCode;
use crate::ids::{AppId, DeviceKind, EndpointId, PairingId, TicketId};
use crate::roles::{Role, RoleClass, RoleSwitchMode};
use crate::scopes::{Scope, ScopeSet};
use crate::secret::PairingSecret;
use crate::version::ProtocolRange;
use crate::words::Words;

/// Header naming the calling app on every node request.
pub const HDR_APP: &str = "x-delores-app";
/// Fixed `Host` header value on node requests.
pub const HOST: &str = "spark-link";
/// Prefix of headers that only the agent may set.
pub const HDR_PREFIX_RESERVED: &str = "x-delores-";
/// App id the agent uses for its own (non-app) calls: admin UI, keepalive, events.
pub const AGENT_APP_ID: &str = "spark-link-agent";

/// Body caps (item 7).
pub const MAX_CONTROL_BODY: usize = 64 * 1024;
pub const MAX_INFER_BODY: usize = 32 * 1024 * 1024;
/// Concurrent `/v1` streams per (device, app) (item 7).
pub const MAX_STREAMS_PER_DEVICE_APP: usize = 4;

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// Every route the node serves. Anything else is 404. `Route::parse` is the
/// single routing table shared by the node (dispatch) and the agent (path
/// construction via [`Route::method`] / [`Route::path`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// GET /link/v1/node -> [`NodeInfo`]. Any paired device.
    NodeInfo,
    /// GET /link/v1/metrics -> [`MetricsSnapshot`]. `metrics`.
    Metrics,
    /// GET /link/v1/events -> `text/event-stream` of [`NodeEvent`]. Any paired device;
    /// admin-only events are filtered for non-admins.
    Events,
    /// GET /link/v1/devices -> `Vec<DeviceRecord>`. `admin`.
    DevicesList,
    /// DELETE /link/v1/devices/{id}?cascade=0|1 -> [`RevokeResponse`]. `admin` for non-admin
    /// targets; owner (or CLI) for admin targets; a device may delete itself.
    DeviceRevoke(EndpointId),
    /// POST /link/v1/devices/{id}/promote [`PromoteRequest`] -> [`DeviceRecord`]. Owner only.
    DevicePromote(EndpointId),
    /// POST /link/v1/devices/{id}/demote [`DemoteRequest`] -> [`DeviceRecord`]. Owner only.
    DeviceDemote(EndpointId),
    /// GET|PUT|DELETE /link/v1/grants/{device}/{app}. `admin`, or the device itself (PUT within
    /// its max_scopes and the app cap). PUT body [`GrantPutRequest`] -> [`Grant`].
    GrantGet(EndpointId, AppId),
    GrantPut(EndpointId, AppId),
    GrantDelete(EndpointId, AppId),
    /// GET /link/v1/grants/{device} -> `Vec<Grant>`. `admin` or self.
    GrantsForDevice(EndpointId),
    /// POST /link/v1/pairing-tickets [`TicketCreateRequest`] -> [`TicketCreated`]. `admin`.
    TicketCreate,
    /// DELETE /link/v1/pairing-tickets/{id}. `admin`.
    TicketCancel(TicketId),
    /// GET /link/v1/pairing-tickets -> `Vec<TicketSummary>` (live tickets; no secrets). `admin`.
    TicketsList,
    /// GET /link/v1/pairings -> `Vec<PendingPairing>`. `admin`.
    PairingsList,
    /// POST /link/v1/pairings/{id}/approve [`ApproveRequest`] -> [`DeviceRecord`]. `admin` + UV.
    PairingApprove(PairingId),
    /// POST /link/v1/pairings/{id}/deny. `admin`.
    PairingDeny(PairingId),
    /// GET /link/v1/audit?after=<seq>&limit=<n> -> [`AuditPage`]. `admin`.
    Audit,
    /// GET /link/v1/role -> [`RoleState`]. Any paired device.
    RoleGet,
    /// POST /link/v1/role [`RoleSetRequest`] -> [`RoleState`]. Owner + UV.
    RoleSet,
    /// GET /link/v1/models -> `Vec<ModelInfo>`. `models.read`.
    ModelsList,
    /// POST /link/v1/models/allow [`ModelAllowRequest`] -> [`ModelInfo`]. Owner.
    ModelsAllow,
    /// GET /link/v1/engines -> `Vec<EngineSummary>`. `models.read` or `engines.manage`.
    EnginesList,
    /// POST /link/v1/engines/ensure [`EnsureRequest`] -> [`EnsureResponse`]. `infer`;
    /// llm role only. M1: allowed models on the adopted Ollama only.
    EnginesEnsure,
    /// POST /link/v1/engines/{id}/exposure-fix [`ExposureFixRequest`] -> [`ExposureFixResponse`].
    /// Owner + UV (item 7). Audits the owner's approval, re-probes the engine and
    /// audits `engine_exposure_fixed` with the before and after listen addresses.
    EngineExposureFix(String),
    /// /link/v1/jobs[/...] (any method). `compute`; compute role only. M1: after the role
    /// check passes, 501 `not_implemented` (compute plane is M3b).
    Jobs(String),
    /// GET /v1/models. `infer`; llm role.
    V1Models,
    /// POST /v1/chat/completions. `infer`; llm role.
    V1ChatCompletions,
    /// POST /v1/completions. `infer`; llm role.
    V1Completions,
    /// POST /v1/embeddings. `infer`; llm role.
    V1Embeddings,
    /// /apps/{app_id}/... `app:<id>`. M1: 501 `not_implemented` (app planes are M3).
    AppPlane(AppId, String),
}

/// Who may call a route (checked after the connection's device is known and
/// not revoked). Grants are per (device, app): for `Scope(..)` the node checks
/// `grant(device, X-Delores-App)` permits it AND `max_scopes(device)` permits it.
/// For agent-internal calls (`X-Delores-App: spark-link-agent`) the device's
/// max_scopes alone apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Requirement {
    AnyPaired,
    Scope(Scope),
    /// Either of two scopes.
    AnyOf(Scope, Scope),
    Admin,
    /// `admin`, or the target device itself.
    AdminOrSelf(EndpointId),
    /// The owner device only (item 3); also requires a `uv` assertion in the body where noted.
    Owner,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteError {
    NotFound,
    MethodNotAllowed,
    /// Path contains a forbidden sequence (item 2 / L4).
    Rejected,
}

impl RouteError {
    pub fn code(self) -> ErrorCode {
        match self {
            RouteError::NotFound => ErrorCode::NotFound,
            RouteError::MethodNotAllowed => ErrorCode::MethodNotAllowed,
            RouteError::Rejected => ErrorCode::RequestRejected,
        }
    }
}

/// Engine ids are short lowercase identifiers (`ollama`).
pub fn valid_engine_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && s != "ensure"
}

/// Reject non-canonical paths before routing.
pub fn path_is_canonical(path: &str) -> bool {
    if !path.starts_with('/') || path.len() > 1024 {
        return false;
    }
    let lower = path.to_ascii_lowercase();
    !(path.contains("//")
        || path.contains('\\')
        || path.contains('\0')
        || path.contains('?')
        || path.contains('#')
        || lower.contains("%2f")
        || lower.contains("%5c")
        || lower.contains("%00")
        || lower.contains("%2e")
        || path.split('/').any(|seg| seg == ".." || seg == "."))
}

impl Route {
    /// Route a request. `path` excludes the query string.
    pub fn parse(method: &str, path: &str) -> Result<Route, RouteError> {
        if !path_is_canonical(path) {
            return Err(RouteError::Rejected);
        }
        let segs: Vec<&str> = path[1..].split('/').collect();
        let m = method;
        let id = |s: &str| s.parse::<EndpointId>().map_err(|_| RouteError::NotFound);
        let app = |s: &str| AppId::new(s).map_err(|_| RouteError::NotFound);
        let only = |want: &str, r: Route| if m == want { Ok(r) } else { Err(RouteError::MethodNotAllowed) };
        match segs.as_slice() {
            ["v1", "models"] => only("GET", Route::V1Models),
            ["v1", "chat", "completions"] => only("POST", Route::V1ChatCompletions),
            ["v1", "completions"] => only("POST", Route::V1Completions),
            ["v1", "embeddings"] => only("POST", Route::V1Embeddings),
            ["apps", a, rest @ ..] => Ok(Route::AppPlane(app(a)?, rest.join("/"))),
            ["link", "v1", rest @ ..] => match rest {
                ["node"] => only("GET", Route::NodeInfo),
                ["metrics"] => only("GET", Route::Metrics),
                ["events"] => only("GET", Route::Events),
                ["devices"] => only("GET", Route::DevicesList),
                ["devices", d] => only("DELETE", Route::DeviceRevoke(id(d)?)),
                ["devices", d, "promote"] => only("POST", Route::DevicePromote(id(d)?)),
                ["devices", d, "demote"] => only("POST", Route::DeviceDemote(id(d)?)),
                ["grants", d] => only("GET", Route::GrantsForDevice(id(d)?)),
                ["grants", d, a] => {
                    let (d, a) = (id(d)?, app(a)?);
                    match m {
                        "GET" => Ok(Route::GrantGet(d, a)),
                        "PUT" => Ok(Route::GrantPut(d, a)),
                        "DELETE" => Ok(Route::GrantDelete(d, a)),
                        _ => Err(RouteError::MethodNotAllowed),
                    }
                }
                ["pairing-tickets"] => match m {
                    "POST" => Ok(Route::TicketCreate),
                    "GET" => Ok(Route::TicketsList),
                    _ => Err(RouteError::MethodNotAllowed),
                },
                ["pairing-tickets", t] => only("DELETE", Route::TicketCancel(t.parse().map_err(|_| RouteError::NotFound)?)),
                ["pairings"] => only("GET", Route::PairingsList),
                ["pairings", p, "approve"] => only("POST", Route::PairingApprove(p.parse().map_err(|_| RouteError::NotFound)?)),
                ["pairings", p, "deny"] => only("POST", Route::PairingDeny(p.parse().map_err(|_| RouteError::NotFound)?)),
                ["audit"] => only("GET", Route::Audit),
                ["role"] => match m {
                    "GET" => Ok(Route::RoleGet),
                    "POST" => Ok(Route::RoleSet),
                    _ => Err(RouteError::MethodNotAllowed),
                },
                ["models"] => only("GET", Route::ModelsList),
                ["models", "allow"] => only("POST", Route::ModelsAllow),
                ["engines"] => only("GET", Route::EnginesList),
                ["engines", "ensure"] => only("POST", Route::EnginesEnsure),
                ["engines", e, "exposure-fix"] if valid_engine_id(e) => only("POST", Route::EngineExposureFix((*e).to_string())),
                ["jobs", rest @ ..] => Ok(Route::Jobs(rest.join("/"))),
                _ => Err(RouteError::NotFound),
            },
            _ => Err(RouteError::NotFound),
        }
    }

    pub fn method(&self) -> &'static str {
        use Route::*;
        match self {
            NodeInfo | Metrics | Events | DevicesList | GrantGet(..) | GrantsForDevice(_) | PairingsList | Audit | RoleGet | ModelsList
            | EnginesList | V1Models | TicketsList => "GET",
            DeviceRevoke(_) | GrantDelete(..) | TicketCancel(_) => "DELETE",
            GrantPut(..) => "PUT",
            DevicePromote(_) | DeviceDemote(_) | TicketCreate | PairingApprove(_) | PairingDeny(_) | RoleSet | ModelsAllow | EnginesEnsure
            | EngineExposureFix(_) | V1ChatCompletions | V1Completions | V1Embeddings => "POST",
            Jobs(_) | AppPlane(..) => "GET",
        }
    }

    pub fn path(&self) -> String {
        use Route::*;
        match self {
            NodeInfo => "/link/v1/node".into(),
            Metrics => "/link/v1/metrics".into(),
            Events => "/link/v1/events".into(),
            DevicesList => "/link/v1/devices".into(),
            DeviceRevoke(d) => format!("/link/v1/devices/{d}"),
            DevicePromote(d) => format!("/link/v1/devices/{d}/promote"),
            DeviceDemote(d) => format!("/link/v1/devices/{d}/demote"),
            GrantGet(d, a) | GrantPut(d, a) | GrantDelete(d, a) => format!("/link/v1/grants/{d}/{a}"),
            GrantsForDevice(d) => format!("/link/v1/grants/{d}"),
            TicketCreate | TicketsList => "/link/v1/pairing-tickets".into(),
            TicketCancel(t) => format!("/link/v1/pairing-tickets/{t}"),
            PairingsList => "/link/v1/pairings".into(),
            PairingApprove(p) => format!("/link/v1/pairings/{p}/approve"),
            PairingDeny(p) => format!("/link/v1/pairings/{p}/deny"),
            Audit => "/link/v1/audit".into(),
            RoleGet | RoleSet => "/link/v1/role".into(),
            ModelsList => "/link/v1/models".into(),
            ModelsAllow => "/link/v1/models/allow".into(),
            EnginesList => "/link/v1/engines".into(),
            EnginesEnsure => "/link/v1/engines/ensure".into(),
            EngineExposureFix(e) => format!("/link/v1/engines/{e}/exposure-fix"),
            Jobs(rest) if rest.is_empty() => "/link/v1/jobs".into(),
            Jobs(rest) => format!("/link/v1/jobs/{rest}"),
            V1Models => "/v1/models".into(),
            V1ChatCompletions => "/v1/chat/completions".into(),
            V1Completions => "/v1/completions".into(),
            V1Embeddings => "/v1/embeddings".into(),
            AppPlane(a, rest) => format!("/apps/{a}/{rest}"),
        }
    }

    pub fn requirement(&self) -> Requirement {
        use Route::*;
        match self {
            NodeInfo | Events | RoleGet => Requirement::AnyPaired,
            Metrics => Requirement::Scope(Scope::Metrics),
            DevicesList | TicketCreate | TicketCancel(_) | TicketsList | PairingsList | PairingApprove(_) | PairingDeny(_) | Audit => Requirement::Admin,
            DeviceRevoke(d) | GrantGet(d, _) | GrantPut(d, _) | GrantDelete(d, _) | GrantsForDevice(d) => Requirement::AdminOrSelf(*d),
            DevicePromote(_) | DeviceDemote(_) | RoleSet | ModelsAllow | EngineExposureFix(_) => Requirement::Owner,
            ModelsList => Requirement::Scope(Scope::ModelsRead),
            EnginesList => Requirement::AnyOf(Scope::ModelsRead, Scope::EnginesManage),
            EnginesEnsure | V1Models | V1ChatCompletions | V1Completions | V1Embeddings => Requirement::Scope(Scope::Infer),
            Jobs(_) => Requirement::Scope(Scope::Compute),
            AppPlane(a, _) => Requirement::Scope(Scope::App(a.clone())),
        }
    }

    /// Section 16 role gate, applied after authorization.
    pub fn role_class(&self) -> RoleClass {
        use Route::*;
        match self {
            V1Models | V1ChatCompletions | V1Completions | V1Embeddings | EnginesEnsure => RoleClass::Inference,
            Jobs(_) => RoleClass::ComputeJobs,
            _ => RoleClass::Neutral,
        }
    }

    /// Item 2: routes the agent forwards on behalf of an app (data plane). All
    /// other routes are agent-internal and reachable only through agent UI.
    pub fn app_forwardable(&self) -> bool {
        use Route::*;
        matches!(self, NodeInfo | RoleGet | Metrics | ModelsList | EnginesList | EnginesEnsure | V1Models | V1ChatCompletions | V1Completions | V1Embeddings | AppPlane(..) | Jobs(_))
    }

    /// Item 7: the exact `/v1` (method, path) allowlist proxied to an adopted engine.
    /// Never `/api/*`.
    pub fn upstream_v1_path(&self) -> Option<&'static str> {
        match self {
            Route::V1Models => Some("/v1/models"),
            Route::V1ChatCompletions => Some("/v1/chat/completions"),
            Route::V1Completions => Some("/v1/completions"),
            Route::V1Embeddings => Some("/v1/embeddings"),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Payloads
// ---------------------------------------------------------------------------

/// OS user verification asserted by the agent for admin actions (item 2).
/// The node cannot verify it; it records it in the audit log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserVerification {
    pub method: UvMethod,
    pub verified_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UvMethod {
    WindowsHello,
    MacosLocalAuth,
    Polkit,
    /// Confirmed on a second admin device.
    SecondAdmin,
    /// Local `spark-link-node` CLI on the Spark (shell access is the proof).
    SparkCli,
    /// Test builds only; the node rejects it unless started with `allow_test_uv`.
    Test,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeInfo {
    pub v: u16,
    pub node_id: EndpointId,
    pub label: String,
    pub node_version: String,
    pub protocol: ProtocolRange,
    pub role: Role,
    /// Present while a role switch drains/stops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_switch: Option<RoleSwitchProgress>,
    pub relay_url: Option<String>,
    pub direct_addrs: Vec<String>,
    pub hardware: HardwareSummary,
    pub engines: Vec<EngineSummary>,
    pub owner: EndpointId,
    pub time: u64,
    /// Amendment A1 (additive): optional features of this node, e.g. `jobs`
    /// once the compute plane exists. 0.21 nodes advertise none (the key is
    /// omitted); a role switch to `compute` is useful only with `jobs`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// 0.25 (additive): the closed compute runtimes this node knows, ready or not, with the image
    /// digest pinned for its architecture and the probe result. Older nodes omit it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runtimes: Vec<RuntimeInfo>,
}

/// One registry runtime as a node reports it (`NodeInfo.runtimes`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeInfo {
    /// Capability token (`python_ml`, `blender@4.5.9`).
    pub token: String,
    /// Its probe passed and the token is advertised.
    pub ready: bool,
    /// CPU architecture of this node (`aarch64`, `x86_64`).
    pub arch: String,
    /// The pinned image for this architecture (`name@sha256:...` or `sha256:<image id>`); absent
    /// when the registry has no image for this architecture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Why it is not ready (missing image, failed probe), in plain words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// SHA-256 of the probe's output, when it ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_sha256: Option<String>,
    /// What the probe found (devices, OptiX, denoiser device), bounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HardwareSummary {
    pub arch: String,
    pub os: String,
    /// The first GPU's name (0.21 field; `devices` lists every one).
    pub gpu: Option<String>,
    /// The first GPU shares system memory (0.21 field; see `devices`).
    pub unified_memory: bool,
    pub memory_total_mb: Option<u64>,
    pub memory_available_mb: Option<u64>,
    /// 0.25 (additive): every compute device with its own memory. A host-agnostic list: one GPU
    /// or several, discrete or sharing system memory. Empty when none was found.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<ComputeDevice>,
    /// GPU driver version, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver_version: Option<String>,
    /// 0.25 (additive): CPU cores this host reports (its available parallelism). A compute job's
    /// CPU share and thread counts are derived from it, never from a constant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_cores: Option<u32>,
}

/// One compute device of a host (`HardwareSummary.devices`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputeDevice {
    pub index: u32,
    /// `gpu`.
    pub kind: String,
    /// `nvidia`.
    pub vendor: String,
    pub name: String,
    /// The device's own memory, or for a device that shares system memory, the host's.
    pub memory_total_mb: Option<u64>,
    pub memory_available_mb: Option<u64>,
    /// Shares system memory with the CPU (no separate device memory to report).
    pub unified: bool,
    /// CUDA compute capability (`12.1`), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compute_capability: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineKind {
    Ollama,
    LlamaServer,
    Vllm,
}

/// An inference engine the node knows. M1: only adopted (pre-existing,
/// user-managed) Ollama, read-only, never reconfigured silently (item 7).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineSummary {
    pub id: String,
    pub kind: EngineKind,
    /// false = adopted (user-managed).
    pub managed: bool,
    pub version: Option<String>,
    pub reachable: bool,
    pub exposure: Exposure,
    /// Section 16: in `compute` role an adopted engine is a conflict.
    pub role_conflict: bool,
}

/// Item 7 adoption record. `exposed` drives a persistent admin warning.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Exposure {
    pub exposed: bool,
    pub reasons: Vec<ExposureReason>,
    pub listen_addr: Option<String>,
    pub origins: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExposureReason {
    ListensNonLoopback,
    OriginsWildcard,
    VersionBelowMinimum,
    /// Configuration could not be read; treated as exposed.
    Unknown,
}

/// Minimum adopted Ollama version (CVE-2024-37032 fixed in 0.1.34). Re-check at implementation.
pub const OLLAMA_MIN_VERSION: &str = "0.1.34";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MetricsSnapshot {
    pub time: u64,
    pub memory_total_mb: Option<u64>,
    pub memory_available_mb: Option<u64>,
    pub gpu_util_pct: Option<u8>,
    pub gpu_temp_c: Option<i16>,
    pub gpu_power_w: Option<u32>,
    pub disk_free_mb: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub id: EndpointId,
    pub name: String,
    pub kind: DeviceKind,
    pub max_scopes: ScopeSet,
    pub owner: bool,
    pub paired_at: u64,
    /// The admin device that created the ticket (S3 cascade). None for the owner.
    pub paired_by: Option<EndpointId>,
    /// The admin device that approved the pairing (item 1). None for the owner.
    pub approved_by: Option<EndpointId>,
    /// Audit seq of the approval/installation record (item 1: no row without it).
    pub audit_seq: u64,
    pub last_seen: Option<u64>,
    pub online: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RevokeResponse {
    pub revoked: Vec<EndpointId>,
    /// Live connections/streams closed (S3: within 1 s).
    pub closed_connections: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PromoteRequest {
    /// Scopes to add (may include admin-equivalent ones).
    pub add: ScopeSet,
    pub uv: UserVerification,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DemoteRequest {
    pub remove: ScopeSet,
    pub uv: UserVerification,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Grant {
    pub device: EndpointId,
    pub app_id: AppId,
    pub scopes: ScopeSet,
    pub updated_at: u64,
    pub updated_by: EndpointId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrantPutRequest {
    pub scopes: ScopeSet,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TicketCreateRequest {
    /// Must pass `ScopeSet::validate_ticketable` (item 3), else 400 `scope_not_ticketable`.
    pub max_scopes: ScopeSet,
    /// 1..=300 (design 4).
    pub ttl_s: u32,
    /// Label the joiner will see as "name from the code".
    pub label: Option<String>,
    pub uv: UserVerification,
}

/// Returned once to the creating admin agent; the node keeps only HKDF keys.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TicketCreated {
    pub ticket_id: TicketId,
    pub secret: PairingSecret,
    pub expires_at: u64,
    pub max_scopes: ScopeSet,
    pub node_fingerprint_words: Words,
    /// Dial hints for the link's r= / a=.
    pub relay_url: Option<String>,
    pub direct_addrs: Vec<String>,
}

/// `GET /link/v1/pairing-tickets`: a live ticket, without its secret.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TicketSummary {
    pub ticket_id: TicketId,
    pub created_by: EndpointId,
    pub max_scopes: ScopeSet,
    pub label: Option<String>,
    pub expires_at: u64,
    /// A joiner presented a valid mac and awaits approval.
    pub pending: bool,
}

/// Item 7: the owner approved closing an exposed adopted engine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExposureFixRequest {
    pub uv: UserVerification,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExposureFixResponse {
    pub engine_id: String,
    pub before: Exposure,
    pub after: Exposure,
    /// `after.exposed == false`.
    pub fixed: bool,
}

/// Where the item 7 drop-in for an adopted Ollama goes. systemd applies
/// drop-ins in file-name order and the last `Environment=` for a variable
/// wins, so the name sorts after `override.conf` (what `systemctl edit` and
/// NVIDIA's DGX Spark playbook create).
pub const OLLAMA_DROPIN_PATH: &str = "/etc/systemd/system/ollama.service.d/zz-spark-link-loopback.conf";
/// The `[Service]` section of the drop-in: loopback only, no extra browser origins.
pub const OLLAMA_DROPIN_SERVICE: &str = "[Service]\nEnvironment=\"OLLAMA_HOST=127.0.0.1:11434\"\nEnvironment=\"OLLAMA_ORIGINS=\"\n";
/// The drop-in file as printed by `spark-link-node engines ollama-dropin`.
pub const OLLAMA_DROPIN: &str = "# Proposed by Spark Link (item 7). Review, then install as root and run:\n#   sudo systemctl daemon-reload && sudo systemctl restart ollama\n[Service]\nEnvironment=\"OLLAMA_HOST=127.0.0.1:11434\"\nEnvironment=\"OLLAMA_ORIGINS=\"\n";

/// The exact commands the owner runs on the Spark to apply the drop-in
/// (shown by the agent; the node never applies it and never holds sudo).
pub fn ollama_exposure_fix_commands() -> String {
    format!(
        "sudo mkdir -p /etc/systemd/system/ollama.service.d\nsudo tee {OLLAMA_DROPIN_PATH} >/dev/null <<'EOF'\n{OLLAMA_DROPIN_SERVICE}EOF\nsudo systemctl daemon-reload && sudo systemctl restart ollama\nss -ltn | grep 11434"
    )
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingPairing {
    pub pairing_id: PairingId,
    pub ticket_id: TicketId,
    pub device_name: String,
    pub device_kind: DeviceKind,
    pub joiner_id: EndpointId,
    pub max_scopes: ScopeSet,
    /// Exactly 3 candidate codes; one equals the joiner's sas words (item 1).
    pub candidates: Vec<Words>,
    /// The admin device that created the ticket.
    pub created_by: EndpointId,
    pub expires_at: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApproveRequest {
    /// The code the user tapped. A wrong choice burns the ticket (item 1 check c).
    pub choice: Words,
    pub uv: UserVerification,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuditPage {
    pub entries: Vec<AuditEntry>,
    pub latest_checkpoint: Option<AuditCheckpoint>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoleState {
    pub role: Role,
    pub switch: Option<RoleSwitchProgress>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleSwitchProgress {
    pub target: Role,
    pub mode: RoleSwitchMode,
    pub started_at: u64,
    /// Running streams/jobs of the old role still draining.
    pub remaining: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoleSetRequest {
    pub role: Role,
    pub mode: RoleSwitchMode,
    pub uv: UserVerification,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// The routable model id used in `/v1` bodies.
    pub id: String,
    pub engine_id: String,
    pub size_bytes: Option<u64>,
    /// Item 7: only allowed models are routable.
    pub allowed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelAllowRequest {
    pub model: String,
    pub allowed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnsureRequest {
    /// M1: a model id from `GET /link/v1/models` with `allowed: true`.
    pub model: String,
    #[serde(default)]
    pub engine: Option<EngineKind>,
    #[serde(default)]
    pub ctx: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnsureResponse {
    pub model: String,
    pub engine_id: String,
}

/// Server-sent events on `GET /link/v1/events`: `event: <kind>\ndata: <json>\n\n`.
/// Admin-only kinds are marked; others go to every paired device (filtered to
/// the receiving device where noted).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeEvent {
    /// admin: render an approval card with OS user verification.
    PairingPending(PendingPairing),
    /// admin
    PairingResolved { pairing_id: PairingId, outcome: PairingOutcome, device: Option<EndpointId> },
    /// admin: "This pairing code was copied. No device was added."
    TicketPoisoned { ticket_id: TicketId, created_by: EndpointId },
    /// admin: a valid mac arrived after the ticket was consumed; offer one-tap revoke of `used_by`.
    LateMacAlert { ticket_id: TicketId, used_by: EndpointId, attempted_by: EndpointId },
    /// admin
    DevicePaired { device: DeviceRecord },
    /// admin (item 3: every promotion pushed to all admins)
    DevicePromoted { device: EndpointId, scopes: ScopeSet, by: EndpointId },
    /// admin, plus the demoted device (N3). `scopes` = its max_scopes afterwards.
    DeviceDemoted { device: EndpointId, scopes: ScopeSet, by: Option<EndpointId> },
    /// admin, plus the revoked device itself
    DeviceRevoked { devices: Vec<EndpointId>, by: Option<EndpointId> },
    /// admin, plus the device concerned
    GrantChanged { device: EndpointId, app_id: AppId, scopes: ScopeSet },
    /// everyone
    RoleSwitching { progress: RoleSwitchProgress },
    /// everyone
    RoleChanged { role: Role, by: Option<EndpointId> },
    /// admin: persistent warning (item 7)
    EngineExposed { engine_id: String, exposure: Exposure },
    /// admin: the adopted engine is no longer exposed (item 7).
    EngineExposureCleared { engine_id: String, exposure: Exposure },
    /// admin (S4)
    AuditCheckpoint { checkpoint: AuditCheckpoint },
    /// keepalive comment-equivalent; sent every 15 s
    Ping { time: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingOutcome {
    Approved,
    Denied,
    WrongCode,
    Expired,
    Poisoned,
    Cancelled,
}

impl NodeEvent {
    /// SSE `event:` name (same as the serde tag).
    pub fn kind(&self) -> String {
        serde_json::to_value(self).ok().and_then(|v| v.get("kind").and_then(|k| k.as_str().map(str::to_owned))).unwrap_or_default()
    }
    pub fn admin_only(&self) -> bool {
        !matches!(self, NodeEvent::RoleSwitching { .. } | NodeEvent::RoleChanged { .. } | NodeEvent::Ping { .. } | NodeEvent::DeviceRevoked { .. } | NodeEvent::GrantChanged { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_table() {
        let d = EndpointId::from_bytes([1; 32]);
        let cases = [
            Route::NodeInfo,
            Route::Metrics,
            Route::Events,
            Route::DevicesList,
            Route::DeviceRevoke(d),
            Route::DevicePromote(d),
            Route::GrantPut(d, AppId::new("phaseforge").unwrap()),
            Route::GrantsForDevice(d),
            Route::TicketCreate,
            Route::TicketCancel(TicketId::from_bytes([2; 16])),
            Route::PairingApprove(PairingId::from_bytes([3; 16])),
            Route::Audit,
            Route::RoleSet,
            Route::ModelsAllow,
            Route::EnginesEnsure,
            Route::Jobs("abc/logs".into()),
            Route::V1ChatCompletions,
            Route::V1Embeddings,
            Route::AppPlane(AppId::new("tuningspork").unwrap(), "files/x".into()),
        ];
        for r in cases {
            let method = if matches!(r, Route::Jobs(_) | Route::AppPlane(..)) { "GET" } else { r.method() };
            assert_eq!(Route::parse(method, &r.path()), Ok(r.clone()), "{r:?}");
        }
    }

    #[test]
    fn rejects_noncanonical_and_unknown() {
        for p in ["/v1/../link/v1/node", "/v1//models", "/v1/models%2f", "/apps/x\\y", "/v1/models?x", "/link/v1/./node", "/apps/a/%2e%2e/x"] {
            assert_eq!(Route::parse("GET", p), Err(RouteError::Rejected), "{p}");
        }
        assert_eq!(Route::parse("GET", "/api/pull"), Err(RouteError::NotFound));
        assert_eq!(Route::parse("POST", "/api/tags"), Err(RouteError::NotFound));
        assert_eq!(Route::parse("GET", "/v1/chat/completions"), Err(RouteError::MethodNotAllowed));
        assert_eq!(Route::parse("POST", "/link/v1/devices/notanid/promote"), Err(RouteError::NotFound));
    }

    #[test]
    fn role_and_scope_requirements() {
        assert_eq!(Route::V1ChatCompletions.role_class(), RoleClass::Inference);
        assert_eq!(Route::EnginesEnsure.role_class(), RoleClass::Inference);
        assert_eq!(Route::Jobs(String::new()).role_class(), RoleClass::ComputeJobs);
        assert_eq!(Route::Jobs(String::new()).requirement(), Requirement::Scope(Scope::Compute));
        assert_eq!(Route::RoleSet.requirement(), Requirement::Owner);
        assert_eq!(Route::DevicePromote(EndpointId::from_bytes([0; 32])).requirement(), Requirement::Owner);
        assert!(!Route::TicketCreate.app_forwardable());
        assert!(!Route::GrantPut(EndpointId::from_bytes([0; 32]), AppId::new("phaseforge").unwrap()).app_forwardable());
        assert!(Route::V1Models.app_forwardable());
    }

    #[test]
    fn event_tags() {
        let e = NodeEvent::RoleChanged { role: Role::Compute, by: None };
        assert_eq!(e.kind(), "role_changed");
        assert!(!e.admin_only());
        assert!(NodeEvent::TicketPoisoned { ticket_id: TicketId::from_bytes([0; 16]), created_by: EndpointId::from_bytes([0; 32]) }.admin_only());
    }
}
