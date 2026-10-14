//! Strict job files. Unknown fields are errors; paths stay strings and are resolved by the
//! caller relative to the file.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::limits::{MAX_ASSETS_PER_JOB, MAX_DEADLINE_MS, MAX_WASM_MEMORY};
use crate::manifest::is_commit_rev;

/// A job file is unreadable or inconsistent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JobError {
    /// Not valid TOML or not matching the schema.
    #[error("job file: {0}")]
    Parse(String),
    /// Valid syntax, invalid content.
    #[error("job file: {0}")]
    Invalid(String),
}

fn invalid(msg: impl Into<String>) -> JobError {
    JobError::Invalid(msg.into())
}

/// Job kind as written in a job file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobKind {
    /// `[wasm]` table.
    Wasm,
    /// `[wgsl]` table.
    Wgsl,
    /// `[embed]` table.
    Embed,
    /// `[llm]` table.
    Llm,
    /// `[exec]` table.
    Exec,
    /// `[train]` table.
    Train,
}

/// Verification mode as written in a job file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerifyChoice {
    /// Every task twice.
    Full,
    /// Sampled recomputation.
    Sample,
    /// None.
    None,
}

/// Where task inputs come from.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputsToml {
    /// One task per file, sorted by name.
    pub dir: Option<String>,
    /// JSON lines file.
    pub jsonl: Option<String>,
    /// Lines per task for `jsonl` (default 2000).
    pub lines_per_task: Option<u32>,
    /// Built-in generator name.
    pub generate: Option<String>,
    /// Number of generated tasks.
    pub count: Option<u32>,
    /// Scalar generator parameters.
    pub params: Option<toml::Table>,
}

/// How outputs are merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MergeKind {
    /// One file per task.
    None,
    /// Concatenate in task order.
    Concat,
    /// JSON lines, ordered by `uid_field`.
    Jsonl,
}

/// Output settings.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputToml {
    /// Output directory.
    pub dir: Option<String>,
    /// Merge mode.
    pub merge: Option<MergeKind>,
    /// Unique id field for `jsonl` merges.
    pub uid_field: Option<String>,
}

/// Comparator kind as written in a job file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompareKind {
    /// Byte equality.
    Exact,
    /// Element-wise tolerance.
    Tolerance,
    /// Cosine similarity.
    Cosine,
    /// Logit gap.
    Logits,
    /// JSON lines field tolerance.
    Jsonl,
    /// No comparison.
    None,
}

/// Output comparison settings.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompareToml {
    /// Comparator.
    pub kind: CompareKind,
    /// Absolute tolerance.
    pub abs: Option<f64>,
    /// Relative tolerance.
    pub rel: Option<f64>,
    /// Share of elements allowed outside tolerance, 0..=1.
    pub max_bad: Option<f64>,
    /// Minimum cosine similarity.
    pub min_cos: Option<f64>,
    /// Largest logit gap.
    pub max_gap: Option<f64>,
    /// JSON field compared.
    pub field: Option<String>,
    /// Tolerance on that field.
    pub tol: Option<f64>,
}

/// `[wasm]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WasmToml {
    /// Module path.
    pub module: String,
    /// Asset files handed to every task in the asset envelope, in this order (paths relative
    /// to the job file; each asset is named after its file name).
    #[serde(default)]
    pub assets: Vec<String>,
}

/// The name a job asset gets in the manifest: the last component of its path.
pub fn asset_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// `[wgsl]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WgslToml {
    /// Shader path.
    pub shader: String,
    /// Entry point (default `main`).
    pub entry: Option<String>,
    /// Workgroup size.
    pub workgroup: Option<[u32; 3]>,
}

/// `[embed]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbedToml {
    /// Model name.
    pub model: String,
    /// Token limit per text.
    pub max_tokens: Option<u32>,
}

/// `[llm]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmToml {
    /// Model name.
    pub model: String,
    /// Answer letters.
    pub letters: Option<String>,
}

/// `[exec]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecToml {
    /// Repository URL.
    pub repo: String,
    /// Commit, 40 hex characters.
    pub rev: String,
    /// Command line.
    pub argv: Vec<String>,
    /// Environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Time limit, e.g. `"4h"`.
    pub timeout: Option<String>,
    /// Needs a GPU.
    #[serde(default)]
    pub gpu: bool,
}

