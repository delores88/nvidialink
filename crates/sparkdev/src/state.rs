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
}
