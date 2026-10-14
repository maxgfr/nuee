//! Int8 error-feedback encoding at the edges of the f32 range.

use nuee_proto::DeltaCodec;
use nuee_train::{Layout, TrainError, decode_delta, encode_delta};

fn fuzz_layout() -> Layout {
    Layout {
        names: vec!["a".into(), "b".into(), "c".into(), "d".into()],
        shapes: vec![vec![2, 3], vec![4], vec![0, 5], vec![1]],
    }
}

fn one(n: usize) -> Layout {
    Layout {
        names: vec!["w".into()],
        shapes: vec![vec![n]],
    }
}

/// An F32 delta found by the `delta_codec` fuzz target: NaN and values near `f32::MAX`.
const CRASH: [u8; 85] = [
    0x4e, 0x44, 0x4c, 0x31, 0x00, 0x04, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x20,
    0x4e, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f, 0xff, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x03, 0x00, 0x4f, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03,
    0x00, 0x00, 0x00, 0x00, 0x80,
];

/// Same round trip as the fuzz target: decode, re-encode as int8, decode, compare.
#[test]
fn fuzz_crash_round_trips_finite_within_half_a_step() {
    let layout = fuzz_layout();
    let values = decode_delta(&CRASH, &layout).expect("the crash input decodes");
    assert!(values.iter().any(|v| v.is_nan()));
    let mut residual = Vec::new();
    let int8 = encode_delta(DeltaCodec::Int8Ef, &layout, &values, &mut residual).expect("encode");
    let approx = decode_delta(&int8, &layout).expect("an int8 delta decodes");
    let mut off = 0;
    for len in layout.lens() {
        let x = &values[off..off + len];
        let y = &approx[off..off + len];
        let max = x
            .iter()
            .filter(|v| v.is_finite())
            .fold(0.0f32, |m, v| m.max(v.abs()));
        let step = max / 127.0;
        for (a, b) in x.iter().zip(y) {
            assert!(b.is_finite(), "{a} came back as {b}");
            if a.is_finite() {
                let err = f64::from(*a) - f64::from(*b);
                let tol = 0.51 * f64::from(step)
                    + 4.0 * f64::from(f32::EPSILON) * f64::from(a.abs())
                    + 128.0 * f64::from(f32::MIN_POSITIVE);
                assert!(err.abs() <= tol, "{a} came back as {b} (step {step})");
            }
        }
        off += len;
    }
    assert!(residual.iter().all(|e| e.is_finite()));
}

#[test]
fn extreme_scale_stays_finite_and_decoder_rejects_overflowing_scale() {
    let layout = one(3);
    let delta = [f32::MAX, -f32::MAX, 1.0];
    let mut residual = Vec::new();
    let enc = encode_delta(DeltaCodec::Int8Ef, &layout, &delta, &mut residual).expect("encode");
    let scale = f32::from_le_bytes(enc[13..17].try_into().expect("scale bytes"));
    assert!((127.0 * scale).is_finite());
    assert!(scale <= f32::MAX / 127.0);
    let back = decode_delta(&enc, &layout).expect("decode");
    assert!(back.iter().all(|v| v.is_finite()));
    assert!(residual.iter().all(|e| e.is_finite()));

    // A hand-made tensor whose scale overflows once multiplied by 127.
    let mut bad = enc.clone();
    bad[13..17].copy_from_slice(&f32::MAX.to_le_bytes());
    assert!(matches!(
        decode_delta(&bad, &layout),
        Err(TrainError::Delta(_))
    ));
}

#[test]
fn non_finite_values_are_sent_as_zero_and_not_carried_over() {
    let layout = one(4);
    let mut residual = Vec::new();
    let first = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 2.0];
    let enc = encode_delta(DeltaCodec::Int8Ef, &layout, &first, &mut residual).expect("encode");
    let back = decode_delta(&enc, &layout).expect("decode");
    assert_eq!(&back[..3], &[0.0, 0.0, 0.0]);
    assert_eq!(&residual[..3], &[0.0, 0.0, 0.0]);
    assert!(residual.iter().all(|e| e.is_finite()));

    // The next round sends those parameters normally again.
    let second = [1.0, -1.0, 0.5, 0.0];
    let enc = encode_delta(DeltaCodec::Int8Ef, &layout, &second, &mut residual).expect("encode");
    let back = decode_delta(&enc, &layout).expect("decode");
    assert!((back[0] - 1.0).abs() < 0.02);
    assert!((back[1] + 1.0).abs() < 0.02);
    assert!((back[2] - 0.5).abs() < 0.02);
}
