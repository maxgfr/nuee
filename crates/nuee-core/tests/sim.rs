//! Deterministic swarm simulator: one coordinator, many in-memory workers, a virtual clock.
//!
//! The task is a truth function, `output = blake3(input)`. Honest workers compute it,
//! cheaters return garbage, some workers fail every task or never answer.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

use nuee_core::{
    Action, Coordinator, CoordinatorConfig, Event, JobSpec, Outcome, Worker, WorkerAction,
    WorkerConfig,
};
use nuee_proto::{
    BoundedBytes, BoundedString, BoundedVec, ByeReason, Caps, Comparator, EndpointId, FailKind,
    Hash, LocalLimits, Manifest, Msg, Payload, SecretKey, TaskKind, VerifyMode, VerifyPolicy,
};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

const LATENCY_MS: u64 = 5;
const TICK_MS: u64 = 200;
const LOCAL_MS: u64 = 5;
const MAX_SIM_MS: u64 = 3_600_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Behavior {
    Honest,
    Cheat,
    FailOom,
    Silent,
    /// Answers `Unsupported` (e.g. an exec worker that has not approved the command).
    Refuse,
}

struct Spec {
    seed: u64,
    mode: VerifyMode,
    sample_permille: u16,
    canary_permille: u16,
    tasks: u32,
    honest: usize,
    churn: usize,
    churn_window: (u64, u64),
    sybil: usize,
    sybil_speed: (u64, u64),
    fail_oom: usize,
    silent: usize,
    refuse: usize,
    local_slots: u16,
    deadline_ms: u32,
    /// The coordinator answers `Unsupported` to its own executions.
    local_unsupported: bool,
}

impl Spec {
    fn new(seed: u64, mode: VerifyMode, tasks: u32) -> Self {
        Self {
            seed,
            mode,
            sample_permille: 0,
            canary_permille: 0,
            tasks,
            honest: 10,
            churn: 0,
            churn_window: (0, 0),
            sybil: 0,
            sybil_speed: (20, 200),
            fail_oom: 0,
            silent: 0,
            refuse: 0,
            local_slots: 2,
            deadline_ms: 10_000,
            local_unsupported: false,
        }
    }
}

enum Ev {
    ToCoord {
        from: usize,
        msg: Msg,
    },
    ToWorker {
        w: usize,
        msg: Msg,
    },
    Done {
        w: usize,
        lease: u64,
        input: Vec<u8>,
    },
    LocalDone {
        lease: u64,
        input: Vec<u8>,
    },
    Join(usize),
    Disconnect(usize),
    Tick,
}

struct SimWorker {
    id: EndpointId,
    worker: Worker,
    behavior: Behavior,
    speed: u64,
    connected: bool,
    running: BTreeSet<u64>,
}

#[derive(Default)]
struct Report {
    done_at: Option<u64>,
    finals: BTreeMap<u32, (Hash, EndpointId)>,
    banned: BTreeSet<EndpointId>,
    cancels_to: BTreeMap<EndpointId, u32>,
    failed: u32,
    mismatches: u32,
    redone: u32,
    local_runs: u32,
    /// `WaitingForChecker` events: (job, task, waiting, kind).
    waiting: Vec<(u32, u32, u32, TaskKind)>,
    verified: u32,
    /// Leases sent to each peer.
    leases_to: BTreeMap<EndpointId, u32>,
}

struct Sim {
    now: u64,
    seq: u64,
    queue: BinaryHeap<Reverse<(u64, u64)>>,
    events: BTreeMap<u64, Ev>,
    coord: Coordinator,
    coord_id: EndpointId,
    workers: Vec<SimWorker>,
    index: BTreeMap<EndpointId, usize>,
    local_slots: u16,
    local_unsupported: bool,
    local_running: BTreeSet<u64>,
    /// (peer, task) pairs that returned a result.
    returned: BTreeSet<(EndpointId, u32)>,
    /// lease -> (peer, task) for leases outstanding at a worker.
    holding: BTreeMap<u64, (EndpointId, u32)>,
    report: Report,
}

fn truth(input: &[u8]) -> Vec<u8> {
    blake3::hash(input).as_bytes().to_vec()
}

fn key(seed: u64, n: u64) -> SecretKey {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&seed.to_le_bytes());
    bytes[8..16].copy_from_slice(&n.to_le_bytes());
    bytes[16] = 0x5a;
    SecretKey::from_bytes(&bytes)
}

fn input_of(task: u32) -> Vec<u8> {
    let mut v = b"task-".to_vec();
    v.extend_from_slice(&task.to_le_bytes());
    v
}

