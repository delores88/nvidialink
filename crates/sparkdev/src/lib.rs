pub mod inventory;
pub mod local;
pub mod mcp;
pub mod state;
pub mod transport;
use base64::{Engine, engine::general_purpose::STANDARD};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Debug)]
pub struct AppError {
    pub code: u8,
    pub message: String,
}
impl AppError {
    pub fn new(code: u8, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for AppError {}
impl From<std::io::Error> for AppError {
    fn from(_: std::io::Error) -> Self {
        Self::new(3, "Local filesystem or process operation failed")
    }
}
impl From<serde_json::Error> for AppError {
    fn from(_: serde_json::Error) -> Self {
        Self::new(2, "Invalid JSON data")
    }
}
pub type Result<T> = std::result::Result<T, AppError>;
#[derive(Parser)]
#[command(
    name = "sparkdev",
    version,
    about = "Run isolated development jobs on your Spark or laptop"
)]
struct Cli {
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, global = true, default_value = "human", value_enum)]
    client: Client,
    #[command(subcommand)]
    command: Commands,
}
#[derive(Clone, Copy, ValueEnum)]
enum Client {
    Claude,
    Codex,
    Human,
}
impl Client {
    fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Human => "human",
        }
    }
}
#[derive(Clone, Copy, ValueEnum, PartialEq, Debug)]
enum Placement {
    Auto,
    Laptop,
    Spark,
}
#[derive(Subcommand)]
enum Commands {
    Setup {
        #[arg(long)]
        ssh_alias: String,
        #[arg(long = "host")]
        hosts: Vec<String>,
        #[arg(long)]
        host_key_alias: String,
        #[arg(long)]
        identity_file: Option<PathBuf>,
        #[arg(long)]
        user: Option<String>,
        #[arg(long, default_value_t = 22)]
        port: u16,
        #[arg(long)]
        allow_remote: bool,
        #[arg(long)]
        local_build_gate: Option<PathBuf>,
    },
    Run {
        dir: PathBuf,
        #[arg(long)]
        detach: bool,
        #[arg(long)]
        runtime: Option<String>,
        #[arg(long)]
        argv_json: Option<PathBuf>,
        #[arg(long)]
        request_id: Option<String>,
        #[arg(long, value_enum, default_value = "auto")]
        r#where: Placement,
        #[arg(long)]
        no_cache: bool,
        #[arg(last = true)]
        command: Vec<String>,
    },
    Sync {
        dir: PathBuf,
        #[arg(long)]
        check: bool,
    },
    Status {
        #[arg(long)]
        all: bool,
    },
    Devices,
    Connection {
        #[arg(long, value_enum)]
        mode: Option<state::ConnectionMode>,
        #[arg(long)]
        laptop_jobs: Option<usize>,
    },
    Connect,
    Disconnect,
    Resources {
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long, conflicts_with = "file")]
        stdin: bool,
    },
    Plan {
        dir: PathBuf,
        #[arg(long)]
        runtime: Option<String>,
        #[arg(long)]
        argv_json: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "auto")]
        r#where: Placement,
        #[arg(last = true)]
        command: Vec<String>,
    },
    Doctor,
    Prepare {
        dir: PathBuf,
        #[arg(long)]
        runtime: String,
        #[arg(long)]
        request_id: Option<String>,
    },
    Logs {
        job: String,
        #[arg(long, default_value_t = 0)]
        since: u64,
        #[arg(long, default_value_t = 1048576)]
        limit: usize,
    },
    Wait {
        job: String,
        #[arg(long,default_value_t=30,value_parser=clap::value_parser!(u64).range(0..=60))]
        timeout: u64,
    },
    Cancel {
        job: String,
    },
    Pair,
    #[command(hide = true)]
    Mcp,
}
pub fn execute(args: Vec<String>) -> Result<Value> {
    execute_with_events(args, &mut |_| {})
}
pub fn mcp_client(args: &[String]) -> Option<String> {
    let cli =
        Cli::try_parse_from(std::iter::once("sparkdev".to_string()).chain(args.iter().cloned()))
            .ok()?;
    if matches!(cli.command, Commands::Mcp) {
        Some(cli.client.name().into())
    } else {
        None
    }
}
pub fn execute_with_events(args: Vec<String>, events: &mut dyn FnMut(&Value)) -> Result<Value> {
    let cli = Cli::try_parse_from(std::iter::once("sparkdev".into()).chain(args))
        .map_err(|e| AppError::new(if e.use_stderr() { 2 } else { 0 }, e.to_string()))?;
    let client = cli.client.name();
    match cli.command {
        Commands::Setup {
            ssh_alias,
            hosts,
            host_key_alias,
            identity_file,
            user,
            port,
            allow_remote,
            local_build_gate,
        } => {
            if state::diagnostics()["redirected"] == true {
                return Err(AppError::new(
                    3,
                    "Windows redirected this process's state folder. Configure SSH in the normally installed DGXBridge app, or set SPARKDEV_HOME to the verified owner state location; no configuration was copied or saved.",
                ));
            }
            let previous = state::config().ok();
            let config = state::Config {
                ssh_alias,
                hosts,
                host_key_alias,
                identity_file,
                user,
                port,
                allow_remote,
                connection_mode: previous.as_ref().and_then(|c| c.connection_mode),
                connection_enabled: previous.as_ref().is_none_or(|c| c.connection_enabled),
                laptop_jobs: previous.as_ref().map(|c| c.laptop_jobs).unwrap_or(2),
                local_build_gate,
            };
            state::validate_config(&config)?;
            state::atomic_json(&state::root()?.join("config.json"), &config)?;
            Ok(json!({"ok":true,"configured":true,"remote_enabled":allow_remote}))
        }
        Commands::Sync { dir, check } => {
            let inv = inventory::collect(&dir)?;
            if check {
                return Ok(
                    json!({"ok":true,"checked":true,"project":inv.project,"manifest_id":inv.manifest_id,"files":inv.manifest.len(),"bytes":inv.bytes}),
                );
            }
            let mut tr = transport::Transport::new()?;
            flush_cancels(&mut tr);
            let mut out = sync(&inv, &mut tr)?;
            out["transport"] = tr.info();
            Ok(out)
        }
        Commands::Devices => devices(),
        Commands::Connection { mode, laptop_jobs } => {
            let config = if mode.is_some() || laptop_jobs.is_some() {
                state::update_config(|config| {
                    if let Some(mode) = mode {
                        config.connection_mode = Some(mode);
                        config.allow_remote = config.allows_remote();
                    }
                    if let Some(jobs) = laptop_jobs {
                        config.laptop_jobs = jobs;
                    }
                })?
            } else {
                state::config()?
            };
            Ok(connection_response(&config))
        }
        Commands::Connect => {
            let config = state::update_config(|config| config.connection_enabled = true)?;
            let mut result = connection_response(&config);
            let mut tr = transport::Transport::new()?;
            match tr.rpc(json!({"v":1,"op":"doctor"})) {
                Ok(doctor) => {
                    result["reachable"] = json!(true);
                    result["doctor"] = doctor;
                }
                Err(error) => {
                    result["ok"] = json!(false);
                    result["reachable"] = json!(false);
                    result["error"] = json!({"code":error.code,"message":error.message});
                    result["process_exit_code"] = json!(error.code);
                }
            }
            result["transport"] = tr.info();
            Ok(result)
        }
        Commands::Disconnect => {
            let config = state::update_config(|config| config.connection_enabled = false)?;
            Ok(connection_response(&config))
        }
        Commands::Resources { file, stdin } => {
            let policy = if stdin {
                Some(read_resource_input(std::io::stdin().lock())?)
            } else {
                file.map(|file| state::read_json(&file)).transpose()?
            };
            resource_limits(policy)
        }
        Commands::Plan {
            dir,
            runtime,
            argv_json,
            r#where,
            command,
        } => {
            let argv = load_argv(command, argv_json)?;
            // This deliberately has no transport, receipt or admission path.
            let inv = inventory::collect_readonly(&dir)?;
            let (chosen, reason) = placement(r#where, &argv, &inv.root)?;
            let runtime = runtime.unwrap_or_else(|| default_runtime(&argv).into());
            state::validate_id(&runtime)?;
            Ok(
                json!({"ok":true,"placement":if chosen == Placement::Laptop {"laptop"} else {"spark"},"placement_reason":reason,"runtime":runtime,"project":inv.project,"manifest_id":inv.manifest_id,"files":inv.manifest.len(),"bytes":inv.bytes,"argv":argv,"admission_required":true,"claimed":false,"uploaded":false,"submitted":false,"runtime_registry_checked":false,"observation_only":true,"note":"Placement preview only, not a reservation. Run revalidates source and requires current scheduler admission; runtime availability is checked on submission."}),
            )
        }
        Commands::Prepare {
            dir,
            runtime,
            request_id,
        } => {
            let inv = inventory::collect(&dir)?;
            let request_id = request_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            state::validate_id(&request_id)?;
            let request = json!({"v":1,"op":"prepare","project":inv.project,"manifest_id":inv.manifest_id,"runtime":runtime,"client":client,"request_id":request_id});
            state::save_request(&request_id, &request)?;
            let mut tr = transport::Transport::new()?;
            flush_cancels(&mut tr);
            sync(&inv, &mut tr)?;
            let mut result = tr.rpc(request)?;
            result["request_id"] = json!(request_id);
            result["transport"] = tr.info();
            Ok(result)
        }
        Commands::Run {
            dir,
            detach,
            runtime,
            argv_json,
            request_id,
            r#where,
            no_cache,
            command,
        } => {
            let argv = load_argv(command, argv_json)?;
            let inv = inventory::collect(&dir)?;
            let (where_, reason) = placement(r#where, &argv, &inv.root)?;
            let runtime = runtime.unwrap_or_else(|| default_runtime(&argv).into());
            let request_id = request_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            state::validate_id(&request_id)?;
            let mut request = json!({"v":1,"op":"run","project":inv.project,"manifest_id":inv.manifest_id,"argv":argv,"runtime":runtime,"client":client,"request_id":request_id,"no_cache":no_cache});
            // Persist the immutable request before submission, so an ambiguous disconnect is recoverable.
            let saved_request = json!({"placement":format!("{where_:?}"),"request":request});
            state::save_request(&request_id, &saved_request)?;
            let mut out = if where_ == Placement::Laptop {
                local::submit(&inv, &argv, client, &request_id, &reason)?
            } else {
                let mut tr = transport::Transport::new()?;
                flush_cancels(&mut tr);
                let synced = sync(&inv, &mut tr)?;
                request["manifest_id"] = synced["manifest_id"].clone();
                let mut value = tr.rpc(request)?;
                value["transport"] = tr.info();
                value
            };
            out["request_id"] = json!(request_id);
            out["placement"] = json!(if where_ == Placement::Laptop {
                "laptop"
            } else {
                "spark"
            });
            out["placement_reason"] = json!(reason);
            if detach {
                return Ok(out);
            }
            let job = out["job"]
                .as_str()
                .ok_or_else(|| AppError::new(6, "Remote omitted job ID"))?
                .to_string();
            events(
                &json!({"event":"submitted","job":job,"request_id":request_id,"placement":out["placement"]}),
            );
            let mut offset = 0u64;
            loop {
                let result = logs(&job, offset, inventory::CHUNK)?;
                let previous = offset;
                offset = result["next_offset"].as_u64().unwrap_or(offset);
                if result["data"].as_str().is_some_and(|s| !s.is_empty()) {
                    events(&result)
                }
                if terminal(&result) && offset - previous < inventory::CHUNK as u64 {
                    let mut result = result;
                    result["job"] = json!(job);
                    result["request_id"] = json!(request_id);
                    result["process_exit_code"] = result["exit_code"].clone();
                    return Ok(result);
                }
                if !terminal(&result) {
                    std::thread::sleep(Duration::from_millis(750));
                }
            }
        }
        Commands::Status { all } => {
            let mut tr = transport::Transport::new()?;
            flush_cancels(&mut tr);
            let remote = tr.rpc(json!({"v":1,"op":"status","all":all}));
            let local = local::status()?;
            match remote {
                Ok(mut v) => {
                    v["transport"] = tr.info();
                    if all {
                        v["laptop_jobs"] = local;
                    }
                    Ok(v)
                }
                Err(e) if all => Ok(
                    json!({"ok":true,"remote_available":false,"remote_error":{"code":e.code,"message":e.message},"laptop_jobs":local,"transport":tr.info()}),
                ),
                Err(e) => Err(e),
            }
        }
        Commands::Doctor => {
            let mut tr = transport::Transport::new()?;
            let mut v = tr.rpc(json!({"v":1,"op":"doctor"}))?;
            v["transport"] = tr.info();
            v["client"] = json!({"version":env!("CARGO_PKG_VERSION"),"os":std::env::consts::OS,"state_writable":true,"process_launch":true,"ssh_network":tr.selected=="ssh"});
            Ok(v)
        }
        Commands::Logs { job, since, limit } => logs(&job, since, limit.min(inventory::CHUNK)),
        Commands::Wait { job, timeout } => {
            state::validate_id(&job)?;
            if job.starts_with("laptop-") {
                local::wait(&job, timeout)
            } else {
                let mut tr = transport::Transport::new()?;
                flush_cancels(&mut tr);
                tr.rpc(json!({"v":1,"op":"wait","job":job,"timeout":timeout}))
            }
        }
        Commands::Cancel { job } => {
            state::validate_id(&job)?;
            if job.starts_with("laptop-") {
                local::cancel(&job)
            } else {
                let file = state::root()?
                    .join("pending-cancels")
                    .join(format!("{job}.json"));
                let request = json!({"v":1,"op":"cancel","job":job});
                state::atomic_json(&file, &request)?;
                let mut tr = transport::Transport::new()?;
                match tr.rpc(request) {
                    Ok(v) => {
                        fs::remove_file(file)?;
                        Ok(v)
                    }
                    Err(e) if e.code == 4 => Ok(
                        json!({"ok":true,"job":job,"state":"cancel_pending","delivery":"queued_until_authenticated_reconnect"}),
                    ),
                    Err(e) => Err(e),
                }
            }
        }
        Commands::Pair => Err(AppError::new(
            2,
            "Use the desktop Link Spark action for the private SSH bootstrap and QR approval",
        )),
        Commands::Mcp => Err(AppError::new(2, "MCP stdio must run as a process")),
    }
}
pub fn resource_limits(policy: Option<Value>) -> Result<Value> {
    let mut request = json!({"v":1,"op":"resources"});
    if let Some(policy) = policy {
        request["limits"] = policy;
    }
    let mut tr = transport::Transport::new()?;
    let mut response = tr.rpc(request)?;
    response["transport"] = tr.info();
    Ok(response)
}
fn read_resource_input(input: impl Read) -> Result<Value> {
    let mut bytes = Vec::new();
    input.take(65537).read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        return Err(AppError::new(2, "Resource policy JSON exceeds 64 KiB"));
    }
    let policy: Value = serde_json::from_slice(&bytes)?;
    if !policy.is_object() {
        return Err(AppError::new(2, "Resource policy must be a JSON object"));
    }
    Ok(policy)
}
fn connection_response(config: &state::Config) -> Value {
    json!({"ok":true,"connection":config.connection_info(),"laptop":config.laptop_info(),"state":state::diagnostics(),"running_jobs_unchanged":true})
}
fn devices() -> Result<Value> {
    let mut laptop = json!({"id":"laptop","availability":"available","reachable":true,"os":std::env::consts::OS,"architecture":std::env::consts::ARCH,"logical_parallelism":std::thread::available_parallelism().ok().map(|n|n.get()),"managed_job_limit":2,"free_cpu":null,"free_memory_bytes":null,"capacity_note":"Logical parallelism is process-visible topology, not idle capacity. Native jobs are not container-sandboxed."});
    if let Ok(config) = state::config() {
        laptop["managed_job_limit"] = json!(config.laptop_jobs);
        laptop["resource_controls"] = config.laptop_info();
    }
    match local::status() {
        Ok(jobs) => laptop["jobs"] = jobs,
        Err(e) => laptop["state_error"] = json!({"code":e.code,"message":e.message}),
    }
    let mut spark = json!({"id":"spark","availability":"unconfigured","reachable":false,"os":null,"architecture":null,"free_cpu":null,"free_memory_bytes":null,"transport":{"selected":"unselected","route":"none","authenticated":false,"attempted_routes":[]}});
    let mut board = Value::Null;
    let mut scheduler_reachable = false;
    let mut scheduler_healthy = false;
    // Device inspection does not flush deferred cancellation or terminal writes.
    let observation = match transport::Transport::new() {
        Ok(mut tr) => {
            spark["availability"] = json!("unreachable");
            let result = tr.rpc(json!({"v":1,"op":"status","all":true}));
            spark["transport"] = tr.info();
            if !tr.config.connection_enabled {
                spark["availability"] = json!("disconnected");
            }
            result
        }
        Err(e) => Err(e),
    };
    match observation {
        Ok(status) => {
            spark["reachable"] = json!(true);
            spark["availability"] = json!("reachable");
            spark["scheduler"] = status["scheduler"].clone();
            spark["managed_limits"] = status["limits"].clone();
            spark["hardware"] = status["hardware"].clone();
            spark["runtimes"] = status["runtimes"].clone();
            spark["capacity_note"] = json!(
                "Scheduler limits and recorded jobs are observations, not free hardware capacity or a reservation."
            );
            board = status["jobs"].clone();
            scheduler_reachable = true;
            scheduler_healthy = status["scheduler"]["healthy"].as_bool() == Some(true);
        }
        Err(e) => {
            let authenticated = spark["transport"]["authenticated"] == true;
            spark["error"] = json!({"code":e.code,"message":e.message,"kind":match e.code {7=>"authentication_failed",6 if authenticated=>"authenticated_controller_failed",6=>"protocol_error",3 if spark["availability"] == "unconfigured"=>"configuration_unavailable",3=>"remote_adapter_unconfigured",_=>"transport_unavailable"}});
        }
    }
    let admission = admission_observation(scheduler_reachable, scheduler_healthy);
    for device in [&mut laptop, &mut spark] {
        device["can_submit"] = if scheduler_healthy {
            Value::Null
        } else {
            json!(false)
        };
        device["admission"] = admission.clone();
    }
    Ok(
        json!({"ok":true,"observed_at":state::now(),"devices":[laptop,spark],"shared_board":board,"observation_only":true,"admission_required":true,"claimed":false}),
    )
}
fn admission_observation(reachable: bool, healthy: bool) -> Value {
    json!({"scheduler_reachable":reachable,"scheduler_healthy":healthy,"available":if reachable && healthy {"requires_request_check"} else {"unavailable"},"reservation":false,"reason":if !reachable {"Managed admission on both devices requires the Spark scheduler; offline laptop fallback is unavailable"} else if !healthy {"Scheduler status is reachable but its heartbeat is not healthy"} else {"Runtime, client quota, resources and worktree claim must be checked for the submitted request"}})
}
fn load_argv(command: Vec<String>, file: Option<PathBuf>) -> Result<Vec<String>> {
    if file.is_some() && !command.is_empty() {
        return Err(AppError::new(
            2,
            "Use either --argv-json or a command after --",
        ));
    }
    let argv = if let Some(path) = file {
        serde_json::from_slice::<Vec<String>>(&fs::read(path)?)?
    } else {
        command
    };
    if argv.is_empty()
        || argv[0].is_empty()
        || argv.len() > 4096
        || argv
            .iter()
            .any(|s| s.contains('\0') || s.len() > 128 * 1024)
    {
        return Err(AppError::new(
            2,
            "Command must be a nonempty exact argument array",
        ));
    }
    for arg in &argv {
        inventory::scan(arg.as_bytes())?;
        let lower = arg.to_ascii_lowercase();
        if [
            "--password",
            "--token",
            "--api-key",
            "--secret",
            "authorization: bearer ",
        ]
        .iter()
        .any(|p| {
            lower == *p
                || lower.starts_with(&format!("{p}="))
                || lower.contains("authorization: bearer ")
        }) {
            return Err(AppError::new(5, "Credential arguments are refused"));
        }
    }
    Ok(argv)
}
fn default_runtime(argv: &[String]) -> &'static str {
    let name = Path::new(&argv[0])
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    match name {
        "cargo" | "rustc" | "rustup" => "rust",
        "node" | "npm" | "npx" => "node",
        _ => "python",
    }
}
fn placement(requested: Placement, argv: &[String], root: &Path) -> Result<(Placement, String)> {
    if requested != Placement::Auto {
        return Ok((requested, "Explicit --where selection".into()));
    }
    let windows = argv[0].to_ascii_lowercase().ends_with(".exe")
        || argv.iter().any(|a| {
            let a = a.to_ascii_lowercase();
            a.contains("windows-")
                || a.contains("-windows")
                || a.contains("msvc")
                || a.contains("nsis")
                || a.contains("windows_sandbox")
                || a.contains("windows-sandbox")
                || a.contains("cfg(windows)")
                || a.ends_with(".ps1")
                || a.ends_with(".cmd")
                || a.ends_with(".bat")
                || matches!(
                    a.as_str(),
                    "powershell"
                        | "powershell.exe"
                        | "pwsh"
                        | "pwsh.exe"
                        | "cmd"
                        | "cmd.exe"
                        | "makensis"
                        | "makensis.exe"
                        | "signtool"
                )
        });
    let tauri = (argv.iter().any(|a| a == "tauri")
        && argv.iter().any(|a| a == "build" || a == "bundle"))
        || root.join("tauri.conf.json").exists()
        || argv
            .iter()
            .any(|a| a.replace('\\', "/").ends_with("src-tauri/Cargo.toml"));
    // Windows-only work remains local even when the request omitted a target triple.
    if windows || tauri {
        return Ok((
            Placement::Laptop,
            "Windows command, target, sandbox or installer detected".into(),
        ));
    }
    Ok((
        Placement::Spark,
        "Command has no Windows-specific requirement; using the Spark runtime".into(),
    ))
}
pub fn terminal(value: &Value) -> bool {
    matches!(
        value["state"].as_str(),
        Some("succeeded" | "failed" | "cancelled" | "completed" | "error")
    )
}
fn sync(inv: &inventory::Inventory, tr: &mut transport::Transport) -> Result<Value> {
    let started = Instant::now();
    let hashes: std::collections::BTreeSet<_> = inv
        .manifest
        .iter()
        .flat_map(|e| e.chunks.iter().map(|c| c.sha256.clone()))
        .collect();
    let hashes: Vec<_> = hashes.into_iter().collect();
    let mut uploaded = 0u64;
    let mut objects: Vec<Value> = Vec::new();
    let mut payload_size = 64usize;
    for batch in hashes.chunks(2048) {
        let missing = tr.rpc(json!({"v":1,"op":"missing","hashes":batch}))?;
        let missing = missing["missing"]
            .as_array()
            .ok_or_else(|| AppError::new(6, "Remote missing response is invalid"))?;
        for hash in missing {
            let hash = hash
                .as_str()
                .ok_or_else(|| AppError::new(6, "Invalid missing hash"))?;
            if !batch.iter().any(|h| h == hash) {
                return Err(AppError::new(6, "Remote requested an unlisted object"));
            }
            let bytes = fs::read(state::root()?.join("cas").join(hash))?;
            if inventory::hash(&bytes) != hash {
                return Err(AppError::new(5, "Local transfer object hash mismatch"));
            }
            let object = json!({"sha256":hash,"data":STANDARD.encode(&bytes)});
            let size = serde_json::to_vec(&object)?.len() + 1;
            if objects.len() >= 128 || payload_size + size > 3 * inventory::CHUNK - 256 {
                upload_batch(tr, &objects)?;
                objects.clear();
                payload_size = 64;
            }
            objects.push(object);
            payload_size += size;
            uploaded += bytes.len() as u64;
        }
    }
    if !objects.is_empty() {
        upload_batch(tr, &objects)?;
    }
    let mut out =
        tr.rpc(json!({"v":1,"op":"sync","project":inv.project,"manifest":inv.manifest}))?;
    if out["manifest_id"] != inv.manifest_id {
        return Err(AppError::new(
            6,
            "Remote manifest digest differs from local canonical digest",
        ));
    }
    out["uploaded_bytes"] = json!(uploaded);
    out["sync_seconds"] = json!(started.elapsed().as_secs_f64());
    Ok(out)
}
fn upload_batch(tr: &mut transport::Transport, objects: &[Value]) -> Result<()> {
    let response = tr.rpc(json!({"v":1,"op":"put_many","chunks":objects}))?;
    let accepted = response["accepted"]
        .as_array()
        .ok_or_else(|| AppError::new(6, "Missing upload batch receipt"))?;
    if accepted.len() != objects.len()
        || accepted
            .iter()
            .zip(objects)
            .any(|(a, b)| a["sha256"] != b["sha256"])
    {
        return Err(AppError::new(
            6,
            "Upload batch receipt does not match requested objects",
        ));
    }
    Ok(())
}
fn flush_cancels(tr: &mut transport::Transport) {
    local::flush_pending(tr);
    let Ok(root) = state::root() else { return };
    let Ok(files) = fs::read_dir(root.join("pending-cancels")) else {
        return;
    };
    for entry in files.flatten() {
        if let Ok(request) = state::read_json(&entry.path()) {
            if tr.rpc(request).is_ok() {
                let _ = fs::remove_file(entry.path());
            } else {
                break;
            }
        }
    }
}
pub fn logs(job: &str, since: u64, limit: usize) -> Result<Value> {
    state::validate_id(job)?;
    let out = if job.starts_with("laptop-") {
        local::logs(job, since, limit)?
    } else {
        let mut tr = transport::Transport::new()?;
        flush_cancels(&mut tr);
        tr.rpc(json!({"v":1,"op":"logs","job":job,"since":since,"limit":limit}))?
    };
    if let Some(data) = out["data"].as_str() {
        let bytes = STANDARD
            .decode(data)
            .map_err(|_| AppError::new(6, "Invalid log encoding"))?;
        let returned = out["since"].as_u64().unwrap_or(since);
        if returned != since || out["next_offset"].as_u64() != Some(since + bytes.len() as u64) {
            return Err(AppError::new(6, "Log offset integrity check failed"));
        }
        let root = state::root()?.join("logs");
        fs::create_dir_all(&root)?;
        let path = root.join(format!("{job}.log"));
        let lock = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(root.join(format!("{job}.lock")))?;
        fs2::FileExt::lock_exclusive(&lock)?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        file.seek(SeekFrom::Start(since))?;
        file.write_all(&bytes)?;
        file.sync_data()?;
        fs2::FileExt::unlock(&lock)?;
    }
    Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn argv_is_exact() {
        let v = vec![
            "python".into(),
            "a b".into(),
            "$HOME; & \"x\"".into(),
            "".into(),
        ];
        assert_eq!(load_argv(v.clone(), None).unwrap(), v)
    }
    #[test]
    fn windows_placement() {
        let a = vec![
            "cargo".into(),
            "test".into(),
            "--target".into(),
            "x86_64-pc-windows-msvc".into(),
        ];
        assert_eq!(
            placement(Placement::Auto, &a, Path::new(".")).unwrap().0,
            Placement::Laptop
        );
        assert_eq!(
            placement(Placement::Auto, &["pytest".into()], Path::new("."))
                .unwrap()
                .0,
            Placement::Spark
        )
    }
    #[test]
    fn command_parser_accepts_global_flags_and_delimiter() {
        let args = [
            "sparkdev",
            "run",
            ".",
            "--detach",
            "--json",
            "--client",
            "codex",
            "--where",
            "spark",
            "--",
            "python",
            "-c",
            "print('a b')",
        ];
        assert!(Cli::try_parse_from(args).is_ok())
    }
    #[test]
    fn command_parser_bounds_wait() {
        assert!(Cli::try_parse_from(["sparkdev", "wait", "abc", "--timeout", "61"]).is_err())
    }
    #[test]
    fn resource_stdin_is_exclusive_bounded_and_object_only() {
        assert!(Cli::try_parse_from(["sparkdev", "resources", "--stdin"]).is_ok());
        assert!(
            Cli::try_parse_from(["sparkdev", "resources", "--stdin", "--file", "policy.json"])
                .is_err()
        );
        assert_eq!(
            read_resource_input(&b"{\"cpu_total\":16}"[..]).unwrap()["cpu_total"],
            16
        );
        assert!(read_resource_input(&b"[]"[..]).is_err());
        assert!(read_resource_input(&b"{broken"[..]).is_err());
        assert!(read_resource_input(&vec![b' '; 65537][..]).is_err());
    }
    #[test]
    fn command_arguments_cannot_start_mcp() {
        assert!(mcp_client(&["run".into(), ".".into(), "--".into(), "mcp".into()]).is_none());
        assert_eq!(
            mcp_client(&["--client".into(), "codex".into(), "mcp".into()]),
            Some("codex".into())
        )
    }
    #[test]
    fn plan_preserves_argv_and_has_no_submission_or_reservation() {
        let source = tempfile::tempdir().unwrap();
        let arguments = tempfile::tempdir().unwrap();
        fs::write(source.path().join("main.rs"), b"fn main() {}\n").unwrap();
        let argv = json!([
            "cargo",
            "test",
            "--target",
            "x86_64-pc-windows-msvc",
            "two words",
            ""
        ]);
        let argv_file = arguments.path().join("argv.json");
        fs::write(&argv_file, serde_json::to_vec(&argv).unwrap()).unwrap();
        let out = execute(vec![
            "plan".into(),
            source.path().to_string_lossy().into(),
            "--argv-json".into(),
            argv_file.to_string_lossy().into(),
        ])
        .unwrap();
        assert_eq!(out["placement"], "laptop");
        assert_eq!(out["runtime"], "rust");
        assert_eq!(out["argv"], argv);
        assert_eq!(out["admission_required"], true);
        for key in [
            "claimed",
            "uploaded",
            "submitted",
            "runtime_registry_checked",
        ] {
            assert_eq!(out[key], false);
        }
        assert!(out.get("job").is_none());
        assert!(out.get("request_id").is_none());
        assert!(out.get("transport").is_none());
        assert_eq!(fs::read_dir(source.path()).unwrap().count(), 1);
        let explicit = execute(vec![
            "plan".into(),
            source.path().to_string_lossy().into(),
            "--where".into(),
            "spark".into(),
            "--".into(),
            "cargo".into(),
            "test".into(),
        ])
        .unwrap();
        assert_eq!(explicit["placement"], "spark");
    }
    #[test]
    fn plan_refuses_secret_source_and_credential_argv() {
        let source = tempfile::tempdir().unwrap();
        let base = vec![
            "plan".into(),
            source.path().to_string_lossy().into(),
            "--".into(),
        ];
        let mut command = base.clone();
        command.extend(["python".into(), "--token=example".into()]);
        assert_eq!(execute(command).unwrap_err().code, 5);
        fs::write(source.path().join(".env"), b"EXAMPLE=value\n").unwrap();
        let mut command = base;
        command.push("python".into());
        assert_eq!(execute(command).unwrap_err().code, 5);
    }
    #[test]
    fn scheduler_observation_never_claims_reserved_or_free_capacity() {
        let offline = admission_observation(false, false);
        assert_eq!(offline["available"], "unavailable");
        assert_eq!(offline["scheduler_reachable"], false);
        let online = admission_observation(true, true);
        assert_eq!(online["available"], "requires_request_check");
        assert_eq!(online["reservation"], false);
        assert_eq!(
            admission_observation(true, false)["available"],
            "unavailable"
        );
    }
}
