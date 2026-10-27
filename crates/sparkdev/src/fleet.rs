//! Device dispatch above the retained single-profile CLI.
//!
//! Each child has its own SPARKDEV_HOME and return worker. One owner-local
//! journal atomically binds request identity, device identity and project claims.
use crate::{
    AppError, Cli, Commands, NodeCommands, Placement, Result, Switch, inventory, location, nodes,
    state, transport,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

const MAX_JOURNAL: u64 = 16 * inventory::MIB;
const MAX_RECORDS: usize = 10_000;
// All devices share one parallel observation wave, inside the desktop's
// existing 40-second CLI budget even when the legacy Spark is unreachable.
const OBSERVATION_TIMEOUT: Duration = Duration::from_secs(32);
const SUBMISSION_TIMEOUT: Duration = Duration::from_secs(1200);

#[derive(Clone, Debug)]
struct Target {
    id: String,
    label: String,
    root: PathBuf,
    node: Option<nodes::Node>,
}

impl Target {
    fn load(id: &str) -> Result<Self> {
        match id {
            "spark" | "laptop" => Ok(Self {
                id: id.into(),
                label: if id == "spark" {
                    "Spark"
                } else {
                    "This laptop"
                }
                .into(),
                root: fs::canonicalize(state::root()?)?,
                node: None,
            }),
            _ => {
                let node = nodes::get(id)?;
                Ok(Self {
                    id: id.into(),
                    label: node.label.clone(),
                    root: fs::canonicalize(nodes::profile_root(id)?)?,
                    node: Some(node),
                })
            }
        }
    }

    fn enabled(&self) -> bool {
        self.node.as_ref().is_none_or(|node| node.enabled)
    }

    fn identity(&self) -> Result<String> {
        let config = bounded_file(&self.root.join("config.json"), inventory::MIB)?
            .ok_or_else(|| AppError::new(3, "The selected device has no SSH configuration"))?;
        let config: state::Config = serde_json::from_slice(&config)?;
        state::validate_config(&config)?;
        let endpoint = match bounded_file(&self.root.join("endpoint.json"), 16 * 1024)? {
            Some(bytes) => serde_json::from_slice::<nodes::EndpointSettings>(&bytes)?,
            None => nodes::EndpointSettings::default(),
        };
        nodes::validate_endpoint(&endpoint)?;
        let ssh_config_sha256 = endpoint
            .ssh_config
            .as_ref()
            .map(|path| {
                bounded_file(path, 128 * 1024)?
                    .map(|bytes| inventory::hash(&bytes))
                    .ok_or_else(|| {
                        AppError::new(3, "The device SSH configuration reference is missing")
                    })
            })
            .transpose()?;
        // Reachability and owner enable/disable settings are intentionally not
        // identity: a LAN address may change without moving a retained job.
        Ok(inventory::hash(&serde_json::to_vec(&json!({
            "ssh_alias":config.ssh_alias,"host_key_alias":config.host_key_alias,
            "identity_file":config.identity_file,"user":config.user,"port":config.port,
            "endpoint":endpoint,"ssh_config_sha256":ssh_config_sha256,
            "local_build_gate":if self.id == "laptop" {config.local_build_gate} else {None}
        }))?))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Submission {
    fingerprint: String,
    device: String,
    profile_root: PathBuf,
    target_identity: String,
    project: Option<String>,
    manifest_id: Option<String>,
    job: Option<String>,
    complete: bool,
    rejected: bool,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    v: u32,
    requests: BTreeMap<String, Submission>,
    claims: BTreeMap<String, String>,
}

fn error(message: &str) -> AppError {
    AppError::new(2, message)
}

fn reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn bounded_file(path: &Path, maximum: u64) -> Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || reparse(&metadata) || metadata.len() > maximum {
        return Err(AppError::new(
            3,
            "Fleet state must be a bounded ordinary file",
        ));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000);
    }
    let file = options.open(path)?;
    let actual = file.metadata()?;
    if !actual.is_file() || reparse(&actual) || actual.len() > maximum {
        return Err(AppError::new(3, "Fleet state file type or size changed"));
    }
    let mut bytes = Vec::new();
    file.take(maximum + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err(error("Fleet state exceeds its size limit"));
    }
    Ok(Some(bytes))
}

fn journal() -> Result<Journal> {
    let Some(bytes) = bounded_file(&state::root()?.join("fleet.json"), MAX_JOURNAL)? else {
        return Ok(Journal {
            v: 1,
            ..Default::default()
        });
    };
    let value: Journal = serde_json::from_slice(&bytes)?;
    if value.v != 1 || value.requests.len() > MAX_RECORDS || value.claims.len() > MAX_RECORDS {
        return Err(error("Unsupported or oversized fleet journal"));
    }
    for (id, request) in &value.requests {
        state::validate_id(id)?;
        if request.device != "spark" && request.device != "laptop" {
            nodes::validate_id(&request.device)?;
        }
        if !request.profile_root.is_absolute()
            || request.fingerprint.len() != 64
            || request.target_identity.len() != 64
        {
            return Err(error("Fleet journal contains an invalid retained identity"));
        }
        if let Some(job) = &request.job {
            state::validate_id(job)?;
        }
    }
    for (project, id) in &value.claims {
        if value
            .requests
            .get(id)
            .is_none_or(|request| request.project.as_ref() != Some(project) || request.complete)
        {
            return Err(error(
                "Fleet project claim differs from its retained request",
            ));
        }
    }
    Ok(value)
}

fn change_journal<T>(change: impl FnOnce(&mut Journal) -> Result<T>) -> Result<T> {
    let root = state::root()?;
    let info = fs::symlink_metadata(&root)?;
    if !root.is_absolute() || !info.is_dir() || reparse(&info) {
        return Err(error("Fleet owner-state directory is unsafe"));
    }
    let path = root.join("fleet.lock");
    if let Ok(info) = fs::symlink_metadata(&path) {
        if !info.is_file() || reparse(&info) {
            return Err(error("Fleet lock is unsafe"));
        }
    }
    let mut options = fs::OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000);
    }
    let lock = options.open(path)?;
    if reparse(&lock.metadata()?) {
        return Err(error("Fleet lock is unsafe"));
    }
    fs2::FileExt::lock_exclusive(&lock)?;
    let mut journal = journal()?;
    let result = change(&mut journal)?;
    if journal.requests.len() > MAX_RECORDS
        || serde_json::to_vec(&journal)?.len() as u64 > MAX_JOURNAL
    {
        return Err(error(
            "Fleet journal capacity reached; retain existing receipts and archive through the owner before new submissions",
        ));
    }
    state::atomic_json(&root.join("fleet.json"), &journal)?;
    Ok(result)
}

fn saved_request(id: &str) -> Result<Option<Submission>> {
    state::validate_id(id)?;
    Ok(journal()?.requests.get(id).cloned())
}

fn same_explicit_target(selector: Option<&str>, device: &str) -> Result<()> {
    if selector.is_some_and(|value| value != "auto" && value != device) {
        return Err(error(
            "This request or job belongs to a different retained device; it cannot be moved",
        ));
    }
    Ok(())
}

fn retained_target(record: &Submission) -> Result<Target> {
    let target = Target::load(&record.device)?;
    if !location::same_directory(&target.root, &record.profile_root)
        || target.identity()? != record.target_identity
    {
        return Err(error(
            "The retained device SSH identity changed; refusing to redirect its existing request or job",
        ));
    }
    Ok(target)
}

fn claim_request(id: &str, record: Submission) -> Result<Submission> {
    state::validate_id(id)?;
    change_journal(|journal| {
        if let Some(old) = journal.requests.get(id) {
            if old.fingerprint != record.fingerprint
                || old.device != record.device
                || !location::same_directory(&old.profile_root, &record.profile_root)
                || old.target_identity != record.target_identity
            {
                return Err(error(
                    "This request ID already belongs to different source, arguments or device; the original request is retained",
                ));
            }
            if old.rejected {
                return Err(error(
                    "This request ID was definitively rejected; use a new request ID for a new submission",
                ));
            }
            return Ok(old.clone());
        }
        if let Some(project) = &record.project {
            if let Some(claim) = journal.claims.get(project) {
                if claim != id {
                    return Err(AppError::new(
                        8,
                        "This worktree has an active or uncertain request on another device; inspect that request before submitting new work",
                    ));
                }
            }
            journal.claims.insert(project.clone(), id.into());
        }
        journal.requests.insert(id.into(), record.clone());
        Ok(record)
    })
}

fn delivery_terminal(value: &Value) -> bool {
    crate::terminal(value)
        && !matches!(
            value["return_status"].as_str(),
            Some("pending" | "retrying")
        )
}

fn release_request(journal: &mut Journal, id: &str) {
    let project = journal
        .requests
        .get_mut(id)
        .map(|record| {
            record.complete = true;
            record.project.clone()
        })
        .flatten();
    if let Some(project) = project {
        if journal
            .claims
            .get(&project)
            .is_some_and(|claim| claim == id)
        {
            journal.claims.remove(&project);
        }
    }
}

fn response_failed(value: &Value) -> bool {
    value["ok"] == false
        || value["process_exit_code"]
            .as_i64()
            .is_some_and(|code| code != 0)
}

fn record_response(id: &str, target: &Target, value: &Value) -> Result<()> {
    // A foreground retry may fail validation or authentication while the
    // independent return worker already accepted the original submission.
    // Recover that durable job first; such an error cannot release its claim.
    recover_jobs(target)?;
    change_journal(|journal| {
        let Some(record) = journal.requests.get_mut(id) else {
            return Err(error("Fleet submission lost its retained request identity"));
        };
        if record.device != target.id
            || !location::same_directory(&record.profile_root, &target.root)
        {
            return Err(error("Fleet response belongs to a different device"));
        }
        if let Some(job) = value["job"].as_str() {
            if record.rejected {
                return Err(error(
                    "A rejected request cannot be reopened with an accepted job",
                ));
            }
            state::validate_id(job)?;
            if record.job.as_deref().is_some_and(|old| old != job) {
                return Err(error("The device changed an idempotent job identity"));
            }
            record.job = Some(job.into());
        }
        if delivery_terminal(value) {
            release_request(journal, id);
        } else if value["ok"] == false
            && record.job.is_none()
            && matches!(value["code"].as_u64(), Some(2 | 5 | 7 | 8))
        {
            if let Some(record) = journal.requests.get_mut(id) {
                record.rejected = true;
            }
            release_request(journal, id);
        }
        Ok(())
    })
}

fn jobs(value: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    if value["job"].is_string() {
        out.push(value);
    }
    for key in ["jobs", "laptop_jobs", "shared_board"] {
        if let Some(items) = value[key].as_array() {
            out.extend(items);
        }
    }
    out
}

fn recover_jobs(target: &Target) -> Result<()> {
    let saved = journal()?;
    let pending = saved
        .requests
        .iter()
        .filter(|(_, record)| {
            record.device == target.id
                && location::same_directory(&record.profile_root, &target.root)
                && record.job.is_none()
                && !record.complete
        })
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return Ok(());
    }
    let identity = target.identity()?;
    let mut recovered = Vec::new();
    for (id, record) in pending {
        if record.target_identity != identity {
            return Err(error(
                "Retained device identity changed; pending submission remains bound to its original device",
            ));
        }
        let Some(bytes) = bounded_file(
            &target.root.join("returns").join(format!("{id}.json")),
            32 * inventory::MIB,
        )?
        else {
            continue;
        };
        let receipt: Value = serde_json::from_slice(&bytes)?;
        if receipt["v"] != 1
            || receipt["request_id"] != id.as_str()
            || receipt["request"]["request_id"] != id.as_str()
        {
            return Err(error(
                "Recovered result receipt differs from its retained request",
            ));
        }
        if record
            .project
            .as_deref()
            .is_some_and(|project| receipt["request"]["project"] != project)
            || record
                .manifest_id
                .as_deref()
                .is_some_and(|manifest| receipt["request"]["manifest_id"] != manifest)
        {
            return Err(error(
                "Recovered result receipt changed the retained project or source manifest",
            ));
        }
        if let Some(job) = receipt["job"].as_str() {
            state::validate_id(job)?;
            recovered.push((id.clone(), Some(job.to_string())));
        } else if receipt["complete"] == true
            && receipt["job"].is_null()
            && receipt["submission"].is_null()
            && receipt["error"]
                .as_str()
                .is_some_and(|error| error.starts_with("Submission rejected; no automatic retry: "))
        {
            recovered.push((id.clone(), None));
        }
    }
    if recovered.is_empty() {
        return Ok(());
    }
    change_journal(|journal| {
        for (id, job) in recovered {
            if let Some(record) = journal.requests.get_mut(&id) {
                if record.device != target.id
                    || !location::same_directory(&record.profile_root, &target.root)
                    || record.target_identity != identity
                    || record
                        .job
                        .as_ref()
                        .is_some_and(|old| Some(old) != job.as_ref())
                {
                    return Err(error(
                        "Recovered submission changed its retained device or job",
                    ));
                }
                if let Some(job) = job {
                    record.job = Some(job);
                } else {
                    record.rejected = true;
                    release_request(journal, &id);
                }
            }
        }
        Ok(())
    })
}