fn caps(slots: u16) -> Caps {
    Caps {
        agent: BoundedString::try_from("sim").unwrap_or_default(),
        browser: false,
        cpu_slots: slots,
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

fn manifest(spec: &Spec, coordinator: EndpointId) -> Manifest {
    Manifest {
        version: nuee_proto::PROTO_VERSION,
        job: 0,
        name: BoundedString::try_from("sim").unwrap_or_default(),
        kind: TaskKind::Wasm,
        module: Some(Hash::of(b"truth module")),
        module_size: 12,
        max_memory: 16 << 20,
        assets: BoundedVec::new(),
        tasks: spec.tasks,
        verify: VerifyPolicy {
            mode: spec.mode,
            sample_permille: spec.sample_permille,
            canary_permille: spec.canary_permille,
        },
        compare: Comparator::Exact,
        deadline_ms: spec.deadline_ms,
        exec: None,
        train: None,
        coordinator,
    }
}

impl Sim {
    fn new(spec: &Spec, cfg: CoordinatorConfig) -> (Self, StdRng) {
        let mut rng = StdRng::seed_from_u64(spec.seed);
        let ckey = key(spec.seed, 0);
        let coord_id = ckey.public();
        let local_slots = cfg.local_slots.max(cfg.verify_slots);
        let coord = Coordinator::new(cfg, Box::new(ckey));
        let mut workers = Vec::new();
        let mut index = BTreeMap::new();
        let plan: Vec<Behavior> = std::iter::repeat_n(Behavior::Honest, spec.honest)
            .chain(std::iter::repeat_n(Behavior::Cheat, spec.sybil))
            .chain(std::iter::repeat_n(Behavior::FailOom, spec.fail_oom))
            .chain(std::iter::repeat_n(Behavior::Silent, spec.silent))
            .chain(std::iter::repeat_n(Behavior::Refuse, spec.refuse))
            .collect();
        for (i, behavior) in plan.into_iter().enumerate() {
            let k = key(spec.seed, i as u64 + 1);
            let id = k.public();
            let speed = if behavior == Behavior::Cheat {
                rng.random_range(spec.sybil_speed.0..=spec.sybil_speed.1)
            } else {
                rng.random_range(20..=200)
            };
            let worker = Worker::new(
                WorkerConfig {
                    caps: caps(2),
                    limits: LocalLimits::default(),
                },
                coord_id,
                Box::new(k),
            );
            index.insert(id, i);
            workers.push(SimWorker {
                id,
                worker,
                behavior,
                speed,
                connected: false,
                running: BTreeSet::new(),
            });
        }
        let sim = Self {
            now: 0,
            seq: 0,
            queue: BinaryHeap::new(),
            events: BTreeMap::new(),
            coord,
            coord_id,
            workers,
            index,
            local_slots,
            local_unsupported: spec.local_unsupported,
            local_running: BTreeSet::new(),
            returned: BTreeSet::new(),
            holding: BTreeMap::new(),
            report: Report::default(),
        };
        (sim, rng)
    }

    fn at(&mut self, t: u64, ev: Ev) {
        self.seq += 1;
        self.queue.push(Reverse((t, self.seq)));
        self.events.insert(self.seq, ev);
    }

    fn coord_actions(&mut self, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::Send { to, msg } => {
                    match &msg {
                        Msg::Lease(l) => {
                            assert!(
                                !self.returned.contains(&(to, l.task)),
                                "task {} leased again to {to} which already returned it",
                                l.task
                            );
                            assert!(
                                !self.holding.values().any(|h| *h == (to, l.task)),
                                "two concurrent leases of task {} to {to}",
                                l.task
                            );
                            self.holding.insert(l.lease, (to, l.task));
                            *self.report.leases_to.entry(to).or_default() += 1;
                        }
                        Msg::Cancel { lease } => {
                            self.holding.remove(lease);
                            *self.report.cancels_to.entry(to).or_default() += 1;
                        }
                        _ => {}
                    }
                    if let Some(&w) = self.index.get(&to) {
                        let t = self.now + LATENCY_MS;
                        self.at(t, Ev::ToWorker { w, msg });
                    }
                }
                Action::Close { peer, reason } => {
                    if reason == ByeReason::Banned {
                        self.report.banned.insert(peer);
                    }
                    self.holding.retain(|_, h| h.0 != peer);
                    if let Some(&w) = self.index.get(&peer)
                        && let Some(sw) = self.workers.get_mut(w)
                    {
                        sw.connected = false;
                    }
                }
                Action::RunLocal { lease, .. } => {
                    self.report.local_runs += 1;
                    self.local_running.insert(lease.lease);
                    let input = match lease.input {
                        Payload::Inline(b) => b.into_inner(),
                        Payload::Blob { .. } => panic!("the simulator only uses inline inputs"),
                    };
                    let t = self.now + LOCAL_MS;
                    self.at(
                        t,
                        Ev::LocalDone {
                            lease: lease.lease,
                            input,
                        },
                    );
                }
                Action::FetchOutput { .. } => panic!("outputs are small enough to be inline"),
                Action::Accepted {
                    task,
                    output,
                    output_hash,
                    by,
                    ..
                } => {
                    assert_eq!(Hash::of(&output), output_hash);
                    self.report.finals.insert(task, (output_hash, by));
                }
                Action::Event(e) => match e {
                    Event::JobDone { failed, .. } => {
                        self.report.done_at = Some(self.now);
                        self.report.failed = failed;
                    }
                    Event::Banned { peer, redo } => {
                        self.report.redone += redo;
                        self.report.banned.insert(peer);
                    }
                    Event::Mismatch { .. } => self.report.mismatches += 1,
                    Event::WaitingForChecker {
                        job,
                        task,
                        waiting,
                        kind,
                    } => self.report.waiting.push((job, task, waiting, kind)),
                    Event::Verified { .. } => self.report.verified += 1,
                    _ => {}
                },
            }
        }
    }

    fn worker_actions(&mut self, w: usize, actions: Vec<WorkerAction>) {
        for a in actions {
            let Some(sw) = self.workers.get_mut(w) else {
                return;
            };
            match a {
                WorkerAction::Send(msg) => {
                    if sw.connected {
                        let t = self.now + LATENCY_MS;
                        self.at(t, Ev::ToCoord { from: w, msg });
                    }
                }
                WorkerAction::Run { lease, .. } => {
                    sw.running.insert(lease.lease);
                    let input = match lease.input {
                        Payload::Inline(b) => b.into_inner(),
                        Payload::Blob { .. } => panic!("the simulator only uses inline inputs"),
                    };
                    if sw.behavior != Behavior::Silent {
                        let t = self.now + sw.speed;
                        self.at(
                            t,
                            Ev::Done {
                                w,
                                lease: lease.lease,
                                input,
                            },
                        );
                    }
                }
                WorkerAction::Abort { lease } => {
                    sw.running.remove(&lease);
                }
                WorkerAction::Ended(_) => sw.connected = false,
                WorkerAction::Publish { .. }
                | WorkerAction::Fetch { .. }
                | WorkerAction::Event(_) => {}
            }
        }
    }

    fn local_fill(&mut self) {
        let free = self
            .local_slots
            .saturating_sub(self.local_running.len() as u16);
        if free > 0 {
            let a = self.coord.local_slots(self.now, free);
            self.coord_actions(a);
        }
    }

    fn step(&mut self, ev: Ev) {
        match ev {
            Ev::Join(w) => {
                let Some(sw) = self.workers.get_mut(w) else {
                    return;
                };
                sw.connected = true;
                let (id, caps) = (sw.id, caps(2));
                let a = self.coord.admitted(self.now, id, caps);
                self.coord_actions(a);
            }
            Ev::ToCoord { from, msg } => {
                let Some(sw) = self.workers.get(from) else {
                    return;
                };
                if !sw.connected {
                    return;
                }
                let id = sw.id;
                if let Msg::Result { lease, .. } = &msg
                    && let Some((peer, task)) = self.holding.remove(lease)
                    && peer == id
                {
                    self.returned.insert((id, task));
                }
                if let Msg::Fail { lease, .. } = &msg {
                    self.holding.remove(lease);
                }
                let a = self.coord.handle(self.now, id, msg);
                self.coord_actions(a);
            }
            Ev::ToWorker { w, msg } => {
                let Some(sw) = self.workers.get_mut(w) else {
                    return;
                };
                if !sw.connected {
                    return;
                }
                let a = sw.worker.handle(self.now, msg);
                self.worker_actions(w, a);
            }
            Ev::Done { w, lease, input } => {
                let Some(sw) = self.workers.get_mut(w) else {
                    return;
                };
                if !sw.connected || !sw.running.remove(&lease) {
                    return;
                }
                let out = match sw.behavior {
                    Behavior::Honest => Outcome::Ok(truth(&input)),
                    Behavior::Cheat => {
                        let mut garbage = input.clone();
                        garbage.extend_from_slice(sw.id.as_bytes());
                        Outcome::Ok(truth(&garbage))
                    }
                    Behavior::FailOom => Outcome::Fail(FailKind::Oom, "out of memory".into()),
                    Behavior::Silent => return,
                    Behavior::Refuse => {
                        Outcome::Fail(FailKind::Unsupported, "command not approved".into())
                    }
                };
                let a = sw.worker.task_done(self.now, lease, out);
                self.worker_actions(w, a);
            }
            Ev::LocalDone { lease, input } => {
                self.local_running.remove(&lease);
                let out = if self.local_unsupported {
                    Outcome::Fail(FailKind::Unsupported, "kind not runnable here".into())
                } else {
                    Outcome::Ok(truth(&input))
                };
                let a = self.coord.local_done(self.now, lease, out);
                self.coord_actions(a);
                self.local_fill();
            }
            Ev::Disconnect(w) => {
                let Some(sw) = self.workers.get_mut(w) else {
                    return;
                };
                if !sw.connected {
                    return;
                }
                sw.connected = false;
                let id = sw.id;
                self.holding.retain(|_, h| h.0 != id);
                let a = self.coord.disconnected(self.now, id);
                self.coord_actions(a);
            }
            Ev::Tick => {
                let a = self.coord.tick(self.now);
                self.coord_actions(a);
                for w in 0..self.workers.len() {
                    let Some(sw) = self.workers.get_mut(w) else {
                        continue;
                    };
                    if sw.connected {
                        let a = sw.worker.tick(self.now);
                        self.worker_actions(w, a);
                    }
                }
                self.local_fill();
                let t = self.now + TICK_MS;
                self.at(t, Ev::Tick);
            }
        }
    }

    /// Runs every event up to time `end`, stopping early if the job completes.
    fn run_until(&mut self, end: u64) {
        while let Some(Reverse((t, seq))) = self.queue.peek().copied() {
            if t > end {
                break;
            }
            self.queue.pop();
            self.now = t;
            if let Some(ev) = self.events.remove(&seq) {
                self.step(ev);
            }
            if self.report.done_at.is_some() {
                return;
            }
        }
        self.now = self.now.max(end);
    }

    fn run_until_done(&mut self) {
        while let Some(Reverse((t, seq))) = self.queue.pop() {
            assert!(t <= MAX_SIM_MS, "simulation did not finish");
            self.now = t;
            if let Some(ev) = self.events.remove(&seq) {
                self.step(ev);
            }
            if self.report.done_at.is_some() {
                return;
            }
        }
    }
}

