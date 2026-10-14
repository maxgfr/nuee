use nuee_proto::*;

fn manifest(coordinator: EndpointId) -> Manifest {
    Manifest {
        version: PROTO_VERSION,
        job: 3,
        name: BoundedString::try_from("job").unwrap(),
        kind: TaskKind::Wasm,
        module: Some(Hash::of(b"m")),
        module_size: 10,
        max_memory: 1 << 20,
        assets: vec![Asset {
            hash: Hash::of(b"a"),
            size: 3,
            name: BoundedString::try_from("a.bin").unwrap(),
        }]
        .try_into()
        .unwrap(),
        tasks: 10,
        verify: VerifyPolicy::default(),
        compare: Comparator::ElementTolerance {
            abs: 1e-3,
            rel: 1e-3,
            max_bad_permille: 5,
        },
        deadline_ms: 60_000,
        exec: None,
        train: None,
        coordinator,
    }
}

#[test]
fn sign_and_open() {
    let k = SecretKey::from_bytes(&[1; 32]);
    let m = manifest(k.public());
    let signed = m.sign(&k).unwrap();
    assert_eq!(signed.open(&k.public()).unwrap(), m);
}

#[test]
fn tampered_byte_is_rejected() {
    let k = SecretKey::from_bytes(&[1; 32]);
    let signed = manifest(k.public()).sign(&k).unwrap();
    for i in 0..signed.bytes.len() {
        let mut raw = signed.bytes.to_vec();
        raw[i] ^= 1;
        let t = SignedManifest {
            bytes: raw.try_into().unwrap(),
            sig: signed.sig,
        };
        assert!(matches!(
            t.open(&k.public()),
            Err(ManifestError::BadSignature)
        ));
    }
}

#[test]
fn wrong_coordinator_is_rejected() {
    let k = SecretKey::from_bytes(&[1; 32]);
    let other = SecretKey::from_bytes(&[2; 32]);
    let signed = manifest(k.public()).sign(&k).unwrap();
    assert!(signed.open(&other.public()).is_err());
    // Signed by `other` but naming `k` as coordinator.
    let forged = manifest(k.public()).sign(&other).unwrap();
    assert!(matches!(
        forged.open(&other.public()),
        Err(ManifestError::WrongCoordinator)
    ));
    assert!(matches!(
        forged.open(&k.public()),
        Err(ManifestError::BadSignature)
    ));
}

#[test]
fn limits_are_enforced_on_open() {
    let k = SecretKey::from_bytes(&[1; 32]);
    let mut m = manifest(k.public());
    m.max_memory = limits::MAX_WASM_MEMORY + 1;
    assert!(matches!(
        m.sign(&k).unwrap().open(&k.public()),
        Err(ManifestError::Invalid(_))
    ));
    let mut m = manifest(k.public());
    m.kind = TaskKind::Exec;
    m.exec = Some(ExecSpec {
        repo: BoundedString::try_from("https://github.com/maxgfr/skycmd").unwrap(),
        rev: BoundedString::try_from("781D4C176B2D6E301D0EEB82C69781B356505EC0").unwrap(),
        argv: BoundedVec::new(),
        env: BoundedVec::new(),
        timeout_s: 60,
        gpu: false,
    });
    assert!(matches!(
        m.sign(&k).unwrap().open(&k.public()),
        Err(ManifestError::Invalid(_))
    ));
    let mut m = manifest(k.public());
    m.version = 9;
    assert!(matches!(
        m.sign(&k).unwrap().open(&k.public()),
        Err(ManifestError::Version(9))
    ));
}
