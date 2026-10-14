//! Closed compute operations. No caller-supplied HTTP methods, URLs or headers.
//! What a node admits for `Create` lives in [`crate::job_contract`]; the `jobs.v2` transfer and
//! event shapes (chunk hashes, manifest, event log) in [`crate::job_transfer`].
use serde::{Deserialize, Serialize};
use serde_json::Value;
use crate::errors::{ApiError, ErrorCode};
use crate::ids::EndpointId;

/// The 0.21 chunk size: the default output read, and every chunk an older client sends.
pub const CHUNK_BYTES: usize = 1024 * 1024;
/// Largest upload chunk or output read of `jobs.v2` ([`crate::job_transfer::CHUNK_BYTES`]).
pub const CHUNK_BYTES_V2: usize = crate::job_transfer::CHUNK_BYTES as usize;
/// Largest job request body: one base64 `jobs.v2` chunk plus its JSON envelope.
pub const MAX_JOB_BODY: usize = 6 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobRequest {
    pub node_id: EndpointId,
    pub operation: JobOperation,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum JobOperation {
    Create { request: Value },
    Status { job_id: String },
    Start { job_id: String },
    Cancel { job_id: String },
    /// One upload chunk at `offset`. `jobs.v2`: `chunk_sha256` is the SHA-256 of these bytes,
    /// checked before they are stored; chunks up to 4 MiB.
    Input {
        job_id: String,
        name: String,
        offset: u64,
        complete: bool,
        data: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chunk_sha256: Option<String>,
    },
    /// Read an output from `offset`: at most 1 MiB, or up to `max_bytes` (at most 4 MiB) on a
    /// `jobs.v2` node, whose chunk-aligned answers carry the chunk's recorded SHA-256.
    Output {
        job_id: String,
        name: String,
        offset: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_bytes: Option<u64>,
    },
    /// The owner imported this finished job's results: the node may prune it (least recently
    /// used first) when admission needs room. Finished jobs with confirmed cleanup only.
    Ack { job_id: String },
    /// Remove this finished job and its files now. Owner only; finished jobs with confirmed
    /// cleanup only. Not a cancel: a running job must be cancelled first.
    Delete { job_id: String },
    /// `jobs.v2`: the verified outputs with their chunk hashes.
    Manifest { job_id: String },
    /// `jobs.v2`: events after the cursor, held open up to `wait_ms` when there are none.
    Events {
        job_id: String,
        #[serde(default)]
        after: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        epoch: Option<String>,
        #[serde(default)]
        wait_ms: u64,
    },
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 240 && name.split('/').count() <= 8 &&
        name.split('/').all(|p| !p.is_empty() && p != "." && p != ".." && p.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)))
}
fn valid_sha(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl JobOperation {
    pub fn job_id(&self) -> Option<&str> {
        use JobOperation::*;
        match self {
            Create { .. } => None,
            Status { job_id } | Start { job_id } | Cancel { job_id } | Input { job_id, .. } | Output { job_id, .. } | Ack { job_id } | Delete { job_id }
            | Manifest { job_id } | Events { job_id, .. } => Some(job_id),
        }
    }

    pub fn wire(&self) -> Result<(&'static str, String, Option<Value>), ApiError> {
        use JobOperation::*;
        let bad = || ApiError::new(ErrorCode::BadRequest, "invalid compute job id, file name or chunk size");
        if let Create { request } = self {
            return Ok(("POST", "/link/v1/jobs".into(), Some(request.clone())));
        }
        let id = self.job_id().expect("non-create operations name a job");
        if !valid_id(id) { return Err(bad()); }
        let base = format!("/link/v1/jobs/{id}");
        Ok(match self {
            Status {..} => ("GET", base, None),
            Start {..} => ("POST", format!("{base}/start"), Some(serde_json::json!({}))),
            Cancel {..} => ("DELETE", base, None),
            Ack {..} => ("POST", format!("{base}/ack"), Some(serde_json::json!({}))),
            Delete {..} => ("POST", format!("{base}/delete"), Some(serde_json::json!({}))),
            Manifest {..} => ("GET", format!("{base}/manifest"), None),
            Events { after, epoch, wait_ms, .. } => {
                if epoch.as_ref().is_some_and(|e| e.is_empty() || e.len() > 64 || !e.bytes().all(|b| b.is_ascii_alphanumeric())) { return Err(bad()); }
                let body = crate::job_transfer::EventsRequest { after: *after, epoch: epoch.clone(), wait_ms: (*wait_ms).min(crate::job_transfer::EVENT_WAIT_MAX_MS) };
                ("POST", format!("{base}/events"), Some(serde_json::to_value(body).map_err(|_| bad())?))
            }
            Input {name, offset, complete, data, chunk_sha256, ..} => {
                if !valid_name(name) || data.len() > CHUNK_BYTES_V2.div_ceil(3) * 4 || chunk_sha256.as_deref().is_some_and(|s| !valid_sha(s)) { return Err(bad()); }
                let mut body = serde_json::json!({"offset":offset,"complete":complete,"data":data});
                if let Some(sha) = chunk_sha256 { body["chunk_sha256"] = Value::String(sha.clone()); }
                ("PUT", format!("{base}/inputs/{name}"), Some(body))
            },
            Output {name, offset, max_bytes, ..} => {
                if !valid_name(name) || max_bytes.is_some_and(|m| m == 0 || m > CHUNK_BYTES_V2 as u64) { return Err(bad()); }
                let mut body = serde_json::json!({"offset":offset});
                if let Some(max) = max_bytes { body["max_bytes"] = Value::from(*max); }
                ("POST", format!("{base}/outputs/{name}"), Some(body))
            },
            Create {..} => unreachable!(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_operation_maps_to_one_fixed_route() {
        let id = "0123456789abcdef0123456789abcdef".to_string();
        let cases = [
            (JobOperation::Status { job_id: id.clone() }, "GET", format!("/link/v1/jobs/{id}")),
            (JobOperation::Start { job_id: id.clone() }, "POST", format!("/link/v1/jobs/{id}/start")),
            (JobOperation::Cancel { job_id: id.clone() }, "DELETE", format!("/link/v1/jobs/{id}")),
            (JobOperation::Ack { job_id: id.clone() }, "POST", format!("/link/v1/jobs/{id}/ack")),
            (JobOperation::Delete { job_id: id.clone() }, "POST", format!("/link/v1/jobs/{id}/delete")),
            (JobOperation::Manifest { job_id: id.clone() }, "GET", format!("/link/v1/jobs/{id}/manifest")),
            (JobOperation::Events { job_id: id.clone(), after: 3, epoch: None, wait_ms: 100 }, "POST", format!("/link/v1/jobs/{id}/events")),
            (JobOperation::Output { job_id: id.clone(), name: "a/b.json".into(), offset: 0, max_bytes: None }, "POST", format!("/link/v1/jobs/{id}/outputs/a/b.json")),
        ];
        for (op, method, path) in cases {
            let (m, p, _) = op.wire().unwrap();
            assert_eq!((m, p), (method, path));
        }
        for bad in ["", "../x", "ABC", &"a".repeat(65)] {
            assert!(JobOperation::Delete { job_id: bad.into() }.wire().is_err(), "{bad}");
            assert!(JobOperation::Ack { job_id: bad.into() }.wire().is_err(), "{bad}");
            assert!(JobOperation::Events { job_id: bad.into(), after: 0, epoch: None, wait_ms: 0 }.wire().is_err(), "{bad}");
        }
        // The IPC enum is closed: delete and ack are tagged actions, and unknown fields are refused.
        let parsed: JobOperation = serde_json::from_value(serde_json::json!({"action": "delete", "job_id": id})).unwrap();
        assert_eq!(parsed, JobOperation::Delete { job_id: id.clone() });
        assert!(serde_json::from_value::<JobOperation>(serde_json::json!({"action": "ack", "job_id": id, "force": true})).is_err());
        assert!(serde_json::from_value::<JobOperation>(serde_json::json!({"action": "prune"})).is_err());
    }

    /// A 0.21 client's operations parse and travel exactly as before: no chunk hash, no read size.
    #[test]
    fn version_one_bodies_are_unchanged_and_v2_fields_are_optional() {
        let id = "0123456789abcdef0123456789abcdef";
        let old: JobOperation = serde_json::from_value(serde_json::json!({"action":"input","job_id":id,"name":"x.py","offset":0,"complete":true,"data":"AA=="})).unwrap();
        assert_eq!(old.wire().unwrap().2.unwrap(), serde_json::json!({"offset":0,"complete":true,"data":"AA=="}));
        let old: JobOperation = serde_json::from_value(serde_json::json!({"action":"output","job_id":id,"name":"r.json","offset":7})).unwrap();
        assert_eq!(old.wire().unwrap().2.unwrap(), serde_json::json!({"offset":7}));
        let v2 = JobOperation::Input { job_id: id.into(), name: "x.py".into(), offset: 0, complete: false, data: "AA==".into(), chunk_sha256: Some("a".repeat(64)) };
        assert_eq!(v2.wire().unwrap().2.unwrap()["chunk_sha256"], "a".repeat(64));
        let bad = JobOperation::Input { job_id: id.into(), name: "x.py".into(), offset: 0, complete: false, data: "AA==".into(), chunk_sha256: Some("XYZ".into()) };
        assert!(bad.wire().is_err());
        assert!(JobOperation::Output { job_id: id.into(), name: "r".into(), offset: 0, max_bytes: Some(CHUNK_BYTES_V2 as u64 + 1) }.wire().is_err());
        // A 4 MiB chunk fits the job body and an IPC frame.
        let chunk = "A".repeat(CHUNK_BYTES_V2.div_ceil(3) * 4);
        let op = JobOperation::Input { job_id: id.into(), name: "x.bin".into(), offset: 0, complete: false, data: chunk, chunk_sha256: Some("a".repeat(64)) };
        let body = serde_json::to_vec(&op.wire().unwrap().2.unwrap()).unwrap();
        assert!(body.len() <= MAX_JOB_BODY && body.len() + 4096 <= crate::ipc::IPC_FRAME_MAX, "{}", body.len());
        // Events waits are capped by the contract.
        let (_, _, body) = JobOperation::Events { job_id: id.into(), after: 9, epoch: Some("abc123".into()), wait_ms: 10_000_000 }.wire().unwrap();
        assert_eq!(body.unwrap(), serde_json::json!({"after":9,"epoch":"abc123","wait_ms":crate::job_transfer::EVENT_WAIT_MAX_MS}));
    }
}
