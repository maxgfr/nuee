//! Finished jobs can be forgotten, which frees their slot among the `MAX_JOBS`.

use nuee_core::{Action, Coordinator, CoordinatorConfig, CoreError, Event, JobSpec, Outcome};
use nuee_proto::limits::MAX_JOBS;
use nuee_proto::{
    BoundedBytes, BoundedString, BoundedVec, Comparator, Hash, Manifest, Payload, SecretKey,
    TaskKind, VerifyMode, VerifyPolicy,
};

fn spec(c: &Coordinator) -> JobSpec {
    JobSpec {
        manifest: Manifest {
            version: nuee_proto::PROTO_VERSION,
            job: 0,
            name: BoundedString::try_from("round").expect("name"),
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
            coordinator: c.id(),
        },
        inputs: vec![Payload::Inline(
            BoundedBytes::try_from(b"x".to_vec()).expect("input"),
        )],
        compare: None,
    }
}

fn run_local(c: &mut Coordinator, now: u64, actions: Vec<Action>) -> Vec<Action> {
    let mut out = Vec::new();
    for a in actions {
        if let Action::RunLocal { lease, .. } = a {
            out.extend(c.local_done(now, lease.lease, Outcome::Ok(b"y".to_vec())));
        } else {
            out.push(a);
        }
    }
    out
}

#[test]
fn only_finished_jobs_are_forgotten_and_their_slot_is_reused() {
    let cfg = CoordinatorConfig {
        local_slots: 1,
        ..CoordinatorConfig::default()
    };
    let mut c = Coordinator::new(cfg, Box::new(SecretKey::from_bytes(&[7; 32])));
    let mut jobs = Vec::new();
    let mut started = Vec::new();
    for i in 0..MAX_JOBS {
        jobs.push(c.submit(i as u64, spec(&c)).expect("submit"));
        started.extend(c.drain());
    }
    assert!(matches!(
        c.submit(100, spec(&c)),
        Err(CoreError::TooManyJobs(_))
    ));
    assert!(!c.forget(jobs[0]), "an unfinished job stays");
    assert!(!c.forget(9_999), "unknown job");

    started.extend(c.local_slots(200, 1));
    let out = run_local(&mut c, 201, started);
    let done = out
        .iter()
        .any(|a| matches!(a, Action::Event(Event::JobDone { .. })));
    assert!(done, "{out:?}");
    let finished = out
        .iter()
        .find_map(|a| match a {
            Action::Event(Event::JobDone { job, .. }) => Some(*job),
            _ => None,
        })
        .expect("job done");
    assert!(c.forget(finished));
    assert!(!c.forget(finished), "already forgotten");
    let next = c.submit(300, spec(&c)).expect("slot freed");
    assert!(!jobs.contains(&next));
}
