//! `nuee run`: coordinate a job, run tasks locally, print the link workers join with.
//!
//! Also home of [`WasmExec`], the native `TaskExec` used by `run` (local slots) and `join`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use bytes::Bytes;
use nuee_core::{CoordinatorConfig, Event, JobSpec, Outcome};
use nuee_exec::{NativeRunner, RunLimits, RunnerConfig, WasmRunner};
use nuee_net::driver::BoxFut;
use nuee_net::{CoordinatorDriver, DriverConfig, HostEvent, TaskExec};
use nuee_proto::limits::{DEFAULT_DEADLINE_MS, MAX_ASSET, MAX_INLINE_INPUT};
use nuee_proto::{
    ALPN_JOB, Asset, BoundedBytes, BoundedString, BoundedVec, ByeReason, Comparator, EndpointId,
    FailKind, Hash, JobKind, JobLink, JobToml, Lease, Link, LocalLimits, Manifest, ModuleInfo,
    PROTO_VERSION, Payload, TaskKind, VerifyMode, VerifyPolicy,
};
use nuee_store::{HubConfig, StoreHub};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::mpsc;

use crate::{Ctx, exit, fail};
use nuee_proto::job_toml::{CompareKind, ExecToml, MergeKind, VerifyChoice};
use nuee_proto::{EnvVar, ExecSpec};

pub mod jobfile;
mod llmjob;

/// Golden mandelbrot module, the fallback when `target/tasks/mandelbrot.wasm` is absent.
pub const GOLDEN_MANDELBROT: &[u8] = include_bytes!("../../../../tasks/golden/mandelbrot.wasm");
/// Golden toy trainer module for `demo:train` (fallback for `target/tasks/toy-trainer.wasm`).
pub const GOLDEN_TOY_TRAINER: &[u8] = include_bytes!("../../../../tasks/golden/toy-trainer.wasm");
/// Golden optdigits dataset for `demo:train` (fallback for `target/tasks/optdigits.bin`).
pub const GOLDEN_OPTDIGITS: &[u8] = include_bytes!("../../../../tasks/golden/optdigits.bin");
/// The skycmd-arena module, embedded only by builds with the `embed-tasks` feature (releases).
#[cfg(feature = "embed-tasks")]
pub const EMBEDDED_SKYCMD_ARENA: Option<&[u8]> =
    Some(include_bytes!("../../../../target/tasks/skycmd-arena.wasm"));
/// The skycmd-arena module, embedded only by builds with the `embed-tasks` feature (releases).
#[cfg(not(feature = "embed-tasks"))]
pub const EMBEDDED_SKYCMD_ARENA: Option<&[u8]> = None;

/// BLAKE3 of the s3-pico decision model used by `demo:skycmd`.
pub const S3_PICO_BLAKE3: &str = "1b0963f9f364246e01fdf2dc84b7383d95f05dda252103c4a7b9de8e2c0b1d11";
/// Where `demo:skycmd` downloads the model when the repository copy is absent.
pub const S3_PICO_URL: &str = "https://maxgfr.github.io/skycmd/models/s3-pico.gguf";

/// Verification modes on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum VerifyArg {
    /// Every task runs twice
    Full,
    /// A sample is recomputed, plus canaries
    Sample,
    /// No verification
    None,
}

/// Arguments of `nuee run`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// A job TOML file, `demo:mandelbrot`, `demo:mandelbrot-gpu`, `demo:skycmd`, `demo:skycmd-dagger`, or a `.wasm` module (with --inputs)
    pub spec: String,
    /// Directory of task inputs for a `.wasm` module (one file per task, sorted by name)
    #[arg(long, value_name = "DIR")]
    pub inputs: Option<PathBuf>,
    /// Verification mode
    #[arg(long, value_enum)]
    pub verify: Option<VerifyArg>,
    /// Share of results recomputed in sample mode, 0..=1 (default 0.10)
    #[arg(long, value_name = "FRACTION")]
    pub sample: Option<f64>,
    /// Share of known-answer canary tasks in sample mode, 0..=1 (default 0.03)
    #[arg(long, value_name = "FRACTION")]
    pub canary: Option<f64>,
    /// Tasks this machine runs at once (default: half the cores); with 0 it takes no work
    /// but still recomputes the samples and mismatches that check peers
    #[arg(long, value_name = "N")]
    pub local_slots: Option<u16>,
    /// Approve each joining peer by hand
    #[arg(long)]
    pub approve: bool,
    /// Output directory (default ./nuee-out/<job name>)
    #[arg(long, value_name = "DIR")]
    pub out: Option<PathBuf>,
    /// Ignore outputs of an interrupted run and start over
    #[arg(long)]
    pub no_resume: bool,
    /// Task deadline, such as 30s or 5m (default 60s)
    #[arg(long, value_name = "DURATION")]
    pub deadline: Option<String>,
    /// Let browsers exchange data directly (WebRTC)
    #[arg(long)]
    pub webrtc: bool,
    /// Also write the link's QR code to this SVG file
    #[arg(long, value_name = "FILE")]
    pub qr_svg: Option<PathBuf>,
    /// Wait until this many peers have joined before handing out tasks
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub wait_peers: u16,
    /// Also run this job's native command here when it comes from this repository revision
    /// (repo@rev); the command is the one in the job file, nothing else runs
    #[arg(long, value_name = "REPO@REV")]
    pub allow_exec: Vec<String>,
    /// Fetch the --allow-exec repository URL from a local clone instead (offline or air-gapped
    /// machines; every --allow-exec entry must name URL)
    #[arg(long, value_name = "URL=PATH", requires = "allow_exec")]
    pub exec_mirror: Option<String>,
    /// Also score `llm` tasks here with this local OpenAI-compatible endpoint
    #[arg(long, value_name = "URL")]
    pub llm: Option<String>,
    /// Hand the job to this anchor (its link, or `-` for stdin) and return at once
    #[arg(long, value_name = "LINK")]
    pub anchor: Option<String>,
    /// With --anchor: wait for the job and download its results into --out
    #[arg(long, requires = "anchor")]
    pub follow: bool,
}

// ---------------------------------------------------------------------------------------------
// Local execution.

/// A lease run by [`WasmExec`], reported to whoever watches.
#[derive(Debug, Clone, Copy)]
pub struct Ran {
    /// Lease id.
    pub lease: u64,
    /// Job number.
    pub job: u32,
    /// Task index.
    pub task: u32,
    /// Wall time.
    pub ms: u64,
    /// Finished with an output.
    pub ok: bool,
}

/// Runs WASM leases with wasmtime on blocking threads, within this node's local limits.
pub struct WasmExec {
    runner: Arc<WasmRunner>,
    limits: LocalLimits,
    observer: Option<mpsc::UnboundedSender<Ran>>,
    native: Option<Arc<NativeRunner>>,
    /// Error-feedback state of exec trainer deltas (one residual, for the current job).
    train_enc: Arc<std::sync::Mutex<nuee_train::ExecEncoder>>,
    gpu: Option<Arc<nuee_gpu::GpuRunner>>,
    llm: Option<Arc<nuee_gpu::llm::LlmEndpoint>>,
    embed: Option<Arc<crate::cmd::embed::LocalEmbedder>>,
}

impl WasmExec {
    /// A runner with default limits; `observer` receives one [`Ran`] per lease.
    pub fn new(observer: Option<mpsc::UnboundedSender<Ran>>) -> anyhow::Result<WasmExec> {
        let runner = WasmRunner::new(RunnerConfig::default())
            .map_err(|e| anyhow::anyhow!("cannot start the WASM engine: {e}"))?;
        Ok(WasmExec {
            runner: Arc::new(runner),
            limits: LocalLimits::default(),
            observer,
            native: None,
            train_enc: Arc::default(),
            gpu: None,
            llm: None,
            embed: None,
        })
    }

    /// Also runs `exec` leases with `native` (only its allowlisted `repo@rev`).
    #[must_use]
    pub fn with_native(mut self, native: Arc<NativeRunner>) -> WasmExec {
        self.native = Some(native);
        self
    }

    /// Also runs `embed` leases with `embedder`.
    #[must_use]
    pub fn with_embed(mut self, embedder: Arc<crate::cmd::embed::LocalEmbedder>) -> WasmExec {
        self.embed = Some(embedder);
        self
    }

    async fn exec_embed(&self, input: Bytes) -> Outcome {
        let Some(e) = &self.embed else {
            return Outcome::Fail(
                FailKind::Unsupported,
                "no embedding model on this node".into(),
            );
        };
        if input.len() > self.limits.max_input {
            return Outcome::Fail(FailKind::Invalid, "input above the local cap".into());
        }
        e.run(input, self.limits.max_output).await
    }

    /// Also runs `wgsl` leases on `gpu`.
    #[must_use]
    pub fn with_gpu(mut self, gpu: Arc<nuee_gpu::GpuRunner>) -> WasmExec {
        self.gpu = Some(gpu);
        self
    }

    async fn exec_wgsl(&self, lease: &Lease, module: Option<Bytes>, input: Bytes) -> Outcome {
        let Some(gpu) = self.gpu.clone() else {
            return Outcome::Fail(
                FailKind::Unsupported,
                "no GPU offered on this node (see --gpu)".into(),
            );
        };
        let Some(module) = module else {
            return Outcome::Fail(FailKind::Invalid, "lease without a shader".into());
        };
        if input.len() > self.limits.max_input {
            return Outcome::Fail(FailKind::Invalid, "input above the local cap".into());
        }
        let deadline = Duration::from_millis(u64::from(lease.deadline_ms.max(1)));
        let max_output = self.limits.max_output;
        let res = tokio::task::spawn_blocking(move || {
            let wgsl = std::str::from_utf8(&module)
                .map_err(|_| nuee_gpu::GpuError::invalid("the shader is not UTF-8"))?;
            let out = gpu.run(wgsl, &input, deadline)?;
            if out.len() > max_output {
                return Err(nuee_gpu::GpuError::invalid("output above the local cap"));
            }
            Ok(out)
        })
        .await;
        match res {
            Ok(Ok(out)) => Outcome::Ok(out),
            Ok(Err(e)) => Outcome::Fail(e.kind, e.detail),
            Err(e) => Outcome::Fail(FailKind::Io, format!("task thread: {e}")),
        }
    }

    /// Also runs `llm` leases against `endpoint`.
    #[must_use]
    pub fn with_llm(mut self, endpoint: Arc<nuee_gpu::llm::LlmEndpoint>) -> WasmExec {
        self.llm = Some(endpoint);
        self
    }

    async fn exec_llm(&self, lease: &Lease, input: Bytes) -> Outcome {
        let Some(llm) = &self.llm else {
            return Outcome::Fail(
                FailKind::Unsupported,
                "no LLM endpoint offered on this node (see --llm)".into(),
            );
        };
        if input.len() > self.limits.max_input {
            return Outcome::Fail(FailKind::Invalid, "input above the local cap".into());
        }
        let deadline = Duration::from_millis(u64::from(lease.deadline_ms.max(1)));
        match llm.run(&input, deadline).await {
            Ok(out) if out.len() > self.limits.max_output => {
                Outcome::Fail(FailKind::Invalid, "output above the local cap".into())
            }
            Ok(out) => Outcome::Ok(out),
            Err(e) => Outcome::Fail(e.kind, e.detail),
        }
    }

    async fn exec_native(&self, lease: &Lease, manifest: &Manifest, input: Bytes) -> Outcome {
        let (Some(native), Some(spec)) = (&self.native, &manifest.exec) else {
            return Outcome::Fail(
                FailKind::Unsupported,
                "native commands are disabled on this node (see --allow-exec)".into(),
            );
        };
        let deadline = Duration::from_millis(u64::from(lease.deadline_ms.max(1)));
        let res = if manifest.kind == TaskKind::TrainExec {
            let task = match nuee_train::TaskInput::decode(&input) {
                Ok(t) => t,
                Err(e) => return Outcome::Fail(FailKind::Invalid, e.to_string()),
            };
            let args = nuee_exec::TrainArgs {
                steps: task.steps,
                shard: task.shard,
                n_shards: task.n_shards,
                seed: task.seed,
            };
            let trained = match native.run_train(spec, task.params, args, deadline).await {
                Ok(out) => out,
                Err(e) => return Outcome::Fail(e.kind, e.detail),
            };
            return self.encode_train_output(manifest, &input, trained).await;
        } else {
            native.run(spec, &input, deadline).await
        };
        match res {
            Ok(out) => Outcome::Ok(out),
            Err(e) => Outcome::Fail(e.kind, e.detail),
        }
    }

