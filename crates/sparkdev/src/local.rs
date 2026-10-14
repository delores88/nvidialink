//! Laptop jobs run as native owner processes. They are not Docker-sandboxed.
//! Admission and the worktree lease are shared with the Spark scheduler.
use crate::{
    AppError, Result,
    inventory::{self, Inventory},
    state, terminal, transport,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
fn dir(job: &str) -> Result<PathBuf> {
    state::validate_id(job)?;
    Ok(state::root()?.join("jobs").join(job))
}
fn read(job: &str) -> Result<Value> {
    state::read_json(&dir(job)?.join("job.json"))
}
fn update(job: &str, value: &Value) -> Result<()> {
    state::atomic_json(&dir(job)?.join("job.json"), value)
}
fn lock() -> Result<fs::File> {
    let root = state::root()?;
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(root.join("laptop-admission.lock"))?;
    fs2::FileExt::lock_exclusive(&file)?;
    Ok(file)
}
pub fn submit(
    inv: &Inventory,
    argv: &[String],
    client: &str,
    request_id: &str,
    reason: &str,
) -> Result<Value> {
    let _guard = lock()?;
    let jobs = status()?;
    for job in jobs.as_array().unwrap() {
        if job["request_id"] == request_id {
            return Ok(job.clone());
        }
        if !terminal(job) && job["project"] == inv.project {
            return Err(AppError::new(
                8,
                "This worktree already has an active laptop job",
            ));
        }
    }
    let running = jobs
        .as_array()
        .unwrap()
        .iter()
        .filter(|j| !terminal(j))
        .count();
    if running >= 2 {
        return Err(AppError::new(
            8,
            "Laptop concurrency limit reached (2); retry after a job completes",
        ));
    }
    let config = state::config()?;
    if is_cargo(argv) {
        let gate = config.local_build_gate.as_ref().ok_or_else(|| {
            AppError::new(
                3,
                "Configure --local-build-gate before running local Cargo jobs",
            )
        })?;
        if !gate.is_file() {
            return Err(AppError::new(
                3,
                "Configured local Cargo build gate does not exist",
            ));
        }
    }
    let id = format!(
        "laptop-{}",
        &inventory::hash(format!("{}:{request_id}", inv.project).as_bytes())[..32]
    );
    let mut tr = transport::Transport::new()?;
    flush_pending(&mut tr);
    let claim=tr.rpc(json!({"v":1,"op":"claim","project":inv.project,"client":client,"request_id":request_id,"job":id,"ttl":60}))?;
    let lease = claim["lease"]
        .as_str()
        .ok_or_else(|| AppError::new(6, "Scheduler omitted laptop lease"))?;
    let value = json!({"ok":true,"job":id,"state":"queued","placement":"laptop","placement_reason":reason,"project":inv.project,"manifest_id":inv.manifest_id,"root":inv.root,"argv":argv,"client":client,"request_id":request_id,"lease":lease,"created_at":state::now(),"exit_code":null,"cache_hit":false,"cache_reason":"Native toolchain identity is not locked; laptop result reuse is disabled"});
    update(&id, &value)?;
    if spawn_worker(&id).is_err() {
        let _ = tr.rpc(json!({"v":1,"op":"release","lease":lease}));
        let mut failed = value;
        failed["state"] = json!("failed");
        failed["exit_code"] = json!(125);
        update(&id, &failed)?;
        return Err(AppError::new(3, "Laptop worker could not be started"));
    }
    public(value)
}
#[cfg(windows)]
fn spawn_worker(job: &str) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{
            CREATE_NEW_PROCESS_GROUP, CreateProcessW, DETACHED_PROCESS, PROCESS_INFORMATION,
            STARTUPINFOW,
        },
    };
    state::validate_id(job)?;
    let exe = std::env::current_exe()?;
    let application: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut line = std::ffi::OsString::from("\"");
    line.push(&exe);
    line.push("\" __worker ");
    line.push(job);
    let mut line: Vec<u16> = line.encode_wide().chain(Some(0)).collect();
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // False handle inheritance is essential: leaked CLI capture pipes make a
    // caller's communicate() wait for a supposedly detached workload to finish.
    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP,
            std::ptr::null(),
            std::ptr::null(),
            &startup,
            &mut process,
        )
    };
    if created == 0 {
        return Err(AppError::new(
            3,
            "Detached laptop worker could not be created",
        ));
    }
    unsafe {
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
    }
    Ok(())
}
#[cfg(not(windows))]
fn spawn_worker(job: &str) -> Result<()> {
    Command::new(std::env::current_exe()?)
        .args(["__worker", job])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}
