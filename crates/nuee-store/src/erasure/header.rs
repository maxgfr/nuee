//! The erasure set header: format, limits and decoding. This file depends only on
//! `nuee-proto`, `serde` and `thiserror`, so the fuzz targets compile it on its own.

use nuee_proto::Hash;
use serde::{Deserialize, Serialize};

/// First bytes of every header.
pub const MAGIC: &[u8; 4] = b"NRS1";
/// Header format version.
pub const VERSION: u8 = 1;
/// Most shards (data + parity) in one set.
pub const MAX_SHARDS: usize = 64;
/// Largest shard.
pub const MAX_SHARD_SIZE: u32 = 64 * 1024 * 1024;
/// Data shards by default.
pub const DEFAULT_DATA: u16 = 4;
/// Parity shards by default.
pub const DEFAULT_PARITY: u16 = 2;

const FIXED: usize = 4 + 1 + 2 + 2 + 8 + 4;

/// Largest encoded header.
pub const MAX_HEADER: usize = FIXED + 32 * (MAX_SHARDS + 1);
const _: () = assert!(MAX_HEADER == nuee_proto::MAX_ERASURE_HEADER);

/// What can go wrong with an erasure set.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ErasureError {
    /// The header bytes are not a valid header.
    #[error("invalid erasure header: {0}")]
    Header(String),
    /// `k` or `m` is out of bounds.
    #[error("unsupported scheme {k}+{m} (k >= 1, m >= 1, k + m <= 64)")]
    Scheme {
        /// Data shards.
        k: u16,
        /// Parity shards.
        m: u16,
    },
    /// The ciphertext needs shards above [`MAX_SHARD_SIZE`].
    #[error("{len} bytes need shards above {MAX_SHARD_SIZE} bytes with k = {k}")]
    TooLarge {
        /// Ciphertext length.
        len: u64,
        /// Data shards.
        k: u16,
    },
    /// Fewer than `k` valid shards are available.
    #[error("{have} valid shards, {need} needed")]
    NotEnough {
        /// Valid distinct shards given.
        have: usize,
        /// Shards needed (`k`).
        need: usize,
    },
    /// The asked shard index is not in the set.
    #[error("no shard {0} in this set")]
    Index(u16),
    /// The rebuilt data does not match the header.
    #[error("integrity check failed: {0}")]
    Integrity(String),
    /// The coder failed.
    #[error("reed-solomon: {0}")]
    Coder(String),
}

/// The description of an erasure set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    /// Data shards.
    pub k: u16,
    /// Parity shards.
    pub m: u16,
    /// Length of the original ciphertext.
    pub len: u64,
    /// Size of every shard.
    pub shard_size: u32,
    /// Hash of every shard, data shards first.
    pub shards: Vec<Hash>,
    /// Hash of the original ciphertext.
    pub original: Hash,
}

pub(super) fn check_scheme(k: u16, m: u16) -> Result<(), ErasureError> {
    if k == 0 || m == 0 || usize::from(k) + usize::from(m) > MAX_SHARDS {
        return Err(ErasureError::Scheme { k, m });
    }
    Ok(())
}

/// The shard size for `len` bytes split in `k` data shards: `ceil(len / k)` rounded up to an
/// even count, at least 2.
pub fn shard_size(len: u64, k: u16) -> Result<u32, ErasureError> {
    if k == 0 {
        return Err(ErasureError::Scheme { k, m: 0 });
    }
    let per = len.div_ceil(u64::from(k));
    let even = per.saturating_add(per % 2).max(2);
    match u32::try_from(even) {
        Ok(s) if s <= MAX_SHARD_SIZE => Ok(s),
        _ => Err(ErasureError::TooLarge { len, k }),
    }
}

/// Parses a scheme written `k+m`, such as `4+2`.
pub fn parse_scheme(s: &str) -> Result<(u16, u16), ErasureError> {
    let bad = || ErasureError::Header(format!("scheme {s:?} is not k+m"));
    let (k, m) = s.trim().split_once('+').ok_or_else(bad)?;
    let k: u16 = k.trim().parse().map_err(|_| bad())?;
    let m: u16 = m.trim().parse().map_err(|_| bad())?;
    check_scheme(k, m)?;
    Ok((k, m))
}