    /// Turns an exec trainer's safetensors into the delta format of WASM trainers, with the
    /// job's codec (int8 with error feedback by default) and this node's residual for the job.
    async fn encode_train_output(
        &self,
        manifest: &Manifest,
        input: &Bytes,
        trained: Vec<u8>,
    ) -> Outcome {
        let Some(train) = &manifest.train else {
            return Outcome::Fail(
                FailKind::Invalid,
                "training job without train settings".into(),
            );
        };
        let (job, codec) = (manifest.job, train.codec);
        let enc = self.train_enc.clone();
        let input = input.clone();
        let res = tokio::task::spawn_blocking(move || {
            let task = nuee_train::TaskInput::decode(&input)?;
            let mut enc = enc
                .lock()
                .map_err(|_| nuee_train::TrainError::Task("delta encoder poisoned"))?;
            enc.encode(job, codec, task.params, &trained)
        })
        .await;
        match res {
            Ok(Ok(out)) => Outcome::Ok(out),
            Ok(Err(e)) => Outcome::Fail(FailKind::Invalid, format!("trainer output: {e}")),
            Err(e) => Outcome::Fail(FailKind::Io, format!("task thread: {e}")),
        }
    }

    async fn exec(
        &self,
        lease: &Lease,
        manifest: &Manifest,
        module: Option<Bytes>,
        input: Bytes,
    ) -> Outcome {
        if matches!(manifest.kind, TaskKind::Exec | TaskKind::TrainExec) {
            return self.exec_native(lease, manifest, input).await;
        }
        if manifest.kind == TaskKind::Llm {
            return self.exec_llm(lease, input).await;
        }
        if manifest.kind == TaskKind::Wgsl {
            return self.exec_wgsl(lease, module, input).await;
        }
        if manifest.kind == TaskKind::Embed {
            return self.exec_embed(input).await;
        }
        if !matches!(manifest.kind, TaskKind::Wasm | TaskKind::TrainWasm) {
            return Outcome::Fail(
                FailKind::Unsupported,
                format!("{:?} tasks are not supported by this node", manifest.kind),
            );
        }
        let Some(module) = module else {
            return Outcome::Fail(FailKind::Invalid, "lease without a module".into());
        };
        if module.len() > self.limits.max_module {
            return Outcome::Fail(FailKind::Invalid, "module above the local cap".into());
        }
        if input.len() > self.limits.max_input {
            return Outcome::Fail(FailKind::Invalid, "input above the local cap".into());
        }
        let limits = RunLimits {
            deadline: Duration::from_millis(u64::from(lease.deadline_ms.max(1))),
            max_memory: manifest.max_memory.min(self.limits.max_memory),
            max_output: self.limits.max_output,
        };
        let runner = self.runner.clone();
        let res = tokio::task::spawn_blocking(move || {
            let info = runner.prepare(&module)?;
            runner.run(&info.hash, &input, &limits)
        })
        .await;
        match res {
            Ok(Ok(out)) => Outcome::Ok(out),
            Ok(Err(e)) => Outcome::Fail(e.kind, e.detail),
            Err(e) => Outcome::Fail(FailKind::Io, format!("task thread: {e}")),
        }
    }
}

impl TaskExec for WasmExec {
    fn run<'a>(
        &'a self,
        lease: &'a Lease,
        manifest: &'a Manifest,
        module: Option<Bytes>,
        input: Bytes,
    ) -> BoxFut<'a, Outcome> {
        Box::pin(async move {
            let start = Instant::now();
            let out = self.exec(lease, manifest, module, input).await;
            if let Some(tx) = &self.observer {
                let ran = Ran {
                    lease: lease.lease,
                    job: lease.job,
                    task: lease.task,
                    ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                    ok: matches!(out, Outcome::Ok(_)),
                };
                if tx.send(ran).is_err() {
                    tracing::trace!("nobody watches finished leases");
                }
            }
            out
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Job preparation.

const MANDEL_SIZE: u32 = 1024;
const MANDEL_TILE: u32 = 64;
const MANDEL_ITER: u32 = 500;
const MANDEL_X0: f64 = -2.0;
const MANDEL_Y0: f64 = -1.25;
const MANDEL_SPAN: f64 = 2.5;

const SKY_GAMES: [&str; 4] = ["flappy", "rescue", "dodge", "doom"];
const SKY_SEEDS: u64 = 4;
const SKY_EPISODES: u32 = 10;
const SKY_TAU: f32 = 0.6;
const SKY_MAX_PIPES: u32 = 200;
/// Oracle-labelled decision records per `demo:skycmd-dagger` task.
const SKY_DAGGER_N: u32 = 4000;

enum Preset {
    Mandelbrot,
    MandelbrotGpu,
    Skycmd,
    Plain,
    /// A job file with its own kind, comparator and output merge.
    Job(Box<JobPlan>),
    /// Embeddings (`nuee embed` or a `kind = "embed"` job file), merged into vectors + meta.
    Embed(Box<EmbedPlan>),
}

/// Texts of an embedding job and the cosine its results must reach.
struct EmbedPlan {
    job: crate::cmd::embed::EmbedJob,
    min_cos: f32,
}

impl Preset {
    fn kind(&self) -> TaskKind {
        match self {
            Preset::MandelbrotGpu => TaskKind::Wgsl,
            Preset::Job(j) => j.kind,
            Preset::Embed(_) => TaskKind::Embed,
            _ => TaskKind::Wasm,
        }
    }

    fn compare(&self) -> Comparator {
        match self {
            Preset::MandelbrotGpu => nuee_gpu::wgsl::MANDELBROT_COMPARE,
            Preset::Job(j) => j.compare.clone(),
            Preset::Embed(e) => Comparator::Cosine { min: e.min_cos },
            _ => Comparator::Exact,
        }
    }
}

/// What a job file adds to a plain module run.
struct JobPlan {
    kind: TaskKind,
    exec: Option<ExecSpec>,
    compare: Comparator,
    merge: MergeKind,
    uid_field: Option<String>,
    /// Index of the first input line of each task (JSONL shards); 0 for other sources.
    first_lines: Vec<u64>,
}

struct Prepared {
    name: String,
    preset: Preset,
    module: Vec<u8>,
    info: ModuleInfo,
    assets: Vec<(String, Vec<u8>)>,
    inputs: Vec<Vec<u8>>,
    verify: VerifyPolicy,
    deadline_ms: Option<u64>,
    max_memory: Option<u64>,
    out: Option<PathBuf>,
}

/// Candidate locations of a repository file: the current directory, then the source tree
/// this binary was built from.
fn repo_candidates(rel: &str) -> [PathBuf; 2] {
    [
        PathBuf::from(rel),
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(rel),
    ]
}

pub(crate) fn find_repo_file(rel: &str) -> Option<PathBuf> {
    repo_candidates(rel).into_iter().find(|p| p.is_file())
}

/// Where demo presets look for repository files; tests swap in a lookup that finds nothing.
pub(crate) type RepoLookup = fn(&str) -> Option<PathBuf>;

/// A demo file: a fresh repository build wins, else the copy embedded in this binary.
pub(crate) fn repo_or_embedded(
    rel: &str,
    embedded: Option<&[u8]>,
    lookup: RepoLookup,
) -> anyhow::Result<Option<Vec<u8>>> {
    match lookup(rel) {
        Some(p) => read_file(&p).map(Some),
        None => Ok(embedded.map(<[u8]>::to_vec)),
    }
}

fn read_file(path: &Path) -> anyhow::Result<Vec<u8>> {
    std::fs::read(path)
        .map_err(|e| fail(exit::ERROR, format!("cannot read {}: {e}", path.display())))
}

fn validate(module: &[u8], what: &str) -> anyhow::Result<ModuleInfo> {
    nuee_proto::validate_module(module).map_err(|e| {
        fail(
            exit::ERROR,
            format!("{what} is not a valid task module: {e}"),
        )
    })
}

fn mandel_input(tx: u32, ty: u32) -> Vec<u8> {
    let step = MANDEL_SPAN / f64::from(MANDEL_SIZE);
    let x0 = MANDEL_X0 + f64::from(tx * MANDEL_TILE) * step;
    let y0 = MANDEL_Y0 + f64::from(ty * MANDEL_TILE) * step;
    let mut v = Vec::with_capacity(36);
    v.extend_from_slice(&x0.to_le_bytes());
    v.extend_from_slice(&y0.to_le_bytes());
    v.extend_from_slice(&step.to_le_bytes());
    v.extend_from_slice(&MANDEL_TILE.to_le_bytes());
    v.extend_from_slice(&MANDEL_TILE.to_le_bytes());
    v.extend_from_slice(&MANDEL_ITER.to_le_bytes());
    v
}

fn prepare_mandelbrot(lookup: RepoLookup) -> anyhow::Result<Prepared> {
    let module = repo_or_embedded(
        "target/tasks/mandelbrot.wasm",
        Some(GOLDEN_MANDELBROT),
        lookup,
    )?
    .unwrap_or_else(|| GOLDEN_MANDELBROT.to_vec());
    let info = validate(&module, "the mandelbrot module")?;
    let tiles = MANDEL_SIZE / MANDEL_TILE;
    let inputs = (0..tiles)
        .flat_map(|ty| (0..tiles).map(move |tx| mandel_input(tx, ty)))
        .collect();
    Ok(Prepared {
        name: "mandelbrot".into(),
        preset: Preset::Mandelbrot,
        module,
        info,
        assets: Vec::new(),
        inputs,
        verify: VerifyPolicy::default(),
        deadline_ms: None,
        max_memory: None,
        out: None,
    })
}

/// The mandelbrot image as WGSL tasks: same tiles, f32 on the GPU, u32 counts out.
fn prepare_mandelbrot_gpu() -> Prepared {
    let module = nuee_gpu::wgsl::MANDELBROT_WGSL.as_bytes().to_vec();
    let info = ModuleInfo {
        hash: Hash::of(&module),
        size: module.len(),
        min_memory: 0,
        max_memory: 256 << 20,
    };
    let step = MANDEL_SPAN / f64::from(MANDEL_SIZE);
    let tiles = MANDEL_SIZE / MANDEL_TILE;
    let inputs = (0..tiles)
        .flat_map(|ty| {
            (0..tiles).map(move |tx| {
                nuee_gpu::wgsl::mandelbrot_input(
                    (MANDEL_X0 + f64::from(tx * MANDEL_TILE) * step) as f32,
                    (MANDEL_Y0 + f64::from(ty * MANDEL_TILE) * step) as f32,
                    step as f32,
                    MANDEL_TILE,
                    MANDEL_TILE,
                    MANDEL_ITER,
                )
            })
        })
        .collect();
    Prepared {
        name: "mandelbrot-gpu".into(),
        preset: Preset::MandelbrotGpu,
        module,
        info,
        assets: Vec::new(),
        inputs,
        verify: VerifyPolicy::default(),
        deadline_ms: None,
        max_memory: None,
        out: None,
    }
}

async fn load_model() -> anyhow::Result<Vec<u8>> {
    let want: Hash = S3_PICO_BLAKE3
        .parse()
        .map_err(|_| anyhow::anyhow!("bad pinned model hash"))?;
    if let Some(path) = find_repo_file("tasks/skycmd-arena/testdata/s3-pico.gguf") {
        let bytes = read_file(&path)?;
        if Hash::of(&bytes) == want {
            return Ok(bytes);
        }
        tracing::warn!(
            "{} does not match the pinned hash; downloading",
            path.display()
        );
    }
    let resp = reqwest::get(S3_PICO_URL)
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| fail(exit::NETWORK, format!("cannot download {S3_PICO_URL}: {e}")))?;
    if resp.content_length().is_some_and(|n| n > MAX_ASSET as u64) {
        return Err(fail(exit::INTEGRITY, "the model is larger than allowed"));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| fail(exit::NETWORK, format!("cannot download {S3_PICO_URL}: {e}")))?;
    if Hash::of(&bytes) != want {
        return Err(fail(
            exit::INTEGRITY,
            format!("the downloaded model does not match BLAKE3 {S3_PICO_BLAKE3}"),
        ));
    }
    Ok(bytes.to_vec())
}

/// The skycmd-arena module: a repository build, else the one embedded by `embed-tasks`.
fn skycmd_module(lookup: RepoLookup) -> anyhow::Result<Vec<u8>> {
    repo_or_embedded("target/tasks/skycmd-arena.wasm", EMBEDDED_SKYCMD_ARENA, lookup)?.ok_or_else(
        || {
            fail(
                exit::ERROR,
                "this nuee build has no embedded skycmd-arena module (a source build without the \
                 `embed-tasks` feature) and target/tasks/skycmd-arena.wasm was not found: install a \
                 release binary, or run scripts/build-tasks.sh in a nuee checkout and run nuee from \
                 there (or build with `cargo build --release -p nuee --features embed-tasks`)",
            )
        },
    )
}

/// `demo:skycmd-dagger` task inputs: per game and seed, `SKY_DAGGER_N` states the model visits,
/// each labelled by the simulator's oracle (skycmd's DAgger data for sky-train fine-tuning).
fn dagger_inputs() -> Vec<Vec<u8>> {
    let mut inputs = Vec::new();
    for game in SKY_GAMES {
        for seed in 1..=SKY_SEEDS {
            let v = json!({
                "mode": "dagger",
                "game": game,
                "seed": seed,
                "n": SKY_DAGGER_N,
                "tau": SKY_TAU,
                "max_pipes": SKY_MAX_PIPES,
            });
            inputs.push(v.to_string().into_bytes());
        }
    }
    inputs
}

/// Exact verification, as for `demo:skycmd`; the records are merged in task order.
fn dagger_plan(tasks: usize) -> JobPlan {
    JobPlan {
        kind: TaskKind::Wasm,
        exec: None,
        compare: Comparator::Exact,
        merge: MergeKind::Jsonl,
        uid_field: None,
        first_lines: vec![0; tasks],
    }
}

async fn prepare_skycmd_dagger() -> anyhow::Result<Prepared> {
    let module = skycmd_module(find_repo_file)?;
    let info = validate(&module, "the skycmd-arena module")?;
    let model = load_model().await?;
    let inputs = dagger_inputs();
    Ok(Prepared {
        name: "skycmd-dagger".into(),
        preset: Preset::Job(Box::new(dagger_plan(inputs.len()))),
        module,
        info,
        assets: vec![("s3-pico.gguf".into(), model)],
        inputs,
        verify: VerifyPolicy::default(),
        deadline_ms: Some(10 * 60_000),
        max_memory: None,
        out: None,
    })
}

async fn prepare_skycmd() -> anyhow::Result<Prepared> {
    let module = skycmd_module(find_repo_file)?;
    let info = validate(&module, "the skycmd-arena module")?;
    let model = load_model().await?;
    let mut inputs = Vec::new();
    for game in SKY_GAMES {
        for seed in 1..=SKY_SEEDS {
            let v = json!({
                "mode": "eval",
                "game": game,
                "seed": seed,
                "episodes": SKY_EPISODES,
                "tau": SKY_TAU,
                "max_pipes": SKY_MAX_PIPES,
            });
            inputs.push(v.to_string().into_bytes());
        }
    }
    Ok(Prepared {
        name: "skycmd".into(),
        preset: Preset::Skycmd,
        module,
        info,
        assets: vec![("s3-pico.gguf".into(), model)],
        inputs,
        verify: VerifyPolicy::default(),
        deadline_ms: Some(10 * 60_000),
        max_memory: None,
        out: None,
    })
}

fn inputs_from_dir(dir: &Path) -> anyhow::Result<Vec<Vec<u8>>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| fail(exit::ERROR, format!("cannot read {}: {e}", dir.display())))?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<_, _>>()?;
    files.retain(|p| p.is_file());
    files.sort();
    if files.is_empty() {
        return Err(fail(
            exit::USAGE,
            format!("{} holds no input file", dir.display()),
        ));
    }
    files.iter().map(|p| read_file(p)).collect()
}

