//! `nuee train`: coordinate a DiLoCo run. Each round is one job of `workers` training tasks;
//! accepted deltas are aggregated by `nuee-train` and the outer step is applied here.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use bytes::Bytes;
use nuee_core::{CoordinatorConfig, Event, JobSpec};
use nuee_net::{CoordinatorDriver, CoordinatorHandle, DriverConfig, HostEvent};
use nuee_proto::job_toml::{ExecToml, TrainerChoice, VerifyChoice};
use nuee_proto::limits::{DEFAULT_DEADLINE_MS, MAX_ASSETS_PER_JOB, MAX_INLINE_INPUT};
use nuee_proto::{
    ALPN_JOB, Asset, BoundedBytes, BoundedString, BoundedVec, ByeReason, Comparator, DeltaCodec,
    EndpointId, EnvVar, ExecSpec, Hash, JobKind, JobLink, JobToml, Link, Manifest, PROTO_VERSION,
    Payload, TaskKind, TrainSpec, TrainerKind, VerifyMode, VerifyPolicy,
};
use nuee_store::{HubConfig, StoreHub};
use nuee_train::{Params, Reject, RoundReport, TrainState, TrainTask};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::cmd::run::{
    GOLDEN_OPTDIGITS, GOLDEN_TOY_TRAINER, RepoLookup, WasmExec, repo_or_embedded,
};
use crate::{Ctx, exit, fail};

/// Delta encodings on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum CodecArg {
    /// int8 with error feedback (4x smaller)
    Int8,
    /// Raw f32
    F32,
}

/// Arguments of `nuee train`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// A job TOML file with kind = "train", or demo:train (the toy trainer on UCI optdigits)
    pub spec: PathBuf,
    /// Output directory for checkpoints and train.json (default ./nuee-out/<job name>)
    #[arg(long, value_name = "DIR")]
    pub out: Option<PathBuf>,
    /// Ignore checkpoints of an interrupted run and start over
    #[arg(long)]
    pub no_resume: bool,
    /// Tasks this machine runs at once (default: half the cores); with 0 a wasm trainer's
    /// samples are still recomputed here
    #[arg(long, value_name = "N")]
    pub local_slots: Option<u16>,
    /// Approve each joining peer by hand
    #[arg(long)]
    pub approve: bool,
    /// Wait until this many peers have joined before the first round
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub wait_peers: u16,
    /// Close a round after this long once --min-workers deltas arrived, such as 2m
    #[arg(long, value_name = "DURATION")]
    pub round_timeout: Option<String>,
    /// Deltas needed to close a round early or after failures (default: all workers)
    #[arg(long, value_name = "N")]
    pub min_workers: Option<u16>,
    /// Inner learning rate handed to trainers (default: the trainer's own)
    #[arg(long, value_name = "LR")]
    pub inner_lr: Option<f32>,
    /// Delta encoding of exec trainers; WASM trainers always send f32 (exact verification)
    #[arg(long, value_enum, default_value_t = CodecArg::Int8)]
    pub codec: CodecArg,
    /// Seed of shard assignment and task seeds (default: random; kept in checkpoints)
    #[arg(long, value_name = "N")]
    pub seed: Option<u64>,
    /// Task deadline, such as 30s or 5m (default: the job file's, else 60s)
    #[arg(long, value_name = "DURATION")]
    pub deadline: Option<String>,
    /// Also write the link's QR code to this SVG file
    #[arg(long, value_name = "FILE")]
    pub qr_svg: Option<PathBuf>,
}

/// The codec a job declares: exec trainers use `--codec`; WASM trainer outputs are recomputed
/// and compared exactly, which an error-feedback residual (worker state) would defeat, so they
/// always use f32.
fn job_codec(trainer: TrainerKind, codec: CodecArg) -> DeltaCodec {
    match (trainer, codec) {
        (TrainerKind::Wasm, _) | (TrainerKind::Exec, CodecArg::F32) => DeltaCodec::F32,
        (TrainerKind::Exec, CodecArg::Int8) => DeltaCodec::Int8Ef,
    }
}

struct Prepared {
    name: String,
    spec: TrainSpec,
    init: Params,
    module: Option<Vec<u8>>,
    exec: Option<ExecSpec>,
    max_memory: u64,
    assets: Vec<(String, Vec<u8>)>,
    verify: VerifyPolicy,
    deadline_ms: u32,
    out: Option<PathBuf>,
}

