//! First-time trust for another computer, without hand-written SSH setup.
//!
//! The laptop makes one key per device. The owner runs one printed command on
//! that device, which authorizes only that key. Here the owner then confirms the
//! device's host-key fingerprint, and trust writes a DGXBridge-owned pin and SSH
//! profile under the state root and registers the device. Nothing in the owner's
//! own ~/.ssh is read or changed, and a device is never trusted on first sight:
//! the fingerprint the owner confirmed must still be the one the device presents.
use crate::{AppError, Result, discover, fleet, nodes, state, transport};
use base64::{Engine, engine::general_purpose::STANDARD, engine::general_purpose::STANDARD_NO_PAD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    net::{Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const STEP: Duration = Duration::from_secs(20);
const SCAN_CONNECT: Duration = Duration::from_millis(450);
const SCAN_LIMIT: usize = 1024;
const NAME_BUDGET: Duration = Duration::from_millis(2500);

fn invalid(message: &str) -> AppError {
    AppError::new(2, message)
}

fn alias(id: &str) -> String {
    format!("dgxbridge-{id}")
}

/// A host name or IPv4 address that needs no quoting anywhere it is used.
fn validate_host(host: &str) -> Result<()> {
    if host.is_empty()
        || host.len() > 253
        || host.starts_with(['-', '.'])
        || !host
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-".contains(&c))
    {
        return Err(invalid(
            "Enter the device's name or address, for example AntonPC or 192.168.1.20",
        ));
    }
    Ok(())
}

/// The device's IPv4 address. OpenSSH on Windows can fail to resolve a bare
/// computer name it would find over IPv4, so names are resolved here.
fn resolve(host: &str, port: u16) -> Result<Ipv4Addr> {
    validate_host(host)?;
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Ok(ip);
    }
    (host, port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut found| {
            found.find_map(|address| match address {
                SocketAddr::V4(v4) => Some(*v4.ip()),
                SocketAddr::V6(_) => None,
            })
        })
        .ok_or_else(|| {
            AppError::new(
                4,
                format!("Couldn't find {host} on this network. Check it is on, or enter its address instead."),
            )
        })
}

/// Whether `address` is one of `hosts`, comparing names by what they resolve to.
fn same_device(address: Ipv4Addr, hosts: &[String], port: u16) -> bool {
    hosts.iter().any(|host| resolve(host, port).is_ok_and(|ip| ip == address))
}

/// One computer is one device: refuse an address the Spark or another device uses.
fn refuse_duplicate(address: Ipv4Addr, port: u16) -> Result<()> {
    let spark = state::config().map(|config| config.hosts).unwrap_or_default();
    let mut saved = vec![("your Spark".to_string(), spark)];
    saved.extend(nodes::saved_hosts()?);
    if let Some((label, _)) = saved.iter().find(|(_, hosts)| same_device(address, hosts, port)) {
        return Err(invalid(&format!(
            "{address} is already added as {label}. Each computer only needs adding once."
        )));
    }
    Ok(())
}

fn validate_user(user: &str) -> Result<()> {
    if user.is_empty()
        || user.len() > 64
        || user.starts_with(['-', '.'])
        || !user
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    {
        return Err(invalid(
            "Enter the account name you sign in with on that device, for example benny",
        ));
    }
    Ok(())
}

/// The device's own folder under the state root. It must be a real directory.
fn device_dir(id: &str, create: bool) -> Result<PathBuf> {
    nodes::validate_id(id)?;
    let dir = state::root()?.join("devices").join(id);
    if create {
        fs::create_dir_all(&dir)?;
    }
    match fs::symlink_metadata(&dir) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => Ok(dir),
        Ok(_) => Err(AppError::new(
            3,
            "The device's setup folder is not a plain folder; nothing was changed",
        )),
        Err(_) if !create => Err(invalid(
            "Start by naming the device, so DGXBridge can make its key",
        )),
        Err(error) => Err(error.into()),
    }
}

