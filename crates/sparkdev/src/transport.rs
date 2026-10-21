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
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
/// OpenSSH ConnectTimeout, in its unit of whole seconds, of a candidate's
/// dial in planned order: the budget every configured host had before
/// rediscovery.
pub const CONNECT_TIMEOUT_SECS: u64 = 5;
/// ConnectTimeout of the later dial of a configured host the precheck held
/// back: what the precheck deadline left of CONNECT_TIMEOUT_SECS, so the
/// check plus this dial never cost that host more than its one dial did
/// before rediscovery. The host still gets a fresh SYN after the check's own.
pub const HELD_BACK_CONNECT_TIMEOUT_SECS: u64 = CONNECT_TIMEOUT_SECS - PRECHECK_DEADLINE.as_secs();
const CLOSED_REASON: &str = "The SSH port refused or did not answer within the 1.5s precheck";
const UNRESOLVED_REASON: &str = "The address resolved to nothing during the precheck";
const UNREACHABLE_REASON: &str =
    "SSH reported connection refusal, timeout, unreachable network or DNS failure";
const REMEMBERED_REJECTED_REASON: &str = "SSH authentication or host pin rejected at a remembered address; forgotten, next address tried, no fallback";
const REMEMBERED_UNLISTED_REJECTED_REASON: &str = "SSH authentication or host pin rejected at a remembered address that is not a configured host; forgotten, next address tried";
const UNLISTED_REJECTED_REASON: &str = "SSH authentication or host pin rejected at the alias's HostName, which is not a configured host; next address tried";
const REMEMBERED_MISMATCH_REASON: &str = "Remembered address answered with an incompatible SSH handshake before authentication; forgotten as stale";
const UNLISTED_HANDSHAKE_REASON: &str =
    "The alias's HostName failed before SSH authentication; next address tried";
const UNLISTED_TIMEOUT_REASON: &str =
    "The alias's HostName timed out before SSH authentication; next address tried";
const PRE_AUTH_FAILED_REASON: &str =
    "SSH failed before authentication without evidence that the LAN host is unreachable";
const POST_AUTH_FAILED_REASON: &str = "SSH failed after authentication or the remote command failed; it may have run, so no other address is tried";
const HANDSHAKE_DROPPED_REASON: &str = "SSH connection dropped in the identification exchange, before authentication; nothing ran remotely";
const SESSION_DROPPED_REASON: &str = "SSH connection dropped without evidence that it was still before authentication; the command may have run remotely";
const COMMAND_FAILED: &str =
    "SSH command failed; inspect the configured service and key (stderr withheld)";
/// The fallback-eligible result: no address answered.
pub const LAN_UNREACHABLE: &str = "Configured LAN addresses are unreachable";
/// Also fallback-eligible: every drop was proven to be before authentication.
pub const LAN_HANDSHAKE_DROPPED: &str = "SSH connections to the Spark dropped during the handshake, before authentication; nothing ran remotely. The network link looks unstable.";
/// A drop that may have run the command, which was not repeated.
pub const SESSION_UNSAFE: &str = "SSH connection to the Spark dropped after the command started; it may have run remotely and was not repeated. Check its result before retrying.";
/// A drop that may have run a request the controller can safely receive again.
pub const SESSION_REPEATABLE: &str = "SSH connection to the Spark dropped mid-request; it may have completed remotely and is safe to repeat (run and prepare: reuse the same request ID).";
/// Whether `error` says that nothing on the LAN served the request and
/// nothing ran remotely, so the paired transport may be tried.
fn lan_unavailable(error: &AppError) -> bool {
    error.code == 4 && (error.message == LAN_UNREACHABLE || error.message == LAN_HANDSHAKE_DROPPED)
}
/// Whether `error` reports a dropped SSH connection.
pub fn is_dropped(error: &AppError) -> bool {
    error.code == 4
        && [LAN_HANDSHAKE_DROPPED, SESSION_UNSAFE, SESSION_REPEATABLE]
            .contains(&error.message.as_str())
}
const TRANSPORT_TIMEOUT: &str =
    "Transport timeout; request may have completed remotely. Retry using the same request ID.";
