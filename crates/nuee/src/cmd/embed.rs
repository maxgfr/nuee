//! `nuee embed`: chunk text files, embed the chunks across the swarm, and write
//! `vectors.f32` (N x 384 f32 little-endian, in `meta.jsonl` order) and `meta.jsonl`.
//!
//! Also home of [`LocalEmbedder`], the candle runner `run` and `join` use for `embed` leases,
//! and of the text collection shared with `kind = "embed"` job files.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context as _;
use bytes::Bytes;
use nuee_core::Outcome;
use nuee_gpu::embed::{self, DIM, EmbedDevice, Embedder, MINILM, ModelError};
use nuee_proto::FailKind;
use serde::Serialize;

use clap::Parser as _;

use crate::cmd::run::{self, VerifyArg};
use crate::{Ctx, exit, fail};

/// Texts per task.
pub const TEXTS_PER_TASK: usize = 64;

/// Arguments of `nuee embed`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Text files or directories (read recursively; files that are not UTF-8 are skipped)
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<PathBuf>,
    /// Output directory (default ./nuee-out/embed)
    #[arg(long, value_name = "DIR")]
    pub out: Option<PathBuf>,
    /// Verification mode
    #[arg(long, value_enum)]
    pub verify: Option<VerifyArg>,
    /// Share of results recomputed in sample mode, 0..=1 (default 0.10)
    #[arg(long, value_name = "FRACTION")]
    pub sample: Option<f64>,
    /// Tasks this machine runs at once (default 1)
    #[arg(long, value_name = "N")]
    pub local_slots: Option<u16>,
    /// Approve each joining peer by hand
    #[arg(long)]
    pub approve: bool,
    /// Ignore outputs of an interrupted run and start over
    #[arg(long)]
    pub no_resume: bool,
    /// Task deadline, such as 30s or 5m (default 5m)
    #[arg(long, value_name = "DURATION")]
    pub deadline: Option<String>,
    /// Also write the link's QR code to this SVG file
    #[arg(long, value_name = "FILE")]
    pub qr_svg: Option<PathBuf>,
    /// Wait until this many peers have joined before handing out tasks
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub wait_peers: u16,
}

// ---------------------------------------------------------------------------------------------
// Texts and outputs.

/// Where an embedded text comes from; one `meta.jsonl` line.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ChunkMeta {
    /// Row in `vectors.f32`.
    pub id: u64,
    /// Source file.
    pub path: String,
    /// Line of a JSONL source (0-based).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    /// Chunk index within the file (or the line).
    pub chunk: u32,
    /// First byte of the chunk in the file (or the line's text).
    pub start: usize,
    /// One past its last byte.
    pub end: usize,
}

/// Texts to embed and where each comes from.
#[derive(Debug, Default)]
pub struct EmbedJob {
    /// One row per text.
    pub meta: Vec<ChunkMeta>,
    /// The texts.
    pub texts: Vec<String>,
}

impl EmbedJob {
    fn push(&mut self, path: &str, line: Option<u64>, text: &str) {
        for (chunk, c) in embed::chunk_text(text).into_iter().enumerate() {
            let Some(t) = text.get(c.start..c.end) else {
                continue;
            };
            self.meta.push(ChunkMeta {
                id: self.meta.len() as u64,
                path: path.to_owned(),
                line,
                chunk: u32::try_from(chunk).unwrap_or(u32::MAX),
                start: c.start,
                end: c.end,
            });
            self.texts.push(t.to_owned());
        }
    }

    /// Chunks of the UTF-8 files under `paths` (directories recursively, in name order).
    /// Files that are not UTF-8 or hold a NUL byte are skipped and returned apart.
    pub fn from_paths(paths: &[PathBuf]) -> anyhow::Result<(EmbedJob, Vec<PathBuf>)> {
        let mut files = Vec::new();
        for p in paths {
            collect(p, &mut files)?;
        }
        let mut job = EmbedJob::default();
        let mut skipped = Vec::new();
        for f in files {
            let bytes = std::fs::read(&f)
                .map_err(|e| fail(exit::ERROR, format!("cannot read {}: {e}", f.display())))?;
            match String::from_utf8(bytes) {
                Ok(text) if !text.contains('\0') => {
                    job.push(&f.display().to_string(), None, &text);
                }
                _ => skipped.push(f),
            }
        }
        Ok((job, skipped))
    }

