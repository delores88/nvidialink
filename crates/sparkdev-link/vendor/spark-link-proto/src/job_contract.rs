//! The compute-job admission contract of a Spark Link node, in one dependency-light file.
//!
//! This is the single source of truth for what a node admits as a compute job and how long it
//! keeps it: the closed runtime set and the capability tokens advertised for it, the request
//! bounds per runtime, each runtime's argument schema, the retention limits and pruning order,
//! the expiry of unclaimed jobs, and the node API error code each job error is reported with.
//! `spark-link-node` admits jobs with exactly this code. PhaseForge's test mock compiles this
//! same file by path (`backend/src/compute/spark_mock.rs`), so the mock cannot admit a request
//! the node refuses, or refuse one the node admits.
//!
//! Execution details of each runtime (image digest per CPU architecture, fixed entry point,
//! environment, probe, GPU-memory watchdog) are the node's registry
//! (`spark-link-node/src/runtimes.rs`), which names exactly the tokens in [`RUNTIMES`].
//!
//! Keep it self-contained: `std`, `serde` and `serde_json` only, no `crate::` paths, and code
//! that builds under both Rust 2021 (PhaseForge backend) and Rust 2024 (this workspace).
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Generic capability: the node runs compute jobs at all.
pub const JOBS: &str = "jobs";
/// The 0.25 transfer and event protocol (`spark_link_proto::job_transfer`): per-chunk hashes,
/// resumable content-addressed uploads, chunk-aligned output reads with chunk hashes, an output
/// manifest, a job event log, and expiry of unclaimed jobs. Every node with this code
/// advertises it next to `jobs`; clients use it only when the whole path (agent and node) has it.
pub const JOBS_V2: &str = "jobs.v2";
/// Runtime token: model-authored Python in the fixed, digest-pinned NVIDIA PyTorch image.
pub const PYTHON_ML: &str = "python_ml";
/// Runtime token: Blender 4.5.9 (Cycles CUDA/OptiX, OIDN) in PhaseForge's digest-pinned image,
/// the version PhaseForge pins on Windows, so a scene authored there opens identically here.
pub const BLENDER: &str = "blender@4.5.9";

/// Admission bounds of one closed runtime. A request's `workload` names the runtime token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeBounds {
    pub token: &'static str,
    /// The `runner` value its arguments must carry.
    pub runner: &'static str,
    pub max_timeout_s: u64,
    /// Aggregate declared input bytes.
    pub input_limit: u64,
    /// Aggregate verified output bytes, logs included.
    pub output_limit: u64,
    pub output_file_limit: u64,
    /// (min, max, default) of the container's memory cgroup, MiB.
    pub memory_mb: (u64, u64, u64),
    /// (min, max, default) of `--pids-limit`, which counts threads.
    pub process_limit: (u32, u32, u32),
    /// (min, max, default) of the private `/work` tmpfs, MiB.
    pub storage_mb: (u64, u64, u64),
    /// (min, max) of an optional GPU-memory budget, MiB; `None` = not accepted.
    pub gpu_memory_mb: Option<(u64, u64)>,
}

const MIB: u64 = 1024 * 1024;

/// The closed runtime set, in advertisement order. `python_ml` keeps its 0.21 bounds unchanged.
pub const RUNTIMES: [RuntimeBounds; 2] = [
    RuntimeBounds {
        token: PYTHON_ML,
        runner: "python",
        max_timeout_s: 1800,
        input_limit: 256 * MIB,
        output_limit: 256 * MIB,
        output_file_limit: 64 * MIB,
        memory_mb: (128, 16384, 16384),
        process_limit: (16, 128, 128),
        storage_mb: (64, 1024, 1024),
        gpu_memory_mb: None,
    },
    // Sized to the Spark (trace-spark section 5): wall time up to 4 h, memory up to 64 GiB,
    // pids at least 512 (the pids controller counts threads; Cycles, OIDN and the driver start
    // one per core), outputs 512 MiB per file and 4 GiB aggregate (local Studio bounds).
    RuntimeBounds {
        token: BLENDER,
        runner: "blender",
        max_timeout_s: 4 * 3600,
        input_limit: 2048 * MIB,
        output_limit: 4096 * MIB,
        output_file_limit: 512 * MIB,
        memory_mb: (1024, 65536, 16384),
        process_limit: (512, 4096, 1024),
        storage_mb: (256, 16384, 4096),
        gpu_memory_mb: Some((1024, 262144)),
    },
];

