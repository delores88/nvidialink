//! Compute-job transfers and events (0.25 S3, capability `jobs.v2`), in one dependency-light file.
//!
//! - **Content-addressed, resumable uploads.** A declared input is stored by its SHA-256. Each
//!   upload chunk carries its own SHA-256, is verified before it is appended, and the node reports
//!   how many bytes of each input it holds (`InputProgress.received`), so a dropped upload resumes
//!   from the last verified chunk. An input the node already holds (same hash) is not sent again.
//! - **Hashed once, chunk-addressed outputs.** The node hashes each output once, as it is written,
//!   and keeps a manifest: whole-file SHA-256 plus one SHA-256 per [`CHUNK_BYTES`] chunk. Output
//!   reads are chunk-aligned and carry the chunk's hash, so a client verifies every chunk as it
//!   arrives and resumes a dropped download from its last verified chunk.
//! - **Events.** The node keeps a bounded per-job event log (`state`, `progress`, `log`) with a
//!   sequence number and a per-process `epoch`; clients long-poll it with a cursor, so a reconnect
//!   never replays or skips silently (`dropped` counts evicted events).
//!
//! `spark-link-node` implements this; PhaseForge's backend compiles this file by path for its
//! client and test mock. Keep it self-contained: `std`, `serde`, `serde_json` and `sha2` only, and
//! code that builds under Rust 2021 and 2024.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Upload and output chunk size of `jobs.v2`. Base64 of one chunk plus its JSON envelope stays
/// under the 6 MiB job body and the 8 MiB IPC frame.
pub const CHUNK_BYTES: u64 = 4 * 1024 * 1024;
/// The 0.21 chunk size (`jobs` without `jobs.v2`).
pub const CHUNK_BYTES_V1: u64 = 1024 * 1024;
/// Longest a node holds an events request open before answering with no events.
pub const EVENT_WAIT_MAX_MS: u64 = 25_000;
/// Events retained per job; older ones are dropped and counted.
pub const EVENT_LOG_LIMIT: usize = 1024;
/// Longest log line kept in an event, in characters.
pub const LOG_LINE_MAX: usize = 500;
/// Log lines kept in a job's status (`log_tail`).
pub const LOG_TAIL_LINES: usize = 20;

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 15) as usize] as char);
    }
    out
}

/// Number of chunks of a file of `size` bytes (an empty file has none).
pub fn chunk_count(size: u64, chunk: u64) -> u64 {
    size.div_ceil(chunk)
}

/// Byte range `[start, end)` of chunk `index`.
pub fn chunk_range(size: u64, index: u64, chunk: u64) -> (u64, u64) {
    let start = index.saturating_mul(chunk).min(size);
    (start, start.saturating_add(chunk).min(size))
}

/// Hashes a file once as it is written: the whole-file SHA-256 and one per chunk.
pub struct ChunkHasher {
    chunk: u64,
    whole: Sha256,
    current: Sha256,
    filled: u64,
    size: u64,
    chunks: Vec<String>,
}

impl ChunkHasher {
    pub fn new(chunk: u64) -> Self {
        Self { chunk: chunk.max(1), whole: Sha256::new(), current: Sha256::new(), filled: 0, size: 0, chunks: Vec::new() }
    }

    pub fn update(&mut self, mut bytes: &[u8]) {
        self.whole.update(bytes);
        self.size += bytes.len() as u64;
        while !bytes.is_empty() {
            let room = (self.chunk - self.filled) as usize;
            let take = room.min(bytes.len());
            self.current.update(&bytes[..take]);
            self.filled += take as u64;
            bytes = &bytes[take..];
            if self.filled == self.chunk {
                let done = std::mem::replace(&mut self.current, Sha256::new());
                self.chunks.push(hex(&done.finalize()));
                self.filled = 0;
            }
        }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn finish(mut self, name: &str) -> FileManifest {
        if self.filled > 0 {
            let done = std::mem::replace(&mut self.current, Sha256::new());
            self.chunks.push(hex(&done.finalize()));
        }
        FileManifest { name: name.to_string(), size: self.size, sha256: hex(&self.whole.finalize()), chunk_bytes: self.chunk, chunks: self.chunks }
    }
}

/// One output as the node hashed it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileManifest {
    pub name: String,
    pub size: u64,
    pub sha256: String,
    pub chunk_bytes: u64,
    /// SHA-256 of each chunk, in order.
    pub chunks: Vec<String>,
}

