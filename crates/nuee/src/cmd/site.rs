//! `nuee site`: publish a static site folder (tar + age, `#/S/` link) or fetch one.

use std::path::PathBuf;
use std::process::ExitCode;

use nuee_proto::Link;
use serde_json::json;

use crate::{Ctx, exit, fail};

/// Arguments of `nuee site`.
#[derive(Debug, clap::Args)]
pub struct Args {
    #[command(subcommand)]
    pub action: Action,
}

/// What to do with a site.
#[derive(Debug, clap::Subcommand)]
pub enum Action {
    /// Publish a folder as a static site until Ctrl-C
    Publish {
        /// The site folder (with an index.html)
        dir: PathBuf,
        /// Hand the site to this anchor's gateway (its link, or `-`) and return; the anchor
        /// gets the decryption key and serves the plaintext
        #[arg(long, value_name = "ANCHOR_LINK")]
        on: Option<String>,
    },
    /// Download a published site
    Get {
        /// The site link, or `-` to read it from stdin
        link: String,
        /// Where to unpack (default: the current directory)
        #[arg(short, long, value_name = "DIR")]
        out: Option<PathBuf>,
        /// Overwrite existing files
        #[arg(long)]
        force: bool,
    },
}

/// Runs the action.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    match args.action {
        Action::Publish { dir, on } => publish(dir, on.as_deref(), ctx).await,
        Action::Get { link, out, force } => get(&link, out, force, ctx).await,
    }
}

async fn publish(dir: PathBuf, on: Option<&str>, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    if !dir.is_dir() {
        return Err(fail(
            exit::USAGE,
            format!("{} is not a folder", dir.display()),
        ));
    }
    if !dir.join("index.html").is_file() {
        ctx.ui
            .note("no index.html in the folder: the preview will list files only");
    }
    let anchor = on.map(crate::cmd::serve::anchor_link).transpose()?;
    let net = match &anchor {
        Some(link) => crate::cmd::serve::client_net(ctx, link).await?,
        None => ctx.online_net().await?,
    };
    let hub = ctx.hub(&net).await?;
    let share = nuee_store::share_paths(&net, &hub, std::slice::from_ref(&dir))
        .await
        .map_err(crate::store_failure)?;
    for skipped in &share.skipped {
        ctx.ui.note(&format!(
            "skipped {} (links and special files are not published)",
            skipped.display()
        ));
    }
    let url = Link::Site(share.link.clone()).to_url(&ctx.global.web_base);
    if let Some(link) = &anchor {
        let res = crate::cmd::serve::publish_on_anchor(ctx, &net, link, &url).await;
        share.shutdown().await.map_err(crate::store_failure)?;
        let path = res?;
        ctx.ui
            .say(&format!("served by the anchor's gateway at {path}"));
        return Ok(ExitCode::SUCCESS);
    }
    ctx.ui.show_link("site", &url, None, None)?;
    ctx.ui.emit(
        "serving",
        json!({ "hash": share.link.hash.to_string(), "size": share.link.size }),
    );
    ctx.ui.note(&format!(
        "publishing {} bytes (encrypted) until Ctrl-C",
        share.link.size
    ));
    crate::ctrl_c().await;
    share.shutdown().await.map_err(crate::store_failure)?;
    ctx.ui.emit("stopped", json!({}));
    Ok(ExitCode::SUCCESS)
}

async fn get(link: &str, out: Option<PathBuf>, force: bool, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let text = crate::link_arg(link)?;
    let link = match Link::parse(&text).map_err(crate::link_failure)? {
        Link::Site(l) => l,
        Link::File(_) => return Err(fail(exit::USAGE, "this is a file link: use `nuee get`")),
        Link::Job(_) => return Err(fail(exit::USAGE, "this is a swarm link: use `nuee join`")),
        Link::Service(_) => {
            return Err(fail(
                exit::USAGE,
                "this is a service link: use `nuee connect`",
            ));
        }
    };
    let dest = out.unwrap_or_else(|| PathBuf::from("."));
    let net = ctx.online_net().await?;
    let hub = ctx.hub(&net).await?;
    let bar = ctx.ui.bar(link.size, "bytes");
    let res = nuee_store::get(&net, &hub, &link, &dest, force, |done, _total| {
        bar.set_position(done);
    })
    .await;
    bar.finish_and_clear();
    net.close().await;
    let report = res.map_err(crate::store_failure)?;
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
