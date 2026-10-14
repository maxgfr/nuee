//! Job files come from users and examples: parsing and validation must never panic, and
//! validation must agree with the checked constructor.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuee_proto::JobToml;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let parsed = JobToml::parse(&text);
    let checked = JobToml::from_str_checked(&text);
    match parsed {
        Ok(job) => {
            let valid = job.validate();
            assert_eq!(valid.is_ok(), checked.is_ok());
            if valid.is_ok() {
                assert!(job.deadline_ms().is_ok());
                assert!(job.max_memory_bytes().is_ok());
            }
        }
        Err(_) => assert!(checked.is_err()),
    }
});
