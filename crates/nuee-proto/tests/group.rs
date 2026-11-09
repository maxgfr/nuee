use nuee_proto::link::DEFAULT_BASE;
use nuee_proto::*;

fn key(n: u8) -> SecretKey {
    SecretKey::from_bytes(&[n; 32])
}

fn member(n: u8, keeper_rank: Option<u16>) -> Member {
    Member {
        id: key(n).public(),
        name: BoundedString::try_from("machine").unwrap(),
        worker: true,
        store: false,
        keeper_rank,
    }
}

fn roster(revision: u64, members: Vec<Member>) -> Roster {
    Roster {
        version: PROTO_VERSION,
        group: key(1).public(),
        revision,
        members: members.try_into().unwrap(),
    }
}

fn manifest(coordinator: EndpointId) -> Manifest {
    Manifest {
        version: PROTO_VERSION,
        job: 1,
        name: BoundedString::try_from("job").unwrap(),
        kind: TaskKind::Wasm,
        module: Some(Hash::of(b"m")),
        module_size: 10,
        max_memory: 1 << 20,
        assets: BoundedVec::new(),
        tasks: 1,
        verify: VerifyPolicy::default(),
        compare: Comparator::Exact,
        deadline_ms: 60_000,
        exec: None,
        train: None,
        coordinator,
    }
}

#[test]
fn a_signed_roster_opens_with_the_group_key_only() {
    let r = roster(1, vec![member(2, Some(0))]);
    let signed = r.sign(&key(1)).unwrap();
    assert_eq!(signed.open(&key(1).public()).unwrap(), r);
    assert!(matches!(
        signed.open(&key(9).public()),
        Err(RosterError::BadSignature)
    ));
}

#[test]
fn only_the_group_key_can_sign() {
    let r = roster(1, vec![member(2, Some(0))]);
    assert!(matches!(r.sign(&key(2)), Err(RosterError::WrongGroup)));
}

#[test]
fn a_tampered_roster_is_refused() {
    let mut signed = roster(1, vec![member(2, Some(0))]).sign(&key(1)).unwrap();
    let mut bytes = signed.bytes.as_slice().to_vec();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    signed.bytes = bytes.try_into().unwrap();
    assert!(matches!(
        signed.open(&key(1).public()),
        Err(RosterError::BadSignature)
    ));
}

#[test]
fn inconsistent_rosters_are_refused() {
    let no_keeper = roster(1, vec![member(2, None)]);
    let twice = roster(1, vec![member(2, Some(0)), member(2, None)]);
    let same_rank = roster(1, vec![member(2, Some(0)), member(3, Some(0))]);
    for r in [no_keeper, twice, same_rank] {
        assert!(matches!(r.check(), Err(RosterError::Invalid(_))));
        assert!(r.sign(&key(1)).is_err());
    }
}

#[test]
fn keepers_come_preferred_first() {
    let r = roster(
        1,
        vec![member(2, Some(5)), member(3, None), member(4, Some(1))],
    );
    let ids: Vec<_> = r.keepers().iter().map(|m| m.id).collect();
    assert_eq!(ids, vec![key(4).public(), key(2).public()]);
}

#[test]
fn an_old_roster_cannot_replace_a_newer_one() {
    let old = roster(1, vec![member(2, Some(0))]);
    let new = roster(2, vec![member(3, Some(0))]);
    assert!(new.replaces(&old));
    assert!(!old.replaces(&new));
    assert!(!new.replaces(&new));
    let mut other_group = roster(3, vec![member(3, Some(0))]);
    other_group.group = key(8).public();
    assert!(!other_group.replaces(&old));
}

#[test]
fn only_a_keeper_can_hand_out_manifests() {
    let r = roster(1, vec![member(2, Some(0)), member(3, None)]);

    let by_keeper = manifest(key(2).public()).sign(&key(2)).unwrap();
    assert!(r.open_manifest(&by_keeper, &key(2).public()).is_ok());

    let by_worker = manifest(key(3).public()).sign(&key(3)).unwrap();
    assert!(matches!(
        r.open_manifest(&by_worker, &key(3).public()),
        Err(ManifestError::WrongCoordinator)
    ));

    let by_stranger = manifest(key(7).public()).sign(&key(7)).unwrap();
    assert!(matches!(
        r.open_manifest(&by_stranger, &key(7).public()),
        Err(ManifestError::WrongCoordinator)
    ));
}

#[test]
fn a_keeper_cannot_pass_off_a_manifest_it_did_not_sign() {
    let r = roster(1, vec![member(2, Some(0)), member(3, Some(1))]);
    let signed_by_other = manifest(key(3).public()).sign(&key(3)).unwrap();
    assert!(matches!(
        r.open_manifest(&signed_by_other, &key(2).public()),
        Err(ManifestError::BadSignature)
    ));
}

fn group_link(bootstrap: Vec<Provider>) -> Link {
    Link::Group(GroupLink {
        group: key(1).public(),
        secret: [4; 16],
        bootstrap: bootstrap.try_into().unwrap(),
    })
}

fn provider(n: u8) -> Provider {
    Provider {
        id: key(n).public(),
        relay: Some(BoundedString::try_from("https://relay.example/").unwrap()),
    }
}

#[test]
fn a_group_link_round_trips() {
    let link = group_link(vec![provider(2), provider(3)]);
    let url = link.to_url(DEFAULT_BASE);
    assert!(url.contains("#/G/"));
    assert_eq!(Link::parse(&url).unwrap(), link);
    assert_eq!(PinLink::parse(&url), Err(LinkError::WrongKind));
}

#[test]
fn a_group_link_with_no_machine_is_corrupt() {
    let url = group_link(Vec::new()).to_url(DEFAULT_BASE);
    assert_eq!(Link::parse(&url), Err(LinkError::Corrupt));
}

#[test]
fn a_group_link_stays_small_enough_for_a_qr_code() {
    let link = group_link(vec![provider(2), provider(3), provider(4), provider(5)]);
    // A version 25 QR code at level M holds about 1 000 alphanumeric characters.
    assert!(link.to_url(DEFAULT_BASE).len() < 1_000);
}
