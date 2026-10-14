//! Link parsing must never panic on pasted text, and a parsed link must render back to a
//! URL that parses to the same link.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuee_proto::{Link, PinLink, link::DEFAULT_BASE};

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    if let Ok(link) = Link::parse(&text) {
        let url = link.to_url(DEFAULT_BASE);
        assert_eq!(Link::parse(&url), Ok(link.clone()));
        let local = link.to_url("http://127.0.0.1:8080/");
        assert_eq!(Link::parse(&local), Ok(link));
    }
    if let Ok(pin) = PinLink::parse(&text) {
        let url = pin.to_url(DEFAULT_BASE);
        assert_eq!(PinLink::parse(&url), Ok(pin));
    }
});
