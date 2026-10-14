//! Length-prefixed frames on QUIC streams: a `u32` big-endian length, then the payload.

use iroh::endpoint::{ReadExactError, RecvStream, SendStream, WriteError};
use serde::{Serialize, de::DeserializeOwned};

use nuee_proto::ProtoError;

/// A frame could not be written or read.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// The announced or actual length is above the cap.
    #[error("frame of {len} bytes exceeds the cap of {max}")]
    TooLarge {
        /// Announced length.
        len: usize,
        /// Allowed maximum.
        max: usize,
    },
    /// The stream ended cleanly before a new frame started.
    #[error("stream closed")]
    Closed,
    /// The stream ended in the middle of a frame.
    #[error("stream ended in the middle of a frame")]
    Truncated,
    /// Reading failed.
    #[error("read failed: {0}")]
    Read(#[source] iroh::endpoint::ReadError),
    /// Writing failed.
    #[error("write failed: {0}")]
    Write(#[from] WriteError),
    /// The payload is not a valid message.
    #[error(transparent)]
    Proto(#[from] ProtoError),
}

fn map_read(e: ReadExactError, started: bool) -> FrameError {
    match e {
        ReadExactError::FinishedEarly(0) if !started => FrameError::Closed,
        ReadExactError::FinishedEarly(_) => FrameError::Truncated,
        ReadExactError::ReadError(e) => FrameError::Read(e),
    }
}

/// Writes one frame; `payload` longer than `u32::MAX` is refused.
pub async fn write_frame(send: &mut SendStream, payload: &[u8]) -> Result<(), FrameError> {
    let len = u32::try_from(payload.len()).map_err(|_| FrameError::TooLarge {
        len: payload.len(),
        max: u32::MAX as usize,
    })?;
    send.write_all(&len.to_be_bytes()).await?;
    send.write_all(payload).await?;
    Ok(())
}

/// Reads one frame of at most `max` bytes. The length is checked before any allocation.
pub async fn read_frame(recv: &mut RecvStream, max: usize) -> Result<Vec<u8>, FrameError> {
    let mut head = [0u8; 4];
    recv.read_exact(&mut head)
        .await
        .map_err(|e| map_read(e, false))?;
    let len = u32::from_be_bytes(head) as usize;
    if len > max {
        return Err(FrameError::TooLarge { len, max });
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf)
        .await
        .map_err(|e| map_read(e, true))?;
    Ok(buf)
}

/// Encodes `msg` with postcard and writes it as one frame.
pub async fn send_msg<T: Serialize>(send: &mut SendStream, msg: &T) -> Result<(), FrameError> {
    let bytes = nuee_proto::encode(msg)?;
    write_frame(send, &bytes).await
}

/// Reads one frame of at most `max` bytes and decodes it.
pub async fn recv_msg<T: DeserializeOwned>(
    recv: &mut RecvStream,
    max: usize,
) -> Result<T, FrameError> {
    let bytes = read_frame(recv, max).await?;
    Ok(nuee_proto::decode_with_limit(&bytes, max)?)
}

/// Splits a buffer holding whole frames, for tests and in-memory transports.
pub fn split_frames(mut buf: &[u8], max: usize) -> Result<Vec<&[u8]>, FrameError> {
    let mut out = Vec::new();
    while !buf.is_empty() {
        let (head, rest) = buf.split_first_chunk::<4>().ok_or(FrameError::Truncated)?;
        let len = u32::from_be_bytes(*head) as usize;
        if len > max {
            return Err(FrameError::TooLarge { len, max });
        }
        if rest.len() < len {
            return Err(FrameError::Truncated);
        }
        let (frame, tail) = rest.split_at(len);
        out.push(frame);
        buf = tail;
    }
    Ok(out)
}

/// Prefixes `payload` with its length.
pub fn frame_bytes(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    let len = u32::try_from(payload.len()).map_err(|_| FrameError::TooLarge {
        len: payload.len(),
        max: u32::MAX as usize,
    })?;
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let a = frame_bytes(b"hello").unwrap();
        let mut both = a.clone();
        both.extend(frame_bytes(b"").unwrap());
        let frames = split_frames(&both, 16).unwrap();
        assert_eq!(frames, vec![&b"hello"[..], &b""[..]]);
    }

    #[test]
    fn truncated() {
        let a = frame_bytes(b"hello").unwrap();
        assert!(matches!(
            split_frames(&a[..a.len() - 1], 16),
            Err(FrameError::Truncated)
        ));
        assert!(matches!(
            split_frames(&a[..2], 16),
            Err(FrameError::Truncated)
        ));
    }

    #[test]
    fn too_large() {
        let a = frame_bytes(&[0u8; 17]).unwrap();
        assert!(matches!(
            split_frames(&a, 16),
            Err(FrameError::TooLarge { len: 17, max: 16 })
        ));
        let huge = u32::MAX.to_be_bytes();
        assert!(matches!(
            split_frames(&huge, 16),
            Err(FrameError::TooLarge { .. })
        ));
    }
}
