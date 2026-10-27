//! Proof-bound, browser-approved updates. Credentials never cross into the UI or disk.
use crate::{
    update_helper,
    update_verify::{self as verify, APP, Artifact, Envelope, ORIGIN, Result, Verified},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tauri::Manager;
use tokio::io::AsyncWriteExt;
use zeroize::Zeroizing;

#[derive(Clone)]
pub struct UpdateState {
    inner: Arc<tokio::sync::Mutex<Inner>>,
    view: Arc<Mutex<Value>>,
    busy: Arc<AtomicBool>,
}
struct Inner {
    candidate: Option<Artifact>,
    authorization: Option<Authorization>,
    downloaded: Option<PathBuf>,
}
struct Authorization {
    request_id: String,
    verifier: Zeroizing<String>,
    expires: DateTime<Utc>,
    next_poll: Instant,
    grant: Option<Grant>,
    verified: Option<Verified>,
}
struct Grant {
    secret: Zeroizing<String>,
    expires: DateTime<Utc>,
}
impl Default for UpdateState {
    fn default() -> Self {
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(Inner {
                candidate: None,
                authorization: None,
                downloaded: None,
            })),
            view: Arc::new(Mutex::new(
                json!({"ok":true,"status":"idle","current_version":env!("CARGO_PKG_VERSION"),"available_version":null,"message":"Check for an approved RTXBridge update."}),
            )),
            busy: Arc::new(AtomicBool::new(false)),
        }
    }
}
impl UpdateState {
    fn set(&self, status: &str, message: &str, extra: Value) {
        if let Ok(mut view) = self.view.lock() {
            view["ok"] = json!(status != "error");
            view["status"] = json!(status);
            view["message"] = json!(message);
            if let Some(fields) = extra.as_object() {
                for (k, v) in fields {
                    view[k] = v.clone();
                }
            }
        }
    }
    fn snapshot(&self) -> Value {
        let mut value = self.view.lock().map(|v| v.clone()).unwrap_or_else(
            |_| json!({"ok":false,"status":"error","message":"Update state is unavailable."}),
        );
        value["busy"] = json!(self.busy.load(Ordering::SeqCst));
        value["last_update"] = update_helper::last_receipt().unwrap_or(Value::Null);
        value
    }
}
pub fn root() -> Result<PathBuf> {
    let root = sparkdev::state::root()
        .map_err(|_| "Application state is unavailable.")?
        .join("updates");
    std::fs::create_dir_all(&root).map_err(|_| "Update storage is unavailable.")?;
    update_helper::plain(&root)?;
    Ok(root)
}
pub fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(12))
        .timeout(Duration::from_secs(35))
        .user_agent(concat!("RTXBridge/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| "Secure update transport is unavailable.".into())
}
pub async fn json_request(
    client: &reqwest::Client,
    path: &str,
    body: Option<Value>,
) -> Result<Value> {
    // Callers construct this path from fixed endpoints and validated 24-byte IDs.
    if !path.starts_with("/api/v1/") || path.contains(['\\', '#']) {
        return Err("Invalid update request.".into());
    }
    let req = if let Some(body) = body {
        client.post(format!("{ORIGIN}{path}")).json(&body)
    } else {
        client.get(format!("{ORIGIN}{path}"))
    };
    let mut response=req.header("Cache-Control","no-store").send().await.map_err(|_|"Marketplace connection failed. If authorization was being exchanged, request new browser approval.")?;
    let status = response.status();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(
        |_| "Marketplace response was interrupted. Request new approval before another exchange.",
    )? {
        if bytes.len() + chunk.len() > 1_048_576 {
            return Err("Marketplace response exceeded its safe limit.".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        // Never relay untrusted error text, request bodies, proof or response tokens.
        return Err(match status.as_u16(){
            401|403=>"Update authorization was refused. Request new browser approval.",
            404=>"This update request is unavailable. Request new browser approval.",
            409=>"The one-use authorization or release changed. Check for updates and explicitly approve a new request.",
            410=>"Update approval expired. Request new browser approval.",
            429=>"Marketplace is busy. Wait before explicitly requesting new approval.",
            500..=599=>"Marketplace verification is temporarily unavailable. No update was installed.",
            _=>"Marketplace returned an unsupported update response. No update was installed.",
        }.into());
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| "Marketplace returned invalid update metadata.".into())
}
pub fn request_id(s: &str) -> bool {
    s.len() == 24 && verify::safe_id(s)
}
pub fn proof_valid(s: &str) -> bool {
    URL_SAFE_NO_PAD
        .decode(s)
        .is_ok_and(|v| v.len() == 32 && URL_SAFE_NO_PAD.encode(v) == s)
}
pub fn expiry(value: &Value, max_minutes: i64) -> Result<DateTime<Utc>> {
    let date = DateTime::parse_from_rfc3339(value.as_str().ok_or("Missing authorization expiry.")?)
        .map_err(|_| "Invalid authorization expiry.")?
        .with_timezone(&Utc);
    let now = Utc::now();
    if date <= now
        || date > now + chrono::Duration::minutes(max_minutes) + chrono::Duration::seconds(30)
    {
        return Err("Authorization expiry is outside the supported window.".into());
    }
    Ok(date)
}
fn envelope(value: &Value) -> Result<Envelope> {
    serde_json::from_value(value.clone())
        .map_err(|_| "Signed update authorization is missing or invalid.".into())
}
pub async fn revalidate(
    client: &reqwest::Client,
    id: &str,
    proof: &str,
    expected: &Artifact,
    root: &Path,
) -> Result<Verified> {
    if !request_id(id) || !proof_valid(proof) {
        return Err("Update proof is invalid.".into());
    }
    let v = json_request(
        client,
        &format!("/api/v1/native-updates/{id}/revalidate"),
        Some(json!({"codeVerifier":proof})),
    )
    .await?;
    if v["artifact"] != expected.raw {
        return Err("The approved release changed. Request new browser approval.".into());
    }
    expiry(&v["expiresAt"], 10)?;
    let verified = verify::verify(&envelope(&v["metadata"])?, expected, Utc::now())?;
    verify::persist(&verified, root, Utc::now())?;
    Ok(verified)
}
pub async fn action(app: &tauri::AppHandle, action: &str) -> Result<Value> {
    let state = app.state::<UpdateState>().inner().clone();
    if action == "status" {
        return Ok(state.snapshot());
    }
    if !["check", "authorize", "poll", "download", "install"].contains(&action) {
        return Err("Unsupported update operation.".into());
    }
    if state
        .busy
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Ok(state.snapshot());
    }
    let app = app.clone();
    let operation = action.to_owned();
    let worker = state.clone();
    tauri::async_runtime::spawn(async move {
        let result = perform(&app, &worker, &operation).await;
        if let Err(message) = result {
            worker.set(
                "error",
                &message,
                json!({"verification_code":null,"verification_uri":null}),
            );
        }
        worker.busy.store(false, Ordering::SeqCst);
    });
    Ok(state.snapshot())
}
async fn perform(app: &tauri::AppHandle, state: &UpdateState, action: &str) -> Result<()> {
    let mut inner = state.inner.lock().await;
    let client = client()?;
    let root = root()?;
    match action {
        "check" => {
            inner.authorization = None;
            inner.downloaded = None;
            inner.candidate = None;
            state.set("checking","Checking approved stable releases.",json!({"available_version":null,"verification_code":null,"verification_uri":null,"expires_at":null,"progress_bytes":null,"total_bytes":null}));
            let catalog = json_request(
                &client,
                "/api/v1/catalog/dgxbridge?acceptancePolicy=immutable-release",
                None,
            )
            .await?;
            inner.candidate = verify::select(&catalog, env!("CARGO_PKG_VERSION"))?;
            if let Some(a) = &inner.candidate {
                state.set(
                    "available",
                    "An approved stable release is available.",
                    json!({"available_version":a.version,"total_bytes":a.size}),
                );
            } else {
                state.set(
                    "current",
                    "No newer approved stable release is available.",
                    json!({}),
                );
            }
        }
        "authorize" => {
            let candidate = inner
                .candidate
                .clone()
                .ok_or("Check for an approved update first.")?;
            // Every explicit authorize action starts fresh; uncertain exchanges are never retried.
            inner.authorization = None;
            inner.downloaded = None;
            let mut random = [0u8; 32];
            getrandom::fill(&mut random).map_err(|_| "Secure randomness is unavailable.")?;
            let verifier = Zeroizing::new(URL_SAFE_NO_PAD.encode(random));
            let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
            let reply=json_request(&client,"/api/v1/native-updates",Some(json!({"appId":APP,"artifactId":candidate.id,"codeChallenge":challenge,"codeChallengeMethod":"S256","acceptancePolicy":"immutable-release"}))).await?;
            let id = reply["requestId"]
                .as_str()
                .filter(|id| request_id(id))
                .ok_or("Marketplace returned an invalid request identity.")?
                .to_owned();
            let uri = format!("{ORIGIN}/native-updates/{id}");
            let code = reply["verificationCode"]
                .as_str()
                .filter(|s| {
                    s.len() == 9
                        && s.as_bytes()[4] == b'-'
                        && s.bytes()
                            .enumerate()
                            .all(|(i, b)| i == 4 || b.is_ascii_uppercase() || b.is_ascii_digit())
                })
                .ok_or("Marketplace returned an invalid approval code.")?;
            if reply["verificationUri"] != uri || reply["interval"] != 5 {
                return Err("Marketplace returned an unsupported approval destination.".into());
            }
            let expires = expiry(&reply["expiresAt"], 10)?;
            inner.authorization = Some(Authorization {
                request_id: id,
                verifier,
                expires,
                next_poll: Instant::now() + Duration::from_secs(5),
                grant: None,
                verified: None,
            });
            state.set(
                "awaiting_approval",
                "Compare this code in your browser, sign in, and approve the exact update.",
                json!({"verification_code":code,"verification_uri":uri,"expires_at":expires}),
            );
            if update_helper::open_browser(&uri).is_err() {
                state.set(
                    "awaiting_approval",
                    "Open the approval link in your browser and compare the code.",
                    json!({}),
                );
            }
        }
        "poll" => {
            let candidate = inner
                .candidate
                .clone()
                .ok_or("Check for an update first.")?;
            let auth = inner
                .authorization
                .as_mut()
                .ok_or("Request new browser approval.")?;
            if auth.verified.is_some() {
                return Ok(());
            }
            if auth.expires <= Utc::now() {
                inner.authorization = None;
                state.set(
                    "expired",
                    "Approval expired. Request new browser approval.",
                    json!({}),
                );
                return Ok(());
            }
            if Instant::now() < auth.next_poll {
                return Ok(());
            }
            auth.next_poll = Instant::now() + Duration::from_secs(5);
            let reply = match json_request(
                &client,
                &format!("/api/v1/native-updates/{}/poll", auth.request_id),
                Some(json!({"codeVerifier":auth.verifier.as_str()})),
            )
            .await
            {
                Ok(v) => v,
                Err(e) => {
                    inner.authorization = None;
                    return Err(e);
                }
            };
            match reply["status"].as_str() {
                Some("pending") => {}
                Some("denied" | "expired") => {
                    let status = reply["status"].as_str().unwrap();
                    inner.authorization = None;
                    state.set(status,"Browser approval was denied or expired. Request a new approval when ready.",json!({}));
                }
                Some("approved") => {
                    // Consume a successful response once; any validation error also destroys it.
                    let mut auth = inner.authorization.take().ok_or("Approval was lost.")?;
                    if reply["artifact"] != candidate.raw
                        || reply["downloadUrl"]
                            != format!(
                                "{ORIGIN}/api/v1/native-updates/{}/download",
                                auth.request_id
                            )
                    {
                        return Err("Approved update binding or download address changed.".into());
                    }
                    let grant = reply["downloadAuthorization"]
                        .as_str()
                        .filter(|s| proof_valid(s))
                        .ok_or("The download capability was invalid.")?;
                    let expires = expiry(&reply["downloadExpiresAt"], 5)?;
                    if expires > auth.expires {
                        return Err("Download capability outlives browser approval.".into());
                    }
                    let verified =
                        verify::verify(&envelope(&reply["metadata"])?, &candidate, Utc::now())?;
                    verify::persist(&verified, &root, Utc::now())?;
                    auth.grant = Some(Grant {
                        secret: Zeroizing::new(grant.to_owned()),
                        expires,
                    });
                    auth.verified = Some(verified);
                    inner.authorization = Some(auth);
                    state.set("approved","Browser approval verified. Download the approved installer before authorization expires.",json!({"verification_code":null,"verification_uri":null}));
                }
                _ => {
                    inner.authorization = None;
                    return Err("Marketplace returned an unsupported approval state.".into());
                }
            }
        }
        "download" => {
            let mut auth = inner
                .authorization
                .take()
                .ok_or("Request browser approval before downloading.")?;
            let verified = auth
                .verified
                .as_ref()
                .ok_or("Browser approval has not completed.")?;
            let grant = auth.grant.take().ok_or(
                "The one-use download was already attempted. Request new browser approval.",
            )?;
            if grant.expires <= Utc::now() || auth.expires <= Utc::now() {
                return Err("Download authorization expired. Request new browser approval.".into());
            }
            let mut random = [0u8; 16];
            getrandom::fill(&mut random).map_err(|_| "Secure randomness unavailable.")?;
            let session = root.join(format!("u-{}", verify::digest(&random)[..24].to_owned()));
            std::fs::create_dir(&session).map_err(|_| "Could not reserve update staging.")?;
            update_helper::plain(&session)?;
            let partial = session.join("installer.partial");
            let target = session.join("RTXBridge-update.exe");
            state.set(
                "downloading",
                "Downloading the exact approved installer.",
                json!({"progress_bytes":0,"total_bytes":verified.artifact.size}),
            );
            let result = download(
                &client,
                &auth.request_id,
                &auth.verifier,
                &grant.secret,
                &verified.artifact,
                &partial,
                state,
            )
            .await;
            if let Err(e) = result {
                let _ = std::fs::remove_file(&partial);
                return Err(e);
            }
            std::fs::rename(&partial, &target)
                .map_err(|_| "Verified download could not be finalized.")?;
            inner.downloaded = Some(target);
            inner.authorization = Some(auth);
            state.set("ready","Installer verified. Install update opens ordinary Windows setup and closes RTXBridge after a safe handoff.",json!({}));
        }
        "install" => {
            update_helper::normal_context()?;
            let candidate = inner
                .candidate
                .as_ref()
                .ok_or("No approved update selected.")?
                .clone();
            let target = inner
                .downloaded
                .as_ref()
                .ok_or("Download and verify the update first.")?
                .clone();
            let auth = inner
                .authorization
                .as_mut()
                .ok_or("Approval expired. Request new browser approval.")?;
            if auth.expires <= Utc::now() {
                return Err("Approval expired. Request new browser approval.".into());
            }
            let verified =
                revalidate(&client, &auth.request_id, &auth.verifier, &candidate, &root).await?;
            update_helper::start(
                &target,
                &verified,
                &auth.request_id,
                &auth.verifier,
                auth.expires,
            )
            .await?;
            state.set("installing","RTXBridge is closing. Follow Windows setup; your existing connection and job state stay in place.",json!({}));
            inner.authorization = None;
            app.exit(0);
        }
        _ => return Err("Unsupported updater operation.".into()),
    }
    Ok(())
}
async fn download(
    client: &reqwest::Client,
    id: &str,
    proof: &str,
    grant: &str,
    a: &Artifact,
    path: &Path,
    state: &UpdateState,
) -> Result<()> {
    if !request_id(id) || !proof_valid(proof) || !proof_valid(grant) {
        return Err("Invalid download authorization.".into());
    }
    let response=client.get(format!("{ORIGIN}/api/v1/native-updates/{id}/download")).bearer_auth(grant).header("X-Native-Update-Proof",proof)
        .header("Cache-Control","no-store").timeout(Duration::from_secs(300)).send().await.map_err(|_|"Download attempt was interrupted. Request new browser approval; the capability cannot be retried.")?;
    receive(response, a, path, state).await
}
async fn receive(
    mut response: reqwest::Response,
    a: &Artifact,
    path: &Path,
    state: &UpdateState,
) -> Result<()> {
    if response.status() != reqwest::StatusCode::OK {
        return Err("Download was refused or interrupted. Request new browser approval; nothing was installed.".into());
    }
    if response.content_length().is_some_and(|n| n != a.size) {
        return Err("Installer length does not match its signed approval.".into());
    }
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
        .map_err(|_| "Could not create protected update staging.")?;
    let mut hasher = Sha256::new();
    let mut count = 0u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Download was interrupted. Request new browser approval.")?
    {
        count = count
            .checked_add(chunk.len() as u64)
            .ok_or("Download exceeded its approved size.")?;
        if count > a.size {
            return Err("Download exceeded its approved size.".into());
        }
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|_| "Update disk write failed.")?;
        state.set(
            "downloading",
            "Downloading the exact approved installer.",
            json!({"progress_bytes":count}),
        );
    }
    let hash: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if count != a.size || hash != a.sha256 {
        return Err("Installer SHA-256 or size did not match its signed approval.".into());
    }
    file.sync_all()
        .await
        .map_err(|_| "Verified download could not be saved.")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn s256_uses_ascii_verifier_not_decoded_random_bytes() {
        let proof = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert!(proof_valid(proof));
        assert_eq!(
            URL_SAFE_NO_PAD.encode(Sha256::digest(proof.as_bytes())),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_ne!(
            URL_SAFE_NO_PAD.encode(Sha256::digest(URL_SAFE_NO_PAD.decode(proof).unwrap())),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
    #[test]
    fn proof_and_ids_are_canonical_and_bounded() {
        assert!(request_id("abcdefghijklmnopqrstuvwx"));
        assert!(!request_id("abcdefghijklmnopqrstuvw/"));
        assert!(!proof_valid(&"a".repeat(43)));
        assert!(!proof_valid(&format!(
            "{}=",
            URL_SAFE_NO_PAD.encode([0u8; 32])
        )));
        assert!(proof_valid(&URL_SAFE_NO_PAD.encode([0u8; 32])));
    }
    #[test]
    fn ui_snapshot_contains_no_proof_or_capability() {
        let state = UpdateState::default();
        let snapshot = state.snapshot().to_string();
        for name in [
            "codeVerifier",
            "downloadAuthorization",
            "verifier",
            "grant",
            "envelope",
        ] {
            assert!(!snapshot.contains(name));
        }
        state.set("awaiting_approval","Compare your code.",json!({"verification_code":"ABCD-EF01","verification_uri":"https://deloresai.com/native-updates/abcdefghijklmnopqrstuvwx"}));
        assert_eq!(state.snapshot()["verification_code"], "ABCD-EF01");
    }
    #[test]
    fn authorization_lifetime_is_bounded() {
        assert!(expiry(&json!(Utc::now() - chrono::Duration::seconds(1)), 10).is_err());
        assert!(expiry(&json!(Utc::now() + chrono::Duration::minutes(11)), 10).is_err());
        assert!(expiry(&json!(Utc::now() + chrono::Duration::minutes(9)), 10).is_ok());
    }
    #[tokio::test]
    async fn real_http_transfer_checks_status_length_and_digest() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
        };
        for case in 0..4 {
            let body = if case == 2 {
                b"wrong".to_vec()
            } else {
                b"right".to_vec()
            };
            let server = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = server.local_addr().unwrap();
            let writer = std::thread::spawn(move || {
                let (mut connection, _) = server.accept().unwrap();
                connection
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut input = [0u8; 2048];
                let _ = connection.read(&mut input);
                let status = if case == 3 { "302 Found" } else { "200 OK" };
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                connection.write_all(header.as_bytes()).unwrap();
                let _ = connection.write_all(&body);
            });
            let a = Artifact {
                raw: json!({}),
                id: "fixture".into(),
                version: "0.3.0".into(),
                size: if case == 1 { 6 } else { 5 },
                sha256: verify::digest(b"right"),
            };
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("partial");
            let response = client()
                .unwrap()
                .get(format!("http://{address}/fixture"))
                .send()
                .await
                .unwrap();
            let result = receive(response, &a, &path, &UpdateState::default()).await;
            assert_eq!(result.is_ok(), case == 0, "case {case}");
            if case == 0 {
                assert_eq!(std::fs::read(&path).unwrap(), b"right");
            }
            writer.join().unwrap();
        }
    }
}
