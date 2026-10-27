//! Staged same-binary helper; only its owned installer is launched, never a shell script.
use crate::{
    update_verify::{self as verify, Artifact, Envelope, Result, Verified},
    updates,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use zeroize::Zeroizing;
const FLAG: &str = "--dgxbridge-update-helper";
const READY: &str = "DGXBRIDGE_UPDATE_READY";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    schema: u32,
    parent_pid: u32,
    executable: PathBuf,
    installer: PathBuf,
    artifact: Value,
    envelope: Envelope,
    request_id: String,
    request_expires: DateTime<Utc>,
    from_version: String,
}
// Namespace-aware PE manifest validation adapted from the pinned Marketplace nsis.rs.
fn unsafe_setup() -> String {
    "The installer requests unsupported Windows privileges or has an invalid manifest.".into()
}
fn validate_manifest(raw: &[u8]) -> Result<()> {
    use quick_xml::{NsReader, events::Event, name::ResolveResult};
    if raw.len() > 65536 {
        return Err(unsafe_setup());
    }
    let original = if raw.starts_with(&[0xff, 0xfe]) {
        if raw.len() % 2 != 0 {
            return Err(unsafe_setup());
        }
        String::from_utf16(
            &raw[2..]
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect::<Vec<_>>(),
        )
        .map_err(|_| unsafe_setup())?
    } else {
        std::str::from_utf8(raw)
            .map_err(|_| unsafe_setup())?
            .trim_start_matches('\u{feff}')
            .to_owned()
    };
    if original.contains('&') || original.contains('\0') {
        return Err(unsafe_setup());
    }
    let mut reader = NsReader::from_str(&original);
    reader.config_mut().check_comments = true;
    let mut stack: Vec<(String, String)> = Vec::new();
    let mut levels = 0;
    let mut roots = 0;
    let mut events = 0;
    loop {
        let (namespace, event) = reader.read_resolved_event().map_err(|_| unsafe_setup())?;
        events += 1;
        if events > 4096 {
            return Err(unsafe_setup());
        }
        let empty = matches!(&event, Event::Empty(_));
        match event {
            Event::Start(element) | Event::Empty(element) => {
                let ns = match namespace {
                    ResolveResult::Bound(value) => value.as_ref().to_owned(),
                    ResolveResult::Unbound => String::new(),
                    ResolveResult::Unknown(_) => return Err(unsafe_setup()),
                };
                let name = element.local_name().as_ref().to_owned();
                if stack.is_empty() {
                    roots += 1;
                    if roots != 1 || name != "assembly" || ns != "urn:schemas-microsoft-com:asm.v1"
                    {
                        return Err(unsafe_setup());
                    }
                }
                let attributes = element
                    .attributes()
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|_| unsafe_setup())?;
                if name == "requestedExecutionLevel" {
                    levels += 1;
                    let expected = ["assembly", "trustInfo", "security", "requestedPrivileges"];
                    if stack.len() != 4
                        || stack
                            .iter()
                            .zip(expected)
                            .any(|((name, _), expected)| name != expected)
                        || stack
                            .iter()
                            .skip(1)
                            .any(|(_, ns)| ns != "urn:schemas-microsoft-com:asm.v3")
                        || ns != "urn:schemas-microsoft-com:asm.v3"
                    {
                        return Err(unsafe_setup());
                    }
                    let mut level = None;
                    let mut ui_access = None;
                    for attribute in attributes {
                        match attribute.key.as_ref() {
                            "level" => level = Some(attribute.value.into_owned()),
                            "uiAccess" => ui_access = Some(attribute.value.into_owned()),
                            key if key == "xmlns" || key.starts_with("xmlns:") => {}
                            _ => return Err(unsafe_setup()),
                        }
                    }
                    if level.as_deref() != Some("asInvoker")
                        || ui_access.as_deref() != Some("false")
                    {
                        return Err(unsafe_setup());
                    }
                }
                if !empty {
                    stack.push((name, ns));
                    if stack.len() > 32 {
                        return Err(unsafe_setup());
                    }
                }
            }
            Event::End(_) => {
                if stack.pop().is_none() {
                    return Err(unsafe_setup());
                }
            }
            Event::Text(text) => {
                if stack.is_empty() && text.as_ref().bytes().any(|v| !v.is_ascii_whitespace()) {
                    return Err(unsafe_setup());
                }
            }
            Event::Comment(_) => {}
            Event::Decl(_) => {
                if roots != 0 {
                    return Err(unsafe_setup());
                }
            }
            Event::Eof => break,
            _ => return Err(unsafe_setup()),
        }
    }
    if levels != 1 || roots != 1 || !stack.is_empty() {
        return Err(unsafe_setup());
    }
    Ok(())
}

