//! Reed-Solomon erasure coding of shared ciphertexts.
//!
//! A ciphertext is split into `k` data shards and `m` parity shards (4+2 by default); any `k`
//! of the `k + m` shards rebuild it. Each shard is stored as its own blob (its own BLAKE3
//! hash), so each can live on a different store member. A small header, itself a blob, lists
//! the shard hashes; its hash identifies the set.
//!
//! Access is per set: the header and every shard are served with the set's access secret.
//! A member holding several blobs of a set admits a peer for all of them after one hello, so
//! a downloader that does not know who holds what asks each provider once, without a single
//! failed proof (failed proofs count towards the provider's anti-guessing throttle).
//!
//! A set link first names the sharer. Once the sharer is gone, the link's providers must be
//! the current holders, which the coordinator records per blob (`holding` in its
//! `durable.json`, or [`crate::durable::DurabilityService::state`]): a header holder first,
//! then shard holders.
//!
//! Header layout (little endian, exact length, nothing after it):
//!
//! ```text
//! b"NRS1" | version u8 (= 1) | k u16 | m u16 | original length u64 | shard size u32
//!        | k + m shard hashes (32 bytes each) | BLAKE3 of the original ciphertext (32 bytes)
//! ```
//!
//! Bounds: `k >= 1`, `m >= 1`, `k + m <= 64`, shard size `<= 64 MiB`, and the shard size must
//! be exactly the one [`shard_size`] gives for the length and `k`.
//!
//! Shard size rule: `ceil(len / k)` rounded up to an even byte count (the coder works on
//! 16-bit symbols and needs a non-zero, even shard size), at least 2. The last data shard is
//! zero-padded; the result of a reconstruction is truncated back to the original length.
//!
//! Encoding and reconstruction use `reed-solomon-simd`; they, and the set download, compile for
//! wasm32 as well as natively, so a browser can rebuild a set itself.

mod header;

use header::check_scheme;
pub use header::{
    DEFAULT_DATA, DEFAULT_PARITY, ErasureError, Header, MAGIC, MAX_HEADER, MAX_SHARD_SIZE,
    MAX_SHARDS, VERSION, decode_header, parse_scheme, shard_size,
};

pub use coding::{ErasureSet, encode, rebuild_shard, reconstruct};
pub use transfer::{SetOffer, fetch_set, fetch_set_with, offer_set};

mod transfer {
    use nuee_net::Net;
    use nuee_proto::{Hash, Provider};

    use super::{ErasureError, Header, encode, reconstruct};
    use crate::StoreError;
    use crate::gate::StoreHub;
    use crate::share::{FilesAcceptor, Offer, SetSession};

    fn store_error(e: ErasureError) -> StoreError {
        match e {
            ErasureError::NotEnough { .. } => StoreError::Unreachable(e.to_string()),
            ErasureError::TooLarge { len, .. } => StoreError::TooLarge {
                size: len,
                max: u64::from(super::MAX_SHARD_SIZE) * 63,
            },
            other => StoreError::Integrity(other.to_string()),
        }
    }

