use nuee_proto::*;
use proptest::prelude::*;
use serde::{Serialize, de::DeserializeOwned};

fn key(seed: u8) -> SecretKey {
    SecretKey::from_bytes(&[seed; 32])
}

fn roundtrip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(v: &T) {
    let bytes = encode(v).expect("encode");
    let back: T = decode(&bytes).expect("decode");
    assert_eq!(&back, v);
}

fn bs<const N: usize>(s: &str) -> BoundedString<N> {
    BoundedString::try_from(s).expect("fits")
}

fn caps() -> Caps {
    Caps {
        agent: bs("nuee/0.1 test"),
        browser: true,
        cpu_slots: 4,
        gpu: Some(GpuCaps {
            adapter: bs("Test GPU"),
            max_buffer: 1 << 28,
            max_storage_binding: 1 << 27,
            f16: true,
        }),
        mem_mb: 2048,
        kinds: TaskKind::Wasm.bit() | TaskKind::Wgsl.bit(),
        cached: vec![Hash::of(b"m")].try_into().unwrap(),
        exec_allow: vec![bs("https://github.com/maxgfr/skycmd")]
            .try_into()
            .unwrap(),
        store_quota_mb: 512,
        relay: Some(bs("https://relay.example/")),
        rtc: false,
    }
}

fn sample_manifest(coordinator: EndpointId) -> Manifest {
    Manifest {
        version: PROTO_VERSION,
        job: 1,
        name: bs("fractal"),
        kind: TaskKind::Wasm,
        module: Some(Hash::of(b"module")),
        module_size: 1000,
        max_memory: 64 << 20,
        assets: BoundedVec::new(),
        tasks: 256,
        verify: VerifyPolicy::default(),
        compare: Comparator::Exact,
        deadline_ms: 60_000,
        exec: None,
        train: None,
        coordinator,
    }
}

#[test]
fn every_message_roundtrips() {
    let k = key(1);
    let other = key(2).public();
    let signed = sample_manifest(k.public()).sign(&k).unwrap();
    let h = Hash::of(b"out");
    let msgs = vec![
        Msg::Hello {
            version: PROTO_VERSION,
            proof: [9; 32],
            caps: caps(),
        },
        Msg::Pending,
        Msg::Welcome {
            swarm: SwarmInfo {
                name: bs("demo"),
                coordinator: k.public(),
                mode: SwarmMode::Files,
                file: Some(FileInfo { hash: h, size: 42 }),
            },
            jobs: vec![signed.clone()].try_into().unwrap(),
            webrtc: true,
            stun: vec![bs("stun:stun.example:3478")].try_into().unwrap(),
        },
        Msg::Job(signed),
        Msg::Want { slots: 3 },
        Msg::Lease(Lease {
            lease: 7,
            job: 1,
            task: 2,
            kind: TaskKind::Wasm,
            module: Some(h),
            assets: vec![h, h].try_into().unwrap(),
            input: Payload::Inline(BoundedBytes::try_from(&b"input"[..]).unwrap()),
            deadline_ms: 5000,
        }),
        Msg::Result {
            lease: 7,
            output: Payload::Blob {
                hash: h,
                size: 2 << 20,
            },
            output_hash: h,
            duration_ms: 12,
            sig: sign_result(&k, 7, &h),
        },
        Msg::Fail {
            lease: 7,
            kind: FailKind::Oom,
            detail: bs("out of memory"),
        },
        Msg::RtcOffer(RtcSignal {
            peer: other,
            payload: bs("v=0"),
        }),
        Msg::RtcAnswer(RtcSignal {
            peer: other,
            payload: bs("v=0"),
        }),
        Msg::RtcIce(RtcSignal {
            peer: other,
            payload: bs("candidate"),
        }),
        Msg::Cancel { lease: 7 },
        Msg::Bye {
            reason: ByeReason::Banned,
        },
    ];
    for m in &msgs {
        roundtrip(m);
    }

    let provider = Provider {
        id: other,
        relay: Some(bs("https://r/")),
    };
    let pins = vec![
        PinMsg::Hello {
            version: 0,
            proof: [1; 32],
        },
        PinMsg::Replicate {
            hash: h,
            size: 10,
            providers: vec![provider].try_into().unwrap(),
            access: [3; 16],
            ttl_s: 3600,
        },
        PinMsg::Release { hash: h },
        PinMsg::List,
        PinMsg::Pins {
            items: vec![PinInfo {
                hash: h,
                size: 10,
                complete: true,
            }]
            .try_into()
            .unwrap(),
        },
        PinMsg::Accepted { hash: h },
        PinMsg::Refused {
            hash: h,
            reason: PinRefusal::Quota,
        },
        PinMsg::Done { hash: h },
        PinMsg::ReplicateSet {
            header: vec![7u8; MAX_ERASURE_HEADER].try_into().unwrap(),
            providers: vec![Provider {
                id: other,
                relay: None,
            }]
            .try_into()
            .unwrap(),
            access: [4; 16],
        },
        PinMsg::Where { hash: h },
        PinMsg::Holders {
            hash: h,
            providers: BoundedVec::new(),
        },
        PinMsg::Holders {
            hash: h,
            providers: vec![
                Provider {
                    id: other,
                    relay: Some(bs("https://r/")),
                };
                4
            ]
            .try_into()
            .unwrap(),
        },
    ];
    for m in &pins {
        roundtrip(m);
    }
    for m in &[
        TcpMsg::Hello {
            version: 0,
            proof: [2; 32],
        },
        TcpMsg::Ready,
        TcpMsg::Refused { reason: bs("busy") },
    ] {
        roundtrip(m);
    }
    let header = Header {
        name: bs("content-type"),
        value: bs("text/plain"),
    };
    roundtrip(&HttpRequest {
        proof: [4; 32],
        method: bs("GET"),
        path: bs("/api?x=1"),
        headers: vec![header.clone()].try_into().unwrap(),
        body: BoundedBytes::new(),
    });
    roundtrip(&HttpResponse {
        status: 200,
        headers: vec![header].try_into().unwrap(),
        body: BoundedBytes::try_from(&b"hello"[..]).unwrap(),
    });
}