pub fn plain(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err("Update paths must be absolute.".into());
    }
    for part in path.ancestors() {
        if let Ok(meta) = fs::symlink_metadata(part) {
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if meta.file_attributes() & 0x400 != 0 {
                    return Err("Update paths cannot contain links or reparse points.".into());
                }
            }
            if meta.file_type().is_symlink() {
                return Err("Update paths cannot contain symbolic links.".into());
            }
        }
    }
    path.canonicalize()
        .map_err(|_| "An update path is missing or inaccessible.".into())
}
pub fn normal_context() -> Result<()> {
    #[cfg(windows)]
    {
        let mut length = 0u32;
        // 15700 = APPMODEL_ERROR_NO_PACKAGE. All other results fail closed.
        if unsafe { win::GetCurrentPackageFullName(&mut length, std::ptr::null_mut()) } != 15700 {
            return Err("Windows setup cannot run from a packaged host such as Codex. Close RTXBridge and open its shortcut from ordinary Windows Explorer, then retry Install update.".into());
        }
        win::normal_user()?;
        let current =
            plain(&std::env::current_exe().map_err(|_| "Application location unavailable.")?)?;
        let name = current
            .to_string_lossy()
            .to_ascii_lowercase()
            .replace('/', "\\");
        if name.contains("\\packages\\") && name.contains("\\localcache\\") {
            return Err("This copy is inside a packaged host's private installation. Run the verified RTXBridge installer from ordinary Windows Explorer first.".into());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Err("In-app installation is supported on Windows x64 only.".into())
    }
}
pub fn open_browser(uri: &str) -> Result<()> {
    let prefix = format!("{}/native-updates/", verify::ORIGIN);
    if !uri.strip_prefix(&prefix).is_some_and(updates::request_id) {
        return Err("Invalid browser approval address.".into());
    }
    #[cfg(windows)]
    {
        win::open(uri)
    }
    #[cfg(not(windows))]
    {
        let _ = uri;
        Err("Open the approval URL in your browser.".into())
    }
}
fn laptop_guard() -> Result<File> {
    let root = sparkdev::state::root().map_err(|_| "Laptop job state is unavailable.")?;
    let guard = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(root.join("laptop-admission.lock"))
        .map_err(|_| "Laptop job admission is unavailable.")?;
    guard
        .try_lock()
        .map_err(|_| "A laptop job is being admitted. Wait before updating.")?;
    let jobs = sparkdev::local::status().map_err(|_| "Laptop job status could not be verified.")?;
    if jobs
        .as_array()
        .ok_or("Laptop job state is invalid.")?
        .iter()
        .any(|job| !sparkdev::terminal(job))
    {
        return Err(
            "Wait for active laptop jobs to finish before updating. Spark jobs can keep running."
                .into(),
        );
    }
    Ok(guard)
}
fn save_receipt(value: &Value) -> Result<()> {
    sparkdev::state::atomic_json(&updates::root()?.join("last-update.json"), value)
        .map_err(|_| "Update receipt could not be saved.".into())
}
/// Public receipt projection excludes local paths, plan contents and all credentials.
pub fn last_receipt() -> Option<Value> {
    let path = updates::root().ok()?.join("last-update.json");
    if fs::metadata(&path).ok()?.len() > 16384 {
        return None;
    }
    let value: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let mut safe = json!({});
    for name in [
        "status",
        "message",
        "from_version",
        "to_version",
        "launched_version",
        "installer_exit_code",
        "finished_at",
        "failed_at",
        "relaunch_at",
        "visible_readiness",
    ] {
        if let Some(field) = value.get(name) {
            if field.is_number() || field.as_str().is_some_and(|s| s.len() <= 1000) {
                safe[name] = field.clone();
            }
        }
    }
    Some(safe)
}
fn read_plan(path: &Path) -> Result<(Plan, PathBuf, Artifact)> {
    let path = plain(path)?;
    let root = plain(&updates::root()?)?;
    let session = path.parent().ok_or("Missing update session.")?;
    if path.file_name().and_then(|v| v.to_str()) != Some("plan.json")
        || session.parent() != Some(root.as_path())
        || !session
            .file_name()
            .and_then(|v| v.to_str())
            .is_some_and(|n| n.starts_with("u-") && n.len() == 26 && verify::safe_id(n))
    {
        return Err("Unexpected update plan location.".into());
    }
    if fs::metadata(&path)
        .map_err(|_| "Update plan unavailable.")?
        .len()
        > 1_048_576
    {
        return Err("Update plan exceeded its limit.".into());
    }
    let plan: Plan =
        serde_json::from_slice(&fs::read(&path).map_err(|_| "Update plan unavailable.")?)
            .map_err(|_| "Invalid update plan.")?;
    let executable = plain(&plan.executable)?;
    let installer = plain(&plan.installer)?;
    if plan.schema != 1
        || plan.parent_pid == 0
        || !updates::request_id(&plan.request_id)
        || plan.request_expires <= Utc::now()
        || installer != session.join("RTXBridge-update.exe")
        || !executable
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case("dgxbridge.exe"))
        || executable.starts_with(&root)
        || root.starts_with(
            executable
                .parent()
                .ok_or("Invalid installation directory.")?,
        )
    {
        return Err("Update plan identity or expiry is invalid.".into());
    }
    let a = verify::artifact(&plan.artifact)?;
    let from =
        semver::Version::parse(&plan.from_version).map_err(|_| "Invalid installed version.")?;
    if semver::Version::parse(&a.version).map_err(|_| "Invalid target version.")? <= from {
        return Err("Update plan would downgrade or repeat a release.".into());
    }
    verify::verify(&plan.envelope, &a, Utc::now())?;
    Ok((plan, session.to_path_buf(), a))
}
pub async fn start(
    target: &Path,
    verified: &Verified,
    id: &str,
    proof: &str,
    expires: DateTime<Utc>,
) -> Result<()> {
    normal_context()?;
    if !updates::request_id(id) || !updates::proof_valid(proof) {
        return Err("Invalid update request.".into());
    }
    {
        let _guard = laptop_guard()?;
    }
    let target = plain(target)?;
    let root = plain(&updates::root()?)?;
    let session = target.parent().ok_or("Missing update staging.")?;
    if session.parent() != Some(root.as_path())
        || target.file_name().and_then(|v| v.to_str()) != Some("RTXBridge-update.exe")
    {
        return Err("Invalid installer staging location.".into());
    }
    let executable =
        plain(&std::env::current_exe().map_err(|_| "Application location unavailable.")?)?;
    let plan = Plan {
        schema: 1,
        parent_pid: std::process::id(),
        executable: executable.clone(),
        installer: target.clone(),
        artifact: verified.artifact.raw.clone(),
        envelope: verified.envelope.clone(),
        request_id: id.into(),
        request_expires: expires,
        from_version: env!("CARGO_PKG_VERSION").into(),
    };
    let plan_path = session.join("plan.json");
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&plan_path)
        .map_err(|_| "This update handoff was already attempted. Request new approval.")?;
    let data = serde_json::to_vec(&plan).map_err(|_| "Could not prepare update plan.")?;
    out.write_all(&data)
        .and_then(|_| out.sync_all())
        .map_err(|_| "Could not save update plan.")?;
    drop(out);
    let helper = session.join("update-helper.exe");
    let mut source = File::open(&executable).map_err(|_| "Could not stage the update helper.")?;
    let mut staged = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&helper)
        .map_err(|_| "Could not reserve the update helper.")?;
    std::io::copy(&mut source, &mut staged)
        .and_then(|_| staged.sync_all())
        .map_err(|_| "Could not stage the update helper.")?;
    drop(staged);
    drop(source);
    let original = fs::read(&executable).map_err(|_| "Could not verify update helper.")?;
    let copy = fs::read(&helper).map_err(|_| "Could not verify staged helper.")?;
    if verify::digest(&original) != verify::digest(&copy) {
        return Err("Update helper copy failed verification.".into());
    }
    let mut command = tokio::process::Command::new(&helper);
    command
        .args([std::ffi::OsStr::new(FLAG), plan_path.as_os_str()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .current_dir(session);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = command
        .spawn()
        .map_err(|_| "The update helper could not start. RTXBridge remains open.")?;
    let result = async {
        let mut input = child
            .stdin
            .take()
            .ok_or("Update helper input unavailable.")?;
        input
            .write_all(proof.as_bytes())
            .await
            .map_err(|_| "Update helper proof transfer failed.")?;
        input
            .shutdown()
            .await
            .map_err(|_| "Update helper input failed.")?;
        drop(input);
        let output = child
            .stdout
            .take()
            .ok_or("Update helper handshake unavailable.")?;
        let mut reader = BufReader::new(output);
        let mut line = Vec::new();
        // A bounded exact line is the only event that permits the GUI to exit.
        use tokio::io::AsyncReadExt;
        (&mut reader)
            .take(1025)
            .read_until(b'\n', &mut line)
            .await
            .map_err(|_| "Update helper handshake failed.")?;
        if line != format!("{READY}\n").as_bytes() && line != format!("{READY}\r\n").as_bytes() {
            return Err("Update helper did not confirm a safe handoff.");
        }
        Ok::<(), &str>(())
    };
    match tokio::time::timeout(Duration::from_secs(20), result).await {
        Ok(Ok(())) => Ok(()),
        _ => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err("Update helper could not confirm readiness. RTXBridge remains open; request a fresh update before retrying.".into())
        }
    }
}
fn hash_locked(file: &mut File, artifact: &Artifact) -> Result<()> {
    if file
        .metadata()
        .map_err(|_| "Installer metadata unavailable.")?
        .len()
        != artifact.size
    {
        return Err("Installer size changed. Nothing was installed.".into());
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|_| "Installer unavailable.")?;
    let mut hasher = Sha256::new();
    let mut bytes = [0u8; 65536];
    let mut total = 0u64;
    loop {
        let n = file
            .read(&mut bytes)
            .map_err(|_| "Installer read failed.")?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > artifact.size {
            return Err("Installer grew after download.".into());
        }
        hasher.update(&bytes[..n]);
    }
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if total != artifact.size || digest != artifact.sha256 {
        return Err("Installer hash changed. Nothing was installed.".into());
    }
    Ok(())
}
fn nsis_header(file: &mut File, verified: &Verified) -> Result<()> {
    file.seek(SeekFrom::Start(verified.header_offset))
        .map_err(|_| "Installer header unavailable.")?;
    let mut bytes = [0u8; 28];
    file.read_exact(&mut bytes)
        .map_err(|_| "Installer header is truncated.")?;
    // NSIS firstheader: flags, signature 0xdeadbeef, 'NullsoftInst', header size, total length.
    let flags = u32::from_le_bytes(bytes[..4].try_into().unwrap());
    let header = u32::from_le_bytes(bytes[20..24].try_into().unwrap());
    let length = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
    if flags & !15 != 0
        || flags & 1 != 0
        || header == 0
        || length < 28
        || u64::from(length) > verified.artifact.size - verified.header_offset
        || bytes[4..8] != 0xdeadbeefu32.to_le_bytes()
        || &bytes[8..20] != b"NullsoftInst"
    {
        return Err("The installer is not the reviewed NSIS installation package.".into());
    }
    Ok(())
}
fn nsis_destination(directory: &Path) -> Result<String> {
    let physical = plain(directory)?;
    let text = physical
        .to_str()
        .ok_or("The existing installation path is not valid Windows text.")?;
    let text = text.strip_prefix(r"\\?\").unwrap_or(text);
    let bytes = text.as_bytes();
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || bytes[2] != b'\\'
        || text.contains(['"', '\r', '\n', '\0'])
        || text.encode_utf16().count() > 240
    {
        return Err(
            "The existing installation path cannot be passed safely to Windows setup.".into(),
        );
    }
    // NSIS /D consumes the remaining raw command line. It MUST be last and unquoted.
    Ok(format!("/D={text}"))
}
pub fn run_if_requested() -> bool {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).and_then(|s| s.to_str()) != Some(FLAG) {
        return false;
    }
    let mut recover_application: Option<PathBuf> = None;
    let result = (|| -> Result<()> {
        if args.len() != 3 {
            return Err("Invalid update helper request.".into());
        }
        normal_context()?;
        let mut proof = Zeroizing::new(String::new());
        std::io::stdin()
            .take(4097)
            .read_to_string(&mut proof)
            .map_err(|_| "Update proof unavailable.")?;
        if !updates::proof_valid(&proof) {
            return Err("Update proof is invalid.".into());
        }
        let (plan, _, artifact) = read_plan(Path::new(&args[2]))?;
        let root = updates::root()?;
        let active = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(root.join("active.lock"))
            .map_err(|_| "Update lock unavailable.")?;
        active
            .try_lock()
            .map_err(|_| "Another installer handoff is active.")?;
        let _jobs = laptop_guard()?;
        #[cfg(windows)]
        let parent = win::parent(plan.parent_pid, &plan.executable)?;
        save_receipt(
            &json!({"schema":1,"status":"waiting_for_shutdown","from_version":plan.from_version,"to_version":artifact.version,"artifact_id":artifact.id,"installer_sha256":artifact.sha256,"installer_bytes":artifact.size,"executable":plan.executable,"started_at":Utc::now(),"user_state":"preserved in place","visible_readiness":"unverified"}),
        )?;
        println!("{READY}");
        std::io::stdout()
            .flush()
            .map_err(|_| "Update handshake could not be sent.")?;
        #[cfg(windows)]
        win::wait_parent(parent)?;
        recover_application = Some(plan.executable.clone());
        if plan.request_expires <= Utc::now() {
            return Err(
                "Browser approval expired before installation. Nothing was installed.".into(),
            );
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| "Update verification runtime unavailable.")?;
        let verified = runtime.block_on(updates::revalidate(
            &updates::client()?,
            &plan.request_id,
            &proof,
            &artifact,
            &root,
        ))?;
        #[cfg(windows)]
        win::mark_download(&plan.installer)?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(1);
        }
        let mut file = options
            .open(&plan.installer)
            .map_err(|_| "The verified installer is unavailable or being changed.")?;
        hash_locked(&mut file, &artifact)?;
        nsis_header(&mut file, &verified)?;
        #[cfg(windows)]
        validate_manifest(&win::manifest(&plan.installer)?)?;
        if verified.expires_at <= Utc::now() || plan.request_expires <= Utc::now() {
            return Err(
                "Approval expired while checking the installer. Nothing was installed.".into(),
            );
        }
        normal_context()?;
        save_receipt(
            &json!({"schema":1,"status":"installing","from_version":plan.from_version,"to_version":artifact.version,"artifact_id":artifact.id,"installer_sha256":artifact.sha256,"installer_bytes":artifact.size,"executable":plan.executable,"visible_readiness":"unverified"}),
        )?;
        #[cfg(windows)]
        let exit = win::install(
            &plan.installer,
            plan.executable
                .parent()
                .ok_or("The existing installation directory is unavailable.")?,
        )?;
        #[cfg(not(windows))]
        let exit = 1u32;
        drop(file);
        let state = if exit == 0 {
            "installer_finished"
        } else {
            "installer_cancelled_or_failed"
        };
        save_receipt(
            &json!({"schema":1,"status":state,"from_version":plan.from_version,"to_version":artifact.version,"artifact_id":artifact.id,"installer_sha256":artifact.sha256,"installer_bytes":artifact.size,"installer_exit_code":exit,"executable":plan.executable,"finished_at":Utc::now(),"user_state":"preserved in place","visible_readiness":"unverified"}),
        )?;
        // The relaunch is a process-start request, never a claim that the visible UI is ready.
        #[cfg(windows)]
        {
            win::reopen_path(&plan.executable)?;
        }
        Ok(())
    })();
    if let Err(message) = result {
        let _ = save_receipt(
            &json!({"schema":1,"status":"failed","message":message,"failed_at":Utc::now(),"visible_readiness":"unverified"}),
        );
        #[cfg(windows)]
        if let Some(path) = recover_application {
            let _ = win::reopen_path(&path);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(bytes: &[u8]) -> Artifact {
        Artifact {
            raw: json!({}),
            id: "fixture".into(),
            version: "0.3.0".into(),
            size: bytes.len() as u64,
            sha256: verify::digest(bytes),
        }
    }
    #[test]
    fn locked_installer_rehash_rejects_mutated_or_truncated_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("installer.exe");
        let data = b"synthetic installer bytes";
        fs::write(&path, data).unwrap();
        hash_locked(&mut File::open(&path).unwrap(), &fixture(data)).unwrap();
        fs::write(&path, b"synthetic installer BYTEs").unwrap();
        assert!(hash_locked(&mut File::open(&path).unwrap(), &fixture(data)).is_err());
        fs::write(&path, b"short").unwrap();
        assert!(hash_locked(&mut File::open(&path).unwrap(), &fixture(data)).is_err());
    }
    #[test]
    fn approval_browser_url_has_exact_origin_and_request_id() {
        for url in [
            "http://deloresai.com/native-updates/abcdefghijklmnopqrstuvwx",
            "https://evil.test/native-updates/abcdefghijklmnopqrstuvwx",
            "https://deloresai.com/native-updates/short",
            "https://deloresai.com/native-updates/abcdefghijklmnopqrstuvwx?proof=bad",
        ] {
            assert!(open_browser(url).is_err());
        }
    }
    #[test]
    fn relative_or_missing_staging_paths_are_rejected() {
        assert!(plain(Path::new("relative/plan.json")).is_err());
        let dir = tempfile::tempdir().unwrap();
        assert!(plain(&dir.path().join("missing")).is_err());
    }
    #[test]
    fn manifest_requires_exact_non_elevated_namespace_and_attributes() {
        let good = r#"<assembly xmlns="urn:schemas-microsoft-com:asm.v1"><trustInfo xmlns="urn:schemas-microsoft-com:asm.v3"><security><requestedPrivileges><requestedExecutionLevel level="asInvoker" uiAccess="false"/></requestedPrivileges></security></trustInfo></assembly>"#;
        validate_manifest(good.as_bytes()).unwrap();
        for bad in [
            good.replace("asInvoker", "requireAdministrator"),
            good.replace("asInvoker", "highestAvailable"),
            good.replace("uiAccess=\"false\"", "uiAccess=\"true\""),
            good.replace("asm.v3", "wrong.namespace"),
            good.replace("asInvoker", "as&#73;nvoker"),
            format!("<!DOCTYPE assembly>{good}"),
            format!("{good}{good}"),
        ] {
            assert!(validate_manifest(bad.as_bytes()).is_err());
        }
    }
    #[cfg(windows)]
    #[test]
    fn nsis_destination_is_exact_unquoted_final_physical_path() {
        let dir = tempfile::tempdir().unwrap();
        let location = dir.path().join("DGX Bridge");
        fs::create_dir(&location).unwrap();
        let argument = nsis_destination(&location).unwrap();
        assert!(argument.starts_with("/D="));
        assert!(argument.ends_with("\\DGX Bridge"));
        assert!(!argument.contains('"'));
        assert!(!argument.contains("/S"));
        assert_eq!(
            plain(Path::new(&argument[3..])).unwrap(),
            plain(&location).unwrap()
        );
    }
    #[cfg(windows)]
    #[test]
    fn downloaded_file_keeps_windows_internet_zone() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("fixture.exe");
        fs::write(&file, b"synthetic package").unwrap();
        win::mark_download(&file).unwrap();
        let zone = PathBuf::from(format!("{}:Zone.Identifier", file.display()));
        assert_eq!(
            fs::read(zone).unwrap(),
            b"[ZoneTransfer]\r\nZoneId=3\r\nHostUrl=https://deloresai.com/\r\n"
        );
        assert_eq!(fs::read(file).unwrap(), b"synthetic package");
    }
}
/// A running new app can confirm its own version; native visible acceptance remains separate.
pub fn confirm_launch() {
    if confirm_once() {
        return;
    }
    // NSIS's Finish page can start the new GUI just before its process exits.
    // Retry only that bounded, observed 'installing' state; never block the UI.
    std::thread::spawn(|| {
        for _ in 0..60 {
            std::thread::sleep(Duration::from_secs(1));
            if confirm_once() {
                break;
            }
        }
    });
}
fn confirm_once() -> bool {
    let Ok(root) = updates::root() else {
        return true;
    };
    let path = root.join("last-update.json");
    let Ok(meta) = fs::metadata(&path) else {
        return true;
    };
    if meta.len() > 16384 {
        return true;
    }
    let Ok(bytes) = fs::read(&path) else {
        return true;
    };
    let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) else {
        return true;
    };
    if value["status"] == "installing" && value["to_version"] == env!("CARGO_PKG_VERSION") {
        return false;
    }
    if value["status"] == "installer_finished"
        && value["to_version"] == env!("CARGO_PKG_VERSION")
        && normal_context().is_ok()
    {
        let Ok(current) = std::env::current_exe().and_then(|p| p.canonicalize()) else {
            return true;
        };
        let Some(expected) = value["executable"].as_str() else {
            return true;
        };
        if Path::new(expected).canonicalize().ok().as_ref() != Some(&current) {
            return true;
        }
        value["status"] = json!("relaunch_version_confirmed");
        value["launched_version"] = json!(env!("CARGO_PKG_VERSION"));
        value["relaunch_at"] = json!(Utc::now());
        let _ = save_receipt(&value);
    }
    true
}