fn ssh_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Make this device's key once and return its public half, which the owner's
/// setup command authorizes on the device.
pub fn key(id: &str) -> Result<Value> {
    if nodes::get(id).is_ok() {
        return Err(invalid(
            "A device with that name is already added; choose another name",
        ));
    }
    let dir = device_dir(id, true)?;
    let key = dir.join("key");
    if !key.exists() {
        let mut command = Command::new("ssh-keygen");
        command
            .args(["-q", "-t", "ed25519", "-N", "", "-C"])
            .arg(alias(id))
            .arg("-f")
            .arg(&key);
        match transport::run_process(command, &[], STEP)? {
            transport::Ran::Exited((Some(0), _, _)) if key.exists() => {}
            _ => {
                return Err(AppError::new(
                    3,
                    "Windows OpenSSH could not make a key for this device",
                ));
            }
        }
    }
    let public = fs::read_to_string(dir.join("key.pub"))?;
    let public = public.trim();
    if !public.starts_with("ssh-ed25519 ") || public.contains(['\r', '\n', '\'', '"']) {
        return Err(AppError::new(3, "The device's public key is unreadable"));
    }
    Ok(json!({"ok":true,"id":id,"public_key":public}))
}

/// The device's current ed25519 host key, as presented to anyone who asks.
fn keyscan(host: &str, port: u16) -> Result<String> {
    let mut command = Command::new("ssh-keyscan");
    command
        .args(["-T", "5", "-t", "ed25519", "-p"])
        .arg(port.to_string())
        .arg(host);
    let output = match transport::run_process(command, &[], STEP)? {
        transport::Ran::Exited((_, stdout, _)) => stdout,
        transport::Ran::TimedOut(_) => Vec::new(),
    };
    let text = String::from_utf8_lossy(&output);
    for line in text.lines().filter(|line| !line.starts_with('#')) {
        let mut parts = line.split_whitespace();
        if let (Some(_), Some("ssh-ed25519"), Some(blob)) = (parts.next(), parts.next(), parts.next()) {
            if STANDARD.decode(blob).is_ok_and(|bytes| !bytes.is_empty()) {
                return Ok(blob.to_string());
            }
        }
    }
    Err(AppError::new(
        4,
        format!("Couldn't reach {host} for remote setup. Check it is on, on this network, and that the setup command ran on it."),
    ))
}

pub fn fingerprint(blob: &str) -> Result<String> {
    let bytes = STANDARD
        .decode(blob)
        .map_err(|_| AppError::new(3, "The device presented an unreadable identity"))?;
    Ok(format!("SHA256:{}", STANDARD_NO_PAD.encode(Sha256::digest(&bytes))))
}

/// An SSH profile owned by DGXBridge: this device's key, its one pinned host key,
/// key-only login and no prompts.
fn profile_text(id: &str, host: &str, user: &str, port: u16, key: &Path, known: &Path) -> String {
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    format!(
        "Host {alias}\n    HostName {host}\n    User {user}\n    Port {port}\n    IdentityFile \"{key}\"\n    IdentitiesOnly yes\n    BatchMode yes\n    PreferredAuthentications publickey\n    PasswordAuthentication no\n    KbdInteractiveAuthentication no\n    HostKeyAlias {alias}\n    HostKeyAlgorithms ssh-ed25519\n    UserKnownHostsFile \"{known}\"\n    GlobalKnownHostsFile {null}\n    StrictHostKeyChecking yes\n    UpdateHostKeys no\n    ConnectTimeout 5\n    ServerAliveInterval 15\n    ServerAliveCountMax 3\n",
        alias = alias(id),
        key = ssh_path(key),
        known = ssh_path(known),
    )
}

fn write_profile(dir: &Path, prefix: &str, id: &str, host: &str, user: &str, port: u16, blob: &str) -> Result<PathBuf> {
    let known = dir.join(format!("{prefix}known_hosts"));
    state::atomic_bytes(&known, format!("{} ssh-ed25519 {blob}\n", alias(id)).as_bytes())?;
    let profile = dir.join(format!("{prefix}ssh.conf"));
    state::atomic_bytes(&profile, profile_text(id, host, user, port, &dir.join("key"), &known).as_bytes())?;
    Ok(profile)
}

