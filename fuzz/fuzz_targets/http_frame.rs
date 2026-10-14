//! Console traffic on `nuee/http/0`: one length-prefixed frame per request or response,
//! decoded with `MAX_HTTP_MSG` exactly as the host and the client do. Arbitrary bytes must be
//! rejected without panicking; whatever decodes must respect every bound the host relies on
//! (method, path, header count and sizes, body size), still fit in one frame once re-encoded,
//! and survive a round trip unchanged.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuee_net::framing::{frame_bytes, split_frames};
use nuee_proto::limits::{MAX_HTTP_BODY, MAX_HTTP_HEADERS};
use nuee_proto::{
    Header, HttpRequest, HttpResponse, MAX_HTTP_MSG, decode_with_limit, encode_with_limit,
};

fn check_headers(headers: &[Header]) {
    assert!(headers.len() <= MAX_HTTP_HEADERS);
    for h in headers {
        assert!(h.name.len() <= 128);
        assert!(h.value.len() <= 4096);
    }
}

/// Re-encodes `value`, checks it still fits a frame, and decodes it back.
fn roundtrip<T>(value: &T) -> T
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let bytes = encode_with_limit(value, MAX_HTTP_MSG).expect("a decoded message re-encodes");
    assert!(bytes.len() <= MAX_HTTP_MSG);
    let frame = frame_bytes(&bytes).expect("a message fits in a frame");
    let frames = split_frames(&frame, MAX_HTTP_MSG).expect("our own frame splits");
    assert_eq!(frames, [&bytes[..]]);
    decode_with_limit(&bytes, MAX_HTTP_MSG).expect("a re-encoded message decodes")
}

fn payload(payload: &[u8]) {
    if let Ok(req) = decode_with_limit::<HttpRequest>(payload, MAX_HTTP_MSG) {
        assert!(req.method.len() <= 16);
        assert!(req.path.len() <= 2048);
        check_headers(&req.headers);
        assert!(req.body.len() <= MAX_HTTP_BODY);
        assert_eq!(roundtrip(&req), req);
    }
    if let Ok(resp) = decode_with_limit::<HttpResponse>(payload, MAX_HTTP_MSG) {
        check_headers(&resp.headers);
        assert!(resp.body.len() <= MAX_HTTP_BODY);
        assert_eq!(roundtrip(&resp), resp);
    }
}

fuzz_target!(|data: &[u8]| {
    // As read from the stream: a length prefix, then the message.
    if let Ok(frames) = split_frames(data, MAX_HTTP_MSG) {
        for f in frames {
            assert!(f.len() <= MAX_HTTP_MSG);
            payload(f);
        }
    }
    // The bare message, as handed to the decoder once the frame is read.
    payload(data);
});
