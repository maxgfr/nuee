//! Exposed services: a TCP tunnel to one loopback port (`nuee/tcp/0`) and bounded HTTP
//! console requests to the same port (`nuee/http/0`).
//!
//! The host only ever connects to `127.0.0.1:<port>` chosen when the service is exposed;
//! nothing a client sends can name another target.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::protocol::Router;
use nuee_net::driver::close_code;
use nuee_net::{Admission, AdmitConfig, FrameError, Net, NetError};
use nuee_proto::{ALPN_HTTP, ALPN_TCP, BoundedString, EndpointId, ServiceLink};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub mod gateway;
pub mod http;
pub mod tcp;

pub use http::{HttpHandler, http_request};
pub use tcp::{ExposeHandler, Tunnel, TunnelClient, connect_tunnel, splice};

/// Most tunnels open at once on one exposed service.
pub const MAX_TUNNELS: usize = 64;
/// Most console requests in flight at once on one exposed service.
pub const MAX_HTTP_INFLIGHT: usize = 64;
/// A tunnel with no traffic in either direction for this long is closed.
pub const TUNNEL_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Total time allowed for one console request against the local service.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Time allowed to reach the exposed service, or the local port for a tunnel.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Most handshakes one connection may have in progress at once.
pub const MAX_PENDING_PER_CONN: usize = 4;

/// Per-connection admission state: until one of its streams has proved the secret, a
/// connection that opens no stream within the hello deadline is closed, and no connection
/// holds more than [`MAX_PENDING_PER_CONN`] handshakes at once.
#[derive(Debug, Clone)]
pub(crate) struct ConnGate {
    authed: Arc<AtomicBool>,
    pending: Arc<Semaphore>,
}

impl ConnGate {
    pub(crate) fn new() -> Self {
        Self {
            authed: Arc::new(AtomicBool::new(false)),
            pending: Arc::new(Semaphore::new(MAX_PENDING_PER_CONN)),
        }
    }

    /// The next stream of `conn`; `None` once the connection ended or was closed because it
    /// stayed unauthenticated and idle for `idle`.
    pub(crate) async fn next_stream(
        &self,
        conn: &Connection,
        idle: Duration,
    ) -> Option<(SendStream, RecvStream)> {
        let peer = conn.remote_id();
        let next = if self.authed.load(Ordering::Acquire) {
            conn.accept_bi().await
        } else {
            match tokio::time::timeout(idle, conn.accept_bi()).await {
                Ok(next) => next,
                Err(_) if self.authed.load(Ordering::Acquire) => conn.accept_bi().await,
                Err(_) => {
                    tracing::debug!(%peer, "unauthenticated connection idle, closed");
                    conn.close(close_code::TIMEOUT.into(), b"no hello");
                    return None;
                }
            }
        };
        match next {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::debug!(%peer, "connection ended: {e}");
                None
            }
        }
    }

    /// A handshake slot of this connection, when one is free.
    pub(crate) fn slot(&self) -> Option<OwnedSemaphorePermit> {
        self.pending.clone().try_acquire_owned().ok()
    }

    /// A stream of this connection proved the secret.
    pub(crate) fn authenticated(&self) {
        self.authed.store(true, Ordering::Release);
    }
}

/// Decides whether a client may use the service; asked once per client id.
pub type Approver =
    Arc<dyn Fn(EndpointId) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

/// What happens on an exposed service, for logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExposeEvent {
    /// A tunnel opened.
    TunnelOpened {
        /// Client.
        peer: EndpointId,
    },
    /// A tunnel closed.
    TunnelClosed {
        /// Client.
        peer: EndpointId,
        /// Bytes from the client to the service.
        up: u64,
        /// Bytes from the service to the client.
        down: u64,
        /// Why it ended, when not cleanly.
        error: Option<String>,
    },
    /// A console request was answered.
    Http {
        /// Client.
        peer: EndpointId,
        /// Method.
        method: String,
        /// Path.
        path: String,
        /// Status sent back.
        status: u16,
        /// Response body size.
        bytes: u64,
    },
    /// A client was turned away.
    Refused {
        /// Client.
        peer: EndpointId,
        /// Why.
        reason: String,
    },
}

