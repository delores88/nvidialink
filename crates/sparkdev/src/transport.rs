use crate::{
    AppError, Result,
    state::{self, Config},
};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub struct Transport {
    pub config: Config,
    pub selected: String,
    pub reason: String,
    pub host: Option<String>,
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
        Ok(Self {
            config: state::config()?,
            selected: "unselected".into(),
            reason: "No authenticated request yet".into(),
            host: None,
        })
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
    fn decode(&self, bytes: &[u8]) -> Result<Value> {
        let value: Value = serde_json::from_slice(bytes)
            .map_err(|_| AppError::new(6, "Remote returned an invalid JSON response"))?;
        if value["ok"] == false {
            let code = value["code"].as_str().unwrap_or("REMOTE_ERROR");
            let message = value["message"]
                .as_str()
                .unwrap_or("Remote operation failed");
            return Err(AppError::new(6, format!("{code}: {message}")));
        }
        Ok(value)
    }
    pub fn ssh_raw(&mut self, remote: &str, input: &[u8], timeout: Duration) -> Result<Vec<u8>> {
        let hosts: Vec<Option<String>> = if let Some(host) = &self.host {
            vec![Some(host.clone())]
        } else if self.config.hosts.is_empty() {
            vec![None]
        } else {
            self.config.hosts.iter().cloned().map(Some).collect()
        };
        for host in hosts {
            let result = process(self.command(host.as_deref(), remote), input, timeout);
            let (success, out, err) = match result {
                Ok(r) => r,
                Err(e) => return Err(e),
            };
            if success {
                self.selected = "ssh".into();
                self.reason = "LAN SSH authenticated with the configured known-host pin".into();
                self.host = host;
                return Ok(out);
            }
            let message = String::from_utf8_lossy(&err).to_lowercase();
            if message.contains("host key verification failed")
                || message.contains("remote host identification has changed")
                || message.contains("permission denied")
                || message.contains("bad permissions")
                || message.contains("no matching host key")
            {
                return Err(AppError::new(
                    7,
                    "SSH authentication or pinned host-key verification failed; fallback refused",
                ));
            }
            if !(message.contains("connection timed out")
                || message.contains("connection refused")
                || message.contains("no route to host")
                || message.contains("network is unreachable")
                || message.contains("could not resolve hostname"))
            {
                return Err(AppError::new(
                    4,
                    "SSH command failed; inspect the configured service and key (stderr withheld)",
                ));
            }
        }
        Err(AppError::new(4, "Configured LAN addresses are unreachable"))
    }
    pub fn rpc(&mut self, request: Value) -> Result<Value> {
        let input = serde_json::to_vec(&request)?;
        let timeout = Duration::from_secs(request["timeout"].as_u64().unwrap_or(0).min(60) + 30);
        if self.selected != "iroh" {
            match self.ssh_raw("python3 ~/sparkdev/controller.py rpc", &input, timeout) {
                Ok(bytes) => return self.decode(&bytes),
                Err(e) => {
                    if e.code != 4
                        || e.message != "Configured LAN addresses are unreachable"
                        || !self.config.allow_remote
                    {
                        return Err(e);
                    }
                }
            }
        }
        let payload = serde_json::to_vec(&json!({"request":request}))?;
        for attempt in 0..3 {
            let mut command = Command::new(sidecar()?);
            command.arg("rpc");
            let (success, out, _) = process(command, &payload, timeout)?;
            if !success {
                let error = serde_json::from_slice::<Value>(&out).unwrap_or(Value::Null);
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
            self.reason = "LAN unreachable; paired endpoint transport explicitly enabled".into();
            return self.decode(&out);
        }
        Err(AppError::new(4, "Authenticated remote link is busy"))
    }
    pub fn info(&self) -> Value {
        json!({"selected":self.selected,"reason":self.reason})
    }
}
