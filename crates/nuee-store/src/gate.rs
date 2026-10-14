//! Who may talk iroh-blobs with us, and the blob store behind it.
//!
//! Every blobs connection is intercepted: an unknown peer is refused at connect time, a get
//! or observe is allowed only for a hash the peer was admitted for, a per-peer byte budget
//! bounds what one peer can pull, and pushes and get-many requests are disabled.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use bytes::Bytes;
use iroh::EndpointAddr;
use iroh_blobs::api::Store;
use iroh_blobs::api::blobs::BlobStatus;
use iroh_blobs::get::GetError;
use iroh_blobs::provider::events::{
    AbortReason, ConnectMode, EventMask, EventResult, EventSender, ObserveMode, ProviderMessage,
    RequestMode, ThrottleMode,
};
use iroh_blobs::{BlobsProtocol, HashAndFormat};
use n0_future::StreamExt;
use nuee_net::Net;
use nuee_net::driver::{BlobError, BlobHub, BoxFut};
use nuee_proto::{EndpointId, Hash};

use crate::{MEM_STORE_CAP, StoreError};

/// Time allowed to open a blobs connection.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn to_iroh(h: Hash) -> iroh_blobs::Hash {
    iroh_blobs::Hash::from_bytes(h.0)
}

fn from_iroh(h: &iroh_blobs::Hash) -> Hash {
    Hash(*h.as_bytes())
}

/// Name of the tag that keeps a published or fetched blob alive.
pub fn blob_tag(hash: Hash) -> String {
    format!("nuee/blob/{hash}")
}

// ---------------------------------------------------------------------------------------------
// Gate.

#[derive(Debug, Default)]
struct Rights {
    all: bool,
    hashes: HashSet<Hash>,
    /// Files sessions open with this peer (see [`Gate::open_session`]).
    sessions: u32,
}

#[derive(Debug, Default)]
struct GateState {
    peers: HashMap<EndpointId, Rights>,
    served: HashMap<EndpointId, u64>,
    conns: HashMap<u64, EndpointId>,
}

/// Admission list of the blobs protocol, with a per-peer byte budget.
#[derive(Debug, Clone)]
pub struct Gate {
    state: Arc<Mutex<GateState>>,
    budget: u64,
}

impl Gate {
    /// A gate that lets each peer pull at most `budget` bytes in total.
    pub fn new(budget: u64) -> Gate {
        Gate {
            state: Arc::default(),
            budget,
        }
    }

    fn state(&self) -> MutexGuard<'_, GateState> {
        // Plain maps behind the lock; a panic elsewhere cannot leave them half-updated.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Per-peer byte budget.
    pub fn budget(&self) -> u64 {
        self.budget
    }

    /// Lets `peer` fetch any blob we hold (compute swarms).
    pub fn admit(&self, peer: EndpointId) {
        self.state().peers.entry(peer).or_default().all = true;
    }

    /// Lets `peer` fetch `hash` only (file shares, pins).
    pub fn admit_for(&self, peer: EndpointId, hash: Hash) {
        self.state()
            .peers
            .entry(peer)
            .or_default()
            .hashes
            .insert(hash);
    }

    /// A files session with `peer` starts: it may fetch `hashes` until its last open
    /// session ends ([`Gate::close_session`]). Sessions overlap when one peer fetches several
    /// blobs of one access secret at once (the shards of an erasure set).
    pub fn open_session(&self, peer: EndpointId, hashes: impl IntoIterator<Item = Hash>) {
        let mut st = self.state();
        let rights = st.peers.entry(peer).or_default();
        rights.hashes.extend(hashes);
        rights.sessions = rights.sessions.saturating_add(1);
    }

    /// A files session with `peer` ends; its rights are withdrawn with the last one.
    pub fn close_session(&self, peer: EndpointId) {
        let mut st = self.state();
        let last = match st.peers.get_mut(&peer) {
            Some(rights) => {
                rights.sessions = rights.sessions.saturating_sub(1);
                rights.sessions == 0
            }
            None => false,
        };
        if last {
            st.peers.remove(&peer);
        }
    }

    /// Withdraws every right of `peer`. Its byte count is kept.
    pub fn revoke(&self, peer: EndpointId) {
        self.state().peers.remove(&peer);
    }

    /// Whether `peer` may open a blobs connection.
    pub fn is_admitted(&self, peer: &EndpointId) -> bool {
        self.state().peers.contains_key(peer)
    }

