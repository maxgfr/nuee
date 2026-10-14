//! toy-trainer: the WASM DiLoCo trainer on UCI optdigits. Exact outputs, and a DiLoCo run
//! through nuee-train where 4 workers do at least as well as 1 for the same steps per worker.

use std::path::PathBuf;
use std::time::Duration;

use nuee_exec::{RunLimits, RunnerConfig, WasmRunner};
use nuee_proto::{DeltaCodec, Hash, TrainSpec, TrainerKind, build_envelope};
use nuee_train::{Params, TaskInput, TrainState};

const MAX_MEMORY: u64 = 256 * 1024 * 1024;
/// Outer rounds of the DiLoCo runs.
const ROUNDS: u32 = 10;
/// Inner steps per worker and round.
const INNER_STEPS: u32 = 30;
const INNER_LR: f32 = 0.1;
const INIT_SEED: u64 = 7;
const RUN_SEED: u64 = 2024;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(rel: &str) -> Vec<u8> {
    std::fs::read(root().join(rel))
        .unwrap_or_else(|e| panic!("{rel} (run scripts/build-tasks.sh): {e}"))
}

struct Trainer {
    runner: WasmRunner,
    hash: Hash,
    dataset: Vec<u8>,
}

impl Trainer {
    fn new() -> Self {
        let runner = WasmRunner::new(RunnerConfig::default()).expect("runner");
        let info = runner
            .prepare(&read("target/tasks/toy-trainer.wasm"))
            .expect("prepare toy-trainer");
        Trainer {
            runner,
            hash: info.hash,
            dataset: read("target/tasks/optdigits.bin"),
        }
    }

    fn run(&self, lease: &[u8]) -> Vec<u8> {
        let input = build_envelope(&[&self.dataset], lease);
        let limits = RunLimits {
            deadline: Duration::from_secs(60),
            max_memory: MAX_MEMORY,
            max_output: 1024 * 1024,
        };
        self.runner
            .run(&self.hash, &input, &limits)
            .expect("toy-trainer run")
    }

    fn init(&self) -> Params {
        let mut lease = b"NIN1".to_vec();
        lease.extend_from_slice(&INIT_SEED.to_le_bytes());
        Params::from_safetensors(&self.run(&lease)).expect("init params")
    }

    /// Test accuracy and loss of `params`.
    fn evaluate(&self, params: &Params) -> (f32, f32) {
        let mut lease = b"NEV1".to_vec();
        lease.extend_from_slice(&params.to_safetensors().expect("safetensors"));
        let out = self.run(&lease);
        assert_eq!(
            out.len(),
            8 + 1797,
            "accuracy, loss and one prediction per test sample"
        );
        let f = |i: usize| f32::from_le_bytes(out[i..i + 4].try_into().expect("4 bytes"));
        (f(0), f(4))
    }
}

fn spec(workers: u16) -> TrainSpec {
    TrainSpec {
        rounds: ROUNDS,
        inner_steps: INNER_STEPS,
        workers,
        outer_lr: 0.7,
        outer_momentum: 0.9,
        init: Hash::of(b"toy-trainer init"),
        trainer: TrainerKind::Wasm,
        codec: DeltaCodec::F32,
        trim: 0.2,
        cos_min: 0.0,
        norm_max_ratio: 3.0,
    }
}

struct Run {
    losses: Vec<f32>,
    accuracy: f32,
    test_loss: f32,
}

fn diloco(t: &Trainer, workers: u16) -> Run {
    let mut state = TrainState::new(spec(workers), t.init(), RUN_SEED).expect("train state");
    state.set_inner_lr(INNER_LR);
    let mut losses = Vec::new();
    while !state.is_done() {
        for task in state.round_tasks(state.round()) {
            let out = t.run(&state.task_input(&task));
            state.ingest(&task, &out).expect("ingest");
        }
        let report = state.finish_round().expect("finish round");
        assert!(
            report.rejected.is_empty(),
            "rejected: {:?}",
            report.rejected
        );
        losses.push(report.mean_loss.expect("mean loss"));
    }
    let (accuracy, test_loss) = t.evaluate(state.params());
    Run {
        losses,
        accuracy,
        test_loss,
    }
}

#[test]
fn dataset_is_the_uci_split() {
    let d = read("target/tasks/optdigits.bin");
    assert_eq!(&d[..4], b"OPTD");
    assert_eq!(u32::from_le_bytes(d[4..8].try_into().unwrap()), 3823);
    assert_eq!(u32::from_le_bytes(d[8..12].try_into().unwrap()), 1797);
    assert_eq!(d.len(), 12 + (3823 + 1797) * 65);
}

#[test]
fn same_lease_same_bytes() {
    let t = Trainer::new();
    let init = t.init();
    assert_eq!(init.data.len(), 2410);
    assert_eq!(init.names, ["b1", "b2", "w1", "w2"]);
    let params = init.to_safetensors().expect("safetensors");
    let lease = TaskInput {
        steps: 20,
        shard: 1,
        n_shards: 4,
        seed: 99,
        lr: INNER_LR,
        params: &params,
    }
    .encode();
    let a = t.run(&lease);
    let b = t.run(&lease);
    assert_eq!(a, b);
    assert_eq!(blake3::hash(&a), blake3::hash(&b));
    let (delta, loss) = nuee_train::split_output(&a).expect("loss trailer");
    assert!(loss.is_finite() && loss > 0.0, "loss {loss}");
    let delta = nuee_train::decode_delta(delta, &init.layout()).expect("F32 delta");
    assert!(delta.iter().any(|d| *d != 0.0));
    // The same parameters from another seed give another delta.
    let other = TaskInput {
        steps: 20,
        shard: 1,
        n_shards: 4,
        seed: 100,
        lr: INNER_LR,
        params: &params,
    }
    .encode();
    assert_ne!(t.run(&other), a);
}

#[test]
fn four_workers_beat_one_and_the_loss_falls() {
    let t = Trainer::new();
    let (acc0, _) = t.evaluate(&t.init());
    let one = diloco(&t, 1);
    let four = diloco(&t, 4);
    eprintln!(
        "R={ROUNDS} H={INNER_STEPS}: init acc {acc0:.4}; 1 worker acc {:.4} loss {:.4}; 4 workers acc {:.4} loss {:.4}",
        one.accuracy, one.test_loss, four.accuracy, four.test_loss
    );
    eprintln!("1 worker losses {:?}", one.losses);
    eprintln!("4 worker losses {:?}", four.losses);
    assert!(
        four.accuracy >= one.accuracy,
        "4 workers {} < 1 worker {}",
        four.accuracy,
        one.accuracy
    );
    assert!(four.accuracy >= 0.85, "4-worker accuracy {}", four.accuracy);
    for run in [&one, &four] {
        let (first, last) = (run.losses[0], run.losses[run.losses.len() - 1]);
        assert!(last < first, "loss did not fall: {first} -> {last}");
    }
}
