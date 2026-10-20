use crate::{
    AppError, Result,
    state::{self, Config},
};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs},
    path::PathBuf,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

pub struct Transport {
    pub config: Config,
    pub selected: String,
    pub reason: String,
    pub host: Option<String>,
    pub authenticated: bool,
    attempts: Vec<Value>,
    /// The newest remembered address while its entry is fresh (verified within
    /// a day), so a success there does not rewrite lan-route.json on every
    /// request or process. A stale head is rewritten, refreshing verified_at.
    remembered_head: Option<String>,
}

/// Where a LAN candidate came from. Every source is dialed through the same
/// pinned `command()`; the source only orders attempts and labels receipts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Sticky,
    Remembered,
    Configured,
    Alias,
}
impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Self::Sticky => "sticky",
            Self::Remembered => "remembered",
            Self::Configured => "configured",
            Self::Alias => "alias",
        }
    }
}
/// `host: None` leaves address resolution (including any proxy) to OpenSSH.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub source: Source,
    pub host: Option<String>,
}
pub const MAX_REMEMBERED: usize = 4;
pub const MAX_CANDIDATES: usize = 8;
pub const PRECHECK_CONNECT: Duration = Duration::from_millis(1500);
pub const PRECHECK_DEADLINE: Duration = Duration::from_secs(2);
const PRECHECK_REASON: &str = "No TCP answer on the SSH port within 1.5s";

