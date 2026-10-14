//! `nuee relay`: run an iroh relay, plain HTTP for local use or HTTPS with Let's Encrypt.

use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use iroh_relay::server::{
    Access, AccessControl, AcmeConfig, CertConfig, ClientRateLimit, ClientRequest, ConnectionId,
    QuicConfig, RelayConfig, Server, ServerConfig, TlsConfig,
};
use nuee_proto::{EndpointId, Hash};
use serde_json::json;

use crate::{Ctx, exit, fail};

/// Arguments of `nuee relay`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Plain-HTTP relay for local use and tests (no TLS, no ACME)
    #[arg(long)]
    pub dev: bool,
    /// Address of the dev relay (port 0 picks a free port)
    #[arg(
        long,
        value_name = "ADDR",
        default_value = "127.0.0.1:3340",
        requires = "dev"
    )]
    pub bind: SocketAddr,
    /// Public host name of the relay (HTTPS through Let's Encrypt)
    #[arg(
        long,
        value_name = "FQDN",
        required_unless_present = "dev",
        conflicts_with = "dev"
    )]
    pub hostname: Option<String>,
    /// Contact email for Let's Encrypt
    #[arg(
        long,
        value_name = "EMAIL",
        required_unless_present = "dev",
        conflicts_with = "dev"
    )]
    pub acme_contact: Option<String>,
    /// Where certificates are cached (default: <data dir>/acme)
    #[arg(long, value_name = "DIR")]
    pub acme_cache: Option<PathBuf>,
    /// File holding the bearer token clients must present
    #[arg(long, value_name = "PATH")]
    pub token_file: Option<PathBuf>,
    /// Most clients connected at once
    #[arg(long, value_name = "N")]
    pub max_conns: Option<usize>,
    /// Per-client incoming rate limit, in bytes per second
    #[arg(long, value_name = "BYTES_PER_S")]
    pub rate: Option<NonZeroU32>,
}

/// Admits clients holding the token, up to a number of concurrent connections.
#[derive(Debug)]
struct Gate {
    token: Option<Hash>,
    /// Endpoints of this machine that use the relay without the token.
    exempt: Vec<EndpointId>,
    max_conns: Option<usize>,
    conns: AtomicUsize,
}

impl Gate {
    fn token_ok(&self, request: &ClientRequest) -> bool {
        let Some(want) = &self.token else {
            return true;
        };
        if self.exempt.contains(&request.endpoint_id()) {
            return true;
        }
        let Some(got) = request.auth_token() else {
            return false;
        };
        // Compare digests, which have a fixed length, without an early exit.
        let got = Hash::of(got.as_bytes());
        want.as_bytes()
            .iter()
            .zip(got.as_bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }

    fn take_slot(&self) -> bool {
        let Some(max) = self.max_conns else {
            self.conns.fetch_add(1, Ordering::AcqRel);
            return true;
        };
        self.conns
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < max).then_some(n + 1)
            })
            .is_ok()
    }
}

impl AccessControl for Gate {
    async fn on_connect(&self, request: &ClientRequest) -> Access {
        if !self.token_ok(request) {
            return Access::Deny {
                reason: Some("missing or wrong token".into()),
            };
        }
        if !self.take_slot() {
            return Access::Deny {
                reason: Some("relay full".into()),
            };
        }
        Access::Allow
    }

    fn on_disconnect(&self, _endpoint_id: EndpointId, _connection_id: ConnectionId) {
        // Every admitted connection took a slot.
        if self
            .conns
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
            .is_err()
        {
            tracing::warn!("connection count already at zero");
        }
    }
}

fn read_token(path: &PathBuf) -> anyhow::Result<Hash> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| fail(exit::ERROR, format!("cannot read {}: {e}", path.display())))?;
    let token = text.trim();
    if token.is_empty() {
        return Err(fail(
            exit::USAGE,
            format!("{} holds no token", path.display()),
        ));
    }
    Ok(Hash::of(token.as_bytes()))
}

