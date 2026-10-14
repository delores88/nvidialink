#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#[cfg(windows)]
mod windows_taskbar;
mod updates;
mod update_verify;
mod update_helper;
mod setup_bundle;

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tauri::Manager;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
};

#[derive(Clone)]
struct PairState {
    last: Arc<Mutex<Value>>,
    child: Arc<tokio::sync::Mutex<Option<Child>>>,
    running: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    lifecycle: Arc<tokio::sync::Mutex<()>>,
    device: Arc<Mutex<Option<String>>>,
}
impl Default for PairState {
    fn default() -> Self {
        Self {
            last: Arc::new(Mutex::new(json!({"ok":true,"state":"idle"}))),
            child: Arc::new(tokio::sync::Mutex::new(None)),
            running: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicU64::new(0)),
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
            device: Arc::new(Mutex::new(None)),
        }
    }
}

async fn cancel_ticket(state: &PairState) {
    let device = state
        .device
        .lock()
        .ok()
        .and_then(|mut device| device.take());
    if let Some(device) = device {
        let _ = tauri::async_runtime::spawn_blocking(move || {
            if let Ok(mut tr) = sparkdev::transport::Transport::new() {
                let _ = tr.ssh_raw(
                    &format!("~/sparkdev/bin/sparkdev-link owner-cancel --device-id {device}"),
                    &[],
                    Duration::from_secs(15),
                );
            }
        })
        .await;
    }
}

fn binary(app: &tauri::AppHandle, name: &str) -> Result<PathBuf, String> {
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.into()
    };
    let resources = app
        .path()
        .resource_dir()
        .map_err(|_| "Application resource directory unavailable")?;
    let packaged = resources.join("bin").join(&file);
    if packaged.is_file() {
        return Ok(packaged);
    }
    let adjacent = std::env::current_exe()
        .map_err(|_| "Application executable unavailable")?
        .parent()
        .ok_or("Missing application directory")?
        .join(&file);
    if adjacent.is_file() {
        return Ok(adjacent);
    }
    Err(format!("Bundled {name} is missing. Reinstall DGXBridge."))
}

async fn launch(
    app: tauri::AppHandle,
    name: &str,
    args: Vec<String>,
    input: Option<Value>,
    seconds: u64,
) -> Result<Value, String> {
    let mut command = Command::new(binary(&app, name)?);
    command
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = command
        .spawn()
        .map_err(|_| format!("Could not start {name}"))?;
    if let Some(value) = input {
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(value.to_string().as_bytes())
                .await
                .map_err(|_| "Could not send request")?;
            stdin
                .shutdown()
                .await
                .map_err(|_| "Could not finish request")?;
        }
    }
    let output = tokio::time::timeout(Duration::from_secs(seconds), child.wait_with_output())
        .await
        .map_err(|_| "The request timed out. Check connection status before retrying.")?
        .map_err(|_| "Could not read the response")?;
    serde_json::from_slice(&output.stdout)
        .or_else(|_| {
            // Some streaming CLI operations emit one JSON record per line.
            output
                .stdout
                .split(|b| *b == b'\n')
                .rev()
                .find_map(|line| serde_json::from_slice::<Value>(line).ok())
                .ok_or(serde_json::from_str::<Value>("").unwrap_err())
        })
        .map_err(|_| {
            format!(
                "{name} returned no structured response (exit {}).",
                output.status.code().unwrap_or(-1)
            )
        })
}

#[tauri::command]
async fn cli(app: tauri::AppHandle, args: Vec<String>) -> Result<Value, String> {
    if args.first().map(String::as_str) == Some("setup")
        && app.state::<PairState>().running.load(Ordering::SeqCst)
    {
        return Err(
            "Finish or cancel the current pairing before changing the SSH connection.".into(),
        );
    }
    let allowed = ["setup", "status", "doctor", "devices", "logs", "cancel", "wait", "connect", "disconnect"];
    if args.is_empty()
        || !allowed.contains(&args[0].as_str())
        || args.len() > 24
        || args.iter().any(|s| s.contains('\0'))
    {
        return Err("Unsupported desktop operation".into());
    }
    let mut full = vec!["--json".into(), "--client".into(), "human".into()];
    full.extend(args);
    launch(app, "sparkdev", full, None, 40).await
}

