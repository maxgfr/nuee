//! A task that honest peers compute identically never fails for good just because it takes
//! longer than its siblings: once it overran a deadline estimated from a peer's other tasks,
//! its next leases get the job's own deadline, and when peers did run out of attempts, a peer
//! lease still out at that moment cannot fail the task before the coordinator's own
//! last-chance run has answered.

use nuee_core::{Action, Coordinator, CoordinatorConfig, Event, JobSpec, Outcome};
use nuee_proto::{
    BoundedBytes, BoundedString, BoundedVec, Caps, Comparator, FailKind, Hash, Lease, Manifest,
    Msg, Payload, SecretKey, TaskKind, VerifyMode, VerifyPolicy,
};

const DEADLINE_MS: u32 = 60_000;

fn spec(c: &Coordinator, mode: VerifyMode, tasks: u32) -> JobSpec {
    JobSpec {
        manifest: Manifest {
            version: nuee_proto::PROTO_VERSION,
            job: 0,
            name: BoundedString::try_from("slow").expect("name"),
            kind: TaskKind::Wasm,
            module: Some(Hash::of(b"module")),
            module_size: 6,
            max_memory: 16 << 20,
            assets: BoundedVec::new(),
            tasks,
            verify: VerifyPolicy {
                mode,
                sample_permille: 0,
                canary_permille: 0,
            },
            compare: Comparator::Exact,
            deadline_ms: DEADLINE_MS,
            exec: None,
            train: None,
            coordinator: c.id(),
        },
        inputs: (0..tasks)
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
        agent: BoundedString::try_from("nuee-web/0.0.0").expect("agent"),
        browser: true,
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

fn leases_to(actions: &[Action], key: &SecretKey) -> Vec<Lease> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Send {
                to,
                msg: Msg::Lease(l),
            } if *to == key.public() => Some(l.clone()),
            _ => None,
        })
        .collect()
}

/// Asks for one lease for `key`.
fn want(c: &mut Coordinator, now: u64, key: &SecretKey, seen: &mut Vec<Action>) -> Lease {
    let out = c.handle(now, key.public(), Msg::Want { slots: 1 });
    let lease = leases_to(&out, key).pop().expect("a lease");
    seen.extend(out);
    lease
}

fn answer(c: &mut Coordinator, now: u64, key: &SecretKey, lease: &Lease, seen: &mut Vec<Action>) {
    let output = b"true answer".to_vec();
    let output_hash = Hash::of(&output);
    let sig = nuee_proto::sign_result(key, lease.lease, &output_hash);
    seen.extend(c.handle(
        now,
        key.public(),
        Msg::Result {
            lease: lease.lease,
            output: Payload::Inline(BoundedBytes::try_from(output).expect("output")),
            output_hash,
            duration_ms: 1,
            sig,
        },
    ));
}

fn fail(
    c: &mut Coordinator,
    now: u64,
    key: &SecretKey,
    lease: &Lease,
    kind: FailKind,
    seen: &mut Vec<Action>,
) {
    seen.extend(c.handle(
        now,
        key.public(),
        Msg::Fail {
            lease: lease.lease,
            kind,
            detail: BoundedString::new(),
        },
    ));
}

