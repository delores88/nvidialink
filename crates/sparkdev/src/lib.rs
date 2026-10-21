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
            if check {
                // A check never stages transfer objects.
                let inv = inventory::collect_readonly(&dir)?;
                let large = inv
                    .manifest
                    .iter()
                    .filter(|e| e.size > inventory::LARGE_FILE)
                    .count();
                return Ok(
                    json!({"ok":true,"checked":true,"project":inv.project,"manifest_id":inv.manifest_id,"files":inv.manifest.len(),"bytes":inv.bytes,"small_bytes":inv.small_bytes,"large_files":large,"limits":local_limits()}),
                );
            }
            let inv = inventory::collect(&dir)?;
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
            if !Rpc::connection_enabled(&tr) {
                return Err(disconnected());
            }
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
            // Placement first: a laptop job reads its own tree and needs no transfer objects.
            let root = dir.canonicalize()?;
            let (where_, reason) = placement(r#where, &argv, &root)?;
            let inv = if where_ == Placement::Laptop {
                inventory::collect_readonly(&root)?
            } else {
                inventory::collect(&root)?
            };
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
                // A sync can poll for minutes; a disconnect meanwhile also stops the submission.
                if !Rpc::connection_enabled(&tr) {
                    return Err(disconnected());
                }
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
fn local_limits() -> Value {
    json!({"max_file":inventory::MAX_FILE,"max_tree":inventory::MAX_TREE,"max_small_tree":inventory::MAX_SMALL_TREE,"large_file":inventory::LARGE_FILE,"max_files":inventory::MAX_FILES})
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
    let mut spark = json!({"id":"spark","availability":"unconfigured","reachable":false,"os":null,"architecture":null,"free_cpu":null,"free_memory_bytes":null,"network":null,"transport":{"selected":"unselected","route":"none","authenticated":false,"attempted_routes":[]}});
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
            spark["network"] = spark_network(&status);
            // Absent (null) from a controller that predates disk reporting.
            spark["storage"] = status.get("storage").cloned().unwrap_or(Value::Null);
            spark["sync_limits"] = status.get("sync_limits").cloned().unwrap_or(Value::Null);
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
/// The controller's sanitized link report, passed through unchanged. Additive
/// fields keep `v:1`; only a breaking change bumps `v`, which reads as null here
/// (as does an older controller that sends no report).
fn spark_network(status: &Value) -> Value {
    match status.get("network") {
        Some(network) if network.is_object() && network["v"] == 1 => network.clone(),
        _ => Value::Null,
    }
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
/// One authenticated request/response exchange with the Spark controller.
pub trait Rpc {
    fn call(&mut self, request: Value) -> Result<Value>;
    /// Route of the latest authenticated exchange: "ssh", "iroh", or anything else while unknown.
    fn route(&self) -> String;
    /// Owner intent, read again from saved state before every retry.
    fn connection_enabled(&self) -> bool;
}
impl Rpc for transport::Transport {
    fn call(&mut self, request: Value) -> Result<Value> {
        self.rpc(request)
    }
    fn route(&self) -> String {
        self.selected.clone()
    }
    fn connection_enabled(&self) -> bool {
        state::config().is_ok_and(|config| config.connection_enabled)
    }
}
const IROH_REQUEST_LIMIT: u64 = 4 * inventory::MIB - 64 * 1024;
const SSH_BATCH_BODY: u64 = 8 * inventory::MIB;
const BATCH_MARGIN: u64 = 256;
const BATCH_OBJECTS: usize = 128;
const MISSING_BATCH: usize = 8192;
const SYNC_ROUNDS: usize = 3;
const RETRY_DELAYS: [u64; 5] = [2, 4, 8, 16, 30];
const POLL_DELAY: Duration = Duration::from_secs(2);
const STALL_LIMIT: Duration = Duration::from_secs(600);
/// The limits one sync works under: advertised by the controller, or legacy.
#[derive(Clone, Debug, PartialEq)]
struct SparkLimits {
    advertised: bool,
    max_file: u64,
    max_tree: u64,
    max_small_tree: u64,
    large_file: u64,
    max_files: u64,
    max_request: u64,
    put_many_body: u64,
    missing_batch: usize,
}
impl SparkLimits {
    fn legacy() -> Self {
        Self {
            advertised: false,
            max_file: inventory::LEGACY_MAX_FILE,
            max_tree: inventory::LEGACY_MAX_TREE,
            max_small_tree: inventory::LEGACY_MAX_TREE,
            large_file: inventory::LEGACY_MAX_FILE,
            max_files: inventory::MAX_FILES as u64,
            max_request: inventory::MAX_REQUEST,
            put_many_body: inventory::LEGACY_BATCH_BODY,
            missing_batch: inventory::LEGACY_MISSING_BATCH,
        }
    }
    /// Read the `missing []` probe. No `limits` means a controller from before this
    /// change; advertised values never raise what this CLI itself accepts.
    fn from_probe(probe: &Value) -> Result<Self> {
        let Some(limits) = probe.get("limits").filter(|value| !value.is_null()) else {
            return Ok(Self::legacy());
        };
        let field = |name: &str| {
            limits
                .get(name)
                .and_then(Value::as_u64)
                .filter(|value| *value > 0)
                .ok_or_else(|| {
                    AppError::new(
                        6,
                        "Spark controller advertised invalid sync limits; update Spark setup",
                    )
                })
        };
        if field("chunk")? != inventory::MIB {
            return Err(AppError::new(
                6,
                "Spark controller uses a different transfer chunk size; update Spark setup",
            ));
        }
        Ok(Self {
            advertised: true,
            max_file: field("max_file")?.min(inventory::MAX_FILE),
            max_tree: field("max_tree")?.min(inventory::MAX_TREE),
            max_small_tree: field("max_small_tree")?.min(inventory::MAX_SMALL_TREE),
            large_file: field("large_file")?.min(inventory::LARGE_FILE),
            max_files: field("max_files")?.min(inventory::MAX_FILES as u64),
            max_request: field("max_request")?.min(inventory::MAX_REQUEST),
            put_many_body: field("put_many_body")?,
            missing_batch: field("missing_batch")?.min(MISSING_BATCH as u64) as usize,
        })
    }
    fn view(&self) -> Value {
        json!({"source":if self.advertised {"advertised"} else {"legacy"},"max_file":self.max_file,"max_tree":self.max_tree,"max_small_tree":self.max_small_tree,"large_file":self.large_file,"max_files":self.max_files})
    }
    /// Refuse, before any upload, a project this controller cannot accept.
    fn check(&self, inv: &inventory::Inventory) -> Result<()> {
        let small: u64 = inv
            .manifest
            .iter()
            .filter(|entry| entry.size <= self.large_file)
            .map(|entry| entry.size)
            .sum();
        if inv.manifest.len() as u64 > self.max_files
            || inv.manifest.iter().any(|entry| entry.size > self.max_file)
            || inv.bytes > self.max_tree
            || small > self.max_small_tree
        {
            let message = if self.advertised {
                format!(
                    "Project exceeds the Spark controller's {} file, {} project or {} small-file limit; update Spark setup",
                    inventory::size_text(self.max_file),
                    inventory::size_text(self.max_tree),
                    inventory::size_text(self.max_small_tree)
                )
            } else {
                format!(
                    "This Spark controller accepts at most {} per file and {} per project; update Spark setup to sync larger projects",
                    inventory::size_text(self.max_file),
                    inventory::size_text(self.max_tree)
                )
            };
            return Err(AppError::new(5, message));
        }
        Ok(())
    }
    /// Largest put_many body for the current route.
    fn batch_body(&self, route: &str) -> u64 {
        if self.advertised && route == "ssh" {
            self.put_many_body
                .min(SSH_BATCH_BODY)
                .saturating_sub(BATCH_MARGIN)
        } else {
            inventory::LEGACY_BATCH_BODY - BATCH_MARGIN
        }
    }
    /// Largest request body for the current route.
    fn request_limit(&self, route: &str) -> u64 {
        if route == "iroh" {
            IROH_REQUEST_LIMIT
        } else {
            self.max_request - 1
        }
    }
}
fn remote_code(error: &AppError) -> Option<&str> {
    (error.code == 6)
        .then(|| error.message.split_once(": ").map(|(code, _)| code))
        .flatten()
}
fn source_changed() -> AppError {
    AppError::new(3, "Source changed during sync; rerun")
}
fn disconnected() -> AppError {
    AppError::new(
        4,
        "Bridge is disconnected; use sparkdev connect to enable new requests",
    )
}
/// A transport timeout, or the paired route's sidecar failing because the Spark daemon
/// cut off a request frame that took over 10 seconds: either way the batch was too slow.
fn too_slow(error: &AppError) -> bool {
    error.message.starts_with("Transport timeout")
        || error
            .message
            .starts_with("Authenticated remote sidecar unavailable")
}
fn gib_text(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / inventory::GIB as f64)
}
/// Refuse before an upload round when the Spark's advertised free space, less what this
/// sync already sent, cannot hold the absent chunks twice (stored objects, then the
/// assembled copy) above its reserve. A controller that advertises no storage is not checked.
fn disk_preflight(storage: &Value, uploaded: u64, absent: u64) -> Result<()> {
    let (Some(free), Some(reserve)) = (
        storage["free_bytes"].as_u64(),
        storage["reserve_bytes"].as_u64(),
    ) else {
        return Ok(());
    };
    let (free, needed) = (free.saturating_sub(uploaded), absent.saturating_mul(2));
    if free < reserve.saturating_add(needed) {
        return Err(AppError::new(
            6,
            format!(
                "insufficient_disk: Spark has {} GiB free; this upload needs {} GiB and DGXBridge keeps {} in reserve. Free Spark disk space, then retry",
                gib_text(free),
                gib_text(needed),
                inventory::size_text(reserve)
            ),
        ));
    }
    Ok(())
}
/// A chunk's bytes: from the staged store, else from the source at its offset,
/// always verified against the hash the manifest committed to.
fn chunk_data(inv: &inventory::Inventory, hash: &str) -> Result<Vec<u8>> {
    if let Some(cas) = &inv.cas {
        if let Ok(file) = fs::File::open(cas.join(hash)) {
            let mut bytes = Vec::new();
            if file
                .take(inventory::MIB + 1)
                .read_to_end(&mut bytes)
                .is_ok()
                && inventory::hash(&bytes) == hash
            {
                return Ok(bytes);
            }
        }
    }
    let source = inv
        .sources
        .get(hash)
        .ok_or_else(|| AppError::new(6, "Remote requested an unlisted object"))?;
    let mut file = fs::File::open(&source.path).map_err(|_| source_changed())?;
    file.seek(SeekFrom::Start(source.offset))
        .map_err(|_| source_changed())?;
    let mut bytes = vec![0; source.len as usize];
    file.read_exact(&mut bytes).map_err(|_| source_changed())?;
    if inventory::hash(&bytes) != hash {
        return Err(source_changed());
    }
    Ok(bytes)
}
/// Every chunk hash once, in the order the manifest first uses it, so reads
/// that fall back to the source tree stay sequential.
fn first_occurrence(inv: &inventory::Inventory) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    inv.manifest
        .iter()
        .flat_map(|entry| entry.chunks.iter())
        .filter(|chunk| seen.insert(chunk.sha256.as_str()))
        .map(|chunk| chunk.sha256.clone())
        .collect()
}
struct Session<'a> {
    rpc: &'a mut dyn Rpc,
    sleep: &'a mut dyn FnMut(Duration),
    succeeded: bool,
    retries: u64,
    uploaded: u64,
}
impl Session<'_> {
    /// Only an ambiguous transport failure is retried, and only after this sync
    /// has reached the controller once. Owner intent is never overridden.
    fn retry(&mut self, error: &AppError, attempt: usize) -> bool {
        if error.code != 4
            || !self.succeeded
            || attempt >= RETRY_DELAYS.len()
            || error.message.contains("Bridge is disconnected")
            || error.message.contains("LAN-only mode")
            || !self.rpc.connection_enabled()
        {
            return false;
        }
        (self.sleep)(Duration::from_secs(RETRY_DELAYS[attempt]));
        self.retries += 1;
        self.rpc.connection_enabled()
    }
    /// A sync may poll and upload for many minutes, while the transport keeps the
    /// connection state it read at startup: every new request reads saved owner intent.
    fn owner_allows(&self) -> Result<()> {
        if self.rpc.connection_enabled() {
            Ok(())
        } else {
            Err(disconnected())
        }
    }
    fn call(&mut self, request: &Value, retry: bool) -> Result<Value> {
        self.owner_allows()?;
        let mut attempt = 0;
        loop {
            match self.rpc.call(request.clone()) {
                Ok(value) => {
                    self.succeeded = true;
                    return Ok(value);
                }
                Err(error) => {
                    if !retry || !self.retry(&error, attempt) {
                        return Err(error);
                    }
                    attempt += 1;
                }
            }
        }
    }
    /// Ask which of `hashes` the Spark lacks, keeping their first-occurrence order.
    fn missing(&mut self, hashes: &[String], batch: usize, retry: bool) -> Result<Vec<String>> {
        let mut absent = Vec::new();
        for group in hashes.chunks(batch.max(1)) {
            let response = self.call(&json!({"v":1,"op":"missing","hashes":group}), retry)?;
            let listed = response["missing"]
                .as_array()
                .ok_or_else(|| AppError::new(6, "Remote missing response is invalid"))?;
            let asked: std::collections::HashSet<_> = group.iter().map(String::as_str).collect();
            let mut wanted = std::collections::HashSet::new();
            for hash in listed {
                let hash = hash
                    .as_str()
                    .ok_or_else(|| AppError::new(6, "Invalid missing hash"))?;
                if !asked.contains(hash) {
                    return Err(AppError::new(6, "Remote requested an unlisted object"));
                }
                wanted.insert(hash.to_string());
            }
            absent.extend(group.iter().filter(|hash| wanted.contains(*hash)).cloned());
        }
        Ok(absent)
    }
    /// Upload `hashes` in order. Each batch is sized for the route at that moment
    /// and halves after a batch proves too slow for the link; one chunk always fits.
    fn upload(
        &mut self,
        inv: &inventory::Inventory,
        hashes: &[String],
        limits: &SparkLimits,
        retry: bool,
    ) -> Result<()> {
        let (mut next, mut halvings, mut attempt) = (0usize, 0u32, 0usize);
        while next < hashes.len() {
            self.owner_allows()?;
            let cap = limits.batch_body(&self.rpc.route()) >> halvings;
            let (mut count, mut size) = (0usize, 64u64);
            for hash in &hashes[next..] {
                let len = inv.sources.get(hash).map_or(inventory::MIB, |s| s.len);
                // {"data":"<base64>","sha256":"<64 hex>"} plus a separator.
                let object = len.div_ceil(3) * 4 + 88;
                if count > 0 && (count >= BATCH_OBJECTS || size + object > cap) {
                    break;
                }
                count += 1;
                size += object;
            }
            let batch = &hashes[next..next + count];
            let mut objects = Vec::with_capacity(count);
            let mut bytes = 0u64;
            for hash in batch {
                let data = chunk_data(inv, hash)?;
                bytes += data.len() as u64;
                objects.push(json!({"sha256":hash,"data":STANDARD.encode(&data)}));
            }
            match self
                .rpc
                .call(json!({"v":1,"op":"put_many","chunks":objects}))
            {
                Ok(response) => {
                    self.succeeded = true;
                    let accepted = response["accepted"]
                        .as_array()
                        .ok_or_else(|| AppError::new(6, "Missing upload batch receipt"))?;
                    if accepted.len() != batch.len()
                        || accepted
                            .iter()
                            .zip(batch)
                            .any(|(receipt, hash)| receipt["sha256"] != hash.as_str())
                    {
                        return Err(AppError::new(
                            6,
                            "Upload batch receipt does not match requested objects",
                        ));
                    }
                    self.uploaded += bytes;
                    next += count;
                    attempt = 0;
                }
                Err(error) => {
                    let slow = too_slow(&error);
                    if !retry || !self.retry(&error, attempt) {
                        return Err(error);
                    }
                    if slow {
                        halvings = (halvings + 1).min(24);
                    }
                    attempt += 1;
                }
            }
        }
        Ok(())
    }
}
fn preflight(body: u64, limits: &SparkLimits, route: &str) -> Result<()> {
    let limit = limits.request_limit(route);
    if body > limit {
        return Err(AppError::new(
            5,
            format!(
                "The project manifest needs a {:.1} MiB sync request, above the {:.1} MiB {} limit; reduce the file count{}",
                body as f64 / inventory::MIB as f64,
                limit as f64 / inventory::MIB as f64,
                if route == "iroh" {
                    "paired remote route"
                } else {
                    "controller request"
                },
                if route == "iroh" {
                    " or sync over LAN SSH"
                } else {
                    ""
                }
            ),
        ));
    }
    Ok(())
}
/// "Spark is assembling this snapshot: X of Y MiB; ..." carries progress X.
fn assembled_mib(error: &AppError) -> Option<u64> {
    let (_, rest) = error.message.split_once("assembling this snapshot: ")?;
    rest.split(' ').next()?.parse().ok()
}
fn sync(inv: &inventory::Inventory, tr: &mut transport::Transport) -> Result<Value> {
    sync_with(inv, tr, &mut |duration| std::thread::sleep(duration))
}
/// Sync one inventory. A controller that advertises limits is asked to commit
/// first, so an unchanged project takes two RPCs; absent chunks are uploaded in
/// at most three rounds, and a snapshot still assembling is polled. A legacy
/// controller gets today's order: missing, upload, then one sync never retried.
pub fn sync_with(
    inv: &inventory::Inventory,
    rpc: &mut dyn Rpc,
    sleep: &mut dyn FnMut(Duration),
) -> Result<Value> {
    let started = Instant::now();
    let mut session = Session {
        rpc,
        sleep,
        succeeded: false,
        retries: 0,
        uploaded: 0,
    };
    let probe = session.call(&json!({"v":1,"op":"missing","hashes":[]}), false)?;
    let limits = SparkLimits::from_probe(&probe)?;
    limits.check(inv)?;
    let request = json!({"v":1,"op":"sync","project":inv.project,"manifest":inv.manifest});
    let body = serde_json::to_vec(&request)?.len() as u64;
    // The route is known only after the probe.
    preflight(body, &limits, &session.rpc.route())?;
    let order = first_occurrence(inv);
    let (mut rounds, mut polls) = (0usize, 0u64);
    let mut out = if !limits.advertised {
        for group in order.chunks(limits.missing_batch) {
            let absent = session.missing(group, group.len(), true)?;
            session.upload(inv, &absent, &limits, true)?;
        }
        preflight(body, &limits, &session.rpc.route())?;
        session.call(&request, false)?
    } else {
        let (mut stall_mark, mut stall_slept, mut last_done) =
            (Instant::now(), Duration::ZERO, None);
        loop {
            preflight(body, &limits, &session.rpc.route())?;
            let error = match session.call(&request, true) {
                Ok(value) => break value,
                Err(error) => error,
            };
            match remote_code(&error) {
                Some("materializing") => {
                    let done = assembled_mib(&error);
                    if done.is_some() && done > last_done {
                        (stall_mark, stall_slept, last_done) =
                            (Instant::now(), Duration::ZERO, done);
                    } else if stall_mark.elapsed().max(stall_slept) >= STALL_LIMIT {
                        return Err(AppError::new(
                            6,
                            "Spark snapshot assembly made no progress for 10 minutes; run the same command again to continue",
                        ));
                    }
                    polls += 1;
                    (session.sleep)(POLL_DELAY);
                    stall_slept += POLL_DELAY;
                }
                Some("missing_chunks" | "hash_mismatch") if rounds < SYNC_ROUNDS => {
                    rounds += 1;
                    let absent = session.missing(&order, limits.missing_batch, true)?;
                    let bytes = absent
                        .iter()
                        .map(|hash| inv.sources.get(hash).map_or(inventory::MIB, |s| s.len))
                        .sum();
                    // A controller that checks disk only after the upload is caught here.
                    disk_preflight(&probe["storage"], session.uploaded, bytes)?;
                    session.upload(inv, &absent, &limits, true)?;
                    (stall_mark, stall_slept) = (Instant::now(), Duration::ZERO);
                }
                _ => return Err(error),
            }
        }
    };
    if out["manifest_id"] != inv.manifest_id {
        return Err(AppError::new(
            6,
            "Remote manifest digest differs from local canonical digest",
        ));
    }
    out["uploaded_bytes"] = json!(session.uploaded);
    out["sync_seconds"] = json!(started.elapsed().as_secs_f64());
    out["sync_rounds"] = json!(rounds);
    out["materialize_polls"] = json!(polls);
    out["transfer_retries"] = json!(session.retries);
    out["spark_limits"] = limits.view();
    out["spark_storage"] = probe.get("storage").cloned().unwrap_or(Value::Null);
    Ok(out)
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
    fn spark_network_passes_only_a_version_one_object() {
        let report = json!({"v":1,"observed":true,"basis":"default_route","active":{"interface":"wlP9s9","kind":"wifi","operstate":"up","speed_mbps":null},"underlay":null,"wired":[],"wired_up":0,"wired_lan_up":0,"wifi":null,"later_additive_field":true});
        assert_eq!(spark_network(&json!({"ok":true,"network":report})), report);
        let unobserved = json!({"v":1,"observed":false,"basis":"unknown"});
        assert_eq!(spark_network(&json!({"network":unobserved})), unobserved);
        for status in [
            json!({"ok":true}),
            json!({"network":null}),
            json!({"network":{"v":2,"observed":true}}),
            json!({"network":{"v":"1","observed":true}}),
            json!({"network":{"v":1.5}}),
            json!({"network":{"observed":true}}),
            json!({"network":[{"v":1}]}),
            json!({"network":"wifi"}),
            json!([]),
        ] {
            assert_eq!(spark_network(&status), Value::Null, "{status}");
        }
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
#[cfg(test)]
mod sync_tests {
    use super::*;
    use inventory::{CHUNK, GIB, Inventory, MIB};
    use std::collections::{BTreeMap, BTreeSet, VecDeque};

    fn advertised() -> Value {
        json!({"chunk":MIB,"max_file":16*GIB,"max_tree":32*GIB,"max_small_tree":8*GIB,"large_file":512*MIB,"max_files":100000,"max_request":32*MIB,"put_many_body":12*MIB,"missing_batch":10000})
    }
    /// A controller double: content-addressed objects, a commit that needs every
    /// chunk, and scripted failures.
    struct Fake {
        limits: Option<Value>,
        objects: BTreeSet<String>,
        calls: Vec<Value>,
        route: String,
        enabled: bool,
        /// Each entry fails the next request with that op.
        inject: VecDeque<(&'static str, AppError)>,
        /// A sync with every chunk present first reports these MiB assembled.
        assembling: VecDeque<u64>,
        /// Unlinked by the next complete sync, which then reports hash_mismatch.
        heal: Vec<String>,
        always_missing: bool,
        route_after_put: Option<&'static str>,
        /// Fails the request with this zero-based call index.
        fail_calls: BTreeMap<usize, AppError>,
        /// The probe's advertised storage block.
        storage: Value,
        /// The owner disconnects once this many requests have been made.
        disable_after: Option<usize>,
    }
    impl Fake {
        fn new(limits: Option<Value>) -> Self {
            Self {
                limits,
                objects: BTreeSet::new(),
                calls: Vec::new(),
                route: "ssh".into(),
                enabled: true,
                inject: VecDeque::new(),
                assembling: VecDeque::new(),
                heal: Vec::new(),
                always_missing: false,
                route_after_put: None,
                fail_calls: BTreeMap::new(),
                storage: json!({"free_bytes":100*GIB,"reserve_bytes":64*GIB}),
                disable_after: None,
            }
        }
        fn holding(mut self, inv: &Inventory) -> Self {
            self.objects = inv.sources.keys().cloned().collect();
            self
        }
        fn ops(&self) -> Vec<&str> {
            self.calls
                .iter()
                .map(|c| c["op"].as_str().unwrap())
                .collect()
        }
        fn batches(&self) -> Vec<(usize, usize)> {
            self.calls
                .iter()
                .filter(|c| c["op"] == "put_many")
                .map(|c| {
                    (
                        c["chunks"].as_array().unwrap().len(),
                        serde_json::to_vec(c).unwrap().len(),
                    )
                })
                .collect()
        }
        fn uploaded(&self) -> Vec<String> {
            self.calls
                .iter()
                .filter(|c| c["op"] == "put_many")
                .flat_map(|c| c["chunks"].as_array().unwrap().clone())
                .map(|c| c["sha256"].as_str().unwrap().to_string())
                .collect()
        }
    }
    impl Rpc for Fake {
        fn call(&mut self, request: Value) -> Result<Value> {
            self.calls.push(request.clone());
            if self.disable_after == Some(self.calls.len()) {
                self.enabled = false;
            }
            let op = request["op"].as_str().unwrap();
            if let Some(error) = self.fail_calls.remove(&(self.calls.len() - 1)) {
                return Err(error);
            }
            if self.inject.front().is_some_and(|(o, _)| *o == op) {
                return Err(self.inject.pop_front().unwrap().1);
            }
            match op {
                "missing" => {
                    let missing: Vec<_> = request["hashes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|h| !self.objects.contains(h.as_str().unwrap()))
                        .cloned()
                        .collect();
                    let mut out = json!({"ok":true,"missing":missing});
                    if let Some(limits) = &self.limits {
                        out["limits"] = limits.clone();
                        out["storage"] = self.storage.clone();
                    }
                    Ok(out)
                }
                "put_many" => {
                    let mut accepted = Vec::new();
                    for chunk in request["chunks"].as_array().unwrap() {
                        let data = STANDARD.decode(chunk["data"].as_str().unwrap()).unwrap();
                        assert_eq!(inventory::hash(&data), chunk["sha256"]);
                        assert!(data.len() <= CHUNK);
                        self.objects
                            .insert(chunk["sha256"].as_str().unwrap().into());
                        accepted.push(json!({"sha256":chunk["sha256"],"size":data.len()}));
                    }
                    if let Some(route) = self.route_after_put.take() {
                        self.route = route.into();
                    }
                    Ok(json!({"ok":true,"accepted":accepted}))
                }
                "sync" => {
                    let manifest = &request["manifest"];
                    let complete = manifest.as_array().unwrap().iter().all(|e| {
                        e["chunks"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .all(|c| self.objects.contains(c["sha256"].as_str().unwrap()))
                    });
                    if self.always_missing || !complete {
                        return Err(AppError::new(
                            6,
                            "missing_chunks: Upload missing chunks before committing the snapshot",
                        ));
                    }
                    if !self.heal.is_empty() {
                        for hash in self.heal.drain(..) {
                            self.objects.remove(&hash);
                        }
                        return Err(AppError::new(
                            6,
                            "hash_mismatch: Stored chunk failed verification; it was removed for upload again",
                        ));
                    }
                    if let Some(done) = self.assembling.pop_front() {
                        return Err(AppError::new(
                            6,
                            format!(
                                "materializing: Spark is assembling this snapshot: {done} of 900 MiB; run the same command again to continue"
                            ),
                        ));
                    }
                    let id = inventory::hash(&serde_json::to_vec(manifest).unwrap());
                    Ok(
                        json!({"ok":true,"project":request["project"],"manifest_id":id,"files":manifest.as_array().unwrap().len()}),
                    )
                }
                other => panic!("unexpected {other}"),
            }
        }
        fn route(&self) -> String {
            self.route.clone()
        }
        fn connection_enabled(&self) -> bool {
            self.enabled
        }
    }
    struct Tree {
        _source: tempfile::TempDir,
        cas: tempfile::TempDir,
        inv: Inventory,
    }
    fn tree(files: &[(&str, Vec<u8>)], large_file: u64) -> Tree {
        let source = tempfile::tempdir().unwrap();
        let cas = tempfile::tempdir().unwrap();
        for (name, bytes) in files {
            fs::write(source.path().join(name), bytes).unwrap();
        }
        let limits = inventory::Limits {
            large_file,
            available_space: |_| Some(u64::MAX),
            ..inventory::Limits::default()
        };
        let inv = inventory::collect_with(source.path(), Some(cas.path()), limits).unwrap();
        Tree {
            _source: source,
            cas,
            inv,
        }
    }
    /// `count` distinct full chunks.
    fn chunks(count: usize) -> Vec<u8> {
        (0..count)
            .flat_map(|i| vec![b'a' + i as u8; CHUNK])
            .collect()
    }
    fn sizes_only(sizes: &[u64]) -> Inventory {
        let manifest: Vec<_> = sizes
            .iter()
            .enumerate()
            .map(|(i, size)| inventory::Entry {
                path: format!("f{i}.bin"),
                size: *size,
                sha256: inventory::hash(format!("{i}").as_bytes()),
                mode: 0o644,
                chunks: Vec::new(),
            })
            .collect();
        Inventory {
            root: PathBuf::from("."),
            project: "b".repeat(64),
            manifest_id: inventory::hash(
                &serde_json::to_vec(&serde_json::to_value(&manifest).unwrap()).unwrap(),
            ),
            bytes: sizes.iter().sum(),
            small_bytes: 0,
            manifest,
            sources: BTreeMap::new(),
            cas: None,
        }
    }
    fn run(inv: &Inventory, fake: &mut Fake) -> (Result<Value>, Vec<Duration>) {
        let mut sleeps = Vec::new();
        let result = sync_with(inv, fake, &mut |d| sleeps.push(d));
        (result, sleeps)
    }
    fn secs(values: &[u64]) -> Vec<Duration> {
        values.iter().map(|s| Duration::from_secs(*s)).collect()
    }
    fn transient(message: &str) -> AppError {
        AppError::new(4, message)
    }
    const TIMEOUT: &str =
        "Transport timeout; request may have completed remotely. Retry using the same request ID.";
    #[test]
    fn legacy_controller_refuses_large_projects_with_zero_uploads() {
        for sizes in [vec![inventory::LEGACY_MAX_FILE + 1], vec![500 * MIB; 17]] {
            let inv = sizes_only(&sizes);
            let mut fake = Fake::new(None);
            let error = run(&inv, &mut fake).0.unwrap_err();
            assert_eq!(error.code, 5);
            assert!(
                error.message.contains("update Spark setup"),
                "{}",
                error.message
            );
            assert_eq!(fake.ops(), ["missing"]);
        }
        // Exactly the legacy limits are still accepted and synced the old way.
        let inv = sizes_only(&[inventory::LEGACY_MAX_FILE; 16]);
        let mut fake = Fake::new(None);
        let out = run(&inv, &mut fake).0.unwrap();
        assert_eq!(out["spark_limits"]["source"], "legacy");
        assert_eq!(fake.ops(), ["missing", "sync"]);
    }
    #[test]
    fn advertised_limits_bound_what_is_sent_and_chunk_size_must_match() {
        let mut small = advertised();
        small["max_file"] = json!(GIB);
        let inv = sizes_only(&[2 * GIB]);
        let mut fake = Fake::new(Some(small));
        let error = run(&inv, &mut fake).0.unwrap_err();
        assert_eq!(error.code, 5);
        assert!(
            error.message.contains("1 GiB file") && error.message.contains("update Spark setup")
        );
        assert_eq!(fake.ops(), ["missing"]);
        let mut odd = advertised();
        odd["chunk"] = json!(2 * MIB);
        let mut fake = Fake::new(Some(odd));
        assert_eq!(run(&sizes_only(&[1]), &mut fake).0.unwrap_err().code, 6);
        assert_eq!(fake.ops(), ["missing"]);
        let mut fake = Fake::new(Some(json!({"chunk":MIB})));
        assert_eq!(run(&sizes_only(&[1]), &mut fake).0.unwrap_err().code, 6);
    }
    #[test]
    fn an_unchanged_project_takes_two_rpcs() {
        let t = tree(
            &[("main.rs", b"fn main() {}\n".to_vec())],
            inventory::LARGE_FILE,
        );
        let mut fake = Fake::new(Some(advertised())).holding(&t.inv);
        let out = run(&t.inv, &mut fake).0.unwrap();
        assert_eq!(fake.ops(), ["missing", "sync"]);
        assert_eq!(fake.calls[0]["hashes"], json!([]));
        assert_eq!(out["uploaded_bytes"], 0);
        assert_eq!(out["spark_limits"]["source"], "advertised");
        assert_eq!(out["spark_storage"]["reserve_bytes"], 64 * GIB);
    }
    #[test]
    fn missing_chunks_upload_in_bounded_rounds_and_first_occurrence_order() {
        let t = tree(
            &[
                ("a.txt", b"zzzz first".to_vec()),
                ("b.txt", b"aaaa second".to_vec()),
                ("c.txt", b"zzzz first".to_vec()),
            ],
            inventory::LARGE_FILE,
        );
        let mut fake = Fake::new(Some(advertised()));
        let out = run(&t.inv, &mut fake).0.unwrap();
        assert_eq!(
            fake.ops(),
            ["missing", "sync", "missing", "put_many", "sync"]
        );
        let order = vec![
            inventory::hash(b"zzzz first"),
            inventory::hash(b"aaaa second"),
        ];
        assert_eq!(fake.calls[2]["hashes"], json!(order));
        assert_eq!(fake.uploaded(), order);
        assert_eq!(
            (out["sync_rounds"].as_u64(), out["uploaded_bytes"].as_u64()),
            (Some(1), Some(21))
        );
        // Self-heal unlinks an object between missing and sync: one more round.
        let mut fake = Fake::new(Some(advertised()));
        fake.heal = vec![order[1].clone()];
        let out = run(&t.inv, &mut fake).0.unwrap();
        assert_eq!(out["sync_rounds"], 2);
        assert_eq!(
            fake.uploaded(),
            [order[0].clone(), order[1].clone(), order[1].clone()]
        );
        // Never more than three rounds.
        let mut fake = Fake::new(Some(advertised()));
        fake.always_missing = true;
        let error = run(&t.inv, &mut fake).0.unwrap_err();
        assert!(error.message.starts_with("missing_chunks:"));
        assert_eq!(fake.ops().iter().filter(|op| **op == "sync").count(), 4);
    }
    #[test]
    fn a_snapshot_being_assembled_is_polled_until_done_or_stalled() {
        let t = tree(
            &[("main.rs", b"fn main() {}\n".to_vec())],
            inventory::LARGE_FILE,
        );
        let mut fake = Fake::new(Some(advertised())).holding(&t.inv);
        fake.assembling = VecDeque::from([0, 10, 10, 30]);
        let (out, sleeps) = run(&t.inv, &mut fake);
        assert_eq!(out.unwrap()["materialize_polls"], 4);
        assert_eq!(sleeps, secs(&[2, 2, 2, 2]));
        let mut fake = Fake::new(Some(advertised())).holding(&t.inv);
        fake.assembling = std::iter::repeat_n(5, 1000).collect();
        let (out, sleeps) = run(&t.inv, &mut fake);
        let error = out.unwrap_err();
        assert_eq!(error.code, 6);
        assert!(
            error.message.contains("no progress for 10 minutes")
                && error.message.contains("run the same command again")
        );
        assert_eq!(sleeps.iter().sum::<Duration>(), Duration::from_secs(600));
    }
    #[test]
    fn retries_are_bounded_and_never_override_owner_intent() {
        let t = tree(
            &[("main.rs", b"fn main() {}\n".to_vec())],
            inventory::LARGE_FILE,
        );
        // Nothing has succeeded yet: the probe is never retried.
        let mut fake = Fake::new(Some(advertised())).holding(&t.inv);
        fake.inject.push_back(("missing", transient(TIMEOUT)));
        let (out, sleeps) = run(&t.inv, &mut fake);
        assert_eq!((out.unwrap_err().code, sleeps.len()), (4, 0));
        // After one success an ambiguous failure is retried with backoff.
        let mut fake = Fake::new(Some(advertised())).holding(&t.inv);
        fake.inject.push_back(("sync", transient(TIMEOUT)));
        let (out, sleeps) = run(&t.inv, &mut fake);
        assert_eq!(out.unwrap()["transfer_retries"], 1);
        assert_eq!(sleeps, secs(&[2]));
        let unreachable = "Configured LAN addresses are unreachable";
        for _ in 0..5 {
            fake.inject.push_back(("sync", transient(unreachable)));
        }
        let (out, sleeps) = run(&t.inv, &mut fake);
        assert!(out.is_ok());
        assert_eq!(sleeps, secs(&[2, 4, 8, 16, 30]));
        for _ in 0..6 {
            fake.inject.push_back(("sync", transient(unreachable)));
        }
        let (out, sleeps) = run(&t.inv, &mut fake);
        assert_eq!(
            (out.unwrap_err().code, sleeps),
            (4, secs(&[2, 4, 8, 16, 30]))
        );
        // Owner intent, authentication and controller rejections are final.
        for error in [
            transient("Bridge is disconnected; use sparkdev connect to enable new requests"),
            transient("LAN-only mode does not permit the paired remote transport"),
            AppError::new(
                7,
                "SSH authentication or pinned host-key verification failed; fallback refused",
            ),
            AppError::new(6, "controller_error: Controller operation failed"),
        ] {
            let code = error.code;
            let mut fake = Fake::new(Some(advertised())).holding(&t.inv);
            fake.inject.push_back(("sync", error));
            let (out, sleeps) = run(&t.inv, &mut fake);
            assert_eq!((out.unwrap_err().code, sleeps.len()), (code, 0));
        }
        // The owner disconnected during the sync: saved state is read before retrying.
        let mut fake = Fake::new(Some(advertised())).holding(&t.inv);
        fake.enabled = false;
        fake.inject.push_back(("sync", transient(TIMEOUT)));
        let (out, sleeps) = run(&t.inv, &mut fake);
        assert_eq!((out.unwrap_err().code, sleeps.len()), (4, 0));
        // A legacy sync is never retried, though its missing queries and uploads may be.
        let mut fake = Fake::new(None);
        fake.fail_calls = BTreeMap::from([(1, transient(TIMEOUT)), (4, transient(TIMEOUT))]);
        let (out, sleeps) = run(&t.inv, &mut fake);
        assert_eq!((out.unwrap_err().code, sleeps), (4, secs(&[2])));
        assert_eq!(
            fake.ops(),
            ["missing", "missing", "missing", "put_many", "sync"]
        );
    }
    #[test]
    fn upload_batches_follow_the_route_halve_on_timeout_and_switch_routes() {
        let t = tree(&[("data.bin", chunks(8))], inventory::LARGE_FILE);
        let ssh_cap = 8 * MIB as usize - 256;
        let iroh_cap = 3 * MIB as usize - 256;
        let counts = |fake: &Fake| fake.batches().iter().map(|b| b.0).collect::<Vec<_>>();
        let mut fake = Fake::new(Some(advertised()));
        run(&t.inv, &mut fake).0.unwrap();
        assert_eq!(counts(&fake), [5, 3]);
        assert!(fake.batches().iter().all(|b| b.1 <= ssh_cap));
        let mut fake = Fake::new(Some(advertised()));
        fake.route = "iroh".into();
        run(&t.inv, &mut fake).0.unwrap();
        assert_eq!(counts(&fake), [2, 2, 2, 2]);
        assert!(fake.batches().iter().all(|b| b.1 <= iroh_cap));
        // A legacy controller gets three-MiB bodies even over SSH.
        let mut fake = Fake::new(None);
        run(&t.inv, &mut fake).0.unwrap();
        assert_eq!(counts(&fake), [2, 2, 2, 2]);
        // An advertised body smaller than the SSH cap is honored.
        let mut narrow = advertised();
        narrow["put_many_body"] = json!(5 * MIB);
        let mut fake = Fake::new(Some(narrow));
        run(&t.inv, &mut fake).0.unwrap();
        assert_eq!(counts(&fake), [3, 3, 2]);
        assert!(fake.batches().iter().all(|b| b.1 <= 5 * MIB as usize - 256));
        // A timeout halves the body for the rest of the sync.
        let mut fake = Fake::new(Some(advertised()));
        fake.inject.push_back(("put_many", transient(TIMEOUT)));
        let (out, sleeps) = run(&t.inv, &mut fake);
        out.unwrap();
        assert_eq!(counts(&fake), [5, 2, 2, 2, 2]);
        assert_eq!(sleeps, secs(&[2]));
        // Falling back to the paired route mid-sync shrinks the next batches.
        let mut fake = Fake::new(Some(advertised()));
        fake.route_after_put = Some("iroh");
        run(&t.inv, &mut fake).0.unwrap();
        assert_eq!(counts(&fake), [5, 2, 1]);
        assert!(fake.batches()[1..].iter().all(|b| b.1 <= iroh_cap));
    }
    #[test]
    fn chunks_missing_from_the_store_are_read_from_the_source_at_their_offset() {
        let mut bytes = chunks(1);
        bytes.extend_from_slice(b"tail of the file");
        let mut large = vec![b'x'; 2 * CHUNK];
        large.extend_from_slice(b"not staged at all");
        let t = tree(
            &[("two.bin", bytes.clone()), ("large.bin", large.clone())],
            2 * MIB,
        );
        let second = inventory::hash(&bytes[CHUNK..]);
        let unstaged = inventory::hash(&large[2 * CHUNK..]);
        assert!(!t.cas.path().join(&unstaged).exists());
        assert_eq!(t.inv.sources[&unstaged].offset, 2 * MIB);
        fs::remove_file(t.cas.path().join(&second)).unwrap();
        let mut fake = Fake::new(Some(advertised()));
        run(&t.inv, &mut fake).0.unwrap();
        assert!(fake.objects.contains(&second) && fake.objects.contains(&unstaged));
        // A source changed after inventory never uploads different bytes.
        let mut changed = bytes;
        let end = changed.len();
        changed[end - 1] = b'!';
        fs::write(t.inv.root.join("two.bin"), &changed).unwrap();
        let mut fake = Fake::new(Some(advertised()));
        let error = run(&t.inv, &mut fake).0.unwrap_err();
        assert_eq!(
            (error.code, error.message.as_str()),
            (3, "Source changed during sync; rerun")
        );
        // A damaged staged object falls back to the source, which still matches.
        let first = inventory::hash(&changed[..CHUNK]);
        fs::write(t.cas.path().join(&first), b"damaged").unwrap();
        let mut fake = Fake::new(Some(advertised()));
        fake.objects.insert(second.clone());
        fake.objects
            .extend([inventory::hash(&large[..CHUNK]), unstaged]);
        run(&t.inv, &mut fake).0.unwrap();
        assert_eq!(fake.uploaded(), [first]);
    }
    #[test]
    fn the_sync_request_is_preflighted_for_the_route_after_the_probe() {
        let inv = sizes_only(&vec![0; 40_000]);
        let body = serde_json::to_vec(
            &json!({"v":1,"op":"sync","project":inv.project,"manifest":inv.manifest}),
        )
        .unwrap()
        .len() as u64;
        assert!(body > IROH_REQUEST_LIMIT && body < 32 * MIB);
        let mut fake = Fake::new(Some(advertised()));
        fake.route = "iroh".into();
        let (out, sleeps) = run(&inv, &mut fake);
        let error = out.unwrap_err();
        assert_eq!(error.code, 5);
        assert!(error.message.contains("paired remote route"));
        assert_eq!((fake.ops(), sleeps.len()), (vec!["missing"], 0));
        let mut tiny = advertised();
        tiny["max_request"] = json!(1000);
        let mut fake = Fake::new(Some(tiny));
        assert_eq!(run(&inv, &mut fake).0.unwrap_err().code, 5);
        assert_eq!(fake.ops(), ["missing"]);
        let mut fake = Fake::new(Some(advertised()));
        assert!(run(&inv, &mut fake).0.is_ok());
        // A fallback to the paired route before the commit is refused, never retried.
        let t = tree(&[("big.bin", chunks(1))], inventory::LARGE_FILE);
        let mut many: Vec<_> = (0..40_000)
            .map(|i| inventory::Entry {
                path: format!("f{i:05}.txt"),
                size: 0,
                sha256: inventory::hash(b""),
                mode: 0o644,
                chunks: Vec::new(),
            })
            .collect();
        many.extend(t.inv.manifest.iter().cloned());
        let inv = Inventory {
            manifest_id: inventory::hash(
                &serde_json::to_vec(&serde_json::to_value(&many).unwrap()).unwrap(),
            ),
            manifest: many,
            root: t.inv.root.clone(),
            project: t.inv.project.clone(),
            bytes: t.inv.bytes,
            small_bytes: t.inv.small_bytes,
            sources: t.inv.sources.clone(),
            cas: t.inv.cas.clone(),
        };
        let mut fake = Fake::new(Some(advertised()));
        fake.route_after_put = Some("iroh");
        let (out, sleeps) = run(&inv, &mut fake);
        assert_eq!((out.unwrap_err().code, sleeps.len()), (5, 0));
        assert_eq!(fake.ops(), ["missing", "sync", "missing", "put_many"]);
    }
    #[test]
    fn a_paired_route_frame_cut_off_by_the_spark_halves_the_batch() {
        let t = tree(&[("data.bin", chunks(8))], inventory::LARGE_FILE);
        let counts = |fake: &Fake| fake.batches().iter().map(|b| b.0).collect::<Vec<_>>();
        // The Spark daemon's 10 s frame deadline surfaces as a sidecar failure, not a timeout.
        let sidecar = "Authenticated remote sidecar unavailable, busy or not paired; retry the same request ID";
        let mut fake = Fake::new(Some(advertised()));
        fake.route = "iroh".into();
        fake.inject.push_back(("put_many", transient(sidecar)));
        let (out, sleeps) = run(&t.inv, &mut fake);
        out.unwrap();
        assert_eq!(counts(&fake), [2, 1, 1, 1, 1, 1, 1, 1, 1]);
        assert_eq!(sleeps, secs(&[2]));
        // A busy link is not a slow one: the batch keeps its size.
        let mut fake = Fake::new(Some(advertised()));
        fake.route = "iroh".into();
        fake.inject
            .push_back(("put_many", transient("Authenticated remote link is busy")));
        run(&t.inv, &mut fake).0.unwrap();
        assert_eq!(counts(&fake), [2, 2, 2, 2, 2]);
    }
    #[test]
    fn an_owner_disconnect_stops_polling_uploads_and_further_requests() {
        let t = tree(
            &[("main.rs", b"fn main() {}\n".to_vec())],
            inventory::LARGE_FILE,
        );
        // Disconnected after the first poll: no further sync is sent.
        let mut fake = Fake::new(Some(advertised())).holding(&t.inv);
        fake.assembling = VecDeque::from([5, 10, 15]);
        fake.disable_after = Some(2);
        let (out, sleeps) = run(&t.inv, &mut fake);
        let error = out.unwrap_err();
        assert_eq!(
            (error.code, error.message.as_str(), sleeps),
            (
                4,
                "Bridge is disconnected; use sparkdev connect to enable new requests",
                secs(&[2])
            )
        );
        assert_eq!(fake.ops(), ["missing", "sync"]);
        // Disconnected after the first upload batch: the next batch is never sent.
        let big = tree(&[("data.bin", chunks(8))], inventory::LARGE_FILE);
        let mut fake = Fake::new(Some(advertised()));
        fake.disable_after = Some(4);
        let error = run(&big.inv, &mut fake).0.unwrap_err();
        assert_eq!(error.code, 4);
        assert_eq!(fake.ops(), ["missing", "sync", "missing", "put_many"]);
    }
    #[test]
    fn spark_disk_is_checked_before_an_upload_round() {
        let t = tree(&[("data.bin", chunks(3))], inventory::LARGE_FILE);
        // 3 MiB absent needs 6 MiB above the reserve: objects, then the assembled copy.
        let mut fake = Fake::new(Some(advertised()));
        fake.storage = json!({"free_bytes":64*GIB + 5*MIB,"reserve_bytes":64*GIB});
        let error = run(&t.inv, &mut fake).0.unwrap_err();
        assert_eq!(error.code, 6);
        assert!(
            error
                .message
                .starts_with("insufficient_disk: Spark has 64.0 GiB free")
                && error.message.contains("keeps 64 GiB in reserve"),
            "{}",
            error.message
        );
        assert_eq!(fake.ops(), ["missing", "sync", "missing"]);
        let mut fake = Fake::new(Some(advertised()));
        fake.storage = json!({"free_bytes":64*GIB + 6*MIB,"reserve_bytes":64*GIB});
        run(&t.inv, &mut fake).0.unwrap();
        // No storage block (an older controller): nothing to compare.
        let mut fake = Fake::new(Some(advertised()));
        fake.storage = Value::Null;
        run(&t.inv, &mut fake).0.unwrap();
    }
}
