//! I/O-free coordinator and worker state machines for Nuée.
//!
//! Both machines are driven by the caller: every input carries the current time in
//! milliseconds, every output is a list of actions to perform. Randomness comes from a
//! seeded generator, so a run is reproducible from its inputs.

mod coordinator;
mod events;
mod sched;
mod verify;
mod worker;

pub use coordinator::{Coordinator, CoordinatorConfig, JobSpec};
pub use events::{
    Action, Event, JobStatus, PeerStatus, RtcKind, SwarmStatus, WorkerAction, WorkerEvent,
};
pub use nuee_proto::Lease;
pub use worker::{Worker, WorkerConfig};

use nuee_proto::{FailKind, ManifestError};

/// Decides whether two outputs of the same task agree.
pub trait Compare: Send + Sync {
    /// Whether `a` and `b` are equivalent outputs.
    fn same(&self, a: &[u8], b: &[u8]) -> bool;
}

/// Result of running one lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Output bytes.
    Ok(Vec<u8>),
    /// The task did not produce an output.
    Fail(FailKind, String),
}

/// A request to a state machine was refused.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// The swarm already holds the maximum number of jobs.
    #[error("too many jobs (at most {0})")]
    TooManyJobs(usize),
    /// The number of inputs differs from the manifest's task count.
    #[error("the manifest declares {tasks} tasks but {inputs} inputs were given")]
    TaskCount {
        /// Tasks declared by the manifest.
        tasks: u32,
        /// Inputs supplied.
        inputs: usize,
    },
    /// The manifest uses a tolerance comparator but none was supplied.
    #[error("the manifest comparator needs an implementation")]
    MissingComparator,
    /// The manifest is invalid or could not be signed.
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    /// A signalling payload exceeds the protocol cap.
    #[error("signalling payload too large")]
    SignalTooLarge,
    /// The target peer is not connected.
    #[error("peer not connected")]
    NotConnected,
}
