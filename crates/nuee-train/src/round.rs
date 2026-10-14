//! Round state machine of a DiLoCo run.
//!
//! A round hands out `workers` tasks over the current parameters, ingests their outputs, then
//! [`TrainState::finish_round`] aggregates the deltas and applies the outer step. When to finish
//! (all tasks in, or a deadline with enough workers) is the caller's decision.

use std::collections::{BTreeMap, HashMap};

use nuee_proto::{DeltaCodec, TrainSpec};

use crate::aggregate::{AggConfig, Reject, aggregate};
use crate::codec::{decode_delta, split_output};
use crate::outer::{Outer, OuterConfig};
use crate::{Params, RESERVED_PREFIX, TaskInput, TrainError, read_safetensors, write_safetensors};

const VELOCITY_PREFIX: &str = "nuee.velocity/";
const META_ROUND: &str = "nuee.round";
const META_SEED: &str = "nuee.seed";
const META_LAYOUT: &str = "nuee.layout";
/// Metadata key an exec trainer may set on its output with the mean inner loss.
pub const META_LOSS: &str = "nuee.loss";

/// One task of a round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrainTask {
    /// Round index, from 0.
    pub round: u32,
    /// Worker slot in the round, 0..workers (also the task index of the round's job).
    pub worker_slot: u16,
    /// Data shard.
    pub shard: u32,
    /// Shard count (= workers).
    pub n_shards: u32,
    /// Seed of the local run.
    pub seed: u64,
    /// Local steps.
    pub steps: u32,
    /// [`Params::hash`] of the parameters the task starts from.
    pub params_hash: [u8; 32],
}

/// Summary of a finished round.
#[derive(Debug, Clone, PartialEq)]
pub struct RoundReport {
    /// Round index that finished, from 0.
    pub round: u32,
    /// Slots whose delta was used.
    pub accepted: Vec<u16>,
    /// Slots whose delta was left out, with the reason.
    pub rejected: Vec<(u16, Reject)>,
    /// Mean inner loss reported by the accepted workers, when any reported one.
    pub mean_loss: Option<f32>,
    /// Norm of the change applied to the parameters.
    pub update_norm: f64,
}

/// State of a DiLoCo run.
#[derive(Debug, Clone)]
pub struct TrainState {
    spec: TrainSpec,
    params: Params,
    outer: Outer,
    round: u32,
    seed: u64,
    inner_lr: f32,
    params_bytes: Vec<u8>,
    params_hash: [u8; 32],
    pending: BTreeMap<u16, (Vec<f32>, Option<f32>)>,
}

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

impl TrainState {
    /// Starts a run from `init`. `seed` derives every shard assignment and task seed.
    pub fn new(spec: TrainSpec, init: Params, seed: u64) -> Result<TrainState, TrainError> {
        if spec.workers == 0 || spec.inner_steps == 0 {
            return Err(TrainError::Task("workers and inner steps must be above 0"));
        }
        let n = init.data.len();
        let mut s = TrainState {
            spec,
            params: init,
            outer: Outer::new(n),
            round: 0,
            seed,
            inner_lr: 0.0,
            params_bytes: Vec::new(),
            params_hash: [0; 32],
            pending: BTreeMap::new(),
        };
        s.refresh()?;
        Ok(s)
    }

    fn refresh(&mut self) -> Result<(), TrainError> {
        self.params_bytes = self.params.to_safetensors()?;
        self.params_hash = self.params.hash();
        Ok(())
    }

    /// Sets the inner learning rate passed to trainers (0 = trainer default).
    pub fn set_inner_lr(&mut self, lr: f32) {
        self.inner_lr = lr;
    }

    /// Current round index (number of finished rounds).
    pub fn round(&self) -> u32 {
        self.round
    }

    /// Run seed.
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// The training settings.
    pub fn spec(&self) -> &TrainSpec {
        &self.spec
    }

    /// Current global parameters.
    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Outer momentum.
    pub fn velocity(&self) -> &[f32] {
        &self.outer.velocity
    }

    /// Whether every round has run.
    pub fn is_done(&self) -> bool {
        self.round >= self.spec.rounds
    }

    /// Slots that delivered a delta this round.
    pub fn received(&self) -> usize {
        self.pending.len()
    }

    /// Tasks of round `r`. Shards rotate between rounds so every worker slot sees every shard,
    /// and seeds are unique per (run seed, round, slot). `params_hash` is that of the current
    /// parameters, so only the current round's tasks can be ingested.
    pub fn round_tasks(&self, r: u32) -> Vec<TrainTask> {
        let n = u32::from(self.spec.workers);
        let rot = (splitmix(self.seed ^ u64::from(r).rotate_left(32)) % u64::from(n)) as u32;
        (0..self.spec.workers)
            .map(|slot| TrainTask {
                round: r,
                worker_slot: slot,
                shard: (u32::from(slot) + rot) % n,
                n_shards: n,
                seed: splitmix(splitmix(self.seed ^ (u64::from(r) << 20)) ^ u64::from(slot)),
                steps: self.spec.inner_steps,
                params_hash: self.params_hash,
            })
            .collect()
    }

    /// Lease input of a task of the current round (`NTR1` header + current params).
    pub fn task_input(&self, task: &TrainTask) -> Vec<u8> {
        TaskInput {
            steps: task.steps,
            shard: task.shard,
            n_shards: task.n_shards,
            seed: task.seed,
            lr: self.inner_lr,
            params: &self.params_bytes,
        }
        .encode()
    }

