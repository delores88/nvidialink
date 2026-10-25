//! Automatic, restartable result delivery. The caller never has to stay alive.
use crate::{AppError, Result, Rpc, inventory, state, transport};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const MAX_MANIFEST: usize = 3 * inventory::CHUNK;
const MAX_OUTPUT: u64 = 64 * inventory::GIB;
const MAX_FILES: usize = 10_000;
const SPACE_RESERVE: u64 = 64 * inventory::MIB;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Entry {
    scope: String,
    path: String,
    bytes: u64,
    sha256: String,
    chunks: Vec<String>,
    original_sha256: Option<String>,
}
#[derive(Clone, Deserialize, Serialize)]
struct Record {
    v: u32,
    request_id: String,
    project: PathBuf,
    originals: BTreeMap<String, String>,
    request: Value,
    job: Option<String>,
    disabled: bool,
    #[serde(default)]
    submission: Option<Value>,
    #[serde(default)]
    complete: bool,
    #[serde(default)]
    returned: Vec<Value>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    manifest_sha256: Option<String>,
}
fn err(text: &str) -> AppError {
    AppError::new(6, text)
}
fn folder() -> Result<PathBuf> {
    let path = state::root()?.join("returns");
    fs::create_dir_all(&path)?;
    Ok(path)
}
fn record_path(id: &str) -> Result<PathBuf> {
    state::validate_id(id)?;
    Ok(folder()?.join(format!("{id}.json")))
}
fn records() -> Result<Vec<(PathBuf, Record)>> {
    let mut out = Vec::new();
    for item in fs::read_dir(folder()?)? {
        let path = item?.path();
        if path.extension().is_some_and(|v| v == "json") {
            let bytes = fs::read(&path)?;
            if bytes.len() > 32 * inventory::CHUNK {
                return Err(err("Saved return record exceeds its size limit"));
            }
            let record: Record = serde_json::from_slice(&bytes)?;
            if record.v != 1 {
                return Err(err("Unsupported saved return record"));
            }
            out.push((path, record));
        }
    }
    Ok(out)
}
fn lock(name: &str) -> Result<Option<fs::File>> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(folder()?.join(name))?;
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(Some(file)),
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
        {
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}
/// Persist source identity before an uncertain submission. Recovery repeats only
/// this exact request; it never recollects changed source or creates a new job ID.
pub fn register(inv: &inventory::Inventory, request: &Value, disabled: bool) -> Result<()> {
    let id = request["request_id"]
        .as_str()
        .ok_or_else(|| err("Missing return request ID"))?;
    let path = record_path(id)?;
    let guard = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(folder()?.join("registration.lock"))?;
    fs2::FileExt::lock_exclusive(&guard)?;
    let record = Record {
        v: 1,
        request_id: id.into(),
        project: inv.root.clone(),
        originals: inv
            .manifest
            .iter()
            .map(|e| (e.path.clone(), e.sha256.clone()))
            .collect(),
        request: request.clone(),
        job: None,
        disabled,
        submission: None,
        complete: false,
        returned: vec![],
        error: None,
        manifest_sha256: None,
    };
    if path.exists() {
        let old: Record = serde_json::from_slice(&fs::read(path)?)?;
        if old.request != record.request
            || old.project != record.project
            || old.originals != record.originals
            || old.disabled != disabled
        {
            return Err(err(
                "This request ID already has a different result destination",
            ));
        }
    } else {
        state::atomic_json(&path, &record)?;
    }
    Ok(())
}
/// The background process owns submission and return state. The foreground
/// caller only waits for its durable response, so exiting mid-RPC loses nothing.
pub fn submission(id: &str, timeout: Duration) -> Result<Value> {
    let path = record_path(id)?;
    let until = Instant::now() + timeout;
    loop {
        let record: Record = serde_json::from_slice(&fs::read(&path)?)?;
        if let Some(value) = record.submission {
            return Ok(value);
        }
        if record.complete {
            return Err(err(record
                .error
                .as_deref()
                .unwrap_or("Submission rejected")));
        }
        if Instant::now() >= until {
            return Err(AppError::new(
                4,
                format!(
                    "Submission is still being reconciled by the bridge. Retry with the same request ID {id}; no second job will be created."
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
fn hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn valid_path(path: &str) -> Result<()> {
    if path.len() > 2048 || !inventory::path_policy(path)? {
        return Err(err("Spark result has an unsafe or excluded path"));
    }
    for part in path.split('/') {
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        if part.ends_with(['.', ' '])
            || part
                .chars()
                .any(|c| c.is_control() || "<>\"|?*".contains(c))
            || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit())
        {
            return Err(err(
                "Spark result path cannot be safely represented on the laptop",
            ));
        }
    }
    Ok(())
}
fn parse_manifest(value: &Value, record: &Record) -> Result<Vec<Entry>> {
    let bytes = serde_json::to_vec(&value["manifest"])?;
    if bytes.len() > MAX_MANIFEST
        || value["manifest"]["v"] != 1
        || value["manifest_sha256"] != inventory::hash(&bytes)
    {
        return Err(err(
            "Spark result manifest failed its SHA-256 or size check",
        ));
    }
    let entries: Vec<Entry> = serde_json::from_value(value["manifest"]["entries"].clone())?;
    let mut names = BTreeSet::new();
    let mut total = 0u64;
    for e in &entries {
        valid_path(&e.path)?;
        if !matches!(e.scope.as_str(), "work" | "output")
            || !hex(&e.sha256)
            || e.bytes > MAX_OUTPUT
            || e.chunks.len() as u64 != e.bytes.div_ceil(inventory::MIB)
            || e.chunks.iter().any(|h| !hex(h))
            || !names.insert(format!("{}/{}", e.scope, e.path.to_lowercase()))
        {
            return Err(err(
                "Spark result manifest has an invalid or duplicate entry",
            ));
        }
        if e.scope == "work" && e.original_sha256.as_ref() != record.originals.get(&e.path) {
            return Err(err(
                "Spark result original does not match the uploaded laptop source",
            ));
        }
        total = total
            .checked_add(e.bytes)
            .ok_or_else(|| err("Spark results exceed 64 GiB"))?;
    }
    if total > MAX_OUTPUT || entries.len() > MAX_FILES {
        return Err(err("Spark results exceed the 64 GiB or 10,000 file limit"));
    }
    // Validate the whole namespace before applying anything: a file cannot
    // also be the parent directory of another file, including Windows aliases.
    for name in &names {
        let mut parent = name.as_str();
        while let Some((prefix, _)) = parent.rsplit_once('/') {
            if names.contains(prefix) {
                return Err(err(
                    "Spark result manifest uses a file as another file's parent directory",
                ));
            }
            parent = prefix;
        }
    }
    Ok(entries)
}
fn check_space(available: u64, bytes: u64) -> Result<()> {
    if bytes
        .checked_add(SPACE_RESERVE)
        .is_none_or(|needed| available < needed)
    {
        return Err(err(&format!(
            "Not enough free disk space to return Spark results: {} bytes plus 64 MiB of headroom are needed. Free space and the bridge will retry automatically.",
            bytes
        )));
    }
    Ok(())
}
fn require_space(path: &Path, bytes: u64) -> Result<()> {
    let available = fs2::available_space(path).map_err(|_| {
        err("Unable to check free disk space for Spark results; delivery will retry automatically")
    })?;
    check_space(available, bytes)
}
fn reparse(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}
fn open_file(path: &Path, write: bool) -> Result<fs::File> {
    let mut opts = fs::OpenOptions::new();
    opts.read(true).write(write);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.custom_flags(0x00200000);
    }
    let file = opts.open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || reparse(&meta) {
        return Err(err("Refusing a linked or non-regular result file"));
    }
    Ok(file)
}
fn file_hash(path: &Path) -> Result<String> {
    let mut f = open_file(path, false)?;
    let mut hash = Sha256::new();
    let mut bytes = vec![0; inventory::CHUNK];
    loop {
        let n = f.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        hash.update(&bytes[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
/// Windows holds each ancestor against deletion/reparse replacement. Linux
/// uses pinned directory handles and /proc/self/fd for descriptor-relative I/O.
struct Directory {
    path: PathBuf,
    _handles: Vec<fs::File>,
}
impl Directory {
    fn root(path: &Path) -> Result<Self> {
        if !path.is_absolute() {
            return Err(err("Result destination must be absolute"));
        }
        let mut current = PathBuf::new();
        let mut handles = Vec::new();
        for part in path.components() {
            #[cfg(target_os = "linux")]
            if matches!(part, std::path::Component::Normal(_)) {
                use std::os::fd::AsRawFd;
                if let Some(parent) = handles.last() {
                    current =
                        PathBuf::from(format!("/proc/self/fd/{}", fs::File::as_raw_fd(parent)));
                }
            }
            current.push(part.as_os_str());
            if !matches!(part, std::path::Component::Normal(_)) {
                continue;
            }
            let file = Self::open_dir(&current)?;
            handles.push(file);
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            if let Some(f) = handles.last() {
                current = PathBuf::from(format!("/proc/self/fd/{}", f.as_raw_fd()));
            }
        }
        Ok(Self {
            path: current,
            _handles: handles,
        })
    }
    fn open_dir(path: &Path) -> Result<fs::File> {
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options
                .custom_flags(0x02000000 | 0x00200000)
                .share_mode(1 | 2);
        }
        let file = options.open(path)?;
        let meta = file.metadata()?;
        if !meta.is_dir() || reparse(&meta) {
            return Err(err(
                "Refusing a symlink or reparse point in a result destination",
            ));
        }
        Ok(file)
    }
    fn descend(mut self, parts: &[&str]) -> Result<Self> {
        for name in parts {
            let next = self.path.join(name);
            match fs::create_dir(&next) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
            let file = Self::open_dir(&next)?;
            #[cfg(target_os = "linux")]
            {
                use std::os::fd::AsRawFd;
                self.path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
            }
            #[cfg(not(target_os = "linux"))]
            {
                self.path = next;
            }
            self._handles.push(file);
        }
        Ok(self)
    }
}
/// Result files smaller than this come back by RPC alone.
const COPY_MIN: u64 = 4 * inventory::MIB;
/// A copy slice shorter than this would spend most of it connecting.
const COPY_SLICE_MIN: Duration = Duration::from_secs(1);
/// On the LAN route, continue a result file into `stage` with one pinned copy
/// at the link rate. Each copy ends with the pump's budget, so this one worker
/// still serves submissions and other returns between slices; the next pass
/// continues from the bytes already staged and never starts over. Best effort:
/// `download` then verifies every chunk and the whole file against the
/// manifest, and fetches anything that differs by RPC.
fn copy_whole(rpc: &mut dyn Rpc, job: &str, entry: &Entry, stage: &Path, until: Instant) {
    if entry.bytes < COPY_MIN || rpc.route() != "ssh" {
        return;
    }
    // A link is never written through; a full-size stage needs only verifying.
    let staged = match fs::symlink_metadata(stage) {
        Ok(m) if m.file_type().is_symlink() || reparse(&m) || !m.is_file() => return,
        Ok(m) => m.len(),
        Err(_) => 0,
    };
    let left = until.saturating_duration_since(Instant::now());
    if staged >= entry.bytes || left < COPY_SLICE_MIN {
        return;
    }
    let (Some(dir), Some(name)) = (stage.parent(), stage.file_name().and_then(|n| n.to_str()))
    else {
        return;
    };
    // The same free-space reserve as an RPC download, checked before any byte lands.
    if require_space(dir, entry.bytes - staged).is_err() {
        return;
    }
    let Ok(found) = rpc
        .call(json!({"v":1,"op":"output_locate","job":job,"scope":entry.scope,"path":entry.path}))
    else {
        return;
    };
    let Some(file) = found["file"]
        .as_str()
        .filter(|_| found["bytes"] == entry.bytes && found["sha256"] == entry.sha256.as_str())
    else {
        return;
    };
    if let Some((command, input)) = rpc.copy_command(dir, name, file, false) {
        let left = until.saturating_duration_since(Instant::now());
        let _ = transport::run_process(command, &input, left.max(COPY_SLICE_MIN));
    }
}
fn download(
    rpc: &mut dyn Rpc,
    job: &str,
    entry: &Entry,
    stage: &Path,
    until: Instant,
) -> Result<bool> {
    copy_whole(rpc, job, entry, stage, until);
    let mut opts = fs::OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW).mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.custom_flags(0x00200000);
    }
    let mut file = opts.open(stage)?;
    if reparse(&file.metadata()?) || !file.metadata()?.is_file() {
        return Err(err("Unsafe result staging file"));
    }
    // Validate the existing prefix, including the final partial chunk. A bad or
    // interrupted chunk is discarded; confirmed data never gets redownloaded.
    let mut buffer = vec![0; inventory::CHUNK];
    let mut verified = 0u64;
    let mut space_checked = false;
    let mut advanced = false;
    for (index, expected) in entry.chunks.iter().enumerate() {
        let size = (entry.bytes - index as u64 * inventory::MIB).min(inventory::MIB) as usize;
        file.seek(SeekFrom::Start(verified))?;
        if file.read_exact(&mut buffer[..size]).is_ok()
            && inventory::hash(&buffer[..size]) == *expected
        {
            verified += size as u64;
            continue;
        }
        file.set_len(verified)?;
        // Prefix verification may itself consume the pump budget for a large
        // render. Always accept one new verified chunk so each retry advances.
        if advanced && Instant::now() >= until {
            return Ok(false);
        }
        if !space_checked {
            require_space(
                stage
                    .parent()
                    .ok_or_else(|| err("Invalid staging directory"))?,
                entry.bytes - verified,
            )?;
            space_checked = true;
        }
        let chunk = rpc.call(json!({"v":1,"op":"output_chunk","job":job,"scope":entry.scope,"path":entry.path,"index":index}))?;
        let encoded = chunk["data"]
            .as_str()
            .filter(|s| s.len() <= 2 * inventory::CHUNK)
            .ok_or_else(|| err("Invalid result chunk encoding"))?;
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|_| err("Invalid result chunk encoding"))?;
        if bytes.len() != size
            || chunk["sha256"] != *expected
            || inventory::hash(&bytes) != *expected
            || chunk["index"] != index
        {
            return Err(err("Spark result chunk failed its SHA-256 or offset check"));
        }
        file.seek(SeekFrom::Start(verified))?;
        file.write_all(&bytes)?;
        file.sync_data()?;
        verified += bytes.len() as u64;
        advanced = true;
    }
    file.set_len(entry.bytes)?;
    file.sync_all()?;
    drop(file);
    if file_hash(stage)? != entry.sha256 {
        return Err(err("Downloaded result failed its whole-file SHA-256 check"));
    }
    Ok(true)
}
fn conflict_name(path: &str, job: &str) -> String {
    let p = Path::new(path);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or(path);
    let ext = p
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| format!(".{s}"))
        .unwrap_or_default();
    format!("{stem}.spark-{job}{ext}")
}
fn publish_new(
    stage: &Path,
    dir: &Directory,
    name: &str,
    digest: &str,
    permissions: Option<fs::Permissions>,
) -> Result<()> {
    let tmp = dir
        .path
        .join(format!(".spark-return-{}.tmp", uuid::Uuid::new_v4()));
    let mut src = open_file(stage, false)?;
    require_space(&dir.path, src.metadata()?.len())?;
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    let copied = (|| -> Result<()> {
        std::io::copy(&mut src, &mut out)?;
        if let Some(permissions) = permissions {
            out.set_permissions(permissions)?;
        }
        out.sync_all()?;
        Ok(())
    })();
    drop(out);
    if let Err(error) = copied {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    if file_hash(&tmp)? != digest {
        let _ = fs::remove_file(&tmp);
        return Err(err("Result changed while being installed"));
    }
    // Exclusive final creation: an editor that created the path in the meantime
    // wins. Hard linking avoids partial visible output and overwrite races.
    let installed = fs::hard_link(&tmp, dir.path.join(name));
    let _ = fs::remove_file(&tmp);
    installed?;
    Ok(())
}
fn apply(entry: &Entry, stage: &Path, project: &Path, results: &Path, job: &str) -> Result<Value> {
    let parts: Vec<_> = entry.path.split('/').collect();
    let root = if entry.scope == "work" {
        project
    } else {
        results
    };
    let dir = Directory::root(root)?.descend(&parts[..parts.len() - 1])?;
    let name = parts[parts.len() - 1];
    let target = dir.path.join(name);
    let existing = fs::symlink_metadata(&target).ok();
    let mut status = "written";
    let mut actual_name = name.to_owned();
    if let Some(meta) = existing {
        if reparse(&meta) || !meta.is_file() {
            return Err(err(
                "Refusing to overwrite a linked or non-regular local file",
            ));
        }
        let current = file_hash(&target)?;
        if current == entry.sha256 {
            status = "unchanged";
        } else if entry.scope == "work"
            && entry.original_sha256.as_deref() == Some(current.as_str())
        {
            // Check before moving the user's original. Publication rechecks as
            // well because other processes can consume disk space meanwhile.
            require_space(&dir.path, entry.bytes)?;
            #[cfg(unix)]
            let permissions = {
                use std::os::unix::fs::PermissionsExt;
                // Keep the original rwx bits, never copy setuid/setgid/sticky.
                Some(fs::Permissions::from_mode(
                    meta.permissions().mode() & 0o777,
                ))
            };
            #[cfg(not(unix))]
            let permissions = None;
            // Preserve the exact previous inode until the whole return succeeds.
            // If a concurrent editor changes it, its edits remain recoverable.
            let backup = dir.path.join(format!(
                ".{name}.spark-original-{job}-{}",
                uuid::Uuid::new_v4()
            ));
            fs::rename(&target, &backup)?;
            if file_hash(&backup)? != current {
                if !target.exists() {
                    fs::hard_link(&backup, &target)?;
                }
                status = "conflict";
                actual_name = conflict_name(name, job);
            }
            if status != "conflict" {
                if let Err(error) = publish_new(stage, &dir, name, &entry.sha256, permissions) {
                    if !target.exists() {
                        let _ = fs::hard_link(&backup, &target);
                    }
                    return Err(error);
                }
            }
        } else {
            status = "conflict";
            actual_name = conflict_name(name, job);
        }
    } else {
        publish_new(stage, &dir, name, &entry.sha256, None)?;
    }
    if status == "conflict" {
        let conflict = dir.path.join(&actual_name);
        if conflict.exists() {
            if file_hash(&conflict)? != entry.sha256 {
                return Err(err(
                    "The result conflict copy already exists with different content; it was preserved",
                ));
            }
        } else {
            publish_new(stage, &dir, &actual_name, &entry.sha256, None)?;
        }
    }
    let final_path = dir.path.join(&actual_name);
    if file_hash(&final_path)? != entry.sha256 {
        return Err(err("Installed result failed SHA-256 verification"));
    }
    let relative = parts[..parts.len() - 1]
        .iter()
        .copied()
        .chain(std::iter::once(actual_name.as_str()))
        .collect::<Vec<_>>()
        .join("/");
    Ok(
        json!({"path":root.join(relative),"source_path":entry.path,"scope":entry.scope,"bytes":entry.bytes,"sha256":entry.sha256,"status":status}),
    )
}
fn results_folder(job: &str) -> Result<PathBuf> {
    state::validate_id(job)?;
    let root = state::root()?;
    let _guard = Directory::root(&root)?.descend(&["results", job])?;
    Ok(root.join("results").join(job))
}
fn reconcile(path: &Path, record: &mut Record, rpc: &mut dyn Rpc, until: Instant) -> Result<()> {
    if record.complete {
        return Ok(());
    }
    if record.job.is_none() {
        let result = match rpc.call(record.request.clone()) {
            Ok(value) => value,
            Err(error) => {
                if crate::remote_code(&error).is_some() || matches!(error.code, 2 | 7 | 8) {
                    record.complete = true;
                    record.error = Some(format!(
                        "Submission rejected; no automatic retry: {}",
                        error.message
                    ));
                    state::atomic_json(path, record)?;
                }
                return Err(error);
            }
        };
        let job = result["job"]
            .as_str()
            .ok_or_else(|| err("Recovered submission omitted job ID"))?;
        state::validate_id(job)?;
        record.job = Some(job.into());
        record.submission = Some(result);
        record.error = None;
        state::atomic_json(path, record)?;
    }
    if record.disabled {
        record.complete = true;
        return state::atomic_json(path, record);
    }
    let job = record.job.clone().unwrap();
    let outputs = rpc.call(json!({"v":1,"op":"outputs","job":job}))?;
    match outputs["state"].as_str() {
        Some("pending" | "collecting") => return Ok(()),
        Some("ready") => {}
        _ => {
            return Err(err(outputs["error"]["message"].as_str().unwrap_or(
                "Spark results are unavailable; the job is not yet fully returned",
            )));
        }
    }
    let entries = parse_manifest(&outputs, record)?;
    let digest = outputs["manifest_sha256"].as_str().unwrap();
    if record
        .manifest_sha256
        .as_deref()
        .is_some_and(|old| old != digest)
    {
        return Err(err("Spark changed an immutable result manifest"));
    }
    record.manifest_sha256 = Some(digest.into());
    state::atomic_json(path, record)?;
    let result_dir = results_folder(&job)?;
    let stage_root = folder()?.join("staging");
    let _staging_guard = Directory::root(&folder()?)?.descend(&["staging"])?;
    for entry in entries {
        if record.returned.iter().any(|r| {
            r["source_path"] == entry.path
                && r["scope"] == entry.scope
                && r["sha256"] == entry.sha256
        }) {
            continue;
        }
        if Instant::now() >= until {
            return Ok(());
        }
        let key = inventory::hash(format!("{job}:{}:{}", entry.scope, entry.path).as_bytes());
        let stage = stage_root.join(format!("{key}.part"));
        if !download(rpc, &job, &entry, &stage, until)? {
            return Ok(());
        }
        let receipt = apply(&entry, &stage, &record.project, &result_dir, &job)?;
        record.returned.push(receipt);
        record.error = None;
        state::atomic_json(path, record)?;
        let _ = fs::remove_file(stage);
    }
    let ack = rpc.call(json!({"v":1,"op":"outputs_ack","job":job,"manifest_sha256":digest}))?;
    if ack["acknowledged"] != true || ack["manifest_sha256"] != digest {
        return Err(err(
            "Spark did not acknowledge the verified result delivery",
        ));
    }
    record.complete = true;
    record.error = None;
    state::atomic_json(path, record)
}
/// Reconcile a bounded batch. The daemon owns transfers, while wait/status can
/// still read durable receipts without waiting behind a large video download.
pub fn pump(rpc: &mut dyn Rpc, budget: Duration) -> Result<()> {
    let Some(_guard) = lock("delivery.lock")? else {
        return Ok(());
    };
    let until = Instant::now() + budget;
    let mut pending = records()?;
    pending.sort_by_key(|(_, r)| r.job.is_some());
    for (path, mut record) in pending {
        if Instant::now() >= until {
            break;
        }
        if let Err(error) = reconcile(&path, &mut record, rpc, until) {
            record.error = Some(error.message);
            state::atomic_json(&path, &record)?;
        }
    }
    Ok(())
}
pub fn decorate(value: &mut Value) -> Result<()> {
    let saved = records()?;
    let one = |value: &mut Value| {
        let id = value["job"]
            .as_str()
            .or_else(|| value["id"].as_str())
            .unwrap_or("")
            .to_owned();
        if let Some((_, r)) = saved
            .iter()
            .find(|(_, r)| r.job.as_deref() == Some(id.as_str()))
        {
            value["returned"] = json!(r.returned);
            value["results_folder"] = json!(state::root().ok().map(|p| p.join("results").join(id)));
            value["return_status"] = json!(if r.disabled {
                "disabled"
            } else if r.complete {
                "complete"
            } else if r.error.is_some() {
                "retrying"
            } else {
                "pending"
            });
            if let Some(error) = &r.error {
                value["return_error"] = json!(error);
            }
            if !r.complete && crate::terminal(value) {
                value["workload_state"] = value["state"].clone();
                value["state"] = json!("returning");
            }
        }
    };
    if let Some(jobs) = value["jobs"].as_array_mut() {
        for job in jobs {
            one(job);
        }
    } else {
        one(value);
    }
    Ok(())
}
pub fn outputs(job: &str) -> Result<Value> {
    state::validate_id(job)?;
    let mut tr = transport::Transport::new()?;
    let mut out = tr.rpc(json!({"v":1,"op":"outputs","job":job}))?;
    decorate(&mut out)?;
    Ok(out)
}
fn existing_state(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() || !path.is_dir() {
        return Err(err(
            "The result worker needs an existing absolute owner-state directory",
        ));
    }
    let config_path = path.join("config.json");
    let info = fs::symlink_metadata(&config_path)?;
    if !info.is_file() || reparse(&info) || info.len() > inventory::MIB {
        return Err(err(
            "The result worker configuration is not a bounded ordinary file",
        ));
    }
    let config: state::Config = serde_json::from_slice(&fs::read(&config_path)?)?;
    state::validate_config(&config)?;
    Ok(fs::canonicalize(path)?)
}

fn verified_worker_state() -> Result<PathBuf> {
    let configured = state::root()?;
    let physical = existing_state(&configured)?;
    // Match setup's physical-state boundary before registering a persistent
    // startup path. Never silently adopt an MSIX-redirected identity.
    if state::diagnostics()["redirected"] != false {
        return Err(err(
            "The result worker state is redirected or cannot be physically verified",
        ));
    }
    Ok(physical)
}

/// Called before threads are started by the dedicated worker executable.
pub fn configure_worker_state(path: &Path) -> Result<()> {
    existing_state(path)?; // Refuse missing state before root() could create it.
    // SAFETY: the dedicated executable calls this once before starting threads.
    unsafe {
        std::env::set_var("SPARKDEV_HOME", path);
    }
    verified_worker_state()?;
    Ok(())
}

#[cfg(any(windows, test))]
#[derive(Serialize, Deserialize)]
struct Startup {
    v: u32,
    state_dir: PathBuf,
    worker: PathBuf,
}

#[cfg(any(windows, test))]
fn startup_path_key(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\").to_lowercase();
    let text = if let Some(tail) = text.strip_prefix("\\\\?\\unc\\") {
        format!("\\\\{tail}")
    } else {
        text.strip_prefix("\\\\?\\").unwrap_or(&text).to_owned()
    };
    for prefix in ["\\\\localhost\\", "\\\\127.0.0.1\\"] {
        if let Some(tail) = text.strip_prefix(prefix) {
            let bytes = tail.as_bytes();
            if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && &bytes[1..3] == b"$\\" {
                return format!("{}:{}", &tail[..1], &tail[2..]);
            }
        }
    }
    text
}

#[cfg(any(windows, test))]
fn owns_startup(record: &Startup, worker: &Path) -> bool {
    record.v == 1 && startup_path_key(&record.worker) == startup_path_key(worker)
}

#[cfg(windows)]
mod startup_records {
    use super::*;
    // This owner-profile file is outside AppData and the MSIX virtual registry.
    // The normal installer owns the sole HKCU Run value; packaged CLIs only
    // record verified nonsecret paths here for that normal logon process.
    fn folder() -> Result<PathBuf> {
        let profile = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .ok_or_else(|| err("The owner profile is unavailable"))?;
        let physical = fs::canonicalize(&profile)?;
        if startup_path_key(&physical) != startup_path_key(&profile) {
            return Err(err("The owner profile is redirected or cannot be verified"));
        }
        let path = physical.join(".dgxbridge");
        fs::create_dir_all(&path)?;
        let info = fs::symlink_metadata(&path)?;
        if !info.is_dir()
            || reparse(&info)
            || startup_path_key(&fs::canonicalize(&path)?) != startup_path_key(&path)
        {
            return Err(err(
                "The result-worker startup folder is linked or redirected",
            ));
        }
        Ok(path)
    }
    fn read(path: &Path) -> Result<Vec<Startup>> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let info = fs::symlink_metadata(path)?;
        if !info.is_file() || reparse(&info) || info.len() > inventory::MIB {
            return Err(err(
                "The result-worker startup record is unsafe or too large",
            ));
        }
        let records: Vec<Startup> = serde_json::from_slice(&fs::read(path)?)?;
        if records.len() > 128 {
            return Err(err("Too many result-worker startup states"));
        }
        Ok(records)
    }
    pub(super) fn register(startup: Startup) -> Result<()> {
        let root = folder()?;
        let lock_path = root.join("startup.lock");
        if lock_path.exists() && reparse(&fs::symlink_metadata(&lock_path)?) {
            return Err(err("The result-worker startup lock is linked"));
        }
        let guard = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        fs2::FileExt::lock_exclusive(&guard)?;
        let file = root.join("return-workers.json");
        let mut records = read(&file)?;
        if !records.iter().any(|r| {
            owns_startup(r, &startup.worker)
                && startup_path_key(&r.state_dir) == startup_path_key(&startup.state_dir)
        }) {
            if records.len() >= 128 {
                return Err(err("Too many result-worker startup states"));
            }
            records.push(startup);
            state::atomic_json(&file, &records)?;
        }
        Ok(())
    }
    pub(super) fn owned(worker: &Path) -> Result<Vec<Startup>> {
        Ok(read(&folder()?.join("return-workers.json"))?
            .into_iter()
            .filter(|r| owns_startup(r, worker) && r.state_dir.is_absolute())
            .collect())
    }
}

#[cfg(windows)]
fn worker_executable() -> Result<PathBuf> {
    Ok(fs::canonicalize(
        std::env::current_exe()?.with_file_name("sparkdev-return.exe"),
    )?)
}

#[cfg(windows)]
fn worker_command_line(worker: &Path, state: &Path) -> Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    let mut line = Vec::new();
    for arg in [
        worker.as_os_str(),
        std::ffi::OsStr::new("--state-dir"),
        state.as_os_str(),
    ] {
        if !line.is_empty() {
            line.push(b' ' as u16);
        }
        line.push(b'"' as u16);
        let mut slashes = 0;
        for unit in arg.encode_wide() {
            if unit == 0 {
                return Err(err("The result-worker path contains a null character"));
            }
            if unit == b'\\' as u16 {
                slashes += 1;
                continue;
            }
            // Windows argv doubles backslashes before a quote, including the
            // closing quote. Preserve UTF-16 paths without lossy conversion.
            line.extend(std::iter::repeat_n(
                b'\\' as u16,
                if unit == b'"' as u16 {
                    slashes * 2 + 1
                } else {
                    slashes
                },
            ));
            line.push(unit);
            slashes = 0;
        }
        line.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
        line.push(b'"' as u16);
    }
    line.push(0);
    if line.len() > 32767 {
        return Err(err(
            "The result-worker command exceeds the Windows command-line limit",
        ));
    }
    Ok(line)
}

#[cfg(windows)]
fn spawn_return_worker(worker: &Path, state: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{
            CREATE_NEW_PROCESS_GROUP, CreateProcessW, DETACHED_PROCESS, PROCESS_INFORMATION,
            STARTUPINFOW,
        },
    };
    let mut line = worker_command_line(worker, state)?;
    let application: Vec<u16> = worker.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // Replacing the standard streams with NUL does not exclude other inherited
    // capture-pipe handles. No handles may reach this persistent process, or
    // the submitting caller can wait forever for EOF after the CLI has exited.
    // The explicit state argument configures SPARKDEV_HOME before child threads.
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
            "The automatic result worker could not be started",
        ));
    }
    unsafe {
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
    }
    Ok(())
}