/// Trainer kind as written in a job file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrainerChoice {
    /// WASM trainer.
    Wasm,
    /// Native trainer.
    Exec,
}

/// `[train]` table.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrainToml {
    /// Outer rounds.
    pub rounds: u32,
    /// Local steps per round.
    pub inner_steps: u32,
    /// Workers per round.
    pub workers: u16,
    /// Outer learning rate (default 0.7).
    pub outer_lr: Option<f64>,
    /// Outer momentum (default 0.9).
    pub outer_momentum: Option<f64>,
    /// Initial parameters path.
    pub init: String,
    /// Trainer kind.
    pub trainer: TrainerChoice,
    /// Trainer module path, for WASM trainers.
    pub module: Option<String>,
    /// Trimmed-mean fraction (default 0.2).
    pub trim: Option<f64>,
    /// Minimum delta cosine (default 0.0).
    pub cos_min: Option<f64>,
    /// Largest delta norm ratio (default 3.0).
    pub norm_max_ratio: Option<f64>,
}

/// `[[sweep]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepToml {
    /// Variant name.
    pub name: String,
    /// Extra arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// A parsed job file.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobToml {
    /// Job name, 1 to 64 characters.
    pub name: String,
    /// Job kind.
    pub kind: JobKind,
    /// Verification mode (default sample).
    pub verify: Option<VerifyChoice>,
    /// Sampled share, 0..=1 (default 0.10).
    pub sample: Option<f64>,
    /// Canary share, 0..=1 (default 0.03).
    pub canary: Option<f64>,
    /// Task deadline (default 60s).
    pub deadline: Option<String>,
    /// Memory cap, at most 1GiB.
    pub max_memory: Option<String>,
    /// Inputs.
    pub inputs: Option<InputsToml>,
    /// Output.
    pub output: Option<OutputToml>,
    /// Comparison.
    pub compare: Option<CompareToml>,
    /// WASM settings.
    pub wasm: Option<WasmToml>,
    /// WGSL settings.
    pub wgsl: Option<WgslToml>,
    /// Embedding settings.
    pub embed: Option<EmbedToml>,
    /// LLM settings.
    pub llm: Option<LlmToml>,
    /// Native command settings.
    pub exec: Option<ExecToml>,
    /// Training settings.
    pub train: Option<TrainToml>,
    /// Sweep variants.
    #[serde(default)]
    pub sweep: Vec<SweepToml>,
}

/// Parses a duration such as `250ms`, `60s`, `5m` or `4h` into milliseconds.
pub fn parse_duration_ms(s: &str) -> Result<u64, JobError> {
    let s = s.trim();
    let (num, mul) = if let Some(n) = s.strip_suffix("ms") {
        (n, 1)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1000)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60_000)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3_600_000)
    } else {
        return Err(invalid(format!(
            "duration `{s}` needs a unit: ms, s, m or h"
        )));
    };
    let n: u64 = num
        .trim()
        .parse()
        .map_err(|_| invalid(format!("duration `{s}` is not a whole number")))?;
    n.checked_mul(mul)
        .ok_or_else(|| invalid(format!("duration `{s}` is too large")))
}

/// Parses a size such as `512KiB`, `256MiB` or `1GiB` (or plain bytes) into bytes.
pub fn parse_size_bytes(s: &str) -> Result<u64, JobError> {
    let s = s.trim();
    let (num, mul) = if let Some(n) = s.strip_suffix("KiB") {
        (n, 1u64 << 10)
    } else if let Some(n) = s.strip_suffix("MiB") {
        (n, 1 << 20)
    } else if let Some(n) = s.strip_suffix("GiB") {
        (n, 1 << 30)
    } else {
        (s, 1)
    };
    let n: u64 = num.trim().parse().map_err(|_| {
        invalid(format!(
            "size `{s}` is not a whole number with KiB, MiB or GiB"
        ))
    })?;
    n.checked_mul(mul)
        .ok_or_else(|| invalid(format!("size `{s}` is too large")))
}