#[cfg(windows)]
mod win {
    use super::*;
    use std::{ffi::c_void, os::windows::ffi::OsStrExt, ptr};
    type Handle = *mut c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn GetCurrentPackageFullName(length: *mut u32, name: *mut u16) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn QueryFullProcessImageNameW(
            process: Handle,
            flags: u32,
            name: *mut u16,
            size: *mut u32,
        ) -> i32;
        fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
        fn GetExitCodeProcess(process: Handle, code: *mut u32) -> i32;
        fn K32EnumProcesses(ids: *mut u32, bytes: u32, needed: *mut u32) -> i32;
        fn LoadLibraryExW(path: *const u16, file: Handle, flags: u32) -> Handle;
        fn FindResourceW(module: Handle, name: *const u16, kind: *const u16) -> Handle;
        fn SizeofResource(module: Handle, resource: Handle) -> u32;
        fn LoadResource(module: Handle, resource: Handle) -> Handle;
        fn LockResource(resource: Handle) -> Handle;
        fn FreeLibrary(module: Handle) -> i32;
        fn GetCurrentProcess() -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
    }
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
        fn GetTokenInformation(
            token: Handle,
            class: u32,
            buffer: Handle,
            length: u32,
            needed: *mut u32,
        ) -> i32;
    }
    #[link(name = "ole32")]
    unsafe extern "system" {
        fn CoInitializeEx(reserved: Handle, flags: u32) -> i32;
        fn CoUninitialize();
    }
    struct Com;
    impl Com {
        fn initialize() -> Result<Self> {
            if unsafe { CoInitializeEx(ptr::null_mut(), 6) } < 0 {
                Err("Windows shell security initialization failed.".into())
            } else {
                Ok(Self)
            }
        }
    }
    impl Drop for Com {
        fn drop(&mut self) {
            unsafe {
                CoUninitialize();
            }
        }
    }
    #[repr(C)]
    struct ShellInfo {
        size: u32,
        mask: u32,
        window: Handle,
        verb: *const u16,
        file: *const u16,
        parameters: *const u16,
        directory: *const u16,
        show: i32,
        instance: Handle,
        idlist: Handle,
        class: *const u16,
        classkey: Handle,
        hotkey: u32,
        icon: Handle,
        process: Handle,
    }
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteExW(info: *mut ShellInfo) -> i32;
    }
    pub struct Process(Handle);
    impl Drop for Process {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    fn wide(value: &std::ffi::OsStr) -> Vec<u16> {
        value.encode_wide().chain(Some(0)).collect()
    }
    pub fn open(value: &str) -> Result<()> {
        let value = value.to_owned();
        std::thread::spawn(move || open_os(std::ffi::OsStr::new(&value)))
            .join()
            .map_err(|_| "Windows browser launch failed.")?
    }
    pub fn open_path(value: &Path) -> Result<()> {
        plain(value)?;
        open_os(value.as_os_str())
    }
    pub fn normal_user() -> Result<()> {
        let mut token = ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), 8, &mut token) } == 0 {
            return Err("Windows process privileges could not be verified.".into());
        }
        let token = Process(token);
        let mut elevated = 0u32;
        let mut needed = 0u32;
        let okay = unsafe {
            GetTokenInformation(
                token.0,
                20,
                (&mut elevated as *mut u32).cast(),
                4,
                &mut needed,
            )
        } != 0;
        if !okay || needed != 4 || elevated != 0 {
            return Err("Run RTXBridge as your ordinary Windows user before updating; administrator setup is blocked.".into());
        }
        Ok(())
    }
    pub fn manifest(path: &Path) -> Result<Vec<u8>> {
        let name = wide(path.as_os_str());
        let module = unsafe { LoadLibraryExW(name.as_ptr(), ptr::null_mut(), 0x60) };
        if module.is_null() {
            return Err(unsafe_setup());
        }
        let result = (|| {
            let resource =
                unsafe { FindResourceW(module, 1usize as *const u16, 24usize as *const u16) };
            if resource.is_null() {
                return Err(unsafe_setup());
            }
            let size = unsafe { SizeofResource(module, resource) } as usize;
            if size == 0 || size > 65536 {
                return Err(unsafe_setup());
            }
            let loaded = unsafe { LoadResource(module, resource) };
            if loaded.is_null() {
                return Err(unsafe_setup());
            }
            let bytes = unsafe { LockResource(loaded) };
            if bytes.is_null() {
                return Err(unsafe_setup());
            }
            Ok(unsafe { std::slice::from_raw_parts(bytes.cast::<u8>(), size) }.to_vec())
        })();
        unsafe {
            FreeLibrary(module);
        };
        result
    }
    pub fn reopen_path(value: &Path) -> Result<()> {
        // Interactive NSIS can already launch the app from its Finish page.
        // Observe that exact executable before requesting another process.
        let expected = plain(value)?;
        let mut ids = vec![0u32; 16384];
        let mut needed = 0u32;
        if unsafe { K32EnumProcesses(ids.as_mut_ptr(), (ids.len() * 4) as u32, &mut needed) } == 0
            || needed as usize >= ids.len() * 4
        {
            return Err(
                "Setup ended. Open RTXBridge from its Windows shortcut to confirm the result."
                    .into(),
            );
        }
        for id in ids.into_iter().take(needed as usize / 4) {
            let handle = unsafe { OpenProcess(0x1000, 0, id) };
            if handle.is_null() {
                continue;
            }
            let process = Process(handle);
            let mut buffer = vec![0u16; 32768];
            let mut len = buffer.len() as u32;
            if unsafe { QueryFullProcessImageNameW(process.0, 0, buffer.as_mut_ptr(), &mut len) }
                != 0
            {
                let path = PathBuf::from(String::from_utf16_lossy(&buffer[..len as usize]));
                if path.canonicalize().ok().as_ref() == Some(&expected) {
                    return Ok(());
                }
            }
        }
        open_path(value)
    }
    fn open_os(value: &std::ffi::OsStr) -> Result<()> {
        let _com = Com::initialize()?;
        let value = wide(value);
        let verb = wide(std::ffi::OsStr::new("open"));
        let mut info = ShellInfo {
            size: std::mem::size_of::<ShellInfo>() as u32,
            mask: 0x100,
            window: ptr::null_mut(),
            verb: verb.as_ptr(),
            file: value.as_ptr(),
            parameters: ptr::null(),
            directory: ptr::null(),
            show: 1,
            instance: ptr::null_mut(),
            idlist: ptr::null_mut(),
            class: ptr::null(),
            classkey: ptr::null_mut(),
            hotkey: 0,
            icon: ptr::null_mut(),
            process: ptr::null_mut(),
        };
        if unsafe { ShellExecuteExW(&mut info) } == 0 {
            Err("Windows could not open the requested browser or application.".into())
        } else {
            Ok(())
        }
    }
    pub fn parent(pid: u32, expected: &Path) -> Result<Process> {
        let handle = unsafe { OpenProcess(0x100000 | 0x1000, 0, pid) };
        if handle.is_null() {
            return Err("The original RTXBridge process is unavailable.".into());
        }
        let process = Process(handle);
        let mut buffer = vec![0u16; 32768];
        let mut len = buffer.len() as u32;
        if unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut len) } == 0 {
            return Err("Could not verify the original RTXBridge process.".into());
        }
        let name = PathBuf::from(String::from_utf16_lossy(&buffer[..len as usize]));
        if plain(&name)? != plain(expected)? {
            return Err("The updater parent process identity changed.".into());
        }
        Ok(process)
    }
    pub fn wait_parent(parent: Process) -> Result<()> {
        if unsafe { WaitForSingleObject(parent.0, 90_000) } != 0 {
            return Err("RTXBridge did not close. Nothing was installed.".into());
        }
        Ok(())
    }
    pub fn mark_download(path: &Path) -> Result<()> {
        // Preserve Windows' downloaded-file warning. This installer is intentionally unsigned.
        let zone = PathBuf::from(format!("{}:Zone.Identifier", path.display()));
        fs::write(
            zone,
            b"[ZoneTransfer]\r\nZoneId=3\r\nHostUrl=https://deloresai.com/\r\n",
        )
        .map_err(|_| "Windows download provenance could not be recorded.".into())
    }
    pub fn install(path: &Path, destination: &Path) -> Result<u32> {
        normal_user()?;
        let _com = Com::initialize()?;
        let parameters = wide(std::ffi::OsStr::new(&nsis_destination(destination)?));
        let file = wide(path.as_os_str());
        let verb = wide(std::ffi::OsStr::new("open"));
        let mut info = ShellInfo {
            size: std::mem::size_of::<ShellInfo>() as u32,
            mask: 0x40 | 0x100,
            window: ptr::null_mut(),
            verb: verb.as_ptr(),
            file: file.as_ptr(),
            parameters: parameters.as_ptr(),
            directory: ptr::null(),
            show: 1,
            instance: ptr::null_mut(),
            idlist: ptr::null_mut(),
            class: ptr::null(),
            classkey: ptr::null_mut(),
            hotkey: 0,
            icon: ptr::null_mut(),
            process: ptr::null_mut(),
        };
        if unsafe { ShellExecuteExW(&mut info) } == 0 || info.process.is_null() {
            return Err("Windows setup was cancelled or could not start. Open RTXBridge to try a fresh approval.".into());
        }
        let process = Process(info.process);
        if unsafe { WaitForSingleObject(process.0, u32::MAX) } != 0 {
            return Err("Windows setup ended without a confirmed result.".into());
        }
        let mut code = 0;
        if unsafe { GetExitCodeProcess(process.0, &mut code) } == 0 {
            return Err("Windows setup result is unavailable.".into());
        }
        Ok(code)
    }
}
