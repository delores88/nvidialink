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
    let config = state::config()?;
    if running >= config.laptop_jobs {
        return Err(AppError::new(
            8,
            format!(
                "Laptop concurrency limit reached ({}); retry after a job completes",
                config.laptop_jobs
            ),
        ));
    }
    // A build gate is optional; when one is set, Cargo runs through it.
    if let Some(gate) = config.local_build_gate.as_ref().filter(|_| is_cargo(argv)) {
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
    let value = json!({"ok":true,"job":id,"state":"queued","placement":"laptop","placement_reason":reason,"project":inv.project,"manifest_id":inv.manifest_id,"root":inv.root,"argv":argv,"client":client,"request_id":request_id,"lease":lease,"created_at":state::now(),"exit_code":null,"cache_hit":false,"cache_reason":"Native toolchain identity is not locked; laptop result reuse is disabled","connection_config":tr.config});
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
    state::validate_id(job)?;
    spawn_detached("__worker", Some(job))
}
#[cfg(windows)]
pub(crate) fn spawn_detached(mode: &str, job: Option<&str>) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{
            CREATE_NEW_PROCESS_GROUP, CreateProcessW, DETACHED_PROCESS, PROCESS_INFORMATION,
            STARTUPINFOW,
        },
    };
    let current = std::env::current_exe()?;
    let exe = if mode == "__return_worker" {
        let worker = current.with_file_name("sparkdev-return.exe");
        if !worker.is_file() { return Err(AppError::new(3, "The automatic result worker is missing; reinstall RTXBridge")); }
        worker
    } else { current };
    let application: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut line = std::ffi::OsString::from("\"");
    line.push(&exe);
    line.push("\" ");
    line.push(mode);
    if let Some(job) = job { line.push(" "); line.push(job); }
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
    spawn_detached("__worker", Some(job))
}
#[cfg(not(windows))]
pub(crate) fn spawn_detached(mode: &str, job: Option<&str>) -> Result<()> {
    let mut command = Command::new(std::env::current_exe()?);
    command.arg(mode);
    if let Some(job) = job { command.arg(job); }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Only the async-signal-safe setsid syscall runs between fork and exec.
        unsafe { command.pre_exec(|| {
            if libc::setsid() == -1 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
        }); }
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}
fn public(mut value: Value) -> Result<Value> {
    if let Some(map) = value.as_object_mut() {
        for key in ["lease", "root", "argv", "connection_config"] {
            map.remove(key);
        }
    }
    Ok(value)
}
#[cfg(test)]
mod receipt_tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    /// Drive the worker's heartbeat schedule on a fake clock in 100 ms passes
    /// for `seconds`, answering each beat from `script` (its duration and
    /// whether it succeeded; then quick successes). Returns the start of
    /// every beat and when the lease was lost, both from the beginning.
    fn schedule(script: &[(u64, bool)], seconds: u64) -> (Vec<Duration>, Option<Duration>) {
        let begin = Instant::now();
        let clock = Cell::new(begin);
        let starts = RefCell::new(Vec::new());
        let mut script = script.iter().copied();
        let mut beats = Heartbeats::new(begin);
        while clock.get() - begin < Duration::from_secs(seconds) {
            let lost = beats
                .tick(
                    || clock.get(),
                    || {
                        starts.borrow_mut().push(clock.get() - begin);
                        let (took, ok) = script.next().unwrap_or((1, true));
                        clock.set(clock.get() + Duration::from_secs(took));
                        Ok(ok)
                    },
                )
                .unwrap();
            if lost {
                return (starts.into_inner(), Some(clock.get() - begin));
            }
            clock.set(clock.get() + Duration::from_millis(100));
        }
        (starts.into_inner(), None)
    }
    #[test]
    fn an_overrunning_heartbeat_is_followed_at_once_so_a_short_outage_keeps_the_job() {
        // The beat at 15 s meets a stalled Wi-Fi link: a pre-authentication
        // stall drops after about 10 s, the backoff passes, the one repeat
        // fails to connect, and the beat returns at about 33 s. The link is
        // back by then. Scheduled from its end, the next beat would start
        // at 48 s, after the job was ended at 45 s; from its start it runs
        // on the next pass of the loop and keeps the lease.
        let (starts, lost) = schedule(&[(18, false), (2, true)], 120);
        assert_eq!(lost, None);
        assert_eq!(starts[0], Duration::from_secs(15));
        assert_eq!(starts[1], Duration::from_millis(33_100));
        assert!(starts[2] <= Duration::from_secs(50), "{starts:?}");
        // Quick beats keep their 15 s spacing.
        let (starts, _) = schedule(&[], 50);
        assert_eq!(starts, [15, 30, 45].map(Duration::from_secs));
    }
    #[test]
    fn a_lease_without_a_successful_heartbeat_for_45_seconds_is_lost() {
        // The beat due at 45 s still runs first; when it fails too, at 46 s,
        // the job ends.
        let (starts, lost) = schedule(&[(1, false); 8], 120);
        assert_eq!(lost, Some(Duration::from_secs(46)));
        assert_eq!(starts, [15, 30, 45].map(Duration::from_secs));
        // A beat that succeeds late, even past 45 s, still keeps the lease.
        let (_, lost) = schedule(&[(31, true)], 60);
        assert_eq!(lost, None);
    }
    #[test]
    fn worker_connection_snapshot_is_not_exposed_in_public_receipts() {
        let value = public(json!({"job":"laptop-fixture","lease":"private-lease","root":"private-worktree","argv":["private-command"],"connection_config":{"ssh_alias":"private-host","connection_enabled":true}})).unwrap();
        assert_eq!(value, json!({"job":"laptop-fixture"}));
    }
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
/// A running laptop job sends a lease heartbeat this often.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(15);
/// With no successful heartbeat for this long the job is ended.
const LEASE_LOST: Duration = Duration::from_secs(45);
/// When the worker's next lease heartbeat is due and when the lease is lost.
///
/// A heartbeat is due HEARTBEAT_EVERY after the previous one started, not
/// after it returned. One heartbeat can take up to its 30 s request deadline:
/// a drop, the backoff and the one repeat the transport allows. Scheduled
/// from its end, the next beat could land after LEASE_LOST, so a Wi-Fi
/// outage of well under 45 s would end a job the next beat would have kept.
/// From its start, an overrunning beat is followed at once by another.
struct Heartbeats {
    started: Instant,
    ok: Instant,
}
impl Heartbeats {
    fn new(now: Instant) -> Self {
        Self {
            started: now,
            ok: now,
        }
    }
    /// One pass of the worker loop: send a heartbeat through `beat` when one
    /// is due, then say whether the lease is lost. `beat` says whether the
    /// heartbeat succeeded; `now` reads the clock.
    fn tick(
        &mut self,
        now: impl Fn() -> Instant,
        beat: impl FnOnce() -> Result<bool>,
    ) -> Result<bool> {
        let start = now();
        if start.saturating_duration_since(self.started) >= HEARTBEAT_EVERY {
            self.started = start;
            if beat()? {
                self.ok = now();
            }
        }
        Ok(now().saturating_duration_since(self.ok) >= LEASE_LOST)
    }
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
    // The accepted lease owns this transport snapshot. A later owner disconnect
    // prevents new submissions without killing an admitted native workload.
    let config: state::Config = if let Some(snapshot) = value.get("connection_config") {
        serde_json::from_value(snapshot.clone())?
    } else {
        state::config()?
    };
    state::validate_config(&config)?;
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
    if inventory::collect_readonly(&root)?.manifest_id != value["manifest_id"] {
        return Err(AppError::new(
            5,
            "Laptop source changed after submission; refusing stale job",
        ));
    }
    let mut tr = transport::Transport::from_config(config.clone());
    tr.rpc(json!({"v":1,"op":"heartbeat","lease":lease,"state":"running","ttl":60}))?;
    let gate = config.local_build_gate.as_ref().filter(|_| is_cargo(&argv));
    let mut command = if let Some(gate) = gate {
        let mut command = Command::new("python");
        command.arg("-I").arg(gate).arg("--").args(&argv);
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
    let mut beats = Heartbeats::new(Instant::now());
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
        let lost = beats.tick(Instant::now, || {
            let chunk = logs(job, offset, inventory::CHUNK)?;
            let heartbeat = json!({"v":1,"op":"heartbeat","lease":lease,"state":"running","ttl":60,"since":offset,"data":chunk["data"]});
            let ok = tr.rpc(heartbeat).is_ok();
            if ok {
                offset = chunk["next_offset"].as_u64().unwrap_or(offset);
            }
            Ok(ok)
        })?;
        if lost {
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
