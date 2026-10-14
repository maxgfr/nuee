//! DiLoCo for Nuée: the outer Nesterov step, the worker → coordinator delta codec, robust
//! aggregation of deltas and the round state machine.
//!
//! Everything here is pure (no I/O, no clock, no threads) and compiles to wasm32, so a browser
//! page can coordinate a run as well as the CLI.
//!
//! # Trainer contract
//!
//! Parameters travel as safetensors files holding only `F32` tensors. Names starting with
//! [`RESERVED_PREFIX`] are reserved for checkpoints and ignored when loading parameters.
//!
//! Every training lease carries the same input, built by [`TaskInput::encode`]
//! (all integers little-endian):
//!
//! ```text
//! b"NTR1" | u32 steps | u32 shard | u32 n_shards | u64 seed | f32 lr | params (safetensors)
//! ```
//!
//! `lr` is the inner learning rate; `0.0` means "the trainer's own default".
//!
//! - **WASM trainer** (`TaskKind::TrainWasm`): asset 0 of the job is the dataset (the usual
//!   asset envelope wraps the input above). The module runs `steps` local optimiser steps on
//!   shard `shard` of `n_shards`, seeded by `seed`, and returns the encoded delta
//!   `θ_local − θ_input` (see [`codec`], codec named in the job's `TrainSpec`) followed by the
//!   mean inner loss as an `f32` LE trailer. Its outputs are deterministic and compared exactly.
//! - **exec trainer** (`TaskKind::TrainExec`): the host writes the params to a file and runs the
//!   command with the placeholders `$NUEE_INIT` (input safetensors path), `$NUEE_STEPS`,
//!   `$NUEE_SHARD` (`i/n`), `$NUEE_SEED` and `$NUEE_OUT` (output path) substituted, i.e.
//!   `--init <in.safetensors> --steps <H> --shard <i/n> --seed <s> --out <out.safetensors>`.
//!   The trainer writes the new parameters as safetensors (optionally with a `nuee.loss`
//!   metadata entry); the worker host turns them into the WASM trainer format above with
//!   [`ExecEncoder`] (the job's codec, int8 with error feedback by default, the residual kept
//!   per job on the worker). Exec outputs are not compared, robust aggregation is the only
//!   defence.
//!
//! WASM trainer jobs use the `F32` codec: their outputs are recomputed and compared exactly, and
//! an error-feedback residual is worker state a verifier cannot replay.

pub mod aggregate;
pub mod codec;
pub mod outer;
pub mod round;

use std::collections::HashMap;

use safetensors::{Dtype, SafeTensors, tensor::TensorView};

pub use aggregate::{AggConfig, Aggregate, Reject, aggregate};
pub use codec::{ExecEncoder, decode_delta, encode_delta, split_output};
pub use outer::{Outer, OuterConfig};
pub use round::{RoundReport, TrainState, TrainTask};

/// Tensor names starting with this prefix are reserved (checkpoint state).
pub const RESERVED_PREFIX: &str = "nuee.";
/// Most tensors in one parameter set.
pub const MAX_TENSORS: usize = 4096;
/// Largest parameter set or delta, in bytes of f32 data.
pub const MAX_PARAM_BYTES: usize = 1 << 30;
/// Largest number of f32 parameters.
pub const MAX_PARAMS: usize = MAX_PARAM_BYTES / 4;
/// Magic of a training lease input.
pub const TASK_MAGIC: &[u8; 4] = b"NTR1";

