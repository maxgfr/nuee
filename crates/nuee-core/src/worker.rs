//! Worker state machine: manifests, lease admission against local limits, signed results.

use std::collections::BTreeMap;
use std::sync::Arc;

use nuee_proto::limits::{MAX_INLINE_OUTPUT, MAX_JOBS, MAX_SIGNAL};
use nuee_proto::{
    BoundedBytes, BoundedString, Caps, EndpointId, FailKind, Hash, Lease, LocalLimits, Manifest,
    Msg, PROTO_VERSION, Payload, RtcSignal, SignedManifest, Signer, sign_result,
};

use crate::events::{RtcKind, WorkerAction, WorkerEvent};
use crate::{CoreError, Outcome};

/// Worker settings.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Capabilities announced in `Hello`.
    pub caps: Caps,
    /// Caps applied to every lease whatever the manifest says.
    pub limits: LocalLimits,
}

struct Running {
    started: u64,
    deadline_at: u64,
}

/// A worker connected to one coordinator.
pub struct Worker {
    cfg: WorkerConfig,
    coordinator: EndpointId,
    signer: Box<dyn Signer>,
    jobs: BTreeMap<u32, Arc<Manifest>>,
    running: BTreeMap<u64, Running>,
    slots: u16,
    /// Slots announced with `Want` and not yet filled by a lease.
    credit: u16,
    welcomed: bool,
    ended: bool,
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or("")
}

fn payload_size(p: &Payload) -> u64 {
    match p {
        Payload::Inline(b) => b.len() as u64,
        Payload::Blob { size, .. } => *size,
    }
}

impl Worker {
    /// Creates a worker for the swarm of `coordinator`.
    pub fn new(cfg: WorkerConfig, coordinator: EndpointId, signer: Box<dyn Signer>) -> Self {
        let slots = cfg.caps.cpu_slots;
        Self {
            cfg,
            coordinator,
            signer,
            jobs: BTreeMap::new(),
            running: BTreeMap::new(),
            slots,
            credit: 0,
            welcomed: false,
            ended: false,
        }
    }

    /// The `Hello` message carrying `proof`.
    pub fn hello(&self, proof: [u8; 32]) -> Msg {
        Msg::Hello {
            version: PROTO_VERSION,
            proof,
            caps: self.cfg.caps.clone(),
        }
    }

    /// Leases currently running.
    pub fn inflight(&self) -> usize {
        self.running.len()
    }

    fn capacity(&self) -> usize {
        usize::from(self.slots).min(self.cfg.limits.max_leases)
    }

    fn want(&mut self, out: &mut Vec<WorkerAction>) {
        if !self.welcomed || self.ended {
            return;
        }
        let used = self.running.len() + usize::from(self.credit);
        let free = self.capacity().saturating_sub(used);
        let free = u16::try_from(free).unwrap_or(u16::MAX);
        if free > 0 {
            self.credit = self.credit.saturating_add(free);
            out.push(WorkerAction::Send(Msg::Want { slots: free }));
        }
    }

    fn fail(&mut self, lease: u64, kind: FailKind, detail: &str, out: &mut Vec<WorkerAction>) {
        let detail = BoundedString::try_from(truncate(detail, 256)).unwrap_or_default();
        out.push(WorkerAction::Send(Msg::Fail {
            lease,
            kind,
            detail,
        }));
    }

