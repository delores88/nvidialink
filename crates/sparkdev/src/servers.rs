//! Durable model-server requests using the authenticated controller, never public ports.
use super::*;
use sha2::{Digest, Sha256};

const INPUT_LIMIT: usize = 1024 * 1024;
const REPLY_LIMIT: u64 = 256 * 1024 * 1024;
const CHUNK_LIMIT: u64 = 2 * 1024 * 1024;

fn check_expected_body(body: &[u8], expected: Option<&str>) -> Result<()> {
    if expected.is_some_and(|expected| inventory::hash(body) != expected) {
        return Err(AppError::new(5, "Model input changed after device selection; restore the retained input or submit a new request ID"));
    }
    Ok(())
}

pub(crate) fn valid_name(name: &str) -> bool {
    !name.starts_with("laptop")
        && name.len() <= 24
        && name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
fn call_id(id: &str) -> Result<()> {
    if id.len() > 64
        || !id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(AppError::new(2, "Invalid model call request ID"));
    }
    state::validate_id(id)
}

/// Preserve global --json output while accepting the documented call --json BODY.
pub(crate) fn normalize_call_args(mut args: Vec<String>) -> Vec<String> {
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" | "--profile-scoped" => index += 1,
            "--client" | "--device" | "--min-memory-gb" | "--min-vram-gb" => index += 2,
            v if ["--client=", "--device=", "--min-memory-gb=", "--min-vram-gb="]
                .iter().any(|prefix| v.starts_with(prefix)) => index += 1,
            "call" => {
                index += 1;
                while index < args.len() {
                    let value = args[index].as_str();
                    if value == "--" {
                        break;
                    }
                    if value == "--json" {
                        args[index] = "--body-json".into();
                        index += 2; // Its payload is data, even if it resembles an option.
                    } else if let Some(body) = value.strip_prefix("--json=") {
                        args[index] = format!("--body-json={body}");
                        index += 1;
                    } else if [
                        "--body-json",
                        "--input",
                        "--path",
                        "--content-type",
                        "--save",
                        "--timeout",
                        "--request-id",
                        "--client",
                        "--device",
                        "--min-memory-gb",
                        "--min-vram-gb",
                    ]
                    .contains(&value)
                    {
                        index += 2;
                    } else {
                        index += 1;
                    }
                }
                break;
            }
            _ => break,
        }
    }
    args
}

