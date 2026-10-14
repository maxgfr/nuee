//! `nuee pin`: keep an encrypted copy of a share and help serve it.

use std::process::ExitCode;

use nuee_proto::{Hash, Link, PinLink};
use nuee_store::Registry;
use serde_json::json;

use crate::{Ctx, exit, fail};

/// Arguments of `nuee pin`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// A pin or file link, or `-` to read it from stdin
    #[arg(required_unless_present_any = ["list", "rm"])]
    pub link: Option<String>,
    /// List pinned items
    #[arg(long, conflicts_with_all = ["link", "rm"])]
    pub list: bool,
    /// Unpin the item with this hash
    #[arg(long, value_name = "HASH", conflicts_with = "link")]
    pub rm: Option<String>,
}

fn registry(ctx: &Ctx) -> anyhow::Result<Registry> {
    if ctx.global.ephemeral {
        return Ok(Registry::in_memory());
    }
    let path = Registry::default_path(&ctx.data_dir()?);
    Registry::load(&path).map_err(anyhow::Error::new)
}

/// Pins, lists or unpins.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let mut reg = registry(ctx)?;
    if args.list {
        for e in reg.list() {
            ctx.ui.emit(
                "pin",
                json!({ "hash": e.hash, "size": e.size, "providers": e.providers.len() }),
            );
            if !ctx.ui.is_json() {
                ctx.ui.say(&format!("{}  {} bytes", e.hash, e.size));
            }
        }
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(hex) = &args.rm {
        let hash: Hash = hex
            .parse()
            .map_err(|_| fail(exit::USAGE, format!("{hex:?} is not a BLAKE3 hash")))?;
        if reg.remove(&hash).is_none() {
            return Err(fail(exit::ERROR, format!("{hash} is not pinned")));
        }
        reg.save().map_err(anyhow::Error::new)?;
        if !ctx.global.ephemeral {
            nuee_store::unpin_local(&ctx.data_dir()?, hash)
                .await
                .map_err(crate::store_failure)?;
        }
        ctx.ui.emit("unpinned", json!({ "hash": hash.to_string() }));
        ctx.ui.say(&format!("unpinned {hash}"));
        return Ok(ExitCode::SUCCESS);
    }
    let text = crate::link_arg(args.link.as_deref().unwrap_or("-"))?;
    let link = match PinLink::parse(&text) {
        Ok(l) => l,
        Err(nuee_proto::LinkError::WrongKind) => {
            match Link::parse(&text).map_err(crate::link_failure)? {
                Link::File(f) | Link::Site(f) => f.pin_link(),
                _ => {
                    return Err(fail(
                        exit::USAGE,
                        "only file, site and pin links can be pinned",
                    ));
                }
            }
        }
        Err(e) => return Err(crate::link_failure(e)),
    };
    let net = ctx.online_net().await?;
    let hub = ctx.hub(&net).await?;
    let bar = ctx.ui.bar(link.size, "bytes");
    let res = nuee_store::pin(&net, &hub, &link, &mut reg, |done, _| {
        bar.set_position(done)
    })
    .await;
    bar.finish_and_clear();
    net.close().await;
    let augmented = res.map_err(crate::store_failure)?;
    let url = augmented.to_url(&ctx.global.web_base);
    ctx.ui.emit(
        "pinned",
        json!({ "hash": link.hash.to_string(), "size": link.size, "link": url }),
    );
    ctx.ui.say(&url);
    ctx.ui
        .note("pinned; run `nuee serve` to serve it (the pin link lists this node)");
    Ok(ExitCode::SUCCESS)
}