    fn add_job(&mut self, signed: &SignedManifest, out: &mut Vec<WorkerAction>) {
        match signed.open(&self.coordinator) {
            Ok(m) => {
                if !self.jobs.contains_key(&m.job) && self.jobs.len() >= MAX_JOBS {
                    // Job numbers only grow and a coordinator holds at most MAX_JOBS live
                    // jobs (it forgets finished ones), so the oldest manifest goes first.
                    let oldest = self.jobs.keys().next().copied();
                    match oldest {
                        Some(old) if old < m.job => {
                            self.jobs.remove(&old);
                        }
                        _ => {
                            out.push(WorkerAction::Event(WorkerEvent::JobRejected {
                                reason: "too many jobs".to_owned(),
                            }));
                            return;
                        }
                    }
                }
                let job = m.job;
                let name = m.name.as_str().to_owned();
                let usable = self.cfg.caps.supports(m.kind)
                    && m.module_size as usize <= self.cfg.limits.max_module
                    && m.max_memory <= self.cfg.limits.max_memory;
                if usable
                    && let Some(module) = m.module
                    && !self.cfg.caps.cached.contains(&module)
                {
                    out.push(WorkerAction::Fetch {
                        hash: module,
                        size: u64::from(m.module_size),
                    });
                }
                let manifest = Arc::new(m);
                self.jobs.insert(job, manifest.clone());
                out.push(WorkerAction::Event(WorkerEvent::JobAdded {
                    job,
                    name,
                    manifest,
                }));
            }
            Err(e) => out.push(WorkerAction::Event(WorkerEvent::JobRejected {
                reason: e.to_string(),
            })),
        }
    }

    fn check_lease(&self, l: &Lease) -> Result<Arc<Manifest>, (FailKind, &'static str)> {
        let m = self
            .jobs
            .get(&l.job)
            .ok_or((FailKind::Invalid, "unknown job"))?;
        if self.running.contains_key(&l.lease) {
            return Err((FailKind::Invalid, "duplicate lease"));
        }
        if self.running.len() >= self.cfg.limits.max_leases {
            return Err((FailKind::Invalid, "too many leases"));
        }
        if l.kind != m.kind || !self.cfg.caps.supports(l.kind) {
            return Err((FailKind::Unsupported, "task kind not supported"));
        }
        if l.module != m.module {
            return Err((FailKind::Invalid, "module differs from the manifest"));
        }
        if !l.assets.iter().eq(m.assets.iter().map(|a| &a.hash)) {
            return Err((FailKind::Invalid, "assets differ from the manifest"));
        }
        let limits = &self.cfg.limits;
        if m.module_size as usize > limits.max_module {
            return Err((FailKind::Invalid, "module above the local limit"));
        }
        if m.max_memory > limits.max_memory {
            return Err((FailKind::Invalid, "memory above the local limit"));
        }
        if m.assets.iter().any(|a| a.size > limits.max_asset as u64) {
            return Err((FailKind::Invalid, "asset above the local limit"));
        }
        if payload_size(&l.input) > limits.max_input as u64 {
            return Err((FailKind::Invalid, "input above the local limit"));
        }
        Ok(m.clone())
    }

    /// A message from the coordinator.
    pub fn handle(&mut self, now: u64, msg: Msg) -> Vec<WorkerAction> {
        let mut out = Vec::new();
        if self.ended {
            return out;
        }
        match msg {
            Msg::Pending => out.push(WorkerAction::Event(WorkerEvent::Pending)),
            Msg::Welcome {
                swarm,
                jobs,
                webrtc,
                stun,
            } => {
                for j in &jobs {
                    self.add_job(j, &mut out);
                }
                self.welcomed = true;
                out.push(WorkerAction::Event(WorkerEvent::Welcomed {
                    swarm,
                    webrtc,
                    stun: stun.iter().map(|s| s.as_str().to_owned()).collect(),
                }));
                self.want(&mut out);
            }
            Msg::Job(signed) => self.add_job(&signed, &mut out),
            Msg::Lease(l) => {
                self.credit = self.credit.saturating_sub(1);
                match self.check_lease(&l) {
                    Ok(manifest) => {
                        self.running.insert(
                            l.lease,
                            Running {
                                started: now,
                                deadline_at: now.saturating_add(u64::from(l.deadline_ms)),
                            },
                        );
                        out.push(WorkerAction::Run { lease: l, manifest });
                    }
                    Err((kind, detail)) => {
                        self.fail(l.lease, kind, detail, &mut out);
                        out.push(WorkerAction::Event(WorkerEvent::LeaseRefused {
                            lease: l.lease,
                            kind,
                        }));
                        self.want(&mut out);
                    }
                }
            }
            Msg::Cancel { lease } => {
                if self.running.remove(&lease).is_some() {
                    out.push(WorkerAction::Abort { lease });
                    self.want(&mut out);
                }
            }
            Msg::Bye { reason } => {
                self.ended = true;
                for lease in std::mem::take(&mut self.running).into_keys() {
                    out.push(WorkerAction::Abort { lease });
                }
                out.push(WorkerAction::Ended(reason));
            }
            other => {
                if let Some((kind, signal)) = RtcKind::split(other) {
                    out.push(WorkerAction::Event(WorkerEvent::Rtc {
                        from: signal.peer,
                        kind,
                        payload: signal.payload.into_inner(),
                    }));
                }
            }
        }
        out
    }