/// Failures of the host and client helpers.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    /// The network failed.
    #[error(transparent)]
    Net(#[from] NetError),
    /// A frame could not be read or written.
    #[error(transparent)]
    Frame(#[from] FrameError),
    /// The host refused the tunnel.
    #[error("refused by the host: {0}")]
    Refused(String),
    /// The host did not answer in time.
    #[error("the host did not answer in time")]
    Timeout,
    /// The connection broke.
    #[error("connection failed: {0}")]
    Connection(String),
    /// The host sent something unexpected.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// A local I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Settings of an exposed service.
#[derive(Clone)]
pub struct Expose {
    /// The loopback port served; the only target ever connected to.
    pub port: u16,
    /// Service secret carried by the link.
    pub secret: [u8; 16],
    /// Manual approval of each new client, when set.
    pub approve: Option<Approver>,
    /// Serve console requests on `nuee/http/0` too.
    pub http: bool,
    /// Admission gate settings.
    pub admit: AdmitConfig,
    /// Idle timeout of a tunnel.
    pub idle_timeout: Duration,
    /// Total timeout of a console request.
    pub http_timeout: Duration,
    /// Where to report what happens.
    pub events: Option<UnboundedSender<ExposeEvent>>,
}

impl std::fmt::Debug for Expose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Expose")
            .field("port", &self.port)
            .field("approve", &self.approve.is_some())
            .field("http", &self.http)
            .field("idle_timeout", &self.idle_timeout)
            .field("http_timeout", &self.http_timeout)
            .finish_non_exhaustive()
    }
}

impl Expose {
    /// Exposes `127.0.0.1:port` with this secret, HTTP console on, no approval.
    pub fn new(port: u16, secret: [u8; 16]) -> Self {
        Self {
            port,
            secret,
            approve: None,
            http: true,
            admit: AdmitConfig::default(),
            idle_timeout: TUNNEL_IDLE_TIMEOUT,
            http_timeout: HTTP_TIMEOUT,
            events: None,
        }
    }

    /// The `X` link of this service on `net` (relay left empty when there is none).
    pub fn link(&self, net: &Net) -> Result<ServiceLink, HostError> {
        let relay = match net.home_relay() {
            Some(url) => BoundedString::try_from(url.to_string())
                .map_err(|e| HostError::Protocol(format!("relay URL too long for a link: {e}")))?,
            None => BoundedString::new(),
        };
        Ok(ServiceLink {
            host: net.id(),
            relay,
            secret: self.secret,
        })
    }

    /// Starts serving on `net`; shutting the router down stops the service.
    pub fn spawn(self, net: &Net) -> Router {
        let shared = Shared::new(&self, net.id());
        let tcp = ExposeHandler::new(shared.clone());
        let router = net.router().accept(ALPN_TCP, tcp);
        let router = if self.http {
            router.accept(ALPN_HTTP, HttpHandler::new(shared))
        } else {
            router
        };
        router.spawn()
    }
}

/// State shared by both handlers of one service.
#[derive(Clone)]
pub(crate) struct Shared {
    pub(crate) cfg: Arc<Expose>,
    pub(crate) own: EndpointId,
    pub(crate) admission: Arc<Admission>,
    /// One decision per peer; the map lock is only held to find or create the entry.
    decisions: Arc<std::sync::Mutex<HashMap<EndpointId, Arc<tokio::sync::OnceCell<bool>>>>>,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared")
            .field("cfg", &self.cfg)
            .field("own", &self.own)
            .finish_non_exhaustive()
    }
}

impl Shared {
    fn new(cfg: &Expose, own: EndpointId) -> Self {
        Self {
            admission: Arc::new(Admission::new(cfg.admit)),
            cfg: Arc::new(cfg.clone()),
            own,
            decisions: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Whether `peer` may proceed; asks the approver once per peer.
    ///
    /// Concurrent callers for the same undecided peer wait on one shared prompt; a pending
    /// prompt never delays any other peer.
    pub(crate) async fn approved(&self, peer: EndpointId) -> bool {
        let Some(approve) = &self.cfg.approve else {
            return true;
        };
        let cell = {
            let mut decisions = self
                .decisions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            decisions.entry(peer).or_default().clone()
        };
        *cell.get_or_init(|| approve(peer)).await
    }

    pub(crate) fn report(&self, event: ExposeEvent) {
        if let Some(tx) = &self.cfg.events
            && tx.send(event).is_err()
        {
            tracing::debug!("event receiver gone");
        }
    }

    pub(crate) fn refused(&self, peer: EndpointId, reason: &str) {
        tracing::info!(%peer, "refused: {reason}");
        self.report(ExposeEvent::Refused {
            peer,
            reason: reason.to_owned(),
        });
    }

    pub(crate) fn target(&self) -> std::net::SocketAddr {
        std::net::SocketAddr::from(([127, 0, 0, 1], self.cfg.port))
    }
}

/// Records how to reach the host of `link` (its relay, when the link names one).
pub(crate) fn learn_host(net: &Net, link: &ServiceLink) -> Result<(), HostError> {
    if !link.relay.as_str().is_empty() {
        net.add_peer_relay_str(link.host, link.relay.as_str())?;
    }
    Ok(())
}
