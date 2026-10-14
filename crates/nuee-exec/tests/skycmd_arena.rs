//! skycmd-arena: the skycmd decision games played by the s3-pico model inside WebAssembly.
//! Deterministic output, and the model holds its own against the oracle on the same seeds.

use std::path::PathBuf;
use std::time::Duration;

use nuee_exec::{RunLimits, RunnerConfig, WasmRunner};
use nuee_proto::{Hash, build_envelope, validate_module};

const MAX_MEMORY: u64 = 256 * 1024 * 1024;
/// s3-pico's calibrated early-exit threshold.
const TAU: f32 = 0.6;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn module() -> Vec<u8> {
    std::fs::read(root().join("target/tasks/skycmd-arena.wasm"))
        .expect("target/tasks/skycmd-arena.wasm (run scripts/build-tasks.sh)")
}

fn model() -> Vec<u8> {
    std::fs::read(root().join("tasks/skycmd-arena/testdata/s3-pico.gguf"))
        .expect("tasks/skycmd-arena/testdata/s3-pico.gguf")
}

struct Arena {
    runner: WasmRunner,
    hash: Hash,
    model: Vec<u8>,
}

impl Arena {
    fn new() -> Self {
        let runner = WasmRunner::new(RunnerConfig::default()).expect("runner");
        let info = runner.prepare(&module()).expect("prepare skycmd-arena");
        Arena {
            runner,
            hash: info.hash,
            model: model(),
        }
    }

    fn run(&self, request: &serde_json::Value) -> Vec<u8> {
        let body = serde_json::to_vec(request).expect("request json");
        let input = build_envelope(&[&self.model], &body);
        let limits = RunLimits {
            deadline: Duration::from_secs(120),
            max_memory: MAX_MEMORY,
            max_output: 16 * 1024 * 1024,
        };
        self.runner
            .run(&self.hash, &input, &limits)
            .unwrap_or_else(|e| panic!("{request}: {e}"))
    }

    fn metrics(&self, mode: &str, game: &str, seed: u64, episodes: u32) -> serde_json::Value {
        let out = self.run(&serde_json::json!({
            "mode": mode, "game": game, "seed": seed, "episodes": episodes, "tau": TAU,
        }));
        let report: serde_json::Value = serde_json::from_slice(&out).expect("report json");
        assert_eq!(report["mode"], mode);
        assert_eq!(report["game"], game);
        assert_eq!(report["seed"], seed);
        assert_eq!(report["episodes"], episodes);
        report["metrics"].clone()
    }
}

#[test]
fn module_is_valid_and_imports_nothing() {
    let bytes = module();
    let info = validate_module(&bytes).expect("valid task module");
    assert!(
        info.max_memory <= MAX_MEMORY,
        "max memory {}",
        info.max_memory
    );
    let engine = wasmtime::Engine::default();
    let compiled = wasmtime::Module::new(&engine, &bytes).expect("compile");
    assert_eq!(compiled.imports().count(), 0);
    println!(
        "skycmd-arena: hash {} size {} max_memory {}; s3-pico.gguf blake3 {}",
        info.hash,
        info.size,
        info.max_memory,
        Hash::of(&model())
    );
}

#[test]
fn eval_is_deterministic() {
    let arena = Arena::new();
    let request = serde_json::json!({
        "mode": "eval", "game": "flappy", "seed": 7, "episodes": 3, "tau": TAU,
    });
    let first = arena.run(&request);
    let second = arena.run(&request);
    assert!(!first.is_empty());
    assert_eq!(Hash::of(&first), Hash::of(&second));
    assert_eq!(first, second);
}

#[test]
fn model_scores_close_to_the_oracle() {
    let arena = Arena::new();
    for (game, episodes) in [("flappy", 3), ("rescue", 5)] {
        let model = arena.metrics("eval", game, 7, episodes);
        let oracle = arena.metrics("oracle", game, 7, episodes);
        let (m, o) = (
            model["mean_score"].as_f64().expect("model score"),
            oracle["mean_score"].as_f64().expect("oracle score"),
        );
        println!("{game}: model {m} oracle {o}");
        assert!(o > 0.0, "{game}: oracle score {o}");
        assert!(m >= 0.95 * o, "{game}: model {m} < 0.95 x oracle {o}");
    }
}

#[test]
fn dagger_emits_the_requested_records() {
    let arena = Arena::new();
    let out = arena.run(&serde_json::json!({
        "mode": "dagger", "game": "flappy", "seed": 7, "n": 200, "tau": TAU,
    }));
    let text = std::str::from_utf8(&out).expect("utf-8");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 200);
    for line in lines {
        let record: serde_json::Value = serde_json::from_str(line).expect("record json");
        let opts = record["opts"].as_array().expect("opts");
        let target = record["target"].as_array().expect("target");
        assert!(!opts.is_empty());
        assert_eq!(target.len(), opts.len());
        assert_eq!(record["src"], "game:flappy");
    }
}

#[test]
fn unknown_game_traps() {
    let arena = Arena::new();
    let body = serde_json::to_vec(&serde_json::json!({
        "mode": "oracle", "game": "chess", "seed": 1, "episodes": 1,
    }))
    .expect("request json");
    let input = build_envelope(&[&arena.model], &body);
    let limits = RunLimits {
        deadline: Duration::from_secs(30),
        max_memory: MAX_MEMORY,
        max_output: 1024,
    };
    let err = arena
        .runner
        .run(&arena.hash, &input, &limits)
        .expect_err("unknown game must fail");
    assert_eq!(err.kind, nuee_proto::FailKind::Trap);
}
