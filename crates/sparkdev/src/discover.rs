//! Find my Spark: an owner-only search of this laptop's own private network
//! for the pinned Spark, run only when the owner asks for it.
//!
//! `search` writes nothing. It picks the laptop's physical wired and Wi-Fi
//! adapters, takes at most two of their RFC 1918 networks (each clamped to
//! the laptop's own /24 or narrower), keeps only addresses that Windows routes
//! out of one of those adapters, sends one TCP connect per address to the SSH
//! port, and confirms each answering port with the same pinned `ssh` command
//! every other dial uses. Other devices are reported only as counts.
//!
//! `save` adds one address that passed that pinned check, and only after
//! checking it again. An address becomes dialable only through this explicit
//! owner save into `config.hosts`; automatic rediscovery never searches.
use crate::{
    AppError, Result,
    state::{self, Config},
    transport::{self, Finished, Reach},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    net::Ipv4Addr,
    process::Command,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// Per-connect budget of the sweep. Long enough for ARP retries to a Spark
/// whose Wi-Fi is in power save.
pub const SWEEP_CONNECT: Duration = Duration::from_secs(3);
pub const SWEEP_DEADLINE: Duration = Duration::from_secs(4);
/// Process timeout of one pinned check (ssh's own ConnectTimeout is 5 s).
pub const VERIFY_TIMEOUT: Duration = Duration::from_secs(8);
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);
/// Answering addresses confirmed per search.
pub const MAX_VERIFY: usize = 32;
/// Pinned checks running at once.
pub const PARALLEL: usize = 16;
pub const MAX_NETWORKS: usize = 2;
/// Configured addresses re-checked beside the chosen one during a save.
pub const SAVE_CONFIGURED: usize = 15;
/// Saved addresses beyond which a save warns that the list is getting long.
const MANY_HOSTS: usize = 4;

/// Shown when the gate refuses a caller other than the owner.
pub const OWNER_ONLY: &str =
    "Find my Spark is an owner action; run it from DGXBridge or with --client human";
pub const WINDOWS_ONLY: &str = "Find my Spark is available in the Windows app";
// The same refusals ssh_raw gives.
const DISCONNECTED: &str = "Bridge is disconnected; use sparkdev connect to enable new requests";
const REMOTE_ONLY: &str =
    "Remote-only mode does not permit SSH; choose LAN-only or Both for initial setup";
const NOT_DIRECT: &str =
    "Find my Spark needs a direct SSH route; this alias uses a proxy or bind option";
const LOOKUP_FAILED: &str =
    "Find my Spark could not read this alias's SSH settings with ssh -G; nothing was checked";
const ADAPTERS_FAILED: &str = "Windows did not list this laptop's network adapters";
const NOT_IPV4: &str = "Choose an IPv4 address that Find my Spark found; nothing was saved";
const NOT_PRIVATE: &str =
    "Find my Spark saves only private home or office network addresses; nothing was saved";
const NO_NETWORK: &str = "This laptop has no wired or Wi-Fi connection to a private network right now; nothing was saved";
const OWN_ADDRESS: &str = "This is this laptop's own address, not the Spark's; nothing was saved";
const OFF_NETWORK: &str =
    "This address is not on a network this laptop reaches directly right now; nothing was saved";
const NOT_PINNED: &str =
    "This address did not pass your pinned host key and SSH login; nothing was saved";
const NO_ANSWER: &str = "This address did not answer the pinned SSH check; nothing was saved";
const CHANGED: &str = "The saved SSH connection changed during Find my Spark; search again";

/// The laptop side of a network: what kind of adapter carries it. This is
/// never a statement about the Spark's own link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Medium {
    Wired,
    WiFi,
}
impl Medium {
    pub fn name(self) -> &'static str {
        match self {
            Self::Wired => "wired",
            Self::WiFi => "wifi",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Address {
    pub ip: Ipv4Addr,
    /// The on-link prefix length Windows reports for this address.
    pub prefix: u8,
}
/// A physical adapter that may carry a search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Adapter {
    pub name: String,
    pub medium: Medium,
    pub if_index: u32,
    pub addresses: Vec<Address>,
}
/// What Windows reports about one adapter, before `select` decides whether
/// it is a usable physical adapter.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawAdapter {
    pub if_index: u32,
    /// IANA ifType: 6 is Ethernet, 71 is Wi-Fi.
    pub if_type: u32,
    pub up: bool,
    /// TUNNEL_TYPE; 0 is not a tunnel.
    pub tunnel: i32,
    /// A non-zero IPv4 default gateway is configured.
    pub gateway: bool,
    /// MIB_IF_ROW2 InterfaceAndOperStatusFlags, or None when GetIfEntry2
    /// failed. 0x01 HardwareInterface, 0x02 FilterInterface, 0x04
    /// ConnectorPresent.
    pub flags: Option<u8>,
    pub name: String,
    pub addresses: Vec<RawAddress>,
}
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawAddress {
    pub ip: Ipv4Addr,
    pub prefix: u8,
    /// Duplicate address detection finished and the address is preferred.
    pub preferred: bool,
}
/// Keep only up, untunnelled Ethernet and Wi-Fi hardware with a connector, a
/// default gateway and at least one preferred private IPv4 address. Virtual
/// switches, VPNs, filters and disconnected adapters fall out here.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn select(raw: Vec<RawAdapter>, private: fn(Ipv4Addr) -> bool) -> Vec<Adapter> {
    raw.into_iter()
        .filter_map(|adapter| {
            let medium = match adapter.if_type {
                6 => Medium::Wired,
                71 => Medium::WiFi,
                _ => return None,
            };
            let hardware = adapter
                .flags
                .is_some_and(|f| f & 0x01 != 0 && f & 0x04 != 0 && f & 0x02 == 0);
            if !adapter.up || adapter.tunnel != 0 || !adapter.gateway || !hardware {
                return None;
            }
            let addresses: Vec<Address> = adapter
                .addresses
                .into_iter()
                .filter(|a| a.preferred && private(a.ip))
                .map(|a| Address {
                    ip: a.ip,
                    prefix: a.prefix,
                })
                .collect();
            (!addresses.is_empty()).then(|| Adapter {
                name: adapter.name,
                medium,
                if_index: adapter.if_index,
                addresses,
            })
        })
        .collect()
}
/// RFC 1918 only.
pub fn rfc1918(ip: Ipv4Addr) -> bool {
    ip.is_private()
}
/// This laptop's physical adapters on private networks.
#[cfg(windows)]
pub fn adapters() -> Result<Vec<Adapter>> {
    windows::adapters().map(|raw| select(raw, rfc1918))
}
#[cfg(not(windows))]
pub fn adapters() -> Result<Vec<Adapter>> {
    Err(AppError::new(2, WINDOWS_ONLY))
}
/// The interface Windows would send `ip` out of.
#[cfg(windows)]
pub fn routed_via(ip: Ipv4Addr) -> Option<u32> {
    windows::routed_via(ip)
}
#[cfg(not(windows))]
pub fn routed_via(_: Ipv4Addr) -> Option<u32> {
    None
}