fn reconcile(target: &Target, value: &Value) -> Result<()> {
    let observations = jobs(value);
    if !state::root()?.join("fleet.json").exists() {
        return Ok(());
    }
    recover_jobs(target)?;
    if observations.is_empty() {
        return Ok(());
    }
    let identity = target.identity()?;
    change_journal(|journal| {
        let mut complete = Vec::new();
        for (id, record) in &journal.requests {
            if record.device != target.id
                || !location::same_directory(&record.profile_root, &target.root)
            {
                continue;
            }
            if record.target_identity != identity {
                return Err(error(
                    "Retained device identity changed; its project claims remain held",
                ));
            }
            if record.complete {
                continue;
            }
            if record.job.as_deref().is_some_and(|job| {
                observations
                    .iter()
                    .any(|value| value["job"] == job && delivery_terminal(value))
            }) {
                complete.push(id.clone());
            }
        }
        for id in complete {
            release_request(journal, &id);
        }
        Ok(())
    })
}

fn child(
    target: &Target,
    client: &str,
    args: Vec<String>,
    manifest: Option<&str>,
    input: &[u8],
    timeout: Duration,
) -> Result<Value> {
    child_with_body(target, client, args, manifest, None, input, timeout)
}

#[allow(clippy::too_many_arguments)]
fn child_with_body(
    target: &Target,
    client: &str,
    args: Vec<String>,
    manifest: Option<&str>,
    body_sha256: Option<&str>,
    input: &[u8],
    timeout: Duration,
) -> Result<Value> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["--profile-scoped", "--json", "--client", client])
        .args(args)
        .env("SPARKDEV_HOME", &target.root)
        .env_remove("SPARKDEV_EXPECTED_MANIFEST")
        .env_remove("SPARKDEV_EXPECTED_CALL_BODY");
    if let Some(manifest) = manifest {
        command.env("SPARKDEV_EXPECTED_MANIFEST", manifest);
    }
    if let Some(hash) = body_sha256 {
        command.env("SPARKDEV_EXPECTED_CALL_BODY", hash);
    }
    let (code, out, _) = transport::process_local_cli(command, input, timeout)?;
    let mut value = out
        .split(|byte| *byte == b'\n')
        .rev()
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .find(Value::is_object)
        .ok_or_else(|| {
            AppError::new(
                6,
                "Device worker returned no bounded JSON result; its stderr is withheld",
            )
        })?;
    if code != Some(0) && value.get("process_exit_code").is_none() {
        value["process_exit_code"] = json!(code.unwrap_or(125));
    }
    Ok(value)
}

fn qualify_job(device: &str, job: &str) -> String {
    if matches!(device, "spark" | "laptop") || job.contains(':') {
        job.into()
    } else {
        format!("{device}:{job}")
    }
}

fn split_job(value: &str) -> Result<(Option<&str>, &str)> {
    if let Some((device, job)) = value.split_once(':') {
        if device != "spark" && device != "laptop" {
            nodes::validate_id(device)?;
        }
        state::validate_id(job)?;
        Ok((Some(device), job))
    } else {
        state::validate_id(value)?;
        Ok((None, value))
    }
}

fn annotate(value: &mut Value, target: &Target) {
    if !value.is_object() {
        return;
    }
    value["device"] = json!(target.id);
    value["device_label"] = json!(target.label);
    if let Some(job) = value["job"].as_str().map(String::from) {
        value["job"] = json!(qualify_job(&target.id, &job));
        if target.node.is_some() {
            value["remote_job"] = json!(job);
        }
        if target.node.is_some() {
            value["placement"] = json!(target.id);
        }
    }
    if let Some(job) = value["cached_from"].as_str().map(String::from) {
        value["cached_from"] = json!(qualify_job(&target.id, &job));
    }
    for key in ["jobs", "shared_board"] {
        if let Some(items) = value[key].as_array_mut() {
            for job in items {
                annotate(job, target);
            }
        }
    }
}

/// Everything a new computer's profile is made from.
pub(crate) struct Registration {
    pub id: String,
    pub label: String,
    pub ssh_alias: String,
    pub hosts: Vec<String>,
    pub host_key_alias: String,
    pub identity_file: Option<std::path::PathBuf>,
    pub user: Option<String>,
    pub port: u16,
    pub ssh_config: Option<std::path::PathBuf>,
    pub endpoint: nodes::Endpoint,
    pub priority: i32,
}

/// Register a computer alongside the Spark, never replacing it.
pub(crate) fn register_node(registration: Registration) -> Result<Value> {
    let config = state::Config {
        ssh_alias: registration.ssh_alias,
        hosts: registration.hosts,
        host_key_alias: registration.host_key_alias,
        identity_file: registration.identity_file,
        user: registration.user,
        port: registration.port,
        allow_remote: false,
        connection_mode: Some(state::ConnectionMode::LanOnly),
        connection_enabled: true,
        auto_addresses: false,
        laptop_jobs: 2,
        local_build_gate: state::config()
            .ok()
            .and_then(|config| config.local_build_gate),
        known_hosts: None,
    };
    let node = nodes::Node {
        id: registration.id,
        label: registration.label,
        enabled: true,
        priority: registration.priority,
        endpoint: registration.endpoint,
        ssh_config: registration.ssh_config,
    };
    let node = nodes::add(node, config)?;
    Ok(
        json!({"ok":true,"node":node,"configured":true,"runtime_verified":false,"note":"Profile registered without replacing the existing Spark. Run device inspection to verify its controller and runtimes."}),
    )
}

fn node_crud(command: &NodeCommands) -> Result<Value> {
    match command {
        NodeCommands::List => Ok(json!({"ok":true,"nodes":nodes::list()?})),
        NodeCommands::Scan => crate::enroll::scan(),
        NodeCommands::Key { id } => crate::enroll::key(id),
        NodeCommands::Probe { id, host, user, port } => crate::enroll::probe(id, host, user, *port),
        NodeCommands::Authorize { id, host, user, port } => {
            crate::enroll::authorize(id, host, user, *port, &crate::enroll::read_password()?)
        }
        NodeCommands::Trust {
            id,
            label,
            host,
            user,
            port,
            fingerprint,
            wsl_distro,
            wsl_user,
        } => {
            let wsl = match (wsl_distro, wsl_user) {
                (Some(distribution), Some(user)) => Some((distribution.as_str(), user.as_str())),
                (None, None) => None,
                _ => return Err(error("A Windows device needs both its WSL distribution and Linux account")),
            };
            crate::enroll::trust(id, label, host, user, *port, fingerprint, wsl)
        }
        NodeCommands::Enable { id, enabled } => Ok(
            json!({"ok":true,"node":nodes::set_enabled(id,&matches!(enabled,Switch::On))?,"running_jobs_unchanged":true}),
        ),
        NodeCommands::Add {
            id,
            label,
            ssh_alias,
            hosts,
            host_key_alias,
            identity_file,
            user,
            port,
            ssh_config,
            wsl_distro,
            wsl_user,
            controller_path,
            priority,
        } => {
            let endpoint = match (wsl_distro, wsl_user, controller_path) {
                (None, None, None) => nodes::Endpoint::LinuxSsh,
                (Some(distribution), Some(user), Some(controller_path)) => {
                    nodes::Endpoint::WindowsWsl {
                        distribution: distribution.clone(),
                        user: user.clone(),
                        controller_path: controller_path.clone(),
                    }
                }
                _ => {
                    return Err(error(
                        "A WSL device requires --wsl-distro, --wsl-user and --controller-path together",
                    ));
                }
            };
            register_node(Registration {
                id: id.clone(),
                label: label.clone().unwrap_or_else(|| id.clone()),
                ssh_alias: ssh_alias.clone(),
                hosts: hosts.clone(),
                host_key_alias: host_key_alias.clone(),
                identity_file: identity_file.clone(),
                user: user.clone(),
                port: *port,
                ssh_config: ssh_config.clone(),
                endpoint,
                priority: *priority,
            })
        }
    }
}

fn observe(target: &Target, client: &str) -> Result<Value> {
    let value = child(
        target,
        client,
        vec!["devices".into()],
        None,
        &[],
        OBSERVATION_TIMEOUT,
    )?;
    if response_failed(&value) {
        return Err(AppError::new(
            value["code"].as_u64().unwrap_or(6) as u8,
            "Device observation failed; inspect the selected device configuration",
        ));
    }
    let wanted = if target.id == "laptop" {
        "laptop"
    } else {
        "spark"
    };
    let mut device = value["devices"]
        .as_array()
        .and_then(|devices| devices.iter().find(|device| device["id"] == wanted))
        .cloned()
        .ok_or_else(|| AppError::new(6, "Device worker omitted its remote observation"))?;
    if let Some(board) = value.get("shared_board").filter(|board| board.is_array()) {
        device["jobs"] = board.clone();
    }
    if let Err(error) = reconcile(target, &device) {
        device["fleet_claim_error"] = json!({"code":error.code,"message":error.message});
    }
    device["id"] = json!(target.id);
    device["label"] = json!(target.label);
    device["enabled"] = json!(target.enabled());
    if !target.enabled() {
        device["availability"] = json!("disabled");
    }
    Ok(device)
}

