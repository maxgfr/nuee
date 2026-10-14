//! Messages of every ALPN and their bounded postcard encoding.

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::bounded::{BoundedBytes, BoundedString, BoundedVec};
use crate::id::{EndpointId, Hash, Signature};
use crate::limits::{
    MAX_ASSETS_PER_JOB, MAX_HTTP_BODY, MAX_HTTP_HEADERS, MAX_INLINE_INPUT, MAX_LINK_PROVIDERS,
    MAX_MSG, MAX_SIGNAL,
};
use crate::manifest::SignedManifest;

/// Encoding or decoding failure.
#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    /// The encoded form is longer than allowed.
    #[error("message of {len} bytes exceeds the cap of {max}")]
    TooLarge {
        /// Actual length.
        len: usize,
        /// Allowed maximum.
        max: usize,
    },
    /// The bytes are not a valid message.
    #[error("malformed message: {0}")]
    Malformed(#[from] postcard::Error),
    /// Bytes remain after the message.
    #[error("{0} trailing bytes after the message")]
    Trailing(usize),
}

/// Encodes `value` with postcard, refusing results above [`MAX_MSG`].
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, ProtoError> {
    encode_with_limit(value, MAX_MSG)
}

/// Encodes `value` with postcard, refusing results above `max` bytes.
pub fn encode_with_limit<T: Serialize>(value: &T, max: usize) -> Result<Vec<u8>, ProtoError> {
    let out = postcard::to_allocvec(value)?;
    if out.len() > max {
        return Err(ProtoError::TooLarge {
            len: out.len(),
            max,
        });
    }
    Ok(out)
}

/// Decodes a whole message of at most [`MAX_MSG`] bytes.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ProtoError> {
    decode_with_limit(bytes, MAX_MSG)
}

/// Decodes a whole message of at most `max` bytes; trailing bytes are an error.
pub fn decode_with_limit<T: DeserializeOwned>(bytes: &[u8], max: usize) -> Result<T, ProtoError> {
    if bytes.len() > max {
        return Err(ProtoError::TooLarge {
            len: bytes.len(),
            max,
        });
    }
    let (value, rest) = postcard::take_from_bytes::<T>(bytes)?;
    if !rest.is_empty() {
        return Err(ProtoError::Trailing(rest.len()));
    }
    Ok(value)
}

/// Largest encoded console request or response.
pub const MAX_HTTP_MSG: usize = MAX_HTTP_BODY + 512 * 1024;

/// Kind of work a task performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskKind {
    /// Deterministic WASM module.
    Wasm,
    /// WGSL compute shader.
    Wgsl,
    /// Text embeddings.
    Embed,
    /// Multiple-choice LLM scoring.
    Llm,
    /// Native command from a pinned repository.
    Exec,
    /// Training round run by a WASM trainer.
    TrainWasm,
    /// Training round run by a native trainer.
    TrainExec,
}

impl TaskKind {
    /// Every kind, in bit order.
    pub const ALL: [TaskKind; 7] = [
        TaskKind::Wasm,
        TaskKind::Wgsl,
        TaskKind::Embed,
        TaskKind::Llm,
        TaskKind::Exec,
        TaskKind::TrainWasm,
        TaskKind::TrainExec,
    ];
    /// Bit of this kind in [`Caps::kinds`].
    pub fn bit(self) -> u32 {
        1 << (self as u32)
    }
}

/// Why a task did not produce an output. Never a verification mismatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FailKind {
    /// The module trapped.
    Trap,
    /// Memory exhausted.
    Oom,
    /// Deadline exceeded.
    Timeout,
    /// Call stack exhausted.
    StackOverflow,
    /// Module or input rejected.
    Invalid,
    /// The worker cannot run this kind of task.
    Unsupported,
    /// The lease was cancelled.
    Cancelled,
    /// Local I/O or transfer failure.
    Io,
}

/// Why a swarm connection ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ByeReason {
    /// All jobs finished.
    Done,
    /// The peer was banned after a confirmed mismatch.
    Banned,
    /// The sender is shutting down.
    Shutdown,
    /// Admission was refused.
    Rejected,
    /// The swarm has no room left.
    Full,
}

/// Task input or output, inline or as a blob served by the sender.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Payload {
    /// Bytes carried in the message.
    Inline(BoundedBytes<MAX_INLINE_INPUT>),
    /// Blob to fetch from the sender's blob store.
    Blob {
        /// Content hash.
        hash: Hash,
        /// Size in bytes.
        size: u64,
    },
}

