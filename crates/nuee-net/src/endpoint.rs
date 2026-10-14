//! iroh endpoint setup: explicit relays, peer addresses from links only (no pkarr, no DNS).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use iroh::address_lookup::memory::MemoryLookup;
use iroh::endpoint::{Connection, presets};
use iroh::protocol::RouterBuilder;
use iroh::{Endpoint, EndpointAddr, RelayMode, RelayUrl, TransportAddr};
use nuee_proto::{EndpointId, SecretKey};

/// Default time allowed to reach the home relay.
pub const ONLINE_TIMEOUT: Duration = Duration::from_secs(10);

/// Which relay servers the endpoint uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayChoice {
    /// The public n0 relays.
    N0,
    /// Self-hosted relays.
    Custom(Vec<RelayUrl>),
    /// No relay: direct addresses only (tests, LAN).
    Disabled,
}

/// Endpoint settings.
#[derive(Debug, Clone)]
pub struct NetConfig {
    /// Identity of this endpoint.
    pub secret: SecretKey,
    /// Relay servers.
    pub relay: RelayChoice,
    /// Only talk through the relay (no direct UDP). Ignored in browsers, which are relay-only.
    pub relay_only: bool,
    /// ALPNs accepted on incoming connections. A `Router` replaces them with its own list.
    pub alpns: Vec<Vec<u8>>,
}

impl NetConfig {
    /// Settings with the n0 relays, direct paths allowed and no ALPN.
    pub fn new(secret: SecretKey) -> Self {
        Self {
            secret,
            relay: RelayChoice::N0,
            relay_only: false,
            alpns: Vec::new(),
        }
    }
}

