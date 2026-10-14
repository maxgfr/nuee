//! `nuee share`: share files and folders, encrypted, until Ctrl-C.

use std::path::PathBuf;
use std::process::ExitCode;

use nuee_proto::Link;
use serde_json::json;

use crate::{Ctx, exit, fail};

/// How long `share --on` waits for the swarm to place the copies before printing the link.
const PLACEMENT_WAIT: std::time::Duration = std::time::Duration::from_secs(600);

/// Arguments of `nuee share`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Files or folders to share
    #[arg(required = true)]
    pub paths: Vec<PathBuf>,
    /// Ask the store members of the swarm given with --on to keep this many copies
    #[arg(long, value_name = "N", requires = "on", value_parser = clap::value_parser!(u8).range(1..=8))]
    pub replicas: Option<u8>,
    /// Ask the swarm given with --on to keep it as a Reed-Solomon set of k data + m parity
    /// shards (e.g. 4+2), one shard per store member; prints a second link to the set
    #[arg(long, value_name = "K+M", requires = "on", conflicts_with = "replicas")]
    pub erasure: Option<String>,
    /// Swarm link whose coordinator keeps the copies (`-` reads it from stdin)
    #[arg(long, value_name = "LINK")]
    pub on: Option<String>,
}

/// Packs, encrypts and serves the paths; prints the link.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    for p in &args.paths {
        if !p.exists() {
            return Err(fail(exit::USAGE, format!("{} does not exist", p.display())));
        }
    }
    let scheme = match &args.erasure {
        Some(s) => Some(
            nuee_store::erasure::parse_scheme(s).map_err(|e| fail(exit::USAGE, e.to_string()))?,
        ),
        None => None,
    };
    let swarm = match &args.on {
        Some(on) => match Link::parse(&crate::link_arg(on)?).map_err(crate::link_failure)? {
            Link::Job(job) => Some(job),
            _ => {
                return Err(fail(
                    exit::USAGE,
                    "--on expects a swarm link (from `nuee run`)",
                ));
            }
        },
        None => None,
    };
    let net = ctx.online_net().await?;
    let hub = ctx.hub(&net).await?;
    let share = nuee_store::share_paths(&net, &hub, &args.paths)
        .await
        .map_err(crate::store_failure)?;
    for skipped in &share.skipped {
        ctx.ui.note(&format!(
            "skipped {} (links and special files are not shared)",
            skipped.display()
        ));
    }
    let url = Link::File(share.link.clone()).to_url(&ctx.global.web_base);
    ctx.ui.show_link("file", &url, None, None)?;
    ctx.ui.emit(
        "serving",
        json!({ "hash": share.link.hash.to_string(), "size": share.link.size }),
    );
    if let Some(job) = &swarm {
        let (kept, keep) = match scheme {
            Some((k, m)) => {
                let access =
                    nuee_proto::new_secret16().map_err(|e| fail(exit::ERROR, e.to_string()))?;
                let offer = nuee_store::erasure::offer_set(
                    &hub,
                    &share.serve.acceptor,
                    share.link.hash,
                    access,
                    k,
                    m,
                )
                .await
                .map_err(crate::store_failure)?;
                let mut set_link = share.link.clone();
                set_link.hash = offer.id;
                set_link.size = offer.bytes.len() as u64;
                set_link.access = access;
                ctx.ui.emit(
                    "erasure",
                    json!({ "set": offer.id.to_string(), "k": k, "m": m,
                            "coordinator": job.coordinator.to_string() }),
                );
                (set_link, nuee_store::durable::Keep::Set(offer.bytes))
            }
            None => (
                share.link.clone(),
                nuee_store::durable::Keep::Replicas(args.replicas.unwrap_or(1)),
            ),
        };
        ctx.ui
            .note("waiting for the swarm to place the copies (Ctrl-C skips the wait)");
        let me = nuee_store::self_provider(&net);
        let keeping = nuee_store::durable::keep_durably(
            &net,
            job,
            &kept,
            keep,
            PLACEMENT_WAIT,
            Some(me),
            |elapsed| {
                let s = elapsed.as_secs();
                if s > 0 && s % 10 == 0 {
                    ctx.ui.emit("placing", json!({ "seconds": s }));
                    ctx.ui.note(&format!("still placing after {s} s"));
                }
            },
        );
        let skip = crate::ctrl_c();
        let outcome = tokio::select! {
            r = keeping => Some(r.map_err(crate::store_failure)?),
            () = skip => None,
        };
        match outcome {
            None => ctx.ui.note("stopped waiting; still serving from this node"),
            Some((nuee_store::durable::Placement::Refused(reason), _)) => {
                ctx.ui.emit(
                    "replicas_refused",
                    json!({ "reason": format!("{reason:?}") }),
                );
                ctx.ui
                    .note(&format!("the swarm refused to keep copies: {reason:?}"));
            }
            Some((placement, durable)) => {
                let placed = matches!(placement, nuee_store::durable::Placement::Placed(_));
                let holders: Vec<String> = placement
                    .holders()
                    .iter()
                    .map(|p| p.id.to_string())
                    .collect();
                ctx.ui.emit(
                    "durable",
                    json!({ "hash": durable.hash.to_string(), "placed": placed,
                            "holders": holders }),
                );
                if !placed {
                    ctx.ui
                        .note("placement not complete yet; the link names the holders so far");
                }
                let url = Link::File(durable).to_url(&ctx.global.web_base);
                ctx.ui.show_link("durable", &url, None, None)?;
            }
        }
    }
    ctx.ui.note(&format!(
        "serving {} bytes (encrypted) until Ctrl-C",
        share.link.size
    ));
    crate::ctrl_c().await;
    share.shutdown().await.map_err(crate::store_failure)?;
    ctx.ui.emit("stopped", json!({}));
    Ok(ExitCode::SUCCESS)
}
