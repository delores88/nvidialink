//! DGXBridge's deliberately narrow, locally anchored Marketplace update authority.
//! Adapted from DeloresAI metadata.rs/review.rs; see docs/UPDATER-PROVENANCE.md.
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

pub type Result<T> = std::result::Result<T, String>;
pub const ORIGIN: &str = "https://deloresai.com";
pub const APP: &str = "dgxbridge";
pub const PUBLISHER: &str = "idFUpIv8h0zRX8OxpjsAWgQk";
pub const REPOSITORY: u64 = 1418188832;
const POLICY: &str = "2026-09-10.release.1";
const REQUIRED: [&str; 8] = [
    "file.identity",
    "package.identity",
    "publisher.authorization",
    "release.source-binding",
    "malware.scan",
    "artifact.sbom",
    "source.dependencies",
    "source.secrets",
];
const INVALID: &str =
    "The signed Marketplace approval is invalid or does not authorize this DGXBridge installer.";

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Envelope {
    pub payload: String,
    pub signature: String,
    pub key_id: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Metadata {
    schema_version: u32,
    sequence: u64,
    issued_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    artifact: Value,
    purpose: String,
    protocol_version: u32,
    release_review: Envelope,
    #[serde(default)]
    install: Option<Value>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReviewPayload {
    purpose: String,
    key_id: String,
    report: Value,
}
#[derive(Clone)]
pub struct Artifact {
    pub raw: Value,
    pub id: String,
    pub version: String,
    pub size: u64,
    pub sha256: String,
}
#[derive(Clone)]
pub struct Verified {
    pub artifact: Artifact,
    pub envelope: Envelope,
    pub expires_at: DateTime<Utc>,
    pub sequence: u64,
    pub payload_digest: String,
    pub header_offset: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Watermark {
    sequence: u64,
    payload_digest: String,
    observed_at: DateTime<Utc>,
}

pub fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub fn safe_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
pub fn sha(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn text(v: &Value) -> Result<&str> {
    v.as_str().ok_or_else(|| INVALID.into())
}
fn bounded(v: &Value, n: usize) -> bool {
    v.as_str()
        .is_some_and(|s| !s.trim().is_empty() && s.len() <= n)
}
fn timestamp(v: &Value) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text(v)?)
        .map(|x| x.with_timezone(&Utc))
        .map_err(|_| INVALID.into())
}
fn recent(value: &Value, at: DateTime<Utc>, days: i64) -> bool {
    timestamp(value).is_ok_and(|t| t <= at && at - t <= chrono::Duration::days(days))
}
fn require(ok: bool) -> Result<()> {
    if ok { Ok(()) } else { Err(INVALID.into()) }
}

pub fn artifact(value: &Value) -> Result<Artifact> {
    let id = text(&value["id"])?;
    let version = text(&value["version"])?;
    let parsed = semver::Version::parse(version).map_err(|_| INVALID.to_owned())?;
    let hash = text(&value["sha256"])?;
    let size = value["size"].as_u64().ok_or(INVALID)?;
    require(
        safe_id(id)
            && value["appId"] == APP
            && value["publisherId"] == PUBLISHER
            && value["repositoryId"] == REPOSITORY
            && value["status"] == "accepted"
            && value["revoked"] == false
            && value["channel"] == "stable"
            && parsed.pre.is_empty()
            && parsed.build.is_empty()
            && parsed.to_string() == version
            && value["os"] == "windows"
            && value["architectures"] == json!(["x64"])
            && value["format"] == "nsis"
            && value["installationAdapter"] == "windows-nsis"
            && value["verificationLevel"] == "release-baseline"
            && value["verificationMethod"] == "automated-static"
            && value["policyVersion"] == POLICY
            && value["acceptancePolicy"] == "immutable-release"
            && size > 0
            && size <= 2_000_000_000
            && sha(hash),
    )?;
    require(value["sourceCommit"].as_str().is_some_and(|s| {
        s.len() == 40
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }))?;
    for key in ["releaseId", "assetId"] {
        require(
            value[key]
                .as_u64()
                .is_some_and(|x| x > 0 && x <= 9_007_199_254_740_991),
        )?;
    }
    let filename = text(&value["filename"])?;
    require(filename == format!("DGXBridge_{version}_x64-setup.exe"))?;
    // The upstream URL is identity evidence only. The client never fetches it.
    let expected =
        format!("https://github.com/delores88/nvidialink/releases/download/v{version}/{filename}");
    require(value["url"] == expected)?;
    Ok(Artifact {
        raw: value.clone(),
        id: id.into(),
        version: version.into(),
        size,
        sha256: hash.into(),
    })
}
pub fn select(catalog: &Value, current: &str) -> Result<Option<Artifact>> {
    require(catalog["app"]["id"] == APP && catalog["app"]["slug"] == APP)?;
    let current = semver::Version::parse(current)
        .map_err(|_| "The installed version cannot be compared safely.")?;
    let list = catalog["artifacts"].as_array().ok_or(INVALID)?;
    let mut found: Option<(semver::Version, Artifact)> = None;
    for raw in list {
        if let Ok(a) = artifact(raw) {
            let v = semver::Version::parse(&a.version).map_err(|_| INVALID)?;
            if v > current && found.as_ref().is_none_or(|(old, _)| v > *old) {
                found = Some((v, a));
            }
        }
    }
    Ok(found.map(|(_, a)| a))
}
fn decode(envelope: &Envelope, keys: &BTreeMap<String, String>) -> Result<Vec<u8>> {
    require(
        envelope.payload.len() <= 262_144
            && envelope.signature.len() <= 100
            && envelope.key_id.len() <= 128,
    )?;
    let encoded=keys.get(&envelope.key_id).ok_or("Marketplace signing key is not trusted by this DGXBridge build. Install a trusted release; verification cannot be bypassed.")?;
    let key: [u8; 32] = STANDARD
        .decode(encoded)
        .map_err(|_| INVALID)?
        .try_into()
        .map_err(|_| INVALID)?;
    let key = VerifyingKey::from_bytes(&key).map_err(|_| INVALID)?;
    let bytes = STANDARD.decode(&envelope.payload).map_err(|_| INVALID)?;
    let sig = Signature::from_slice(&STANDARD.decode(&envelope.signature).map_err(|_| INVALID)?)
        .map_err(|_| INVALID)?;
    key.verify_strict(&bytes, &sig).map_err(|_| INVALID)?;
    require(
        STANDARD.encode(&bytes) == envelope.payload
            && STANDARD.encode(sig.to_bytes()) == envelope.signature,
    )?;
    Ok(bytes)
}
pub fn verify(envelope: &Envelope, expected: &Artifact, now: DateTime<Utc>) -> Result<Verified> {
    let acquisition =
        serde_json::from_str(include_str!("update-acquisition-keys.json")).map_err(|_| INVALID)?;
    let review =
        serde_json::from_str(include_str!("update-review-keys.json")).map_err(|_| INVALID)?;
    verify_with_keys(envelope, expected, now, &acquisition, &review)
}
fn verify_with_keys(
    envelope: &Envelope,
    expected: &Artifact,
    now: DateTime<Utc>,
    acquisition: &BTreeMap<String, String>,
    review: &BTreeMap<String, String>,
) -> Result<Verified> {
    let bytes = decode(envelope, acquisition)?;
    let m: Metadata = serde_json::from_slice(&bytes).map_err(|_| INVALID)?;
    require(
        m.schema_version == 1
            && m.sequence > 0
            && m.purpose == "deloresai-native-acquisition"
            && m.protocol_version == 2
            && m.install.is_none()
            && m.issued_at <= now + chrono::Duration::seconds(30)
            && m.expires_at > now
            && m.expires_at > m.issued_at
            && m.expires_at - m.issued_at <= chrono::Duration::minutes(10)
            && m.artifact == expected.raw,
    )?;
    let a = artifact(&m.artifact)?;
    let r: ReviewPayload =
        serde_json::from_slice(&decode(&m.release_review, review)?).map_err(|_| INVALID)?;
    require(r.purpose == "deloresai-release-review" && r.key_id == m.release_review.key_id)?;
    let r = &r.report;
    let v = &a.raw;
    let binding = json!({"appId":APP,"publisherId":PUBLISHER,"repositoryId":REPOSITORY,"sourceCommit":v["sourceCommit"],"releaseId":v["releaseId"],"assetId":v["assetId"],"sha256":a.sha256,"size":a.size,"os":"windows","architectures":["x64"],"format":"nsis"});
    let verified = timestamp(&r["verifiedAt"])?;
    let expired = timestamp(&r["expiresAt"])?;
    require(
        r["schemaVersion"] == 1
            && r["policyVersion"] == POLICY
            && r["verificationMethod"] == "automated-static"
            && r["binding"] == binding
            && r["manifestSha256"] == v["manifestSha256"]
            && sha(text(&r["manifestSha256"])?)
            && timestamp(&v["acceptedAt"])? == verified
            && timestamp(&v["expiresAt"])? == expired
            && verified <= now + chrono::Duration::seconds(30)
            && expired > verified
            && expired - verified <= chrono::Duration::days(7)
            && matches!(text(&r["osSigning"])?, "unsigned-disclosed" | "verified")
            && v["signing"] == r["osSigning"],
    )?;
    // Protocol 2 retains immutable approval, including truthful historical scan dates.
    let e = &r["evidence"];
    let p = &e["package"];
    let malware = &e["malware"];
    let offset = p["headerOffset"].as_u64().ok_or(INVALID)?;
    require(
        e["sha256"] == a.sha256
            && e["size"] == a.size
            && p["format"] == "nsis"
            && p["architecture"] == "x64"
            && sha(text(&p["payloadSha256"])?)
            && offset < a.size
            && offset < 16 * 1024 * 1024
            && e["rights"]["freeUse"] == true
            && e["rights"]["marketplaceDistribution"] == true
            && e["rights"]["sourceVisibility"] == "public"
            && v["sourceVisibility"].as_str().unwrap_or("public") == "public"
            && malware["engine"] == "ClamAV"
            && bounded(&malware["engineVersion"], 80)
            && bounded(&malware["signatureVersion"], 80)
            && malware["result"] == "no-threats-found"
            && malware["exitCode"] == 0
            && recent(&malware["scannedAt"], verified, 1)
            && recent(
                &malware["signatureUpdatedAt"],
                timestamp(&malware["scannedAt"])?,
                3,
            ),
    )?;
    let auto = &r["automation"];
    require(
        bounded(&auto["jobId"], 128)
            && bounded(&auto["executionId"], 1000)
            && sha(text(&auto["nonce"])?)
            && sha(text(&auto["reportSha256"])?)
            && sha(text(&auto["sourceArchiveSha256"])?)
            && (auto["imageDigest"].is_null() || sha(text(&auto["imageDigest"])?)),
    )?;
    let tools = auto["tools"].as_array().ok_or(INVALID)?;
    require(tools.len() == 4)?;
    let mut seen = BTreeSet::new();
    for tool in tools {
        let name = text(&tool["tool"])?;
        require(
            ["clamscan", "syft", "grype", "gitleaks"].contains(&name)
                && seen.insert(name)
                && bounded(&tool["version"], 100)
                && sha(text(&tool["reportSha256"])?)
                && (!["clamscan", "grype"].contains(&name)
                    || recent(
                        &tool["databaseUpdatedAt"],
                        timestamp(&malware["scannedAt"])?,
                        3,
                    )),
        )?;
    }
    let checks = r["checks"].as_array().ok_or(INVALID)?;
    require((8..=30).contains(&checks.len()))?;
    let mut seen = BTreeMap::new();
    for c in checks {
        let id = text(&c["id"])?;
        let status = text(&c["status"])?;
        require(
            bounded(&c["id"], 120)
                && bounded(&c["label"], 180)
                && bounded(&c["summary"], 2000)
                && ["passed", "not-run", "not-required"].contains(&status)
                && seen.insert(id, status).is_none(),
        )?;
    }
    require(REQUIRED.iter().all(|id| seen.get(id) == Some(&"passed")))?;
    Ok(Verified {
        artifact: a,
        envelope: envelope.clone(),
        expires_at: m.expires_at,
        sequence: m.sequence,
        payload_digest: digest(&bytes),
        header_offset: offset,
    })
}
pub fn persist(verified: &Verified, root: &Path, now: DateTime<Utc>) -> Result<()> {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(root.join("trust.lock"))
        .map_err(|_| "Update trust storage is unavailable.")?;
    lock.lock().map_err(|_| "Update trust storage is busy.")?;
    let path = root.join("trust-stable.json");
    if path.exists() {
        require(std::fs::metadata(&path).map_err(|_| INVALID)?.len() < 8192)?;
        let old: Watermark = serde_json::from_slice(&std::fs::read(&path).map_err(|_| INVALID)?)
            .map_err(|_| INVALID)?;
        require(
            verified.sequence >= old.sequence
                && (verified.sequence != old.sequence
                    || verified.payload_digest == old.payload_digest)
                && now >= old.observed_at - chrono::Duration::minutes(5),
        )?;
    }
    sparkdev::state::atomic_json(
        &path,
        &Watermark {
            sequence: verified.sequence,
            payload_digest: verified.payload_digest.clone(),
            observed_at: now,
        },
    )
    .map_err(|_| "Update trust state could not be saved.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    fn sign(value: &Value, key: &SigningKey, id: &str) -> Envelope {
        let bytes = serde_json::to_vec(value).unwrap();
        Envelope {
            payload: STANDARD.encode(&bytes),
            signature: STANDARD.encode(key.sign(&bytes).to_bytes()),
            key_id: id.into(),
        }
    }
    fn fixture() -> (Artifact, Value, SigningKey, SigningKey, DateTime<Utc>) {
        // Explicit deterministic TEST signing material; never used by production authority maps.
        let outer = SigningKey::from_bytes(&[11; 32]);
        let inner = SigningKey::from_bytes(&[22; 32]);
        let now = Utc::now();
        let verified = now - chrono::Duration::days(8);
        let expires = verified + chrono::Duration::days(7);
        let hash = "0".repeat(64);
        let version = "0.3.0";
        let a = json!({"id":"fixture_artifact","appId":APP,"publisherId":PUBLISHER,"repositoryId":REPOSITORY,"releaseId":1,"assetId":2,"sourceCommit":"0".repeat(40),"manifestSha256":hash,
            "filename":"DGXBridge_0.3.0_x64-setup.exe","url":"https://github.com/delores88/nvidialink/releases/download/v0.3.0/DGXBridge_0.3.0_x64-setup.exe",
            "version":version,"channel":"stable","os":"windows","architectures":["x64"],"format":"nsis","installationAdapter":"windows-nsis","size":1024,"sha256":hash,
            "status":"accepted","revoked":false,"signing":"unsigned-disclosed","verificationLevel":"release-baseline","verificationMethod":"automated-static","policyVersion":POLICY,"acceptancePolicy":"immutable-release","sourceVisibility":"public","acceptedAt":verified,"expiresAt":expires});
        let checks:Vec<_>=REQUIRED.iter().map(|id|json!({"id":id,"label":"fixture check","status":"passed","summary":"Synthetic verification fixture"})).collect();
        let tools:Vec<_>=["clamscan","syft","grype","gitleaks"].iter().map(|name|json!({"tool":name,"version":"test","reportSha256":hash,"databaseUpdatedAt":verified})).collect();
        let report = json!({"schemaVersion":1,"policyVersion":POLICY,"binding":{"appId":APP,"publisherId":PUBLISHER,"repositoryId":REPOSITORY,"releaseId":1,"assetId":2,"sourceCommit":"0".repeat(40),"sha256":hash,"size":1024,"os":"windows","architectures":["x64"],"format":"nsis"},
            "manifestSha256":hash,"verifiedAt":verified,"expiresAt":expires,"verificationMethod":"automated-static","osSigning":"unsigned-disclosed","checks":checks,
            "automation":{"jobId":"fixture","executionId":"fixture","nonce":hash,"reportSha256":hash,"sourceArchiveSha256":hash,"tools":tools},
            "evidence":{"sha256":hash,"size":1024,"package":{"format":"nsis","architecture":"x64","headerOffset":32,"payloadSha256":hash},"rights":{"freeUse":true,"marketplaceDistribution":true,"sourceVisibility":"public"},
            "malware":{"engine":"ClamAV","engineVersion":"test","signatureVersion":"test","signatureUpdatedAt":verified,"scannedAt":verified,"result":"no-threats-found","exitCode":0}}});
        let review = sign(
            &json!({"purpose":"deloresai-release-review","keyId":"test-review","report":report}),
            &inner,
            "test-review",
        );
        let metadata = json!({"schemaVersion":1,"sequence":5,"issuedAt":now,"expiresAt":now+chrono::Duration::minutes(5),"artifact":a,"purpose":"deloresai-native-acquisition","protocolVersion":2,"releaseReview":review});
        (artifact(&a).unwrap(), metadata, outer, inner, now)
    }
    fn maps(
        outer: &SigningKey,
        inner: &SigningKey,
    ) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
        (
            BTreeMap::from([(
                "test-acquisition".into(),
                STANDARD.encode(outer.verifying_key().to_bytes()),
            )]),
            BTreeMap::from([(
                "test-review".into(),
                STANDARD.encode(inner.verifying_key().to_bytes()),
            )]),
        )
    }
    #[test]
    fn authenticates_both_purposes_and_retains_historical_immutable_review() {
        let (a, m, k, r, now) = fixture();
        let (outer, inner) = maps(&k, &r);
        let e = sign(&m, &k, "test-acquisition");
        assert!(verify_with_keys(&e, &a, now, &outer, &inner).is_ok());
        assert!(verify_with_keys(&e, &a, now, &BTreeMap::new(), &inner).is_err());
        assert!(verify_with_keys(&e, &a, now, &outer, &BTreeMap::new()).is_err());
        assert!(verify_with_keys(&e, &a, now, &inner, &outer).is_err());
    }
    #[test]
    fn rejects_tampered_payload_expired_grant_and_full_install_claim() {
        let (a, m, k, r, now) = fixture();
        let (outer, inner) = maps(&k, &r);
        for (field, value) in [
            ("purpose", json!("other")),
            ("protocolVersion", json!(1)),
            ("install", json!({"automaticUpdateSafe":true})),
            ("expiresAt", json!(now - chrono::Duration::seconds(1))),
        ] {
            let mut bad = m.clone();
            bad[field] = value;
            assert!(
                verify_with_keys(&sign(&bad, &k, "test-acquisition"), &a, now, &outer, &inner)
                    .is_err(),
                "{field}"
            );
        }
        let mut e = sign(&m, &k, "test-acquisition");
        e.payload.replace_range(..1, "A");
        assert!(verify_with_keys(&e, &a, now, &outer, &inner).is_err());
    }
    #[test]
    fn rejects_correctly_signed_wrong_nested_binding_and_failed_check() {
        let (a, m, k, r, now) = fixture();
        let (outer, inner) = maps(&k, &r);
        for failure in ["binding", "check", "purpose"] {
            let nested: Envelope = serde_json::from_value(m["releaseReview"].clone()).unwrap();
            let mut payload: Value =
                serde_json::from_slice(&STANDARD.decode(nested.payload).unwrap()).unwrap();
            match failure {
                "binding" => payload["report"]["binding"]["assetId"] = json!(9),
                "check" => payload["report"]["checks"][0]["status"] = json!("not-run"),
                _ => payload["purpose"] = json!("native-full-install"),
            };
            let mut bad = m.clone();
            bad["releaseReview"] = serde_json::to_value(sign(&payload, &r, "test-review")).unwrap();
            assert!(
                verify_with_keys(&sign(&bad, &k, "test-acquisition"), &a, now, &outer, &inner)
                    .is_err()
            );
        }
    }
    #[test]
    fn pins_app_publisher_repo_platform_version_and_url() {
        let (a, _, _, _, _) = fixture();
        for (field, value) in [
            ("appId", json!("other")),
            ("publisherId", json!("other")),
            ("repositoryId", json!(2)),
            ("architectures", json!(["arm64"])),
            ("channel", json!("beta")),
            ("version", json!("0.3.0+changed")),
            ("url", json!("https://example.com/setup.exe")),
            ("revoked", json!(true)),
        ] {
            let mut bad = a.raw.clone();
            bad[field] = value;
            assert!(artifact(&bad).is_err(), "{field}");
        }
    }
    #[test]
    fn persists_replay_watermark_and_rejects_sequence_rollback_or_clock_rollback() {
        let (a, m, k, r, now) = fixture();
        let (outer, inner) = maps(&k, &r);
        let mut v =
            verify_with_keys(&sign(&m, &k, "test-acquisition"), &a, now, &outer, &inner).unwrap();
        let dir = tempfile::tempdir().unwrap();
        persist(&v, dir.path(), now).unwrap();
        persist(&v, dir.path(), now).unwrap();
        v.sequence -= 1;
        assert!(persist(&v, dir.path(), now).is_err());
        v.sequence += 1;
        assert!(persist(&v, dir.path(), now - chrono::Duration::minutes(6)).is_err());
        v.payload_digest = "f".repeat(64);
        assert!(persist(&v, dir.path(), now).is_err());
    }
    #[test]
    fn discovery_never_downgrades_or_accepts_beta() {
        let (a, _, _, _, _) = fixture();
        let catalog = json!({"app":{"id":APP,"slug":APP},"artifacts":[a.raw]});
        assert!(select(&catalog, "0.2.0").unwrap().is_some());
        assert!(select(&catalog, "0.3.0").unwrap().is_none());
        assert!(select(&catalog, "0.4.0").unwrap().is_none());
    }
}
