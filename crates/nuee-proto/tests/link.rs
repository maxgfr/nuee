use nuee_proto::link::DEFAULT_BASE;
use nuee_proto::*;

fn bs<const N: usize>(s: &str) -> BoundedString<N> {
    BoundedString::try_from(s).unwrap()
}

fn file_link() -> FileLink {
    FileLink {
        providers: vec![
            Provider {
                id: SecretKey::from_bytes(&[1; 32]).public(),
                relay: Some(bs("https://relay.example/")),
            },
            Provider {
                id: SecretKey::from_bytes(&[2; 32]).public(),
                relay: None,
            },
        ]
        .try_into()
        .unwrap(),
        hash: Hash::of(b"ciphertext"),
        size: 123_456,
        access: [7; 16],
        key: [8; 32],
    }
}

fn all_links() -> Vec<Link> {
    let id = SecretKey::from_bytes(&[3; 32]).public();
    vec![
        Link::Job(JobLink {
            coordinator: id,
            relay: bs("https://relay.example/"),
            secret: [1; 16],
        }),
        Link::File(file_link()),
        Link::Service(ServiceLink {
            host: id,
            relay: bs("https://relay.example/"),
            secret: [2; 16],
        }),
        Link::Site(file_link()),
    ]
}

#[test]
fn roundtrip_every_kind_and_form() {
    for link in all_links() {
        let url = link.to_url(DEFAULT_BASE);
        assert!(url.starts_with("https://maxgfr.github.io/nuee/#/"));
        let payload = url.rsplit('/').next().unwrap();
        assert!(
            payload
                .chars()
                .all(|c| c.is_ascii_uppercase() || ('2'..='7').contains(&c))
        );
        assert_eq!(Link::parse(&url).unwrap(), link);
        let fragment = &url[url.find('#').unwrap()..];
        assert_eq!(Link::parse(fragment).unwrap(), link);
        let short = &fragment[2..];
        assert_eq!(Link::parse(short).unwrap(), link);
        assert_eq!(Link::parse(&format!("  {url}\n")).unwrap(), link);
        let local = link.to_url("http://127.0.0.1:8080/");
        assert!(local.starts_with("http://127.0.0.1:8080/#/"));
        assert_eq!(Link::parse(&local).unwrap(), link);
    }
}

#[test]
fn every_truncation_is_detected() {
    for link in all_links() {
        let url = link.to_url(DEFAULT_BASE);
        for cut in 0..url.len() {
            let r = Link::parse(&url[..cut]);
            assert!(
                matches!(r, Err(LinkError::Truncated) | Err(LinkError::Corrupt)),
                "cut {cut}: {r:?}"
            );
        }
    }
}

#[test]
fn unknown_version_is_reported() {
    let url = all_links()[0].to_url(DEFAULT_BASE);
    let payload = url.rsplit('/').next().unwrap();
    let mut bytes = data_encoding::BASE32_NOPAD
        .decode(payload.as_bytes())
        .unwrap();
    bytes[0] = 2;
    let n = bytes.len() - 4;
    let sum = blake3::hash(&bytes[..n]);
    bytes[n..].copy_from_slice(&sum.as_bytes()[..4]);
    let forged = format!("J/{}", data_encoding::BASE32_NOPAD.encode(&bytes));
    assert_eq!(Link::parse(&forged), Err(LinkError::UnsupportedVersion(2)));
}

#[test]
fn checksum_valid_garbage_is_corrupt() {
    let mut bytes = vec![1u8, 0xff, 0xff];
    let sum = blake3::hash(&bytes);
    bytes.extend_from_slice(&sum.as_bytes()[..4]);
    let s = format!("F/{}", data_encoding::BASE32_NOPAD.encode(&bytes));
    assert_eq!(Link::parse(&s), Err(LinkError::Corrupt));
}

#[test]
fn pin_links_have_no_key_and_their_own_kind() {
    let pin = file_link().pin_link();
    let url = pin.to_url(DEFAULT_BASE);
    assert_eq!(PinLink::parse(&url).unwrap(), pin);
    assert_eq!(Link::parse(&url), Err(LinkError::WrongKind));
    let file_url = Link::File(file_link()).to_url(DEFAULT_BASE);
    assert_eq!(PinLink::parse(&file_url), Err(LinkError::WrongKind));
}
