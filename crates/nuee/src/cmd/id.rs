//! `nuee id`: this node's identity.

use std::process::ExitCode;

use serde_json::json;

use crate::{Ctx, config, exit, fail};

/// Arguments of `nuee id`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Create a new identity
    #[arg(long)]
    pub new: bool,
    /// With --new, replace an existing identity
    #[arg(long, requires = "new")]
    pub force: bool,
}

/// Prints the EndpointId and fingerprint, creating the key on first use.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let key = if ctx.global.ephemeral {
        ctx.secret()?
    } else {
        let dir = config::config_dir(ctx.global.config_dir.as_deref())?;
        let exists = dir.join(config::KEY_FILE).exists();
        if args.new {
            if exists && !args.force {
                return Err(fail(
                    exit::ERROR,
                    format!(
                        "an identity already exists in {}; pass --force to replace it (peers will see a new id)",
                        dir.display()
                    ),
                ));
            }
            config::create(&dir, exists)?
        } else {
            config::load_or_create(&dir)?
        }
    };
    let id = key.public();
    let fp = nuee_proto::fingerprint(&id);
    ctx.ui.emit(
        "id",
        json!({ "id": id.to_string(), "fingerprint": fp.to_vec() }),
    );
    if !ctx.ui.is_json() {
        ctx.ui.say(&id.to_string());
        ctx.ui.say(&format!("fingerprint {}", fp.join(" ")));
    }
    Ok(ExitCode::SUCCESS)
}
