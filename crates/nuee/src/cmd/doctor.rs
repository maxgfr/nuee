//! `nuee doctor`: check this machine's setup, each check with a fix hint.

use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nuee_exec::{RunnerConfig, WasmRunner};
use nuee_net::{Net, NetConfig};
use serde_json::json;

use crate::{Ctx, config, exit, fail};

/// How long the GPU adapter probe may take.
const GPU_PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Arguments of `nuee doctor`.
#[derive(Debug, clap::Args)]
pub struct Args {}

fn in_path(tool: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(tool).is_file()))
}

/// Runs the checks; exit 1 when a required one fails.
pub async fn run(_args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let mut required_ok = true;
    let ui = &ctx.ui;

    // Identity key.
    match config::config_dir(ctx.global.config_dir.as_deref()) {
        Ok(dir) => match config::check_key(&dir) {
            Ok(Some(())) => ui.check(
                "identity",
                true,
                &format!("{} is private", dir.join(config::KEY_FILE).display()),
                None,
            ),
            Ok(None) => ui.check("identity", true, "no key yet (created on first use)", None),
            Err(e) => {
                required_ok = false;
                ui.check(
                    "identity",
                    false,
                    &format!("{e:#}"),
                    Some("fix the permissions as shown"),
                );
            }
        },
        Err(e) => {
            required_ok = false;
            ui.check(
                "identity",
                false,
                &format!("{e:#}"),
                Some("set NUEE_CONFIG_DIR"),
            );
        }
    }

    // Data directory.
    match ctx.data_dir() {
        Ok(dir) => ui.check("data", true, &dir.display().to_string(), None),
        Err(e) => {
            required_ok = false;
            ui.check("data", false, &format!("{e:#}"), Some("set NUEE_DATA_DIR"));
        }
    }

    // Relay, with a throwaway identity.
    let relay = match (config::fresh_key(), ctx.relay_choice()) {
        (Ok(secret), Ok(relay)) => {
            let cfg = NetConfig {
                secret,
                relay,
                relay_only: ctx.global.relay_only,
                alpns: Vec::new(),
            };
            match Net::bind(cfg).await {
                Ok(net) => {
                    let res = net.online(crate::online_timeout()).await;
                    let home = Ctx::relay_string(&net);
                    net.close().await;
                    res.map(|()| home).map_err(|e| anyhow::anyhow!("{e}"))
                }
                Err(e) => Err(anyhow::anyhow!("{e}")),
            }
        }
        (Err(e), _) | (_, Err(e)) => Err(e),
    };
    match relay {
        Ok(home) => ui.check("relay", true, &format!("online via {home}"), None),
        Err(e) => {
            required_ok = false;
            ui.check(
                "relay",
                false,
                &format!("{e:#}"),
                Some("check your connection or pass --relay <url> (see `nuee relay`)"),
            );
        }
    }

    // WASM engine and golden outputs.
    let golden = tokio::task::spawn_blocking(|| {
        let runner = WasmRunner::new(RunnerConfig::default())
            .map_err(|e| anyhow::anyhow!("WASM engine: {e}"))?;
        super::selftest::golden_check(&runner)
    })
    .await?;
    match golden {
        Ok(detail) => ui.check("wasm", true, &detail, None),
        Err(e) => {
            required_ok = false;
            ui.check("wasm", false, &format!("{e:#}"), Some("reinstall nuee"));
        }
    }

    // GPU adapter, probed as `join --gpu` does; its absence only turns GPU tasks off.
    let probe = tokio::time::timeout(
        GPU_PROBE_TIMEOUT,
        tokio::task::spawn_blocking(nuee_gpu::probe_adapter),
    )
    .await;
    match probe {
        Ok(Ok(Some((caps, backend)))) => ui.check(
            "gpu",
            true,
            &format!("{} ({backend})", caps.adapter.as_str()),
            None,
        ),
        Ok(Ok(None)) => ui.check(
            "gpu",
            false,
            "no usable GPU adapter: GPU tasks are off on this machine",
            Some("optional: `join --gpu` needs a Metal, Vulkan or DX12 adapter"),
        ),
        Ok(Err(e)) => ui.check(
            "gpu",
            false,
            &format!("the adapter probe failed ({e}): GPU tasks are off on this machine"),
            Some("optional: only needed for `join --gpu`"),
        ),
        Err(_) => ui.check(
            "gpu",
            false,
            "the adapter probe timed out: GPU tasks are off on this machine",
            Some("optional: only needed for `join --gpu`"),
        ),
    }

    // Sandbox tools for native commands (optional).
    let (tool, present) = if cfg!(target_os = "macos") {
        ("sandbox-exec", Path::new("/usr/bin/sandbox-exec").is_file())
    } else {
        ("bwrap", in_path("bwrap"))
    };
    ui.check(
        "exec sandbox",
        present,
        &format!("{tool} {}", if present { "found" } else { "missing" }),
        Some("only needed to run native commands (install bubblewrap on Linux)"),
    );

    // Clock.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    // 2025-01-01T00:00:00Z.
    let sane = now.as_secs() > 1_735_689_600;
    if !sane {
        required_ok = false;
    }
    ui.check(
        "clock",
        sane,
        if sane { "plausible" } else { "set before 2025" },
        Some("set the system date"),
    );

    ui.emit("doctor", json!({ "ok": required_ok }));
    if required_ok {
        Ok(ExitCode::SUCCESS)
    } else {
        Err(fail(exit::ERROR, "some required checks failed"))
    }
}
