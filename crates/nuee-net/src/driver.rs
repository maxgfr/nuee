//! Glue between the `nuee-core` state machines and iroh connections.
//!
//! One task owns each state machine and receives every input (network frames, timer ticks,
//! finished executions, fetched blobs, host commands) through a single channel, so the core
//! never needs locking. Natively the tasks run on tokio; on wasm32 they run on the browser
//! event loop through `n0_future`, and nothing has to be `Send` there.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use iroh::endpoint::{Connection, RecvStream, SendStream, VarInt};
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{EndpointAddr, RelayUrl};
use n0_future::task::{AbortHandle, spawn};
use n0_future::time::Instant;
use nuee_core::{
    Action, Coordinator, CoordinatorConfig, CoreError, Event, JobSpec, Outcome, RtcKind,
    SwarmStatus, Worker, WorkerAction, WorkerConfig, WorkerEvent,
};
use nuee_proto::limits::{MAX_JOBS, MAX_MSG};
use nuee_proto::{
    ALPN_JOB, ByeReason, Caps, EndpointId, FailKind, Hash, JobLink, Lease, Manifest, Msg,
    PROOF_HELLO, PROTO_VERSION, Payload, SecretKey,
};
use tokio::sync::{mpsc, oneshot};

use crate::admit::{Admission, AdmitConfig, AdmitError};
use crate::endpoint::{Net, NetError};
use crate::framing::{FrameError, recv_msg, send_msg};

/// Largest `Hello` frame accepted before admission.
pub const MAX_HELLO: usize = 64 * 1024;
/// Period of the core timer.
pub const TICK: Duration = Duration::from_millis(250);

/// QUIC application close codes used on `nuee/job/0`.
pub mod close_code {
    /// Normal end of the swarm or of the session.
    pub const DONE: u32 = 0;
    /// Admission refused (wrong proof, rejected by the operator, bad version).
    pub const REFUSED: u32 = 1;
    /// Too many handshakes in progress or the remote is throttled.
    pub const BUSY: u32 = 2;
    /// No hello before the deadline.
    pub const TIMEOUT: u32 = 3;
    /// Malformed or oversized frame.
    pub const PROTOCOL: u32 = 4;
    /// Banned after a confirmed mismatch.
    pub const BANNED: u32 = 5;
    /// The swarm is full.
    pub const FULL: u32 = 6;
    /// The sender shuts down.
    pub const SHUTDOWN: u32 = 7;
}

fn reason_code(reason: ByeReason) -> u32 {
    match reason {
        ByeReason::Done => close_code::DONE,
        ByeReason::Banned => close_code::BANNED,
        ByeReason::Shutdown => close_code::SHUTDOWN,
        ByeReason::Rejected => close_code::REFUSED,
        ByeReason::Full => close_code::FULL,
    }
}

/// The application close code of a closed connection, if the peer closed it on purpose.
pub fn close_code_of(conn: &Connection) -> Option<u32> {
    match conn.close_reason()? {
        iroh::endpoint::ConnectionError::ApplicationClosed(ac) => {
            u32::try_from(u64::from(ac.error_code)).ok()
        }
        _ => None,
    }
}

fn close(conn: &Connection, code: u32, reason: &[u8]) {
    conn.close(VarInt::from_u32(code), reason);
}

// ---------------------------------------------------------------------------------------------
// Send bounds that only apply natively.

/// `Send` natively, nothing on wasm32 (browser futures are single-threaded).
#[cfg(not(target_family = "wasm"))]
pub trait MaybeSend: Send {}
#[cfg(not(target_family = "wasm"))]
impl<T: Send + ?Sized> MaybeSend for T {}
/// `Send` natively, nothing on wasm32 (browser futures are single-threaded).
#[cfg(target_family = "wasm")]
pub trait MaybeSend {}
#[cfg(target_family = "wasm")]
impl<T: ?Sized> MaybeSend for T {}

/// `Sync` natively, nothing on wasm32.
#[cfg(not(target_family = "wasm"))]
pub trait MaybeSync: Sync {}
#[cfg(not(target_family = "wasm"))]
impl<T: Sync + ?Sized> MaybeSync for T {}
/// `Sync` natively, nothing on wasm32.
#[cfg(target_family = "wasm")]
pub trait MaybeSync {}
#[cfg(target_family = "wasm")]
impl<T: ?Sized> MaybeSync for T {}

/// Boxed future returned by the integration traits (`Send` natively).
#[cfg(not(target_family = "wasm"))]
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
/// Boxed future returned by the integration traits (`Send` natively).
#[cfg(target_family = "wasm")]
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

// ---------------------------------------------------------------------------------------------
// Integration seams.

/// Runs one lease. Implemented by `nuee-exec` natively and by the browser task worker.
pub trait TaskExec: MaybeSend + MaybeSync + 'static {
    /// Runs `lease` of `manifest` on `input` (already wrapped in the asset envelope when the
    /// job has assets). Never panics on bad input: failures are [`Outcome::Fail`].
    fn run<'a>(
        &'a self,
        lease: &'a Lease,
        manifest: &'a Manifest,
        module: Option<Bytes>,
        input: Bytes,
    ) -> BoxFut<'a, Outcome>;
}

/// A blob transfer failed.
#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    /// No provider has the blob.
    #[error("blob {0} not found")]
    NotFound(Hash),
    /// The bytes do not match the announced hash or size.
    #[error("blob {0} failed its integrity check")]
    Integrity(Hash),
    /// The blob is larger than allowed.
    #[error("blob of {size} bytes exceeds the cap of {max}")]
    TooLarge {
        /// Announced size.
        size: u64,
        /// Allowed maximum.
        max: u64,
    },
    /// Transport or storage failure.
    #[error("blob transfer failed: {0}")]
    Io(String),
}

/// Content-addressed blob exchange between admitted peers. `nuee-store` provides the
/// iroh-blobs implementation; [`MemBlobHub`] serves tests.
pub trait BlobHub: Send + Sync + 'static {
    /// Fetches `hash` (exactly `size` bytes) from `from`.
    fn fetch(
        &self,
        from: EndpointAddr,
        hash: Hash,
        size: u64,
    ) -> BoxFut<'_, Result<Bytes, BlobError>>;
    /// Stores `bytes` and serves them to admitted peers.
    fn publish(&self, bytes: Bytes) -> BoxFut<'_, Result<Hash, BlobError>>;
    /// Reads `hash` (exactly `size` bytes) from our own store, without any transfer.
    fn local(&self, hash: Hash, size: u64) -> BoxFut<'_, Result<Bytes, BlobError>>;
    /// Allows `peer` to fetch from us.
    fn admit(&self, peer: EndpointId);
    /// Withdraws that permission.
    fn revoke(&self, peer: EndpointId);
}

/// In-memory blob hub: every instance created from the same [`MemBlobHub::shared`] map sees
/// every published blob. Fetches check hash and size.
#[derive(Debug, Clone, Default)]
pub struct MemBlobHub {
    blobs: Arc<Mutex<HashMap<Hash, Bytes>>>,
    admitted: Arc<Mutex<std::collections::HashSet<EndpointId>>>,
}