    /// Takes a task output: an encoded delta plus loss trailer, from WASM and exec trainers alike.
    pub fn ingest(&mut self, task: &TrainTask, output: &[u8]) -> Result<(), TrainError> {
        if task.round != self.round || task.params_hash != self.params_hash {
            return Err(TrainError::Unexpected("task of another round"));
        }
        if task.worker_slot >= self.spec.workers {
            return Err(TrainError::Unexpected("unknown worker slot"));
        }
        if self.pending.contains_key(&task.worker_slot) {
            return Err(TrainError::Unexpected("slot already delivered"));
        }
        // WASM and exec trainers deliver the same format (exec outputs are encoded on the
        // worker by `codec::ExecEncoder`); `decode_delta` checks it against the run's layout.
        let (enc, loss) = split_output(output)?;
        let delta = decode_delta(enc, &self.params.layout())?;
        let loss = loss.is_finite().then_some(loss);
        self.pending.insert(task.worker_slot, (delta, loss));
        Ok(())
    }

    /// Aggregates the deltas received this round and applies the outer step. On
    /// [`TrainError::NoDelta`] the round stays open and nothing changes.
    pub fn finish_round(&mut self) -> Result<RoundReport, TrainError> {
        let slots: Vec<u16> = self.pending.keys().copied().collect();
        let deltas: Vec<Vec<f32>> = self.pending.values().map(|(d, _)| d.clone()).collect();
        let agg = aggregate(
            &deltas,
            self.params.data.len(),
            AggConfig {
                trim: self.spec.trim,
                cos_min: self.spec.cos_min,
                norm_max_ratio: self.spec.norm_max_ratio,
            },
        )?;
        let update_norm = self.outer.step(
            OuterConfig {
                lr: self.spec.outer_lr,
                momentum: self.spec.outer_momentum,
            },
            &mut self.params.data,
            &agg.delta,
        );
        let losses: Vec<f32> = agg
            .accepted
            .iter()
            .filter_map(|&i| self.pending.get(&slots[i]).and_then(|p| p.1))
            .collect();
        let mean_loss = (!losses.is_empty()).then(|| {
            (losses.iter().map(|l| f64::from(*l)).sum::<f64>() / losses.len() as f64) as f32
        });
        let report = RoundReport {
            round: self.round,
            accepted: agg.accepted.iter().map(|&i| slots[i]).collect(),
            rejected: agg.rejected.iter().map(|&(i, r)| (slots[i], r)).collect(),
            mean_loss,
            update_norm,
        };
        self.pending.clear();
        self.round += 1;
        self.refresh()?;
        Ok(report)
    }

    /// Saves θ, the momentum, the round index and the seed as one safetensors file. The
    /// parameters keep their names, so a checkpoint loads as a plain model.
    pub fn checkpoint(&self) -> Result<Vec<u8>, TrainError> {
        let vnames: Vec<String> = self
            .params
            .names
            .iter()
            .map(|n| format!("{VELOCITY_PREFIX}{n}"))
            .collect();
        let mut tensors = self.params.tensors();
        let mut off = 0;
        for (name, shape) in vnames.iter().zip(&self.params.shapes) {
            let len: usize = shape.iter().product();
            tensors.push((name, shape, &self.outer.velocity[off..off + len]));
            off += len;
        }
        let meta = HashMap::from([
            (META_ROUND.to_string(), self.round.to_string()),
            (META_SEED.to_string(), self.seed.to_string()),
            (META_LAYOUT.to_string(), hex(&self.params.layout_hash())),
        ]);
        write_safetensors(&tensors, Some(meta))
    }

    /// Restores θ, the momentum, the round and the seed from [`TrainState::checkpoint`]. The
    /// checkpoint must have the same layout as the current parameters.
    pub fn restore(&mut self, bytes: &[u8]) -> Result<(), TrainError> {
        let raw = read_safetensors(bytes)?;
        let meta = |k: &str| {
            raw.metadata
                .get(k)
                .ok_or(TrainError::Checkpoint("missing metadata"))
        };
        let round: u32 = meta(META_ROUND)?
            .parse()
            .map_err(|_| TrainError::Checkpoint("bad round"))?;
        let seed: u64 = meta(META_SEED)?
            .parse()
            .map_err(|_| TrainError::Checkpoint("bad seed"))?;
        let mut params = Vec::new();
        let mut velocity = BTreeMap::new();
        for (name, shape, values) in raw.tensors {
            if let Some(base) = name.strip_prefix(VELOCITY_PREFIX) {
                velocity.insert(base.to_string(), (shape, values));
            } else if !name.starts_with(RESERVED_PREFIX) {
                params.push((name, shape, values));
            }
        }
        let params = Params::new(params)?;
        if params.layout_hash() != self.params.layout_hash()
            || *meta(META_LAYOUT)? != hex(&params.layout_hash())
        {
            return Err(TrainError::Checkpoint("layout differs from the run"));
        }
        let mut v = Vec::with_capacity(params.data.len());
        for (name, shape) in params.names.iter().zip(&params.shapes) {
            let (vs, vals) = velocity
                .remove(name)
                .ok_or(TrainError::Checkpoint("momentum missing"))?;
            if &vs != shape {
                return Err(TrainError::Checkpoint("momentum shape differs"));
            }
            v.extend(vals);
        }
        if !velocity.is_empty() {
            return Err(TrainError::Checkpoint("momentum of an unknown tensor"));
        }
        self.params = params;
        self.outer = Outer { velocity: v };
        self.round = round;
        self.seed = seed;
        self.pending.clear();
        self.refresh()
    }

    /// Codec workers use for their deltas.
    pub fn codec(&self) -> DeltaCodec {
        self.spec.codec
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
