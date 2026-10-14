//! Attacks on verification: colluding identities in `full`, telling canaries apart by order,
//! and failing tasks on purpose.

use nuee_core::{Action, Coordinator, CoordinatorConfig, Event, JobSpec, Outcome};
use nuee_proto::{
    BoundedBytes, BoundedString, BoundedVec, Caps, Comparator, EndpointId, FailKind, Hash, Lease,
    Manifest, Msg, Payload, SecretKey, TaskKind, VerifyMode, VerifyPolicy,
};

fn key(n: u8) -> SecretKey {
    SecretKey::from_bytes(&[n; 32])
}

fn truth(input: &[u8]) -> Vec<u8> {
    blake3::hash(input).as_bytes().to_vec()
}

fn input(task: u32) -> Vec<u8> {
    task.to_le_bytes().to_vec()
}

fn coordinator(local_slots: u16, verify_slots: u16) -> Coordinator {
    Coordinator::new(
        CoordinatorConfig {
            local_slots,
            verify_slots,
            seed: 7,
            ..CoordinatorConfig::default()
        },
        Box::new(key(0)),
    )
}

fn submit(c: &mut Coordinator, mode: VerifyMode, sample: u16, canary: u16, tasks: u32) {
    let manifest = Manifest {
        version: nuee_proto::PROTO_VERSION,
        job: 0,
        name: BoundedString::try_from("attack").expect("name"),
        kind: TaskKind::Wasm,
        module: Some(Hash::of(b"module")),
        module_size: 6,
        max_memory: 16 << 20,
        assets: BoundedVec::new(),
        tasks,
        verify: VerifyPolicy {
            mode,
            sample_permille: sample,
            canary_permille: canary,
        },
        compare: Comparator::Exact,
        deadline_ms: 60_000,
        exec: None,
        train: None,
        coordinator: c.id(),
    };
    let inputs = (0..tasks)
        .map(|t| Payload::Inline(BoundedBytes::try_from(input(t)).expect("input")))
        .collect();
    c.submit(
        0,
        JobSpec {
            manifest,
            inputs,
            compare: None,
        },
    )
    .expect("submit");
    c.drain();
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

fn leases(out: &[Action], to: EndpointId) -> Vec<Lease> {
    out.iter()
        .filter_map(|a| match a {
            Action::Send {
                to: t,
                msg: Msg::Lease(l),
            } if *t == to => Some(l.clone()),
            _ => None,
        })
        .collect()
}

fn local_runs(out: &[Action]) -> Vec<Lease> {
    out.iter()
        .filter_map(|a| match a {
            Action::RunLocal { lease, .. } => Some(lease.clone()),
            _ => None,
        })
        .collect()
}

fn bytes(p: &Payload) -> Vec<u8> {
    match p {
        Payload::Inline(b) => b.as_slice().to_vec(),
        Payload::Blob { .. } => panic!("inline inputs only"),
    }
}

fn result(n: u8, lease: u64, output: Vec<u8>) -> Msg {
    let output_hash = Hash::of(&output);
    Msg::Result {
        lease,
        output: Payload::Inline(BoundedBytes::try_from(output).expect("output")),
        output_hash,
        duration_ms: 1,
        sig: nuee_proto::sign_result(&key(n), lease, &output_hash),
    }
}

fn fail(lease: u64) -> Msg {
    Msg::Fail {
        lease,
        kind: FailKind::Trap,
        detail: BoundedString::try_from("trap").expect("detail"),
    }
}

fn banned(out: &[Action]) -> Vec<EndpointId> {
    out.iter()
        .filter_map(|a| match a {
            Action::Event(Event::Banned { peer, .. }) => Some(*peer),
            _ => None,
        })
        .collect()
}

fn job_done(out: &[Action]) -> bool {
    out.iter()
        .any(|a| matches!(a, Action::Event(Event::JobDone { .. })))
}

fn task_failed(out: &[Action]) -> bool {
    out.iter()
        .any(|a| matches!(a, Action::Event(Event::TaskFailed { .. })))
}

/// B1: two identities agreeing on a forged output in `full` are caught by the coordinator's
/// sampled recompute and both banned.
#[test]
fn full_mode_colluding_pair_is_recomputed_and_banned() {
    let mut c = coordinator(0, 1);
    submit(&mut c, VerifyMode::Full, 1000, 0, 1);
    let (a, b) = (key(1).public(), key(2).public());
    c.admitted(0, a, caps());
    c.admitted(0, b, caps());
    let la = leases(&c.handle(1, a, Msg::Want { slots: 1 }), a);
    let lb = leases(&c.handle(1, b, Msg::Want { slots: 1 }), b);
    let (la, lb) = (la.first().expect("lease a"), lb.first().expect("lease b"));
    assert_eq!(la.task, lb.task);
    let mut out = c.handle(2, a, result(1, la.lease, b"forged".to_vec()));
    out.extend(c.handle(3, b, result(2, lb.lease, b"forged".to_vec())));
    assert!(
        !out.iter()
            .any(|x| matches!(x, Action::Event(Event::Verified { .. }))),
        "the agreeing pair is not verified before the coordinator's recompute"
    );
    let out = c.local_slots(4, 1);
    let run = local_runs(&out);
    let run = run.first().expect("the coordinator recomputes the pair");
    let out = c.local_done(5, run.lease, Outcome::Ok(truth(&bytes(&run.input))));
    let mut ban = banned(&out);
    ban.sort();
    let mut both = vec![a, b];
    both.sort();
    assert_eq!(ban, both, "both identities of the forger are banned");
    assert!(job_done(&out));
}

/// B1: `full` jobs get canaries once the coordinator holds a known answer.
#[test]
fn full_mode_issues_canaries() {
    let mut c = coordinator(1, 0);
    submit(&mut c, VerifyMode::Full, 0, 1000, 3);
    let out = c.local_slots(0, 1);
    let run = local_runs(&out);
    let run = run.first().expect("a local task");
    let known = run.task;
    c.local_done(1, run.lease, Outcome::Ok(truth(&bytes(&run.input))));
    let a = key(1).public();
    c.admitted(2, a, caps());
    let got = leases(&c.handle(3, a, Msg::Want { slots: 1 }), a);
    let l = got.first().expect("a lease");
    assert_eq!(l.task, known, "a canary of the known answer");
    let out = c.handle(4, a, result(1, l.lease, b"forged".to_vec()));
    assert_eq!(banned(&out), vec![a], "a wrong canary answer bans");
}

/// B2: fresh tasks do not go out in index order, so a lease's index tells nothing.
#[test]
fn fresh_tasks_go_out_in_a_random_order() {
    let mut c = coordinator(0, 0);
    submit(&mut c, VerifyMode::None, 0, 0, 64);
    let a = key(1).public();
    c.admitted(0, a, caps());
    let got: Vec<u32> = leases(&c.handle(1, a, Msg::Want { slots: 32 }), a)
        .iter()
        .map(|l| l.task)
        .collect();
    assert_eq!(got.len(), 32);
    let mut sorted = got.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 32, "distinct tasks");
    assert_ne!(got, (0..32).collect::<Vec<u32>>(), "not FIFO by index");
}

