use crate::{AppError, Result, state};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};
pub const CHUNK: usize = 1024 * 1024;
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
#[derive(Debug)]
pub struct Inventory {
    pub root: PathBuf,
    pub project: String,
    pub manifest_id: String,
    pub manifest: Vec<Entry>,
    pub bytes: u64,
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
pub fn scan(bytes: &[u8]) -> Result<()> {
    let text = String::from_utf8_lossy(bytes);
    // Patterns require credential-shaped values, not source identifiers alone.
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let regex=PATTERN.get_or_init(||Regex::new(r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----|PuTTY-User-Key-File-[0-9]|\b(?:AKIA|ASIA)[A-Z0-9]{16}\b|\bgh[pousr]_[A-Za-z0-9]{30,}\b|\bgithub_pat_[A-Za-z0-9_]{40,}\b|\bsk-(?:proj-)?[A-Za-z0-9_-]{32,}\b").unwrap());
    if regex.is_match(&text) {
        return Err(AppError::new(
            5,
            "Recognizable credential content refused (contents withheld)",
        ));
    }
    Ok(())
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
    collect_into(path, &state::root()?.join("cas"))
}
/// Validate and hash source without creating state or transfer objects.
pub fn collect_readonly(path: &Path) -> Result<Inventory> {
    collect_impl(path, None)
}
fn collect_into(path: &Path, cas: &Path) -> Result<Inventory> {
    collect_impl(path, Some(cas))
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
fn collect_impl(path: &Path, cas: Option<&Path>) -> Result<Inventory> {
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
    if paths.len() > 100_000 {
        return Err(AppError::new(5, "Project exceeds 100000 files"));
    }
    let mut manifest = Vec::new();
    let mut total = 0u64;
    for rel in paths {
        let path = root.join(&rel);
        if !path.exists() {
            continue;
        }
        check_regular(&root, &path)?;
        let length = fs::metadata(&path)?.len();
        if length > 512 * CHUNK as u64 || total + length > 8 * 1024 * 1024 * 1024 {
            return Err(AppError::new(
                5,
                "Source exceeds the 512MiB file or 8GiB project limit",
            ));
        }
        let bytes = fs::read(&path)?;
        total += bytes.len() as u64;
        if bytes.len() > 512 * CHUNK || total > 8 * 1024 * 1024 * 1024 {
            return Err(AppError::new(
                5,
                "Source exceeds the 512MiB file or 8GiB project limit",
            ));
        }
        scan(&bytes)?;
        let mut chunks = Vec::new();
        for chunk in bytes.chunks(CHUNK) {
            let sha = hash(chunk);
            if let Some(cas) = cas {
                let target = cas.join(&sha);
                if !target.exists() {
                    state::atomic_bytes(&target, chunk)?
                }
            }
            chunks.push(Chunk {
                sha256: sha,
                size: chunk.len() as u64,
            });
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
            if fs::metadata(&path)?.permissions().mode() & 0o111 != 0 {
                mode = 0o755
            }
        }
        manifest.push(Entry {
            path: rel,
            size: bytes.len() as u64,
            sha256: hash(&bytes),
            mode,
            chunks,
        });
    }
    let canonical = serde_json::to_vec(&serde_json::to_value(&manifest)?)?;
    Ok(Inventory {
        project: hash(root.to_string_lossy().as_bytes()),
        root,
        manifest_id: hash(&canonical),
        manifest,
        bytes: total,
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
}