fn add_candidate(plan: &mut Vec<Candidate>, source: Source, host: &str) {
    if !plan.iter().any(|c| {
        c.host
            .as_deref()
            .is_some_and(|h| h.eq_ignore_ascii_case(host))
    }) {
        plan.push(Candidate {
            source,
            host: Some(host.into()),
        });
    }
}
/// Order LAN candidates: a sticky address from this process alone; otherwise
/// remembered verified addresses, the configured hosts, then the SSH alias.
/// Without configured hosts the plan stays exactly the alias, as before.
pub fn plan_candidates(
    sticky: Option<&str>,
    remembered: &[String],
    configured: &[String],
    alias_hostname: Option<&str>,
) -> Vec<Candidate> {
    if let Some(host) = sticky {
        return vec![Candidate {
            source: Source::Sticky,
            host: Some(host.into()),
        }];
    }
    let alias = Candidate {
        source: Source::Alias,
        host: None,
    };
    if configured.is_empty() {
        return vec![alias];
    }
    let mut plan = Vec::new();
    for host in remembered.iter().take(MAX_REMEMBERED) {
        add_candidate(&mut plan, Source::Remembered, host);
    }
    for host in configured {
        add_candidate(&mut plan, Source::Configured, host);
    }
    match alias_hostname {
        Some(host) => add_candidate(&mut plan, Source::Alias, host),
        None => plan.push(alias),
    }
    plan.truncate(MAX_CANDIDATES);
    plan
}
/// Extract the alias's configured HostName from `ssh -G` output. A configured
/// ProxyJump or ProxyCommand means the address is not directly dialable, and
/// a hostname that merely echoes the alias means no HostName is configured;
/// either way the alias stays with OpenSSH (None) and is not prechecked.
pub fn parse_ssh_g(output: &str, alias: &str) -> Option<String> {
    let mut hostname = None;
    for line in output.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(value) = line
            .strip_prefix("proxyjump ")
            .or_else(|| line.strip_prefix("proxycommand "))
        {
            if !value.trim().eq_ignore_ascii_case("none") {
                return None;
            }
        } else if let Some(value) = line.strip_prefix("hostname ") {
            hostname.get_or_insert(value.trim());
        }
    }
    hostname
        .filter(|host| state::validate_host(host).is_ok() && !host.eq_ignore_ascii_case(alias))
        .map(str::to_owned)
}
/// `ssh -G` only evaluates local OpenSSH configuration; it opens no network
/// connection and never touches known_hosts.
pub fn alias_lookup_command(config: &Config) -> Command {
    let mut command = Command::new("ssh");
    command.arg("-G").arg("-p").arg(config.port.to_string());
    if let Some(user) = &config.user {
        command.arg("-l").arg(user);
    }
    command.arg(&config.ssh_alias);
    command
}
/// The alias's own HostName, or None when it is unavailable or proxied. The
/// `-G` output may describe private configuration and is never logged.
pub fn alias_hostname(config: &Config) -> Option<String> {
    let (success, out, _) =
        process(alias_lookup_command(config), &[], Duration::from_secs(3)).ok()?;
    if !success {
        return None;
    }
    parse_ssh_g(&String::from_utf8_lossy(&out), &config.ssh_alias)
}
fn resolve(host: &str, port: u16, deadline: Instant) -> Option<Vec<SocketAddr>> {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return Some(vec![SocketAddr::new(ip, port)]);
    }
    // getaddrinfo has no timeout. A detached helper performs the lookup so the
    // scoped precheck still ends at its deadline; a late answer is discarded.
    let (sender, receiver) = mpsc::channel();
    let name = bare.to_owned();
    thread::Builder::new()
        .spawn(move || {
            let addresses = (name.as_str(), port)
                .to_socket_addrs()
                .map(|found| found.collect::<Vec<_>>());
            let _ = sender.send(addresses);
        })
        .ok()?;
    receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()?
        .ok()
}
fn port_answers(host: &str, port: u16, per_connect: Duration, deadline: Instant) -> bool {
    for address in resolve(host, port, deadline).unwrap_or_default() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        if TcpStream::connect_timeout(&address, per_connect.min(remaining)).is_ok() {
            return true;
        }
    }
    false
}
/// Parallel TCP reachability check of each concrete address on the SSH port,
/// all started at once and bounded by one deadline. `consume` receives the
/// results in candidate order and each is awaited only when it is pulled, so
/// a live early candidate is not held up by a dead later one. `None` marks a
/// candidate without an address (dialed without precheck), otherwise
/// `Some(answered)`. An answering port is not authentication: it is still
/// dialed through strict pinned SSH.
pub fn with_precheck<T>(
    hosts: &[Option<&str>],
    port: u16,
    per_connect: Duration,
    deadline: Duration,
    consume: impl FnOnce(&mut dyn Iterator<Item = Option<bool>>) -> T,
) -> T {
    let deadline = Instant::now() + deadline;
    thread::scope(|scope| {
        let checks: Vec<_> = hosts
            .iter()
            .map(|host| {
                host.map(|host| {
                    scope.spawn(move || port_answers(host, port, per_connect, deadline))
                })
            })
            .collect();
        consume(
            &mut checks
                .into_iter()
                .map(|check| check.map(|handle| handle.join().unwrap_or(false))),
        )
    })
}
pub fn precheck(
    hosts: &[Option<&str>],
    port: u16,
    per_connect: Duration,
    deadline: Duration,
) -> Vec<Option<bool>> {
    with_precheck(hosts, port, per_connect, deadline, |results| {
        results.collect()
    })
}
fn ssh_failure(message: &str) -> &'static str {
    if [
        "host key verification failed",
        "remote host identification has changed",
        "permission denied",
        "bad permissions",
        "no matching host key",
    ]
    .iter()
    .any(|s| message.contains(s))
    {
        "authentication_failed"
    } else if [
        "connection timed out",
        "connection refused",
        "no route to host",
        "network is unreachable",
        "could not resolve hostname",
    ]
    .iter()
    .any(|s| message.contains(s))
    {
        "host_unreachable"
    } else {
        "ssh_command_failed"
    }
}
pub fn quiet(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    #[cfg(not(windows))]
    let _ = command;
}
pub fn process(
    mut command: Command,
    input: &[u8],
    timeout: Duration,
) -> Result<(bool, Vec<u8>, Vec<u8>)> {
    quiet(&mut command);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|_| AppError::new(4, "Transport process could not be started"))?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let bytes = input.to_vec();
    let writer = thread::spawn(move || stdin.write_all(&bytes));
    let reader = thread::spawn(move || {
        let mut b = Vec::new();
        let _ = stdout.by_ref().take(8 * 1024 * 1024).read_to_end(&mut b);
        b
    });
    let errors = thread::spawn(move || {
        let mut b = Vec::new();
        let _ = stderr.by_ref().take(64 * 1024).read_to_end(&mut b);
        b
    });
    let started = Instant::now();
    let success = loop {
        if let Some(status) = child.try_wait()? {
            break status.success();
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AppError::new(
                4,
                "Transport timeout; request may have completed remotely. Retry using the same request ID.",
            ));
        }
        thread::sleep(Duration::from_millis(25));
    };
    let _ = writer.join();
    Ok((
        success,
        reader.join().unwrap_or_default(),
        errors.join().unwrap_or_default(),
    ))
}
pub fn sidecar() -> Result<PathBuf> {
    let sibling = std::env::current_exe()?.with_file_name(if cfg!(windows) {
        "sparkdev-link.exe"
    } else {
        "sparkdev-link"
    });
    if !sibling.is_file() {
        return Err(AppError::new(
            3,
            "sparkdev-link sidecar is not installed beside sparkdev",
        ));
    }
    Ok(sibling)
}
impl Transport {
    pub fn new() -> Result<Self> {
        Ok(Self::from_config(state::config()?))
    }
    pub fn from_config(config: Config) -> Self {
        Self {
            config,
            selected: "unselected".into(),
            reason: "No authenticated request yet".into(),
            host: None,
            authenticated: false,
            attempts: Vec::new(),
            remembered_head: None,
        }
    }
    fn push_attempt(&mut self, attempt: Value) {
        if self.attempts.len() >= 32 {
            self.attempts.remove(0);
        }
        self.attempts.push(attempt);
    }
    fn record(
        &mut self,
        route: &str,
        attempted: bool,
        status: &str,
        reason: &str,
        target: Option<usize>,
    ) {
        self.push_attempt(json!({"route":route,"attempted":attempted,"status":status,"reason":reason,"target_index":target}));
    }
    /// LAN candidate receipt. `target_index` still indexes config.hosts and is
    /// null for sticky, remembered and alias candidates.
    fn record_candidate(&mut self, candidate: &Candidate, status: &str, reason: &str) {
        let target = match (candidate.source, &candidate.host) {
            (Source::Configured, Some(host)) => self.config.hosts.iter().position(|h| h == host),
            _ => None,
        };
        self.push_attempt(json!({"route":"lan_ssh","attempted":true,"status":status,"reason":reason,"target_index":target,"source":candidate.source.name(),"host":candidate.host}));
    }
    pub fn command(&self, host: Option<&str>, remote: &str) -> Command {
        let mut command = Command::new("ssh");
        command.args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "ConnectionAttempts=1",
            "-o",
            "ServerAliveInterval=5",
            "-o",
            "ServerAliveCountMax=2",
            "-o",
            "PasswordAuthentication=no",
            "-o",
            "KbdInteractiveAuthentication=no",
            "-o",
            "ForwardAgent=no",
            "-o",
            "ClearAllForwardings=yes",
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
        ]);
        command
            .arg("-o")
            .arg(format!("HostKeyAlias={}", self.config.host_key_alias));
        if let Some(host) = host {
            command.arg("-o").arg(format!("HostName={host}"));
        }
        if let Some(key) = &self.config.identity_file {
            command
                .arg("-i")
                .arg(key)
                .args(["-o", "IdentitiesOnly=yes"]);
        }
        if let Some(user) = &self.config.user {
            command.arg("-l").arg(user);
        }
        command
            .arg("-p")
            .arg(self.config.port.to_string())
            .arg(&self.config.ssh_alias)
            .arg(remote);
        command
    }
    fn decode(bytes: &[u8]) -> Result<Value> {
        let value: Value = serde_json::from_slice(bytes)
            .map_err(|_| AppError::new(6, "Remote returned an invalid JSON response"))?;
        if value["ok"] == false {
            let error = value
                .get("error")
                .filter(|v| v.is_object())
                .unwrap_or(&value);
            let code = error["code"].as_str().unwrap_or("REMOTE_ERROR");
            let message = error["message"]
                .as_str()
                .unwrap_or("Remote operation failed");
            return Err(AppError::new(6, format!("{code}: {message}")));
        }
        if value["ok"] != true {
            return Err(AppError::new(
                6,
                "Remote response omitted its protocol success flag",
            ));
        }
        Ok(value)
    }
    pub fn ssh_raw(&mut self, remote: &str, input: &[u8], timeout: Duration) -> Result<Vec<u8>> {
        self.authenticated = false;
        self.check_enabled()?;
        if !self.config.allows_lan() {
            self.record(
                "lan_ssh",
                false,
                "disabled",
                "Remote-only mode does not permit SSH",
                None,
            );
            return Err(AppError::new(
                4,
                "Remote-only mode does not permit SSH; choose LAN-only or Both for initial setup",
            ));
        }
        let plan = self.lan_plan(alias_hostname);
        let hosts: Vec<Option<&str>> = plan.iter().map(|c| c.host.as_deref()).collect();
        let port = self.config.port;
        with_precheck(&hosts, port, PRECHECK_CONNECT, PRECHECK_DEADLINE, |open| {
            self.dial(&plan, open, remote, |command| {
                process(command, input, timeout)
            })
        })
    }
    /// The candidate plan for the next LAN request. `alias_lookup` reads the
    /// alias's HostName (a local `ssh -G`) and is only called when needed.
    fn lan_plan(&mut self, alias_lookup: impl FnOnce(&Config) -> Option<String>) -> Vec<Candidate> {
        match self.host.clone() {
            Some(sticky) => plan_candidates(Some(&sticky), &[], &[], None),
            None if self.config.hosts.is_empty() => plan_candidates(None, &[], &[], None),
            None => {
                let remembered = state::remembered_lan(&self.config);
                self.remembered_head = remembered.fresh_head;
                let alias = alias_lookup(&self.config);
                plan_candidates(
                    None,
                    &remembered.hosts,
                    &self.config.hosts,
                    alias.as_deref(),
                )
            }
        }
    }
    /// Dial prechecked candidates in planned order through the pinned
    /// `command()`. Only reachability failures move to the next candidate;
    /// authentication, host-key, service and process failures stop at once.
    /// `open` is pulled lazily, one result per candidate reached.
    fn dial(
        &mut self,
        plan: &[Candidate],
        open: impl IntoIterator<Item = Option<bool>>,
        remote: &str,
        mut run: impl FnMut(Command) -> Result<(bool, Vec<u8>, Vec<u8>)>,
    ) -> Result<Vec<u8>> {
        for (candidate, open) in plan.iter().zip(open) {
            let host = candidate.host.as_deref();
            if open == Some(false) {
                self.record_candidate(candidate, "host_unreachable", PRECHECK_REASON);
                continue;
            }
            let (success, out, err) = match run(self.command(host, remote)) {
                Ok(r) => r,
                Err(e) => {
                    let status = if e.message.starts_with("Transport timeout") {
                        "transport_timeout"
                    } else {
                        "process_error"
                    };
                    self.record_candidate(candidate, status, &e.message);
                    return Err(e);
                }
            };
            if success {
                self.selected = "ssh".into();
                self.reason = "LAN SSH authenticated with the configured known-host pin".into();
                self.host = candidate.host.clone();
                self.authenticated = true;
                self.record_candidate(
                    candidate,
                    "authenticated",
                    "Strict SSH host pin and key authentication succeeded",
                );
                if let Some(host) = host {
                    if self.remembered_head.as_deref() != Some(host) {
                        // Best effort: a failed write never fails the request.
                        let _ = state::remember_host(&self.config, host);
                        self.remembered_head = Some(host.into());
                    }
                }
                return Ok(out);
            }
            let message = String::from_utf8_lossy(&err).to_lowercase();
            let failure = ssh_failure(&message);
            self.record_candidate(candidate, failure, match failure {"authentication_failed"=>"SSH authentication or host pin rejected; no fallback", "host_unreachable"=>"SSH reported connection refusal, timeout, unreachable network or DNS failure", _=>"SSH command failed without evidence that the LAN host is unreachable"});
            if failure == "authentication_failed" {
                if let (Source::Remembered, Some(host)) = (candidate.source, host) {
                    let _ = state::forget_host(&self.config, host);
                    if self.remembered_head.as_deref() == Some(host) {
                        self.remembered_head = None;
                    }
                }
                return Err(AppError::new(
                    7,
                    "SSH authentication or pinned host-key verification failed; fallback refused",
                ));
            }
            if failure != "host_unreachable" {
                return Err(AppError::new(
                    4,
                    "SSH command failed; inspect the configured service and key (stderr withheld)",
                ));
            }
        }
        Err(AppError::new(4, "Configured LAN addresses are unreachable"))
    }
    pub fn rpc(&mut self, request: Value) -> Result<Value> {
        self.authenticated = false;
        self.check_enabled()?;
        let input = serde_json::to_vec(&request)?;
        let timeout = Duration::from_secs(request["timeout"].as_u64().unwrap_or(0).min(60) + 30);
        if self.config.allows_lan() && (self.selected != "iroh" || !self.config.allows_remote()) {
            match self.ssh_raw("python3 ~/sparkdev/controller.py rpc", &input, timeout) {
                Ok(bytes) => return Self::decode(&bytes),
                Err(e) => {
                    if e.message == "Configured LAN addresses are unreachable"
                        && !self.config.allows_remote()
                    {
                        self.record(
                            "paired_iroh",
                            false,
                            "disabled",
                            "Remote transport is disabled in local configuration",
                            None,
                        );
                    }
                    if !self.can_fallback(&e) {
                        return Err(e);
                    }
                }
            }
        } else if !self.config.allows_lan() {
            self.record(
                "lan_ssh",
                false,
                "disabled",
                "Remote-only mode skips SSH",
                None,
            );
        }
        if !self.config.allows_remote() {
            return Err(AppError::new(
                4,
                "LAN-only mode does not permit the paired remote transport",
            ));
        }
        let payload = serde_json::to_vec(&json!({"request":request}))?;
        for attempt in 0..3 {
            let sidecar = match sidecar() {
                Ok(path) => path,
                Err(e) => {
                    self.record(
                        "paired_iroh",
                        false,
                        "unconfigured",
                        "Remote adapter is not installed",
                        None,
                    );
                    return Err(e);
                }
            };
            let mut command = Command::new(sidecar);
            command.arg("rpc");
            let (success, out, _) = match process(command, &payload, timeout) {
                Ok(result) => result,
                Err(e) => {
                    self.record("paired_iroh", true, "transport_error", &e.message, None);
                    return Err(e);
                }
            };
            if !success {
                let error = serde_json::from_slice::<Value>(&out).unwrap_or(Value::Null);
                let status = if error["error"]["code"] == "link_busy" {
                    "busy"
                } else if error["error"]["message"] == "this computer is not paired" {
                    "unconfigured"
                } else {
                    "remote_unavailable"
                };
                self.record(
                    "paired_iroh",
                    true,
                    status,
                    match status {
                        "busy" => "Paired endpoint is in use by another local request",
                        "unconfigured" => "This computer has no paired Spark endpoint",
                        _ => "Remote adapter reported failure; physical network path is unknown",
                    },
                    None,
                );
                if error["error"]["code"] == "link_busy" && attempt < 2 {
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
                return Err(AppError::new(
                    4,
                    "Authenticated remote sidecar unavailable, busy or not paired; retry the same request ID",
                ));
            }
            self.selected = "iroh".into();
            self.reason = if self.config.allows_lan() { "LAN unreachable; paired endpoint transport explicitly enabled" } else { "Remote-only mode; paired endpoint authenticated (direct or relay path is not reported)" }.into();
            self.authenticated = true;
            self.record("paired_iroh", true, "authenticated", "Paired endpoint authenticated; adapter does not report whether its network path is direct or relayed", None);
            return Self::decode(&out);
        }
        Err(AppError::new(4, "Authenticated remote link is busy"))
    }
    pub fn info(&self) -> Value {
        let route = if !self.authenticated {
            "none"
        } else if self.selected == "ssh" {
            "lan_ssh"
        } else {
            "paired_iroh"
        };
        let mut info = json!({"selected":self.selected,"route":route,"authenticated":self.authenticated,"reason":self.reason,"attempted_routes":self.attempts,"connection":self.config.connection_info()});
        if let (Some(host), "lan_ssh") = (&self.host, route) {
            info["lan_host"] = json!(host);
        }
        info
    }
    fn check_enabled(&mut self) -> Result<()> {
        if !self.config.connection_enabled {
            self.selected = "disconnected".into();
            self.reason =
                "Disconnected by owner; existing jobs continue and credentials are retained".into();
            self.record(
                "none",
                false,
                "disconnected",
                "New requests are disabled by the owner",
                None,
            );
            return Err(AppError::new(
                4,
                "Bridge is disconnected; use sparkdev connect to enable new requests",
            ));
        }
        Ok(())
    }
    fn can_fallback(&self, error: &AppError) -> bool {
        self.config.connection_enabled
            && self.config.allows_remote()
            && error.code == 4
            && error.message == "Configured LAN addresses are unreachable"
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsStr, net::TcpListener};
    fn configured(mode: &str, enabled: bool) -> Transport {
        Transport::from_config(serde_json::from_value(json!({"ssh_alias":"unreachable-test-fixture","hosts":[],"host_key_alias":"test-pin","connection_mode":mode,"connection_enabled":enabled})).unwrap())
    }
    fn fixture(hosts: &[&str], mode: &str) -> Transport {
        Transport::from_config(serde_json::from_value(json!({"ssh_alias":"unreachable-test-fixture","hosts":hosts,"host_key_alias":"test-pin","user":"owner","connection_mode":mode})).unwrap())
    }
    fn candidate(source: Source, host: Option<&str>) -> Candidate {
        Candidate {
            source,
            host: host.map(str::to_owned),
        }
    }
    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }
    fn arguments(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }
    fn hostname_of(command: &Command) -> Option<String> {
        arguments(command)
            .iter()
            .find_map(|a| a.strip_prefix("HostName=").map(str::to_owned))
    }
    fn assert_pinned(command: &Command, pin: &str) {
        assert_eq!(command.get_program(), OsStr::new("ssh"));
        let args = arguments(command);
        let pin = format!("HostKeyAlias={pin}");
        for required in [
            "BatchMode=yes",
            "StrictHostKeyChecking=yes",
            "PasswordAuthentication=no",
            pin.as_str(),
        ] {
            assert!(
                args.windows(2).any(|w| w[0] == "-o" && w[1] == required),
                "{required} missing from {args:?}"
            );
        }
        for forbidden in [
            "accept-new",
            "stricthostkeychecking=no",
            "userknownhostsfile",
        ] {
            assert!(
                !args
                    .iter()
                    .any(|a| a.to_ascii_lowercase().contains(forbidden)),
                "{forbidden} present in {args:?}"
            );
        }
    }
    #[test]
    fn plan_orders_remembered_configured_then_alias_with_dedup_and_caps() {
        let configured = strings(&["192.0.2.1", "192.0.2.2"]);
        assert_eq!(
            plan_candidates(
                None,
                &strings(&["198.51.100.9", "198.51.100.8"]),
                &configured,
                Some("192.0.2.3")
            ),
            [
                candidate(Source::Remembered, Some("198.51.100.9")),
                candidate(Source::Remembered, Some("198.51.100.8")),
                candidate(Source::Configured, Some("192.0.2.1")),
                candidate(Source::Configured, Some("192.0.2.2")),
                candidate(Source::Alias, Some("192.0.2.3")),
            ]
        );
        // A remembered configured address keeps its earlier remembered slot,
        // and an alias HostName already planned is not dialed twice.
        assert_eq!(
            plan_candidates(
                None,
                &strings(&["192.0.2.2"]),
                &configured,
                Some("192.0.2.1")
            ),
            [
                candidate(Source::Remembered, Some("192.0.2.2")),
                candidate(Source::Configured, Some("192.0.2.1")),
            ]
        );
        assert_eq!(
            plan_candidates(
                None,
                &[],
                &strings(&["Spark.Local", "spark.local"]),
                Some("SPARK.LOCAL")
            ),
            [candidate(Source::Configured, Some("Spark.Local"))]
        );
        // An unresolvable or proxied alias is still dialed last through OpenSSH.
        assert_eq!(
            plan_candidates(None, &[], &configured, None),
            [
                candidate(Source::Configured, Some("192.0.2.1")),
                candidate(Source::Configured, Some("192.0.2.2")),
                candidate(Source::Alias, None),
            ]
        );
        let remembered = strings(&[
            "198.51.100.1",
            "198.51.100.2",
            "198.51.100.3",
            "198.51.100.4",
            "198.51.100.5",
            "198.51.100.6",
        ]);
        let only_remembered = plan_candidates(None, &remembered, &strings(&["192.0.2.1"]), None);
        assert_eq!(
            only_remembered
                .iter()
                .filter(|c| c.source == Source::Remembered)
                .count(),
            MAX_REMEMBERED
        );
        assert_eq!(only_remembered.len(), MAX_REMEMBERED + 2);
        let many = strings(&[
            "192.0.2.11",
            "192.0.2.12",
            "192.0.2.13",
            "192.0.2.14",
            "192.0.2.15",
            "192.0.2.16",
        ]);
        let capped = plan_candidates(None, &remembered, &many, Some("192.0.2.3"));
        assert_eq!(capped.len(), MAX_CANDIDATES);
        assert_eq!(
            capped[0],
            candidate(Source::Remembered, Some("198.51.100.1"))
        );
        assert_eq!(capped[7], candidate(Source::Configured, Some("192.0.2.14")));
    }
    #[test]
    fn plan_sticky_is_exclusive_and_empty_hosts_are_unchanged() {
        assert_eq!(
            plan_candidates(
                Some("192.0.2.7"),
                &strings(&["198.51.100.1"]),
                &strings(&["192.0.2.1"]),
                Some("192.0.2.3")
            ),
            [candidate(Source::Sticky, Some("192.0.2.7"))]
        );
        for alias in [None, Some("192.0.2.3")] {
            assert_eq!(
                plan_candidates(None, &strings(&["198.51.100.1"]), &[], alias),
                [candidate(Source::Alias, None)]
            );
        }
    }
    #[test]
    fn ssh_g_parser_reads_only_hostname_and_refuses_proxies() {
        let plain =
            "user owner\nhostname 192.168.1.186\nport 22\nproxycommand none\nhostkeyalias pin\n";
        let alias = "my-spark";
        assert_eq!(parse_ssh_g(plain, alias).as_deref(), Some("192.168.1.186"));
        assert_eq!(
            parse_ssh_g(&plain.replace('\n', "\r\n"), alias).as_deref(),
            Some("192.168.1.186")
        );
        assert_eq!(
            parse_ssh_g(
                "proxyjump none\nhostname spark.local\nhostname 192.0.2.9\n",
                alias
            )
            .as_deref(),
            Some("spark.local")
        );
        for refused in [
            "hostname 192.0.2.5\nproxyjump bastion\n",
            "proxycommand ssh -W %h:%p bastion\r\nhostname 192.0.2.5\r\n",
            "hostname 192.0.2.5\nproxyjump \n",
            "hostname -oProxyCommand=x\n",
            "hostname spark;id\n",
            "hostname \n",
            "hostnames 192.0.2.6\nxhostname 192.0.2.6\n",
            // No HostName configured: -G echoes the (lowercased) alias.
            "hostname my-spark\nport 22\n",
            "hostname MY-SPARK\n",
            "",
        ] {
            assert_eq!(parse_ssh_g(refused, alias), None, "{refused:?}");
        }
    }
    #[test]
    fn every_ssh_dial_keeps_the_strict_pin() {
        let mut tr = fixture(&["192.0.2.1"], "both");
        tr.config.identity_file = Some(std::env::temp_dir().join("fixture-key"));
        for host in [Some("192.0.2.1"), Some("spark.local"), None] {
            let command = tr.command(host, "true");
            assert_pinned(&command, "test-pin");
            assert_eq!(hostname_of(&command).as_deref(), host);
            let args = arguments(&command);
            assert_eq!(
                &args[args.len() - 4..],
                ["-p", "22", "unreachable-test-fixture", "true"]
            );
            assert!(args.windows(2).any(|w| w == ["-l", "owner"]));
        }
        let mut lookup = fixture(&["192.0.2.1"], "both").config;
        lookup.port = 2222;
        let command = alias_lookup_command(&lookup);
        assert_eq!(command.get_program(), OsStr::new("ssh"));
        assert_eq!(
            arguments(&command),
            [
                "-G",
                "-p",
                "2222",
                "-l",
                "owner",
                "unreachable-test-fixture"
            ]
        );
    }
    #[test]
    fn precheck_separates_open_and_closed_ports_within_the_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let open_port = listener.local_addr().unwrap().port();
        let closed_port = {
            let probe = TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let slack = Duration::from_millis(750);
        let started = Instant::now();
        assert_eq!(
            precheck(
                &[Some("127.0.0.1"), None, Some("[127.0.0.1]")],
                open_port,
                PRECHECK_CONNECT,
                PRECHECK_DEADLINE
            ),
            [Some(true), None, Some(true)]
        );
        assert!(started.elapsed() < PRECHECK_DEADLINE + slack);
        let started = Instant::now();
        assert_eq!(
            precheck(
                &[
                    Some("127.0.0.1"),
                    Some("127.0.0.1"),
                    Some("127.0.0.1"),
                    None
                ],
                closed_port,
                PRECHECK_CONNECT,
                PRECHECK_DEADLINE
            ),
            [Some(false), Some(false), Some(false), None]
        );
        // Three closed checks in parallel finish within one deadline.
        assert!(started.elapsed() < PRECHECK_DEADLINE + slack);
        assert!(precheck(&[], open_port, PRECHECK_CONNECT, PRECHECK_DEADLINE).is_empty());
        drop(listener);
    }
    fn refused() -> Result<(bool, Vec<u8>, Vec<u8>)> {
        Ok((
            false,
            Vec::new(),
            b"ssh: connect to host 192.0.2.1 port 22: Connection refused\r\n".to_vec(),
        ))
    }
    #[test]
    fn dial_skips_closed_ports_continues_on_unreachable_and_remembers_winner() {
        let _home = state::TestHome::new();
        let mut tr = fixture(&["192.0.2.1", "192.0.2.2"], "both");
        let plan = plan_candidates(
            None,
            &strings(&["198.51.100.7"]),
            &tr.config.hosts.clone(),
            Some("192.0.2.3"),
        );
        let open = [Some(false), Some(true), Some(true), Some(true)];
        let mut dialed = Vec::new();
        let out = tr
            .dial(&plan, open, "true", |command| {
                assert_pinned(&command, "test-pin");
                let host = hostname_of(&command);
                dialed.push(host.clone());
                match host.as_deref() {
                    Some("192.0.2.1") => refused(),
                    _ => Ok((true, b"ok".to_vec(), Vec::new())),
                }
            })
            .unwrap();
        assert_eq!(out, b"ok");
        assert_eq!(
            dialed,
            [Some("192.0.2.1".to_owned()), Some("192.0.2.2".to_owned())]
        );
        let info = tr.info();
        let attempts = info["attempted_routes"].as_array().unwrap();
        assert_eq!(attempts.len(), 3);
        assert_eq!(
            attempts[0],
            json!({"route":"lan_ssh","attempted":true,"status":"host_unreachable","reason":PRECHECK_REASON,"target_index":null,"source":"remembered","host":"198.51.100.7"})
        );
        assert_eq!(attempts[1]["status"], "host_unreachable");
        assert_eq!(attempts[1]["source"], "configured");
        assert_eq!(attempts[1]["target_index"], 0);
        assert_eq!(attempts[2]["status"], "authenticated");
        assert_eq!(attempts[2]["host"], "192.0.2.2");
        assert_eq!(attempts[2]["target_index"], 1);
        assert_eq!(info["route"], "lan_ssh");
        assert_eq!(info["lan_host"], "192.0.2.2");
        assert_eq!(state::remembered_hosts(&tr.config), ["192.0.2.2"]);
        // The sticky winner is already the newest remembered entry: no rewrite.
        let route_file = state::root().unwrap().join("lan-route.json");
        std::fs::remove_file(&route_file).unwrap();
        let sticky = plan_candidates(tr.host.as_deref(), &[], &[], None);
        assert_eq!(sticky, [candidate(Source::Sticky, Some("192.0.2.2"))]);
        tr.dial(&sticky, [Some(true)], "true", |_| {
            Ok((true, Vec::new(), Vec::new()))
        })
        .unwrap();
        assert!(!route_file.exists());
        assert_eq!(tr.info()["attempted_routes"][3]["source"], "sticky");
        assert_eq!(
            tr.info()["attempted_routes"][3]["target_index"],
            Value::Null
        );
        // An alias dialed through OpenSSH has no concrete address to report.
        let mut alias = fixture(&[], "lan-only");
        alias
            .dial(
                &[candidate(Source::Alias, None)],
                [None],
                "true",
                |command| {
                    assert_eq!(hostname_of(&command), None);
                    Ok((true, Vec::new(), Vec::new()))
                },
            )
            .unwrap();
        assert_eq!(alias.info()["route"], "lan_ssh");
        assert!(alias.info().get("lan_host").is_none());
        assert_eq!(alias.info()["attempted_routes"][0]["host"], Value::Null);
        assert!(!route_file.exists());
    }
    #[test]
    fn a_stale_remembered_head_is_refreshed_and_a_fresh_one_is_left_alone() {
        let home = state::TestHome::new();
        let hosts = ["192.0.2.1", "192.0.2.2"];
        let binding = state::lan_binding(&fixture(&hosts, "both").config);
        let file = home.path.join("lan-route.json");
        let verified_at = || {
            state::read_json(&file).unwrap()["entries"][0]["verified_at"]
                .as_u64()
                .unwrap()
        };
        let two_days_ago = state::now() - 2 * 24 * 60 * 60;
        std::fs::write(
            &file,
            serde_json::to_vec(&json!({"v":1,"entries":[{"binding":binding,"host":"192.0.2.2","verified_at":two_days_ago}]})).unwrap(),
        )
        .unwrap();
        // Each process reads the file again; the head winning every day must
        // still be rewritten so it does not expire 30 days after first use.
        let mut tr = fixture(&hosts, "both");
        let plan = tr.lan_plan(|_| None);
        assert_eq!(plan[0], candidate(Source::Remembered, Some("192.0.2.2")));
        tr.dial(&plan, [Some(true)], "true", |_| {
            Ok((true, Vec::new(), Vec::new()))
        })
        .unwrap();
        let refreshed = verified_at();
        assert!(refreshed >= state::now() - 60, "{refreshed}");
        // A fresh head in the next process is not rewritten.
        let before = std::fs::read(&file).unwrap();
        let mut next = fixture(&hosts, "both");
        let plan = next.lan_plan(|_| None);
        next.dial(&plan, [Some(true)], "true", |_| {
            Ok((true, Vec::new(), Vec::new()))
        })
        .unwrap();
        assert_eq!(next.info()["lan_host"], "192.0.2.2");
        assert_eq!(std::fs::read(&file).unwrap(), before);
    }
    #[test]
    fn dial_failures_keep_fail_closed_semantics() {
        let _home = state::TestHome::new();
        let hosts = ["192.0.2.1", "192.0.2.2"];
        // A remembered address that fails the pin is forgotten; no fallback.
        let mut tr = fixture(&hosts, "both");
        state::remember_host(&tr.config, "198.51.100.8").unwrap();
        state::remember_host(&tr.config, "198.51.100.9").unwrap();
        let remembered = state::remembered_hosts(&tr.config);
        let plan = plan_candidates(None, &remembered, &tr.config.hosts.clone(), None);
        let open = [Some(true), Some(true), Some(true), Some(true), None];
        let mut dialed = 0;
        let error = tr
            .dial(&plan, open, "true", |_| {
                dialed += 1;
                Ok((
                    false,
                    Vec::new(),
                    b"Host key verification failed.\r\n".to_vec(),
                ))
            })
            .unwrap_err();
        assert_eq!((error.code, dialed), (7, 1));
        assert!(!tr.can_fallback(&error));
        assert!(!tr.authenticated);
        assert_eq!(tr.info()["route"], "none");
        assert_eq!(
            tr.info()["attempted_routes"][0]["status"],
            "authentication_failed"
        );
        assert_eq!(state::remembered_hosts(&tr.config), ["198.51.100.8"]);
        // A configured address failing authentication fails closed but does
        // not erase remembered addresses.
        let mut tr = fixture(&hosts, "both");
        let plan = plan_candidates(
            None,
            &strings(&["198.51.100.8"]),
            &tr.config.hosts.clone(),
            None,
        );
        let error = tr
            .dial(
                &plan,
                [Some(false), Some(true), Some(true), None],
                "true",
                |_| {
                    Ok((
                        false,
                        Vec::new(),
                        b"owner@192.0.2.1: Permission denied (publickey).".to_vec(),
                    ))
                },
            )
            .unwrap_err();
        assert_eq!(error.code, 7);
        assert_eq!(tr.info()["attempted_routes"].as_array().unwrap().len(), 2);
        assert_eq!(state::remembered_hosts(&tr.config), ["198.51.100.8"]);
        // A service failure on an answering host stops without fallback.
        let mut tr = fixture(&hosts, "both");
        let mut dialed = 0;
        let error = tr
            .dial(
                &plan,
                [Some(true), Some(true), Some(true), None],
                "true",
                |_| {
                    dialed += 1;
                    Ok((false, Vec::new(), b"python3: command not found".to_vec()))
                },
            )
            .unwrap_err();
        assert_eq!((error.code, dialed), (4, 1));
        assert!(!tr.can_fallback(&error));
        assert_eq!(
            tr.info()["attempted_routes"][0]["status"],
            "ssh_command_failed"
        );
        // A local process failure is returned as-is.
        let mut tr = fixture(&hosts, "both");
        let error = tr
            .dial(&plan, [Some(true), Some(true), Some(true), None], "true", |_| {
                Err(AppError::new(4, "Transport timeout; request may have completed remotely. Retry using the same request ID."))
            })
            .unwrap_err();
        assert!(!tr.can_fallback(&error));
        assert_eq!(
            tr.info()["attempted_routes"][0]["status"],
            "transport_timeout"
        );
        // Every address unreachable is exactly the fallback-eligible error.
        for mode in ["both", "lan-only"] {
            let mut tr = fixture(&hosts, mode);
            let mut dialed = Vec::new();
            let error = tr
                .dial(
                    &plan,
                    [Some(false), Some(false), Some(false), None],
                    "true",
                    |command| {
                        dialed.push(hostname_of(&command));
                        Ok((
                            false,
                            Vec::new(),
                            b"ssh: Could not resolve hostname unreachable-test-fixture".to_vec(),
                        ))
                    },
                )
                .unwrap_err();
            assert_eq!(error.code, 4);
            assert_eq!(error.message, "Configured LAN addresses are unreachable");
            assert_eq!(tr.can_fallback(&error), mode == "both");
            assert_eq!(dialed, [None::<String>]);
            let attempts = tr.info()["attempted_routes"].as_array().unwrap().clone();
            assert_eq!(attempts.len(), 4);
            assert!(attempts.iter().all(|a| a["status"] == "host_unreachable"));
            assert_eq!(attempts[3]["source"], "alias");
            assert_eq!(
                attempts[3]["reason"],
                "SSH reported connection refusal, timeout, unreachable network or DNS failure"
            );
        }
        assert_eq!(
            state::remembered_hosts(&fixture(&hosts, "both").config),
            ["198.51.100.8"]
        );
    }
    #[test]
    fn disconnected_requests_and_remote_only_ssh_never_spawn() {
        let mut disconnected = configured("both", false);
        assert_eq!(
            disconnected
                .rpc(json!({"v":1,"op":"status"}))
                .unwrap_err()
                .code,
            4
        );
        assert_eq!(disconnected.info()["selected"], "disconnected");
        assert_eq!(
            disconnected.info()["attempted_routes"][0]["attempted"],
            false
        );
        let mut remote = configured("remote-only", true);
        assert!(remote.ssh_raw("true", &[], Duration::from_secs(1)).is_err());
        assert_eq!(remote.info()["attempted_routes"][0]["attempted"], false);
        assert_eq!(remote.info()["attempted_routes"][0]["route"], "lan_ssh");
    }
    #[test]
    fn fallback_requires_both_mode_and_explicit_unreachability() {
        let unavailable = AppError::new(4, "Configured LAN addresses are unreachable");
        assert!(!configured("lan-only", true).can_fallback(&unavailable));
        assert!(!configured("both", false).can_fallback(&unavailable));
        let both = configured("both", true);
        assert!(both.can_fallback(&unavailable));
        for error in [
            AppError::new(7, "Host pin mismatch"),
            AppError::new(4, "SSH command failed"),
            AppError::new(4, "Transport timeout"),
        ] {
            assert!(!both.can_fallback(&error));
        }
    }
    #[test]
    fn ssh_authentication_and_service_errors_are_not_network_fallbacks() {
        assert_eq!(
            ssh_failure("permission denied (publickey)"),
            "authentication_failed"
        );
        assert_eq!(
            ssh_failure("host key verification failed"),
            "authentication_failed"
        );
        assert_eq!(
            ssh_failure("connect to host: connection timed out"),
            "host_unreachable"
        );
        assert_eq!(
            ssh_failure("python3: command not found"),
            "ssh_command_failed"
        );
        assert_eq!(
            ssh_failure("connection reset by peer"),
            "ssh_command_failed"
        );
    }
    #[test]
    fn controller_rejections_preserve_nested_and_legacy_details() {
        for response in [
            json!({"ok":false,"error":{"code":"project_busy","message":"Worktree is already claimed"}}),
            json!({"ok":false,"code":"project_busy","message":"Worktree is already claimed"}),
        ] {
            let error = Transport::decode(&serde_json::to_vec(&response).unwrap()).unwrap_err();
            assert_eq!(error.code, 6);
            assert_eq!(error.message, "project_busy: Worktree is already claimed");
        }
        assert!(Transport::decode(b"{}").is_err());
        assert_eq!(
            Transport::decode(br#"{"ok":true,"jobs":[]}"#).unwrap()["ok"],
            true
        );
    }
    #[test]
    fn a_live_early_candidate_does_not_wait_for_later_prechecks() {
        let _home = state::TestHome::new();
        let mut tr = fixture(&["192.0.2.1", "192.0.2.2"], "both");
        let plan = plan_candidates(None, &[], &tr.config.hosts.clone(), None);
        let open = [Some(true)].into_iter().chain(std::iter::from_fn(|| {
            panic!("a later precheck was awaited before dialing an earlier candidate")
        }));
        tr.dial(&plan, open, "true", |_| Ok((true, Vec::new(), Vec::new())))
            .unwrap();
        assert_eq!(tr.info()["lan_host"], "192.0.2.1");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let first = with_precheck(
            &[Some("127.0.0.1"), None, Some("127.0.0.1")],
            port,
            PRECHECK_CONNECT,
            PRECHECK_DEADLINE,
            |results| results.next(),
        );
        assert_eq!(first, Some(Some(true)));
    }
}