fn failed_tasks(seen: &[Action]) -> Vec<(u32, FailKind)> {
    seen.iter()
        .filter_map(|a| match a {
            Action::Event(Event::TaskFailed { task, kind, .. }) => Some((*task, *kind)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_task_that_overran_its_estimated_deadline_gets_the_jobs_deadline_next() {
    let mut c = Coordinator::new(
        CoordinatorConfig::default(),
        Box::new(SecretKey::from_bytes(&[30; 32])),
    );
    c.submit(0, spec(&c, VerifyMode::None, 3)).expect("submit");
    c.drain();
    let peer = SecretKey::from_bytes(&[31; 32]);
    let mut seen = c.admitted(0, peer.public(), caps());

    // A first quick task gives the peer a short history: its next leases get the floor.
    let first = want(&mut c, 10, &peer, &mut seen);
    assert_eq!(first.deadline_ms, DEADLINE_MS, "no history yet");
    answer(&mut c, 20, &peer, &first, &mut seen);

    // A much longer task: the peer reports that it ran past that estimate.
    let slow = want(&mut c, 30, &peer, &mut seen);
    assert!(slow.deadline_ms < DEADLINE_MS, "an estimated deadline");
    fail(
        &mut c,
        30 + u64::from(slow.deadline_ms),
        &peer,
        &slow,
        FailKind::Timeout,
        &mut seen,
    );

    // Another long one: the peer stays silent and the coordinator notices the overrun.
    let silent = want(&mut c, 6_000, &peer, &mut seen);
    assert_ne!(silent.task, slow.task);
    assert!(silent.deadline_ms < DEADLINE_MS, "an estimated deadline");
    seen.extend(c.tick(6_000 + u64::from(silent.deadline_ms)));

    // Both come back with the job's own deadline, not the estimate they overran.
    let mut now = 12_000;
    for _ in 0..2 {
        let retry = want(&mut c, now, &peer, &mut seen);
        assert!(retry.task == slow.task || retry.task == silent.task);
        assert_eq!(
            retry.deadline_ms, DEADLINE_MS,
            "a task that overran its estimate gets the job's own deadline"
        );
        answer(&mut c, now + 20_000, &peer, &retry, &mut seen);
        now += 30_000;
    }
    assert!(failed_tasks(&seen).is_empty(), "{:?}", failed_tasks(&seen));
}

#[test]
fn a_straggling_peer_failure_waits_for_the_coordinators_last_chance_run() {
    // Verify full, a coordinator that checks (verify slot) but takes no fresh work.
    let mut c = Coordinator::new(
        CoordinatorConfig {
            local_slots: 0,
            verify_slots: 1,
            ..CoordinatorConfig::default()
        },
        Box::new(SecretKey::from_bytes(&[40; 32])),
    );
    c.submit(0, spec(&c, VerifyMode::Full, 1)).expect("submit");
    c.drain();
    let a = SecretKey::from_bytes(&[41; 32]);
    let b = SecretKey::from_bytes(&[42; 32]);
    let d = SecretKey::from_bytes(&[43; 32]);
    let e = SecretKey::from_bytes(&[44; 32]);
    let mut seen = Vec::new();
    for k in [&a, &b, &d, &e] {
        seen.extend(c.admitted(0, k.public(), caps()));
    }

    // Full: two peer executions at once. Three distinct peers fail it in turn (the default
    // three attempts) while e's lease is still out.
    let la = want(&mut c, 10, &a, &mut seen);
    let lb = want(&mut c, 10, &b, &mut seen);
    fail(&mut c, 20, &a, &la, FailKind::Timeout, &mut seen);
    let ld = want(&mut c, 30, &d, &mut seen);
    fail(&mut c, 40, &b, &lb, FailKind::Timeout, &mut seen);
    let le = want(&mut c, 50, &e, &mut seen);
    fail(&mut c, 60, &d, &ld, FailKind::Timeout, &mut seen);
    assert!(
        failed_tasks(&seen).is_empty(),
        "attempts ran out: the coordinator runs the task once itself first"
    );

    // The straggler: e's lease, issued before attempts ran out, fails too.
    fail(&mut c, 70, &e, &le, FailKind::Timeout, &mut seen);
    assert!(
        failed_tasks(&seen).is_empty(),
        "a peer failure while the last-chance run is due failed the task: {:?}",
        failed_tasks(&seen)
    );

    // The coordinator's own run answers and settles the task.
    let out = c.local_slots(80, 1);
    let run = out
        .iter()
        .find_map(|a| match a {
            Action::RunLocal { lease, .. } => Some(lease.clone()),
            _ => None,
        })
        .expect("the last-chance run");
    seen.extend(out);
    seen.extend(c.local_done(90, run.lease, Outcome::Ok(b"true answer".to_vec())));
    assert!(failed_tasks(&seen).is_empty(), "{:?}", failed_tasks(&seen));
    assert!(
        seen.iter()
            .any(|a| matches!(a, Action::Event(Event::JobDone { failed: 0, .. }))),
        "the job ends with every task done"
    );
}