fn run(spec: &Spec, cfg: CoordinatorConfig) -> Sim {
    let (mut sim, mut rng) = Sim::new(spec, cfg);
    let m = manifest(spec, sim.coord_id);
    let inputs = (0..spec.tasks)
        .map(|t| Payload::Inline(BoundedBytes::try_from(input_of(t)).unwrap_or_default()))
        .collect();
    let job = sim.coord.submit(
        0,
        JobSpec {
            manifest: m,
            inputs,
            compare: None,
        },
    );
    assert_eq!(job.ok(), Some(0));
    let queued = sim.coord.drain();
    sim.coord_actions(queued);
    for w in 0..sim.workers.len() {
        let t = rng.random_range(0..50);
        sim.at(t, Ev::Join(w));
    }
    // Churn hits honest workers only, so the cheaters stay around to be caught.
    let mut churners: Vec<usize> = (0..spec.honest).collect();
    for i in 0..spec.churn.min(churners.len()) {
        let j = rng.random_range(i..churners.len());
        churners.swap(i, j);
    }
    for &w in churners.iter().take(spec.churn) {
        let t = rng.random_range(spec.churn_window.0..=spec.churn_window.1);
        sim.at(t, Ev::Disconnect(w));
    }
    sim.at(TICK_MS, Ev::Tick);
    sim.local_fill();
    sim.run_until_done();
    sim
}