/// The admission bounds of a runtime token, if it is one of the closed set.
pub fn runtime(token: &str) -> Option<&'static RuntimeBounds> {
    RUNTIMES.iter().find(|r| r.token == token)
}

/// The capability tokens a node advertises in `GET /link/v1/node`: none unless at least one
/// runtime probed successfully, then `jobs`, `jobs.v2` and one token per ready runtime (in
/// [`RUNTIMES`] order). Clients route a workload to a node only when that workload's runtime
/// token is listed here.
pub fn advertised_capabilities(ready: &[&str]) -> Vec<String> {
    let tokens: Vec<String> = RUNTIMES.iter().filter(|r| ready.contains(&r.token)).map(|r| r.token.to_string()).collect();
    if tokens.is_empty() {
        return Vec::new();
    }
    let mut out = vec![JOBS.to_string(), JOBS_V2.to_string()];
    out.extend(tokens);
    out
}

/// `python_ml` bounds, kept under their 0.21 names.
pub const INPUT_LIMIT: u64 = RUNTIMES[0].input_limit;
pub const OUTPUT_LIMIT: u64 = RUNTIMES[0].output_limit;
pub const OUTPUT_FILE_LIMIT: u64 = RUNTIMES[0].output_file_limit;
pub const MAX_TIMEOUT_S: u64 = RUNTIMES[0].max_timeout_s;
pub const FILE_LIMIT: usize = 32;
pub const CONTROL_LIMIT: usize = 65536;
/// Log files the worker always writes and returns; no input or declared output may use them.
pub const LOG_NAMES: [&str; 2] = ["stdout.txt", "stderr.txt"];

/// Job records a node retains, in any state. Before 0.25 this was a lifetime cap of 32 with no
/// way to delete, so a node stopped admitting work after its 32nd job.
pub const RETAINED_JOB_LIMIT: usize = 256;
/// Accounted bytes (declared inputs plus verified outputs) the retained records may hold. The
/// pre-0.25 worst case was 32 jobs x (256 MiB in + 256 MiB out) = 16 GiB; raising the record
/// limit does not raise this bound.
pub const RETAINED_BYTE_LIMIT: u64 = 16 * 1024 * 1024 * 1024;

/// How long an unclaimed job is kept after its owner last touched it: a finished job that was
/// never acknowledged or deleted, or a staged job that was never started. Configurable on the
/// node (`serve --unclaimed-job-grace-hours`) within [`UNCLAIMED_GRACE_RANGE_S`].
pub const DEFAULT_UNCLAIMED_GRACE_S: u64 = 3 * 24 * 3600;
pub const UNCLAIMED_GRACE_RANGE_S: (u64, u64) = (3600, 90 * 24 * 3600);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    pub device: String,
    pub app: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobFile {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

/// `POST /link/v1/jobs`. Unknown fields are refused.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobCreate {
    /// The runtime token (`python_ml`, `blender@4.5.9`).
    pub workload: String,
    pub args: serde_json::Value,
    pub inputs: Vec<JobFile>,
    pub expected_outputs: Vec<String>,
    pub timeout_s: u64,
    pub app: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for JobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for JobError {}

pub fn err(code: &str, message: impl Into<String>) -> JobError {
    JobError { code: code.into(), message: message.into() }
}

/// `args` of the `python_ml` runtime. Unknown fields are refused, so a caller cannot name an
/// image, host path, network, environment or command.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PythonArgs {
    pub runner: String,
    pub script: String,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub python_flags: Vec<String>,
    #[serde(default)]
    pub requirements: Vec<String>,
    #[serde(default = "job_cwd")]
    pub cwd: String,
    #[serde(default = "stdout_name")]
    pub stdout: String,
    #[serde(default = "stderr_name")]
    pub stderr: String,
    #[serde(default)]
    pub limits: Option<ContainerLimits>,
}

