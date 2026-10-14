//! `nuee/tcp/0`: one bi-stream per tunnel. The client sends `TcpMsg::Hello`, the host
//! answers `Ready` or `Refused`, then the stream carries raw bytes to and from
//! `127.0.0.1:<declared port>`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::protocol::{AcceptError, ProtocolHandler};
use nuee_net::{AdmitError, Net, recv_msg, send_msg};
use nuee_proto::{
    ALPN_TCP, BoundedString, EndpointId, PROOF_TCP, PROTO_VERSION, ServiceLink, TcpMsg,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, Join};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use crate::{CONNECT_TIMEOUT, ConnGate, ExposeEvent, HostError, MAX_TUNNELS, Shared, learn_host};

/// Largest handshake frame on `nuee/tcp/0`.
const MAX_TCP_MSG: usize = 512;
/// Copy buffer of each tunnel direction.
const BUF: usize = 64 * 1024;

/// Host side of `nuee/tcp/0`.
#[derive(Debug, Clone)]
pub struct ExposeHandler {
    shared: Shared,
    slots: Arc<Semaphore>,
}

impl ExposeHandler {
    pub(crate) fn new(shared: Shared) -> Self {
        Self {
            shared,
            slots: Arc::new(Semaphore::new(MAX_TUNNELS)),
        }
    }

    async fn handle(&self, conn: Connection) {
        let peer = conn.remote_id();
        let conn_gate = ConnGate::new();
        let idle = self.shared.cfg.admit.hello_timeout;
        while let Some((send, recv)) = conn_gate.next_stream(&conn, idle).await {
            let this = self.clone();
            let Some(slot) = conn_gate.slot() else {
                tokio::spawn(async move {
                    this.refuse(peer, send, "too many handshakes on this connection")
                        .await;
                });
                continue;
            };
            let conn_gate = conn_gate.clone();
            tokio::spawn(async move { this.stream(peer, send, recv, slot, conn_gate).await });
        }
    }

    async fn refuse(&self, peer: EndpointId, mut send: SendStream, reason: &str) {
        self.shared.refused(peer, reason);
        let msg = TcpMsg::Refused {
            reason: BoundedString::try_from(reason).unwrap_or_default(),
        };
        if let Err(e) = send_msg(&mut send, &msg).await {
            tracing::debug!(%peer, "refusal not delivered: {e}");
        }
        if let Err(e) = send.finish() {
            tracing::debug!(%peer, "stream already closed: {e}");
        }
        // Let the refusal reach the client before the stream is dropped.
        if tokio::time::timeout(Duration::from_secs(2), send.stopped())
            .await
            .is_err()
        {
            tracing::debug!(%peer, "client did not read the refusal in time");
        }
    }

    async fn stream(
        &self,
        peer: EndpointId,
        send: SendStream,
        mut recv: RecvStream,
        slot: OwnedSemaphorePermit,
        conn_gate: ConnGate,
    ) {
        let shared = &self.shared;
        let gate = &shared.admission;
        let hello = match gate.begin(&peer) {
            Ok(_pending) => {
                gate.within(&peer, async {
                    match recv_msg::<TcpMsg>(&mut recv, MAX_TCP_MSG).await? {
                        TcpMsg::Hello { version, proof } => {
                            if version != PROTO_VERSION {
                                return Err(AdmitError::BadVersion(version));
                            }
                            gate.verify(PROOF_TCP, &shared.cfg.secret, &peer, &shared.own, &proof)
                        }
                        _ => Err(AdmitError::Unexpected),
                    }
                })
                .await
            }
            Err(e) => Err(e),
        };
        drop(slot);
        if let Err(e) = hello {
            if matches!(e, AdmitError::Frame(_) | AdmitError::Unexpected) {
                gate.record_failure(&peer);
            }
            self.refuse(peer, send, &e.to_string()).await;
            return;
        }
        conn_gate.authenticated();
        if !shared.approved(peer).await {
            self.refuse(peer, send, "not approved by the host").await;
            return;
        }
        let Ok(_slot) = self.slots.clone().try_acquire_owned() else {
            self.refuse(peer, send, "too many open tunnels").await;
            return;
        };
        let target = shared.target();
        let local = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(target)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                tracing::warn!("cannot reach {target}: {e}");
                self.refuse(peer, send, "the exposed service is not answering")
                    .await;
                return;
            }
            Err(_) => {
                self.refuse(peer, send, "the exposed service is not answering")
                    .await;
                return;
            }
        };
        let mut send = send;
        if let Err(e) = send_msg(&mut send, &TcpMsg::Ready).await {
            tracing::debug!(%peer, "tunnel aborted before ready: {e}");
            return;
        }
        shared.report(ExposeEvent::TunnelOpened { peer });
        let (lr, lw) = local.into_split();
        let res = pump(recv, send, lr, lw, shared.cfg.idle_timeout).await;
        shared.report(ExposeEvent::TunnelClosed {
            peer,
            up: res.up,
            down: res.down,
            error: res.error,
        });
    }
}

impl ProtocolHandler for ExposeHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        self.handle(connection).await;
        Ok(())
    }
}