/// Something about training data is wrong.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum TrainError {
    /// Not a readable safetensors file.
    #[error("safetensors: {0}")]
    Safetensors(String),
    /// A tensor is not f32.
    #[error("tensor `{name}` has dtype {dtype}; only F32 is supported")]
    Dtype {
        /// Tensor name.
        name: String,
        /// Its dtype.
        dtype: String,
    },
    /// Too many tensors or parameters.
    #[error("too large: {0}")]
    TooLarge(&'static str),
    /// No tensor at all.
    #[error("the parameter set is empty")]
    Empty,
    /// Names or shapes differ from the run's parameters.
    #[error("layout mismatch: {0}")]
    Layout(&'static str),
    /// A delta could not be decoded.
    #[error("bad delta: {0}")]
    Delta(&'static str),
    /// A lease input or task output is malformed.
    #[error("bad task data: {0}")]
    Task(&'static str),
    /// A result for another round, an unknown slot or a duplicate.
    #[error("unexpected result: {0}")]
    Unexpected(&'static str),
    /// Every delta of the round was rejected or none arrived.
    #[error("no usable delta this round")]
    NoDelta,
    /// A checkpoint is malformed or belongs to another run.
    #[error("bad checkpoint: {0}")]
    Checkpoint(&'static str),
}

/// Names and shapes of a parameter set, in sorted name order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Tensor names, sorted.
    pub names: Vec<String>,
    /// Shape of each tensor.
    pub shapes: Vec<Vec<usize>>,
}

impl Layout {
    /// Element count of each tensor.
    pub fn lens(&self) -> Vec<usize> {
        self.shapes.iter().map(|s| s.iter().product()).collect()
    }

    /// Total element count.
    pub fn total(&self) -> usize {
        self.shapes
            .iter()
            .map(|s| s.iter().product::<usize>())
            .sum()
    }

    /// BLAKE3 over names and shapes; two parameter sets with the same hash line up.
    pub fn hash(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"nuee/layout/0");
        h.update(&(self.names.len() as u64).to_le_bytes());
        for (name, shape) in self.names.iter().zip(&self.shapes) {
            h.update(&(name.len() as u64).to_le_bytes());
            h.update(name.as_bytes());
            h.update(&(shape.len() as u64).to_le_bytes());
            for d in shape {
                h.update(&(*d as u64).to_le_bytes());
            }
        }
        *h.finalize().as_bytes()
    }
}

/// A flat f32 parameter set.
#[derive(Debug, Clone, PartialEq)]
pub struct Params {
    /// Tensor names, sorted.
    pub names: Vec<String>,
    /// Shape of each tensor.
    pub shapes: Vec<Vec<usize>>,
    /// All values, tensor after tensor in name order.
    pub data: Vec<f32>,
}

/// Raw tensors and metadata of a safetensors file, reserved names included.
pub(crate) struct RawTensors {
    pub tensors: Vec<(String, Vec<usize>, Vec<f32>)>,
    pub metadata: HashMap<String, String>,
}

pub(crate) fn read_safetensors(bytes: &[u8]) -> Result<RawTensors, TrainError> {
    let (_, meta) =
        SafeTensors::read_metadata(bytes).map_err(|e| TrainError::Safetensors(e.to_string()))?;
    let metadata = meta.metadata().clone().unwrap_or_default();
    let st = SafeTensors::deserialize(bytes).map_err(|e| TrainError::Safetensors(e.to_string()))?;
    if st.len() > 2 * MAX_TENSORS {
        return Err(TrainError::TooLarge("more than 4096 tensors"));
    }
    let mut tensors = Vec::with_capacity(st.len());
    let mut total = 0usize;
    for (name, view) in st.iter() {
        if view.dtype() != Dtype::F32 {
            return Err(TrainError::Dtype {
                name: name.to_string(),
                dtype: format!("{:?}", view.dtype()),
            });
        }
        let raw = view.data();
        total = total.saturating_add(raw.len());
        if total > 2 * MAX_PARAM_BYTES {
            return Err(TrainError::TooLarge("more than 1 GiB of parameters"));
        }
        let values = raw
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect();
        tensors.push((name.to_string(), view.shape().to_vec(), values));
    }
    tensors.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(RawTensors { tensors, metadata })
}

pub(crate) fn write_safetensors(
    tensors: &[(&str, &[usize], &[f32])],
    metadata: Option<HashMap<String, String>>,
) -> Result<Vec<u8>, TrainError> {
    let bytes: Vec<Vec<u8>> = tensors
        .iter()
        .map(|(_, _, v)| v.iter().flat_map(|x| x.to_le_bytes()).collect())
        .collect();
    let mut views = Vec::with_capacity(tensors.len());
    for ((name, shape, _), raw) in tensors.iter().zip(&bytes) {
        let view = TensorView::new(Dtype::F32, shape.to_vec(), raw)
            .map_err(|e| TrainError::Safetensors(e.to_string()))?;
        views.push((name.to_string(), view));
    }
    safetensors::serialize(views, metadata).map_err(|e| TrainError::Safetensors(e.to_string()))
}

impl Params {
    /// Builds a parameter set; tensors are sorted by name.
    pub fn new(mut tensors: Vec<(String, Vec<usize>, Vec<f32>)>) -> Result<Params, TrainError> {
        if tensors.is_empty() {
            return Err(TrainError::Empty);
        }
        if tensors.len() > MAX_TENSORS {
            return Err(TrainError::TooLarge("more than 4096 tensors"));
        }
        tensors.sort_by(|a, b| a.0.cmp(&b.0));
        let mut names = Vec::with_capacity(tensors.len());
        let mut shapes = Vec::with_capacity(tensors.len());
        let mut data = Vec::new();
        for (name, shape, values) in tensors {
            if names.last() == Some(&name) {
                return Err(TrainError::Layout("duplicate tensor name"));
            }
            let len = shape
                .iter()
                .try_fold(1usize, |a, d| a.checked_mul(*d))
                .ok_or(TrainError::TooLarge("tensor shape overflows"))?;
            if len != values.len() {
                return Err(TrainError::Layout("tensor data does not match its shape"));
            }
            if data.len() + len > MAX_PARAMS {
                return Err(TrainError::TooLarge("more than 1 GiB of parameters"));
            }
            data.extend_from_slice(&values);
            names.push(name);
            shapes.push(shape);
        }
        Ok(Params {
            names,
            shapes,
            data,
        })
    }

    /// Reads f32 tensors from a safetensors file, skipping reserved (`nuee.`) names.
    pub fn from_safetensors(bytes: &[u8]) -> Result<Params, TrainError> {
        let raw = read_safetensors(bytes)?;
        Params::new(
            raw.tensors
                .into_iter()
                .filter(|(n, _, _)| !n.starts_with(RESERVED_PREFIX))
                .collect(),
        )
    }

    /// Writes the parameters as a safetensors file.
    pub fn to_safetensors(&self) -> Result<Vec<u8>, TrainError> {
        self.to_safetensors_with(None)
    }

    /// Writes the parameters with string metadata.
    pub fn to_safetensors_with(
        &self,
        metadata: Option<HashMap<String, String>>,
    ) -> Result<Vec<u8>, TrainError> {
        let tensors = self.tensors();
        write_safetensors(&tensors, metadata)
    }

    pub(crate) fn tensors(&self) -> Vec<(&str, &[usize], &[f32])> {
        let mut out = Vec::with_capacity(self.names.len());
        let mut off = 0;
        for (name, shape) in self.names.iter().zip(&self.shapes) {
            let len: usize = shape.iter().product();
            out.push((name.as_str(), shape.as_slice(), &self.data[off..off + len]));
            off += len;
        }
        out
    }

    /// Names and shapes.
    pub fn layout(&self) -> Layout {
        Layout {
            names: self.names.clone(),
            shapes: self.shapes.clone(),
        }
    }

    /// BLAKE3 of names and shapes.
    pub fn layout_hash(&self) -> [u8; 32] {
        self.layout().hash()
    }

    /// BLAKE3 of the layout and every value; identifies one version of the parameters.
    pub fn hash(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"nuee/params/0");
        h.update(&self.layout_hash());
        for x in &self.data {
            h.update(&x.to_le_bytes());
        }
        *h.finalize().as_bytes()
    }
}

/// Decoded training lease input (see the crate docs for the layout).
#[derive(Debug, Clone, PartialEq)]
pub struct TaskInput<'a> {
    /// Local optimiser steps.
    pub steps: u32,
    /// Shard index.
    pub shard: u32,
    /// Shard count.
    pub n_shards: u32,
    /// Seed of the local run.
    pub seed: u64,
    /// Inner learning rate, 0 = trainer default.
    pub lr: f32,
    /// Current parameters, safetensors.
    pub params: &'a [u8],
}

const TASK_HEADER: usize = 4 + 4 + 4 + 4 + 8 + 4;

impl<'a> TaskInput<'a> {
    /// Encodes the lease input.
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(TASK_HEADER + self.params.len());
        v.extend_from_slice(TASK_MAGIC);
        v.extend_from_slice(&self.steps.to_le_bytes());
        v.extend_from_slice(&self.shard.to_le_bytes());
        v.extend_from_slice(&self.n_shards.to_le_bytes());
        v.extend_from_slice(&self.seed.to_le_bytes());
        v.extend_from_slice(&self.lr.to_le_bytes());
        v.extend_from_slice(self.params);
        v
    }

    /// Decodes a lease input.
    pub fn decode(bytes: &'a [u8]) -> Result<TaskInput<'a>, TrainError> {
        let (head, params) = bytes
            .split_at_checked(TASK_HEADER)
            .ok_or(TrainError::Task("input shorter than its header"))?;
        if &head[..4] != TASK_MAGIC {
            return Err(TrainError::Task("input does not start with NTR1"));
        }
        let u32_at =
            |i: usize| u32::from_le_bytes([head[i], head[i + 1], head[i + 2], head[i + 3]]);
        let mut seed = [0u8; 8];
        seed.copy_from_slice(&head[16..24]);
        let input = TaskInput {
            steps: u32_at(4),
            shard: u32_at(8),
            n_shards: u32_at(12),
            seed: u64::from_le_bytes(seed),
            lr: f32::from_bits(u32_at(24)),
            params,
        };
        if input.n_shards == 0 || input.shard >= input.n_shards {
            return Err(TrainError::Task("shard out of range"));
        }
        if !input.lr.is_finite() || input.lr < 0.0 {
            return Err(TrainError::Task("inner learning rate out of range"));
        }
        Ok(input)
    }
}

pub(crate) fn norm(v: &[f32]) -> f64 {
    v.iter()
        .map(|x| f64::from(*x) * f64::from(*x))
        .sum::<f64>()
        .sqrt()
}