impl Header {
    /// Data plus parity shards.
    pub fn total(&self) -> usize {
        usize::from(self.k) + usize::from(self.m)
    }

    /// Checks the header's internal consistency.
    pub fn validate(&self) -> Result<(), ErasureError> {
        check_scheme(self.k, self.m)?;
        if self.shards.len() != self.total() {
            return Err(ErasureError::Header(format!(
                "{} shard hashes for {} shards",
                self.shards.len(),
                self.total()
            )));
        }
        let expected = shard_size(self.len, self.k)?;
        if self.shard_size != expected {
            return Err(ErasureError::Header(format!(
                "shard size {} instead of {expected}",
                self.shard_size
            )));
        }
        Ok(())
    }

    /// The header bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(FIXED + 32 * (self.shards.len() + 1));
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        out.extend_from_slice(&self.k.to_le_bytes());
        out.extend_from_slice(&self.m.to_le_bytes());
        out.extend_from_slice(&self.len.to_le_bytes());
        out.extend_from_slice(&self.shard_size.to_le_bytes());
        for h in &self.shards {
            out.extend_from_slice(&h.0);
        }
        out.extend_from_slice(&self.original.0);
        out
    }

    /// The hash of the header bytes, which identifies the set.
    pub fn id(&self) -> Hash {
        Hash::of(&self.encode())
    }

    /// Whether `bytes` is shard `index` of this set (size and hash).
    pub fn check_shard(&self, index: u16, bytes: &[u8]) -> bool {
        self.shards
            .get(usize::from(index))
            .is_some_and(|h| bytes.len() == self.shard_size as usize && Hash::of(bytes) == *h)
    }
}

fn take<'a>(bytes: &mut &'a [u8], n: usize) -> Result<&'a [u8], ErasureError> {
    if bytes.len() < n {
        return Err(ErasureError::Header("truncated".into()));
    }
    let (head, rest) = bytes.split_at(n);
    *bytes = rest;
    Ok(head)
}

fn take_array<const N: usize>(bytes: &mut &[u8]) -> Result<[u8; N], ErasureError> {
    let mut out = [0u8; N];
    out.copy_from_slice(take(bytes, N)?);
    Ok(out)
}

/// Decodes and validates header bytes. Never allocates more than [`MAX_HEADER`] bytes' worth.
pub fn decode_header(bytes: &[u8]) -> Result<Header, ErasureError> {
    if bytes.len() > MAX_HEADER {
        return Err(ErasureError::Header(format!(
            "{} bytes, at most {MAX_HEADER}",
            bytes.len()
        )));
    }
    let mut rest = bytes;
    if take(&mut rest, 4)? != MAGIC {
        return Err(ErasureError::Header("bad magic".into()));
    }
    let version = take_array::<1>(&mut rest)?[0];
    if version != VERSION {
        return Err(ErasureError::Header(format!(
            "unsupported version {version}"
        )));
    }
    let k = u16::from_le_bytes(take_array(&mut rest)?);
    let m = u16::from_le_bytes(take_array(&mut rest)?);
    check_scheme(k, m)?;
    let len = u64::from_le_bytes(take_array(&mut rest)?);
    let shard_size = u32::from_le_bytes(take_array(&mut rest)?);
    if shard_size > MAX_SHARD_SIZE {
        return Err(ErasureError::Header(format!(
            "shard size {shard_size} above {MAX_SHARD_SIZE}"
        )));
    }
    let total = usize::from(k) + usize::from(m);
    if rest.len() != 32 * (total + 1) {
        return Err(ErasureError::Header(format!(
            "{} bytes of hashes, expected {}",
            rest.len(),
            32 * (total + 1)
        )));
    }
    let mut shards = Vec::with_capacity(total);
    for _ in 0..total {
        shards.push(Hash(take_array(&mut rest)?));
    }
    let original = Hash(take_array(&mut rest)?);
    let header = Header {
        k,
        m,
        len,
        shard_size,
        shards,
        original,
    };
    header.validate()?;
    Ok(header)
}
