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
    /// Whether the planned route is dialed directly from this computer, so a
    /// local TCP precheck tests the path ssh uses. Kept for the sticky host.
    direct: bool,
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
/// Plan size beyond which the alias's HostName is no longer added. Configured
/// hosts are never dropped: every one of them was dialed before rediscovery.
pub const MAX_CANDIDATES: usize = 8;
pub const PRECHECK_CONNECT: Duration = Duration::from_millis(1500);
pub const PRECHECK_DEADLINE: Duration = Duration::from_secs(2);
const CLOSED_REASON: &str = "No TCP answer on the SSH port within the 1.5s precheck";
const UNRESOLVED_REASON: &str = "The address resolved to nothing during the precheck";
const UNREACHABLE_REASON: &str =
    "SSH reported connection refusal, timeout, unreachable network or DNS failure";
const REMEMBERED_REJECTED_REASON: &str = "SSH authentication or host pin rejected at a remembered address; forgotten, next address tried, no fallback";
const REMEMBERED_UNLISTED_REJECTED_REASON: &str = "SSH authentication or host pin rejected at a remembered address that is not a configured host; forgotten, next address tried";
const UNLISTED_REJECTED_REASON: &str = "SSH authentication or host pin rejected at the alias's HostName, which is not a configured host; next address tried";
const REMEMBERED_HANDSHAKE_REASON: &str =
    "Remembered address broke off the SSH handshake before authentication; forgotten as stale";
const UNLISTED_HANDSHAKE_REASON: &str =
    "The alias's HostName broke off the SSH handshake before authentication; next address tried";