    /// One text per line of a JSONL file: the `text` field of an object, else the raw line.
    pub fn from_jsonl(path: &Path) -> anyhow::Result<EmbedJob> {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            fail(
                exit::ERROR,
                format!("cannot read {} as UTF-8: {e}", path.display()),
            )
        })?;
        let name = path.display().to_string();
        let mut job = EmbedJob::default();
        for (n, line) in raw.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let text = match serde_json::from_str::<serde_json::Value>(line) {
                Ok(serde_json::Value::Object(o)) => match o.get("text") {
                    Some(serde_json::Value::String(s)) => s.clone(),
                    _ => {
                        return Err(fail(
                            exit::USAGE,
                            format!(
                                "{name}:{}: a JSON object needs a string `text` field",
                                n + 1
                            ),
                        ));
                    }
                },
                _ => line.to_owned(),
            };
            job.push(&name, Some(n as u64), &text);
        }
        Ok(job)
    }

    /// NEM1 task inputs, [`TEXTS_PER_TASK`] texts each.
    pub fn tasks(&self) -> anyhow::Result<Vec<Vec<u8>>> {
        self.texts
            .chunks(TEXTS_PER_TASK)
            .map(|c| embed::encode_input(c).map_err(|e| fail(exit::ERROR, e.detail)))
            .collect()
    }

    /// Joins the task outputs found in `out` into `vectors.f32` and `meta.jsonl`.
    pub fn write_outputs(&self, out: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
        let vectors = out.join("vectors.f32");
        let meta = out.join("meta.jsonl");
        let tmp = vectors.with_extension("tmp");
        let mut f = std::io::BufWriter::new(
            std::fs::File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?,
        );
        for (task, texts) in self.texts.chunks(TEXTS_PER_TASK).enumerate() {
            let bytes = std::fs::read(out.join(format!("task-{task}.bin")))
                .with_context(|| format!("reading the output of task {task}"))?;
            if bytes.len() != texts.len() * DIM * 4 {
                return Err(anyhow::anyhow!(
                    "task {task} output has {} bytes, expected {}",
                    bytes.len(),
                    texts.len() * DIM * 4
                ));
            }
            f.write_all(&bytes)?;
        }
        f.into_inner()
            .map_err(|e| anyhow::anyhow!("writing {}: {e}", tmp.display()))?
            .sync_all()?;
        std::fs::rename(&tmp, &vectors)?;
        let mut lines = Vec::new();
        for m in &self.meta {
            serde_json::to_writer(&mut lines, m)?;
            lines.push(b'\n');
        }
        let tmp = meta.with_extension("tmp");
        std::fs::write(&tmp, &lines).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &meta)?;
        Ok((vectors, meta))
    }
}

fn collect(path: &Path, files: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    let md = std::fs::symlink_metadata(path)
        .map_err(|e| fail(exit::USAGE, format!("cannot read {}: {e}", path.display())))?;
    if md.is_file() {
        files.push(path.to_owned());
    } else if md.is_dir() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(path)
            .map_err(|e| fail(exit::ERROR, format!("cannot read {}: {e}", path.display())))?
            .map(|e| e.map(|e| e.path()))
            .collect::<Result<_, _>>()?;
        entries.sort();
        for e in entries {
            let hidden = e
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'));
            if !hidden {
                collect(&e, files)?;
            }
        }
    }
    // Symbolic links and special files are not followed.
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Local execution.

/// The pinned MiniLM on this machine, downloaded and loaded on first use.
pub struct LocalEmbedder {
    root: PathBuf,
    cell: tokio::sync::OnceCell<Arc<Embedder>>,
}

impl LocalEmbedder {
    /// Models are cached under `<data dir>/models/`.
    pub fn new(data_dir: &Path) -> LocalEmbedder {
        LocalEmbedder {
            root: data_dir.join("models"),
            cell: tokio::sync::OnceCell::new(),
        }
    }