fn permille(fraction: f64, what: &str) -> anyhow::Result<u16> {
    if !(0.0..=1.0).contains(&fraction) {
        return Err(fail(exit::USAGE, format!("{what} must be between 0 and 1")));
    }
    // In range by the check above.
    Ok((fraction * 1000.0).round() as u16)
}

fn usage(e: impl std::fmt::Display) -> anyhow::Error {
    fail(exit::USAGE, e.to_string())
}

/// Verification policy of a job file.
pub(crate) fn job_verify(job: &JobToml) -> anyhow::Result<VerifyPolicy> {
    let mut verify = VerifyPolicy::default();
    if let Some(v) = job.verify {
        verify.mode = match v {
            VerifyChoice::Full => VerifyMode::Full,
            VerifyChoice::Sample => VerifyMode::Sample,
            VerifyChoice::None => VerifyMode::None,
        };
    }
    if let Some(s) = job.sample {
        verify.sample_permille = permille(s, "sample")?;
    }
    if let Some(c) = job.canary {
        verify.canary_permille = permille(c, "canary")?;
    }
    Ok(verify)
}

/// Time limit of an `[exec]` command (default 1 h), in milliseconds.
pub(crate) fn exec_timeout_ms(e: &ExecToml) -> anyhow::Result<u64> {
    match &e.timeout {
        Some(t) => nuee_proto::job_toml::parse_duration_ms(t).map_err(usage),
        None => Ok(3_600_000),
    }
}

