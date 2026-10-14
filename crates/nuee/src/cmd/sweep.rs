//! `nuee sweep`: one native `exec` task per `[[sweep]]` entry of a job file, then a summary
//! table of the metrics each entry printed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use bytes::Bytes;
use nuee_core::{CoordinatorConfig, Event, JobSpec};
use nuee_net::{CoordinatorDriver, DriverConfig, HostEvent};
use nuee_proto::limits::{MAX_INLINE_INPUT, MAX_JOBS};
use nuee_proto::{
    ALPN_JOB, BoundedBytes, BoundedString, BoundedVec, ByeReason, Comparator, EndpointId, ExecSpec,
    JobKind, JobLink, JobToml, Link, Manifest, PROTO_VERSION, Payload, TaskKind, VerifyPolicy,
};
use nuee_store::{HubConfig, StoreHub};
use serde_json::json;

use super::run::{self, jobfile};
use crate::{Ctx, exit, fail};

/// Arguments of `nuee sweep`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// A job TOML file of kind "exec" with `[[sweep]]` entries
    pub spec: PathBuf,
    /// Output directory (default ./nuee-out/<job name>)
    #[arg(long, value_name = "DIR")]
    pub out: Option<PathBuf>,
    /// Tasks this machine runs at once when it allows the command (default: half the cores);
    /// with 0 it still recomputes the samples that check peers
    #[arg(long, value_name = "N")]
    pub local_slots: Option<u16>,
    /// Also run the sweep's own commands here when they come from this repository revision
    /// (repo@rev); only the commands in the sweep file run
    #[arg(long, value_name = "REPO@REV")]
    pub allow_exec: Vec<String>,
    /// Fetch the --allow-exec repository URL from a local clone instead (offline or air-gapped
    /// machines; every --allow-exec entry must name URL)
    #[arg(long, value_name = "URL=PATH", requires = "allow_exec")]
    pub exec_mirror: Option<String>,
    /// Wait until this many peers have joined before handing out entries
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub wait_peers: u16,
    /// Also write the link's QR code to this SVG file
    #[arg(long, value_name = "FILE")]
    pub qr_svg: Option<PathBuf>,
}

/// One sweep entry, ready to submit.
struct Entry {
    name: String,
    exec: ExecSpec,
}

/// Settings shared by every entry.
struct Common {
    verify: VerifyPolicy,
    compare: Comparator,
    deadline_ms: u32,
}

impl Common {
    fn manifest(&self, e: &Entry, coordinator: EndpointId) -> anyhow::Result<Manifest> {
        let manifest = Manifest {
            version: PROTO_VERSION,
            job: 0,
            name: BoundedString::try_from(e.name.as_str())
                .map_err(|_| usage(format!("sweep name `{}` longer than 64 bytes", e.name)))?,
            kind: TaskKind::Exec,
            module: None,
            module_size: 0,
            max_memory: 0,
            assets: BoundedVec::default(),
            tasks: 1,
            verify: self.verify,
            compare: self.compare.clone(),
            deadline_ms: self.deadline_ms,
            exec: Some(e.exec.clone()),
            train: None,
            coordinator,
        };
        manifest.check().map_err(usage)?;
        Ok(manifest)
    }
}

/// How an entry ended.
struct Row {
    name: String,
    ok: bool,
    ms: u64,
    peer: String,
    metrics: Vec<(String, f64)>,
}

fn usage(e: impl std::fmt::Display) -> anyhow::Error {
    fail(exit::USAGE, e.to_string())
}

/// The shared input of every entry: the whole `inputs.jsonl` file, or nothing.
fn sweep_input(base: &Path, job: &JobToml) -> anyhow::Result<Vec<u8>> {
    let Some(i) = &job.inputs else {
        return Ok(Vec::new());
    };
    match (&i.jsonl, i.lines_per_task) {
        (Some(file), None) => {
            let path = base.join(file);
            std::fs::read(&path)
                .map_err(|e| fail(exit::ERROR, format!("cannot read {}: {e}", path.display())))
        }
        _ => Err(usage(
            "a sweep takes at most `inputs.jsonl` (the whole file goes to every entry)",
        )),
    }
}

