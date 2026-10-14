//! Global protocol caps. Workers enforce them locally whatever a manifest says.

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;

/// Largest encoded message on any stream.
pub const MAX_MSG: usize = 8 * MIB;
/// Largest task input carried inline in a lease.
pub const MAX_INLINE_INPUT: usize = MIB;
/// Largest task output carried inline in a result.
pub const MAX_INLINE_OUTPUT: usize = MIB;
/// Largest task module.
pub const MAX_MODULE: usize = 32 * MIB;
/// Largest job asset.
pub const MAX_ASSET: usize = 64 * MIB;
/// Largest declared linear memory of a task module.
pub const MAX_WASM_MEMORY: u64 = 1024 * 1024 * 1024;
/// Most peers in one swarm.
pub const MAX_PEERS: usize = 256;
/// Most concurrent leases held by one peer.
pub const MAX_LEASES_PER_PEER: usize = 32;
/// Time a new connection has to send a valid `Hello`, in milliseconds.
pub const HELLO_TIMEOUT_MS: u64 = 5_000;
/// Most connections waiting for admission at once.
pub const MAX_PENDING_UNAUTH: usize = 64;
/// Most jobs in one swarm.
pub const MAX_JOBS: usize = 64;
/// Most assets attached to one job.
pub const MAX_ASSETS_PER_JOB: usize = 16;
/// Largest WebRTC signalling payload (SDP or ICE candidate).
pub const MAX_SIGNAL: usize = 16 * KIB;
/// Largest HTTP body proxied through the console.
pub const MAX_HTTP_BODY: usize = 8 * MIB;
/// Most HTTP headers in one console request or response.
pub const MAX_HTTP_HEADERS: usize = 64;
/// Most providers listed in a file link.
pub const MAX_LINK_PROVIDERS: usize = 4;
/// Largest encoded manifest.
pub const MAX_MANIFEST: usize = 64 * KIB;
/// Default task deadline, in milliseconds.
pub const DEFAULT_DEADLINE_MS: u32 = 60_000;
/// Lower clamp of a per-peer deadline, in milliseconds.
pub const MIN_PEER_DEADLINE_MS: u32 = 5_000;
/// Upper clamp of a per-peer deadline, in milliseconds.
pub const MAX_PEER_DEADLINE_MS: u32 = 10 * 60_000;
/// Upper bound of any deadline (exec jobs), in milliseconds.
pub const MAX_DEADLINE_MS: u32 = 24 * 3600 * 1000;

/// Worker-side caps, applied to every lease regardless of the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalLimits {
    /// Largest module this worker accepts.
    pub max_module: usize,
    /// Largest asset this worker downloads.
    pub max_asset: usize,
    /// Largest linear memory this worker grants.
    pub max_memory: u64,
    /// Largest task input this worker accepts.
    pub max_input: usize,
    /// Largest task output this worker produces.
    pub max_output: usize,
    /// Most leases this worker holds at once.
    pub max_leases: usize,
}

impl Default for LocalLimits {
    fn default() -> Self {
        Self {
            max_module: MAX_MODULE,
            max_asset: MAX_ASSET,
            max_memory: MAX_WASM_MEMORY,
            max_input: MAX_MSG,
            max_output: MAX_MSG,
            max_leases: MAX_LEASES_PER_PEER,
        }
    }
}