impl MemBlobHub {
    /// An empty hub with its own map.
    pub fn new() -> Self {
        Self::default()
    }

    /// A hub over an existing shared map.
    pub fn shared(blobs: Arc<Mutex<HashMap<Hash, Bytes>>>) -> Self {
        Self {
            blobs,
            admitted: Arc::default(),
        }
    }

    /// The shared map, to build more hubs on.
    pub fn map(&self) -> Arc<Mutex<HashMap<Hash, Bytes>>> {
        self.blobs.clone()
    }

    /// Whether `peer` is currently admitted.
    pub fn is_admitted(&self, peer: &EndpointId) -> bool {
        lock(&self.admitted).contains(peer)
    }

    /// Stores `bytes` and returns their hash.
    pub fn insert(&self, bytes: Bytes) -> Hash {
        let hash = Hash::of(&bytes);
        lock(&self.blobs).insert(hash, bytes);
        hash
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // Plain data behind the lock; a panic elsewhere cannot leave it half-updated.
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl BlobHub for MemBlobHub {
    fn fetch(
        &self,
        _from: EndpointAddr,
        hash: Hash,
        size: u64,
    ) -> BoxFut<'_, Result<Bytes, BlobError>> {
        let found = lock(&self.blobs).get(&hash).cloned();
        Box::pin(async move {
            let bytes = found.ok_or(BlobError::NotFound(hash))?;
            if bytes.len() as u64 != size || Hash::of(&bytes) != hash {
                return Err(BlobError::Integrity(hash));
            }
            Ok(bytes)
        })
    }

    fn publish(&self, bytes: Bytes) -> BoxFut<'_, Result<Hash, BlobError>> {
        let hash = self.insert(bytes);
        Box::pin(async move { Ok(hash) })
    }

    fn local(&self, hash: Hash, size: u64) -> BoxFut<'_, Result<Bytes, BlobError>> {
        let found = lock(&self.blobs).get(&hash).cloned();
        Box::pin(async move {
            let bytes = found.ok_or(BlobError::NotFound(hash))?;
            if bytes.len() as u64 != size {
                return Err(BlobError::Integrity(hash));
            }
            Ok(bytes)
        })
    }

    fn admit(&self, peer: EndpointId) {
        lock(&self.admitted).insert(peer);
    }

    fn revoke(&self, peer: EndpointId) {
        lock(&self.admitted).remove(&peer);
    }
}

/// Starting a driver task failed.
#[derive(Debug, thiserror::Error)]
#[error("cannot start the driver task: {0}")]
pub struct SpawnError(#[from] std::io::Error);

/// Runs the future built by `make` as the owner of a state machine. The state machines hold
/// a `Box<dyn Signer>` and are not `Send`, so natively they get a thread of their own that
/// drives them on the caller's tokio runtime; their side tasks still run on the runtime pool.
#[cfg(not(target_family = "wasm"))]
fn spawn_owner<F, Fut>(name: &str, make: F) -> Result<(), SpawnError>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let handle = tokio::runtime::Handle::current();
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let _guard = handle.enter();
            handle.block_on(make());
        })?;
    Ok(())
}

#[cfg(target_family = "wasm")]
fn spawn_owner<F, Fut>(_name: &str, make: F) -> Result<(), SpawnError>
where
    F: FnOnce() -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    spawn(make());
    Ok(())
}

fn now_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn relay_url(s: &str) -> Option<RelayUrl> {
    if s.is_empty() {
        return None;
    }
    match s.parse() {
        Ok(url) => Some(url),
        Err(e) => {
            tracing::debug!("ignoring malformed relay URL {s:?}: {e}");
            None
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Coordinator side.

/// Driver settings.
#[derive(Debug, Clone, Copy)]
pub struct DriverConfig {
    /// Admission gate settings (hello deadline, pending cap, throttling).
    pub admit: AdmitConfig,
    /// Tasks the coordinator runs itself at once (0 = none). Raised to the core's
    /// `verify_slots`, so a coordinator that takes no work still checks results.
    pub local_slots: u16,
    /// Period of the core timer.
    pub tick: Duration,
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self {
            admit: AdmitConfig::default(),
            local_slots: 1,
            tick: TICK,
        }
    }
}

/// What the coordinator reports to its host.
#[derive(Debug, Clone)]
pub enum HostEvent {
    /// A task output was accepted.
    Accepted {
        /// Job number.
        job: u32,
        /// Task index.
        task: u32,
        /// Output bytes.
        output: Arc<[u8]>,
        /// BLAKE3 of the output.
        output_hash: Hash,
        /// Peer whose result was accepted.
        by: EndpointId,
        /// Milliseconds from the lease to the result, measured by the coordinator.
        ms: Option<u64>,
    },
    /// Core event (peers, progress, verification).
    Event(Event),
    /// A peer connection ended.
    Closed {
        /// The peer.
        peer: EndpointId,
        /// Why, when the coordinator closed it.
        reason: Option<ByeReason>,
    },
}

enum Out {
    Msg(Box<Msg>),
    Close(ByeReason),
}

enum CoordIn {
    Admitted {
        peer: EndpointId,
        conn_id: u64,
        caps: Caps,
        tx: mpsc::UnboundedSender<Out>,
    },
    Inbound {
        peer: EndpointId,
        conn_id: u64,
        msg: Msg,
    },
    Gone {
        peer: EndpointId,
        conn_id: u64,
    },
    LocalDone {
        lease: u64,
        out: Outcome,
    },
    Fetched {
        lease: u64,
        from: EndpointId,
        res: Result<Bytes, BlobError>,
    },
    Submit {
        spec: Box<JobSpec>,
        reply: oneshot::Sender<Result<u32, CoreError>>,
    },
    MarkDone {
        job: u32,
        task: u32,
        hash: Hash,
    },
    Forget {
        job: u32,
        reply: oneshot::Sender<bool>,
    },
    Approve {
        peer: EndpointId,
        accept: bool,
    },
    Status {
        reply: oneshot::Sender<SwarmStatus>,
    },
    SetWebrtc {
        on: bool,
    },
    Lane {
        peer: EndpointId,
        msg: Msg,
    },
    SendRtc {
        to: EndpointId,
        kind: RtcKind,
        payload: String,
        reply: oneshot::Sender<Result<(), CoreError>>,
    },
    Shutdown {
        reason: ByeReason,
        reply: oneshot::Sender<()>,
    },
}

/// The coordinator's `nuee/job/0` protocol handler; register it on a router with
/// `net.router().accept(ALPN_JOB, driver.clone()).spawn()`.
#[derive(Clone)]
pub struct CoordinatorDriver {
    inner: Arc<HandlerInner>,
}

struct HandlerInner {
    own: EndpointId,
    secret: [u8; 16],
    gate: Admission,
    net: Net,
    tx: mpsc::UnboundedSender<CoordIn>,
    next_conn: std::sync::atomic::AtomicU64,
}

impl fmt::Debug for CoordinatorDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CoordinatorDriver")
            .field("own", &self.inner.own)
            .finish_non_exhaustive()
    }
}

/// Host-side control of a running coordinator.
#[derive(Debug)]
pub struct CoordinatorHandle {
    tx: mpsc::UnboundedSender<CoordIn>,
    /// Accepted outputs, core events and closed connections, in order.
    pub events: mpsc::UnboundedReceiver<HostEvent>,
}