fn observation(target: &Target, value: &Value) -> nodes::NodeObservation {
    let node = target.node.clone().unwrap_or(nodes::Node {
        id: "spark".into(),
        label: target.label.clone(),
        enabled: true,
        priority: 0,
        endpoint: nodes::Endpoint::LinuxSsh,
        ssh_config: None,
    });
    let (mut used_cpu, mut used_memory) = (Some(0.0), Some(0.0));
    if let Some(jobs) = value["jobs"].as_array() {
        for job in jobs {
            if matches!(job["state"].as_str(), Some("starting" | "running"))
                && job["where"] != "laptop"
            {
                used_cpu = used_cpu
                    .zip(job["reserved_cpu"].as_f64())
                    .map(|(used, reserved)| used + reserved);
                used_memory = used_memory
                    .zip(job["reserved_memory_gb"].as_f64())
                    .map(|(used, reserved)| used + reserved);
            }
        }
    } else {
        used_cpu = None;
        used_memory = None;
    }
    let limits = if value["managed_limits"].is_object() {
        &value["managed_limits"]
    } else {
        &value["limits"]
    };
    let available_cpu = limits["cpu_total"]
        .as_f64()
        .zip(used_cpu)
        .map(|(total, used)| (total - used).max(0.0));
    let budget_memory = limits["memory_gb_total"]
        .as_f64()
        .zip(used_memory)
        .map(|(total, used)| ((total - used).max(0.0) * inventory::GIB as f64) as u64);
    let physical = value["capabilities"]["system_memory"]["available_bytes"].as_u64();
    let headroom = limits["headroom_gb"]
        .as_u64()
        .map(|gb| gb.saturating_mul(inventory::GIB));
    let per_job = limits["memory_gb_per_job"]
        .as_u64()
        .map(|gb| gb.saturating_mul(inventory::GIB));
    let available_memory_bytes = match (budget_memory, physical, headroom, per_job) {
        (Some(budget), Some(physical), Some(headroom), Some(per_job)) => {
            Some(budget.min(physical.saturating_sub(headroom)).min(per_job))
        }
        // Scheduler budget alone is a recorded reservation budget. Without a
        // physical observation it cannot prove a requested memory fit.
        _ => None,
    };
    let gpu = &value["capabilities"]["gpu"];
    let gpu_vram_bytes = if gpu["observed"] == true {
        gpu["devices"].as_array().and_then(|devices| {
            devices
                .iter()
                .filter_map(|device| {
                    match (
                        device["total_memory_bytes"].as_u64(),
                        device["free_memory_bytes"].as_u64(),
                    ) {
                        (Some(total), Some(free)) => Some(total.min(free)),
                        _ => None,
                    }
                })
                .max()
        })
    } else {
        None
    };
    nodes::NodeObservation {
        node,
        authenticated: value["transport"]["authenticated"] == true,
        scheduler_healthy: value["scheduler"]["healthy"] == true,
        runtimes: value["runtimes"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
        available_cpu,
        available_memory_bytes,
        gpu_vram_bytes,
    }
}

/// Readiness is measured against the controller's default reservation, not
/// against physical idle CPU. Explicit selection may still queue on this node.
fn default_job_fits(value: &Value, client: &str) -> bool {
    let limits = if value["managed_limits"].is_object() {
        &value["managed_limits"]
    } else {
        &value["limits"]
    };
    let (
        Some(cpu_total),
        Some(memory_total),
        Some(cpu_job),
        Some(memory_job),
        Some(client_jobs),
        Some(client_cpu),
        Some(jobs),
    ) = (
        limits["cpu_total"].as_f64(),
        limits["memory_gb_total"].as_f64(),
        limits["cpu_per_job"].as_f64(),
        limits["memory_gb_per_job"].as_f64(),
        limits["client_jobs"].as_u64(),
        limits["client_cpu"].as_f64(),
        value["jobs"].as_array(),
    )
    else {
        return false;
    };
    if client_jobs == 0
        || ![cpu_total, memory_total, cpu_job, memory_job, client_cpu]
            .iter()
            .all(|value| value.is_finite() && *value > 0.0)
    {
        return false;
    }
    let (mut cpu_used, mut memory_used, mut client_cpu_used, mut client_jobs_used) =
        (0.0, 0.0, 0.0, 0u64);
    for job in jobs {
        if matches!(job["state"].as_str(), Some("starting" | "running")) && job["where"] != "laptop"
        {
            let (Some(cpu), Some(memory)) = (
                job["reserved_cpu"].as_f64(),
                job["reserved_memory_gb"].as_f64(),
            ) else {
                return false;
            };
            if !cpu.is_finite() || !memory.is_finite() || cpu < 0.0 || memory < 0.0 {
                return false;
            }
            cpu_used += cpu;
            memory_used += memory;
            if job["kind"] != "service" {
                let Some(owner) = job["client"].as_str() else {
                    return false;
                };
                if owner == client {
                    client_cpu_used += cpu;
                    client_jobs_used += 1;
                }
            }
        }
    }
    if cpu_total - cpu_used < cpu_job || memory_total - memory_used < memory_job {
        return false;
    }
    if client_jobs_used >= client_jobs || client_cpu_used + cpu_job > client_cpu {
        return false;
    }
    if let (Some(available), Some(headroom)) = (
        value["capabilities"]["system_memory"]["available_bytes"].as_u64(),
        limits["headroom_gb"].as_u64(),
    ) {
        if available.saturating_sub(headroom.saturating_mul(inventory::GIB))
            < (memory_job * inventory::GIB as f64) as u64
        {
            return false;
        }
    }
    true
}

fn requirements(cli: &Cli, runtime: &str) -> nodes::SelectionRequirements {
    nodes::SelectionRequirements {
        runtime: runtime.into(),
        min_cpu: None,
        min_memory_bytes: cli
            .min_memory_gb
            .map(|value| value.saturating_mul(inventory::GIB)),
        min_gpu_vram_bytes: cli
            .min_vram_gb
            .map(|value| value.saturating_mul(inventory::GIB)),
    }
}

fn is_blender(argv: &[String]) -> bool {
    argv.first()
        .and_then(|arg| Path::new(arg).file_stem())
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("blender"))
}

fn command_supported(target: &Target, value: &Value, argv: &[String]) -> bool {
    if target.node.is_none() || !is_blender(argv) {
        return true;
    }
    let blender = &value["gpu_video"]["blender"];
    blender["supported"] == true
        || matches!(
            blender["state"].as_str(),
            Some("ready" | "probed" | "supported")
        )
        || blender["devices"]
            .as_object()
            .is_some_and(|devices| !devices.is_empty())
}