pub fn resume_autostart(default_state: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        let worker = worker_executable()?;
        let mut states: BTreeMap<String, PathBuf> = startup_records::owned(&worker)?
            .into_iter()
            .map(|record| (startup_path_key(&record.state_dir), record.state_dir))
            .collect();
        if default_state.join("config.json").is_file() {
            states.insert(startup_path_key(default_state), default_state.into());
        }
        for state in states.into_values() {
            // The new process performs physical-state validation before work.
            if existing_state(&state).is_ok() {
                spawn_return_worker(&worker, &state)?;
            }
        }
    }
    #[cfg(not(windows))]
    let _ = default_state;
    Ok(())
}

pub fn worker() -> Result<()> {
    verified_worker_state()?;
    let Some(_guard) = lock("worker.lock")? else {
        return Ok(());
    };
    loop {
        if records()?.iter().all(|(_, r)| r.complete) {
            std::thread::sleep(Duration::from_secs(15));
            continue;
        }
        if let Ok(mut tr) = transport::Transport::new() {
            let _ = pump(&mut tr, Duration::from_secs(20));
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}
pub fn ensure_worker() -> Result<()> {
    if records()?.iter().all(|(_, r)| r.complete) {
        return Ok(());
    }
    #[cfg(windows)]
    let startup = Startup {
        v: 1,
        state_dir: verified_worker_state()?,
        worker: worker_executable()?,
    };
    #[cfg(windows)]
    startup_records::register(Startup {
        v: 1,
        state_dir: startup.state_dir.clone(),
        worker: startup.worker.clone(),
    })?;
    // Check while held, then release before spawn. Races may create an extra
    // process briefly; the process-level lock admits only one transfer worker.
    let Some(guard) = lock("worker.lock")? else {
        return Ok(());
    };
    drop(guard);
    #[cfg(windows)]
    {
        spawn_return_worker(&startup.worker, &startup.state_dir)
    }
    #[cfg(not(windows))]
    {
        crate::local::spawn_detached("__return_worker", None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[cfg(windows)]
    #[test]
    fn worker_argv_round_trips_windows_state_paths() {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::{Foundation::LocalFree, UI::Shell::CommandLineToArgvW};
        let worker = Path::new(r"\\?\UNC\localhost\C$\Owner Apps\DGXBridge\sparkdev-return.exe");
        for state in [
            Path::new(r"C:\Owner Data\DGXBridge"),
            Path::new(r"\\?\UNC\localhost\C$\Owner Data\"),
            Path::new(r"C:\"),
            Path::new("C:\\Données\\火花"),
        ] {
            let line = worker_command_line(worker, state).unwrap();
            let mut count = 0;
            let parsed = unsafe { CommandLineToArgvW(line.as_ptr(), &mut count) };
            assert!(!parsed.is_null());
            let actual: Vec<Vec<u16>> = unsafe {
                std::slice::from_raw_parts(parsed, count as usize)
                    .iter()
                    .map(|&arg| {
                        let mut len = 0;
                        while *arg.add(len) != 0 {
                            len += 1;
                        }
                        std::slice::from_raw_parts(arg, len).to_vec()
                    })
                    .collect()
            };
            unsafe {
                LocalFree(parsed.cast());
            }
            let expected: Vec<Vec<u16>> = [
                worker.as_os_str(),
                std::ffi::OsStr::new("--state-dir"),
                state.as_os_str(),
            ]
            .into_iter()
            .map(|arg| arg.encode_wide().collect())
            .collect();
            assert_eq!(actual, expected);
        }
        assert!(worker_command_line(worker, Path::new("C:\\bad\0state")).is_err());
        assert!(worker_command_line(worker, Path::new(&"a".repeat(32768))).is_err());
    }

    #[test]
    fn startup_ownership_matches_physical_windows_spellings_only() {
        let worker = PathBuf::from(r"C:\Users\Owner\DGXBridge\sparkdev-return.exe");
        for spelling in [
            r"\\?\C:\Users\Owner\DGXBridge\sparkdev-return.exe",
            r"\\localhost\C$\Users\Owner\DGXBridge\sparkdev-return.exe",
            r"\\?\UNC\127.0.0.1\C$\Users\Owner\DGXBridge\sparkdev-return.exe",
        ] {
            let record = Startup {
                v: 1,
                state_dir: PathBuf::from("unused"),
                worker: spelling.into(),
            };
            assert!(owns_startup(&record, &worker));
        }
        let foreign = Startup {
            v: 1,
            state_dir: PathBuf::from("unused"),
            worker: r"\\other-host\C$\Users\Owner\DGXBridge\sparkdev-return.exe".into(),
        };
        assert!(!owns_startup(&foreign, &worker));
        assert!(!owns_startup(&Startup { v: 2, ..foreign }, &worker));
    }

    #[test]
    fn worker_state_validation_never_creates_missing_identity() {
        let root = tempdir().unwrap();
        let absent = root.path().join("absent");
        assert!(existing_state(&absent).is_err());
        assert!(!absent.exists());
        assert!(existing_state(Path::new("relative-state")).is_err());
        fs::write(root.path().join("config.json"), b"{}").unwrap();
        assert!(existing_state(root.path()).is_err());
        fs::write(
            root.path().join("config.json"),
            br#"{"ssh_alias":"spark","hosts":[],"host_key_alias":"pin","laptop_jobs":1}"#,
        )
        .unwrap();
        assert_eq!(
            existing_state(root.path()).unwrap(),
            fs::canonicalize(root.path()).unwrap()
        );
        fs::remove_file(root.path().join("config.json")).unwrap();
        fs::create_dir(root.path().join("config.json")).unwrap();
        assert!(existing_state(root.path()).is_err());
    }

    fn entry(path: &str, bytes: &[u8], original: Option<&[u8]>) -> Entry {
        Entry {
            scope: "work".into(),
            path: path.into(),
            bytes: bytes.len() as u64,
            sha256: inventory::hash(bytes),
            chunks: bytes
                .chunks(inventory::CHUNK)
                .map(inventory::hash)
                .collect(),
            original_sha256: original.map(inventory::hash),
        }
    }
    fn record(project: &Path) -> Record {
        Record {
            v: 1,
            request_id: "request".into(),
            project: project.into(),
            originals: BTreeMap::new(),
            request: json!({}),
            job: Some("job1".into()),
            disabled: false,
            submission: None,
            complete: false,
            returned: vec![],
            error: None,
            manifest_sha256: None,
        }
    }
    fn manifest(entries: &[Entry]) -> Value {
        let value = json!({"v":1,"entries":entries});
        json!({"manifest_sha256":inventory::hash(&serde_json::to_vec(&value).unwrap()),"manifest":value})
    }
    struct ChunkRpc {
        bytes: Vec<u8>,
        calls: Vec<usize>,
        corrupt: bool,
    }
    impl Rpc for ChunkRpc {
        fn call(&mut self, v: Value) -> Result<Value> {
            assert_eq!(v["op"], "output_chunk");
            let i = v["index"].as_u64().unwrap() as usize;
            self.calls.push(i);
            let bytes = self.bytes.chunks(inventory::CHUNK).nth(i).unwrap();
            Ok(
                json!({"index":i,"bytes":bytes.len(),"data":STANDARD.encode(bytes),"sha256":if self.corrupt {"0".repeat(64)} else {inventory::hash(bytes)}}),
            )
        }
        fn route(&self) -> String {
            "ssh".into()
        }
        fn connection_enabled(&self) -> bool {
            true
        }
    }
    /// A LAN controller double for result copies: answers the submission and
    /// return RPCs, and hands out `copy` as the file-copy command, recording
    /// how much was already staged each time a copy started.
    struct LanRpc {
        bytes: Vec<u8>,
        ops: Vec<String>,
        chunks: Vec<usize>,
        submitted: Vec<String>,
        copy: fn() -> std::process::Command,
        staged_at_copy: std::cell::RefCell<Vec<Option<u64>>>,
    }
    impl LanRpc {
        fn new(bytes: Vec<u8>, copy: fn() -> std::process::Command) -> Self {
            Self {
                bytes,
                ops: vec![],
                chunks: vec![],
                submitted: vec![],
                copy,
                staged_at_copy: Default::default(),
            }
        }
    }
    impl Rpc for LanRpc {
        fn call(&mut self, v: Value) -> Result<Value> {
            let op = v["op"].as_str().unwrap().to_string();
            self.ops.push(op.clone());
            Ok(match op.as_str() {
                "run" => {
                    let id = v["request_id"].as_str().unwrap().to_string();
                    self.submitted.push(id.clone());
                    json!({"ok":true,"job":format!("job-{id}"),"state":"queued"})
                }
                "outputs" => {
                    let mut out = manifest(&[entry("render.mp4", &self.bytes, None)]);
                    out["state"] = json!("ready");
                    out
                }
                "output_locate" => {
                    json!({"file":"/spark/results/job/0","bytes":self.bytes.len(),"sha256":inventory::hash(&self.bytes)})
                }
                "output_chunk" => {
                    let i = v["index"].as_u64().unwrap() as usize;
                    self.chunks.push(i);
                    let bytes = self.bytes.chunks(inventory::CHUNK).nth(i).unwrap();
                    json!({"index":i,"bytes":bytes.len(),"data":STANDARD.encode(bytes),"sha256":inventory::hash(bytes)})
                }
                "outputs_ack" => {
                    json!({"acknowledged":true,"manifest_sha256":v["manifest_sha256"]})
                }
                other => panic!("unexpected {other}"),
            })
        }
        fn route(&self) -> String {
            "ssh".into()
        }
        fn connection_enabled(&self) -> bool {
            true
        }
        fn copy_command(
            &self,
            dir: &Path,
            name: &str,
            remote: &str,
            upload: bool,
        ) -> Option<(std::process::Command, Vec<u8>)> {
            assert!(!upload && remote == "/spark/results/job/0");
            self.staged_at_copy
                .borrow_mut()
                .push(fs::metadata(dir.join(name)).ok().map(|m| m.len()));
            let mut command = (self.copy)();
            command.current_dir(dir);
            Some((command, Vec::new()))
        }
    }
    /// A copy that brings nothing and succeeds.
    fn idle_copy() -> std::process::Command {
        if cfg!(windows) {
            let mut command = std::process::Command::new("cmd");
            command.args(["/C", "exit 0"]);
            command
        } else {
            std::process::Command::new("true")
        }
    }
    /// A copy that would take a minute.
    fn slow_copy() -> std::process::Command {
        let mut command = std::process::Command::new(if cfg!(windows) { "ping" } else { "sleep" });
        if cfg!(windows) {
            command.args(["-n", "61", "127.0.0.1"]);
        } else {
            command.arg("60");
        }
        command
    }
    #[test]
    fn a_copied_result_continues_its_staged_prefix_and_never_starts_over() {
        let temp = tempdir().unwrap();
        let stage = temp.path().join("part");
        let bytes: Vec<u8> = (0..5 * inventory::CHUNK + 7)
            .map(|i| (i % 251) as u8)
            .collect();
        fs::write(&stage, &bytes[..3 * inventory::CHUNK]).unwrap();
        let mut rpc = LanRpc::new(bytes.clone(), idle_copy);
        let item = entry("render.mp4", &bytes, None);
        let until = Instant::now() + Duration::from_secs(30);
        assert!(download(&mut rpc, "job1", &item, &stage, until).unwrap());
        // The copy started on the 3 MiB already staged, and RPC repaired only the rest.
        assert_eq!(
            *rpc.staged_at_copy.borrow(),
            vec![Some(3 * inventory::CHUNK as u64)]
        );
        assert_eq!(rpc.chunks, vec![3, 4, 5]);
        assert_eq!(fs::read(&stage).unwrap(), bytes);
        // A full-size stage is only verified, never copied again.
        assert!(download(&mut rpc, "job1", &item, &stage, until).unwrap());
        assert_eq!(rpc.staged_at_copy.borrow().len(), 1);
    }
    #[test]
    fn a_result_copy_checks_the_free_space_reserve_before_any_byte_lands() {
        let temp = tempdir().unwrap();
        let stage = temp.path().join("part");
        let mut rpc = LanRpc::new(vec![], idle_copy);
        let mut huge = entry("render.mp4", &[0; 8], None);
        huge.bytes = 1 << 60;
        let until = Instant::now() + Duration::from_secs(30);
        copy_whole(&mut rpc, "job1", &huge, &stage, until);
        assert!(rpc.ops.is_empty());
        assert!(rpc.staged_at_copy.borrow().is_empty());
        assert!(!stage.exists());
    }
    #[test]
    fn a_slow_result_copy_never_holds_a_pending_submission_past_one_pump() {
        let home = state::TestHome::new();
        let (inv, mut render) = queued(&home.path);
        render["request_id"] = json!("render");
        register(&inv, &render, false).unwrap();
        let mut rpc = LanRpc::new(vec![7; 5 * inventory::CHUNK], slow_copy);
        // The first pass submits the render and starts its return over a copy
        // that would take a minute; the pass still ends with its budget.
        let started = Instant::now();
        pump(&mut rpc, Duration::from_secs(2)).unwrap();
        let held = started.elapsed();
        assert!(held < Duration::from_secs(6), "one pump held for {held:?}");
        assert_eq!(rpc.staged_at_copy.borrow().len(), 1);
        // Another agent's job arrives meanwhile: the very next pass submits it.
        let (_, mut other) = queued(&home.path);
        other["request_id"] = json!("other");
        register(&inv, &other, true).unwrap();
        pump(&mut rpc, Duration::from_secs(2)).unwrap();
        assert_eq!(rpc.submitted, vec!["render", "other"]);
        assert_eq!(
            submission("other", Duration::ZERO).unwrap()["job"],
            "job-other"
        );
    }
    #[test]
    fn resume_keeps_verified_prefix_and_repairs_torn_chunk() {
        let temp = tempdir().unwrap();
        let stage = temp.path().join("part");
        let bytes = vec![42; inventory::CHUNK * 2 + 17];
        fs::write(&stage, &bytes[..inventory::CHUNK + 9]).unwrap();
        let mut rpc = ChunkRpc {
            bytes: bytes.clone(),
            calls: vec![],
            corrupt: false,
        };
        assert!(
            download(
                &mut rpc,
                "job1",
                &entry("render.mp4", &bytes, None),
                &stage,
                Instant::now() + Duration::from_secs(3)
            )
            .unwrap()
        );
        assert_eq!(rpc.calls, vec![1, 2]);
        assert_eq!(fs::read(stage).unwrap(), bytes);
    }
    #[test]
    fn expired_budget_after_prefix_validation_still_advances_one_chunk() {
        let temp = tempdir().unwrap();
        let stage = temp.path().join("part");
        let bytes = vec![42; inventory::CHUNK * 3];
        fs::write(&stage, &bytes[..inventory::CHUNK]).unwrap();
        let mut rpc = ChunkRpc {
            bytes: bytes.clone(),
            calls: vec![],
            corrupt: false,
        };
        let item = entry("render.mp4", &bytes, None);
        assert!(!download(&mut rpc, "job1", &item, &stage, Instant::now()).unwrap());
        assert_eq!(rpc.calls, vec![1]);
        assert_eq!(
            fs::metadata(&stage).unwrap().len(),
            (inventory::CHUNK * 2) as u64
        );
        assert!(download(&mut rpc, "job1", &item, &stage, Instant::now()).unwrap());
        assert_eq!(rpc.calls, vec![1, 2]);
        assert_eq!(fs::read(stage).unwrap(), bytes);
    }
    #[test]
    fn corrupt_chunk_is_never_committed() {
        let temp = tempdir().unwrap();
        let stage = temp.path().join("part");
        let mut rpc = ChunkRpc {
            bytes: b"remote".to_vec(),
            calls: vec![],
            corrupt: true,
        };
        assert!(
            download(
                &mut rpc,
                "job1",
                &entry("out.txt", b"remote", None),
                &stage,
                Instant::now() + Duration::from_secs(2)
            )
            .is_err()
        );
        assert_eq!(fs::metadata(stage).unwrap().len(), 0);
    }
    #[test]
    fn changed_laptop_file_is_preserved_and_conflict_is_idempotent() {
        let temp = tempdir().unwrap();
        let stage = temp.path().join("stage");
        fs::write(&stage, b"remote").unwrap();
        fs::write(temp.path().join("scene.py"), b"local edit").unwrap();
        let e = entry("scene.py", b"remote", Some(b"original"));
        for _ in 0..2 {
            let r = apply(&e, &stage, temp.path(), temp.path(), "job1").unwrap();
            assert_eq!(r["status"], "conflict");
        }
        assert_eq!(
            fs::read(temp.path().join("scene.py")).unwrap(),
            b"local edit"
        );
        assert_eq!(
            fs::read(temp.path().join("scene.spark-job1.py")).unwrap(),
            b"remote"
        );
    }
    #[test]
    fn unchanged_original_replaced_but_original_inode_is_recoverable() {
        let temp = tempdir().unwrap();
        let stage = temp.path().join("stage");
        fs::write(&stage, b"remote").unwrap();
        fs::write(temp.path().join("x.txt"), b"original").unwrap();
        let e = entry("x.txt", b"remote", Some(b"original"));
        assert_eq!(
            apply(&e, &stage, temp.path(), temp.path(), "job1").unwrap()["status"],
            "written"
        );
        assert_eq!(
            apply(&e, &stage, temp.path(), temp.path(), "job1").unwrap()["status"],
            "unchanged"
        );
        let backup = fs::read_dir(temp.path())
            .unwrap()
            .flatten()
            .find(|e| e.file_name().to_string_lossy().contains(".spark-original-"))
            .unwrap();
        assert_eq!(fs::read(backup.path()).unwrap(), b"original");
    }
    #[cfg(unix)]
    #[test]
    fn replacement_preserves_executable_permissions_without_special_bits() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempdir().unwrap();
        let stage = temp.path().join("stage");
        let target = temp.path().join("script.sh");
        fs::write(&stage, b"remote script").unwrap();
        fs::write(&target, b"original script").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o6750)).unwrap();
        let e = entry("script.sh", b"remote script", Some(b"original script"));
        apply(&e, &stage, temp.path(), temp.path(), "job1").unwrap();
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o7777,
            0o750
        );
        assert_eq!(fs::read(&target).unwrap(), b"remote script");
    }
    #[test]
    fn disk_space_gate_keeps_headroom_and_rejects_overflow() {
        assert!(check_space(SPACE_RESERVE + 100, 100).is_ok());
        assert!(
            check_space(SPACE_RESERVE + 99, 100)
                .unwrap_err()
                .message
                .contains("Free space")
        );
        assert!(check_space(u64::MAX, u64::MAX).is_err());
        assert!(check_space(SPACE_RESERVE, 0).is_ok());
    }
    #[test]
    fn manifest_rejects_case_aliases_and_forged_original() {
        let temp = tempdir().unwrap();
        let r = record(temp.path());
        let e = entry("a.txt", b"remote", Some(b"forged"));
        assert!(parse_manifest(&manifest(&[e]), &r).is_err());
        let a = entry("A.txt", b"a", None);
        let b = entry("a.txt", b"b", None);
        assert!(parse_manifest(&manifest(&[a, b]), &r).is_err());
    }
    #[test]
    fn manifest_rejects_file_parent_collisions_before_any_application() {
        let temp = tempdir().unwrap();
        let r = record(temp.path());
        for (first, second) in [
            ("folder", "folder/file.txt"),
            ("A/CHILD.txt", "a"),
            ("a/b", "A/b/c"),
        ] {
            let entries = [entry(first, b"one", None), entry(second, b"two", None)];
            assert!(parse_manifest(&manifest(&entries), &r).is_err());
        }
        let mut output = entry("folder/file.txt", b"two", None);
        output.scope = "output".into();
        assert!(parse_manifest(&manifest(&[entry("folder", b"one", None), output]), &r).is_ok());
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    }
    #[test]
    fn paths_cannot_escape_or_alias_windows_devices() {
        for name in [
            "../bad",
            "a/../../b",
            "C:/bad",
            "a\\bad",
            "NUL.txt",
            "a/COM1",
            "file:stream",
            "trailing.",
            "a/CON ",
            ".ssh/id_rsa",
            "target/cache",
        ] {
            assert!(valid_path(name).is_err(), "{name}");
        }
    }
    #[cfg(unix)]
    #[test]
    fn symlink_parent_and_target_never_receive_results() {
        use std::os::unix::fs::symlink;
        let temp = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let stage = temp.path().join("stage");
        fs::write(&stage, b"remote").unwrap();
        symlink(outside.path(), temp.path().join("linked")).unwrap();
        assert!(
            apply(
                &entry("linked/x", b"remote", None),
                &stage,
                temp.path(),
                temp.path(),
                "job1"
            )
            .is_err()
        );
        fs::write(outside.path().join("x"), b"safe").unwrap();
        symlink(outside.path().join("x"), temp.path().join("x")).unwrap();
        assert!(
            apply(
                &entry("x", b"remote", None),
                &stage,
                temp.path(),
                temp.path(),
                "job1"
            )
            .is_err()
        );
        assert_eq!(fs::read(outside.path().join("x")).unwrap(), b"safe");
    }
    #[test]
    fn unicode_manifest_uses_utf8_canonical_hash() {
        let temp = tempdir().unwrap();
        let e = entry("rendu-é.png", b"pixels", None);
        let m = manifest(&[e]);
        assert_eq!(parse_manifest(&m, &record(temp.path())).unwrap().len(), 1);
        let mut tampered = m;
        tampered["manifest"]["entries"][0]["path"] = json!("changed.png");
        assert!(parse_manifest(&tampered, &record(temp.path())).is_err());
    }

    #[derive(Default)]
    struct SubmissionRpc {
        replies: std::collections::VecDeque<Result<Value>>,
        requests: Vec<Value>,
    }
    impl Rpc for SubmissionRpc {
        fn call(&mut self, value: Value) -> Result<Value> {
            self.requests.push(value);
            self.replies
                .pop_front()
                .expect("Unexpected additional submission")
        }
        fn route(&self) -> String {
            "fixture".into()
        }
        fn connection_enabled(&self) -> bool {
            true
        }
    }
    fn queued(root: &Path) -> (inventory::Inventory, Value) {
        let inv = inventory::Inventory {
            root: root.into(),
            project: "project".into(),
            manifest_id: "manifest".into(),
            manifest: vec![],
            bytes: 0,
            small_bytes: 0,
            sources: BTreeMap::new(),
            cas: None,
        };
        let request = json!({"v":1,"op":"run","request_id":"retained-id","project":"project",
            "manifest_id":"manifest","argv":["python3","task.py"],"runtime":"python","client":"codex","no_return":true});
        (inv, request)
    }
    #[test]
    fn only_delivery_owner_submits_and_registration_preserves_its_receipt() {
        let home = state::TestHome::new();
        let (inv, request) = queued(&home.path);
        let response = json!({"job":"job1","state":"queued","ok":true});
        let mut rpc = SubmissionRpc {
            replies: [Ok(response.clone())].into(),
            ..Default::default()
        };
        register(&inv, &request, true).unwrap();
        let guard = lock("delivery.lock").unwrap().unwrap();
        pump(&mut rpc, Duration::from_secs(1)).unwrap();
        assert!(rpc.requests.is_empty());
        drop(guard);
        pump(&mut rpc, Duration::from_secs(1)).unwrap();
        register(&inv, &request, true).unwrap(); // A foreground retry is read-only.
        pump(&mut rpc, Duration::from_secs(1)).unwrap();
        assert_eq!(rpc.requests, vec![request]);
        assert_eq!(submission("retained-id", Duration::ZERO).unwrap(), response);
        let saved: Record =
            serde_json::from_slice(&fs::read(record_path("retained-id").unwrap()).unwrap())
                .unwrap();
        assert!(saved.complete);
    }
    #[test]
    fn definitive_rejection_is_saved_and_never_replayed() {
        let home = state::TestHome::new();
        let (inv, request) = queued(&home.path);
        register(&inv, &request, true).unwrap();
        let mut rpc = SubmissionRpc {
            replies: [Err(AppError::new(6, "worktree_busy: already claimed"))].into(),
            ..Default::default()
        };
        pump(&mut rpc, Duration::from_secs(1)).unwrap();
        pump(&mut rpc, Duration::from_secs(1)).unwrap();
        assert_eq!(rpc.requests.len(), 1);
        assert!(
            submission("retained-id", Duration::ZERO)
                .unwrap_err()
                .message
                .contains("worktree_busy")
        );
        let saved: Record =
            serde_json::from_slice(&fs::read(record_path("retained-id").unwrap()).unwrap())
                .unwrap();
        assert!(saved.complete);
        assert!(saved.job.is_none());
    }
    #[test]
    fn uncertain_transport_failure_replays_exact_retained_submission() {
        let home = state::TestHome::new();
        let (inv, request) = queued(&home.path);
        register(&inv, &request, true).unwrap();
        let mut rpc = SubmissionRpc {
            replies: [
                Err(AppError::new(4, "connection dropped")),
                Ok(json!({"job":"job1","state":"queued","ok":true})),
            ]
            .into(),
            ..Default::default()
        };
        pump(&mut rpc, Duration::from_secs(1)).unwrap();
        let pending: Record =
            serde_json::from_slice(&fs::read(record_path("retained-id").unwrap()).unwrap())
                .unwrap();
        assert!(!pending.complete);
        pump(&mut rpc, Duration::from_secs(1)).unwrap();
        assert_eq!(rpc.requests, vec![request.clone(), request]);
        assert_eq!(
            submission("retained-id", Duration::ZERO).unwrap()["job"],
            "job1"
        );
        let recovered: Record =
            serde_json::from_slice(&fs::read(record_path("retained-id").unwrap()).unwrap())
                .unwrap();
        assert!(recovered.error.is_none());
    }
}