enum Remote {
    Ran(i32, String),
    Refused,
    Unreachable,
}

fn remote(profile: &Path, id: &str, command_line: &str) -> Result<Remote> {
    let mut command = Command::new("ssh");
    command.arg("-F").arg(profile).arg(alias(id)).arg(command_line);
    Ok(match transport::run_process(command, &[], STEP)? {
        transport::Ran::Exited((Some(255), _, stderr)) => {
            let text = String::from_utf8_lossy(&stderr).to_ascii_lowercase();
            if text.contains("permission denied") || text.contains("too many authentication") {
                Remote::Refused
            } else {
                Remote::Unreachable
            }
        }
        transport::Ran::Exited((code, stdout, _)) => Remote::Ran(
            code.unwrap_or(-1),
            String::from_utf8_lossy(&stdout).into_owned(),
        ),
        transport::Ran::TimedOut(_) => Remote::Unreachable,
    })
}

/// `wsl -l -q` writes UTF-16LE; keep plain distribution names only.
fn wsl_names(raw: &str) -> Vec<String> {
    let bytes: Vec<u16> = raw
        .as_bytes()
        .chunks(2)
        .map(|pair| u16::from_le_bytes([pair[0], *pair.get(1).unwrap_or(&0)]))
        .collect();
    let decoded = if raw.as_bytes().iter().skip(1).step_by(2).take(8).all(|b| *b == 0) {
        String::from_utf16_lossy(&bytes)
    } else {
        raw.to_string()
    };
    decoded
        .lines()
        .map(|line| line.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}' || c == '\0'))
        .filter(|name| {
            !name.is_empty()
                && name.len() <= 64
                && !name.starts_with("docker-desktop")
                && name.as_bytes()[0].is_ascii_alphanumeric()
                && name.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        })
        .map(String::from)
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .fold(Vec::new(), |mut names: Vec<String>, name| {
            // General-purpose Linux first, so the wizard's default is the obvious one.
            if name.starts_with("Ubuntu") || name.starts_with("Debian") {
                names.insert(names.iter().take_while(|n| n.starts_with("Ubuntu") || n.starts_with("Debian")).count(), name);
            } else {
                names.push(name);
            }
            names
        })
}

fn gpus(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| {
            let (name, memory) = line.rsplit_once(',')?;
            let mib = memory.trim().parse::<u64>().ok()?;
            let name = name.trim();
            (!name.is_empty() && name.len() <= 96).then(|| json!({"name":name,"memory_gib":(mib as f64 / 1024.0 * 10.0).round() / 10.0}))
        })
        .take(8)
        .collect()
}