fn refused_error() -> AppError {
    AppError::new(
        7,
        "SSH authentication or pinned host-key verification failed; fallback refused",
    )
}
/// Whether a request may be sent again after its connection dropped where
/// the command may already have run. See `repeat_for`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repeat {
    /// Only a drop proven to be before authentication, when nothing ran, is
    /// repeated.
    HandshakeOnly,
    /// The Spark controller treats a repeat of this request as the same
    /// request, so a drop at any phase may be repeated.
    Idempotent,
}
/// Controller requests a repeat cannot duplicate: reads, content-addressed
/// uploads and syncs, lease updates keyed by their lease, cancellation, and
/// submissions and claims keyed by a request ID. Anything else, including an
/// unknown operation, a resource policy write and every raw command, is
/// `HandshakeOnly`.
pub fn repeat_for(request: &Value) -> Repeat {
    let request_id = request["request_id"]
        .as_str()
        .is_some_and(|id| !id.is_empty());
    match request["op"].as_str() {
        Some(
            "missing" | "put" | "put_many" | "sync" | "status" | "logs" | "wait" | "doctor"
            | "cancel" | "heartbeat" | "release",
        ) => Repeat::Idempotent,
        Some("resources") if request.get("limits").is_none() => Repeat::Idempotent,
        Some("run" | "prepare" | "claim") if request_id => Repeat::Idempotent,
        _ => Repeat::HandshakeOnly,
    }
}
/// The time a dial is left for its own work on top of any server-side wait:
/// a repeat, or another candidate's dial after a drop, needs at least this.
pub const MIN_RETRY_WINDOW: Duration = Duration::from_secs(10);
/// The one repeat of a dropped dial that a request may spend.
#[derive(Clone, Debug)]
pub struct Retry {
    repeat: Repeat,
    /// Taken before the plan and the precheck. After a drop no dial runs
    /// past it.
    deadline: Instant,
    /// Time a dial needs: after a drop, no dial starts with less left.
    min_left: Duration,
    backoff: Duration,
    spent: bool,
}
/// One second plus up to 499 ms of jitter, so requests that lost the same
/// link at once do not redial in lockstep.
fn backoff() -> Duration {
    let jitter = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|now| now.subsec_millis() % 500)
        .unwrap_or(0);
    Duration::from_secs(1) + Duration::from_millis(jitter.into())
}
impl Retry {
    /// For a controller request sent with `timeout` per dial: its deadline
    /// is one dial's timeout from now, and a dial needs the server-side wait
    /// the request asks for plus MIN_RETRY_WINDOW.
    pub fn rpc(request: &Value, timeout: Duration) -> Self {
        let wait = Duration::from_secs(request["timeout"].as_u64().unwrap_or(0).min(60));
        Self {
            repeat: repeat_for(request),
            deadline: Instant::now() + timeout,
            min_left: wait + MIN_RETRY_WINDOW,
            backoff: backoff(),
            spent: false,
        }
    }
    /// For a raw command, which is never known to be safe to repeat.
    pub fn raw(timeout: Duration) -> Self {
        Self {
            repeat: Repeat::HandshakeOnly,
            deadline: Instant::now() + timeout,
            min_left: MIN_RETRY_WINDOW,
            backoff: backoff(),
            spent: false,
        }
    }
    /// No repeat at all, and no deadline that could cut a dial short.
    #[cfg(test)]
    fn none() -> Self {
        Self {
            repeat: Repeat::HandshakeOnly,
            deadline: Instant::now() + Duration::from_secs(24 * 60 * 60),
            min_left: Duration::ZERO,
            backoff: Duration::ZERO,
            spent: true,
        }
    }
    fn left(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
    fn decide(&self, phase: Phase) -> Decision {
        if self.spent {
            Decision::Exhausted
        } else if phase == Phase::Session && self.repeat == Repeat::HandshakeOnly {
            Decision::NotSafeToRepeat
        } else if self.left() < self.backoff + self.min_left {
            Decision::InsufficientTime
        } else {
            Decision::Scheduled
        }
    }
}
/// What became of a dropped dial's repeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decision {
    Scheduled,
    Exhausted,
    NotSafeToRepeat,
    InsufficientTime,
}
impl Decision {
    fn name(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Exhausted => "exhausted",
            Self::NotSafeToRepeat => "not_safe_to_repeat",
            Self::InsufficientTime => "insufficient_time",
        }
    }
}
/// What one exchange's passes over the candidates have learned so far.
struct Dials {
    retry: Retry,
    /// Each dial's process timeout, before any drop.
    timeout: Duration,
    /// A connection dropped in this exchange: every later dial is bounded
    /// by the deadline.
    dropped: bool,
    /// A configured host rejected the pin or login: no fallback.
    refused: bool,
    /// A candidate's last dial dropped in the handshake: nothing ran remotely.
    handshake_dropped: bool,
}
impl Dials {
    fn new(retry: Retry, timeout: Duration) -> Self {
        Self {
            retry,
            timeout,
            dropped: false,
            refused: false,
            handshake_dropped: false,
        }
    }
    /// The process timeout of the next dial.
    fn window(&self) -> Duration {
        if self.dropped {
            self.timeout.min(self.retry.left())
        } else {
            self.timeout
        }
    }
    /// After a drop, another candidate is not dialed without the time a
    /// dial needs.
    fn out_of_time(&self) -> bool {
        self.dropped && self.retry.left() < self.retry.min_left
    }
    /// The result when a drop that may have run the command ends the request.
    fn session_dropped(&self) -> AppError {
        AppError::new(
            4,
            match self.retry.repeat {
                Repeat::Idempotent => SESSION_REPEATABLE,
                Repeat::HandshakeOnly => SESSION_UNSAFE,
            },
        )
    }
    /// The result when no candidate served the request.
    fn unavailable(&self) -> AppError {
        if self.refused {
            refused_error()
        } else if self.handshake_dropped {
            AppError::new(4, LAN_HANDSHAKE_DROPPED)
        } else {
            AppError::new(4, LAN_UNREACHABLE)
        }
    }
}
// What one dial means for the request.
enum Outcome {
    /// Authenticated; the remote command's stdout.
    Done(Vec<u8>),
    /// Nothing ran remotely; the next candidate is dialed.
    Next,
    /// Nothing ran remotely, and the failure ends the request as it did
    /// before rediscovery, once the configured hosts held back ahead of this
    /// candidate have had their dial.
    Ends(AppError),
    /// Stops at once: the command may have run, or the local process failed.
    Stop(AppError),
}
/// The TCP reachability hint for one candidate. `Closed` and `Unresolved`
/// hold a candidate back: a configured host is dialed later, before any
/// later candidate's failure can end the request and before giving up, and
/// an alias HostName outside the configured hosts is not dialed. The others
/// are dialed in order through the pinned `command()`, and an answering port
/// is never trust.
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
/// When a connection dropped, as far as stderr can prove it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// In the identification exchange, before authentication: nothing ran.
    Handshake,
    /// Nothing proves the drop came before authentication, so the command
    /// may have run remotely.
    Session,
}
impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Self::Handshake => "handshake",
            Self::Session => "session",
        }
    }
}
/// How one failed dial ended. See `classify`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The pinned host key or the login was rejected.
    Rejected,
    /// OpenSSH could not reach the address: refusal, timeout, no route or DNS.
    Unreachable,
    /// Something answered that is not a compatible SSH server.
    Mismatch,
    /// The connection broke off.
    Dropped(Phase),
    /// Anything else, including a remote command's own exit status.
    CommandFailed,
}
impl Failure {
    /// The `attempted_routes` status of a dial that failed this way.
    pub fn status(self) -> &'static str {
        match self {
            Self::Rejected => "authentication_failed",
            Self::Unreachable => "host_unreachable",
            Self::Dropped(_) => "connection_dropped",
            Self::Mismatch | Self::CommandFailed => "ssh_command_failed",
        }
    }
}
const AUTH_MARKERS: &[&str] = &[
    "host key verification failed",
    "remote host identification has changed",
    "permission denied",
    "bad permissions",
    "no matching host key",
];
const UNREACHABLE_MARKERS: &[&str] = &[
    "connection timed out",
    "connection refused",
    "no route to host",
    "network is unreachable",
    "could not resolve hostname",
];
/// OpenSSH's names for the identification (banner) exchange, which comes
/// before key exchange and authentication; "ssh_exchange_identification" is
/// the name before OpenSSH 8.x.
const TAG_MARKERS: &[&str] = &[
    "kex_exchange_identification",
    "ssh_exchange_identification",
    "banner exchange",
];
/// The link itself broke, in Unix and Windows wordings.
const LINK_MARKERS: &[&str] = &[
    "connection reset",
    "connection closed",
    "connection aborted",
    "broken pipe",
    "software caused connection abort",
];
/// Besides LINK_MARKERS, what OpenSSH prints when an open connection is lost.
const LOST_MARKERS: &[&str] = &[
    "closed by remote host",
    "read from remote host",
    "client_loop",
    "timeout, server",
    "corrupted mac",
    "message authentication code incorrect",
    "bad packet length",
];
fn has_any(message: &str, markers: &[&str]) -> bool {
    markers.iter().any(|marker| message.contains(marker))
}
/// What OpenSSH itself wrote to stderr, without the text a device on the SSH
/// port controls before the pinned host key is checked. Measured with
/// OpenSSH 9.5 (Windows) against a loopback peer, that text reaches stderr
/// verbatim in three messages: a malformed banner in "Bad remote protocol
/// version identification: '...'", the text of the peer's disconnect message
/// after "Received disconnect from <ip> port <n>:<code>: ", and the peer's
/// algorithm list after "Their offer: " in "Unable to negotiate with ...".
/// The banner cannot hold a line break, so that one line is dropped. The
/// other two pass CR and LF through and can forge whole lines after them, so
/// the text ends where they begin; OpenSSH exits right after either one.
/// `message` is lowercased stderr. Every marker search reads only this.
pub fn openssh_own(message: &str) -> String {
    let mut own = String::new();
    for line in message.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.contains("bad remote protocol version identification") {
            continue;
        }
        if line.contains("received disconnect from") {
            break;
        }
        if line.contains("unable to negotiate with") {
            own.push_str(line.split(" their offer:").next().unwrap_or(line));
            own.push('\n');
            break;
        }
        own.push_str(line);
        own.push('\n');
    }
    own
}
fn lost(message: &str) -> bool {
    has_any(message, LINK_MARKERS)
        || has_any(message, LOST_MARKERS)
        || (message.contains("connection to ") && message.contains(" timed out"))
}
/// Classify a failed dial from its exit code, stdout and lowercased stderr.
/// Only what OpenSSH itself wrote counts (see `openssh_own`): text a device
/// on the SSH port echoes before the host-key check never decides a result.
///
/// An authentication or host-key marker is a rejection at any exit code.
/// Otherwise only OpenSSH's own failure (exit 255) can be unreachability or
/// a drop; a remote command's exit status is `CommandFailed`, whatever it
/// printed. The authentication line (see `authenticated`) or any stdout
/// proves the session phase, so text a remote command writes can never look
/// like a handshake drop. Without that proof, a drop still counts as a
/// handshake drop only when OpenSSH tagged it with the identification
/// exchange: OpenSSH prints the untagged "Connection reset by <ip> port <n>"
/// and "Connection closed by <ip> port <n>" from one template in every
/// phase, so those count as session drops.
pub fn classify(code: Option<i32>, stdout: &[u8], message: &str) -> Failure {
    let message = &openssh_own(message);
    if has_any(message, AUTH_MARKERS) {
        return Failure::Rejected;
    }
    if code != Some(255) {
        return Failure::CommandFailed;
    }
    let tag = has_any(message, TAG_MARKERS);
    let unreachable = has_any(message, UNREACHABLE_MARKERS);
    if authenticated(message) || !stdout.is_empty() {
        return if lost(message) || tag || unreachable {
            Failure::Dropped(Phase::Session)
        } else {
            Failure::CommandFailed
        };
    }
    if lost(message) && !tag {
        return Failure::Dropped(Phase::Session);
    }
    if unreachable {
        return Failure::Unreachable;
    }
    if tag && has_any(message, LINK_MARKERS) {
        return Failure::Dropped(Phase::Handshake);
    }
    if tag || message.contains("unable to negotiate with") {
        return Failure::Mismatch;
    }
    Failure::CommandFailed
}
/// Positive evidence that OpenSSH authenticated. Every dial runs with
/// `LogLevel=VERBOSE`, at which OpenSSH writes a line starting "Authenticated
/// to <host> ... using ..." (or "... (via proxy) using ...") to stderr when
/// user authentication succeeds, before it opens the session that runs the
/// command. Only such a line that OpenSSH wrote itself counts, never the same
/// words inside a peer's banner or disconnect text. `message` is lowercased
/// stderr.
fn authenticated(message: &str) -> bool {
    openssh_own(message)
        .lines()
        .any(|line| line.starts_with("authenticated to "))
}
/// OpenSSH's own failure (exit 255) with no stdout and without the
/// authentication line: it ended before authentication, so no remote command
/// ran and dialing another address cannot duplicate work. A remote command's
/// own exit status, any stdout, or the authentication line means the command
/// may have run.
fn before_auth(code: Option<i32>, stdout: &[u8], message: &str) -> bool {
    code == Some(255) && stdout.is_empty() && !authenticated(message)
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
/// How a child ended: it exited, or it was killed at the timeout. A killed
/// child's stderr is kept only when the pipe closed within STDERR_GRACE
/// after the kill, so what is kept is complete; a grandchild such as a
/// ProxyCommand can hold the pipe open, and then it is None.
#[derive(Debug, PartialEq, Eq)]
pub enum Ran {
    Exited(Finished),
    TimedOut(Option<Vec<u8>>),
}
const STDERR_GRACE: Duration = Duration::from_millis(500);
pub fn process(command: Command, input: &[u8], timeout: Duration) -> Result<Finished> {
    match run_process(command, input, timeout)? {
        Ran::Exited(finished) => Ok(finished),
        Ran::TimedOut(_) => Err(AppError::new(4, TRANSPORT_TIMEOUT)),
    }
}
pub fn run_process(mut command: Command, input: &[u8], timeout: Duration) -> Result<Ran> {
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
    let (errors, error_output) = mpsc::channel();
    thread::spawn(move || {
        let mut b = Vec::new();
        let _ = stderr.by_ref().take(64 * 1024).read_to_end(&mut b);
        let _ = errors.send(b);
    });
    let started = Instant::now();
    let code = loop {
        if let Some(status) = child.try_wait()? {
            break status.code();
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Ran::TimedOut(error_output.recv_timeout(STDERR_GRACE).ok()));
        }
        thread::sleep(Duration::from_millis(25));
    };
    let _ = writer.join();
    Ok(Ran::Exited((
        code,
        reader.join().unwrap_or_default(),
        error_output.recv().unwrap_or_default(),
    )))
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
    fn record_candidate(
        &mut self,
        candidate: &Candidate,
        status: &str,
        reason: &str,
    ) -> &mut Value {
        let target = candidate
            .host
            .as_deref()
            .and_then(|host| self.configured_index(host));
        self.push_attempt(json!({"route":"lan_ssh","attempted":true,"status":status,"reason":reason,"target_index":target,"source":candidate.source.name(),"host":candidate.host}));
        self.attempts
            .last_mut()
            .expect("an attempt was just recorded")
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
        self.dial_command(host, remote, CONNECT_TIMEOUT_SECS)
    }
    /// The one pinned ssh command line; only the connect budget varies.
    /// `LogLevel=VERBOSE` adds OpenSSH's authentication line to the stderr
    /// that is read for classification and never shown or stored.
    fn dial_command(&self, host: Option<&str>, remote: &str, connect_timeout: u64) -> Command {
        let mut command = Command::new("ssh");
        command.args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "LogLevel=VERBOSE",
        ]);
        command
            .arg("-o")
            .arg(format!("ConnectTimeout={connect_timeout}"));
        command.args([
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
    /// Run `remote` over pinned LAN SSH with `input` on its stdin. A raw
    /// command is never known to be safe to repeat, so only a drop proven
    /// to be before authentication is dialed again.
    pub fn ssh_raw(&mut self, remote: &str, input: &[u8], timeout: Duration) -> Result<Vec<u8>> {
        self.exchange(
            remote,
            timeout,
            Retry::raw(timeout),
            |command, window| run_process(command, input, window),
            thread::sleep,
        )
    }
    /// One LAN request: plan, precheck and dial the candidates. Each dial
    /// runs through `run` with its process timeout; `pause` waits out the
    /// backoff before the one repeat `retry` may allow. `retry` carries the
    /// request deadline, taken before planning and the precheck.
    fn exchange(
        &mut self,
        remote: &str,
        timeout: Duration,
        retry: Retry,
        run: impl FnMut(Command, Duration) -> Result<Ran>,
        pause: impl FnMut(Duration),
    ) -> Result<Vec<u8>> {
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
            self.dial_with(&plan, reach, remote, timeout, retry, run, pause)
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
    /// Addresses to precheck. A plan of one candidate is not prechecked:
    /// nothing could be dialed ahead of it, so a check could only add its own
    /// time to the dial that candidate had before rediscovery. When
    /// ProxyJump, ProxyCommand or a bind option routes the alias, ssh reaches
    /// every candidate through that route, so a direct local connect or
    /// lookup would test the wrong path (and expose names the proxy keeps
    /// private): nothing is prechecked.
    fn precheck_targets<'a>(&self, plan: &'a [Candidate]) -> Vec<Option<&'a str>> {
        let direct = self.direct && plan.len() > 1;
        plan.iter()
            .map(|c| c.host.as_deref().filter(|_| direct))
            .collect()
    }
    /// Dial prechecked candidates in planned order through the pinned
    /// `command()`; `reach` is pulled lazily, one per candidate. See `attempt`
    /// for what each failure means, and for the one repeat of a dropped dial
    /// that `retry` allows per request.
    ///
    /// A configured host the precheck held back (`Closed` or `Unresolved`)
    /// keeps its place ahead of later candidates for every failure that can
    /// end the request without anything having run remotely: before such a
    /// failure at a later candidate ends it, and before giving up, each
    /// held-back configured host gets one dial, in planned order. So a
    /// precheck false negative (a first SYN or ARP over 1.5s, a flaky
    /// resolver) never lets a later host's refusal or broken handshake end a
    /// request the earlier host could have served. A failure after
    /// authentication, a drop that may have run the command, or a timeout
    /// still stops at once.
    ///
    /// Each dial's process timeout is `timeout`. After any drop in this
    /// exchange it is also bounded by the request deadline, and no further
    /// candidate is dialed once less than the time a dial needs is left.
    #[allow(clippy::too_many_arguments)]
    fn dial_with(
        &mut self,
        plan: &[Candidate],
        reach: impl IntoIterator<Item = Reach>,
        remote: &str,
        timeout: Duration,
        retry: Retry,
        mut run: impl FnMut(Command, Duration) -> Result<Ran>,
        mut pause: impl FnMut(Duration),
    ) -> Result<Vec<u8>> {
        let mut dials = Dials::new(retry, timeout);
        let mut held_back = Vec::new();
        let mut reach = reach.into_iter();
        for candidate in plan {
            if dials.out_of_time() {
                break;
            }
            let Some(reach) = reach.next() else { break };
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
            match self.attempt(
                candidate,
                CONNECT_TIMEOUT_SECS,
                remote,
                &mut dials,
                &mut run,
                &mut pause,
            ) {
                Outcome::Done(out) => return Ok(out),
                Outcome::Next => {}
                Outcome::Stop(error) => return Err(error),
                Outcome::Ends(error) => {
                    return match self
                        .redial(&held_back, remote, &mut dials, &mut run, &mut pause)?
                    {
                        Some(out) => Ok(out),
                        None => Err(error),
                    };
                }
            }
        }
        if let Some(out) = self.redial(&held_back, remote, &mut dials, &mut run, &mut pause)? {
            return Ok(out);
        }
        Err(dials.unavailable())
    }
    /// The later dial of each held-back configured host, in planned order,
    /// with what the precheck left of the connect budget. The first of them
    /// whose failure ends the request decides it, as the earliest such host
    /// did before rediscovery.
    fn redial(
        &mut self,
        held_back: &[&Candidate],
        remote: &str,
        dials: &mut Dials,
        run: &mut impl FnMut(Command, Duration) -> Result<Ran>,
        pause: &mut impl FnMut(Duration),
    ) -> Result<Option<Vec<u8>>> {
        for candidate in held_back {
            if dials.out_of_time() {
                break;
            }
            match self.attempt(
                candidate,
                HELD_BACK_CONNECT_TIMEOUT_SECS,
                remote,
                dials,
                &mut *run,
                &mut *pause,
            ) {
                Outcome::Done(out) => return Ok(Some(out)),
                Outcome::Next => {}
                Outcome::Ends(error) | Outcome::Stop(error) => return Err(error),
            }
        }
        Ok(None)
    }
    /// A LAN receipt for one dial; a repeat says which try it was.
    fn record_try(
        &mut self,
        candidate: &Candidate,
        attempt: u32,
        status: &str,
        reason: &str,
    ) -> &mut Value {
        let record = self.record_candidate(candidate, status, reason);
        if attempt > 1 {
            record["try"] = json!(attempt);
        }
        record
    }
    /// Dial one candidate, repeating a dropped dial once if `dials.retry`
    /// allows it.
    ///
    /// Whether anything ran remotely decides what may follow (see
    /// `classify`). A rejection, unreachability or an incompatible server
    /// before authentication ran nothing; so did a drop OpenSSH tagged with
    /// the identification exchange, which moves on and leaves every address
    /// remembered, since a reset is no evidence that an address is stale.
    /// Any other drop may have run the command. Any other failure after
    /// authentication, including a remote command's own exit status, stops
    /// the request at once with 4 for every source; so does a timeout, except
    /// as noted below, and a local process error is returned as-is.
    ///
    /// A dropped dial is repeated once per request, at this candidate, with
    /// the identical pinned command, after the backoff: a handshake drop for
    /// any request, and a drop that may have run the command only for an
    /// `Idempotent` request, in both cases only when the backoff plus the time
    /// a dial needs is left before the deadline. A rejection, timeout or
    /// process error is never repeated. Once any try here dropped where the
    /// command may have run, the request ends at this candidate whatever the
    /// last try showed: 7 for a rejection before authentication, otherwise 4
    /// saying whether the request is safe to repeat. It never moves to
    /// another candidate or the fallback, and keeps the address remembered
    /// and the sticky host unchanged.
    ///
    /// Before authentication, reachability failures always move on. A
    /// configured host, the alias left to OpenSSH, or a sticky configured
    /// host keeps the semantics it had before rediscovery: an authentication
    /// or host-key rejection ends the request with 7 (no fallback), an
    /// incompatible server or other failure with 4, after the held-back hosts
    /// ahead of it (see `dial_with`).
    ///
    /// A remembered address is only a learned hint. When it rejects the pin
    /// or login, or answers with an incompatible SSH server, it is forgotten
    /// and the next candidate dialed, still through the same pin. A rejection
    /// at a configured host (remembered or not) still refuses fallback:
    /// unless another candidate authenticates, the request fails with 7.
    ///
    /// The alias's HostName outside the configured hosts was never dialed
    /// before rediscovery, so it may only add a way to succeed: any failure
    /// there that ran nothing (a rejection, an incompatible server, too many
    /// authentication failures) moves on, and so does a timeout whose
    /// complete stderr shows it never authenticated.
    fn attempt(
        &mut self,
        candidate: &Candidate,
        connect_timeout: u64,
        remote: &str,
        dials: &mut Dials,
        run: &mut impl FnMut(Command, Duration) -> Result<Ran>,
        pause: &mut impl FnMut(Duration),
    ) -> Outcome {
        let host = candidate.host.as_deref();
        let configured = self.dialed_before_rediscovery(host);
        let remembered = candidate.source == Source::Remembered;
        // A try here dropped where the command may have run.
        let mut session = false;
        let mut attempt = 0;
        let (failure, before) = loop {
            attempt += 1;
            let window = dials.window();
            let (code, out, err) =
                match run(self.dial_command(host, remote, connect_timeout), window) {
                    Ok(Ran::Exited(finished)) => finished,
                    Ok(Ran::TimedOut(stderr)) => {
                        let never_authenticated = stderr.is_some_and(|e| {
                            !authenticated(&String::from_utf8_lossy(&e).to_lowercase())
                        });
                        if never_authenticated && !configured && !session {
                            self.record_try(
                                candidate,
                                attempt,
                                "host_unreachable",
                                UNLISTED_TIMEOUT_REASON,
                            );
                            return Outcome::Next;
                        }
                        self.record_try(candidate, attempt, "transport_timeout", TRANSPORT_TIMEOUT);
                        return Outcome::Stop(AppError::new(4, TRANSPORT_TIMEOUT));
                    }
                    Err(e) => {
                        self.record_try(candidate, attempt, "process_error", &e.message);
                        return Outcome::Stop(e);
                    }
                };
            if code == Some(0) {
                self.selected = "ssh".into();
                self.reason = "LAN SSH authenticated with the configured known-host pin".into();
                self.host = candidate.host.clone();
                self.authenticated = true;
                self.record_try(
                    candidate,
                    attempt,
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
                return Outcome::Done(out);
            }
            let message = String::from_utf8_lossy(&err).to_lowercase();
            let failure = classify(code, &out, &message);
            let before = before_auth(code, &out, &message);
            let Failure::Dropped(phase) = failure else {
                break (failure, before);
            };
            dials.dropped = true;
            session |= phase == Phase::Session;
            let decision = dials.retry.decide(phase);
            let reason = match phase {
                Phase::Handshake => HANDSHAKE_DROPPED_REASON,
                Phase::Session => SESSION_DROPPED_REASON,
            };
            let record = self.record_try(candidate, attempt, failure.status(), reason);
            record["phase"] = json!(phase.name());
            record["try"] = json!(attempt);
            record["retry"] = json!(decision.name());
            if decision != Decision::Scheduled {
                break (failure, before);
            }
            dials.retry.spent = true;
            pause(dials.retry.backoff);
        };
        let rejected = failure == Failure::Rejected;
        if session {
            // The last try's own receipt, unless it was the drop itself.
            if !matches!(failure, Failure::Dropped(_)) {
                let (status, reason) = match (failure, before) {
                    (_, false) => ("ssh_command_failed", POST_AUTH_FAILED_REASON),
                    (Failure::Rejected, true) => (
                        failure.status(),
                        "SSH authentication or host pin rejected; no fallback",
                    ),
                    (Failure::Unreachable, true) => (failure.status(), UNREACHABLE_REASON),
                    (_, true) => (failure.status(), PRE_AUTH_FAILED_REASON),
                };
                self.record_try(candidate, attempt, status, reason);
            }
            return Outcome::Stop(if rejected && before {
                refused_error()
            } else {
                dials.session_dropped()
            });
        }
        if failure == Failure::Dropped(Phase::Handshake) {
            dials.handshake_dropped = true;
            return Outcome::Next;
        }
        if !before {
            self.record_try(
                candidate,
                attempt,
                "ssh_command_failed",
                POST_AUTH_FAILED_REASON,
            );
            return Outcome::Stop(AppError::new(4, COMMAND_FAILED));
        }
        if failure == Failure::Unreachable {
            self.record_try(candidate, attempt, failure.status(), UNREACHABLE_REASON);
            return Outcome::Next;
        }
        // Evidence that something other than the pinned Spark now holds a
        // remembered address; a drop or another failure is not.
        let stale = remembered && (rejected || failure == Failure::Mismatch);
        if !configured || stale {
            let reason = match failure {
                Failure::Rejected if configured => REMEMBERED_REJECTED_REASON,
                Failure::Rejected if remembered => REMEMBERED_UNLISTED_REJECTED_REASON,
                Failure::Rejected => UNLISTED_REJECTED_REASON,
                Failure::Mismatch if remembered => REMEMBERED_MISMATCH_REASON,
                _ => UNLISTED_HANDSHAKE_REASON,
            };
            self.record_try(candidate, attempt, failure.status(), reason);
            if let (true, Some(host)) = (stale, host) {
                self.forget_remembered(host);
            }
            dials.refused |= rejected && configured;
            return Outcome::Next;
        }
        if rejected {
            self.record_try(
                candidate,
                attempt,
                failure.status(),
                "SSH authentication or host pin rejected; no fallback",
            );
            return Outcome::Ends(refused_error());
        }
        self.record_try(candidate, attempt, failure.status(), PRE_AUTH_FAILED_REASON);
        Outcome::Ends(AppError::new(4, COMMAND_FAILED))
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
        // The deadline starts now, before the plan and the precheck, and the
        // request bytes are the same on every dial.
        let retry = Retry::rpc(&request, timeout);
        if self.config.allows_lan() && (self.selected != "iroh" || !self.config.allows_remote()) {
            match self.exchange(
                "python3 ~/sparkdev/controller.py rpc",
                timeout,
                retry,
                |command, window| run_process(command, &input, window),
                thread::sleep,
            ) {
                Ok(bytes) => return Self::decode(&bytes),
                Err(e) => {
                    if lan_unavailable(&e) && !self.config.allows_remote() {
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
        self.config.connection_enabled && self.config.allows_remote() && lan_unavailable(error)
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
            "LogLevel=VERBOSE",
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
            assert_eq!(connect_timeout_of(&command), Some(CONNECT_TIMEOUT_SECS));
            // A held-back host's later dial differs only in its budget.
            let later = tr.dial_command(host, "true", HELD_BACK_CONNECT_TIMEOUT_SECS);
            assert_pinned(&later, "test-pin");
            assert_eq!(
                connect_timeout_of(&later),
                Some(HELD_BACK_CONNECT_TIMEOUT_SECS)
            );
            assert_eq!(
                arguments(&later)
                    .iter()
                    .filter(|a| !a.starts_with("ConnectTimeout="))
                    .collect::<Vec<_>>(),
                args.iter()
                    .filter(|a| !a.starts_with("ConnectTimeout="))
                    .collect::<Vec<_>>()
            );
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
    fn ok(out: &[u8]) -> Result<Ran> {
        Ok(Ran::Exited((Some(0), out.to_vec(), Vec::new())))
    }
    /// OpenSSH's own failure: exit 255 with its message on stderr.
    fn ssh_error(stderr: &str) -> Result<Ran> {
        Ok(Ran::Exited((
            Some(255),
            Vec::new(),
            stderr.as_bytes().to_vec(),
        )))
    }
    /// The line OpenSSH 9.5 (Windows) writes at LogLevel=VERBOSE once user
    /// authentication succeeded, as observed against a loopback server.
    fn auth_line(host: &str) -> String {
        format!("Authenticated to {host} ([{host}]:22) using \"publickey\".\r\n")
    }
    /// A failure after authentication: the authentication line, then `stderr`.
    fn after_auth(host: &str, stderr: &str) -> Result<Ran> {
        ssh_error(&format!("{}{stderr}", auth_line(host)))
    }
    fn refused() -> Result<Ran> {
        ssh_error("ssh: connect to host 192.0.2.1 port 22: Connection refused\r\n")
    }
    fn connect_timeout_of(command: &Command) -> Option<u64> {
        arguments(command)
            .iter()
            .find_map(|a| a.strip_prefix("ConnectTimeout=")?.parse().ok())
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
        // not erase remembered addresses. The held-back remembered address
        // ahead of it is dialed first, as it was before rediscovery.
        let mut tr = fixture(&hosts, "both");
        let plan = tr.lan_plan(|_| AliasRoute::default());
        assert_eq!(
            plan,
            [
                candidate(Source::Remembered, Some("192.0.2.1")),
                candidate(Source::Configured, Some("192.0.2.2")),
            ]
        );
        let mut dialed = Vec::new();
        let error = tr
            .dial(&plan, [Closed, Open], "true", |command| {
                let host = hostname_of(&command);
                dialed.push(host.clone());
                match host.as_deref() {
                    Some("192.0.2.2") => {
                        ssh_error("owner@192.0.2.2: Permission denied (publickey).")
                    }
                    _ => refused(),
                }
            })
            .unwrap_err();
        assert_eq!(error.code, 7);
        assert!(!tr.can_fallback(&error));
        assert_eq!(
            dialed,
            [Some("192.0.2.2".to_owned()), Some("192.0.2.1".to_owned())]
        );
        let attempts = tr.info()["attempted_routes"].clone();
        assert_eq!(attempts.as_array().unwrap().len(), 3);
        assert_eq!(attempts[1]["source"], "configured");
        assert_eq!(attempts[1]["status"], "authentication_failed");
        assert_eq!(attempts[2]["host"], "192.0.2.1");
        assert_eq!(attempts[2]["status"], "host_unreachable");
        assert_eq!(state::remembered_hosts(&tr.config), ["192.0.2.1"]);
        // A service failure on an answering host stops without fallback.
        let mut tr = fixture(&hosts, "both");
        let mut dialed = 0;
        let error = tr
            .dial(&plan, [Open, Open], "true", |_| {
                dialed += 1;
                Ok(Ran::Exited((
                    Some(127),
                    Vec::new(),
                    format!("{}bash: python3: command not found", auth_line("192.0.2.1"))
                        .into_bytes(),
                )))
            })
            .unwrap_err();
        assert_eq!((error.code, dialed), (4, 1));
        assert!(!tr.can_fallback(&error));
        assert_eq!(
            tr.info()["attempted_routes"][0]["status"],
            "ssh_command_failed"
        );
        // A timeout at a configured address is returned as-is, whatever its
        // stderr shows, and no other address is tried.
        for stderr in [
            None,
            Some(Vec::new()),
            Some(auth_line("192.0.2.1").into_bytes()),
        ] {
            let mut tr = fixture(&hosts, "both");
            let mut dialed = 0;
            let error = tr
                .dial(&plan, [Open, Open], "true", |_| {
                    dialed += 1;
                    Ok(Ran::TimedOut(stderr.clone()))
                })
                .unwrap_err();
            assert_eq!((error.code, dialed), (4, 1));
            assert_eq!(error.message, TRANSPORT_TIMEOUT);
            assert!(!tr.can_fallback(&error));
            assert_eq!(
                tr.info()["attempted_routes"][0]["status"],
                "transport_timeout"
            );
        }
        // A local process failure is returned as-is.
        let mut tr = fixture(&hosts, "both");
        let error = tr
            .dial(&plan, [Open, Open], "true", |_| {
                Err(AppError::new(4, "Transport process could not be started"))
            })
            .unwrap_err();
        assert!(!tr.can_fallback(&error));
        assert_eq!(tr.info()["attempted_routes"][0]["status"], "process_error");
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
        let unavailable = AppError::new(4, LAN_UNREACHABLE);
        let dropped = AppError::new(4, LAN_HANDSHAKE_DROPPED);
        for error in [&unavailable, &dropped] {
            assert!(!configured("lan-only", true).can_fallback(error));
            assert!(!configured("both", false).can_fallback(error));
            assert!(configured("both", true).can_fallback(error));
        }
        let both = configured("both", true);
        for error in [
            AppError::new(7, "Host pin mismatch"),
            AppError::new(7, LAN_UNREACHABLE),
            AppError::new(4, "SSH command failed"),
            AppError::new(4, COMMAND_FAILED),
            AppError::new(4, "Transport timeout"),
            AppError::new(4, SESSION_UNSAFE),
            AppError::new(4, SESSION_REPEATABLE),
        ] {
            assert!(!both.can_fallback(&error), "{}", error.message);
        }
        for message in [LAN_HANDSHAKE_DROPPED, SESSION_UNSAFE, SESSION_REPEATABLE] {
            assert!(is_dropped(&AppError::new(4, message)));
            assert!(!is_dropped(&AppError::new(7, message)));
        }
        for message in [LAN_UNREACHABLE, COMMAND_FAILED, TRANSPORT_TIMEOUT] {
            assert!(!is_dropped(&AppError::new(4, message)));
        }
        // The plain messages never send the owner to the service and key.
        for message in [LAN_HANDSHAKE_DROPPED, SESSION_UNSAFE, SESSION_REPEATABLE] {
            assert!(!message.contains("inspect"), "{message}");
        }
    }
    #[test]
    fn classify_reads_drops_and_unreachability_only_from_openssh_itself() {
        use Failure::{CommandFailed, Dropped, Mismatch, Rejected, Unreachable};
        use Phase::{Handshake, Session};
        let auth = auth_line("192.0.2.5");
        let classified = |code: Option<i32>, stdout: &[u8], stderr: &str| {
            classify(code, stdout, &stderr.to_lowercase())
        };
        let codes = [Some(255), Some(1), Some(127), None];
        // A rejection at every exit code, even after the authentication line.
        for rejection in [
            "Host key verification failed.\r\n",
            "owner@192.0.2.5: Permission denied (publickey).\r\n",
            "@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@\r\n",
            "Bad permissions. Try removing permissions for user: OWNER\r\n",
            "Unable to negotiate with 192.0.2.5 port 22: no matching host key type found.\r\n",
        ] {
            for code in codes {
                assert_eq!(
                    classified(code, b"", rejection),
                    Rejected,
                    "{code:?} {rejection}"
                );
                let late = format!("{auth}{rejection}");
                assert_eq!(classified(code, b"{", &late), Rejected, "{code:?} {late}");
            }
        }
        // A remote command's own exit status is never unreachability or a
        // drop, whatever it printed: sparkdev-link or the controller can
        // write these words themselves.
        for code in [Some(1), Some(127), Some(70), None] {
            for remote in [
                "ssh: connect to host 192.0.2.5 port 22: Connection refused\r\n",
                "BrokenPipeError: [Errno 32] Broken pipe\r\n",
                "kex_exchange_identification: read: Connection reset by peer\r\n",
                "Connection reset by 192.0.2.5 port 22\r\n",
                "Connection to 192.0.2.5 closed by remote host.\r\n",
                "Could not resolve hostname spark.local\r\n",
                "",
            ] {
                assert_eq!(
                    classified(code, b"", remote),
                    CommandFailed,
                    "{code:?} {remote}"
                );
            }
        }
        // OpenSSH's own failure (exit 255) without the authentication line.
        for (stderr, expected) in [
            // Unix and Windows wordings of a link lost in the session, or
            // untagged: OpenSSH prints "Connection reset by" and "Connection
            // closed by" from one template in every phase.
            (
                "Read from remote host 192.0.2.5: Connection timed out\r\n",
                Dropped(Session),
            ),
            (
                "Read from remote host 192.0.2.5: Connection reset by peer\r\n",
                Dropped(Session),
            ),
            (
                "Connection reset by 192.0.2.5 port 22\r\n",
                Dropped(Session),
            ),
            (
                "Connection closed by 192.0.2.5 port 22\r\n",
                Dropped(Session),
            ),
            (
                "Connection closed by UNKNOWN port 65535\r\n",
                Dropped(Session),
            ),
            (
                "client_loop: send disconnect: Connection reset\r\n",
                Dropped(Session),
            ),
            (
                "client_loop: send disconnect: Broken pipe\r\n",
                Dropped(Session),
            ),
            (
                "Connection to 192.0.2.5 closed by remote host.\r\n",
                Dropped(Session),
            ),
            (
                "Timeout, server 192.0.2.5 not responding.\r\n",
                Dropped(Session),
            ),
            (
                "Connection to 192.0.2.5 port 22 timed out\r\n",
                Dropped(Session),
            ),
            (
                "ssh_dispatch_run_fatal: Connection to 192.0.2.5 port 22 timed out\r\n",
                Dropped(Session),
            ),
            (
                "Corrupted MAC on input.\r\nssh_dispatch_run_fatal: Connection to 192.0.2.5 port 22: message authentication code incorrect\r\n",
                Dropped(Session),
            ),
            (
                "ssh_dispatch_run_fatal: Connection to 192.0.2.5 port 22: Bad packet length 1349676916.\r\n",
                Dropped(Session),
            ),
            (
                "Write failed: Software caused connection abort\r\n",
                Dropped(Session),
            ),
            // Tagged with the identification exchange: before authentication.
            (
                "kex_exchange_identification: read: Connection reset by peer\r\nConnection reset by 192.0.2.5 port 22\r\n",
                Dropped(Handshake),
            ),
            // OpenSSH 9.5 (Windows) leaves out "by peer".
            (
                "kex_exchange_identification: read: Connection reset\r\n",
                Dropped(Handshake),
            ),
            (
                "kex_exchange_identification: Connection closed by remote host\r\nConnection closed by 192.0.2.5 port 22\r\n",
                Dropped(Handshake),
            ),
            (
                "ssh_exchange_identification: Connection closed by remote host\r\n",
                Dropped(Handshake),
            ),
            (
                "kex_exchange_identification: read: Software caused connection abort\r\n",
                Dropped(Handshake),
            ),
            (
                "banner exchange: Connection to 192.0.2.5 port 22: Broken pipe\r\n",
                Dropped(Handshake),
            ),
            // Nothing answered in time, or a ProxyCommand could not connect.
            (
                "Connection timed out during banner exchange\r\nbanner exchange: Connection to 192.0.2.5 port 22 timed out\r\n",
                Unreachable,
            ),
            (
                "nc: connect to 192.0.2.5 port 22 (tcp) failed: Connection refused\r\nkex_exchange_identification: Connection closed by remote host\r\nConnection closed by UNKNOWN port 65535\r\n",
                Unreachable,
            ),
            (
                "ssh: connect to host 192.0.2.5 port 22: Connection refused\r\n",
                Unreachable,
            ),
            (
                "ssh: connect to host 192.0.2.5 port 22: Connection timed out\r\n",
                Unreachable,
            ),
            (
                "ssh: connect to host 192.0.2.5 port 22: Network is unreachable\r\n",
                Unreachable,
            ),
            (
                "ssh: connect to host 192.0.2.5 port 22: No route to host\r\n",
                Unreachable,
            ),
            (
                "ssh: Could not resolve hostname spark.local: No such host is known. \r\n",
                Unreachable,
            ),
            // Something other than a compatible SSH server answered.
            (
                "Unable to negotiate with 192.0.2.5 port 22: no matching key exchange method found. Their offer: diffie-hellman-group1-sha1\r\n",
                Mismatch,
            ),
            (
                "banner exchange: Connection to 192.0.2.5 port 22: invalid format\r\n",
                Mismatch,
            ),
            (
                "kex_exchange_identification: banner line contains invalid characters\r\n",
                Mismatch,
            ),
            // No evidence of either.
            ("", CommandFailed),
            (
                "Received disconnect from 192.0.2.5 port 22:2: Too many authentication failures\r\nDisconnected from 192.0.2.5 port 22\r\n",
                CommandFailed,
            ),
        ] {
            assert_eq!(classified(Some(255), b"", stderr), expected, "{stderr}");
        }
        // The authentication line or any stdout proves the session phase:
        // text written by the remote never reads as a handshake drop or as
        // unreachability.
        let proxied = "Authenticated to 192.0.2.5 (via proxy) using \"publickey\".\r\n";
        for (stderr, expected) in [
            (
                "kex_exchange_identification: read: Connection reset\r\n",
                Dropped(Session),
            ),
            (
                "Connection reset by 192.0.2.5 port 22\r\n",
                Dropped(Session),
            ),
            (
                "Connection closed by 192.0.2.5 port 22\r\n",
                Dropped(Session),
            ),
            (
                "ssh: connect to host 192.0.2.5 port 22: Connection refused\r\n",
                Dropped(Session),
            ),
            (
                "banner exchange: Connection to 192.0.2.5 port 22: invalid format\r\n",
                Dropped(Session),
            ),
            (
                "Unable to negotiate with 192.0.2.5 port 22: no matching cipher found\r\n",
                CommandFailed,
            ),
            ("bash: python3: command not found\r\n", CommandFailed),
            ("", CommandFailed),
        ] {
            for prefix in [auth.as_str(), proxied] {
                let message = format!("{prefix}{stderr}");
                assert_eq!(classified(Some(255), b"", &message), expected, "{message}");
            }
            assert_eq!(
                classified(Some(255), b"{", stderr),
                expected,
                "stdout {stderr}"
            );
        }
        // Before the host-key check a device on the SSH port controls some
        // of stderr: a malformed banner, the text of a disconnect message and
        // its algorithm list. These were captured from OpenSSH 9.5 (Windows)
        // against a loopback peer; the disconnect text keeps CR and LF, so
        // it can forge whole lines. None of it decides the result: a dial
        // that never authenticated never reads as one that may have run.
        let echoes = [
            (
                "Bad remote protocol version identification: 'SSH-2.0x authenticated to x connection reset'\r\nbanner exchange: Connection to 127.0.0.1 port 55420: invalid format\r\n",
                Mismatch,
            ),
            (
                "Received disconnect from 127.0.0.1 port 55422:2: x authenticated to x connection reset\r\nDisconnected from 127.0.0.1 port 55422\r\n",
                CommandFailed,
            ),
            (
                "Received disconnect from 127.0.0.1 port 63404:2: x\r\nAuthenticated to 192.0.2.5 ([192.0.2.5]:22) using \"publickey\".\r\nConnection reset by 192.0.2.5 port 22\r\nDisconnected from 127.0.0.1 port 63404\r\n",
                CommandFailed,
            ),
            (
                "Received disconnect from 127.0.0.1 port 55451:2: Permission denied (publickey). remote host identification has changed\r\nDisconnected from 127.0.0.1 port 55451\r\n",
                CommandFailed,
            ),
            (
                "Unable to negotiate with 192.0.2.5 port 22: no matching key exchange method found. Their offer: x\r\nAuthenticated to 192.0.2.5 ([192.0.2.5]:22) using \"publickey\".\r\nConnection reset by 192.0.2.5 port 22\r\n",
                Mismatch,
            ),
            (
                "Unable to negotiate with 192.0.2.5 port 22: no matching key exchange method found. Their offer: host key verification failed, connection reset\r\n",
                Mismatch,
            ),
        ];
        for (stderr, expected) in echoes {
            assert_eq!(classified(Some(255), b"", stderr), expected, "{stderr}");
            assert!(
                before_auth(Some(255), b"", &stderr.to_lowercase()),
                "{stderr}"
            );
        }
        // OpenSSH's own part of a failed negotiation still counts.
        assert_eq!(
            classified(
                Some(255),
                b"",
                "Unable to negotiate with 192.0.2.5 port 22: no matching host key type found. Their offer: ssh-dss\r\n"
            ),
            Rejected
        );
        // The authentication line counts only at the start of a line.
        assert!(before_auth(
            Some(255),
            b"",
            "kex_exchange_identification: x authenticated to 192.0.2.5\r\n"
        ));
        assert_eq!(
            openssh_own("a\r\nbad remote protocol version identification: 'x'\r\nb\r\nreceived disconnect from x\r\nc\r\n"),
            "a\nb\n"
        );
        assert_eq!(Rejected.status(), "authentication_failed");
        assert_eq!(Unreachable.status(), "host_unreachable");
        assert_eq!(Dropped(Handshake).status(), "connection_dropped");
        assert_eq!(Dropped(Session).status(), "connection_dropped");
        assert_eq!(Mismatch.status(), "ssh_command_failed");
        assert_eq!(CommandFailed.status(), "ssh_command_failed");
        // Only OpenSSH's own failure with no stdout and no authentication
        // line ended before authentication.
        assert!(before_auth(Some(255), b"", ""));
        assert!(before_auth(
            Some(255),
            b"",
            "connection reset by 192.0.2.5 port 22"
        ));
        assert!(!before_auth(Some(1), b"", ""));
        assert!(!before_auth(None, b"", ""));
        assert!(!before_auth(Some(255), b"{", ""));
        assert!(!before_auth(Some(255), b"", &auth.to_lowercase()));
        assert!(!before_auth(Some(255), b"", &proxied.to_lowercase()));
    }
    #[test]
    fn a_remembered_address_is_forgotten_only_on_evidence_that_it_is_stale() {
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
        let kept =
            |tr: &Transport| state::remembered_hosts(&tr.config).contains(&"192.0.2.51".to_owned());
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
        assert_eq!(attempts[0]["status"], "ssh_command_failed");
        assert_eq!(attempts[0]["reason"], REMEMBERED_MISMATCH_REASON);
        assert_eq!(attempts[1]["status"], "authenticated");
        assert_eq!(state::remembered_hosts(&tr.config), ["192.0.2.50"]);
        // A reset in the identification exchange ran nothing, but it is no
        // evidence that the address is stale: on a flaky link it is the
        // good address. It is kept, and with nothing else answering the
        // result is fallback-eligible.
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
        assert_eq!(
            (error.code, error.message.as_str()),
            (4, LAN_HANDSHAKE_DROPPED)
        );
        assert!(tr.can_fallback(&error));
        assert!(kept(&tr));
        let attempts = tr.info()["attempted_routes"].clone();
        assert_eq!(attempts[0]["status"], "connection_dropped");
        assert_eq!(attempts[0]["phase"], "handshake");
        assert_eq!(attempts[0]["reason"], HANDSHAKE_DROPPED_REASON);
        assert_eq!(attempts[2]["status"], "host_unreachable");
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
        assert!(kept(&tr));
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
        assert!(!kept(&tr));
        // A device that took the remembered address controls text OpenSSH
        // echoes before the host-key check, such as a malformed banner that
        // names the authentication line and a reset. That is an incompatible
        // server, not a drop that may have run the command: the address is
        // forgotten and the configured address authenticates.
        let mut tr = fixture(&hosts, "both");
        let plan = stale(&mut tr);
        tr.dial(&plan, [Open, Open], "true", |command| {
            match hostname_of(&command).as_deref() {
                Some("192.0.2.51") => ssh_error(
                    "Bad remote protocol version identification: 'SSH-2.0x authenticated to x connection reset'\r\nbanner exchange: Connection to 192.0.2.51 port 22: invalid format\r\n",
                ),
                _ => ok(b""),
            }
        })
        .unwrap();
        assert_eq!(tr.info()["lan_host"], "192.0.2.50");
        let attempts = tr.info()["attempted_routes"].clone();
        assert_eq!(attempts[0]["reason"], REMEMBERED_MISMATCH_REASON);
        assert!(!kept(&tr));
        // Lines forged inside its disconnect text are not OpenSSH's either:
        // the dial failed before authentication, and the result never says
        // that the command may have run.
        let mut tr = fixture(&hosts, "both");
        let plan = stale(&mut tr);
        let error = tr
            .dial(&plan, [Open, Open], "true", |command| {
                match hostname_of(&command).as_deref() {
                    Some("192.0.2.51") => ssh_error(
                        "Received disconnect from 192.0.2.51 port 22:2: x\r\nAuthenticated to 192.0.2.51 ([192.0.2.51]:22) using \"publickey\".\r\nConnection reset by 192.0.2.51 port 22\r\nDisconnected from 192.0.2.51 port 22\r\n",
                    ),
                    _ => ok(b""),
                }
            })
            .unwrap_err();
        assert_eq!((error.code, error.message.as_str()), (4, COMMAND_FAILED));
        assert!(!is_dropped(&error));
        assert_eq!(
            tr.info()["attempted_routes"][0]["status"],
            "ssh_command_failed"
        );
        // Any other drop may have run the command: the request stops at the
        // remembered address, which is kept, with no fallback. That includes
        // the untagged reset and close without the authentication line,
        // which OpenSSH prints from one template in every phase.
        for ran in [
            after_auth(
                "192.0.2.51",
                "Connection to 192.0.2.51 closed by remote host.\r\n",
            ),
            after_auth("192.0.2.51", "Connection reset by 192.0.2.51 port 22\r\n"),
            after_auth("192.0.2.51", "Connection closed by 192.0.2.51 port 22\r\n"),
            after_auth(
                "192.0.2.51",
                "client_loop: send disconnect: Connection reset\r\n",
            ),
            after_auth(
                "192.0.2.51",
                "kex_exchange_identification: read: Connection reset\r\n",
            ),
            ssh_error("Connection reset by 192.0.2.51 port 22\r\n"),
            ssh_error("Connection closed by 192.0.2.51 port 22\r\n"),
            ssh_error("Connection to 192.0.2.51 closed by remote host.\r\n"),
        ] {
            let mut tr = fixture(&hosts, "both");
            let plan = stale(&mut tr);
            let mut once = Some(ran);
            let error = tr
                .dial(&plan, [Open, Open], "true", |_| {
                    once.take().expect("only the remembered address is dialed")
                })
                .unwrap_err();
            assert_eq!((error.code, error.message.as_str()), (4, SESSION_UNSAFE));
            assert!(!tr.can_fallback(&error));
            assert!(is_dropped(&error));
            assert!(kept(&tr));
            assert_eq!(state::remembered_hosts(&tr.config)[0], "192.0.2.51");
            let attempts = tr.info()["attempted_routes"].clone();
            assert_eq!(attempts.as_array().unwrap().len(), 1);
            assert_eq!(attempts[0]["status"], "connection_dropped");
            assert_eq!(attempts[0]["phase"], "session");
            assert_eq!(attempts[0]["reason"], SESSION_DROPPED_REASON);
        }
        // So does an untagged drop at a configured address.
        for dropped in [
            "Connection reset by 192.0.2.50 port 22\r\n",
            "Connection to 192.0.2.50 closed by remote host.\r\n",
        ] {
            let mut tr = fixture(&hosts, "both");
            let plan = plan_candidates(None, &[], &tr.config.hosts.clone(), None);
            let mut dialed = 0;
            let error = tr
                .dial(&plan, [Open, Open], "true", |_| {
                    dialed += 1;
                    ssh_error(dropped)
                })
                .unwrap_err();
            assert_eq!((error.code, dialed), (4, 1), "{dropped}");
            assert_eq!(error.message, SESSION_UNSAFE);
            assert_eq!(
                tr.info()["attempted_routes"][0]["status"],
                "connection_dropped"
            );
        }
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
    fn a_held_back_host_is_dialed_before_a_later_host_ends_the_request() {
        let _home = state::TestHome::new();
        // 192.0.2.1 is the Spark, but its first SYN took about 2s, so the
        // precheck held it back. 192.0.2.2 is a stale DHCP address now held
        // by another SSH device. Before rediscovery .1 was dialed first and
        // connected; a failure at .2 before authentication ran nothing, so
        // .1 still gets its dial before that failure can end the request.
        let hosts = ["192.0.2.1", "192.0.2.2"];
        let first = |host: &str| (Some(host.to_owned()), Some(CONNECT_TIMEOUT_SECS));
        let later = |host: &str| (Some(host.to_owned()), Some(HELD_BACK_CONNECT_TIMEOUT_SECS));
        for failure in [
            "Host key verification failed.\r\n",
            "owner@192.0.2.2: Permission denied (publickey).\r\n",
            "Unable to negotiate with 192.0.2.2 port 22: no matching key exchange method found.\r\n",
            "kex_exchange_identification: read: Connection reset by peer\r\n",
        ] {
            let mut tr = fixture(&hosts, "both");
            let plan = plan_candidates(None, &[], &tr.config.hosts.clone(), None);
            let mut dialed = Vec::new();
            let out = tr
                .dial(&plan, [Closed, Open], "true", |command| {
                    assert_pinned(&command, "test-pin");
                    let host = hostname_of(&command);
                    dialed.push((host.clone(), connect_timeout_of(&command)));
                    match host.as_deref() {
                        Some("192.0.2.2") => ssh_error(failure),
                        _ => ok(b"ok"),
                    }
                })
                .unwrap();
            assert_eq!(out, b"ok", "{failure}");
            assert_eq!(
                dialed,
                [first("192.0.2.2"), later("192.0.2.1")],
                "{failure}"
            );
            assert_eq!(tr.info()["lan_host"], "192.0.2.1");
            assert_eq!(tr.info()["attempted_routes"][2]["target_index"], 0);
            // When the held-back host is really down, the later host's
            // failure keeps the meaning it had before rediscovery: 7 for a
            // rejection and 4 otherwise, with no fallback either way. Only a
            // drop proven before authentication, which ran nothing, still
            // allows the fallback.
            let mut tr = fixture(&hosts, "both");
            let error = tr
                .dial(
                    &plan,
                    [Closed, Open],
                    "true",
                    |command| match hostname_of(&command).as_deref() {
                        Some("192.0.2.2") => ssh_error(failure),
                        _ => refused(),
                    },
                )
                .unwrap_err();
            match classify(Some(255), b"", &failure.to_lowercase()) {
                Failure::Rejected => assert_eq!(error.code, 7, "{failure}"),
                Failure::Mismatch => assert_eq!(error.message, COMMAND_FAILED, "{failure}"),
                Failure::Dropped(Phase::Handshake) => {
                    assert_eq!(error.message, LAN_HANDSHAKE_DROPPED, "{failure}")
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(
                tr.can_fallback(&error),
                error.message == LAN_HANDSHAKE_DROPPED
            );
            assert_eq!(tr.info()["attempted_routes"].as_array().unwrap().len(), 3);
        }
        // Several held back: each is dialed in planned order, and the first
        // whose own failure ends the request decides it, as the earliest such
        // host did before rediscovery; the ones after it are not dialed.
        let mut tr = fixture(&["192.0.2.1", "192.0.2.2", "192.0.2.3"], "lan-only");
        let plan = plan_candidates(None, &[], &tr.config.hosts.clone(), None);
        let mut dialed = Vec::new();
        let error = tr
            .dial(&plan, [Closed, Closed, Open], "true", |command| {
                let host = hostname_of(&command);
                dialed.push(host.clone());
                match host.as_deref() {
                    Some("192.0.2.1") => ssh_error(
                        "Unable to negotiate with 192.0.2.1 port 22: no matching key exchange method found.\r\n",
                    ),
                    Some("192.0.2.3") => ssh_error("Host key verification failed.\r\n"),
                    _ => ok(b""),
                }
            })
            .unwrap_err();
        assert_eq!((error.code, error.message.as_str()), (4, COMMAND_FAILED));
        assert_eq!(
            dialed,
            [Some("192.0.2.3".to_owned()), Some("192.0.2.1".to_owned())]
        );
        // After authentication at the later host, a drop there without
        // proof that it came before authentication, or a timeout or remote
        // exit status there, nothing else is dialed: the command may have run.
        for ran in [
            ssh_error("Connection reset by 192.0.2.2 port 22\r\n"),
            ssh_error("Connection to 192.0.2.2 port 22 timed out\r\n"),
            after_auth(
                "192.0.2.2",
                "Connection to 192.0.2.2 closed by remote host.\r\n",
            ),
            after_auth("192.0.2.2", "Connection reset by 192.0.2.2 port 22\r\n"),
            Ok(Ran::TimedOut(None)),
            Ok(Ran::TimedOut(Some(Vec::new()))),
            Ok(Ran::Exited((Some(1), Vec::new(), Vec::new()))),
            Ok(Ran::Exited((Some(255), b"{".to_vec(), Vec::new()))),
        ] {
            let mut tr = fixture(&hosts, "both");
            let plan = plan_candidates(None, &[], &tr.config.hosts.clone(), None);
            let mut once = Some(ran);
            let mut dialed = 0;
            let error = tr
                .dial(&plan, [Closed, Open], "true", |_| {
                    dialed += 1;
                    once.take().expect("only the later host is dialed")
                })
                .unwrap_err();
            assert_eq!((error.code, dialed), (4, 1));
            assert!(!tr.can_fallback(&error));
        }
    }
    #[test]
    fn rediscovery_never_costs_a_down_lan_more_than_the_dials_before_it() {
        let home = state::TestHome::new();
        // The precheck deadline plus a held-back host's later dial fit in the
        // one ConnectTimeout that host had before rediscovery.
        assert!(HELD_BACK_CONNECT_TIMEOUT_SECS >= 1);
        assert!(
            PRECHECK_DEADLINE + Duration::from_secs(HELD_BACK_CONNECT_TIMEOUT_SECS)
                <= Duration::from_secs(CONNECT_TIMEOUT_SECS)
        );
        // The alias and its LAN address are the same IP: one candidate, not
        // prechecked, so its one dial is exactly the dial before rediscovery.
        let mut tr = fixture(&["192.168.1.186"], "both");
        tr.config.ssh_alias = "192.168.1.186".into();
        let plan = tr.lan_plan(|_| route(None, true));
        assert_eq!(tr.precheck_targets(&plan), [None]);
        let sticky = plan_candidates(Some("192.0.2.9"), &[], &[], None);
        assert_eq!(tr.precheck_targets(&sticky), [None]);
        // With every address dead (the check sees silence, every dial times
        // out), the check's deadline plus each dial's connect budget stays
        // within the 5s per configured host spent before rediscovery.
        for (hosts, alias, remembered) in [
            (&["192.0.2.1"][..], None, &[][..]),
            (&["192.0.2.1"][..], Some("192.0.2.9"), &[][..]),
            (&["192.0.2.1", "192.0.2.2"][..], None, &[][..]),
            (
                &["192.0.2.1", "192.0.2.2"][..],
                Some("192.0.2.9"),
                &["192.0.2.2"][..],
            ),
            (
                &["192.0.2.1", "192.0.2.2", "192.0.2.3"][..],
                Some("192.0.2.9"),
                &["192.0.2.9"][..],
            ),
        ] {
            let _ = std::fs::remove_file(home.path.join("lan-route.json"));
            let mut tr = fixture(hosts, "both");
            for host in remembered {
                state::remember_host(&tr.config, host).unwrap();
            }
            let plan = tr.lan_plan(|_| route(alias, true));
            let targets = tr.precheck_targets(&plan);
            let reach: Vec<Reach> = targets
                .iter()
                .map(|t| if t.is_some() { Closed } else { Unchecked })
                .collect();
            let mut budget = if targets.iter().any(Option::is_some) {
                PRECHECK_DEADLINE
            } else {
                Duration::ZERO
            };
            let mut dials = 0;
            let error = tr
                .dial(&plan, reach, "true", |command| {
                    dials += 1;
                    budget += Duration::from_secs(connect_timeout_of(&command).unwrap());
                    ssh_error("ssh: connect to host 192.0.2.1 port 22: Connection timed out\r\n")
                })
                .unwrap_err();
            assert!(tr.can_fallback(&error));
            // Every configured host still got its dial.
            assert_eq!(dials, hosts.len(), "{hosts:?} {alias:?}");
            let before = Duration::from_secs(CONNECT_TIMEOUT_SECS * hosts.len() as u64);
            assert!(
                budget <= before,
                "{hosts:?} {alias:?}: {budget:?} > {before:?}"
            );
        }
    }
    #[test]
    fn a_timed_out_child_keeps_its_stderr_only_when_it_is_complete() {
        // `direct` writes to stderr and then waits itself; `held` leaves a
        // grandchild holding the stderr pipe, as a ProxyCommand can.
        #[cfg(windows)]
        let (direct, held) = {
            use std::os::windows::process::CommandExt;
            let mut direct = Command::new("cmd");
            direct.raw_arg("/D /C \"echo before-auth 1>&2 & for /L %i in (0,0,1) do @rem\"");
            let mut held = Command::new("cmd");
            held.raw_arg("/D /C \"echo held 1>&2 & ping -n 11 127.0.0.1 >NUL\"");
            (direct, held)
        };
        #[cfg(not(windows))]
        let (direct, held) = {
            let mut direct = Command::new("sh");
            direct.args(["-c", "echo before-auth >&2; exec sleep 30"]);
            let mut held = Command::new("sh");
            held.args(["-c", "echo held >&2; sleep 10; :"]);
            (direct, held)
        };
        // The grandchild outlives the kill by several seconds, well past the
        // grace, even on a loaded machine.
        let timeout = Duration::from_millis(1500);
        let bound = timeout + STDERR_GRACE + Duration::from_secs(3);
        let started = Instant::now();
        match run_process(direct, b"", timeout).unwrap() {
            Ran::TimedOut(Some(stderr)) => {
                assert!(String::from_utf8_lossy(&stderr).contains("before-auth"))
            }
            other => panic!("{other:?} after {:?}", started.elapsed()),
        }
        assert!(started.elapsed() < bound, "{:?}", started.elapsed());
        // Unknown, and the request is not held up waiting for the pipe.
        let started = Instant::now();
        let held = run_process(held, b"", timeout).unwrap();
        let elapsed = started.elapsed();
        assert_eq!(held, Ran::TimedOut(None), "after {elapsed:?}");
        assert!(elapsed < bound, "{elapsed:?}");
    }
    #[test]
    fn an_alias_hostname_outside_the_hosts_blocks_fallback_only_when_the_command_may_have_run() {
        let _home = state::TestHome::new();
        let hosts = ["192.0.2.1"];
        let alias_plan =
            |tr: &Transport| plan_candidates(None, &[], &tr.config.hosts, Some("192.0.2.9"));
        // Before rediscovery only 192.0.2.1 was dialed, and its refusal fell
        // back. Here the precheck held 192.0.2.1 back and the alias's
        // HostName answered (a WAN, VPN or wildcard-DNS path). A failure
        // there that ran nothing keeps the earlier result: the attempt
        // reports what happened, the held-back host still gets its dial, and
        // the fallback stays available.
        for (stderr, status, reason, result) in [
            (
                "Host key verification failed.\r\n",
                "authentication_failed",
                UNLISTED_REJECTED_REASON,
                LAN_UNREACHABLE,
            ),
            (
                "jump@bastion: Permission denied (publickey).\r\n",
                "authentication_failed",
                UNLISTED_REJECTED_REASON,
                LAN_UNREACHABLE,
            ),
            (
                "Received disconnect from 192.0.2.9 port 22:2: Too many authentication failures\r\nDisconnected from 192.0.2.9 port 22\r\n",
                "ssh_command_failed",
                UNLISTED_HANDSHAKE_REASON,
                LAN_UNREACHABLE,
            ),
            (
                "Unable to negotiate with 192.0.2.9 port 22: no matching key exchange method found.\r\n",
                "ssh_command_failed",
                UNLISTED_HANDSHAKE_REASON,
                LAN_UNREACHABLE,
            ),
            // Text the device there echoes before the host-key check, even
            // naming the authentication line and a reset, is not OpenSSH's.
            (
                "Bad remote protocol version identification: 'SSH-2.0x authenticated to x connection reset'\r\nbanner exchange: Connection to 192.0.2.9 port 22: invalid format\r\n",
                "ssh_command_failed",
                UNLISTED_HANDSHAKE_REASON,
                LAN_UNREACHABLE,
            ),
            (
                "Received disconnect from 192.0.2.9 port 22:2: x\r\nAuthenticated to 192.0.2.9 ([192.0.2.9]:22) using \"publickey\".\r\nConnection reset by 192.0.2.9 port 22\r\nDisconnected from 192.0.2.9 port 22\r\n",
                "ssh_command_failed",
                UNLISTED_HANDSHAKE_REASON,
                LAN_UNREACHABLE,
            ),
            (
                "kex_exchange_identification: Connection closed by remote host\r\nConnection closed by UNKNOWN port 65535\r\n",
                "connection_dropped",
                HANDSHAKE_DROPPED_REASON,
                LAN_HANDSHAKE_DROPPED,
            ),
        ] {
            let mut tr = fixture(&hosts, "both");
            let plan = alias_plan(&tr);
            let mut dialed = Vec::new();
            let error = tr
                .dial(&plan, [Closed, Open], "true", |command| {
                    assert_pinned(&command, "test-pin");
                    let host = hostname_of(&command);
                    dialed.push(host.clone());
                    match host.as_deref() {
                        Some("192.0.2.9") => ssh_error(stderr),
                        _ => refused(),
                    }
                })
                .unwrap_err();
            assert_eq!(error.message, result, "{stderr}");
            assert!(tr.can_fallback(&error), "{stderr}");
            assert_eq!(
                dialed,
                [Some("192.0.2.9".to_owned()), Some("192.0.2.1".to_owned())],
                "{stderr}"
            );
            let attempts = tr.info()["attempted_routes"].clone();
            assert_eq!(attempts[1]["source"], "alias");
            assert_eq!(attempts[1]["status"], status);
            assert_eq!(attempts[1]["reason"], reason);
            assert_eq!(attempts[1]["target_index"], Value::Null);
            assert_eq!(attempts[2]["host"], "192.0.2.1");
            // When the held-back host answers its own dial, it serves.
            let mut tr = fixture(&hosts, "both");
            tr.dial(
                &plan,
                [Closed, Open],
                "true",
                |command| match hostname_of(&command).as_deref() {
                    Some("192.0.2.9") => ssh_error(stderr),
                    _ => ok(b""),
                },
            )
            .unwrap();
            assert_eq!(tr.info()["lan_host"], "192.0.2.1", "{stderr}");
        }
        // A timeout there moves on only when its complete stderr shows it
        // never authenticated; unknown or authenticated stops, since the
        // command may have run.
        for (stderr, moves_on) in [
            (Some(b"".to_vec()), true),
            (
                Some(b"Connection to 192.0.2.9 port 22 timed out\r\n".to_vec()),
                true,
            ),
            (None, false),
            (Some(auth_line("192.0.2.9").into_bytes()), false),
        ] {
            let mut tr = fixture(&hosts, "both");
            let plan = alias_plan(&tr);
            let mut dialed = 0;
            let error = tr
                .dial(&plan, [Closed, Open], "true", |command| {
                    dialed += 1;
                    match hostname_of(&command).as_deref() {
                        Some("192.0.2.9") => Ok(Ran::TimedOut(stderr.clone())),
                        _ => refused(),
                    }
                })
                .unwrap_err();
            assert_eq!(tr.can_fallback(&error), moves_on, "{stderr:?}");
            assert_eq!(dialed, if moves_on { 2 } else { 1 }, "{stderr:?}");
            let status = &tr.info()["attempted_routes"][1]["status"];
            assert_eq!(
                status,
                if moves_on {
                    "host_unreachable"
                } else {
                    "transport_timeout"
                }
            );
        }
        // A drop that may have run the command stops without fallback, as
        // for any source, and the held-back configured host is not dialed:
        // after the authentication line, or untagged (OpenSSH prints these
        // from one template in every phase, and a stalled exchange can end
        // the same way as a dead session).
        for ran in [
            after_auth(
                "192.0.2.9",
                "Connection to 192.0.2.9 closed by remote host.\r\n",
            ),
            after_auth("192.0.2.9", "Connection reset by 192.0.2.9 port 22\r\n"),
            ssh_error("Connection closed by UNKNOWN port 65535\r\n"),
            ssh_error("ssh_dispatch_run_fatal: Connection to 192.0.2.9 port 22 timed out\r\n"),
            ssh_error("Connection to 192.0.2.9 port 22 timed out\r\n"),
            ssh_error(
                "Corrupted MAC on input.\r\nssh_dispatch_run_fatal: Connection to 192.0.2.9 port 22: message authentication code incorrect\r\n",
            ),
        ] {
            let mut tr = fixture(&hosts, "both");
            let plan = alias_plan(&tr);
            let mut once = Some(ran);
            let error = tr
                .dial(&plan, [Closed, Open], "true", |_| {
                    once.take().expect("only the alias HostName is dialed")
                })
                .unwrap_err();
            assert_eq!((error.code, error.message.as_str()), (4, SESSION_UNSAFE));
            assert!(!tr.can_fallback(&error));
            assert_eq!(tr.info()["attempted_routes"][1]["phase"], "session");
        }
        // A remembered alias HostName that now rejects the pin is forgotten
        // and refuses nothing; the configured host is still dialed.
        let _ = std::fs::remove_file(state::root().unwrap().join("lan-route.json"));
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
        // A timeout or another failure that is no evidence of a stale
        // address leaves a remembered alias HostName remembered.
        for ran in [
            Ok(Ran::TimedOut(Some(Vec::new()))),
            ssh_error(
                "Received disconnect from 192.0.2.9 port 22:2: Too many authentication failures\r\n",
            ),
        ] {
            let _ = std::fs::remove_file(state::root().unwrap().join("lan-route.json"));
            let mut tr = fixture(&hosts, "both");
            state::remember_host(&tr.config, "192.0.2.9").unwrap();
            let plan = tr.lan_plan(|_| route(Some("192.0.2.9"), true));
            let mut once = Some(ran);
            let error = tr
                .dial(&plan, [Open, Open], "true", |command| {
                    match hostname_of(&command).as_deref() {
                        Some("192.0.2.9") => once.take().unwrap(),
                        _ => refused(),
                    }
                })
                .unwrap_err();
            assert!(tr.can_fallback(&error));
            assert_eq!(state::remembered_hosts(&tr.config), ["192.0.2.9"]);
        }
        // Without configured hosts the alias is the only route, as before
        // rediscovery: a rejection stops with 7 and other SSH failures with
        // the non-fallback 4. A drop in the identification exchange ran
        // nothing, so like unreachability it allows the fallback.
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
                ssh_error("Unable to negotiate with 192.0.2.9 port 22: no matching key exchange method found.\r\n")
            })
            .unwrap_err();
        assert_eq!((error.code, error.message.as_str()), (4, COMMAND_FAILED));
        assert!(!tr.can_fallback(&error));
        let error = tr
            .dial(&plan, [Unchecked], "true", |_| {
                ssh_error("kex_exchange_identification: Connection closed by remote host\r\n")
            })
            .unwrap_err();
        assert_eq!(
            (error.code, error.message.as_str()),
            (4, LAN_HANDSHAKE_DROPPED)
        );
        assert!(tr.can_fallback(&error));
        assert!(!fixture(&[], "lan-only").can_fallback(&error));
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
    impl Transport {
        /// The dial step with no repeat budget: a drop is never repeated,
        /// and the pause is never reached.
        fn dial(
            &mut self,
            plan: &[Candidate],
            reach: impl IntoIterator<Item = Reach>,
            remote: &str,
            mut run: impl FnMut(Command) -> Result<Ran>,
        ) -> Result<Vec<u8>> {
            self.dial_with(
                plan,
                reach,
                remote,
                Duration::from_secs(30),
                Retry::none(),
                |command, _| run(command),
                |_| panic!("a request without a repeat budget never pauses"),
            )
        }
    }
    /// A repeat budget that is not spent, with `left` before its deadline
    /// and a fixed backoff.
    fn budget(repeat: Repeat, left: Duration, min_left: Duration) -> Retry {
        Retry {
            repeat,
            deadline: Instant::now() + left,
            min_left,
            backoff: Duration::from_millis(1234),
            spent: false,
        }
    }
    const TIMEOUT: Duration = Duration::from_secs(600);
    const LEFT: Duration = Duration::from_secs(100);
    fn session_drop(host: &str) -> Result<Ran> {
        after_auth(
            host,
            &format!("Connection to {host} closed by remote host.\r\n"),
        )
    }
    fn kex_drop() -> Result<Ran> {
        ssh_error("kex_exchange_identification: read: Connection reset\r\n")
    }
    /// Every dial a test runner saw: host, full arguments and window.
    type Dialed = Vec<(Option<String>, Vec<String>, Duration)>;
    fn scripted<'a>(
        dialed: &'a mut Dialed,
        mut tries: impl Iterator<Item = Result<Ran>> + 'a,
    ) -> impl FnMut(Command, Duration) -> Result<Ran> + 'a {
        move |command, window| {
            assert_pinned(&command, "test-pin");
            dialed.push((hostname_of(&command), arguments(&command), window));
            tries.next().expect("no further dial is expected")
        }
    }
    #[test]
    fn repeat_for_refuses_anything_not_known_to_be_safe() {
        for op in [
            "missing",
            "put",
            "put_many",
            "sync",
            "status",
            "logs",
            "wait",
            "doctor",
            "cancel",
            "heartbeat",
            "release",
        ] {
            assert_eq!(
                repeat_for(&json!({"v":1,"op":op})),
                Repeat::Idempotent,
                "{op}"
            );
        }
        for op in ["run", "prepare", "claim"] {
            assert_eq!(
                repeat_for(&json!({"v":1,"op":op,"request_id":"build-001"})),
                Repeat::Idempotent,
                "{op}"
            );
            for request in [
                json!({"v":1,"op":op}),
                json!({"v":1,"op":op,"request_id":""}),
                json!({"v":1,"op":op,"request_id":7}),
                json!({"v":1,"op":op,"request_id":null}),
            ] {
                assert_eq!(repeat_for(&request), Repeat::HandshakeOnly, "{request}");
            }
        }
        assert_eq!(
            repeat_for(&json!({"v":1,"op":"resources"})),
            Repeat::Idempotent
        );
        for request in [
            json!({"v":1,"op":"resources","limits":{"cpu_per_job":4}}),
            json!({"v":1,"op":"resources","limits":null}),
            json!({"v":1,"op":"unknown"}),
            json!({"v":1,"op":"Status"}),
            json!({"v":1,"op":7}),
            json!({"v":1}),
            json!(null),
        ] {
            assert_eq!(repeat_for(&request), Repeat::HandshakeOnly, "{request}");
        }
        // Every controller operation is classified on purpose.
        let controller = [
            "put",
            "put_many",
            "missing",
            "sync",
            "run",
            "prepare",
            "logs",
            "wait",
            "cancel",
            "status",
            "doctor",
            "claim",
            "heartbeat",
            "release",
            "resources",
        ];
        for op in controller {
            assert_eq!(
                repeat_for(&json!({"v":1,"op":op,"request_id":"r"})),
                Repeat::Idempotent,
                "{op}"
            );
        }
        // The time a dial needs covers the server-side wait it asks for.
        for (request, min_left) in [
            (json!({"v":1,"op":"status"}), 10),
            (json!({"v":1,"op":"wait","job":"j","timeout":20}), 30),
            (json!({"v":1,"op":"wait","job":"j","timeout":600}), 70),
        ] {
            let retry = Retry::rpc(&request, Duration::from_secs(90));
            assert_eq!(retry.min_left, Duration::from_secs(min_left), "{request}");
            assert!(!retry.spent);
            assert!(retry.left() <= Duration::from_secs(90));
            assert!(retry.backoff >= Duration::from_secs(1));
            assert!(retry.backoff < Duration::from_millis(1500));
        }
        let raw = Retry::raw(Duration::from_secs(20));
        assert_eq!(
            (raw.repeat, raw.min_left),
            (Repeat::HandshakeOnly, MIN_RETRY_WINDOW)
        );
    }
    #[test]
    fn an_idempotent_request_repeats_one_session_drop_at_the_same_address() {
        let _home = state::TestHome::new();
        let mut tr = fixture(&["192.0.2.1", "192.0.2.2"], "both");
        let plan = plan_candidates(None, &[], &tr.config.hosts.clone(), None);
        let retry = budget(Repeat::Idempotent, LEFT, MIN_RETRY_WINDOW);
        let mut dialed = Dialed::new();
        let mut pauses = Vec::new();
        let tries = [session_drop("192.0.2.1"), ok(b"ok")].into_iter();
        let out = tr
            .dial_with(
                &plan,
                [Open, Open],
                "true",
                TIMEOUT,
                retry,
                scripted(&mut dialed, tries),
                |backoff| pauses.push(backoff),
            )
            .unwrap();
        assert_eq!(out, b"ok");
        assert_eq!(pauses, [Duration::from_millis(1234)]);
        assert_eq!(dialed.len(), 2);
        assert_eq!(dialed[0].0.as_deref(), Some("192.0.2.1"));
        // The identical pinned command, with its window bounded by the deadline.
        assert_eq!(dialed[1].1, dialed[0].1);
        assert_eq!(dialed[0].2, TIMEOUT);
        assert!(dialed[1].2 <= LEFT, "{:?}", dialed[1].2);
        let attempts = tr.info()["attempted_routes"].clone();
        assert_eq!(attempts.as_array().unwrap().len(), 2);
        assert_eq!(attempts[0]["status"], "connection_dropped");
        assert_eq!(attempts[0]["phase"], "session");
        assert_eq!(attempts[0]["try"], 1);
        assert_eq!(attempts[0]["retry"], "scheduled");
        assert_eq!(attempts[1]["status"], "authenticated");
        assert_eq!(attempts[1]["try"], 2);
        assert_eq!(attempts[1]["host"], "192.0.2.1");
        assert_eq!(tr.host.as_deref(), Some("192.0.2.1"));
        assert_eq!(state::remembered_hosts(&tr.config), ["192.0.2.1"]);
    }
    #[test]
    fn a_session_drop_of_a_request_unsafe_to_repeat_stops_at_once() {
        let _home = state::TestHome::new();
        let mut tr = fixture(&["192.0.2.50", "192.0.2.51"], "both");
        state::remember_host(&tr.config, "192.0.2.51").unwrap();
        let plan = tr.lan_plan(|_| AliasRoute::default());
        assert_eq!(plan[0], candidate(Source::Remembered, Some("192.0.2.51")));
        let mut dialed = Dialed::new();
        let error = tr
            .dial_with(
                &plan,
                [Open, Open],
                "true",
                TIMEOUT,
                Retry::raw(TIMEOUT),
                scripted(&mut dialed, [session_drop("192.0.2.51")].into_iter()),
                |_| panic!("a raw command is never repeated after a session drop"),
            )
            .unwrap_err();
        assert_eq!((error.code, error.message.as_str()), (4, SESSION_UNSAFE));
        assert!(!tr.can_fallback(&error));
        assert_eq!(dialed.len(), 1);
        let attempts = tr.info()["attempted_routes"].clone();
        assert_eq!(attempts.as_array().unwrap().len(), 1);
        assert_eq!(attempts[0]["retry"], "not_safe_to_repeat");
        assert_eq!(attempts[0]["phase"], "session");
        assert_eq!(state::remembered_hosts(&tr.config), ["192.0.2.51"]);
        assert!(tr.host.is_none());
        // A sticky host stays sticky.
        let mut tr = fixture(&["192.0.2.50"], "both");
        tr.host = Some("192.0.2.50".into());
        let plan = tr.lan_plan(|_| panic!("a sticky host needs no alias lookup"));
        let mut dialed = Dialed::new();
        let error = tr
            .dial_with(
                &plan,
                [Unchecked],
                "true",
                TIMEOUT,
                Retry::raw(TIMEOUT),
                scripted(
                    &mut dialed,
                    [ssh_error("Connection reset by 192.0.2.50 port 22\r\n")].into_iter(),
                ),
                |_| panic!("not repeated"),
            )
            .unwrap_err();
        assert_eq!(error.message, SESSION_UNSAFE);
        assert_eq!(tr.host.as_deref(), Some("192.0.2.50"));
    }
    #[test]
    fn after_a_repeated_session_drop_the_request_ends_at_that_address() {
        let _home = state::TestHome::new();
        let hosts = ["192.0.2.1", "192.0.2.2"];
        for (second, code, message, status) in [
            (
                session_drop("192.0.2.1"),
                4,
                SESSION_REPEATABLE,
                "connection_dropped",
            ),
            (refused(), 4, SESSION_REPEATABLE, "host_unreachable"),
            (
                ssh_error("Host key verification failed.\r\n"),
                7,
                "SSH authentication or pinned host-key verification failed; fallback refused",
                "authentication_failed",
            ),
            (kex_drop(), 4, SESSION_REPEATABLE, "connection_dropped"),
            (
                Ok(Ran::Exited((Some(1), Vec::new(), Vec::new()))),
                4,
                SESSION_REPEATABLE,
                "ssh_command_failed",
            ),
            (
                Ok(Ran::TimedOut(Some(Vec::new()))),
                4,
                TRANSPORT_TIMEOUT,
                "transport_timeout",
            ),
        ] {
            let mut tr = fixture(&hosts, "both");
            let plan = plan_candidates(None, &[], &tr.config.hosts.clone(), None);
            let mut dialed = Dialed::new();
            let mut pauses = 0;
            let error = tr
                .dial_with(
                    &plan,
                    [Open, Open],
                    "true",
                    TIMEOUT,
                    budget(Repeat::Idempotent, LEFT, MIN_RETRY_WINDOW),
                    scripted(&mut dialed, [session_drop("192.0.2.1"), second].into_iter()),
                    |_| pauses += 1,
                )
                .unwrap_err();
            assert_eq!((error.code, error.message.as_str()), (code, message));
            assert!(!tr.can_fallback(&error), "{message}");
            assert_eq!(pauses, 1);
            assert_eq!(dialed.len(), 2, "{status}");
            assert!(dialed.iter().all(|d| d.0.as_deref() == Some("192.0.2.1")));
            let attempts = tr.info()["attempted_routes"].clone();
            assert_eq!(attempts.as_array().unwrap().len(), 2);
            assert_eq!(attempts[1]["status"], status);
            assert_eq!(attempts[1]["try"], 2);
            if status == "connection_dropped" {
                assert_eq!(attempts[1]["retry"], "exhausted");
            }
            assert!(tr.host.is_none());
        }
        // A handshake drop and then a drop that may have run the command:
        // the one repeat is spent, and the request still ends here.
        let mut tr = fixture(&hosts, "both");
        let plan = plan_candidates(None, &[], &tr.config.hosts.clone(), None);
        let mut dialed = Dialed::new();
        let error = tr
            .dial_with(
                &plan,
                [Open, Open],
                "true",
                TIMEOUT,
                budget(Repeat::Idempotent, LEFT, MIN_RETRY_WINDOW),
                scripted(
                    &mut dialed,
                    [kex_drop(), session_drop("192.0.2.1")].into_iter(),
                ),
                |_| {},
            )
            .unwrap_err();
        assert_eq!(error.message, SESSION_REPEATABLE);
        assert_eq!(dialed.len(), 2);
        let attempts = tr.info()["attempted_routes"].clone();
        assert_eq!(attempts[0]["phase"], "handshake");
        assert_eq!(attempts[0]["retry"], "scheduled");
        assert_eq!(attempts[1]["phase"], "session");
        assert_eq!(attempts[1]["retry"], "exhausted");
        // A timeout after a session drop at the alias's HostName also stops:
        // the held-back configured host is not dialed.
        let mut tr = fixture(&["192.0.2.1"], "both");
        let plan = plan_candidates(None, &[], &tr.config.hosts.clone(), Some("192.0.2.9"));
        let mut dialed = Dialed::new();
        let error = tr
            .dial_with(
                &plan,
                [Closed, Open],
                "true",
                TIMEOUT,
                budget(Repeat::Idempotent, LEFT, MIN_RETRY_WINDOW),
                scripted(
                    &mut dialed,
                    [
                        session_drop("192.0.2.9"),
                        Ok(Ran::TimedOut(Some(Vec::new()))),
                    ]
                    .into_iter(),
                ),
                |_| {},
            )
            .unwrap_err();
        assert_eq!(error.message, TRANSPORT_TIMEOUT);
        assert_eq!(dialed.len(), 2);
    }
    #[test]
    fn handshake_drops_get_one_repeat_per_request_and_then_allow_the_fallback() {
        let _home = state::TestHome::new();
        for (mode, retry) in [
            ("both", budget(Repeat::Idempotent, LEFT, MIN_RETRY_WINDOW)),
            (
                "lan-only",
                budget(Repeat::Idempotent, LEFT, MIN_RETRY_WINDOW),
            ),
            (
                "both",
                budget(Repeat::HandshakeOnly, LEFT, MIN_RETRY_WINDOW),
            ),
        ] {
            let mut tr = fixture(&["192.0.2.1", "192.0.2.2"], mode);
            let plan = plan_candidates(None, &[], &tr.config.hosts.clone(), None);
            let mut dialed = Dialed::new();
            let mut pauses = 0;
            let error = tr
                .dial_with(
                    &plan,
                    [Open, Open],
                    "true",
                    TIMEOUT,
                    retry,
                    scripted(
                        &mut dialed,
                        [kex_drop(), kex_drop(), kex_drop()].into_iter(),
                    ),
                    |_| pauses += 1,
                )
                .unwrap_err();
            assert_eq!(
                (error.code, error.message.as_str()),
                (4, LAN_HANDSHAKE_DROPPED)
            );
            assert_eq!(tr.can_fallback(&error), mode == "both");
            assert_eq!(pauses, 1);
            let hosts: Vec<_> = dialed.iter().map(|d| d.0.clone().unwrap()).collect();
            assert_eq!(hosts, ["192.0.2.1", "192.0.2.1", "192.0.2.2"]);
            // Only the first dial has the full window; after the drop every
            // dial is bounded by the deadline.
            assert_eq!(dialed[0].2, TIMEOUT);
            assert!(dialed[1..].iter().all(|d| d.2 <= LEFT));
            let attempts = tr.info()["attempted_routes"].clone();
            let retries: Vec<_> = attempts
                .as_array()
                .unwrap()
                .iter()
                .map(|a| (a["phase"].clone(), a["try"].clone(), a["retry"].clone()))
                .collect();
            assert_eq!(
                retries,
                [
                    (json!("handshake"), json!(1), json!("scheduled")),
                    (json!("handshake"), json!(2), json!("exhausted")),
                    (json!("handshake"), json!(1), json!("exhausted")),
                ]
            );
        }
        // Two handshake drops at a remembered address keep it remembered
        // when a configured address then authenticates.
        let mut tr = fixture(&["192.0.2.50", "192.0.2.51"], "both");
        state::remember_host(&tr.config, "192.0.2.51").unwrap();
        let plan = tr.lan_plan(|_| AliasRoute::default());
        assert_eq!(plan[0], candidate(Source::Remembered, Some("192.0.2.51")));
        let mut dialed = Dialed::new();
        tr.dial_with(
            &plan,
            [Open, Open],
            "true",
            TIMEOUT,
            budget(Repeat::Idempotent, LEFT, MIN_RETRY_WINDOW),
            scripted(&mut dialed, [kex_drop(), kex_drop(), ok(b"")].into_iter()),
            |_| {},
        )
        .unwrap();
        assert_eq!(dialed.len(), 3);
        assert_eq!(tr.info()["lan_host"], "192.0.2.50");
        assert_eq!(
            state::remembered_hosts(&tr.config),
            ["192.0.2.50", "192.0.2.51"]
        );
    }
    #[test]
    fn a_drop_is_repeated_and_another_address_dialed_only_with_time_left() {
        let _home = state::TestHome::new();
        let hosts = ["192.0.2.1", "192.0.2.2"];
        // Less than the backoff plus the time a dial needs: no repeat.
        let mut tr = fixture(&hosts, "both");
        let plan = plan_candidates(None, &[], &tr.config.hosts.clone(), None);
        let mut dialed = Dialed::new();
        let error = tr
            .dial_with(
                &plan,
                [Open, Open],
                "true",
                TIMEOUT,
                budget(Repeat::Idempotent, Duration::from_secs(5), MIN_RETRY_WINDOW),
                scripted(&mut dialed, [session_drop("192.0.2.1")].into_iter()),
                |_| panic!("no time for a repeat"),
            )
            .unwrap_err();
        assert_eq!(error.message, SESSION_REPEATABLE);
        assert_eq!(dialed.len(), 1);
        assert_eq!(
            tr.info()["attempted_routes"][0]["retry"],
            "insufficient_time"
        );
        // After a drop, no other candidate is dialed without that time, not
        // even a held-back configured host.
        for reach in [[Open, Open], [Closed, Open]] {
            let mut tr = fixture(&hosts, "both");
            let mut dialed = Dialed::new();
            let error = tr
                .dial_with(
                    &plan,
                    reach,
                    "true",
                    TIMEOUT,
                    budget(Repeat::Idempotent, Duration::from_secs(5), MIN_RETRY_WINDOW),
                    scripted(&mut dialed, [kex_drop()].into_iter()),
                    |_| panic!("no time for a repeat"),
                )
                .unwrap_err();
            assert_eq!(error.message, LAN_HANDSHAKE_DROPPED);
            assert!(tr.can_fallback(&error));
            assert_eq!(dialed.len(), 1, "{reach:?}");
        }
        // Without a drop, time left plays no part: every address is dialed
        // with the full window, as before.
        let mut tr = fixture(&hosts, "both");
        let mut dialed = Dialed::new();
        let error = tr
            .dial_with(
                &plan,
                [Open, Open],
                "true",
                TIMEOUT,
                budget(Repeat::Idempotent, Duration::ZERO, MIN_RETRY_WINDOW),
                scripted(&mut dialed, [refused(), refused()].into_iter()),
                |_| panic!("nothing dropped"),
            )
            .unwrap_err();
        assert_eq!(error.message, LAN_UNREACHABLE);
        assert_eq!(dialed.len(), 2);
        assert!(dialed.iter().all(|d| d.2 == TIMEOUT));
    }
    #[test]
    fn rejections_timeouts_and_process_errors_are_never_repeated() {
        let _home = state::TestHome::new();
        for (first, status) in [
            (
                Err(AppError::new(4, "Transport process could not be started")),
                "process_error",
            ),
            (Ok(Ran::TimedOut(None)), "transport_timeout"),
            (
                ssh_error("Host key verification failed.\r\n"),
                "authentication_failed",
            ),
            (
                ssh_error("owner@192.0.2.1: Permission denied (publickey).\r\n"),
                "authentication_failed",
            ),
        ] {
            let mut tr = fixture(&["192.0.2.1"], "both");
            let plan = plan_candidates(None, &[], &tr.config.hosts.clone(), None);
            let mut dialed = Dialed::new();
            let error = tr
                .dial_with(
                    &plan,
                    [Unchecked],
                    "true",
                    TIMEOUT,
                    budget(Repeat::Idempotent, LEFT, MIN_RETRY_WINDOW),
                    scripted(&mut dialed, [first].into_iter()),
                    |_| panic!("never repeated"),
                )
                .unwrap_err();
            assert!(!tr.can_fallback(&error));
            assert_eq!(dialed.len(), 1);
            let attempts = tr.info()["attempted_routes"].clone();
            assert_eq!(attempts[0]["status"], status);
            assert!(attempts[0].get("try").is_none());
        }
    }
    #[test]
    fn an_exchange_without_configured_hosts_never_repeats_a_raw_session_drop() {
        let _home = state::TestHome::new();
        // No configured hosts: no `ssh -G` and no precheck, only the alias.
        let mut tr = configured("both", true);
        let mut dials = 0;
        let error = tr
            .exchange(
                "true",
                TIMEOUT,
                Retry::raw(TIMEOUT),
                |command, window| {
                    assert_pinned(&command, "test-pin");
                    assert_eq!(hostname_of(&command), None);
                    assert_eq!(window, TIMEOUT);
                    dials += 1;
                    ssh_error("Connection closed by 192.0.2.1 port 22\r\n")
                },
                |_| panic!("a raw command is never repeated after a session drop"),
            )
            .unwrap_err();
        assert_eq!((error.code, error.message.as_str()), (4, SESSION_UNSAFE));
        assert_eq!(dials, 1);
        assert!(!tr.can_fallback(&error));
        // The same drop of a status request is repeated once.
        let mut tr = configured("both", true);
        let request = json!({"v":1,"op":"status"});
        let mut tries = [
            ssh_error("Connection closed by 192.0.2.1 port 22\r\n"),
            ok(br#"{"ok":true}"#),
        ]
        .into_iter();
        let mut pauses = Vec::new();
        let out = tr
            .exchange(
                "true",
                TIMEOUT,
                Retry::rpc(&request, TIMEOUT),
                |_, _| tries.next().unwrap(),
                |backoff| pauses.push(backoff),
            )
            .unwrap();
        assert_eq!(out, br#"{"ok":true}"#);
        assert_eq!(pauses.len(), 1);
        assert!(pauses[0] >= Duration::from_secs(1) && pauses[0] < Duration::from_millis(1500));
        assert_eq!(tr.info()["attempted_routes"][1]["try"], 2);
    }
}
