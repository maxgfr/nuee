//! File shares: a swarm in `Files` mode on `nuee/job/0` whose only purpose is to admit
//! downloaders to one blob, plus the downloading side.
//!
//! The provider runs a [`FilesAcceptor`] instead of a compute coordinator: it checks the
//! `Hello` proof against the access secret of each offered blob (one for a share, many for
//! a pin server), admits the peer for that hash only, answers `Welcome` with the file, and
//! keeps the admission while the session lasts.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use iroh::EndpointAddr;
use iroh::endpoint::{Connection, SendStream, VarInt};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use nuee_net::driver::{MAX_HELLO, close_code, close_code_of};
use nuee_net::{Admission, AdmitConfig, AdmitError, FrameError, Net, recv_msg, send_msg};
use nuee_proto::limits::MAX_MSG;
use nuee_proto::{
    ALPN_JOB, BoundedString, BoundedVec, ByeReason, Caps, EndpointId, FileInfo, FileLink, Hash,
    Msg, PROOF_HELLO, PROTO_VERSION, Provider, SwarmInfo, SwarmMode,
};

use crate::StoreError;
use crate::crypt::{AgeKey, decrypt_bytes, encrypt_bytes, pack_bytes, unpack_bytes};
use crate::gate::{CONNECT_TIMEOUT, StoreHub};

/// Time allowed for the provider to answer our `Hello`.
pub const WELCOME_TIMEOUT: Duration = Duration::from_secs(10);

/// A blob offered to whoever proves knowledge of `access`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Offer {
    /// Access secret from the link.
    pub access: [u8; 16],
    /// Ciphertext hash.
    pub hash: Hash,
    /// Ciphertext size.
    pub size: u64,
}

// ---------------------------------------------------------------------------------------------
// Provider side.

/// `nuee/job/0` handler of a files swarm. Register it with
/// `net.router().accept(ALPN_JOB, acceptor).accept(iroh_blobs::ALPN, hub.protocol())`.
#[derive(Debug, Clone)]
pub struct FilesAcceptor {
    inner: Arc<AcceptorInner>,
}

#[derive(Debug)]
struct AcceptorInner {
    own: EndpointId,
    name: BoundedString<64>,
    admission: Admission,
    hub: StoreHub,
    offers: Mutex<Vec<Offer>>,
}

fn close(conn: &Connection, code: u32, reason: &[u8]) {
    conn.close(VarInt::from_u32(code), reason);
}

impl FilesAcceptor {
    /// An acceptor serving `offers` from `hub`.
    pub fn new(hub: &StoreHub, name: &str, offers: Vec<Offer>) -> FilesAcceptor {
        let name = BoundedString::try_from(name).unwrap_or_else(|_| {
            tracing::debug!("swarm name too long, left empty");
            BoundedString::new()
        });
        FilesAcceptor {
            inner: Arc::new(AcceptorInner {
                own: hub.net().id(),
                name,
                admission: Admission::new(AdmitConfig::default()),
                hub: hub.clone(),
                offers: Mutex::new(offers),
            }),
        }
    }

    fn offers(&self) -> std::sync::MutexGuard<'_, Vec<Offer>> {
        // A plain list behind the lock; a panic elsewhere cannot leave it half-updated.
        self.inner
            .offers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Starts offering one more blob.
    pub fn add(&self, offer: Offer) {
        let mut offers = self.offers();
        offers.retain(|o| o.hash != offer.hash);
        offers.push(offer);
    }

    /// Stops offering `hash` to new sessions.
    pub fn remove(&self, hash: &Hash) {
        self.offers().retain(|o| o.hash != *hash);
    }

    /// The blobs on offer.
    pub fn list(&self) -> Vec<Offer> {
        self.offers().clone()
    }

    /// The offers whose access secret produced `proof`; every candidate is checked in
    /// constant time. Several blobs may share one access secret (the header and shards of
    /// an erasure set): a session is then admitted for all of them.
    fn matching(&self, peer: &EndpointId, proof: &[u8; 32]) -> Vec<Offer> {
        let own = self.inner.own;
        let mut found = Vec::new();
        for o in self.offers().iter() {
            if nuee_proto::check_proof(PROOF_HELLO, &o.access, peer, &own, proof) {
                found.push(*o);
            }
        }
        found
    }