/// Check the device before trusting it: who answers at `host`, whether this
/// device's key can log in, and what it runs. Nothing is registered.
pub fn probe(id: &str, name: &str, user: &str, port: u16) -> Result<Value> {
    validate_user(user)?;
    let ip = resolve(name, port)?;
    refuse_duplicate(ip, port)?;
    let dir = device_dir(id, false)?;
    if !dir.join("key").exists() {
        return Err(invalid("Start by naming the device, so DGXBridge can make its key"));
    }
    let address = ip.to_string();
    let host = address.as_str();
    let blob = keyscan(host, port)?;
    let fingerprint = fingerprint(&blob)?;
    let profile = write_profile(&dir, "candidate-", id, host, user, port, &blob)?;
    let base = json!({"ok":true,"id":id,"name":name,"address":host,"port":port,"user":user,"fingerprint":fingerprint});
    let linux = match remote(&profile, id, "uname -sm")? {
        Remote::Refused => {
            return Ok(with(base, json!({"login":"refused","message":format!("{host} answered but didn't accept this device's key for {user}. Run the setup command on it (as administrator on Windows), and check the account name.")})));
        }
        Remote::Unreachable => {
            return Ok(with(base, json!({"login":"unreachable","message":format!("{host} stopped answering during the check. Make sure it stays on, then check again.")})));
        }
        Remote::Ran(0, out) if out.trim_start().starts_with("Linux") => Some(out.split_whitespace().nth(1).unwrap_or("").to_string()),
        Remote::Ran(..) => None,
    };
    let hostname = match remote(&profile, id, "hostname")? {
        Remote::Ran(0, out) => out.trim().chars().take(64).collect::<String>(),
        _ => String::new(),
    };
    let gpu_list = match remote(&profile, id, "nvidia-smi --query-gpu=name,memory.total --format=csv,noheader,nounits")? {
        Remote::Ran(0, out) => gpus(&out),
        _ => Vec::new(),
    };
    let mut result = with(base, json!({"login":"ok","hostname":hostname,"gpus":gpu_list}));
    if let Some(arch) = linux {
        result["os"] = json!("linux");
        result["architecture"] = json!(arch);
        return Ok(result);
    }
    // Windows OpenSSH runs commands through cmd.exe, where `ver` is built in.
    let windows = matches!(remote(&profile, id, "ver")?, Remote::Ran(0, out) if out.contains("Windows"));
    if !windows {
        result["os"] = json!(null);
        result["message"] = json!("This device is neither Linux nor Windows as far as DGXBridge can tell.");
        return Ok(result);
    }
    result["os"] = json!("windows");
    let mut distributions = Vec::new();
    if let Remote::Ran(0, out) = remote(&profile, id, "wsl.exe -l -q")? {
        for name in wsl_names(&out) {
            if let Remote::Ran(0, who) = remote(&profile, id, &format!("wsl.exe -d {name} -e whoami"))? {
                let who = who.trim().to_string();
                let plain = !who.is_empty()
                    && who.len() <= 32
                    && (who.as_bytes()[0].is_ascii_lowercase() || who.starts_with('_'))
                    && who.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_-".contains(&c));
                if plain {
                    distributions.push(json!({"distribution":name,"user":who}));
                }
            }
        }
    }
    if distributions.is_empty() {
        result["message"] = json!("This Windows PC needs Windows' Linux layer (WSL) before it can run jobs.");
    }
    result["wsl"] = json!(distributions);
    Ok(result)
}

fn with(mut base: Value, extra: Value) -> Value {
    if let Value::Object(extra) = extra {
        for (key, value) in extra {
            base[key] = value;
        }
    }
    base
}

/// Trust the device the owner confirmed and register it. The fingerprint must
/// still match what the device presents now; otherwise nothing is saved.
pub fn trust(
    id: &str,
    label: &str,
    host: &str,
    user: &str,
    port: u16,
    confirmed: &str,
    wsl: Option<(&str, &str)>,
) -> Result<Value> {
    let ip = resolve(host, port)?;
    refuse_duplicate(ip, port)?;
    let address = ip.to_string();
    let host = address.as_str();
    validate_user(user)?;
    if label.trim().is_empty() || label.len() > 96 || label.chars().any(char::is_control) {
        return Err(invalid("Give the device a name of at most 96 characters"));
    }
    let dir = device_dir(id, false)?;
    let blob = keyscan(host, port)?;
    if fingerprint(&blob)? != confirmed {
        return Err(AppError::new(
            7,
            "The device's identity changed since you checked it, so nothing was saved. Check that you're adding the right computer, then check it again.",
        ));
    }
    let profile = write_profile(&dir, "", id, host, user, port, &blob)?;
    match remote(&profile, id, "hostname")? {
        Remote::Ran(0, _) => {}
        _ => {
            return Err(AppError::new(
                4,
                "The device stopped accepting this device's key before it could be saved. Check it again.",
            ));
        }
    }
    let endpoint = match wsl {
        Some((distribution, wsl_user)) => nodes::Endpoint::WindowsWsl {
            distribution: distribution.to_string(),
            user: wsl_user.to_string(),
            controller_path: if wsl_user == "root" {
                "/root/sparkdev/controller.py".to_string()
            } else {
                format!("/home/{wsl_user}/sparkdev/controller.py")
            },
        },
        None => nodes::Endpoint::LinuxSsh,
    };
    let registered = fleet::register_node(fleet::Registration {
        id: id.to_string(),
        label: label.trim().to_string(),
        ssh_alias: alias(id),
        hosts: vec![host.to_string()],
        host_key_alias: alias(id),
        identity_file: Some(dir.join("key")),
        user: Some(user.to_string()),
        port,
        ssh_config: Some(profile),
        endpoint,
        priority: 0,
    })?;
    for name in ["candidate-known_hosts", "candidate-ssh.conf"] {
        let _ = fs::remove_file(dir.join(name));
    }
    Ok(registered)
}