fn select_target(
    cli: &Cli,
    runtime: &str,
    argv: &[String],
    explicit: Option<&str>,
) -> Result<Target> {
    if let Some(id) = explicit.filter(|id| *id != "auto") {
        let target = Target::load(id)?;
        if !target.enabled() {
            return Err(error("The selected device is disabled for new submissions"));
        }
        if id == "laptop" {
            if cli.min_memory_gb.is_some() || cli.min_vram_gb.is_some() {
                return Err(error(
                    "Minimum remote memory requirements cannot be applied to a native laptop job",
                ));
            }
            return Ok(target);
        }
        if cli.min_memory_gb.is_some()
            || cli.min_vram_gb.is_some()
            || (target.node.is_some() && is_blender(argv))
        {
            let value = observe(&target, cli.client.name())?;
            if !command_supported(&target, &value, argv) {
                return Err(error(
                    "The selected device has no verified Blender runtime capability",
                ));
            }
            nodes::select(&[observation(&target, &value)], &requirements(cli, runtime))?;
        }
        return Ok(target);
    }
    let mut targets = vec![Target::load("spark")?];
    for node in nodes::list()?.into_iter().filter(|node| node.enabled) {
        if let Ok(target) = Target::load(&node.id) {
            targets.push(target);
        }
    }
    let observations = std::thread::scope(|scope| {
        let handles = targets
            .iter()
            .map(|target| {
                scope.spawn(move || {
                    observe(target, cli.client.name())
                        .ok()
                        .filter(|value| {
                            command_supported(target, value, argv)
                                && default_job_fits(value, cli.client.name())
                        })
                        .map(|value| observation(target, &value))
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok().flatten())
            .collect::<Vec<_>>()
    });
    let selected = nodes::select(&observations, &requirements(cli, runtime))?;
    Target::load(&selected.id)
}

fn add_flag(args: &mut Vec<String>, name: &str, value: impl ToString) {
    args.push(name.into());
    args.push(value.to_string());
}

fn run_args(
    dir: &Path,
    argv: &[String],
    runtime: &str,
    id: &str,
    placement: &str,
    no_cache: bool,
    no_return: bool,
) -> Vec<String> {
    let mut args = vec![
        "run".into(),
        dir.to_string_lossy().into_owned(),
        "--detach".into(),
    ];
    add_flag(&mut args, "--runtime", runtime);
    add_flag(&mut args, "--request-id", id);
    add_flag(&mut args, "--where", placement);
    if no_cache {
        args.push("--no-cache".into());
    }
    if no_return {
        args.push("--no-return".into());
    }
    args.push("--".into());
    args.extend(argv.iter().cloned());
    args
}

fn job_args(command: &Commands, job: &str) -> Result<Vec<String>> {
    Ok(match command {
        Commands::Wait { timeout, .. } => vec![
            "wait".into(),
            job.into(),
            "--timeout".into(),
            timeout.to_string(),
        ],
        Commands::Logs { since, limit, .. } => vec![
            "logs".into(),
            job.into(),
            "--since".into(),
            since.to_string(),
            "--limit".into(),
            limit.to_string(),
        ],
        Commands::Cancel { .. } => vec!["cancel".into(), job.into()],
        Commands::Outputs { .. } => vec!["outputs".into(), job.into()],
        _ => return Err(error("This command does not address a job")),
    })
}

fn job_target<'a>(cli: &'a Cli, reference: &'a str) -> Result<(&'a str, &'a str)> {
    let (qualified, job) = split_job(reference)?;
    if let Some(device) = qualified {
        same_explicit_target(cli.device.as_deref(), device)?;
    }
    let id = qualified
        .or(cli.device.as_deref().filter(|id| *id != "auto"))
        .unwrap_or(if job.starts_with("laptop-") {
            "laptop"
        } else {
            "spark"
        });
    if job.starts_with("laptop-") && id != "laptop" && id != "spark" {
        return Err(error("A laptop job cannot be routed to another device"));
    }
    if id == "laptop" && !job.starts_with("laptop-") {
        return Err(error(
            "--device laptop requires a native laptop- job ID; a remote job or server cannot be routed to the laptop",
        ));
    }
    if job.starts_with("laptop-") && matches!(cli.command, Commands::Outputs { .. }) {
        return Err(error(
            "Native laptop jobs write files in their local worktree and do not have a remote output manifest",
        ));
    }
    Ok((id, job))
}

fn route_job(cli: &Cli, reference: &str) -> Result<Value> {
    let (id, job) = job_target(cli, reference)?;
    let target = Target::load(id)?;
    recover_jobs(&target)?;
    for record in journal()?
        .requests
        .values()
        .filter(|record| record.device == id && record.job.as_deref() == Some(job))
    {
        retained_target(record)?;
    }
    let mut value = child(
        &target,
        cli.client.name(),
        job_args(&cli.command, job)?,
        None,
        &[],
        Duration::from_secs(95),
    )?;
    reconcile(&target, &value)?;
    annotate(&mut value, &target);
    Ok(value)
}

fn parallel_observations<L, N>(
    registrations: &[nodes::Node],
    legacy: L,
    named: N,
) -> (Result<Value>, Vec<Value>)
where
    L: FnOnce() -> Result<Value> + Send,
    N: Fn(&nodes::Node) -> Value + Sync,
{
    std::thread::scope(|scope| {
        let legacy = scope.spawn(legacy);
        let named = &named;
        let handles = registrations
            .iter()
            .map(|node| (node, scope.spawn(move || named(node))))
            .collect::<Vec<_>>();
        let legacy = legacy.join().unwrap_or_else(|_| {
            Err(AppError::new(
                3,
                "Legacy device observation failed; other device observations are retained",
            ))
        });
        let named=handles.into_iter().map(|(node,handle)|handle.join().unwrap_or_else(|_|json!({"id":node.id,"label":node.label,"reachable":false,"availability":"observation_failed"}))).collect();
        (legacy, named)
    })
}

fn legacy_unavailable(mut failure: Value, is_devices: bool) -> Value {
    let limited = metadata_limited(&failure);
    if limited {
        failure["kind"] = json!(transport::LOCAL_METADATA_LIMIT);
    }
    let remote_reachable = if limited { Value::Null } else { json!(false) };
    let availability = if limited {
        "observation_limited"
    } else {
        "unreachable"
    };
    let local = crate::local::status();
    let local_error = local
        .as_ref()
        .err()
        .map(|error| json!({"code":error.code,"message":error.message}));
    let local = local.unwrap_or_else(|_| json!([]));
    if is_devices {
        json!({"ok":true,"legacy_error":failure,"devices":[
            {"id":"laptop","availability":"available","reachable":true,"os":std::env::consts::OS,"architecture":std::env::consts::ARCH,"logical_parallelism":std::thread::available_parallelism().ok().map(|value|value.get()),"jobs":local,"state_error":local_error,"free_cpu":null,"free_memory_bytes":null},
            {"id":"spark","availability":availability,"reachable":remote_reachable,"error":failure,"transport":{"selected":"unselected","authenticated":false},"free_cpu":null,"free_memory_bytes":null}
        ],"shared_board":null,"observation_only":true,"admission_required":true,"claimed":false})
    } else {
        json!({"ok":true,"remote_available":remote_reachable,"remote_error":failure,"legacy_error":failure,"jobs":[],"laptop_jobs":local,"local_state_error":local_error,"transport":{"selected":"unselected","authenticated":false}})
    }
}

fn metadata_limited(failure: &Value) -> bool {
    failure["message"]
        .as_str()
        .is_some_and(|message| message.starts_with(transport::LOCAL_METADATA_LIMIT))
}

fn observation_failure(id: &str, label: &str, enabled: bool, error: AppError) -> Value {
    let failure = json!({"code":error.code,"message":error.message});
    if metadata_limited(&failure) {
        json!({"id":id,"label":label,"enabled":enabled,"reachable":null,"availability":"observation_limited","error":{"kind":transport::LOCAL_METADATA_LIMIT,"code":failure["code"],"message":failure["message"]}})
    } else {
        json!({"id":id,"label":label,"enabled":enabled,"reachable":false,"availability":"unavailable","error":failure})
    }
}

fn aggregate(cli: &Cli) -> Result<Value> {
    let legacy = Target::load("spark")?;
    let is_devices = matches!(cli.command, Commands::Devices);
    let explicit = cli.device.as_deref().filter(|device| *device != "auto");
    if let Some(id) = explicit {
        let target = Target::load(id)?;
        if is_devices {
            let mut device = match observe(&target, cli.client.name()) {
                Ok(device) => device,
                Err(error) if error.message.starts_with(transport::LOCAL_METADATA_LIMIT) => {
                    observation_failure(
                        &target.id,
                        &target.label,
                        target.node.as_ref().is_none_or(|node| node.enabled),
                        error,
                    )
                }
                Err(error) => return Err(error),
            };
            annotate(&mut device, &target);
            return Ok(
                json!({"ok":true,"devices":[device],"observation_only":true,"admission_required":true,"claimed":false}),
            );
        }
        if id == "laptop" {
            return Ok(json!({"ok":true,"device":"laptop","jobs":crate::local::status()?}));
        }
        let mut value = match child(
            &target,
            cli.client.name(),
            vec!["status".into(), "--all".into()],
            None,
            &[],
            OBSERVATION_TIMEOUT,
        ) {
            Ok(value) => value,
            Err(error) if error.message.starts_with(transport::LOCAL_METADATA_LIMIT) => {
                let mut value = observation_failure(
                    &target.id,
                    &target.label,
                    target.node.as_ref().is_none_or(|node| node.enabled),
                    error,
                );
                value["ok"] = json!(true);
                value["jobs"] = json!([]);
                annotate(&mut value, &target);
                return Ok(value);
            }
            Err(error) => return Err(error),
        };
        reconcile(&target, &value)?;
        annotate(&mut value, &target);
        return Ok(value);
    }
    let registrations = nodes::list()?;
    let (legacy_result, observed) = parallel_observations(
        &registrations,
        || {
            child(
                &legacy,
                cli.client.name(),
                if is_devices {
                    vec!["devices".into()]
                } else {
                    vec!["status".into(), "--all".into()]
                },
                None,
                &[],
                OBSERVATION_TIMEOUT,
            )
        },
        |node| match Target::load(&node.id) {
            Ok(target) => match observe(&target, cli.client.name()) {
                Ok(mut device) => {
                    annotate(&mut device, &target);
                    device
                }
                Err(error) => observation_failure(&node.id, &node.label, node.enabled, error),
            },
            Err(error) => {
                json!({"id":node.id,"label":node.label,"enabled":node.enabled,"reachable":false,"availability":"invalid_profile","error":{"code":error.code,"message":error.message}})
            }
        },
    );
    let mut value = match legacy_result {
        Ok(value) => value,
        Err(error) => legacy_unavailable(
            json!({"code":error.code,"message":error.message}),
            is_devices,
        ),
    };
    if let Err(error) = reconcile(&legacy, &value) {
        value["fleet_claim_error"] = json!({"code":error.code,"message":error.message});
    }
    if let Err(error) = reconcile(&Target::load("laptop")?, &value) {
        value["fleet_laptop_claim_error"] = json!({"code":error.code,"message":error.message});
    }
    if value["ok"] == false {
        let failure = value.clone();
        value = legacy_unavailable(failure, is_devices);
    }
    let mut added_jobs = Vec::new();
    for device in &observed {
        if let Some(jobs) = device["jobs"].as_array() {
            added_jobs.extend(jobs.iter().cloned());
        }
    }
    let key = if is_devices { "shared_board" } else { "jobs" };
    if !value[key].is_array() {
        value[key] = json!([]);
    }
    if let Some(jobs) = value[key].as_array_mut() {
        jobs.extend(added_jobs);
    }
    if is_devices {
        if !value["devices"].is_array() {
            value["devices"] = json!([]);
        }
        value["devices"].as_array_mut().unwrap().extend(observed);
    } else {
        value["nodes"] = json!(observed);
    }
    value["fleet"] = json!({"v":1,"registered_nodes":registrations.len(),"observed_at":state::now(),"observation_only":true,"reservation":false});
    Ok(value)
}

fn plan(
    cli: &Cli,
    dir: &Path,
    runtime: &Option<String>,
    argv: &[String],
    requested: Placement,
) -> Result<Value> {
    let inv = inventory::collect_readonly(dir)?;
    let (placement, reason) = crate::placement(requested, argv, &inv.root)?;
    let selector = cli.device.as_deref().unwrap_or("auto");
    let mut selected = if placement == Placement::Laptop {
        "laptop".to_string()
    } else {
        selector.to_string()
    };
    if !matches!(selector, "auto" | "spark" | "laptop") {
        let target = Target::load(selector)?;
        let (automatic, _) = crate::placement(Placement::Auto, argv, &inv.root)?;
        if requested == Placement::Laptop || automatic == Placement::Laptop {
            return Err(error("This Windows-only command must remain on the laptop"));
        }
        selected = target.id;
    } else if selector == "laptop" {
        selected = "laptop".into();
    } else if selector == "spark" && requested == Placement::Laptop {
        return Err(error("--device spark conflicts with --where laptop"));
    }
    let runtime = runtime
        .clone()
        .unwrap_or_else(|| crate::default_runtime(argv).into());
    state::validate_id(&runtime)?;
    Ok(
        json!({"ok":true,"device":selected,"placement":if selected=="auto" {"spark"} else {&selected},"placement_reason":reason,"runtime":runtime,
        "project":inv.project,"manifest_id":inv.manifest_id,"files":inv.manifest.len(),"bytes":inv.bytes,"argv":argv,
        "requested_min_memory_gb":cli.min_memory_gb,"requested_min_vram_gb":cli.min_vram_gb,
        "device_selection":if selected=="auto" {"deferred_until_run"} else {"explicit_preview"},
        "admission_required":true,"claimed":false,"uploaded":false,"submitted":false,"runtime_registry_checked":false,"observation_only":true,
        "note":"Preview only. Device capability, current admission and optional memory requirements are checked by run; auto selects a device at submission."}),
    )
}

fn legacy_conflict(project: &str, source: &Path, request_id: &str, client: &str) -> Result<()> {
    if let Some(jobs) = crate::local::status()?.as_array() {
        if jobs.iter().any(|job| {
            job["project"] == project && !crate::terminal(job) && job["request_id"] != request_id
        }) {
            return Err(AppError::new(
                8,
                "This worktree already has an active laptop job",
            ));
        }
    }
    let returns = state::root()?.join("returns");
    if returns.exists() {
        let info = fs::symlink_metadata(&returns)?;
        if !info.is_dir() || reparse(&info) {
            return Err(error("Legacy result state directory is unsafe"));
        }
        let mut count = 0;
        let mut unreturned_opt_out = Vec::new();
        for item in fs::read_dir(returns)? {
            let path = item?.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            count += 1;
            if count > MAX_RECORDS {
                return Err(error(
                    "Too many retained legacy result records to check project ownership",
                ));
            }
            let Some(bytes) = bounded_file(&path, 32 * inventory::MIB)? else {
                continue;
            };
            let record: Value = serde_json::from_slice(&bytes)?;
            if record["request_id"] == request_id {
                continue;
            }
            let same = record["project"]
                .as_str()
                .is_some_and(|path| Path::new(path) == source);
            if same && record["complete"] != true {
                return Err(AppError::new(
                    8,
                    "This worktree has outstanding results or an uncertain legacy Spark submission; finish its existing delivery before starting another device",
                ));
            }
            // no-return records complete immediately after submission, which
            // is not proof that their workload stopped modifying this tree.
            if same && record["disabled"] == true {
                if let Some(job) = record["job"].as_str() {
                    unreturned_opt_out.push(job.to_string());
                }
            }
        }
        if !unreturned_opt_out.is_empty() {
            let target = Target::load("spark")?;
            for id in unreturned_opt_out {
                let status = child(
                    &target,
                    client,
                    vec!["wait".into(), id, "--timeout".into(), "0".into()],
                    None,
                    &[],
                    OBSERVATION_TIMEOUT,
                )?;
                if response_failed(&status) || !delivery_terminal(&status) {
                    return Err(AppError::new(
                        8,
                        "This worktree has a retained no-return Spark job whose terminal state is not currently verified",
                    ));
                }
            }
        }
    }
    Ok(())
}

fn existing_legacy_device(id: &str) -> Result<Option<String>> {
    let path = state::root()?.join("requests").join(format!("{id}.json"));
    let Some(bytes) = bounded_file(&path, inventory::MAX_REQUEST)? else {
        return Ok(None);
    };
    let record: Value = serde_json::from_slice(&bytes)?;
    Ok(Some(
        if record["placement"]
            .as_str()
            .is_some_and(|placement| placement.eq_ignore_ascii_case("laptop"))
        {
            "laptop"
        } else {
            "spark"
        }
        .into(),
    ))
}

#[allow(clippy::too_many_arguments)]
fn submit(
    cli: &Cli,
    dir: &Path,
    argv: Vec<String>,
    runtime: String,
    id: Option<String>,
    requested: Placement,
    no_cache: bool,
    no_return: bool,
    detach: bool,
    prepare: bool,
    events: &mut dyn FnMut(&Value),
) -> Result<Value> {
    let inv = inventory::collect_readonly(dir)?;
    let id = id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    state::validate_id(&id)?;
    let body = json!({"op":if prepare {"prepare"} else {"run"},"project":inv.project,"manifest_id":inv.manifest_id,"argv":argv,"runtime":runtime,"client":cli.client.name(),"no_cache":no_cache,"no_return":no_return,"min_memory_gb":cli.min_memory_gb,"min_vram_gb":cli.min_vram_gb});
    let fingerprint = inventory::hash(&serde_json::to_vec(&body)?);
    let existing = saved_request(&id)?;
    let target = if let Some(old) = &existing {
        if old.fingerprint != fingerprint {
            return Err(error(
                "This request ID already belongs to different source or arguments; the original request and device are retained",
            ));
        }
        if old.rejected {
            return Err(error(
                "This request ID was definitively rejected; use a new request ID for a new submission",
            ));
        }
        same_explicit_target(cli.device.as_deref(), &old.device)?;
        if (requested == Placement::Laptop && old.device != "laptop")
            || (requested == Placement::Spark && old.device == "laptop")
        {
            return Err(error(
                "Explicit placement conflicts with this request's retained device",
            ));
        }
        retained_target(old)?
    } else {
        let (natural, _) = crate::placement(Placement::Auto, &argv, &inv.root)?;
        let (chosen, _) = crate::placement(requested, &argv, &inv.root)?;
        let explicit = cli.device.as_deref().filter(|id| *id != "auto");
        if explicit.is_some_and(|id| id != "spark" && id != "laptop")
            && (natural == Placement::Laptop || requested == Placement::Laptop)
        {
            return Err(error("This Windows-only command must remain on the laptop"));
        }
        if explicit == Some("spark") && requested == Placement::Laptop {
            return Err(error("--device spark conflicts with --where laptop"));
        }
        if explicit == Some("laptop") && requested == Placement::Spark {
            return Err(error("--device laptop conflicts with --where spark"));
        }
        let legacy = existing_legacy_device(&id)?;
        if let Some(device) = legacy.as_deref() {
            same_explicit_target(explicit, device)?;
        }
        let desired = legacy
            .as_deref()
            .or(explicit)
            .or(if chosen == Placement::Laptop {
                Some("laptop")
            } else if requested == Placement::Spark {
                Some("spark")
            } else {
                None
            });
        select_target(cli, &runtime, &argv, desired)?
    };
    if !target.enabled() && existing.as_ref().is_none_or(|record| record.job.is_none()) {
        return Err(error(
            "The selected device is disabled for new submissions; its retained request will not move",
        ));
    }
    if existing.is_none() {
        legacy_conflict(&inv.project, &inv.root, &id, cli.client.name())?;
    }
    let record = claim_request(
        &id,
        Submission {
            fingerprint,
            device: target.id.clone(),
            profile_root: target.root.clone(),
            target_identity: target.identity()?,
            project: Some(inv.project.clone()),
            manifest_id: Some(inv.manifest_id.clone()),
            job: None,
            complete: false,
            rejected: false,
        },
    )?;
    let args = if prepare {
        vec![
            "prepare".into(),
            inv.root.to_string_lossy().into_owned(),
            "--runtime".into(),
            runtime.clone(),
            "--request-id".into(),
            id.clone(),
        ]
    } else {
        run_args(
            &inv.root,
            &argv,
            &runtime,
            &id,
            if target.id == "laptop" {
                "laptop"
            } else {
                "spark"
            },
            no_cache,
            no_return,
        )
    };
    let result = child(
        &target,
        cli.client.name(),
        args,
        record.manifest_id.as_deref(),
        &[],
        SUBMISSION_TIMEOUT,
    );
    let mut value = match result {
        Ok(value) => value,
        Err(mut failure) => {
            failure.message = format!(
                "{} Retained device: {}. Request ID: {}. Retry this same ID to reconcile; no other device was substituted.",
                failure.message, target.id, id
            );
            return Err(failure);
        }
    };
    record_response(&id, &target, &value)?;
    value["request_id"] = json!(id);
    if detach || prepare || response_failed(&value) {
        annotate(&mut value, &target);
        return Ok(value);
    }
    let job = value["job"]
        .as_str()
        .ok_or_else(|| AppError::new(6, "Device submission omitted its job ID"))?
        .to_string();
    events(
        &json!({"event":"submitted","job":qualify_job(&target.id,&job),"request_id":id,"device":target.id,"placement":target.id}),
    );
    let mut offset = 0;
    loop {
        let mut logs = child(
            &target,
            cli.client.name(),
            vec![
                "logs".into(),
                job.clone(),
                "--since".into(),
                offset.to_string(),
                "--limit".into(),
                inventory::CHUNK.to_string(),
            ],
            None,
            &[],
            Duration::from_secs(45),
        )?;
        if response_failed(&logs) {
            logs["request_id"] = json!(id);
            annotate(&mut logs, &target);
            return Ok(logs);
        }
        let next = logs["next_offset"].as_u64().unwrap_or(offset);
        if logs["data"].as_str().is_some_and(|data| !data.is_empty()) {
            let mut event = logs.clone();
            annotate(&mut event, &target);
            events(&event);
        }
        reconcile(&target, &logs)?;
        if delivery_terminal(&logs) && next.saturating_sub(offset) < inventory::CHUNK as u64 {
            logs["request_id"] = json!(id);
            logs["process_exit_code"] = logs["exit_code"].clone();
            annotate(&mut logs, &target);
            return Ok(logs);
        }
        let consumed = next.saturating_sub(offset);
        offset = next;
        if consumed < inventory::CHUNK as u64 {
            let waited = child(
                &target,
                cli.client.name(),
                vec!["wait".into(), job.clone(), "--timeout".into(), "20".into()],
                None,
                &[],
                Duration::from_secs(65),
            )?;
            reconcile(&target, &waited)?;
            if response_failed(&waited) {
                let mut waited = waited;
                waited["request_id"] = json!(id);
                annotate(&mut waited, &target);
                return Ok(waited);
            }
        }
    }
}

fn service(cli: &Cli) -> Result<Value> {
    let (id, selector, fingerprint_body, manifest_id, args) = match &cli.command {
        Commands::Serve {
            dir,
            name,
            port,
            memory,
            runtime,
            idle_minutes,
            replace,
            request_id,
            argv_json,
            command,
        } => {
            let inv = inventory::collect_readonly(dir)?;
            let argv = crate::load_argv(command.clone(), argv_json.clone())?;
            let (natural, _) = crate::placement(Placement::Auto, &argv, &inv.root)?;
            if natural == Placement::Laptop {
                return Err(error(
                    "A remote model server cannot run a Windows-only command",
                ));
            }
            let id = request_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let mut args = vec!["serve".into(), inv.root.to_string_lossy().into_owned()];
            add_flag(&mut args, "--name", name);
            add_flag(&mut args, "--port", port);
            add_flag(&mut args, "--memory", memory);
            add_flag(&mut args, "--idle-minutes", idle_minutes);
            add_flag(&mut args, "--request-id", &id);
            if let Some(runtime) = runtime {
                add_flag(&mut args, "--runtime", runtime);
            }
            if *replace {
                args.push("--replace".into());
            }
            args.push("--".into());
            args.extend(argv.clone());
            let body = json!({"op":"serve","project":inv.project,"manifest":inv.manifest_id,"argv":argv,"runtime":runtime,"name":name,"port":port,"memory":memory,"idle":idle_minutes,"replace":replace,"client":cli.client.name()});
            (
                id,
                cli.device
                    .clone()
                    .filter(|id| id != "auto")
                    .unwrap_or("spark".into()),
                body,
                Some(inv.manifest_id),
                args,
            )
        }
        Commands::Call {
            name,
            body_json,
            input,
            path,
            content_type,
            save,
            timeout,
            request_id,
        } => {
            let (qualified, name) = split_job(name)?;
            if let Some(device) = qualified {
                same_explicit_target(cli.device.as_deref(), device)?;
            }
            let selector = qualified
                .or(cli.device.as_deref().filter(|device| *device != "auto"))
                .unwrap_or("spark")
                .to_string();
            let body = match (body_json, input) {
                (Some(body), None) if body.len() <= inventory::MIB as usize => {
                    body.as_bytes().to_vec()
                }
                (None, Some(path)) => bounded_file(path, inventory::MIB)?
                    .ok_or_else(|| error("Model request body file is missing"))?,
                (None, None) => Vec::new(),
                _ => return Err(error("Use one bounded model request body or input file")),
            };
            let id = request_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let body_hash = inventory::hash(&body);
            let effective_content_type = content_type.clone().unwrap_or_else(|| {
                if body_json.is_some() {
                    "application/json"
                } else {
                    "application/octet-stream"
                }
                .into()
            });
            let fingerprint = json!({"op":"call","name":name,"body_sha256":body_hash,"path":path,"content_type":effective_content_type,"client":cli.client.name()});
            let mut args = vec!["call".into(), name.into()];
            if let Some(body) = body_json {
                add_flag(&mut args, "--body-json", body);
            }
            if let Some(input) = input {
                add_flag(&mut args, "--input", input.to_string_lossy());
            }
            add_flag(&mut args, "--path", path);
            add_flag(&mut args, "--timeout", timeout);
            add_flag(&mut args, "--request-id", &id);
            if let Some(content_type) = content_type {
                add_flag(&mut args, "--content-type", content_type);
            }
            if let Some(save) = save {
                add_flag(&mut args, "--save", save.to_string_lossy());
            }
            (id, selector, fingerprint, None, args)
        }
        _ => return Err(error("This operation is not a model service request")),
    };
    state::validate_id(&id)?;
    if selector == "laptop" {
        return Err(error("Model services require a remote device"));
    }
    let fingerprint = inventory::hash(&serde_json::to_vec(&fingerprint_body)?);
    let existing = saved_request(&id)?;
    let target = if let Some(record) = existing {
        if record.fingerprint != fingerprint {
            return Err(error(
                "This service request ID belongs to a different immutable request",
            ));
        }
        if record.rejected {
            return Err(error(
                "This service request ID was definitively rejected; use a new request ID",
            ));
        }
        same_explicit_target(Some(&selector), &record.device)?;
        retained_target(&record)?
    } else {
        if let Some(legacy) = existing_legacy_device(&id)? {
            same_explicit_target(Some(&selector), &legacy)?;
        }
        Target::load(&selector)?
    };
    if !target.enabled() {
        return Err(error(
            "The selected device is disabled for new model service requests",
        ));
    }
    let record = claim_request(
        &id,
        Submission {
            fingerprint,
            device: target.id.clone(),
            profile_root: target.root.clone(),
            target_identity: target.identity()?,
            project: None,
            manifest_id,
            job: None,
            complete: false,
            rejected: false,
        },
    )?;
    let expected_body = fingerprint_body["body_sha256"].as_str();
    let mut value = child_with_body(
        &target,
        cli.client.name(),
        args,
        record.manifest_id.as_deref(),
        expected_body,
        &[],
        SUBMISSION_TIMEOUT,
    )
    .map_err(|mut failure| {
        failure.message = format!(
            "{} Retained device: {}. Request ID: {}.",
            failure.message, target.id, id
        );
        failure
    })?;
    record_response(&id, &target, &value)?;
    value["request_id"] = json!(id);
    annotate(&mut value, &target);
    Ok(value)
}

fn simple_args(command: &Commands, job_name: Option<&str>) -> Result<(Vec<String>, Vec<u8>)> {
    let mut args = Vec::new();
    let mut input = Vec::new();
    match command {
        Commands::Doctor => args.push("doctor".into()),
        Commands::Sync { dir, check } => {
            args.extend(["sync".into(), dir.to_string_lossy().into_owned()]);
            if *check {
                args.push("--check".into());
            }
        }
        Commands::Connect => args.push("connect".into()),
        Commands::Disconnect => args.push("disconnect".into()),
        Commands::Connection {
            mode,
            laptop_jobs,
            auto_addresses,
        } => {
            args.push("connection".into());
            if let Some(mode) = mode {
                add_flag(
                    &mut args,
                    "--mode",
                    match mode {
                        state::ConnectionMode::LanOnly => "lan-only",
                        state::ConnectionMode::RemoteOnly => "remote-only",
                        state::ConnectionMode::Both => "both",
                    },
                );
            }
            if let Some(jobs) = laptop_jobs {
                add_flag(&mut args, "--laptop-jobs", jobs);
            }
            if let Some(value) = auto_addresses {
                add_flag(
                    &mut args,
                    "--auto-addresses",
                    if matches!(value, Switch::On) {
                        "on"
                    } else {
                        "off"
                    },
                );
            }
        }
        Commands::Resources {
            file,
            stdin,
            internet,
        } => {
            args.push("resources".into());
            if let Some(file) = file {
                add_flag(&mut args, "--file", file.to_string_lossy());
            }
            if *stdin {
                args.push("--stdin".into());
                std::io::stdin().take(65537).read_to_end(&mut input)?;
                if input.len() > 65536 {
                    return Err(error("Resource input exceeds its size limit"));
                }
            }
            if let Some(value) = internet {
                add_flag(
                    &mut args,
                    "--internet",
                    if matches!(value, Switch::On) {
                        "on"
                    } else {
                        "off"
                    },
                );
            }
        }
        Commands::Downloads { keep_gb, free_up } => {
            args.push("downloads".into());
            if let Some(keep) = keep_gb {
                add_flag(&mut args, "--keep-gb", keep);
            }
            if *free_up {
                args.push("--free-up".into());
            }
        }
        Commands::Call {
            name,
            body_json,
            input: source,
            path,
            content_type,
            save,
            timeout,
            request_id,
        } => {
            args.extend(["call".into(), job_name.unwrap_or(name).into()]);
            if let Some(body) = body_json {
                add_flag(&mut args, "--body-json", body);
            }
            if let Some(file) = source {
                add_flag(&mut args, "--input", file.to_string_lossy());
            }
            add_flag(&mut args, "--path", path);
            if let Some(content_type) = content_type {
                add_flag(&mut args, "--content-type", content_type);
            }
            if let Some(file) = save {
                add_flag(&mut args, "--save", file.to_string_lossy());
            }
            add_flag(&mut args, "--timeout", timeout);
            if let Some(id) = request_id {
                add_flag(&mut args, "--request-id", id);
            }
        }
        _ => return Err(error("This command cannot be redirected to a named device")),
    }
    Ok((args, input))
}

pub(crate) fn dispatch(
    cli: &Cli,
    raw: &[String],
    events: &mut dyn FnMut(&Value),
) -> Result<Option<Value>> {
    // Preserve the legacy before-I/O owner gates. Binding a parsed default
    // client to a child must never turn an implicit human into explicit owner
    // approval for internet, cache changes or Find my Spark.
    match &cli.command {
        Commands::Discover { .. } => crate::owner_only(raw, cli.client)?,
        // A password sign-in, and replacing the Spark connection, are the owner's.
        Commands::Node { command: NodeCommands::Authorize { .. } } if !crate::explicitly_owner(raw, cli.client) => {
            return Err(error(crate::OWNER_SIGN_IN));
        }
        Commands::Node { command: NodeCommands::Trust { id, .. } } if id == "spark" && !crate::explicitly_owner(raw, cli.client) => {
            return Err(error(crate::OWNER_SIGN_IN));
        }
        Commands::Resources {
            internet: Some(_), ..
        } if !crate::explicitly_owner(raw, cli.client) => {
            return Err(error(
                "Only the owner can change job internet; use exactly one --client human.",
            ));
        }
        Commands::Downloads { keep_gb, free_up }
            if (keep_gb.is_some() || *free_up) && !crate::explicitly_owner(raw, cli.client) =>
        {
            return Err(error(
                "Only the owner can change saved downloads; use exactly one --client human.",
            ));
        }
        _ => {}
    }
    if cli.device.as_deref() == Some("laptop")
        && !matches!(
            cli.command,
            Commands::Run { .. }
                | Commands::Plan { .. }
                | Commands::Status { .. }
                | Commands::Devices
                | Commands::Wait { .. }
                | Commands::Logs { .. }
                | Commands::Cancel { .. }
                | Commands::Outputs { .. }
        )
    {
        return Err(error(
            "This operation manages a remote device and cannot be applied to --device laptop; use laptop run, plan, status or job inspection",
        ));
    }
    if let Commands::Agent { command } = &cli.command {
        if matches!(command, crate::AgentCommands::Signout { .. }) && !crate::explicitly_owner(raw, cli.client) {
            return Err(error("Removing an agent sign-in from the Spark is an owner action; run it from RTXBridge or with --client human"));
        }
        return crate::agent_command(cli, command, events).map(Some);
    }
    if let Commands::Node { command } = &cli.command {
        if cli.device.is_some() || cli.min_memory_gb.is_some() || cli.min_vram_gb.is_some() {
            return Err(error(
                "Node management does not accept device or capacity selectors",
            ));
        }
        return node_crud(command).map(Some);
    }
    if matches!(cli.command, Commands::Bootstrap { .. })
        && cli
            .device
            .as_deref()
            .is_none_or(|id| matches!(id, "spark" | "laptop" | "auto"))
    {
        return Err(error(
            "Bootstrap requires an explicit registered --device ID; the existing Spark installation is preserved",
        ));
    }
    let registered = nodes::list()?;
    let qualified = match &cli.command {
        Commands::Wait { job, .. }
        | Commands::Logs { job, .. }
        | Commands::Cancel { job }
        | Commands::Outputs { job } => job.contains(':'),
        _ => false,
    };
    let constrained = cli.min_memory_gb.is_some() || cli.min_vram_gb.is_some();
    if registered.is_empty() && cli.device.is_none() && !qualified && !constrained {
        return Ok(None);
    }
    if constrained
        && !matches!(
            cli.command,
            Commands::Run { .. } | Commands::Plan { .. } | Commands::Prepare { .. }
        )
    {
        return Err(error(
            "Minimum memory selectors apply only to run, plan or prepare",
        ));
    }
    match &cli.command {
        Commands::Devices | Commands::Status { .. } => aggregate(cli).map(Some),
        Commands::Wait { job, .. }
        | Commands::Logs { job, .. }
        | Commands::Cancel { job }
        | Commands::Outputs { job } => route_job(cli, job).map(Some),
        Commands::Plan {
            dir,
            runtime,
            argv_json,
            r#where,
            command,
        } => {
            let argv = crate::load_argv(command.clone(), argv_json.clone())?;
            plan(cli, dir, runtime, &argv, *r#where).map(Some)
        }
        Commands::Run {
            dir,
            detach,
            runtime,
            argv_json,
            request_id,
            r#where,
            no_cache,
            no_return,
            command,
        } => {
            let argv = crate::load_argv(command.clone(), argv_json.clone())?;
            let runtime = runtime
                .clone()
                .unwrap_or_else(|| crate::default_runtime(&argv).into());
            submit(
                cli,
                dir,
                argv,
                runtime,
                request_id.clone(),
                *r#where,
                *no_cache,
                *no_return,
                *detach,
                false,
                events,
            )
            .map(Some)
        }
        Commands::Prepare {
            dir,
            runtime,
            request_id,
        } => submit(
            cli,
            dir,
            vec![
                "/usr/bin/python3".into(),
                "/sparkdev-prepare.py".into(),
                runtime.clone(),
            ],
            runtime.clone(),
            request_id.clone(),
            Placement::Spark,
            true,
            false,
            true,
            true,
            events,
        )
        .map(Some),
        Commands::Serve { .. } | Commands::Call { .. } => service(cli).map(Some),
        Commands::Bootstrap { bundle, cuda } => {
            let Some(id) = cli
                .device
                .as_deref()
                .filter(|id| !matches!(*id, "spark" | "laptop" | "auto"))
            else {
                return Err(error(
                    "Bootstrap requires an explicit registered --device ID; the existing Spark installation is preserved",
                ));
            };
            let target = Target::load(id)?;
            if !target.enabled() {
                return Err(error("The selected device is disabled"));
            }
            let mut args = vec![
                "bootstrap".into(),
                "--bundle".into(),
                bundle.to_string_lossy().into_owned(),
            ];
            if *cuda {
                args.push("--cuda".into());
            }
            let mut value = child(
                &target,
                cli.client.name(),
                args,
                None,
                &[],
                SUBMISSION_TIMEOUT,
            )?;
            annotate(&mut value, &target);
            Ok(Some(value))
        }
        Commands::Setup { .. } | Commands::Pair | Commands::Discover { .. } | Commands::Mcp => {
            if cli
                .device
                .as_deref()
                .is_some_and(|id| id != "spark" && id != "auto")
            {
                return Err(error(
                    "This operation belongs to the retained Spark configuration; node identities cannot be replaced",
                ));
            }
            Ok(None)
        }
        _ => {
            let Some(selector) = cli.device.as_deref().filter(|id| *id != "auto") else {
                return Ok(None);
            };
            if matches!(
                &cli.command,
                Commands::Resources {
                    internet: Some(_),
                    ..
                } | Commands::Downloads {
                    keep_gb: Some(_),
                    ..
                } | Commands::Downloads { free_up: true, .. }
            ) && !crate::explicitly_owner(raw, cli.client)
            {
                return Err(error(
                    "Changing owner internet or download policy requires exactly one explicit --client human",
                ));
            }
            let target = Target::load(selector)?;
            if matches!(cli.command, Commands::Call { .. }) && !target.enabled() {
                return Err(error("The selected device is disabled for new model calls"));
            }
            let (args, input) = simple_args(&cli.command, None)?;
            let mut value = child(
                &target,
                cli.client.name(),
                args,
                None,
                &input,
                SUBMISSION_TIMEOUT,
            )?;
            annotate(&mut value, &target);
            Ok(Some(value))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn record(device: &str, project: &str) -> Submission {
        Submission {
            fingerprint: "a".repeat(64),
            device: device.into(),
            profile_root: state::root().unwrap(),
            target_identity: "b".repeat(64),
            project: Some(project.into()),
            manifest_id: Some("c".repeat(64)),
            job: None,
            complete: false,
            rejected: false,
        }
    }

    fn test_config() -> state::Config {
        serde_json::from_value(
            json!({"ssh_alias":"test-node","hosts":["192.0.2.7"],"host_key_alias":"test-pin"}),
        )
        .unwrap()
    }

    fn target_fixture(device: &str) -> Target {
        let root = state::root().unwrap();
        state::atomic_json(&root.join("config.json"), &test_config()).unwrap();
        Target {
            id: device.into(),
            label: device.into(),
            root,
            node: None,
        }
    }

    fn request_for(target: &Target, project: &str) -> Submission {
        let mut value = record(&target.id, project);
        value.profile_root = target.root.clone();
        value.target_identity = target.identity().unwrap();
        value
    }

    #[test]
    fn request_identity_and_claims_are_atomic_and_uncertain_work_cannot_move() {
        let home = state::TestHome::new();
        let original = record("antonpc", "project-a");
        claim_request("request-a", original.clone()).unwrap();
        let bytes = fs::read(home.path.join("fleet.json")).unwrap();
        assert_eq!(
            claim_request("request-a", original.clone()).unwrap().device,
            "antonpc"
        );
        let mut moved = original.clone();
        moved.device = "other".into();
        assert!(claim_request("request-a", moved).is_err());
        let mut changed = original.clone();
        changed.fingerprint = "d".repeat(64);
        assert!(claim_request("request-a", changed).is_err());
        assert!(claim_request("request-b", record("other", "project-a")).is_err());
        assert_eq!(fs::read(home.path.join("fleet.json")).unwrap(), bytes);
        assert!(journal().unwrap().claims.contains_key("project-a"));
    }

    #[test]
    fn workload_exit_does_not_release_a_claim_until_delivery_is_terminal() {
        let _home = state::TestHome::new();
        let target = target_fixture("antonpc");
        claim_request("request-a", request_for(&target, "project-a")).unwrap();
        record_response("request-a",&target,&json!({"job":"job-a","state":"returning","workload_state":"succeeded","return_status":"pending"})).unwrap();
        assert!(claim_request("request-b", record("other", "project-a")).is_err());
        assert!(!delivery_terminal(
            &json!({"state":"succeeded","return_status":"retrying"})
        ));
        record_response(
            "request-a",
            &target,
            &json!({"job":"job-a","state":"succeeded","return_status":"complete"}),
        )
        .unwrap();
        assert!(journal().unwrap().claims.is_empty());
        assert!(claim_request("request-b", record("other", "project-a")).is_ok());
    }

    #[test]
    fn local_retry_failure_keeps_the_original_accepted_job_claimed() {
        let _home = state::TestHome::new();
        let target = target_fixture("antonpc");
        claim_request("request-a", request_for(&target, "project-a")).unwrap();
        record_response(
            "request-a",
            &target,
            &json!({"job":"original-job","state":"running"}),
        )
        .unwrap();
        record_response(
            "request-a",
            &target,
            &json!({"ok":false,"code":7,"message":"authentication failed"}),
        )
        .unwrap();
        assert_eq!(
            saved_request("request-a").unwrap().unwrap().job.as_deref(),
            Some("original-job")
        );
        assert!(claim_request("new-request", record("spark", "project-a")).is_err());
    }

    #[test]
    fn detached_worker_receipt_recovers_an_uncertain_foreground_submission() {
        let _home = state::TestHome::new();
        let target = target_fixture("antonpc");
        claim_request("request-a", request_for(&target, "project-a")).unwrap();
        state::atomic_json(&target.root.join("returns/request-a.json"),&json!({"v":1,"request_id":"request-a","request":{"request_id":"request-a","project":"project-a","manifest_id":"c".repeat(64)},"job":"worker-accepted-job","complete":false})).unwrap();
        record_response(
            "request-a",
            &target,
            &json!({"ok":false,"code":5,"message":"source changed on retry"}),
        )
        .unwrap();
        let recovered = saved_request("request-a").unwrap().unwrap();
        assert_eq!(recovered.job.as_deref(), Some("worker-accepted-job"));
        assert!(!recovered.complete);
        assert!(claim_request("new-request", record("spark", "project-a")).is_err());
        reconcile(&target,&json!({"jobs":[{"job":"worker-accepted-job","state":"succeeded","return_status":"complete"}]})).unwrap();
        assert!(journal().unwrap().claims.is_empty());
    }

    #[test]
    fn retained_jobs_reject_ssh_identity_edits_and_remain_readable_when_disabled() {
        let _home = state::TestHome::new();
        let node = nodes::Node {
            id: "antonpc".into(),
            label: "Anton".into(),
            enabled: true,
            priority: 0,
            endpoint: nodes::Endpoint::LinuxSsh,
            ssh_config: None,
        };
        nodes::add(node, test_config()).unwrap();
        let target = Target::load("antonpc").unwrap();
        let request = request_for(&target, "project-a");
        nodes::set_enabled("antonpc", &false).unwrap();
        assert!(retained_target(&request).is_ok());
        let mut different = test_config();
        different.host_key_alias = "different-pin".into();
        state::atomic_json(&target.root.join("config.json"), &different).unwrap();
        assert!(retained_target(&request).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn physical_profile_alias_preserves_retry_recovery_and_completion_in_both_directions() {
        let _home = state::TestHome::new();
        nodes::add(
            nodes::Node {
                id: "antonpc".into(),
                label: "Anton".into(),
                enabled: true,
                priority: 0,
                endpoint: nodes::Endpoint::LinuxSsh,
                ssh_config: None,
            },
            test_config(),
        )
        .unwrap();
        let target = Target::load("antonpc").unwrap();
        let alias = location::loopback_alias(&target.root, "localhost");
        if !alias.is_dir() {
            eprintln!(
                "Local administrative share unavailable; profile alias fixture not exercised"
            );
            return;
        }
        let mut alternate = target.clone();
        alternate.root = alias.clone();
        let original = request_for(&alternate, "project-a");
        claim_request("request-a", original.clone()).unwrap();
        let retry = claim_request("request-a", request_for(&target, "project-a")).unwrap();
        assert_eq!(retry.profile_root, alias); // Never rewrite the retained I/O path.
        assert!(retained_target(&original).is_ok());
        state::atomic_json(&target.root.join("returns/request-a.json"), &json!({
            "v":1,"request_id":"request-a","request":{"request_id":"request-a","project":"project-a","manifest_id":"c".repeat(64)},
            "job":"recovered-job","complete":false,
        })).unwrap();
        recover_jobs(&target).unwrap();
        assert_eq!(
            saved_request("request-a").unwrap().unwrap().job.as_deref(),
            Some("recovered-job")
        );
        record_response(
            "request-a",
            &target,
            &json!({"job":"recovered-job","state":"returning","return_status":"pending"}),
        )
        .unwrap();
        assert!(journal().unwrap().claims.contains_key("project-a"));
        reconcile(&target, &json!({"jobs":[{"job":"recovered-job","state":"succeeded","return_status":"complete"}]})).unwrap();
        assert!(!journal().unwrap().claims.contains_key("project-a"));
        assert_eq!(
            saved_request("request-a").unwrap().unwrap().profile_root,
            alias
        );

        claim_request("request-b", request_for(&target, "project-b")).unwrap();
        record_response(
            "request-b",
            &alternate,
            &json!({"job":"reverse-job","state":"running"}),
        )
        .unwrap();
        reconcile(
            &alternate,
            &json!({"jobs":[{"job":"reverse-job","state":"succeeded","return_status":"complete"}]}),
        )
        .unwrap();
        assert!(!journal().unwrap().claims.contains_key("project-b"));
        assert_eq!(
            saved_request("request-b").unwrap().unwrap().profile_root,
            target.root
        );

        let other = target.root.join("different-state");
        fs::create_dir(&other).unwrap();
        let mut redirected = original;
        redirected.profile_root = other;
        assert!(claim_request("request-a", redirected.clone()).is_err());
        assert!(retained_target(&redirected).is_err());
    }

    #[test]
    fn definitive_rejection_requires_a_new_id_and_never_reopens_an_unclaimed_job() {
        let _home = state::TestHome::new();
        let target = target_fixture("antonpc");
        let original = request_for(&target, "project-a");
        claim_request("request-a", original.clone()).unwrap();
        record_response(
            "request-a",
            &target,
            &json!({"ok":false,"code":7,"message":"authentication failed"}),
        )
        .unwrap();
        assert!(saved_request("request-a").unwrap().unwrap().rejected);
        assert!(journal().unwrap().claims.is_empty());
        assert!(
            claim_request("request-a", original)
                .unwrap_err()
                .message
                .contains("new request ID")
        );
        assert!(claim_request("new-request", record("spark", "project-a")).is_ok());
    }

    #[test]
    fn durable_no_job_refusal_releases_claim_but_ambiguous_failure_does_not() {
        let _home = state::TestHome::new();
        let target = target_fixture("antonpc");
        claim_request("request-a", request_for(&target, "project-a")).unwrap();
        let mut receipt = json!({"v":1,"request_id":"request-a","request":{"request_id":"request-a","project":"project-a","manifest_id":"c".repeat(64)},"job":null,"submission":null,"complete":true,"error":"Transport timeout; request may have completed"});
        state::atomic_json(&target.root.join("returns/request-a.json"), &receipt).unwrap();
        reconcile(&target, &json!({"jobs":[]})).unwrap();
        assert!(journal().unwrap().claims.contains_key("project-a"));
        receipt["error"] = json!(
            "Submission rejected; no automatic retry: runtime_unavailable: runtime not provisioned"
        );
        state::atomic_json(&target.root.join("returns/request-a.json"), &receipt).unwrap();
        reconcile(&target, &json!({"jobs":[]})).unwrap();
        assert!(
            journal().unwrap().claims.is_empty(),
            "an empty fresh board still reconciles the detached worker's definitive refusal"
        );
        record_response("request-a",&target,&json!({"ok":false,"code":6,"message":"Submission rejected; no automatic retry: runtime_unavailable: runtime not provisioned"})).unwrap();
        let rejected = saved_request("request-a").unwrap().unwrap();
        assert!(rejected.complete && rejected.rejected);
        assert!(rejected.job.is_none());
        assert!(journal().unwrap().claims.is_empty());
        assert!(claim_request("request-a", request_for(&target, "project-a")).is_err());
        assert!(claim_request("new-request", record("spark", "project-a")).is_ok());
    }

    #[test]
    fn legacy_failure_and_healthy_named_device_are_observed_in_one_parallel_wave() {
        let registrations = vec![nodes::Node {
            id: "antonpc".into(),
            label: "Anton".into(),
            enabled: true,
            priority: 0,
            endpoint: nodes::Endpoint::LinuxSsh,
            ssh_config: None,
        }];
        let (sender, receiver) = std::sync::mpsc::channel();
        let (legacy, named) = parallel_observations(
            &registrations,
            move || {
                receiver.recv_timeout(Duration::from_secs(1)).map_err(|_| {
                    AppError::new(
                        9,
                        "Named observation waited behind the failed legacy device",
                    )
                })?;
                Err(AppError::new(4, "legacy offline"))
            },
            |node| {
                sender.send(()).unwrap();
                json!({"id":node.id,"reachable":true,"jobs":[{"job":"antonpc:retained-job","state":"running"}]})
            },
        );
        assert_eq!(legacy.unwrap_err().code, 4);
        assert_eq!(named.len(), 1);
        assert_eq!(named[0]["id"], "antonpc");
        assert_eq!(named[0]["reachable"], true);
        assert_eq!(named[0]["jobs"][0]["job"], "antonpc:retained-job");
        assert!(OBSERVATION_TIMEOUT < Duration::from_secs(40));
    }

    #[test]
    fn local_metadata_limit_is_an_incomplete_observation_not_network_failure() {
        let _home = state::TestHome::new();
        let limited = || {
            AppError::new(
                6,
                format!("{}: bounded response", transport::LOCAL_METADATA_LIMIT),
            )
        };
        let named = observation_failure("antonpc", "Anton", true, limited());
        assert_eq!(named["reachable"], Value::Null);
        assert_eq!(named["availability"], "observation_limited");
        assert_eq!(named["error"]["kind"], "local_metadata_limit");
        let legacy = legacy_unavailable(json!({"code":6,"message":limited().message}), true);
        assert_eq!(legacy["devices"][0]["reachable"], true);
        assert_eq!(legacy["devices"][1]["reachable"], Value::Null);
        assert_eq!(legacy["devices"][1]["availability"], "observation_limited");
        let status = legacy_unavailable(json!({"code":6,"message":limited().message}), false);
        assert_eq!(status["remote_available"], Value::Null);
        let offline = observation_failure("antonpc", "Anton", true, AppError::new(4, "offline"));
        assert_eq!(offline["reachable"], false);
        assert_eq!(offline["availability"], "unavailable");
    }

    #[test]
    fn owner_gates_and_bootstrap_scope_refuse_before_device_lookup() {
        let home = state::TestHome::new();
        for args in [
            vec![
                "--client",
                "codex",
                "--device",
                "unknown",
                "resources",
                "--internet",
                "on",
            ],
            vec!["--device", "unknown", "downloads", "--free-up"],
        ] {
            let args = args.into_iter().map(String::from).collect::<Vec<_>>();
            let cli = Cli::try_parse_from(std::iter::once("sparkdev".into()).chain(args.clone()))
                .unwrap();
            let failure = dispatch(&cli, &args, &mut |_| {}).unwrap_err();
            assert_eq!(failure.code, 2);
            assert!(failure.message.contains("Only the owner"));
        }
        let args = vec![
            "bootstrap".into(),
            "--bundle".into(),
            "missing-bundle".into(),
        ];
        let cli =
            Cli::try_parse_from(std::iter::once("sparkdev".into()).chain(args.clone())).unwrap();
        assert!(
            dispatch(&cli, &args, &mut |_| {})
                .unwrap_err()
                .message
                .contains("explicit registered")
        );
        assert!(!home.path.join("fleet.json").exists());
        assert!(!home.path.join("nodes.json").exists());
    }

    #[test]
    fn explicit_laptop_cannot_mutate_or_relabel_remote_spark_controls() {
        let home = state::TestHome::new();
        for command in [
            vec!["resources", "--internet", "off"],
            vec!["resources"],
            vec!["downloads", "--free-up"],
            vec!["connect"],
            vec!["disconnect"],
            vec!["connection"],
            vec!["doctor"],
            vec!["sync", "missing-project"],
            vec!["connection", "--laptop-jobs", "2"],
        ] {
            let args = ["--client", "human", "--device", "laptop"]
                .into_iter()
                .chain(command)
                .map(String::from)
                .collect::<Vec<_>>();
            let cli = Cli::try_parse_from(std::iter::once("sparkdev".into()).chain(args.clone()))
                .unwrap();
            let failure = dispatch(&cli, &args, &mut |_| {}).unwrap_err();
            assert_eq!(failure.code, 2);
            assert!(failure.message.contains("--device laptop"));
        }
        assert_eq!(
            fs::read_dir(&home.path).unwrap().count(),
            0,
            "rejected laptop controls must not touch the Spark configuration, registry or fleet journal"
        );
    }

    #[test]
    fn laptop_job_scope_never_forwards_remote_ids_or_server_names_to_spark() {
        let home = state::TestHome::new();
        let remote = "0123456789abcdef0123456789abcdef";
        for reference in [
            remote,
            "server-name",
            "laptop:server-name",
            "laptop:0123456789abcdef0123456789abcdef",
        ] {
            for operation in ["wait", "logs", "cancel", "outputs"] {
                let args = [
                    "--client", "codex", "--device", "laptop", operation, reference,
                ]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>();
                let cli =
                    Cli::try_parse_from(std::iter::once("sparkdev".into()).chain(args.clone()))
                        .unwrap();
                let failure = dispatch(&cli, &args, &mut |_| {}).unwrap_err();
                assert_eq!(failure.code, 2);
                assert!(failure.message.contains("native laptop- job ID"));
            }
        }
        for operation in ["wait", "logs", "cancel"] {
            for (selector, reference, expected_device, expected_job) in [
                (None, "laptop-native-job", "laptop", "laptop-native-job"),
                (
                    Some("laptop"),
                    "laptop:laptop-native-job",
                    "laptop",
                    "laptop-native-job",
                ),
                (None, remote, "spark", remote),
                (Some("spark"), remote, "spark", remote),
                (Some("antonpc"), remote, "antonpc", remote),
            ] {
                let mut args: Vec<String> = vec!["--client".into(), "codex".into()];
                if let Some(selector) = selector {
                    args.extend(["--device".into(), selector.into()]);
                }
                args.extend([operation.into(), reference.into()]);
                let cli =
                    Cli::try_parse_from(std::iter::once("sparkdev".into()).chain(args)).unwrap();
                assert_eq!(
                    job_target(&cli, reference).unwrap(),
                    (expected_device, expected_job)
                );
            }
        }
        let args = ["outputs", "laptop-native-job"]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        let cli = Cli::try_parse_from(std::iter::once("sparkdev".into()).chain(args)).unwrap();
        assert!(
            job_target(&cli, "laptop-native-job")
                .unwrap_err()
                .message
                .contains("do not have a remote output manifest")
        );
        assert_eq!(
            fs::read_dir(&home.path).unwrap().count(),
            0,
            "scope validation cannot write configuration, jobs or fleet state"
        );
    }

    #[test]
    fn qualified_jobs_cannot_escape_or_move_and_legacy_ids_remain_raw() {
        assert_eq!(
            split_job("antonpc:job-123").unwrap(),
            (Some("antonpc"), "job-123")
        );
        assert_eq!(split_job("job-123").unwrap(), (None, "job-123"));
        assert_eq!(qualify_job("spark", "job-123"), "job-123");
        assert_eq!(qualify_job("laptop", "laptop-123"), "laptop-123");
        assert_eq!(qualify_job("antonpc", "job-123"), "antonpc:job-123");
        for value in [
            "../outside:job",
            "antonpc:../job",
            "antonpc:other:job",
            "auto:job",
            "ANTONPC:job",
        ] {
            assert!(split_job(value).is_err(), "{value}");
        }
        assert!(same_explicit_target(Some("other"), "antonpc").is_err());
        assert!(same_explicit_target(Some("auto"), "antonpc").is_ok());
    }

    #[test]
    fn workload_flags_are_data_and_child_scope_cannot_be_overridden() {
        let args = run_args(
            Path::new("C:/project"),
            &[
                "python3".into(),
                "script.py".into(),
                "--device".into(),
                "other".into(),
                "--where".into(),
                "laptop".into(),
                "--request-id".into(),
                "workload-id".into(),
            ],
            "python",
            "retained-id",
            "spark",
            true,
            false,
        );
        let delimiter = args.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(
            &args[delimiter + 1..],
            &[
                "python3",
                "script.py",
                "--device",
                "other",
                "--where",
                "laptop",
                "--request-id",
                "workload-id"
            ]
        );
        assert_eq!(
            args[args.iter().position(|arg| arg == "--request-id").unwrap() + 1],
            "retained-id"
        );
        assert_eq!(
            args[args.iter().position(|arg| arg == "--where").unwrap() + 1],
            "spark"
        );
    }

    #[test]
    fn old_unscoped_commands_and_readonly_plans_do_not_create_fleet_state() {
        let home = state::TestHome::new();
        let args = vec!["--client".into(), "codex".into(), "devices".into()];
        let cli =
            Cli::try_parse_from(std::iter::once("sparkdev".into()).chain(args.clone())).unwrap();
        assert!(dispatch(&cli, &args, &mut |_| {}).unwrap().is_none());
        assert!(!home.path.join("fleet.json").exists());
        assert!(!home.path.join("fleet.lock").exists());
        let tree = tempfile::tempdir().unwrap();
        fs::write(tree.path().join("example.py"), b"print(1)\n").unwrap();
        let args = vec![
            "--device".into(),
            "auto".into(),
            "plan".into(),
            tree.path().to_string_lossy().into_owned(),
            "--".into(),
            "python3".into(),
            "example.py".into(),
        ];
        let cli =
            Cli::try_parse_from(std::iter::once("sparkdev".into()).chain(args.clone())).unwrap();
        let preview = dispatch(&cli, &args, &mut |_| {}).unwrap().unwrap();
        assert_eq!(preview["device_selection"], "deferred_until_run");
        assert_eq!(preview["claimed"], false);
        assert_eq!(preview["submitted"], false);
        assert!(!home.path.join("fleet.json").exists());
        assert!(!home.path.join("fleet.lock").exists());
        assert!(!home.path.join("cas").exists());
    }

    #[test]
    fn corrupt_journal_never_loses_claims_by_defaulting_to_empty() {
        let home = state::TestHome::new();
        fs::write(
            home.path.join("fleet.json"),
            br#"{"v":9,"requests":{},"claims":{}}"#,
        )
        .unwrap();
        assert!(journal().is_err());
        assert!(claim_request("request-a", record("antonpc", "project-a")).is_err());
        assert_eq!(
            fs::read(home.path.join("fleet.json")).unwrap(),
            br#"{"v":9,"requests":{},"claims":{}}"#
        );
    }

    #[test]
    fn discrete_gpu_fit_does_not_substitute_system_ram_or_unknown_memory() {
        let target = Target {
            id: "antonpc".into(),
            label: "Anton".into(),
            root: PathBuf::new(),
            node: Some(nodes::Node {
                id: "antonpc".into(),
                label: "Anton".into(),
                enabled: true,
                priority: 0,
                endpoint: nodes::Endpoint::LinuxSsh,
                ssh_config: None,
            }),
        };
        let value = json!({"transport":{"authenticated":true},"scheduler":{"healthy":true},"runtimes":["cuda"],"jobs":[],"managed_limits":{"cpu_total":8,"cpu_per_job":4,"memory_gb_total":64,"memory_gb_per_job":8,"headroom_gb":32},"capabilities":{"system_memory":{"total_bytes":256*inventory::GIB,"available_bytes":200*inventory::GIB},"gpu":{"observed":true,"memory_model":"discrete","devices":[{"total_memory_bytes":24*inventory::GIB,"free_memory_bytes":20*inventory::GIB}]}}});
        let observed = observation(&target, &value);
        assert_eq!(observed.gpu_vram_bytes, Some(20 * inventory::GIB));
        assert_eq!(observed.available_memory_bytes, Some(8 * inventory::GIB));
        let required = nodes::SelectionRequirements {
            runtime: "cuda".into(),
            min_gpu_vram_bytes: Some(32 * inventory::GIB),
            ..Default::default()
        };
        assert!(nodes::select(&[observed], &required).is_err());
        let required = nodes::SelectionRequirements {
            runtime: "cuda".into(),
            min_memory_bytes: Some(32 * inventory::GIB),
            ..Default::default()
        };
        assert!(nodes::select(&[observation(&target, &value)], &required).is_err());
        let old = json!({"transport":{"authenticated":true},"scheduler":{"healthy":true},"runtimes":["cuda"],"managed_limits":{"cpu_total":8,"memory_gb_total":64,"headroom_gb":32},"hardware":{"memory_gb":256}});
        assert!(observation(&target, &old).gpu_vram_bytes.is_none());
        assert!(observation(&target, &old).available_memory_bytes.is_none());
    }

    #[test]
    fn auto_uses_an_idle_spark_instead_of_a_saturated_preferred_4090() {
        let preferred = nodes::Node {
            id: "antonpc".into(),
            label: "4090".into(),
            enabled: true,
            priority: 1000,
            endpoint: nodes::Endpoint::LinuxSsh,
            ssh_config: None,
        };
        let anton = Target {
            id: "antonpc".into(),
            label: "4090".into(),
            root: PathBuf::new(),
            node: Some(preferred),
        };
        let spark = Target {
            id: "spark".into(),
            label: "Spark".into(),
            root: PathBuf::new(),
            node: None,
        };
        let idle = json!({"transport":{"authenticated":true},"scheduler":{"healthy":true},"runtimes":["python","cuda"],"jobs":[],"managed_limits":{"cpu_total":8,"cpu_per_job":4,"memory_gb_total":32,"memory_gb_per_job":8,"headroom_gb":32,"client_jobs":2,"client_cpu":8},"capabilities":{"system_memory":{"available_bytes":128*inventory::GIB}}});
        let mut busy = idle.clone();
        busy["jobs"] = json!([{"state":"running","where":"spark","reserved_cpu":8,"reserved_memory_gb":32,"client":"codex","kind":"batch"}]);
        assert!(!default_job_fits(&busy, "codex"));
        assert!(default_job_fits(&idle, "codex"));
        let observations = [(&anton, &busy), (&spark, &idle)]
            .into_iter()
            .filter(|(_, value)| default_job_fits(value, "codex"))
            .map(|(target, value)| observation(target, value))
            .collect::<Vec<_>>();
        assert_eq!(
            nodes::select(
                &observations,
                &nodes::SelectionRequirements {
                    runtime: "python".into(),
                    ..Default::default()
                }
            )
            .unwrap()
            .id,
            "spark"
        );
        busy["jobs"] = json!([{"state":"running","where":"spark","reserved_cpu":6,"reserved_memory_gb":8,"client":"codex","kind":"batch"}]);
        assert!(
            !default_job_fits(&busy, "codex"),
            "two CPU budget units cannot admit this node's four-CPU default job"
        );
        let mut unknown = idle.clone();
        unknown["jobs"] = json!([{"state":"running","where":"spark"}]);
        assert!(
            !default_job_fits(&unknown, "codex"),
            "unknown reservations must not be treated as zero"
        );
    }

    #[test]
    fn automatic_fit_checks_the_bound_client_quota_and_exempts_model_services() {
        let preferred = nodes::Node {
            id: "antonpc".into(),
            label: "4090".into(),
            enabled: true,
            priority: 1000,
            endpoint: nodes::Endpoint::LinuxSsh,
            ssh_config: None,
        };
        let anton = Target {
            id: "antonpc".into(),
            label: "4090".into(),
            root: PathBuf::new(),
            node: Some(preferred),
        };
        let spark = Target {
            id: "spark".into(),
            label: "Spark".into(),
            root: PathBuf::new(),
            node: None,
        };
        let idle = json!({"transport":{"authenticated":true},"scheduler":{"healthy":true},"runtimes":["python"],"jobs":[],"managed_limits":{"cpu_total":16,"cpu_per_job":4,"memory_gb_total":64,"memory_gb_per_job":8,"headroom_gb":32,"client_jobs":1,"client_cpu":8},"capabilities":{"system_memory":{"available_bytes":128*inventory::GIB}}});
        let mut exhausted = idle.clone();
        exhausted["jobs"] = json!([{"state":"running","where":"spark","kind":"batch","client":"codex","reserved_cpu":4,"reserved_memory_gb":8}]);
        assert_eq!(
            observation(&anton, &exhausted).available_cpu,
            Some(12.0),
            "the preferred node still has total CPU budget"
        );
        assert!(!default_job_fits(&exhausted, "codex"));
        let candidates = [(&anton, &exhausted), (&spark, &idle)]
            .into_iter()
            .filter(|(_, value)| default_job_fits(value, "codex"))
            .map(|(target, value)| observation(target, value))
            .collect::<Vec<_>>();
        assert_eq!(
            nodes::select(
                &candidates,
                &nodes::SelectionRequirements {
                    runtime: "python".into(),
                    ..Default::default()
                }
            )
            .unwrap()
            .id,
            "spark"
        );
        exhausted["managed_limits"]["client_jobs"] = json!(2);
        exhausted["managed_limits"]["client_cpu"] = json!(4);
        assert!(
            !default_job_fits(&exhausted, "codex"),
            "per-client CPU exhaustion also prevents automatic placement"
        );
        exhausted["managed_limits"]["client_jobs"] = json!(1);
        exhausted["managed_limits"]["client_cpu"] = json!(8);
        exhausted["jobs"][0]["kind"] = json!("service");
        assert!(
            default_job_fits(&exhausted, "codex"),
            "model services count against totals but are exempt from batch client quotas"
        );
        exhausted["jobs"][0]["kind"] = json!("batch");
        exhausted["jobs"][0]["client"] = json!("claude");
        assert!(default_job_fits(&exhausted, "codex"));
        assert!(!default_job_fits(&exhausted, "claude"));
    }
}