#[tauri::command]
async fn link(app: tauri::AppHandle, op: String) -> Result<Value, String> {
    let state = app.state::<PairState>().inner().clone();
    match op.as_str() {
        "status" => launch(app, "sparkdev-link", vec!["pairing-info".into()], None, 15).await,
        "pair-status" => Ok(state
            .last
            .lock()
            .map_err(|_| "Pairing state unavailable")?
            .clone()),
        "pair-cancel" => {
            // Serialize ticket creation and cancellation: a cancelled initializer
            // must not mint a ticket after this cleanup, nor revoke a newer one.
            let _lifecycle = state.lifecycle.lock().await;
            state.generation.fetch_add(1, Ordering::SeqCst);
            state.running.store(false, Ordering::SeqCst);
            if let Some(mut child) = state.child.lock().await.take() {
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
            cancel_ticket(&state).await;
            *state.last.lock().map_err(|_| "Pairing state unavailable")? =
                json!({"ok":true,"state":"cancelled"});
            Ok(json!({"ok":true,"state":"cancelled"}))
        }
        "pair-start" => {
            let config = sparkdev::state::config().map_err(|e| e.message)?;
            if !config.connection_enabled || !config.allows_lan() {
                return Err("Enable the connection and choose LAN only or Both before pairing over SSH.".into());
            }
            let _lifecycle = state.lifecycle.lock().await;
            if updates::action(&app, "status").await?["busy"] == true {
                return Err("Finish the current update operation before starting phone pairing.".into());
            }
            if state.running.swap(true, Ordering::SeqCst) {
                return Ok(state
                    .last
                    .lock()
                    .map_err(|_| "Pairing state unavailable")?
                    .clone());
            }
            *state.last.lock().map_err(|_| "Pairing state unavailable")? =
                json!({"ok":true,"state":"starting"});
            let generation = state.generation.fetch_add(1, Ordering::SeqCst) + 1;
            let session_state = state.clone();
            tauri::async_runtime::spawn(async move {
                let state = session_state;
                let result = pair_session(app, state.clone(), generation).await;
                let _lifecycle = state.lifecycle.lock().await;
                if state.generation.load(Ordering::SeqCst) == generation {
                    if let Err(message) = result {
                        if let Some(mut child) = state.child.lock().await.take() {
                            let _ = child.kill().await;
                            let _ = child.wait().await;
                        }
                        cancel_ticket(&state).await;
                        if state.running.load(Ordering::SeqCst) {
                            if let Ok(mut last) = state.last.lock() {
                                *last = json!({"ok":false,"state":"failed","error":{"message":message}});
                            }
                        }
                    }
                    state.running.store(false, Ordering::SeqCst);
                }
            });
            Ok(json!({"ok":true,"state":"starting"}))
        }
        _ => Err("Unsupported link operation".into()),
    }
}

async fn pair_session(
    app: tauri::AppHandle,
    state: PairState,
    generation: u64,
) -> Result<(), String> {
    let lifecycle = state.lifecycle.lock().await;
    if !state.running.load(Ordering::SeqCst)
        || state.generation.load(Ordering::SeqCst) != generation
    {
        return Ok(());
    }
    let id = launch(
        app.clone(),
        "sparkdev-link",
        vec!["identity".into()],
        None,
        15,
    )
    .await?;
    let device = id["device_id"]
        .as_str()
        .filter(|s| s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit()))
        .ok_or("Link identity was invalid")?
        .to_owned();
    *state
        .device
        .lock()
        .map_err(|_| "Pairing identity unavailable")? = Some(device.clone());
    let command = format!(
        "~/sparkdev/bin/sparkdev-link owner-ticket --device-id {device} --device-name 'Windows laptop'"
    );
    let ticket = tauri::async_runtime::spawn_blocking(move || {
        let mut tr = sparkdev::transport::Transport::new().map_err(|e| e.message)?;
        let bytes = tr
            .ssh_raw(&command, &[], Duration::from_secs(20))
            .map_err(|e| e.message)?;
        serde_json::from_slice::<Value>(&bytes)
            .map_err(|_| "The link service returned an invalid ticket response".to_owned())
    })
    .await
    .map_err(|_| "Could not prepare pairing")??;
    if !state.running.load(Ordering::SeqCst)
        || state.generation.load(Ordering::SeqCst) != generation
    {
        return Ok(());
    }
    if ticket["ok"] != true {
        return Err(
            "Spark link service could not create a pairing ticket. Check its installation.".into(),
        );
    }
    let mut command = Command::new(binary(&app, "sparkdev-link")?);
    command
        .arg("pair")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = command.spawn().map_err(|_| "Could not start pairing")?;
    let mut input = child.stdin.take().ok_or("Pairing input unavailable")?;
    input
        .write_all(ticket.to_string().as_bytes())
        .await
        .map_err(|_| "Could not pass the pairing ticket")?;
    input
        .shutdown()
        .await
        .map_err(|_| "Could not close pairing input")?;
    drop(input);
    let stdout = child.stdout.take().ok_or("Pairing output unavailable")?;
    {
        let mut slot = state.child.lock().await;
        if state.generation.load(Ordering::SeqCst) != generation {
            let _ = child.kill().await;
            return Ok(());
        }
        *slot = Some(child);
    }
    drop(lifecycle);
    let mut lines = BufReader::new(stdout).lines();
    let mut terminal = false;
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|_| "Pairing connection ended")?
    {
        if !state.running.load(Ordering::SeqCst)
            || state.generation.load(Ordering::SeqCst) != generation
        {
            break;
        }
        let event: Value = serde_json::from_str(&line).map_err(|_| "Invalid pairing response")?;
        if event["state"] == "paired" {
            terminal = true;
            *state
                .device
                .lock()
                .map_err(|_| "Pairing identity unavailable")? = None;
        } else if event["ok"] == false {
            return Err("Pairing was not approved or the link connection ended. Create a new code to try again.".into());
        }
        *state.last.lock().map_err(|_| "Pairing state unavailable")? = event;
    }
    if state.generation.load(Ordering::SeqCst) == generation {
        if let Some(mut child) = state.child.lock().await.take() {
            let _ = child.wait().await;
        }
    }
    if !terminal && state.generation.load(Ordering::SeqCst) == generation {
        return Err(
            "The pairing process ended before approval. Create a new code to try again.".into(),
        );
    }
    Ok(())
}