fn read_file(path: &Path) -> anyhow::Result<Vec<u8>> {
    std::fs::read(path)
        .map_err(|e| fail(exit::ERROR, format!("cannot read {}: {e}", path.display())))
}

fn usage(e: impl std::fmt::Display) -> anyhow::Error {
    fail(exit::USAGE, e.to_string())
}

fn dataset_files(dir: &Path) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| fail(exit::ERROR, format!("cannot read {}: {e}", dir.display())))?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<_, _>>()?;
    files.retain(|p| p.is_file());
    files.sort();
    if files.len() > MAX_ASSETS_PER_JOB {
        return Err(fail(
            exit::USAGE,
            format!(
                "{} holds more than {MAX_ASSETS_PER_JOB} files",
                dir.display()
            ),
        ));
    }
    files
        .iter()
        .map(|p| {
            let name = p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("data")
                .chars()
                .take(128)
                .collect();
            Ok((name, read_file(p)?))
        })
        .collect()
}

fn exec_spec(e: &ExecToml) -> anyhow::Result<ExecSpec> {
    let s = |v: &str| BoundedString::try_from(v).map_err(|_| usage("[exec] entry too long"));
    let argv = e
        .argv
        .iter()
        .map(|a| s(a))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let env = e
        .env
        .iter()
        .map(|(k, v)| {
            Ok(EnvVar {
                key: BoundedString::try_from(k.as_str())
                    .map_err(|_| usage("[exec] env key too long"))?,
                value: BoundedString::try_from(v.as_str())
                    .map_err(|_| usage("[exec] env value too long"))?,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let timeout_ms = match &e.timeout {
        Some(t) => nuee_proto::job_toml::parse_duration_ms(t).map_err(usage)?,
        None => 3_600_000,
    };
    Ok(ExecSpec {
        repo: BoundedString::try_from(e.repo.as_str())
            .map_err(|_| usage("[exec] repo too long"))?,
        rev: BoundedString::try_from(e.rev.as_str()).map_err(|_| usage("[exec] rev invalid"))?,
        argv: BoundedVec::try_from(argv).map_err(|_| usage("[exec] argv too long"))?,
        env: BoundedVec::try_from(env).map_err(|_| usage("[exec] too many env entries"))?,
        timeout_s: u32::try_from(timeout_ms.div_ceil(1000))
            .map_err(|_| usage("timeout too long"))?,
        gpu: e.gpu,
    })
}

/// The built-in demo: the toy MLP trainer on UCI optdigits, 10 rounds of 4 workers x 30 steps.
const DEMO_TRAIN: &str = "demo:train";

fn prepare_demo(args: &Args, lookup: RepoLookup) -> anyhow::Result<Prepared> {
    // A fresh build in a checkout wins; otherwise the golden copies embedded in the binary.
    let module = repo_or_embedded(
        "target/tasks/toy-trainer.wasm",
        Some(GOLDEN_TOY_TRAINER),
        lookup,
    )?
    .unwrap_or_else(|| GOLDEN_TOY_TRAINER.to_vec());
    let dataset = repo_or_embedded("target/tasks/optdigits.bin", Some(GOLDEN_OPTDIGITS), lookup)?
        .unwrap_or_else(|| GOLDEN_OPTDIGITS.to_vec());
    let info = nuee_proto::validate_module(&module)
        .map_err(|e| fail(exit::ERROR, format!("toy-trainer.wasm: {e}")))?;
    // Initial parameters come from the trainer itself (`NIN1` + seed).
    let runner = nuee_exec::WasmRunner::new(nuee_exec::RunnerConfig::default())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let hash = runner
        .prepare(&module)
        .map_err(|e| anyhow::anyhow!("toy-trainer.wasm: {e}"))?
        .hash;
    let mut lease = b"NIN1".to_vec();
    lease.extend_from_slice(&7u64.to_le_bytes());
    let init_bytes = runner
        .run(
            &hash,
            &nuee_proto::build_envelope(&[&dataset], &lease),
            &nuee_exec::RunLimits {
                deadline: Duration::from_secs(30),
                max_memory: info.max_memory,
                max_output: 1 << 20,
            },
        )
        .map_err(|e| anyhow::anyhow!("toy-trainer init: {e}"))?;
    let init = Params::from_safetensors(&init_bytes)
        .map_err(|e| anyhow::anyhow!("toy-trainer init: {e}"))?;
    let deadline_ms = match &args.deadline {
        Some(d) => nuee_proto::job_toml::parse_duration_ms(d)
            .map_err(|e| usage(format!("--deadline: {e}")))?,
        None => u64::from(DEFAULT_DEADLINE_MS),
    };
    Ok(Prepared {
        name: "toy-trainer".into(),
        spec: TrainSpec {
            rounds: 10,
            inner_steps: 30,
            workers: 4,
            outer_lr: 0.7,
            outer_momentum: 0.9,
            init: Hash::of(&init_bytes),
            trainer: TrainerKind::Wasm,
            // The toy trainer always sends raw f32 deltas.
            codec: DeltaCodec::F32,
            trim: 0.2,
            cos_min: 0.0,
            norm_max_ratio: 3.0,
        },
        init,
        module: Some(module),
        exec: None,
        max_memory: info.max_memory,
        assets: vec![("optdigits.bin".into(), dataset)],
        verify: VerifyPolicy::default(),
        deadline_ms: u32::try_from(deadline_ms).map_err(|_| usage("deadline too long"))?,
        out: None,
    })
}

fn prepare(args: &Args) -> anyhow::Result<Prepared> {
    if args.spec.as_os_str() == DEMO_TRAIN {
        return prepare_demo(args, crate::cmd::run::find_repo_file);
    }
    let path = &args.spec;
    let text = std::fs::read_to_string(path)
        .map_err(|e| fail(exit::ERROR, format!("cannot read {}: {e}", path.display())))?;
    let job = JobToml::from_str_checked(&text).map_err(usage)?;
    if job.kind != JobKind::Train {
        return Err(usage(format!(
            "{} is not a training job (kind = \"train\"); use `nuee run` for it",
            path.display()
        )));
    }
    let t = job
        .train
        .as_ref()
        .ok_or_else(|| usage("job file: missing [train] table"))?;
    let base = path.parent().unwrap_or(Path::new("."));
    let init_bytes = read_file(&base.join(&t.init))?;
    let init =
        Params::from_safetensors(&init_bytes).map_err(|e| usage(format!("{}: {e}", t.init)))?;
    let (trainer, module, exec, module_memory) = match t.trainer {
        TrainerChoice::Wasm => {
            let module_path = t
                .module
                .as_ref()
                .ok_or_else(|| usage("`train.module` is required with trainer = \"wasm\""))?;
            let module = read_file(&base.join(module_path))?;
            let info = nuee_proto::validate_module(&module).map_err(|e| {
                fail(
                    exit::ERROR,
                    format!("{module_path} is not a valid task module: {e}"),
                )
            })?;
            (TrainerKind::Wasm, Some(module), None, info.max_memory)
        }
        TrainerChoice::Exec => {
            let e = job
                .exec
                .as_ref()
                .ok_or_else(|| usage("trainer = \"exec\" needs an `[exec]` table"))?;
            (TrainerKind::Exec, None, Some(exec_spec(e)?), 0)
        }
    };
    let assets = match job.inputs.as_ref().and_then(|i| i.dir.as_ref()) {
        Some(dir) => dataset_files(&base.join(dir))?,
        None => Vec::new(),
    };
    let mut verify = VerifyPolicy::default();
    if let Some(v) = job.verify {
        verify.mode = match v {
            VerifyChoice::Full => VerifyMode::Full,
            VerifyChoice::Sample => VerifyMode::Sample,
            VerifyChoice::None => VerifyMode::None,
        };
    }
    // Ranges were checked by `validate`.
    if let Some(s) = job.sample {
        verify.sample_permille = (s * 1000.0).round() as u16;
    }
    if let Some(c) = job.canary {
        verify.canary_permille = (c * 1000.0).round() as u16;
    }
    let deadline_ms = match &args.deadline {
        Some(d) => nuee_proto::job_toml::parse_duration_ms(d)
            .map_err(|e| usage(format!("--deadline: {e}")))?,
        None => job.deadline_ms().map_err(usage)?,
    };
    let deadline_ms = u32::try_from(deadline_ms).map_err(|_| usage("deadline too long"))?;
    let max_memory = job
        .max_memory_bytes()
        .map_err(usage)?
        .unwrap_or(module_memory);
    let spec = TrainSpec {
        rounds: t.rounds,
        inner_steps: t.inner_steps,
        workers: t.workers,
        outer_lr: t.outer_lr.unwrap_or(0.7) as f32,
        outer_momentum: t.outer_momentum.unwrap_or(0.9) as f32,
        init: Hash::of(&init_bytes),
        trainer,
        codec: job_codec(trainer, args.codec),
        trim: t.trim.unwrap_or(0.2) as f32,
        cos_min: t.cos_min.unwrap_or(0.0) as f32,
        norm_max_ratio: t.norm_max_ratio.unwrap_or(3.0) as f32,
    };
    let out = job
        .output
        .as_ref()
        .and_then(|o| o.dir.as_ref())
        .map(|d| base.join(d));
    Ok(Prepared {
        name: job.name.clone(),
        spec,
        init,
        module,
        exec,
        max_memory,
        assets,
        verify,
        deadline_ms,
        out,
    })
}

// ---------------------------------------------------------------------------------------------
// Checkpoints and history.

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RoundLog {
    round: u32,
    loss: Option<f32>,
    accepted: usize,
    rejected: usize,
    update_norm: f64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct History {
    name: String,
    init: String,
    rounds: Vec<RoundLog>,
}

fn checkpoint_file(out: &Path, round: u32) -> PathBuf {
    out.join(format!("checkpoint-{round}.safetensors"))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, bytes)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| fail(exit::ERROR, format!("cannot write {}: {e}", path.display())))
}

/// The highest `checkpoint-<r>.safetensors` in `out`.
fn last_checkpoint(out: &Path) -> anyhow::Result<Option<(u32, PathBuf)>> {
    let entries = match std::fs::read_dir(out) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(fail(
                exit::ERROR,
                format!("cannot read {}: {e}", out.display()),
            ));
        }
    };
    let mut best = None;
    for entry in entries {
        let path = entry?.path();
        let round = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix("checkpoint-"))
            .and_then(|n| n.strip_suffix(".safetensors"))
            .and_then(|n| n.parse::<u32>().ok());
        if let Some(r) = round
            && best.as_ref().is_none_or(|(b, _)| r > *b)
        {
            best = Some((r, path));
        }
    }
    Ok(best)
}