fn cheaters(sim: &Sim) -> Vec<EndpointId> {
    sim.workers
        .iter()
        .filter(|w| w.behavior == Behavior::Cheat)
        .map(|w| w.id)
        .collect()
}

fn assert_all_true(sim: &Sim, tasks: u32) {
    assert!(sim.report.done_at.is_some(), "job did not complete");
    assert_eq!(sim.report.failed, 0);
    assert_eq!(sim.report.finals.len(), tasks as usize);
    for t in 0..tasks {
        let (hash, _) = sim.report.finals.get(&t).copied().unwrap_or_default_pair();
        assert_eq!(
            hash,
            Hash::of(&truth(&input_of(t))),
            "task {t} output differs"
        );
    }
    let status = sim.coord.status();
    let job = status.jobs.first().map(|j| (j.done, j.total));
    assert_eq!(job, Some((tasks, tasks)));
}

trait PairDefault {
    fn unwrap_or_default_pair(self) -> (Hash, EndpointId);
}

impl PairDefault for Option<(Hash, EndpointId)> {
    fn unwrap_or_default_pair(self) -> (Hash, EndpointId) {
        match self {
            Some(p) => p,
            None => panic!("task has no accepted output"),
        }
    }
}

#[test]
fn full_verification_with_churn_and_sybil() {
    let mut spec = Spec::new(1, VerifyMode::Full, 2000);
    spec.honest = 95;
    spec.sybil = 5;
    spec.churn = 30;
    spec.churn_window = (300, 2000);
    let cfg = CoordinatorConfig {
        local_slots: 2,
        seed: 11,
        ..CoordinatorConfig::default()
    };
    let sim = run(&spec, cfg);
    assert_all_true(&sim, spec.tasks);
    for c in cheaters(&sim) {
        assert!(sim.report.banned.contains(&c), "cheater {c} not banned");
    }
    for c in cheaters(&sim) {
        assert!(
            sim.report.finals.values().all(|(_, by)| *by != c),
            "an output of a banned identity remains"
        );
    }
    let honest: BTreeSet<EndpointId> = sim
        .workers
        .iter()
        .filter(|w| w.behavior == Behavior::Honest)
        .map(|w| w.id)
        .collect();
    assert!(
        sim.report.banned.is_disjoint(&honest),
        "an honest worker was banned"
    );
}

