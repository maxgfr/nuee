//! Coordinator state machine: admission, pull scheduling, verification and bans.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use nuee_proto::limits::{
    DEFAULT_DEADLINE_MS, MAX_DEADLINE_MS, MAX_JOBS, MAX_LEASES_PER_PEER, MAX_PEER_DEADLINE_MS,
    MAX_PEERS, MAX_SIGNAL, MIN_PEER_DEADLINE_MS,
};
use nuee_proto::{
    BoundedString, BoundedVec, ByeReason, Caps, Comparator, EndpointId, FailKind, Hash, Lease,
    Manifest, Msg, Payload, RtcSignal, SignedManifest, Signer, SwarmInfo, SwarmMode, VerifyMode,
    verify_result,
};
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};

use crate::events::{Action, Event, JobStatus, PeerStatus, RtcKind, SwarmStatus};
use crate::sched::{DeadlineBounds, Durations, lease_deadline, routable};
use crate::verify::{Cmp, Ctx, Exec, Snapshot, TaskState};
use crate::{Compare, CoreError, Outcome};

/// Signalling messages accepted per peer and per minute.
const RTC_PER_MINUTE: u32 = 64;
const RTC_WINDOW_MS: u64 = 60_000;
/// Most peers that may already have run a task picked as a canary.
const CANARY_MAX_RUNNERS: usize = 8;
/// Leases carried in `Welcome`; further jobs follow as `Job` messages.
const WELCOME_JOBS: usize = 16;
/// How long the peer set must stay unchanged before a verify-only coordinator gives a lone
/// peer's results their second execution itself (other peers may still be joining).
const LONE_CHECK_GRACE_MS: u64 = 3_000;
/// Failures on one job before a peer's failure rate is judged.
const SHUN_MIN_FAILS: u32 = 4;

/// Coordinator settings.
#[derive(Debug, Clone)]
pub struct CoordinatorConfig {
    /// Swarm display name.
    pub name: BoundedString<64>,
    /// Tasks the coordinator runs itself at once (0: never).
    pub local_slots: u16,
    /// Recomputations the coordinator runs at once to check peers (sampled checks, mismatch
    /// arbitration, canary answers), even when `local_slots` is 0. With neither, a mismatch
    /// only discards both results and two new agreeing peer executions are required.
    pub verify_slots: u16,
    /// New peers wait for manual approval.
    pub approve: bool,
    /// Most peers admitted at once (at most `MAX_PEERS`).
    pub max_peers: usize,
    /// Most leases per peer (at most `MAX_LEASES_PER_PEER`).
    pub max_leases_per_peer: u16,
    /// Seed of the scheduling and sampling generator.
    pub seed: u64,
    /// Lower clamp of per-peer deadlines.
    pub deadline_floor_ms: u64,
    /// Upper clamp of per-peer deadlines.
    pub deadline_ceil_ms: u64,
    /// Upper clamp of per-peer deadlines for exec tasks.
    pub exec_deadline_ceil_ms: u64,
    /// Failures before a task is given up.
    pub max_attempts: u8,
    /// Relay WebRTC signalling between peers.
    pub webrtc: bool,
    /// STUN servers announced in `Welcome`.
    pub stun: BoundedVec<BoundedString<256>, 4>,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            name: BoundedString::new(),
            local_slots: 0,
            verify_slots: 0,
            approve: false,
            max_peers: MAX_PEERS,
            max_leases_per_peer: MAX_LEASES_PER_PEER as u16,
            seed: 0,
            deadline_floor_ms: u64::from(MIN_PEER_DEADLINE_MS),
            deadline_ceil_ms: u64::from(MAX_PEER_DEADLINE_MS),
            exec_deadline_ceil_ms: u64::from(MAX_DEADLINE_MS),
            max_attempts: 3,
            webrtc: false,
            stun: BoundedVec::new(),
        }
    }
}

/// A job to distribute.
pub struct JobSpec {
    /// Manifest; `job` and `coordinator` are overwritten by `submit`.
    pub manifest: Manifest,
    /// One input per task.
    pub inputs: Vec<Payload>,
    /// Comparator for tolerance-based manifests.
    pub compare: Option<Arc<dyn Compare>>,
}

struct Job {
    manifest: Arc<Manifest>,
    signed: SignedManifest,
    assets: BoundedVec<Hash, { nuee_proto::limits::MAX_ASSETS_PER_JOB }>,
    inputs: Vec<Payload>,
    cmp: Cmp,
    tasks: Vec<TaskState>,
    queue: VecDeque<u32>,
    known: Vec<u32>,
    /// Peers caught in an unarbitrated disagreement: their results need two agreeing executions.
    suspects: BTreeSet<EndpointId>,
    done: u32,
    verified: u32,
    failed: u32,
    started: u64,
    finished: bool,
    /// The coordinator answered `Unsupported` for this job's kind: only peers check it.
    no_local: bool,
    /// Peers able to run the kind when `WaitingForChecker` was last emitted.
    waiting_signal: Option<Vec<EndpointId>>,
    /// Peers that answered `Unsupported` for this job, or fail it far more often than they
    /// succeed: never leased it again while connected.
    refused: BTreeSet<EndpointId>,
    /// Per peer: results received and failures on this job.
    outcomes: BTreeMap<EndpointId, (u32, u32)>,
}

impl Job {
    fn mode(&self) -> VerifyMode {
        self.manifest.verify.mode
    }
}

struct Peer {
    caps: Caps,
    credit: u16,
    inflight: BTreeSet<u64>,
    durations: Durations,
    done: u32,
    verified: u32,
    rtc_window: u64,
    rtc_count: u32,
}

/// What the coordinator knew about a peer when it banned it, kept for status snapshots.
#[derive(Debug, Clone, Copy)]
struct BannedPeer {
    browser: bool,
    slots: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    Normal,
    LocalTask,
    LocalVerify,
}

struct LeaseRec {
    peer: EndpointId,
    job: u32,
    task: u32,
    purpose: Purpose,
    issued: u64,
    deadline_at: u64,
}

struct Fetching {
    peer: EndpointId,
    job: u32,
    task: u32,
    hash: Hash,
    issued: u64,
    since: u64,
}

/// The coordinator of a compute swarm.
pub struct Coordinator {
    cfg: CoordinatorConfig,
    signer: Box<dyn Signer>,
    id: EndpointId,
    rng: StdRng,
    jobs: BTreeMap<u32, Job>,
    next_job: u32,
    next_lease: u64,
    peers: BTreeMap<EndpointId, Peer>,
    pending: BTreeMap<EndpointId, Caps>,
    banned: BTreeMap<EndpointId, BannedPeer>,
    leases: BTreeMap<u64, LeaseRec>,
    fetching: BTreeMap<u64, Fetching>,
    local_verify: VecDeque<(u32, u32)>,
    /// Last time a peer joined or left.
    peers_changed: u64,
    rr: usize,
    now: u64,
    out: Vec<Action>,
}

