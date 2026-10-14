//! A task verified only by two peers agreeing is never a known answer and never bans anyone:
//! a ban follows the coordinator's own recompute (plan S-004).

use std::collections::BTreeSet;

use nuee_core::{Action, Coordinator, CoordinatorConfig, Event, JobSpec};
use nuee_proto::{
    BoundedBytes, BoundedString, BoundedVec, Caps, Comparator, EndpointId, Hash, Manifest, Msg,
    Payload, SecretKey, TaskKind, VerifyMode, VerifyPolicy,
};

const TASKS: u32 = 4;

fn spec(c: &Coordinator) -> JobSpec {
    JobSpec {
        manifest: Manifest {
            version: nuee_proto::PROTO_VERSION,
            job: 0,
            name: BoundedString::try_from("sybil").expect("name"),
            kind: TaskKind::Wasm,
            module: Some(Hash::of(b"module")),
            module_size: 6,
            max_memory: 16 << 20,
            assets: BoundedVec::new(),
            tasks: TASKS,
            verify: VerifyPolicy {
                mode: VerifyMode::Sample,
                sample_permille: 1000,
                canary_permille: 1000,
            },
            compare: Comparator::Exact,
            deadline_ms: 60_000,
            exec: None,
            train: None,
            coordinator: c.id(),
        },
        inputs: (0..TASKS)
            .map(|i| {
                Payload::Inline(
                    BoundedBytes::try_from(vec![u8::try_from(i).unwrap_or(0)]).expect("input"),
                )
            })
            .collect(),
        compare: None,
    }
}

fn caps() -> Caps {
    Caps {
        agent: BoundedString::try_from("cli").expect("agent"),
        browser: false,
        cpu_slots: 1,
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

/// Asks for one task and answers it with `answer`; returns the task and the actions seen.
fn work(
    c: &mut Coordinator,
    now: u64,
    key: &SecretKey,
    answer: &[u8],
    seen: &mut Vec<Action>,
) -> Option<u32> {
    let peer = key.public();
    let out = c.handle(now, peer, Msg::Want { slots: 1 });
    let lease = out.iter().find_map(|a| match a {
        Action::Send {
            msg: Msg::Lease(l), ..
        } => Some((l.lease, l.task)),
        _ => None,
    });
    seen.extend(out);
    let (lease, task) = lease?;
    let output_hash = Hash::of(answer);
    let sig = nuee_proto::sign_result(key, lease, &output_hash);
    let out = c.handle(
        now + 1,
        peer,
        Msg::Result {
            lease,
            output: Payload::Inline(BoundedBytes::try_from(answer.to_vec()).expect("output")),
            output_hash,
            duration_ms: 1,
            sig,
        },
    );
    seen.extend(out);
    Some(task)
}

fn banned(seen: &[Action], peer: EndpointId) -> bool {
    seen.iter()
        .any(|a| matches!(a, Action::Event(Event::Banned { peer: p, .. }) if *p == peer))
}

#[test]
fn peer_agreement_never_serves_as_a_canary_nor_bans() {
    // A coordinator that cannot run the kind at all (an exec job without --allow-exec).
    let mut c = Coordinator::new(
        CoordinatorConfig {
            local_slots: 0,
            verify_slots: 0,
            ..CoordinatorConfig::default()
        },
        Box::new(SecretKey::from_bytes(&[3; 32])),
    );
    c.submit(0, spec(&c)).expect("submit");
    c.drain();
    let a = SecretKey::from_bytes(&[4; 32]);
    let b = SecretKey::from_bytes(&[5; 32]);
    let honest = SecretKey::from_bytes(&[6; 32]);
    for k in [&a, &b, &honest] {
        c.admitted(0, k.public(), caps());
    }

    // Two Sybil peers agree on the same wrong answer, so some tasks become verified.
    let mut seen = Vec::new();
    let mut now = 10;
    for _ in 0..3 {
        work(&mut c, now, &a, b"x", &mut seen);
        work(&mut c, now + 2, &b, b"x", &mut seen);
        now += 10;
    }
    let verified: BTreeSet<u32> = seen
        .iter()
        .filter_map(|a| match a {
            Action::Event(Event::Verified { task, .. }) => Some(*task),
            _ => None,
        })
        .collect();
    assert!(
        !verified.is_empty(),
        "the Sybil pair verified a task: {seen:?}"
    );

    // The honest peer asks for work repeatedly and always answers the true result.
    let mut got = Vec::new();
    for _ in 0..4 {
        if let Some(task) = work(&mut c, now, &honest, b"y", &mut seen) {
            got.push(task);
        }
        now += 10;
    }
    assert!(
        got.iter().all(|t| !verified.contains(t)),
        "a peer-verified answer was used as a known answer: {got:?} vs {verified:?}"
    );
    assert!(
        !banned(&seen, honest.public()),
        "the honest peer was banned without a coordinator recompute"
    );
    let status = c.status();
    let row = status
        .peers
        .iter()
        .find(|p| p.id == honest.public())
        .expect("the honest peer is listed");
    assert!(!row.banned);
}