fn refused_error() -> AppError {
    AppError::new(
        7,
        "SSH authentication or pinned host-key verification failed; fallback refused",
    )
}
/// The TCP reachability hint for one candidate. `Closed` and `Unresolved`
/// hold a candidate back: a configured host is dialed only after every other
/// candidate failed, and an alias HostName outside the configured hosts is
/// not dialed. The others are dialed in order through the pinned
/// `command()`, and an answering port is never trust.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// Not prechecked, or the precheck ran out of time before it could decide
    /// (a slow name lookup, or a connect cut short by the deadline). OpenSSH
    /// then resolves and connects with its own budgets, as before prechecks.
    Unchecked,
    /// A resolved address accepted a TCP connection on the SSH port.
    Open,
    /// The name lookup finished without any address.
    Unresolved,
    /// Every resolved address refused, or stayed silent for the full
    /// per-connect budget.
    Closed,
}

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
/// remembered verified addresses, the configured hosts, then the alias's own
/// concrete HostName. Remembered addresses only reorder that current set: an
/// address no longer configured (nor the alias's HostName) is never dialed,
/// so changing `--host` or the HostName takes effect at once. Without
/// configured hosts the plan stays exactly the alias, resolved by OpenSSH, as
/// before. With them the bare alias is never dialed, as before rediscovery:
/// every candidate is a concrete address, so the same address is not dialed
/// twice, a win is sticky and remembered, and an alias that is only a label
/// is never resolved by DNS to another host.
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
    if configured.is_empty() {
        return vec![Candidate {
            source: Source::Alias,
            host: None,
        }];
    }
    let current = |host: &str| {
        configured
            .iter()
            .map(String::as_str)
            .chain(alias_hostname)
            .any(|c| c.eq_ignore_ascii_case(host))
    };
    let mut plan = Vec::new();
    for host in remembered
        .iter()
        .filter(|host| current(host))
        .take(MAX_REMEMBERED)
    {
        add_candidate(&mut plan, Source::Remembered, host);
    }
    for host in configured {
        add_candidate(&mut plan, Source::Configured, host);
    }
    if let Some(host) = alias_hostname.filter(|_| plan.len() < MAX_CANDIDATES) {
        add_candidate(&mut plan, Source::Alias, host);
    }
    plan
}
/// What the alias's local OpenSSH configuration says about reaching it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AliasRoute {
    /// The alias's own concrete HostName. None when `ssh -G` merely echoes
    /// the alias (no HostName is configured) or the value is not a plain host.
    pub hostname: Option<String>,
    /// OpenSSH connects to this alias's addresses directly from this
    /// computer: no ProxyJump, ProxyCommand, BindAddress or BindInterface
    /// applies. Every candidate is dialed through the alias, so only then
    /// does a local TCP precheck test the path ssh will use. False when the
    /// configuration could not be read.
    pub direct: bool,
}
/// Read the alias's HostName and routing options from `ssh -G` output.
pub fn parse_ssh_g(output: &str, alias: &str) -> AliasRoute {
    let mut hostname = None;
    let mut routed = false;
    for line in output.lines() {
        let Some((key, value)) = line.trim_end_matches('\r').split_once(' ') else {
            continue;
        };
        let value = value.trim();
        match key {
            "proxyjump" | "proxycommand" | "bindaddress" | "bindinterface" => {
                routed |= !value.eq_ignore_ascii_case("none");
            }
            "hostname" => {
                hostname.get_or_insert(value);
            }
            _ => {}
        }
    }
    AliasRoute {
        // `ssh -G` always prints a hostname; without one this is not its output.
        direct: hostname.is_some() && !routed,
        hostname: hostname
            .filter(|host| state::validate_host(host).is_ok() && !host.eq_ignore_ascii_case(alias))
            .map(str::to_owned),
    }
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
/// The alias's HostName and routing. A failed lookup is not direct, so no
/// candidate is prechecked and OpenSSH dials each one as before prechecks.
/// The `-G` output may describe private configuration and is never logged.
pub fn alias_route(config: &Config) -> AliasRoute {
    match process(alias_lookup_command(config), &[], Duration::from_secs(3)) {
        Ok((Some(0), out, _)) => parse_ssh_g(&String::from_utf8_lossy(&out), &config.ssh_alias),
        _ => AliasRoute::default(),
    }
}
/// The addresses of `host`, or None when the lookup did not finish before the
/// deadline (or could not be started), which leaves reachability unknown.
fn resolve(host: &str, port: u16, deadline: Instant) -> Option<Vec<SocketAddr>> {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return Some(vec![SocketAddr::new(ip, port)]);
    }
    // getaddrinfo has no timeout. A detached helper performs the lookup so the
    // precheck still ends at its deadline; a late answer is discarded.
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
        .ok()
        .map(|found| found.unwrap_or_default())
}
fn probe(host: &str, port: u16, per_connect: Duration, deadline: Instant) -> Reach {
    let Some(addresses) = resolve(host, port, deadline) else {
        return Reach::Unchecked;
    };
    if addresses.is_empty() {
        return Reach::Unresolved;
    }
    let mut cut_short = false;
    for address in addresses {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            cut_short = true;
            break;
        }
        let budget = per_connect.min(remaining);
        match TcpStream::connect_timeout(&address, budget) {
            Ok(_) => return Reach::Open,
            Err(error) => {
                cut_short |= error.kind() == std::io::ErrorKind::TimedOut && budget < per_connect
            }
        }
    }
    if cut_short {
        Reach::Unchecked
    } else {
        Reach::Closed
    }
}
/// Parallel TCP reachability check of each concrete address on the SSH port,
/// all started at once and bounded by one deadline. `consume` receives the
/// results in candidate order and each is awaited only when it is pulled, so
/// a live early candidate is not held up by a dead later one, neither before
/// it is dialed nor when the request returns. A candidate without an address
/// is `Unchecked` and dialed without precheck.
pub fn with_precheck<T>(
    hosts: &[Option<&str>],
    port: u16,
    per_connect: Duration,
    deadline: Duration,
    consume: impl FnOnce(&mut dyn Iterator<Item = Reach>) -> T,
) -> T {
    with_probe(
        hosts,
        deadline,
        move |host: String, until| probe(&host, port, per_connect, until),
        consume,
    )
}
/// Time allowed past the deadline for a probe to report; probes bound
/// themselves by the deadline, so this only absorbs scheduling delay.
const PRECHECK_GRACE: Duration = Duration::from_millis(250);
/// Each probe runs on a detached thread that reports over its own channel.
/// Nothing joins the threads, so returning never waits for a probe still
/// stuck on a dead or slow address; its late result is discarded and the
/// thread ends by the deadline. A probe that never reports is `Unchecked`.
fn with_probe<T>(
    hosts: &[Option<&str>],
    deadline: Duration,
    probe: impl Fn(String, Instant) -> Reach + Clone + Send + 'static,
    consume: impl FnOnce(&mut dyn Iterator<Item = Reach>) -> T,
) -> T {
    let deadline = Instant::now() + deadline;
    let pending: Vec<Option<mpsc::Receiver<Reach>>> = hosts
        .iter()
        .map(|host| {
            let host = (*host)?.to_owned();
            let (sender, receiver) = mpsc::channel();
            let probe = probe.clone();
            thread::Builder::new()
                .spawn(move || {
                    let _ = sender.send(probe(host, deadline));
                })
                .ok()?;
            Some(receiver)
        })
        .collect();
    consume(&mut pending.into_iter().map(|receiver| {
        receiver
            .and_then(|receiver| {
                let wait = deadline.saturating_duration_since(Instant::now()) + PRECHECK_GRACE;
                receiver.recv_timeout(wait).ok()
            })
            .unwrap_or(Reach::Unchecked)
    }))
}
pub fn precheck(
    hosts: &[Option<&str>],
    port: u16,
    per_connect: Duration,
    deadline: Duration,
) -> Vec<Reach> {
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
/// OpenSSH's own failure (exit 255) before authentication finished: the
/// identification, banner or key exchange broke, or the peer closed or reset
/// the connection while still negotiating. Nothing reached stdout, and the
/// post-authentication forms ("Connection to X closed by remote host", "Read
/// from remote host", client_loop) are excluded, so no remote command ran.
fn broke_before_auth(code: Option<i32>, stdout: &[u8], message: &str) -> bool {
    let negotiation = [
        "kex_exchange_identification",
        // The same identification phase as named by OpenSSH before 8.x.
        "ssh_exchange_identification",
        "unable to negotiate with",
        "banner exchange",
    ]
    .iter()
    .any(|s| message.contains(s));
    let dropped = ["connection closed by ", "connection reset by "]
        .iter()
        .any(|s| message.contains(s))
        && ![
            "client_loop",
            "read from remote host",
            "closed by remote host",
        ]
        .iter()
        .any(|s| message.contains(s));
    code == Some(255) && stdout.is_empty() && (negotiation || dropped)
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
/// A finished child: exit code (None when ended by a signal), stdout, stderr.
pub type Finished = (Option<i32>, Vec<u8>, Vec<u8>);
pub fn process(mut command: Command, input: &[u8], timeout: Duration) -> Result<Finished> {
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
    let code = loop {
        if let Some(status) = child.try_wait()? {
            break status.code();
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
        code,
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
            direct: false,
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
    /// LAN candidate receipt. `target_index` is the dialed address's position
    /// in config.hosts (compared case-insensitively) whatever its source, so
    /// a configured address keeps its index once it is remembered or sticky.
    /// It is null for an address that is not a configured host.
    fn record_candidate(&mut self, candidate: &Candidate, status: &str, reason: &str) {
        let target = candidate
            .host
            .as_deref()
            .and_then(|host| self.configured_index(host));
        self.push_attempt(json!({"route":"lan_ssh","attempted":true,"status":status,"reason":reason,"target_index":target,"source":candidate.source.name(),"host":candidate.host}));
    }
    fn configured_index(&self, host: &str) -> Option<usize> {
        self.config
            .hosts
            .iter()
            .position(|h| h.eq_ignore_ascii_case(host))
    }
    /// Whether the code before rediscovery dialed this candidate: one of the
    /// configured hosts, or the alias left to OpenSSH (planned only when no
    /// hosts are configured). Its failures keep their earlier meaning; any
    /// other candidate may only add a way to succeed.
    fn dialed_before_rediscovery(&self, host: Option<&str>) -> bool {
        host.is_none_or(|host| self.configured_index(host).is_some())
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
        let plan = self.lan_plan(alias_route);
        let hosts = self.precheck_targets(&plan);
        let port = self.config.port;
        with_precheck(&hosts, port, PRECHECK_CONNECT, PRECHECK_DEADLINE, |reach| {
            self.dial(&plan, reach, remote, |command| {
                process(command, input, timeout)
            })
        })
    }
    /// The candidate plan for the next LAN request. `alias_lookup` reads the
    /// alias's HostName and routing (a local `ssh -G`) and is only called
    /// when needed; a sticky host keeps the routing seen when it was planned.
    fn lan_plan(&mut self, alias_lookup: impl FnOnce(&Config) -> AliasRoute) -> Vec<Candidate> {
        match self.host.clone() {
            Some(sticky) => plan_candidates(Some(&sticky), &[], &[], None),
            None if self.config.hosts.is_empty() => {
                self.direct = false;
                plan_candidates(None, &[], &[], None)
            }
            None => {
                let remembered = state::remembered_lan(&self.config);
                self.remembered_head = remembered.fresh_head;
                let alias = alias_lookup(&self.config);
                self.direct = alias.direct;
                plan_candidates(
                    None,
                    &remembered.hosts,
                    &self.config.hosts,
                    alias.hostname.as_deref(),
                )
            }
        }
    }
    /// Addresses to precheck. When ProxyJump, ProxyCommand or a bind option
    /// routes the alias, ssh reaches every candidate through that route, so a
    /// direct local connect or lookup would test the wrong path (and expose
    /// names the proxy keeps private): nothing is prechecked.
    fn precheck_targets<'a>(&self, plan: &'a [Candidate]) -> Vec<Option<&'a str>> {
        plan.iter()
            .map(|c| c.host.as_deref().filter(|_| self.direct))
            .collect()
    }
    /// Dial prechecked candidates in planned order through the pinned
    /// `command()`; `reach` is pulled lazily, one per candidate. A configured
    /// host the precheck held back (`Closed` or `Unresolved`) is dialed once
    /// more after every other candidate failed, so a precheck false negative
    /// never stops the owner's own host from being tried. See `attempt` for
    /// what each failure means.
    fn dial(
        &mut self,
        plan: &[Candidate],
        reach: impl IntoIterator<Item = Reach>,
        remote: &str,
        mut run: impl FnMut(Command) -> Result<Finished>,
    ) -> Result<Vec<u8>> {
        let mut refused = false;
        let mut held_back = Vec::new();
        for (candidate, reach) in plan.iter().zip(reach) {
            let skipped = match reach {
                Reach::Closed => Some(CLOSED_REASON),
                Reach::Unresolved => Some(UNRESOLVED_REASON),
                Reach::Open | Reach::Unchecked => None,
            };
            if let Some(reason) = skipped {
                self.record_candidate(candidate, "host_unreachable", reason);
                if self.dialed_before_rediscovery(candidate.host.as_deref()) {
                    held_back.push(candidate);
                }
                continue;
            }
            if let Some(out) = self.attempt(candidate, remote, &mut run, &mut refused)? {
                return Ok(out);
            }
        }
        // The precheck can miss a slow host (a first SYN or ARP over 1.5s, a
        // flaky resolver). Before giving up, each configured host it held
        // back gets the dial it had before prechecks, with OpenSSH's budgets.
        for candidate in held_back {
            if let Some(out) = self.attempt(candidate, remote, &mut run, &mut refused)? {
                return Ok(out);
            }
        }
        if refused {
            return Err(refused_error());
        }
        Err(AppError::new(4, "Configured LAN addresses are unreachable"))
    }
    /// Dial one candidate. `Ok(None)` moves on to the next one.
    ///
    /// Reachability failures always move on. A configured host, the alias
    /// left to OpenSSH, or a sticky configured host keeps the semantics it
    /// had before rediscovery: an authentication or host-key rejection stops
    /// at once with 7 (no fallback), any other SSH failure with 4.
    ///
    /// A remembered address is only a learned hint. When it rejects the pin
    /// or login, or OpenSSH breaks off before authentication, it is forgotten
    /// and the next candidate dialed, still through the same pin. The alias's
    /// HostName outside the configured hosts was never dialed before
    /// rediscovery, so the same failures there also move on. Nothing ran
    /// remotely in any of these cases, so retrying elsewhere cannot duplicate
    /// work. A rejection at a configured host (remembered or not) still
    /// refuses fallback: unless a later candidate authenticates, the request
    /// fails with 7. A rejection at an address outside the configured hosts
    /// cannot turn an otherwise unreachable result into 7.
    ///
    /// For every source, a failure after authentication may have run the
    /// command, so it stops with 4, and a local process error or timeout is
    /// returned as-is.
    fn attempt(
        &mut self,
        candidate: &Candidate,
        remote: &str,
        run: &mut impl FnMut(Command) -> Result<Finished>,
        refused: &mut bool,
    ) -> Result<Option<Vec<u8>>> {
        let host = candidate.host.as_deref();
        let (code, out, err) = match run(self.command(host, remote)) {
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
        if code == Some(0) {
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
            return Ok(Some(out));
        }
        let message = String::from_utf8_lossy(&err).to_lowercase();
        let failure = ssh_failure(&message);
        if failure == "host_unreachable" {
            self.record_candidate(candidate, failure, UNREACHABLE_REASON);
            return Ok(None);
        }
        let rejected = failure == "authentication_failed";
        let broken = !rejected && broke_before_auth(code, &out, &message);
        let configured = self.dialed_before_rediscovery(host);
        let remembered = candidate.source == Source::Remembered;
        if (rejected || broken) && (remembered || !configured) {
            let (status, reason) = match (rejected, remembered, configured) {
                (true, true, true) => (failure, REMEMBERED_REJECTED_REASON),
                (true, true, false) => (failure, REMEMBERED_UNLISTED_REJECTED_REASON),
                (true, false, _) => (failure, UNLISTED_REJECTED_REASON),
                (false, true, _) => ("host_unreachable", REMEMBERED_HANDSHAKE_REASON),
                (false, false, _) => ("host_unreachable", UNLISTED_HANDSHAKE_REASON),
            };
            self.record_candidate(candidate, status, reason);
            if let (true, Some(host)) = (remembered, host) {
                self.forget_remembered(host);
            }
            *refused |= rejected && configured;
            return Ok(None);
        }
        if rejected {
            self.record_candidate(
                candidate,
                failure,
                "SSH authentication or host pin rejected; no fallback",
            );
            return Err(refused_error());
        }
        self.record_candidate(
            candidate,
            failure,
            "SSH command failed without evidence that the LAN host is unreachable",
        );
        Err(AppError::new(
            4,
            "SSH command failed; inspect the configured service and key (stderr withheld)",
        ))
    }
    fn forget_remembered(&mut self, host: &str) {
        // Best effort: a failed write never fails the request.
        let _ = state::forget_host(&self.config, host);
        if self.remembered_head.as_deref() == Some(host) {
            self.remembered_head = None;
        }
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
            let (code, out, _) = match process(command, &payload, timeout) {
                Ok(result) => result,
                Err(e) => {
                    self.record("paired_iroh", true, "transport_error", &e.message, None);
                    return Err(e);
                }
            };
            if code != Some(0) {
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
    use Reach::{Closed, Open, Unchecked, Unresolved};
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
            plan_candidates(None, &[], &configured, Some("192.0.2.3")),
            [
                candidate(Source::Configured, Some("192.0.2.1")),
                candidate(Source::Configured, Some("192.0.2.2")),
                candidate(Source::Alias, Some("192.0.2.3")),
            ]
        );
        // Remembered addresses move current ones forward, newest first; a
        // remembered configured address or alias HostName keeps that earlier
        // slot and is not dialed twice.
        assert_eq!(
            plan_candidates(
                None,
                &strings(&["192.0.2.3", "192.0.2.2"]),
                &configured,
                Some("192.0.2.3")
            ),
            [
                candidate(Source::Remembered, Some("192.0.2.3")),
                candidate(Source::Remembered, Some("192.0.2.2")),
                candidate(Source::Configured, Some("192.0.2.1")),
            ]
        );
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
                &strings(&["SPARK.LOCAL"]),
                &strings(&["Spark.Local", "spark.local"]),
                Some("SPARK.LOCAL")
            ),
            [candidate(Source::Remembered, Some("SPARK.LOCAL"))]
        );
        // With configured hosts, an alias without a concrete HostName is not
        // dialed at all: not as a second, unprechecked dial of an address
        // already planned, and not as a label OpenSSH might resolve to an
        // unrelated host.
        assert_eq!(
            plan_candidates(None, &[], &configured, None),
            [
                candidate(Source::Configured, Some("192.0.2.1")),
                candidate(Source::Configured, Some("192.0.2.2")),
            ]
        );
        // An address the configuration no longer names (an old --host or a
        // previous HostName) is never planned, however recently verified.
        assert_eq!(
            plan_candidates(
                None,
                &strings(&["198.51.100.9", "192.0.2.2", "198.51.100.8"]),
                &configured,
                Some("192.0.2.3")
            ),
            [
                candidate(Source::Remembered, Some("192.0.2.2")),
                candidate(Source::Configured, Some("192.0.2.1")),
                candidate(Source::Alias, Some("192.0.2.3")),
            ]
        );
        let six = strings(&[
            "192.0.2.11",
            "192.0.2.12",
            "192.0.2.13",
            "192.0.2.14",
            "192.0.2.15",
            "192.0.2.16",
        ]);
        let mut newest_first = six.clone();
        newest_first.reverse();
        let reordered = plan_candidates(None, &newest_first, &six, None);
        assert_eq!(
            reordered
                .iter()
                .filter(|c| c.source == Source::Remembered)
                .count(),
            MAX_REMEMBERED
        );
        assert_eq!(reordered.len(), 6);
        assert_eq!(
            reordered[0],
            candidate(Source::Remembered, Some("192.0.2.16"))
        );
        assert_eq!(
            reordered[4],
            candidate(Source::Configured, Some("192.0.2.11"))
        );
        // Configured hosts are never truncated; only the alias addition stops
        // at the cap.
        let nine: Vec<String> = (1..=9).map(|n| format!("192.0.2.{}", 20 + n)).collect();
        let all = plan_candidates(None, &strings(&["192.0.2.29"]), &nine, Some("192.0.2.3"));
        assert_eq!(all.len(), 9);
        assert_eq!(all[0], candidate(Source::Remembered, Some("192.0.2.29")));
        assert_eq!(all[8], candidate(Source::Configured, Some("192.0.2.28")));
        assert!(all.iter().all(|c| c.source != Source::Alias));
        let seven = &nine[..7];
        let with_alias = plan_candidates(None, &[], seven, Some("192.0.2.3"));
        assert_eq!(with_alias.len(), MAX_CANDIDATES);
        assert_eq!(with_alias[7], candidate(Source::Alias, Some("192.0.2.3")));
    }
    #[test]
    fn a_re_saved_address_replaces_the_remembered_one_at_once() {
        let home = state::TestHome::new();
        // .40 authenticated before the Spark moved; the owner saved .50.
        let before = fixture(&["192.0.2.40"], "both");
        state::remember_host(&before.config, "192.0.2.40").unwrap();
        let mut tr = fixture(&["192.0.2.50"], "both");
        assert_eq!(
            state::lan_binding(&tr.config),
            state::lan_binding(&before.config)
        );
        let plan = tr.lan_plan(|_| route(None, true));
        assert_eq!(plan, [candidate(Source::Configured, Some("192.0.2.50"))]);
        // An old address is still planned while the alias's HostName names it.
        let plan = tr.lan_plan(|_| route(Some("192.0.2.40"), true));
        assert_eq!(
            plan,
            [
                candidate(Source::Remembered, Some("192.0.2.40")),
                candidate(Source::Configured, Some("192.0.2.50")),
            ]
        );
        assert!(home.path.join("lan-route.json").exists());
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
    fn route(hostname: Option<&str>, direct: bool) -> AliasRoute {
        AliasRoute {
            hostname: hostname.map(str::to_owned),
            direct,
        }
    }
    #[test]
    fn ssh_g_parser_reads_hostname_and_detects_proxied_or_bound_routes() {
        let plain =
            "user owner\nhostname 192.168.1.186\nport 22\nproxycommand none\nhostkeyalias pin\n";
        let alias = "my-spark";
        assert_eq!(
            parse_ssh_g(plain, alias),
            route(Some("192.168.1.186"), true)
        );
        assert_eq!(
            parse_ssh_g(&plain.replace('\n', "\r\n"), alias),
            route(Some("192.168.1.186"), true)
        );
        assert_eq!(
            parse_ssh_g(
                "proxyjump none\nhostname spark.local\nhostname 192.0.2.9\n",
                alias
            ),
            route(Some("spark.local"), true)
        );
        // A proxied or bound alias keeps its HostName (dialed through the
        // alias's own route) but nothing may be prechecked directly.
        for routed in [
            "hostname 192.0.2.5\nproxyjump bastion\n",
            "proxycommand ssh -W %h:%p bastion\r\nhostname 192.0.2.5\r\n",
            "hostname 192.0.2.5\nproxyjump \n",
            "hostname 192.0.2.5\nbindaddress 198.51.100.2\n",
            "bindinterface eth1\nhostname 192.0.2.5\n",
        ] {
            assert_eq!(
                parse_ssh_g(routed, alias),
                route(Some("192.0.2.5"), false),
                "{routed:?}"
            );
        }
        assert_eq!(
            parse_ssh_g("hostname my-spark\nproxyjump bastion\n", alias),
            route(None, false)
        );
        for unusable in [
            "hostname -oProxyCommand=x\n",
            "hostname spark;id\n",
            "hostname \n",
            // No HostName configured: -G echoes the (lowercased) alias.
            "hostname my-spark\nport 22\n",
            "hostname MY-SPARK\n",
        ] {
            assert_eq!(
                parse_ssh_g(unusable, alias),
                route(None, true),
                "{unusable:?}"
            );
        }
        // Output without a hostname line is not `ssh -G` output at all.
        for unknown in ["hostnames 192.0.2.6\nxhostname 192.0.2.6\n", ""] {
            assert_eq!(
                parse_ssh_g(unknown, alias),
                route(None, false),
                "{unknown:?}"
            );
        }
        assert_eq!(AliasRoute::default(), route(None, false));
    }
    #[test]
    fn with_configured_hosts_every_candidate_is_concrete_so_a_win_sticks() {
        let _home = state::TestHome::new();
        // The alias is itself the LAN address, so `ssh -G` only echoes it:
        // the address is planned and dialed once, never again as the alias.
        let mut tr = fixture(&["192.168.1.186"], "both");
        tr.config.ssh_alias = "192.168.1.186".into();
        let plan = tr.lan_plan(|_| route(None, true));
        assert_eq!(plan, [candidate(Source::Configured, Some("192.168.1.186"))]);
        // A label alias (no HostName) is never dialed for OpenSSH to resolve.
        let mut tr = fixture(&["192.0.2.1"], "both");
        let plan = tr.lan_plan(|_| route(None, true));
        assert_eq!(plan, [candidate(Source::Configured, Some("192.0.2.1"))]);
        assert!(plan.iter().all(|c| c.host.is_some()));
        // A concrete alias HostName wins as an address: it is sticky for the
        // rest of the process and remembered for the next one.
        let plan = tr.lan_plan(|_| route(Some("192.0.2.9"), true));
        let mut dialed = Vec::new();
        tr.dial(&plan, [Closed, Open], "true", |command| {
            dialed.push(hostname_of(&command));
            ok(b"")
        })
        .unwrap();
        assert_eq!(dialed, [Some("192.0.2.9".to_owned())]);
        assert_eq!(tr.host.as_deref(), Some("192.0.2.9"));
        assert_eq!(tr.info()["lan_host"], "192.0.2.9");
        assert_eq!(state::remembered_hosts(&tr.config), ["192.0.2.9"]);
        let sticky = tr.lan_plan(|_| panic!("a sticky host needs no alias lookup"));
        assert_eq!(sticky, [candidate(Source::Sticky, Some("192.0.2.9"))]);
    }
    #[test]
    fn a_proxied_alias_prechecks_no_candidate_and_dials_each_through_its_route() {
        let _home = state::TestHome::new();
        let mut tr = fixture(&["192.0.2.6", "192.0.2.5"], "lan-only");
        let plan = tr.lan_plan(|_| route(Some("192.0.2.5"), false));
        assert_eq!(
            plan.iter().map(|c| c.host.as_deref()).collect::<Vec<_>>(),
            [Some("192.0.2.6"), Some("192.0.2.5")]
        );
        assert_eq!(tr.precheck_targets(&plan), [None, None]);
        let mut dialed = Vec::new();
        tr.dial(&plan, [Unchecked, Unchecked], "true", |command| {
            assert_pinned(&command, "test-pin");
            dialed.push(hostname_of(&command));
            ok(b"")
        })
        .unwrap();
        // The address goes through the alias, and so through its ProxyJump.
        assert_eq!(dialed, [Some("192.0.2.6".to_owned())]);
        // The sticky host keeps the proxied routing: still no direct check.
        let sticky = tr.lan_plan(|_| panic!("a sticky host needs no alias lookup"));
        assert_eq!(tr.precheck_targets(&sticky), [None]);
        // A failed lookup leaves every candidate to OpenSSH, as before.
        let mut unknown = fixture(&["192.0.2.6"], "lan-only");
        let plan = unknown.lan_plan(|_| AliasRoute::default());
        assert_eq!(unknown.precheck_targets(&plan), [None]);
        // A direct alias prechecks every concrete address.
        let mut direct = fixture(&["192.0.2.6"], "lan-only");
        let plan = direct.lan_plan(|_| route(Some("192.0.2.5"), true));
        assert_eq!(
            direct.precheck_targets(&plan),
            [Some("192.0.2.6"), Some("192.0.2.5")]
        );
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
            [Open, Unchecked, Open]
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
            [Closed, Closed, Closed, Unchecked]
        );
        // Three closed checks in parallel finish within one deadline.
        assert!(started.elapsed() < PRECHECK_DEADLINE + slack);
        assert!(precheck(&[], open_port, PRECHECK_CONNECT, PRECHECK_DEADLINE).is_empty());
        drop(listener);
    }
    #[test]
    fn a_precheck_that_runs_out_of_time_is_unknown_not_closed() {
        let closed_port = {
            let probe = TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        // A lookup or connect cut short by the deadline proves nothing about
        // the host, so the candidate is left to OpenSSH instead of skipped.
        let expired = Instant::now();
        assert_eq!(
            probe("127.0.0.1", closed_port, PRECHECK_CONNECT, expired),
            Unchecked
        );
        assert_eq!(
            probe("localhost", closed_port, PRECHECK_CONNECT, expired),
            Unchecked
        );
        assert_eq!(
            probe(
                "127.0.0.1",
                closed_port,
                PRECHECK_CONNECT,
                Instant::now() + PRECHECK_DEADLINE
            ),
            Closed
        );
    }
    #[test]
    fn unknown_prechecks_are_dialed_and_unresolved_names_skipped_with_their_reason() {
        let _home = state::TestHome::new();
        let mut tr = fixture(&["spark-a.local", "spark-b.local"], "lan-only");
        let plan = plan_candidates(None, &[], &tr.config.hosts.clone(), Some("192.0.2.3"));
        let mut dialed = Vec::new();
        tr.dial(&plan, [Unresolved, Unchecked], "true", |command| {
            dialed.push(hostname_of(&command));
            ok(b"")
        })
        .unwrap();
        assert_eq!(dialed, [Some("spark-b.local".to_owned())]);
        let attempts = tr.info()["attempted_routes"].clone();
        assert_eq!(attempts[0]["status"], "host_unreachable");
        assert_eq!(attempts[0]["reason"], UNRESOLVED_REASON);
        assert_eq!(attempts[1]["status"], "authenticated");
    }
    fn ok(out: &[u8]) -> Result<Finished> {
        Ok((Some(0), out.to_vec(), Vec::new()))
    }
    /// OpenSSH's own failure: exit 255 with its message on stderr.
    fn ssh_error(stderr: &str) -> Result<Finished> {
        Ok((Some(255), Vec::new(), stderr.as_bytes().to_vec()))
    }
    fn refused() -> Result<Finished> {
        ssh_error("ssh: connect to host 192.0.2.1 port 22: Connection refused\r\n")
    }
    #[test]
    fn dial_skips_closed_ports_continues_on_unreachable_and_remembers_winner() {
        let _home = state::TestHome::new();
        let mut tr = fixture(&["192.0.2.1", "192.0.2.2"], "both");
        let plan = plan_candidates(
            None,
            &strings(&["192.0.2.3"]),
            &tr.config.hosts.clone(),
            Some("192.0.2.3"),
        );
        let open = [Closed, Open, Open];
        let mut dialed = Vec::new();
        let out = tr
            .dial(&plan, open, "true", |command| {
                assert_pinned(&command, "test-pin");
                let host = hostname_of(&command);
                dialed.push(host.clone());
                match host.as_deref() {
                    Some("192.0.2.1") => refused(),
                    _ => ok(b"ok"),
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
            json!({"route":"lan_ssh","attempted":true,"status":"host_unreachable","reason":CLOSED_REASON,"target_index":null,"source":"remembered","host":"192.0.2.3"})
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
        tr.dial(&sticky, [Open], "true", |_| ok(b"")).unwrap();
        assert!(!route_file.exists());
        assert_eq!(tr.info()["attempted_routes"][3]["source"], "sticky");
        // A sticky configured address keeps its configured position.
        assert_eq!(tr.info()["attempted_routes"][3]["target_index"], 1);
        // An alias dialed through OpenSSH has no concrete address to report.
        let mut alias = fixture(&[], "lan-only");
        alias
            .dial(
                &[candidate(Source::Alias, None)],
                [Unchecked],
                "true",
                |command| {
                    assert_eq!(hostname_of(&command), None);
                    ok(b"")
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
        let plan = tr.lan_plan(|_| AliasRoute::default());
        assert_eq!(plan[0], candidate(Source::Remembered, Some("192.0.2.2")));
        tr.dial(&plan, [Open], "true", |_| ok(b"")).unwrap();
        let refreshed = verified_at();
        assert!(refreshed >= state::now() - 60, "{refreshed}");
        // A fresh head in the next process is not rewritten.
        let before = std::fs::read(&file).unwrap();
        let mut next = fixture(&hosts, "both");
        let plan = next.lan_plan(|_| AliasRoute::default());
        next.dial(&plan, [Open], "true", |_| ok(b"")).unwrap();
        assert_eq!(next.info()["lan_host"], "192.0.2.2");
        assert_eq!(std::fs::read(&file).unwrap(), before);
    }
    #[test]
    fn dial_failures_keep_fail_closed_semantics() {
        let _home = state::TestHome::new();
        let hosts = ["192.0.2.1", "192.0.2.2"];
        // A remembered address that fails the pin is forgotten. When nothing
        // later authenticates the request still fails with 7: no fallback.
        let mut tr = fixture(&hosts, "both");
        state::remember_host(&tr.config, "192.0.2.1").unwrap();
        state::remember_host(&tr.config, "192.0.2.2").unwrap();
        let plan = tr.lan_plan(|_| AliasRoute::default());
        assert_eq!(
            plan,
            [
                candidate(Source::Remembered, Some("192.0.2.2")),
                candidate(Source::Remembered, Some("192.0.2.1")),
            ]
        );
        // The precheck held 192.0.2.1 back, so it is dialed after the
        // rejection, and only the rejected address is forgotten.
        let mut dialed = Vec::new();
        let error = tr
            .dial(&plan, [Open, Closed], "true", |command| {
                assert_pinned(&command, "test-pin");
                let host = hostname_of(&command);
                dialed.push(host.clone());
                match host.as_deref() {
                    Some("192.0.2.2") => ssh_error("Host key verification failed.\r\n"),
                    _ => refused(),
                }
            })
            .unwrap_err();
        assert_eq!(error.code, 7);
        assert_eq!(
            dialed,
            [Some("192.0.2.2".to_owned()), Some("192.0.2.1".to_owned())]
        );
        assert!(!tr.can_fallback(&error));
        assert!(!tr.authenticated);
        assert_eq!(tr.info()["route"], "none");
        let attempts = tr.info()["attempted_routes"].clone();
        assert_eq!(attempts[0]["status"], "authentication_failed");
        assert_eq!(attempts[0]["reason"], REMEMBERED_REJECTED_REASON);
        // Remembered configured addresses keep their configured position.
        assert_eq!(attempts[0]["target_index"], 1);
        assert_eq!(attempts[1]["status"], "host_unreachable");
        assert_eq!(attempts[1]["reason"], CLOSED_REASON);
        assert_eq!(attempts[2]["status"], "host_unreachable");
        assert_eq!(attempts[2]["host"], "192.0.2.1");
        assert_eq!(attempts[2]["target_index"], 0);
        assert_eq!(state::remembered_hosts(&tr.config), ["192.0.2.1"]);
        // A configured address failing authentication fails closed but does
        // not erase remembered addresses.
        let mut tr = fixture(&hosts, "both");
        let plan = tr.lan_plan(|_| AliasRoute::default());
        assert_eq!(
            plan,
            [
                candidate(Source::Remembered, Some("192.0.2.1")),
                candidate(Source::Configured, Some("192.0.2.2")),
            ]
        );
        let error = tr
            .dial(&plan, [Closed, Open], "true", |_| {
                ssh_error("owner@192.0.2.2: Permission denied (publickey).")
            })
            .unwrap_err();
        assert_eq!(error.code, 7);
        assert_eq!(tr.info()["attempted_routes"].as_array().unwrap().len(), 2);
        assert_eq!(tr.info()["attempted_routes"][1]["source"], "configured");
        assert_eq!(state::remembered_hosts(&tr.config), ["192.0.2.1"]);
        // A service failure on an answering host stops without fallback.
        let mut tr = fixture(&hosts, "both");
        let mut dialed = 0;
        let error = tr
            .dial(&plan, [Open, Open], "true", |_| {
                dialed += 1;
                Ok((
                    Some(127),
                    Vec::new(),
                    b"bash: python3: command not found".to_vec(),
                ))
            })
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
            .dial(&plan, [Open, Open], "true", |_| {
                Err(AppError::new(4, "Transport timeout; request may have completed remotely. Retry using the same request ID."))
            })
            .unwrap_err();
        assert!(!tr.can_fallback(&error));
        assert_eq!(
            tr.info()["attempted_routes"][0]["status"],
            "transport_timeout"
        );
        // Every address unreachable is exactly the fallback-eligible error,
        // after the held-back configured address got its own dial.
        for mode in ["both", "lan-only"] {
            let mut tr = fixture(&hosts, mode);
            let mut dialed = Vec::new();
            let error = tr
                .dial(&plan, [Closed, Unchecked], "true", |command| {
                    dialed.push(hostname_of(&command));
                    ssh_error("ssh: Could not resolve hostname 192.0.2.2")
                })
                .unwrap_err();
            assert_eq!(error.code, 4);
            assert_eq!(error.message, "Configured LAN addresses are unreachable");
            assert_eq!(tr.can_fallback(&error), mode == "both");
            assert_eq!(
                dialed,
                [Some("192.0.2.2".to_owned()), Some("192.0.2.1".to_owned())]
            );
            let attempts = tr.info()["attempted_routes"].as_array().unwrap().clone();
            assert_eq!(attempts.len(), 3);
            assert!(attempts.iter().all(|a| a["status"] == "host_unreachable"));
            assert_eq!(attempts[1]["source"], "configured");
            assert_eq!(attempts[1]["reason"], UNREACHABLE_REASON);
            assert_eq!(attempts[2]["source"], "remembered");
            assert_eq!(attempts[2]["reason"], UNREACHABLE_REASON);
        }
        assert_eq!(
            state::remembered_hosts(&fixture(&hosts, "both").config),
            ["192.0.2.1"]
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
    fn only_ssh_own_pre_authentication_breaks_count_as_a_stale_address() {
        for before in [
            "kex_exchange_identification: connection closed by remote host",
            "kex_exchange_identification: read: connection reset by peer",
            "kex_exchange_identification: banner line contains invalid characters",
            "unable to negotiate with 192.0.2.5 port 22: no matching key exchange method found. their offer: diffie-hellman-group1-sha1",
            "banner exchange: connection to 192.0.2.5 port 22: invalid format",
            "connection closed by 192.0.2.5 port 22",
            "connection reset by 192.0.2.5 port 22",
            "connection closed by authenticating user owner 192.0.2.5 port 22 [preauth]",
        ] {
            assert!(broke_before_auth(Some(255), b"", before), "{before}");
            // Never for a remote command's own exit status, or once output
            // arrived, since the command may then have run.
            assert!(!broke_before_auth(Some(1), b"", before), "{before}");
            assert!(!broke_before_auth(None, b"", before), "{before}");
            assert!(!broke_before_auth(Some(255), b"{", before), "{before}");
        }
        for after in [
            "connection to 192.0.2.5 closed by remote host.",
            "read from remote host 192.0.2.5: connection reset by peer",
            "client_loop: send disconnect: connection reset by peer",
            "client_loop: ssh_packet_write_poll: connection reset by 192.0.2.5 port 22",
            "timeout, server 192.0.2.5 not responding.",
            "host key verification failed.",
            "bash: python3: command not found",
            "",
        ] {
            assert!(!broke_before_auth(Some(255), b"", after), "{after}");
        }
    }
    #[test]
    fn a_stale_remembered_address_is_forgotten_and_the_next_one_dialed() {
        let _home = state::TestHome::new();
        let hosts = ["192.0.2.50", "192.0.2.51"];
        let stale = |tr: &mut Transport| {
            let _ = std::fs::remove_file(state::root().unwrap().join("lan-route.json"));
            state::remember_host(&tr.config, "192.0.2.51").unwrap();
            let plan = tr.lan_plan(|_| AliasRoute::default());
            assert_eq!(
                plan,
                [
                    candidate(Source::Remembered, Some("192.0.2.51")),
                    candidate(Source::Configured, Some("192.0.2.50")),
                ]
            );
            plan
        };
        // DHCP gave the remembered address to a device whose SSH server
        // offers no common key exchange: forget it and dial the next one.
        let mut tr = fixture(&hosts, "both");
        let plan = stale(&mut tr);
        let mut dialed = Vec::new();
        let out = tr
            .dial(&plan, [Open, Open], "true", |command| {
                assert_pinned(&command, "test-pin");
                let host = hostname_of(&command);
                dialed.push(host.clone());
                match host.as_deref() {
                    Some("192.0.2.51") => ssh_error(
                        "Unable to negotiate with 192.0.2.51 port 22: no matching key exchange method found.\r\n",
                    ),
                    _ => ok(b"ok"),
                }
            })
            .unwrap();
        assert_eq!(out, b"ok");
        assert_eq!(dialed.len(), 2);
        let attempts = tr.info()["attempted_routes"].clone();
        assert_eq!(attempts[0]["status"], "host_unreachable");
        assert_eq!(attempts[0]["reason"], REMEMBERED_HANDSHAKE_REASON);
        assert_eq!(attempts[1]["status"], "authenticated");
        assert_eq!(state::remembered_hosts(&tr.config), ["192.0.2.50"]);
        // A listener that resets the handshake and nothing else answering is
        // plain unreachability, so the paired fallback stays available.
        let mut tr = fixture(&hosts, "both");
        let plan = stale(&mut tr);
        let error = tr
            .dial(
                &plan,
                [Open, Closed],
                "true",
                |command| match hostname_of(&command).as_deref() {
                    Some("192.0.2.51") => {
                        ssh_error("kex_exchange_identification: read: Connection reset by peer\r\n")
                    }
                    _ => refused(),
                },
            )
            .unwrap_err();
        assert_eq!(error.message, "Configured LAN addresses are unreachable");
        assert!(tr.can_fallback(&error));
        assert!(!state::remembered_hosts(&tr.config).contains(&"192.0.2.51".to_owned()));
        // Older OpenSSH names the identification phase differently.
        let mut tr = fixture(&hosts, "both");
        let plan = stale(&mut tr);
        tr.dial(&plan, [Open, Open], "true", |command| {
            match hostname_of(&command).as_deref() {
                Some("192.0.2.51") => {
                    ssh_error("ssh_exchange_identification: Connection closed by remote host\r\n")
                }
                _ => ok(b""),
            }
        })
        .unwrap();
        assert_eq!(tr.info()["lan_host"], "192.0.2.50");
        // Another SSH server's host key at the remembered address: forget it,
        // and the configured address still authenticates through the pin.
        let mut tr = fixture(&hosts, "both");
        let plan = stale(&mut tr);
        tr.dial(&plan, [Open, Open], "true", |command| {
            match hostname_of(&command).as_deref() {
                Some("192.0.2.51") => ssh_error("Host key verification failed.\r\n"),
                _ => ok(b""),
            }
        })
        .unwrap();
        assert_eq!(tr.info()["route"], "lan_ssh");
        assert_eq!(tr.info()["lan_host"], "192.0.2.50");
        assert_eq!(
            tr.info()["attempted_routes"][0]["status"],
            "authentication_failed"
        );
        assert!(!state::remembered_hosts(&tr.config).contains(&"192.0.2.51".to_owned()));
        // After authentication a dropped connection may have run the command:
        // a remembered address then stops the request and is kept.
        let mut tr = fixture(&hosts, "both");
        let plan = stale(&mut tr);
        let mut dialed = 0;
        let error = tr
            .dial(&plan, [Open, Open], "true", |_| {
                dialed += 1;
                ssh_error("Connection to 192.0.2.51 closed by remote host.\r\n")
            })
            .unwrap_err();
        assert_eq!((error.code, dialed), (4, 1));
        assert!(!tr.can_fallback(&error));
        assert_eq!(state::remembered_hosts(&tr.config)[0], "192.0.2.51");
        // A configured address with a broken handshake still stops at once.
        let mut tr = fixture(&hosts, "both");
        let plan = plan_candidates(None, &[], &tr.config.hosts.clone(), None);
        let mut dialed = 0;
        let error = tr
            .dial(&plan, [Open, Open], "true", |_| {
                dialed += 1;
                ssh_error("Connection reset by 192.0.2.50 port 22\r\n")
            })
            .unwrap_err();
        assert_eq!((error.code, dialed), (4, 1));
        assert_eq!(
            tr.info()["attempted_routes"][0]["status"],
            "ssh_command_failed"
        );
    }
    #[test]
    fn a_configured_host_the_precheck_missed_still_gets_its_dial() {
        let _home = state::TestHome::new();
        // Both configured hosts answered too slowly for the precheck and the
        // alias's HostName is dead. After everything else failed, the
        // configured hosts are dialed in order; the alias HostName is not.
        let mut tr = fixture(&["192.0.2.1", "spark-b.local"], "lan-only");
        let plan = plan_candidates(None, &[], &tr.config.hosts.clone(), Some("192.0.2.9"));
        let mut dialed = Vec::new();
        tr.dial(&plan, [Closed, Unresolved, Closed], "true", |command| {
            assert_pinned(&command, "test-pin");
            let host = hostname_of(&command);
            dialed.push(host.clone());
            match host.as_deref() {
                Some("192.0.2.1") => refused(),
                _ => ok(b""),
            }
        })
        .unwrap();
        assert_eq!(
            dialed,
            [
                Some("192.0.2.1".to_owned()),
                Some("spark-b.local".to_owned())
            ]
        );
        let attempts = tr.info()["attempted_routes"].clone();
        assert_eq!(attempts.as_array().unwrap().len(), 5);
        assert_eq!(attempts[2]["source"], "alias");
        assert_eq!(attempts[2]["reason"], CLOSED_REASON);
        assert_eq!(attempts[3]["status"], "host_unreachable");
        assert_eq!(attempts[4]["status"], "authenticated");
        assert_eq!(attempts[4]["target_index"], 1);
        assert_eq!(tr.info()["lan_host"], "spark-b.local");
        assert_eq!(state::remembered_hosts(&tr.config), ["spark-b.local"]);
        // That later dial keeps the fail-closed rule of a configured host.
        let mut tr = fixture(&["192.0.2.1"], "both");
        let plan = plan_candidates(None, &[], &tr.config.hosts.clone(), None);
        let error = tr
            .dial(&plan, [Closed], "true", |_| {
                ssh_error("Host key verification failed.\r\n")
            })
            .unwrap_err();
        assert_eq!(error.code, 7);
        assert!(!tr.can_fallback(&error));
        // Only the alias HostName was held back: it is not dialed, and the
        // result is the plain fallback-eligible one.
        let mut tr = fixture(&["192.0.2.1"], "both");
        let plan = plan_candidates(None, &[], &tr.config.hosts.clone(), Some("192.0.2.9"));
        let mut dialed = Vec::new();
        let error = tr
            .dial(&plan, [Open, Closed], "true", |command| {
                dialed.push(hostname_of(&command));
                refused()
            })
            .unwrap_err();
        assert_eq!(dialed, [Some("192.0.2.1".to_owned())]);
        assert!(tr.can_fallback(&error));
    }
    #[test]
    fn an_alias_hostname_outside_the_hosts_never_blocks_fallback() {
        let _home = state::TestHome::new();
        let hosts = ["192.0.2.1"];
        let alias_plan =
            |tr: &Transport| plan_candidates(None, &[], &tr.config.hosts, Some("192.0.2.9"));
        // Before rediscovery only 192.0.2.1 was dialed, and its refusal fell
        // back. A rejection or a broken handshake at the alias's HostName
        // keeps that result; the attempt still reports what happened.
        for (stderr, status, reason) in [
            (
                "Host key verification failed.\r\n",
                "authentication_failed",
                UNLISTED_REJECTED_REASON,
            ),
            (
                "jump@bastion: Permission denied (publickey).\r\n",
                "authentication_failed",
                UNLISTED_REJECTED_REASON,
            ),
            (
                "Connection closed by UNKNOWN port 65535\r\n",
                "host_unreachable",
                UNLISTED_HANDSHAKE_REASON,
            ),
        ] {
            let mut tr = fixture(&hosts, "both");
            let plan = alias_plan(&tr);
            let error = tr
                .dial(&plan, [Open, Open], "true", |command| {
                    assert_pinned(&command, "test-pin");
                    match hostname_of(&command).as_deref() {
                        Some("192.0.2.9") => ssh_error(stderr),
                        _ => refused(),
                    }
                })
                .unwrap_err();
            assert_eq!(
                error.message, "Configured LAN addresses are unreachable",
                "{stderr}"
            );
            assert!(tr.can_fallback(&error), "{stderr}");
            let attempts = tr.info()["attempted_routes"].clone();
            assert_eq!(attempts[1]["source"], "alias");
            assert_eq!(attempts[1]["status"], status);
            assert_eq!(attempts[1]["reason"], reason);
            assert_eq!(attempts[1]["target_index"], Value::Null);
        }
        // Once the pinned Spark authenticated there, a dropped connection may
        // have run the command: it stops without fallback, as for any source.
        let mut tr = fixture(&hosts, "both");
        let plan = alias_plan(&tr);
        let mut dialed = 0;
        let error = tr
            .dial(&plan, [Closed, Open], "true", |_| {
                dialed += 1;
                ssh_error("Connection to 192.0.2.9 closed by remote host.\r\n")
            })
            .unwrap_err();
        assert_eq!((error.code, dialed), (4, 1));
        assert!(!tr.can_fallback(&error));
        // A remembered alias HostName that now rejects the pin is forgotten
        // and refuses nothing; the configured host is still dialed.
        let mut tr = fixture(&hosts, "both");
        state::remember_host(&tr.config, "192.0.2.9").unwrap();
        let plan = tr.lan_plan(|_| route(Some("192.0.2.9"), true));
        assert_eq!(
            plan,
            [
                candidate(Source::Remembered, Some("192.0.2.9")),
                candidate(Source::Configured, Some("192.0.2.1")),
            ]
        );
        let mut dialed = Vec::new();
        let error = tr
            .dial(&plan, [Open, Open], "true", |command| {
                let host = hostname_of(&command);
                dialed.push(host.clone());
                match host.as_deref() {
                    Some("192.0.2.9") => ssh_error("Host key verification failed.\r\n"),
                    _ => refused(),
                }
            })
            .unwrap_err();
        assert_eq!(dialed.len(), 2);
        assert!(tr.can_fallback(&error));
        assert_eq!(
            tr.info()["attempted_routes"][0]["reason"],
            REMEMBERED_UNLISTED_REJECTED_REASON
        );
        assert!(state::remembered_hosts(&tr.config).is_empty());
        // Without configured hosts the alias is the only route, as before
        // rediscovery: a rejection stops with 7 and any other SSH failure,
        // a broken handshake included, with the non-fallback 4.
        let mut tr = fixture(&[], "both");
        let plan = tr.lan_plan(|_| panic!("no alias lookup without configured hosts"));
        assert_eq!(plan, [candidate(Source::Alias, None)]);
        let error = tr
            .dial(&plan, [Unchecked], "true", |_| {
                ssh_error("Host key verification failed.\r\n")
            })
            .unwrap_err();
        assert_eq!(error.code, 7);
        assert!(!tr.can_fallback(&error));
        let error = tr
            .dial(&plan, [Unchecked], "true", |_| {
                ssh_error("kex_exchange_identification: Connection closed by remote host\r\n")
            })
            .unwrap_err();
        assert_eq!(error.code, 4);
        assert!(!tr.can_fallback(&error));
    }
    #[test]
    fn every_attempt_at_a_configured_address_carries_its_configured_index() {
        let _home = state::TestHome::new();
        // A configured address that became remembered, then sticky, keeps
        // its configured position (compared case-insensitively), so the GUI
        // can still say which configured address was used.
        let mut tr = fixture(&["192.0.2.1", "spark-b.local"], "both");
        state::remember_host(&tr.config, "SPARK-B.LOCAL").unwrap();
        let plan = tr.lan_plan(|_| route(Some("192.0.2.9"), true));
        assert_eq!(
            plan,
            [
                candidate(Source::Remembered, Some("SPARK-B.LOCAL")),
                candidate(Source::Configured, Some("192.0.2.1")),
                candidate(Source::Alias, Some("192.0.2.9")),
            ]
        );
        tr.dial(&plan, [Open], "true", |_| ok(b"")).unwrap();
        let sticky = tr.lan_plan(|_| panic!("a sticky host needs no alias lookup"));
        tr.dial(&sticky, [Open], "true", |_| ok(b"")).unwrap();
        let attempts = tr.info()["attempted_routes"].clone();
        assert_eq!(attempts[0]["source"], "remembered");
        assert_eq!(attempts[0]["target_index"], 1);
        assert_eq!(attempts[1]["source"], "sticky");
        assert_eq!(attempts[1]["target_index"], 1);
        assert_eq!(tr.info()["lan_host"], "SPARK-B.LOCAL");
        // The alias HostName is not a configured address: no index.
        let mut alias = fixture(&["192.0.2.1"], "both");
        let plan = alias.lan_plan(|_| route(Some("192.0.2.9"), true));
        alias
            .dial(&plan, [Closed, Open], "true", |_| ok(b""))
            .unwrap();
        let attempts = alias.info()["attempted_routes"].clone();
        assert_eq!(attempts[0]["target_index"], 0);
        assert_eq!(attempts[1]["source"], "alias");
        assert_eq!(attempts[1]["target_index"], Value::Null);
        assert_eq!(alias.info()["lan_host"], "192.0.2.9");
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
        let open = [Open].into_iter().chain(std::iter::from_fn(|| {
            panic!("a later precheck was awaited before dialing an earlier candidate")
        }));
        tr.dial(&plan, open, "true", |_| ok(b"")).unwrap();
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
        assert_eq!(first, Some(Open));
    }
    #[test]
    fn a_request_returns_without_waiting_for_a_dead_later_precheck() {
        let _home = state::TestHome::new();
        let mut tr = fixture(&["192.0.2.1", "192.0.2.2"], "both");
        let plan = plan_candidates(None, &[], &tr.config.hosts.clone(), None);
        let hosts: Vec<Option<&str>> = plan.iter().map(|c| c.host.as_deref()).collect();
        // The later address stays silent for the whole connect budget, as an
        // unplugged interface or a dead DHCP lease does.
        let silent = |host: String, _until: Instant| {
            if host == "192.0.2.2" {
                thread::sleep(PRECHECK_CONNECT);
                return Closed;
            }
            Open
        };
        let started = Instant::now();
        let out = with_probe(&hosts, PRECHECK_DEADLINE, silent, |reach| {
            tr.dial(&plan, reach, "true", |_| ok(b"ok"))
        })
        .unwrap();
        let elapsed = started.elapsed();
        assert_eq!(out, b"ok");
        assert_eq!(tr.info()["lan_host"], "192.0.2.1");
        assert!(
            elapsed < Duration::from_millis(500),
            "returned after {elapsed:?}"
        );
        // A probe that never reports in time leaves the candidate to OpenSSH.
        let started = Instant::now();
        let stuck = |_host: String, until: Instant| {
            thread::sleep(until.saturating_duration_since(Instant::now()) + 2 * PRECHECK_GRACE);
            Closed
        };
        assert_eq!(
            with_probe(
                &[Some("192.0.2.1")],
                Duration::from_millis(50),
                stuck,
                |reach| reach.collect::<Vec<_>>()
            ),
            [Unchecked]
        );
        assert!(started.elapsed() < Duration::from_millis(50) + 2 * PRECHECK_GRACE);
    }
}