/// Outcome of a tunnel.
#[derive(Debug)]
pub(crate) struct Pumped {
    pub(crate) up: u64,
    pub(crate) down: u64,
    pub(crate) error: Option<String>,
}

async fn copy_half<R, W>(
    mut r: R,
    mut w: W,
    n: &AtomicU64,
    last: &AtomicU64,
    start: Instant,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; BUF];
    loop {
        let k = r.read(&mut buf).await?;
        if k == 0 {
            w.shutdown().await?;
            return Ok(());
        }
        w.write_all(buf.get(..k).unwrap_or_default()).await?;
        n.fetch_add(k as u64, Ordering::Relaxed);
        last.store(elapsed_ms(start), Ordering::Relaxed);
    }
}

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Copies both ways until both directions ended, one failed, or nothing moved for `idle`.
pub(crate) async fn pump<AR, AW, BR, BW>(ar: AR, aw: AW, br: BR, bw: BW, idle: Duration) -> Pumped
where
    AR: AsyncRead + Unpin,
    AW: AsyncWrite + Unpin,
    BR: AsyncRead + Unpin,
    BW: AsyncWrite + Unpin,
{
    let start = Instant::now();
    let up = AtomicU64::new(0);
    let down = AtomicU64::new(0);
    let last = AtomicU64::new(0);
    let both = async {
        tokio::try_join!(
            copy_half(ar, bw, &up, &last, start),
            copy_half(br, aw, &down, &last, start)
        )
    };
    let idle_ms = u64::try_from(idle.as_millis()).unwrap_or(u64::MAX);
    let watchdog = async {
        loop {
            let quiet = elapsed_ms(start).saturating_sub(last.load(Ordering::Relaxed));
            if quiet >= idle_ms {
                return;
            }
            tokio::time::sleep(Duration::from_millis(idle_ms - quiet)).await;
        }
    };
    let error = tokio::select! {
        res = both => res.err().map(|e| e.to_string()),
        () = watchdog => Some("idle timeout".to_owned()),
    };
    Pumped {
        up: up.load(Ordering::Relaxed),
        down: down.load(Ordering::Relaxed),
        error,
    }
}

/// An open tunnel: read and write it like a TCP stream.
#[derive(Debug)]
pub struct Tunnel {
    /// The byte stream (QUIC receive and send halves).
    pub io: Join<RecvStream, SendStream>,
}

impl Tunnel {
    /// The two halves, to copy each way separately.
    pub fn into_split(self) -> (RecvStream, SendStream) {
        self.io.into_inner()
    }
}

/// A connection to an exposed service that opens tunnels on demand.
#[derive(Debug, Clone)]
pub struct TunnelClient {
    conn: Connection,
    proof: [u8; 32],
}

impl TunnelClient {
    /// Connects to the host of `link`.
    pub async fn connect(net: &Net, link: &ServiceLink) -> Result<Self, HostError> {
        learn_host(net, link)?;
        let conn = tokio::time::timeout(CONNECT_TIMEOUT, net.connect(link.host, ALPN_TCP))
            .await
            .map_err(|_| HostError::Timeout)??;
        let proof = nuee_proto::proof(PROOF_TCP, &link.secret, &net.id(), &link.host);
        Ok(Self { conn, proof })
    }

    /// Opens one tunnel to the exposed port.
    pub async fn open(&self) -> Result<Tunnel, HostError> {
        let (mut send, mut recv) = self
            .conn
            .open_bi()
            .await
            .map_err(|e| HostError::Connection(e.to_string()))?;
        let hello = TcpMsg::Hello {
            version: PROTO_VERSION,
            proof: self.proof,
        };
        send_msg(&mut send, &hello).await?;
        let answer = tokio::time::timeout(
            CONNECT_TIMEOUT * 2,
            recv_msg::<TcpMsg>(&mut recv, MAX_TCP_MSG),
        )
        .await
        .map_err(|_| HostError::Timeout)?;
        match answer {
            Ok(TcpMsg::Ready) => Ok(Tunnel {
                io: tokio::io::join(recv, send),
            }),
            Ok(TcpMsg::Refused { reason }) => Err(HostError::Refused(reason.into_inner())),
            Ok(TcpMsg::Hello { .. }) => Err(HostError::Protocol("unexpected hello".into())),
            Err(e) => Err(HostError::Frame(e)),
        }
    }

    /// Closes the connection.
    pub fn close(&self) {
        self.conn.close(0u32.into(), b"done");
    }
}

/// Connects to the host of `link` and opens one tunnel.
pub async fn connect_tunnel(
    net: &Net,
    link: &ServiceLink,
) -> Result<(TunnelClient, Tunnel), HostError> {
    let client = TunnelClient::connect(net, link).await?;
    let tunnel = client.open().await?;
    Ok((client, tunnel))
}

/// Copies a local TCP connection through `tunnel` until either side ends.
pub async fn splice(
    local: TcpStream,
    tunnel: Tunnel,
    idle: Duration,
) -> (u64, u64, Option<String>) {
    let (lr, lw) = local.into_split();
    let (recv, send) = tunnel.into_split();
    let p = pump(lr, lw, recv, send, idle).await;
    (p.up, p.down, p.error)
}
