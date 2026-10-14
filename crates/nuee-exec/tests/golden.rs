//! Golden corpus: fixed mandelbrot inputs whose output hashes every engine must reproduce
//! (wasmtime here, the browsers in e2e, `nuee selftest`). Also the NaN lint: the hashes must
//! not change when wasmtime canonicalizes NaNs.

use std::path::PathBuf;
use std::time::Duration;

use nuee_exec::{RunLimits, RunnerConfig, WasmRunner};
use nuee_proto::Hash;
use nuee_proto::limits::MAX_WASM_MEMORY;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn golden_module() -> Vec<u8> {
    std::fs::read(root().join("tasks/golden/mandelbrot.wasm"))
        .expect("tasks/golden/mandelbrot.wasm")
}

fn input(x0: f64, y0: f64, step: f64, width: u32, height: u32, max_iter: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(36);
    v.extend_from_slice(&x0.to_le_bytes());
    v.extend_from_slice(&y0.to_le_bytes());
    v.extend_from_slice(&step.to_le_bytes());
    v.extend_from_slice(&width.to_le_bytes());
    v.extend_from_slice(&height.to_le_bytes());
    v.extend_from_slice(&max_iter.to_le_bytes());
    v
}

/// (name, input, expected BLAKE3 of the output, expected output length).
fn corpus() -> Vec<(&'static str, Vec<u8>, &'static str, usize)> {
    vec![
        (
            "full view 256x256 max_iter 500",
            input(-2.0, -1.5, 3.0 / 256.0, 256, 256, 500),
            FULL_VIEW,
            256 * 256 * 2,
        ),
        (
            "deep zoom 64x64 max_iter 2000",
            input(
                -0.743_643_887_037_151,
                0.131_825_904_205_33,
                1.0e-7,
                64,
                64,
                2000,
            ),
            DEEP_ZOOM,
            64 * 64 * 2,
        ),
        (
            "1x1 origin max_iter 100",
            input(0.0, 0.0, 1.0, 1, 1, 100),
            ONE_PIXEL,
            2,
        ),
    ]
}

const FULL_VIEW: &str = "4997e7fcca5edc96e1296d3f8819a9a5b7247a3bf3f953e4511f5b21b1080b59";
const DEEP_ZOOM: &str = "34f54682a94f9f96256bf6bcf7718ed9fd6f23d4e4712fe31db36ca724152fa4";
const ONE_PIXEL: &str = "d9dba825ecc31a0edc64d04c8535c66a73bc3e74c4c48df84da3272949e38f01";

fn limits() -> RunLimits {
    RunLimits {
        deadline: Duration::from_secs(60),
        max_memory: MAX_WASM_MEMORY,
        max_output: 8 * 1024 * 1024,
    }
}

fn run_corpus(nan_canonicalization: bool) -> Vec<Vec<u8>> {
    let runner = WasmRunner::new(RunnerConfig {
        nan_canonicalization,
        ..RunnerConfig::default()
    })
    .expect("runner");
    let info = runner
        .prepare(&golden_module())
        .expect("prepare golden module");
    corpus()
        .into_iter()
        .map(|(name, inp, _, _)| {
            runner
                .run(&info.hash, &inp, &limits())
                .unwrap_or_else(|e| panic!("{name}: {e}"))
        })
        .collect()
}

#[test]
fn golden_module_matches_fresh_build() {
    let fresh = std::fs::read(root().join("target/tasks/mandelbrot.wasm"))
        .expect("target/tasks/mandelbrot.wasm (run scripts/build-tasks.sh)");
    assert_eq!(Hash::of(&golden_module()), Hash::of(&fresh));
}

#[test]
fn corpus_hashes_are_pinned() {
    let outs = run_corpus(false);
    for ((name, _, _, _), out) in corpus().into_iter().zip(&outs) {
        println!("{name}: {}", Hash::of(out));
    }
    for ((name, _, expected, len), out) in corpus().into_iter().zip(&outs) {
        assert_eq!(out.len(), len, "{name}: output length");
        assert_eq!(Hash::of(out).to_string(), expected, "{name}: output hash");
    }
}

#[test]
fn one_pixel_never_escapes() {
    let outs = run_corpus(false);
    assert_eq!(outs[2], 100u16.to_le_bytes());
}

#[test]
fn nan_canonicalization_does_not_change_outputs() {
    assert_eq!(run_corpus(false), run_corpus(true));
}

#[test]
fn two_runs_are_identical() {
    assert_eq!(run_corpus(false), run_corpus(false));
}
