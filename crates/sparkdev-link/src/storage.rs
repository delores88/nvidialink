use anyhow::{Context, Result};
use iroh::SecretKey;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

pub fn client_dir() -> Result<PathBuf> {
    let root = if let Some(v) = std::env::var_os("SPARKDEV_HOME") {
        PathBuf::from(v)
    } else if cfg!(windows) {
        windows_local_data()?.join("SparkDevBridge")
    } else {
        PathBuf::from(std::env::var_os("HOME").context("HOME unavailable")?)
            .join(".local/state/sparkdev")
    };
    Ok(root.join("link"))
}

#[cfg(windows)]
fn windows_local_data() -> Result<PathBuf> {
    use windows_sys::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath},
    };
    let mut ptr = std::ptr::null_mut();
    let code = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_LocalAppData,
            0x10000,
            std::ptr::null_mut(),
            &mut ptr,
        )
    };
    if code >= 0 && !ptr.is_null() {
        let mut len = 0;
        unsafe {
            while *ptr.add(len) != 0 && len < 32768 {
                len += 1;
            }
        }
        let text = unsafe { String::from_utf16(std::slice::from_raw_parts(ptr, len)) };
        unsafe {
            CoTaskMemFree(ptr.cast());
        }
        let path = PathBuf::from(text.context("invalid Windows local-data path")?);
        anyhow::ensure!(
            path.is_absolute(),
            "Windows local-data path is not absolute"
        );
        return Ok(path);
    }
    let home = PathBuf::from(std::env::var_os("USERPROFILE").context("USERPROFILE unavailable")?);
    anyhow::ensure!(
        home.is_absolute() && home.is_dir(),
        "invalid Windows user profile"
    );
    Ok(home.join("AppData/Local"))
}
#[cfg(not(windows))]
fn windows_local_data() -> Result<PathBuf> {
    anyhow::bail!("Windows local-data path unavailable")
}

/// Iroh relays route one active connection per endpoint key. Serialize the brief
/// subprocess endpoints; remote jobs themselves stay concurrent in Docker.
#[derive(Debug)]
pub struct LinkBusy;
impl std::fmt::Display for LinkBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Another Spark connection is active; retry this idempotent request")
    }
}
impl std::error::Error for LinkBusy {}

pub async fn endpoint_lease(dir: &Path) -> Result<File> {
    private_dir(dir)?;
    let path = dir.join("endpoint.lock");
    if path.exists() {
        regular(&path)?;
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    let start = std::time::Instant::now();
    loop {
        if file.try_lock().is_ok() {
            return Ok(file);
        }
        if start.elapsed() >= std::time::Duration::from_secs(5) {
            return Err(LinkBusy.into());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}
pub fn server_dir() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var_os("HOME").context("HOME unavailable")?).join("sparkdev/link"))
}

pub fn private_dir(path: &Path) -> Result<()> {
    anyhow::ensure!(path.is_absolute(), "state path must be absolute");
    // Refuse a redirected ancestor as well as a redirected leaf.
    for ancestor in path.ancestors() {
        if let Ok(m) = fs::symlink_metadata(ancestor) {
            anyhow::ensure!(!m.file_type().is_symlink(), "state path contains a symlink");
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                anyhow::ensure!(
                    m.file_attributes() & 0x400 == 0,
                    "state path contains a reparse point"
                );
            }
        }
    }
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let m = fs::symlink_metadata(path)?;
        anyhow::ensure!(
            m.uid() == unsafe { libc::geteuid() },
            "state directory belongs to another user"
        );
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn regular(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    anyhow::ensure!(
        m.is_file() && !m.file_type().is_symlink(),
        "state item is not a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
            "state file permissions are unsafe"
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        anyhow::ensure!(m.file_attributes() & 0x400 == 0, "state file is redirected");
    }
    Ok(())
}

pub fn lock(dir: &Path, name: &str) -> Result<File> {
    private_dir(dir)?;
    let path = dir.join(name);
    if path.exists() {
        regular(&path)?;
    }
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let f = opts.open(path)?;
    f.lock().context("cannot lock identity state")?;
    Ok(f)
}

pub fn save<T: Serialize>(dir: &Path, name: &str, data: &T) -> Result<()> {
    private_dir(dir)?;
    let target = dir.join(name);
    if target.exists() {
        regular(&target)?;
    }
    let random = spark_link_proto::random_bytes::<16>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let temporary = dir.join(format!(".{name}.{random}.tmp"));
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut f = opts.open(&temporary)?;
    serde_json::to_writer(&mut f, data)?;
    f.flush()?;
    f.sync_all()?;
    fs::rename(temporary, target)?;
    Ok(())
}
pub fn read<T: DeserializeOwned>(dir: &Path, name: &str) -> Result<T> {
    let path = dir.join(name);
    regular(&path)?;
    let data = fs::read(path)?;
    anyhow::ensure!(data.len() <= 1024 * 1024, "state file exceeds limit");
    serde_json::from_slice(&data).context("invalid saved state")
}

