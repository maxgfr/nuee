//! `nuee expose`: serve a local TCP port (and HTTP console requests to it) through a link.

use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::sync::Arc;

use nuee_host::{Approver, Expose, ExposeEvent};
use nuee_proto::{EndpointId, Link, fingerprint_string};
use serde_json::json;

use crate::{Ctx, exit, fail};

/// Arguments of `nuee expose`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Local port to expose (only 127.0.0.1:<port> is ever reached)
    pub port: u16,
    /// Ask before letting each new client in
    #[arg(long)]
    pub approve: bool,
    /// Do not serve HTTP console requests, only raw TCP tunnels
    #[arg(long)]
    pub no_http: bool,
}

/// Asks on the terminal whether `peer` may connect.
fn ask(peer: EndpointId) -> bool {
    let question = format!(
        "client {} ({}) wants to connect. Allow? [y/N] ",
        peer.fmt_short(),
        fingerprint_string(&peer)
    );
    let mut err = std::io::stderr().lock();
    if write!(err, "{question}")
        .and_then(|()| err.flush())
        .is_err()
    {
        return false;
    }
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(_) => matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        Err(e) => {
            tracing::warn!("cannot read the answer: {e}");
            false
        }
    }
}

fn approver() -> Approver {
    Arc::new(|peer| {
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || ask(peer)).await {
                Ok(yes) => yes,
                Err(e) => {
                    tracing::warn!("approval prompt failed: {e}");
                    false
                }
            }
        })
    })
}

/// Exposes the port until Ctrl-C.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    if args.port == 0 {
        return Err(fail(exit::USAGE, "the port must be between 1 and 65535"));
    }
    if args.approve && !std::io::stdin().is_terminal() {
        return Err(fail(exit::USAGE, "--approve needs an interactive terminal"));
    }
    if let Err(e) = tokio::net::TcpStream::connect(("127.0.0.1", args.port)).await {
        ctx.ui.note(&format!(
            "nothing answers on 127.0.0.1:{} yet ({e}); clients will be refused until it does",
            args.port
        ));
    }
    let secret = nuee_proto::new_secret16().map_err(|e| fail(exit::ERROR, e.to_string()))?;
    let net = ctx.online_net().await?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut cfg = Expose::new(args.port, secret);
    cfg.http = !args.no_http;
    cfg.events = Some(tx);
    if args.approve {
        cfg.approve = Some(approver());
    }
    let link = cfg
        .link(&net)
        .map_err(|e| fail(exit::NETWORK, e.to_string()))?;
    let router = cfg.spawn(&net);
    let url = Link::Service(link).to_url(&ctx.global.web_base);
    ctx.ui.show_link("service", &url, Some(&net.id()), None)?;
    ctx.ui.emit(
        "serving",
        json!({ "port": args.port, "http": !args.no_http }),
    );
    ctx.ui
        .note(&format!("exposing 127.0.0.1:{} until Ctrl-C", args.port));
    let stop = crate::ctrl_c();
    tokio::pin!(stop);
    loop {
        tokio::select! {
            () = &mut stop => break,
            ev = rx.recv() => match ev {
                Some(ev) => log(ctx, &ev),
                None => break,
            },
        }
    }
    if let Err(e) = router.shutdown().await {
        tracing::debug!("router shutdown: {e}");
    }
    net.close().await;
    ctx.ui.emit("stopped", json!({}));
    Ok(ExitCode::SUCCESS)
}

fn log(ctx: &Ctx, ev: &ExposeEvent) {
    match ev {
        ExposeEvent::TunnelOpened { peer } => {
            ctx.ui
                .emit("tunnel_open", json!({ "peer": peer.to_string() }));
            ctx.ui.note(&format!(
                "tunnel opened by {} ({})",
                peer.fmt_short(),
                fingerprint_string(peer)
            ));
        }
        ExposeEvent::TunnelClosed {
            peer,
            up,
            down,
            error,
        } => {
            ctx.ui.emit(
                "tunnel_closed",
                json!({ "peer": peer.to_string(), "up": up, "down": down, "error": error }),
            );
            let why = error
                .as_deref()
                .map(|e| format!(" ({e})"))
                .unwrap_or_default();
            ctx.ui.note(&format!(
                "tunnel of {} closed: {up} bytes in, {down} bytes out{why}",
                peer.fmt_short()
            ));
        }
        ExposeEvent::Http {
            peer,
            method,
            path,
            status,
            bytes,
        } => {
            ctx.ui.emit(
                "http",
                json!({ "peer": peer.to_string(), "method": method, "path": path,
                        "status": status, "bytes": bytes }),
            );
            ctx.ui.note(&format!(
                "{} {method} {path} -> {status} ({bytes} bytes)",
                peer.fmt_short()
            ));
        }
        ExposeEvent::Refused { peer, reason } => {
            ctx.ui.emit(
                "refused",
                json!({ "peer": peer.to_string(), "reason": reason }),
            );
            ctx.ui
                .note(&format!("refused {}: {reason}", peer.fmt_short()));
        }
    }
}
