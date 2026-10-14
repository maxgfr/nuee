//! Module validation runs on untrusted bytes before anything is compiled: it must never
//! panic, and an accepted module must be described consistently.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuee_proto::limits::MAX_WASM_MEMORY;
use nuee_proto::{Hash, validate_module};

fuzz_target!(|data: &[u8]| {
    if let Ok(info) = validate_module(data) {
        assert_eq!(info.size, data.len());
        assert_eq!(info.hash, Hash::of(data));
        assert!(info.max_memory <= MAX_WASM_MEMORY);
        assert!(info.min_memory <= info.max_memory);
    }
});