/// The coordinator task has stopped.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("the coordinator has stopped")]
pub struct Stopped;

/// A lane frame was not handed to the coordinator.
#[derive(Debug, thiserror::Error)]
pub enum LaneInError {
    /// Malformed frame.
    #[error(transparent)]
    Frame(#[from] crate::rtc::LaneError),
    /// The coordinator task has stopped.
    #[error(transparent)]
    Stopped(#[from] Stopped),
}

/// A job could not be submitted.
#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    /// Rejected by the core.
    #[error(transparent)]
    Core(#[from] CoreError),
    /// The coordinator task has stopped.
    #[error(transparent)]
    Stopped(#[from] Stopped),
}

impl CoordinatorHandle {
    /// Submits a job; its module and assets must already be published in the blob hub.
    pub async fn submit(&self, spec: JobSpec) -> Result<u32, SubmitError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CoordIn::Submit {
                spec: Box::new(spec),
                reply,
            })
            .map_err(|_| Stopped)?;
        Ok(rx.await.map_err(|_| Stopped)??)
    }

    /// Records a task already done in an earlier run (resume).
    pub fn mark_done(&self, job: u32, task: u32, output_hash: Hash) -> Result<(), Stopped> {
        self.tx
            .send(CoordIn::MarkDone {
                job,
                task,
                hash: output_hash,
            })
            .map_err(|_| Stopped)
    }

    /// Drops a finished job so its slot among the `MAX_JOBS` is free again; false when the
    /// job is unknown or not finished.
    pub async fn forget(&self, job: u32) -> Result<bool, Stopped> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CoordIn::Forget { job, reply })
            .map_err(|_| Stopped)?;
        rx.await.map_err(|_| Stopped)
    }

    /// Answers a pending manual approval.
    pub fn approve(&self, peer: EndpointId, accept: bool) -> Result<(), Stopped> {
        self.tx
            .send(CoordIn::Approve { peer, accept })
            .map_err(|_| Stopped)
    }

    /// Snapshot of the swarm.
    pub async fn status(&self) -> Result<SwarmStatus, Stopped> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CoordIn::Status { reply })
            .map_err(|_| Stopped)?;
        rx.await.map_err(|_| Stopped)
    }

    /// Turns the relaying of WebRTC signalling between peers on or off.
    pub fn set_webrtc(&self, on: bool) -> Result<(), Stopped> {
        self.tx.send(CoordIn::SetWebrtc { on }).map_err(|_| Stopped)
    }

    /// Hands over a result frame that `peer` sent on its WebRTC lane (see [`crate::rtc`]).
    /// Returns the ack frame to send back on that lane; the result is processed as if it had
    /// arrived on the peer's iroh stream, and only while that peer is connected.
    pub fn lane_frame(&self, peer: EndpointId, frame: &[u8]) -> Result<[u8; 9], LaneInError> {
        let (lease, msg) = crate::rtc::decode_result(frame)?;
        self.tx
            .send(CoordIn::Lane { peer, msg })
            .map_err(|_| Stopped)?;
        Ok(crate::rtc::ack_frame(lease))
    }

    /// Sends a WebRTC signalling message from the coordinator to a connected peer.
    pub async fn send_rtc(
        &self,
        to: EndpointId,
        kind: RtcKind,
        payload: String,
    ) -> Result<(), SubmitError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CoordIn::SendRtc {
                to,
                kind,
                payload,
                reply,
            })
            .map_err(|_| Stopped)?;
        Ok(rx.await.map_err(|_| Stopped)??)
    }

    /// Says goodbye to every peer and stops the coordinator task.
    pub async fn shutdown(&self) -> Result<(), Stopped> {
        self.shutdown_with(ByeReason::Shutdown).await
    }

    /// Like [`CoordinatorHandle::shutdown`], telling peers why (`Done` once the work is over).
    pub async fn shutdown_with(&self, reason: ByeReason) -> Result<(), Stopped> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CoordIn::Shutdown { reason, reply })
            .map_err(|_| Stopped)?;
        rx.await.map_err(|_| Stopped)
    }
}

impl CoordinatorDriver {
    /// Starts the coordinator task. `secret` is the link secret workers prove knowledge of.
    pub fn spawn(
        net: Net,
        secret: [u8; 16],
        core_cfg: CoordinatorConfig,
        cfg: DriverConfig,
        exec: Arc<dyn TaskExec>,
        blobs: Arc<dyn BlobHub>,
    ) -> Result<(CoordinatorDriver, CoordinatorHandle), SpawnError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let own = net.id();
        let signer = net.endpoint().secret_key().clone();
        let task = CoordTask {
            core_cfg: Some(core_cfg),
            signer,
            cfg,
            exec,
            blobs,
            net: net.clone(),
            tx: tx.clone(),
            events: ev_tx,
            peers: HashMap::new(),
            running: 0,
            start: Instant::now(),
        };
        spawn_owner("nuee-coordinator", move || task.run(rx))?;
        let driver = CoordinatorDriver {
            inner: Arc::new(HandlerInner {
                own,
                secret,
                gate: Admission::new(cfg.admit),
                net,
                tx: tx.clone(),
                next_conn: std::sync::atomic::AtomicU64::new(1),
            }),
        };
        Ok((driver, CoordinatorHandle { tx, events: ev_rx }))
    }

    /// The admission gate (for diagnostics and tests).
    pub fn admission(&self) -> &Admission {
        &self.inner.gate
    }

    async fn handle(&self, conn: Connection) {
        let inner = &self.inner;
        let peer = conn.remote_id();
        let pending = match inner.gate.begin(&peer) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(%peer, "connection refused: {e}");
                close(&conn, close_code::BUSY, b"busy");
                return;
            }
        };
        let hello = inner
            .gate
            .within(&peer, async {
                let (send, mut recv) = conn.accept_bi().await.map_err(|e| {
                    AdmitError::Frame(FrameError::Read(iroh::endpoint::ReadError::from(e)))
                })?;
                let msg: Msg = recv_msg(&mut recv, MAX_HELLO).await?;
                match msg {
                    Msg::Hello {
                        version,
                        proof,
                        caps,
                    } => {
                        if version != PROTO_VERSION {
                            return Err(AdmitError::BadVersion(version));
                        }
                        inner
                            .gate
                            .verify(PROOF_HELLO, &inner.secret, &peer, &inner.own, &proof)?;
                        Ok((send, recv, caps))
                    }
                    _ => Err(AdmitError::Unexpected),
                }
            })
            .await;
        drop(pending);
        let (send, recv, caps) = match hello {
            Ok(h) => h,
            Err(e) => {
                tracing::debug!(%peer, "admission failed: {e}");
                let code = match e {
                    AdmitError::Timeout => close_code::TIMEOUT,
                    AdmitError::Frame(_) | AdmitError::Unexpected => close_code::PROTOCOL,
                    AdmitError::Busy | AdmitError::Throttled => close_code::BUSY,
                    AdmitError::BadProof | AdmitError::BadVersion(_) => close_code::REFUSED,
                };
                close(&conn, code, b"refused");
                return;
            }
        };
        // Blob access is granted with the Welcome (see `apply_one`), so a peer waiting for
        // `--approve` reads nothing.
        if let Some(url) = caps.relay.as_ref().and_then(|r| relay_url(r.as_str())) {
            if inner.net.peer_relay_allowed(&url) {
                inner.net.add_peer(peer, Some(url), Vec::new());
            } else {
                tracing::info!(%peer, %url, "ignoring the relay a peer announced");
            }
        }
        let conn_id = inner
            .next_conn
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        if inner
            .tx
            .send(CoordIn::Admitted {
                peer,
                conn_id,
                caps,
                tx: out_tx,
            })
            .is_err()
        {
            close(&conn, close_code::SHUTDOWN, b"shutdown");
            return;
        }
        let reader = read_loop(recv, inner.tx.clone(), peer, conn_id);
        let writer = write_loop(send, out_rx, conn.clone());
        futures_util::future::join(reader, writer).await;
    }
}

