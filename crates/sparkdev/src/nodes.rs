//! Named remote profiles. Every profile retains its own legacy controller state.
//!
//! The registry contains references, never private keys or account credentials.
//! Selecting a profile does not change SPARKDEV_HOME in the current process.
use crate::{AppError, Result, state};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
};

pub const MAX_NODES: usize = 16;
const MAX_REGISTRY_BYTES: u64 = 128 * 1024;
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_ENDPOINT_BYTES: u64 = 16 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Endpoint {
    #[default]
    LinuxSsh,
    WindowsWsl {
        distribution: String,
        user: String,
        controller_path: String,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointSettings {
    #[serde(default)]
    pub endpoint: Endpoint,
    #[serde(default)]
    pub ssh_config: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    pub id: String,
    pub label: String,
    #[serde(default = "enabled")]
    pub enabled: bool,
    /// Higher values are preferred, after capability and admission checks.
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub endpoint: Endpoint,
    #[serde(default)]
    pub ssh_config: Option<PathBuf>,
}

fn enabled() -> bool {
    true
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    v: u32,
    nodes: Vec<Node>,
}

fn invalid(message: &str) -> AppError {
    AppError::new(2, message)
}

fn unsafe_state() -> AppError {
    AppError::new(
        3,
        "Named-device state has unsafe paths, ownership or file types",
    )
}

pub fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 32
        || !id.as_bytes()[0].is_ascii_lowercase()
        || id.ends_with('-')
        || id.contains("--")
        || !id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        || matches!(id, "spark" | "laptop" | "auto")
    {
        return Err(invalid(
            "Device ID must be 1-32 lowercase letters, digits or single hyphens, start with a letter, and not be spark, laptop or auto",
        ));
    }
    Ok(())
}

pub fn validate_endpoint(settings: &EndpointSettings) -> Result<()> {
    if let Some(path) = &settings.ssh_config {
        if !path.is_absolute()
            || path
                .as_os_str()
                .to_string_lossy()
                .contains(['\0', '\r', '\n'])
            || path
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            return Err(invalid(
                "SSH configuration reference must be an absolute path without parent traversal",
            ));
        }
    }
    if let Endpoint::WindowsWsl {
        distribution,
        user,
        controller_path,
    } = &settings.endpoint
    {
        if distribution.is_empty()
            || distribution.len() > 64
            || !distribution.as_bytes()[0].is_ascii_alphanumeric()
            || !distribution
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        {
            return Err(invalid(
                "WSL distribution must be a plain name of at most 64 characters",
            ));
        }
        if user.is_empty()
            || user.len() > 32
            || !(user.as_bytes()[0].is_ascii_lowercase() || user.starts_with('_'))
            || !user
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_-".contains(&c))
        {
            return Err(invalid(
                "WSL user must be a plain lowercase POSIX account name",
            ));
        }
        if !controller_path.starts_with('/')
            || controller_path.len() > 512
            || !controller_path.ends_with("/sparkdev/controller.py")
            || controller_path[1..].split('/').any(|part| {
                part.is_empty()
                    || matches!(part, "." | "..")
                    || !part
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
            })
        {
            return Err(invalid(
                "WSL controller must be an absolute plain path ending in /sparkdev/controller.py",
            ));
        }
    }
    Ok(())
}

fn validate_node(node: &Node) -> Result<()> {
    validate_id(&node.id)?;
    validate_node_metadata(node)
}

fn validate_node_metadata(node: &Node) -> Result<()> {
    if node.label.is_empty()
        || node.label.len() > 96
        || node.label.trim() != node.label
        || node.label.chars().any(char::is_control)
        || !(-1000..=1000).contains(&node.priority)
    {
        return Err(invalid(
            "Device label must be 1-96 printable characters and priority must be between -1000 and 1000",
        ));
    }
    validate_endpoint(&EndpointSettings {
        endpoint: node.endpoint.clone(),
        ssh_config: node.ssh_config.clone(),
    })
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

fn owned(metadata: &fs::Metadata, regular: bool) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o022 == 0
            && (!regular || metadata.nlink() == 1)
    }
    #[cfg(not(unix))]
    {
        let _ = (metadata, regular);
        true
    }
}