    async fn handle(&self, conn: Connection) {
        let inner = &self.inner;
        let peer = conn.remote_id();
        let pending = match inner.admission.begin(&peer) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(%peer, "files connection refused: {e}");
                close(&conn, close_code::BUSY, b"busy");
                return;
            }
        };
        let hello = inner
            .admission
            .within(&peer, async {
                let (send, mut recv) = conn.accept_bi().await.map_err(|e| {
                    AdmitError::Frame(FrameError::Read(iroh::endpoint::ReadError::from(e)))
                })?;
                match recv_msg::<Msg>(&mut recv, MAX_HELLO).await? {
                    Msg::Hello { version, proof, .. } => {
                        if version != PROTO_VERSION {
                            return Err(AdmitError::BadVersion(version));
                        }
                        let offers = self.matching(&peer, &proof);
                        match offers.first().copied() {
                            Some(offer) => Ok((send, recv, offer, offers)),
                            None => {
                                inner.admission.record_failure(&peer);
                                Err(AdmitError::BadProof)
                            }
                        }
                    }
                    _ => Err(AdmitError::Unexpected),
                }
            })
            .await;
        drop(pending);
        let (mut send, mut recv, offer, offers) = match hello {
            Ok(h) => h,
            Err(e) => {
                tracing::debug!(%peer, "files admission failed: {e}");
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
        let gate = inner.hub.gate();
        gate.open_session(peer, offers.iter().map(|o| o.hash));
        let welcome = Msg::Welcome {
            swarm: SwarmInfo {
                name: inner.name.clone(),
                coordinator: inner.own,
                mode: SwarmMode::Files,
                file: Some(FileInfo {
                    hash: offer.hash,
                    size: offer.size,
                }),
            },
            jobs: BoundedVec::new(),
            webrtc: false,
            stun: BoundedVec::new(),
        };
        if let Err(e) = send_msg(&mut send, &welcome).await {
            tracing::debug!(%peer, "welcome not delivered: {e}");
            gate.close_session(peer);
            close(&conn, close_code::PROTOCOL, b"send failed");
            return;
        }
        // The session lasts until the downloader says goodbye or goes away.
        loop {
            match recv_msg::<Msg>(&mut recv, MAX_MSG).await {
                Ok(Msg::Bye { .. }) => break,
                Ok(Msg::Want { .. }) => {}
                Ok(other) => {
                    tracing::debug!(%peer, "unexpected message in a files swarm: {other:?}");
                    break;
                }
                Err(e) => {
                    tracing::debug!(%peer, "files session ended: {e}");
                    break;
                }
            }
        }
        gate.close_session(peer);
        close(&conn, close_code::DONE, b"done");
    }
}

impl ProtocolHandler for FilesAcceptor {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        self.handle(connection).await;
        Ok(())
    }
}

/// A running files swarm: the router serving `nuee/job/0` and the blobs protocol.
#[derive(Debug)]
pub struct ServeHandle {
    /// The router; shutting it down stops serving (and closes the store).
    pub router: Router,
    /// The acceptor, to add or remove offers while running.
    pub acceptor: FilesAcceptor,
}

impl ServeHandle {
    /// Starts serving `acceptor` and the blobs of its hub on `net`.
    pub fn spawn(net: &Net, hub: &StoreHub, acceptor: FilesAcceptor) -> ServeHandle {
        let router = net
            .router()
            .accept(ALPN_JOB, acceptor.clone())
            .accept(iroh_blobs::ALPN, hub.protocol())
            .spawn();
        ServeHandle { router, acceptor }
    }

    /// Stops serving.
    pub async fn shutdown(&self) -> Result<(), StoreError> {
        self.router
            .shutdown()
            .await
            .map_err(|e| StoreError::Store(e.to_string()))
    }
}

/// A file share in progress.
#[derive(Debug)]
pub struct ShareHandle {
    /// The link to hand out.
    pub link: FileLink,
    /// What serves it.
    pub serve: ServeHandle,
    /// Symbolic links and special files left out of the archive.
    pub skipped: Vec<std::path::PathBuf>,
}