fn share(name: &str, v: Option<f64>) -> Result<(), JobError> {
    match v {
        Some(x) if !(0.0..=1.0).contains(&x) => Err(invalid(format!(
            "`{name}` must be between 0 and 1, got {x}"
        ))),
        _ => Ok(()),
    }
}

fn non_negative(name: &str, v: Option<f64>) -> Result<(), JobError> {
    match v {
        Some(x) if !(x.is_finite() && x >= 0.0) => Err(invalid(format!(
            "`{name}` must be a finite number >= 0, got {x}"
        ))),
        _ => Ok(()),
    }
}

impl JobToml {
    /// Parses a job file without checking its content (see [`JobToml::validate`]).
    pub fn parse(text: &str) -> Result<JobToml, JobError> {
        toml::from_str(text).map_err(|e| JobError::Parse(e.to_string()))
    }

    /// Parses and validates a job file.
    pub fn from_str_checked(text: &str) -> Result<JobToml, JobError> {
        let job = Self::parse(text)?;
        job.validate()?;
        Ok(job)
    }

    /// Task deadline in milliseconds.
    pub fn deadline_ms(&self) -> Result<u64, JobError> {
        match &self.deadline {
            Some(d) => parse_duration_ms(d),
            None => Ok(60_000),
        }
    }

    /// Memory cap in bytes, if set.
    pub fn max_memory_bytes(&self) -> Result<Option<u64>, JobError> {
        self.max_memory.as_deref().map(parse_size_bytes).transpose()
    }

    /// Checks every rule of the schema.
    pub fn validate(&self) -> Result<(), JobError> {
        let n = self.name.chars().count();
        if !(1..=64).contains(&n) {
            return Err(invalid(format!(
                "`name` must be 1 to 64 characters, got {n}"
            )));
        }
        share("sample", self.sample)?;
        share("canary", self.canary)?;

        let deadline = self.deadline_ms()?;
        if deadline == 0 || deadline > u64::from(MAX_DEADLINE_MS) {
            return Err(invalid("`deadline` must be above 0 and at most 24h"));
        }
        if let Some(mem) = self.max_memory_bytes()?
            && (mem == 0 || mem > MAX_WASM_MEMORY)
        {
            return Err(invalid("`max_memory` must be above 0 and at most 1GiB"));
        }

        self.validate_tables()?;
        if let Some(inputs) = &self.inputs {
            validate_inputs(inputs)?;
        } else if matches!(
            self.kind,
            JobKind::Wasm | JobKind::Wgsl | JobKind::Embed | JobKind::Llm
        ) {
            return Err(invalid("`[inputs]` is required for this kind"));
        }
        if let Some(out) = &self.output
            && out.uid_field.is_some()
            && out.merge != Some(MergeKind::Jsonl)
        {
            return Err(invalid(
                "`output.uid_field` only applies to merge = \"jsonl\"",
            ));
        }
        if let Some(c) = &self.compare {
            validate_compare(c)?;
        }
        if !self.sweep.is_empty() && self.kind != JobKind::Exec {
            return Err(invalid("`[[sweep]]` only applies to kind = \"exec\""));
        }
        let mut names = std::collections::BTreeSet::new();
        for s in &self.sweep {
            if s.name.is_empty() || s.name.chars().count() > 64 {
                return Err(invalid("sweep `name` must be 1 to 64 characters"));
            }
            if !names.insert(s.name.as_str()) {
                return Err(invalid(format!("duplicate sweep name `{}`", s.name)));
            }
        }
        Ok(())
    }