impl Coordinator {
    /// Creates a coordinator signing with `signer`.
    pub fn new(mut cfg: CoordinatorConfig, signer: Box<dyn Signer>) -> Self {
        cfg.max_peers = cfg.max_peers.min(MAX_PEERS);
        cfg.max_leases_per_peer = cfg.max_leases_per_peer.clamp(1, MAX_LEASES_PER_PEER as u16);
        cfg.max_attempts = cfg.max_attempts.max(1);
        let id = signer.id();
        let rng = StdRng::seed_from_u64(cfg.seed);
        Self {
            cfg,
            signer,
            id,
            rng,
            jobs: BTreeMap::new(),
            next_job: 0,
            next_lease: 1,
            peers: BTreeMap::new(),
            pending: BTreeMap::new(),
            banned: BTreeMap::new(),
            leases: BTreeMap::new(),
            fetching: BTreeMap::new(),
            local_verify: VecDeque::new(),
            peers_changed: 0,
            rr: 0,
            now: 0,
            out: Vec::new(),
        }
    }

    /// Identity of the coordinator.
    pub fn id(&self) -> EndpointId {
        self.id
    }

    /// Turns WebRTC signalling relay on or off.
    pub fn set_webrtc(&mut self, on: bool) {
        self.cfg.webrtc = on;
    }

    /// Actions queued by calls that do not return any (`submit`, `mark_done`).
    pub fn drain(&mut self) -> Vec<Action> {
        self.finish()
    }

    fn begin(&mut self, now: u64) {
        self.now = self.now.max(now);
    }

    fn finish(&mut self) -> Vec<Action> {
        self.watch_checkers();
        std::mem::take(&mut self.out)
    }

    /// Signals jobs whose tasks wait for a peer execution nobody admitted can provide.
    fn watch_checkers(&mut self) {
        let max = self.cfg.max_attempts;
        let ids: Vec<u32> = self.jobs.keys().copied().collect();
        for job in ids {
            let can_local = self.can_local_job(job);
            let Some(j) = self.jobs.get(&job) else {
                continue;
            };
            if j.finished || can_local {
                continue;
            }
            let able: Vec<EndpointId> = self
                .peers
                .iter()
                .filter(|(id, p)| routable(&p.caps, &j.manifest) && !j.refused.contains(*id))
                .map(|(id, _)| *id)
                .collect();
            let mode = j.mode();
            let mut first = None;
            let mut waiting = 0u32;
            for (i, t) in j.tasks.iter().enumerate() {
                let tried = !t.runners.is_empty() || !t.results.is_empty() || !j.refused.is_empty();
                if !tried || t.want(mode, false) == 0 {
                    continue;
                }
                let free = able.iter().any(|p| {
                    !t.runners.contains(p)
                        && t.results.iter().all(|e| e.by != *p)
                        && !t.barred(p, max)
                });
                if !free {
                    first.get_or_insert(i as u32);
                    waiting += 1;
                }
            }
            let kind = j.manifest.kind;
            let Some(task) = first else {
                continue;
            };
            if j.waiting_signal.as_ref() == Some(&able) {
                continue;
            }
            if let Some(j) = self.jobs.get_mut(&job) {
                j.waiting_signal = Some(able);
            }
            self.event(Event::WaitingForChecker {
                job,
                task,
                waiting,
                kind,
            });
        }
    }

    fn event(&mut self, e: Event) {
        self.out.push(Action::Event(e));
    }

    fn send(&mut self, to: EndpointId, msg: Msg) {
        self.out.push(Action::Send { to, msg });
    }

    fn bounds(&self) -> DeadlineBounds {
        DeadlineBounds {
            floor: self.cfg.deadline_floor_ms,
            ceil: self.cfg.deadline_ceil_ms,
            exec_ceil: self.cfg.exec_deadline_ceil_ms,
        }
    }

    /// The coordinator can check results by running tasks itself.
    fn can_local(&self) -> bool {
        self.cfg.local_slots > 0 || self.cfg.verify_slots > 0
    }

    /// The coordinator can check results of `job` itself.
    fn can_local_job(&self, job: u32) -> bool {
        self.can_local() && self.jobs.get(&job).is_some_and(|j| !j.no_local)
    }

