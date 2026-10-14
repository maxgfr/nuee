//! Task input envelope used when a job has assets: `u32 LE count`, then per asset
//! `u32 LE length || bytes` in manifest order, then the lease input to the end.

use crate::limits::MAX_ASSETS_PER_JOB;

/// Builds the bytes handed to `nuee_run` for a job with assets.
pub fn build_envelope(assets: &[&[u8]], input: &[u8]) -> Vec<u8> {
    let total = 4 + assets.iter().map(|a| 4 + a.len()).sum::<usize>() + input.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(assets.len() as u32).to_le_bytes());
    for asset in assets {
        out.extend_from_slice(&(asset.len() as u32).to_le_bytes());
        out.extend_from_slice(asset);
    }
    out.extend_from_slice(input);
    out
}

/// Splits an envelope into its assets and the lease input; `None` when malformed.
pub fn split_envelope(bytes: &[u8]) -> Option<(Vec<&[u8]>, &[u8])> {
    let (count, mut rest) = read_u32(bytes)?;
    let count = usize::try_from(count).ok()?;
    if count > MAX_ASSETS_PER_JOB {
        return None;
    }
    let mut assets = Vec::with_capacity(count);
    for _ in 0..count {
        let (len, tail) = read_u32(rest)?;
        let len = usize::try_from(len).ok()?;
        if len > tail.len() {
            return None;
        }
        let (asset, tail) = tail.split_at(len);
        assets.push(asset);
        rest = tail;
    }
    Some((assets, rest))
}

fn read_u32(bytes: &[u8]) -> Option<(u32, &[u8])> {
    let (head, tail) = bytes.split_first_chunk::<4>()?;
    Some((u32::from_le_bytes(*head), tail))
}