    fn validate_tables(&self) -> Result<(), JobError> {
        let present = [
            ("wasm", self.wasm.is_some(), JobKind::Wasm),
            ("wgsl", self.wgsl.is_some(), JobKind::Wgsl),
            ("embed", self.embed.is_some(), JobKind::Embed),
            ("llm", self.llm.is_some(), JobKind::Llm),
            ("exec", self.exec.is_some(), JobKind::Exec),
            ("train", self.train.is_some(), JobKind::Train),
        ];
        for (table, is_present, kind) in present {
            if kind == self.kind && !is_present {
                return Err(invalid(format!(
                    "`[{table}]` is required for kind = \"{table}\""
                )));
            }
            // A native trainer declares its command in `[exec]`.
            let exec_trainer = kind == JobKind::Exec
                && self.kind == JobKind::Train
                && self
                    .train
                    .as_ref()
                    .is_some_and(|t| t.trainer == TrainerChoice::Exec);
            if kind != self.kind && is_present && !exec_trainer {
                return Err(invalid(format!(
                    "`[{table}]` is not allowed for this kind; remove it or change `kind`"
                )));
            }
        }
        if let Some(w) = &self.wasm {
            validate_wasm_assets(&w.assets)?;
        }
        if let Some(w) = &self.wgsl
            && let Some(g) = w.workgroup
            && g.contains(&0)
        {
            return Err(invalid("`wgsl.workgroup` sizes must be above 0"));
        }
        if let Some(l) = &self.llm
            && let Some(letters) = &l.letters
            && (letters.len() < 2 || !letters.chars().all(|c| c.is_ascii_uppercase()))
        {
            return Err(invalid(
                "`llm.letters` must be at least two uppercase letters",
            ));
        }
        if let Some(e) = &self.exec {
            validate_exec(e)?;
        }
        if let Some(t) = &self.train {
            validate_train(t)?;
            if t.trainer == TrainerChoice::Exec && self.exec.is_none() {
                return Err(invalid(
                    "trainer = \"exec\" needs an `[exec]` table with the trainer command",
                ));
            }
        }
        Ok(())
    }
}

fn validate_wasm_assets(assets: &[String]) -> Result<(), JobError> {
    if assets.len() > MAX_ASSETS_PER_JOB {
        return Err(invalid(format!(
            "`wasm.assets` lists {} files, at most {MAX_ASSETS_PER_JOB}",
            assets.len()
        )));
    }
    let mut names = std::collections::BTreeSet::new();
    for path in assets {
        let name = asset_name(path);
        if name.is_empty() || name == "." || name == ".." || name.len() > 128 {
            return Err(invalid(format!(
                "`wasm.assets` entry {path:?} must name a file (name at most 128 bytes)"
            )));
        }
        if !names.insert(name) {
            return Err(invalid(format!(
                "`wasm.assets` has two files named {name:?}"
            )));
        }
    }
    Ok(())
}

fn validate_inputs(i: &InputsToml) -> Result<(), JobError> {
    let sources = [i.dir.is_some(), i.jsonl.is_some(), i.generate.is_some()]
        .into_iter()
        .filter(|b| *b)
        .count();
    if sources != 1 {
        return Err(invalid(
            "`[inputs]` needs exactly one of `dir`, `jsonl` or `generate`",
        ));
    }
    if i.lines_per_task.is_some() && i.jsonl.is_none() {
        return Err(invalid("`inputs.lines_per_task` only applies to `jsonl`"));
    }
    if i.lines_per_task == Some(0) {
        return Err(invalid("`inputs.lines_per_task` must be above 0"));
    }
    if (i.count.is_some() || i.params.is_some()) && i.generate.is_none() {
        return Err(invalid(
            "`inputs.count` and `inputs.params` only apply to `generate`",
        ));
    }
    if i.generate.is_some() && i.count.unwrap_or(0) == 0 {
        return Err(invalid(
            "`inputs.count` is required with `generate` and must be above 0",
        ));
    }
    if let Some(params) = &i.params {
        for (k, v) in params {
            if v.is_table() || v.is_array() {
                return Err(invalid(format!("`inputs.params.{k}` must be a scalar")));
            }
        }
    }
    Ok(())
}

