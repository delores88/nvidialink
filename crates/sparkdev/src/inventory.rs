use crate::{AppError, Result, state};
use regex::bytes::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
    process::Command,
};
/// Transfer chunk size; a controller advertising any other value is refused.
pub const CHUNK: usize = 1024 * 1024;
pub const MIB: u64 = CHUNK as u64;
pub const GIB: u64 = 1024 * MIB;
/// Unchanged protocol bounds.
pub const MAX_REQUEST: u64 = 32 * MIB;
pub const MAX_FILES: usize = 100_000;
/// Source limits, mirrored by the controller. Files over LARGE_FILE are "large";
/// MAX_SMALL_TREE caps the combined size of every other file.
pub const MAX_FILE: u64 = 16 * GIB;
pub const MAX_TREE: u64 = 32 * GIB;
pub const MAX_SMALL_TREE: u64 = 8 * GIB;
pub const LARGE_FILE: u64 = 512 * MIB;
/// Laptop space kept free when staging transfer objects.
pub const CAS_RESERVE: u64 = 4 * GIB;
/// What a controller that advertises no limits accepts from this CLI.
pub const LEGACY_MAX_FILE: u64 = 512 * MIB;
pub const LEGACY_MAX_TREE: u64 = 8 * GIB;
pub const LEGACY_BATCH_BODY: u64 = 3 * MIB;
pub const LEGACY_MISSING_BATCH: usize = 2048;
/// Look-ahead into the next chunk when one chunk is scanned for credentials.
pub const SCAN_OVERLAP: usize = 64 * 1024;
/// Injectable inventory limits; `Limits::default()` is what the CLI enforces.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_file: u64,
    pub max_tree: u64,
    pub max_small_tree: u64,
    pub large_file: u64,
    pub max_files: usize,
    /// Free bytes where transfer objects are staged; `None` stages nothing.
    pub available_space: fn(&Path) -> Option<u64>,
}
fn available_space(path: &Path) -> Option<u64> {
    fs2::available_space(path).ok()
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file: MAX_FILE,
            max_tree: MAX_TREE,
            max_small_tree: MAX_SMALL_TREE,
            large_file: LARGE_FILE,
            max_files: MAX_FILES,
            available_space,
        }
    }
}
impl Limits {
    pub fn refusal(&self) -> AppError {
        AppError::new(
            5,
            format!(
                "Source exceeds the {} file, {} project or {} small-file limit",
                size_text(self.max_file),
                size_text(self.max_tree),
                size_text(self.max_small_tree)
            ),
        )
    }
}
pub fn size_text(bytes: u64) -> String {
    if bytes >= GIB && bytes % GIB == 0 {
        format!("{} GiB", bytes / GIB)
    } else if bytes >= MIB && bytes % MIB == 0 {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{bytes} bytes")
    }
}
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Chunk {
    pub sha256: String,
    pub size: u64,
}
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Entry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub mode: u32,
    pub chunks: Vec<Chunk>,
}
/// Where a chunk's bytes first occur in the source tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub path: PathBuf,
    pub offset: u64,
    pub len: u64,
}
#[derive(Debug)]
pub struct Inventory {
    pub root: PathBuf,
    pub project: String,
    pub manifest_id: String,
    pub manifest: Vec<Entry>,
    pub bytes: u64,
    /// Combined size of files at or below `LARGE_FILE`.
    pub small_bytes: u64,
    /// First occurrence of every chunk, for reads when a chunk is not staged. Never serialized.
    pub sources: BTreeMap<String, Source>,
    /// The transfer-object store this inventory staged into, if any.
    pub cas: Option<PathBuf>,
}
pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn excluded(part: &str) -> bool {
    matches!(
        part.to_ascii_lowercase().as_str(),
        ".git"
            | "target"
            | "node_modules"
            | "dist"
            | "build"
            | ".next"
            | ".venv"
            | "venv"
            | "__pycache__"
            | ".pytest_cache"
            | ".mypy_cache"
    )
}
pub fn path_policy(path: &str) -> Result<bool> {
    if path.is_empty()
        || path.contains('\\')
        || path.contains(':')
        || path.starts_with('/')
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return Err(AppError::new(5, "Unsafe relative source path"));
    }
    let parts: Vec<_> = path.split('/').collect();
    if parts.iter().any(|s| excluded(s)) {
        return Ok(false);
    }
    let name = parts.last().unwrap().to_ascii_lowercase();
    let ext = Path::new(&name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let secret_dirs = [
        ".ssh",
        ".aws",
        ".azure",
        ".gnupg",
        ".kube",
        ".docker",
        ".password-store",
        "secrets",
        "credentials",
        "private_keys",
    ];
    let exact = [
        ".netrc",
        "netrc",
        ".npmrc",
        ".pypirc",
        ".git-credentials",
        ".dockercfg",
        "known_hosts",
        "authorized_keys",
        ".htpasswd",
    ];
    let env_template = matches!(
        name.as_str(),
        ".env.example" | ".env.sample" | ".env.template" | ".env.defaults"
    );
    let data = matches!(
        ext,
        "" | "json"
            | "yaml"
            | "yml"
            | "toml"
            | "ini"
            | "cfg"
            | "conf"
            | "txt"
            | "env"
            | "properties"
            | "xml"
            | "csv"
            | "dat"
            | "bin"
            | "db"
            | "sqlite"
            | "sqlite3"
    );
    let suspicious = [
        "credential",
        "password",
        "passwd",
        "private_key",
        "private-key",
        "api_key",
        "api-key",
        "client_secret",
        "access_token",
    ];
    if parts[..parts.len() - 1]
        .iter()
        .any(|p| secret_dirs.contains(&p.to_ascii_lowercase().as_str()))
        || exact.contains(&name.as_str())
        || name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
        || name.starts_with("id_ecdsa")
        || name.starts_with("nvsync.")
        || (name.starts_with(".env") && !env_template)
        || matches!(
            ext,
            "pem"
                | "key"
                | "p12"
                | "pfx"
                | "jks"
                | "keystore"
                | "kdbx"
                | "ppk"
                | "ovpn"
                | "gpg"
                | "pgp"
                | "tfvars"
                | "age"
                | "asc"
        )
        || (data && suspicious.iter().any(|s| name.contains(s)))
    {
        return Err(AppError::new(
            5,
            format!("Refused credential-pattern file: {path}"),
        ));
    }
    Ok(true)
}
fn credential_pattern() -> &'static Regex {
    // Patterns require credential-shaped values, not source identifiers alone. Bytes
    // mode with ASCII word boundaries: no lossy decoding, and a token beside a
    // non-ASCII byte is still found.
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"(?-u)-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----|PuTTY-User-Key-File-[0-9]|\b(?:AKIA|ASIA)[A-Z0-9]{16}\b|\bgh[pousr]_[A-Za-z0-9]{30,}\b|\bgithub_pat_[A-Za-z0-9_]{40,}\b|\bsk-(?:proj-)?[A-Za-z0-9_-]{32,}\b|\bhf_[A-Za-z]{34}\b").unwrap())
}
fn credential_refusal() -> AppError {
    AppError::new(
        5,
        "Recognizable credential content refused (contents withheld)",
    )
}
/// Scan one whole buffer, such as an argument.
pub fn scan(bytes: &[u8]) -> Result<()> {
    if credential_pattern().is_match(bytes) {
        return Err(credential_refusal());
    }
    Ok(())
}
/// Scan one chunk of a file. The window is the previous chunk's last byte (word
/// boundary context), the chunk, and up to 64 KiB of the next chunk; a match
/// counts only if it starts inside this chunk, so every token is found exactly
/// where a whole-buffer scan would find it.
pub fn scan_chunk(previous: Option<u8>, chunk: &[u8], next: &[u8]) -> Result<()> {
    let ahead = &next[..next.len().min(SCAN_OVERLAP)];
    let mut window = Vec::with_capacity(1 + chunk.len() + ahead.len());
    window.extend(previous);
    let start = window.len();
    window.extend_from_slice(chunk);
    window.extend_from_slice(ahead);
    if credential_pattern()
        .find_at(&window, start)
        .is_some_and(|found| found.start() < start + chunk.len())
    {
        return Err(credential_refusal());
    }
    Ok(())
}
fn source_changed() -> AppError {
    AppError::new(3, "Source file changed while it was read; retry")
}
/// One file's streamed digest.
#[derive(Debug)]
pub struct FileDigest {
    pub sha256: String,
    pub size: u64,
    pub chunks: Vec<Chunk>,
}
/// Fill one buffer with exactly `CHUNK` bytes unless the stream ends first.
fn fill(reader: &mut impl Read) -> Result<Vec<u8>> {
    let mut buffer = vec![0; CHUNK];
    let mut filled = 0;
    while filled < CHUNK {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    buffer.truncate(filled);
    Ok(buffer)
}
/// Stream a file of `expected` bytes: per-chunk hashes, an incremental whole-file
/// hash and a credential scan, holding at most two chunks. `each` receives every
/// chunk with its offset. Reading never goes past `expected + 1` bytes, and any
/// other length than `expected` means the file changed while it was read.
pub fn digest_stream(
    reader: impl Read,
    expected: u64,
    mut each: impl FnMut(&str, &[u8], u64) -> Result<()>,
) -> Result<FileDigest> {
    let mut reader = reader.take(expected.saturating_add(1));
    let mut whole = Sha256::new();
    let mut chunks = Vec::new();
    let (mut offset, mut previous) = (0u64, None);
    let mut current = fill(&mut reader)?;
    while !current.is_empty() {
        if offset + current.len() as u64 > expected {
            return Err(source_changed());
        }
        let next = if current.len() == CHUNK {
            fill(&mut reader)?
        } else {
            Vec::new()
        };
        scan_chunk(previous, &current, &next)?;
        let sha = hash(&current);
        whole.update(&current);
        each(&sha, &current, offset)?;
        chunks.push(Chunk {
            sha256: sha,
            size: current.len() as u64,
        });
        offset += current.len() as u64;
        previous = current.last().copied();
        current = next;
    }
    if offset != expected {
        return Err(source_changed());
    }
    Ok(FileDigest {
        sha256: format!("{:x}", whole.finalize()),
        size: offset,
        chunks,
    })
}
fn check_regular(root: &Path, path: &Path) -> Result<()> {
    for ancestor in path.ancestors().take_while(|p| *p != root) {
        let meta = fs::symlink_metadata(ancestor)?;
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & 0x400 != 0 {
                return Err(AppError::new(5, "Reparse points are refused"));
            }
        }
        if meta.file_type().is_symlink() {
            return Err(AppError::new(5, "Symbolic links are refused"));
        }
    }
    if !path.canonicalize()?.starts_with(root) || !path.is_file() {
        return Err(AppError::new(
            5,
            "Source path leaves project or is not a regular file",
        ));
    }
    Ok(())
}
fn walk(root: &Path, dir: &Path, paths: &mut BTreeSet<String>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap()
            .to_str()
            .ok_or_else(|| AppError::new(5, "Non-UTF8 paths are unsupported"))?
            .replace('\\', "/");
        if !path_policy(&rel)? {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(AppError::new(5, "Symbolic links are refused"));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if entry.metadata()?.file_attributes() & 0x400 != 0 {
                return Err(AppError::new(5, "Reparse points are refused"));
            }
        }
        if kind.is_dir() {
            walk(root, &path, paths)?
        } else if kind.is_file() {
            paths.insert(rel);
        } else {
            return Err(AppError::new(5, "Special files are refused"));
        }
    }
    Ok(())
}
pub fn collect(path: &Path) -> Result<Inventory> {
    collect_with(path, Some(&state::root()?.join("cas")), Limits::default())
}
/// Validate and hash source without creating state or transfer objects.
pub fn collect_readonly(path: &Path) -> Result<Inventory> {
    collect_with(path, None, Limits::default())
}
#[cfg(test)]
fn collect_into(path: &Path, cas: &Path) -> Result<Inventory> {
    collect_with(path, Some(cas), Limits::default())
}
/// Inventory with explicit limits; with `cas`, files of at most `large_file`
/// bytes are staged there while `CAS_RESERVE` stays free.
pub fn collect_with(path: &Path, cas: Option<&Path>, limits: Limits) -> Result<Inventory> {
    collect_impl(path, cas, limits)
}
fn require_git_at_repository_boundary(root: &Path, git_top: Option<&Path>) -> Result<()> {
    if git_top.is_none() && root.ancestors().any(|path| path.join(".git").exists()) {
        return Err(AppError::new(
            5,
            "Git inventory is required at a repository boundary; install Git or resolve its repository error",
        ));
    }
    Ok(())
}
fn collect_impl(path: &Path, cas: Option<&Path>, limits: Limits) -> Result<Inventory> {
    let root = path.canonicalize()?;
    if !root.is_dir() {
        return Err(AppError::new(2, "Project must be a directory"));
    }
    let git = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "--show-toplevel"])
        .output();
    let git_top = git
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|p| PathBuf::from(p.trim()).canonicalize().ok());
    require_git_at_repository_boundary(&root, git_top.as_deref())?;
    // An explicitly selected plain directory can live inside a parent's ignored
    // scratch folder. Parent Git rules must not silently turn that source into
    // an empty manifest. A real repository root still honors all its ignores.
    let ignored_plain_child = git_top.as_ref().is_some_and(|top| top != &root)
        && Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["check-ignore", "-q", "."])
            .status()
            .is_ok_and(|s| s.success());
    let mut paths = BTreeSet::new();
    let mut executable = BTreeSet::new();
    if git_top.is_some() && !ignored_plain_child {
        let output = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args([
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "-z",
            ])
            .output()?;
        if !output.status.success() {
            return Err(AppError::new(5, "Git inventory failed"));
        }
        for part in output.stdout.split(|b| *b == 0).filter(|s| !s.is_empty()) {
            let rel = std::str::from_utf8(part)
                .map_err(|_| AppError::new(5, "Non-UTF8 Git path refused"))?
                .to_string();
            if path_policy(&rel)? {
                paths.insert(rel);
            }
        }
        let modes = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["ls-files", "--stage", "-z"])
            .output()?;
        for part in modes.stdout.split(|b| *b == 0) {
            if part.starts_with(b"100755 ") {
                if let Some(i) = part.iter().position(|b| *b == b'\t') {
                    if let Ok(s) = std::str::from_utf8(&part[i + 1..]) {
                        executable.insert(s.to_string());
                    }
                }
            }
        }
    } else {
        walk(&root, &root, &mut paths)?;
    }
    if paths.len() > limits.max_files {
        return Err(AppError::new(
            5,
            format!("Project exceeds {} files", limits.max_files),
        ));
    }
    // Staging stops while less than CAS_RESERVE plus the file would stay free.
    let mut budget = match cas {
        Some(cas) => {
            fs::create_dir_all(cas)?;
            (limits.available_space)(cas).unwrap_or(0)
        }
        None => 0,
    };
    let mut manifest = Vec::new();
    let mut sources = BTreeMap::new();
    let (mut total, mut small) = (0u64, 0u64);
    for rel in paths {
        let path = root.join(&rel);
        if !path.exists() {
            continue;
        }
        check_regular(&root, &path)?;
        let file = fs::File::open(&path)?;
        let length = file.metadata()?.len();
        let is_small = length <= limits.large_file;
        if length > limits.max_file
            || total + length > limits.max_tree
            || (is_small && small + length > limits.max_small_tree)
        {
            return Err(limits.refusal());
        }
        let stage = cas.filter(|_| is_small && budget >= length.saturating_add(CAS_RESERVE));
        if stage.is_some() {
            budget -= length;
        }
        let digest = digest_stream(&file, length, |sha, bytes, offset| {
            sources.entry(sha.to_string()).or_insert_with(|| Source {
                path: path.clone(),
                offset,
                len: bytes.len() as u64,
            });
            if let Some(cas) = stage {
                let target = cas.join(sha);
                if !target.exists() {
                    state::atomic_bytes(&target, bytes)?
                }
            }
            Ok(())
        })?;
        total += digest.size;
        if is_small {
            small += digest.size;
        }
        #[allow(unused_mut)]
        let mut mode = if executable.contains(&rel) {
            0o755
        } else {
            0o644
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if file.metadata()?.permissions().mode() & 0o111 != 0 {
                mode = 0o755
            }
        }
        manifest.push(Entry {
            path: rel,
            size: digest.size,
            sha256: digest.sha256,
            mode,
            chunks: digest.chunks,
        });
    }
    let canonical = serde_json::to_vec(&serde_json::to_value(&manifest)?)?;
    Ok(Inventory {
        project: hash(root.to_string_lossy().as_bytes()),
        root,
        manifest_id: hash(&canonical),
        manifest,
        bytes: total,
        small_bytes: small,
        sources,
        cas: cas.map(Path::to_path_buf),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_fail_closed() {
        for p in [
            "../x",
            "C:/x",
            "a\\b",
            ".ssh/config",
            ".env",
            "foo/private_key.json",
            "id_ed25519",
        ] {
            assert!(path_policy(p).is_err(), "{p}")
        }
        assert_eq!(path_policy("target/x").unwrap(), false);
        assert!(path_policy("src/secrets.rs").unwrap());
        assert!(path_policy(".env.example").unwrap())
    }
    #[test]
    fn content_scan_rejects_real_shapes() {
        let value = ["-----BEGIN ", "OPENSSH PRIVATE KEY-----"].concat();
        assert!(scan(value.as_bytes()).is_err());
        assert!(scan(b"pub const SECRET_NAME: &str = \"example\";").is_ok());
    }
    #[test]
    fn known_hash() {
        assert_eq!(
            hash(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        )
    }
    #[test]
    fn chunks_reconstruct_snapshot_and_manifest_is_stable() {
        let root = tempfile::tempdir().unwrap();
        let cas = tempfile::tempdir().unwrap();
        let bytes = vec![b'x'; CHUNK + 17];
        fs::write(root.path().join("large.rs"), &bytes).unwrap();
        fs::write(root.path().join("empty"), b"").unwrap();
        fs::create_dir(root.path().join("target")).unwrap();
        fs::write(root.path().join("target/ignored"), b"build output").unwrap();
        let first = collect_into(root.path(), cas.path()).unwrap();
        assert_eq!(first.manifest.len(), 2);
        assert_eq!(first.manifest[1].chunks.len(), 2);
        let assembled: Vec<_> = first.manifest[1]
            .chunks
            .iter()
            .flat_map(|c| fs::read(cas.path().join(&c.sha256)).unwrap())
            .collect();
        assert_eq!(assembled, bytes);
        let second = collect_into(root.path(), cas.path()).unwrap();
        assert_eq!(first.manifest_id, second.manifest_id);
        fs::write(root.path().join("large.rs"), b"changed").unwrap();
        assert_ne!(
            first.manifest_id,
            collect_into(root.path(), cas.path()).unwrap().manifest_id
        )
    }
    #[test]
    fn readonly_inventory_matches_upload_manifest_without_creating_objects() {
        let root = tempfile::tempdir().unwrap();
        let cas = tempfile::tempdir().unwrap();
        fs::write(root.path().join("source.py"), b"print('hello')\n").unwrap();
        let preview = collect_readonly(root.path()).unwrap();
        assert_eq!(fs::read_dir(cas.path()).unwrap().count(), 0);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
        let upload = collect_into(root.path(), cas.path()).unwrap();
        assert_eq!(preview.manifest_id, upload.manifest_id);
        assert_eq!(fs::read_dir(cas.path()).unwrap().count(), 1);
    }
    #[test]
    fn failed_git_cannot_bypass_repository_ignores() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".git")).unwrap();
        let child = root.path().join("child");
        fs::create_dir(&child).unwrap();
        assert_eq!(
            require_git_at_repository_boundary(&child, None)
                .unwrap_err()
                .code,
            5
        );
        assert!(require_git_at_repository_boundary(&child, Some(root.path())).is_ok());
    }
    #[test]
    fn rejected_tree_never_becomes_a_manifest() {
        let root = tempfile::tempdir().unwrap();
        let cas = tempfile::tempdir().unwrap();
        fs::write(root.path().join(".env"), b"VALUE=secret").unwrap();
        assert_eq!(collect_into(root.path(), cas.path()).unwrap_err().code, 5)
    }
    #[test]
    fn git_includes_untracked_but_respects_ignore() {
        let root = tempfile::tempdir().unwrap();
        let cas = tempfile::tempdir().unwrap();
        if !Command::new("git")
            .args(["init", "--quiet"])
            .arg(root.path())
            .status()
            .is_ok_and(|s| s.success())
        {
            return;
        }
        fs::write(root.path().join(".gitignore"), b"ignored.txt\n").unwrap();
        fs::write(root.path().join("ignored.txt"), b"ignore").unwrap();
        fs::write(root.path().join("new.rs"), b"new").unwrap();
        let result = collect_into(root.path(), cas.path()).unwrap();
        assert!(result.manifest.iter().any(|e| e.path == "new.rs"));
        assert!(!result.manifest.iter().any(|e| e.path == "ignored.txt"));
    }
    #[test]
    fn explicit_plain_directory_inside_parent_ignore_is_not_empty() {
        let parent = tempfile::tempdir().unwrap();
        let cas = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .arg(parent.path())
                .status()
                .unwrap()
                .success()
        );
        fs::write(parent.path().join(".gitignore"), b"scratch/\n").unwrap();
        let root = parent.path().join("scratch/project");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("main.rs"), b"fn main() {}\n").unwrap();
        let result = collect_into(&root, cas.path()).unwrap();
        assert_eq!(result.manifest.len(), 1);
        assert_eq!(result.manifest[0].path, "main.rs");
        let repo = collect_into(parent.path(), cas.path()).unwrap();
        assert!(!repo.manifest.iter().any(|e| e.path.starts_with("scratch/")));
    }
    /// Deterministic content with no credential shapes.
    fn content(len: usize, seed: u32) -> Vec<u8> {
        let mut state = seed.wrapping_mul(2654435761).wrapping_add(1);
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1103515245).wrapping_add(12345);
                b"abcdefghijklmnopqrstuvwxyz \n"[(state >> 16) as usize % 28]
            })
            .collect()
    }
    /// Returns at most a varying handful of bytes per read.
    struct ShortReads<'a> {
        data: &'a [u8],
        step: usize,
    }
    impl Read for ShortReads<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.step = (self.step * 31 + 7) % 8191 + 1;
            let count = self.step.min(buffer.len()).min(self.data.len());
            buffer[..count].copy_from_slice(&self.data[..count]);
            self.data = &self.data[count..];
            Ok(count)
        }
    }
    fn reference(path: &str, bytes: &[u8]) -> Entry {
        Entry {
            path: path.into(),
            size: bytes.len() as u64,
            sha256: hash(bytes),
            mode: 0o644,
            chunks: bytes
                .chunks(CHUNK)
                .map(|c| Chunk {
                    sha256: hash(c),
                    size: c.len() as u64,
                })
                .collect(),
        }
    }
    const SIZES: [usize; 6] = [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 3 * CHUNK + 7];
    #[test]
    fn streaming_digest_matches_the_whole_buffer_reference_with_short_reads() {
        for (index, size) in SIZES.into_iter().enumerate() {
            let bytes = content(size, index as u32);
            let expected = reference("f", &bytes);
            let mut offsets = Vec::new();
            let plain = digest_stream(&bytes[..], size as u64, |sha, chunk, offset| {
                assert_eq!(hash(chunk), sha);
                offsets.push(offset);
                Ok(())
            })
            .unwrap();
            let short = digest_stream(
                ShortReads {
                    data: &bytes,
                    step: index,
                },
                size as u64,
                |_, _, _| Ok(()),
            )
            .unwrap();
            for result in [&plain, &short] {
                assert_eq!(result.sha256, expected.sha256, "{size}");
                assert_eq!(result.size, size as u64);
                let chunks: Vec<_> = result
                    .chunks
                    .iter()
                    .map(|c| (c.sha256.clone(), c.size))
                    .collect();
                let wanted: Vec<_> = expected
                    .chunks
                    .iter()
                    .map(|c| (c.sha256.clone(), c.size))
                    .collect();
                assert_eq!(chunks, wanted, "{size}");
            }
            assert_eq!(
                offsets,
                (0..size.div_ceil(CHUNK))
                    .map(|i| (i * CHUNK) as u64)
                    .collect::<Vec<_>>()
            );
        }
    }
    #[test]
    fn streamed_manifest_id_matches_the_whole_buffer_reference() {
        let root = tempfile::tempdir().unwrap();
        let cas = tempfile::tempdir().unwrap();
        let mut entries = Vec::new();
        for (index, size) in SIZES.into_iter().enumerate() {
            let name = format!("file{index}.txt");
            let bytes = content(size, 100 + index as u32);
            fs::write(root.path().join(&name), &bytes).unwrap();
            entries.push(reference(&name, &bytes));
        }
        let canonical = serde_json::to_vec(&serde_json::to_value(&entries).unwrap()).unwrap();
        let inv = collect_into(root.path(), cas.path()).unwrap();
        assert_eq!(inv.manifest_id, hash(&canonical));
        assert_eq!(inv.bytes, SIZES.iter().map(|s| *s as u64).sum::<u64>());
        assert_eq!(inv.small_bytes, inv.bytes);
        assert_eq!(entries[0].chunks.len(), 0, "an empty file has no chunks");
    }
    #[test]
    fn a_file_that_changes_length_while_read_is_refused() {
        for (data, expected) in [(&b"abcdef"[..], 5u64), (&b"abcd"[..], 5u64)] {
            let error = digest_stream(data, expected, |_, _, _| Ok(())).unwrap_err();
            assert_eq!(
                (error.code, error.message.as_str()),
                (3, "Source file changed while it was read; retry")
            );
        }
        // Growth past a whole chunk is caught by the one byte read beyond the length.
        let grown = vec![b'a'; CHUNK + 1];
        assert!(digest_stream(&grown[..], CHUNK as u64, |_, _, _| Ok(())).is_err());
    }
    fn tokens() -> Vec<Vec<u8>> {
        [
            ["-----BEGIN ", "RSA PRIVATE KEY-----"].concat(),
            ["PuTTY-User-", "Key-File-2"].concat(),
            ["AKIA", "ABCDEFGHIJKLMNOP"].concat(),
            ["ghp_", "abcdefghijklmnopqrstuvwxyz0123456789"].concat(),
            [
                "github_pat_",
                "11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyz",
            ]
            .concat(),
            ["sk-proj-", "abcdefghijklmnopqrstuvwxyz0123456789ABCD"].concat(),
            ["hf_", "abcdefghijklmnopqrstuvwxyzABCDEFGH"].concat(),
        ]
        .into_iter()
        .map(String::into_bytes)
        .collect()
    }
    /// Chunked scan of a two-chunk buffer, exactly as `digest_stream` windows it.
    fn chunked(buffer: &[u8]) -> Result<()> {
        let (first, second) = buffer.split_at(CHUNK);
        scan_chunk(None, first, second)?;
        scan_chunk(first.last().copied(), second, &[])
    }
    #[test]
    fn tokens_split_at_every_offset_around_a_chunk_boundary_are_found() {
        let mut buffer = vec![b' '; CHUNK + 256];
        assert!(chunked(&buffer).is_ok());
        for token in tokens() {
            for start in CHUNK - 80..=CHUNK + 5 {
                buffer[start..start + token.len()].copy_from_slice(&token);
                assert!(scan(&buffer).is_err(), "{start}");
                assert!(chunked(&buffer).is_err(), "{start}");
                buffer[start..start + token.len()].fill(b' ');
            }
        }
        // A match that starts in the look-ahead belongs to the next chunk only.
        buffer[CHUNK + 10..CHUNK + 30].copy_from_slice(&tokens()[2]);
        let (first, second) = buffer.split_at(CHUNK);
        assert!(scan_chunk(None, first, second).is_ok());
        assert!(scan_chunk(first.last().copied(), second, &[]).is_err());
    }
    #[test]
    fn word_boundaries_are_ascii_so_non_ascii_neighbours_do_not_hide_tokens() {
        let key = ["AKIA", "ABCDEFGHIJKLMNOP"].concat();
        for text in [
            format!("é{key}é"),
            format!("名{key}"),
            format!("\u{fffd}{key}\u{fffd}"),
        ] {
            assert!(scan(text.as_bytes()).is_err(), "{text}");
        }
        let mut invalid = vec![0xff, 0xfe];
        invalid.extend_from_slice(key.as_bytes());
        assert!(scan(&invalid).is_err());
        // An ASCII identifier character still forms a word, as before.
        assert!(scan(format!("x{key}").as_bytes()).is_ok());
        // The boundary context crosses a chunk edge: a word byte before the chunk hides the token.
        assert!(scan_chunk(Some(b'x'), key.as_bytes(), &[]).is_ok());
        assert!(scan_chunk(Some(0xa9), key.as_bytes(), &[]).is_err());
    }
    #[test]
    fn argument_scan_uses_the_same_pattern() {
        assert!(
            scan(
                ["--key=", "ghp_", "abcdefghijklmnopqrstuvwxyz0123456789"]
                    .concat()
                    .as_bytes()
            )
            .is_err()
        );
        assert!(scan(b"--message=ordinary words").is_ok());
    }
    fn limits(available: fn(&Path) -> Option<u64>) -> Limits {
        Limits {
            large_file: 64,
            available_space: available,
            ..Limits::default()
        }
    }
    #[test]
    fn only_small_files_are_staged_and_every_chunk_has_a_source() {
        let root = tempfile::tempdir().unwrap();
        let cas = tempfile::tempdir().unwrap();
        fs::write(root.path().join("small.txt"), b"small").unwrap();
        let big = content(CHUNK + 100, 7);
        fs::write(root.path().join("large.bin"), &big).unwrap();
        let inv = collect_with(root.path(), Some(cas.path()), limits(|_| Some(u64::MAX))).unwrap();
        let staged: BTreeSet<_> = fs::read_dir(cas.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(staged, BTreeSet::from([hash(b"small")]));
        assert_eq!(inv.small_bytes, 5);
        assert_eq!(inv.bytes, 5 + big.len() as u64);
        let second = hash(&big[CHUNK..]);
        assert_eq!(
            inv.sources[&second],
            Source {
                path: inv.root.join("large.bin"),
                offset: CHUNK as u64,
                len: 100
            }
        );
        assert_eq!(inv.sources.len(), 3);
        assert_eq!(inv.cas.as_deref(), Some(cas.path()));
    }
    #[test]
    fn staging_keeps_the_laptop_reserve_free() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("small.txt"), b"small").unwrap();
        let short = tempfile::tempdir().unwrap();
        collect_with(
            root.path(),
            Some(short.path()),
            limits(|_| Some(CAS_RESERVE + 4)),
        )
        .unwrap();
        assert_eq!(fs::read_dir(short.path()).unwrap().count(), 0);
        let exact = tempfile::tempdir().unwrap();
        collect_with(
            root.path(),
            Some(exact.path()),
            limits(|_| Some(CAS_RESERVE + 5)),
        )
        .unwrap();
        assert_eq!(fs::read_dir(exact.path()).unwrap().count(), 1);
        let unknown = tempfile::tempdir().unwrap();
        let inv = collect_with(root.path(), Some(unknown.path()), limits(|_| None)).unwrap();
        assert_eq!(fs::read_dir(unknown.path()).unwrap().count(), 0);
        assert_eq!(inv.sources.len(), 1);
    }
    #[test]
    fn injected_limits_refuse_before_upload() {
        assert_eq!(
            Limits::default().refusal().message,
            "Source exceeds the 16 GiB file, 32 GiB project or 8 GiB small-file limit"
        );
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a.txt"), vec![b'a'; 40]).unwrap();
        fs::write(root.path().join("b.txt"), vec![b'b'; 40]).unwrap();
        let base = Limits {
            available_space: |_| None,
            ..Limits::default()
        };
        for limits in [
            Limits {
                max_file: 39,
                ..base
            },
            Limits {
                max_tree: 79,
                ..base
            },
            Limits {
                max_small_tree: 79,
                ..base
            },
            Limits {
                max_files: 1,
                ..base
            },
        ] {
            assert_eq!(collect_with(root.path(), None, limits).unwrap_err().code, 5);
        }
        // Large files do not count toward the small-file limit.
        let large = Limits {
            max_small_tree: 40,
            large_file: 39,
            ..base
        };
        let inv = collect_with(root.path(), None, large).unwrap();
        assert_eq!((inv.bytes, inv.small_bytes), (80, 0));
        assert!(
            collect_with(
                root.path(),
                None,
                Limits {
                    max_file: 39,
                    ..base
                }
            )
            .unwrap_err()
            .message
            .contains("39 bytes file")
        );
    }
}