    /// Downloads what is missing, checks every file, and loads the model.
    pub async fn prepare(&self) -> anyhow::Result<&'static str> {
        MINILM.fetch(&self.root).await.map_err(|e| match e {
            ModelError::Integrity { .. } => fail(exit::INTEGRITY, e.to_string()),
            ModelError::Network { .. } => fail(exit::NETWORK, e.to_string()),
            ModelError::Io { .. } => fail(exit::ERROR, e.to_string()),
        })?;
        let e = self.get().await.map_err(|e| fail(exit::ERROR, e.detail))?;
        Ok(e.device_name())
    }

    async fn get(&self) -> Result<Arc<Embedder>, nuee_gpu::GpuError> {
        let root = self.root.clone();
        self.cell
            .get_or_try_init(|| async move {
                // On its own task, so a lease that gives up does not abort the download.
                tokio::spawn(async move {
                    Embedder::fetch_and_load(&root, EmbedDevice::Auto)
                        .await
                        .map(Arc::new)
                })
                .await
                .map_err(|e| nuee_gpu::GpuError::new(FailKind::Io, format!("loader task: {e}")))?
            })
            .await
            .cloned()
    }

    /// Runs one NEM1 lease.
    pub async fn run(&self, input: Bytes, max_output: usize) -> Outcome {
        let e = match self.get().await {
            Ok(e) => e,
            Err(e) => return Outcome::Fail(e.kind, e.detail),
        };
        let res = tokio::task::spawn_blocking(move || e.run(&input)).await;
        match res {
            Ok(Ok(out)) if out.len() > max_output => {
                Outcome::Fail(FailKind::Invalid, "output above the local cap".into())
            }
            Ok(Ok(out)) => Outcome::Ok(out),
            Ok(Err(e)) => Outcome::Fail(e.kind, e.detail),
            Err(e) => Outcome::Fail(FailKind::Io, format!("task thread: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The verb.

#[derive(clap::Parser)]
struct RunArgs {
    #[command(flatten)]
    args: run::Args,
}

/// Embeds the text files under `args.paths`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let (job, skipped) = EmbedJob::from_paths(&args.paths)?;
    for s in &skipped {
        ctx.ui
            .note(&format!("skipping {} (not UTF-8 text)", s.display()));
    }
    if job.texts.is_empty() {
        return Err(fail(
            exit::USAGE,
            "no text to embed: the paths hold no UTF-8 text file with content",
        ));
    }
    ctx.ui.note(&format!(
        "{} chunks in {} tasks",
        job.texts.len(),
        job.texts.len().div_ceil(TEXTS_PER_TASK)
    ));
    // The run flags mean the same here: parse them as `nuee run` would.
    let mut argv: Vec<std::ffi::OsString> = vec!["run".into(), "embed".into()];
    let mut flag = |name: &str, value: Option<String>| {
        if let Some(v) = value {
            argv.push(format!("--{name}").into());
            argv.push(v.into());
        }
    };
    flag(
        "verify",
        args.verify.map(|v| {
            match v {
                VerifyArg::Full => "full",
                VerifyArg::Sample => "sample",
                VerifyArg::None => "none",
            }
            .to_owned()
        }),
    );
    flag("sample", args.sample.map(|s| s.to_string()));
    flag("local-slots", args.local_slots.map(|n| n.to_string()));
    flag("deadline", args.deadline);
    flag("wait-peers", Some(args.wait_peers.to_string()));
    for (name, path) in [("out", args.out), ("qr-svg", args.qr_svg)] {
        if let Some(p) = path {
            argv.push(format!("--{name}").into());
            argv.push(p.into_os_string());
        }
    }
    for (name, on) in [("approve", args.approve), ("no-resume", args.no_resume)] {
        if on {
            argv.push(format!("--{name}").into());
        }
    }
    let run_args = RunArgs::try_parse_from(argv)
        .map_err(|e| fail(exit::USAGE, e.to_string()))?
        .args;
    let run_args = run::Args {
        out: run_args
            .out
            .or_else(|| Some(Path::new("nuee-out").join("embed"))),
        ..run_args
    };
    run::run_embed(run_args, "embed", job, ctx).await
}