/// B3: three identities answering `Fail` do not fail a task when the coordinator can run it.
#[test]
fn failing_identities_leave_the_task_to_the_coordinator() {
    let mut c = coordinator(0, 1);
    submit(&mut c, VerifyMode::Sample, 0, 0, 1);
    let mut out = Vec::new();
    for n in 1..=3u8 {
        let p = key(n).public();
        c.admitted(0, p, caps());
        let got = leases(&c.handle(1, p, Msg::Want { slots: 1 }), p);
        let l = got.first().expect("a lease");
        out.extend(c.handle(2, p, fail(l.lease)));
    }
    assert!(!task_failed(&out), "peers alone cannot fail the task");
    let out = c.local_slots(3, 1);
    let run = local_runs(&out);
    let run = run.first().expect("the coordinator runs it");
    let out = c.local_done(4, run.lease, Outcome::Ok(truth(&bytes(&run.input))));
    assert!(job_done(&out) && !task_failed(&out));
}

/// B3: attempts count distinct peers: one peer failing again and again does not use them up.
#[test]
fn one_peer_failing_repeatedly_counts_once() {
    let mut c = coordinator(0, 0);
    submit(&mut c, VerifyMode::None, 0, 0, 1);
    let a = key(1).public();
    c.admitted(0, a, caps());
    let mut out = Vec::new();
    let mut got = leases(&c.handle(1, a, Msg::Want { slots: 1 }), a);
    for now in 2..5u64 {
        let l = got.pop().expect("the lone peer retries");
        out.extend(c.handle(now, a, fail(l.lease)));
        if now < 4 {
            got.extend(leases(&c.handle(now, a, Msg::Want { slots: 1 }), a));
        }
    }
    assert!(!task_failed(&out), "three failures of one identity");
    let b = key(2).public();
    c.admitted(10, b, caps());
    let got = leases(&c.handle(11, b, Msg::Want { slots: 1 }), b);
    let l = got.first().expect("an honest peer gets it");
    let out = c.handle(12, b, result(2, l.lease, truth(&bytes(&l.input))));
    assert!(job_done(&out) && !task_failed(&out));
}