fn load_history(out: &Path, name: &str, init: &str, upto: u32) -> History {
    let fresh = History {
        name: name.into(),
        init: init.into(),
        rounds: Vec::new(),
    };
    let Ok(text) = std::fs::read_to_string(out.join("train.json")) else {
        return fresh;
    };
    match serde_json::from_str::<History>(&text) {
        Ok(mut h) if h.init == init => {
            h.rounds.retain(|r| r.round <= upto);
            h
        }
        Ok(_) => fresh,
        Err(e) => {
            tracing::warn!("ignoring an unreadable train.json: {e}");
            fresh
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The run.

struct Session<'a> {
    ctx: &'a Ctx,
    handle: CoordinatorHandle,
    peers: BTreeSet<EndpointId>,
    banned: u32,
    /// Collected rounds whose job is dropped from the coordinator once it is finished.
    to_forget: BTreeSet<u32>,
}

enum Step {
    Accepted {
        job: u32,
        task: u32,
        output: Arc<[u8]>,
    },
    JobDone {
        job: u32,
    },
    PeerJoined,
    Other,
}

impl Session<'_> {
    /// Frees the slot of a collected round's job once the coordinator has finished it.
    async fn forget(&mut self, job: u32) -> anyhow::Result<()> {
        self.to_forget.insert(job);
        if self
            .handle
            .forget(job)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
        {
            self.to_forget.remove(&job);
        }
        Ok(())
    }

    async fn next(&mut self) -> anyhow::Result<Step> {
        let ev = self
            .handle
            .events
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("the coordinator stopped unexpectedly"))?;
        let ui = &self.ctx.ui;
        Ok(match ev {
            HostEvent::Accepted {
                job, task, output, ..
            } => Step::Accepted { job, task, output },
            HostEvent::Event(e) => match e {
                Event::PeerJoined { peer, caps } => {
                    self.peers.insert(peer);
                    ui.emit(
                        "peer",
                        json!({ "id": peer.to_string(), "state": "joined",
                                "agent": caps.agent.as_str(), "slots": caps.cpu_slots }),
                    );
                    ui.note(&super::run::peer_joined_line(&peer, &caps));
                    Step::PeerJoined
                }
                Event::PeerPending { peer, caps } => {
                    ui.emit(
                        "peer",
                        json!({ "id": peer.to_string(), "state": "pending",
                                "fingerprint": nuee_proto::fingerprint(&peer).to_vec() }),
                    );
                    let question = super::run::admit_question(&peer, &caps);
                    let yes = tokio::task::block_in_place(|| ui.confirm(&question))?;
                    self.handle
                        .approve(peer, yes)
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    Step::Other
                }
                Event::PeerLeft { peer } => {
                    self.peers.remove(&peer);
                    ui.emit("peer", json!({ "id": peer.to_string(), "state": "left" }));
                    Step::Other
                }
                Event::Mismatch { job, task, peer } => {
                    ui.emit(
                        "mismatch",
                        json!({ "job": job, "task": task, "peer": peer.to_string() }),
                    );
                    Step::Other
                }
                Event::Banned { peer, redo } => {
                    self.banned += 1;
                    self.peers.remove(&peer);
                    ui.emit("banned", json!({ "peer": peer.to_string(), "redo": redo }));
                    ui.note(&format!(
                        "peer {peer} banned after a confirmed mismatch; {redo} tasks redone"
                    ));
                    Step::Other
                }
                Event::TaskFailed { job, task, kind } => {
                    ui.emit(
                        "task_failed",
                        json!({ "job": job, "task": task, "kind": format!("{kind:?}") }),
                    );
                    ui.note(&format!("training task {task} failed for good ({kind:?})"));
                    Step::Other
                }
                Event::WaitingForChecker {
                    job, waiting, kind, ..
                } => {
                    ui.emit(
                        "waiting_checker",
                        json!({ "job": job, "waiting": waiting, "kind": format!("{kind:?}") }),
                    );
                    ui.note(&super::run::waiting_checker_line(
                        waiting,
                        kind,
                        "set verify = \"none\" in the job file and restart",
                    ));
                    Step::Other
                }
                Event::JobDone { job, .. } => {
                    if self.to_forget.contains(&job) {
                        self.forget(job).await?;
                    }
                    Step::JobDone { job }
                }
                _ => Step::Other,
            },
            HostEvent::Closed { .. } => Step::Other,
        })
    }
}