/// `args` of the `blender@4.5.9` runtime. The node runs exactly
/// `blender --background --factory-startup --disable-autoexec --python-exit-code <code>
/// --python <script> -- <argv...>` in the job directory; callers choose no flag, image or path.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlenderArgs {
    pub runner: String,
    pub script: String,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default = "blender_exit_code")]
    pub python_exit_code: u8,
    #[serde(default)]
    pub limits: Option<ContainerLimits>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerLimits {
    pub memory_mb: u64,
    pub process_limit: u32,
    pub storage_mb: u64,
    /// GPU-memory budget the node's watchdog enforces (runtimes that accept one only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_memory_mb: Option<u64>,
}

impl Default for ContainerLimits {
    /// `python_ml`'s 0.21 defaults.
    fn default() -> Self {
        Self::defaults(&RUNTIMES[0])
    }
}

impl ContainerLimits {
    pub fn defaults(bounds: &RuntimeBounds) -> Self {
        Self { memory_mb: bounds.memory_mb.2, process_limit: bounds.process_limit.2, storage_mb: bounds.storage_mb.2, gpu_memory_mb: None }
    }
}

fn job_cwd() -> String {
    "job".into()
}
fn stdout_name() -> String {
    LOG_NAMES[0].into()
}
fn stderr_name() -> String {
    LOG_NAMES[1].into()
}
fn blender_exit_code() -> u8 {
    2
}

/// What a create request runs, after admission.
#[derive(Clone, Debug)]
pub enum RuntimeArgs {
    Python(PythonArgs),
    Blender(BlenderArgs),
}

impl RuntimeArgs {
    pub fn limits(&self) -> Option<&ContainerLimits> {
        match self {
            RuntimeArgs::Python(a) => a.limits.as_ref(),
            RuntimeArgs::Blender(a) => a.limits.as_ref(),
        }
    }
    pub fn script(&self) -> &str {
        match self {
            RuntimeArgs::Python(a) => &a.script,
            RuntimeArgs::Blender(a) => &a.script,
        }
    }
    pub fn argv(&self) -> &[String] {
        match self {
            RuntimeArgs::Python(a) => &a.argv,
            RuntimeArgs::Blender(a) => &a.argv,
        }
    }
}

/// Full admission check for one create request, in the node's order: owner, control size,
/// runtime, counts and timeout, runtime arguments, then input and output names. Whether the
/// runtime is installed and probed on this node is the node's separate check.
pub fn validate(owner: &Owner, r: &JobCreate) -> Result<(), JobError> {
    check_owner_and_size(owner, r)?;
    let bounds = runtime(&r.workload).ok_or_else(|| err("not_implemented", "No such compute runtime is admitted by this node"))?;
    check_counts(r, bounds.max_timeout_s)?;
    runtime_args(r)?;
    check_files(r, bounds.input_limit)
}

/// The authenticated device and app must own the job; the control body is bounded.
pub fn check_owner_and_size(owner: &Owner, r: &JobCreate) -> Result<(), JobError> {
    if owner.device.is_empty() || owner.device.len() > 256 || owner.app.is_empty() || owner.app.len() > 128 || r.app != owner.app {
        return Err(err("invalid_owner", "Authenticated device and app must match job ownership"));
    }
    if serde_json::to_vec(r).map_err(|e| err("invalid_request", e.to_string()))?.len() > CONTROL_LIMIT {
        return Err(err("request_limit", "Job control body exceeds limit"));
    }
    Ok(())
}