impl ShareHandle {
    /// Stops sharing.
    pub async fn shutdown(&self) -> Result<(), StoreError> {
        self.serve.shutdown().await
    }
}

/// This node as a link provider.
pub fn self_provider(net: &Net) -> Provider {
    let relay = net.home_relay().and_then(|url| {
        let s = url.to_string();
        match BoundedString::try_from(s.as_str()) {
            Ok(b) => Some(b),
            Err(e) => {
                tracing::warn!("relay URL {s:?} too long for a link: {e}");
                None
            }
        }
    });
    Provider {
        id: net.id(),
        relay,
    }
}

async fn start_share(
    net: &Net,
    hub: &StoreHub,
    hash: Hash,
    size: u64,
    key: AgeKey,
    skipped: Vec<std::path::PathBuf>,
) -> Result<ShareHandle, StoreError> {
    let access = nuee_proto::new_secret16()?;
    let mut providers = BoundedVec::new();
    providers
        .push(self_provider(net))
        .map_err(|e| StoreError::Store(e.to_string()))?;
    let link = FileLink {
        providers,
        hash,
        size,
        access,
        key: key.0,
    };
    let acceptor = FilesAcceptor::new(hub, "files", vec![Offer { access, hash, size }]);
    let serve = ServeHandle::spawn(net, hub, acceptor);
    Ok(ShareHandle {
        link,
        serve,
        skipped,
    })
}

/// Shares in-memory files (`(relative name, bytes)`): packs, encrypts, stores, then serves
/// the files swarm on `net`.
pub async fn share_bytes(
    net: &Net,
    hub: &StoreHub,
    files: &[(String, Vec<u8>)],
) -> Result<ShareHandle, StoreError> {
    let tar = pack_bytes(files)?;
    let (cipher, key) = encrypt_bytes(&tar)?;
    let size = cipher.len() as u64;
    let hash = hub.add_bytes(bytes::Bytes::from(cipher)).await?;
    start_share(net, hub, hash, size, key, Vec::new()).await
}

/// Shares files and directories from disk; the archive and its ciphertext never sit in
/// memory as a whole.
#[cfg(not(target_family = "wasm"))]
pub async fn share_paths(
    net: &Net,
    hub: &StoreHub,
    paths: &[std::path::PathBuf],
) -> Result<ShareHandle, StoreError> {
    let mut packed = crate::crypt::pack(paths)?;
    let tmp = match hub.data_dir() {
        Some(dir) => {
            let t = dir.join("tmp");
            crate::ensure_private_dir(&t)?;
            tempfile::NamedTempFile::new_in(t)?
        }
        None => tempfile::NamedTempFile::new()?,
    };
    let key = {
        let mut out = std::io::BufWriter::new(tmp.as_file());
        let key = crate::crypt::encrypt(&mut packed.file, &mut out)?;
        std::io::Write::flush(&mut out)?;
        key
    };
    let (hash, size) = hub.add_file(tmp.path()).await?;
    start_share(net, hub, hash, size, key, packed.skipped).await
}

// ---------------------------------------------------------------------------------------------
// Downloader side.

/// Caps of a pure downloader: no slot, no task kind.
fn downloader_caps(net: &Net) -> Caps {
    Caps {
        agent: BoundedString::try_from("nuee-get").unwrap_or_default(),
        browser: cfg!(target_family = "wasm"),
        cpu_slots: 0,
        gpu: None,
        mem_mb: 0,
        kinds: 0,
        cached: BoundedVec::new(),
        exec_allow: BoundedVec::new(),
        store_quota_mb: 0,
        relay: self_provider(net).relay,
        rtc: false,
    }
}

/// An open files session with one provider.
struct Session {
    conn: Connection,
    send: SendStream,
}