/// The manifest's command for `[exec]`, run as `argv` with `env`.
pub(crate) fn exec_spec(
    e: &ExecToml,
    argv: &[String],
    env: &BTreeMap<String, String>,
) -> anyhow::Result<ExecSpec> {
    let s = |v: &str| {
        BoundedString::try_from(v).map_err(|_| usage(format!("[exec] `{v}` is too long")))
    };
    let argv = argv
        .iter()
        .map(|a| s(a))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let env = env
        .iter()
        .map(|(k, v)| {
            Ok(EnvVar {
                key: BoundedString::try_from(k.as_str())
                    .map_err(|_| usage(format!("[exec] env key `{k}` is too long")))?,
                value: s(v)?,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(ExecSpec {
        repo: BoundedString::try_from(e.repo.as_str())
            .map_err(|_| usage("[exec] repo is too long"))?,
        rev: BoundedString::try_from(e.rev.as_str()).map_err(|_| usage("[exec] rev is invalid"))?,
        argv: BoundedVec::try_from(argv).map_err(|_| usage("[exec] at most 64 arguments"))?,
        env: BoundedVec::try_from(env).map_err(|_| usage("[exec] at most 32 env entries"))?,
        timeout_s: u32::try_from(exec_timeout_ms(e)?.div_ceil(1000))
            .map_err(|_| usage("[exec] timeout is too long"))?,
        gpu: e.gpu,
    })
}

/// Comparator of a job file.
pub(crate) fn job_compare(job: &JobToml) -> anyhow::Result<Comparator> {
    let Some(c) = &job.compare else {
        return Ok(Comparator::Exact);
    };
    // Tolerances were checked finite and non-negative by the schema.
    Ok(match c.kind {
        CompareKind::Exact => Comparator::Exact,
        CompareKind::None => Comparator::None,
        CompareKind::Tolerance => Comparator::ElementTolerance {
            abs: c.abs.unwrap_or(0.0) as f32,
            rel: c.rel.unwrap_or(0.0) as f32,
            max_bad_permille: permille(c.max_bad.unwrap_or(0.0), "compare.max_bad")?,
        },
        CompareKind::Jsonl => Comparator::Jsonl {
            field: BoundedString::try_from(c.field.as_deref().unwrap_or_default())
                .map_err(|_| usage("`compare.field` must be at most 64 bytes"))?,
            tol: c.tol.unwrap_or(0.0) as f32,
        },
        CompareKind::Logits => Comparator::Logits {
            max_gap: c.max_gap.unwrap_or(0.0) as f32,
        },
        CompareKind::Cosine => Comparator::Cosine {
            min: c.min_cos.unwrap_or(f64::from(nuee_gpu::embed::TAU)) as f32,
        },
    })
}

/// The check the coordinator runs for comparators beyond byte equality.
pub(crate) fn checker(kind: TaskKind, compare: &Comparator) -> Option<Arc<dyn nuee_core::Compare>> {
    match compare {
        Comparator::Jsonl { field, tol } => Some(Arc::new(jobfile::JsonlCompare {
            field: field.as_str().to_owned(),
            tol: *tol,
        })),
        other => nuee_gpu::verify::comparator(kind, other),
    }
}

/// Task inputs of `[inputs]` and the first input line of each (JSONL shards only).
fn job_inputs(
    base: &Path,
    i: &nuee_proto::job_toml::InputsToml,
) -> anyhow::Result<(Vec<Vec<u8>>, Vec<u64>)> {
    if let Some(dir) = &i.dir {
        let inputs = inputs_from_dir(&base.join(dir))?;
        let firsts = vec![0; inputs.len()];
        return Ok((inputs, firsts));
    }
    if let Some(file) = &i.jsonl {
        let path = base.join(file);
        let text = read_file(&path)?;
        let per = usize::try_from(i.lines_per_task.unwrap_or(2000))?;
        let shards = jobfile::shard_lines(&text, per);
        if shards.is_empty() {
            return Err(usage(format!("{} holds no line", path.display())));
        }
        return Ok(shards.into_iter().map(|s| (s.bytes, s.first_line)).unzip());
    }
    let name = i.generate.as_deref().unwrap_or_default();
    let mut params = BTreeMap::new();
    for (k, v) in i.params.iter().flatten() {
        let x = v
            .as_float()
            .or_else(|| v.as_integer().map(|n| n as f64))
            .ok_or_else(|| usage(format!("`inputs.params.{k}` must be a number")))?;
        params.insert(k.clone(), x);
    }
    let inputs = jobfile::generate(name, i.count.unwrap_or(0), &params).map_err(usage)?;
    let firsts = vec![0; inputs.len()];
    Ok((inputs, firsts))
}

fn prepare_toml(path: &Path) -> anyhow::Result<Prepared> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| fail(exit::ERROR, format!("cannot read {}: {e}", path.display())))?;
    let job = JobToml::from_str_checked(&text).map_err(usage)?;
    match job.kind {
        JobKind::Wasm | JobKind::Exec => {}
        JobKind::Llm if job.sweep.is_empty() => {
            return llmjob::prepare(&job, path.parent().unwrap_or(Path::new(".")));
        }
        JobKind::Embed if job.sweep.is_empty() => {
            return prepare_embed_toml(&job, path.parent().unwrap_or(Path::new(".")));
        }
        JobKind::Wgsl if job.sweep.is_empty() => {
            return prepare_wgsl_toml(&job, path.parent().unwrap_or(Path::new(".")));
        }
        JobKind::Train => return Err(usage("training jobs run with `nuee train`")),
        other => {
            return Err(fail(
                exit::ERROR,
                format!("{other:?} job files are not available in this build"),
            ));
        }
    }
    if !job.sweep.is_empty() {
        return Err(usage(
            "this file has [[sweep]] entries: run it with `nuee sweep`",
        ));
    }
    let base = path.parent().unwrap_or(Path::new("."));
    let assets = match &job.wasm {
        Some(w) => job_assets(base, &w.assets)?,
        None => Vec::new(),
    };
    let (kind, module, info, exec) = match (&job.wasm, &job.exec) {
        (Some(wasm), _) => {
            let module = read_file(&base.join(&wasm.module))?;
            let info = validate(&module, &wasm.module)?;
            (TaskKind::Wasm, module, info, None)
        }
        (None, Some(e)) => {
            let info = ModuleInfo {
                hash: Hash::of(&[]),
                size: 0,
                min_memory: 0,
                max_memory: 0,
            };
            let spec = exec_spec(e, &e.argv, &e.env)?;
            (TaskKind::Exec, Vec::new(), info, Some(spec))
        }
        (None, None) => return Err(usage("job file: missing [wasm] or [exec] table")),
    };
    let (inputs, first_lines) = match &job.inputs {
        Some(i) => job_inputs(base, i)?,
        // A command without inputs runs once, with an empty $NUEE_IN.
        None => (vec![Vec::new()], vec![0]),
    };
    let verify = job_verify(&job)?;
    let deadline_ms = match (&job.deadline, &job.exec) {
        (None, Some(e)) => exec_timeout_ms(e)?,
        _ => job.deadline_ms().map_err(usage)?,
    };
    let max_memory = job.max_memory_bytes().map_err(usage)?;
    let output = job.output.as_ref();
    let out = output.and_then(|o| o.dir.as_ref()).map(|d| base.join(d));
    let plan = JobPlan {
        kind,
        exec,
        compare: job_compare(&job)?,
        merge: output.and_then(|o| o.merge).unwrap_or(MergeKind::None),
        uid_field: output.and_then(|o| o.uid_field.clone()),
        first_lines,
    };
    Ok(Prepared {
        name: job.name.clone(),
        preset: Preset::Job(Box::new(plan)),
        module,
        info,
        assets,
        inputs,
        verify,
        deadline_ms: Some(deadline_ms),
        max_memory,
        out,
    })
}

/// The `[wasm] assets` of a job file, read relative to `base` and named after their file
/// names (count and name uniqueness were checked with the job file).
pub(crate) fn job_assets(base: &Path, paths: &[String]) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::with_capacity(paths.len());
    for rel in paths {
        let path = base.join(rel);
        let size = std::fs::metadata(&path)
            .map_err(|e| usage(format!("asset {rel}: cannot read {}: {e}", path.display())))?
            .len();
        if size > MAX_ASSET as u64 {
            return Err(usage(format!(
                "asset {rel} is {size} bytes, above the {MAX_ASSET}-byte limit"
            )));
        }
        let bytes = read_file(&path)?;
        if bytes.len() > MAX_ASSET {
            return Err(usage(format!(
                "asset {rel} is above the {MAX_ASSET}-byte limit"
            )));
        }
        out.push((nuee_proto::job_toml::asset_name(rel).to_owned(), bytes));
    }
    Ok(out)
}

fn prepare_wasm(path: &Path, inputs: Option<&Path>) -> anyhow::Result<Prepared> {
    let dir = inputs.ok_or_else(|| {
        fail(
            exit::USAGE,
            "a .wasm module needs --inputs <dir> (one file per task)",
        )
    })?;
    let module = read_file(path)?;
    let info = validate(&module, &path.display().to_string())?;
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("job")
        .chars()
        .take(64)
        .collect();
    Ok(Prepared {
        name,
        preset: Preset::Plain,
        module,
        info,
        assets: Vec::new(),
        inputs: inputs_from_dir(dir)?,
        verify: VerifyPolicy::default(),
        deadline_ms: None,
        max_memory: None,
        out: None,
    })
}

async fn prepare(args: &Args) -> anyhow::Result<Prepared> {
    let spec = args.spec.as_str();
    let mut p = match spec {
        "demo:mandelbrot" => prepare_mandelbrot(find_repo_file)?,
        "demo:mandelbrot-gpu" => prepare_mandelbrot_gpu(),
        "demo:skycmd" => prepare_skycmd().await?,
        "demo:skycmd-dagger" => prepare_skycmd_dagger().await?,
        s if s.starts_with("demo:") => {
            return Err(fail(
                exit::USAGE,
                format!(
                    "unknown demo {s:?} (try demo:mandelbrot, demo:mandelbrot-gpu, demo:skycmd or \
                     demo:skycmd-dagger)"
                ),
            ));
        }
        s if s.ends_with(".wasm") => prepare_wasm(Path::new(s), args.inputs.as_deref())?,
        s if s.ends_with(".toml") => prepare_toml(Path::new(s))?,
        s => {
            return Err(fail(
                exit::USAGE,
                format!(
                    "cannot run {s:?}: expected a job .toml file, a .wasm module or demo:mandelbrot"
                ),
            ));
        }
    };
    apply_args(args, &mut p)?;
    Ok(p)
}

/// Command-line overrides of a prepared job.
fn apply_args(args: &Args, p: &mut Prepared) -> anyhow::Result<()> {
    if let Some(v) = args.verify {
        p.verify.mode = match v {
            VerifyArg::Full => VerifyMode::Full,
            VerifyArg::Sample => VerifyMode::Sample,
            VerifyArg::None => VerifyMode::None,
        };
    }
    if let Some(s) = args.sample {
        p.verify.sample_permille = permille(s, "--sample")?;
    }
    if let Some(c) = args.canary {
        p.verify.canary_permille = permille(c, "--canary")?;
    }
    if let Some(d) = &args.deadline {
        let ms = nuee_proto::job_toml::parse_duration_ms(d)
            .map_err(|e| fail(exit::USAGE, format!("--deadline: {e}")))?;
        p.deadline_ms = Some(ms);
    }
    if p.inputs.is_empty() {
        return Err(fail(exit::USAGE, "the job has no task"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Outputs and resume.

const RESUME_FILE: &str = ".nuee-resume.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DoneTask {
    blake3: String,
    by: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Resume {
    job_key: String,
    tasks: BTreeMap<u32, DoneTask>,
}

fn task_file(out: &Path, task: u32) -> PathBuf {
    out.join(format!("task-{task}.bin"))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn job_key(p: &Prepared) -> String {
    let mut buf = Vec::with_capacity(32 * (1 + p.assets.len() + p.inputs.len()));
    buf.extend_from_slice(p.info.hash.as_bytes());
    for (_, a) in &p.assets {
        buf.extend_from_slice(Hash::of(a).as_bytes());
    }
    for i in &p.inputs {
        buf.extend_from_slice(Hash::of(i).as_bytes());
    }
    if let Preset::Job(j) = &p.preset {
        // Another command or comparator is another job.
        let what = format!("{:?}/{:?}", j.exec, j.compare);
        buf.extend_from_slice(Hash::of(what.as_bytes()).as_bytes());
    }
    Hash::of(&buf).to_string()
}

/// An embedding job over the texts of `job`, whose rows must agree to `min_cos`.
fn embed_prepared(
    name: &str,
    job: crate::cmd::embed::EmbedJob,
    min_cos: f32,
) -> anyhow::Result<Prepared> {
    let inputs = job.tasks()?;
    if inputs.is_empty() {
        return Err(usage("no text to embed"));
    }
    Ok(Prepared {
        name: name.to_owned(),
        preset: Preset::Embed(Box::new(EmbedPlan { job, min_cos })),
        module: Vec::new(),
        info: ModuleInfo {
            hash: Hash::of(&[]),
            size: 0,
            min_memory: 0,
            max_memory: 0,
        },
        assets: Vec::new(),
        inputs,
        verify: VerifyPolicy::default(),
        deadline_ms: None,
        max_memory: None,
        out: None,
    })
}

/// A `kind = "embed"` job file: texts from `[inputs] jsonl` (one per line) or the text files
/// of `[inputs] dir`, chunked like `nuee embed`.
fn prepare_embed_toml(job: &JobToml, base: &Path) -> anyhow::Result<Prepared> {
    if let Some(e) = &job.embed {
        let m = e.model.as_str();
        if m != "all-MiniLM-L6-v2" && m != nuee_gpu::embed::MINILM.repo {
            return Err(usage(format!(
                "[embed] model {m:?} is not available (use \"all-MiniLM-L6-v2\")"
            )));
        }
        if e.max_tokens
            .is_some_and(|t| t as usize != nuee_gpu::embed::MAX_TOKENS)
        {
            return Err(usage(format!(
                "[embed] max_tokens must be {}",
                nuee_gpu::embed::MAX_TOKENS
            )));
        }
    }
    let Some(i) = &job.inputs else {
        return Err(usage("`[inputs]` is required for this kind"));
    };
    let texts = match (&i.jsonl, &i.dir) {
        (Some(file), _) => crate::cmd::embed::EmbedJob::from_jsonl(&base.join(file))?,
        (None, Some(dir)) => crate::cmd::embed::EmbedJob::from_paths(&[base.join(dir)])?.0,
        (None, None) => {
            return Err(usage(
                "an embed job reads `[inputs] jsonl` or `[inputs] dir`",
            ));
        }
    };
    let min_cos = match job_compare(job)? {
        Comparator::Exact => nuee_gpu::embed::TAU,
        Comparator::Cosine { min } => min,
        other => {
            return Err(usage(format!(
                "an embed job compares by cosine, not {other:?}"
            )));
        }
    };
    let mut p = embed_prepared(&job.name, texts, min_cos)?;
    p.verify = job_verify(job)?;
    p.deadline_ms = Some(match job.deadline {
        Some(_) => job.deadline_ms().map_err(usage)?,
        None => 5 * 60_000,
    });
    let output = job.output.as_ref();
    p.out = output.and_then(|o| o.dir.as_ref()).map(|d| base.join(d));
    Ok(p)
}

/// A `kind = "wgsl"` job file: `[wgsl] shader` and one ready NWG1 input per file of
/// `[inputs] dir`, compared with `[compare]`.
fn prepare_wgsl_toml(job: &JobToml, base: &Path) -> anyhow::Result<Prepared> {
    let Some(w) = &job.wgsl else {
        return Err(usage("job file: missing [wgsl] table"));
    };
    if w.entry.is_some() || w.workgroup.is_some() {
        return Err(usage(
            "[wgsl] entry and workgroup are set by each NWG1 input, not by the job file",
        ));
    }
    let Some(dir) = job.inputs.as_ref().and_then(|i| i.dir.as_ref()) else {
        return Err(usage(
            "a wgsl job reads its NWG1 inputs from `[inputs] dir`",
        ));
    };
    let module = read_file(&base.join(&w.shader))?;
    if module.len() > nuee_gpu::wgsl::MAX_SOURCE {
        return Err(usage(format!("{} is larger than 1 MiB", w.shader)));
    }
    let wgsl =
        std::str::from_utf8(&module).map_err(|_| usage(format!("{} is not UTF-8", w.shader)))?;
    let inputs = inputs_from_dir(&base.join(dir))?;
    for (n, input) in inputs.iter().enumerate() {
        nuee_gpu::wgsl::parse_input(input)
            .map_err(|e| usage(format!("input {n} of {dir}: {}", e.detail)))?;
    }
    if let Some(first) = inputs.first()
        && let Ok(task) = nuee_gpu::wgsl::parse_input(first)
    {
        nuee_gpu::wgsl::check_shader(wgsl, &task)
            .map_err(|e| usage(format!("{}: {}", w.shader, e.detail)))?;
    }
    let output = job.output.as_ref();
    let plan = JobPlan {
        kind: TaskKind::Wgsl,
        exec: None,
        compare: job_compare(job)?,
        merge: output.and_then(|o| o.merge).unwrap_or(MergeKind::None),
        uid_field: None,
        first_lines: vec![0; inputs.len()],
    };
    Ok(Prepared {
        name: job.name.clone(),
        preset: Preset::Job(Box::new(plan)),
        info: ModuleInfo {
            hash: Hash::of(&module),
            size: module.len(),
            min_memory: 0,
            max_memory: 256 << 20,
        },
        module,
        assets: Vec::new(),
        inputs,
        verify: job_verify(job)?,
        deadline_ms: Some(job.deadline_ms().map_err(usage)?),
        max_memory: None,
        out: output.and_then(|o| o.dir.as_ref()).map(|d| base.join(d)),
    })
}

/// The local candle embedder of an embedding job, downloaded and checked before the link is
/// shown. Embeddings already use every core, so one local slot by default.
async fn local_embedder(
    args: &Args,
    prepared: &Prepared,
    local_slots: &mut u16,
    ctx: &Ctx,
) -> anyhow::Result<Option<Arc<crate::cmd::embed::LocalEmbedder>>> {
    if prepared.preset.kind() != TaskKind::Embed {
        return Ok(None);
    }
    *local_slots = args.local_slots.unwrap_or(1);
    // Without local slots the model still checks the peers' vectors.
    if *local_slots == 0 && prepared.verify.mode == VerifyMode::None {
        return Ok(None);
    }
    let e = Arc::new(crate::cmd::embed::LocalEmbedder::new(&ctx.data_dir()?));
    ctx.ui
        .note("checking the embedding model (downloaded once, about 90 MB)");
    let device = e.prepare().await?;
    ctx.ui.note(&format!(
        "embedding locally with all-MiniLM-L6-v2 on the {device}"
    ));
    Ok(Some(e))
}

/// The `--llm` endpoint for an llm job; without one, only peers score it.
fn local_llm(
    args: &Args,
    prepared: &mut Prepared,
    local_slots: &mut u16,
    ctx: &Ctx,
) -> anyhow::Result<Option<Arc<nuee_gpu::llm::LlmEndpoint>>> {
    let Some(url) = &args.llm else {
        if prepared.preset.kind() == TaskKind::Llm {
            *local_slots = 0;
            if prepared.verify.mode == VerifyMode::Sample {
                prepared.verify.mode = VerifyMode::Full;
            }
            ctx.ui
                .note("no --llm here: peers score every record (samples go to a second peer)");
        }
        return Ok(None);
    };
    let ep = nuee_gpu::llm::LlmEndpoint::new(url).map_err(|e| usage(e.detail))?;
    Ok(Some(Arc::new(ep)))
}

/// The native runner of `--allow-exec` entries; native commands stay off without any. Only the
/// exact `commands` of the operator's own job file whose `repo@rev` is allowed may run.
pub(crate) fn local_native(
    allow_exec: &[String],
    exec_mirror: Option<&str>,
    commands: &[&nuee_proto::ExecSpec],
    ctx: &Ctx,
) -> anyhow::Result<Option<Arc<NativeRunner>>> {
    if allow_exec.is_empty() {
        return Ok(None);
    }
    let mut allow = Vec::with_capacity(allow_exec.len());
    for entry in allow_exec {
        let parsed: nuee_exec::AllowEntry = entry
            .parse()
            .map_err(|e| usage(format!("--allow-exec {entry}: {e}")))?;
        if !allow.contains(&parsed) {
            allow.push(parsed);
        }
    }
    let mut policy = nuee_exec::ExecPolicy::new(ctx.data_dir()?.join("exec"));
    policy.repo_source = exec_mirror_source(exec_mirror, &allow)?;
    approve_own_commands(&mut policy, &allow, commands)?;
    policy.allow = allow;
    let runner = NativeRunner::new(policy);
    if runner.sandbox() == nuee_exec::Sandbox::None {
        ctx.ui
            .note("warning: no sandbox-exec or bwrap here; native commands will run unconfined");
    }
    Ok(Some(Arc::new(runner)))
}

/// Approves the job file's own commands whose `repo@rev` the operator allowed.
pub(crate) fn approve_own_commands(
    policy: &mut nuee_exec::ExecPolicy,
    allow: &[nuee_exec::AllowEntry],
    commands: &[&nuee_proto::ExecSpec],
) -> anyhow::Result<()> {
    for spec in commands {
        if allow
            .iter()
            .any(|a| a.repo == spec.repo.as_str() && a.rev == spec.rev.as_str())
        {
            policy
                .approve(spec)
                .map_err(|e| usage(format!("exec command: {e}")))?;
        }
    }
    Ok(())
}

/// Where `--allow-exec` repositories are fetched from: their `https://` URL, or with
/// `--exec-mirror URL=PATH` the local clone at PATH. A mirror stands in for one repository, so
/// every allowlisted entry must name URL.
pub(crate) fn exec_mirror_source(
    mirror: Option<&str>,
    allow: &[nuee_exec::AllowEntry],
) -> anyhow::Result<nuee_exec::RepoSource> {
    let Some(mirror) = mirror else {
        return Ok(nuee_exec::RepoSource::Https);
    };
    let (url, path) = mirror
        .split_once('=')
        .filter(|(u, p)| !u.is_empty() && !p.is_empty())
        .ok_or_else(|| usage(format!("--exec-mirror {mirror}: expected URL=PATH")))?;
    if let Some(other) = allow.iter().find(|a| a.repo != url) {
        return Err(usage(format!(
            "--exec-mirror {url}: --allow-exec {other} names another repository"
        )));
    }
    let path = Path::new(path);
    if !path.is_dir() {
        return Err(usage(format!(
            "--exec-mirror: {} is not a directory",
            path.display()
        )));
    }
    let path = path
        .canonicalize()
        .map_err(|e| usage(format!("--exec-mirror {}: {e}", path.display())))?;
    Ok(nuee_exec::RepoSource::LocalMirror(path))
}

/// A job name as one safe path component: letters, digits, `-`, `_` and `.` are kept, anything
/// else becomes `_`; a name of dots only (`.`, `..`) or an empty one never names a directory
/// outside where it is joined.
pub(crate) fn file_name(name: &str) -> String {
    let out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        "job".to_owned()
    } else if out.chars().all(|c| c == '.') {
        out.replace('.', "_")
    } else {
        out
    }
}

/// The default output directory of a job: `nuee-out/<name>` with the name as one component.
pub(crate) fn default_out(name: &str) -> PathBuf {
    Path::new("nuee-out").join(file_name(name))
}

/// Writes the merged output of a finished job file; rewritten whole on every completion, so
/// a resumed run produces the same file.
fn merge_outputs(
    out: &Path,
    name: &str,
    total: usize,
    plan: &JobPlan,
) -> anyhow::Result<Option<PathBuf>> {
    if plan.merge == MergeKind::None {
        return Ok(None);
    }
    let mut outputs = Vec::with_capacity(total);
    for task in 0..total {
        let task = u32::try_from(task)?;
        let bytes = std::fs::read(task_file(out, task))
            .with_context(|| format!("reading the output of task {task}"))?;
        outputs.push(bytes);
    }
    let (merged, path) = if plan.merge == MergeKind::Concat {
        (
            outputs.concat(),
            out.join(format!("{}.out", file_name(name))),
        )
    } else {
        let parts: Vec<(u64, &[u8])> = outputs
            .iter()
            .enumerate()
            .map(|(i, b)| (plan.first_lines.get(i).copied().unwrap_or(0), b.as_slice()))
            .collect();
        let merged = jobfile::merge_jsonl(&parts, plan.uid_field.as_deref())
            .map_err(|e| fail(exit::ERROR, format!("cannot merge the outputs: {e}")))?;
        (merged, out.join(format!("{}.jsonl", file_name(name))))
    };
    write_atomic(&path, &merged)?;
    Ok(Some(path))
}

/// Outputs of an earlier run of the same job that are still intact on disk.
fn load_resume(out: &Path, key: &str) -> Resume {
    let path = out.join(RESUME_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Resume {
            job_key: key.to_owned(),
            tasks: BTreeMap::new(),
        };
    };
    let mut resume: Resume = match serde_json::from_str(&text) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("ignoring {}: {e}", path.display());
            Resume::default()
        }
    };
    if resume.job_key != key {
        return Resume {
            job_key: key.to_owned(),
            tasks: BTreeMap::new(),
        };
    }
    resume.tasks.retain(|task, done| {
        std::fs::read(task_file(out, *task))
            .is_ok_and(|bytes| Hash::of(&bytes).to_string() == done.blake3)
    });
    resume
}

fn save_resume(out: &Path, resume: &Resume) -> anyhow::Result<()> {
    write_atomic(&out.join(RESUME_FILE), &serde_json::to_vec_pretty(resume)?)
}

fn write_results(
    out: &Path,
    name: &str,
    total: usize,
    coordinator: EndpointId,
    resume: &Resume,
) -> anyhow::Result<PathBuf> {
    let outputs: Vec<_> = resume
        .tasks
        .iter()
        .map(|(task, d)| {
            json!({
                "task": task,
                "file": format!("task-{task}.bin"),
                "blake3": d.blake3,
                "by": d.by,
            })
        })
        .collect();
    let doc = json!({
        "job": name,
        "tasks": total,
        "done": resume.tasks.len(),
        "coordinator": coordinator.to_string(),
        "outputs": outputs,
    });
    let path = out.join("results.json");
    write_atomic(&path, &serde_json::to_vec_pretty(&doc)?)?;
    Ok(path)
}

fn palette(n: u16) -> [u8; 3] {
    let max = MANDEL_ITER as u16;
    if n >= max {
        return [0, 0, 0];
    }
    let t = (f64::from(n) / f64::from(max)).sqrt();
    let u = 1.0 - t;
    let c = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    [
        c(9.0 * u * t * t * t),
        c(15.0 * u * u * t * t),
        c(8.5 * u * u * u * t),
    ]
}

/// The image of a mandelbrot job whose outputs hold `elem`-byte counts (2: CPU, 4: GPU).
fn write_ppm(out: &Path, elem: usize) -> anyhow::Result<PathBuf> {
    let size = MANDEL_SIZE as usize;
    let tile = MANDEL_TILE as usize;
    let tiles = size / tile;
    let mut pixels = vec![0u8; size * size * 3];
    for ty in 0..tiles {
        for tx in 0..tiles {
            let task = u32::try_from(ty * tiles + tx)?;
            let bytes = std::fs::read(task_file(out, task))
                .with_context(|| format!("reading the output of task {task}"))?;
            if bytes.len() != tile * tile * elem {
                return Err(anyhow::anyhow!(
                    "task {task} output has {} bytes, expected {}",
                    bytes.len(),
                    tile * tile * elem
                ));
            }
            for (i, px) in bytes.chunks_exact(elem).enumerate() {
                let mut le = [0u8; 4];
                le[..elem].copy_from_slice(px);
                let n = u16::try_from(u32::from_le_bytes(le)).unwrap_or(u16::MAX);
                let (row, col) = (ty * tile + i / tile, tx * tile + i % tile);
                let at = (row * size + col) * 3;
                pixels[at..at + 3].copy_from_slice(&palette(n));
            }
        }
    }
    let mut ppm = format!("P6\n{size} {size}\n255\n").into_bytes();
    ppm.extend_from_slice(&pixels);
    let path = out.join("mandelbrot.ppm");
    write_atomic(&path, &ppm)?;
    Ok(path)
}

fn skycmd_summary(ctx: &Ctx, out: &Path, total: usize) -> anyhow::Result<()> {
    #[derive(Default)]
    struct Row {
        runs: u32,
        score: f64,
        completion: f64,
        crashes: f64,
    }
    let mut rows: BTreeMap<String, Row> = BTreeMap::new();
    for task in 0..total {
        let task = u32::try_from(task)?;
        let bytes = std::fs::read(task_file(out, task))
            .with_context(|| format!("reading the output of task {task}"))?;
        let v: serde_json::Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("task {task} output is not JSON"))?;
        let game = v["game"].as_str().unwrap_or("?").to_owned();
        let m = &v["metrics"];
        let row = rows.entry(game).or_default();
        row.runs += 1;
        row.score += m["mean_score"].as_f64().unwrap_or(0.0);
        row.completion += m["completion_rate"].as_f64().unwrap_or(0.0);
        row.crashes += m["crashes"].as_f64().unwrap_or(0.0);
    }
    ctx.ui.say(&format!(
        "{:<8} {:>5} {:>11} {:>11} {:>8}",
        "game", "runs", "mean score", "completion", "crashes"
    ));
    for (game, r) in &rows {
        let n = f64::from(r.runs.max(1));
        ctx.ui.emit(
            "summary",
            json!({
                "game": game,
                "runs": r.runs,
                "mean_score": r.score / n,
                "completion_rate": r.completion / n,
                "crashes": r.crashes,
            }),
        );
        ctx.ui.say(&format!(
            "{:<8} {:>5} {:>11.2} {:>11.2} {:>8}",
            game,
            r.runs,
            r.score / n,
            r.completion / n,
            r.crashes
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The run itself.

fn payload(
    hub_inputs: &mut Vec<(usize, Vec<u8>)>,
    index: usize,
    input: Vec<u8>,
) -> Option<Payload> {
    if input.len() <= MAX_INLINE_INPUT {
        BoundedBytes::try_from(input).ok().map(Payload::Inline)
    } else {
        hub_inputs.push((index, input));
        None
    }
}

/// Runs a job.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let prepared = prepare(&args).await?;
    run_prepared(args, prepared, ctx).await
}

/// Runs the texts of `job` as an embedding job named `name`.
pub(crate) async fn run_embed(
    args: Args,
    name: &str,
    job: crate::cmd::embed::EmbedJob,
    ctx: &Ctx,
) -> anyhow::Result<ExitCode> {
    let mut prepared = embed_prepared(name, job, nuee_gpu::embed::TAU)?;
    prepared.deadline_ms = Some(5 * 60_000);
    apply_args(&args, &mut prepared)?;
    run_prepared(args, prepared, ctx).await
}

async fn run_prepared(args: Args, mut prepared: Prepared, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    if args.approve && !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return Err(fail(
            exit::USAGE,
            "--approve needs an interactive terminal to answer y/N",
        ));
    }
    if let Some(anchor) = &args.anchor {
        return run_on_anchor(anchor, &args, prepared, ctx).await;
    }
    let out = args
        .out
        .clone()
        .or_else(|| prepared.out.clone())
        .unwrap_or_else(|| default_out(&prepared.name));
    std::fs::create_dir_all(&out)
        .map_err(|e| fail(exit::ERROR, format!("cannot create {}: {e}", out.display())))?;
    let key = job_key(&prepared);
    let mut resume = if args.no_resume {
        Resume {
            job_key: key.clone(),
            tasks: BTreeMap::new(),
        }
    } else {
        load_resume(&out, &key)
    };
    let total = prepared.inputs.len();
    let mut local_slots = args.local_slots.unwrap_or_else(crate::default_slots);
    let mut gpu = None;
    if prepared.preset.kind() == TaskKind::Wgsl {
        gpu = tokio::task::spawn_blocking(nuee_gpu::GpuRunner::new).await?;
        match &gpu {
            Some(g) => {
                local_slots = args.local_slots.unwrap_or(1);
                ctx.ui.note(&format!(
                    "recomputing samples on the GPU {}",
                    g.caps().adapter.as_str()
                ));
            }
            None => {
                local_slots = 0;
                prepared.verify.mode = VerifyMode::Full;
                ctx.ui
                    .note("no GPU here: every task runs on two peers (full verification)");
            }
        }
    }
    let llm = local_llm(&args, &mut prepared, &mut local_slots, ctx)?;
    let embedder = local_embedder(&args, &prepared, &mut local_slots, ctx).await?;
    let own: Vec<&nuee_proto::ExecSpec> = match &prepared.preset {
        Preset::Job(j) => j.exec.iter().collect(),
        _ => Vec::new(),
    };
    let native = local_native(&args.allow_exec, args.exec_mirror.as_deref(), &own, ctx)?;
    let mut exec_here = true;
    if let Preset::Job(j) = &prepared.preset
        && let Some(spec) = &j.exec
        && !native.as_ref().is_some_and(|n| n.allowed(spec))
    {
        local_slots = 0;
        exec_here = false;
        ctx.ui.note(&format!(
            "only peers run this command (allow it here with --allow-exec {}@{})",
            spec.repo.as_str(),
            spec.rev.as_str()
        ));
    }
    let checks_here = match prepared.preset.kind() {
        TaskKind::Wgsl => gpu.is_some(),
        TaskKind::Llm => llm.is_some(),
        TaskKind::Embed => embedder.is_some(),
        _ => exec_here,
    };
    let verify_slots = u16::from(checks_here);
    if !checks_here && prepared.verify.mode != VerifyMode::None {
        ctx.ui.emit(
            "guarantee",
            json!({ "coordinator_checks": false, "kind": format!("{:?}", prepared.preset.kind()) }),
        );
        ctx.ui.note(
            "this machine cannot run these tasks, so it cannot check them: results are only \
             compared between two peers, which one person holding the link can fake",
        );
    }

    let net = ctx.online_net().await?;
    let hub = StoreHub::memory(&net, HubConfig::default());
    let module_hash = hub
        .add_bytes(Bytes::from(prepared.module.clone()))
        .await
        .map_err(crate::store_failure)?;
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
    let mut inputs = Vec::with_capacity(total);
    let mut big = Vec::new();
    for (i, input) in prepared.inputs.iter().enumerate() {
        inputs.push(payload(&mut big, i, input.clone()));
    }
    for (i, bytes) in big {
        let size = bytes.len() as u64;
        let hash = hub
            .add_bytes(Bytes::from(bytes))
            .await
            .map_err(crate::store_failure)?;
        inputs[i] = Some(Payload::Blob { hash, size });
    }
    let inputs: Vec<Payload> = inputs
        .into_iter()
        .collect::<Option<_>>()
        .ok_or_else(|| anyhow::anyhow!("an input could not be encoded"))?;

    let manifest = Manifest {
        version: PROTO_VERSION,
        job: 0,
        name: BoundedString::try_from(prepared.name.as_str())
            .map_err(|_| fail(exit::USAGE, "job name longer than 64 bytes"))?,
        kind: prepared.preset.kind(),
        module: (!matches!(
            prepared.preset.kind(),
            TaskKind::Exec | TaskKind::Llm | TaskKind::Embed
        ))
        .then_some(module_hash),
        module_size: u32::try_from(prepared.module.len())?,
        max_memory: prepared.max_memory.unwrap_or(prepared.info.max_memory),
        assets: BoundedVec::try_from(assets).map_err(|_| anyhow::anyhow!("too many assets"))?,
        tasks: u32::try_from(total)?,
        verify: prepared.verify,
        compare: prepared.preset.compare(),
        deadline_ms: u32::try_from(
            prepared
                .deadline_ms
                .unwrap_or(u64::from(DEFAULT_DEADLINE_MS)),
        )
        .map_err(|_| fail(exit::USAGE, "deadline too long"))?,
        exec: match &prepared.preset {
            Preset::Job(j) => j.exec.clone(),
            _ => None,
        },
        train: None,
        coordinator: net.id(),
    };
    manifest
        .check()
        .map_err(|e| fail(exit::USAGE, e.to_string()))?;

    let secret = nuee_proto::new_secret16().context("system random source")?;
    let seed_bytes = nuee_proto::new_secret16().context("system random source")?;
    let seed = u64::from_le_bytes(seed_bytes[..8].try_into()?);
    let core_cfg = CoordinatorConfig {
        name: manifest.name.clone(),
        local_slots,
        verify_slots,
        approve: args.approve,
        seed,
        webrtc: args.webrtc,
        ..CoordinatorConfig::default()
    };
    let mut exec = WasmExec::new(None)?;
    if let Some(native) = native {
        exec = exec.with_native(native);
    }
    if let Some(gpu) = gpu {
        exec = exec.with_gpu(Arc::new(gpu));
    }
    if let Some(e) = embedder {
        exec = exec.with_embed(e);
    }
    if let Some(llm) = llm {
        exec = exec.with_llm(llm);
    }
    let exec = Arc::new(exec);
    let (driver, mut handle) = CoordinatorDriver::spawn(
        net.clone(),
        secret,
        core_cfg,
        DriverConfig {
            local_slots,
            ..DriverConfig::default()
        },
        exec,
        Arc::new(hub.clone()),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    // Keeps replicas of shared files on the store members of this swarm (`share --on`).
    let durable_path = if ctx.global.ephemeral {
        None
    } else {
        Some(nuee_store::durable::DurabilityService::default_path(
            &ctx.data_dir()?,
        ))
    };
    let durability = nuee_store::durable::DurabilityService::new(
        &net,
        secret,
        crate::durable_config(),
        durable_path,
    )
    .map_err(crate::store_failure)?;
    // Lost erasure shards are rebuilt and served from a second endpoint, since this node's
    // nuee/job/0 runs the compute swarm.
    let _rebuild_serve = {
        let mut key = [0u8; 32];
        key[..16].copy_from_slice(&nuee_proto::new_secret16().context("system random source")?);
        key[16..].copy_from_slice(&nuee_proto::new_secret16().context("system random source")?);
        let rebuild_net = nuee_net::Net::bind(nuee_net::NetConfig {
            secret: nuee_proto::SecretKey::from_bytes(&key),
            relay: ctx.relay_choice()?,
            relay_only: ctx.global.relay_only,
            alpns: Vec::new(),
        })
        .await
        .map_err(|e| fail(exit::NETWORK, e.to_string()))?;
        ctx.keep(&rebuild_net);
        let rebuild_hub = StoreHub::memory(&rebuild_net, HubConfig::default());
        let acceptor = nuee_store::FilesAcceptor::new(&rebuild_hub, "rebuild", Vec::new());
        durability.set_rebuilder(rebuild_hub.clone(), acceptor.clone());
        nuee_store::ServeHandle::spawn(&rebuild_net, &rebuild_hub, acceptor)
    };
    // nuee/pin/0 follows the job driver's decisions: approvals and bans.
    durability.require_approval(args.approve);
    let _durability_task = durability.start();
    let router = net
        .router()
        .accept(ALPN_JOB, driver)
        .accept(iroh_blobs::ALPN, hub.protocol())
        .accept(nuee_proto::ALPN_PIN, durability.clone())
        .spawn();

    let link = Link::Job(JobLink {
        coordinator: net.id(),
        relay: BoundedString::try_from(Ctx::relay_string(&net).as_str())
            .map_err(|_| anyhow::anyhow!("relay URL too long for a link"))?,
        secret,
    });
    let url = link.to_url(&ctx.global.web_base);
    ctx.ui
        .show_link("job", &url, Some(&net.id()), args.qr_svg.as_deref())?;
    ctx.ui.note(&format!(
        "{} tasks, verify {:?}, {} local slots, outputs in {}",
        total,
        manifest.verify.mode,
        local_slots,
        out.display()
    ));

    let compare = checker(manifest.kind, &manifest.compare);
    let spec = JobSpec {
        manifest,
        inputs,
        compare,
    };
    let mut pending_spec = Some(spec);
    let mut job: Option<u32> = None;
    if args.wait_peers == 0 {
        job = Some(submit(&handle, &mut pending_spec, &resume).await?);
    } else {
        ctx.ui
            .note(&format!("waiting for {} peer(s) to join", args.wait_peers));
    }

    let bar = ctx.ui.bar(total as u64, "tasks");
    bar.set_position(resume.tasks.len() as u64);
    let mut peers: BTreeSet<EndpointId> = BTreeSet::new();
    let mut banned = 0u32;
    let stop = crate::ctrl_c();
    tokio::pin!(stop);
    let outcome: anyhow::Result<u32> = loop {
        let ev = tokio::select! {
            ev = handle.events.recv() => ev,
            () = &mut stop => break Err(fail(exit::CANCELLED, format!(
                "interrupted; {} of {total} outputs kept in {} (run again to resume)",
                resume.tasks.len(), out.display()))),
        };
        let Some(ev) = ev else {
            break Err(anyhow::anyhow!("the coordinator stopped unexpectedly"));
        };
        match ev {
            HostEvent::Accepted {
                job: j,
                task,
                output,
                output_hash,
                by,
                ms,
            } if Some(j) == job => {
                // `ms` is measured by this coordinator from lease to result, so a worker
                // cannot report a better time than it took.
                ctx.ui.emit(
                    "task",
                    json!({ "job": j, "task": task, "by": by.to_string(), "ms": ms }),
                );
                write_atomic(&task_file(&out, task), &output)?;
                resume.tasks.insert(
                    task,
                    DoneTask {
                        blake3: output_hash.to_string(),
                        by: by.to_string(),
                    },
                );
                save_resume(&out, &resume)?;
            }
            HostEvent::Accepted { .. } => {}
            HostEvent::Event(e) => match e {
                Event::PeerJoined { peer, caps } => {
                    peers.insert(peer);
                    durability.member_joined(peer, &caps);
                    ctx.ui.emit(
                        "peer",
                        json!({ "id": peer.to_string(), "state": "joined",
                                "agent": caps.agent.as_str(), "slots": caps.cpu_slots }),
                    );
                    ctx.ui.bar_line(&bar, &peer_joined_line(&peer, &caps));
                    if job.is_none() && peers.len() >= usize::from(args.wait_peers) {
                        job = Some(submit(&handle, &mut pending_spec, &resume).await?);
                    }
                }
                Event::PeerPending { peer, caps } => {
                    ctx.ui.emit(
                        "peer",
                        json!({ "id": peer.to_string(), "state": "pending",
                                "fingerprint": nuee_proto::fingerprint(&peer).to_vec() }),
                    );
                    let question = admit_question(&peer, &caps);
                    let yes =
                        bar.suspend(|| tokio::task::block_in_place(|| ctx.ui.confirm(&question)))?;
                    handle
                        .approve(peer, yes)
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                }
                Event::PeerLeft { peer } => {
                    peers.remove(&peer);
                    durability.member_left(&peer);
                    ctx.ui
                        .emit("peer", json!({ "id": peer.to_string(), "state": "left" }));
                }
                Event::Progress {
                    job: j,
                    done,
                    total: t,
                    verified,
                    rate_per_s,
                } => {
                    ctx.ui.emit(
                        "progress",
                        json!({ "job": j, "done": done, "total": t, "verified": verified,
                                "rate": rate_per_s, "peers": peers.len() }),
                    );
                    bar.set_position(u64::from(done));
                    bar.set_message(format!(
                        "verified {verified} · {} peers · {rate_per_s:.1}/s",
                        peers.len()
                    ));
                }
                Event::Verified { job: j, task, by } => {
                    ctx.ui.emit(
                        "verified",
                        json!({ "job": j, "task": task, "by": by.to_string() }),
                    );
                }
                Event::Mismatch { job: j, task, peer } => {
                    ctx.ui.emit(
                        "mismatch",
                        json!({ "job": j, "task": task, "peer": peer.to_string() }),
                    );
                }
                Event::Banned { peer, redo } => {
                    durability.peer_banned(peer);
                    banned += 1;
                    peers.remove(&peer);
                    ctx.ui
                        .emit("banned", json!({ "peer": peer.to_string(), "redo": redo }));
                    ctx.ui.bar_line(
                        &bar,
                        &format!(
                            "peer {peer} banned after a confirmed mismatch; {redo} tasks redone"
                        ),
                    );
                }
                Event::TaskFailed { job: j, task, kind } => {
                    ctx.ui.emit(
                        "task_failed",
                        json!({ "job": j, "task": task, "kind": format!("{kind:?}") }),
                    );
                    ctx.ui
                        .bar_line(&bar, &format!("task {task} failed for good ({kind:?})"));
                }
                Event::WaitingForChecker {
                    job: j,
                    task: _,
                    waiting,
                    kind,
                } => {
                    ctx.ui.emit(
                        "waiting_checker",
                        json!({ "job": j, "waiting": waiting, "kind": format!("{kind:?}") }),
                    );
                    ctx.ui.bar_line(
                        &bar,
                        &waiting_checker_line(waiting, kind, "restart with --verify none"),
                    );
                }
                Event::JobDone { job: j, failed } if Some(j) == job => break Ok(failed),
                Event::TaskAccepted { .. } | Event::JobDone { .. } | Event::Rtc { .. } => {}
            },
            HostEvent::Closed { .. } => {}
        }
    };
    bar.finish_and_clear();

    let results = write_results(&out, &prepared.name, total, net.id(), &resume)?;
    // Per-peer credit, taken before the peers are told goodbye. `by` in the results and
    // in `verified` events names only the execution that arrived first; these counters
    // also credit the second, agreeing one.
    let peer_credit = match handle.status().await {
        Ok(status) => peers_json(&status),
        Err(e) => {
            tracing::warn!("no per-peer counters for the done event: {e}");
            Vec::new()
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

    let failed = outcome?;
    if failed > 0 {
        return Err(fail(
            exit::ERROR,
            format!(
                "{failed} tasks failed for good; the others are in {}",
                out.display()
            ),
        ));
    }
    let mut extra = serde_json::Map::new();
    match prepared.preset {
        Preset::Mandelbrot | Preset::MandelbrotGpu => {
            let elem = if matches!(prepared.preset, Preset::MandelbrotGpu) {
                4
            } else {
                2
            };
            let ppm = write_ppm(&out, elem)?;
            extra.insert("image".into(), json!(ppm.display().to_string()));
            ctx.ui.say(&format!("image: {}", ppm.display()));
        }
        Preset::Skycmd => skycmd_summary(ctx, &out, total)?,
        Preset::Plain => {}
        Preset::Job(j) => {
            if let Some(merged) = merge_outputs(&out, &prepared.name, total, &j)? {
                extra.insert("merged".into(), json!(merged.display().to_string()));
                ctx.ui.say(&format!("merged: {}", merged.display()));
            }
        }
        Preset::Embed(e) => {
            let (vectors, meta) = e.job.write_outputs(&out)?;
            extra.insert("vectors".into(), json!(vectors.display().to_string()));
            extra.insert("meta".into(), json!(meta.display().to_string()));
            extra.insert("chunks".into(), json!(e.job.texts.len()));
            ctx.ui.say(&format!(
                "{} vectors: {} ({})",
                e.job.texts.len(),
                vectors.display(),
                meta.display()
            ));
        }
    }
    let mut done = json!({
        "job": prepared.name,
        "tasks": total,
        "out": out.display().to_string(),
        "results": results.display().to_string(),
        "banned": banned,
        "peers": peer_credit,
    });
    if let serde_json::Value::Object(m) = &mut done {
        m.extend(extra);
    }
    ctx.ui.emit("done", done);
    ctx.ui.say(&format!(
        "done: {total} tasks, results in {}",
        results.display()
    ));
    Ok(ExitCode::SUCCESS)
}

/// The `peers` field of the `done` event: what each peer produced and how much of it
/// another execution confirmed.
fn peers_json(status: &nuee_core::SwarmStatus) -> Vec<serde_json::Value> {
    status
        .peers
        .iter()
        .map(|p| {
            json!({
                "id": p.id.to_string(),
                "browser": p.browser,
                "done": p.done,
                "verified": p.verified,
                "banned": p.banned,
            })
        })
        .collect()
}

/// The manifest entries of a job's assets (BLAKE3, size and name, in order).
fn manifest_assets(assets: &[(String, Vec<u8>)]) -> anyhow::Result<Vec<Asset>> {
    assets
        .iter()
        .map(|(name, bytes)| {
            Ok(Asset {
                hash: Hash::of(bytes),
                size: bytes.len() as u64,
                name: BoundedString::try_from(name.as_str())
                    .map_err(|_| anyhow::anyhow!("asset name too long"))?,
            })
        })
        .collect()
}

/// `run --anchor`: the anchor coordinates the job; this process only uploads it.
async fn run_on_anchor(
    anchor: &str,
    args: &Args,
    prepared: Prepared,
    ctx: &Ctx,
) -> anyhow::Result<ExitCode> {
    let link = crate::cmd::serve::anchor_link(anchor)?;
    let kind = prepared.preset.kind();
    let assets = manifest_assets(&prepared.assets)?;
    let manifest = Manifest {
        version: PROTO_VERSION,
        job: 0,
        name: BoundedString::try_from(prepared.name.as_str())
            .map_err(|_| fail(exit::USAGE, "job name longer than 64 bytes"))?,
        kind,
        module: (kind != TaskKind::Exec).then(|| Hash::of(&prepared.module)),
        module_size: u32::try_from(prepared.module.len())?,
        max_memory: prepared.max_memory.unwrap_or(prepared.info.max_memory),
        assets: BoundedVec::try_from(assets).map_err(|_| anyhow::anyhow!("too many assets"))?,
        tasks: u32::try_from(prepared.inputs.len())?,
        verify: prepared.verify,
        compare: prepared.preset.compare(),
        deadline_ms: u32::try_from(
            prepared
                .deadline_ms
                .unwrap_or(u64::from(DEFAULT_DEADLINE_MS)),
        )
        .map_err(|_| fail(exit::USAGE, "deadline too long"))?,
        exec: match &prepared.preset {
            Preset::Job(j) => j.exec.clone(),
            _ => None,
        },
        train: None,
        coordinator: link.host,
    };
    manifest
        .check()
        .map_err(|e| fail(exit::USAGE, e.to_string()))?;
    let out = args
        .out
        .clone()
        .or_else(|| prepared.out.clone())
        .unwrap_or_else(|| default_out(&prepared.name));
    let net = crate::cmd::serve::client_net(ctx, &link).await?;
    let job = crate::cmd::serve::AnchorJob {
        manifest,
        module: if kind == TaskKind::Exec {
            Vec::new()
        } else {
            prepared.module
        },
        assets: prepared.assets.into_iter().map(|(_, b)| b).collect(),
        inputs: prepared.inputs,
    };
    let res =
        crate::cmd::serve::submit_to_anchor(ctx, &link, &net, job, args.follow.then_some(out))
            .await;
    net.close().await;
    res
}

async fn submit(
    handle: &nuee_net::CoordinatorHandle,
    spec: &mut Option<JobSpec>,
    resume: &Resume,
) -> anyhow::Result<u32> {
    let spec = spec
        .take()
        .ok_or_else(|| anyhow::anyhow!("job already submitted"))?;
    let job = handle
        .submit(spec)
        .await
        .map_err(|e| fail(exit::ERROR, format!("cannot submit the job: {e}")))?;
    for (task, done) in &resume.tasks {
        let hash: Hash = done
            .blake3
            .parse()
            .map_err(|_| anyhow::anyhow!("bad hash in the resume file"))?;
        handle
            .mark_done(job, *task, hash)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    Ok(job)
}

/// The operator's question for a peer waiting for approval; the agent string is the peer's own
/// and is escaped before it reaches the terminal.
pub(crate) fn admit_question(peer: &EndpointId, caps: &nuee_proto::Caps) -> String {
    format!(
        "admit peer {peer} ({}, fingerprint {})?",
        crate::ui::clean(caps.agent.as_str()),
        nuee_proto::fingerprint_string(peer)
    )
}

/// The line shown when a peer joins, its agent string escaped.
pub(crate) fn peer_joined_line(peer: &EndpointId, caps: &nuee_proto::Caps) -> String {
    format!(
        "peer joined: {peer} ({})",
        crate::ui::clean(caps.agent.as_str())
    )
}

/// What to tell the user when tasks wait for a second peer able to run their kind.
/// `opt_out` says how to run without verification instead.
pub(crate) fn waiting_checker_line(waiting: u32, kind: TaskKind, opt_out: &str) -> String {
    let what = match kind {
        TaskKind::Exec | TaskKind::TrainExec => "with --allow-exec for this command",
        TaskKind::Wgsl => "with a GPU",
        TaskKind::Llm => "with --llm",
        TaskKind::Embed => "able to run embeddings",
        TaskKind::Wasm | TaskKind::TrainWasm => "",
    };
    let device = if what.is_empty() {
        "another device".to_owned()
    } else {
        format!("another device {what}")
    };
    let tasks = if waiting == 1 {
        "task waits"
    } else {
        "tasks wait"
    };
    format!(
        "{waiting} {tasks} for a second peer able to run them: this machine cannot check \
         {kind:?} results and no other admitted peer can run them. Join {device}, or {opt_out} \
         to accept unchecked results"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `done` event lists every peer with its own counters.
    #[test]
    fn done_event_lists_peer_credit() {
        let id = nuee_proto::SecretKey::from_bytes(&[5; 32]).public();
        let status = nuee_core::SwarmStatus {
            peers: vec![nuee_core::PeerStatus {
                id,
                browser: true,
                slots: 1,
                inflight: 0,
                done: 7,
                verified: 3,
                banned: false,
            }],
            jobs: Vec::new(),
        };
        assert_eq!(
            peers_json(&status),
            vec![json!({ "id": id.to_string(), "browser": true, "done": 7,
                         "verified": 3, "banned": false })]
        );
        assert!(peers_json(&nuee_core::SwarmStatus::default()).is_empty());
    }

    /// A job file names its job `../../x` or `/tmp/x`: the default output stays one directory
    /// inside `nuee-out`.
    #[test]
    fn job_names_stay_inside_nuee_out() {
        for name in [
            "../../x",
            "/tmp/x",
            "..",
            ".",
            "",
            "a/../../b",
            "C:\\x",
            "x\u{1b}[2J",
        ] {
            let out = default_out(name);
            let parts: Vec<_> = out.components().collect();
            assert_eq!(parts.len(), 2, "{name:?} -> {}", out.display());
            assert_eq!(parts[0], std::path::Component::Normal("nuee-out".as_ref()));
            assert!(
                matches!(parts[1], std::path::Component::Normal(_)),
                "{name:?} -> {}",
                out.display()
            );
        }
        assert_eq!(
            default_out("mandelbrot-1.2"),
            Path::new("nuee-out/mandelbrot-1.2")
        );
    }

    #[test]
    fn peer_strings_are_escaped_in_prompts() {
        let peer = nuee_proto::SecretKey::from_bytes(&[3; 32]).public();
        let caps = nuee_proto::Caps {
            agent: BoundedString::try_from("x\u{1b}[1A\u{1b}[2K\u{9b}y").expect("agent"),
            browser: false,
            cpu_slots: 1,
            gpu: None,
            mem_mb: 1024,
            kinds: 0,
            cached: BoundedVec::new(),
            exec_allow: BoundedVec::new(),
            store_quota_mb: 0,
            relay: None,
            rtc: false,
        };
        for line in [admit_question(&peer, &caps), peer_joined_line(&peer, &caps)] {
            assert!(!line.chars().any(char::is_control), "{line:?}");
            assert!(line.contains("x\\u{1b}[1A"), "{line}");
        }
    }

    #[test]
    fn waiting_checker_line_names_the_way_out() {
        let line = waiting_checker_line(3, TaskKind::Exec, "restart with --verify none");
        assert!(line.starts_with("3 tasks wait for a second peer"), "{line}");
        assert!(line.contains("--allow-exec"), "{line}");
        assert!(line.contains("--verify none"), "{line}");
        let one = waiting_checker_line(1, TaskKind::Wgsl, "restart with --verify none");
        assert!(one.starts_with("1 task waits"), "{one}");
        assert!(one.contains("with a GPU"), "{one}");
    }

    fn nowhere(_: &str) -> Option<PathBuf> {
        None
    }

    #[test]
    fn demo_mandelbrot_prepares_without_a_checkout() {
        let p = prepare_mandelbrot(nowhere).expect("prepare");
        assert_eq!(p.module, GOLDEN_MANDELBROT);
        assert_eq!(p.info.hash.to_string(), crate::cmd::selftest::GOLDEN_MODULE);
        assert!(!p.inputs.is_empty());
    }

    #[test]
    fn skycmd_module_without_checkout_or_embedding_says_why() {
        let r = skycmd_module(nowhere);
        match EMBEDDED_SKYCMD_ARENA {
            Some(m) => assert_eq!(r.expect("embedded module"), m),
            None => {
                let msg = format!("{:#}", r.expect_err("no module anywhere"));
                assert!(msg.contains("no embedded skycmd-arena"), "{msg}");
                assert!(msg.contains("embed-tasks"), "{msg}");
            }
        }
    }

    #[test]
    fn dagger_preset_inputs_and_merge() {
        let inputs = dagger_inputs();
        assert_eq!(inputs.len(), SKY_GAMES.len() * SKY_SEEDS as usize);
        for (i, input) in inputs.iter().enumerate() {
            let v: serde_json::Value = serde_json::from_slice(input).expect("json");
            assert_eq!(v["mode"], "dagger");
            assert_eq!(v["n"], SKY_DAGGER_N);
            assert_eq!(v["game"], SKY_GAMES[i / SKY_SEEDS as usize]);
            assert_eq!(v["seed"], (i % SKY_SEEDS as usize) as u64 + 1);
        }
        let plan = dagger_plan(3);
        assert_eq!(plan.compare, Comparator::Exact);
        let dir = tempfile::tempdir().expect("tempdir");
        for (task, text) in ["{\"r\":0}\n{\"r\":1}\n", "{\"r\":2}\n", "{\"r\":3}\n"]
            .iter()
            .enumerate()
        {
            std::fs::write(task_file(dir.path(), task as u32), text).expect("write");
        }
        let merged = merge_outputs(dir.path(), "skycmd-dagger", 3, &plan)
            .expect("merge")
            .expect("a merged file");
        assert_eq!(
            merged.file_name().and_then(|n| n.to_str()),
            Some("skycmd-dagger.jsonl")
        );
        assert_eq!(
            std::fs::read_to_string(merged).expect("read"),
            "{\"r\":0}\n{\"r\":1}\n{\"r\":2}\n{\"r\":3}\n"
        );
    }

    #[test]
    fn wasm_job_assets_are_read_hashed_and_bounded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path();
        std::fs::write(base.join("task.wasm"), GOLDEN_MANDELBROT).expect("module");
        std::fs::create_dir(base.join("models")).expect("dir");
        std::fs::write(base.join("models/a.gguf"), b"model a").expect("asset");
        std::fs::write(base.join("b.bin"), b"bee").expect("asset");
        std::fs::create_dir(base.join("in")).expect("inputs");
        std::fs::write(base.join("in/0"), b"x").expect("input");
        let job = |assets: &str| {
            format!(
                "name = \"t\"\nkind = \"wasm\"\n[wasm]\nmodule = \"task.wasm\"\nassets = [{assets}]\n[inputs]\ndir = \"in\"\n"
            )
        };
        let path = base.join("job.toml");
        std::fs::write(&path, job("\"models/a.gguf\", \"b.bin\"")).expect("job");
        let p = prepare_toml(&path).expect("prepare");
        let assets = manifest_assets(&p.assets).expect("assets");
        let got: Vec<(String, Hash, u64)> = assets
            .iter()
            .map(|a| (a.name.as_str().to_owned(), a.hash, a.size))
            .collect();
        assert_eq!(
            got,
            vec![
                ("a.gguf".to_owned(), Hash::of(b"model a"), 7),
                ("b.bin".to_owned(), Hash::of(b"bee"), 3),
            ]
        );

        let err = |assets: &str| {
            std::fs::write(&path, job(assets)).expect("job");
            match prepare_toml(&path) {
                Ok(_) => panic!("accepted {assets}"),
                Err(e) => format!("{e:#}"),
            }
        };
        assert!(err("\"missing.bin\"").contains("missing.bin"));
        assert!(err("\"b.bin\", \"models/../b.bin\"").contains("two files named"));
        let many: Vec<String> = (0..17).map(|i| format!("\"f{i}\"")).collect();
        assert!(err(&many.join(", ")).contains("at most 16"));
        let big = std::fs::File::create(base.join("big.bin")).expect("big");
        big.set_len(MAX_ASSET as u64 + 1).expect("sparse file");
        assert!(err("\"big.bin\"").contains("limit"));
    }

    /// Adds 0.01 * steps * (shard + 1) to every value and reports loss = 0.5; with
    /// `--garbage` it writes a file that is not safetensors.
    const TRAINER: &str = r#"import json, struct, sys
a = dict(zip(sys.argv[1::2], sys.argv[2::2]))
if "--garbage" in a:
    open(a["--out"], "wb").write(b"not safetensors")
    sys.exit(0)
raw = open(a["--init"], "rb").read()
n = struct.unpack("<Q", raw[:8])[0]
head = json.loads(raw[8:8 + n])
data = raw[8 + n:]
steps = int(a["--steps"])
shard, count = map(int, a["--shard"].split("/"))
out = {}
blob = b""
for name in sorted(k for k in head if k != "__metadata__"):
    t = head[name]
    b, e = t["data_offsets"]
    vals = struct.unpack("<%df" % ((e - b) // 4), data[b:e])
    vals = [v + 0.01 * steps * (shard + 1) * (1 + (i % 7)) for i, v in enumerate(vals)]
    enc = struct.pack("<%df" % len(vals), *vals)
    out[name] = {"dtype": "F32", "shape": t["shape"], "data_offsets": [len(blob), len(blob) + len(enc)]}
    blob += enc
out["__metadata__"] = {"nuee.loss": "0.5"}
h = json.dumps(out).encode()
h += b" " * ((8 - len(h) % 8) % 8)
open(a["--out"], "wb").write(struct.pack("<Q", len(h)) + h + blob)
"#;

    const REPO: &str = "https://example.invalid/nuee/trainer.git";

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn train_manifest(
        job: u32,
        rev: &str,
        codec: nuee_proto::DeltaCodec,
        extra: &[&str],
    ) -> Manifest {
        fn bs<const N: usize>(v: &str) -> BoundedString<N> {
            BoundedString::try_from(v).expect("bounded")
        }
        let mut argv: Vec<BoundedString<_>> = [
            "python3",
            "train.py",
            "--init",
            "$NUEE_INIT",
            "--steps",
            "$NUEE_STEPS",
            "--shard",
            "$NUEE_SHARD",
            "--seed",
            "$NUEE_SEED",
            "--out",
            "$NUEE_OUT",
        ]
        .iter()
        .map(|a| bs(a))
        .collect();
        argv.extend(extra.iter().map(|a| bs(a)));
        Manifest {
            version: PROTO_VERSION,
            job,
            name: bs("train"),
            kind: TaskKind::TrainExec,
            module: None,
            module_size: 0,
            max_memory: 0,
            assets: BoundedVec::new(),
            tasks: 1,
            verify: VerifyPolicy::default(),
            compare: Comparator::None,
            deadline_ms: 60_000,
            exec: Some(ExecSpec {
                repo: bs(REPO),
                rev: bs(rev),
                argv: BoundedVec::try_from(argv).expect("argv"),
                env: BoundedVec::new(),
                timeout_s: 60,
                gpu: false,
            }),
            train: Some(nuee_proto::TrainSpec {
                rounds: 1,
                inner_steps: 10,
                workers: 1,
                outer_lr: 0.7,
                outer_momentum: 0.9,
                init: Hash::of(b"init"),
                trainer: nuee_proto::TrainerKind::Exec,
                codec,
                trim: 0.2,
                cos_min: 0.0,
                norm_max_ratio: 3.0,
            }),
            coordinator: nuee_proto::SecretKey::from_bytes(&[1; 32]).public(),
        }
    }

    #[tokio::test]
    async fn exec_trainer_outputs_are_encoded_deltas() {
        use nuee_exec::native::{AllowEntry, ExecPolicy, RepoSource};
        use nuee_proto::DeltaCodec;
        use nuee_train::{Params, TaskInput, decode_delta, split_output};

        let dir = tempfile::tempdir().expect("tempdir");
        let mirror = dir.path().join("mirror");
        std::fs::create_dir(&mirror).expect("mkdir");
        std::fs::write(mirror.join("train.py"), TRAINER).expect("write");
        git(&mirror, &["init", "--quiet", "-b", "main"]);
        git(&mirror, &["add", "."]);
        git(
            &mirror,
            &[
                "-c",
                "user.name=f",
                "-c",
                "user.email=f@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "t",
            ],
        );
        let rev = git(&mirror, &["rev-parse", "HEAD"]);
        let mut policy = ExecPolicy::new(dir.path().join("cache"));
        policy.allow = vec![AllowEntry::new(REPO, &rev).expect("entry")];
        policy.repo_source = RepoSource::LocalMirror(mirror.clone());
        for extra in [&[][..], &["--garbage", "1"][..]] {
            let m = train_manifest(0, &rev, DeltaCodec::F32, extra);
            policy
                .approve(m.exec.as_ref().expect("exec"))
                .expect("approvable");
        }
        let exec = WasmExec::new(None)
            .expect("exec")
            .with_native(Arc::new(NativeRunner::new(policy)));

        let init = Params::new(vec![
            ("bias".into(), vec![4], vec![0.5; 4]),
            (
                "weight".into(),
                vec![64, 64],
                (0..4096).map(|i| i as f32 / 4096.0).collect(),
            ),
        ])
        .expect("params");
        let params = init.to_safetensors().expect("st");
        let input = Bytes::from(
            TaskInput {
                steps: 10,
                shard: 0,
                n_shards: 1,
                seed: 3,
                lr: 0.0,
                params: &params,
            }
            .encode(),
        );
        let lease = |job: u32| Lease {
            lease: u64::from(job),
            job,
            task: 0,
            kind: TaskKind::TrainExec,
            module: None,
            assets: BoundedVec::new(),
            input: Payload::Inline(BoundedBytes::new()),
            deadline_ms: 60_000,
        };
        let run = |job: u32, codec: DeltaCodec, extra: &'static [&'static str]| {
            let exec = &exec;
            let input = input.clone();
            let rev = rev.clone();
            async move {
                let m = train_manifest(job, &rev, codec, extra);
                exec.exec(&lease(job), &m, None, input).await
            }
        };

        let Outcome::Ok(f32_out) = run(1, DeltaCodec::F32, &[]).await else {
            panic!("f32 run failed");
        };
        let Outcome::Ok(int8_out) = run(2, DeltaCodec::Int8Ef, &[]).await else {
            panic!("int8 run failed");
        };
        assert_eq!(&int8_out[..5], b"NDL1\x01");
        let ratio = int8_out.len() as f64 / f32_out.len() as f64;
        assert!(
            ratio > 0.24 && ratio < 0.27,
            "int8 {} f32 {}",
            int8_out.len(),
            f32_out.len()
        );
        let (enc, loss) = split_output(&int8_out).expect("trailer");
        assert_eq!(loss, 0.5);
        let d = decode_delta(enc, &init.layout()).expect("decode");
        let (f, _) = split_output(&f32_out).expect("trailer");
        let exact = decode_delta(f, &init.layout()).expect("decode");
        for (a, b) in d.iter().zip(&exact) {
            assert!((a - b).abs() <= 0.7 / 127.0, "{a} vs {b}");
        }

        // A trainer that writes something else than safetensors is a failure, not a panic.
        match run(3, DeltaCodec::Int8Ef, &["--garbage", "1"]).await {
            Outcome::Fail(FailKind::Invalid, detail) => {
                assert!(detail.contains("trainer output"), "{detail}")
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }
}
