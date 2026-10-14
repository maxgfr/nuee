//! Erasure set headers (`NRS1`) come from untrusted providers. The decoder must reject arbitrary
//! bytes without panicking; a header it accepts must be self-consistent and re-encode to exactly
//! the bytes it came from, so its id is the hash of those bytes.
//!
//! The header module is compiled from its source file: `nuee-store` itself depends on
//! `iroh-blobs`, which the nightly compiler used for fuzzing does not build.
#![no_main]

#[path = "../../crates/nuee-store/src/erasure/header.rs"]
mod header;

use header::{
    DEFAULT_DATA, DEFAULT_PARITY, ErasureError, MAX_HEADER, MAX_SHARDS, decode_header,
    parse_scheme, shard_size,
};
use libfuzzer_sys::fuzz_target;
use nuee_proto::Hash;

/// Every error renders, whatever numbers the input puts in it.
fn render(data: &[u8]) {
    let n = data.first().copied().unwrap_or(0);
    let errors = [
        ErasureError::NotEnough {
            have: usize::from(n),
            need: usize::from(DEFAULT_DATA),
        },
        ErasureError::Index(u16::from(n)),
        ErasureError::Integrity(String::from_utf8_lossy(data).into_owned()),
        ErasureError::Coder(String::from_utf8_lossy(data).into_owned()),
    ];
    for e in errors {
        assert!(!e.to_string().is_empty());
    }
}

fuzz_target!(|data: &[u8]| {
    assert_eq!(
        parse_scheme(&format!("{DEFAULT_DATA}+{DEFAULT_PARITY}")),
        Ok((DEFAULT_DATA, DEFAULT_PARITY))
    );
    render(data);
    if let Ok(h) = decode_header(data) {
        assert!(data.len() <= MAX_HEADER);
        assert!(h.validate().is_ok());
        assert!(h.k >= 1 && h.m >= 1 && h.total() <= MAX_SHARDS);
        assert_eq!(h.shards.len(), h.total());
        assert_eq!(h.encode(), data);
        assert_eq!(h.id(), Hash::of(data));
        assert_eq!(decode_header(&h.encode()), Ok(h.clone()));
        for i in 0..h.total() {
            let i = u16::try_from(i).expect("at most 64 shards");
            // Shards are at least two bytes: an empty one is refused before hashing.
            assert!(!h.check_shard(i, &[]));
        }
        assert!(!h.check_shard(u16::try_from(h.total()).expect("at most 64 shards"), &[]));
    }

    if let Ok(s) = std::str::from_utf8(data)
        && let Ok((k, m)) = parse_scheme(s)
    {
        assert!(k >= 1 && m >= 1 && usize::from(k) + usize::from(m) <= MAX_SHARDS);
        assert_eq!(parse_scheme(&format!("{k}+{m}")), Ok((k, m)));
    }

    if let Some((len, rest)) = data.split_first_chunk::<8>()
        && let Some((k, _)) = rest.split_first_chunk::<2>()
    {
        let len = u64::from_le_bytes(*len);
        let k = u16::from_le_bytes(*k);
        if let Ok(size) = shard_size(len, k) {
            assert!(size >= 2 && size % 2 == 0);
            assert!(u64::from(size) * u64::from(k) >= len);
        }
    }
});
