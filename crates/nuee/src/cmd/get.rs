//! `nuee get`: download, verify, decrypt and unpack a shared link.

use std::path::PathBuf;
use std::process::ExitCode;

use nuee_proto::Link;
use serde_json::json;

use crate::{Ctx, exit, fail};

/// Arguments of `nuee get`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// The file link, or `-` to read it from stdin
    pub link: String,
    /// Where to unpack (default: the current directory)
    #[arg(short, long, value_name = "DIR")]
    pub out: Option<PathBuf>,
    /// Overwrite existing files
    #[arg(long)]
    pub force: bool,
    /// Swarm link whose coordinator keeps copies: asked for fresh holders when the link's
    /// providers fail
    #[arg(long, value_name = "LINK")]
    pub on: Option<String>,
}

/// Downloads a share.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let text = crate::link_arg(&args.link)?;
    let link = match Link::parse(&text).map_err(crate::link_failure)? {
        Link::File(l) | Link::Site(l) => l,
        Link::Job(_) => {
            return Err(fail(
                exit::USAGE,
                "this is a swarm link: use `nuee join` to take part",
            ));
        }
        Link::Service(_) => {
            return Err(fail(
                exit::USAGE,
                "this is a service link: use `nuee connect`",
            ));
        }
    };
    let swarm = match &args.on {
        Some(on) => match Link::parse(&crate::link_arg(on)?).map_err(crate::link_failure)? {
            Link::Job(job) => Some(job),
            _ => return Err(fail(exit::USAGE, "--on expects a swarm link")),
        },
        None => None,
    };
    let dest = args.out.clone().unwrap_or_else(|| PathBuf::from("."));
    let net = ctx.online_net().await?;
    let hub = ctx.hub(&net).await?;
    let bar = ctx.ui.bar(link.size, "bytes");
    let res = nuee_store::durable::get_durable(
        &net,
        &hub,
        &link,
        swarm.as_ref(),
        &dest,
        args.force,
        |done, _total| {
            bar.set_position(done);
        },
    )
    .await;
    bar.finish_and_clear();
    net.close().await;
    let (report, asked) = res.map_err(crate::store_failure)?;
    if asked {
        ctx.ui
            .note("the link's providers failed; fetched from the holders the swarm knows");
    }
    for f in &report.files {
        ctx.ui
            .emit("file", json!({ "path": f.display().to_string() }));
        ctx.ui.say(&crate::ui::clean(&f.display().to_string()));
    }
    ctx.ui.emit(
        "verified",
        json!({ "hash": link.hash.to_string(), "files": report.files.len(), "bytes": report.bytes }),
    );
    if !ctx.ui.is_json() {
        let mark = ctx.ui.paint(crate::ui::OK, "32");
        ctx.ui.say(&format!("{mark} verified (BLAKE3)"));
    }
    Ok(ExitCode::SUCCESS)
}
