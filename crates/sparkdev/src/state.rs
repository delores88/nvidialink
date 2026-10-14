use crate::{AppError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

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
    pub local_build_gate: Option<PathBuf>,
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
    #[cfg(windows)]
    if std::env::var_os("SPARKDEV_HOME").is_none() && !path.join("config.json").exists() {
        if let Some(previous) = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|p| p.join("SparkDevBridge/config.json"))
        {
            if previous != path.join("config.json") && previous.is_file() {
                let bytes = fs::read(previous)?;
                let config: Config = serde_json::from_slice(&bytes)?;
                validate_config(&config)?;
                atomic_json(&path.join("config.json"), &config)?;
            }
        }
    }
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
    // KF_FLAG_NO_PACKAGE_REDIRECTION keeps state identical in packaged Codex and Explorer.
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
pub fn validate_config(config: &Config) -> Result<()> {
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
