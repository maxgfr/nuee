use std::collections::HashSet;

use nuee_proto::fingerprint::EMOJI;
use nuee_proto::{SecretKey, fingerprint, fingerprint_string};

#[test]
fn table_has_256_distinct_single_scalars() {
    let set: HashSet<&str> = EMOJI.iter().copied().collect();
    assert_eq!(set.len(), 256);
    for e in EMOJI {
        assert_eq!(e.chars().count(), 1, "{e} is not a single scalar");
    }
}

#[test]
fn fingerprint_is_stable() {
    let id = SecretKey::from_bytes(&[42; 32]).public();
    assert_eq!(fingerprint(&id), fingerprint(&id));
    assert_eq!(fingerprint_string(&id), FIXED);
    let other = SecretKey::from_bytes(&[43; 32]).public();
    assert_ne!(fingerprint(&other), fingerprint(&id));
}

// Frozen test vector: changing the table or the derivation breaks every shown fingerprint.
const FIXED: &str = "🍪 🐟 🍇 🐼 🦝 🏓";