/// Network failures, mapped to CLI exit codes by the binaries.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    /// The endpoint could not be created.
    #[error("cannot start the network endpoint: {0}")]
    Bind(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The home relay did not answer in time.
    #[error("relay unreachable")]
    RelayUnreachable,
    /// A connection could not be opened.
    #[error("cannot connect to {peer}: {source}")]
    Connect {
        /// Remote peer.
        peer: EndpointId,
        /// Cause.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// A relay URL is malformed.
    #[error("invalid relay URL {0:?}")]
    BadRelay(String),
    /// A relay URL announced by a remote peer points at a host we do not dial on a peer's say-so.
    #[error("refused peer relay URL {0:?}")]
    RelayRefused(String),
}

/// A bound endpoint plus the in-memory address book fed from links.
#[derive(Debug, Clone)]
pub struct Net {
    endpoint: Endpoint,
    lookup: MemoryLookup,
    relay: RelayChoice,
}

impl Net {
    /// Binds an endpoint with the `Minimal` preset, the chosen relays and a [`MemoryLookup`].
    pub async fn bind(cfg: NetConfig) -> Result<Net, NetError> {
        let lookup = MemoryLookup::new();
        let mode = match &cfg.relay {
            RelayChoice::N0 => RelayMode::Default,
            RelayChoice::Custom(urls) => RelayMode::custom(urls.iter().cloned()),
            RelayChoice::Disabled => RelayMode::Disabled,
        };
        let builder = Endpoint::builder(presets::Minimal)
            .secret_key(cfg.secret)
            .relay_mode(mode)
            .address_lookup(lookup.clone())
            .alpns(cfg.alpns);
        let builder = relay_only(builder, cfg.relay_only);
        let endpoint = builder
            .bind()
            .await
            .map_err(|e| NetError::Bind(Box::new(e)))?;
        Ok(Net {
            endpoint,
            lookup,
            relay: cfg.relay,
        })
    }

    /// Waits until the home relay accepted us; returns at once when relays are disabled.
    pub async fn online(&self, timeout: Duration) -> Result<(), NetError> {
        if self.relay == RelayChoice::Disabled {
            return Ok(());
        }
        n0_future::time::timeout(timeout, self.endpoint.online())
            .await
            .map_err(|_| NetError::RelayUnreachable)
    }

    /// This endpoint's identity.
    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// The relay this endpoint is reachable through, once known.
    pub fn home_relay(&self) -> Option<RelayUrl> {
        self.endpoint.addr().relay_urls().next().cloned()
    }

    /// The full current address (relay and direct addresses).
    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    /// Records how to reach `id`.
    pub fn add_peer(&self, id: EndpointId, relay: Option<RelayUrl>, direct: Vec<SocketAddr>) {
        let addrs = relay
            .into_iter()
            .map(TransportAddr::Relay)
            .chain(direct.into_iter().map(TransportAddr::Ip));
        self.lookup
            .add_endpoint_info(EndpointAddr::from_parts(id, addrs));
    }

    /// Records a peer from a relay URL string the local user chose to trust: one carried by a
    /// link they opened (a local `http://127.0.0.1` dev relay included). A relay supplied by
    /// another peer goes through [`Net::add_announced_relay_str`] instead.
    pub fn add_peer_relay_str(&self, id: EndpointId, relay: &str) -> Result<(), NetError> {
        let url: RelayUrl = relay
            .parse()
            .map_err(|_| NetError::BadRelay(relay.to_owned()))?;
        self.add_peer(id, Some(url), Vec::new());
        Ok(())
    }

    /// Records a peer from a relay URL string that a remote peer supplied (its own announced
    /// relay, or a provider it names), only if [`Net::peer_relay_allowed`] accepts it.
    pub fn add_announced_relay_str(&self, id: EndpointId, relay: &str) -> Result<(), NetError> {
        let url: RelayUrl = relay
            .parse()
            .map_err(|_| NetError::BadRelay(relay.to_owned()))?;
        if !self.peer_relay_allowed(&url) {
            return Err(NetError::RelayRefused(relay.to_owned()));
        }
        self.add_peer(id, Some(url), Vec::new());
        Ok(())
    }

    /// Whether a relay URL announced by a remote peer may be dialled: one of our own relays,
    /// or an `https` relay whose host is not `localhost` nor an IP literal in a private,
    /// loopback, link-local, shared or unspecified range (no requests into the operator's
    /// network on a peer's say-so).
    pub fn peer_relay_allowed(&self, url: &RelayUrl) -> bool {
        let own = match &self.relay {
            RelayChoice::Custom(urls) => urls.contains(url),
            RelayChoice::N0 | RelayChoice::Disabled => false,
        };
        own || self.home_relay().as_ref() == Some(url) || public_https_relay(url)
    }

    /// What the address book knows about `id`.
    pub fn peer_addr(&self, id: EndpointId) -> Option<EndpointAddr> {
        self.lookup
            .get_endpoint_info(id)
            .map(|info| EndpointAddr::from_parts(id, info.data.addrs().cloned()))
    }

    /// Opens a connection to `id` on `alpn`, using the address book.
    pub async fn connect(&self, id: EndpointId, alpn: &[u8]) -> Result<Connection, NetError> {
        self.endpoint
            .connect(EndpointAddr::new(id), alpn)
            .await
            .map_err(|e| NetError::Connect {
                peer: id,
                source: Box::new(e),
            })
    }

    /// The underlying endpoint.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// A router builder on this endpoint, to register protocol handlers.
    pub fn router(&self) -> RouterBuilder {
        iroh::protocol::Router::builder(self.endpoint.clone())
    }

    /// Closes every connection and the endpoint.
    pub async fn close(&self) {
        self.endpoint.close().await;
    }
}

/// An `https` URL whose host is a DNS name other than `localhost`, or a public IP literal.
pub fn public_https_relay(url: &RelayUrl) -> bool {
    if url.scheme() != "https" {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    match bare.parse::<IpAddr>() {
        Ok(ip) => public_ip(ip),
        Err(_) => {
            let name = bare.trim_end_matches('.').to_ascii_lowercase();
            !name.is_empty() && name != "localhost" && !name.ends_with(".localhost")
        }
    }
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return public_v4(v4);
            }
            let seg0 = v6.segments().first().copied().unwrap_or(0);
            !(v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                || (seg0 & 0xfe00) == 0xfc00
                || (seg0 & 0xffc0) == 0xfe80)
        }
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || a == 0
        || (a == 100 && (64..128).contains(&b)))
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn relay_only(builder: iroh::endpoint::Builder, on: bool) -> iroh::endpoint::Builder {
    if on {
        builder.clear_ip_transports()
    } else {
        builder
    }
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
fn relay_only(builder: iroh::endpoint::Builder, _on: bool) -> iroh::endpoint::Builder {
    builder
}

#[cfg(test)]
mod tests {
    use super::public_https_relay;

    fn ok(s: &str) -> bool {
        public_https_relay(&s.parse().expect("url"))
    }

    #[test]
    fn peer_relays_must_be_public_https() {
        assert!(ok("https://relay.example.com/"));
        assert!(ok("https://8.8.8.8/"));
        for bad in [
            "http://relay.example.com/",
            "https://localhost/",
            "https://LOCALHOST./",
            "https://127.0.0.1/",
            "https://10.1.2.3/",
            "https://172.16.0.1/",
            "https://192.168.1.1/",
            "https://169.254.169.254/",
            "https://100.64.0.1/",
            "https://0.0.0.0/",
            "https://[::1]/",
            "https://[fe80::1]/",
            "https://[fd00::1]/",
            "https://[::ffff:10.0.0.1]/",
        ] {
            assert!(!ok(bad), "{bad}");
        }
    }
}