fn plan(path: &Path) -> anyhow::Result<(JobToml, Common, Vec<Entry>, Vec<u8>)> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| fail(exit::ERROR, format!("cannot read {}: {e}", path.display())))?;
    let job = JobToml::from_str_checked(&text).map_err(usage)?;
    if job.kind != JobKind::Exec || job.sweep.is_empty() {
        return Err(usage(
            "`nuee sweep` needs kind = \"exec\" and at least one [[sweep]] entry",
        ));
    }
    if job.sweep.len() > MAX_JOBS {
        return Err(usage(format!("at most {MAX_JOBS} [[sweep]] entries")));
    }
    if job.output.as_ref().is_some_and(|o| o.merge.is_some()) {
        return Err(usage(
            "`output.merge` does not apply to sweeps (each entry has its own output)",
        ));
    }
    let e = job
        .exec
        .as_ref()
        .ok_or_else(|| usage("job file: missing [exec] table"))?;
    let base = path.parent().unwrap_or(Path::new("."));
    let input = sweep_input(base, &job)?;
    let verify = run::job_verify(&job)?;
    let compare = run::job_compare(&job)?;
    let deadline_ms = match &job.deadline {
        Some(_) => job.deadline_ms().map_err(usage)?,
        None => run::exec_timeout_ms(e)?,
    };
    let mut entries = Vec::with_capacity(job.sweep.len());
    for s in &job.sweep {
        let (argv, env) = jobfile::sweep_command(&e.argv, &e.env, &s.args, &s.env);
        let spec =
            run::exec_spec(e, &argv, &env).with_context(|| format!("sweep entry `{}`", s.name))?;
        entries.push(Entry {
            name: s.name.clone(),
            exec: spec,
        });
    }
    let common = Common {
        verify,
        compare,
        deadline_ms: u32::try_from(deadline_ms).map_err(|_| usage("deadline too long"))?,
    };
    Ok((job, common, entries, input))
}

fn print_table(ctx: &Ctx, rows: &[Row]) {
    let metric_rows: Vec<Vec<(String, f64)>> = rows.iter().map(|r| r.metrics.clone()).collect();
    let mut columns: Vec<&str> = Vec::new();
    for m in jobfile::METRICS {
        if metric_rows.iter().flatten().any(|(k, _)| k == m) {
            columns.push(m);
        }
    }
    let width = rows.iter().map(|r| r.name.len()).max().unwrap_or(4).max(4);
    let mut head = format!(
        "{:<width$}  {:<6}  {:>9}  {:<10}",
        "name", "status", "duration", "peer"
    );
    for c in &columns {
        head.push_str(&format!("  {c:>11}"));
    }
    ctx.ui.say(&head);
    for i in jobfile::rank(&metric_rows) {
        let Some(r) = rows.get(i) else { continue };
        let peer: String = r.peer.chars().take(10).collect();
        let mut line = format!(
            "{:<width$}  {:<6}  {:>8.1}s  {:<10}",
            r.name,
            if r.ok { "ok" } else { "failed" },
            r.ms as f64 / 1000.0,
            peer
        );
        for c in &columns {
            match r.metrics.iter().find(|(k, _)| k == c) {
                Some((_, v)) => line.push_str(&format!("  {v:>11.4}")),
                None => line.push_str(&format!("  {:>11}", "-")),
            }
        }
        ctx.ui.say(&line);
    }
}