#[test]
fn sample_and_canaries_catch_fast_sybil_and_redo_its_work() {
    let mut spec = Spec::new(2, VerifyMode::Sample, 5000);
    spec.sample_permille = 100;
    spec.canary_permille = 30;
    spec.honest = 95;
    spec.sybil = 5;
    spec.sybil_speed = (2, 5);
    spec.churn = 30;
    spec.churn_window = (300, 3000);
    let cfg = CoordinatorConfig {
        local_slots: 2,
        seed: 22,
        ..CoordinatorConfig::default()
    };
    let sim = run(&spec, cfg);
    let banned = &sim.report.banned;
    for c in cheaters(&sim) {
        assert!(banned.contains(&c), "cheater {c} not banned");
    }
    for (task, (_, by)) in &sim.report.finals {
        assert!(
            !banned.contains(by),
            "task {task} output comes from a banned identity"
        );
    }
    assert_all_true(&sim, spec.tasks);
    let honest: BTreeSet<EndpointId> = sim
        .workers
        .iter()
        .filter(|w| w.behavior == Behavior::Honest)
        .map(|w| w.id)
        .collect();
    assert!(banned.is_disjoint(&honest), "an honest worker was banned");
}

#[test]
fn verification_without_local_slots_never_reuses_the_original_peer() {
    // Without coordinator slots every check is a peer re-execution; the harness asserts
    // in `coord_actions` that no peer gets a task it already returned.
    let mut spec = Spec::new(3, VerifyMode::Full, 400);
    spec.honest = 12;
    spec.sybil = 2;
    spec.local_slots = 0;
    let cfg = CoordinatorConfig {
        local_slots: 0,
        seed: 33,
        ..CoordinatorConfig::default()
    };
    let sim = run(&spec, cfg);
    assert!(sim.report.done_at.is_some());
    assert!(sim.report.mismatches > 0);
    for t in 0..spec.tasks {
        let (hash, by) = sim.report.finals.get(&t).copied().unwrap_or_default_pair();
        assert_eq!(
            hash,
            Hash::of(&truth(&input_of(t))),
            "task {t} output differs"
        );
        assert!(!cheaters(&sim).contains(&by));
    }
}

fn honest_ids(sim: &Sim) -> BTreeSet<EndpointId> {
    sim.workers
        .iter()
        .filter(|w| w.behavior == Behavior::Honest)
        .map(|w| w.id)
        .collect()
}