fn safe_directory(path: &Path, missing: bool) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !reparse(&metadata) && owned(&metadata, false) => {
            Ok(())
        }
        Err(error) if missing && error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => Err(unsafe_state()),
    }
}

fn root() -> Result<PathBuf> {
    let root = state::root()?;
    if !root.is_absolute()
        || root
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(unsafe_state());
    }
    // Child profiles may be entered through SPARKDEV_HOME. Inspect their
    // ancestry too, rather than trusting a normal final directory reached
    // through a replaced nodes/ parent or another junction.
    for ancestor in root.ancestors() {
        let metadata = fs::symlink_metadata(ancestor)?;
        if !metadata.is_dir() || reparse(&metadata) {
            return Err(unsafe_state());
        }
    }
    safe_directory(&root, false)?;
    Ok(root)
}

fn nodes_directory(root: &Path) -> Result<PathBuf> {
    let directory = root.join("nodes");
    safe_directory(&directory, true)?;
    Ok(directory)
}

fn read_bounded(path: &Path, maximum: u64) -> Result<Option<Vec<u8>>> {
    let before = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(unsafe_state()),
    };
    if !before.is_file() || reparse(&before) || !owned(&before, true) || before.len() > maximum {
        return Err(unsafe_state());
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
        options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT.
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || reparse(&metadata)
        || !owned(&metadata, true)
        || metadata.len() > maximum
    {
        return Err(unsafe_state());
    }
    let mut bytes = Vec::new();
    file.take(maximum + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err(unsafe_state());
    }
    Ok(Some(bytes))
}

fn read_registry(root: &Path) -> Result<Registry> {
    let Some(bytes) = read_bounded(&root.join("nodes.json"), MAX_REGISTRY_BYTES)? else {
        return Ok(Registry {
            v: 1,
            nodes: vec![],
        });
    };
    let registry: Registry = serde_json::from_slice(&bytes)?;
    if registry.v != 1 || registry.nodes.len() > MAX_NODES {
        return Err(invalid("Unsupported or oversized named-device registry"));
    }
    let mut identifiers = BTreeSet::new();
    for node in &registry.nodes {
        validate_node(node)?;
        if !identifiers.insert(node.id.as_str()) {
            return Err(invalid("Named-device registry contains a duplicate ID"));
        }
    }
    Ok(registry)
}

fn with_registry_lock<T>(root: &Path, change: impl FnOnce() -> Result<T>) -> Result<T> {
    let path = root.join("nodes.lock");
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        if !metadata.is_file() || reparse(&metadata) || !owned(&metadata, true) {
            return Err(unsafe_state());
        }
    }
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
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
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || reparse(&metadata) || !owned(&metadata, true) {
        return Err(unsafe_state());
    }
    fs2::FileExt::lock_exclusive(&file)?;
    change()
}

/// An absent registry means the unchanged legacy Spark/laptop configuration.
/// Observation does not create registry files, node directories or identities.
pub fn list() -> Result<Vec<Node>> {
    let root = root()?;
    Ok(read_registry(&root)?.nodes)
}

/// Return a safe profile path without creating it or switching process state.
pub fn profile_root(id: &str) -> Result<PathBuf> {
    validate_id(id)?;
    let root = root()?;
    let directory = nodes_directory(&root)?;
    let profile = directory.join(id);
    safe_directory(&profile, true)?;
    Ok(profile)
}

fn profile_settings(profile: &Path) -> Result<(state::Config, EndpointSettings)> {
    safe_directory(profile, false)?;
    let config = read_bounded(&profile.join("config.json"), MAX_CONFIG_BYTES)?
        .ok_or_else(|| invalid("Named-device profile has no saved SSH configuration"))?;
    let config: state::Config = serde_json::from_slice(&config)?;
    state::validate_config(&config)?;
    let endpoint = read_bounded(&profile.join("endpoint.json"), MAX_ENDPOINT_BYTES)?
        .ok_or_else(|| invalid("Named-device profile has no endpoint descriptor"))?;
    let settings: EndpointSettings = serde_json::from_slice(&endpoint)?;
    validate_endpoint(&settings)?;
    Ok((config, settings))
}

