//! Actions and events produced by the state machines, and status snapshots.

use std::sync::Arc;

use nuee_proto::{
    ByeReason, Caps, EndpointId, FailKind, Hash, Lease, Manifest, Msg, RtcSignal, SwarmInfo,
    TaskKind,
};

/// Kind of WebRTC signalling message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RtcKind {
    /// SDP offer.
    Offer,
    /// SDP answer.
    Answer,
    /// ICE candidate.
    Ice,
}

impl RtcKind {
    /// Splits a signalling message into its kind and body; `None` for other messages.
    pub fn split(msg: Msg) -> Option<(RtcKind, RtcSignal)> {
        match msg {
            Msg::RtcOffer(s) => Some((RtcKind::Offer, s)),
            Msg::RtcAnswer(s) => Some((RtcKind::Answer, s)),
            Msg::RtcIce(s) => Some((RtcKind::Ice, s)),
            _ => None,
        }
    }

    /// Builds the message of this kind.
    pub fn wrap(self, signal: RtcSignal) -> Msg {
        match self {
            RtcKind::Offer => Msg::RtcOffer(signal),
            RtcKind::Answer => Msg::RtcAnswer(signal),
            RtcKind::Ice => Msg::RtcIce(signal),
        }
    }
}

/// Something that happened in the swarm, for UIs and logs.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A peer was admitted.
    PeerJoined {
        /// Peer identity.
        peer: EndpointId,
        /// Announced capabilities.
        caps: Caps,
    },
    /// A peer waits for manual approval.
    PeerPending {
        /// Peer identity.
        peer: EndpointId,
        /// Announced capabilities.
        caps: Caps,
    },
    /// A peer disconnected.
    PeerLeft {
        /// Peer identity.
        peer: EndpointId,
    },
    /// A task output was accepted (possibly provisionally).
    TaskAccepted {
        /// Job number.
        job: u32,
        /// Task index.
        task: u32,
        /// Producer of the accepted output.
        by: EndpointId,
        /// Milliseconds from the lease to the result, measured by the coordinator (round
        /// trip included); `None` for outputs restored from an earlier run.
        ms: Option<u64>,
    },
    /// A task output was confirmed by an independent execution.
    Verified {
        /// Job number.
        job: u32,
        /// Task index.
        task: u32,
        /// Producer of the accepted output.
        by: EndpointId,
    },
    /// Two executions of a task disagree.
    Mismatch {
        /// Job number.
        job: u32,
        /// Task index.
        task: u32,
        /// Peer whose result triggered the disagreement.
        peer: EndpointId,
    },
    /// A peer was banned after a confirmed mismatch.
    Banned {
        /// Peer identity.
        peer: EndpointId,
        /// Tasks re-queued because of the ban.
        redo: u32,
    },
    /// Tasks of a job the coordinator cannot check itself want another peer execution, and no
    /// admitted peer able to run the kind is left that has not already run them. Emitted once
    /// per job until the set of peers able to run the kind changes.
    WaitingForChecker {
        /// Job number.
        job: u32,
        /// First waiting task.
        task: u32,
        /// Tasks waiting.
        waiting: u32,
        /// The job's kind.
        kind: TaskKind,
    },
    /// A task failed too many times.
    TaskFailed {
        /// Job number.
        job: u32,
        /// Task index.
        task: u32,
        /// Last failure class.
        kind: FailKind,
    },
    /// Job progress.
    Progress {
        /// Job number.
        job: u32,
        /// Accepted tasks.
        done: u32,
        /// Total tasks.
        total: u32,
        /// Verified tasks.
        verified: u32,
        /// Accepted tasks per second since submission.
        rate_per_s: f64,
    },
    /// Every task is settled and no verification is pending.
    JobDone {
        /// Job number.
        job: u32,
        /// Tasks that failed for good.
        failed: u32,
    },
    /// Signalling addressed to the coordinator itself.
    Rtc {
        /// Sender.
        from: EndpointId,
        /// Message kind.
        kind: RtcKind,
        /// SDP or ICE candidate.
        payload: String,
    },
}