impl Session {
    async fn end(mut self) {
        if let Err(e) = send_msg(
            &mut self.send,
            &Msg::Bye {
                reason: ByeReason::Done,
            },
        )
        .await
        {
            tracing::debug!("goodbye not delivered: {e}");
        }
        if let Err(e) = self.send.finish() {
            tracing::debug!("stream already closed: {e}");
        }
        let stopped = self.send.stopped();
        if n0_future::time::timeout(Duration::from_secs(2), stopped)
            .await
            .is_err()
        {
            tracing::debug!("provider did not acknowledge the goodbye in time");
        }
        close(&self.conn, close_code::DONE, b"done");
    }
}

async fn open_session(
    net: &Net,
    provider: &Provider,
    access: &[u8; 16],
) -> Result<(Session, Option<FileInfo>), StoreError> {
    let id = provider.id;
    if let Some(relay) = &provider.relay
        && let Err(e) = net.add_peer_relay_str(id, relay.as_str())
    {
        tracing::debug!(%id, "ignoring the provider relay: {e}");
    }
    let conn = n0_future::time::timeout(CONNECT_TIMEOUT, net.connect(id, ALPN_JOB))
        .await
        .map_err(|_| StoreError::Unreachable(format!("{id}: connection timed out")))?
        .map_err(|e| StoreError::Unreachable(e.to_string()))?;
    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|e| StoreError::Unreachable(format!("{id}: {e}")))?;
    let proof = nuee_proto::proof(PROOF_HELLO, access, &net.id(), &id);
    let hello = Msg::Hello {
        version: PROTO_VERSION,
        proof,
        caps: downloader_caps(net),
    };
    send_msg(&mut send, &hello)
        .await
        .map_err(|e| StoreError::Unreachable(format!("{id}: {e}")))?;
    let welcome = n0_future::time::timeout(WELCOME_TIMEOUT, async {
        loop {
            match recv_msg::<Msg>(&mut recv, MAX_MSG).await {
                Ok(Msg::Pending) => continue,
                other => return other,
            }
        }
    })
    .await
    .map_err(|_| StoreError::Unreachable(format!("{id}: no answer to hello")))?;
    match welcome {
        Ok(Msg::Welcome { swarm, .. }) => {
            if swarm.mode != SwarmMode::Files || swarm.file.is_none() {
                close(&conn, close_code::DONE, b"wrong swarm");
                return Err(StoreError::Integrity(format!("{id} is not a files swarm")));
            }
            Ok((Session { conn, send }, swarm.file))
        }
        Ok(Msg::Bye { .. }) => Err(StoreError::Refused),
        Ok(other) => {
            close(&conn, close_code::PROTOCOL, b"unexpected");
            Err(StoreError::Unreachable(format!(
                "{id}: unexpected answer {other:?}"
            )))
        }
        Err(e) => {
            // Give the close frame a moment to arrive before classifying the end.
            let code = n0_future::time::timeout(Duration::from_secs(1), conn.closed())
                .await
                .ok()
                .and_then(|_| close_code_of(&conn));
            match code {
                Some(close_code::REFUSED | close_code::BUSY | close_code::TIMEOUT) => {
                    Err(StoreError::Refused)
                }
                _ => Err(StoreError::Unreachable(format!("{id}: {e}"))),
            }
        }
    }
}

fn rank(e: &StoreError) -> u8 {
    match e {
        StoreError::Integrity(_) => 4,
        StoreError::TooLarge { .. } => 3,
        StoreError::Refused => 2,
        StoreError::Unreachable(_) => 1,
        _ => 0,
    }
}

/// Clears the relay of every provider that a remote peer named (in a `Replicate` order, a
/// placement order or an erasure set) when [`Net::peer_relay_allowed`] refuses it, so it is
/// never registered nor dialled. Providers from a link the local user opened keep their relay.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn drop_refused_relays(net: &Net, providers: &mut [Provider]) {
    for p in providers {
        let refused = p.relay.as_ref().is_some_and(|r| {
            !r.as_str().is_empty()
                && r.as_str()
                    .parse()
                    .map_or(true, |url| !net.peer_relay_allowed(&url))
        });
        if refused {
            tracing::debug!(provider = %p.id, relay = ?p.relay, "ignoring a refused provider relay");
            p.relay = None;
        }
    }
}