    /// An erasure set offered by this node.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SetOffer {
        /// The set description.
        pub header: Header,
        /// The encoded header.
        pub bytes: Vec<u8>,
        /// Hash of the encoded header: the set identity.
        pub id: Hash,
    }

    /// Splits the ciphertext `cipher` (already in `hub`) into `k + m` shards, stores the
    /// shards and the header in `hub` and offers them through `acceptor`: the header with
    /// `access`, like every shard.
    pub async fn offer_set(
        hub: &StoreHub,
        acceptor: &FilesAcceptor,
        cipher: Hash,
        access: [u8; 16],
        k: u16,
        m: u16,
    ) -> Result<SetOffer, StoreError> {
        let bytes = hub.read(cipher).await?;
        if Hash::of(&bytes) != cipher {
            return Err(StoreError::Integrity(format!(
                "stored bytes do not match {cipher}"
            )));
        }
        let set = encode(&bytes, k, m).map_err(store_error)?;
        drop(bytes);
        for (hash, shard) in set.header.shards.iter().zip(set.shards) {
            let size = shard.len() as u64;
            hub.add_bytes(bytes::Bytes::from(shard)).await?;
            acceptor.add(Offer {
                access,
                hash: *hash,
                size,
            });
        }
        let encoded = set.header.encode();
        let id = hub.add_bytes(bytes::Bytes::from(encoded.clone())).await?;
        acceptor.add(Offer {
            access,
            hash: id,
            size: encoded.len() as u64,
        });
        Ok(SetOffer {
            header: set.header,
            bytes: encoded,
            id,
        })
    }

    /// Downloads shards of `header` from `providers` until `k` of them verify, then
    /// rebuilds and checks the original ciphertext. Each provider gets one hello with the
    /// set's `access` and is then asked for every shard still missing; the shards it does not
    /// hold are refused by its blob gate, which costs nothing towards its throttle.
    pub async fn fetch_set(
        net: &Net,
        hub: &StoreHub,
        providers: &[Provider],
        access: &[u8; 16],
        header: &Header,
    ) -> Result<Vec<u8>, StoreError> {
        fetch_set_with(net, hub, providers, access, header, |_, _| {}).await
    }

    /// [`fetch_set`], reporting `(bytes of verified shards, ciphertext length)` after each
    /// shard (the first value is capped at the length).
    pub async fn fetch_set_with<P: FnMut(u64, u64)>(
        net: &Net,
        hub: &StoreHub,
        providers: &[Provider],
        access: &[u8; 16],
        header: &Header,
        mut progress: P,
    ) -> Result<Vec<u8>, StoreError> {
        header.validate().map_err(store_error)?;
        hub.check_size(header.len)?;
        let k = usize::from(header.k);
        let size = u64::from(header.shard_size);
        let mut have: Vec<(u16, Vec<u8>)> = Vec::new();
        let mut last: Option<StoreError> = None;
        for provider in providers {
            if have.len() >= k {
                break;
            }
            let session = match SetSession::open(net, provider, access).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(provider = %provider.id, "set provider failed: {e}");
                    last = Some(e);
                    continue;
                }
            };
            for (index, hash) in (0u16..).zip(header.shards.iter().copied()) {
                if have.len() >= k {
                    break;
                }
                if have.iter().any(|(i, _)| *i == index) {
                    continue;
                }
                if let Err(e) = hub
                    .fetch_to_store(session.addr.clone(), hash, size, &mut |_, _| {})
                    .await
                {
                    tracing::debug!(provider = %provider.id, index, %hash, "shard not fetched: {e}");
                    last = Some(e);
                    continue;
                }
                let bytes = hub.read(hash).await?;
                // A shard repeated in the set (degenerate data) counts for every index.
                for (i, h) in (0u16..).zip(header.shards.iter()) {
                    if *h == hash
                        && !have.iter().any(|(j, _)| *j == i)
                        && header.check_shard(i, &bytes)
                    {
                        have.push((i, bytes.to_vec()));
                    }
                }
                progress((have.len() as u64 * size).min(header.len), header.len);
            }
            session.end().await;
        }
        if have.len() < k {
            return Err(last.unwrap_or(StoreError::NoProvider));
        }
        reconstruct(header, have).map_err(store_error)
    }
}

mod coding {
    use std::collections::BTreeMap;

    use nuee_proto::Hash;

    use super::{ErasureError, Header, check_scheme, shard_size};

