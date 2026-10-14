//! Every wire decoder must reject arbitrary bytes without panicking, and whatever it
//! accepts must survive an encode/decode round trip unchanged.
#![no_main]

use std::hint::black_box;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use nuee_proto::limits::{MAX_MANIFEST, MAX_MSG};
use nuee_proto::{
    BoundedBytes, EndpointId, HttpRequest, HttpResponse, MAX_HTTP_MSG, Manifest, Msg, PinMsg,
    SecretKey, Signature, SignedManifest, TcpMsg, decode_with_limit, encode_with_limit,
};

fn coordinator() -> &'static EndpointId {
    static ID: OnceLock<EndpointId> = OnceLock::new();
    ID.get_or_init(|| SecretKey::from_bytes(&[7; 32]).public())
}

/// Opening must never panic; a manifest that opens must pass its own checks.
fn open(m: &SignedManifest) {
    if let Ok(manifest) = m.open(coordinator()) {
        assert!(manifest.check().is_ok());
    }
}

macro_rules! roundtrip {
    ($ty:ty, $data:expr, $max:expr) => {{
        match decode_with_limit::<$ty>($data, $max) {
            Ok(value) => {
                let bytes = encode_with_limit(&value, $max).expect("a decoded value re-encodes");
                let again: $ty = decode_with_limit(&bytes, $max).expect("a re-encoded value decodes");
                assert_eq!(value, again);
                Some(value)
            }
            Err(_) => None,
        }
    }};
}

fuzz_target!(|data: &[u8]| {
    if let Some(msg) = roundtrip!(Msg, data, MAX_MSG) {
        match &msg {
            Msg::Job(m) => open(m),
            Msg::Welcome { jobs, .. } => jobs.iter().for_each(open),
            _ => {}
        }
    }
    roundtrip!(PinMsg, data, MAX_MSG);
    roundtrip!(TcpMsg, data, MAX_MSG);
    roundtrip!(HttpRequest, data, MAX_HTTP_MSG);
    roundtrip!(HttpResponse, data, MAX_HTTP_MSG);
    if let Some(m) = roundtrip!(SignedManifest, data, MAX_MSG) {
        open(&m);
    }

    // Manifests hold floats (NaN != NaN), so compare encodings rather than values.
    if let Ok(manifest) = decode_with_limit::<Manifest>(data, MAX_MANIFEST) {
        let bytes = encode_with_limit(&manifest, MAX_MANIFEST).expect("a decoded manifest re-encodes");
        let again: Manifest =
            decode_with_limit(&bytes, MAX_MANIFEST).expect("a re-encoded manifest decodes");
        let bytes2 = encode_with_limit(&again, MAX_MANIFEST).expect("a manifest re-encodes twice");
        assert_eq!(bytes, bytes2);
        black_box(manifest.check().is_ok());
    }

    // Raw manifest bytes followed by a 64-byte signature, as `SignedManifest::open` sees them.
    if let Some((body, sig)) = data.split_last_chunk::<64>()
        && let Ok(bytes) = BoundedBytes::<MAX_MANIFEST>::try_from(body)
    {
        open(&SignedManifest {
            bytes,
            sig: Signature::from_bytes(sig),
        });
    }
});
