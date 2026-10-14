//! DiLoCo runs hosted by a browser: `host()` with `kind: 'trainWasm'` and a `train` spec.
//!
//! Each round is one job of `workers` tasks built by nuee-train's [`TrainState`]; accepted
//! outputs are ingested, the round is closed once every task is in (or the job ended with at
//! least one delta), and the next round is submitted. The page sees the usual events plus
//! `trainRound` after every round and `trainDone` with the final parameters.

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use nuee_net::HostEvent;
use nuee_proto::{DeltaCodec, Hash, TaskKind, TrainSpec, TrainerKind};
use nuee_train::{Params, TrainState, TrainTask};
use tokio::sync::mpsc;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::spawn_local;

use super::{
    Host, JobInput, Obj, Sink, bytes_js, field, host_event, opt_f32, opt_str, opt_u16, opt_u32,
    random_err, submit_job, to_bytes,
};
use crate::{WebError, code};

/// Most rounds a page may ask for.
const MAX_ROUNDS: u32 = 10_000;
/// Most local steps per task.
const MAX_INNER_STEPS: u32 = 100_000;
/// Most workers per round (tasks per round job).
const MAX_WORKERS: u16 = 256;

/// The training part of a host spec.
pub(super) struct Setup {
    pub(super) spec: TrainSpec,
    init: Params,
    inner_lr: f32,
}

fn num(spec: &JsValue, key: &str) -> Result<f32, WebError> {
    opt_f32(spec, key)?.ok_or_else(|| WebError::invalid(format!("train.{key} is required")))
}

/// Reads `train` from a job spec. Training needs `kind: 'trainWasm'` and the other way round.
pub(super) fn parse(spec: &JsValue, kind: TaskKind) -> Result<Option<Setup>, WebError> {
    let train = field(spec, "train")?;
    let t = match (kind, train) {
        (TaskKind::TrainWasm, Some(t)) => t,
        (TaskKind::TrainWasm, None) => {
            return Err(WebError::invalid("trainWasm jobs need a `train` spec"));
        }
        (_, Some(_)) => return Err(WebError::invalid("`train` needs kind 'trainWasm'")),
        (_, None) => return Ok(None),
    };
    let rounds = opt_u32(&t, "rounds")?.unwrap_or(0);
    let inner_steps = opt_u32(&t, "innerSteps")?.unwrap_or(0);
    let workers = opt_u16(&t, "workers")?.unwrap_or(0);
    if !(1..=MAX_ROUNDS).contains(&rounds)
        || !(1..=MAX_INNER_STEPS).contains(&inner_steps)
        || !(1..=MAX_WORKERS).contains(&workers)
    {
        return Err(WebError::invalid(
            "train.rounds, innerSteps and workers must be within 1..=10000, 1..=100000, 1..=256",
        ));
    }
    let outer_lr = num(&t, "outerLr")?;
    let outer_momentum = num(&t, "outerMomentum")?;
    let inner_lr = opt_f32(&t, "innerLr")?.unwrap_or(0.0);
    let trim = opt_f32(&t, "trim")?.unwrap_or(0.2);
    let cos_min = opt_f32(&t, "cosMin")?.unwrap_or(0.0);
    let norm_max_ratio = opt_f32(&t, "normMaxRatio")?.unwrap_or(3.0);
    let ok = |x: f32, lo: f32, hi: f32| x.is_finite() && (lo..=hi).contains(&x);
    if !ok(outer_lr, 0.0, 10.0)
        || !ok(outer_momentum, 0.0, 1.0)
        || !ok(inner_lr, 0.0, 10.0)
        || !ok(trim, 0.0, 0.49)
        || !ok(cos_min, -1.0, 1.0)
        || !ok(norm_max_ratio, 0.0, 1e6)
    {
        return Err(WebError::invalid("a train setting is out of range"));
    }
    let codec = match opt_str(&t, "codec")?.as_deref() {
        None | Some("f32") => DeltaCodec::F32,
        Some("int8ef") => DeltaCodec::Int8Ef,
        Some(other) => return Err(WebError::invalid(format!("unknown codec {other:?}"))),
    };
    let init_bytes = match field(&t, "init")? {
        Some(v) => to_bytes(v, "train.init")?,
        None => return Err(WebError::invalid("train.init is required")),
    };
    let init = Params::from_safetensors(&init_bytes)
        .map_err(|e| WebError::invalid(format!("train.init: {e}")))?;
    Ok(Some(Setup {
        spec: TrainSpec {
            rounds,
            inner_steps,
            workers,
            outer_lr,
            outer_momentum,
            init: Hash::of(&init_bytes),
            trainer: TrainerKind::Wasm,
            codec,
            trim,
            cos_min,
            norm_max_ratio,
        },
        init,
        inner_lr,
    }))
}

/// What every round job shares.
struct Base {
    name: String,
    module: Option<Vec<u8>>,
    max_memory: u64,
    assets: Vec<Vec<u8>>,
    verify: nuee_proto::VerifyPolicy,
    compare: nuee_proto::Comparator,
    deadline_ms: u32,
    spec: TrainSpec,
}

/// The round being collected.
struct Round {
    job: u32,
    tasks: BTreeMap<u32, TrainTask>,
    seen: BTreeSet<u32>,
}