#[test]
fn full_verification_without_contribution_bans_after_the_coordinator_recompute() {
    // The coordinator takes no work (`local_slots` 0) but still checks: a mismatch goes to its
    // own recompute, the liar is banned and what it had got accepted is redone.
    let mut spec = Spec::new(7, VerifyMode::Full, 400);
    spec.honest = 6;
    spec.sybil = 2;
    spec.local_slots = 0;
    let cfg = CoordinatorConfig {
        local_slots: 0,
        verify_slots: 1,
        seed: 77,
        ..CoordinatorConfig::default()
    };
    let sim = run(&spec, cfg);
    assert_all_true(&sim, spec.tasks);
    for c in cheaters(&sim) {
        assert!(sim.report.banned.contains(&c), "cheater {c} not banned");
        assert!(sim.report.finals.values().all(|(_, by)| *by != c));
    }
    assert!(sim.report.banned.is_disjoint(&honest_ids(&sim)));
    assert!(sim.report.mismatches > 0);
    assert!(
        sim.report.local_runs < spec.tasks / 4,
        "the coordinator only checks: {} local runs",
        sim.report.local_runs
    );
}

#[test]
fn sampled_verification_without_contribution_bans_and_redoes() {
    let mut spec = Spec::new(8, VerifyMode::Sample, 2000);
    spec.sample_permille = 100;
    spec.canary_permille = 30;
    spec.honest = 10;
    spec.sybil = 2;
    spec.sybil_speed = (2, 5);
    spec.local_slots = 0;
    let cfg = CoordinatorConfig {
        local_slots: 0,
        verify_slots: 1,
        seed: 88,
        ..CoordinatorConfig::default()
    };
    let sim = run(&spec, cfg);
    for c in cheaters(&sim) {
        assert!(sim.report.banned.contains(&c), "cheater {c} not banned");
    }
    assert!(
        sim.report.redone > 0,
        "accepted results of a liar are redone"
    );
    assert!(
        sim.report.local_runs > 0,
        "samples and canary answers are recomputed by the coordinator"
    );
    for (task, (_, by)) in &sim.report.finals {
        assert!(
            !sim.report.banned.contains(by),
            "task {task} kept a liar's output"
        );
    }
    assert_all_true(&sim, spec.tasks);
    assert!(sim.report.banned.is_disjoint(&honest_ids(&sim)));
}

#[test]
fn a_coordinator_that_cannot_run_the_kind_falls_back_to_peer_agreement() {
    for mode in [VerifyMode::Full, VerifyMode::Sample] {
        let mut spec = Spec::new(9, mode, 300);
        spec.sample_permille = 200;
        spec.canary_permille = 30;
        spec.honest = 8;
        spec.sybil = 1;
        spec.local_slots = 0;
        spec.local_unsupported = true;
        let cfg = CoordinatorConfig {
            local_slots: 0,
            verify_slots: 1,
            seed: 99,
            ..CoordinatorConfig::default()
        };
        let sim = run(&spec, cfg);
        assert_eq!(sim.report.failed, 0, "{mode:?}: no task given up");
        assert!(sim.report.local_runs > 0, "{mode:?}: the coordinator tried");
        for t in 0..spec.tasks {
            let (hash, by) = sim.report.finals.get(&t).copied().unwrap_or_default_pair();
            assert_eq!(hash, Hash::of(&truth(&input_of(t))), "{mode:?}: task {t}");
            assert!(!cheaters(&sim).contains(&by));
        }
    }
}

#[test]
fn full_verification_with_one_peer_is_checked_by_a_verify_only_coordinator() {
    // `nuee run --local-slots 0 --verify full` with a single joiner: the coordinator takes no
    // fresh work, so each second execution has to come from its verification capacity.
    let mut spec = Spec::new(12, VerifyMode::Full, 24);
    spec.honest = 1;
    spec.local_slots = 0;
    let cfg = CoordinatorConfig {
        local_slots: 0,
        verify_slots: 1,
        seed: 120,
        ..CoordinatorConfig::default()
    };
    let sim = run(&spec, cfg);
    assert_all_true(&sim, spec.tasks);
    assert_eq!(sim.report.verified, spec.tasks, "every task is checked");
    assert_eq!(
        sim.report.local_runs, spec.tasks,
        "one coordinator execution per task, none of it fresh work"
    );
    assert!(sim.report.waiting.is_empty(), "a checker was there");
}

