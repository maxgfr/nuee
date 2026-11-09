//! `nuee site`: publish a static site folder (tar + age, `#/S/` link) or fetch one.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nuee_proto::Link;
use serde_json::json;

use crate::{Ctx, exit, fail};

// The same files as the page's own copy, which the web app hands out too.
const LOADER_PAGE: &str = include_str!("../../../../web/loader/index.html");
const LOADER_SCRIPT: &str = include_str!("../../../../web/loader/nuee-loader.js");
const LOADER_WORKER: &str = include_str!("../../../../web/loader/nuee-sw.js");

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
        /// Hand the site to this anchor's or group's gateway (its link, or `-`) and return; it
        /// gets the decryption key and serves the plaintext
        #[arg(long, value_name = "LINK")]
        on: Option<String>,
        /// Give the site a stable link on this group (`--on`): publishing again under the same
        /// name changes what the link shows, and the link stays the same
        #[arg(long, value_name = "NAME", requires = "on")]
        name: Option<String>,
    },
    /// Write a small page that serves a named site from any static host (GitHub Pages, etc.)
    Loader {
        /// The site's stable link (`#/N/`), or `-` to read it from stdin
        link: String,
        /// Where to write the files
        #[arg(short, long, value_name = "DIR", default_value = "nuee-loader")]
        out: PathBuf,
        /// Overwrite existing files
        #[arg(long)]
        force: bool,
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
        Action::Publish { dir, on, name } => {
            publish(dir, on.as_deref(), name.as_deref(), ctx).await
        }
        Action::Loader { link, out, force } => loader(&link, &out, force, ctx),
        Action::Get { link, out, force } => get(&link, out, force, ctx).await,
    }
}

/// Writes the three files that serve a named site from any static host.
fn loader(link: &str, out: &Path, force: bool, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let text = crate::link_arg(link)?;
    if !matches!(
        Link::parse(&text).map_err(crate::link_failure)?,
        Link::Name(_)
    ) {
        return Err(fail(
            exit::USAGE,
            "the loader needs a site's stable link: `nuee site publish --on <group> --name <name>`",
        ));
    }
    let base = format!("{}/", ctx.global.web_base.trim_end_matches('/'));
    let script = LOADER_SCRIPT
        .replace("\"__NUEE_LINK__\"", &serde_json::to_string(&text)?)
        .replace("\"__NUEE_BASE__\"", &serde_json::to_string(&base)?);
    let files = [
        ("index.html", LOADER_PAGE),
        ("nuee-loader.js", script.as_str()),
        ("nuee-sw.js", LOADER_WORKER),
    ];
    std::fs::create_dir_all(out)
        .map_err(|e| fail(exit::ERROR, format!("{}: {e}", out.display())))?;
    if !force {
        for (name, _) in files {
            if out.join(name).exists() {
                return Err(fail(
                    exit::USAGE,
                    format!("{} exists; pass --force", out.join(name).display()),
                ));
            }
        }
    }
    for (name, content) in files {
        std::fs::write(out.join(name), content)
            .map_err(|e| fail(exit::ERROR, format!("{name}: {e}")))?;
    }
    ctx.ui
        .emit("loader", json!({ "dir": out.display().to_string() }));
    ctx.ui.say(&format!(
        "put the files of {} on any static host; the site comes from the group",
        crate::ui::clean(&out.display().to_string())
    ));
    Ok(ExitCode::SUCCESS)
}

async fn publish(
    dir: PathBuf,
    on: Option<&str>,
    name: Option<&str>,
    ctx: &Ctx,
) -> anyhow::Result<ExitCode> {
    if name.is_some_and(|n| !nuee_proto::valid_site_name(n)) {
        return Err(fail(
            exit::USAGE,
            "a site's name uses a-z, 0-9 and -, 63 characters at most",
        ));
    }
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
    let on = on.map(crate::link_arg).transpose()?;
    let group = on
        .as_deref()
        .is_some_and(|t| matches!(Link::parse(t), Ok(Link::Group(_))));
    if name.is_some() && !group {
        return Err(fail(exit::USAGE, "a site's name needs --on <group link>"));
    }
    let (net, anchor) = match on.as_deref() {
        Some(text) if group => {
            let (net, target) = crate::cmd::group::member_session(text, ctx).await?;
            (net, Some(target))
        }
        Some(text) => {
            let link = crate::cmd::serve::anchor_link(text)?;
            (crate::cmd::serve::client_net(ctx, &link).await?, Some(link))
        }
        None => (ctx.online_net().await?, None),
    };
    // Next to a running `group up`, whose store this machine's data directory already holds.
    let scratch = tempfile::tempdir().map_err(|e| fail(exit::ERROR, format!("temp dir: {e}")))?;
    let hub = if group {
        nuee_store::StoreHub::open(&net, scratch.path(), nuee_store::HubConfig::default())
            .await
            .map_err(crate::store_failure)?
    } else {
        ctx.hub(&net).await?
    };
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
        let res = crate::cmd::serve::publish_on_anchor(ctx, &net, link, &url, name).await;
        share.shutdown().await.map_err(crate::store_failure)?;
        let path = res?;
        if !path.is_empty() {
            ctx.ui
                .say(&format!("served by the anchor's gateway at {path}"));
        }
        if let (Some(name), Some(group)) = (name, on.as_deref()) {
            let stable = crate::cmd::group::name_link(&net, group, name, ctx).await?;
            ctx.ui.show_link(
                "named site",
                &Link::Name(stable).to_url(&ctx.global.web_base),
                None,
                None,
            )?;
        }
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
    // A visitor of a name may be a machine of the group, whose `group up` holds the data directory.
    let scratch = tempfile::tempdir().map_err(|e| fail(exit::ERROR, format!("temp dir: {e}")))?;
    let (net, parsed, visiting) = match Link::parse(&text).map_err(crate::link_failure)? {
        Link::Name(name) => {
            let net = crate::cmd::group::visitor_net(ctx, &name).await?;
            let found = crate::cmd::serve::resolve_name(&net, &name).await?;
            (net, Link::parse(&found).map_err(crate::link_failure)?, true)
        }
        other => (ctx.online_net().await?, other, false),
    };
    let link = match parsed {
        Link::Site(l) => l,
        Link::File(_) => return Err(fail(exit::USAGE, "this is a file link: use `nuee get`")),
        Link::Job(_) => return Err(fail(exit::USAGE, "this is a swarm link: use `nuee join`")),
        Link::Service(_) => {
            return Err(fail(
                exit::USAGE,
                "this is a service link: use `nuee connect`",
            ));
        }
        Link::Group(_) => {
            return Err(fail(
                exit::USAGE,
                "this is a group link: use `nuee group up`",
            ));
        }
        Link::Name(_) => return Err(fail(exit::ERROR, "a name pointed to another name")),
    };
    let dest = out.unwrap_or_else(|| PathBuf::from("."));
    let hub = if visiting {
        nuee_store::StoreHub::open(&net, scratch.path(), nuee_store::HubConfig::default())
            .await
            .map_err(crate::store_failure)?
    } else {
        ctx.hub(&net).await?
    };
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