#[cfg(windows)]
fn reverse_name(ip: Ipv4Addr) -> Option<String> {
    use windows_sys::Win32::Networking::WinSock::{AF_INET, GetNameInfoW, IN_ADDR, IN_ADDR_0, NI_NAMEREQD, SOCKADDR, SOCKADDR_IN};
    let address = SOCKADDR_IN {
        sin_family: AF_INET,
        sin_port: 0,
        sin_addr: IN_ADDR { S_un: IN_ADDR_0 { S_addr: u32::from_ne_bytes(ip.octets()) } },
        sin_zero: [0; 8],
    };
    let mut name = [0u16; 256];
    let found = unsafe {
        GetNameInfoW(
            (&raw const address).cast::<SOCKADDR>(),
            std::mem::size_of::<SOCKADDR_IN>() as i32,
            name.as_mut_ptr(),
            name.len() as u32,
            std::ptr::null_mut(),
            0,
            NI_NAMEREQD as i32,
        )
    };
    let end = name.iter().position(|&c| c == 0)?;
    let text = String::from_utf16_lossy(&name[..end]);
    let short = text.split('.').next().unwrap_or("").to_string();
    (found == 0 && !short.is_empty() && short.parse::<Ipv4Addr>().is_err()).then_some(short)
}
#[cfg(not(windows))]
fn reverse_name(_: Ipv4Addr) -> Option<String> {
    None
}