    /// Whether `peer` may fetch `hash`.
    pub fn may_get(&self, peer: &EndpointId, hash: &Hash) -> bool {
        self.state()
            .peers
            .get(peer)
            .is_some_and(|r| r.all || r.hashes.contains(hash))
    }

    /// Bytes served to `peer` so far.
    pub fn served(&self, peer: &EndpointId) -> u64 {
        self.state().served.get(peer).copied().unwrap_or(0)
    }

    fn on_connect(&self, conn: u64, peer: Option<EndpointId>) -> EventResult {
        let Some(peer) = peer else {
            return Err(AbortReason::Permission);
        };
        let mut st = self.state();
        if !st.peers.contains_key(&peer) {
            tracing::debug!(%peer, "blobs connection refused: peer not admitted");
            return Err(AbortReason::Permission);
        }
        st.conns.insert(conn, peer);
        Ok(())
    }

    fn on_request(&self, conn: u64, hash: &iroh_blobs::Hash, single_blob: bool) -> EventResult {
        let hash = from_iroh(hash);
        let peer = self.state().conns.get(&conn).copied();
        match peer {
            Some(peer) if single_blob && self.may_get(&peer, &hash) => Ok(()),
            Some(peer) => {
                tracing::debug!(%peer, %hash, "blob request refused");
                Err(AbortReason::Permission)
            }
            None => Err(AbortReason::Permission),
        }
    }

    fn on_send(&self, conn: u64, size: u64) -> EventResult {
        let mut st = self.state();
        let Some(peer) = st.conns.get(&conn).copied() else {
            return Err(AbortReason::Permission);
        };
        if !st.peers.contains_key(&peer) {
            return Err(AbortReason::Permission);
        }
        let served = st.served.entry(peer).or_insert(0);
        *served = served.saturating_add(size);
        if *served > self.budget {
            tracing::debug!(%peer, "blob byte budget exhausted");
            return Err(AbortReason::RateLimited);
        }
        Ok(())
    }

    /// The provider event hook: connections and gets intercepted, sends throttled through
    /// the budget, pushes and get-many disabled. Spawns the task that answers the events.
    pub fn event_sender(&self) -> EventSender {
        let mask = EventMask {
            connected: ConnectMode::Intercept,
            get: RequestMode::Intercept,
            get_many: RequestMode::Disabled,
            push: RequestMode::Disabled,
            observe: ObserveMode::Intercept,
            throttle: ThrottleMode::Intercept,
        };
        let (tx, rx) = EventSender::channel(64, mask);
        n0_future::task::spawn(self.clone().answer(rx));
        tx
    }

