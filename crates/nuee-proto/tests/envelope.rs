use nuee_proto::{build_envelope, split_envelope};

#[test]
fn envelope_roundtrip() {
    let a: &[u8] = b"first asset";
    let b: &[u8] = b"";
    let env = build_envelope(&[a, b], b"the input");
    let (assets, input) = split_envelope(&env).unwrap();
    assert_eq!(assets, vec![a, b]);
    assert_eq!(input, b"the input");
    let env = build_envelope(&[], b"");
    assert_eq!(split_envelope(&env).unwrap(), (vec![], &b""[..]));
}

#[test]
fn malformed_envelopes_are_refused() {
    assert!(split_envelope(b"").is_none());
    assert!(split_envelope(&[1, 0, 0]).is_none());
    // One asset claiming 100 bytes with only 3 present.
    assert!(split_envelope(&[1, 0, 0, 0, 100, 0, 0, 0, 1, 2, 3]).is_none());
    // Count above MAX_ASSETS_PER_JOB.
    let mut many = 17u32.to_le_bytes().to_vec();
    many.extend(std::iter::repeat_n(0u8, 17 * 4));
    assert!(split_envelope(&many).is_none());
    // Every truncation of a valid envelope inside the asset table is refused.
    let env = build_envelope(&[b"abc", b"de"], b"");
    for cut in 0..env.len() {
        assert!(split_envelope(&env[..cut]).is_none(), "cut {cut}");
    }
}
