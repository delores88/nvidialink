//! Length-prefixed JSON frames, used on the pairing ALPN and on the agent IPC.
//!
//! ```text
//! frame = length(u32, big-endian) || payload(length bytes of UTF-8 JSON, one object)
//! ```
//! `length` must be 1..=max (max is [`crate::pairing::PAIR_FRAME_MAX`] on the
//! pairing ALPN and [`crate::ipc::IPC_FRAME_MAX`] on the IPC). A peer that sends
//! an oversize or malformed frame is disconnected.

use serde::Serialize;
use serde::de::DeserializeOwned;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame length {0} outside 1..={1}")]
    BadLength(usize, usize),
    #[error("frame payload is not valid JSON for this message: {0}")]
    Json(#[from] serde_json::Error),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
}

/// Encode `msg` as one frame.
pub fn encode<T: Serialize>(msg: &T, max: usize) -> Result<Vec<u8>, FrameError> {
    let payload = serde_json::to_vec(msg)?;
    if payload.is_empty() || payload.len() > max {
        return Err(FrameError::BadLength(payload.len(), max));
    }
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Validate a length header.
pub fn check_len(header: [u8; 4], max: usize) -> Result<usize, FrameError> {
    let n = u32::from_be_bytes(header) as usize;
    if n == 0 || n > max {
        return Err(FrameError::BadLength(n, max));
    }
    Ok(n)
}

/// Decode a payload (without its header).
pub fn decode<T: DeserializeOwned>(payload: &[u8]) -> Result<T, FrameError> {
    Ok(serde_json::from_slice(payload)?)
}

#[cfg(feature = "io")]
mod aio {
    use super::*;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    /// Read one frame. Returns `Ok(None)` on a clean EOF before any header byte.
    pub async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(r: &mut R, max: usize) -> Result<Option<T>, FrameError> {
        let mut header = [0u8; 4];
        let mut got = 0;
        while got < 4 {
            let n = r.read(&mut header[got..]).await?;
            if n == 0 {
                if got == 0 {
                    return Ok(None);
                }
                return Err(FrameError::Io(std::io::ErrorKind::UnexpectedEof.into()));
            }
            got += n;
        }
        let len = check_len(header, max)?;
        let mut payload = vec![0u8; len];
        r.read_exact(&mut payload).await?;
        decode(&payload).map(Some)
    }

    /// Write one frame and flush.
    pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T, max: usize) -> Result<(), FrameError> {
        let bytes = encode(msg, max)?;
        w.write_all(&bytes).await?;
        w.flush().await?;
        Ok(())
    }
}

#[cfg(feature = "io")]
pub use aio::{read_frame, write_frame};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode() {
        let v = serde_json::json!({"t":"hello","v":1});
        let f = encode(&v, 1024).unwrap();
        assert_eq!(&f[..4], &(f.len() as u32 - 4).to_be_bytes());
        let back: serde_json::Value = decode(&f[4..]).unwrap();
        assert_eq!(back, v);
        assert!(check_len([0, 0, 0, 0], 10).is_err());
        assert!(check_len([0, 0, 0, 11], 10).is_err());
        assert!(encode(&"x".repeat(100), 10).is_err());
    }
}