fn public(mut value: Value) -> Result<Value> {
    if let Some(map) = value.as_object_mut() {
        for key in ["lease", "root", "argv"] {
            map.remove(key);
        }
    }
    Ok(value)
}
pub fn status() -> Result<Value> {
    let root = state::root()?.join("jobs");
    let mut jobs = Vec::new();
    if root.exists() {
        for entry in fs::read_dir(root)?.flatten() {
            if let Ok(job) = state::read_json(&entry.path().join("job.json")) {
                jobs.push(public(job)?);
            }
        }
    }
    jobs.sort_by_key(|v| v["created_at"].as_u64().unwrap_or(0));
    Ok(json!(jobs))
}
pub fn wait(job: &str, timeout: u64) -> Result<Value> {
    let start = Instant::now();
    loop {
        let value = read(job)?;
        if terminal(&value) || start.elapsed() >= Duration::from_secs(timeout.min(60)) {
            return public(value);
        }
        thread::sleep(Duration::from_millis(100));
    }
}
pub fn logs(job: &str, since: u64, limit: usize) -> Result<Value> {
    let value = read(job)?;
    let path = dir(job)?.join("output.log");
    let mut bytes = Vec::new();
    if path.exists() {
        let mut file = fs::File::open(path)?;
        if since > file.metadata()?.len() {
            return Err(AppError::new(2, "Log offset is beyond available data"));
        }
        file.seek(SeekFrom::Start(since))?;
        file.take(limit.min(inventory::CHUNK) as u64)
            .read_to_end(&mut bytes)?;
    }
    Ok(
        json!({"ok":true,"job":job,"data":STANDARD.encode(&bytes),"text":String::from_utf8_lossy(&bytes),"since":since,"next_offset":since+bytes.len() as u64,"state":value["state"],"exit_code":value["exit_code"]}),
    )
}
pub fn cancel(job: &str) -> Result<Value> {
    let value = read(job)?;
    if terminal(&value) {
        return public(value);
    }
    state::atomic_bytes(&dir(job)?.join("cancel"), b"cancel")?;
    Ok(json!({"ok":true,"job":job,"state":"cancel_pending"}))
}
fn is_cargo(argv: &[String]) -> bool {
    std::path::Path::new(&argv[0])
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("cargo") || s.eq_ignore_ascii_case("cargo.exe"))
}
fn terminate(child: &mut std::process::Child) {
    #[cfg(windows)]
    {
        let mut command = Command::new("taskkill");
        command.args(["/PID", &child.id().to_string(), "/T", "/F"]);
        transport::quiet(&mut command);
        let _ = command.stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    let _ = child.kill();
    let _ = child.wait();
}
pub fn worker(job: &str) -> Result<()> {
    let result = worker_inner(job);
    if result.is_err() {
        if let Ok(mut value) = read(job) {
            if !terminal(&value) {
                value["state"] = json!("failed");
                value["exit_code"] = json!(125);
                value["finished_at"] = json!(state::now());
                let _ = update(job, &value);
                if let Ok(mut tr) = transport::Transport::new() {
                    let _=tr.rpc(json!({"v":1,"op":"heartbeat","lease":value["lease"],"state":"failed","exit_code":125}));
                }
            }
        }
    }
    result
}
fn worker_inner(job: &str) -> Result<()> {
    let mut value = read(job)?;
    if value["state"] != "queued" {
        return Ok(());
    }
    let config = state::config()?;
    let root = PathBuf::from(
        value["root"]
            .as_str()
            .ok_or_else(|| AppError::new(3, "Missing laptop worktree"))?,
    );
    let argv: Vec<String> = serde_json::from_value(value["argv"].clone())?;
    let lease = value["lease"]
        .as_str()
        .ok_or_else(|| AppError::new(3, "Missing laptop lease"))?
        .to_string();
    if inventory::collect(&root)?.manifest_id != value["manifest_id"] {
        return Err(AppError::new(
            5,
            "Laptop source changed after submission; refusing stale job",
        ));
    }
    let mut tr = transport::Transport::new()?;
    tr.rpc(json!({"v":1,"op":"heartbeat","lease":lease,"state":"running","ttl":60}))?;
    let mut command = if is_cargo(&argv) {
        let mut command = Command::new("python");
        command
            .arg("-I")
            .arg(
                config
                    .local_build_gate
                    .as_ref()
                    .ok_or_else(|| AppError::new(3, "Local Cargo requires build gate"))?,
            )
            .arg("--")
            .args(&argv);
        command
    } else {
        let mut command = Command::new(&argv[0]);
        command.args(&argv[1..]);
        command
    };
    command
        .current_dir(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Keep outputs outside source, and never let a local build consume constrained D: storage.
    if is_cargo(&argv) {
        let target = state::root()?
            .join("targets")
            .join(value["project"].as_str().unwrap());
        fs::create_dir_all(&target)?;
        command.env("CARGO_TARGET_DIR", target);
    }
    transport::quiet(&mut command);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000 | 0x00004000);
    }
    let output = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir(job)?.join("output.log"))?;
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            value["state"] = json!("failed");
            value["exit_code"] = json!(127);
            update(job, &value)?;
            let _ = tr.rpc(
                json!({"v":1,"op":"heartbeat","lease":lease,"state":"failed","exit_code":127}),
            );
            return Ok(());
        }
    };
    value["state"] = json!("running");
    value["pid"] = json!(child.id());
    value["started_at"] = json!(state::now());
    if let Err(error) = update(job, &value) {
        terminate(&mut child);
        return Err(error);
    }
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let shared = std::sync::Arc::new(std::sync::Mutex::new(output));
    let pump = |mut input: Box<dyn Read + Send>,
                file: std::sync::Arc<std::sync::Mutex<fs::File>>| {
        thread::spawn(move || {
            let mut buffer = [0; 8192];
            loop {
                match input.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut file = file.lock().unwrap();
                        let _ = file.write_all(&buffer[..n]);
                        let _ = file.flush();
                    }
                }
            }
        })
    };
    let stdout = pump(Box::new(out), shared.clone());
    let stderr = pump(Box::new(err), shared);
    let mut last_heartbeat = Instant::now();
    let mut last_ok = Instant::now();
    let mut offset = 0u64;
    let (final_state, exit_code) = loop {
        if dir(job)?.join("cancel").exists() {
            terminate(&mut child);
            break ("cancelled", 130);
        }
        if let Some(status) = child.try_wait()? {
            break (
                if status.success() {
                    "succeeded"
                } else {
                    "failed"
                },
                status.code().unwrap_or(125),
            );
        }
        if last_heartbeat.elapsed() >= Duration::from_secs(15) {
            let chunk = logs(job, offset, inventory::CHUNK)?;
            let heartbeat = json!({"v":1,"op":"heartbeat","lease":lease,"state":"running","ttl":60,"since":offset,"data":chunk["data"]});
            if tr.rpc(heartbeat).is_ok() {
                last_ok = Instant::now();
                offset = chunk["next_offset"].as_u64().unwrap_or(offset);
            }
            last_heartbeat = Instant::now();
        }
        if last_ok.elapsed() >= Duration::from_secs(45) {
            terminate(&mut child);
            break ("failed", 125);
        }
        thread::sleep(Duration::from_millis(100));
    };
    let _ = stdout.join();
    let _ = stderr.join();
    value["state"] = json!(final_state);
    value["exit_code"] = json!(exit_code);
    value["finished_at"] = json!(state::now());
    update(job, &value)?;
    state::atomic_json(
        &dir(job)?.join("pending-heartbeat.json"),
        &json!({"lease":lease,"state":final_state,"exit_code":exit_code,"since":offset}),
    )?;
    let _ = flush_one(job, &mut tr, 1024);
    Ok(())
}

