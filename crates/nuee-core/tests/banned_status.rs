//! A banned peer keeps, in status snapshots, what the coordinator knew about it.

use nuee_core::{Action, Coordinator, CoordinatorConfig, Event, JobSpec, Outcome};
use nuee_proto::{
    BoundedBytes, BoundedString, BoundedVec, Caps, Comparator, Hash, Manifest, Msg, Payload,
    SecretKey, TaskKind, VerifyMode, VerifyPolicy,
};

fn spec(c: &Coordinator) -> JobSpec {
    JobSpec {
        manifest: Manifest {
            version: nuee_proto::PROTO_VERSION,
            job: 0,
            name: BoundedString::try_from("ban").expect("name"),
            kind: TaskKind::Wasm,
            module: Some(Hash::of(b"module")),
            module_size: 6,
            max_memory: 16 << 20,
            assets: BoundedVec::new(),
            tasks: 2,
            verify: VerifyPolicy {
                mode: VerifyMode::Sample,
                sample_permille: 1000,
                canary_permille: 0,
            },
            compare: Comparator::Exact,
            deadline_ms: 60_000,
            exec: None,
            train: None,
            coordinator: c.id(),
        },
        inputs: (0..2u8)
            .map(|i| Payload::Inline(BoundedBytes::try_from(vec![i]).expect("input")))
            .collect(),
        compare: None,
    }
}

fn browser_caps() -> Caps {
    Caps {
        agent: BoundedString::try_from("web").expect("agent"),
        browser: true,
        cpu_slots: 3,
        gpu: None,
        mem_mb: 1024,
        kinds: TaskKind::Wasm.bit(),
        cached: BoundedVec::new(),
        exec_allow: BoundedVec::new(),
        store_quota_mb: 0,
        relay: None,
        rtc: false,
    }
}

#[test]
fn a_banned_browser_peer_is_still_reported_as_a_browser() {
    let mut c = Coordinator::new(
        CoordinatorConfig {
            local_slots: 0,
            verify_slots: 1,
            ..CoordinatorConfig::default()
        },
        Box::new(SecretKey::from_bytes(&[3; 32])),
    );
    c.submit(0, spec(&c)).expect("submit");
    c.drain();
    let tab_key = SecretKey::from_bytes(&[4; 32]);
    let tab = tab_key.public();
    c.admitted(0, tab, browser_caps());
    let out = c.handle(0, tab, Msg::Want { slots: 1 });
    let lease = out
        .iter()
        .find_map(|a| match a {
            Action::Send {
                msg: Msg::Lease(l), ..
            } => Some(l.lease),
            _ => None,
        })
        .expect("a lease for the tab");

    // The tab lies; the coordinator's own execution confirms the mismatch.
    let wrong = b"wrong".to_vec();
    let output_hash = Hash::of(&wrong);
    let sig = nuee_proto::sign_result(&tab_key, lease, &output_hash);
    let mut out = c.handle(
        10,
        tab,
        Msg::Result {
            lease,
            output: Payload::Inline(BoundedBytes::try_from(wrong).expect("output")),
            output_hash,
            duration_ms: 1,
            sig,
        },
    );
    out.extend(c.local_slots(20, 1));
    let run = out
        .iter()
        .find_map(|a| match a {
            Action::RunLocal { lease, .. } => Some(lease.lease),
            _ => None,
        })
        .expect("a coordinator check");
    let out = c.local_done(30, run, Outcome::Ok(b"right".to_vec()));
    assert!(
        out.iter()
            .any(|a| matches!(a, Action::Event(Event::Banned { peer, .. }) if *peer == tab)),
        "{out:?}"
    );

    let status = c.status();
    let row = status
        .peers
        .iter()
        .find(|p| p.id == tab)
        .expect("the banned tab is listed");
    assert!(row.banned);
    assert!(row.browser, "a banned browser tab is not a command line");
    assert_eq!(row.slots, 3);
}