fn reject_name(r: Reject) -> String {
    match r {
        Reject::Length => "length".into(),
        Reject::NonFinite => "non-finite".into(),
        Reject::Cosine(c) => format!("cosine {c:.3}"),
        Reject::Norm(n) => format!("norm x{n:.2}"),
    }
}

/// Runs a DiLoCo training job.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    if args.approve && !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return Err(fail(
            exit::USAGE,
            "--approve needs an interactive terminal to answer y/N",
        ));
    }
    if let Some(lr) = args.inner_lr
        && !(lr.is_finite() && lr > 0.0)
    {
        return Err(usage("--inner-lr must be a positive number"));
    }
    let round_timeout = args
        .round_timeout
        .as_deref()
        .map(nuee_proto::job_toml::parse_duration_ms)
        .transpose()
        .map_err(|e| usage(format!("--round-timeout: {e}")))?
        .map(Duration::from_millis);
    let prepared = prepare(&args)?;
    let workers = prepared.spec.workers;
    let min_workers = args.min_workers.unwrap_or(workers);
    if min_workers == 0 || min_workers > workers {
        return Err(usage(format!(
            "--min-workers must be between 1 and {workers}"
        )));
    }
    let out = args
        .out
        .clone()
        .or_else(|| prepared.out.clone())
        .unwrap_or_else(|| super::run::default_out(&prepared.name));
    std::fs::create_dir_all(&out)
        .map_err(|e| fail(exit::ERROR, format!("cannot create {}: {e}", out.display())))?;

    let seed = match args.seed {
        Some(s) => s,
        None => {
            let b = nuee_proto::new_secret16().context("system random source")?;
            u64::from_le_bytes(b[..8].try_into()?)
        }
    };
    let mut state = TrainState::new(prepared.spec.clone(), prepared.init.clone(), seed)
        .map_err(|e| usage(e.to_string()))?;
    if let Some(lr) = args.inner_lr {
        state.set_inner_lr(lr);
    }
    if !args.no_resume
        && let Some((r, path)) = last_checkpoint(&out)?
    {
        state.restore(&read_file(&path)?).map_err(|e| {
            fail(
                exit::ERROR,
                format!("{}: {e} (use --no-resume)", path.display()),
            )
        })?;
        ctx.ui
            .note(&format!("resuming after round {r} from {}", path.display()));
    }
    let init_hash = prepared.spec.init.to_string();
    let mut history = load_history(&out, &prepared.name, &init_hash, state.round());
    let total = prepared.spec.rounds;
    if state.is_done() {
        ctx.ui.say(&format!(
            "all {total} rounds already done in {}",
            out.display()
        ));
        return Ok(ExitCode::SUCCESS);
    }
    // This machine runs no native code for others: exec trainers need peers that joined with
    // --allow-exec (possibly on this machine).
    let local_slots = match prepared.spec.trainer {
        TrainerKind::Wasm => args.local_slots.unwrap_or_else(crate::default_slots),
        TrainerKind::Exec => 0,
    };

    let net = ctx.online_net().await?;
    let hub = StoreHub::memory(&net, HubConfig::default());
    let module_hash = match &prepared.module {
        Some(m) => Some(
            hub.add_bytes(Bytes::from(m.clone()))
                .await
                .map_err(crate::store_failure)?,
        ),
        None => None,
    };
    let mut assets = Vec::new();
    for (name, bytes) in &prepared.assets {
        let hash = hub
            .add_bytes(Bytes::from(bytes.clone()))
            .await
            .map_err(crate::store_failure)?;
        assets.push(Asset {
            hash,
            size: bytes.len() as u64,
            name: BoundedString::try_from(name.as_str())
                .map_err(|_| anyhow::anyhow!("asset name too long"))?,
        });
    }
    let base_manifest = Manifest {
        version: PROTO_VERSION,
        job: 0,
        name: BoundedString::try_from(prepared.name.as_str())
            .map_err(|_| usage("job name longer than 64 bytes"))?,
        kind: match prepared.spec.trainer {
            TrainerKind::Wasm => TaskKind::TrainWasm,
            TrainerKind::Exec => TaskKind::TrainExec,
        },
        module: module_hash,
        module_size: u32::try_from(prepared.module.as_ref().map_or(0, Vec::len))?,
        max_memory: prepared.max_memory,
        assets: BoundedVec::try_from(assets).map_err(|_| anyhow::anyhow!("too many assets"))?,
        tasks: u32::from(workers),
        // Exec trainers are native code: outputs differ between machines, so they are not
        // compared and robust aggregation is the only check.
        verify: match prepared.spec.trainer {
            TrainerKind::Wasm => prepared.verify,
            TrainerKind::Exec => VerifyPolicy {
                mode: VerifyMode::None,
                sample_permille: 0,
                canary_permille: 0,
            },
        },
        compare: match prepared.spec.trainer {
            TrainerKind::Wasm => Comparator::Exact,
            TrainerKind::Exec => Comparator::None,
        },
        deadline_ms: if prepared.deadline_ms == 0 {
            DEFAULT_DEADLINE_MS
        } else {
            prepared.deadline_ms
        },
        exec: prepared.exec.clone(),
        train: Some(prepared.spec.clone()),
        coordinator: net.id(),
    };
    base_manifest.check().map_err(usage)?;

    let secret = nuee_proto::new_secret16().context("system random source")?;
    let core_seed = nuee_proto::new_secret16().context("system random source")?;
    let core_cfg = CoordinatorConfig {
        name: base_manifest.name.clone(),
        local_slots,
        // Wasm trainers are checked here even without local slots; exec trainers are not
        // compared at all.
        verify_slots: u16::from(matches!(prepared.spec.trainer, TrainerKind::Wasm)),
        approve: args.approve,
        seed: u64::from_le_bytes(core_seed[..8].try_into()?),
        ..CoordinatorConfig::default()
    };
    let (driver, handle) = CoordinatorDriver::spawn(
        net.clone(),
        secret,
        core_cfg,
        DriverConfig {
            local_slots,
            ..DriverConfig::default()
        },
        Arc::new(WasmExec::new(None)?),
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
        "{total} rounds of {workers} workers x {} steps, {} local slots, checkpoints in {}",
        prepared.spec.inner_steps,
        local_slots,
        out.display()
    ));

    let mut session = Session {
        ctx,
        handle,
        peers: BTreeSet::new(),
        banned: 0,
        to_forget: BTreeSet::new(),
    };
    let stop = crate::ctrl_c();
    tokio::pin!(stop);
    let outcome: anyhow::Result<()> = async {
        while session.peers.len() < usize::from(args.wait_peers) {
            if session.peers.is_empty() {
                ctx.ui
                    .note(&format!("waiting for {} peer(s) to join", args.wait_peers));
            }
            tokio::select! {
                step = session.next() => { step?; }
                () = &mut stop => return Err(fail(exit::CANCELLED, "interrupted before the first round")),
            }
        }
        while !state.is_done() {
            let report = tokio::select! {
                r = run_round(&mut session, &mut state, &hub, &base_manifest, min_workers, round_timeout) => r?,
                () = &mut stop => return Err(fail(exit::CANCELLED, format!(
                    "interrupted during round {}; run again to resume from checkpoint {}",
                    state.round() + 1, state.round()))),
            };
            let done = state.round();
            write_atomic(
                &checkpoint_file(&out, done),
                &state.checkpoint().map_err(|e| anyhow::anyhow!("{e}"))?,
            )?;
            history.rounds.push(RoundLog {
                round: done,
                loss: report.mean_loss,
                accepted: report.accepted.len(),
                rejected: report.rejected.len(),
                update_norm: report.update_norm,
            });
            write_atomic(&out.join("train.json"), &serde_json::to_vec_pretty(&history)?)?;
            print_round(ctx, &report, done, total);
        }
        Ok(())
    }
    .await;

    let reason = if outcome.is_ok() {
        ByeReason::Done
    } else {
        ByeReason::Shutdown
    };
    if session.handle.shutdown_with(reason).await.is_err() {
        tracing::debug!("the coordinator had already stopped");
    }
    tokio::time::sleep(Duration::from_millis(800)).await;
    if let Err(e) = router.shutdown().await {
        tracing::warn!("closing the endpoint: {e}");
    }
    outcome?;
    let last = checkpoint_file(&out, state.round());
    ctx.ui.emit(
        "done",
        json!({ "job": prepared.name, "rounds": total, "out": out.display().to_string(),
                "checkpoint": last.display().to_string(), "banned": session.banned }),
    );
    ctx.ui.say(&format!(
        "done: {total} rounds, model in {}",
        last.display()
    ));
    Ok(ExitCode::SUCCESS)
}