/// B3: in degraded mode an unchecked output is never settled because checking leases failed.
#[test]
fn failed_checks_never_settle_an_unchecked_output() {
    let mut c = coordinator(0, 0);
    submit(&mut c, VerifyMode::Sample, 1000, 0, 1);
    let a = key(1).public();
    c.admitted(0, a, caps());
    let got = leases(&c.handle(1, a, Msg::Want { slots: 1 }), a);
    let l = got.first().expect("a lease");
    let truth_out = truth(&bytes(&l.input));
    c.handle(2, a, result(1, l.lease, truth_out.clone()));
    let mut out = Vec::new();
    for n in 2..=4u8 {
        let p = key(n).public();
        out.extend(c.admitted(3, p, caps()));
        let got = leases(&c.handle(4, p, Msg::Want { slots: 1 }), p);
        let l = got.first().expect("a checking lease");
        out.extend(c.handle(5, p, fail(l.lease)));
    }
    assert!(!job_done(&out), "settled unchecked after failed checks");
    let e = key(5).public();
    c.admitted(6, e, caps());
    let got = leases(&c.handle(7, e, Msg::Want { slots: 1 }), e);
    let l = got.first().expect("a fresh checker gets it");
    let out = c.handle(8, e, result(5, l.lease, truth_out));
    assert!(job_done(&out));
    assert!(
        out.iter()
            .any(|x| matches!(x, Action::Event(Event::Verified { .. })))
    );
}

/// B3: a peer that fails a job far more often than it succeeds stops receiving its leases.
#[test]
fn a_peer_failing_most_tasks_stops_getting_the_job() {
    let mut c = coordinator(0, 0);
    submit(&mut c, VerifyMode::None, 0, 0, 20);
    let a = key(1).public();
    let b = key(2).public();
    c.admitted(0, a, caps());
    c.admitted(0, b, caps());
    let mut failed = 0;
    let mut got = leases(&c.handle(1, a, Msg::Want { slots: 1 }), a);
    for now in 2..30u64 {
        let Some(l) = got.pop() else {
            break;
        };
        c.handle(now, a, fail(l.lease));
        failed += 1;
        got.extend(leases(&c.handle(now, a, Msg::Want { slots: 1 }), a));
    }
    assert_eq!(failed, 4, "leases stop after the failure rate is judged");
}