/// Each registered device's label and the addresses its profile is saved under.
pub fn saved_hosts() -> Result<Vec<(String, Vec<String>)>> {
    let mut saved = Vec::new();
    for node in list()? {
        if let Ok((config, _)) = profile_settings(&profile_root(&node.id)?) {
            saved.push((node.label, config.hosts));
        }
    }
    Ok(saved)
}

pub fn get(id: &str) -> Result<Node> {
    validate_id(id)?;
    let node = list()?
        .into_iter()
        .find(|node| node.id == id)
        .ok_or_else(|| invalid("Named device is not registered"))?;
    let (_, settings) = profile_settings(&profile_root(id)?)?;
    if settings.endpoint != node.endpoint || settings.ssh_config != node.ssh_config {
        return Err(invalid(
            "Named-device endpoint differs from its retained profile",
        ));
    }
    Ok(node)
}

/// Add a new identity. Replacing an existing profile is deliberately refused:
/// pending requests, jobs and returned files must retain their original target.
pub fn add(node: Node, config: state::Config) -> Result<Node> {
    validate_node(&node)?;
    state::validate_config(&config)?;
    add_with_diagnostics(node, config, state::diagnostics())
}

fn reject_redirected(diagnostics: &serde_json::Value) -> Result<()> {
    // Match the existing setup writer exactly: true refuses; false/unknown
    // retain its established policy rather than creating a new migration rule.
    if diagnostics["redirected"] == true {
        return Err(AppError::new(3, crate::REDIRECTED_STATE));
    }
    Ok(())
}

fn add_with_diagnostics(
    node: Node,
    config: state::Config,
    diagnostics: serde_json::Value,
) -> Result<Node> {
    reject_redirected(&diagnostics)?;
    let config_bytes = serde_json::to_vec(&config)?;
    if config_bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(invalid(
            "Named-device SSH configuration exceeds its size limit",
        ));
    }
    let root = root()?;
    with_registry_lock(&root, || {
        let mut registry = read_registry(&root)?;
        if registry.nodes.iter().any(|old| old.id == node.id) {
            return Err(invalid(
                "This device ID is already registered; its retained identity cannot be replaced",
            ));
        }
        if registry.nodes.len() >= MAX_NODES {
            return Err(invalid("At most 16 named devices may be registered"));
        }
        let directory = nodes_directory(&root)?;
        let profile = directory.join(&node.id);
        // lexists, including dangling links: never repurpose an old state root.
        match fs::symlink_metadata(&profile) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => {
                return Err(invalid(
                    "Device profile already exists and will not be replaced",
                ));
            }
        }
        fs::create_dir_all(&directory)?;
        safe_directory(&directory, false)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        }
        let stage = directory.join(format!(".pending-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&stage)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&stage, fs::Permissions::from_mode(0o700))?;
        }
        state::atomic_bytes(&stage.join("config.json"), &config_bytes)?;
        state::atomic_json(
            &stage.join("endpoint.json"),
            &EndpointSettings {
                endpoint: node.endpoint.clone(),
                ssh_config: node.ssh_config.clone(),
            },
        )?;
        fs::rename(&stage, &profile)?;
        registry.nodes.push(node.clone());
        // A failed registry write leaves a safe, unregistered orphan rather
        // than deleting identity/state that another process may already use.
        state::atomic_json(&root.join("nodes.json"), &registry)?;
        Ok(node)
    })
}

pub fn set_enabled(id: &str, enabled: &bool) -> Result<Node> {
    validate_id(id)?;
    set_enabled_with_diagnostics(id, enabled, state::diagnostics())
}

fn set_enabled_with_diagnostics(
    id: &str,
    enabled: &bool,
    diagnostics: serde_json::Value,
) -> Result<Node> {
    reject_redirected(&diagnostics)?;
    let root = root()?;
    with_registry_lock(&root, || {
        let mut registry = read_registry(&root)?;
        let node = registry
            .nodes
            .iter_mut()
            .find(|node| node.id == id)
            .ok_or_else(|| invalid("Named device is not registered"))?;
        node.enabled = *enabled;
        let updated = node.clone();
        state::atomic_json(&root.join("nodes.json"), &registry)?;
        Ok(updated)
    })
}