/// One network to search, with every adapter on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    pub base: Ipv4Addr,
    pub prefix: u8,
    pub adapters: Vec<(u32, String, Medium)>,
}
fn mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix.min(32)))
    }
}
impl Network {
    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & mask(self.prefix) == u32::from(self.base)
    }
    /// Whether Windows routes through one of this network's adapters. A
    /// wired and a Wi-Fi adapter on the same /24 both count.
    fn carries(&self, if_index: u32) -> bool {
        self.adapters.iter().any(|(index, ..)| *index == if_index)
    }
    pub fn label(&self) -> String {
        format!("{}/{}", self.base, self.prefix)
    }
    fn adapter_label(&self) -> String {
        self.adapters
            .iter()
            .map(|(_, name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
    fn medium_label(&self) -> &'static str {
        let first = self.adapters.first().map(|(.., medium)| *medium);
        if self
            .adapters
            .iter()
            .all(|(.., medium)| Some(*medium) == first)
        {
            first.map_or("mixed", Medium::name)
        } else {
            "mixed"
        }
    }
}
/// The networks to search: each private address's network, clamped to its
/// /24 or narrower and skipped beyond /30, identical networks merged keeping
/// every adapter, at most MAX_NETWORKS.
pub fn sweep_networks(adapters: &[Adapter], private: fn(Ipv4Addr) -> bool) -> Vec<Network> {
    let mut networks: Vec<Network> = Vec::new();
    for adapter in adapters {
        for address in &adapter.addresses {
            if !private(address.ip) {
                continue;
            }
            let prefix = address.prefix.max(24);
            if prefix > 30 {
                continue;
            }
            let base = Ipv4Addr::from(u32::from(address.ip) & mask(prefix));
            let entry = (adapter.if_index, adapter.name.clone(), adapter.medium);
            if let Some(network) = networks
                .iter_mut()
                .find(|n| n.base == base && n.prefix == prefix)
            {
                if !network.carries(adapter.if_index) {
                    network.adapters.push(entry);
                }
            } else if networks.len() < MAX_NETWORKS {
                networks.push(Network {
                    base,
                    prefix,
                    adapters: vec![entry],
                });
            }
        }
    }
    networks
}
/// Every host address of `network` except this laptop's own, ascending, kept
/// only when Windows routes it out of one of the network's adapters, so a
/// VPN that claims private ranges never carries the sweep.
pub fn targets(
    network: &Network,
    own: &[Ipv4Addr],
    routed_via: &dyn Fn(Ipv4Addr) -> Option<u32>,
) -> Vec<Ipv4Addr> {
    let base = u32::from(network.base);
    let size = 1u32 << (32 - u32::from(network.prefix.min(30)));
    (base + 1..base + size - 1)
        .map(Ipv4Addr::from)
        .filter(|ip| !own.contains(ip) && routed_via(*ip).is_some_and(|i| network.carries(i)))
        .collect()
}
fn own_addresses(adapters: &[Adapter]) -> Vec<Ipv4Addr> {
    adapters
        .iter()
        .flat_map(|a| a.addresses.iter().map(|x| x.ip))
        .collect()
}

/// What one pinned check proved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// Pinned host key and login passed, and `true` ran.
    Verified,
    /// The pinned key alias exists and this host's key differs. Only proof
    /// of another device once the pin itself is proven in the same run.
    OtherHost,
    /// This laptop's own key or SSH configuration could not be used. Never
    /// proof of the pin.
    LocalKeyError,
    /// The SSH login was refused.
    LoginRejected,
    /// No pinned key could be checked: missing, or a generic verification
    /// failure that OpenSSH prints for every host when the pin is wrong.
    UnknownKey,
    /// Reset, broken key exchange, timeout or no answer.
    Unverified,
}
impl Class {
    pub fn status(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::OtherHost => "other_host",
            Self::LocalKeyError => "local_key_error",
            Self::LoginRejected => "login_rejected",
            Self::UnknownKey => "unknown_key",
            Self::Unverified => "unverified",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Check {
    pub class: Class,
    /// The pinned host key matched: the check authenticated, or OpenSSH
    /// itself reported that the server refused the login ("Permission denied
    /// ("), which it reaches only after strict host-key verification passed.
    pub pin_proven: bool,
}
/// Classify one pinned check from its exit code and lowercased stderr; the
/// first match wins. stderr is read here only and never shown or stored.
/// Only lines OpenSSH wrote itself count (see `transport::openssh_own`): a
/// device on the SSH port can echo its banner, disconnect text or algorithm
/// list before the host-key check, and none of that proves the pin, another
/// device, a key error or a refused login. A disconnect the server sends,
/// "Too many authentication failures" included, is therefore unverified.
pub fn classify(result: &Result<Finished>) -> Check {
    let Ok((code, _, stderr)) = result else {
        return Check {
            class: Class::Unverified,
            pin_proven: false,
        };
    };
    if *code == Some(0) {
        return Check {
            class: Class::Verified,
            pin_proven: true,
        };
    }
    let message = transport::openssh_own(&String::from_utf8_lossy(stderr).to_lowercase());
    let any = |markers: &[&str]| markers.iter().any(|m| message.contains(m));
    let class = if any(&[
        "remote host identification has changed",
        "has changed and you have requested strict checking",
    ]) {
        Class::OtherHost
    } else if any(&[
        "bad owner or permissions",
        "bad permissions",
        "unprotected private key",
        "load key",
    ]) {
        Class::LocalKeyError
    } else if any(&["permission denied ("]) {
        Class::LoginRejected
    } else if any(&["host key is known for", "host key verification failed"]) {
        Class::UnknownKey
    } else {
        Class::Unverified
    };
    Check {
        class,
        pin_proven: message.contains("permission denied ("),
    }
}

/// Everything search and save touch outside this module, so tests never
/// enumerate adapters, scan, run ssh or read the real diagnostics.
pub struct Deps {
    pub adapters: Box<dyn Fn() -> Result<Vec<Adapter>>>,
    pub routed_via: Box<dyn Fn(Ipv4Addr) -> Option<u32>>,
    /// The alias's local `ssh -G` output; opens no connection.
    pub ssh_g: Box<dyn Fn(&Config) -> Result<Finished>>,
    pub sweep: Box<dyn Fn(&[Ipv4Addr], u16) -> Vec<Reach>>,
    /// One pinned check of one address.
    pub verify: Box<dyn Fn(&Config, Ipv4Addr) -> Result<Finished> + Send + Sync>,
    pub private: fn(Ipv4Addr) -> bool,
    pub diagnostics: Box<dyn Fn() -> Value>,
}
/// The pinned check: the unchanged `Transport::command` with BatchMode,
/// strict host-key checking against the HostKeyAlias pin and no known_hosts
/// write, running `true`.
pub fn verify_command(config: &Config, ip: Ipv4Addr) -> Command {
    transport::Transport::from_config(config.clone()).command(Some(&ip.to_string()), "true")
}
fn sweep_hosts(targets: &[Ipv4Addr], port: u16) -> Vec<Reach> {
    let hosts: Vec<String> = targets.iter().map(ToString::to_string).collect();
    let hosts: Vec<Option<&str>> = hosts.iter().map(|h| Some(h.as_str())).collect();
    transport::precheck(&hosts, port, SWEEP_CONNECT, SWEEP_DEADLINE)
}
impl Deps {
    pub fn system() -> Self {
        Self {
            adapters: Box::new(adapters),
            routed_via: Box::new(routed_via),
            ssh_g: Box::new(|config| {
                transport::process(transport::alias_lookup_command(config), &[], LOOKUP_TIMEOUT)
            }),
            sweep: Box::new(sweep_hosts),
            verify: Box::new(|config, ip| {
                transport::process(verify_command(config, ip), &[], VERIFY_TIMEOUT)
            }),
            private: rfc1918,
            diagnostics: Box::new(state::diagnostics),
        }
    }
}

fn preflight(config: &Config) -> Result<()> {
    if !config.connection_enabled {
        return Err(AppError::new(4, DISCONNECTED));
    }
    if !config.allows_lan() {
        return Err(AppError::new(4, REMOTE_ONLY));
    }
    Ok(())
}
struct Lookup {
    route: transport::AliasRoute,
    /// The `hostname` line as printed, before parse_ssh_g drops a value that
    /// echoes the alias or is not a plain host.
    raw_hostname: Option<String>,
}
fn raw_hostname(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let (key, value) = line.trim_end_matches('\r').split_once(' ')?;
        (key == "hostname").then(|| value.trim().to_owned())
    })
}
fn alias_lookup(config: &Config, deps: &Deps) -> Result<Lookup> {
    let Ok((Some(0), out, _)) = (deps.ssh_g)(config) else {
        return Err(AppError::new(4, LOOKUP_FAILED));
    };
    let text = String::from_utf8_lossy(&out);
    let route = transport::parse_ssh_g(&text, &config.ssh_alias);
    if !route.direct {
        return Err(AppError::new(4, NOT_DIRECT));
    }
    Ok(Lookup {
        route,
        raw_hostname: raw_hostname(&text),
    })
}
/// Run pinned checks of `hosts`, at most PARALLEL at once. None marks an
/// address no worker reached because its thread could not be started.
fn verify_all(
    config: &Config,
    hosts: &[Ipv4Addr],
    verify: &(dyn Fn(&Config, Ipv4Addr) -> Result<Finished> + Send + Sync),
) -> Vec<Option<Check>> {
    let next = AtomicUsize::new(0);
    let results = Mutex::new(vec![None; hosts.len()]);
    thread::scope(|scope| {
        for _ in 0..PARALLEL.min(hosts.len()) {
            let _ = thread::Builder::new().spawn_scoped(scope, || {
                loop {
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    let Some(ip) = hosts.get(index) else { break };
                    let check = classify(&verify(config, *ip));
                    results.lock().unwrap_or_else(|p| p.into_inner())[index] = Some(check);
                }
            });
        }
    });
    results.into_inner().unwrap_or_else(|p| p.into_inner())
}
/// What became of one address in a search.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seen {
    /// Checked by the pinned ssh command.
    Checked(Class),
    /// Answered on the SSH port but beyond MAX_VERIFY.
    NotChecked,
    /// The port check could not decide in time, or no worker reached the
    /// pinned check. A pinned check that ran and timed out is `Unverified`.
    Unchecked,
    /// Refused or silent on the SSH port.
    Closed,
}
impl Seen {
    fn status(self) -> &'static str {
        match self {
            Self::Checked(class) => class.status(),
            Self::NotChecked => "not_checked",
            Self::Unchecked => "unchecked",
            Self::Closed => "closed",
        }
    }
}
fn configured_index(config: &Config, ip: Ipv4Addr) -> Option<usize> {
    config
        .hosts
        .iter()
        .position(|h| h.parse::<Ipv4Addr>() == Ok(ip))
}
fn elapsed(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

/// Search this laptop's own networks for the pinned Spark. Writes nothing.
pub fn search(config: &Config, deps: &Deps) -> Result<Value> {
    let started = Instant::now();
    preflight(config)?;
    let adapters = (deps.adapters)()?;
    let networks = sweep_networks(&adapters, deps.private);
    let mut out = json!({"ok":true,"saved":false,"searched":[],"open":0,"unchecked":0,"found":[],
        "login_rejected":0,"local_key_error":0,"configured":[],"other_ssh_hosts":0,
        "unknown_key_hosts":0,"unverified":0,"not_checked":0,"pin_hint":false});
    let mut seen: BTreeMap<Ipv4Addr, Seen> = BTreeMap::new();
    let report = |out: &mut Value, seen: &BTreeMap<Ipv4Addr, Seen>, started: Instant| {
        out["configured"] = config
            .hosts
            .iter()
            .enumerate()
            .map(|(index, host)| {
                let status = host
                    .parse::<Ipv4Addr>()
                    .ok()
                    .and_then(|ip| seen.get(&ip))
                    .map_or("not_searched", |s| s.status());
                json!({"index":index,"host":host,"status":status})
            })
            .collect();
        out["elapsed_ms"] = json!(elapsed(started));
    };
    if networks.is_empty() {
        out["reason"] = json!("no_physical_adapter");
        report(&mut out, &seen, started);
        return Ok(out);
    }
    let lookup = alias_lookup(config, deps)?;
    let own = own_addresses(&adapters);
    let mut swept: Vec<Ipv4Addr> = Vec::new();
    let mut searched = Vec::new();
    for network in &networks {
        let found: Vec<Ipv4Addr> = targets(network, &own, &*deps.routed_via)
            .into_iter()
            .filter(|ip| !swept.contains(ip))
            .collect();
        searched.push(json!({"adapter":network.adapter_label(),"medium":network.medium_label(),"network":network.label(),"targets":found.len()}));
        swept.extend(found);
    }
    out["searched"] = json!(searched);
    if swept.is_empty() {
        out["reason"] = json!("no_routed_targets");
        report(&mut out, &seen, started);
        return Ok(out);
    }
    let reach = (deps.sweep)(&swept, config.port);
    let mut open = Vec::new();
    for (index, ip) in swept.iter().enumerate() {
        match reach.get(index).copied().unwrap_or(Reach::Unchecked) {
            Reach::Open => open.push(*ip),
            Reach::Unchecked => {
                seen.insert(*ip, Seen::Unchecked);
            }
            Reach::Closed | Reach::Unresolved => {
                seen.insert(*ip, Seen::Closed);
            }
        }
    }
    out["open"] = json!(open.len());
    // Configured addresses first, then the alias's HostName, then ascending.
    let mut order: Vec<Ipv4Addr> = Vec::new();
    let alias_ip = lookup
        .route
        .hostname
        .as_deref()
        .and_then(|h| h.parse::<Ipv4Addr>().ok());
    for ip in config
        .hosts
        .iter()
        .filter_map(|h| h.parse::<Ipv4Addr>().ok())
        .chain(alias_ip)
    {
        if open.contains(&ip) && !order.contains(&ip) {
            order.push(ip);
        }
    }
    let mut rest: Vec<Ipv4Addr> = open
        .iter()
        .filter(|ip| !order.contains(ip))
        .copied()
        .collect();
    rest.sort();
    order.extend(rest);
    let (chosen, skipped) = order.split_at(order.len().min(MAX_VERIFY));
    for ip in skipped {
        seen.insert(*ip, Seen::NotChecked);
    }
    let checks = verify_all(config, chosen, &*deps.verify);
    let pin_proven = checks.iter().flatten().any(|c| c.pin_proven);
    let mut found = Vec::new();
    for (ip, check) in chosen.iter().zip(&checks) {
        let Some(check) = check else {
            seen.insert(*ip, Seen::Unchecked);
            continue;
        };
        // Without a proven pin, "has changed" cannot be told apart from a
        // stale pin, which OpenSSH reports the same way for the Spark itself.
        let class = match check.class {
            Class::OtherHost if !pin_proven => Class::UnknownKey,
            class => class,
        };
        seen.insert(*ip, Seen::Checked(class));
        if class == Class::Verified {
            let route = (deps.routed_via)(*ip);
            let carrier = adapters
                .iter()
                .find(|a| Some(a.if_index) == route)
                .map(|a| (a.name.clone(), a.medium.name()));
            let (adapter, medium) = carrier.unwrap_or_default();
            found.push(json!({"host":ip.to_string(),"adapter":adapter,"medium":medium,"configured_index":configured_index(config, *ip)}));
        }
    }
    let count = |wanted: Seen| seen.values().filter(|s| **s == wanted).count();
    out["found"] = json!(found);
    out["login_rejected"] = json!(count(Seen::Checked(Class::LoginRejected)));
    out["local_key_error"] = json!(count(Seen::Checked(Class::LocalKeyError)));
    out["other_ssh_hosts"] = json!(count(Seen::Checked(Class::OtherHost)));
    out["unknown_key_hosts"] = json!(count(Seen::Checked(Class::UnknownKey)));
    out["unverified"] = json!(count(Seen::Checked(Class::Unverified)));
    out["not_checked"] = json!(count(Seen::NotChecked));
    out["unchecked"] = json!(count(Seen::Unchecked));
    report(&mut out, &seen, started);
    let unknown = Seen::Checked(Class::UnknownKey);
    let mut pin_hint = config
        .hosts
        .iter()
        .filter_map(|h| h.parse::<Ipv4Addr>().ok())
        .any(|ip| seen.get(&ip) == Some(&unknown));
    if let Some(name) = &lookup.route.hostname {
        let status = name
            .parse::<Ipv4Addr>()
            .ok()
            .and_then(|ip| seen.get(&ip).copied());
        pin_hint |= status == Some(unknown);
        out["alias_hostname_status"] = json!(status.map_or("not_searched", Seen::status));
    }
    out["pin_hint"] = json!(!pin_proven && pin_hint);
    Ok(out)
}

/// Whether `fresh` is still the SSH identity `verified` checked against, and
/// still allows a LAN dial.
fn same_identity(verified: &Config, fresh: &Config) -> bool {
    verified.ssh_alias == fresh.ssh_alias
        && verified.host_key_alias == fresh.host_key_alias
        && verified.port == fresh.port
        && verified.user == fresh.user
        && verified.identity_file == fresh.identity_file
        && verified.connection_enabled == fresh.connection_enabled
        && verified.allows_lan() == fresh.allows_lan()
}

/// Save `host` as the first LAN address after checking it again with the
/// pinned ssh command. Configured addresses are removed only when this run
/// proved them to be another device.
pub fn save(config: &Config, host: &str, deps: &Deps) -> Result<Value> {
    // The lan-route write guard: unknown counts as redirected.
    if (deps.diagnostics)()["redirected"] != false {
        return Err(AppError::new(3, crate::REDIRECTED_STATE));
    }
    preflight(config)?;
    let ip: Ipv4Addr = host.parse().map_err(|_| AppError::new(2, NOT_IPV4))?;
    if !(deps.private)(ip) {
        return Err(AppError::new(2, NOT_PRIVATE));
    }
    let adapters = (deps.adapters)()?;
    let networks = sweep_networks(&adapters, deps.private);
    if networks.is_empty() {
        return Err(AppError::new(2, NO_NETWORK));
    }
    let own = own_addresses(&adapters);
    if own.contains(&ip) {
        return Err(AppError::new(2, OWN_ADDRESS));
    }
    let reachable = |candidate: Ipv4Addr| {
        !own.contains(&candidate)
            && networks.iter().any(|n| {
                n.contains(candidate)
                    && targets_member(n, candidate)
                    && (deps.routed_via)(candidate).is_some_and(|i| n.carries(i))
            })
    };
    if !reachable(ip) {
        return Err(AppError::new(2, OFF_NETWORK));
    }
    let lookup = alias_lookup(config, deps)?;
    let mut wave = vec![ip];
    for candidate in config
        .hosts
        .iter()
        .filter_map(|h| h.parse::<Ipv4Addr>().ok())
    {
        if wave.len() > SAVE_CONFIGURED {
            break;
        }
        if !wave.contains(&candidate) && reachable(candidate) {
            wave.push(candidate);
        }
    }
    let checks = verify_all(config, &wave, &*deps.verify);
    match checks[0].map(|c| c.class) {
        Some(Class::Verified) => {}
        Some(
            Class::OtherHost | Class::UnknownKey | Class::LoginRejected | Class::LocalKeyError,
        ) => {
            return Err(AppError::new(7, NOT_PINNED));
        }
        _ => return Err(AppError::new(4, NO_ANSWER)),
    }
    // HOST passed the pin in this run, so "has changed" elsewhere is proof.
    let other: Vec<Ipv4Addr> = wave
        .iter()
        .zip(&checks)
        .skip(1)
        .filter(|(_, check)| check.is_some_and(|c| c.class == Class::OtherHost))
        .map(|(ip, _)| *ip)
        .collect();
    let canonical = ip.to_string();
    let mut aborted = None;
    let mut removed = Vec::new();
    let saved = state::update_config(|fresh| {
        if !same_identity(config, fresh) {
            aborted = Some(AppError::new(4, CHANGED));
            return;
        }
        let previous = std::mem::take(&mut fresh.hosts);
        let mut hosts = vec![canonical.clone()];
        for entry in &previous {
            let parsed = entry.parse::<Ipv4Addr>().ok();
            if entry.eq_ignore_ascii_case(&canonical) || parsed == Some(ip) {
                continue;
            }
            if parsed.is_some_and(|p| other.contains(&p)) {
                removed.push(entry.clone());
                continue;
            }
            hosts.push(entry.clone());
        }
        // Without hosts the bare alias was dialed and resolved by OpenSSH.
        // Keep that path only when ssh -G shows no HostName of its own.
        if previous.is_empty()
            && lookup
                .raw_hostname
                .as_deref()
                .is_some_and(|raw| raw.eq_ignore_ascii_case(&fresh.ssh_alias))
        {
            hosts.push(fresh.ssh_alias.clone());
        }
        fresh.hosts = hosts;
    })?;
    if let Some(error) = aborted {
        return Err(error);
    }
    let _ = state::remember_host(&saved, &canonical);
    let mut out = crate::connection_response(&saved);
    out["saved"] = json!(true);
    out["host"] = json!(canonical);
    out["hosts"] = json!(saved.hosts);
    out["removed"] = json!(removed);
    if saved.hosts.len() > MANY_HOSTS {
        out["warning"] = json!(format!(
            "You now have {} saved LAN addresses, tried in order. Remove ones you no longer use under Saved SSH connection.",
            saved.hosts.len()
        ));
    }
    Ok(out)
}
/// A host address of `network`: not its base or broadcast address.
fn targets_member(network: &Network, ip: Ipv4Addr) -> bool {
    let offset = u32::from(ip).wrapping_sub(u32::from(network.base));
    offset != 0 && offset != (1u32 << (32 - u32::from(network.prefix))) - 1
}

#[cfg(windows)]
mod windows {
    use super::{RawAdapter, RawAddress};
    use crate::{AppError, Result};
    use std::net::Ipv4Addr;
    use windows_sys::Win32::{
        Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_NO_DATA, NO_ERROR},
        NetworkManagement::{
            IpHelper::{
                GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
                GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses, GetBestInterface, GetIfEntry2,
                IP_ADAPTER_ADDRESSES_LH, MIB_IF_ROW2,
            },
            Ndis::{IfOperStatusUp, NET_LUID_LH},
        },
        Networking::WinSock::{AF_INET, IpDadStatePreferred, SOCKADDR_IN, SOCKET_ADDRESS},
    };
    const FIRST_BUFFER: u32 = 15 * 1024;
    const MAX_BUFFER: u32 = 1024 * 1024;
    /// Bounds on the walk, so a malformed list cannot loop forever.
    const MAX_ADAPTERS: usize = 256;
    const MAX_ITEMS: usize = 64;
    const MAX_NAME: usize = 256;

    pub(super) fn adapters() -> Result<Vec<RawAdapter>> {
        let flags = GAA_FLAG_INCLUDE_GATEWAYS
            | GAA_FLAG_SKIP_ANYCAST
            | GAA_FLAG_SKIP_MULTICAST
            | GAA_FLAG_SKIP_DNS_SERVER;
        let mut size = FIRST_BUFFER;
        // The first call plus at most three retries with the size Windows asks for.
        for _ in 0..4 {
            // u64 storage gives the 8-byte alignment the list requires.
            let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
            let mut length = u32::try_from(buffer.len() * 8).unwrap_or(MAX_BUFFER);
            let code = unsafe {
                GetAdaptersAddresses(
                    u32::from(AF_INET),
                    flags,
                    std::ptr::null(),
                    buffer.as_mut_ptr().cast(),
                    &mut length,
                )
            };
            match code {
                NO_ERROR => {
                    // SAFETY: Windows filled the buffer with a valid list.
                    return Ok(unsafe { walk(buffer.as_ptr().cast(), &interface_flags) });
                }
                ERROR_NO_DATA => return Ok(Vec::new()),
                ERROR_BUFFER_OVERFLOW if length <= MAX_BUFFER => size = length.max(size),
                _ => break,
            }
        }
        Err(AppError::new(3, super::ADAPTERS_FAILED))
    }
    fn interface_flags(luid: &NET_LUID_LH) -> Option<u8> {
        let mut row = MIB_IF_ROW2 {
            InterfaceLuid: *luid,
            ..Default::default()
        };
        // SAFETY: row is a valid MIB_IF_ROW2 that Windows fills in place.
        (unsafe { GetIfEntry2(&mut row) } == NO_ERROR)
            .then_some(row.InterfaceAndOperStatusFlags._bitfield)
    }
    pub(super) fn routed_via(ip: Ipv4Addr) -> Option<u32> {
        let mut index = 0u32;
        // GetBestInterface takes the address in network byte order.
        (unsafe { GetBestInterface(u32::from_ne_bytes(ip.octets()), &mut index) } == NO_ERROR)
            .then_some(index)
    }
    /// The IPv4 address in `address`, if it holds one.
    ///
    /// # Safety
    /// A non-null `lpSockaddr` points to at least `iSockaddrLength` bytes.
    unsafe fn ipv4(address: &SOCKET_ADDRESS) -> Option<Ipv4Addr> {
        let length = usize::try_from(address.iSockaddrLength).ok()?;
        if address.lpSockaddr.is_null() || length < size_of::<SOCKADDR_IN>() {
            return None;
        }
        let socket = unsafe { address.lpSockaddr.cast::<SOCKADDR_IN>().read_unaligned() };
        if socket.sin_family != AF_INET {
            return None;
        }
        // S_addr holds the address bytes in network order.
        Some(Ipv4Addr::from(
            unsafe { socket.sin_addr.S_un.S_addr }.to_ne_bytes(),
        ))
    }
    /// At most MAX_NAME UTF-16 units of a NUL-terminated string.
    ///
    /// # Safety
    /// A non-null `text` is NUL-terminated or at least MAX_NAME units long.
    unsafe fn wide(text: *const u16) -> String {
        if text.is_null() {
            return String::new();
        }
        let mut length = 0;
        while length < MAX_NAME && unsafe { *text.add(length) } != 0 {
            length += 1;
        }
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) })
    }
    /// Read the adapter list GetAdaptersAddresses returned. `flags` asks
    /// GetIfEntry2 for an adapter's interface flags.
    ///
    /// # Safety
    /// `head` is null or the head of a valid IP_ADAPTER_ADDRESSES_LH list.
    pub(super) unsafe fn walk(
        head: *const IP_ADAPTER_ADDRESSES_LH,
        flags: &dyn Fn(&NET_LUID_LH) -> Option<u8>,
    ) -> Vec<RawAdapter> {
        let mut found = Vec::new();
        let mut node = head;
        while found.len() < MAX_ADAPTERS {
            let Some(adapter) = (unsafe { node.as_ref() }) else {
                break;
            };
            let mut gateway = false;
            let mut item = adapter.FirstGatewayAddress.cast_const();
            for _ in 0..MAX_ITEMS {
                let Some(entry) = (unsafe { item.as_ref() }) else {
                    break;
                };
                gateway |= unsafe { ipv4(&entry.Address) }.is_some_and(|ip| !ip.is_unspecified());
                item = entry.Next.cast_const();
            }
            let mut addresses = Vec::new();
            let mut item = adapter.FirstUnicastAddress.cast_const();
            for _ in 0..MAX_ITEMS {
                let Some(entry) = (unsafe { item.as_ref() }) else {
                    break;
                };
                if let Some(ip) = unsafe { ipv4(&entry.Address) } {
                    addresses.push(RawAddress {
                        ip,
                        prefix: entry.OnLinkPrefixLength,
                        preferred: entry.DadState == IpDadStatePreferred,
                    });
                }
                item = entry.Next.cast_const();
            }
            found.push(RawAdapter {
                if_index: unsafe { adapter.Anonymous1.Anonymous.IfIndex },
                if_type: adapter.IfType,
                up: adapter.OperStatus == IfOperStatusUp,
                tunnel: adapter.TunnelType,
                gateway,
                flags: flags(&adapter.Luid),
                name: unsafe { wide(adapter.FriendlyName) },
                addresses,
            });
            node = adapter.Next.cast_const();
        }
        found
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows_sys::Win32::{
            NetworkManagement::IpHelper::{
                IP_ADAPTER_ADDRESSES_LH_0, IP_ADAPTER_ADDRESSES_LH_0_0,
                IP_ADAPTER_GATEWAY_ADDRESS_LH, IP_ADAPTER_UNICAST_ADDRESS_LH,
            },
            Networking::WinSock::{AF_INET6, IN_ADDR, IN_ADDR_0, IpDadStateTentative},
        };
        fn socket(ip: [u8; 4]) -> SOCKADDR_IN {
            SOCKADDR_IN {
                sin_family: AF_INET,
                sin_port: 0,
                sin_addr: IN_ADDR {
                    S_un: IN_ADDR_0 {
                        S_addr: u32::from_ne_bytes(ip),
                    },
                },
                sin_zero: [0; 8],
            }
        }
        fn address(socket: &mut SOCKADDR_IN) -> SOCKET_ADDRESS {
            SOCKET_ADDRESS {
                lpSockaddr: (socket as *mut SOCKADDR_IN).cast(),
                iSockaddrLength: size_of::<SOCKADDR_IN>() as i32,
            }
        }
        /// A synthetic list built in memory: the walk is exercised without
        /// asking Windows for this computer's real adapters.
        #[test]
        fn the_adapter_walk_reads_a_synthetic_list_without_enumerating_the_host() {
            let mut own = socket([192, 0, 2, 10]);
            let mut tentative = socket([192, 0, 2, 11]);
            let mut six = socket([198, 51, 100, 1]);
            six.sin_family = AF_INET6;
            let mut router = socket([192, 0, 2, 1]);
            let mut unicast_last = IP_ADAPTER_UNICAST_ADDRESS_LH {
                Address: address(&mut tentative),
                DadState: IpDadStateTentative,
                OnLinkPrefixLength: 24,
                ..Default::default()
            };
            let mut unicast_six = IP_ADAPTER_UNICAST_ADDRESS_LH {
                Next: &mut unicast_last,
                Address: address(&mut six),
                DadState: IpDadStatePreferred,
                OnLinkPrefixLength: 64,
                ..Default::default()
            };
            let mut unicast = IP_ADAPTER_UNICAST_ADDRESS_LH {
                Next: &mut unicast_six,
                Address: address(&mut own),
                DadState: IpDadStatePreferred,
                OnLinkPrefixLength: 22,
                ..Default::default()
            };
            let mut gateway = IP_ADAPTER_GATEWAY_ADDRESS_LH {
                Address: address(&mut router),
                ..Default::default()
            };
            let mut long_name: Vec<u16> = std::iter::repeat_n(u16::from(b'x'), 300).collect();
            long_name.push(0);
            let mut name: Vec<u16> = "Ethernet".encode_utf16().chain([0]).collect();
            let mut wifi = IP_ADAPTER_ADDRESSES_LH {
                FriendlyName: long_name.as_mut_ptr(),
                IfType: 71,
                Luid: NET_LUID_LH { Value: 9 },
                ..Default::default()
            };
            let ethernet = IP_ADAPTER_ADDRESSES_LH {
                Anonymous1: IP_ADAPTER_ADDRESSES_LH_0 {
                    Anonymous: IP_ADAPTER_ADDRESSES_LH_0_0 {
                        Length: size_of::<IP_ADAPTER_ADDRESSES_LH>() as u32,
                        IfIndex: 12,
                    },
                },
                Next: &mut wifi,
                FirstUnicastAddress: &mut unicast,
                FirstGatewayAddress: &mut gateway,
                FriendlyName: name.as_mut_ptr(),
                IfType: 6,
                OperStatus: IfOperStatusUp,
                Luid: NET_LUID_LH { Value: 7 },
                ..Default::default()
            };
            let asked = std::cell::RefCell::new(Vec::new());
            let flags = |luid: &NET_LUID_LH| {
                let value = unsafe { luid.Value };
                asked.borrow_mut().push(value);
                (value == 7).then_some(0x05)
            };
            let raw = unsafe { walk(&ethernet, &flags) };
            assert_eq!(*asked.borrow(), [7, 9]);
            assert_eq!(raw.len(), 2);
            assert_eq!(
                raw[0],
                RawAdapter {
                    if_index: 12,
                    if_type: 6,
                    up: true,
                    tunnel: 0,
                    gateway: true,
                    flags: Some(0x05),
                    name: "Ethernet".into(),
                    addresses: vec![
                        RawAddress {
                            ip: Ipv4Addr::new(192, 0, 2, 10),
                            prefix: 22,
                            preferred: true
                        },
                        RawAddress {
                            ip: Ipv4Addr::new(192, 0, 2, 11),
                            prefix: 24,
                            preferred: false
                        },
                    ],
                }
            );
            assert_eq!(raw[1].name.encode_utf16().count(), MAX_NAME);
            assert!(!raw[1].up && !raw[1].gateway && raw[1].addresses.is_empty());
            assert_eq!(raw[1].flags, None);
            assert!(unsafe { walk(std::ptr::null(), &flags) }.is_empty());
            // The documentation addresses are not RFC 1918, so the production
            // rule finds nothing to search on this synthetic adapter.
            assert!(crate::discover::select(raw.clone(), crate::discover::rfc1918).is_empty());
            let kept = crate::discover::select(raw, super::super::tests::documentation);
            assert_eq!(kept.len(), 1);
            assert_eq!(kept[0].addresses.len(), 1);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{
        collections::HashMap,
        net::TcpListener,
        sync::{Arc, Mutex},
    };
    /// Tests stand in TEST-NET-1 and TEST-NET-2 for private networks, so a
    /// leaked dial could never reach a real device.
    pub(crate) fn documentation(ip: Ipv4Addr) -> bool {
        matches!(ip.octets(), [192, 0, 2, _] | [198, 51, 100, _])
    }
    fn ip(text: &str) -> Ipv4Addr {
        text.parse().unwrap()
    }
    fn adapter(name: &str, medium: Medium, if_index: u32, addresses: &[(&str, u8)]) -> Adapter {
        Adapter {
            name: name.into(),
            medium,
            if_index,
            addresses: addresses
                .iter()
                .map(|(a, prefix)| Address {
                    ip: ip(a),
                    prefix: *prefix,
                })
                .collect(),
        }
    }
    fn config(hosts: &[&str]) -> Config {
        serde_json::from_value(json!({"ssh_alias":"fixture-spark","hosts":hosts,"host_key_alias":"fixture-pin","user":"owner","connection_mode":"both"})).unwrap()
    }
    const CHANGED_KEY: &str = "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\r\n@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\nHost key for fixture-pin has changed and you have requested strict checking.\r\nHost key verification failed.\r\n";
    const MISSING_PIN: &str = "No ED25519 host key is known for fixture-pin and you have requested strict checking.\r\nHost key verification failed.\r\n";
    const LOGIN_REFUSED: &str = "owner@192.0.2.40: Permission denied (publickey).\r\n";
    const RESET: &str = "kex_exchange_identification: read: Connection reset\r\nConnection reset by 192.0.2.40 port 22\r\n";
    #[derive(Default)]
    struct Calls {
        ssh_g: usize,
        sweeps: Vec<Vec<Ipv4Addr>>,
        verified: Vec<Ipv4Addr>,
    }
    struct Fake {
        adapters: Vec<Adapter>,
        /// Interface per /24 base; anything else is unrouted.
        routes: Vec<(Ipv4Addr, u32)>,
        ssh_g: Option<String>,
        open: Vec<Ipv4Addr>,
        unchecked: Vec<Ipv4Addr>,
        replies: HashMap<Ipv4Addr, (i32, &'static str)>,
        redirected: Value,
    }
    impl Fake {
        fn new() -> Self {
            Self {
                adapters: vec![adapter(
                    "Ethernet",
                    Medium::Wired,
                    12,
                    &[("192.0.2.10", 24)],
                )],
                routes: vec![(ip("192.0.2.0"), 12)],
                ssh_g: Some("user owner\nhostname 192.0.2.99\nport 22\nproxyjump none\n".into()),
                open: Vec::new(),
                unchecked: Vec::new(),
                replies: HashMap::new(),
                redirected: json!(false),
            }
        }
        fn reply(mut self, host: &str, code: i32, stderr: &'static str) -> Self {
            self.open.push(ip(host));
            self.replies.insert(ip(host), (code, stderr));
            self
        }
        fn build(self) -> (Deps, Arc<Mutex<Calls>>) {
            let calls = Arc::new(Mutex::new(Calls::default()));
            let routes = self.routes;
            let (ssh_g_calls, sweep_calls, verify_calls) =
                (calls.clone(), calls.clone(), calls.clone());
            let ssh_g = self.ssh_g;
            let (open, unchecked, replies) = (self.open, self.unchecked, self.replies);
            let redirected = self.redirected;
            let adapters = self.adapters;
            let deps = Deps {
                adapters: Box::new(move || Ok(adapters.clone())),
                routed_via: Box::new(move |target| {
                    let base = Ipv4Addr::from(u32::from(target) & mask(24));
                    routes.iter().find(|(b, _)| *b == base).map(|(_, i)| *i)
                }),
                ssh_g: Box::new(move |_| {
                    ssh_g_calls.lock().unwrap().ssh_g += 1;
                    match &ssh_g {
                        Some(text) => Ok((Some(0), text.clone().into_bytes(), Vec::new())),
                        None => Ok((Some(255), Vec::new(), b"ssh: unavailable".to_vec())),
                    }
                }),
                sweep: Box::new(move |targets, _| {
                    sweep_calls.lock().unwrap().sweeps.push(targets.to_vec());
                    targets
                        .iter()
                        .map(|t| {
                            if open.contains(t) {
                                Reach::Open
                            } else if unchecked.contains(t) {
                                Reach::Unchecked
                            } else {
                                Reach::Closed
                            }
                        })
                        .collect()
                }),
                verify: Box::new(move |_, target| {
                    verify_calls.lock().unwrap().verified.push(target);
                    match replies.get(&target) {
                        Some((code, stderr)) => {
                            Ok((Some(*code), Vec::new(), stderr.as_bytes().to_vec()))
                        }
                        None => Err(AppError::new(4, "Transport timeout")),
                    }
                }),
                private: documentation,
                diagnostics: Box::new(move || json!({"redirected":redirected.clone()})),
            };
            (deps, calls)
        }
    }
    fn routes_all(_: Ipv4Addr) -> Option<u32> {
        Some(12)
    }

    #[test]
    fn networks_are_clamped_to_the_own_slash_24_and_skipped_beyond_slash_30() {
        let cases = [
            (16, Some(("192.0.2.0", 24))),
            (22, Some(("192.0.2.0", 24))),
            (24, Some(("192.0.2.0", 24))),
            (28, Some(("192.0.2.32", 28))),
            (31, None),
        ];
        for (prefix, expected) in cases {
            let adapters = [adapter(
                "Ethernet",
                Medium::Wired,
                12,
                &[("192.0.2.40", prefix)],
            )];
            let networks = sweep_networks(&adapters, documentation);
            match expected {
                Some((base, length)) => {
                    assert_eq!(networks.len(), 1, "/{prefix}");
                    assert_eq!(
                        (networks[0].base, networks[0].prefix),
                        (ip(base), length),
                        "/{prefix}"
                    );
                }
                None => assert!(networks.is_empty(), "/{prefix}"),
            }
        }
        let small = sweep_networks(
            &[adapter(
                "Ethernet",
                Medium::Wired,
                12,
                &[("192.0.2.40", 28)],
            )],
            documentation,
        );
        assert_eq!(
            targets(&small[0], &[ip("192.0.2.40")], &routes_all).len(),
            13
        );
        // The production rule refuses addresses that are not RFC 1918.
        let public = [adapter(
            "Ethernet",
            Medium::Wired,
            12,
            &[("192.0.2.40", 24), ("203.0.113.5", 24)],
        )];
        assert!(sweep_networks(&public, rfc1918).is_empty());
        assert_eq!(sweep_networks(&public, documentation).len(), 1);
    }
    #[test]
    fn identical_networks_merge_every_adapter_and_at_most_two_are_searched() {
        let adapters = [
            adapter("Ethernet", Medium::Wired, 12, &[("192.0.2.10", 24)]),
            adapter(
                "Wi-Fi",
                Medium::WiFi,
                15,
                &[("192.0.2.11", 24), ("198.51.100.7", 24)],
            ),
            adapter(
                "USB Ethernet",
                Medium::Wired,
                21,
                &[("198.51.100.8", 25), ("192.0.2.12", 24)],
            ),
        ];
        let networks = sweep_networks(&adapters, documentation);
        assert_eq!(networks.len(), 2);
        assert_eq!(networks[0].label(), "192.0.2.0/24");
        assert_eq!(
            networks[0].adapters.iter().map(|a| a.0).collect::<Vec<_>>(),
            [12, 15, 21]
        );
        assert_eq!(networks[0].medium_label(), "mixed");
        assert_eq!(networks[0].adapter_label(), "Ethernet, Wi-Fi, USB Ethernet");
        assert_eq!(networks[1].label(), "198.51.100.0/24");
        assert_eq!(networks[1].medium_label(), "wifi");
        // A third distinct network is never searched.
        let three = [
            adapter("A", Medium::Wired, 1, &[("192.0.2.10", 24)]),
            adapter("B", Medium::Wired, 2, &[("198.51.100.10", 24)]),
            adapter("C", Medium::Wired, 3, &[("198.51.100.200", 26)]),
        ];
        let capped = sweep_networks(&three, documentation);
        assert_eq!(
            capped.iter().map(Network::label).collect::<Vec<_>>(),
            ["192.0.2.0/24", "198.51.100.0/24"]
        );
    }
    #[test]
    fn targets_skip_own_network_broadcast_and_unrouted_addresses() {
        let adapters = [
            adapter("Ethernet", Medium::Wired, 12, &[("192.0.2.10", 24)]),
            adapter("Wi-Fi", Medium::WiFi, 15, &[("192.0.2.11", 24)]),
        ];
        let network = &sweep_networks(&adapters, documentation)[0];
        let own = own_addresses(&adapters);
        // A VPN claims .128/25; the Wi-Fi adapter carries .100 to .127.
        let route = |target: Ipv4Addr| {
            let last = target.octets()[3];
            Some(if last >= 128 {
                40
            } else if last >= 100 {
                15
            } else {
                12
            })
        };
        let found = targets(network, &own, &route);
        assert_eq!(found.first(), Some(&ip("192.0.2.1")));
        assert_eq!(found.last(), Some(&ip("192.0.2.127")));
        assert_eq!(found.len(), 127 - 2);
        assert!(!found.contains(&ip("192.0.2.0")));
        assert!(!found.contains(&ip("192.0.2.10")) && !found.contains(&ip("192.0.2.11")));
        assert!(found.windows(2).all(|w| w[0] < w[1]));
        assert!(targets(network, &own, &|_| None).is_empty());
        let all = targets(network, &own, &routes_all);
        assert_eq!(all.len(), 252);
        assert!(!all.contains(&ip("192.0.2.255")));
    }
    #[test]
    fn classify_reads_the_pin_and_login_only_from_openssh_text() {
        let ran = |code: Option<i32>, stderr: &str| -> Result<Finished> {
            Ok((code, Vec::new(), stderr.as_bytes().to_vec()))
        };
        let check = |class, pin_proven| Check { class, pin_proven };
        assert_eq!(classify(&ran(Some(0), "")), check(Class::Verified, true));
        assert_eq!(
            classify(&ran(Some(255), CHANGED_KEY)),
            check(Class::OtherHost, false)
        );
        assert_eq!(
            classify(&ran(
                Some(255),
                "Host key for fixture-pin has changed and you have requested strict checking."
            )),
            check(Class::OtherHost, false)
        );
        // A missing or rotated pin prints the generic line for every host,
        // the Spark included: that is never proof of another device.
        assert_eq!(
            classify(&ran(Some(255), MISSING_PIN)),
            check(Class::UnknownKey, false)
        );
        assert_eq!(
            classify(&ran(Some(255), "Host key verification failed.\r\n")),
            check(Class::UnknownKey, false)
        );
        // OpenSSH stops on a bad config or key file before or without
        // authenticating; neither proves the pin.
        assert_eq!(
            classify(&ran(
                Some(255),
                "Bad owner or permissions on C:\\\\Users\\\\owner/.ssh/config\r\n"
            )),
            check(Class::LocalKeyError, false)
        );
        assert_eq!(
            classify(&ran(
                Some(255),
                "Load key \"C:/keys/fixture\": bad permissions\r\n"
            )),
            check(Class::LocalKeyError, false)
        );
        assert_eq!(
            classify(&ran(
                Some(255),
                "WARNING: UNPROTECTED PRIVATE KEY FILE!\r\n"
            )),
            check(Class::LocalKeyError, false)
        );
        // A key that failed to load followed by the server's refusal: the
        // local problem is reported, and the refusal proves the pin.
        assert_eq!(
            classify(&ran(
                Some(255),
                "Load key \"C:/keys/fixture\": bad permissions\r\nowner@192.0.2.40: Permission denied (publickey).\r\n"
            )),
            check(Class::LocalKeyError, true)
        );
        assert_eq!(
            classify(&ran(Some(255), LOGIN_REFUSED)),
            check(Class::LoginRejected, true)
        );
        // Text a device on the SSH port controls never classifies a check:
        // a server's disconnect reason, and the forgeries captured from
        // OpenSSH 9.5 (Windows) against a loopback peer before any key
        // exchange, one with CR and LF forging a whole refusal line.
        for echoed in [
            "Received disconnect from 192.0.2.40 port 22:2: Too many authentication failures\r\nDisconnected from 192.0.2.40 port 22\r\n",
            "Received disconnect from 127.0.0.1 port 55451:2: Permission denied (publickey). remote host identification has changed\r\nDisconnected from 127.0.0.1 port 55451\r\n",
            "Received disconnect from 127.0.0.1 port 63404:2: bye\r\nowner@127.0.0.1: Permission denied (publickey).\r\nDisconnected from 127.0.0.1 port 63404\r\n",
            "Bad remote protocol version identification: 'SSH-x Permission denied (publickey). remote host identification has changed'\r\nbanner exchange: Connection to 127.0.0.1 port 55452: invalid format\r\n",
            "Bad remote protocol version identification: 'SSH-x Permission denied (publickey). load key'\r\nbanner exchange: Connection to 127.0.0.1 port 55453: invalid format\r\n",
            "Unable to negotiate with 127.0.0.1 port 55454: no matching key exchange method found. Their offer: x\r\nowner@127.0.0.1: Permission denied (publickey).\r\n",
        ] {
            assert_eq!(
                classify(&ran(Some(255), echoed)),
                check(Class::Unverified, false),
                "{echoed}"
            );
        }
        for unverified in [
            ran(Some(255), RESET),
            ran(Some(255), ""),
            ran(None, ""),
            ran(Some(1), "true: command failed"),
            Err(AppError::new(4, "Transport timeout")),
        ] {
            assert_eq!(classify(&unverified), check(Class::Unverified, false));
        }
    }
    #[test]
    fn forged_answers_never_prove_the_pin_or_another_device() {
        let home = state::TestHome::new();
        // The owner reinstalled the Spark: its configured address now shows
        // a changed key. One LAN device sends a disconnect forging a refused
        // login and a changed key before any key exchange. The pin stays
        // unproven, so the Spark's address is an unknown key, not another
        // device, and the owner is pointed at known_hosts.
        const FORGED: &str = "Received disconnect from 192.0.2.42 port 22:2: Permission denied (publickey). remote host identification has changed\r\nDisconnected from 192.0.2.42 port 22\r\n";
        let (deps, _) = Fake::new()
            .reply("192.0.2.41", 255, CHANGED_KEY)
            .reply("192.0.2.42", 255, FORGED)
            .build();
        let out = search(&config(&["192.0.2.41"]), &deps).unwrap();
        assert_eq!(out["configured"][0]["status"], "unknown_key");
        assert_eq!(out["other_ssh_hosts"], 0);
        assert_eq!(out["login_rejected"], 0);
        assert_eq!(out["unverified"], 1);
        assert_eq!(out["pin_hint"], true);
        // A save keeps a configured address whose forged disconnect claims
        // a changed key: only OpenSSH's own warning proves another device.
        let fixture = config(&["192.0.2.20"]);
        write_config(&home, &fixture);
        let (deps, _) = Fake::new()
            .reply("192.0.2.40", 0, "")
            .reply(
                "192.0.2.20",
                255,
                "Received disconnect from 192.0.2.20 port 22:2: @ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @\r\nDisconnected from 192.0.2.20 port 22\r\n",
            )
            .build();
        let out = save(&fixture, "192.0.2.40", &deps).unwrap();
        assert_eq!(out["removed"], json!([]));
        assert_eq!(saved(&home).hosts, ["192.0.2.40", "192.0.2.20"]);
    }
    #[test]
    fn the_pinned_check_uses_the_unchanged_strict_ssh_command() {
        let mut fixture = config(&["192.0.2.40"]);
        fixture.identity_file = Some(std::env::temp_dir().join("fixture-key"));
        let command = verify_command(&fixture, ip("192.0.2.41"));
        assert_eq!(command.get_program(), "ssh");
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        for required in [
            "BatchMode=yes",
            "StrictHostKeyChecking=yes",
            "HostKeyAlias=fixture-pin",
            "HostName=192.0.2.41",
            "PasswordAuthentication=no",
            "IdentitiesOnly=yes",
        ] {
            assert!(
                args.windows(2).any(|w| w[0] == "-o" && w[1] == required),
                "{required} missing from {args:?}"
            );
        }
        assert!(!args.iter().any(|a| {
            let a = a.to_ascii_lowercase();
            a.contains("accept-new")
                || a.contains("stricthostkeychecking=no")
                || a.contains("userknownhostsfile")
        }));
        assert_eq!(
            &args[args.len() - 4..],
            ["-p", "22", "fixture-spark", "true"]
        );
    }
    #[test]
    fn the_system_sweep_checks_loopback_on_the_configured_port() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let closed = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let sweep = Deps::system().sweep;
        assert_eq!(sweep(&[Ipv4Addr::LOCALHOST], port), [Reach::Open]);
        assert_eq!(sweep(&[Ipv4Addr::LOCALHOST], closed), [Reach::Closed]);
        drop(listener);
    }
    #[test]
    fn search_refuses_before_any_sweep_or_check() {
        let _home = state::TestHome::new();
        let mut disabled = config(&[]);
        disabled.connection_enabled = false;
        let mut remote = config(&[]);
        remote.connection_mode = Some(state::ConnectionMode::RemoteOnly);
        for (fixture, ssh_g, message) in [
            (disabled, Fake::new().ssh_g, DISCONNECTED),
            (remote, Fake::new().ssh_g, REMOTE_ONLY),
            (
                config(&[]),
                Some("hostname 192.0.2.99\nproxyjump jump-host\n".into()),
                NOT_DIRECT,
            ),
            (
                config(&[]),
                Some("hostname 192.0.2.99\nbindinterface Ethernet\n".into()),
                NOT_DIRECT,
            ),
            (config(&[]), None, LOOKUP_FAILED),
        ] {
            let mut fake = Fake::new().reply("192.0.2.40", 0, "");
            fake.ssh_g = ssh_g;
            let (deps, calls) = fake.build();
            let error = search(&fixture, &deps).unwrap_err();
            assert_eq!((error.code, error.message.as_str()), (4, message));
            let calls = calls.lock().unwrap();
            assert!(
                calls.sweeps.is_empty() && calls.verified.is_empty(),
                "{message}"
            );
            assert!(calls.ssh_g <= 1);
        }
        // No physical adapter: an ordinary result naming why, with no lookup.
        let mut fake = Fake::new();
        fake.adapters.clear();
        let (deps, calls) = fake.build();
        let out = search(&config(&["192.0.2.40"]), &deps).unwrap();
        assert_eq!(out["ok"], true);
        assert_eq!(out["reason"], "no_physical_adapter");
        assert_eq!(out["configured"][0]["status"], "not_searched");
        let calls = calls.lock().unwrap();
        assert_eq!(
            (calls.ssh_g, calls.sweeps.len(), calls.verified.len()),
            (0, 0, 0)
        );
    }
    #[test]
    fn search_checks_only_answering_addresses_and_reports_others_as_counts() {
        let home = state::TestHome::new();
        let mut fake = Fake::new()
            .reply("192.0.2.40", 0, "")
            .reply("192.0.2.41", 255, CHANGED_KEY)
            .reply("192.0.2.42", 255, LOGIN_REFUSED)
            .reply("192.0.2.43", 255, RESET)
            .reply("192.0.2.44", 255, MISSING_PIN);
        fake.unchecked = vec![ip("192.0.2.45")];
        let (deps, calls) = fake.build();
        let fixture = config(&["192.0.2.41", "spark.local", "192.0.2.50", "192.0.2.45"]);
        let out = search(&fixture, &deps).unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.sweeps.len(), 1);
        assert_eq!(calls.sweeps[0].len(), 253);
        let mut verified = calls.verified.clone();
        verified.sort();
        assert_eq!(
            verified,
            [
                "192.0.2.40",
                "192.0.2.41",
                "192.0.2.42",
                "192.0.2.43",
                "192.0.2.44"
            ]
            .map(ip)
        );
        assert_eq!(out["open"], 5);
        assert_eq!(out["unchecked"], 1);
        assert_eq!(
            out["found"],
            json!([{"host":"192.0.2.40","adapter":"Ethernet","medium":"wired","configured_index":null}])
        );
        assert_eq!(
            out["searched"],
            json!([{"adapter":"Ethernet","medium":"wired","network":"192.0.2.0/24","targets":253}])
        );
        // The pin was proven in this run, so the changed key is another device.
        assert_eq!(out["other_ssh_hosts"], 1);
        assert_eq!(out["login_rejected"], 1);
        assert_eq!(out["unverified"], 1);
        assert_eq!(out["unknown_key_hosts"], 1);
        assert_eq!(out["pin_hint"], false);
        assert_eq!(out["alias_hostname_status"], "closed");
        let statuses: Vec<_> = out["configured"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["status"].as_str().unwrap())
            .collect();
        assert_eq!(
            statuses,
            ["other_host", "not_searched", "closed", "unchecked"]
        );
        // Addresses that are neither configured nor found appear only as counts.
        let text = out.to_string();
        for hidden in ["192.0.2.42", "192.0.2.43", "192.0.2.44", "192.0.2.99"] {
            assert!(!text.contains(hidden), "{hidden} leaked into {text}");
        }
        // Nothing is written: no config, route memory or lock file.
        assert_eq!(fs_entries(&home.path), 0);
    }
    fn fs_entries(path: &std::path::Path) -> usize {
        std::fs::read_dir(path).unwrap().count()
    }
    #[test]
    fn search_caps_checks_and_orders_configured_then_alias_then_ascending() {
        let _home = state::TestHome::new();
        let mut fake = Fake::new();
        for n in 100..140 {
            fake = fake.reply(&format!("192.0.2.{n}"), 255, RESET);
        }
        fake = fake
            .reply("192.0.2.200", 255, RESET)
            .reply("192.0.2.99", 0, "");
        let (deps, calls) = fake.build();
        let out = search(&config(&["192.0.2.200", "192.0.2.139"]), &deps).unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.verified.len(), MAX_VERIFY);
        let mut expected: Vec<Ipv4Addr> =
            vec![ip("192.0.2.200"), ip("192.0.2.139"), ip("192.0.2.99")];
        expected.extend((100..129).map(|n| Ipv4Addr::new(192, 0, 2, n)));
        let mut verified = calls.verified.clone();
        verified.sort();
        expected.sort();
        assert_eq!(verified, expected);
        assert_eq!(out["open"], 42);
        assert_eq!(out["not_checked"], 10);
        assert_eq!(out["found"][0]["host"], "192.0.2.99");
        assert_eq!(out["alias_hostname_status"], "verified");
    }
    #[test]
    fn a_changed_key_is_another_device_only_once_the_pin_is_proven() {
        let _home = state::TestHome::new();
        let (deps, _) = Fake::new()
            .reply("192.0.2.41", 255, CHANGED_KEY)
            .reply("192.0.2.42", 255, CHANGED_KEY)
            .build();
        let out = search(&config(&["192.0.2.41"]), &deps).unwrap();
        assert_eq!(out["other_ssh_hosts"], 0);
        assert_eq!(out["unknown_key_hosts"], 2);
        assert_eq!(out["configured"][0]["status"], "unknown_key");
        assert_eq!(out["pin_hint"], true);
        // A login refusal proves the pin, so the same answers are now other devices.
        let (deps, _) = Fake::new()
            .reply("192.0.2.41", 255, CHANGED_KEY)
            .reply("192.0.2.42", 255, LOGIN_REFUSED)
            .build();
        let out = search(&config(&["192.0.2.41"]), &deps).unwrap();
        assert_eq!(out["other_ssh_hosts"], 1);
        assert_eq!(out["configured"][0]["status"], "other_host");
        assert_eq!(out["pin_hint"], false);
        // The hint also follows the alias's HostName.
        let (deps, _) = Fake::new().reply("192.0.2.99", 255, MISSING_PIN).build();
        let out = search(&config(&[]), &deps).unwrap();
        assert_eq!(out["alias_hostname_status"], "unknown_key");
        assert_eq!(out["pin_hint"], true);
        // Every route through a VPN: nothing is swept at all.
        let mut fake = Fake::new().reply("192.0.2.40", 0, "");
        fake.routes = vec![(ip("192.0.2.0"), 40)];
        let (deps, calls) = fake.build();
        let out = search(&config(&[]), &deps).unwrap();
        assert_eq!(out["reason"], "no_routed_targets");
        assert!(calls.lock().unwrap().sweeps.is_empty());
    }

    fn write_config(home: &state::TestHome, config: &Config) {
        state::atomic_json(&home.path.join("config.json"), config).unwrap();
    }
    fn saved(home: &state::TestHome) -> Config {
        serde_json::from_slice(&std::fs::read(home.path.join("config.json")).unwrap()).unwrap()
    }
    fn raw_config(home: &state::TestHome) -> Vec<u8> {
        std::fs::read(home.path.join("config.json")).unwrap()
    }
    #[test]
    fn save_refuses_anything_but_a_reachable_private_ipv4_address() {
        let home = state::TestHome::new();
        let fixture = config(&["192.0.2.20"]);
        write_config(&home, &fixture);
        let before = raw_config(&home);
        for (host, message) in [
            ("spark.local", NOT_IPV4),
            ("2001:db8::1", NOT_IPV4),
            ("[2001:db8::1]", NOT_IPV4),
            ("192.0.2.40:22", NOT_IPV4),
            ("203.0.113.5", NOT_PRIVATE),
            ("198.51.100.20", OFF_NETWORK),
            ("192.0.2.0", OFF_NETWORK),
            ("192.0.2.255", OFF_NETWORK),
            ("192.0.2.10", OWN_ADDRESS),
        ] {
            let (deps, calls) = Fake::new().reply("192.0.2.40", 0, "").build();
            let error = save(&fixture, host, &deps).unwrap_err();
            assert_eq!((error.code, error.message.as_str()), (2, message), "{host}");
            assert!(calls.lock().unwrap().verified.is_empty(), "{host}");
        }
        // Routed through a VPN instead of the network's own adapter.
        let mut fake = Fake::new().reply("192.0.2.40", 0, "");
        fake.routes = vec![(ip("192.0.2.0"), 40)];
        let (deps, _) = fake.build();
        assert_eq!(
            save(&fixture, "192.0.2.40", &deps).unwrap_err().message,
            OFF_NETWORK
        );
        let mut fake = Fake::new();
        fake.adapters.clear();
        let (deps, _) = fake.build();
        assert_eq!(
            save(&fixture, "192.0.2.40", &deps).unwrap_err().message,
            NO_NETWORK
        );
        // A redirected or unknown state folder is refused before any check.
        for redirected in [json!(true), json!(null)] {
            let mut fake = Fake::new().reply("192.0.2.40", 0, "");
            fake.redirected = redirected;
            let (deps, calls) = fake.build();
            let error = save(&fixture, "192.0.2.40", &deps).unwrap_err();
            assert_eq!(
                (error.code, error.message.as_str()),
                (3, crate::REDIRECTED_STATE)
            );
            assert!(calls.lock().unwrap().verified.is_empty());
        }
        let mut disabled = fixture.clone();
        disabled.connection_enabled = false;
        let (deps, _) = Fake::new().reply("192.0.2.40", 0, "").build();
        assert_eq!(
            save(&disabled, "192.0.2.40", &deps).unwrap_err().message,
            DISCONNECTED
        );
        assert_eq!(raw_config(&home), before);
        assert!(!home.path.join("lan-route.json").exists());
    }
    #[test]
    fn save_writes_nothing_unless_the_address_passes_the_pinned_login() {
        let home = state::TestHome::new();
        let fixture = config(&["192.0.2.20"]);
        write_config(&home, &fixture);
        let before = raw_config(&home);
        for (stderr, code, message) in [
            (CHANGED_KEY, 7, NOT_PINNED),
            (MISSING_PIN, 7, NOT_PINNED),
            (LOGIN_REFUSED, 7, NOT_PINNED),
            (
                "Bad owner or permissions on C:/Users/owner/.ssh/config\r\n",
                7,
                NOT_PINNED,
            ),
            (RESET, 4, NO_ANSWER),
            ("", 4, NO_ANSWER),
        ] {
            // A configured address that answers as another device is still kept.
            let (deps, _) = Fake::new()
                .reply("192.0.2.40", 255, stderr)
                .reply("192.0.2.20", 255, CHANGED_KEY)
                .build();
            let error = save(&fixture, "192.0.2.40", &deps).unwrap_err();
            assert_eq!(
                (error.code, error.message.as_str()),
                (code, message),
                "{stderr}"
            );
        }
        // A check that never ran (timeout) is no answer either.
        let (deps, _) = Fake::new().build();
        assert_eq!(
            save(&fixture, "192.0.2.40", &deps).unwrap_err().message,
            NO_ANSWER
        );
        assert_eq!(raw_config(&home), before);
        assert!(!home.path.join("lan-route.json").exists());
    }
    #[test]
    fn save_moves_the_address_first_and_removes_only_proven_other_devices() {
        let home = state::TestHome::new();
        let fixture = config(&[
            "192.0.2.20",
            "192.0.2.21",
            "192.0.2.22",
            "spark-b.local",
            "198.51.100.30",
            "192.0.2.40",
        ]);
        write_config(&home, &fixture);
        let (deps, calls) = Fake::new()
            .reply("192.0.2.40", 0, "")
            .reply("192.0.2.20", 255, CHANGED_KEY)
            .reply("192.0.2.21", 255, MISSING_PIN)
            .build();
        let out = save(&fixture, "192.0.2.40", &deps).unwrap();
        assert_eq!(out["ok"], true);
        assert_eq!(out["saved"], true);
        assert_eq!(out["removed"], json!(["192.0.2.20"]));
        let hosts = [
            "192.0.2.40",
            "192.0.2.21",
            "192.0.2.22",
            "spark-b.local",
            "198.51.100.30",
        ];
        assert_eq!(out["hosts"], json!(hosts));
        assert_eq!(saved(&home).hosts, hosts);
        assert!(
            out["warning"]
                .as_str()
                .unwrap()
                .contains("5 saved LAN addresses")
        );
        assert_eq!(out["connection"]["mode"], "both");
        // One wave: the chosen address plus configured ones on this network.
        let mut wave = calls.lock().unwrap().verified.clone();
        wave.sort();
        assert_eq!(
            wave,
            ["192.0.2.20", "192.0.2.21", "192.0.2.22", "192.0.2.40"].map(ip)
        );
        // The saved address leads the next plan as the remembered address.
        let next = saved(&home);
        let plan = transport::plan_candidates(
            None,
            &state::remembered_hosts(&next),
            &next.hosts,
            Some("192.0.2.99"),
        );
        assert_eq!(
            plan[0],
            transport::Candidate {
                source: transport::Source::Remembered,
                host: Some("192.0.2.40".into())
            }
        );
        // Moving an address that is already listed keeps the rest in order.
        let fixture = config(&["192.0.2.20", "192.0.2.30"]);
        write_config(&home, &fixture);
        let (deps, _) = Fake::new().reply("192.0.2.30", 0, "").build();
        let out = save(&fixture, "192.0.2.30", &deps).unwrap();
        assert_eq!(out["hosts"], json!(["192.0.2.30", "192.0.2.20"]));
        assert!(out.get("warning").is_none());
        assert_eq!(out["removed"], json!([]));
    }
    #[test]
    fn save_keeps_the_bare_alias_only_when_ssh_shows_no_hostname_of_its_own() {
        let home = state::TestHome::new();
        let fixture = config(&[]);
        for (ssh_g, hosts) in [
            (
                "hostname fixture-spark\nport 22\n",
                vec!["192.0.2.40", "fixture-spark"],
            ),
            (
                "hostname FIXTURE-SPARK\nport 22\n",
                vec!["192.0.2.40", "fixture-spark"],
            ),
            ("hostname 192.0.2.99\nport 22\n", vec!["192.0.2.40"]),
            // A HostName that is not a plain host is dropped by parse_ssh_g,
            // and must not bring the label back as a DNS name.
            ("hostname spark;name\nport 22\n", vec!["192.0.2.40"]),
        ] {
            write_config(&home, &fixture);
            let mut fake = Fake::new().reply("192.0.2.40", 0, "");
            fake.ssh_g = Some(ssh_g.into());
            let (deps, _) = fake.build();
            assert_eq!(
                save(&fixture, "192.0.2.40", &deps).unwrap()["hosts"],
                json!(hosts),
                "{ssh_g}"
            );
        }
        // With hosts already configured the alias is never added.
        let fixture = config(&["192.0.2.20"]);
        write_config(&home, &fixture);
        let mut fake = Fake::new().reply("192.0.2.40", 0, "");
        fake.ssh_g = Some("hostname fixture-spark\n".into());
        let (deps, _) = fake.build();
        assert_eq!(
            save(&fixture, "192.0.2.40", &deps).unwrap()["hosts"],
            json!(["192.0.2.40", "192.0.2.20"])
        );
    }
    #[test]
    fn save_writes_nothing_when_the_ssh_identity_changed_during_the_check() {
        let home = state::TestHome::new();
        // The configured address is off this network, so the wave checks
        // only the chosen address and the fake rewrites config.json once.
        let fixture = config(&["198.51.100.20"]);
        let changes: [fn(&mut Config); 7] = [
            |c| c.ssh_alias = "other-spark".into(),
            |c| c.host_key_alias = "other-pin".into(),
            |c| c.port = 2222,
            |c| c.user = Some("someone".into()),
            |c| c.identity_file = Some(std::env::temp_dir().join("other-key")),
            |c| c.connection_enabled = false,
            |c| c.connection_mode = Some(state::ConnectionMode::RemoteOnly),
        ];
        for change in changes {
            write_config(&home, &fixture);
            let path = home.path.join("config.json");
            let (mut deps, _) = Fake::new().build();
            deps.verify = Box::new(move |checked, _| {
                // Setup or Disconnect runs while the pinned check is in flight.
                let mut changed = checked.clone();
                change(&mut changed);
                state::atomic_json(&path, &changed).unwrap();
                Ok((Some(0), Vec::new(), Vec::new()))
            });
            let before = {
                let mut changed = fixture.clone();
                change(&mut changed);
                serde_json::to_vec(&changed).unwrap()
            };
            let error = save(&fixture, "192.0.2.40", &deps).unwrap_err();
            assert_eq!((error.code, error.message.as_str()), (4, CHANGED));
            assert_eq!(raw_config(&home), before);
            assert!(!home.path.join("lan-route.json").exists());
        }
        // A host list edited meanwhile is kept, with the address first.
        write_config(&home, &fixture);
        let path = home.path.join("config.json");
        let (mut deps, _) = Fake::new().build();
        deps.verify = Box::new(move |checked, _| {
            let mut changed = checked.clone();
            changed.hosts.push("192.0.2.21".into());
            state::atomic_json(&path, &changed).unwrap();
            Ok((Some(0), Vec::new(), Vec::new()))
        });
        assert_eq!(
            save(&fixture, "192.0.2.40", &deps).unwrap()["hosts"],
            json!(["192.0.2.40", "198.51.100.20", "192.0.2.21"])
        );
    }
    #[test]
    fn save_holds_the_config_lock_while_it_writes() {
        let home = state::TestHome::new();
        let fixture = config(&["192.0.2.20"]);
        write_config(&home, &fixture);
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(home.path.join("config.lock"))
            .unwrap();
        fs2::FileExt::lock_exclusive(&lock).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = {
            let fixture = fixture.clone();
            thread::spawn(move || {
                let (deps, _) = Fake::new().reply("192.0.2.40", 0, "").build();
                let result = save(&fixture, "192.0.2.40", &deps).map(|out| out["hosts"].clone());
                let _ = sender.send(());
                result
            })
        };
        assert!(receiver.recv_timeout(Duration::from_millis(400)).is_err());
        assert_eq!(saved(&home).hosts, ["192.0.2.20"]);
        fs2::FileExt::unlock(&lock).unwrap();
        assert_eq!(
            worker.join().unwrap().unwrap(),
            json!(["192.0.2.40", "192.0.2.20"])
        );
        assert_eq!(saved(&home).hosts, ["192.0.2.40", "192.0.2.20"]);
    }
    #[test]
    fn save_stores_the_canonical_address() {
        let home = state::TestHome::new();
        let fixture = config(&["192.0.2.20"]);
        write_config(&home, &fixture);
        // Whatever forms the parser accepts are stored as Ipv4Addr prints them.
        for written in ["192.0.2.040", "192.0.2.40"] {
            let (deps, _) = Fake::new().reply("192.0.2.40", 0, "").build();
            match save(&fixture, written, &deps) {
                Ok(out) => assert_eq!(out["host"], "192.0.2.40"),
                Err(error) => assert_eq!((error.code, written), (2, "192.0.2.040")),
            }
        }
        assert_eq!(saved(&home).hosts[0], "192.0.2.40");
    }
}