async fn read_loop(
    mut recv: RecvStream,
    tx: mpsc::UnboundedSender<CoordIn>,
    peer: EndpointId,
    conn_id: u64,
) {
    loop {
        match recv_msg::<Msg>(&mut recv, MAX_MSG).await {
            Ok(msg) => {
                if tx.send(CoordIn::Inbound { peer, conn_id, msg }).is_err() {
                    return;
                }
            }
            Err(e) => {
                tracing::debug!(%peer, "connection ended: {e}");
                // The coordinator may already be gone; nothing else to tell.
                if tx.send(CoordIn::Gone { peer, conn_id }).is_err() {
                    tracing::trace!("coordinator stopped before the disconnect was recorded");
                }
                return;
            }
        }
    }
}

async fn write_loop(mut send: SendStream, mut rx: mpsc::UnboundedReceiver<Out>, conn: Connection) {
    while let Some(out) = rx.recv().await {
        match out {
            Out::Msg(msg) => {
                if let Err(e) = send_msg(&mut send, &msg).await {
                    tracing::debug!("send failed: {e}");
                    close(&conn, close_code::PROTOCOL, b"send failed");
                    return;
                }
            }
            Out::Close(reason) => {
                if let Err(e) = send_msg(&mut send, &Msg::Bye { reason }).await {
                    tracing::debug!("bye not delivered: {e}");
                }
                if let Err(e) = send.finish() {
                    tracing::debug!("stream already closed: {e}");
                }
                // Give the peer a moment to read the bye before tearing the connection down.
                let stopped = send.stopped();
                if n0_future::time::timeout(Duration::from_secs(2), stopped)
                    .await
                    .is_err()
                {
                    tracing::debug!("peer did not acknowledge the bye in time");
                }
                close(&conn, reason_code(reason), b"bye");
                return;
            }
        }
    }
    close(&conn, close_code::SHUTDOWN, b"shutdown");
}

impl ProtocolHandler for CoordinatorDriver {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        self.handle(connection).await;
        Ok(())
    }
}

struct PeerConn {
    conn_id: u64,
    tx: mpsc::UnboundedSender<Out>,
}

struct CoordTask {
    core_cfg: Option<CoordinatorConfig>,
    signer: SecretKey,
    cfg: DriverConfig,
    exec: Arc<dyn TaskExec>,
    blobs: Arc<dyn BlobHub>,
    net: Net,
    tx: mpsc::UnboundedSender<CoordIn>,
    events: mpsc::UnboundedSender<HostEvent>,
    peers: HashMap<EndpointId, PeerConn>,
    running: u16,
    start: Instant,
}

impl CoordTask {
    fn emit(&self, ev: HostEvent) {
        if self.events.send(ev).is_err() {
            tracing::trace!("host dropped the event receiver");
        }
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<CoordIn>) {
        let Some(core_cfg) = self.core_cfg.take() else {
            return;
        };
        self.cfg.local_slots = self.cfg.local_slots.max(core_cfg.verify_slots);
        let mut core = Coordinator::new(core_cfg, Box::new(self.signer.clone()));
        let mut next_tick = Instant::now() + self.cfg.tick;
        let now = now_ms(self.start);
        let actions = core.local_slots(now, self.cfg.local_slots);
        self.apply(actions);
        loop {
            let wait = next_tick.saturating_duration_since(Instant::now());
            let input = match n0_future::time::timeout(wait, rx.recv()).await {
                Ok(Some(input)) => input,
                Ok(None) => return,
                Err(_) => {
                    next_tick = Instant::now() + self.cfg.tick;
                    let now = now_ms(self.start);
                    let actions = core.tick(now);
                    self.apply(actions);
                    self.fill_local(&mut core, now);
                    continue;
                }
            };
            let now = now_ms(self.start);
            let actions = match input {
                CoordIn::Admitted {
                    peer,
                    conn_id,
                    caps,
                    tx,
                } => {
                    if let Some(old) = self.peers.insert(peer, PeerConn { conn_id, tx }) {
                        // A reconnect replaces the old connection.
                        if old.tx.send(Out::Close(ByeReason::Shutdown)).is_err() {
                            tracing::trace!("old connection already gone");
                        }
                        let mut a = core.disconnected(now, peer);
                        a.extend(core.admitted(now, peer, caps));
                        a
                    } else {
                        core.admitted(now, peer, caps)
                    }
                }
                CoordIn::Inbound { peer, conn_id, msg } => {
                    if self.peers.get(&peer).is_some_and(|p| p.conn_id == conn_id) {
                        core.handle(now, peer, msg)
                    } else {
                        Vec::new()
                    }
                }
                CoordIn::Gone { peer, conn_id } => {
                    if self.peers.get(&peer).is_some_and(|p| p.conn_id == conn_id) {
                        self.peers.remove(&peer);
                        self.blobs.revoke(peer);
                        self.emit(HostEvent::Closed { peer, reason: None });
                        core.disconnected(now, peer)
                    } else {
                        Vec::new()
                    }
                }
                CoordIn::LocalDone { lease, out } => {
                    self.running = self.running.saturating_sub(1);
                    let mut a = core.local_done(now, lease, out);
                    a.extend(core.local_slots(now, self.free_slots()));
                    a
                }
                CoordIn::Fetched { lease, from, res } => match res {
                    Ok(bytes) => core.output_fetched(now, lease, bytes.to_vec()),
                    Err(e) => {
                        // The core re-queues the task when the fetch deadline passes.
                        tracing::debug!(%from, lease, "output fetch failed: {e}");
                        Vec::new()
                    }
                },
                CoordIn::Submit { spec, reply } => {
                    let res = core.submit(now, *spec);
                    if reply.send(res).is_err() {
                        tracing::trace!("submit caller went away");
                    }
                    // Job broadcasts and dispatches wait in the core's outbox.
                    core.drain()
                }
                CoordIn::MarkDone { job, task, hash } => {
                    core.mark_done(job, task, hash);
                    core.drain()
                }
                CoordIn::Forget { job, reply } => {
                    if reply.send(core.forget(job)).is_err() {
                        tracing::trace!("forget caller went away");
                    }
                    core.drain()
                }
                CoordIn::Approve { peer, accept } => core.approve(now, peer, accept),
                CoordIn::Status { reply } => {
                    if reply.send(core.status()).is_err() {
                        tracing::trace!("status caller went away");
                    }
                    Vec::new()
                }
                CoordIn::SetWebrtc { on } => {
                    core.set_webrtc(on);
                    Vec::new()
                }
                CoordIn::Lane { peer, msg } => {
                    if self.peers.contains_key(&peer) {
                        core.handle(now, peer, msg)
                    } else {
                        Vec::new()
                    }
                }
                CoordIn::SendRtc {
                    to,
                    kind,
                    payload,
                    reply,
                } => {
                    let (res, actions) = match core.send_rtc(to, kind, &payload) {
                        Ok(a) => (Ok(()), a),
                        Err(e) => (Err(e), Vec::new()),
                    };
                    if reply.send(res).is_err() {
                        tracing::trace!("signalling caller went away");
                    }
                    actions
                }
                CoordIn::Shutdown { reason, reply } => {
                    for (peer, p) in self.peers.drain() {
                        if p.tx.send(Out::Close(reason)).is_err() {
                            tracing::trace!("connection already gone");
                        }
                        self.blobs.revoke(peer);
                    }
                    if reply.send(()).is_err() {
                        tracing::trace!("shutdown caller went away");
                    }
                    return;
                }
            };
            self.apply(actions);
            self.fill_local(&mut core, now);
        }
    }