fn validate_compare(c: &CompareToml) -> Result<(), JobError> {
    for (name, v) in [
        ("abs", c.abs),
        ("rel", c.rel),
        ("max_gap", c.max_gap),
        ("tol", c.tol),
    ] {
        non_negative(&format!("compare.{name}"), v)?;
    }
    share("compare.max_bad", c.max_bad)?;
    if let Some(m) = c.min_cos
        && !(-1.0..=1.0).contains(&m)
    {
        return Err(invalid("`compare.min_cos` must be between -1 and 1"));
    }
    let allowed: &[&str] = match c.kind {
        CompareKind::Exact | CompareKind::None => &[],
        CompareKind::Tolerance => &["abs", "rel", "max_bad"],
        CompareKind::Cosine => &["min_cos"],
        CompareKind::Logits => &["max_gap"],
        CompareKind::Jsonl => &["field", "tol"],
    };
    let set = [
        ("abs", c.abs.is_some()),
        ("rel", c.rel.is_some()),
        ("max_bad", c.max_bad.is_some()),
        ("min_cos", c.min_cos.is_some()),
        ("max_gap", c.max_gap.is_some()),
        ("field", c.field.is_some()),
        ("tol", c.tol.is_some()),
    ];
    for (name, is_set) in set {
        if is_set && !allowed.contains(&name) {
            return Err(invalid(format!(
                "`compare.{name}` does not apply to this comparator kind"
            )));
        }
    }
    let missing = match c.kind {
        CompareKind::Tolerance if c.abs.is_none() && c.rel.is_none() => Some("`abs` or `rel`"),
        CompareKind::Cosine if c.min_cos.is_none() => Some("`min_cos`"),
        CompareKind::Logits if c.max_gap.is_none() => Some("`max_gap`"),
        CompareKind::Jsonl if c.field.is_none() || c.tol.is_none() => Some("`field` and `tol`"),
        _ => None,
    };
    match missing {
        Some(m) => Err(invalid(format!("`[compare]` of this kind needs {m}"))),
        None => Ok(()),
    }
}

fn validate_exec(e: &ExecToml) -> Result<(), JobError> {
    if e.repo.is_empty() || e.repo.len() > 256 {
        return Err(invalid("`exec.repo` must be 1 to 256 bytes"));
    }
    if !is_commit_rev(&e.rev) {
        return Err(invalid(format!(
            "`exec.rev` must be a full 40-character lowercase hex commit, got `{}`",
            e.rev
        )));
    }
    if e.argv.is_empty() || e.argv.len() > 64 {
        return Err(invalid("`exec.argv` must have 1 to 64 entries"));
    }
    if e.argv.iter().any(|a| a.len() > 1024) {
        return Err(invalid("`exec.argv` entries must be at most 1024 bytes"));
    }
    if e.env.len() > 32 {
        return Err(invalid("`exec.env` must have at most 32 entries"));
    }
    if e.env
        .iter()
        .any(|(k, v)| k.is_empty() || k.len() > 128 || v.len() > 1024)
    {
        return Err(invalid(
            "`exec.env` keys must be 1 to 128 bytes, values at most 1024",
        ));
    }
    if let Some(t) = &e.timeout {
        let ms = parse_duration_ms(t)?;
        if ms == 0 || ms > u64::from(MAX_DEADLINE_MS) {
            return Err(invalid("`exec.timeout` must be above 0 and at most 24h"));
        }
    }
    Ok(())
}

fn validate_train(t: &TrainToml) -> Result<(), JobError> {
    if t.rounds == 0 || t.inner_steps == 0 || t.workers == 0 {
        return Err(invalid(
            "`train.rounds`, `inner_steps` and `workers` must be above 0",
        ));
    }
    match (t.trainer, &t.module) {
        (TrainerChoice::Wasm, None) => {
            return Err(invalid(
                "`train.module` is required with trainer = \"wasm\"",
            ));
        }
        (TrainerChoice::Exec, Some(_)) => {
            return Err(invalid("`train.module` only applies to trainer = \"wasm\""));
        }
        _ => {}
    }
    for (name, v) in [
        ("outer_lr", t.outer_lr),
        ("outer_momentum", t.outer_momentum),
        ("norm_max_ratio", t.norm_max_ratio),
    ] {
        non_negative(&format!("train.{name}"), v)?;
    }
    if let Some(m) = t.outer_momentum
        && m >= 1.0
    {
        return Err(invalid("`train.outer_momentum` must be below 1"));
    }
    if let Some(trim) = t.trim
        && !(0.0..0.5).contains(&trim)
    {
        return Err(invalid("`train.trim` must be in [0, 0.5)"));
    }
    if let Some(c) = t.cos_min
        && !(-1.0..=1.0).contains(&c)
    {
        return Err(invalid("`train.cos_min` must be between -1 and 1"));
    }
    Ok(())
}