#[test]
fn trailing_bytes_and_oversize_are_rejected() {
    let mut bytes = encode(&Msg::Pending).unwrap();
    bytes.push(0);
    assert!(matches!(
        decode::<Msg>(&bytes),
        Err(ProtoError::Trailing(1))
    ));
    let big = vec![0u8; limits::MAX_MSG + 1];
    assert!(matches!(
        decode::<Msg>(&big),
        Err(ProtoError::TooLarge { .. })
    ));
}

#[test]
fn huge_length_prefixes_fail_fast() {
    // Msg::Hello { version, proof, caps } with caps.cached claiming u32::MAX entries.
    let c = caps();
    let head = encode(&(
        0u32,
        0u16,
        [0u8; 32],
        &c.agent,
        c.browser,
        c.cpu_slots,
        &c.gpu,
        c.mem_mb,
        c.kinds,
    ))
    .unwrap();
    let tail = encode(&(&c.exec_allow, c.store_quota_mb, &c.relay, c.rtc)).unwrap();
    // Sanity check: the same prefix with a real (empty) count decodes.
    let mut good = head.clone();
    good.push(0);
    good.extend_from_slice(&tail);
    assert!(decode::<Msg>(&good).is_ok());
    let mut bytes = head;
    bytes.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0x0f]);
    bytes.extend_from_slice(&tail);
    let start = std::time::Instant::now();
    assert!(decode::<Msg>(&bytes).is_err());
    assert!(start.elapsed().as_millis() < 500);

    // Bounded containers alone.
    let prefix = [0xff, 0xff, 0xff, 0xff, 0x0f];
    assert!(decode::<BoundedVec<u64, 4>>(&prefix).is_err());
    assert!(decode::<BoundedBytes<4>>(&prefix).is_err());
    assert!(decode::<BoundedString<4>>(&prefix).is_err());
    // Exactly at and just above the cap.
    assert!(decode::<BoundedBytes<4>>(&encode(&vec![1u8; 4].as_slice()).unwrap()).is_ok());
    assert!(decode::<BoundedBytes<4>>(&encode(&[1u8; 5].as_slice()).unwrap()).is_err());
    assert!(decode::<BoundedVec<u8, 2>>(&encode(&vec![1u8, 2, 3]).unwrap()).is_err());
    assert!(decode::<BoundedString<3>>(&encode(&"abcd").unwrap()).is_err());
}

#[test]
fn bounded_constructors_check_caps() {
    assert!(BoundedString::<3>::try_from("abcd").is_err());
    assert!(BoundedBytes::<2>::try_from(vec![1, 2, 3]).is_err());
    let mut v = BoundedVec::<u8, 1>::new();
    assert!(v.push(1).is_ok());
    assert!(v.push(2).is_err());
}

