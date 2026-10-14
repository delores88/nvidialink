//! Error codes shared by the node HTTP API, the agent IPC and the clients.
//!
//! Wire shape (node HTTP and agent IPC alike):
//! `{"error": {"code": "<snake_case>", "message": "<human text>", "detail": <optional JSON>}}`.
//! Clients branch on `code` only; `message` is display text and may change.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    // ---- generic HTTP-ish ----
    BadRequest,
    /// Malformed or ambiguous HTTP framing: CL+TE, duplicate Content-Length,
    /// obs-fold, pipelined second request, zero or several X-Delores-App (item 2).
    RequestRejected,
    Unauthorized,
    Forbidden,
    NotFound,
    MethodNotAllowed,
    Conflict,
    PayloadTooLarge,
    TooManyRequests,
    NotImplemented,
    /// Peer speaks a protocol version outside the supported range.
    VersionUnsupported,
    Internal,
    // ---- authorization ----
    /// First use of an app on a device: no grant for (device, app) covers the route.
    /// detail: {"app_id": "...", "missing": ["infer", ...]}
    GrantRequired,
    /// Requested grant/promotion exceeds the device's max_scopes or the app cap.
    ScopeExceedsMax,
    /// Ticket max_scopes contain a non-ticketable scope (item 3).
    ScopeNotTicketable,
    /// Only the owner device (or the Spark CLI) may do this (item 3).
    OwnerRequired,
    /// The agent refused an operation that needs its own consent UI (item 2).
    ConsentRequired,
    /// The user declined the agent's consent dialog.
    ConsentDenied,
    /// Admin action without OS user verification (item 2).
    UserVerificationRequired,
    UserVerificationFailed,
    /// This device has been revoked by the node.
    Revoked,
    /// Amendment A1: this device paused its own link (`POST /link/v1/devices/self/pause`);
    /// every request except resume and self-revoke is refused until it resumes. HTTP 423.
    DevicePaused,
    // ---- role (section 16) ----
    /// detail: {"role": "llm" | "compute"} (the Spark's current role)
    RoleMismatch,
    RoleSwitchInProgress,
    // ---- inference proxy (item 7) ----
    /// Model not listed at adoption nor added by the owner. detail: {"model": "..."}
    ModelNotAllowed,
    UpstreamUnavailable,
    UpstreamTimeout,
    UpstreamError,
    // ---- pairing (items 1, 6, 9, 11) ----
    /// Generic failure for unknown, expired, consumed or bad-mac tickets (item 11).
    PairingFailed,
    /// Admin denied, wrong code tapped, TTL passed without approval, or ticket poisoned.
    /// detail: {"reason": "denied" | "wrong_code" | "expired" | "poisoned" | "cancelled"}
    PairingRejected,
    // ---- agent / bootstrap (item 5) ----
    /// SSH host key not pinned and not in known_hosts. Stop before USERAUTH.
    /// detail: {"host", "port", "key_type", "fingerprint_sha256", "compare_command"}
    HostVerificationRequired,
    /// Pinned or known_hosts key differs from the presented key.
    HostKeyMismatch,
    SshAuthFailed,
    SshFailed,
    /// Amendment A1: `link.owner_pair.start` / `link.remove` need the app's SSH
    /// connection to the Spark (owner tickets are minted over SSH only).
    SshRequired,
    /// Amendment A1: the Spark's approval page is not on this computer's
    /// network (no LAN address of the Spark is the SSH host or on a local
    /// subnet), so a phone on this Wi-Fi could not open the QR.
    NotSameNetwork,
    /// `spark-link-node install` on a node that already has an owner.
    AlreadyOwned,
    /// Release manifest/signature/version check failed (S6/L5 groundwork).
    ReleaseVerificationFailed,
    DowngradeRefused,
    /// No paired Spark with that node_id.
    NotPaired,
    /// Paired Spark could not be reached (direct or relay).
    NodeUnreachable,
    /// The Spark is paired but the calling app has not selected it (item 9).
    NotSelected,
    // ---- client-side only (never sent by agent/node) ----
    AgentUnavailable,
    IpcHandshakeFailed,
    Cancelled,
    Timeout,
    UnsupportedPlatform,
}

