//! A worker keeps at most `MAX_JOBS` manifests: the oldest goes first, and a lease for an
//! evicted job is refused rather than run.

use nuee_core::{Lease, Worker, WorkerAction, WorkerConfig, WorkerEvent};
use nuee_proto::limits::MAX_JOBS;
use nuee_proto::{
    BoundedBytes, BoundedString, BoundedVec, Caps, Comparator, FailKind, Hash, LocalLimits,
    Manifest, Msg, Payload, SecretKey, TaskKind, VerifyMode, VerifyPolicy,
};

fn caps() -> Caps {
    Caps {
        agent: BoundedString::try_from("test").expect("agent"),
        browser: false,
        cpu_slots: 2,
        gpu: None,
        mem_mb: 1024,
        kinds: TaskKind::Wasm.bit(),
        // The module is cached, so adding a job does not ask for a fetch.
        cached: BoundedVec::try_from(vec![Hash::of(b"module")]).expect("cached"),
        exec_allow: BoundedVec::new(),
        store_quota_mb: 0,
        relay: None,
        rtc: false,
    }
}

fn job(coordinator: &SecretKey, job: u32) -> Msg {
    let m = Manifest {
        version: nuee_proto::PROTO_VERSION,
        job,
        name: BoundedString::try_from("job").expect("name"),
        kind: TaskKind::Wasm,
        module: Some(Hash::of(b"module")),
        module_size: 6,
        max_memory: 16 << 20,
        assets: BoundedVec::new(),
        tasks: 1,
        verify: VerifyPolicy {
            mode: VerifyMode::None,
            sample_permille: 0,
            canary_permille: 0,
        },
        compare: Comparator::Exact,
        deadline_ms: 60_000,
        exec: None,
        train: None,
        coordinator: coordinator.public(),
    };
    Msg::Job(m.sign(coordinator).expect("sign"))
}

fn lease(id: u64, job: u32) -> Msg {
    Msg::Lease(Lease {
        lease: id,
        job,
        task: 0,
        kind: TaskKind::Wasm,
        module: Some(Hash::of(b"module")),
        assets: BoundedVec::new(),
        input: Payload::Inline(BoundedBytes::try_from(b"x".to_vec()).expect("input")),
        deadline_ms: 60_000,
    })
}

fn added(actions: &[WorkerAction]) -> Option<u32> {
    actions.iter().find_map(|a| match a {
        WorkerAction::Event(WorkerEvent::JobAdded { job, .. }) => Some(*job),
        _ => None,
    })
}

#[test]
fn the_oldest_job_is_evicted_and_its_leases_refused() {
    let ckey = SecretKey::from_bytes(&[3; 32]);
    let mut w = Worker::new(
        WorkerConfig {
            caps: caps(),
            limits: LocalLimits::default(),
        },
        ckey.public(),
        Box::new(SecretKey::from_bytes(&[4; 32])),
    );
    let total = u32::try_from(MAX_JOBS).expect("fits") + 1;
    for j in 0..total {
        let out = w.handle(1, job(&ckey, j));
        assert_eq!(added(&out), Some(j), "job {j}: {out:?}");
    }

    // Job 0 was evicted: its lease is refused with a Fail, never run.
    let out = w.handle(2, lease(100, 0));
    assert!(
        !out.iter().any(|a| matches!(a, WorkerAction::Run { .. })),
        "{out:?}"
    );
    assert!(
        out.iter().any(|a| matches!(
            a,
            WorkerAction::Send(Msg::Fail {
                lease: 100,
                kind: FailKind::Invalid,
                ..
            })
        )),
        "{out:?}"
    );
    assert!(
        out.iter().any(|a| matches!(
            a,
            WorkerAction::Event(WorkerEvent::LeaseRefused { lease: 100, .. })
        )),
        "{out:?}"
    );

    // Job 1 (the oldest kept) and the newest still run.
    for (id, j) in [(101, 1), (102, total - 1)] {
        let out = w.handle(3, lease(id, j));
        assert!(
            out.iter()
                .any(|a| matches!(a, WorkerAction::Run { lease, .. } if lease.job == j)),
            "job {j}: {out:?}"
        );
    }

    // An older job than every kept one is rejected instead of evicting a newer one.
    let out = w.handle(4, job(&ckey, 0));
    assert_eq!(added(&out), None, "{out:?}");
    assert!(
        out.iter()
            .any(|a| matches!(a, WorkerAction::Event(WorkerEvent::JobRejected { .. }))),
        "{out:?}"
    );
}