#[test]
fn a_lone_eligible_peer_waits_for_a_checker_instead_of_being_trusted() {
    for mode in [VerifyMode::Full, VerifyMode::Sample] {
        let mut spec = Spec::new(10, mode, 20);
        spec.sample_permille = 1000;
        spec.honest = 2;
        spec.local_slots = 0;
        // What a host that cannot run the kind configures: no local execution at all.
        let cfg = CoordinatorConfig {
            local_slots: 0,
            verify_slots: 0,
            seed: 100,
            ..CoordinatorConfig::default()
        };
        let (mut sim, _) = Sim::new(&spec, cfg);
        let m = manifest(&spec, sim.coord_id);
        let inputs = (0..spec.tasks)
            .map(|t| Payload::Inline(BoundedBytes::try_from(input_of(t)).unwrap_or_default()))
            .collect();
        let job = sim.coord.submit(
            0,
            JobSpec {
                manifest: m,
                inputs,
                compare: None,
            },
        );
        assert_eq!(job.ok(), Some(0));
        let queued = sim.coord.drain();
        sim.coord_actions(queued);
        sim.at(0, Ev::Join(0));
        sim.at(TICK_MS, Ev::Tick);
        sim.local_fill();
        sim.run_until(60_000);

        assert!(
            sim.report.done_at.is_none(),
            "{mode:?}: finished on one peer"
        );
        assert_eq!(sim.report.verified, 0, "{mode:?}: nothing is checked");
        if mode == VerifyMode::Full {
            assert!(sim.report.finals.is_empty(), "{mode:?}: accepted unchecked");
        }
        assert_eq!(
            sim.report.waiting.len(),
            1,
            "{mode:?}: one signal per job: {:?}",
            sim.report.waiting
        );
        let (j, _, waiting, kind) =
            sim.report
                .waiting
                .first()
                .copied()
                .unwrap_or((u32::MAX, 0, 0, TaskKind::Exec));
        assert_eq!((j, kind), (0, TaskKind::Wasm));
        assert!(waiting >= 1);

        let now = sim.now;
        sim.at(now, Ev::Join(1));
        sim.run_until_done();
        assert_all_true(&sim, spec.tasks);
        assert_eq!(sim.report.verified, spec.tasks, "{mode:?}: all checked");
        assert_eq!(
            sim.report.waiting.len(),
            1,
            "{mode:?}: no signal once served"
        );
    }
}

#[test]
fn fail_is_not_a_mismatch() {
    let mut spec = Spec::new(4, VerifyMode::Sample, 300);
    spec.sample_permille = 200;
    spec.honest = 9;
    spec.fail_oom = 1;
    let cfg = CoordinatorConfig {
        local_slots: 1,
        seed: 44,
        ..CoordinatorConfig::default()
    };
    let sim = run(&spec, cfg);
    assert_all_true(&sim, spec.tasks);
    assert!(
        sim.report.banned.is_empty(),
        "a failing worker must not be banned"
    );
    assert_eq!(sim.report.mismatches, 0);
    let oom = sim
        .workers
        .iter()
        .find(|w| w.behavior == Behavior::FailOom)
        .map(|w| w.id);
    assert!(sim.report.finals.values().all(|(_, by)| Some(*by) != oom));
}

#[test]
fn silent_worker_leases_expire_and_are_requeued() {
    let mut spec = Spec::new(5, VerifyMode::None, 100);
    spec.honest = 4;
    spec.silent = 1;
    spec.local_slots = 0;
    spec.deadline_ms = 2_000;
    let cfg = CoordinatorConfig {
        local_slots: 0,
        seed: 55,
        ..CoordinatorConfig::default()
    };
    let sim = run(&spec, cfg);
    assert_all_true(&sim, spec.tasks);
    let silent = sim
        .workers
        .iter()
        .find(|w| w.behavior == Behavior::Silent)
        .map(|w| w.id);
    let cancels = silent.and_then(|s| sim.report.cancels_to.get(&s).copied());
    assert!(
        cancels.unwrap_or(0) > 0,
        "the silent worker's leases were never cancelled"
    );
    assert!(
        sim.report
            .finals
            .values()
            .all(|(_, by)| Some(*by) != silent)
    );
}

fn sends(actions: &[Action], to: EndpointId) -> Vec<Msg> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Send { to: t, msg } if *t == to => Some(msg.clone()),
            _ => None,
        })
        .collect()
}

fn closed(actions: &[Action], peer: EndpointId) -> Option<ByeReason> {
    actions.iter().find_map(|a| match a {
        Action::Close { peer: p, reason } if *p == peer => Some(*reason),
        _ => None,
    })
}