    /// Offers the free local slots again: work may have appeared (a submitted job, a result
    /// to verify) since they were last offered.
    fn fill_local(&mut self, core: &mut Coordinator, now: u64) {
        if self.free_slots() > 0 {
            let actions = core.local_slots(now, self.free_slots());
            self.apply(actions);
        }
    }

    fn free_slots(&self) -> u16 {
        self.cfg.local_slots.saturating_sub(self.running)
    }

    fn apply(&mut self, actions: Vec<Action>) {
        for action in actions {
            self.apply_one(action);
        }
    }

    fn apply_one(&mut self, action: Action) {
        match action {
            Action::Send { to, msg } => {
                // The core sends Welcome only to admitted (and, under `--approve`, approved)
                // peers: that is when they may read the swarm's blobs.
                if matches!(msg, Msg::Welcome { .. }) && self.peers.contains_key(&to) {
                    self.blobs.admit(to);
                }
                if let Some(p) = self.peers.get(&to)
                    && p.tx.send(Out::Msg(Box::new(msg))).is_err()
                {
                    tracing::debug!(peer = %to, "connection writer gone");
                }
            }
            Action::Close { peer, reason } => {
                if let Some(p) = self.peers.remove(&peer)
                    && p.tx.send(Out::Close(reason)).is_err()
                {
                    tracing::debug!(%peer, "connection writer gone");
                }
                self.blobs.revoke(peer);
                self.emit(HostEvent::Closed {
                    peer,
                    reason: Some(reason),
                });
            }
            Action::RunLocal { lease, manifest } => {
                self.running = self.running.saturating_add(1);
                let exec = self.exec.clone();
                let blobs = self.blobs.clone();
                let tx = self.tx.clone();
                spawn(async move {
                    let lease_id = lease.lease;
                    // Our own leases: everything is already in our store.
                    let out =
                        run_lease(&*exec, &*blobs, Source::Local, &lease, &manifest, None).await;
                    if tx
                        .send(CoordIn::LocalDone {
                            lease: lease_id,
                            out,
                        })
                        .is_err()
                    {
                        tracing::trace!("coordinator stopped before a local result");
                    }
                });
            }
            Action::FetchOutput {
                from,
                lease,
                hash,
                size,
            } => {
                let addr = self
                    .net
                    .peer_addr(from)
                    .unwrap_or_else(|| EndpointAddr::new(from));
                let blobs = self.blobs.clone();
                let tx = self.tx.clone();
                spawn(async move {
                    let res = blobs.fetch(addr, hash, size).await;
                    if tx.send(CoordIn::Fetched { lease, from, res }).is_err() {
                        tracing::trace!("coordinator stopped before a fetch ended");
                    }
                });
            }
            Action::Accepted {
                job,
                task,
                output,
                output_hash,
                by,
                ms,
            } => {
                self.emit(HostEvent::Accepted {
                    job,
                    task,
                    output,
                    output_hash,
                    by,
                    ms,
                });
            }
            Action::Event(ev) => {
                self.emit(HostEvent::Event(ev));
            }
        }
    }
}

/// Most bytes of modules and assets a worker keeps in memory between leases.
pub const MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug)]
struct Cached {
    bytes: Bytes,
    used: u64,
}

/// Modules and assets of the jobs a worker holds, bounded by total bytes (least recently
/// used first out). Only blobs named by a held job are kept; a job leaves when the worker
/// core drops it (it keeps at most `MAX_JOBS`, the oldest goes first).
#[derive(Debug)]
pub struct BlobCache {
    cap: u64,
    total: u64,
    clock: u64,
    entries: HashMap<Hash, Cached>,
    jobs: std::collections::BTreeMap<u32, Vec<Hash>>,
}

impl BlobCache {
    /// An empty cache holding at most `cap` bytes.
    pub fn new(cap: u64) -> Self {
        Self {
            cap,
            total: 0,
            clock: 0,
            entries: HashMap::new(),
            jobs: std::collections::BTreeMap::new(),
        }
    }

    /// Bytes held.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Whether `hash` is held.
    pub fn contains(&self, hash: &Hash) -> bool {
        self.entries.contains_key(hash)
    }

    /// Records the blobs `manifest` names, as the worker core does when it accepts a job,
    /// and drops what no held job needs any more.
    pub fn hold_job(&mut self, manifest: &Manifest) {
        let job = manifest.job;
        if !self.jobs.contains_key(&job) && self.jobs.len() >= MAX_JOBS {
            match self.jobs.keys().next().copied() {
                Some(old) if old < job => {
                    self.jobs.remove(&old);
                }
                _ => return,
            }
        }
        let hashes = manifest
            .module
            .into_iter()
            .chain(manifest.assets.iter().map(|a| a.hash))
            .collect();
        self.jobs.insert(job, hashes);
        let wanted: std::collections::HashSet<Hash> =
            self.jobs.values().flatten().copied().collect();
        let mut freed = 0u64;
        self.entries.retain(|h, c| {
            let keep = wanted.contains(h);
            if !keep {
                freed += c.bytes.len() as u64;
            }
            keep
        });
        self.total = self.total.saturating_sub(freed);
    }

    fn wanted(&self, hash: &Hash) -> bool {
        self.jobs.values().any(|v| v.contains(hash))
    }

    /// The bytes of `hash`, when held.
    pub fn get(&mut self, hash: &Hash) -> Option<Bytes> {
        self.clock += 1;
        let clock = self.clock;
        self.entries.get_mut(hash).map(|c| {
            c.used = clock;
            c.bytes.clone()
        })
    }