#[test]
fn proofs_bind_both_identities() {
    let secret = [5u8; 16];
    let a = key(1).public();
    let b = key(2).public();
    let p = proof(PROOF_HELLO, &secret, &a, &b);
    assert!(check_proof(PROOF_HELLO, &secret, &a, &b, &p));
    assert!(!check_proof(PROOF_HELLO, &secret, &b, &a, &p));
    assert!(!check_proof(PROOF_HELLO, &[6u8; 16], &a, &b, &p));
    assert!(!check_proof(PROOF_PIN, &secret, &a, &b, &p));
}

#[test]
fn result_signatures_verify() {
    let k = key(3);
    let h = Hash::of(b"x");
    let sig = sign_result(&k, 9, &h);
    assert!(verify_result(&k.public(), 9, &h, &sig));
    assert!(!verify_result(&k.public(), 10, &h, &sig));
    assert!(!verify_result(&key(4).public(), 9, &h, &sig));
}

#[test]
fn hash_hex_roundtrip() {
    let h = Hash::of(b"abc");
    let s = h.to_string();
    assert_eq!(s.len(), 64);
    assert_eq!(s.parse::<Hash>().unwrap(), h);
    assert!("zz".parse::<Hash>().is_err());
}

#[test]
fn secrets_are_random() {
    assert_ne!(new_secret16().unwrap(), new_secret16().unwrap());
}

#[test]
fn task_kind_bits_are_distinct() {
    let all = TaskKind::ALL.iter().fold(0u32, |acc, k| {
        assert_eq!(acc & k.bit(), 0);
        acc | k.bit()
    });
    assert_eq!(all, 0x7f);
    assert!(caps().supports(TaskKind::Wgsl));
    assert!(!caps().supports(TaskKind::Exec));
}

fn payload() -> impl Strategy<Value = Payload> {
    prop_oneof![
        proptest::collection::vec(any::<u8>(), 0..64)
            .prop_map(|v| Payload::Inline(BoundedBytes::try_from(v).unwrap())),
        (any::<[u8; 32]>(), any::<u64>()).prop_map(|(h, size)| Payload::Blob {
            hash: Hash(h),
            size
        }),
    ]
}

fn fail_kind() -> impl Strategy<Value = FailKind> {
    prop_oneof![
        Just(FailKind::Trap),
        Just(FailKind::Oom),
        Just(FailKind::Timeout),
        Just(FailKind::StackOverflow),
        Just(FailKind::Invalid),
        Just(FailKind::Unsupported),
        Just(FailKind::Cancelled),
        Just(FailKind::Io),
    ]
}

proptest! {
    #[test]
    fn lease_roundtrips(lease in any::<u64>(), job in any::<u32>(), task in any::<u32>(),
                        kind in 0usize..7, assets in proptest::collection::vec(any::<[u8; 32]>(), 0..16),
                        input in payload(), deadline in any::<u32>()) {
        roundtrip(&Msg::Lease(Lease {
            lease, job, task, kind: TaskKind::ALL[kind], module: assets.first().map(|h| Hash(*h)),
            assets: assets.into_iter().map(Hash).collect::<Vec<_>>().try_into().unwrap(),
            input, deadline_ms: deadline,
        }));
    }

    #[test]
    fn result_and_fail_roundtrip(lease in any::<u64>(), output in payload(), d in any::<u32>(),
                                 kind in fail_kind(), detail in "[a-z ]{0,64}") {
        let k = key(1);
        let h = Hash::of(&lease.to_le_bytes());
        roundtrip(&Msg::Result { lease, output, output_hash: h, duration_ms: d, sig: sign_result(&k, lease, &h) });
        roundtrip(&Msg::Fail { lease, kind, detail: bs(&detail) });
    }

    #[test]
    fn hello_roundtrips(slots in any::<u16>(), mem in any::<u32>(), kinds in any::<u32>(), proof in any::<[u8; 32]>()) {
        roundtrip(&Msg::Hello { version: 0, proof, caps: Caps { cpu_slots: slots, mem_mb: mem, kinds, ..caps() } });
    }

    #[test]
    fn random_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = decode::<Msg>(&bytes);
        let _ = decode::<PinMsg>(&bytes);
        let _ = decode::<TcpMsg>(&bytes);
        let _ = decode::<HttpRequest>(&bytes);
        let _ = decode::<Manifest>(&bytes);
    }
}

/// Fixed HMAC-SHA256 vector, cross-checked against an independent HMAC implementation.
#[test]
fn proof_matches_known_answer() {
    let p = proof(PROOF_HELLO, &[5u8; 16], &key(1).public(), &key(2).public());
    let hex: String = p.iter().map(|v| format!("{v:02x}")).collect();
    assert_eq!(
        hex,
        "98321c19b5440b9702418e25eb7494a6a48678824a7d5c09eec681e83f28c0f7"
    );
}