/// Endpoint metadata for the CURRENT profile root. Its absence preserves
/// existing Linux-Spark behavior; malformed saved metadata never defaults away.
pub fn load_endpoint() -> Result<EndpointSettings> {
    let root = root()?;
    let Some(bytes) = read_bounded(&root.join("endpoint.json"), MAX_ENDPOINT_BYTES)? else {
        return Ok(EndpointSettings::default());
    };
    let settings: EndpointSettings = serde_json::from_slice(&bytes)?;
    validate_endpoint(&settings)?;
    Ok(settings)
}

#[derive(Clone, Debug)]
pub struct NodeObservation {
    pub node: Node,
    pub authenticated: bool,
    pub scheduler_healthy: bool,
    pub runtimes: Vec<String>,
    pub available_cpu: Option<f64>,
    pub available_memory_bytes: Option<u64>,
    pub gpu_vram_bytes: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct SelectionRequirements {
    pub runtime: String,
    pub min_cpu: Option<f64>,
    pub min_memory_bytes: Option<u64>,
    pub min_gpu_vram_bytes: Option<u64>,
}

fn observed_cpu(observation: &NodeObservation) -> Option<f64> {
    observation
        .available_cpu
        .filter(|cpu| cpu.is_finite() && *cpu >= 0.0)
}

/// Pure capability ranking, never a reservation or an instruction to retry a
/// request on another machine. The caller must persist its selected identity
/// before any submission, and obtain admission from the selected controller.
pub fn select(observations: &[NodeObservation], required: &SelectionRequirements) -> Result<Node> {
    if required.runtime.is_empty()
        || required.runtime.len() > 64
        || !required
            .runtime
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        || required
            .min_cpu
            .is_some_and(|cpu| !cpu.is_finite() || cpu <= 0.0)
        || required.min_memory_bytes == Some(0)
        || required.min_gpu_vram_bytes == Some(0)
    {
        return Err(invalid("Device selection requirements are invalid"));
    }
    let mut candidates = Vec::new();
    for observation in observations {
        // The retained legacy Spark is an internal observation, never a
        // replaceable registry entry. Named profiles cannot claim its ID.
        if observation.node.id == "spark" {
            validate_node_metadata(&observation.node)?;
        } else {
            validate_node(&observation.node)?;
        }
        if !observation.node.enabled
            || !observation.authenticated
            || !observation.scheduler_healthy
            || !observation
                .runtimes
                .iter()
                .any(|runtime| runtime == &required.runtime)
            || required
                .min_cpu
                .is_some_and(|minimum| observed_cpu(observation).is_none_or(|cpu| cpu < minimum))
            || required.min_memory_bytes.is_some_and(|minimum| {
                observation
                    .available_memory_bytes
                    .is_none_or(|bytes| bytes < minimum)
            })
            || required.min_gpu_vram_bytes.is_some_and(|minimum| {
                observation
                    .gpu_vram_bytes
                    .is_none_or(|bytes| bytes < minimum)
            })
        {
            continue;
        }
        candidates.push(observation);
    }
    candidates.sort_by(|left, right| {
        right
            .node
            .priority
            .cmp(&left.node.priority)
            .then_with(|| {
                // When a VRAM requirement is explicit, prefer the smallest
                // measured GPU that fits, leaving larger GPUs for larger work.
                if required.min_gpu_vram_bytes.is_some() {
                    left.gpu_vram_bytes.cmp(&right.gpu_vram_bytes)
                } else {
                    Ordering::Equal
                }
            })
            .then_with(|| {
                observed_cpu(right)
                    .partial_cmp(&observed_cpu(left))
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| {
                right
                    .available_memory_bytes
                    .cmp(&left.available_memory_bytes)
            })
            .then_with(|| left.node.id.cmp(&right.node.id))
    });
    candidates.first().map(|candidate| candidate.node.clone()).ok_or_else(|| {
        AppError::new(8, "No enabled authenticated device has a healthy scheduler, the requested runtime and the required measured capacity")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(id: &str) -> Node {
        Node {
            id: id.into(),
            label: format!("Device {id}"),
            enabled: true,
            priority: 0,
            endpoint: Endpoint::LinuxSsh,
            ssh_config: None,
        }
    }

    fn config() -> state::Config {
        serde_json::from_value(json!({"ssh_alias":"node-test","hosts":["192.0.2.10"],"host_key_alias":"node-test-pin"})).unwrap()
    }

    fn observation(id: &str) -> NodeObservation {
        NodeObservation {
            node: node(id),
            authenticated: true,
            scheduler_healthy: true,
            runtimes: vec!["python".into(), "cuda".into()],
            available_cpu: Some(4.0),
            available_memory_bytes: Some(16 << 30),
            gpu_vram_bytes: Some(24 << 30),
        }
    }

    #[test]
    fn absence_preserves_legacy_configuration_without_new_files() {
        let home = state::TestHome::new();
        let legacy = serde_json::to_vec(&config()).unwrap();
        fs::write(home.path.join("config.json"), &legacy).unwrap();
        assert!(list().unwrap().is_empty());
        assert_eq!(load_endpoint().unwrap(), EndpointSettings::default());
        assert_eq!(fs::read(home.path.join("config.json")).unwrap(), legacy);
        assert!(!home.path.join("nodes").exists());
        assert!(!home.path.join("nodes.lock").exists());
        assert!(!home.path.join("nodes.json").exists());
        assert!(!home.path.join("endpoint.json").exists());
    }

    #[test]
    fn node_writers_match_setup_redirection_policy_before_mutation() {
        let home = state::TestHome::new();
        let redirected = json!({"redirected":true});
        let failure =
            add_with_diagnostics(node("antonpc"), config(), redirected.clone()).unwrap_err();
        assert_eq!(
            (failure.code, failure.message.as_str()),
            (3, crate::REDIRECTED_STATE)
        );
        assert_eq!(
            set_enabled_with_diagnostics("antonpc", &false, redirected.clone())
                .unwrap_err()
                .code,
            3
        );
        assert_eq!(
            fs::read_dir(&home.path).unwrap().count(),
            0,
            "redirected writers must not create registry, lock or profile files"
        );
        for allowed in [
            json!({"redirected":false}),
            json!({"redirected":null}),
            json!({}),
        ] {
            assert!(reject_redirected(&allowed).is_ok());
        }
        add_with_diagnostics(node("antonpc"), config(), json!({"redirected":false})).unwrap();
        let registry = fs::read(home.path.join("nodes.json")).unwrap();
        let profile_config =
            fs::read(profile_root("antonpc").unwrap().join("config.json")).unwrap();
        assert!(set_enabled_with_diagnostics("antonpc", &false, redirected).is_err());
        assert_eq!(fs::read(home.path.join("nodes.json")).unwrap(), registry);
        assert_eq!(
            fs::read(profile_root("antonpc").unwrap().join("config.json")).unwrap(),
            profile_config
        );
        assert!(get("antonpc").unwrap().enabled);
    }

    #[test]
    fn profiles_are_separate_and_replacement_never_changes_saved_jobs() {
        let home = state::TestHome::new();
        let legacy = serde_json::to_vec(&config()).unwrap();
        fs::write(home.path.join("config.json"), &legacy).unwrap();
        let first = add(node("antonpc"), config()).unwrap();
        let second = add(node("renderbox"), config()).unwrap();
        let first_root = profile_root(&first.id).unwrap();
        let second_root = profile_root(&second.id).unwrap();
        assert_ne!(first_root, second_root);
        assert_eq!(get("antonpc").unwrap(), first);
        assert_eq!(get("renderbox").unwrap(), second);
        fs::write(first_root.join("pending-job.json"), b"retained job").unwrap();
        let saved_config = fs::read(first_root.join("config.json")).unwrap();
        assert!(add(node("antonpc"), config()).is_err());
        assert_eq!(
            fs::read(first_root.join("pending-job.json")).unwrap(),
            b"retained job"
        );
        assert_eq!(
            fs::read(first_root.join("config.json")).unwrap(),
            saved_config
        );
        assert_eq!(fs::read(home.path.join("config.json")).unwrap(), legacy);
    }

    #[test]
    fn disabled_profiles_retain_identity_and_queued_work() {
        let _home = state::TestHome::new();
        add(node("antonpc"), config()).unwrap();
        let root = profile_root("antonpc").unwrap();
        let config = fs::read(root.join("config.json")).unwrap();
        fs::write(root.join("pending-job.json"), b"retained").unwrap();
        assert!(!set_enabled("antonpc", &false).unwrap().enabled);
        assert!(!get("antonpc").unwrap().enabled);
        assert_eq!(fs::read(root.join("config.json")).unwrap(), config);
        assert_eq!(
            fs::read(root.join("pending-job.json")).unwrap(),
            b"retained"
        );
        assert!(set_enabled("antonpc", &true).unwrap().enabled);
    }

    #[test]
    fn wsl_profile_persists_explicit_user_and_pin_reference_without_switching_root() {
        let home = state::TestHome::new();
        let mut target = node("antonpc");
        target.endpoint = Endpoint::WindowsWsl {
            distribution: "Ubuntu-20.04".into(),
            user: "bennyhana88".into(),
            controller_path: "/home/bennyhana88/sparkdev/controller.py".into(),
        };
        target.ssh_config = Some(home.path.join("antonpc-ssh.conf"));
        add(target.clone(), config()).unwrap();
        assert_eq!(get("antonpc").unwrap(), target);
        assert_eq!(state::root().unwrap(), home.path);
        assert_eq!(load_endpoint().unwrap(), EndpointSettings::default());
        let saved: EndpointSettings = serde_json::from_slice(
            &fs::read(profile_root("antonpc").unwrap().join("endpoint.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved.endpoint, target.endpoint);
        assert_eq!(saved.ssh_config, target.ssh_config);
        assert!(!home.path.join("config.json").exists());
        assert!(!home.path.join("endpoint.json").exists());
    }

    #[test]
    fn registry_capacity_refuses_new_identity_without_changing_existing_profiles() {
        let home = state::TestHome::new();
        for index in 0..MAX_NODES {
            add(node(&format!("device-{index}")), config()).unwrap();
        }
        let registry = fs::read(home.path.join("nodes.json")).unwrap();
        assert!(add(node("one-too-many"), config()).is_err());
        assert_eq!(list().unwrap().len(), MAX_NODES);
        assert_eq!(fs::read(home.path.join("nodes.json")).unwrap(), registry);
        assert!(!profile_root("one-too-many").unwrap().exists());
        assert!(get("device-0").is_ok());
    }

    #[test]
    fn invalid_identifiers_and_endpoint_commands_are_rejected_before_writes() {
        let home = state::TestHome::new();
        for id in [
            "",
            "spark",
            "laptop",
            "auto",
            "AntonPC",
            "../outside",
            "node/outside",
            "-node",
            "node-",
            "node--other",
            "node_name",
            "node:other",
        ] {
            assert!(add(node(id), config()).is_err(), "{id}");
            assert!(profile_root(id).is_err(), "{id}");
        }
        let mut invalid_config = config();
        invalid_config.ssh_alias = "host;whoami".into();
        assert!(add(node("antonpc"), invalid_config).is_err());
        assert!(!home.path.join("nodes.json").exists());
        assert!(!home.path.join("nodes").exists());
        let valid = EndpointSettings {
            endpoint: Endpoint::WindowsWsl {
                distribution: "Ubuntu-20.04".into(),
                user: "bennyhana88".into(),
                controller_path: "/home/bennyhana88/sparkdev/controller.py".into(),
            },
            ssh_config: None,
        };
        assert!(validate_endpoint(&valid).is_ok());
        for bad in [
            "Ubuntu;whoami",
            "-Ubuntu",
            "Ubuntu Windows",
            "Ubuntu\nwhoami",
            "",
        ] {
            let mut settings = valid.clone();
            if let Endpoint::WindowsWsl { distribution, .. } = &mut settings.endpoint {
                *distribution = bad.into();
            }
            assert!(validate_endpoint(&settings).is_err(), "{bad}");
        }
        for bad in ["../benny", "-root", "Benny", "root;whoami", ""] {
            let mut settings = valid.clone();
            if let Endpoint::WindowsWsl { user, .. } = &mut settings.endpoint {
                *user = bad.into();
            }
            assert!(validate_endpoint(&settings).is_err(), "{bad}");
        }
        for bad in [
            "home/benny/sparkdev/controller.py",
            "/home/../sparkdev/controller.py",
            "/home//sparkdev/controller.py",
            "/home/benny/sparkdev/controller.py;whoami",
            "/home/benny/other.py",
            "/home/benny/$(whoami)/sparkdev/controller.py",
        ] {
            let mut settings = valid.clone();
            if let Endpoint::WindowsWsl {
                controller_path, ..
            } = &mut settings.endpoint
            {
                *controller_path = bad.into();
            }
            assert!(validate_endpoint(&settings).is_err(), "{bad}");
        }
    }

    #[test]
    fn malformed_and_oversized_registry_never_fall_back_to_legacy() {
        let home = state::TestHome::new();
        let path = home.path.join("nodes.json");
        for bytes in [
            b"not-json".as_slice(),
            br#"{"v":2,"nodes":[]}"#,
            br#"{"v":1,"nodes":[],"unknown":true}"#,
        ] {
            fs::write(&path, bytes).unwrap();
            assert!(list().is_err());
        }
        fs::write(&path, vec![b' '; (MAX_REGISTRY_BYTES + 1) as usize]).unwrap();
        assert!(list().is_err());
        state::atomic_json(
            &path,
            &Registry {
                v: 1,
                nodes: vec![node("antonpc"), node("antonpc")],
            },
        )
        .unwrap();
        assert!(list().is_err());
        state::atomic_json(
            &path,
            &Registry {
                v: 1,
                nodes: (0..=MAX_NODES)
                    .map(|i| node(&format!("node-{i}")))
                    .collect(),
            },
        )
        .unwrap();
        assert!(list().is_err());
    }

    #[test]
    fn endpoint_and_profile_corruption_do_not_select_a_different_identity() {
        let home = state::TestHome::new();
        fs::write(
            home.path.join("endpoint.json"),
            br#"{"endpoint":{"kind":"unknown"}}"#,
        )
        .unwrap();
        assert!(load_endpoint().is_err());
        fs::write(
            home.path.join("endpoint.json"),
            vec![b' '; (MAX_ENDPOINT_BYTES + 1) as usize],
        )
        .unwrap();
        assert!(load_endpoint().is_err());
        fs::remove_file(home.path.join("endpoint.json")).unwrap();
        add(node("antonpc"), config()).unwrap();
        let profile = profile_root("antonpc").unwrap();
        fs::write(profile.join("config.json"), b"{}").unwrap();
        assert!(get("antonpc").is_err());
        state::atomic_json(&profile.join("config.json"), &config()).unwrap();
        state::atomic_json(
            &profile.join("endpoint.json"),
            &EndpointSettings {
                endpoint: Endpoint::WindowsWsl {
                    distribution: "Ubuntu".into(),
                    user: "benny".into(),
                    controller_path: "/home/benny/sparkdev/controller.py".into(),
                },
                ssh_config: None,
            },
        )
        .unwrap();
        assert!(get("antonpc").is_err());
    }

    #[test]
    fn unregistered_existing_directory_is_not_reused() {
        let home = state::TestHome::new();
        let existing = home.path.join("nodes/antonpc");
        fs::create_dir_all(&existing).unwrap();
        fs::write(existing.join("retained"), b"preserve").unwrap();
        assert!(add(node("antonpc"), config()).is_err());
        assert_eq!(fs::read(existing.join("retained")).unwrap(), b"preserve");
        assert!(!home.path.join("nodes.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_roots_profiles_registry_endpoint_and_lock_are_refused() {
        use std::os::unix::fs::symlink;
        let home = state::TestHome::new();
        let other = tempfile::tempdir().unwrap();
        symlink(other.path(), home.path.join("nodes")).unwrap();
        assert!(add(node("antonpc"), config()).is_err());
        assert!(profile_root("antonpc").is_err());
        fs::remove_file(home.path.join("nodes")).unwrap();
        fs::create_dir(home.path.join("nodes")).unwrap();
        symlink(other.path(), home.path.join("nodes/antonpc")).unwrap();
        assert!(add(node("antonpc"), config()).is_err());
        assert!(profile_root("antonpc").is_err());
        fs::remove_file(home.path.join("nodes/antonpc")).unwrap();
        for name in ["nodes.json", "endpoint.json", "nodes.lock"] {
            // Earlier refused writes can legitimately create the ordinary
            // registry lock. Replace only that owned fixture before testing
            // the malicious lock symlink itself.
            let destination = home.path.join(name);
            if destination.exists() {
                assert_eq!(name, "nodes.lock");
                assert!(fs::symlink_metadata(&destination).unwrap().is_file());
                fs::remove_file(&destination).unwrap();
            }
            let target = other.path().join(name);
            fs::write(&target, b"{}").unwrap();
            symlink(&target, &destination).unwrap();
            match name {
                "nodes.json" => assert!(list().is_err()),
                "endpoint.json" => assert!(load_endpoint().is_err()),
                _ => assert!(add(node("antonpc"), config()).is_err()),
            }
            assert_eq!(fs::read(&target).unwrap(), b"{}");
            fs::remove_file(home.path.join(name)).unwrap();
        }
        let linked_root = other.path().join("linked-state");
        symlink(&home.path, &linked_root).unwrap();
        // Call the path guard directly: production root() applies it after
        // resolving the configured root, without changing process environment.
        assert!(safe_directory(&linked_root, false).is_err());
    }

    #[test]
    fn selection_checks_authentication_runtime_health_and_enabled_state() {
        let required = SelectionRequirements {
            runtime: "python".into(),
            ..Default::default()
        };
        let mut observations = vec![
            observation("disabled"),
            observation("untrusted"),
            observation("unhealthy"),
            observation("unavailable"),
            observation("usable"),
        ];
        observations[0].node.enabled = false;
        observations[1].authenticated = false;
        observations[2].scheduler_healthy = false;
        observations[3].runtimes.clear();
        for observation in &mut observations[..4] {
            observation.node.priority = 100;
        }
        assert_eq!(select(&observations, &required).unwrap().id, "usable");
        assert!(select(&observations[..4], &required).is_err());
        assert!(select(&[], &required).is_err());
    }

    #[test]
    fn legacy_spark_can_be_ranked_but_never_registered_as_a_named_profile() {
        let _home = state::TestHome::new();
        let mut legacy = observation("spark");
        legacy.node.priority = 5;
        let required = SelectionRequirements {
            runtime: "python".into(),
            ..Default::default()
        };
        assert_eq!(
            select(&[observation("antonpc"), legacy.clone()], &required)
                .unwrap()
                .id,
            "spark"
        );
        assert!(add(legacy.node, config()).is_err());
        assert!(list().unwrap().is_empty());
    }

    #[test]
    fn capacity_fit_uses_measured_gpu_memory_not_system_ram() {
        let mut small = observation("small-gpu");
        small.available_memory_bytes = Some(256 << 30);
        small.gpu_vram_bytes = Some(24 << 30);
        let mut large = observation("large-gpu");
        large.gpu_vram_bytes = Some(120 << 30);
        let mut unknown = observation("unknown-gpu");
        unknown.gpu_vram_bytes = None;
        unknown.node.priority = 100;
        let observations = vec![small, large, unknown];
        let mut required = SelectionRequirements {
            runtime: "cuda".into(),
            min_gpu_vram_bytes: Some(16 << 30),
            ..Default::default()
        };
        assert_eq!(select(&observations, &required).unwrap().id, "small-gpu");
        required.min_gpu_vram_bytes = Some(32 << 30);
        assert_eq!(select(&observations, &required).unwrap().id, "large-gpu");
        required.min_gpu_vram_bytes = Some(128 << 30);
        assert!(select(&observations, &required).is_err());
    }

    #[test]
    fn required_unknown_resources_fail_closed_and_priority_is_persisted_preference() {
        let mut known = observation("known");
        let mut unknown = observation("unknown");
        unknown.available_cpu = None;
        unknown.available_memory_bytes = None;
        unknown.node.priority = 10;
        let mut required = SelectionRequirements {
            runtime: "python".into(),
            min_cpu: Some(2.0),
            min_memory_bytes: Some(8 << 30),
            ..Default::default()
        };
        assert_eq!(
            select(&[known.clone(), unknown.clone()], &required)
                .unwrap()
                .id,
            "known"
        );
        known.available_cpu = Some(f64::NAN);
        assert!(select(&[known.clone(), unknown.clone()], &required).is_err());
        required.min_cpu = None;
        required.min_memory_bytes = None;
        assert_eq!(select(&[known, unknown], &required).unwrap().id, "unknown");
        required.min_cpu = Some(f64::INFINITY);
        assert!(select(&[], &required).is_err());
    }
}