/// GPU capabilities of a worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuCaps {
    /// Adapter name.
    pub adapter: BoundedString<64>,
    /// Largest buffer.
    pub max_buffer: u64,
    /// Largest storage buffer binding.
    pub max_storage_binding: u64,
    /// Shader f16 support.
    pub f16: bool,
}

/// Capabilities announced by a worker in `Hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caps {
    /// Software agent string.
    pub agent: BoundedString<64>,
    /// Runs in a browser.
    pub browser: bool,
    /// Concurrent CPU task slots.
    pub cpu_slots: u16,
    /// GPU, when available.
    pub gpu: Option<GpuCaps>,
    /// Memory offered, in MiB.
    pub mem_mb: u32,
    /// Bitset of supported [`TaskKind`]s.
    pub kinds: u32,
    /// Modules and assets already cached.
    pub cached: BoundedVec<Hash, 64>,
    /// Repositories allowed for exec tasks.
    pub exec_allow: BoundedVec<BoundedString<160>, 16>,
    /// Storage offered for pins, in MiB.
    pub store_quota_mb: u64,
    /// Home relay URL.
    pub relay: Option<BoundedString<256>>,
    /// Can open WebRTC data channels.
    pub rtc: bool,
}

impl Caps {
    /// Whether the worker supports `kind`.
    pub fn supports(&self, kind: TaskKind) -> bool {
        self.kinds & kind.bit() != 0
    }
}

/// What a swarm serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SwarmMode {
    /// Distributed computation.
    Compute,
    /// File sharing.
    Files,
    /// Exposed service.
    Service,
    /// Static site.
    Site,
}

/// File announced by a files or site swarm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileInfo {
    /// Content hash.
    pub hash: Hash,
    /// Size in bytes.
    pub size: u64,
}

/// Swarm description sent in `Welcome`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmInfo {
    /// Display name.
    pub name: BoundedString<64>,
    /// Coordinator identity.
    pub coordinator: EndpointId,
    /// What the swarm serves.
    pub mode: SwarmMode,
    /// Shared file, for files and site swarms.
    pub file: Option<FileInfo>,
}

/// A peer that serves some content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provider {
    /// Peer identity.
    pub id: EndpointId,
    /// Relay URL of the peer.
    pub relay: Option<BoundedString<256>>,
}

/// WebRTC signalling relayed by the coordinator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RtcSignal {
    /// Other end (target when sent, source when received).
    pub peer: EndpointId,
    /// SDP or ICE candidate.
    pub payload: BoundedString<MAX_SIGNAL>,
}

/// A task assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    /// Lease identifier, unique per coordinator.
    pub lease: u64,
    /// Job number.
    pub job: u32,
    /// Task index within the job.
    pub task: u32,
    /// Kind of work.
    pub kind: TaskKind,
    /// Module to run.
    pub module: Option<Hash>,
    /// Assets, in manifest order.
    pub assets: BoundedVec<Hash, MAX_ASSETS_PER_JOB>,
    /// Task input.
    pub input: Payload,
    /// Time allowed, in milliseconds.
    pub deadline_ms: u32,
}

/// Messages on `nuee/job/0`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Msg {
    /// Worker asks to join.
    Hello {
        /// Protocol version.
        version: u16,
        /// Admission proof.
        proof: [u8; 32],
        /// Worker capabilities.
        caps: Caps,
    },
    /// Waiting for manual approval.
    Pending,
    /// Admission granted.
    Welcome {
        /// Swarm description.
        swarm: SwarmInfo,
        /// Jobs currently open.
        jobs: BoundedVec<SignedManifest, 16>,
        /// Coordinator relays WebRTC signalling.
        webrtc: bool,
        /// STUN servers.
        stun: BoundedVec<BoundedString<256>, 4>,
    },
    /// New job.
    Job(SignedManifest),
    /// Worker has free slots.
    Want {
        /// Number of free slots.
        slots: u16,
    },
    /// Task assignment.
    Lease(Lease),
    /// Task output.
    Result {
        /// Lease answered.
        lease: u64,
        /// Output bytes or blob.
        output: Payload,
        /// BLAKE3 of the output bytes.
        output_hash: Hash,
        /// Execution time.
        duration_ms: u32,
        /// Worker signature over `lease || output_hash`.
        sig: Signature,
    },
    /// Task failure.
    Fail {
        /// Lease answered.
        lease: u64,
        /// Failure class.
        kind: FailKind,
        /// Short human-readable detail.
        detail: BoundedString<256>,
    },
    /// WebRTC offer.
    RtcOffer(RtcSignal),
    /// WebRTC answer.
    RtcAnswer(RtcSignal),
    /// WebRTC ICE candidate.
    RtcIce(RtcSignal),
    /// Lease withdrawn.
    Cancel {
        /// Lease withdrawn.
        lease: u64,
    },
    /// Connection ends.
    Bye {
        /// Why.
        reason: ByeReason,
    },
}

