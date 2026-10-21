//! Spark-reported addresses are hints, never SSH trust. Only the explicit
//! status/doctor/devices/connect hooks learn; workload RPCs never call here.
use crate::{
    Result,
    state::{self, Config},
    transport::Transport,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    net::{Ipv4Addr, UdpSocket},
    path::Path,
};

const FILE: &str = "lan-learned.json";
const MAX_BYTES: u64 = 16 * 1024;
const REFRESH: u64 = 24 * 60 * 60;
const SKEW: u64 = 300;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Route {
    pub host: String,
    pub prefix: u8,
    pub kind: String,
    pub speed_mbps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Suppressed {
    pub host: String,
    pub source: String,
    pub until: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Learned {
    pub binding: String,
    pub epoch: String,
    pub learned_at: u64,
    pub retry_after: u64,
    pub shared_network: bool,
    pub routes: Vec<Route>,
    pub suppressed: Vec<Suppressed>,
}
#[derive(Serialize, Deserialize)]
struct Store {
    v: u32,
    sets: Vec<Learned>,
}

fn hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
pub fn private_host(host: &str) -> Option<Ipv4Addr> {
    host.parse::<Ipv4Addr>()
        .ok()
        .filter(|ip| ip.is_private() && ip.to_string() == host)
}
fn valid_route(route: &Route) -> bool {
    private_host(&route.host).is_some()
        && (8..=30).contains(&route.prefix)
        && matches!(route.kind.as_str(), "wired" | "wifi")
        && route
            .speed_mbps
            .is_none_or(|s| (1..=1_000_000).contains(&s))
}
/// Validate the bounded wire reply. Invalid individual routes are discarded;
/// an invalid version, epoch or over-limit envelope is refused as a whole.
pub fn validate(reply: &Value, binding: String, now: u64) -> Option<Learned> {
    if reply["v"] != 1 {
        return None;
    }
    let epoch = reply["epoch"].as_str().filter(|s| hex(s, 16))?;
    let entries = reply["routes"].as_array().filter(|a| a.len() <= 8)?;
    let shared_network = reply["shared_network"].as_bool()?;
    let mut routes = Vec::new();
    for entry in entries {
        let Some(host) = entry["ipv4"].as_str() else {
            continue;
        };
        let Some(prefix) = entry["prefix"].as_u64().and_then(|v| u8::try_from(v).ok()) else {
            continue;
        };
        let Some(kind) = entry["kind"].as_str() else {
            continue;
        };
        let speed_mbps = if entry["speed_mbps"].is_null() {
            None
        } else if let Some(speed) = entry["speed_mbps"].as_u64() {
            Some(speed)
        } else {
            continue;
        };
        let route = Route {
            host: host.into(),
            prefix,
            kind: kind.into(),
            speed_mbps,
            verified_at: None,
        };
        if valid_route(&route) && !routes.iter().any(|r: &Route| r.host == route.host) {
            routes.push(route);
        }
    }
    Some(Learned {
        binding,
        epoch: epoch.into(),
        learned_at: now,
        retry_after: now.saturating_add(60),
        shared_network,
        routes,
        suppressed: Vec::new(),
    })
}
fn read(path: &Path, now: u64) -> Vec<Learned> {
    let Ok(file) = fs::File::open(path) else {
        return Vec::new();
    };
    let mut bytes = Vec::new();
    if file.take(MAX_BYTES + 1).read_to_end(&mut bytes).is_err() || bytes.len() as u64 > MAX_BYTES {
        return Vec::new();
    }
    let Ok(mut store) = serde_json::from_slice::<Store>(&bytes) else {
        return Vec::new();
    };
    if store.v != 1 || store.sets.len() > 2 {
        return Vec::new();
    }
    store.sets.retain(|s| {
        hex(&s.binding, 64)
            && hex(&s.epoch, 16)
            && s.learned_at <= now.saturating_add(SKEW)
            && s.retry_after <= now.saturating_add(SKEW + 60)
            && s.routes.len() <= 8
            && s.suppressed.len() <= 16
    });
    for set in &mut store.sets {
        let mut hosts = std::collections::BTreeSet::new();
        set.routes.retain(|r| {
            valid_route(r)
                && r.verified_at.is_none_or(|t| t <= now.saturating_add(SKEW))
                && hosts.insert(r.host.clone())
        });
        set.suppressed.retain(|s| {
            private_host(&s.host).is_some()
                && private_host(&s.source).is_some()
                && s.until > now
                && s.until <= now.saturating_add(6 * 60 * 60 + SKEW)
        });
    }
    store.sets
}
pub fn learned(config: &Config) -> Option<Learned> {
    if !config.auto_addresses {
        return None;
    }
    let binding = state::lan_binding(config);
    read(&state::root().ok()?.join(FILE), state::now())
        .into_iter()
        .find(|s| s.binding == binding)
}
fn change(config: &Config, update: impl FnOnce(&mut Learned)) -> Result<bool> {
    let mut changed = false;
    state::with_lan_routes(|route_file| {
        let path = route_file.with_file_name(FILE);
        let now = state::now();
        let binding = state::lan_binding(config);
        let mut sets = read(&path, now);
        let mut set = sets
            .iter()
            .position(|s| s.binding == binding)
            .map(|i| sets.remove(i))
            .unwrap_or(Learned {
                binding,
                epoch: "0000000000000000".into(),
                learned_at: 0,
                retry_after: 0,
                shared_network: false,
                routes: Vec::new(),
                suppressed: Vec::new(),
            });
        update(&mut set);
        sets.insert(0, set);
        sets.truncate(2);
        state::atomic_json(&path, &Store { v: 1, sets })?;
        changed = true;
        Ok(())
    })?;
    Ok(changed)
}
fn same_network(source: Ipv4Addr, target: Ipv4Addr, prefix: u8) -> bool {
    if !(8..=30).contains(&prefix) {
        return false;
    }
    let mask = u32::MAX << (32 - prefix);
    u32::from(source) & mask == u32::from(target) & mask
}
/// Ask the local routing table which IPv4 source would be used. UDP connect
/// sets the peer on an unbound datagram socket; no send/write is performed.
pub fn source_for(route: &Route) -> Option<String> {
    let ip = private_host(&route.host)?;
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((ip, 9)).ok()?;
    let std::net::IpAddr::V4(source) = socket.local_addr().ok()?.ip() else {
        return None;
    };
    same_network(source, ip, route.prefix).then(|| source.to_string())
}
pub fn on_network(route: &Route) -> bool {
    source_for(route).is_some()
}
impl Learned {
    pub fn eligible(&self, route: &Route, source: &str, now: u64) -> bool {
        !self
            .suppressed
            .iter()
            .any(|s| s.host == route.host && s.source == source && s.until > now)
    }
}
pub fn verified(config: &Config, host: &str) {
    let _ = change(config, |set| {
        if let Some(route) = set.routes.iter_mut().find(|r| r.host == host) {
            route.verified_at = Some(state::now());
        }
    });
}
pub fn suppress_other_host(config: &Config, host: &str, source: &str) {
    if private_host(host).is_none() || private_host(source).is_none() {
        return;
    }
    let _ = change(config, |set| {
        set.suppressed
            .retain(|s| !(s.host == host && s.source == source));
        set.suppressed.insert(
            0,
            Suppressed {
                host: host.into(),
                source: source.into(),
                until: state::now().saturating_add(6 * 60 * 60),
            },
        );
        set.suppressed.truncate(16);
    });
}
/// The original status outcome is preserved even if this optional refresh
/// fails. Every learned address still needs the unchanged strict SSH pin.
pub fn learn_if_needed(transport: &mut Transport, response: &Value) {
    learn_with(transport, response, |tr| {
        tr.rpc(json!({"v":1,"op":"lan_routes"}))
    });
}
fn learn_with(
    transport: &mut Transport,
    response: &Value,
    request: impl FnOnce(&mut Transport) -> Result<Value>,
) {
    let config = transport.config.clone();
    let prior = learned(&config);
    let learned_at = prior.as_ref().map(|s| s.learned_at);
    let metadata = |state: &str| json!({"state":state,"learned_at":learned_at});
    if !config.auto_addresses || !config.allows_lan() || !config.connection_enabled {
        transport.lan_learn = metadata("disabled");
        return;
    }
    if !transport.authenticated {
        transport.lan_learn = metadata("unavailable");
        return;
    }
    if state::diagnostics()["redirected"] != false {
        transport.lan_learn = metadata("state_redirected");
        return;
    }
    let Some(epoch) = response["network"]["lan_epoch"]
        .as_str()
        .filter(|s| hex(s, 16))
    else {
        transport.lan_learn = metadata("unavailable");
        return;
    };
    let now = state::now();
    if prior
        .as_ref()
        .is_some_and(|s| s.epoch == epoch && now.saturating_sub(s.learned_at) <= REFRESH)
    {
        transport.lan_learn = metadata("unchanged");
        return;
    }
    // Check under the same lock as the write so concurrent status callers do
    // not both spend a refresh. The temporary cooldown is retained on error.
    let mut refresh = false;
    if change(&config, |set| {
        if set.retry_after <= now {
            set.retry_after = now.saturating_add(60);
            refresh = true;
        }
    })
    .ok()
        != Some(true)
    {
        transport.lan_learn = metadata("unavailable");
        return;
    }
    if !refresh {
        transport.lan_learn = metadata("cooldown");
        return;
    }
    let observation = (
        transport.selected.clone(),
        transport.reason.clone(),
        transport.host.clone(),
        transport.authenticated,
    );
    let reply = request(transport);
    (
        transport.selected,
        transport.reason,
        transport.host,
        transport.authenticated,
    ) = observation;
    let Some(mut fresh) = reply
        .ok()
        .and_then(|v| validate(&v, state::lan_binding(&config), now))
    else {
        transport.lan_learn = metadata("unavailable");
        return;
    };
    let saved = change(&config, |set| {
        for route in &mut fresh.routes {
            route.verified_at = set
                .routes
                .iter()
                .find(|old| old.host == route.host)
                .and_then(|old| old.verified_at);
        }
        // A host explicitly re-listed by the authenticated Spark is usable
        // again. Suppression of absent hosts can remain until its expiry.
        fresh.suppressed = set
            .suppressed
            .iter()
            .filter(|s| !fresh.routes.iter().any(|r| r.host == s.host))
            .cloned()
            .collect();
        *set = fresh;
    })
    .ok()
        == Some(true);
    transport.lan_learn = if saved {
        json!({"state":"updated","learned_at":now})
    } else {
        metadata("unavailable")
    };
    transport.refresh_learned_hints();
}

#[cfg(test)]
mod tests {
    use super::*;
    fn reply() -> Value {
        json!({"v":1,"epoch":"0123456789abcdef","shared_network":true,"routes":[{"ipv4":"192.168.0.7","prefix":24,"kind":"wired","speed_mbps":2500}]})
    }
    #[test]
    fn learning_requires_auth_and_obeys_cooldown_and_reply_epoch() {
        let home = state::TestHome::new();
        let config: Config =
            serde_json::from_value(json!({"ssh_alias":"test","host_key_alias":"test","hosts":[]}))
                .unwrap();
        let mut tr = Transport::from_config(config);
        let status = json!({"network":{"lan_epoch":"aaaaaaaaaaaaaaaa"}});
        learn_with(&mut tr, &status, |_| {
            panic!("unauthenticated must not learn")
        });
        assert!(!home.path.join(FILE).exists());
        tr.authenticated = true;
        tr.selected = "ssh".into();
        tr.host = Some("192.168.0.7".into());
        learn_with(&mut tr, &status, |t| {
            t.authenticated = false;
            t.selected = "failed".into();
            Ok(reply())
        });
        assert_eq!(tr.lan_learn["state"], "updated");
        assert!(tr.authenticated);
        assert_eq!(tr.selected, "ssh");
        assert_eq!(learned(&tr.config).unwrap().epoch, "0123456789abcdef");
        learn_with(&mut tr, &status, |_| {
            panic!("cooldown must prevent second request")
        });
        assert_eq!(tr.lan_learn["state"], "cooldown");
        let matching = json!({"network":{"lan_epoch":"0123456789abcdef"}});
        learn_with(&mut tr, &matching, |_| panic!("unchanged must not refresh"));
        assert_eq!(tr.lan_learn["state"], "unchanged");
        tr.config.auto_addresses = false;
        learn_with(&mut tr, &status, |_| panic!("owner disabled learning"));
        assert_eq!(tr.lan_learn["state"], "disabled");
    }
    #[test]
    fn validates_envelope_and_drops_unsafe_routes() {
        let good = reply();
        assert_eq!(validate(&good, "a".repeat(64), 10).unwrap().routes.len(), 1);
        for bad in [
            "192.168.000.7",
            "8.8.8.8",
            "169.254.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "192.168.0.7 -o X",
        ] {
            let mut v = good.clone();
            v["routes"][0]["ipv4"] = json!(bad);
            assert!(
                validate(&v, "a".repeat(64), 10).unwrap().routes.is_empty(),
                "{bad}"
            );
        }
        for (field, value) in [
            ("prefix", json!(31)),
            ("kind", json!("tunnel")),
            ("speed_mbps", json!(0)),
            ("speed_mbps", json!(1000001)),
            ("speed_mbps", json!("1000")),
        ] {
            let mut v = good.clone();
            v["routes"][0][field] = value;
            assert!(validate(&v, "a".repeat(64), 10).unwrap().routes.is_empty());
        }
        for epoch in ["ABCDEF0123456789", "0123", ""] {
            let mut v = good.clone();
            v["epoch"] = json!(epoch);
            assert!(validate(&v, "a".repeat(64), 10).is_none());
        }
        let mut v = good.clone();
        v["routes"] = json!(vec![good["routes"][0].clone(); 9]);
        assert!(validate(&v, "a".repeat(64), 10).is_none());
    }
    #[test]
    fn network_masks_and_suppression_are_source_scoped() {
        assert!(same_network(
            "192.168.0.2".parse().unwrap(),
            "192.168.0.7".parse().unwrap(),
            24
        ));
        assert!(!same_network(
            "192.168.1.2".parse().unwrap(),
            "192.168.0.7".parse().unwrap(),
            24
        ));
        let mut set = validate(&reply(), "a".repeat(64), 10).unwrap();
        set.suppressed.push(Suppressed {
            host: "192.168.0.7".into(),
            source: "192.168.0.2".into(),
            until: 100,
        });
        assert!(!set.eligible(&set.routes[0], "192.168.0.2", 11));
        assert!(set.eligible(&set.routes[0], "192.168.0.3", 11));
        assert!(set.eligible(&set.routes[0], "192.168.0.2", 101));
    }
    #[test]
    fn state_is_bounded_and_revalidated() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(FILE);
        fs::write(&path, vec![b' '; MAX_BYTES as usize + 1]).unwrap();
        assert!(read(&path, 10).is_empty());
        let mut set = validate(&reply(), "a".repeat(64), 10).unwrap();
        set.routes[0].host = "8.8.8.8".into();
        state::atomic_json(
            &path,
            &Store {
                v: 1,
                sets: vec![set],
            },
        )
        .unwrap();
        assert!(read(&path, 10)[0].routes.is_empty());
    }
}