fn upgrade_error(e: AppError) -> AppError {
    if e.message.starts_with("unknown_operation:") {
        AppError::new(6, "Update RTXBridge on your Spark to use model servers.")
    } else {
        e
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn serve(
    dir: &Path,
    name: &str,
    port: u16,
    memory: u64,
    runtime: Option<String>,
    idle: u64,
    replace: bool,
    request_id: Option<String>,
    argv: Vec<String>,
    client: &str,
) -> Result<Value> {
    if !valid_name(name) || port == 0 || !(1..=48).contains(&memory) || !(5..=240).contains(&idle) {
        return Err(AppError::new(
            2,
            "Use a server name of 1â€“24 lowercase letters, digits or hyphens (not laptop*), a port above zero, 1â€“48 GiB, and 5â€“240 idle minutes.",
        ));
    }
    let inv = inventory::collect(dir)?;
    crate::enforce_expected_manifest(&inv.manifest_id)?;
    let id = request_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    state::validate_id(&id)?;
    let mut request = json!({"v":1,"op":"serve","project":inv.project,"manifest_id":inv.manifest_id,
        "argv":argv,"client":client,"request_id":id,"name":name,"port":port,"memory_gb":memory,
        "idle_minutes":idle,"replace":replace});
    if let Some(runtime) = runtime {
        request["runtime"] = json!(runtime);
    }
    state::save_request(&id, &request)?;
    let mut tr = transport::Transport::new()?;
    let synced = sync(&inv, &mut tr)?;
    if !tr.connection_enabled() {
        return Err(disconnected());
    }
    if synced["manifest_id"] != inv.manifest_id {
        return Err(AppError::new(
            6,
            "Synced server manifest differs from the saved request",
        ));
    }
    let mut value = tr.rpc(request).map_err(upgrade_error)?;
    value["request_id"] = json!(id);
    value["transport"] = tr.info();
    Ok(value)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn call(
    name: &str,
    body_json: Option<String>,
    input: Option<PathBuf>,
    path: &str,
    content_type: Option<String>,
    save: Option<PathBuf>,
    timeout: u64,
    request_id: Option<String>,
    client: &str,
) -> Result<Value> {
    if !valid_name(name)
        || timeout > 86400
        || !path.starts_with('/')
        || path.len() > 1024
        || !path.bytes().all(|c| (33..=126).contains(&c))
    {
        return Err(AppError::new(
            2,
            "Invalid server name, request path or timeout",
        ));
    }
    let is_json = body_json.is_some();
    let body = if let Some(text) = body_json {
        if text.len() > INPUT_LIMIT {
            return Err(AppError::new(2, "Model call input exceeds 1 MiB"));
        }
        let _: Value = serde_json::from_str(&text)?;
        text.into_bytes()
    } else if let Some(file) = input {
        if !fs::symlink_metadata(&file)?.file_type().is_file() {
            return Err(AppError::new(2, "Model input must be a regular file"));
        }
        let mut bytes = Vec::new();
        fs::File::open(file)?
            .take(INPUT_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)?;
        bytes
    } else {
        Vec::new()
    };
    if body.len() > INPUT_LIMIT {
        return Err(AppError::new(2, "Model call input exceeds 1 MiB"));
    }
    let expected_body = std::env::var("SPARKDEV_EXPECTED_CALL_BODY")
        .map(Some)
        .or_else(|error| if matches!(error, std::env::VarError::NotPresent) { Ok(None) } else { Err(error) })
        .map_err(|_| AppError::new(5, "Invalid retained model input identity"))?;
    check_expected_body(&body, expected_body.as_deref())?;
    inventory::scan(&body)?;
    let content_type = content_type.unwrap_or_else(|| {
        if is_json {
            "application/json".into()
        } else {
            "application/octet-stream".into()
        }
    });
    if content_type.is_empty()
        || content_type.len() > 128
        || !content_type.bytes().all(|b| (32..=126).contains(&b))
    {
        return Err(AppError::new(2, "Invalid content type"));
    }
    let id = request_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    call_id(&id)?;
    let request = json!({"v":1,"op":"call","server":name,"client":client,"request_id":id,
        "body":STANDARD.encode(body),"content_type":content_type,"path":path});
    state::save_request(&id, &request)?;
    let mut tr = transport::Transport::new()?;
    let started = Instant::now();
    let mut first = true;
    loop {
        let remaining = Duration::from_secs(timeout).saturating_sub(started.elapsed());
        if !first && remaining.is_zero() {
            return Ok(
                json!({"ok":true,"state":"waiting","request_id":id,"server":name,
                "message":"The model is still working. Repeat the call with this request ID and identical input."}),
            );
        }
        let mut poll = request.clone();
        poll["timeout"] = json!(remaining.as_secs().min(20));
        let response = tr.rpc(poll).map_err(upgrade_error)?;
        first = false;
        if response["request_id"] != id || response["server"] != name {
            return Err(AppError::new(6, "Model reply identity mismatch"));
        }
        match response["state"].as_str() {
            Some("done") => {
                let folder = state::root()?.join("replies").join(name);
                fs::create_dir_all(&folder)?;
                let output = folder.join(format!("{id}.body"));
                let mut result = receive_reply(&mut tr, response, &output)?;
                if let Some(save) = save {
                    copy_without_overwrite(&output, &save)?;
                    result["saved_as"] = json!(save);
                }
                result["transport"] = tr.info();
                return Ok(result);
            }
            Some("failed" | "stopped" | "expired") => {
                let mut result = response;
                result["process_exit_code"] = json!(1);
                return Ok(result);
            }
            Some("loading" | "waiting") => {
                if started.elapsed() >= Duration::from_secs(timeout) {
                    return Ok(response);
                }
                std::thread::sleep(Duration::from_millis(100).min(remaining));
            }
            _ => return Err(AppError::new(6, "Unknown model reply state")),
        }
    }
}

fn copy_without_overwrite(source: &Path, destination: &Path) -> Result<()> {
    let temporary = destination.with_extension(format!("pending-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut target = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let count = std::io::copy(
            &mut fs::File::open(source)?.take(REPLY_LIMIT + 1),
            &mut target,
        )?;
        if count > REPLY_LIMIT {
            return Err(AppError::new(6, "Saved model reply exceeds its size limit"));
        }
        target.sync_all()?;
        drop(target);
        // Same-directory hard linking publishes complete bytes atomically and
        // fails if another caller or the owner already created the destination.
        fs::hard_link(&temporary, destination)?;
        Ok(())
    })();
    let _ = fs::remove_file(temporary);
    result
}

fn existing_reply_matches(path: &Path, size: u64, digest: &str) -> Result<bool> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() != size || size > REPLY_LIMIT {
        return Ok(false);
    }
    let mut stream = fs::File::open(path)?.take(REPLY_LIMIT + 1);
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    let mut count = 0u64;
    loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        count += read as u64;
        hash.update(&buffer[..read]);
    }
    Ok(count == size && format!("{:x}", hash.finalize()) == digest)
}

