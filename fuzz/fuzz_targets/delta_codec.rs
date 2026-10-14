//! Parameter deltas (`NDL1`) and training lease inputs (`NTR1`) come from untrusted workers
//! and coordinators. Decoding arbitrary bytes against a fixed layout must never panic; a delta
//! that decodes must have the layout's length and re-encode to the same values (bit for bit
//! with `F32`, within half a quantisation step with `Int8Ef`).
#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use nuee_proto::DeltaCodec;
use nuee_train::{Layout, TaskInput, decode_delta, encode_delta, split_output};

fn layout() -> &'static Layout {
    static LAYOUT: OnceLock<Layout> = OnceLock::new();
    LAYOUT.get_or_init(|| Layout {
        names: vec!["a".into(), "b".into(), "c".into(), "d".into()],
        shapes: vec![vec![2, 3], vec![4], vec![0, 5], vec![1]],
    })
}

fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|x| x.to_bits()).collect()
}

fn check_delta(bytes: &[u8]) {
    let layout = layout();
    let Ok(values) = decode_delta(bytes, layout) else {
        return;
    };
    assert_eq!(values.len(), layout.total());

    let mut unused = Vec::new();
    let f32s = encode_delta(DeltaCodec::F32, layout, &values, &mut unused)
        .expect("a decoded delta re-encodes as f32");
    let again = decode_delta(&f32s, layout).expect("an f32 delta decodes");
    assert_eq!(bits(&again), bits(&values));
    if bytes[4] == 0 {
        // Only the per-tensor scale, written as 1.0 and ignored, may differ.
        assert_eq!(f32s.len(), bytes.len());
    }

    let mut residual = Vec::new();
    let int8 = encode_delta(DeltaCodec::Int8Ef, layout, &values, &mut residual)
        .expect("a decoded delta re-encodes as int8");
    let approx = decode_delta(&int8, layout).expect("an int8 delta decodes");
    assert_eq!(approx.len(), values.len());
    let lens = layout.lens();
    let mut off = 0;
    for len in lens {
        let x = &values[off..off + len];
        let y = &approx[off..off + len];
        let max = x
            .iter()
            .filter(|v| v.is_finite())
            .fold(0.0f32, |m, v| m.max(v.abs()));
        let step = max / 127.0;
        for (a, b) in x.iter().zip(y) {
            assert!(b.is_finite());
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
}

fuzz_target!(|data: &[u8]| {
    check_delta(data);
    // As returned by a trainer: the delta followed by the mean loss.
    if let Ok((delta, _loss)) = split_output(data) {
        check_delta(delta);
    }

    if let Ok(input) = TaskInput::decode(data) {
        assert!(input.shard < input.n_shards);
        assert!(input.lr.is_finite() && input.lr >= 0.0);
        assert_eq!(input.encode(), data);
    }
});