/// What the coordinator host must do.
#[derive(Debug, Clone)]
pub enum Action {
    /// Send a message to a peer.
    Send {
        /// Recipient.
        to: EndpointId,
        /// Message.
        msg: Msg,
    },
    /// Close the connection to a peer (after sending what precedes).
    Close {
        /// Peer.
        peer: EndpointId,
        /// Why.
        reason: ByeReason,
    },
    /// Run a task on the coordinator's own slots, then call `local_done`.
    RunLocal {
        /// Lease to run.
        lease: Lease,
        /// Job manifest.
        manifest: Arc<Manifest>,
    },
    /// Fetch a blob output from a peer, then call `output_fetched`.
    FetchOutput {
        /// Peer serving the blob.
        from: EndpointId,
        /// Lease answered.
        lease: u64,
        /// Blob hash.
        hash: Hash,
        /// Blob size.
        size: u64,
    },
    /// A task output is accepted; a later one for the same task replaces it.
    Accepted {
        /// Job number.
        job: u32,
        /// Task index.
        task: u32,
        /// Output bytes.
        output: Arc<[u8]>,
        /// BLAKE3 of the output.
        output_hash: Hash,
        /// Producer.
        by: EndpointId,
        /// Milliseconds from the lease to the result, measured by the coordinator (round
        /// trip included); `None` for outputs restored from an earlier run.
        ms: Option<u64>,
    },
    /// Something to report.
    Event(Event),
}

/// Something that happened on the worker side.
#[derive(Debug, Clone, PartialEq)]
pub enum WorkerEvent {
    /// The coordinator asks to wait for manual approval.
    Pending,
    /// Admission granted.
    Welcomed {
        /// Swarm description.
        swarm: SwarmInfo,
        /// The coordinator relays WebRTC signalling.
        webrtc: bool,
        /// STUN servers.
        stun: Vec<String>,
    },
    /// A job manifest was accepted.
    JobAdded {
        /// Job number.
        job: u32,
        /// Display name.
        name: String,
        /// The opened manifest, so hosts can show what the job would run.
        manifest: Arc<Manifest>,
    },
    /// A job manifest was refused.
    JobRejected {
        /// Why.
        reason: String,
    },
    /// A lease was refused before running.
    LeaseRefused {
        /// Lease.
        lease: u64,
        /// Failure reported.
        kind: FailKind,
    },
    /// Signalling relayed by the coordinator.
    Rtc {
        /// Original sender.
        from: EndpointId,
        /// Message kind.
        kind: RtcKind,
        /// SDP or ICE candidate.
        payload: String,
    },
}

/// What the worker host must do.
#[derive(Debug, Clone)]
pub enum WorkerAction {
    /// Send a message to the coordinator.
    Send(Msg),
    /// Run a lease, then call `task_done`.
    Run {
        /// Lease to run.
        lease: Lease,
        /// Job manifest.
        manifest: Arc<Manifest>,
    },
    /// Serve these bytes as a blob; precedes the `Result` that references it.
    Publish {
        /// Blob hash.
        hash: Hash,
        /// Bytes.
        bytes: Vec<u8>,
    },
    /// Prefetch a module or asset from the coordinator.
    Fetch {
        /// Blob hash.
        hash: Hash,
        /// Blob size.
        size: u64,
    },
    /// Stop running a lease; no `task_done` is expected.
    Abort {
        /// Lease.
        lease: u64,
    },
    /// Something to report.
    Event(WorkerEvent),
    /// The coordinator ended the session.
    Ended(ByeReason),
}

/// One peer in a status snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerStatus {
    /// Identity.
    pub id: EndpointId,
    /// Runs in a browser.
    pub browser: bool,
    /// Announced CPU slots.
    pub slots: u16,
    /// Leases held now.
    pub inflight: u16,
    /// Accepted outputs produced.
    pub done: u32,
    /// Outputs confirmed by another execution.
    pub verified: u32,
    /// Banned.
    pub banned: bool,
}

/// One job in a status snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct JobStatus {
    /// Job number.
    pub job: u32,
    /// Display name.
    pub name: String,
    /// Total tasks.
    pub total: u32,
    /// Accepted tasks.
    pub done: u32,
    /// Verified tasks.
    pub verified: u32,
    /// Tasks that failed for good.
    pub failed: u32,
    /// Executions in progress.
    pub in_flight: u32,
    /// Accepted tasks per second since submission.
    pub rate_per_s: f64,
}

/// Snapshot of the swarm for UIs.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SwarmStatus {
    /// Connected and banned peers.
    pub peers: Vec<PeerStatus>,
    /// Jobs.
    pub jobs: Vec<JobStatus>,
}
