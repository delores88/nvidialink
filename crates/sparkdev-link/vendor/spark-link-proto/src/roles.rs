//! Spark roles (design section 16): a Spark is EITHER the LLM host OR the
//! ML/compute host, never both. When it is the LLM host, the connected device
//! does rendering/ML locally; when it is the compute host, the device (or a
//! cloud model) supplies the LLM.
//!
//! The node persists `role` (default `llm`), exposes it in `GET /link/v1/node`,
//! pushes changes as `role_changed` events, and enforces it per route with
//! [`check_role`]. Mismatches return `409 role_mismatch {"role": "<current>"}`,
//! which apps treat as routing information, not a user-visible error.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Spark runs local models (inference only). Compute jobs are refused.
    #[default]
    Llm,
    /// Spark runs ML/compute jobs only. `/v1` and `engines.ensure` are refused;
    /// no inference engine may be resident.
    Compute,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Llm => "llm",
            Role::Compute => "compute",
        }
    }
}

/// `POST /link/v1/role {role, mode}`: how in-flight work of the old role ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RoleSwitchMode {
    /// Stop admitting new work for the old role and wait for running work.
    #[default]
    Drain,
    /// Cancel running work with an audited reason.
    Stop,
}

/// Which role a route belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoleClass {
    /// `/v1/*`, `POST /link/v1/engines/ensure`: only in `llm` role.
    Inference,
    /// `/link/v1/jobs*`: only in `compute` role.
    ComputeJobs,
    /// Everything else (identity, admin, metrics, models listing, events).
    Neutral,
}

/// Body `detail` of `409 role_mismatch`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleMismatch {
    /// The Spark's current role.
    pub role: Role,
}

/// Enforce the exclusive role rule for a route class.
pub fn check_role(current: Role, class: RoleClass) -> Result<(), RoleMismatch> {
    match (current, class) {
        (_, RoleClass::Neutral) => Ok(()),
        (Role::Llm, RoleClass::Inference) => Ok(()),
        (Role::Compute, RoleClass::ComputeJobs) => Ok(()),
        (role, _) => Err(RoleMismatch { role }),
    }
}

/// App-side job placement (section 16 "Apps"): `placement: auto | this_pc | spark`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    #[default]
    Auto,
    ThisPc,
    Spark,
}

/// What kind of work an app wants to place.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkKind {
    /// LLM inference.
    Inference,
    /// ML / simulation / rendering compute.
    Compute,
}

/// Where the work runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementTarget {
    /// This device (its own GPU/CPU, or for inference its local or cloud model).
    ThisDevice,
    Spark,
}

/// Resolve a placement request against the selected Spark's role.
/// `spark_role` is `None` when no Spark is paired, selected and online.
///
/// - `auto`: inference goes to the Spark only in `llm` role; compute goes to
///   the Spark only in `compute` role; otherwise this device.
/// - `spark`: allowed only when the role matches, else `Err(RoleMismatch)`.
/// - `this_pc`: always this device.
pub fn resolve_placement(requested: Placement, kind: WorkKind, spark_role: Option<Role>) -> Result<PlacementTarget, PlacementError> {
    let wanted = match kind {
        WorkKind::Inference => Role::Llm,
        WorkKind::Compute => Role::Compute,
    };
    match (requested, spark_role) {
        (Placement::ThisPc, _) => Ok(PlacementTarget::ThisDevice),
        (Placement::Auto, Some(r)) if r == wanted => Ok(PlacementTarget::Spark),
        (Placement::Auto, _) => Ok(PlacementTarget::ThisDevice),
        (Placement::Spark, None) => Err(PlacementError::NoSpark),
        (Placement::Spark, Some(r)) if r == wanted => Ok(PlacementTarget::Spark),
        (Placement::Spark, Some(r)) => Err(PlacementError::RoleMismatch(RoleMismatch { role: r })),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlacementError {
    #[error("no Spark is paired, selected and online")]
    NoSpark,
    #[error("the Spark is in {} role", .0.role.as_str())]
    RoleMismatch(RoleMismatch),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_roles() {
        assert!(check_role(Role::Llm, RoleClass::Inference).is_ok());
        assert_eq!(check_role(Role::Llm, RoleClass::ComputeJobs), Err(RoleMismatch { role: Role::Llm }));
        assert_eq!(check_role(Role::Compute, RoleClass::Inference), Err(RoleMismatch { role: Role::Compute }));
        assert!(check_role(Role::Compute, RoleClass::ComputeJobs).is_ok());
        assert!(check_role(Role::Compute, RoleClass::Neutral).is_ok());
    }

    #[test]
    fn placement_follows_role() {
        use PlacementTarget::{Spark as OnSpark, ThisDevice};
        use WorkKind::{Compute as C, Inference as I};
        let auto = Placement::Auto;
        assert_eq!(resolve_placement(auto, C, Some(Role::Llm)), Ok(ThisDevice));
        assert_eq!(resolve_placement(auto, I, Some(Role::Llm)), Ok(OnSpark));
        assert_eq!(resolve_placement(auto, C, Some(Role::Compute)), Ok(OnSpark));
        assert_eq!(resolve_placement(auto, I, Some(Role::Compute)), Ok(ThisDevice));
        assert_eq!(resolve_placement(auto, I, None), Ok(ThisDevice));
        assert_eq!(resolve_placement(Placement::Spark, C, Some(Role::Llm)), Err(PlacementError::RoleMismatch(RoleMismatch { role: Role::Llm })));
        assert_eq!(resolve_placement(Placement::Spark, I, None), Err(PlacementError::NoSpark));
        assert_eq!(resolve_placement(Placement::ThisPc, I, Some(Role::Llm)), Ok(ThisDevice));
    }

    #[test]
    fn serde_names() {
        assert_eq!(serde_json::to_string(&Role::Compute).unwrap(), "\"compute\"");
        assert_eq!(serde_json::to_string(&RoleMismatch { role: Role::Llm }).unwrap(), "{\"role\":\"llm\"}");
        assert_eq!(serde_json::to_string(&Placement::ThisPc).unwrap(), "\"this_pc\"");
    }
}