fn provider_addr(net: &Net, provider: &Provider) -> EndpointAddr {
    net.peer_addr(provider.id)
        .unwrap_or_else(|| EndpointAddr::new(provider.id))
}

/// A files session with each of `providers` in turn, for blobs that share `access`.
pub(crate) struct SetSession {
    session: Session,
    /// Where to fetch the blobs.
    pub addr: EndpointAddr,
}

impl SetSession {
    /// Opens a session with `provider`: one hello, admitted for every blob it holds under
    /// `access`.
    pub(crate) async fn open(
        net: &Net,
        provider: &Provider,
        access: &[u8; 16],
    ) -> Result<SetSession, StoreError> {
        let (session, _) = open_session(net, provider, access).await?;
        Ok(SetSession {
            session,
            addr: provider_addr(net, provider),
        })
    }

    /// Says goodbye.
    pub(crate) async fn end(self) {
        self.session.end().await;
    }
}

/// Fetches a ciphertext into `hub` from the first provider that admits us and serves the
/// right bytes, kept under its share tag. Returns the provider used.
pub(crate) async fn fetch_ciphertext<P: FnMut(u64, u64)>(
    net: &Net,
    hub: &StoreHub,
    providers: &[Provider],
    access: &[u8; 16],
    hash: Hash,
    size: u64,
    progress: &mut P,
) -> Result<Provider, StoreError> {
    let tag = crate::gate::blob_tag(hash);
    let want = FileInfo { hash, size };
    fetch_ciphertext_as(net, hub, providers, access, want, &tag, progress).await
}

/// Like [`fetch_ciphertext`], but keeps the ciphertext `want` under `tag` and sets no other
/// tag.
pub(crate) async fn fetch_ciphertext_as<P: FnMut(u64, u64)>(
    net: &Net,
    hub: &StoreHub,
    providers: &[Provider],
    access: &[u8; 16],
    want: FileInfo,
    tag: &str,
    progress: &mut P,
) -> Result<Provider, StoreError> {
    let FileInfo { hash, size } = want;
    hub.check_size(size)?;
    let mut worst: Option<StoreError> = None;
    for provider in providers {
        let res = async {
            let (session, file) = open_session(net, provider, access).await?;
            let wrong = || {
                StoreError::Integrity(format!(
                    "{} serves {file:?} instead of {hash} ({size} bytes)",
                    provider.id
                ))
            };
            // The provider names one of the blobs the secret opens; a set member may hold
            // several under one secret, so another name is only an error if `hash` then
            // cannot be fetched.
            if file.is_some_and(|f| f.hash == hash && f != want) {
                session.end().await;
                return Err(wrong());
            }
            let addr = provider_addr(net, provider);
            let res = hub.fetch_to_store_as(addr, hash, size, tag, progress).await;
            session.end().await;
            match res {
                Err(StoreError::Integrity(e)) => Err(StoreError::Integrity(e)),
                Err(_) if file != Some(want) => Err(wrong()),
                other => other,
            }
        }
        .await;
        match res {
            Ok(()) => return Ok(provider.clone()),
            Err(e) => {
                tracing::debug!(provider = %provider.id, "provider failed: {e}");
                if worst.as_ref().is_none_or(|w| rank(&e) > rank(w)) {
                    worst = Some(e);
                }
            }
        }
    }
    Err(worst.unwrap_or(StoreError::NoProvider))
}

/// Downloads, checks, decrypts and unpacks a share in memory. Works in browsers.
pub async fn get_bytes<P: FnMut(u64, u64)>(
    net: &Net,
    hub: &StoreHub,
    link: &FileLink,
    mut progress: P,
) -> Result<Vec<(String, Vec<u8>)>, StoreError> {
    fetch_ciphertext(
        net,
        hub,
        link.providers.as_slice(),
        &link.access,
        link.hash,
        link.size,
        &mut progress,
    )
    .await?;
    // An erasure header instead of the ciphertext: rebuild it from `k` shards of the set.
    let (cipher, hash, size) = match set_header(hub, link).await? {
        Some(header) => {
            let cipher = crate::erasure::fetch_set_with(
                net,
                hub,
                link.providers.as_slice(),
                &link.access,
                &header,
                &mut progress,
            )
            .await?;
            (bytes::Bytes::from(cipher), header.original, header.len)
        }
        None => (hub.read(link.hash).await?, link.hash, link.size),
    };
    if cipher.len() as u64 != size || Hash::of(&cipher) != hash {
        return Err(StoreError::Integrity(format!(
            "stored bytes do not match {hash}"
        )));
    }
    let tar = decrypt_bytes(&cipher, &AgeKey(link.key))?;
    unpack_bytes(&tar)
}