/// Relay settings shared by `nuee relay` and `nuee serve --anchor`.
pub(crate) struct RelaySetup {
    /// Plain-HTTP relay on `bind`.
    pub dev: bool,
    /// Address of the dev relay.
    pub bind: SocketAddr,
    /// Public host name (HTTPS through Let's Encrypt).
    pub hostname: Option<String>,
    /// Let's Encrypt contact.
    pub acme_contact: Option<String>,
    /// Certificate cache.
    pub acme_cache: Option<PathBuf>,
    /// Bearer token file.
    pub token_file: Option<PathBuf>,
    /// Connection cap.
    pub max_conns: Option<usize>,
    /// Per-client incoming rate limit.
    pub rate: Option<NonZeroU32>,
    /// Endpoints admitted without the token (the anchor's own).
    pub exempt: Vec<EndpointId>,
}

/// Starts a relay server; returns it with its public URL.
pub(crate) async fn start(setup: RelaySetup, ctx: &Ctx) -> anyhow::Result<(Server, String)> {
    let args = setup;
    let gate = Gate {
        token: args.token_file.as_ref().map(read_token).transpose()?,
        exempt: args.exempt.clone(),
        max_conns: args.max_conns,
        conns: AtomicUsize::new(0),
    };
    let (http_addr, tls, quic) = if args.dev {
        (args.bind, None, None)
    } else {
        let (Some(host), Some(contact)) = (&args.hostname, &args.acme_contact) else {
            return Err(fail(
                exit::USAGE,
                "pass --dev, or --hostname and --acme-contact",
            ));
        };
        let cache = match &args.acme_cache {
            Some(d) => d.clone(),
            None => ctx.data_dir()?.join("acme"),
        };
        let contact = if contact.starts_with("mailto:") {
            contact.clone()
        } else {
            format!("mailto:{contact}")
        };
        let acme = AcmeConfig::letsencrypt(true)
            .domains(vec![host.clone()])
            .contact(vec![contact])
            .cache_path(cache);
        let builder = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| anyhow::anyhow!("TLS setup: {e}"))?
        .with_no_client_auth();
        let cert = CertConfig::LetsEncrypt {
            acme_config: acme,
            server_config_builder: builder,
        };
        let tls = TlsConfig::new(SocketAddr::from(([0, 0, 0, 0], 443)), cert);
        let quic = QuicConfig::new(SocketAddr::from((
            [0, 0, 0, 0],
            iroh_relay::defaults::DEFAULT_RELAY_QUIC_PORT,
        )));
        (SocketAddr::from(([0, 0, 0, 0], 80)), Some(tls), Some(quic))
    };
    let mut relay = RelayConfig::new(http_addr);
    relay.tls = tls;
    relay.access = Arc::new(gate);
    if let Some(rate) = args.rate {
        relay.limits.client_rx = Some(ClientRateLimit::new(rate));
    }
    let mut cfg = ServerConfig::default();
    cfg.relay = Some(relay);
    cfg.quic = quic;
    let server = Server::spawn(cfg)
        .await
        .map_err(|e| fail(exit::NETWORK, format!("cannot start the relay: {e}")))?;

    let url = if args.dev {
        server.http_addr().map(|addr| format!("http://{addr}"))
    } else {
        args.hostname.as_ref().map(|h| format!("https://{h}"))
    }
    .ok_or_else(|| anyhow::anyhow!("the relay has no address"))?;
    Ok((server, url))
}

/// Runs the relay until Ctrl-C.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let setup = RelaySetup {
        dev: args.dev,
        bind: args.bind,
        hostname: args.hostname,
        acme_contact: args.acme_contact,
        acme_cache: args.acme_cache,
        token_file: args.token_file,
        max_conns: args.max_conns,
        rate: args.rate,
        exempt: Vec::new(),
    };
    let (server, url) = start(setup, ctx).await?;
    ctx.ui.emit("relay", json!({ "url": url }));
    if !ctx.ui.is_json() {
        ctx.ui.say(&format!("relay ready: {url}"));
    }
    crate::ctrl_c().await;
    server
        .shutdown()
        .await
        .map_err(|e| anyhow::anyhow!("relay shutdown: {e}"))?;
    Ok(ExitCode::SUCCESS)
}
