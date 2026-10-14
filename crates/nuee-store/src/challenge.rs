//! Storage challenges: ask a holder for a few random byte ranges of a blob and verify them
//! against the blob's BLAKE3 root hash alone.
//!
//! The challenger keeps nothing but the hash and the size. The sampled 1 KiB chunks are
//! derived from a fresh nonce, requested with a ranged iroh-blobs GET and checked while they
//! stream in (bao verified streaming). The last chunk is always part of the sample, which
//! binds the announced size to the hash.
//!
//! What a passed challenge proves: at the time of the challenge the holder could produce the
//! sampled chunks, so it holds (or can reach) those ranges. It does not prove that the rest of
//! the blob is stored; repeated challenges with fresh nonces make a partial copy likely to be
//! caught, and full retrievability comes from combining challenges with erasure coding.

use std::ops::Range;

/// Size of a BLAKE3 chunk.
pub const CHUNK: u64 = 1024;
/// Chunks sampled per challenge by default (the last chunk comes on top).
pub const DEFAULT_SAMPLES: usize = 8;

/// Number of 1 KiB chunks of a blob of `size` bytes (an empty blob has one, empty, chunk).
pub fn chunk_count(size: u64) -> u64 {
    size.div_ceil(CHUNK).max(1)
}

/// The chunk indices sampled for `nonce`: up to `k` pseudo-random chunks plus the last one,
/// sorted and without duplicates. Deterministic in the nonce: a fresh nonce per challenge
/// keeps the holder from knowing in advance which chunks it must keep.
pub fn sample(nonce: &[u8; 32], size: u64, k: usize) -> Vec<u64> {
    let chunks = chunk_count(size);
    let mut out = Vec::with_capacity(k + 1);
    let mut hasher = blake3::Hasher::new_keyed(nonce);
    hasher.update(b"nuee/challenge/0");
    hasher.update(&size.to_le_bytes());
    let mut xof = hasher.finalize_xof();
    for _ in 0..k {
        let mut word = [0u8; 8];
        xof.fill(&mut word);
        out.push(u64::from_le_bytes(word) % chunks);
    }
    out.push(chunks - 1);
    out.sort_unstable();
    out.dedup();
    out
}

/// The byte ranges covered by `chunks` in a blob of `size` bytes.
pub fn byte_ranges(chunks: &[u64], size: u64) -> Vec<Range<u64>> {
    chunks
        .iter()
        .map(|&c| {
            let start = c.saturating_mul(CHUNK).min(size);
            let end = c.saturating_add(1).saturating_mul(CHUNK).min(size);
            start..end
        })
        .collect()
}

/// Whether every range in `wanted` lies inside the union of `got` (both sorted by start).
pub fn covered(wanted: &[Range<u64>], got: &[Range<u64>]) -> bool {
    let mut merged: Vec<Range<u64>> = Vec::with_capacity(got.len());
    let mut sorted = got.to_vec();
    sorted.sort_by_key(|r| r.start);
    for r in sorted {
        match merged.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => merged.push(r),
        }
    }
    wanted
        .iter()
        .all(|w| w.is_empty() || merged.iter().any(|m| m.start <= w.start && w.end <= m.end))
}

/// Result of a challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Every sampled range arrived and verified against the root hash.
    Passed {
        /// Chunks sampled.
        chunks: Vec<u64>,
        /// Payload bytes verified.
        bytes: u64,
    },
    /// The holder answered but could not prove possession (missing data, wrong size,
    /// bytes that do not hash to the root, refusal).
    Failed(String),
    /// The holder could not be reached; says nothing about its data.
    Unreachable(String),
}

impl Verdict {
    /// Whether the challenge passed.
    pub fn passed(&self) -> bool {
        matches!(self, Verdict::Passed { .. })
    }
}