fn publish_verified_reply(
    pending: &Path,
    destination: &Path,
    size: u64,
    digest: &str,
) -> Result<()> {
    match fs::hard_link(pending, destination) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if !existing_reply_matches(destination, size, digest)? {
                return Err(AppError::new(
                    3,
                    "Saved model reply already contains different data",
                ));
            }
        }
        Err(error) => return Err(error.into()),
    }
    fs::remove_file(pending)?;
    Ok(())
}

fn receive_reply(tr: &mut impl Rpc, first: Value, destination: &Path) -> Result<Value> {
    let size = first["size"]
        .as_u64()
        .filter(|n| *n <= REPLY_LIMIT)
        .ok_or_else(|| AppError::new(6, "Invalid model reply size"))?;
    let digest = first["sha256"]
        .as_str()
        .filter(|s| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        .ok_or_else(|| AppError::new(6, "Invalid model reply digest"))?
        .to_owned();
    let id = first["request_id"]
        .as_str()
        .ok_or_else(|| AppError::new(6, "Missing model reply identity"))?
        .to_owned();
    call_id(&id)?;
    let server = first["server"]
        .as_str()
        .filter(|s| valid_name(s))
        .ok_or_else(|| AppError::new(6, "Invalid model server identity"))?
        .to_owned();
    let job = first["job"]
        .as_str()
        .ok_or_else(|| AppError::new(6, "Missing model job identity"))?
        .to_owned();
    state::validate_id(&job)?;
    let status = first["status"]
        .as_u64()
        .filter(|v| (100..=599).contains(v))
        .ok_or_else(|| AppError::new(6, "Invalid model HTTP status"))?;
    let content_type = first["content_type"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| (32..=126).contains(&b)))
        .ok_or_else(|| AppError::new(6, "Invalid model content type"))?
        .to_owned();
    let pending = destination.with_extension(format!("pending-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&pending)?;
        let mut hash = Sha256::new();
        let mut inline = if size <= 65536 {
            Some(Vec::with_capacity(size as usize))
        } else {
            None
        };
        let mut offset = 0;
        let mut chunk = first.clone();
        loop {
            if chunk["request_id"] != id
                || chunk["server"] != server
                || chunk["job"] != job
                || chunk["status"] != status
                || chunk["content_type"] != content_type
                || chunk["size"] != size
                || chunk["sha256"] != digest
                || chunk["state"] != "done"
                || chunk["since"] != offset
            {
                return Err(AppError::new(6, "Model reply changed while downloading"));
            }
            let encoded = chunk["data"]
                .as_str()
                .filter(|s| s.len() as u64 <= ((CHUNK_LIMIT + 2) / 3) * 4)
                .ok_or_else(|| AppError::new(6, "Invalid model reply encoding or chunk size"))?;
            let raw = STANDARD
                .decode(encoded)
                .map_err(|_| AppError::new(6, "Invalid model reply encoding"))?;
            let next = offset + raw.len() as u64;
            if raw.len() as u64 > CHUNK_LIMIT
                || next > size
                || chunk["next_offset"] != next
                || (raw.is_empty() && offset < size)
            {
                return Err(AppError::new(6, "Invalid model reply offset"));
            }
            hash.update(&raw);
            if let Some(inline) = &mut inline {
                inline.extend_from_slice(&raw);
            }
            file.write_all(&raw)?;
            offset = next;
            if offset == size {
                break;
            }
            chunk = tr
                .call(
                    json!({"v":1,"op":"reply","request_id":id,"since":offset,"limit":CHUNK_LIMIT}),
                )
                .map_err(upgrade_error)?;
        }
        if format!("{:x}", hash.finalize()) != digest {
            return Err(AppError::new(6, "Model reply hash mismatch"));
        }
        file.sync_all()?;
        drop(file);
        publish_verified_reply(&pending, destination, size, &digest)?;
        let mut value = first;
        value.as_object_mut().unwrap().remove("data");
        value["next_offset"] = json!(size);
        value["file"] = json!(destination);
        value["hash_verified"] = json!(true);
        if let Some(bytes) = inline
            && let Ok(value_inline) = serde_json::from_slice::<Value>(&bytes)
        {
            value["json"] = value_inline;
        }
        value["process_exit_code"] = json!(if value["status"]
            .as_u64()
            .is_some_and(|n| (200..300).contains(&n))
        {
            0
        } else {
            1
        });
        Ok(value)
    })();
    if result.is_err() {
        let _ = fs::remove_file(pending);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        rows: std::collections::VecDeque<Value>,
    }
    impl Rpc for Fake {
        fn call(&mut self, _: Value) -> Result<Value> {
            Ok(self.rows.pop_front().unwrap())
        }
        fn route(&self) -> String {
            "iroh".into()
        }
        fn connection_enabled(&self) -> bool {
            true
        }
    }
    fn reply(raw: &[u8], size: usize, digest: &str, since: usize) -> Value {
        json!({"ok":true,"request_id":"call-1","server":"tts","job":"job-1","content_type":"application/json","state":"done","status":200,"size":size,"sha256":digest,"since":since,"next_offset":since+raw.len(),"data":STANDARD.encode(raw)})
    }
    #[test]
    fn names_and_global_json_remain_unambiguous() {
        for n in ["tts", "model-1"] {
            assert!(valid_name(n));
        }
        for n in ["", "../bad", "laptop", "laptop-1", "TTS", "1tts"] {
            assert!(!valid_name(n));
        }
        let args = ["--json", "--client", "codex", "call", "tts", "--json", "{}"]
            .map(str::to_owned)
            .to_vec();
        let normalized = normalize_call_args(args);
        assert_eq!(normalized[0], "--json");
        assert_eq!(normalized[5], "--body-json");
        assert!(Cli::try_parse_from(std::iter::once("sparkdev".into()).chain(normalized)).is_ok());
        let values = [
            "call",
            "tts",
            "--content-type",
            "--json",
            "--json",
            "{}",
            "--save=--json",
        ]
        .map(str::to_owned)
        .to_vec();
        let normalized = normalize_call_args(values);
        assert_eq!(normalized[3], "--json");
        assert_eq!(normalized[4], "--body-json");
        assert_eq!(normalized[6], "--save=--json");
        for id in ["call.id", "_call", "", "../id"] {
            assert!(call_id(id).is_err());
        }
        assert!(call_id("Call_1-ok").is_ok());
    }
    #[test]
    fn chunked_reply_is_verified_and_retained() {
        let temp = tempfile::tempdir().unwrap();
        let dest = temp.path().join("reply");
        let digest = inventory::hash(b"abcdef");
        let mut rpc = Fake {
            rows: [reply(b"def", 6, &digest, 3)].into(),
        };
        let got = receive_reply(&mut rpc, reply(b"abc", 6, &digest, 0), &dest).unwrap();
        assert_eq!(fs::read(dest).unwrap(), b"abcdef");
        assert_eq!(got["hash_verified"], true);
        assert_eq!(got["process_exit_code"], 0);
    }
    #[test]
    fn bad_hash_or_offset_never_publishes_file() {
        for mutate in [0, 1] {
            let temp = tempfile::tempdir().unwrap();
            let dest = temp.path().join("reply");
            let mut first = reply(b"abc", 3, &inventory::hash(b"wrong"), 0);
            if mutate == 1 {
                first["next_offset"] = json!(2);
            }
            assert!(receive_reply(&mut Fake { rows: [].into() }, first, &dest).is_err());
            assert!(!dest.exists());
            assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
        }
    }
    #[test]
    fn reply_chunks_bind_server_job_status_and_content_type() {
        for (field, value) in [
            ("server", json!("other")),
            ("job", json!("other")),
            ("status", json!(502)),
            ("content_type", json!("text/plain")),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let dest = temp.path().join("reply");
            let digest = inventory::hash(b"abcdef");
            let mut changed = reply(b"def", 6, &digest, 3);
            changed[field] = value;
            let mut rpc = Fake {
                rows: [changed].into(),
            };
            assert!(
                receive_reply(&mut rpc, reply(b"abc", 6, &digest, 0), &dest).is_err(),
                "{field}"
            );
            assert!(!dest.exists());
            assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
        }
    }
    #[test]
    fn publish_reuses_identical_bytes_and_never_overwrites_a_different_file() {
        let temp = tempfile::tempdir().unwrap();
        let dest = temp.path().join("reply");
        let digest = inventory::hash(b"abc");
        let first = reply(b"abc", 3, &digest, 0);
        receive_reply(&mut Fake { rows: [].into() }, first.clone(), &dest).unwrap();
        receive_reply(&mut Fake { rows: [].into() }, first.clone(), &dest).unwrap();
        fs::write(&dest, b"owner content").unwrap();
        assert!(receive_reply(&mut Fake { rows: [].into() }, first, &dest).is_err());
        assert_eq!(fs::read(&dest).unwrap(), b"owner content");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }
    #[test]
    fn save_is_atomic_without_overwriting_and_cleans_pending_files() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let dest = temp.path().join("saved");
        fs::write(&source, b"model response").unwrap();
        copy_without_overwrite(&source, &dest).unwrap();
        fs::write(&dest, b"owner file").unwrap();
        assert!(copy_without_overwrite(&source, &dest).is_err());
        assert_eq!(fs::read(&dest).unwrap(), b"owner file");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 2);
    }
    #[test]
    fn only_small_verified_json_is_inline_and_http_failure_is_an_exit_error() {
        for large in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let raw = if large {
                serde_json::to_vec(&json!({"text":"x".repeat(65536)})).unwrap()
            } else {
                b"{\"answer\":42}".to_vec()
            };
            let mut first = reply(&raw, raw.len(), &inventory::hash(&raw), 0);
            first["status"] = json!(503);
            let got = receive_reply(
                &mut Fake { rows: [].into() },
                first,
                &temp.path().join("result"),
            )
            .unwrap();
            assert_eq!(got.get("json").is_some(), !large);
            assert_eq!(got["process_exit_code"], 1);
        }
    }
}