async fn submit_round(host: &Host, base: &Base, state: &TrainState) -> Result<Round, WebError> {
    let tasks = state.round_tasks(state.round());
    let job = JobInput {
        name: base.name.clone(),
        kind: TaskKind::TrainWasm,
        module: base.module.clone(),
        max_memory: base.max_memory,
        assets: base.assets.clone(),
        inputs: tasks.iter().map(|t| state.task_input(t)).collect(),
        verify: base.verify,
        compare: base.compare.clone(),
        deadline_ms: base.deadline_ms,
        train: Some(Setup {
            spec: base.spec.clone(),
            init: state.params().clone(),
            inner_lr: 0.0,
        }),
    };
    let job = submit_job(host, job).await?;
    Ok(Round {
        job,
        tasks: tasks
            .into_iter()
            .map(|t| (u32::from(t.worker_slot), t))
            .collect(),
        seen: BTreeSet::new(),
    })
}

fn train_err(e: &nuee_train::TrainError) -> WebError {
    WebError::invalid(format!("training: {e}"))
}

/// Starts the run: submits round 1 and returns its job number; the rounds that follow are
/// driven by the host's event stream.
pub(super) async fn start(
    host: Rc<Host>,
    job: JobInput,
    events: mpsc::UnboundedReceiver<HostEvent>,
    sink: Sink,
) -> Result<u32, WebError> {
    let setup = job
        .train
        .ok_or_else(|| WebError::internal("not a training job"))?;
    let seed_bytes = nuee_proto::new_secret16().map_err(random_err)?;
    let mut seed = [0u8; 8];
    seed.copy_from_slice(seed_bytes.get(..8).unwrap_or(&[0; 8]));
    let mut state = TrainState::new(setup.spec.clone(), setup.init, u64::from_le_bytes(seed))
        .map_err(|e| train_err(&e))?;
    state.set_inner_lr(setup.inner_lr);
    let base = Base {
        name: job.name,
        module: job.module,
        max_memory: job.max_memory,
        assets: job.assets,
        verify: job.verify,
        compare: job.compare,
        deadline_ms: job.deadline_ms,
        spec: setup.spec,
    };
    let round = submit_round(&host, &base, &state).await?;
    let first = round.job;
    spawn_local(drive(host, base, state, round, events, sink));
    Ok(first)
}

fn error_event(sink: &Sink, e: &WebError) {
    sink.event(
        "error",
        Obj::new()
            .set("code", e.code)
            .set("message", e.message.as_str()),
    );
}

async fn drive(
    host: Rc<Host>,
    base: Base,
    mut state: TrainState,
    first: Round,
    mut rx: mpsc::UnboundedReceiver<HostEvent>,
    sink: Sink,
) {
    let mut round = Some(first);
    let mut to_forget: BTreeSet<u32> = BTreeSet::new();
    while let Some(ev) = rx.recv().await {
        let mut close = false;
        match (&ev, round.as_mut()) {
            (
                HostEvent::Accepted {
                    job, task, output, ..
                },
                Some(r),
            ) if *job == r.job => {
                if r.seen.insert(*task)
                    && let Some(t) = r.tasks.get(task)
                    && let Err(e) = state.ingest(t, output)
                {
                    web_sys::console::warn_1(&JsValue::from_str(&format!(
                        "training task {task}: unusable output ({e}); left out of the round"
                    )));
                }
                close = r.seen.len() == r.tasks.len();
            }
            (HostEvent::Event(nuee_core::Event::JobDone { job, .. }), Some(r)) if *job == r.job => {
                close = true;
            }
            (HostEvent::Event(nuee_core::Event::JobDone { job, .. }), _)
                if to_forget.contains(job) =>
            {
                if matches!(host.handle.forget(*job).await, Ok(true)) {
                    to_forget.remove(job);
                }
            }
            _ => {}
        }
        host_event(&sink, ev);
        if !close {
            continue;
        }
        let Some(done) = round.take() else {
            continue;
        };
        if state.received() == 0 {
            error_event(
                &sink,
                &WebError::new(
                    code::INTERNAL,
                    format!("round {}: no delta arrived", state.round() + 1),
                ),
            );
            continue;
        }
        let report = match state.finish_round() {
            Ok(r) => r,
            Err(e) => {
                error_event(&sink, &train_err(&e));
                continue;
            }
        };
        if !matches!(host.handle.forget(done.job).await, Ok(true)) {
            to_forget.insert(done.job);
        }
        sink.event(
            "trainRound",
            Obj::new()
                .set("round", state.round())
                .set("rounds", base.spec.rounds)
                .set(
                    "loss",
                    report.mean_loss.map_or(JsValue::NULL, JsValue::from),
                )
                .set("accepted", report.accepted.len() as u32)
                .set("rejected", report.rejected.len() as u32)
                .set("updateNorm", report.update_norm),
        );
        if state.is_done() {
            match state.params().to_safetensors() {
                Ok(bytes) => sink.event(
                    "trainDone",
                    Obj::new()
                        .set("rounds", state.round())
                        .set("params", bytes_js(&bytes)),
                ),
                Err(e) => error_event(&sink, &train_err(&e)),
            }
            continue;
        }
        match submit_round(&host, &base, &state).await {
            Ok(r) => round = Some(r),
            Err(e) => error_event(&sink, &e),
        }
    }
    sink.event("ended", Obj::new().set("reason", "shutdown"));
}
