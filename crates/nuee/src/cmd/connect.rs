//! `nuee connect`: reach an exposed service through a local port on 127.0.0.1.

use std::process::ExitCode;

use nuee_host::{HostError, TUNNEL_IDLE_TIMEOUT, TunnelClient, splice};
use nuee_proto::Link;
use serde_json::json;
use tokio::net::TcpListener;

use crate::{Ctx, exit, fail};

/// Arguments of `nuee connect`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// The service link, or `-` to read it from stdin
    pub link: String,
    /// Local port to listen on (default: a free one)
    #[arg(long, value_name = "PORT")]
    pub port: Option<u16>,
}

fn host_failure(e: HostError) -> anyhow::Error {
    match e {
        HostError::Refused(why) => fail(exit::REFUSED, format!("refused by the host: {why}")),
        other => fail(
            exit::NETWORK,
            format!("cannot reach the service ({other}): is it still exposed?"),
        ),
    }
}

/// Listens locally and tunnels every connection until Ctrl-C.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let text = crate::link_arg(&args.link)?;
    let link = match Link::parse(&text).map_err(crate::link_failure)? {
        Link::Service(l) => l,
        Link::Job(_) => return Err(fail(exit::USAGE, "this is a swarm link: use `nuee join`")),
        Link::File(_) => return Err(fail(exit::USAGE, "this is a file link: use `nuee get`")),
        Link::Site(_) => {
            return Err(fail(
                exit::USAGE,
                "this is a site link: use `nuee site get`",
            ));
        }
    };
    let listener = TcpListener::bind(("127.0.0.1", args.port.unwrap_or(0)))
        .await
        .map_err(|e| fail(exit::ERROR, format!("cannot listen on 127.0.0.1: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| fail(exit::ERROR, e.to_string()))?
        .port();
    let net = ctx.online_net().await?;
    let client = TunnelClient::connect(&net, &link)
        .await
        .map_err(host_failure)?;
    // One tunnel up front: a wrong link or a refusal shows now, not at the first request.
    let probe = client.open().await.map_err(host_failure)?;
    drop(probe);
    ctx.ui.emit("listening", json!({ "port": port }));
    ctx.ui.say(&format!("open http://localhost:{port}"));
    ctx.ui
        .note("tunnelling to the exposed service until Ctrl-C");
    let stop = crate::ctrl_c();
    tokio::pin!(stop);
    loop {
        tokio::select! {
            () = &mut stop => break,
            accepted = listener.accept() => {
                let (stream, from) = match accepted {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::warn!("accept failed: {e}");
                        continue;
                    }
                };
                let client = client.clone();
                tokio::spawn(async move {
                    match client.open().await {
                        Ok(tunnel) => {
                            let (up, down, err) = splice(stream, tunnel, TUNNEL_IDLE_TIMEOUT).await;
                            tracing::info!(%from, up, down, error = ?err, "tunnel closed");
                        }
                        Err(e) => tracing::warn!(%from, "cannot open a tunnel: {e}"),
                    }
                });
            }
        }
    }
    client.close();
    net.close().await;
    ctx.ui.emit("stopped", json!({}));
    Ok(ExitCode::SUCCESS)
}