#[cfg(test)]
mod device_call_regressions {
    use super::*;

    #[test]
    fn named_and_scoped_calls_keep_json_shorthand_and_payloads_unambiguous() {
        for prefix in [
            vec!["--json","--client","codex","--device","render"],
            vec!["--device=render","--profile-scoped","--json"],
            vec!["--min-vram-gb","20","--device","render"],
        ] {
            let mut args=prefix.iter().map(|v|v.to_string()).collect::<Vec<_>>();
            args.extend(["call","tts","--json","{\"value\":\"--device\"}"].map(str::to_owned));
            let normalized=normalize_call_args(args);
            assert_eq!(normalized[normalized.len()-2],"--body-json");
            assert_eq!(normalized.last().unwrap(),"{\"value\":\"--device\"}");
        }
        let args=["run",".","--","call","tts","--json","{}"].map(str::to_owned).to_vec();
        assert_eq!(normalize_call_args(args.clone()),args);
    }

    #[test]
    fn call_body_guard_refuses_changed_bytes_before_remote_submission() {
        let retained=inventory::hash(b"retained body");
        assert!(check_expected_body(b"retained body",Some(&retained)).is_ok());
        let failure=check_expected_body(b"changed body",Some(&retained)).unwrap_err();
        assert_eq!(failure.code,5);
        assert!(check_expected_body(b"legacy body",None).is_ok());
        assert!(check_expected_body(b"retained body",Some("malformed")).is_err());
    }
}
