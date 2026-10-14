//! Per-app permission scopes (design section 6, 15.2 items 2 and 3, 16).
//!
//! | text | meaning | ticketable | admin-equivalent |
//! |---|---|---|---|
//! | `infer` | `/v1` inference, `engines.ensure` of allowed models | yes | no |
//! | `metrics` | `/link/v1/metrics` | yes | no |
//! | `models.read` | `/link/v1/models` | yes | no |
//! | `app:<id>` | that app's own plane (M3) | yes | no |
//! | `models.write` | pull/delete models (M3) | no | yes (until L1) |
//! | `engines.manage` | load/unload/configure engines | no | yes (until L1) |
//! | `compute` | `/link/v1/jobs` compute plane (M3b; remote code execution) | no | yes |
//! | `app:*` | every app plane (owner only) | no | yes |
//! | `admin` | devices, tickets, grants of others, audit, role, update | no | yes |
//!
//! Admin-equivalent scopes are granted only by `POST /link/v1/devices/{id}/promote`
//! from the owner device after OS user verification (item 3).

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ProtoError;
use crate::ids::AppId;

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Scope {
    Infer,
    Metrics,
    ModelsRead,
    ModelsWrite,
    EnginesManage,
    Compute,
    Admin,
    /// `app:*`: wildcard over app planes. Owner only; never ticketable or grantable to an app.
    AnyApp,
    /// `app:<id>`
    App(AppId),
}

impl Scope {
    pub fn as_string(&self) -> String {
        match self {
            Scope::Infer => "infer".into(),
            Scope::Metrics => "metrics".into(),
            Scope::ModelsRead => "models.read".into(),
            Scope::ModelsWrite => "models.write".into(),
            Scope::EnginesManage => "engines.manage".into(),
            Scope::Compute => "compute".into(),
            Scope::Admin => "admin".into(),
            Scope::AnyApp => "app:*".into(),
            Scope::App(id) => format!("app:{id}"),
        }
    }

    /// Item 3: only {infer, metrics, models.read, app:<id>} may appear in a ticket.
    pub fn is_ticketable(&self) -> bool {
        matches!(self, Scope::Infer | Scope::Metrics | Scope::ModelsRead | Scope::App(_))
    }

    /// Item 3 / section 16: scopes that amount to admin or code execution.
    pub fn is_admin_equivalent(&self) -> bool {
        matches!(self, Scope::Admin | Scope::ModelsWrite | Scope::EnginesManage | Scope::Compute | Scope::AnyApp)
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_string())
    }
}

impl FromStr for Scope {
    type Err = ProtoError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "infer" => Scope::Infer,
            "metrics" => Scope::Metrics,
            "models.read" => Scope::ModelsRead,
            "models.write" => Scope::ModelsWrite,
            "engines.manage" => Scope::EnginesManage,
            "compute" => Scope::Compute,
            "admin" => Scope::Admin,
            "app:*" => Scope::AnyApp,
            other => match other.strip_prefix("app:") {
                Some(id) => Scope::App(AppId::new(id)?),
                None => return Err(ProtoError::Invalid { field: "scope", reason: "unknown scope" }),
            },
        })
    }
}

impl Serialize for Scope {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.as_string())
    }
}
impl<'de> Deserialize<'de> for Scope {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Scope::from_str(&s).map_err(serde::de::Error::custom)
    }
}

/// A sorted set of scopes. Serialized as a JSON array of strings in sorted
/// (Ord) order, which makes it canonical for hashing.
#[derive(Clone, PartialEq, Eq, Default, Debug, Serialize, Deserialize, Hash)]
#[serde(transparent)]
pub struct ScopeSet(pub BTreeSet<Scope>);