    /// Keeps `bytes` when a held job names `hash` and they fit, evicting the least recently
    /// used entries to make room.
    pub fn insert(&mut self, hash: Hash, bytes: Bytes) {
        let len = bytes.len() as u64;
        if len > self.cap || self.entries.contains_key(&hash) || !self.wanted(&hash) {
            return;
        }
        while self.total + len > self.cap {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, c)| c.used)
                .map(|(h, _)| *h)
            else {
                break;
            };
            if let Some(c) = self.entries.remove(&oldest) {
                self.total = self.total.saturating_sub(c.bytes.len() as u64);
            }
        }
        self.clock += 1;
        self.total += len;
        self.entries.insert(
            hash,
            Cached {
                bytes,
                used: self.clock,
            },
        );
    }
}

/// Module cache shared by the leases of one worker.
type ModuleCache = Arc<Mutex<BlobCache>>;

/// Where the blobs of a lease come from.
enum Source {
    /// The coordinator that issued the lease.
    Peer(EndpointAddr),
    /// Our own store (the coordinator running its own leases).
    Local,
}

async fn get_blob(
    blobs: &dyn BlobHub,
    src: &Source,
    hash: Hash,
    size: u64,
) -> Result<Bytes, BlobError> {
    match src {
        Source::Peer(from) => blobs.fetch(from.clone(), hash, size).await,
        Source::Local => blobs.local(hash, size).await,
    }
}

async fn fetch_cached(
    blobs: &dyn BlobHub,
    cache: Option<&ModuleCache>,
    src: &Source,
    hash: Hash,
    size: u64,
) -> Result<Bytes, BlobError> {
    if let Some(cache) = cache
        && let Some(b) = lock(cache).get(&hash)
    {
        return Ok(b);
    }
    let bytes = get_blob(blobs, src, hash, size).await?;
    if let Some(cache) = cache {
        lock(cache).insert(hash, bytes.clone());
    }
    Ok(bytes)
}

/// Gathers the module, assets and input of `lease`, builds the envelope and runs it.
async fn run_lease(
    exec: &dyn TaskExec,
    blobs: &dyn BlobHub,
    src: Source,
    lease: &Lease,
    manifest: &Manifest,
    cache: Option<&ModuleCache>,
) -> Outcome {
    let io = |e: BlobError| Outcome::Fail(FailKind::Io, e.to_string());
    let module = match lease.module {
        Some(hash) => {
            let size = u64::from(manifest.module_size);
            match fetch_cached(blobs, cache, &src, hash, size).await {
                Ok(b) => Some(b),
                Err(e) => return io(e),
            }
        }
        None => None,
    };
    let mut assets = Vec::new();
    for hash in lease.assets.as_slice() {
        let Some(asset) = manifest.assets.as_slice().iter().find(|a| a.hash == *hash) else {
            return Outcome::Fail(
                FailKind::Invalid,
                format!("asset {hash} not in the manifest"),
            );
        };
        match fetch_cached(blobs, cache, &src, *hash, asset.size).await {
            Ok(b) => assets.push(b),
            Err(e) => return io(e),
        }
    }
    let input = match &lease.input {
        Payload::Inline(b) => Bytes::copy_from_slice(b.as_slice()),
        Payload::Blob { hash, size } => match get_blob(blobs, &src, *hash, *size).await {
            Ok(b) => b,
            Err(e) => return io(e),
        },
    };
    let input = if assets.is_empty() {
        input
    } else {
        let refs: Vec<&[u8]> = assets.iter().map(|a| a.as_ref()).collect();
        Bytes::from(nuee_proto::build_envelope(&refs, &input))
    };
    exec.run(lease, manifest, module, input).await
}

// ---------------------------------------------------------------------------------------------
// Worker side.

/// Joining a swarm failed.
#[derive(Debug, thiserror::Error)]
pub enum JoinError {
    /// The coordinator refused admission (wrong secret, rejected, full, banned).
    #[error("admission refused")]
    Refused(Option<ByeReason>),
    /// The coordinator could not be reached.
    #[error(transparent)]
    Net(#[from] NetError),
    /// The connection broke during the handshake.
    #[error("handshake failed: {0}")]
    Handshake(String),
}

/// What a worker reports to its host.
#[derive(Debug, Clone)]
pub enum WorkerHostEvent {
    /// Waiting for the operator to approve us.
    Pending,
    /// Admitted.
    Welcomed,
    /// Core event (leases, results, progress).
    Event(WorkerEvent),
    /// The coordinator ended the session.
    Ended(ByeReason),
    /// The connection was lost without a goodbye.
    Lost(String),
}

enum WorkerIn {
    Msg(Msg),
    Lost(String),
    Done {
        lease: u64,
        out: Outcome,
    },
    SetSlots(u16),
    LaneOpen(mpsc::UnboundedSender<Vec<u8>>),
    LaneAck(Vec<u8>),
    LaneClosed,
    SendRtc {
        to: EndpointId,
        kind: RtcKind,
        payload: String,
        reply: oneshot::Sender<Result<(), CoreError>>,
    },
    Stop,
}

/// Host-side control of a joined worker.
#[derive(Debug)]
pub struct WorkerHandle {
    tx: mpsc::UnboundedSender<WorkerIn>,
    /// Pending/welcome, core events, and how the session ended.
    pub events: mpsc::UnboundedReceiver<WorkerHostEvent>,
}

impl fmt::Debug for WorkerIn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WorkerIn")
    }
}

impl WorkerHandle {
    /// Changes the number of concurrent leases (0 pauses).
    pub fn set_slots(&self, slots: u16) -> Result<(), Stopped> {
        self.tx.send(WorkerIn::SetSlots(slots)).map_err(|_| Stopped)
    }

    /// The WebRTC lane to the coordinator opened: results now go as frames to `frames`
    /// (to be written on the data channel) until [`WorkerHandle::lane_closed`].
    pub fn lane_open(&self, frames: mpsc::UnboundedSender<Vec<u8>>) -> Result<(), Stopped> {
        self.tx
            .send(WorkerIn::LaneOpen(frames))
            .map_err(|_| Stopped)
    }

    /// A frame the coordinator sent on the lane (an ack).
    pub fn lane_frame(&self, frame: Vec<u8>) -> Result<(), Stopped> {
        self.tx.send(WorkerIn::LaneAck(frame)).map_err(|_| Stopped)
    }

    /// The lane closed or failed: unacknowledged results are resent through iroh.
    pub fn lane_closed(&self) -> Result<(), Stopped> {
        self.tx.send(WorkerIn::LaneClosed).map_err(|_| Stopped)
    }

    /// Sends a WebRTC signalling message to `to` through the coordinator.
    pub async fn send_rtc(
        &self,
        to: EndpointId,
        kind: RtcKind,
        payload: String,
    ) -> Result<(), SubmitError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(WorkerIn::SendRtc {
                to,
                kind,
                payload,
                reply,
            })
            .map_err(|_| Stopped)?;
        Ok(rx.await.map_err(|_| Stopped)??)
    }

    /// Leaves the swarm.
    pub fn stop(&self) -> Result<(), Stopped> {
        self.tx.send(WorkerIn::Stop).map_err(|_| Stopped)
    }
}