impl ErrorCode {
    /// HTTP status used by the node. The agent IPC carries `code` only.
    pub fn http_status(self) -> u16 {
        use ErrorCode::*;
        match self {
            BadRequest | RequestRejected | ScopeNotTicketable | SshRequired => 400,
            Unauthorized | SshAuthFailed => 401,
            Forbidden | GrantRequired | ScopeExceedsMax | OwnerRequired | ConsentRequired | ConsentDenied
            | UserVerificationRequired | UserVerificationFailed | Revoked | PairingFailed | PairingRejected => 403,
            NotFound | NotPaired => 404,
            MethodNotAllowed => 405,
            Conflict | RoleMismatch | RoleSwitchInProgress | ModelNotAllowed | HostVerificationRequired
            | HostKeyMismatch | AlreadyOwned | DowngradeRefused | NotSelected | NotSameNetwork => 409,
            PayloadTooLarge => 413,
            DevicePaused => 423,
            VersionUnsupported => 426,
            TooManyRequests => 429,
            Cancelled => 499,
            Internal | ReleaseVerificationFailed => 500,
            NotImplemented | UnsupportedPlatform => 501,
            UpstreamUnavailable | UpstreamError | SshFailed => 502,
            NodeUnreachable | AgentUnavailable | IpcHandshakeFailed => 503,
            UpstreamTimeout | Timeout => 504,
        }
    }

    pub fn as_str(self) -> String {
        serde_json::to_value(self).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
    }
}

/// `{"code", "message", "detail"?}`
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

/// `{"error": ErrorBody}`: the body of every non-2xx node response and of
/// every failed IPC result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{}: {}", .error.code.as_str(), .error.message)]
pub struct ApiError {
    pub error: ErrorBody,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { error: ErrorBody { code, message: message.into(), detail: None } }
    }
    pub fn with_detail(mut self, detail: impl Serialize) -> Self {
        self.error.detail = serde_json::to_value(detail).ok();
        self
    }
    pub fn code(&self) -> ErrorCode {
        self.error.code
    }
    pub fn status(&self) -> u16 {
        self.error.code.http_status()
    }
    /// `409 role_mismatch {"role": ...}`
    pub fn role_mismatch(m: crate::roles::RoleMismatch) -> Self {
        Self::new(ErrorCode::RoleMismatch, format!("Spark is in {} role", m.role.as_str())).with_detail(m)
    }
    /// `403 grant_required {"app_id", "missing"}`
    pub fn grant_required(app: &crate::ids::AppId, missing: &crate::scopes::ScopeSet) -> Self {
        Self::new(ErrorCode::GrantRequired, "this app needs permission to use the Spark")
            .with_detail(serde_json::json!({"app_id": app, "missing": missing}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shape() {
        let e = ApiError::role_mismatch(crate::roles::RoleMismatch { role: crate::roles::Role::Compute });
        let j = serde_json::to_string(&e).unwrap();
        assert_eq!(j, r#"{"error":{"code":"role_mismatch","message":"Spark is in compute role","detail":{"role":"compute"}}}"#);
        assert_eq!(e.status(), 409);
        assert_eq!(ErrorCode::ScopeNotTicketable.as_str(), "scope_not_ticketable");
        assert_eq!(ErrorCode::GrantRequired.http_status(), 403);
        assert_eq!(ErrorCode::DevicePaused.as_str(), "device_paused");
        assert_eq!(ErrorCode::DevicePaused.http_status(), 423);
        assert_eq!(ErrorCode::SshRequired.as_str(), "ssh_required");
        assert_eq!(ErrorCode::NotSameNetwork.as_str(), "not_same_network");
        assert_eq!(ErrorCode::NotSameNetwork.http_status(), 409);
    }
}