/// Challenges `holder` on `hash` (`size` bytes) with `k` random chunks drawn from `nonce`.
/// The holder must already admit us to its blobs protocol for that hash.
#[cfg(not(target_family = "wasm"))]
pub async fn challenge(
    net: &nuee_net::Net,
    holder: nuee_proto::EndpointId,
    hash: nuee_proto::Hash,
    size: u64,
    nonce: [u8; 32],
    k: usize,
) -> Verdict {
    let addr = net
        .peer_addr(holder)
        .unwrap_or_else(|| iroh::EndpointAddr::new(holder));
    let conn = match n0_future::time::timeout(
        crate::CONNECT_TIMEOUT,
        net.endpoint().connect(addr, iroh_blobs::ALPN),
    )
    .await
    {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return Verdict::Unreachable(e.to_string()),
        Err(_) => return Verdict::Unreachable("connection timed out".into()),
    };
    let chunks = sample(&nonce, size, k);
    let verdict = match n0_future::time::timeout(
        CHALLENGE_TIMEOUT,
        ranged_get(conn.clone(), hash, size, &chunks),
    )
    .await
    {
        Ok(Ok(bytes)) => Verdict::Passed { chunks, bytes },
        Ok(Err(why)) => Verdict::Failed(why),
        Err(_) => Verdict::Failed("no complete answer in time".into()),
    };
    conn.close(0u32.into(), b"done");
    verdict
}

/// Time a holder has to deliver the sampled ranges.
#[cfg(not(target_family = "wasm"))]
pub const CHALLENGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Runs one ranged GET and returns the verified payload size, or why it failed.
#[cfg(not(target_family = "wasm"))]
async fn ranged_get(
    conn: iroh::endpoint::Connection,
    hash: nuee_proto::Hash,
    size: u64,
    chunks: &[u64],
) -> Result<u64, String> {
    use bao_tree::io::BaoContentItem;
    use iroh_blobs::get::fsm::{self, BlobContentNext, ConnectedNext, EndBlobNext};
    use iroh_blobs::protocol::{ChunkRanges, ChunkRangesExt, GetRequest};

    let mut ranges = ChunkRanges::empty();
    for &c in chunks {
        ranges |= ChunkRanges::chunk(c);
    }
    let request = GetRequest::blob_ranges(crate::gate::to_iroh(hash), ranges);
    let connected = fsm::start(conn, request, Default::default())
        .next()
        .await
        .map_err(|e| format!("open: {e}"))?;
    let root = match connected
        .next()
        .await
        .map_err(|e| format!("request: {e}"))?
    {
        ConnectedNext::StartRoot(root) => root,
        ConnectedNext::StartChild(_) | ConnectedNext::Closing(_) => {
            return Err("unexpected response shape".into());
        }
    };
    let (mut content, announced) = root
        .next()
        .next()
        .await
        .map_err(|e| format!("no data: {e}"))?;
    if announced != size {
        return Err(format!(
            "holder announces {announced} bytes, expected {size}"
        ));
    }
    let mut got = Vec::new();
    let mut bytes = 0u64;
    let end = loop {
        match content.next().await {
            BlobContentNext::More((next, item)) => {
                if let BaoContentItem::Leaf(leaf) =
                    item.map_err(|e| format!("verification: {e}"))?
                {
                    let len = leaf.data.len() as u64;
                    got.push(leaf.offset..leaf.offset.saturating_add(len));
                    bytes = bytes.saturating_add(len);
                }
                content = next;
            }
            BlobContentNext::Done(end) => break end,
        }
    };
    match end.next() {
        EndBlobNext::Closing(closing) => {
            closing.next().await.map_err(|e| format!("closing: {e}"))?;
        }
        EndBlobNext::MoreChildren(_) => return Err("unexpected children".into()),
    }
    if !covered(&byte_ranges(chunks, size), &got) {
        return Err("sampled ranges missing from the answer".into());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_is_deterministic_and_includes_the_last_chunk() {
        let a = sample(&[7; 32], 100_000, 8);
        assert_eq!(a, sample(&[7; 32], 100_000, 8));
        assert_ne!(a, sample(&[8; 32], 100_000, 8));
        assert_eq!(a.last(), Some(&(chunk_count(100_000) - 1)));
        assert!(a.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(sample(&[1; 32], 0, 4), vec![0]);
    }

    #[test]
    fn coverage() {
        let wanted = byte_ranges(&[0, 3], 3500);
        assert_eq!(wanted, vec![0..1024, 3072..3500]);
        assert!(covered(&wanted, &[0..2048, 2048..4096]));
        assert!(!covered(&wanted, &[0..1024, 2048..3072]));
    }
}