/// Why a pin request is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PinRefusal {
    /// Storage quota reached.
    Quota,
    /// Content larger than allowed.
    TooLarge,
    /// Requester is not a swarm member.
    NotMember,
    /// Too many transfers in progress.
    Busy,
}

/// One pinned item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinInfo {
    /// Content hash.
    pub hash: Hash,
    /// Size in bytes.
    pub size: u64,
    /// Whether every byte is stored.
    pub complete: bool,
}

/// Messages on `nuee/pin/0`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PinMsg {
    /// Client opens a session.
    Hello {
        /// Protocol version.
        version: u16,
        /// Admission proof.
        proof: [u8; 32],
    },
    /// Store a copy of some content.
    Replicate {
        /// Content hash.
        hash: Hash,
        /// Size in bytes.
        size: u64,
        /// Where to fetch it.
        providers: BoundedVec<Provider, MAX_LINK_PROVIDERS>,
        /// Access token presented to providers.
        access: [u8; 16],
        /// Time to keep it, in seconds.
        ttl_s: u32,
    },
    /// Drop a pinned copy.
    Release {
        /// Content hash.
        hash: Hash,
    },
    /// Ask for the pinned items.
    List,
    /// Pinned items.
    Pins {
        /// Items.
        items: BoundedVec<PinInfo, 256>,
    },
    /// Replication started.
    Accepted {
        /// Content hash.
        hash: Hash,
    },
    /// Replication refused.
    Refused {
        /// Content hash.
        hash: Hash,
        /// Why.
        reason: PinRefusal,
    },
    /// Replication complete.
    Done {
        /// Content hash.
        hash: Hash,
    },
    /// Keep some content as an erasure set (sharer to coordinator). The header lists the
    /// shard hashes; header and shards are fetched from `providers`. Answered with
    /// `Accepted`/`Refused` carrying the hash of the header bytes.
    ReplicateSet {
        /// Encoded erasure header (`NRS1`).
        header: BoundedBytes<MAX_ERASURE_HEADER>,
        /// Where to fetch the header and the shards.
        providers: BoundedVec<Provider, MAX_LINK_PROVIDERS>,
        /// Access token presented to providers.
        access: [u8; 16],
    },
    /// Who holds `hash` (a blob or an erasure set)? Answered with `Holders`.
    Where {
        /// Blob hash or erasure set identity.
        hash: Hash,
    },
    /// Current holders of `hash`, empty if unknown. Also pushed unasked to a sharer once
    /// the placement it asked for is reached.
    Holders {
        /// Blob hash or erasure set identity.
        hash: Hash,
        /// Holders (for a set, header holders first).
        providers: BoundedVec<Provider, MAX_LINK_PROVIDERS>,
    },
}

/// Largest encoded erasure header: 21 fixed bytes plus 65 hashes (64 shards + original).
pub const MAX_ERASURE_HEADER: usize = 21 + 32 * 65;

/// Messages on `nuee/tcp/0`; after `Ready` the stream carries raw bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TcpMsg {
    /// Client opens a tunnel.
    Hello {
        /// Protocol version.
        version: u16,
        /// Admission proof.
        proof: [u8; 32],
    },
    /// Tunnel open.
    Ready,
    /// Tunnel refused.
    Refused {
        /// Why.
        reason: BoundedString<128>,
    },
}

/// One HTTP header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    /// Header name.
    pub name: BoundedString<128>,
    /// Header value.
    pub value: BoundedString<4096>,
}

/// Console request on `nuee/http/0`, one bi-stream per request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpRequest {
    /// Admission proof.
    pub proof: [u8; 32],
    /// HTTP method.
    pub method: BoundedString<16>,
    /// Path and query.
    pub path: BoundedString<2048>,
    /// Headers.
    pub headers: BoundedVec<Header, MAX_HTTP_HEADERS>,
    /// Body.
    pub body: BoundedBytes<MAX_HTTP_BODY>,
}

/// Console response on `nuee/http/0`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpResponse {
    /// Status code.
    pub status: u16,
    /// Headers.
    pub headers: BoundedVec<Header, MAX_HTTP_HEADERS>,
    /// Body.
    pub body: BoundedBytes<MAX_HTTP_BODY>,
}