pub fn flush_pending(tr: &mut transport::Transport) {
    let Ok(root) = state::root() else { return };
    let Ok(jobs) = fs::read_dir(root.join("jobs")) else {
        return;
    };
    for entry in jobs.flatten() {
        if entry.path().join("pending-heartbeat.json").is_file() {
            if let Some(job) = entry.file_name().to_str() {
                if flush_one(job, tr, 4).is_err() {
                    break;
                }
            }
        }
    }
}
fn flush_one(job: &str, tr: &mut transport::Transport, budget: usize) -> Result<()> {
    let folder = dir(job)?;
    let file = folder.join("pending-heartbeat.json");
    if !file.exists() {
        return Ok(());
    }
    let lock = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(folder.join("heartbeat.lock"))?;
    if fs2::FileExt::try_lock_exclusive(&lock).is_err() {
        return Ok(());
    }
    if !file.exists() {
        return Ok(());
    }
    let mut pending = state::read_json(&file)?;
    for _ in 0..budget {
        let offset = pending["since"].as_u64().unwrap_or(0);
        let chunk = logs(job, offset, inventory::CHUNK)?;
        let next = chunk["next_offset"].as_u64().unwrap_or(offset);
        let mut request = json!({"v":1,"op":"heartbeat","lease":pending["lease"],"state":pending["state"],"exit_code":pending["exit_code"]});
        if next > offset {
            request["since"] = json!(offset);
            request["data"] = chunk["data"].clone();
        }
        tr.rpc(request)?;
        if next == offset {
            fs::remove_file(&file)?;
            break;
        }
        pending["since"] = json!(next);
        state::atomic_json(&file, &pending)?;
    }
    Ok(())
}