    async fn answer(self, mut rx: tokio::sync::mpsc::Receiver<ProviderMessage>) {
        while let Some(msg) = rx.recv().await {
            let sent = match msg {
                ProviderMessage::ClientConnected(m) => {
                    let res = self.on_connect(m.connection_id, m.endpoint_id);
                    m.tx.send(res).await
                }
                ProviderMessage::ConnectionClosed(m) => {
                    self.state().conns.remove(&m.connection_id);
                    Ok(())
                }
                ProviderMessage::GetRequestReceived(m) => {
                    let res = self.on_request(
                        m.connection_id,
                        &m.request.hash,
                        m.request.ranges.is_blob(),
                    );
                    m.tx.send(res).await
                }
                ProviderMessage::ObserveRequestReceived(m) => {
                    let res = self.on_request(m.connection_id, &m.request.hash, true);
                    m.tx.send(res).await
                }
                ProviderMessage::Throttle(m) => {
                    let res = self.on_send(m.connection_id, m.size);
                    m.tx.send(res).await
                }
                ProviderMessage::GetManyRequestReceived(m) => {
                    m.tx.send(Err(AbortReason::Permission)).await
                }
                ProviderMessage::PushRequestReceived(m) => {
                    tracing::warn!(conn = m.connection_id, "push request refused");
                    m.tx.send(Err(AbortReason::Permission)).await
                }
                other => {
                    tracing::trace!("ignored provider event {other:?}");
                    Ok(())
                }
            };
            if let Err(e) = sent {
                // The request was dropped by the provider in the meantime.
                tracing::trace!("provider went away before our answer: {e}");
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Store hub.

/// Store limits.
#[derive(Debug, Clone, Copy)]
pub struct HubConfig {
    /// Largest blob stored or fetched.
    pub max_blob: u64,
    /// Bytes one peer may pull in total; `None` = 4 × `max_blob`.
    pub budget: Option<u64>,
    /// Total size of an in-memory store.
    pub mem_cap: u64,
    /// How often untagged data (released copies, failed or abandoned downloads) is
    /// removed from the store.
    pub gc_interval: Duration,
}

impl Default for HubConfig {
    fn default() -> Self {
        #[cfg(not(target_family = "wasm"))]
        let max_blob = crate::MAX_BLOB_NATIVE;
        #[cfg(target_family = "wasm")]
        let max_blob = MEM_STORE_CAP;
        HubConfig {
            max_blob,
            budget: None,
            mem_cap: MEM_STORE_CAP,
            gc_interval: Duration::from_secs(60),
        }
    }
}

/// A blob store, its gate and the endpoint it talks through. Implements nuee-net's
/// [`BlobHub`]. Cheap to clone.
#[derive(Debug, Clone)]
pub struct StoreHub {
    inner: Arc<HubInner>,
}

#[derive(Debug)]
struct HubInner {
    store: Store,
    gate: Gate,
    net: Net,
    protocol: BlobsProtocol,
    cfg: HubConfig,
    /// What counts against an in-memory store's cap (`None` for a file store).
    mem: Option<Mutex<MemBook>>,
    dir: Option<std::path::PathBuf>,
    #[cfg(not(target_family = "wasm"))]
    _fs: Option<iroh_blobs::store::fs::FsStore>,
}

impl StoreHub {
    fn build(
        net: &Net,
        store: Store,
        cfg: HubConfig,
        mem: bool,
        dir: Option<std::path::PathBuf>,
        #[cfg(not(target_family = "wasm"))] fs: Option<iroh_blobs::store::fs::FsStore>,
    ) -> StoreHub {
        let gate = Gate::new(cfg.budget.unwrap_or_else(|| cfg.max_blob.saturating_mul(4)));
        let protocol = BlobsProtocol::new(&store, Some(gate.event_sender()));
        StoreHub {
            inner: Arc::new(HubInner {
                store,
                gate,
                net: net.clone(),
                protocol,
                cfg,
                mem: mem.then(|| Mutex::new(MemBook::default())),
                dir,
                #[cfg(not(target_family = "wasm"))]
                _fs: fs,
            }),
        }
    }

    /// A store kept in memory, capped at `cfg.mem_cap` bytes in total (browsers, tests).
    /// Untagged data is dropped every `cfg.gc_interval`.
    pub fn memory(net: &Net, cfg: HubConfig) -> StoreHub {
        let mem =
            iroh_blobs::store::mem::MemStore::new_with_opts(iroh_blobs::store::mem::Options {
                gc_config: Some(gc_config(cfg.gc_interval)),
            });
        let store: Store = mem.into();
        #[cfg(not(target_family = "wasm"))]
        return StoreHub::build(net, store, cfg, true, None, None);
        #[cfg(target_family = "wasm")]
        return StoreHub::build(net, store, cfg, true, None);
    }

    /// A file store under `<data_dir>/blobs` (both directories 0700). Partial downloads
    /// survive restarts, so an interrupted fetch resumes; untagged data (released copies,
    /// failed or abandoned downloads) is removed every `cfg.gc_interval`, except while a
    /// fetch of it is running.
    #[cfg(not(target_family = "wasm"))]
    pub async fn open(
        net: &Net,
        data_dir: &std::path::Path,
        cfg: HubConfig,
    ) -> Result<StoreHub, StoreError> {
        crate::ensure_private_dir(data_dir)?;
        let blobs = data_dir.join("blobs");
        crate::ensure_private_dir(&blobs)?;
        let mut opts = iroh_blobs::store::fs::options::Options::new(&blobs);
        opts.gc = Some(gc_config(cfg.gc_interval));
        let fs = iroh_blobs::store::fs::FsStore::load_with_opts(blobs.join("blobs.db"), opts)
            .await
            .map_err(|e| StoreError::Store(e.to_string()))?;
        let store: Store = std::ops::Deref::deref(&fs).clone();
        Ok(StoreHub::build(
            net,
            store,
            cfg,
            false,
            Some(data_dir.to_path_buf()),
            Some(fs),
        ))
    }

    /// The admission list.
    pub fn gate(&self) -> &Gate {
        &self.inner.gate
    }

    /// The underlying iroh-blobs store.
    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    /// The network endpoint.
    pub fn net(&self) -> &Net {
        &self.inner.net
    }

    /// Limits in use.
    pub fn config(&self) -> HubConfig {
        self.inner.cfg
    }

    /// The data directory of a file store.
    pub fn data_dir(&self) -> Option<&std::path::Path> {
        self.inner.dir.as_deref()
    }

    /// The blobs protocol handler, for `router.accept(iroh_blobs::ALPN, hub.protocol())`.
    /// Shutting the router down also shuts the store down.
    pub fn protocol(&self) -> BlobsProtocol {
        self.inner.protocol.clone()
    }

    /// Refuses a blob above the limits (without reserving room for it).
    pub fn check_size(&self, size: u64) -> Result<(), StoreError> {
        let max = self.inner.cfg.max_blob;
        if size > max {
            return Err(StoreError::TooLarge { size, max });
        }
        if let Some(book) = &self.inner.mem {
            let cap = self.inner.cfg.mem_cap;
            let used = lock_book(book).used;
            if used.saturating_add(size) > cap {
                return Err(StoreError::TooLarge {
                    size,
                    max: cap.saturating_sub(used),
                });
            }
        }
        Ok(())
    }

    /// Bytes counted against an in-memory store's cap: blobs held under a tag plus the
    /// room reserved by stores and fetches in progress. `None` for a file store.
    pub fn mem_used(&self) -> Option<u64> {
        self.inner.mem.as_ref().map(|book| lock_book(book).used)
    }

    /// Checks the limits and reserves `size` bytes of an in-memory store until the blob is
    /// stored ([`Reservation::commit`]) or the attempt ends (the reservation is dropped).
    fn reserve(&self, size: u64) -> Result<Reservation<'_>, StoreError> {
        let max = self.inner.cfg.max_blob;
        if size > max {
            return Err(StoreError::TooLarge { size, max });
        }
        if let Some(book) = &self.inner.mem {
            let cap = self.inner.cfg.mem_cap;
            let mut book = lock_book(book);
            if book.used.saturating_add(size) > cap {
                return Err(StoreError::TooLarge {
                    size,
                    max: cap.saturating_sub(book.used),
                });
            }
            book.used = book.used.saturating_add(size);
        }
        Ok(Reservation {
            hub: self,
            size,
            open: true,
        })
    }

    /// Counts a complete blob kept in memory, once.
    fn hold(&self, hash: Hash, size: u64) {
        if let Some(book) = &self.inner.mem {
            let mut book = lock_book(book);
            if let std::collections::hash_map::Entry::Vacant(e) = book.held.entry(hash) {
                e.insert(size);
                book.used = book.used.saturating_add(size);
            }
        }
    }

    /// Whether the store keeps anything of `hash`, complete or partial.
    pub async fn present(&self, hash: Hash) -> Result<bool, StoreError> {
        let status = self
            .store()
            .blobs()
            .status(to_iroh(hash))
            .await
            .map_err(|e| StoreError::Store(e.to_string()))?;
        Ok(!matches!(status, BlobStatus::NotFound))
    }

    /// Whether any tag still protects `hash` from garbage collection.
    pub(crate) async fn tagged(&self, hash: Hash) -> Result<bool, StoreError> {
        let mut tags = self
            .store()
            .tags()
            .list()
            .await
            .map_err(|e| StoreError::Store(e.to_string()))?;
        while let Some(tag) = tags.next().await {
            let tag = tag.map_err(|e| StoreError::Store(e.to_string()))?;
            if from_iroh(&tag.hash) == hash {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether the store holds `hash` completely; returns its size.
    pub async fn complete_size(&self, hash: Hash) -> Result<Option<u64>, StoreError> {
        match self
            .store()
            .blobs()
            .status(to_iroh(hash))
            .await
            .map_err(|e| StoreError::Store(e.to_string()))?
        {
            BlobStatus::Complete { size } => Ok(Some(size)),
            _ => Ok(None),
        }
    }

    /// Stores `bytes` under a persistent tag.
    pub async fn add_bytes(&self, bytes: Bytes) -> Result<Hash, StoreError> {
        let hash = Hash::of(&bytes);
        if let Some(size) = self.complete_size(hash).await? {
            self.tag(&blob_tag(hash), hash).await?;
            self.hold(hash, size);
            return Ok(hash);
        }
        let size = bytes.len() as u64;
        let room = self.reserve(size)?;
        let stored = self
            .store()
            .add_bytes(bytes)
            .with_named_tag(blob_tag(hash))
            .await
            .map_err(|e| StoreError::Store(e.to_string()))?;
        if from_iroh(&stored.hash) != hash {
            return Err(StoreError::Integrity(format!(
                "store computed {} for {hash}",
                from_iroh(&stored.hash)
            )));
        }
        room.commit(hash);
        Ok(hash)
    }

    /// Imports a file under a persistent tag. Returns its hash and size.
    #[cfg(not(target_family = "wasm"))]
    pub async fn add_file(&self, path: &std::path::Path) -> Result<(Hash, u64), StoreError> {
        let path = std::fs::canonicalize(path)?;
        let size = std::fs::metadata(&path)?.len();
        let room = self.reserve(size)?;
        let tag = self
            .store()
            .add_path(&path)
            .with_tag()
            .await
            .map_err(|e| StoreError::Store(e.to_string()))?;
        let hash = from_iroh(&tag.hash);
        self.tag(&blob_tag(hash), hash).await?;
        room.commit(hash);
        Ok((hash, size))
    }

    /// Points the tag `name` at `hash`, so garbage collection keeps it.
    pub async fn tag(&self, name: &str, hash: Hash) -> Result<(), StoreError> {
        self.store()
            .tags()
            .set(name, HashAndFormat::raw(to_iroh(hash)))
            .await
            .map_err(|e| StoreError::Store(e.to_string()))
    }

    /// Whether the tag `name` exists.
    pub async fn has_tag(&self, name: &str) -> Result<bool, StoreError> {
        Ok(self
            .store()
            .tags()
            .get(name)
            .await
            .map_err(|e| StoreError::Store(e.to_string()))?
            .is_some())
    }

    /// Removes the tag `name`. A blob no tag keeps any more stops counting against an
    /// in-memory store's cap and is dropped by the next garbage collection.
    pub async fn untag(&self, name: &str) -> Result<(), StoreError> {
        let target = match &self.inner.mem {
            Some(_) => self
                .store()
                .tags()
                .get(name)
                .await
                .map_err(|e| StoreError::Store(e.to_string()))?
                .map(|t| from_iroh(&t.hash)),
            None => None,
        };
        self.store()
            .tags()
            .delete(name)
            .await
            .map_err(|e| StoreError::Store(e.to_string()))?;
        if let Some(hash) = target
            && !self.tagged(hash).await?
            && let Some(book) = &self.inner.mem
        {
            lock_book(book).drop_blob(&hash);
        }
        Ok(())
    }

    /// The whole content of a complete blob.
    pub async fn read(&self, hash: Hash) -> Result<Bytes, StoreError> {
        self.store()
            .blobs()
            .get_bytes(to_iroh(hash))
            .await
            .map_err(|e| StoreError::Store(e.to_string()))
    }

    /// Copies a complete blob to `target` (an absolute path).
    #[cfg(not(target_family = "wasm"))]
    pub async fn export(&self, hash: Hash, target: &std::path::Path) -> Result<u64, StoreError> {
        self.store()
            .blobs()
            .export(to_iroh(hash), target)
            .finish()
            .await
            .map_err(|e| StoreError::Store(e.to_string()))
    }

    /// Fetches `hash` (exactly `size` bytes) from `from` into the store, BLAKE3-verified as
    /// it streams. Data already held locally is not fetched again, so an interrupted fetch
    /// resumes. `progress(done, total)` follows the transfer. The blob is kept under its
    /// share tag, [`blob_tag`].
    pub async fn fetch_to_store<P: FnMut(u64, u64)>(
        &self,
        from: EndpointAddr,
        hash: Hash,
        size: u64,
        progress: &mut P,
    ) -> Result<(), StoreError> {
        self.fetch_to_store_as(from, hash, size, &blob_tag(hash), progress)
            .await
    }

    /// Like [`StoreHub::fetch_to_store`], but keeps the blob under the tag `tag` and touches
    /// no other tag: until `tag` is set, only a temporary tag protects the data.
    pub async fn fetch_to_store_as<P: FnMut(u64, u64)>(
        &self,
        from: EndpointAddr,
        hash: Hash,
        size: u64,
        tag: &str,
        progress: &mut P,
    ) -> Result<(), StoreError> {
        // Room is reserved before the first byte arrives, so concurrent fetches cannot
        // overrun an in-memory store's cap; it is given back if the fetch fails.
        let room = self.reserve(size)?;
        match self.complete_size(hash).await? {
            Some(s) if s == size => {
                drop(room);
                self.tag(tag, hash).await?;
                self.hold(hash, size);
                progress(size, size);
                return Ok(());
            }
            Some(s) => {
                return Err(StoreError::Integrity(format!(
                    "{hash} holds {s} bytes, the link says {size}"
                )));
            }
            None => {}
        }
        // Garbage collection leaves the partial data of a running fetch alone.
        let _running = self
            .store()
            .tags()
            .temp_tag(HashAndFormat::raw(to_iroh(hash)))
            .await
            .map_err(|e| StoreError::Store(e.to_string()))?;
        let peer = from.id;
        let conn = n0_future::time::timeout(
            CONNECT_TIMEOUT,
            self.net().endpoint().connect(from, iroh_blobs::ALPN),
        )
        .await
        .map_err(|_| StoreError::Unreachable(format!("{peer}: connection timed out")))?
        .map_err(|e| StoreError::Unreachable(format!("{peer}: {e}")))?;
        let res = self.run_fetch(&conn, hash, size, progress).await;
        conn.close(0u32.into(), b"done");
        res?;
        match self.complete_size(hash).await? {
            Some(s) if s == size => {}
            Some(s) => {
                return Err(StoreError::Integrity(format!(
                    "{hash} is {s} bytes, the link says {size}"
                )));
            }
            None => {
                return Err(StoreError::Integrity(format!(
                    "{hash} is incomplete after the transfer"
                )));
            }
        }
        self.tag(tag, hash).await?;
        room.commit(hash);
        Ok(())
    }

    async fn run_fetch<P: FnMut(u64, u64)>(
        &self,
        conn: &iroh::endpoint::Connection,
        hash: Hash,
        size: u64,
        progress: &mut P,
    ) -> Result<(), StoreError> {
        let mut stream = self
            .store()
            .remote()
            .fetch(conn.clone(), to_iroh(hash))
            .stream();
        use iroh_blobs::api::remote::GetProgressItem;
        while let Some(item) = stream.next().await {
            match item {
                GetProgressItem::Progress(done) => {
                    if done > size {
                        return Err(StoreError::TooLarge {
                            size: done,
                            max: size,
                        });
                    }
                    progress(done, size);
                }
                GetProgressItem::Done(_) => return Ok(()),
                GetProgressItem::Error(e) => return Err(classify(conn, e)),
            }
        }
        Err(StoreError::Unreachable(
            "transfer ended without a result".into(),
        ))
    }
}

fn gc_config(interval: Duration) -> iroh_blobs::store::GcConfig {
    iroh_blobs::store::GcConfig {
        interval,
        add_protected: None,
    }
}

/// What counts against an in-memory store's cap.
#[derive(Debug, Default)]
struct MemBook {
    /// Held plus reserved bytes.
    used: u64,
    /// Blobs held under a tag, with their size.
    held: HashMap<Hash, u64>,
}

impl MemBook {
    fn drop_blob(&mut self, hash: &Hash) {
        if let Some(size) = self.held.remove(hash) {
            self.used = self.used.saturating_sub(size);
        }
    }
}

fn lock_book(book: &Mutex<MemBook>) -> MutexGuard<'_, MemBook> {
    // Plain counters behind the lock; a panic elsewhere cannot leave them half-updated.
    book.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Room reserved in an in-memory store; given back when dropped uncommitted.
struct Reservation<'a> {
    hub: &'a StoreHub,
    size: u64,
    open: bool,
}

impl Reservation<'_> {
    /// The blob is stored under a tag: the room becomes its size (once per blob).
    fn commit(mut self, hash: Hash) {
        self.open = false;
        if let Some(book) = &self.hub.inner.mem {
            let mut book = lock_book(book);
            match book.held.entry(hash) {
                std::collections::hash_map::Entry::Occupied(_) => {
                    book.used = book.used.saturating_sub(self.size);
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(self.size);
                }
            }
        }
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if self.open
            && let Some(book) = &self.hub.inner.mem
        {
            let mut book = lock_book(book);
            book.used = book.used.saturating_sub(self.size);
        }
    }
}

fn classify(conn: &iroh::endpoint::Connection, e: GetError) -> StoreError {
    use iroh_blobs::protocol::{ERR_LIMIT, ERR_PERMISSION};
    let code = e.iroh_error_code().or_else(|| match conn.close_reason() {
        Some(iroh::endpoint::ConnectionError::ApplicationClosed(ac)) => Some(ac.error_code),
        _ => None,
    });
    if code == Some(ERR_PERMISSION) || code == Some(ERR_LIMIT) {
        return StoreError::Refused;
    }
    if matches!(e, GetError::Decode { .. }) && e.remote_read().is_none() {
        return StoreError::Integrity(e.to_string());
    }
    StoreError::Unreachable(e.to_string())
}

fn blob_error(hash: Hash, e: StoreError) -> BlobError {
    match e {
        StoreError::TooLarge { size, max } => BlobError::TooLarge { size, max },
        StoreError::Integrity(_) => BlobError::Integrity(hash),
        other => BlobError::Io(other.to_string()),
    }
}

impl BlobHub for StoreHub {
    fn fetch(
        &self,
        from: EndpointAddr,
        hash: Hash,
        size: u64,
    ) -> BoxFut<'_, Result<Bytes, BlobError>> {
        Box::pin(async move {
            self.fetch_to_store(from, hash, size, &mut |_, _| {})
                .await
                .map_err(|e| blob_error(hash, e))?;
            self.read(hash).await.map_err(|e| blob_error(hash, e))
        })
    }

    fn publish(&self, bytes: Bytes) -> BoxFut<'_, Result<Hash, BlobError>> {
        Box::pin(async move {
            let hash = Hash::of(&bytes);
            self.add_bytes(bytes).await.map_err(|e| blob_error(hash, e))
        })
    }

    fn local(&self, hash: Hash, size: u64) -> BoxFut<'_, Result<Bytes, BlobError>> {
        Box::pin(async move {
            match self
                .complete_size(hash)
                .await
                .map_err(|e| blob_error(hash, e))?
            {
                Some(s) if s == size => self.read(hash).await.map_err(|e| blob_error(hash, e)),
                Some(_) => Err(BlobError::Integrity(hash)),
                None => Err(BlobError::NotFound(hash)),
            }
        })
    }

    fn admit(&self, peer: EndpointId) {
        self.inner.gate.admit(peer);
    }

    fn revoke(&self, peer: EndpointId) {
        self.inner.gate.revoke(peer);
    }
}

/// The [`BlobHub`] of a coordinator whose store is small (a page-hosted swarm): a peer's
/// blob output is refused before any byte is fetched when its declared size is above
/// `max_output` (what a worker may produce, `LocalLimits::max_output`), and no copy of a
/// fetched output is kept in the store (the coordinator holds the bytes it returns).
#[derive(Debug, Clone)]
pub struct OutputCappedHub {
    hub: StoreHub,
    max_output: u64,
}

impl OutputCappedHub {
    /// Wraps `hub`; outputs above `max_output` bytes are refused.
    pub fn new(hub: StoreHub, max_output: u64) -> OutputCappedHub {
        OutputCappedHub { hub, max_output }
    }
}

impl BlobHub for OutputCappedHub {
    fn fetch(
        &self,
        from: EndpointAddr,
        hash: Hash,
        size: u64,
    ) -> BoxFut<'_, Result<Bytes, BlobError>> {
        Box::pin(async move {
            if size > self.max_output {
                return Err(BlobError::TooLarge {
                    size,
                    max: self.max_output,
                });
            }
            let ours = self
                .hub
                .complete_size(hash)
                .await
                .map_err(|e| blob_error(hash, e))?
                .is_some();
            let bytes = BlobHub::fetch(&self.hub, from, hash, size).await?;
            if !ours {
                self.hub
                    .untag(&blob_tag(hash))
                    .await
                    .map_err(|e| blob_error(hash, e))?;
            }
            Ok(bytes)
        })
    }

    fn publish(&self, bytes: Bytes) -> BoxFut<'_, Result<Hash, BlobError>> {
        self.hub.publish(bytes)
    }

    fn local(&self, hash: Hash, size: u64) -> BoxFut<'_, Result<Bytes, BlobError>> {
        self.hub.local(hash, size)
    }

    fn admit(&self, peer: EndpointId) {
        self.hub.admit(peer);
    }

    fn revoke(&self, peer: EndpointId) {
        BlobHub::revoke(&self.hub, peer);
    }
}