/// Runs a sweep.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let (job, common, entries, input) = plan(&args.spec)?;
    let base = args.spec.parent().unwrap_or(Path::new("."));
    let out = args
        .out
        .clone()
        .or_else(|| {
            job.output
                .as_ref()
                .and_then(|o| o.dir.as_ref())
                .map(|d| base.join(d))
        })
        .unwrap_or_else(|| Path::new("nuee-out").join(run::file_name(&job.name)));
    std::fs::create_dir_all(&out)
        .map_err(|e| fail(exit::ERROR, format!("cannot create {}: {e}", out.display())))?;

    let own: Vec<&nuee_proto::ExecSpec> = entries.iter().map(|e| &e.exec).collect();
    let native = run::local_native(&args.allow_exec, args.exec_mirror.as_deref(), &own, ctx)?;
    let allowed_here = entries
        .iter()
        .all(|e| native.as_ref().is_some_and(|n| n.allowed(&e.exec)));
    let local_slots = if allowed_here {
        args.local_slots.unwrap_or_else(crate::default_slots)
    } else {
        ctx.ui
            .note("only peers run these commands (allow them here with --allow-exec repo@rev)");
        0
    };

    let net = ctx.online_net().await?;
    let hub = StoreHub::memory(&net, HubConfig::default());
    let input = if input.len() <= MAX_INLINE_INPUT {
        Payload::Inline(
            BoundedBytes::try_from(input).map_err(|_| anyhow::anyhow!("input too large"))?,
        )
    } else {
        let size = input.len() as u64;
        let hash = hub
            .add_bytes(Bytes::from(input))
            .await
            .map_err(crate::store_failure)?;
        Payload::Blob { hash, size }
    };

    let secret = nuee_proto::new_secret16().context("system random source")?;
    let seed_bytes = nuee_proto::new_secret16().context("system random source")?;
    let seed = u64::from_le_bytes(seed_bytes[..8].try_into()?);
    let core_cfg = CoordinatorConfig {
        name: BoundedString::try_from(job.name.as_str())
            .map_err(|_| usage("job name longer than 64 bytes"))?,
        local_slots,
        verify_slots: u16::from(allowed_here),
        seed,
        ..CoordinatorConfig::default()
    };
    let mut exec = run::WasmExec::new(None)?;
    if let Some(native) = native {
        exec = exec.with_native(native);
    }
    let (driver, mut handle) = CoordinatorDriver::spawn(
        net.clone(),
        secret,
        core_cfg,
        DriverConfig {
            local_slots,
            ..DriverConfig::default()
        },
        Arc::new(exec),
        Arc::new(hub.clone()),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let router = net
        .router()
        .accept(ALPN_JOB, driver)
        .accept(iroh_blobs::ALPN, hub.protocol())
        .spawn();
    let link = Link::Job(JobLink {
        coordinator: net.id(),
        relay: BoundedString::try_from(Ctx::relay_string(&net).as_str())
            .map_err(|_| anyhow::anyhow!("relay URL too long for a link"))?,
        secret,
    });
    ctx.ui.show_link(
        "job",
        &link.to_url(&ctx.global.web_base),
        Some(&net.id()),
        args.qr_svg.as_deref(),
    )?;
    ctx.ui.note(&format!(
        "{} sweep entries, outputs in {}",
        entries.len(),
        out.display()
    ));

    let mut pending: Vec<Entry> = entries;
    let names: Vec<String> = pending.iter().map(|e| e.name.clone()).collect();
    let mut jobs: BTreeMap<u32, (usize, Instant)> = BTreeMap::new();
    let mut by: BTreeMap<u32, String> = BTreeMap::new();
    let mut rows: Vec<Row> = Vec::new();
    let mut peers = 0usize;
    let me = net.id();

    let stop = crate::ctrl_c();
    tokio::pin!(stop);
    let outcome: anyhow::Result<()> = loop {
        if !pending.is_empty() && peers >= usize::from(args.wait_peers) {
            for (i, e) in std::mem::take(&mut pending).into_iter().enumerate() {
                let manifest = common.manifest(&e, me)?;
                let spec = JobSpec {
                    compare: run::checker(manifest.kind, &manifest.compare),
                    manifest,
                    inputs: vec![input.clone()],
                };
                let id = handle.submit(spec).await.map_err(|err| {
                    fail(exit::ERROR, format!("cannot submit `{}`: {err}", e.name))
                })?;
                jobs.insert(id, (i, Instant::now()));
            }
        }
        if rows.len() == names.len() {
            break Ok(());
        }
        let ev = tokio::select! {
            ev = handle.events.recv() => ev,
            () = &mut stop => break Err(fail(exit::CANCELLED, format!(
                "interrupted; {} of {} entries finished in {}", rows.len(), names.len(), out.display()))),
        };
        let Some(ev) = ev else {
            break Err(anyhow::anyhow!("the coordinator stopped unexpectedly"));
        };
        match ev {
            HostEvent::Accepted {
                job,
                output,
                by: peer,
                ..
            } => {
                let Some((i, _)) = jobs.get(&job) else {
                    continue;
                };
                let name = names.get(*i).map(String::as_str).unwrap_or_default();
                let dir = out.join(run::file_name(name));
                std::fs::create_dir_all(&dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
                std::fs::write(dir.join("out"), &output)
                    .with_context(|| format!("writing {}", dir.join("out").display()))?;
                let who = if peer == me {
                    "local".to_owned()
                } else {
                    peer.to_string()
                };
                by.insert(job, who);
            }
            HostEvent::Event(Event::PeerJoined { peer, caps }) => {
                peers += 1;
                ctx.ui.emit(
                    "peer",
                    json!({ "id": peer.to_string(), "state": "joined", "agent": caps.agent.as_str() }),
                );
            }
            HostEvent::Event(Event::PeerLeft { .. } | Event::Banned { .. }) => {
                peers = peers.saturating_sub(1);
            }
            HostEvent::Event(Event::JobDone { job, failed }) => {
                let Some((i, started)) = jobs.get(&job) else {
                    continue;
                };
                let name = names.get(*i).cloned().unwrap_or_default();
                let ok = failed == 0;
                let output = out.join(run::file_name(&name)).join("out");
                let metrics = if ok {
                    std::fs::read(&output)
                        .map(|b| jobfile::metrics(&b))
                        .with_context(|| format!("reading {}", output.display()))?
                } else {
                    Vec::new()
                };
                let row = Row {
                    name,
                    ok,
                    ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    peer: by.get(&job).cloned().unwrap_or_else(|| "-".into()),
                    metrics,
                };
                let metric_map: serde_json::Map<String, serde_json::Value> = row
                    .metrics
                    .iter()
                    .map(|(k, v)| (k.clone(), json!(v)))
                    .collect();
                ctx.ui.emit(
                    "sweep_entry",
                    json!({ "name": row.name, "status": if ok { "ok" } else { "failed" },
                            "duration_ms": row.ms, "peer": row.peer,
                            "out": output.display().to_string(), "metrics": metric_map }),
                );
                ctx.ui.note(&format!(
                    "{}: {}",
                    row.name,
                    if ok { "done" } else { "failed" }
                ));
                rows.push(row);
            }
            HostEvent::Event(Event::WaitingForChecker {
                job, waiting, kind, ..
            }) => {
                ctx.ui.emit(
                    "waiting_checker",
                    json!({ "job": job, "waiting": waiting, "kind": format!("{kind:?}") }),
                );
                ctx.ui.note(&run::waiting_checker_line(
                    waiting,
                    kind,
                    "set verify = \"none\" in the job file and restart",
                ));
            }
            HostEvent::Event(_) | HostEvent::Closed { .. } => {}
        }
    };

    let reason = if outcome.is_ok() {
        ByeReason::Done
    } else {
        ByeReason::Shutdown
    };
    if handle.shutdown_with(reason).await.is_err() {
        tracing::debug!("the coordinator had already stopped");
    }
    // Let the goodbyes reach the peers before the endpoint closes.
    tokio::time::sleep(Duration::from_millis(800)).await;
    if let Err(e) = router.shutdown().await {
        tracing::warn!("closing the endpoint: {e}");
    }
    outcome?;

    print_table(ctx, &rows);
    let failed = rows.iter().filter(|r| !r.ok).count();
    ctx.ui.emit(
        "done",
        json!({ "job": job.name, "entries": rows.len(), "failed": failed,
                "out": out.display().to_string() }),
    );
    if failed > 0 {
        return Err(fail(
            exit::ERROR,
            format!("{failed} of {} sweep entries failed", rows.len()),
        ));
    }
    Ok(ExitCode::SUCCESS)
}