/// Timeout, at least one input and one declared output, and the file-count bounds.
pub fn check_counts(r: &JobCreate, max_timeout_s: u64) -> Result<(), JobError> {
    if !(1..=max_timeout_s).contains(&r.timeout_s)
        || r.inputs.is_empty()
        || r.inputs.len() > FILE_LIMIT
        || r.expected_outputs.is_empty()
        || r.expected_outputs.len() > FILE_LIMIT - LOG_NAMES.len()
    {
        return Err(err("request_limit", "Invalid timeout, input or output count"));
    }
    Ok(())
}

/// The argument schema of the request's runtime.
pub fn runtime_args(r: &JobCreate) -> Result<RuntimeArgs, JobError> {
    match r.workload.as_str() {
        PYTHON_ML => python_args(r).map(RuntimeArgs::Python),
        BLENDER => blender_args(r).map(RuntimeArgs::Blender),
        _ => Err(err("not_implemented", "No such compute runtime is admitted by this node")),
    }
}

fn check_argv(argv: &[String]) -> bool {
    argv.len() <= 64 && argv.iter().all(|a| a.len() <= 4096 && !a.contains('\0'))
}

fn check_limits(limits: &Option<ContainerLimits>, bounds: &RuntimeBounds) -> Result<(), JobError> {
    let Some(l) = limits else { return Ok(()) };
    let gpu_ok = match (l.gpu_memory_mb, bounds.gpu_memory_mb) {
        (None, _) => true,
        (Some(v), Some((min, max))) => (min..=max).contains(&v),
        (Some(_), None) => false,
    };
    if !(bounds.memory_mb.0..=bounds.memory_mb.1).contains(&l.memory_mb)
        || !(bounds.process_limit.0..=bounds.process_limit.1).contains(&l.process_limit)
        || !(bounds.storage_mb.0..=bounds.storage_mb.1).contains(&l.storage_mb)
        || !gpu_ok
    {
        return Err(err("invalid_args", "Container limits exceed admitted memory/PID/storage bounds"));
    }
    Ok(())
}

/// The script is one of the declared `.py` inputs, and its name cannot be read as an option: it
/// follows the interpreter in the fixed entry point (`python -I <script>`), where a name such as
/// `-Wx.py` would make Python take it, and the arguments after it, as its own flags.
fn declared_script(r: &JobCreate, script: &str) -> Result<(), JobError> {
    valid_name(script)?;
    if script.starts_with('-') {
        return Err(err("invalid_args", "A script name cannot begin with '-'"));
    }
    if !script.ends_with(".py") || !r.inputs.iter().any(|f| f.name == script) {
        return Err(err("invalid_args", "Script must be a declared Python input"));
    }
    Ok(())
}

/// The `python_ml` argument schema: isolated Python (`-I` only), no requirements, the fixed
/// log names and working directory, bounded argv, bounded container limits, and a script that
/// is one of the declared `.py` inputs. Unchanged from 0.21.
pub fn python_args(r: &JobCreate) -> Result<PythonArgs, JobError> {
    let args: PythonArgs = serde_json::from_value(r.args.clone()).map_err(|_| err("invalid_args", "Unsupported Python job arguments"))?;
    if args.runner != "python"
        || args.cwd != "job"
        || args.stdout != LOG_NAMES[0]
        || args.stderr != LOG_NAMES[1]
        || !args.requirements.is_empty()
        || (!args.python_flags.is_empty() && args.python_flags != ["-I"])
        || !check_argv(&args.argv)
    {
        return Err(err("invalid_args", "Only isolated Python with preinstalled dependencies and bounded argv is supported"));
    }
    valid_name(&args.script)?;
    check_limits(&args.limits, &RUNTIMES[0])?;
    declared_script(r, &args.script)?;
    Ok(args)
}