#[tauri::command]
fn preferences(app: tauri::AppHandle) -> Value {
    let config = sparkdev::state::config();
    let cli = binary(&app, "sparkdev").ok();
    let root = sparkdev::state::root().ok();
    // Packaged agent processes can redirect AppData file I/O even when a
    // known-folder API returns the ordinary path. Offer a loopback path only
    // when this owner can read it and its bytes match this installed instance.
    let agent_cli = cli.as_ref().and_then(|p| verified_loopback_path(p, None))
        .or_else(|| cli.clone());
    let agent_root = root.as_ref().and_then(|p| verified_loopback_path(p, Some("config.json")))
        .or_else(|| root.clone());
    let ordinary_cli = cli.as_ref().and_then(|p| verified_ordinary_path(p, None))
        .or_else(|| cli.clone());
    let ordinary_root = root.as_ref().and_then(|p| verified_ordinary_path(p, Some("config.json")))
        .or_else(|| root.clone());
    let connection = config.as_ref().ok().map(|c| c.connection_info());
    let laptop = config.as_ref().ok().map(|c| c.laptop_info());
    let error = config.as_ref().err().map(|e| json!({"message":e.message}));
    json!({"ok":config.is_ok(),"config":config.ok(),"cli_path":ordinary_cli,"state_root":ordinary_root,
        "agent_cli_path":agent_cli,"agent_state_root":agent_root,
        "agent_paths":{"claude":{"cli_path":ordinary_cli,"state_root":ordinary_root},
            "codex":{"cli_path":agent_cli,"state_root":agent_root}},
        "connection":connection,"laptop":laptop,"state":sparkdev::state::diagnostics(),
        "app_version":env!("CARGO_PKG_VERSION"),"error":error})
}