impl FileManifest {
    /// The manifest of bytes already in memory.
    pub fn of(name: &str, bytes: &[u8], chunk: u64) -> Self {
        let mut hasher = ChunkHasher::new(chunk);
        hasher.update(bytes);
        hasher.finish(name)
    }

    /// The expected hash of the chunk starting at `offset`, when `offset` is chunk-aligned.
    pub fn chunk_at(&self, offset: u64) -> Option<&str> {
        if self.chunk_bytes == 0 || offset % self.chunk_bytes != 0 {
            return None;
        }
        self.chunks.get((offset / self.chunk_bytes) as usize).map(String::as_str)
    }

    /// Internal consistency: chunk count and size agree.
    pub fn consistent(&self) -> bool {
        self.chunk_bytes > 0 && self.chunks.len() as u64 == chunk_count(self.size, self.chunk_bytes) && self.sha256.len() == 64
    }
}

/// `GET /link/v1/jobs/{id}/manifest`: the verified outputs of a succeeded job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputManifest {
    pub job_id: String,
    pub files: Vec<FileManifest>,
}

/// How much of one declared input the node holds (in `JobStatus.inputs`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputProgress {
    pub name: String,
    pub size: u64,
    pub sha256: String,
    /// Verified bytes held; the next upload offset.
    pub received: u64,
    pub complete: bool,
}

/// `POST /link/v1/jobs/{id}/outputs/{name}` answer (`jobs.v2` adds `chunk_sha256`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputChunk {
    pub offset: u64,
    pub size: u64,
    pub complete: bool,
    /// Base64 of the chunk's bytes.
    pub data: String,
    /// SHA-256 of these bytes as recorded when the output was written (chunk-aligned reads).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_sha256: Option<String>,
}

/// One entry of a job's event log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobEvent {
    pub seq: u64,
    /// `state`, `progress` or `log`.
    pub kind: String,
    /// Unix seconds.
    pub at: u64,
    pub data: serde_json::Value,
}

/// `POST /link/v1/jobs/{id}/events` body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsRequest {
    /// Return events with `seq > after`.
    #[serde(default)]
    pub after: u64,
    /// The epoch `after` belongs to; a different epoch (the node restarted) starts from 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<String>,
    /// Hold the request up to this long (capped at [`EVENT_WAIT_MAX_MS`]) when nothing is new.
    #[serde(default)]
    pub wait_ms: u64,
}

/// The events answer: what is new after the cursor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventBatch {
    pub job_id: String,
    /// Changes when the node process restarts; cursors from another epoch start over.
    pub epoch: String,
    pub events: Vec<JobEvent>,
    /// Cursor for the next request (the last returned `seq`, or the request's `after`).
    pub next: u64,
    /// Events that were evicted before this client read them.
    #[serde(default)]
    pub dropped: u64,
    /// The job is finished; no further events will follow.
    #[serde(default)]
    pub terminal: bool,
}

/// A progress line a worker prints on stdout: `PHASEFORGE_PROGRESS {"fraction":0.5,"message":"..."}`.
pub const PROGRESS_PREFIX: &str = "PHASEFORGE_PROGRESS ";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_hashes_match_hashing_each_range_and_survive_any_write_split() {
        let data: Vec<u8> = (0..(CHUNK_BYTES * 2 + 17) as usize).map(|i| (i * 31 % 256) as u8).collect();
        let whole = FileManifest::of("x", &data, CHUNK_BYTES);
        assert!(whole.consistent());
        assert_eq!(whole.chunks.len(), 3);
        for (i, h) in whole.chunks.iter().enumerate() {
            let (a, b) = chunk_range(data.len() as u64, i as u64, CHUNK_BYTES);
            assert_eq!(*h, sha256_hex(&data[a as usize..b as usize]));
        }
        assert_eq!(whole.sha256, sha256_hex(&data));
        let mut split = ChunkHasher::new(CHUNK_BYTES);
        for part in data.chunks(777_777) {
            split.update(part);
        }
        assert_eq!(split.finish("x"), whole);
        assert_eq!(whole.chunk_at(CHUNK_BYTES), Some(whole.chunks[1].as_str()));
        assert_eq!(whole.chunk_at(5), None);
        let empty = FileManifest::of("e", b"", CHUNK_BYTES);
        assert!(empty.consistent() && empty.chunks.is_empty() && empty.sha256 == sha256_hex(b""));
    }

    #[test]
    fn events_requests_are_closed_and_small() {
        assert!(serde_json::from_value::<EventsRequest>(serde_json::json!({"after": 1, "x": 2})).is_err());
        let r: EventsRequest = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(r, EventsRequest { after: 0, epoch: None, wait_ms: 0 });
    }
}