/// The erasure header `link` points to, if its blob (already in `hub`) is one.
async fn set_header(
    hub: &StoreHub,
    link: &FileLink,
) -> Result<Option<crate::erasure::Header>, StoreError> {
    if link.size > crate::erasure::MAX_HEADER as u64 {
        return Ok(None);
    }
    let bytes = hub.read(link.hash).await?;
    if !bytes.starts_with(crate::erasure::MAGIC) {
        return Ok(None);
    }
    crate::erasure::decode_header(&bytes)
        .map(Some)
        .map_err(|e| StoreError::Integrity(e.to_string()))
}

/// What a native get wrote.
#[cfg(not(target_family = "wasm"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetReport {
    /// Files written.
    pub files: Vec<std::path::PathBuf>,
    /// Plaintext archive size.
    pub bytes: u64,
    /// The ciphertext matched the link's BLAKE3 hash and size.
    pub verified: bool,
}

/// Downloads a share and unpacks it under `dest`. The ciphertext is verified, then fully
/// decrypted to a temporary file before anything is unpacked, so a tampered share never
/// leaves partial files behind.
#[cfg(not(target_family = "wasm"))]
pub async fn get<P: FnMut(u64, u64)>(
    net: &Net,
    hub: &StoreHub,
    link: &FileLink,
    dest: &std::path::Path,
    overwrite: bool,
    mut progress: P,
) -> Result<GetReport, StoreError> {
    use std::io::{Read, Seek, SeekFrom};

    fetch_ciphertext(
        net,
        hub,
        link.providers.as_slice(),
        &link.access,
        link.hash,
        link.size,
        &mut progress,
    )
    .await?;
    // A link may point to an erasure header instead of the ciphertext (age output never
    // starts with the header magic): the ciphertext is then rebuilt from its shards.
    let (hash, size) = match set_header(hub, link).await? {
        Some(header) => {
            let cipher = crate::erasure::fetch_set(
                net,
                hub,
                link.providers.as_slice(),
                &link.access,
                &header,
            )
            .await?;
            hub.add_bytes(bytes::Bytes::from(cipher)).await?;
            (header.original, header.len)
        }
        None => (link.hash, link.size),
    };
    let tmp = match hub.data_dir() {
        Some(dir) => {
            let t = dir.join("tmp");
            crate::ensure_private_dir(&t)?;
            tempfile::tempdir_in(t)?
        }
        None => tempfile::tempdir()?,
    };
    let base = std::fs::canonicalize(tmp.path())?;
    let cipher_path = base.join("cipher");
    hub.export(hash, &cipher_path).await?;

    let mut cipher = std::fs::File::open(&cipher_path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut len = 0u64;
    loop {
        let n = cipher.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(buf.get(..n).unwrap_or_default());
        len += n as u64;
    }
    if len != size || Hash(*hasher.finalize().as_bytes()) != hash {
        return Err(StoreError::Integrity(format!(
            "exported bytes do not match {}",
            hash
        )));
    }
    cipher.seek(SeekFrom::Start(0))?;

    let mut plain = tempfile::tempfile_in(&base)?;
    let bytes = {
        let mut out = std::io::BufWriter::new(&mut plain);
        let n = crate::crypt::decrypt(cipher, &AgeKey(link.key), &mut out)?;
        std::io::Write::flush(&mut out)?;
        n
    };
    plain.seek(SeekFrom::Start(0))?;
    let files = crate::crypt::unpack(std::io::BufReader::new(plain), dest, overwrite)?;
    Ok(GetReport {
        files,
        bytes,
        verified: true,
    })
}