fn verified_ordinary_path(path: &std::path::Path, child: Option<&str>) -> Option<PathBuf> {
    if !cfg!(windows) { return Some(path.to_owned()); }
    verified_path_bytes(path, ordinary_candidate(path)?, child)
}

fn ordinary_candidate(path: &std::path::Path) -> Option<PathBuf> {
    let candidate = loopback_candidate(path)?;
    let text = candidate.to_str()?;
    if let Some(rest) = text.strip_prefix(r"\\localhost\") {
        let (share, tail) = rest.split_once('\\')?;
        let bytes = share.as_bytes();
        if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b'$' {
            return Some(PathBuf::from(format!("{}:\\{}", bytes[0] as char, tail)));
        }
    }
    Some(candidate)
}

fn verified_loopback_path(path: &std::path::Path, child: Option<&str>) -> Option<PathBuf> {
    if !cfg!(windows) { return None; }
    let candidate = loopback_candidate(path)?;
    verified_path_bytes(path, candidate, child)
}

fn loopback_candidate(path: &std::path::Path) -> Option<PathBuf> {
    let text = path.to_str()?;
    // Tauri/Windows can return verbatim paths. Strip only the known drive/UNC
    // wrappers when deriving an alias; never turn another device namespace
    // into a local drive. The alias is offered only after the byte check below.
    let normalized;
    let text = if let Some(rest) = text.strip_prefix(r"\\?\") {
        if rest.get(..4).is_some_and(|prefix| prefix.eq_ignore_ascii_case("UNC\\")) {
            normalized = format!(r"\\{}", &rest[4..]);
            normalized.as_str()
        } else {
            rest
        }
    } else {
        text
    };
    if let Some(rest) = text.strip_prefix(r"\\") {
        let mut parts = rest.split('\\');
        let host = parts.next()?;
        let share = parts.next()?;
        if host.is_empty() || matches!(host, "?" | ".") || share.is_empty() {
            return None;
        }
        return Some(PathBuf::from(text));
    }
    let bytes = text.as_bytes();
    if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':'
        || !matches!(bytes[2], b'\\' | b'/') { return None; }
    Some(PathBuf::from(format!("\\\\localhost\\{}$\\{}", bytes[0] as char,
        text[3..].replace('/', "\\"))))
}

fn verified_path_bytes(path: &std::path::Path, candidate: PathBuf, child: Option<&str>) -> Option<PathBuf> {
    let (original_file, candidate_file) = if let Some(name) = child {
        (path.join(name), candidate.join(name))
    } else { (path.to_owned(), candidate.clone()) };
    let source = std::fs::read(original_file).ok()?;
    let physical = std::fs::read(candidate_file).ok()?;
    if source == physical { Some(candidate) } else { None }
}

#[cfg(test)]
mod agent_path_tests {
    use super::{loopback_candidate, ordinary_candidate, verified_path_bytes};
    use std::path::{Path, PathBuf};