/// The `blender@4.5.9` argument schema: the `blender` runner, a declared `.py` script, bounded
/// argv, an exit code for Python failures (1..=255), and limits within the runtime's bounds.
pub fn blender_args(r: &JobCreate) -> Result<BlenderArgs, JobError> {
    let args: BlenderArgs = serde_json::from_value(r.args.clone()).map_err(|_| err("invalid_args", "Unsupported Blender job arguments"))?;
    if args.runner != "blender" || args.python_exit_code == 0 || !check_argv(&args.argv) {
        return Err(err("invalid_args", "Only the fixed Blender entry point with bounded argv is supported"));
    }
    valid_name(&args.script)?;
    check_limits(&args.limits, runtime(BLENDER).expect("blender runtime"))?;
    declared_script(r, &args.script)?;
    Ok(args)
}

/// The limits a job runs with: its declared limits, or its runtime's defaults.
pub fn effective_limits(r: &JobCreate) -> Result<ContainerLimits, JobError> {
    let bounds = runtime(&r.workload).ok_or_else(|| err("not_implemented", "No such compute runtime is admitted by this node"))?;
    Ok(runtime_args(r)?.limits().cloned().unwrap_or_else(|| ContainerLimits::defaults(bounds)))
}

/// Input names, hashes and the aggregate input bound; outputs that do not collide with inputs,
/// each other (case-insensitively) or the log names.
pub fn check_files(r: &JobCreate, input_limit: u64) -> Result<(), JobError> {
    let mut names = BTreeSet::new();
    let mut total = 0u64;
    for f in &r.inputs {
        valid_name(&f.name)?;
        total = total.checked_add(f.size).ok_or_else(|| err("request_limit", "Input size overflow"))?;
        if !hex64(&f.sha256) || total > input_limit || !names.insert(f.name.to_ascii_lowercase()) || LOG_NAMES.contains(&f.name.as_str()) {
            return Err(err("invalid_input", "Invalid input size, identity or duplicate name"));
        }
    }
    let mut outputs = BTreeSet::new();
    for name in &r.expected_outputs {
        valid_name(name)?;
        if names.contains(&name.to_ascii_lowercase()) || !outputs.insert(name.to_ascii_lowercase()) || LOG_NAMES.contains(&name.as_str()) {
            return Err(err("invalid_output", "Output conflicts with inputs or logs"));
        }
    }
    Ok(())
}

pub fn hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Node job ids: 32 lowercase hex characters.
pub fn valid_id(s: &str) -> Result<(), JobError> {
    if s.len() == 32 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        Ok(())
    } else {
        Err(err("not_found", "Invalid job id"))
    }
}

/// A bounded relative ASCII name: `[A-Za-z0-9._-]` segments, at most 8, no `.` or `..`.
pub fn valid_name(s: &str) -> Result<(), JobError> {
    let parts: Vec<_> = s.split('/').collect();
    if s.len() > 200
        || parts.len() > 8
        || parts.iter().any(|p| p.is_empty() || *p == "." || *p == ".." || !p.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)))
    {
        return Err(err("invalid_path", "Expected a bounded relative ASCII file name"));
    }
    Ok(())
}

/// The node API error code (`spark_link_proto::errors::ErrorCode`, snake_case) a job error is
/// reported with. The HTTP status follows from that code.
pub fn api_error_code(job_code: &str) -> &'static str {
    match job_code {
        "not_found" => "not_found",
        "forbidden" => "forbidden",
        "capacity" | "busy" => "too_many_requests",
        "not_available" => "upstream_unavailable",
        "invalid_state" | "conflict" | "offset_mismatch" | "cleanup_unconfirmed" => "conflict",
        "request_limit" => "payload_too_large",
        "io_error" | "corrupt_state" => "internal",
        _ => "bad_request",
    }
}

/// A finished job whose container cleanup is confirmed can be acknowledged or deleted. A job
/// that is still staging, queued, running or cancelling must be cancelled first; a record with
/// unconfirmed cleanup is the evidence that blocks admission until recovery, so it stays.
pub fn check_removable(terminal: bool, cleanup_confirmed: bool) -> Result<(), JobError> {
    if !terminal {
        return Err(err("invalid_state", "Only a finished job can be acknowledged or deleted; cancel it first"));
    }
    if !cleanup_confirmed {
        return Err(err("cleanup_unconfirmed", "Container cleanup is unconfirmed; the job record is kept until recovery"));
    }
    Ok(())
}