    /// A header and its shards (data shards first).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ErasureSet {
        /// The set description.
        pub header: Header,
        /// The `k + m` shards.
        pub shards: Vec<Vec<u8>>,
    }

    fn coder(e: reed_solomon_simd::Error) -> ErasureError {
        ErasureError::Coder(e.to_string())
    }

    /// Splits `ciphertext` into `k` data shards and `m` parity shards.
    pub fn encode(ciphertext: &[u8], k: u16, m: u16) -> Result<ErasureSet, ErasureError> {
        check_scheme(k, m)?;
        let len = ciphertext.len() as u64;
        let size = shard_size(len, k)? as usize;
        let mut data: Vec<Vec<u8>> = (0..usize::from(k))
            .map(|i| {
                let start = (i * size).min(ciphertext.len());
                let end = ((i + 1) * size).min(ciphertext.len());
                let mut shard = ciphertext[start..end].to_vec();
                shard.resize(size, 0);
                shard
            })
            .collect();
        let parity =
            reed_solomon_simd::encode(usize::from(k), usize::from(m), &data).map_err(coder)?;
        data.extend(parity);
        let header = Header {
            k,
            m,
            len,
            shard_size: size as u32,
            shards: data.iter().map(|s| Hash::of(s)).collect(),
            original: Hash::of(ciphertext),
        };
        Ok(ErasureSet {
            header,
            shards: data,
        })
    }

    /// The `k` data shards from any `k` valid shards. Shards whose size or hash does not
    /// match the header, and duplicates, are ignored.
    fn data_shards(
        header: &Header,
        available: Vec<(u16, Vec<u8>)>,
    ) -> Result<Vec<Vec<u8>>, ErasureError> {
        header.validate()?;
        let k = usize::from(header.k);
        let mut valid: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
        for (index, shard) in available {
            let i = usize::from(index);
            if valid.contains_key(&i) {
                continue;
            }
            if header.check_shard(index, &shard) {
                valid.insert(i, shard);
            } else {
                tracing::debug!(index, "erasure shard rejected: size or hash mismatch");
            }
            if valid.len() == k {
                break;
            }
        }
        if valid.len() < k {
            return Err(ErasureError::NotEnough {
                have: valid.len(),
                need: k,
            });
        }
        let (original, recovery): (Vec<_>, Vec<_>) = valid.into_iter().partition(|(i, _)| *i < k);
        let mut data: BTreeMap<usize, Vec<u8>> = original.into_iter().collect();
        if data.len() < k {
            let restored = reed_solomon_simd::decode(
                k,
                usize::from(header.m),
                data.iter().map(|(i, s)| (*i, s.as_slice())),
                recovery.iter().map(|(i, s)| (*i - k, s.as_slice())),
            )
            .map_err(coder)?;
            data.extend(restored);
        }
        let shards: Vec<Vec<u8>> = (0..k).filter_map(|i| data.remove(&i)).collect();
        if shards.len() != k {
            return Err(ErasureError::Integrity(
                "data shards missing after decoding".into(),
            ));
        }
        Ok(shards)
    }

    /// Rebuilds the original ciphertext from any `k` valid shards. Every shard is checked
    /// against its hash before use, the result against the original hash.
    pub fn reconstruct(
        header: &Header,
        available: Vec<(u16, Vec<u8>)>,
    ) -> Result<Vec<u8>, ErasureError> {
        let shards = data_shards(header, available)?;
        let mut out = shards.concat();
        let len = usize::try_from(header.len)
            .map_err(|_| ErasureError::Integrity("length does not fit in memory".into()))?;
        if out.len() < len {
            return Err(ErasureError::Integrity(
                "shards shorter than the length".into(),
            ));
        }
        out.truncate(len);
        if Hash::of(&out) != header.original {
            return Err(ErasureError::Integrity(
                "rebuilt ciphertext does not match the original hash".into(),
            ));
        }
        Ok(out)
    }

    /// Rebuilds shard `index` from any `k` valid shards; the result is checked against the
    /// hash the header lists for it.
    pub fn rebuild_shard(
        header: &Header,
        available: Vec<(u16, Vec<u8>)>,
        index: u16,
    ) -> Result<Vec<u8>, ErasureError> {
        if usize::from(index) >= header.total() {
            return Err(ErasureError::Index(index));
        }
        let mut data = data_shards(header, available)?;
        let k = usize::from(header.k);
        let i = usize::from(index);
        let shard = if i < k {
            data.swap_remove(i)
        } else {
            let mut parity =
                reed_solomon_simd::encode(k, usize::from(header.m), &data).map_err(coder)?;
            if i - k >= parity.len() {
                return Err(ErasureError::Index(index));
            }
            parity.swap_remove(i - k)
        };
        if !header.check_shard(index, &shard) {
            return Err(ErasureError::Integrity(format!(
                "rebuilt shard {index} does not match its hash"
            )));
        }
        Ok(shard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_k_shards_reconstruct_and_rebuild() {
        let data: Vec<u8> = (0..10_001u32).map(|i| (i * 31 % 251) as u8).collect();
        let set = encode(&data, 4, 2).expect("encode");
        let shards: Vec<(u16, Vec<u8>)> = (0u16..).zip(set.shards.iter().cloned()).collect();
        // Lose two data shards: parity makes up for them.
        let kept: Vec<_> = shards
            .iter()
            .filter(|(i, _)| *i != 0 && *i != 2)
            .cloned()
            .collect();
        assert_eq!(
            reconstruct(&set.header, kept.clone()).expect("rebuild"),
            data
        );
        assert_eq!(
            rebuild_shard(&set.header, kept.clone(), 0).expect("shard 0"),
            set.shards[0]
        );
        assert_eq!(
            rebuild_shard(&set.header, kept, 5).expect("shard 5"),
            set.shards[5]
        );
        // Three lost shards of a 4+2 set are too many.
        let three: Vec<_> = shards.into_iter().filter(|(i, _)| *i > 2).collect();
        assert!(matches!(
            reconstruct(&set.header, three),
            Err(ErasureError::NotEnough { have: 3, need: 4 })
        ));
    }
}