/// Connects to the coordinator of `link`, proves knowledge of its secret and waits for
/// `Welcome` (through `Pending` when the operator approves workers by hand).
pub async fn join(
    net: &Net,
    link: &JobLink,
    cfg: WorkerConfig,
    exec: Arc<dyn TaskExec>,
    blobs: Arc<dyn BlobHub>,
) -> Result<WorkerHandle, JoinError> {
    let coord = link.coordinator;
    if let Some(url) = relay_url(link.relay.as_str()) {
        net.add_peer(coord, Some(url), Vec::new());
    }
    let conn = net.connect(coord, ALPN_JOB).await?;
    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|e| JoinError::Handshake(e.to_string()))?;
    let own = net.id();
    let signer = net.endpoint().secret_key().clone();
    let proof = nuee_proto::proof(PROOF_HELLO, &link.secret, &own, &coord);
    let hello = Worker::new(cfg.clone(), coord, Box::new(signer.clone())).hello(proof);
    send_msg(&mut send, &hello)
        .await
        .map_err(|e| JoinError::Handshake(e.to_string()))?;
    blobs.admit(coord);

    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    let mut first = Vec::new();
    loop {
        let msg = match recv_msg::<Msg>(&mut recv, MAX_MSG).await {
            Ok(m) => m,
            Err(FrameError::Closed | FrameError::Read(_) | FrameError::Truncated) => {
                blobs.revoke(coord);
                // Give the close frame a moment to arrive before classifying the end.
                let code = n0_future::time::timeout(Duration::from_secs(1), conn.closed())
                    .await
                    .ok()
                    .and_then(|_| close_code_of(&conn));
                return Err(match code {
                    Some(close_code::REFUSED | close_code::BUSY | close_code::TIMEOUT) => {
                        JoinError::Refused(None)
                    }
                    Some(close_code::BANNED) => JoinError::Refused(Some(ByeReason::Banned)),
                    Some(close_code::FULL) => JoinError::Refused(Some(ByeReason::Full)),
                    _ => JoinError::Handshake("connection closed before welcome".into()),
                });
            }
            Err(e) => {
                blobs.revoke(coord);
                close(&conn, close_code::PROTOCOL, b"bad frame");
                return Err(JoinError::Handshake(e.to_string()));
            }
        };
        match msg {
            Msg::Pending => {
                if ev_tx.send(WorkerHostEvent::Pending).is_err() {
                    tracing::trace!("host dropped the event receiver");
                }
            }
            Msg::Bye { reason } => {
                blobs.revoke(coord);
                close(&conn, reason_code(reason), b"bye");
                return Err(JoinError::Refused(Some(reason)));
            }
            m @ Msg::Welcome { .. } => {
                first.push(m);
                break;
            }
            other => {
                blobs.revoke(coord);
                close(&conn, close_code::PROTOCOL, b"unexpected");
                return Err(JoinError::Handshake(format!(
                    "unexpected message before welcome: {other:?}"
                )));
            }
        }
    }
    if ev_tx.send(WorkerHostEvent::Welcomed).is_err() {
        tracing::trace!("host dropped the event receiver");
    }

    let (tx, rx) = mpsc::unbounded_channel();
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    spawn(worker_reader(recv, tx.clone()));
    spawn(write_loop(send, out_rx, conn.clone()));
    let coord_addr = net
        .peer_addr(coord)
        .unwrap_or_else(|| EndpointAddr::new(coord));
    let task = WorkerTask {
        exec,
        blobs,
        coord,
        coord_addr,
        conn,
        out: out_tx,
        tx: tx.clone(),
        events: ev_tx,
        running: HashMap::new(),
        cache: Arc::new(Mutex::new(BlobCache::new(MAX_CACHE_BYTES))),
        start: Instant::now(),
        tick: TICK,
        lane: crate::rtc::Lane::new(),
        lane_tx: None,
    };
    spawn_owner("nuee-worker", move || {
        let worker = Worker::new(cfg, coord, Box::new(signer));
        task.run(worker, first, rx)
    })
    .map_err(|e| JoinError::Handshake(e.to_string()))?;
    Ok(WorkerHandle { tx, events: ev_rx })
}

async fn worker_reader(mut recv: RecvStream, tx: mpsc::UnboundedSender<WorkerIn>) {
    loop {
        let input = match recv_msg::<Msg>(&mut recv, MAX_MSG).await {
            Ok(m) => WorkerIn::Msg(m),
            Err(e) => WorkerIn::Lost(e.to_string()),
        };
        let lost = matches!(input, WorkerIn::Lost(_));
        if tx.send(input).is_err() || lost {
            return;
        }
    }
}

struct WorkerTask {
    exec: Arc<dyn TaskExec>,
    blobs: Arc<dyn BlobHub>,
    coord: EndpointId,
    coord_addr: EndpointAddr,
    conn: Connection,
    out: mpsc::UnboundedSender<Out>,
    tx: mpsc::UnboundedSender<WorkerIn>,
    events: mpsc::UnboundedSender<WorkerHostEvent>,
    running: HashMap<u64, AbortHandle>,
    cache: ModuleCache,
    start: Instant,
    tick: Duration,
    lane: crate::rtc::Lane,
    lane_tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
}

enum Flow {
    Continue,
    Stop,
}

impl WorkerTask {
    fn emit(&self, ev: WorkerHostEvent) {
        if self.events.send(ev).is_err() {
            tracing::trace!("host dropped the event receiver");
        }
    }

    async fn run(
        mut self,
        mut worker: Worker,
        first: Vec<Msg>,
        mut rx: mpsc::UnboundedReceiver<WorkerIn>,
    ) {
        for msg in first {
            let actions = worker.handle(now_ms(self.start), msg);
            if let Flow::Stop = self.apply(actions).await {
                return self.finish();
            }
        }
        let mut next_tick = Instant::now() + self.tick;
        loop {
            let wait = next_tick.saturating_duration_since(Instant::now());
            let input = match n0_future::time::timeout(wait, rx.recv()).await {
                Ok(Some(input)) => input,
                Ok(None) => return self.finish(),
                Err(_) => {
                    next_tick = Instant::now() + self.tick;
                    let actions = worker.tick(now_ms(self.start));
                    if let Flow::Stop = self.apply(actions).await {
                        return self.finish();
                    }
                    continue;
                }
            };
            let now = now_ms(self.start);
            let actions = match input {
                WorkerIn::Msg(msg) => worker.handle(now, msg),
                WorkerIn::Lost(why) => {
                    self.emit(WorkerHostEvent::Lost(why));
                    return self.finish();
                }
                WorkerIn::Done { lease, out } => {
                    if self.running.remove(&lease).is_none() {
                        continue;
                    }
                    worker.task_done(now, lease, out)
                }
                WorkerIn::SetSlots(n) => worker.set_slots(now, n),
                WorkerIn::LaneOpen(frames) => {
                    self.lane_tx = Some(frames);
                    self.lane.open();
                    continue;
                }
                WorkerIn::LaneAck(frame) => {
                    if let Err(e) = self.lane.ack(&frame) {
                        tracing::debug!("bad frame on the lane: {e}");
                    }
                    continue;
                }
                WorkerIn::LaneClosed => {
                    if let Flow::Stop = self.lane_down() {
                        return self.finish();
                    }
                    continue;
                }
                WorkerIn::SendRtc {
                    to,
                    kind,
                    payload,
                    reply,
                } => {
                    let (res, actions) = match worker.send_rtc(to, kind, &payload) {
                        Ok(a) => (Ok(()), a),
                        Err(e) => (Err(e), Vec::new()),
                    };
                    if reply.send(res).is_err() {
                        tracing::trace!("signalling caller went away");
                    }
                    actions
                }
                WorkerIn::Stop => {
                    if self.out.send(Out::Close(ByeReason::Shutdown)).is_err() {
                        tracing::trace!("connection writer already gone");
                    }
                    self.emit(WorkerHostEvent::Ended(ByeReason::Shutdown));
                    return self.finish_keep_conn();
                }
            };
            if let Flow::Stop = self.apply(actions).await {
                return self.finish();
            }
        }
    }