    /// The coordinator cannot run `job`'s kind: pending recomputes fall back to two agreeing
    /// peer executions.
    fn give_up_local(&mut self, job: u32) {
        let Some(j) = self.jobs.get_mut(&job) else {
            return;
        };
        if j.no_local {
            return;
        }
        j.no_local = true;
        let pending: Vec<u32> = j
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.local_pending && !t.local_running)
            .map(|(i, _)| i as u32)
            .collect();
        for task in pending {
            if let Some(t) = self.task_mut(job, task) {
                let before = t.snapshot();
                t.local_failed();
                self.after_change(job, task, before);
            }
        }
        let count = self.jobs.get(&job).map_or(0, |j| j.tasks.len());
        for task in 0..count as u32 {
            self.ensure_queued(job, task);
        }
    }

    /// The coordinator also takes fresh work, not only checks.
    fn contributes(&self) -> bool {
        self.cfg.local_slots > 0
    }

    /// Local executions a host should run at once: contribution and verification share them.
    pub fn local_capacity(&self) -> u16 {
        self.cfg.local_slots.max(self.cfg.verify_slots)
    }

    /// Registers a job and signs its manifest. Peers already admitted receive it with the
    /// next returned (or drained) actions.
    pub fn submit(&mut self, now: u64, spec: JobSpec) -> Result<u32, CoreError> {
        self.begin(now);
        if self.jobs.len() >= MAX_JOBS {
            return Err(CoreError::TooManyJobs(MAX_JOBS));
        }
        let JobSpec {
            mut manifest,
            inputs,
            compare,
        } = spec;
        if manifest.tasks as usize != inputs.len() {
            return Err(CoreError::TaskCount {
                tasks: manifest.tasks,
                inputs: inputs.len(),
            });
        }
        let cmp = match (&manifest.compare, compare) {
            (Comparator::Exact, _) => Cmp::Exact,
            (Comparator::None, _) => Cmp::Any,
            (_, Some(c)) => Cmp::Custom(c),
            (_, None) => return Err(CoreError::MissingComparator),
        };
        let job_id = self.next_job;
        manifest.job = job_id;
        manifest.coordinator = self.id;
        if manifest.deadline_ms == 0 {
            manifest.deadline_ms = DEFAULT_DEADLINE_MS;
        }
        manifest.check()?;
        let signed = manifest.sign(&*self.signer)?;
        let assets =
            BoundedVec::try_from(manifest.assets.iter().map(|a| a.hash).collect::<Vec<_>>())
                .map_err(|_| nuee_proto::ManifestError::Invalid("too many assets"))?;
        let mut tasks = Vec::with_capacity(inputs.len());
        tasks.resize_with(inputs.len(), TaskState::default);
        let job = Job {
            manifest: Arc::new(manifest),
            signed: signed.clone(),
            assets,
            inputs,
            cmp,
            tasks,
            queue: VecDeque::new(),
            known: Vec::new(),
            suspects: BTreeSet::new(),
            done: 0,
            verified: 0,
            failed: 0,
            started: now,
            finished: false,
            no_local: false,
            waiting_signal: None,
            refused: BTreeSet::new(),
            outcomes: BTreeMap::new(),
        };
        self.next_job += 1;
        self.jobs.insert(job_id, job);
        // Fresh tasks go out in a seeded random order: a lease's task index then says nothing
        // about whether it is fresh work, a retry or a canary.
        let count = self.jobs.get(&job_id).map_or(0, |j| j.tasks.len());
        let mut order: Vec<u32> = (0..count as u32).collect();
        order.shuffle(&mut self.rng);
        for t in order {
            self.ensure_queued(job_id, t);
        }
        let peers: Vec<EndpointId> = self.peers.keys().copied().collect();
        for p in peers {
            self.send(p, Msg::Job(signed.clone()));
        }
        self.check_job_done(job_id);
        self.dispatch_all();
        Ok(job_id)
    }

    /// Drops a finished job (after its `JobDone`), freeing its slot among the `MAX_JOBS`.
    /// Returns false, and keeps the job, when it is unknown or not finished.
    pub fn forget(&mut self, job: u32) -> bool {
        if !self.jobs.get(&job).is_some_and(|j| j.finished) {
            return false;
        }
        let leases: Vec<u64> = self
            .leases
            .iter()
            .filter(|(_, r)| r.job == job)
            .map(|(id, _)| *id)
            .collect();
        for lease in leases {
            if let Some(rec) = self.take_lease(lease, None) {
                self.send(rec.peer, Msg::Cancel { lease });
            }
        }
        self.jobs.remove(&job);
        true
    }

    /// Records a task already completed in an earlier run (resume).
    pub fn mark_done(&mut self, job: u32, task: u32, output_hash: Hash) {
        let id = self.id;
        let Some(t) = self.task_mut(job, task) else {
            return;
        };
        let before = t.snapshot();
        t.accepted = Some(Exec {
            by: id,
            hash: output_hash,
            output: None,
            ms: None,
        });
        t.verified = true;
        t.local_checked = true;
        t.results.clear();
        t.local_pending = false;
        t.verifying = false;
        self.after_change(job, task, before);
    }

    /// A peer passed the admission proof.
    pub fn admitted(&mut self, now: u64, peer: EndpointId, caps: Caps) -> Vec<Action> {
        self.begin(now);
        if self.banned.contains_key(&peer) {
            self.bye(peer, ByeReason::Banned);
            return self.finish();
        }
        if self.peers.contains_key(&peer) || self.pending.contains_key(&peer) {
            self.drop_peer(peer);
        }
        if self.peers.len() + self.pending.len() >= self.cfg.max_peers {
            self.bye(peer, ByeReason::Full);
            return self.finish();
        }
        if self.cfg.approve {
            self.pending.insert(peer, caps.clone());
            self.send(peer, Msg::Pending);
            self.event(Event::PeerPending { peer, caps });
        } else {
            self.join(peer, caps);
        }
        self.finish()
    }

    /// Manual decision on a pending peer.
    pub fn approve(&mut self, now: u64, peer: EndpointId, accept: bool) -> Vec<Action> {
        self.begin(now);
        if let Some(caps) = self.pending.remove(&peer) {
            if accept {
                self.join(peer, caps);
            } else {
                self.bye(peer, ByeReason::Rejected);
            }
        }
        self.finish()
    }

    /// The connection to a peer is gone.
    pub fn disconnected(&mut self, now: u64, peer: EndpointId) -> Vec<Action> {
        self.begin(now);
        if self.drop_peer(peer) {
            self.event(Event::PeerLeft { peer });
        }
        self.dispatch_all();
        self.finish()
    }

    /// A message from an admitted peer.
    pub fn handle(&mut self, now: u64, from: EndpointId, msg: Msg) -> Vec<Action> {
        self.begin(now);
        if !self.peers.contains_key(&from) {
            return self.finish();
        }
        match msg {
            Msg::Want { slots } => {
                let max = self.cfg.max_leases_per_peer;
                if let Some(p) = self.peers.get_mut(&from) {
                    p.credit = if slots == 0 {
                        0
                    } else {
                        p.credit.saturating_add(slots).min(max)
                    };
                }
                self.dispatch(from);
            }
            Msg::Result {
                lease,
                output,
                output_hash,
                sig,
                ..
            } => self.on_result(from, lease, output, output_hash, &sig),
            Msg::Fail { lease, kind, .. } => {
                if let Some(rec) = self.take_lease(lease, Some(from)) {
                    if kind == FailKind::Unsupported {
                        self.on_refused(rec.job, rec.task, from);
                    } else {
                        self.on_fail(rec.job, rec.task, from, kind);
                    }
                }
            }
            other => {
                if let Some((kind, signal)) = RtcKind::split(other) {
                    self.relay_rtc(from, kind, signal);
                }
            }
        }
        self.dispatch_all();
        self.finish()
    }

    /// Bytes of a blob output requested with `FetchOutput`.
    pub fn output_fetched(&mut self, now: u64, lease: u64, bytes: Vec<u8>) -> Vec<Action> {
        self.begin(now);
        if let Some(f) = self.fetching.remove(&lease) {
            if let Some(t) = self.task_mut(f.job, f.task) {
                t.in_flight = t.in_flight.saturating_sub(1);
            }
            if Hash::of(&bytes) != f.hash {
                self.ban(f.peer);
            } else {
                let exec = Exec {
                    by: f.peer,
                    hash: f.hash,
                    output: Some(Arc::from(bytes)),
                    ms: Some(self.now.saturating_sub(f.issued)),
                };
                self.accept_result(f.job, f.task, exec);
            }
            self.dispatch_all();
        }
        self.finish()
    }

    /// The coordinator has `free` local slots; returns `RunLocal` actions.
    pub fn local_slots(&mut self, now: u64, free: u16) -> Vec<Action> {
        self.begin(now);
        if !self.can_local() {
            return self.finish();
        }
        for _ in 0..free {
            if !self.next_local() {
                break;
            }
        }
        self.finish()
    }

    /// A `RunLocal` lease finished.
    pub fn local_done(&mut self, now: u64, lease: u64, out: Outcome) -> Vec<Action> {
        self.begin(now);
        let Some(rec) = self.take_lease(lease, Some(self.id)) else {
            return self.finish();
        };
        match out {
            Outcome::Ok(bytes) => {
                let exec = Exec {
                    by: self.id,
                    hash: Hash::of(&bytes),
                    output: Some(Arc::from(bytes)),
                    ms: Some(self.now.saturating_sub(rec.issued)),
                };
                self.accept_result(rec.job, rec.task, exec);
            }
            Outcome::Fail(kind, _) => {
                if kind == FailKind::Unsupported {
                    self.give_up_local(rec.job);
                }
                if rec.purpose == Purpose::LocalVerify {
                    if let Some(t) = self.task_mut(rec.job, rec.task) {
                        let before = t.snapshot();
                        t.local_failed();
                        let last = t.last_chance && t.accepted.is_none() && !t.failed;
                        if last {
                            self.give_up(rec.job, rec.task, kind);
                        }
                        self.after_change(rec.job, rec.task, before);
                    }
                } else if kind == FailKind::Unsupported {
                    // Not the task's fault: it goes back to the peers.
                    self.ensure_queued(rec.job, rec.task);
                } else {
                    let id = self.id;
                    self.on_fail(rec.job, rec.task, id, kind);
                }
            }
        }
        self.dispatch_all();
        self.finish()
    }

    /// Expires leases past their deadline and stalled output transfers.
    pub fn tick(&mut self, now: u64) -> Vec<Action> {
        self.begin(now);
        let expired: Vec<u64> = self
            .leases
            .iter()
            .filter(|(_, r)| r.peer != self.id && r.deadline_at <= now)
            .map(|(id, _)| *id)
            .collect();
        for lease in expired {
            if let Some(rec) = self.take_lease(lease, None) {
                self.send(rec.peer, Msg::Cancel { lease });
                if let Some(t) = self.task_mut(rec.job, rec.task) {
                    t.note_soft(rec.peer);
                    t.overran = true;
                }
                self.ensure_queued(rec.job, rec.task);
            }
        }
        let ceil = self.cfg.deadline_ceil_ms;
        let stalled: Vec<u64> = self
            .fetching
            .iter()
            .filter(|(_, f)| now.saturating_sub(f.since) > ceil)
            .map(|(id, _)| *id)
            .collect();
        for lease in stalled {
            if let Some(f) = self.fetching.remove(&lease) {
                if let Some(t) = self.task_mut(f.job, f.task) {
                    t.in_flight = t.in_flight.saturating_sub(1);
                    t.runners.retain(|r| *r != f.peer);
                }
                self.on_fail(f.job, f.task, f.peer, FailKind::Io);
            }
        }
        self.dispatch_all();
        self.finish()
    }

    /// Sends a signalling message from the coordinator to a peer.
    pub fn send_rtc(
        &mut self,
        to: EndpointId,
        kind: RtcKind,
        payload: &str,
    ) -> Result<Vec<Action>, CoreError> {
        if !self.peers.contains_key(&to) {
            return Err(CoreError::NotConnected);
        }
        let payload: BoundedString<MAX_SIGNAL> =
            BoundedString::try_from(payload).map_err(|_| CoreError::SignalTooLarge)?;
        let msg = kind.wrap(RtcSignal {
            peer: self.id,
            payload,
        });
        self.send(to, msg);
        Ok(self.finish())
    }

    /// Snapshot for UIs.
    pub fn status(&self) -> SwarmStatus {
        let mut peers: Vec<PeerStatus> = self
            .peers
            .iter()
            .map(|(id, p)| PeerStatus {
                id: *id,
                browser: p.caps.browser,
                slots: p.caps.cpu_slots,
                inflight: u16::try_from(p.inflight.len()).unwrap_or(u16::MAX),
                done: p.done,
                verified: p.verified,
                banned: false,
            })
            .collect();
        peers.extend(self.banned.iter().map(|(id, b)| PeerStatus {
            id: *id,
            browser: b.browser,
            slots: b.slots,
            inflight: 0,
            done: 0,
            verified: 0,
            banned: true,
        }));
        let jobs = self
            .jobs
            .iter()
            .map(|(id, j)| JobStatus {
                job: *id,
                name: j.manifest.name.as_str().to_owned(),
                total: j.manifest.tasks,
                done: j.done,
                verified: j.verified,
                failed: j.failed,
                in_flight: j
                    .tasks
                    .iter()
                    .map(|t| t.in_flight + u32::from(t.local_running))
                    .sum(),
                rate_per_s: rate(j.done, j.started, self.now),
            })
            .collect();
        SwarmStatus { peers, jobs }
    }

    // ---- admission ----

    fn bye(&mut self, peer: EndpointId, reason: ByeReason) {
        self.send(peer, Msg::Bye { reason });
        self.out.push(Action::Close { peer, reason });
    }

    fn join(&mut self, peer: EndpointId, caps: Caps) {
        self.peers_changed = self.now;
        self.peers.insert(
            peer,
            Peer {
                caps: caps.clone(),
                credit: 0,
                inflight: BTreeSet::new(),
                durations: Durations::default(),
                done: 0,
                verified: 0,
                rtc_window: self.now,
                rtc_count: 0,
            },
        );
        let manifests: Vec<SignedManifest> = self.jobs.values().map(|j| j.signed.clone()).collect();
        let (first, rest) = manifests.split_at(manifests.len().min(WELCOME_JOBS));
        let jobs = BoundedVec::try_from(first).unwrap_or_default();
        let welcome = Msg::Welcome {
            swarm: SwarmInfo {
                name: self.cfg.name.clone(),
                coordinator: self.id,
                mode: SwarmMode::Compute,
                file: None,
            },
            jobs,
            webrtc: self.cfg.webrtc,
            stun: self.cfg.stun.clone(),
        };
        self.send(peer, welcome);
        for m in rest {
            self.send(peer, Msg::Job(m.clone()));
        }
        self.event(Event::PeerJoined { peer, caps });
    }

    /// Removes a peer and re-queues its work. Returns whether it was admitted.
    fn drop_peer(&mut self, peer: EndpointId) -> bool {
        self.pending.remove(&peer);
        let Some(p) = self.peers.remove(&peer) else {
            return false;
        };
        for j in self.jobs.values_mut() {
            j.refused.remove(&peer);
            j.outcomes.remove(&peer);
        }
        self.peers_changed = self.now;
        for lease in p.inflight {
            if let Some(rec) = self.leases.remove(&lease) {
                if let Some(t) = self.task_mut(rec.job, rec.task) {
                    t.in_flight = t.in_flight.saturating_sub(1);
                    t.runners.retain(|r| *r != peer);
                }
                self.ensure_queued(rec.job, rec.task);
            }
        }
        let fetches: Vec<u64> = self
            .fetching
            .iter()
            .filter(|(_, f)| f.peer == peer)
            .map(|(id, _)| *id)
            .collect();
        for lease in fetches {
            if let Some(f) = self.fetching.remove(&lease) {
                if let Some(t) = self.task_mut(f.job, f.task) {
                    t.in_flight = t.in_flight.saturating_sub(1);
                    t.runners.retain(|r| *r != peer);
                }
                self.ensure_queued(f.job, f.task);
            }
        }
        true
    }

    fn ban(&mut self, peer: EndpointId) {
        if self.banned.contains_key(&peer) {
            return;
        }
        let known = self
            .peers
            .get(&peer)
            .map(|p| &p.caps)
            .or_else(|| self.pending.get(&peer));
        let record = BannedPeer {
            browser: known.is_some_and(|c| c.browser),
            slots: known.map_or(0, |c| c.cpu_slots),
        };
        self.banned.insert(peer, record);
        self.drop_peer(peer);
        self.bye(peer, ByeReason::Banned);
        let mut redo = 0u32;
        let jobs: Vec<u32> = self.jobs.keys().copied().collect();
        for job in jobs {
            let count = self.jobs.get(&job).map_or(0, |j| j.tasks.len());
            for task in 0..count as u32 {
                let Some(t) = self.task_mut(job, task) else {
                    continue;
                };
                let before = t.snapshot();
                if t.purge(&peer) {
                    redo += 1;
                    self.after_change(job, task, before);
                }
            }
        }
        self.event(Event::Banned { peer, redo });
    }

    // ---- signalling ----

    fn relay_rtc(&mut self, from: EndpointId, kind: RtcKind, signal: RtcSignal) {
        if !self.cfg.webrtc {
            return;
        }
        let now = self.now;
        let Some(p) = self.peers.get_mut(&from) else {
            return;
        };
        if now.saturating_sub(p.rtc_window) >= RTC_WINDOW_MS {
            p.rtc_window = now;
            p.rtc_count = 0;
        }
        if p.rtc_count >= RTC_PER_MINUTE {
            return;
        }
        p.rtc_count += 1;
        let target = signal.peer;
        if target == self.id {
            self.event(Event::Rtc {
                from,
                kind,
                payload: signal.payload.into_inner(),
            });
        } else if self.peers.contains_key(&target) {
            let msg = kind.wrap(RtcSignal {
                peer: from,
                payload: signal.payload,
            });
            self.send(target, msg);
        }
    }

    // ---- scheduling ----

    fn task_mut(&mut self, job: u32, task: u32) -> Option<&mut TaskState> {
        self.jobs.get_mut(&job)?.tasks.get_mut(task as usize)
    }

    fn ensure_queued(&mut self, job: u32, task: u32) {
        let can_local = self.can_local_job(job);
        let Some(j) = self.jobs.get_mut(&job) else {
            return;
        };
        let mode = j.mode();
        let Some(t) = j.tasks.get_mut(task as usize) else {
            return;
        };
        let want = t.want(mode, can_local);
        while t.queued < want {
            j.queue.push_back(task);
            t.queued += 1;
        }
    }

    /// Removes a lease, checking its owner when `owner` is given.
    fn take_lease(&mut self, lease: u64, owner: Option<EndpointId>) -> Option<LeaseRec> {
        let rec = self.leases.get(&lease)?;
        if owner.is_some_and(|o| o != rec.peer) {
            return None;
        }
        let rec = self.leases.remove(&lease)?;
        if let Some(p) = self.peers.get_mut(&rec.peer) {
            p.inflight.remove(&lease);
        }
        let peer = rec.peer;
        let local = rec.purpose != Purpose::Normal;
        if let Some(t) = self.task_mut(rec.job, rec.task) {
            if local {
                t.local_running = false;
            } else {
                t.in_flight = t.in_flight.saturating_sub(1);
                if owner.is_none() {
                    t.runners.retain(|r| *r != peer);
                }
            }
        }
        Some(rec)
    }

    fn dispatch_all(&mut self) {
        let ready: Vec<EndpointId> = self
            .peers
            .iter()
            .filter(|(_, p)| p.credit > 0)
            .map(|(id, _)| *id)
            .collect();
        for p in ready {
            self.dispatch(p);
        }
    }

    fn dispatch(&mut self, peer: EndpointId) {
        let max = usize::from(self.cfg.max_leases_per_peer);
        loop {
            let Some(p) = self.peers.get(&peer) else {
                return;
            };
            if p.credit == 0 || p.inflight.len() >= max {
                return;
            }
            let caps = p.caps.clone();
            if !self.assign_one(peer, &caps) {
                return;
            }
        }
    }

    /// Gives one lease to `peer`, round-robin across jobs. Returns false when nothing fits.
    fn assign_one(&mut self, peer: EndpointId, caps: &Caps) -> bool {
        let ids: Vec<u32> = self.jobs.keys().copied().collect();
        let n = ids.len();
        let peer_count = self.peers.len();
        for k in 0..n {
            let idx = (self.rr + k) % n;
            let Some(&job) = ids.get(idx) else {
                continue;
            };
            let Some(j) = self.jobs.get(&job) else {
                continue;
            };
            if j.queue.is_empty() || !routable(caps, &j.manifest) || j.refused.contains(&peer) {
                continue;
            }
            let canary_rate = if j.mode() != VerifyMode::None {
                u32::from(j.manifest.verify.canary_permille)
            } else {
                0
            };
            if canary_rate > 0 && !j.known.is_empty() {
                let roll = self.rng.random_range(0..1000u32);
                if roll < canary_rate
                    && let Some(task) = self.pick_canary(job, peer)
                {
                    self.issue(peer, job, task);
                    self.rr = (idx + 1) % n;
                    return true;
                }
            }
            if let Some(task) = self.pick_task(job, peer, peer_count) {
                self.issue(peer, job, task);
                self.rr = (idx + 1) % n;
                return true;
            }
        }
        false
    }

    fn pick_canary(&mut self, job: u32, peer: EndpointId) -> Option<u32> {
        let j = self.jobs.get(&job)?;
        let n = j.known.len();
        if n == 0 {
            return None;
        }
        let exact = matches!(j.cmp, Cmp::Exact);
        let start = self.rng.random_range(0..n);
        for k in 0..n.min(8) {
            let task = *j.known.get((start + k) % n)?;
            let t = j.tasks.get(task as usize)?;
            let usable = t
                .accepted
                .as_ref()
                .is_some_and(|a| exact || a.output.is_some());
            if t.verified
                && t.local_checked
                && usable
                && !t.local_pending
                && !t.runners.contains(&peer)
                && t.runners.len() < CANARY_MAX_RUNNERS
            {
                return Some(task);
            }
        }
        None
    }

    fn pick_task(&mut self, job: u32, peer: EndpointId, peer_count: usize) -> Option<u32> {
        let can_local = self.can_local_job(job);
        let max = self.cfg.max_attempts;
        let j = self.jobs.get_mut(&job)?;
        let mode = j.mode();
        let mut i = 0;
        while let Some(&task) = j.queue.get(i) {
            let Some(t) = j.tasks.get_mut(task as usize) else {
                j.queue.remove(i);
                continue;
            };
            if t.queued > t.want(mode, can_local) {
                j.queue.remove(i);
                t.queued = t.queued.saturating_sub(1);
                continue;
            }
            let soft_blocked = t.soft.contains(&peer) && t.soft.len() < peer_count;
            if t.runners.contains(&peer) || soft_blocked || t.barred(&peer, max) {
                i += 1;
                continue;
            }
            j.queue.remove(i);
            t.queued = t.queued.saturating_sub(1);
            return Some(task);
        }
        None
    }

    fn lease_msg(&self, job: u32, task: u32, lease: u64, deadline_ms: u32) -> Option<Lease> {
        let j = self.jobs.get(&job)?;
        Some(Lease {
            lease,
            job,
            task,
            kind: j.manifest.kind,
            module: j.manifest.module,
            assets: j.assets.clone(),
            input: j.inputs.get(task as usize)?.clone(),
            deadline_ms,
        })
    }

    fn issue(&mut self, peer: EndpointId, job: u32, task: u32) {
        let bounds = self.bounds();
        let Some(p) = self.peers.get(&peer) else {
            return;
        };
        let Some(j) = self.jobs.get(&job) else {
            return;
        };
        let mut deadline_ms = lease_deadline(&p.durations, &j.manifest, bounds);
        // The peer's history says how long its other tasks took, not this one: once this task
        // overran such an estimate, it gets the job's own deadline, as a first lease does.
        // Otherwise a task much longer than its siblings times out on every peer, every time.
        if j.tasks.get(task as usize).is_some_and(|t| t.overran) {
            deadline_ms =
                deadline_ms.max(lease_deadline(&Durations::default(), &j.manifest, bounds));
        }
        let lease = self.next_lease;
        let Some(msg) = self.lease_msg(job, task, lease, deadline_ms) else {
            return;
        };
        self.next_lease += 1;
        self.leases.insert(
            lease,
            LeaseRec {
                peer,
                job,
                task,
                purpose: Purpose::Normal,
                issued: self.now,
                deadline_at: self.now.saturating_add(u64::from(deadline_ms)),
            },
        );
        if let Some(t) = self.task_mut(job, task) {
            t.in_flight += 1;
            t.runners.push(peer);
        }
        if let Some(p) = self.peers.get_mut(&peer) {
            p.credit = p.credit.saturating_sub(1);
            p.inflight.insert(lease);
        }
        self.send(peer, Msg::Lease(msg));
    }

    /// Starts one local execution. Returns false when there is nothing to run.
    fn next_local(&mut self) -> bool {
        while let Some((job, task)) = self.local_verify.pop_front() {
            let ready = self
                .jobs
                .get(&job)
                .and_then(|j| j.tasks.get(task as usize))
                .is_some_and(|t| t.local_pending && !t.local_running && !t.failed)
                && self.can_local_job(job);
            if ready {
                self.run_local(job, task, Purpose::LocalVerify);
                return true;
            }
        }
        let ids: Vec<u32> = self.jobs.keys().copied().collect();
        let contributes = self.contributes();
        for precompute in [true, false] {
            if !precompute && !contributes {
                break;
            }
            for &job in &ids {
                let Some(j) = self.jobs.get(&job) else {
                    continue;
                };
                if j.no_local {
                    continue;
                }
                // A verify-only coordinator still computes one known answer so canaries can
                // start, unless that would take the job's only task away from the peers.
                if precompute
                    && !(j.mode() != VerifyMode::None
                        && j.manifest.verify.canary_permille > 0
                        && j.known.is_empty()
                        && (contributes || j.tasks.len() > 1))
                {
                    continue;
                }
                let mode = j.mode();
                let found = j.queue.iter().copied().find(|&task| {
                    j.tasks.get(task as usize).is_some_and(|t| {
                        t.want(mode, true) > 0 && t.in_flight == 0 && t.results.is_empty()
                    })
                });
                if let Some(task) = found {
                    self.run_local(job, task, Purpose::LocalTask);
                    return true;
                }
            }
        }
        // Nothing fresh: a peer result still waiting for its second execution can get it
        // from the coordinator, so a lone peer is not left unconfirmed. That execution is a
        // check, never fresh work: a verify-only coordinator runs it on its verification
        // capacity, but only when no other admitted peer could give that second execution and
        // the peer set has been stable for a while.
        let settled = self.now.saturating_sub(self.peers_changed) >= LONE_CHECK_GRACE_MS;
        if !contributes && !settled {
            return false;
        }
        let me = self.id;
        for &job in &ids {
            let Some(j) = self.jobs.get(&job) else {
                continue;
            };
            if j.no_local {
                continue;
            }
            let mode = j.mode();
            let able: Vec<EndpointId> = if contributes {
                Vec::new()
            } else {
                self.peers
                    .iter()
                    .filter(|(id, p)| routable(&p.caps, &j.manifest) && !j.refused.contains(*id))
                    .map(|(id, _)| *id)
                    .collect()
            };
            let found = j.queue.iter().copied().find(|&task| {
                j.tasks.get(task as usize).is_some_and(|t| {
                    t.want(mode, true) > 0
                        && t.in_flight == 0
                        && !t.results.is_empty()
                        && t.results.iter().all(|e| e.by != me)
                        && able
                            .iter()
                            .all(|p| t.runners.contains(p) || t.results.iter().any(|e| e.by == *p))
                })
            });
            if let Some(task) = found {
                self.run_local(job, task, Purpose::LocalTask);
                return true;
            }
        }
        false
    }

    fn run_local(&mut self, job: u32, task: u32, purpose: Purpose) {
        let lease = self.next_lease;
        let Some(j) = self.jobs.get(&job) else {
            return;
        };
        let manifest = j.manifest.clone();
        let Some(msg) = self.lease_msg(job, task, lease, manifest.deadline_ms) else {
            return;
        };
        self.next_lease += 1;
        self.leases.insert(
            lease,
            LeaseRec {
                peer: self.id,
                job,
                task,
                purpose,
                issued: self.now,
                deadline_at: u64::MAX,
            },
        );
        if let Some(t) = self.task_mut(job, task) {
            t.local_running = true;
        }
        self.out.push(Action::RunLocal {
            lease: msg,
            manifest,
        });
    }

    // ---- results ----

    fn on_result(
        &mut self,
        from: EndpointId,
        lease: u64,
        output: Payload,
        output_hash: Hash,
        sig: &nuee_proto::Signature,
    ) {
        let Some(rec) = self.take_lease(lease, Some(from)) else {
            return;
        };
        let elapsed = self.now.saturating_sub(rec.issued);
        if let Some(p) = self.peers.get_mut(&from) {
            p.durations.push(elapsed);
        }
        if !verify_result(&from, lease, &output_hash, sig) {
            self.ban(from);
            return;
        }
        match output {
            Payload::Inline(bytes) => {
                if Hash::of(&bytes) != output_hash {
                    self.ban(from);
                    return;
                }
                let exec = Exec {
                    by: from,
                    hash: output_hash,
                    output: Some(Arc::from(bytes.into_inner())),
                    ms: Some(elapsed),
                };
                self.accept_result(rec.job, rec.task, exec);
            }
            Payload::Blob { hash, size } => {
                if hash != output_hash {
                    self.ban(from);
                    return;
                }
                if let Some(t) = self.task_mut(rec.job, rec.task) {
                    t.in_flight += 1;
                }
                self.fetching.insert(
                    lease,
                    Fetching {
                        peer: from,
                        job: rec.job,
                        task: rec.task,
                        hash,
                        issued: rec.issued,
                        since: self.now,
                    },
                );
                self.out.push(Action::FetchOutput {
                    from,
                    lease,
                    hash,
                    size,
                });
            }
        }
    }

    /// A peer cannot run this job (e.g. an unapproved exec command): not the task's fault.
    fn on_refused(&mut self, job: u32, task: u32, by: EndpointId) {
        let Some(j) = self.jobs.get_mut(&job) else {
            return;
        };
        j.refused.insert(by);
        if let Some(t) = j.tasks.get_mut(task as usize) {
            t.runners.retain(|r| *r != by);
        }
        self.ensure_queued(job, task);
    }

    fn on_fail(&mut self, job: u32, task: u32, by: EndpointId, kind: FailKind) {
        let max = self.cfg.max_attempts;
        let local = self.id;
        let can_local = self.can_local_job(job);
        if by != local {
            self.note_outcome(job, by, false);
        }
        let Some(t) = self.task_mut(job, task) else {
            return;
        };
        let before = t.snapshot();
        let exhausted = t.note_fail(by, max);
        t.runners.retain(|r| *r != by);
        t.note_soft(by);
        if kind == FailKind::Timeout {
            t.overran = true;
        }
        // Once the coordinator's own last-chance run is queued or running, a peer lease that
        // was still out when attempts ran out failing too changes nothing: that run alone
        // settles or fails the task.
        let awaiting_local = by != local && t.last_chance && (t.local_pending || t.local_running);
        // An output still waiting for its check stays waiting: failing the checking leases
        // never settles it unchecked.
        if exhausted && !t.failed && t.accepted.is_none() && !awaiting_local {
            if can_local && by != local && !t.last_chance {
                // Peers gave up (perhaps a few identities of one attacker answering `Fail`):
                // the coordinator runs the task itself before failing it.
                t.last_chance = true;
                t.local_pending = true;
                self.local_verify.push_back((job, task));
            } else {
                self.give_up(job, task, kind);
            }
        }
        self.after_change(job, task, before);
    }

    /// Fails a task for good.
    fn give_up(&mut self, job: u32, task: u32, kind: FailKind) {
        let Some(t) = self.task_mut(job, task) else {
            return;
        };
        if t.failed {
            return;
        }
        t.failed = true;
        t.local_pending = false;
        t.results.clear();
        if let Some(j) = self.jobs.get_mut(&job) {
            j.failed += 1;
        }
        self.event(Event::TaskFailed { job, task, kind });
    }

    /// Counts a result or a failure of `peer` on `job`; a peer that fails far more often than
    /// it succeeds stops receiving the job's leases.
    fn note_outcome(&mut self, job: u32, peer: EndpointId, ok: bool) {
        let Some(j) = self.jobs.get_mut(&job) else {
            return;
        };
        let e = j.outcomes.entry(peer).or_default();
        if ok {
            e.0 = e.0.saturating_add(1);
        } else {
            e.1 = e.1.saturating_add(1);
        }
        let (good, bad) = *e;
        if bad >= SHUN_MIN_FAILS && bad > good.saturating_mul(2) {
            j.refused.insert(peer);
        }
    }

    fn accept_result(&mut self, job: u32, task: u32, exec: Exec) {
        let local = self.id;
        let can_local = self.can_local_job(job);
        let sample_hit = {
            let Some(j) = self.jobs.get(&job) else {
                return;
            };
            let permille = u32::from(j.manifest.verify.sample_permille);
            self.rng.random_range(0..1000u32) < permille
        };
        if exec.by != local {
            self.note_outcome(job, exec.by, true);
        }
        let Some(j) = self.jobs.get_mut(&job) else {
            return;
        };
        let mode = j.mode();
        let cmp = j.cmp.clone();
        let Some(t) = j.tasks.get_mut(task as usize) else {
            return;
        };
        let before = t.snapshot();
        if j.suspects.contains(&exec.by) && !t.verified {
            t.require_two = true;
        }
        t.results.push(exec);
        let ctx = Ctx {
            mode,
            cmp: &cmp,
            local,
            can_local,
        };
        let r = t.resolve(&ctx, sample_hit);
        for c in &r.credit {
            if let Some(p) = self.peers.get_mut(c) {
                p.verified += 1;
            }
        }
        if let Some(peer) = r.mismatch {
            self.event(Event::Mismatch { job, task, peer });
        }
        if r.need_local {
            self.local_verify.push_back((job, task));
        }
        self.after_change(job, task, before);
        for s in r.suspects {
            self.suspect(job, s);
        }
        for b in r.ban {
            if b != local {
                self.ban(b);
            }
        }
    }

    /// No ban without a coordinator recompute: a peer in a disagreement nobody could
    /// arbitrate has its unconfirmed work re-executed, and its future work double-checked.
    fn suspect(&mut self, job: u32, peer: EndpointId) {
        let Some(j) = self.jobs.get_mut(&job) else {
            return;
        };
        if !j.suspects.insert(peer) {
            return;
        }
        for task in 0..j.tasks.len() as u32 {
            let Some(t) = self.task_mut(job, task) else {
                continue;
            };
            if t.verified || !t.accepted.as_ref().is_some_and(|a| a.by == peer) {
                continue;
            }
            let before = t.snapshot();
            t.purge(&peer);
            t.require_two = true;
            self.after_change(job, task, before);
        }
    }

    /// Emits what changed on a task and keeps counters, queue and job state in sync.
    fn after_change(&mut self, job: u32, task: u32, before: Snapshot) {
        let now = self.now;
        let Some(j) = self.jobs.get_mut(&job) else {
            return;
        };
        let Some(t) = j.tasks.get_mut(task as usize) else {
            return;
        };
        let after = t.snapshot();
        let mut emits = Vec::new();
        match (before.0, after.0) {
            (Some(_), None) => j.done = j.done.saturating_sub(1),
            (None, Some(_)) => j.done += 1,
            _ => {}
        }
        let mut accepted_by = None;
        if after.0.is_some()
            && after.0 != before.0
            && let Some(acc) = &t.accepted
        {
            accepted_by = Some(acc.by);
            emits.push(Action::Event(Event::TaskAccepted {
                job,
                task,
                by: acc.by,
                ms: acc.ms,
            }));
            if let Some(output) = &acc.output {
                emits.push(Action::Accepted {
                    job,
                    task,
                    output: output.clone(),
                    output_hash: acc.hash,
                    by: acc.by,
                    ms: acc.ms,
                });
            }
        }
        match (before.1, after.1) {
            (false, true) => {
                j.verified += 1;
                if let Some((_, by)) = after.0 {
                    emits.push(Action::Event(Event::Verified { job, task, by }));
                }
            }
            (true, false) => j.verified = j.verified.saturating_sub(1),
            _ => {}
        }
        // Only an answer the coordinator computed itself may serve as a known answer.
        if t.verified && t.local_checked && !t.known {
            t.known = true;
            j.known.push(task);
        }
        if before != after {
            emits.push(Action::Event(Event::Progress {
                job,
                done: j.done,
                total: j.manifest.tasks,
                verified: j.verified,
                rate_per_s: rate(j.done, j.started, now),
            }));
        }
        self.out.extend(emits);
        if let Some(by) = accepted_by
            && let Some(p) = self.peers.get_mut(&by)
        {
            p.done += 1;
        }
        self.ensure_queued(job, task);
        self.check_job_done(job);
    }

    fn check_job_done(&mut self, job: u32) {
        let Some(j) = self.jobs.get_mut(&job) else {
            return;
        };
        let settled = j.done + j.failed >= j.manifest.tasks && j.tasks.iter().all(|t| t.settled());
        if settled && !j.finished {
            j.finished = true;
            let failed = j.failed;
            self.event(Event::JobDone { job, failed });
            let stale: Vec<u64> = self
                .leases
                .iter()
                .filter(|(_, r)| r.job == job && r.purpose == Purpose::Normal)
                .map(|(id, _)| *id)
                .collect();
            for lease in stale {
                if let Some(rec) = self.take_lease(lease, None) {
                    self.send(rec.peer, Msg::Cancel { lease });
                }
            }
        } else if !settled && j.finished {
            j.finished = false;
        }
    }
}