/// One retained job as the retention and expiry policies see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Retained {
    pub id: String,
    pub terminal: bool,
    pub cleanup_confirmed: bool,
    /// Set once the owner acknowledged that it imported the job's results.
    pub acknowledged_at: Option<u64>,
    /// Least-recently-used key: the acknowledgement time, refreshed when the job is read.
    pub last_used: u64,
    /// Declared input bytes plus verified output bytes.
    pub bytes: u64,
    /// Still staging: created, never started.
    pub staging: bool,
    /// Unix seconds of the owner's last access (create, upload, read) or of the job's finish,
    /// whichever is later. Records from nodes before 0.25 use their record file's time.
    pub idle_since: u64,
}

/// Which acknowledged jobs to prune, least recently used first, so that one more job declaring
/// `incoming_bytes` of inputs fits both retention limits. Only finished, acknowledged jobs with
/// confirmed cleanup are pruned here; unacknowledged results go only by [`expiry_plan`], so a
/// node full of fresh ones refuses with `capacity` until their owners acknowledge or delete them.
pub fn retention_plan(retained: &[Retained], incoming_bytes: u64) -> Result<Vec<String>, JobError> {
    let mut count = retained.len();
    let mut bytes = retained.iter().fold(0u64, |sum, r| sum.saturating_add(r.bytes));
    let mut prunable: Vec<&Retained> = retained.iter().filter(|r| r.terminal && r.cleanup_confirmed && r.acknowledged_at.is_some()).collect();
    prunable.sort_by(|a, b| (a.last_used, &a.id).cmp(&(b.last_used, &b.id)));
    let mut candidates = prunable.into_iter();
    let mut plan = Vec::new();
    while count >= RETAINED_JOB_LIMIT || bytes.saturating_add(incoming_bytes) > RETAINED_BYTE_LIMIT {
        match candidates.next() {
            Some(r) => {
                count -= 1;
                bytes = bytes.saturating_sub(r.bytes);
                plan.push(r.id.clone());
            }
            None => {
                return Err(err(
                    "capacity",
                    "Retained ML job limit reached; acknowledge or delete finished jobs whose results were imported",
                ));
            }
        }
    }
    Ok(plan)
}

/// Why an unclaimed job expired.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpiryReason {
    /// Finished with confirmed cleanup, never acknowledged or deleted.
    Unacknowledged,
    /// Created and never started (an abandoned upload).
    AbandonedStaging,
}

impl ExpiryReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ExpiryReason::Unacknowledged => "unacknowledged",
            ExpiryReason::AbandonedStaging => "abandoned_staging",
        }
    }
}

/// When an unclaimed job expires, or `None` for a job that never expires by this rule
/// (acknowledged, active, or with unconfirmed cleanup, which is recovery evidence).
pub fn expires_at(r: &Retained, grace_s: u64) -> Option<(u64, ExpiryReason)> {
    let reason = if r.staging {
        ExpiryReason::AbandonedStaging
    } else if r.terminal && r.cleanup_confirmed && r.acknowledged_at.is_none() {
        ExpiryReason::Unacknowledged
    } else {
        return None;
    };
    Some((r.idle_since.saturating_add(grace_s), reason))
}

/// The unclaimed jobs whose grace period has passed at `now`, oldest first. Bounded expiry is
/// what keeps a node from filling up with results nobody will ever claim; acknowledged jobs are
/// pruned by [`retention_plan`] instead.
pub fn expiry_plan(retained: &[Retained], now: u64, grace_s: u64) -> Vec<(String, ExpiryReason)> {
    let mut due: Vec<(u64, &str, ExpiryReason)> =
        retained.iter().filter_map(|r| expires_at(r, grace_s).filter(|(at, _)| *at <= now).map(|(at, why)| (at, r.id.as_str(), why))).collect();
    due.sort();
    due.into_iter().map(|(_, id, why)| (id.to_string(), why)).collect()
}