    /// Falls back to iroh and resends what the lane did not acknowledge.
    fn lane_down(&mut self) -> Flow {
        self.lane_tx = None;
        for msg in self.lane.close() {
            if self.out.send(Out::Msg(Box::new(msg))).is_err() {
                self.emit(WorkerHostEvent::Lost("connection writer stopped".into()));
                return Flow::Stop;
            }
        }
        Flow::Continue
    }

    fn abort_all(&mut self) {
        for (_, h) in self.running.drain() {
            h.abort();
        }
    }

    fn finish(mut self) {
        self.abort_all();
        self.blobs.revoke(self.coord);
        close(&self.conn, close_code::DONE, b"done");
    }

    /// Ends the task but lets the writer deliver the goodbye and close the connection.
    fn finish_keep_conn(mut self) {
        self.abort_all();
        self.blobs.revoke(self.coord);
    }

    async fn apply(&mut self, actions: Vec<WorkerAction>) -> Flow {
        for action in actions {
            match action {
                WorkerAction::Send(msg) => {
                    let msg = match self.lane.route(msg) {
                        crate::rtc::Route::Iroh(m) => m,
                        crate::rtc::Route::Lane(frame) => {
                            if self.lane_tx.as_ref().is_some_and(|t| t.send(frame).is_ok()) {
                                continue;
                            }
                            // The page dropped the lane without saying so: back to iroh.
                            if let Flow::Stop = self.lane_down() {
                                return Flow::Stop;
                            }
                            continue;
                        }
                    };
                    if self.out.send(Out::Msg(Box::new(msg))).is_err() {
                        self.emit(WorkerHostEvent::Lost("connection writer stopped".into()));
                        return Flow::Stop;
                    }
                }
                WorkerAction::Run { lease, manifest } => {
                    let exec = self.exec.clone();
                    let blobs = self.blobs.clone();
                    let from = Source::Peer(self.coord_addr.clone());
                    let cache = self.cache.clone();
                    let tx = self.tx.clone();
                    let id = lease.lease;
                    let handle = spawn(async move {
                        let out =
                            run_lease(&*exec, &*blobs, from, &lease, &manifest, Some(&cache)).await;
                        if tx.send(WorkerIn::Done { lease: id, out }).is_err() {
                            tracing::trace!("worker stopped before a result");
                        }
                    });
                    if let Some(old) = self.running.insert(id, handle.abort_handle()) {
                        old.abort();
                    }
                }
                WorkerAction::Publish { hash, bytes } => {
                    match self.blobs.publish(Bytes::from(bytes)).await {
                        Ok(h) if h == hash => {}
                        Ok(h) => tracing::warn!("published blob {h} differs from expected {hash}"),
                        Err(e) => tracing::warn!("cannot publish output {hash}: {e}"),
                    }
                }
                WorkerAction::Fetch { hash, size } => {
                    let blobs = self.blobs.clone();
                    let from = Source::Peer(self.coord_addr.clone());
                    let cache = self.cache.clone();
                    spawn(async move {
                        if let Err(e) = fetch_cached(&*blobs, Some(&cache), &from, hash, size).await
                        {
                            tracing::debug!("prefetch of {hash} failed: {e}");
                        }
                    });
                }
                WorkerAction::Abort { lease } => {
                    if let Some(h) = self.running.remove(&lease) {
                        h.abort();
                    }
                }
                WorkerAction::Event(ev) => {
                    if let WorkerEvent::JobAdded { manifest, .. } = &ev {
                        lock(&self.cache).hold_job(manifest);
                    }
                    self.emit(WorkerHostEvent::Event(ev));
                }
                WorkerAction::Ended(reason) => {
                    self.emit(WorkerHostEvent::Ended(reason));
                    return Flow::Stop;
                }
            }
        }
        Flow::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuee_proto::{Asset, BoundedString, BoundedVec, Comparator, VerifyMode, VerifyPolicy};

    fn hash(b: u8) -> Hash {
        Hash([b; 32])
    }

    fn manifest(job: u32, module: u8, assets: &[u8]) -> Manifest {
        let mut list = BoundedVec::new();
        for a in assets {
            list.push(Asset {
                hash: hash(*a),
                size: 1,
                name: BoundedString::try_from("a").expect("name"),
            })
            .expect("asset");
        }
        Manifest {
            version: PROTO_VERSION,
            job,
            name: BoundedString::try_from("j").expect("name"),
            kind: nuee_proto::TaskKind::Wasm,
            module: Some(hash(module)),
            module_size: 1,
            max_memory: 1 << 20,
            assets: list,
            tasks: 1,
            verify: VerifyPolicy {
                mode: VerifyMode::Sample,
                sample_permille: 0,
                canary_permille: 0,
            },
            compare: Comparator::Exact,
            deadline_ms: 1000,
            exec: None,
            train: None,
            coordinator: SecretKey::from_bytes(&[1; 32]).public(),
        }
    }

    fn blob(n: usize) -> Bytes {
        Bytes::from(vec![0u8; n])
    }

    #[test]
    fn the_cache_is_bounded_by_bytes_least_recently_used_first() {
        let mut c = BlobCache::new(100);
        c.hold_job(&manifest(1, 1, &[2, 3]));
        c.insert(hash(1), blob(40));
        c.insert(hash(2), blob(40));
        assert!(c.get(&hash(1)).is_some());
        c.insert(hash(3), blob(40));
        assert!(c.total() <= 100, "{}", c.total());
        assert!(c.contains(&hash(1)) && c.contains(&hash(3)));
        assert!(!c.contains(&hash(2)), "the least recently used entry goes");
        c.insert(hash(2), blob(101));
        assert!(!c.contains(&hash(2)), "a blob above the cap is never kept");
        assert!(c.total() <= 100);
    }

    #[test]
    fn blobs_no_held_job_names_are_not_kept() {
        let mut c = BlobCache::new(1 << 30);
        c.insert(hash(9), blob(10));
        assert!(!c.contains(&hash(9)), "no job names it");
        c.hold_job(&manifest(1, 1, &[]));
        c.insert(hash(1), blob(10));
        assert!(c.contains(&hash(1)));
        // The worker core keeps MAX_JOBS manifests and drops the oldest first.
        for job in 2..=u32::try_from(MAX_JOBS).expect("jobs") + 1 {
            c.hold_job(&manifest(job, 100, &[]));
        }
        assert!(!c.contains(&hash(1)), "job 1 is gone, so is its module");
        assert_eq!(c.total(), 0);
    }
}