fn rate(done: u32, started: u64, now: u64) -> f64 {
    let elapsed = now.saturating_sub(started);
    if elapsed == 0 {
        0.0
    } else {
        f64::from(done) * 1000.0 / elapsed as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuee_proto::{SecretKey, TaskKind, VerifyPolicy};

    fn key(n: u8) -> SecretKey {
        SecretKey::from_bytes(&[n; 32])
    }

    fn caps(kind: TaskKind) -> Caps {
        Caps {
            agent: BoundedString::new(),
            browser: false,
            cpu_slots: 4,
            gpu: None,
            mem_mb: 1024,
            kinds: kind.bit(),
            cached: BoundedVec::new(),
            exec_allow: BoundedVec::new(),
            store_quota_mb: 0,
            relay: None,
            rtc: false,
        }
    }

    fn spec(kind: TaskKind, tasks: u32) -> JobSpec {
        JobSpec {
            manifest: Manifest {
                version: nuee_proto::PROTO_VERSION,
                job: 0,
                name: BoundedString::new(),
                kind,
                module: Some(Hash::of(b"module")),
                module_size: 6,
                max_memory: 1 << 20,
                assets: BoundedVec::new(),
                tasks,
                verify: VerifyPolicy {
                    mode: VerifyMode::None,
                    sample_permille: 0,
                    canary_permille: 0,
                },
                compare: Comparator::Exact,
                deadline_ms: 1000,
                exec: None,
                train: None,
                coordinator: key(0).public(),
            },
            inputs: (0..tasks)
                .map(|i| Payload::Inline(BoundedBytes::try_from(vec![i as u8]).unwrap_or_default()))
                .collect(),
            compare: None,
        }
    }

    use nuee_proto::BoundedBytes;

    fn leases(actions: &[Action]) -> Vec<(EndpointId, Lease)> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Send {
                    to,
                    msg: Msg::Lease(l),
                } => Some((*to, l.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn routes_only_to_capable_peers_and_respects_want() {
        let mut c = Coordinator::new(CoordinatorConfig::default(), Box::new(key(0)));
        c.submit(0, spec(TaskKind::Wgsl, 3)).ok();
        let a = key(1).public();
        let b = key(2).public();
        c.admitted(0, a, caps(TaskKind::Wasm));
        c.admitted(0, b, caps(TaskKind::Wgsl));
        let out = c.handle(0, a, Msg::Want { slots: 2 });
        assert!(
            leases(&out).is_empty(),
            "wasm-only peer must not get wgsl tasks"
        );
        // Wgsl needs a GPU as well.
        let out = c.handle(0, b, Msg::Want { slots: 2 });
        assert!(leases(&out).is_empty());

        let mut c = Coordinator::new(CoordinatorConfig::default(), Box::new(key(0)));
        c.submit(0, spec(TaskKind::Wasm, 5)).ok();
        c.admitted(0, a, caps(TaskKind::Wasm));
        let out = c.handle(0, a, Msg::Want { slots: 2 });
        let got = leases(&out);
        assert_eq!(got.len(), 2);
        // Two distinct tasks of the job; fresh tasks go out in a seeded random order.
        let tasks: BTreeSet<u32> = got.iter().map(|(_, l)| l.task).collect();
        assert_eq!(tasks.len(), 2);
        assert!(tasks.iter().all(|t| *t < 5));
    }

    #[test]
    fn accepted_tasks_carry_the_coordinator_measured_time() {
        let mut c = Coordinator::new(
            CoordinatorConfig {
                local_slots: 1,
                ..CoordinatorConfig::default()
            },
            Box::new(key(0)),
        );
        c.submit(0, spec(TaskKind::Wasm, 2)).ok();
        c.drain();
        let a = key(1).public();
        c.admitted(100, a, caps(TaskKind::Wasm));
        let out = c.handle(100, a, Msg::Want { slots: 1 });
        let (_, lease) = leases(&out).into_iter().next().expect("a lease");
        let output = vec![7u8, 7];
        let output_hash = Hash::of(&output);
        let sig = nuee_proto::sign_result(&key(1), lease.lease, &output_hash);
        let out = c.handle(
            350,
            a,
            Msg::Result {
                lease: lease.lease,
                output: Payload::Inline(BoundedBytes::try_from(output).unwrap_or_default()),
                output_hash,
                duration_ms: 1,
                sig,
            },
        );
        let event_ms = out.iter().find_map(|x| match x {
            Action::Event(Event::TaskAccepted { by, ms, .. }) if *by == a => Some(*ms),
            _ => None,
        });
        assert_eq!(
            event_ms,
            Some(Some(250)),
            "the worker's own duration is not used"
        );
        let action_ms = out.iter().find_map(|x| match x {
            Action::Accepted { ms, .. } => Some(*ms),
            _ => None,
        });
        assert_eq!(action_ms, Some(Some(250)));

        let local = c.local_slots(400, 1);
        let run = local.iter().find_map(|x| match x {
            Action::RunLocal { lease, .. } => Some(lease.lease),
            _ => None,
        });
        let out = c.local_done(440, run.expect("a local lease"), Outcome::Ok(vec![1]));
        assert!(
            out.iter()
                .any(|x| matches!(x, Action::Event(Event::TaskAccepted { ms: Some(40), .. })))
        );
    }

    /// One peer, one sampled task: without a check of its own the coordinator waits for a
    /// second peer that never comes (the hang of the CLI embed test with `--local-slots 0`).
    #[test]
    fn a_lone_peer_sampled_task_settles_only_with_a_coordinator_check() {
        for verify_slots in [0u16, 1] {
            let mut c = Coordinator::new(
                CoordinatorConfig {
                    local_slots: 0,
                    verify_slots,
                    ..CoordinatorConfig::default()
                },
                Box::new(key(0)),
            );
            let mut s = spec(TaskKind::Wasm, 1);
            s.manifest.verify = VerifyPolicy {
                mode: VerifyMode::Sample,
                sample_permille: 1000,
                canary_permille: 30,
            };
            c.submit(0, s).ok();
            c.drain();
            assert!(
                c.local_slots(0, 1).is_empty(),
                "a verify-only coordinator does not take the only task"
            );
            let a = key(1).public();
            c.admitted(0, a, caps(TaskKind::Wasm));
            let out = c.handle(0, a, Msg::Want { slots: 4 });
            let (_, lease) = leases(&out).into_iter().next().expect("a lease");
            let output = vec![9u8];
            let output_hash = Hash::of(&output);
            let sig = nuee_proto::sign_result(&key(1), lease.lease, &output_hash);
            let mut out = c.handle(
                10,
                a,
                Msg::Result {
                    lease: lease.lease,
                    output: Payload::Inline(
                        BoundedBytes::try_from(output.clone()).unwrap_or_default(),
                    ),
                    output_hash,
                    duration_ms: 1,
                    sig,
                },
            );
            assert!(
                leases(&out).is_empty(),
                "the peer never gets its own task back"
            );
            out.extend(c.local_slots(20, 1));
            if let Some(run) = out.iter().find_map(|x| match x {
                Action::RunLocal { lease, .. } => Some(lease.lease),
                _ => None,
            }) {
                out.extend(c.local_done(30, run, Outcome::Ok(output.clone())));
            }
            out.extend(c.tick(100_000));
            let done = out
                .iter()
                .any(|x| matches!(x, Action::Event(Event::JobDone { .. })));
            assert_eq!(done, verify_slots > 0, "verify_slots {verify_slots}");
        }
    }

    #[test]
    fn missing_comparator_is_refused() {
        let mut c = Coordinator::new(CoordinatorConfig::default(), Box::new(key(0)));
        let mut s = spec(TaskKind::Wasm, 1);
        s.manifest.compare = Comparator::Cosine { min: 0.9 };
        assert!(matches!(c.submit(0, s), Err(CoreError::MissingComparator)));
        let mut s = spec(TaskKind::Wasm, 2);
        s.inputs.pop();
        assert!(matches!(c.submit(0, s), Err(CoreError::TaskCount { .. })));
    }
}