impl ScopeSet {
    pub fn new() -> Self {
        Self(BTreeSet::new())
    }
    /// Everything the owner device holds (design 6: "The owner device has all scopes").
    pub fn owner() -> Self {
        Self::from_iter([
            Scope::Infer,
            Scope::Metrics,
            Scope::ModelsRead,
            Scope::ModelsWrite,
            Scope::EnginesManage,
            Scope::Compute,
            Scope::Admin,
            Scope::AnyApp,
        ])
    }
    pub fn contains(&self, s: &Scope) -> bool {
        self.0.contains(s)
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = &Scope> {
        self.0.iter()
    }
    pub fn insert(&mut self, s: Scope) -> bool {
        self.0.insert(s)
    }

    /// Whether `scope` is permitted by this set treated as a maximum. `app:*`
    /// covers every `app:<id>`.
    pub fn permits(&self, scope: &Scope) -> bool {
        self.0.contains(scope) || (matches!(scope, Scope::App(_)) && self.0.contains(&Scope::AnyApp))
    }

    /// `self ⊆ max` with `app:*` semantics. The node enforces
    /// `grant(device, app) ⊆ max_scopes(device)` with this.
    pub fn is_subset_of(&self, max: &ScopeSet) -> bool {
        self.0.iter().all(|s| max.permits(s))
    }

    /// Scopes in `self` not permitted by `max`.
    pub fn missing_from(&self, max: &ScopeSet) -> ScopeSet {
        Self(self.0.iter().filter(|s| !max.permits(s)).cloned().collect())
    }

    pub fn intersection(&self, other: &ScopeSet) -> ScopeSet {
        Self(self.0.iter().filter(|s| other.permits(s)).cloned().collect())
    }

    pub fn has_admin(&self) -> bool {
        self.0.contains(&Scope::Admin)
    }

    /// Item 3: a ticket's `max_scopes` must be non-empty and ticketable.
    pub fn validate_ticketable(&self) -> Result<(), ScopeError> {
        if self.0.is_empty() {
            return Err(ScopeError::Empty);
        }
        match self.0.iter().find(|s| !s.is_ticketable()) {
            Some(s) => Err(ScopeError::NotTicketable(s.clone())),
            None => Ok(()),
        }
    }
}

impl FromIterator<Scope> for ScopeSet {
    fn from_iter<I: IntoIterator<Item = Scope>>(it: I) -> Self {
        Self(it.into_iter().collect())
    }
}

impl fmt::Display for ScopeSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v: Vec<String> = self.0.iter().map(|s| s.as_string()).collect();
        f.write_str(&v.join(","))
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ScopeError {
    #[error("scope set is empty")]
    Empty,
    #[error("scope {0} cannot be carried by a pairing ticket")]
    NotTicketable(Scope),
    #[error("scope {0} cannot be granted to app {1}")]
    NotGrantableToApp(Scope, AppId),
}

/// The most an app may ever be granted (item 2/12), independent of the device's
/// max_scopes. Enforced by the node on grant PUT and by the agent before it
/// shows a consent dialog.
///
/// - Third-party apps: `{infer}`.
/// - First-party apps: `{infer, metrics, models.read, compute, app:<self>}`.
///   Compute is explicitly admitted for the owner-approved ML workload; all
///   other administration still runs inside the agent.
pub fn app_scope_cap(app: &AppId) -> ScopeSet {
    if app.is_first_party() {
        ScopeSet::from_iter([Scope::Infer, Scope::Metrics, Scope::ModelsRead, Scope::Compute, Scope::App(app.clone())])
    } else {
        ScopeSet::from_iter([Scope::Infer])
    }
}

/// Check a requested app grant against the app cap.
pub fn validate_app_grant(app: &AppId, requested: &ScopeSet) -> Result<(), ScopeError> {
    let cap = app_scope_cap(app);
    match requested.0.iter().find(|s| !cap.contains(s)) {
        Some(s) => Err(ScopeError::NotGrantableToApp(s.clone(), app.clone())),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(v: &[&str]) -> ScopeSet {
        ScopeSet::from_iter(v.iter().map(|s| s.parse().unwrap()))
    }

    #[test]
    fn round_trip_strings() {
        for s in ["infer", "metrics", "models.read", "models.write", "engines.manage", "compute", "admin", "app:*", "app:phaseforge"] {
            assert_eq!(s.parse::<Scope>().unwrap().as_string(), s);
        }
        assert!("app:".parse::<Scope>().is_err());
        assert!("App:x1".parse::<Scope>().is_err());
        assert!("root".parse::<Scope>().is_err());
    }

    #[test]
    fn ticketability_matches_item_3() {
        assert!(set(&["infer", "metrics", "models.read", "app:phaseforge"]).validate_ticketable().is_ok());
        for bad in ["admin", "models.write", "engines.manage", "compute", "app:*"] {
            assert!(set(&["infer", bad]).validate_ticketable().is_err(), "{bad} must not be ticketable");
        }
        assert_eq!(ScopeSet::new().validate_ticketable(), Err(ScopeError::Empty));
    }

    #[test]
    fn subset_with_wildcard() {
        let owner = ScopeSet::owner();
        assert!(set(&["app:tuningspork", "infer"]).is_subset_of(&owner));
        assert!(!set(&["app:tuningspork"]).is_subset_of(&set(&["infer", "app:phaseforge"])));
        assert_eq!(set(&["infer", "admin"]).missing_from(&set(&["infer"])), set(&["admin"]));
    }

    #[test]
    fn app_caps() {
        let pf = AppId::new("phaseforge").unwrap();
        let other = AppId::new("someapp").unwrap();
        assert!(validate_app_grant(&pf, &set(&["infer", "metrics", "app:phaseforge"])).is_ok());
        assert!(validate_app_grant(&pf, &set(&["app:tuningspork"])).is_err());
        assert!(validate_app_grant(&pf, &set(&["admin"])).is_err());
        assert!(validate_app_grant(&other, &set(&["metrics"])).is_err());
        assert!(validate_app_grant(&other, &set(&["infer"])).is_ok());
    }

    #[test]
    fn serializes_sorted() {
        let s = set(&["models.read", "infer", "app:phaseforge"]);
        let j = serde_json::to_string(&s).unwrap();
        let back: ScopeSet = serde_json::from_str(&j).unwrap();
        assert_eq!(back, s);
        assert_eq!(j, serde_json::to_string(&back).unwrap());
    }
}