    #[test]
    fn verbatim_drive_and_unc_paths_have_native_agent_candidates() {
        let expected = PathBuf::from(r"\\localhost\C$\Users\Example\AppData\Local\DGXBridge\bin\sparkdev.exe");
        for source in [
            r"C:\Users\Example\AppData\Local\DGXBridge\bin\sparkdev.exe",
            r"\\?\C:\Users\Example\AppData\Local\DGXBridge\bin\sparkdev.exe",
            "C:/Users/Example/AppData/Local/DGXBridge/bin/sparkdev.exe",
            r"\\?\UNC\localhost\C$\Users\Example\AppData\Local\DGXBridge\bin\sparkdev.exe",
        ] {
            assert_eq!(loopback_candidate(Path::new(source)), Some(expected.clone()));
        }
        assert_eq!(loopback_candidate(Path::new(r"\\?\UNC\server\share\Tools with spaces\sparkdev.exe")),
            Some(PathBuf::from(r"\\server\share\Tools with spaces\sparkdev.exe")));
        assert_eq!(loopback_candidate(Path::new(r"\\server\share\sparkdev.exe")),
            Some(PathBuf::from(r"\\server\share\sparkdev.exe")));
    }

    #[test]
    fn agent_candidates_reject_relative_and_device_namespaces() {
        for source in [r"C:sparkdev.exe", r"bin\sparkdev.exe", r"\\.\pipe\sparkdev",
            r"\\?\Volume{example}\sparkdev.exe", r"\\?\GLOBALROOT\Device\sparkdev", r"\\server"] {
            assert!(loopback_candidate(Path::new(source)).is_none(), "{source}");
        }
    }

    #[test]
    fn ordinary_agent_paths_remove_only_known_windows_wrappers() {
        let expected = PathBuf::from(r"C:\Users\Example\DGXBridge\bin\sparkdev.exe");
        for source in [r"C:\Users\Example\DGXBridge\bin\sparkdev.exe",
            r"\\?\C:\Users\Example\DGXBridge\bin\sparkdev.exe",
            r"\\localhost\C$\Users\Example\DGXBridge\bin\sparkdev.exe"] {
            assert_eq!(ordinary_candidate(Path::new(source)), Some(expected.clone()));
        }
        assert_eq!(ordinary_candidate(Path::new(r"\\?\UNC\server\share\sparkdev.exe")),
            Some(PathBuf::from(r"\\server\share\sparkdev.exe")));
        assert!(ordinary_candidate(Path::new(r"\\.\pipe\sparkdev")).is_none());
        assert!(ordinary_candidate(Path::new(r"C:sparkdev.exe")).is_none());
    }

    #[test]
    fn agent_alias_requires_readable_identical_physical_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original");
        let physical = dir.path().join("physical");
        std::fs::create_dir(&original).unwrap();
        std::fs::create_dir(&physical).unwrap();
        std::fs::write(original.join("config.json"), b"same owner state").unwrap();
        assert!(verified_path_bytes(&original, physical.clone(), Some("config.json")).is_none());
        std::fs::write(physical.join("config.json"), b"other owner state").unwrap();
        assert!(verified_path_bytes(&original, physical.clone(), Some("config.json")).is_none());
        std::fs::write(physical.join("config.json"), b"same owner state").unwrap();
        assert_eq!(verified_path_bytes(&original, physical.clone(), Some("config.json")), Some(physical.clone()));
        assert_eq!(verified_path_bytes(&original.join("config.json"), physical.join("config.json"), None),
            Some(physical.join("config.json")));
    }
}

#[tauri::command]
async fn connection(app: tauri::AppHandle, mode: Option<String>, enabled: Option<bool>, laptop_jobs: Option<usize>) -> Result<Value, String> {
    if (mode.is_some() || enabled.is_some()) && app.state::<PairState>().running.load(Ordering::SeqCst) {
        return Err("Finish or cancel phone pairing before changing the connection.".into());
    }
    let mut args = vec!["--json".into(), "--client".into(), "human".into(), "connection".into()];
    if let Some(mode) = mode { args.extend(["--mode".into(), mode]); }
    if let Some(jobs) = laptop_jobs { args.extend(["--laptop-jobs".into(), jobs.to_string()]); }
    let saved = launch(app.clone(), "sparkdev", args, None, 15).await?;
    if saved["ok"] != true { return Ok(saved); }
    if let Some(enabled) = enabled {
        return cli(app, vec![if enabled { "connect" } else { "disconnect" }.into()]).await;
    }
    Ok(saved)
}

