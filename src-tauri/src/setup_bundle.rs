//! Installed raw files remain inspectable; base64 exists only in the SSH request.
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fs, io::Read, path::Path};

const MAX_MANIFEST: u64 = 64 * 1024;
const MAX_TOTAL: u64 = 192 * 1024 * 1024;
const ALLOWED: [&str; 15] = [
    "controller.py",
    "worker.py",
    "proxy.py",
    "prepare.py",
    "model_cache.py",
    "model_server.py",
    "install-runtime.py",
    "install-cuda.py",
    "bootstrap.py",
    "runtime/Dockerfile",
    "runtime/Dockerfile.cuda",
    "runtime/requirements.txt",
    "sparkdev-controller.service",
    "sparkdev-link.service",
    "bin/sparkdev-link",
];
const REQUIRED: [&str; 11] = [
    "controller.py",
    "worker.py",
    "proxy.py",
    "prepare.py",
    "model_cache.py",
    "model_server.py",
    "install-runtime.py",
    "bootstrap.py",
    "runtime/Dockerfile",
    "runtime/requirements.txt",
    "sparkdev-controller.service",
];
const INVALID: &str =
    "The installed Spark setup bundle is invalid or incomplete. Reinstall DGXBridge.";
type Result<T> = std::result::Result<T, String>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    v: u32,
    build_runtime: bool,
    files: Vec<Entry>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: String,
    size: u64,
    sha256: String,
}

fn plain(path: &Path) -> Result<fs::Metadata> {
    if !path.is_absolute() {
        return Err(INVALID.into());
    }
    let mut target = None;
    for (index, part) in path.ancestors().enumerate() {
        let meta = fs::symlink_metadata(part).map_err(|_| INVALID)?;
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & 0x400 != 0 {
                return Err(INVALID.into());
            }
        }
        if meta.file_type().is_symlink() || (index > 0 && !meta.is_dir()) {
            return Err(INVALID.into());
        }
        if index == 0 {
            target = Some(meta);
        }
    }
    target.ok_or_else(|| INVALID.into())
}

fn read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let meta = plain(path)?;
    if !meta.is_file() || meta.len() > limit {
        return Err(INVALID.into());
    }
    let file = fs::File::open(path).map_err(|_| INVALID)?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| INVALID)?;
    if bytes.len() as u64 > limit {
        return Err(INVALID.into());
    }
    Ok(bytes)
}

/// Validate every installed file before returning the original v1 SSH envelope.
/// Callers must execute bootstrap.py decoded from this validated envelope.
pub fn load(resources: &Path) -> Result<Value> {
    let manifest: Manifest = serde_json::from_slice(&read(
        &resources.join("sparkdev-bundle.json"),
        MAX_MANIFEST,
    )?)
    .map_err(|_| INVALID)?;
    if manifest.v != 2 || manifest.files.len() > ALLOWED.len() {
        return Err(INVALID.into());
    }
    let mut names = BTreeSet::new();
    let mut total = 0u64;
    for entry in &manifest.files {
        let limit = if entry.path == "bin/sparkdev-link" {
            128 * 1024 * 1024
        } else {
            1024 * 1024
        };
        if !ALLOWED.contains(&entry.path.as_str())
            || !names.insert(entry.path.as_str())
            || entry.size > limit
            || entry.sha256.len() != 64
            || !entry
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(INVALID.into());
        }
        total = total.checked_add(entry.size).ok_or(INVALID)?;
    }
    if total > MAX_TOTAL
        || REQUIRED.iter().any(|name| !names.contains(name))
        || names.contains("bin/sparkdev-link") != names.contains("sparkdev-link.service")
        || names.contains("install-cuda.py") != names.contains("runtime/Dockerfile.cuda")
    {
        return Err(INVALID.into());
    }
    let raw_root = resources.join("sparkdev-remote");
    if !plain(&raw_root)?.is_dir() {
        return Err(INVALID.into());
    }
    let mut files = serde_json::Map::new();
    for entry in manifest.files {
        let bytes = read(&raw_root.join(&entry.path), entry.size)?;
        if bytes.len() as u64 != entry.size
            || format!("{:x}", Sha256::digest(&bytes)) != entry.sha256
        {
            return Err(INVALID.into());
        }
        files.insert(
            entry.path,
            json!({"sha256":entry.sha256,"data":STANDARD.encode(bytes)}),
        );
    }
    Ok(json!({"v":1,"build_runtime":manifest.build_runtime,"files":files}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Value) {
        let dir = tempfile::tempdir().unwrap();
        let mut entries = Vec::new();
        for name in ALLOWED {
            let bytes = format!("fixture:{name}").into_bytes();
            let path = dir.path().join("sparkdev-remote").join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, &bytes).unwrap();
            entries.push(json!({"path":name,"size":bytes.len(),"sha256":format!("{:x}",Sha256::digest(&bytes))}));
        }
        let manifest = json!({"v":2,"build_runtime":true,"files":entries});
        save(dir.path(), &manifest);
        (dir, manifest)
    }
    fn save(root: &Path, value: &Value) {
        fs::write(
            root.join("sparkdev-bundle.json"),
            serde_json::to_vec(value).unwrap(),
        )
        .unwrap();
    }
    #[test]
    fn raw_manifest_yields_byte_identical_v1_wire_files_and_verified_bootstrap() {
        let (dir, _) = fixture();
        let wire = load(dir.path()).unwrap();
        assert_eq!(wire["v"], 1);
        assert_eq!(wire["files"].as_object().unwrap().len(), 15);
        for name in ALLOWED {
            assert_eq!(
                STANDARD
                    .decode(wire["files"][name]["data"].as_str().unwrap())
                    .unwrap(),
                fs::read(dir.path().join("sparkdev-remote").join(name)).unwrap()
            );
        }
    }
    #[test]
    fn rejects_unknown_duplicate_missing_oversize_and_traversal_manifest_entries() {
        for case in 0..7 {
            let (dir, mut value) = fixture();
            match case {
                0 => value["files"][0]["path"] = json!("../controller.py"),
                1 => value["files"][0]["path"] = json!("C:\\controller.py"),
                2 => value["files"][0]["path"] = value["files"][1]["path"].clone(),
                3 => value["files"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|e| e["path"] != "bootstrap.py"),
                4 => value["files"][0]["size"] = json!(1024 * 1024 + 1),
                5 => {
                    value["unrecognized"] = json!(true);
                }
                _ => value["files"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|e| e["path"] != "sparkdev-link.service"),
            }
            save(dir.path(), &value);
            assert!(load(dir.path()).is_err(), "case {case}");
        }
    }
    #[test]
    fn rejects_changed_missing_nonregular_files_and_oversize_manifest() {
        let (dir, _) = fixture();
        let path = dir.path().join("sparkdev-remote/bootstrap.py");
        let original = fs::read(&path).unwrap();
        let mut changed = original.clone();
        changed[0] ^= 1;
        fs::write(&path, changed).unwrap();
        assert!(load(dir.path()).is_err());
        fs::remove_file(&path).unwrap();
        assert!(load(dir.path()).is_err());
        fs::create_dir(&path).unwrap();
        assert!(load(dir.path()).is_err());
        fs::write(
            dir.path().join("sparkdev-bundle.json"),
            vec![b' '; MAX_MANIFEST as usize + 1],
        )
        .unwrap();
        assert!(load(dir.path()).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_payload_or_parent() {
        let (dir, _) = fixture();
        let root = dir.path().join("sparkdev-remote");
        fs::rename(&root, dir.path().join("outside")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("outside"), &root).unwrap();
        assert!(load(dir.path()).is_err());
    }
}