/// Computers on this laptop's private networks that answer on the SSH port,
/// with their names where the network reports them. Read-only.
pub fn scan() -> Result<Value> {
    let adapters = discover::adapters()?;
    let own: Vec<Ipv4Addr> = adapters.iter().flat_map(|a| a.addresses.iter().map(|x| x.ip)).collect();
    let mut addresses = Vec::new();
    for network in discover::sweep_networks(&adapters, discover::rfc1918) {
        addresses.extend(discover::targets(&network, &own, &discover::routed_via));
    }
    addresses.truncate(SCAN_LIMIT);
    let (found, open) = mpsc::channel();
    for chunk in addresses.chunks(64) {
        let handles: Vec<_> = chunk
            .iter()
            .map(|ip| {
                let (ip, found) = (*ip, found.clone());
                thread::spawn(move || {
                    if TcpStream::connect_timeout(&SocketAddr::from((ip, 22)), SCAN_CONNECT).is_ok() {
                        let _ = found.send(ip);
                    }
                })
            })
            .collect();
        for handle in handles {
            let _ = handle.join();
        }
    }
    drop(found);
    let mut answering: Vec<Ipv4Addr> = open.iter().collect();
    answering.sort();
    let (named, names) = mpsc::channel();
    for ip in &answering {
        let (ip, named) = (*ip, named.clone());
        thread::spawn(move || {
            let _ = named.send((ip, reverse_name(ip)));
        });
    }
    drop(named);
    let deadline = Instant::now() + NAME_BUDGET;
    let mut labels = std::collections::BTreeMap::new();
    while let Ok((ip, name)) = names.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        labels.insert(ip, name);
    }
    let candidates: Vec<Value> = answering
        .iter()
        .map(|ip| json!({"address":ip.to_string(),"name":labels.get(ip).cloned().flatten()}))
        .collect();
    Ok(json!({"ok":true,"candidates":candidates,"checked":addresses.len()}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_match_openssh() {
        // Windows OpenSSH `ssh-keygen -lf` on this public key prints exactly this.
        let blob = "AAAAC3NzaC1lZDI1NTE5AAAAIK1qY7sQ/4ABeSXU6Xmg1OkFsb1Y5x2Il2OeNwIuFn5r";
        assert_eq!(fingerprint(blob).unwrap(), "SHA256:Mp/+7chdCKA18z2Nkc1AbQBK9Y0YoxlDfw9ZkdI/cfo");
        assert!(fingerprint("not base64!").is_err());
    }

    #[test]
    fn hosts_and_users_never_need_quoting() {
        for good in ["AntonPC", "192.168.1.121", "antonpc.local", "spark-bb3b"] {
            assert!(validate_host(good).is_ok(), "{good}");
        }
        for bad in ["", "-oProxyCommand=x", ".hidden", "anton pc", "a;b", "a\"b", &"x".repeat(254)] {
            assert!(validate_host(bad).is_err(), "{bad}");
        }
        for good in ["benny", "Benny", "ben.v", "b_1"] {
            assert!(validate_user(good).is_ok(), "{good}");
        }
        for bad in ["", "-l", "ben ny", "ben@x", "a'b"] {
            assert!(validate_user(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_profile_pins_one_key_and_never_prompts() {
        let text = profile_text("antonpc", "192.168.1.121", "benny", 22, Path::new(r"C:\State\devices\antonpc\key"), Path::new(r"C:\State\devices\antonpc\known_hosts"));
        for line in [
            "Host dgxbridge-antonpc",
            "HostName 192.168.1.121",
            "User benny",
            "IdentityFile \"C:/State/devices/antonpc/key\"",
            "IdentitiesOnly yes",
            "BatchMode yes",
            "PasswordAuthentication no",
            "HostKeyAlias dgxbridge-antonpc",
            "UserKnownHostsFile \"C:/State/devices/antonpc/known_hosts\"",
            "StrictHostKeyChecking yes",
            "UpdateHostKeys no",
        ] {
            assert!(text.contains(line), "{line}");
        }
    }

    #[test]
    fn wsl_lists_decode_and_skip_docker_and_odd_names() {
        let utf16: Vec<u8> = "Ubuntu-20.04\r\ndocker-desktop\r\nBad Name\r\nDebian\r\n"
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect();
        // ASCII text in UTF-16LE is also valid UTF-8, which is how it arrives.
        let raw = String::from_utf8(utf16).unwrap();
        assert_eq!(wsl_names(&raw), ["Ubuntu-20.04", "Debian"]);
        assert_eq!(wsl_names("NVIDIA-Workbench
Ubuntu-22.04
"), ["Ubuntu-22.04", "NVIDIA-Workbench"]);
        assert_eq!(wsl_names("Ubuntu-24.04\n"), ["Ubuntu-24.04"]);
    }

    #[test]
    fn one_computer_is_matched_by_address_or_name() {
        let address: Ipv4Addr = "192.0.2.9".parse().unwrap();
        assert!(same_device(address, &["192.0.2.9".to_string()], 22));
        assert!(!same_device(address, &["192.0.2.10".to_string(), "not a host!".to_string()], 22));
        assert!(same_device("127.0.0.1".parse().unwrap(), &["localhost".to_string()], 22));
    }

    #[test]
    fn gpu_lines_parse_name_and_memory() {
        let parsed = gpus("NVIDIA GeForce RTX 4090, 24564\nnot a gpu\n");
        assert_eq!(parsed, [json!({"name":"NVIDIA GeForce RTX 4090","memory_gib":24.0})]);
    }
}
