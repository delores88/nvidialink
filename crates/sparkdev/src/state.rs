use crate::{AppError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ConnectionMode {
    LanOnly,
    RemoteOnly,
    Both,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub ssh_alias: String,
    pub hosts: Vec<String>,
    pub host_key_alias: String,
    #[serde(default)]
    pub identity_file: Option<PathBuf>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub allow_remote: bool,
    #[serde(default)]
    pub connection_mode: Option<ConnectionMode>,
    #[serde(default = "default_enabled")]
    pub connection_enabled: bool,
    #[serde(default = "default_laptop_jobs")]
    pub laptop_jobs: usize,
    #[serde(default)]
    pub local_build_gate: Option<PathBuf>,
}
fn default_enabled() -> bool {
    true
}
fn default_laptop_jobs() -> usize {
    2
}
impl Config {
    pub fn mode(&self) -> ConnectionMode {
        self.connection_mode.unwrap_or(if self.allow_remote {
            ConnectionMode::Both
        } else {
            ConnectionMode::LanOnly
        })
    }
    pub fn allows_lan(&self) -> bool {
        self.mode() != ConnectionMode::RemoteOnly
    }
    pub fn allows_remote(&self) -> bool {
        self.mode() != ConnectionMode::LanOnly
    }
    pub fn connection_info(&self) -> Value {
        serde_json::json!({"mode":self.mode(),"enabled":self.connection_enabled,"remote_allowed":self.allows_remote()})
    }
    pub fn laptop_info(&self) -> Value {
        serde_json::json!({"concurrency_limit":self.laptop_jobs,"logical_parallelism":std::thread::available_parallelism().ok().map(|n|n.get()),"cpu_memory_caps_enforced":false})
    }
}

/// Serialize config updates under one owner-local lock so route changes cannot
/// overwrite SSH edits made by another CLI or desktop process.
pub fn update_config(change: impl FnOnce(&mut Config)) -> Result<Config> {
    let directory = root()?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(directory.join("config.lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let mut result = config()?;
    change(&mut result);
    validate_config(&result)?;
    atomic_json(&directory.join("config.json"), &result)?;
    Ok(result)
}
fn default_port() -> u16 {
    22
}
pub fn root() -> Result<PathBuf> {
    let path = if let Some(value) = std::env::var_os("SPARKDEV_HOME") {
        PathBuf::from(value)
    } else if cfg!(windows) {
        windows_local_data()?.join("SparkDevBridge")
    } else {
        PathBuf::from(
            std::env::var_os("HOME").ok_or_else(|| AppError::new(3, "HOME is unavailable"))?,
        )
        .join(".local/state/sparkdev")
    };
    fs::create_dir_all(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(path)
}
#[cfg(windows)]
fn windows_local_data() -> Result<PathBuf> {
    use windows_sys::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath},
    };
    let mut raw = std::ptr::null_mut();
    // This asks for the owner folder's logical path. Packaged callers can still
    // have their filesystem I/O redirected; diagnostics reports the actual path.
    let hr = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_LocalAppData,
            0x00010000,
            std::ptr::null_mut(),
            &mut raw,
        )
    };
    if hr >= 0 && !raw.is_null() {
        let mut len = 0usize;
        while len < 32768 && unsafe { *raw.add(len) } != 0 {
            len += 1
        }
        let text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(raw, len) });
        unsafe { CoTaskMemFree(raw.cast()) };
        let path = PathBuf::from(text);
        if path.is_absolute() {
            return Ok(path);
        }
    }
    let profile = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| AppError::new(3, "Windows owner profile is unavailable"))?;
    Ok(profile.join("AppData/Local"))
}
#[cfg(not(windows))]
fn windows_local_data() -> Result<PathBuf> {
    Err(AppError::new(3, "Windows state path unavailable"))
}
pub fn config() -> Result<Config> {
    let file = root()?.join("config.json");
    if !file.exists() {
        return Err(AppError::new(3, "Run sparkdev setup before connecting"));
    }
    let result: Config = serde_json::from_slice(&fs::read(file)?)?;
    validate_config(&result)?;
    Ok(result)
}
pub fn diagnostics() -> Value {
    let configured = match root() {
        Ok(path) => path,
        Err(_) => {
            return serde_json::json!({"configured_state_root":null,"resolved_state_root":null,"packaged_context":null,"redirected":null});
        }
    };
    let (resolved, packaged) = physical_context(&configured);
    let normalize = |path: &Path| {
        let text = path.to_string_lossy().replace('/', "\\");
        let text = text
            .strip_prefix("\\\\?\\UNC\\")
            .map(|s| format!("\\\\{s}"))
            .unwrap_or_else(|| text.strip_prefix("\\\\?\\").unwrap_or(&text).to_owned());
        if cfg!(windows) {
            text.to_lowercase()
        } else {
            text
        }
    };
    let redirected = resolved
        .as_ref()
        .map(|path| normalize(path) != normalize(&configured));
    serde_json::json!({"configured_state_root":configured,"resolved_state_root":resolved,"packaged_context":packaged,"redirected":redirected})
}
#[cfg(windows)]
fn physical_context(path: &Path) -> (Option<PathBuf>, Option<bool>) {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFinalPathNameByHandleW(
            handle: *mut std::ffi::c_void,
            buffer: *mut u16,
            size: u32,
            flags: u32,
        ) -> u32;
        fn GetCurrentPackageFullName(length: *mut u32, name: *mut u16) -> i32;
    }
    let mut length = 0u32;
    let package_result = unsafe { GetCurrentPackageFullName(&mut length, std::ptr::null_mut()) };
    let packaged = match package_result {
        15700 => Some(false),
        0 | 122 => Some(true),
        _ => None,
    };
    let resolved = fs::OpenOptions::new()
        .read(true)
        .custom_flags(0x02000000)
        .open(path)
        .ok()
        .and_then(|file| {
            let mut buffer = vec![0u16; 32768];
            let size = unsafe {
                GetFinalPathNameByHandleW(
                    file.as_raw_handle(),
                    buffer.as_mut_ptr(),
                    buffer.len() as u32,
                    0,
                )
            } as usize;
            if size > 0 && size < buffer.len() {
                Some(PathBuf::from(String::from_utf16_lossy(&buffer[..size])))
            } else {
                None
            }
        });
    (resolved, packaged)
}
#[cfg(not(windows))]
fn physical_context(path: &Path) -> (Option<PathBuf>, Option<bool>) {
    (fs::canonicalize(path).ok(), Some(false))
}
pub fn validate_config(config: &Config) -> Result<()> {
    let maximum = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .max(2)
        .min(64);
    if config.laptop_jobs == 0 || config.laptop_jobs > maximum {
        return Err(AppError::new(
            2,
            format!("Laptop concurrency must be between 1 and {maximum}"),
        ));
    }
    for value in std::iter::once(&config.ssh_alias)
        .chain(config.hosts.iter())
        .chain(std::iter::once(&config.host_key_alias))
        .chain(config.user.iter())
    {
        validate_host(value)?;
    }
    if config.port == 0 {
        return Err(AppError::new(2, "SSH port must be nonzero"));
    }
    if let Some(path) = &config.identity_file {
        if !path.is_absolute() {
            return Err(AppError::new(2, "SSH identity path must be absolute"));
        }
    }
    if let Some(path) = &config.local_build_gate {
        if !path.is_absolute() {
            return Err(AppError::new(2, "Build gate path must be absolute"));
        }
    }
    Ok(())
}
/// The single plain-identifier rule for SSH aliases, hosts, users and pins.
/// Values reach OpenSSH as separate arguments, so a leading dash or shell or
/// option syntax is refused rather than escaped.
pub fn validate_host(value: &str) -> Result<()> {
    if value.is_empty()
        || value.starts_with('-')
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:-@[]".contains(c))
    {
        return Err(AppError::new(
            2,
            "SSH host, user and pin alias must be plain host identifiers",
        ));
    }
    Ok(())
}

