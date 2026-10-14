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
    pub authenticated: bool,
    attempts: Vec<Value>,
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
        Ok(Self {
            config: state::config()?,
            selected: "unselected".into(),
            reason: "No authenticated request yet".into(),
            host: None,
            authenticated: false,
            attempts: Vec::new(),
        })
    }
    fn record(
        &mut self,
        route: &str,
        attempted: bool,
        status: &str,
        reason: &str,
        target: Option<usize>,
    ) {
        if self.attempts.len() >= 32 {
            self.attempts.remove(0);
        }
        self.attempts.push(json!({"route":route,"attempted":attempted,"status":status,"reason":reason,"target_index":target}));
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
        self.authenticated = false;
        let hosts: Vec<Option<String>> = if let Some(host) = &self.host {
            vec![Some(host.clone())]
        } else if self.config.hosts.is_empty() {
            vec![None]
        } else {
            self.config.hosts.iter().cloned().map(Some).collect()
        };
        for (index, host) in hosts.into_iter().enumerate() {
            let result = process(self.command(host.as_deref(), remote), input, timeout);
            let (success, out, err) = match result {
                Ok(r) => r,
                Err(e) => {
                    self.record(
                        "lan_ssh",
                        true,
                        if e.message.starts_with("Transport timeout") {
                            "transport_timeout"
                        } else {
                            "process_error"
                        },
                        &e.message,
                        Some(index),
                    );
                    return Err(e);
                }
            };
            if success {
                self.selected = "ssh".into();
                self.reason = "LAN SSH authenticated with the configured known-host pin".into();
                self.host = host;
                self.authenticated = true;
                self.record(
                    "lan_ssh",
                    true,
                    "authenticated",
                    "Strict SSH host pin and key authentication succeeded",
                    Some(index),
                );
                return Ok(out);
            }
            let message = String::from_utf8_lossy(&err).to_lowercase();
            let failure = ssh_failure(&message);
            self.record("lan_ssh", true, failure, match failure {"authentication_failed"=>"SSH authentication or host pin rejected; no fallback", "host_unreachable"=>"SSH reported connection refusal, timeout, unreachable network or DNS failure", _=>"SSH command failed without evidence that the LAN host is unreachable"}, Some(index));
            if failure == "authentication_failed" {
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
        let input = serde_json::to_vec(&request)?;
        let timeout = Duration::from_secs(request["timeout"].as_u64().unwrap_or(0).min(60) + 30);
        if self.selected != "iroh" {
            match self.ssh_raw("python3 ~/sparkdev/controller.py rpc", &input, timeout) {
                Ok(bytes) => return self.decode(&bytes),
                Err(e) => {
                    if e.message == "Configured LAN addresses are unreachable"
                        && !self.config.allow_remote
                    {
                        self.record(
                            "paired_iroh",
                            false,
                            "disabled",
                            "Remote transport is disabled in local configuration",
                            None,
                        );
                    }
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
            self.reason = "LAN unreachable; paired endpoint transport explicitly enabled".into();
            self.authenticated = true;
            self.record("paired_iroh", true, "authenticated", "Paired endpoint authenticated; adapter does not report whether its network path is direct or relayed", None);
            return self.decode(&out);
        }
        Err(AppError::new(4, "Authenticated remote link is busy"))
    }
    pub fn info(&self) -> Value {
        json!({"selected":self.selected,"route":if !self.authenticated {"none"} else if self.selected == "ssh" {"lan_ssh"} else {"paired_iroh"},"authenticated":self.authenticated,"reason":self.reason,"attempted_routes":self.attempts})
    }
}
#[cfg(test)]
mod tests {
    use super::*;
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
}
