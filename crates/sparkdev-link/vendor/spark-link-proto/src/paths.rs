//! Well-known local paths for the agent and its clients (items 4 and 8).
//!
//! Windows (all under `%LOCALAPPDATA%\DeloresAI\SparkLink\`, directories created by the
//! agent with a protected DACL: current user SID + SYSTEM only):
//! - `run\endpoint.json`  current pipe name + server_key (written atomically, only while
//!   the agent holds both `run\agent.lock` and the pipe)
//! - `run\agent.lock`     exclusive per-user lock (share mode 0)
//! - `data\`              device key (DPAPI blob bound to MachineGuid), paired Sparks,
//!   consent records, agent-local audit
//! - `versions\<ver>\`    installed agent copies; `current.json` is the atomic pointer
//!
//! Unix:
//! - run dir: `$XDG_RUNTIME_DIR/delores-spark-link/` (0700), else
//!   `~/.local/state/delores/spark-link/run/` (0700). Never /tmp, never abstract sockets.
//!   Contains `agent.sock`, `endpoint.json`, `agent.lock` (flock).
//! - data: `~/.local/state/delores/spark-link-agent/`; installs:
//!   `~/.local/share/delores/spark-link-agent/versions/<ver>/` with a `current` symlink.
//!
//! Clients re-read endpoint.json on EVERY connect and verify the handshake proof
//! before sending anything else. Unix clients also lstat the run dir: owned by
//! the current uid, `mode & 0o077 == 0`, not a symlink.

use std::path::{Path, PathBuf};

pub const ENDPOINT_FILE: &str = "endpoint.json";
pub const LOCK_FILE: &str = "agent.lock";
pub const UNIX_SOCKET_FILE: &str = "agent.sock";
/// The pre-review fixed pipe name. Never used; tests pre-create it to prove squatting fails.
pub const LEGACY_FIXED_PIPE_NAME: &str = r"\\.\pipe\delores-spark-link";

pub fn windows_base(local_app_data: &Path) -> PathBuf {
    local_app_data.join("DeloresAI").join("SparkLink")
}
pub fn windows_run_dir(local_app_data: &Path) -> PathBuf {
    windows_base(local_app_data).join("run")
}
pub fn windows_data_dir(local_app_data: &Path) -> PathBuf {
    windows_base(local_app_data).join("data")
}
pub fn windows_versions_dir(local_app_data: &Path) -> PathBuf {
    windows_base(local_app_data).join("versions")
}

/// `\\.\pipe\delores-spark-link-<userSID>-<128-bit random as 32 hex>` (item 4).
pub fn pipe_name(user_sid: &str, random_hex: &str) -> String {
    format!(r"\\.\pipe\delores-spark-link-{user_sid}-{random_hex}")
}

pub fn unix_run_dir(xdg_runtime_dir: Option<&Path>, home: &Path) -> PathBuf {
    match xdg_runtime_dir {
        Some(x) if x.is_absolute() => x.join("delores-spark-link"),
        _ => home.join(".local").join("state").join("delores").join("spark-link").join("run"),
    }
}
pub fn unix_agent_data_dir(home: &Path) -> PathBuf {
    home.join(".local").join("state").join("delores").join("spark-link-agent")
}
pub fn unix_agent_install_dir(home: &Path) -> PathBuf {
    home.join(".local").join("share").join("delores").join("spark-link-agent")
}

/// Default run dir for this OS user from the environment. Clients use this
/// unless given an explicit endpoint file (tests).
pub fn default_run_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|v| windows_run_dir(Path::new(&v)))
    } else {
        let home = std::env::var_os("HOME")?;
        let xdg = std::env::var_os("XDG_RUNTIME_DIR");
        Some(unix_run_dir(xdg.as_deref().map(Path::new), Path::new(&home)))
    }
}

pub fn default_endpoint_file() -> Option<PathBuf> {
    default_run_dir().map(|d| d.join(ENDPOINT_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        let la = Path::new(r"C:\Users\u\AppData\Local");
        assert!(windows_run_dir(la).ends_with(Path::new("DeloresAI").join("SparkLink").join("run")));
        assert_eq!(pipe_name("S-1-5-21-1", "00ff"), r"\\.\pipe\delores-spark-link-S-1-5-21-1-00ff");
        let home = Path::new("home");
        // A relative XDG_RUNTIME_DIR is ignored.
        assert_eq!(unix_run_dir(Some(Path::new("relative")), home), unix_run_dir(None, home));
        assert!(unix_run_dir(None, home).ends_with(Path::new(".local").join("state").join("delores").join("spark-link").join("run")));
    }

    #[cfg(unix)]
    #[test]
    fn unix_layout() {
        let home = Path::new("/home/u");
        assert_eq!(unix_run_dir(Some(Path::new("/run/user/1000")), home), Path::new("/run/user/1000/delores-spark-link"));
        assert_eq!(unix_run_dir(None, home), Path::new("/home/u/.local/state/delores/spark-link/run"));
    }
}