    /// A `Run` finished.
    pub fn task_done(&mut self, now: u64, lease: u64, out: Outcome) -> Vec<WorkerAction> {
        let mut actions = Vec::new();
        let Some(r) = self.running.remove(&lease) else {
            return actions;
        };
        let duration_ms = u32::try_from(now.saturating_sub(r.started)).unwrap_or(u32::MAX);
        match out {
            Outcome::Ok(bytes) if bytes.len() > self.cfg.limits.max_output => {
                self.fail(
                    lease,
                    FailKind::Invalid,
                    "output above the local limit",
                    &mut actions,
                );
            }
            Outcome::Ok(bytes) => {
                let output_hash = Hash::of(&bytes);
                let sig = sign_result(&*self.signer, lease, &output_hash);
                let output = if bytes.len() <= MAX_INLINE_OUTPUT {
                    BoundedBytes::try_from(bytes).map(Payload::Inline).ok()
                } else {
                    let size = bytes.len() as u64;
                    actions.push(WorkerAction::Publish {
                        hash: output_hash,
                        bytes,
                    });
                    Some(Payload::Blob {
                        hash: output_hash,
                        size,
                    })
                };
                match output {
                    Some(output) => actions.push(WorkerAction::Send(Msg::Result {
                        lease,
                        output,
                        output_hash,
                        duration_ms,
                        sig,
                    })),
                    None => self.fail(lease, FailKind::Io, "output encoding", &mut actions),
                }
            }
            Outcome::Fail(kind, detail) => self.fail(lease, kind, &detail, &mut actions),
        }
        self.want(&mut actions);
        actions
    }

    /// Changes the number of slots offered; 0 pauses (running tasks finish).
    pub fn set_slots(&mut self, _now: u64, slots: u16) -> Vec<WorkerAction> {
        let mut out = Vec::new();
        self.slots = slots;
        if slots == 0 {
            if self.welcomed && !self.ended {
                self.credit = 0;
                out.push(WorkerAction::Send(Msg::Want { slots: 0 }));
            }
        } else {
            self.want(&mut out);
        }
        out
    }

    /// Enforces lease deadlines locally.
    pub fn tick(&mut self, now: u64) -> Vec<WorkerAction> {
        let mut out = Vec::new();
        let expired: Vec<u64> = self
            .running
            .iter()
            .filter(|(_, r)| r.deadline_at <= now)
            .map(|(id, _)| *id)
            .collect();
        if expired.is_empty() {
            return out;
        }
        for lease in expired {
            self.running.remove(&lease);
            out.push(WorkerAction::Abort { lease });
            self.fail(lease, FailKind::Timeout, "deadline exceeded", &mut out);
        }
        self.want(&mut out);
        out
    }

    /// Sends signalling to another peer through the coordinator.
    pub fn send_rtc(
        &self,
        to: EndpointId,
        kind: RtcKind,
        payload: &str,
    ) -> Result<Vec<WorkerAction>, CoreError> {
        let payload: BoundedString<MAX_SIGNAL> =
            BoundedString::try_from(payload).map_err(|_| CoreError::SignalTooLarge)?;
        Ok(vec![WorkerAction::Send(
            kind.wrap(RtcSignal { peer: to, payload }),
        )])
    }
}