#[tauri::command]
async fn resources(app: tauri::AppHandle, policy: Option<Value>) -> Result<Value, String> {
    let mut args = vec!["--json".into(), "--client".into(), "human".into(), "resources".into()];
    if policy.is_some() { args.push("--stdin".into()); }
    launch(app, "sparkdev", args, policy, 40).await
}

#[tauri::command]
async fn update_action(app: tauri::AppHandle, window: tauri::Window, action: String) -> Result<Value, String> {
    if window.label() != "main" { return Err("Updates are available only from the main DGXBridge window.".into()); }
    if action == "install" {
        let state = app.state::<PairState>().inner().clone();
        let _lifecycle = state.lifecycle.lock().await;
        if state.running.load(Ordering::SeqCst) {
            return Err("Finish or cancel phone pairing before installing an update.".into());
        }
        // The updater sets busy before returning. Pair-start checks it under
        // the same lifecycle lock, closing the check/start race during staging.
        return updates::action(&app, &action).await;
    }
    updates::action(&app, &action).await
}

#[tauri::command]
async fn bootstrap(app: tauri::AppHandle, remote_access: bool) -> Result<Value, String> {
    let state = app.state::<PairState>().inner().clone();
    let _lifecycle = state.lifecycle.lock().await;
    if state.running.load(Ordering::SeqCst) {
        return Err(
            "Finish or cancel the current pairing before installing or changing the connection."
                .into(),
        );
    }
    if updates::action(&app, "status").await?["busy"] == true {
        return Err("Finish the current update operation before installing Spark services.".into());
    }
    let resources = app
        .path()
        .resource_dir()
        .map_err(|_| "Application resources unavailable")?;
    let mut bundle = setup_bundle::load(&resources)?;
    let script = STANDARD.decode(bundle["files"]["bootstrap.py"]["data"].as_str()
        .ok_or("The validated remote installer is missing.")?)
        .map_err(|_| "The validated remote installer is invalid.")?;
    bundle["relay_mode"] = json!(if remote_access {
        "n0_public"
    } else {
        "disabled"
    });
    let request = serde_json::to_vec(&bundle).map_err(|_| "The setup request is invalid")?;
    // Base64 contains no shell metacharacters; source is a packaged first-party file.
    let remote = format!(
        "python3 -c \"import base64;exec(base64.b64decode('{}'))\"",
        STANDARD.encode(script)
    );
    tauri::async_runtime::spawn_blocking(move || {
        let mut tr = sparkdev::transport::Transport::new().map_err(|e| e.message)?;
        let bytes = tr
            .ssh_raw(&remote, &request, Duration::from_secs(90))
            .map_err(|e| e.message)?;
        serde_json::from_slice::<Value>(&bytes)
            .map_err(|_| "The Spark installer returned no valid receipt".into())
    })
    .await
    .map_err(|_| "Remote setup could not finish".to_owned())?
}

fn main() {
    if update_helper::run_if_requested() { return; }
    let context = tauri::generate_context!();
    #[cfg(windows)]
    windows_taskbar::set_app_id(&context.config().identifier)
        .expect("DGXBridge could not initialize its Windows taskbar identity");

    tauri::Builder::default()
        .manage(PairState::default())
        .manage(updates::UpdateState::default())
        .setup(|app| {
            update_helper::confirm_launch();
            #[cfg(windows)]
            {
                let window = app
                    .get_webview_window("main")
                    .ok_or_else(|| std::io::Error::other("DGXBridge main window is unavailable"))?;
                windows_taskbar::set_taskbar_icon(&window)?;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![cli, link, preferences, bootstrap, connection, resources, update_action])
        .run(context)
        .expect("DGXBridge could not start");
}