#[test]
fn approval_flow_and_full_swarm() {
    let cfg = CoordinatorConfig {
        approve: true,
        max_peers: 2,
        ..CoordinatorConfig::default()
    };
    let mut c = Coordinator::new(cfg, Box::new(key(6, 0)));
    let a = key(6, 1).public();
    let b = key(6, 2).public();
    let d = key(6, 3).public();
    let e = key(6, 4).public();

    let out = c.admitted(0, a, caps(1));
    assert_eq!(sends(&out, a), vec![Msg::Pending]);
    assert!(out.iter().any(|x| matches!(
        x,
        Action::Event(Event::PeerPending { peer, .. }) if *peer == a
    )));
    let out = c.approve(1, a, true);
    assert!(matches!(sends(&out, a).first(), Some(Msg::Welcome { .. })));
    assert!(out.iter().any(|x| matches!(
        x,
        Action::Event(Event::PeerJoined { peer, .. }) if *peer == a
    )));

    c.admitted(2, b, caps(1));
    let out = c.approve(3, b, false);
    assert_eq!(
        sends(&out, b),
        vec![Msg::Bye {
            reason: ByeReason::Rejected
        }]
    );
    assert_eq!(closed(&out, b), Some(ByeReason::Rejected));

    let out = c.admitted(4, d, caps(1));
    assert_eq!(sends(&out, d), vec![Msg::Pending]);
    let out = c.admitted(5, e, caps(1));
    assert_eq!(
        sends(&out, e),
        vec![Msg::Bye {
            reason: ByeReason::Full
        }]
    );
    assert_eq!(closed(&out, e), Some(ByeReason::Full));
    let out = c.approve(6, d, true);
    assert!(matches!(sends(&out, d).first(), Some(Msg::Welcome { .. })));
    assert_eq!(c.status().peers.len(), 2);
}

#[test]
fn unsupported_peers_do_not_use_up_attempts() {
    let mut spec = Spec::new(13, VerifyMode::None, 60);
    spec.honest = 1;
    spec.refuse = 3;
    spec.local_slots = 0;
    let cfg = CoordinatorConfig {
        local_slots: 0,
        max_attempts: 2,
        seed: 130,
        ..CoordinatorConfig::default()
    };
    let sim = run(&spec, cfg);
    assert_all_true(&sim, spec.tasks);
    let honest = sim.workers.first().map(|w| w.id);
    assert!(
        sim.report
            .finals
            .values()
            .all(|(_, by)| Some(*by) == honest)
    );
    for w in sim
        .workers
        .iter()
        .filter(|w| w.behavior == Behavior::Refuse)
    {
        let n = sim.report.leases_to.get(&w.id).copied().unwrap_or(0);
        assert!(n <= 2, "a refusing peer kept getting leases: {n}");
    }
}

#[test]
fn an_unsupported_peer_is_excluded_from_that_job_only() {
    let cfg = CoordinatorConfig {
        seed: 140,
        ..CoordinatorConfig::default()
    };
    let mut c = Coordinator::new(cfg, Box::new(key(14, 0)));
    let coord = key(14, 0).public();
    let spec = Spec::new(14, VerifyMode::None, 6);
    for _ in 0..2 {
        let inputs = (0..spec.tasks)
            .map(|t| Payload::Inline(BoundedBytes::try_from(input_of(t)).unwrap_or_default()))
            .collect();
        let job = c.submit(
            0,
            JobSpec {
                manifest: manifest(&spec, coord),
                inputs,
                compare: None,
            },
        );
        assert!(job.is_ok());
    }
    c.drain();
    let a = key(14, 1).public();
    c.admitted(0, a, caps(1));
    let leases = |out: &[Action]| -> Vec<(u64, u32)> {
        sends(out, a)
            .into_iter()
            .filter_map(|m| match m {
                Msg::Lease(l) => Some((l.lease, l.job)),
                _ => None,
            })
            .collect()
    };
    let mut got = leases(&c.handle(1, a, Msg::Want { slots: 1 }));
    let mut refused = None;
    let mut other = 0;
    for step in 0..40u64 {
        let Some((lease, job)) = got.pop() else {
            break;
        };
        let msg = match refused {
            None => {
                refused = Some(job);
                Msg::Fail {
                    lease,
                    kind: FailKind::Unsupported,
                    detail: BoundedString::try_from("not approved").unwrap_or_default(),
                }
            }
            Some(r) => {
                assert_ne!(job, r, "leased the refused job again");
                other += 1;
                Msg::Fail {
                    lease,
                    kind: FailKind::Oom,
                    detail: BoundedString::try_from("oom").unwrap_or_default(),
                }
            }
        };
        let now = 2 + step;
        got.extend(leases(&c.handle(now, a, msg)));
        got.extend(leases(&c.handle(now, a, Msg::Want { slots: 1 })));
    }
    assert!(refused.is_some(), "no lease at all");
    assert!(other > 0, "the other job's tasks were not leased");
}