const LAN_ROUTE_FILE: &str = "lan-route.json";
const LAN_ROUTE_MAX: usize = 8;
const LAN_ROUTE_PER_BINDING: usize = 4;
const LAN_ROUTE_MAX_AGE: u64 = 30 * 24 * 60 * 60;
/// A newest entry verified longer ago than this is rewritten on its next
/// success, so an address in daily use never ages out.
const LAN_ROUTE_REFRESH: u64 = 24 * 60 * 60;
/// Clock skew tolerated for a verified_at in the future. Anything later was
/// written with a wrong clock and would otherwise outlive the 30-day expiry.
const LAN_ROUTE_MAX_SKEW: u64 = 5 * 60;
const LAN_ROUTE_MAX_BYTES: u64 = 64 * 1024;

#[derive(Serialize, Deserialize)]
struct LanRoutes {
    v: u32,
    entries: Vec<LanRoute>,
}
#[derive(Clone, Serialize, Deserialize)]
struct LanRoute {
    binding: String,
    host: String,
    verified_at: u64,
}

/// Remembered LAN addresses belong to one SSH identity: the alias, the pinned
/// host key alias, port and login user. Changing any of them starts empty.
pub fn lan_binding(config: &Config) -> String {
    use sha2::{Digest, Sha256};
    let identity = format!(
        "{}|{}|{}|{}",
        config.ssh_alias,
        config.host_key_alias,
        config.port,
        config.user.as_deref().unwrap_or("")
    );
    format!("{:x}", Sha256::digest(identity.as_bytes()))
}
fn read_lan_routes(path: &Path) -> Vec<LanRoute> {
    use std::io::Read;
    let Ok(file) = fs::File::open(path) else {
        return Vec::new();
    };
    let mut bytes = Vec::new();
    if file
        .take(LAN_ROUTE_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > LAN_ROUTE_MAX_BYTES
    {
        return Vec::new();
    }
    match serde_json::from_slice::<LanRoutes>(&bytes) {
        Ok(routes) if routes.v == 1 => routes.entries,
        _ => Vec::new(),
    }
}
fn lan_route_usable(entry: &LanRoute, now: u64) -> bool {
    entry.verified_at <= now.saturating_add(LAN_ROUTE_MAX_SKEW)
        && now.saturating_sub(entry.verified_at) <= LAN_ROUTE_MAX_AGE
        && validate_host(&entry.host).is_ok()
}
/// Remembered addresses for one SSH identity.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RememberedLan {
    /// Verified addresses, most recent first.
    pub hosts: Vec<String>,
    /// The newest address when it was verified within the last day. A success
    /// there need not rewrite lan-route.json; an older head is refreshed.
    pub fresh_head: Option<String>,
}
fn remembered_at(path: &Path, config: &Config, now: u64) -> RememberedLan {
    let binding = lan_binding(config);
    let mut found = RememberedLan::default();
    for entry in read_lan_routes(path) {
        if found.hosts.len() == LAN_ROUTE_PER_BINDING {
            break;
        }
        if entry.binding == binding
            && lan_route_usable(&entry, now)
            && !found
                .hosts
                .iter()
                .any(|h| h.eq_ignore_ascii_case(&entry.host))
        {
            if found.hosts.is_empty() && now.saturating_sub(entry.verified_at) < LAN_ROUTE_REFRESH {
                found.fresh_head = Some(entry.host.clone());
            }
            found.hosts.push(entry.host);
        }
    }
    found
}
#[cfg(test)]
fn remembered_hosts_at(path: &Path, config: &Config, now: u64) -> Vec<String> {
    remembered_at(path, config, now).hosts
}
fn remember_host_at(path: &Path, config: &Config, host: &str, now: u64) -> Result<()> {
    validate_host(host)?;
    let binding = lan_binding(config);
    let mut entries = vec![LanRoute {
        binding: binding.clone(),
        host: host.into(),
        verified_at: now,
    }];
    entries.extend(read_lan_routes(path).into_iter().filter(|entry| {
        lan_route_usable(entry, now)
            && !(entry.binding == binding && entry.host.eq_ignore_ascii_case(host))
    }));
    let mut per_binding = std::collections::BTreeMap::<String, usize>::new();
    entries.retain(|entry| {
        let count = per_binding.entry(entry.binding.clone()).or_default();
        *count += 1;
        *count <= LAN_ROUTE_PER_BINDING
    });
    entries.truncate(LAN_ROUTE_MAX);
    atomic_json(path, &LanRoutes { v: 1, entries })
}
fn forget_host_at(path: &Path, config: &Config, host: &str) -> Result<()> {
    let binding = lan_binding(config);
    let entries = read_lan_routes(path);
    let before = entries.len();
    let entries: Vec<_> = entries
        .into_iter()
        .filter(|entry| !(entry.binding == binding && entry.host.eq_ignore_ascii_case(host)))
        .collect();
    if entries.len() != before {
        atomic_json(path, &LanRoutes { v: 1, entries })?;
    }
    Ok(())
}
/// lan-route.json is written only when the state folder is known not to be
/// redirected. Unknown (null) counts as redirected: a virtualized private
/// copy would shadow the owner's file without ever learning new addresses.
fn lan_routes_writable(diagnostics: &Value) -> bool {
    diagnostics["redirected"] == false
}
/// The one write path for remember and forget, so both share the guard.
fn with_lan_routes(change: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let directory = root()?;
    with_lan_routes_in(&directory, lan_routes_writable(&diagnostics()), change)
}
/// A skipped write creates neither lan-route.lock nor lan-route.json.
fn with_lan_routes_in(
    directory: &Path,
    writable: bool,
    change: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    if !writable {
        return Ok(());
    }
    let lock = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(directory.join("lan-route.lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    change(&directory.join(LAN_ROUTE_FILE))
}
/// Verified LAN addresses for this SSH identity, most recent first. Missing,
/// oversized or corrupt state, stale entries and invalid hosts are ignored:
/// the list only orders candidates, it never authorizes one.
pub fn remembered_hosts(config: &Config) -> Vec<String> {
    remembered_lan(config).hosts
}
/// remembered_hosts plus whether the newest entry is fresh (see RememberedLan).
pub fn remembered_lan(config: &Config) -> RememberedLan {
    root()
        .map(|directory| remembered_at(&directory.join(LAN_ROUTE_FILE), config, now()))
        .unwrap_or_default()
}
/// Record an address that just completed pinned SSH authentication. Skipped
/// unless the state folder is known not to be redirected (virtualized);
/// config.json is never touched.
pub fn remember_host(config: &Config, host: &str) -> Result<()> {
    validate_host(host)?;
    with_lan_routes(|path| remember_host_at(path, config, host, now()))
}
/// Drop a remembered address that proved stale. Skipped under the same
/// redirection guard as remember_host.
pub fn forget_host(config: &Config, host: &str) -> Result<()> {
    with_lan_routes(|path| forget_host_at(path, config, host))
}
pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    atomic_bytes(path, &serde_json::to_vec(value)?)
}
pub fn atomic_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::new(3, "Invalid state path"))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut out = options.open(&temporary)?;
    out.write_all(bytes)?;
    out.sync_all()?;
    drop(out);
    fs::rename(&temporary, path)?;
    Ok(())
}
pub fn read_json(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
pub fn save_request(id: &str, value: &Value) -> Result<()> {
    validate_id(id)?;
    let folder = root()?.join("requests");
    fs::create_dir_all(&folder)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(folder.join("requests.lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let path = folder.join(format!("{id}.json"));
    if path.exists() {
        if read_json(&path)? != *value {
            return Err(AppError::new(
                2,
                "Request ID already belongs to different content",
            ));
        }
    } else {
        atomic_json(&path, value)?;
    }
    Ok(())
}
pub fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(AppError::new(2, "Invalid job or request identifier"));
    }
    Ok(())
}
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Tests that reach `root()` must hold this guard: it serializes the
/// process-wide SPARKDEV_HOME override and points it at a fresh temporary
/// directory, so no test can read or write the owner's real state.
#[cfg(test)]
pub(crate) struct TestHome {
    pub(crate) path: PathBuf,
    previous: Option<std::ffi::OsString>,
    _directory: tempfile::TempDir,
    _guard: std::sync::MutexGuard<'static, ()>,
}
#[cfg(test)]
impl TestHome {
    pub(crate) fn new() -> Self {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let directory = tempfile::tempdir().unwrap();
        // The canonical form matches what redirection diagnostics resolve.
        let path = fs::canonicalize(directory.path()).unwrap();
        let previous = std::env::var_os("SPARKDEV_HOME");
        // SAFETY: every test that changes or depends on SPARKDEV_HOME holds LOCK.
        unsafe { std::env::set_var("SPARKDEV_HOME", &path) };
        Self {
            path,
            previous,
            _directory: directory,
            _guard: guard,
        }
    }
}
#[cfg(test)]
impl Drop for TestHome {
    fn drop(&mut self) {
        // SAFETY: LOCK is still held; it is released after this body runs.
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var("SPARKDEV_HOME", value),
                None => std::env::remove_var("SPARKDEV_HOME"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn legacy(remote: bool) -> Config {
        serde_json::from_value(serde_json::json!({"ssh_alias":"spark","hosts":["192.0.2.1"],"host_key_alias":"spark-pin","allow_remote":remote})).unwrap()
    }
    #[test]
    fn legacy_connection_settings_migrate_without_losing_references() {
        let mut config = legacy(true);
        assert_eq!(config.mode(), ConnectionMode::Both);
        assert!(config.connection_enabled);
        assert_eq!(config.laptop_jobs, 2);
        assert_eq!(legacy(false).mode(), ConnectionMode::LanOnly);
        config.connection_mode = Some(ConnectionMode::RemoteOnly);
        config.connection_enabled = false;
        let restored: Config =
            serde_json::from_slice(&serde_json::to_vec(&config).unwrap()).unwrap();
        assert_eq!(restored.mode(), ConnectionMode::RemoteOnly);
        assert!(!restored.connection_enabled);
        assert_eq!(restored.host_key_alias, "spark-pin");
        assert_eq!(restored.hosts, vec!["192.0.2.1"]);
    }
    #[test]
    fn connection_mode_and_laptop_limits_reject_invalid_configuration() {
        let mut config = legacy(false);
        config.laptop_jobs = 0;
        assert!(validate_config(&config).is_err());
        config.laptop_jobs = 65;
        assert!(validate_config(&config).is_err());
        assert!(serde_json::from_value::<Config>(serde_json::json!({"ssh_alias":"spark","hosts":[],"host_key_alias":"pin","connection_mode":"insecure"})).is_err());
    }
    #[test]
    fn host_rule_is_shared_with_configuration_validation() {
        for good in [
            "192.0.2.1",
            "spark.local",
            "spark-1",
            "[2001:db8::1]",
            "owner@spark",
        ] {
            assert!(validate_host(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "-oProxyCommand=x",
            "a b",
            "spark;id",
            "a/b",
            "spark%eth0",
            "$(x)",
        ] {
            assert!(validate_host(bad).is_err(), "{bad}");
            let mut config = legacy(false);
            config.hosts = vec![bad.into()];
            let error = validate_config(&config).unwrap_err();
            assert_eq!(error.code, 2);
            assert_eq!(
                error.message,
                "SSH host, user and pin alias must be plain host identifiers"
            );
        }
        assert!(validate_config(&legacy(false)).is_ok());
    }
    fn route_file(path: &Path) -> Value {
        read_json(&path.join(LAN_ROUTE_FILE)).unwrap()
    }
    #[test]
    fn lan_route_roundtrip_is_bounded_and_never_touches_config() {
        let home = TestHome::new();
        let config = legacy(false);
        assert!(remembered_hosts(&config).is_empty());
        remember_host(&config, "192.0.2.10").unwrap();
        remember_host(&config, "192.0.2.11").unwrap();
        remember_host(&config, "192.0.2.10").unwrap();
        assert_eq!(remembered_hosts(&config), ["192.0.2.10", "192.0.2.11"]);
        let file = route_file(&home.path);
        assert_eq!(file["v"], 1);
        assert_eq!(file["entries"].as_array().unwrap().len(), 2);
        assert_eq!(file["entries"][0]["binding"], lan_binding(&config));
        assert_eq!(file["entries"][0]["binding"].as_str().unwrap().len(), 64);
        assert!(file["entries"][0]["verified_at"].as_u64().unwrap() > 0);
        for n in 0..6 {
            remember_host(&config, &format!("198.51.100.{n}")).unwrap();
        }
        assert_eq!(
            remembered_hosts(&config),
            [
                "198.51.100.5",
                "198.51.100.4",
                "198.51.100.3",
                "198.51.100.2"
            ]
        );
        assert_eq!(
            route_file(&home.path)["entries"].as_array().unwrap().len(),
            4
        );
        assert!(!home.path.join("config.json").exists());
        assert!(remember_host(&config, "-oProxyCommand=x").is_err());
    }
    #[test]
    fn lan_route_bindings_are_isolated_and_forget_is_scoped() {
        let home = TestHome::new();
        let first = legacy(false);
        let mut other_pin = legacy(false);
        other_pin.host_key_alias = "other-pin".into();
        let mut other_port = legacy(false);
        other_port.port = 2222;
        let mut other_user = legacy(false);
        other_user.user = Some("owner".into());
        let bindings: std::collections::BTreeSet<_> =
            [&first, &other_pin, &other_port, &other_user]
                .iter()
                .map(|c| lan_binding(c))
                .collect();
        assert_eq!(bindings.len(), 4);
        remember_host(&first, "192.0.2.20").unwrap();
        remember_host(&other_pin, "192.0.2.21").unwrap();
        remember_host(&other_pin, "192.0.2.20").unwrap();
        assert_eq!(remembered_hosts(&first), ["192.0.2.20"]);
        assert_eq!(remembered_hosts(&other_pin), ["192.0.2.20", "192.0.2.21"]);
        assert!(remembered_hosts(&other_port).is_empty());
        assert!(remembered_hosts(&other_user).is_empty());
        forget_host(&first, "192.0.2.20").unwrap();
        assert!(remembered_hosts(&first).is_empty());
        assert_eq!(remembered_hosts(&other_pin), ["192.0.2.20", "192.0.2.21"]);
        forget_host(&first, "192.0.2.99").unwrap();
        // Many identities still share one file capped at eight entries.
        for n in 0..12u16 {
            let mut config = legacy(false);
            config.port = 3000 + n;
            remember_host(&config, "192.0.2.30").unwrap();
        }
        assert_eq!(
            route_file(&home.path)["entries"].as_array().unwrap().len(),
            8
        );
    }
    #[test]
    fn lan_route_head_freshness_and_future_timestamps() {
        let home = TestHome::new();
        let config = legacy(false);
        let path = home.path.join(LAN_ROUTE_FILE);
        let now = now();
        let binding = lan_binding(&config);
        let write = |entries: Value| {
            fs::write(
                &path,
                serde_json::to_vec(&serde_json::json!({"v":1,"entries":entries})).unwrap(),
            )
            .unwrap()
        };
        // A head verified over a day ago is usable but not fresh, so its next
        // success rewrites it instead of letting it expire while in use.
        write(serde_json::json!([
            {"binding":binding,"host":"192.0.2.60","verified_at":now - LAN_ROUTE_REFRESH - 1},
            {"binding":binding,"host":"192.0.2.61","verified_at":now}
        ]));
        assert_eq!(
            remembered_at(&path, &config, now),
            RememberedLan {
                hosts: vec!["192.0.2.60".into(), "192.0.2.61".into()],
                fresh_head: None
            }
        );
        write(serde_json::json!([
            {"binding":binding,"host":"192.0.2.60","verified_at":now - 60}
        ]));
        assert_eq!(
            remembered_lan(&config).fresh_head.as_deref(),
            Some("192.0.2.60")
        );
        // A timestamp beyond the tolerated skew was written with a wrong clock
        // and is ignored rather than living past the 30-day expiry.
        write(serde_json::json!([
            {"binding":binding,"host":"192.0.2.62","verified_at":now + LAN_ROUTE_MAX_SKEW + 1},
            {"binding":binding,"host":"192.0.2.63","verified_at":now + 60},
            {"binding":binding,"host":"192.0.2.64","verified_at":u64::MAX}
        ]));
        assert_eq!(remembered_hosts(&config), ["192.0.2.63"]);
        remember_host_at(&path, &config, "192.0.2.65", now).unwrap();
        let hosts: Vec<_> = route_file(&home.path)["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["host"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(hosts, ["192.0.2.65", "192.0.2.63"]);
    }
    #[test]
    fn lan_route_writes_fail_closed_unless_the_folder_is_known_unredirected() {
        let home = TestHome::new();
        assert!(lan_routes_writable(
            &serde_json::json!({"redirected":false})
        ));
        for unknown in [
            serde_json::json!({"redirected":true}),
            serde_json::json!({"redirected":null}),
            serde_json::json!({}),
        ] {
            assert!(!lan_routes_writable(&unknown), "{unknown}");
        }
        // The test home is canonical, so the real diagnostics permit writes.
        assert!(lan_routes_writable(&diagnostics()));
        let config = legacy(false);
        let folder = home.path.join("redirected-view");
        fs::create_dir_all(&folder).unwrap();
        let file = folder.join(LAN_ROUTE_FILE);
        remember_host_at(&file, &config, "192.0.2.50", now()).unwrap();
        let before = fs::read(&file).unwrap();
        // Neither forget nor remember writes, or even creates the lock, when
        // redirection is detected or unknown.
        with_lan_routes_in(&folder, false, |path| {
            forget_host_at(path, &config, "192.0.2.50")
        })
        .unwrap();
        with_lan_routes_in(&folder, false, |path| {
            remember_host_at(path, &config, "192.0.2.51", now())
        })
        .unwrap();
        assert_eq!(fs::read(&file).unwrap(), before);
        assert!(!folder.join("lan-route.lock").exists());
        with_lan_routes_in(&folder, true, |path| {
            forget_host_at(path, &config, "192.0.2.50")
        })
        .unwrap();
        assert!(folder.join("lan-route.lock").exists());
        assert!(remembered_hosts_at(&file, &config, now()).is_empty());
    }
    #[test]
    fn corrupt_stale_or_invalid_lan_routes_are_ignored() {
        let home = TestHome::new();
        let config = legacy(false);
        let path = home.path.join(LAN_ROUTE_FILE);
        for corrupt in [&b"{broken"[..], &b"[]"[..], &br#"{"v":2,"entries":[]}"#[..]] {
            fs::write(&path, corrupt).unwrap();
            assert!(remembered_hosts(&config).is_empty());
            forget_host(&config, "192.0.2.1").unwrap();
        }
        fs::write(&path, vec![b' '; LAN_ROUTE_MAX_BYTES as usize + 1]).unwrap();
        assert!(remembered_hosts(&config).is_empty());
        remember_host(&config, "192.0.2.40").unwrap();
        assert_eq!(remembered_hosts(&config), ["192.0.2.40"]);
        let now = now();
        let binding = lan_binding(&config);
        let file = serde_json::json!({"v":1,"entries":[
            {"binding":binding,"host":"192.0.2.41","verified_at":now - LAN_ROUTE_MAX_AGE - 1},
            {"binding":binding,"host":"a b","verified_at":now},
            {"binding":binding,"host":"-oProxyCommand=x","verified_at":now},
            {"binding":binding,"host":"192.0.2.42","verified_at":now - LAN_ROUTE_MAX_AGE + 60}
        ]});
        fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
        assert_eq!(remembered_hosts(&config), ["192.0.2.42"]);
        remember_host_at(&path, &config, "192.0.2.43", now).unwrap();
        let hosts: Vec<_> = route_file(&home.path)["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["host"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(hosts, ["192.0.2.43", "192.0.2.42"]);
        assert_eq!(
            remembered_hosts_at(&path, &config, now + LAN_ROUTE_MAX_AGE - 1),
            ["192.0.2.43"]
        );
        assert!(remembered_hosts_at(&path, &config, now + LAN_ROUTE_MAX_AGE + 1).is_empty());
    }
}