fn print_round(ctx: &Ctx, report: &RoundReport, done: u32, total: u32) {
    let loss = report
        .mean_loss
        .map_or_else(|| "n/a".to_string(), |l| format!("{l:.3}"));
    ctx.ui.emit(
        "round",
        json!({
            "round": done,
            "rounds": total,
            "loss": report.mean_loss,
            "accepted": report.accepted.len(),
            "rejected": report.rejected.iter()
                .map(|(slot, r)| json!({ "slot": slot, "reason": reject_name(*r) }))
                .collect::<Vec<_>>(),
            "update_norm": report.update_norm,
        }),
    );
    ctx.ui.say(&format!(
        "round {done}/{total}  loss {loss}  accepted {}  rejected {}",
        report.accepted.len(),
        report.rejected.len()
    ));
    for (slot, r) in &report.rejected {
        ctx.ui.note(&format!(
            "  worker {slot}: delta left out ({})",
            reject_name(*r)
        ));
    }
}

async fn run_round(
    session: &mut Session<'_>,
    state: &mut TrainState,
    hub: &StoreHub,
    base: &Manifest,
    min_workers: u16,
    timeout: Option<Duration>,
) -> anyhow::Result<RoundReport> {
    let tasks: Vec<TrainTask> = state.round_tasks(state.round());
    let mut inputs = Vec::with_capacity(tasks.len());
    for task in &tasks {
        let input = state.task_input(task);
        let payload = if input.len() <= MAX_INLINE_INPUT {
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
        inputs.push(payload);
    }
    let job = session
        .handle
        .submit(JobSpec {
            manifest: base.clone(),
            inputs,
            compare: None,
        })
        .await
        .map_err(|e| fail(exit::ERROR, format!("cannot submit round: {e}")))?;
    let by_index: BTreeMap<u32, &TrainTask> = tasks
        .iter()
        .map(|t| (u32::from(t.worker_slot), t))
        .collect();
    let mut seen: BTreeSet<u32> = BTreeSet::new();
    let deadline = timeout.map(|t| tokio::time::Instant::now() + t);
    let enough = |state: &TrainState| state.received() >= usize::from(min_workers);
    let mut expired = false;
    loop {
        if expired && enough(state) && seen.len() < tasks.len() {
            // Close the round: the remaining tasks are marked done so nobody runs them.
            for t in by_index.keys().filter(|t| !seen.contains(t)) {
                session
                    .handle
                    .mark_done(job, *t, Hash([0; 32]))
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            }
            break;
        }
        let timer = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending().await,
            }
        };
        let step = tokio::select! {
            s = session.next() => s?,
            () = timer, if !expired => {
                expired = true;
                continue;
            }
        };
        match step {
            Step::Accepted {
                job: j,
                task,
                output,
            } if j == job => {
                if !seen.insert(task) {
                    continue;
                }
                let Some(t) = by_index.get(&task) else {
                    continue;
                };
                if let Err(e) = state.ingest(t, &output) {
                    session.ctx.ui.note(&format!(
                        "worker {task}: unusable output ({e}); left out of the round"
                    ));
                }
                if seen.len() == tasks.len() {
                    break;
                }
            }
            Step::JobDone { job: j } if j == job => break,
            _ => {}
        }
    }
    if !enough(state) {
        return Err(fail(
            exit::ERROR,
            format!(
                "round {}: only {} of {min_workers} required deltas arrived",
                state.round() + 1,
                state.received()
            ),
        ));
    }
    let report = state
        .finish_round()
        .map_err(|e| fail(exit::ERROR, format!("round {}: {e}", state.round() + 1)))?;
    session.forget(job).await?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nowhere(_: &str) -> Option<PathBuf> {
        None
    }

    #[test]
    fn wasm_trainers_always_declare_f32() {
        assert_eq!(
            job_codec(TrainerKind::Wasm, CodecArg::Int8),
            DeltaCodec::F32
        );
        assert_eq!(job_codec(TrainerKind::Wasm, CodecArg::F32), DeltaCodec::F32);
        assert_eq!(
            job_codec(TrainerKind::Exec, CodecArg::Int8),
            DeltaCodec::Int8Ef
        );
        assert_eq!(job_codec(TrainerKind::Exec, CodecArg::F32), DeltaCodec::F32);
    }

    #[test]
    fn demo_train_prepares_from_the_embedded_goldens() {
        let args = Args {
            spec: PathBuf::from(DEMO_TRAIN),
            out: None,
            no_resume: false,
            local_slots: None,
            approve: false,
            wait_peers: 0,
            round_timeout: None,
            min_workers: None,
            inner_lr: None,
            codec: CodecArg::Int8,
            seed: None,
            deadline: None,
            qr_svg: None,
        };
        let p = prepare_demo(&args, nowhere).expect("prepare demo:train");
        assert_eq!(p.module.as_deref(), Some(GOLDEN_TOY_TRAINER));
        assert_eq!(p.assets.len(), 1);
        assert_eq!(p.assets[0].1, GOLDEN_OPTDIGITS);
        assert!(p.init.layout().total() > 0);
        assert_eq!(p.spec.trainer, TrainerKind::Wasm);

        // One NTR1 round step of the embedded trainer through nuee-exec, ingested by nuee-train.
        let runner =
            nuee_exec::WasmRunner::new(nuee_exec::RunnerConfig::default()).expect("runner");
        let hash = runner.prepare(GOLDEN_TOY_TRAINER).expect("prepare").hash;
        let mut state = TrainState::new(p.spec.clone(), p.init.clone(), 3).expect("state");
        let task = state.round_tasks(0).remove(0);
        let out = runner
            .run(
                &hash,
                &nuee_proto::build_envelope(&[GOLDEN_OPTDIGITS], &state.task_input(&task)),
                &nuee_exec::RunLimits {
                    deadline: Duration::from_secs(60),
                    max_memory: p.max_memory,
                    max_output: 1 << 20,
                },
            )
            .expect("NTR1 step");
        state.ingest(&task, &out).expect("ingest the delta");
    }
}