pub fn identity(dir: &Path) -> Result<SecretKey> {
    let _guard = lock(dir, "identity.lock")?;
    let path = dir.join("identity.key");
    if !path.exists() {
        let key = Zeroizing::new(spark_link_proto::random_bytes::<32>());
        let wrapped = protect(key.as_ref())?;
        save(dir, "identity.key", &wrapped)?;
    }
    let wrapped: Vec<u8> = read(dir, "identity.key")?;
    let clear = unprotect(&wrapped)?;
    let bytes: [u8; 32] = clear
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid identity length"))?;
    Ok(SecretKey::from_bytes(&Zeroizing::new(bytes)))
}

#[cfg(not(windows))]
fn protect(bytes: &[u8]) -> Result<Vec<u8>> {
    Ok(bytes.to_vec())
}
#[cfg(not(windows))]
fn unprotect(bytes: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    Ok(Zeroizing::new(bytes.to_vec()))
}

#[cfg(windows)]
fn machine_entropy() -> Result<[u8; 32]> {
    use sha2::{Digest, Sha256};
    use windows_sys::Win32::System::Registry::*;
    let key: Vec<u16> = "SOFTWARE\\Microsoft\\Cryptography\0"
        .encode_utf16()
        .collect();
    let name: Vec<u16> = "MachineGuid\0".encode_utf16().collect();
    let mut buf = vec![0u16; 128];
    let mut length = (buf.len() * 2) as u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut length,
        )
    };
    anyhow::ensure!(result == 0, "Windows machine identity unavailable");
    let len = (length as usize / 2).saturating_sub(1).min(buf.len());
    let guid = String::from_utf16(&buf[..len]).context("invalid Windows machine identity")?;
    Ok(Sha256::digest(guid.trim().to_ascii_lowercase().as_bytes()).into())
}

#[cfg(windows)]
fn protect(bytes: &[u8]) -> Result<Vec<u8>> {
    use windows_sys::Win32::{Foundation::LocalFree, Security::Cryptography::*};
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr().cast_mut(),
    };
    let entropy = machine_entropy()?;
    let entropy_blob = CRYPT_INTEGER_BLOB {
        cbData: 32,
        pbData: entropy.as_ptr().cast_mut(),
    };
    let mut out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // User-scoped DPAPI; no roaming credential APIs and no UI fallback.
    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            &entropy_blob,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    anyhow::ensure!(ok != 0, "Windows could not protect the device identity");
    let result = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec() };
    unsafe {
        LocalFree(out.pbData.cast());
    }
    Ok(result)
}
#[cfg(windows)]
fn unprotect(bytes: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    use windows_sys::Win32::{Foundation::LocalFree, Security::Cryptography::*};
    use zeroize::Zeroize;
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr().cast_mut(),
    };
    let entropy = machine_entropy()?;
    let entropy_blob = CRYPT_INTEGER_BLOB {
        cbData: 32,
        pbData: entropy.as_ptr().cast_mut(),
    };
    let mut out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            &entropy_blob,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    anyhow::ensure!(
        ok != 0,
        "Windows could not unlock this device identity; restore the original Windows account or pair a new identity"
    );
    let slice = unsafe { std::slice::from_raw_parts_mut(out.pbData, out.cbData as usize) };
    let result = Zeroizing::new(slice.to_vec());
    slice.zeroize();
    unsafe {
        LocalFree(out.pbData.cast());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identity_survives_restart() {
        let d = tempfile::tempdir().unwrap();
        let a = identity(d.path()).unwrap().public();
        let b = identity(d.path()).unwrap().public();
        assert_eq!(a, b);
        #[cfg(windows)]
        {
            let bytes = fs::read(d.path().join("identity.key")).unwrap();
            assert!(!bytes.windows(32).any(|w| w == a.as_bytes()));
        }
    }

    #[tokio::test]
    async fn endpoint_lease_serializes_process_connections() {
        let dir = tempfile::tempdir().unwrap();
        let first = endpoint_lease(dir.path()).await.unwrap();
        let path = dir.path().to_path_buf();
        let second = tokio::spawn(async move { endpoint_lease(&path).await });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(!second.is_finished());
        drop(first);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), second)
                .await
                .unwrap()
                .unwrap()
                .is_ok()
        );
    }
    #[cfg(windows)]
    #[test]
    fn local_data_is_not_package_redirected() {
        let path = windows_local_data().unwrap();
        assert!(path.is_absolute());
        assert!(
            !path
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains("localcache\\local")
        );
    }
    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_state() {
        let d = tempfile::tempdir().unwrap();
        let link = d.path().join("link");
        std::os::unix::fs::symlink(d.path(), &link).unwrap();
        assert!(identity(&link).is_err());
    }
}
