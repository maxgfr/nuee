//! Hostile and broken modules must fail with the right classification, never hang.

use std::time::{Duration, Instant};

use nuee_exec::{ExecError, RunLimits, RunnerConfig, WasmRunner};
use nuee_proto::FailKind;

const MIB: u64 = 1024 * 1024;

fn runner() -> WasmRunner {
    WasmRunner::new(RunnerConfig::default()).expect("runner")
}

fn limits() -> RunLimits {
    RunLimits {
        deadline: Duration::from_secs(5),
        max_memory: 64 * MIB,
        max_output: 1024 * 1024,
    }
}

/// A task module: 2-page memory (max `max_pages`), a bump allocator at 1024, and `body`
/// as the body of `nuee_run (param $p i32) (param $n i32) (result i64)`.
fn task(max_pages: u32, extra: &str, body: &str) -> Vec<u8> {
    let src = format!(
        r#"(module
  (memory (export "memory") 2 {max_pages})
  (func (export "nuee_alloc") (param i32) (result i32) i32.const 1024)
  {extra}
  (func (export "nuee_run") (param $p i32) (param $n i32) (result i64)
    {body}))"#
    );
    wat::parse_str(src).expect("wat")
}

fn run(module: &[u8], input: &[u8], limits: &RunLimits) -> Result<Vec<u8>, ExecError> {
    let r = runner();
    let info = r.prepare(module)?;
    r.run(&info.hash, input, limits)
}

fn fail_kind(module: &[u8], limits: &RunLimits) -> FailKind {
    match run(module, b"hello", limits) {
        Ok(out) => panic!("expected a failure, got {} output bytes", out.len()),
        Err(e) => e.kind,
    }
}

const ECHO: &str =
    "local.get $p i64.extend_i32_u i64.const 32 i64.shl local.get $n i64.extend_i32_u i64.or";

#[test]
fn echo_returns_input() {
    let m = task(16, "", ECHO);
    assert_eq!(
        run(&m, b"hello swarm", &limits()).expect("run"),
        b"hello swarm"
    );
    assert_eq!(run(&m, b"", &limits()).expect("empty"), b"");
}

#[test]
fn infinite_loop_times_out() {
    let m = task(16, "", "(loop br 0) unreachable");
    let deadline = Duration::from_millis(200);
    let start = Instant::now();
    let kind = fail_kind(
        &m,
        &RunLimits {
            deadline,
            ..limits()
        },
    );
    let took = start.elapsed();
    assert_eq!(kind, FailKind::Timeout);
    assert!(took >= deadline, "stopped early: {took:?}");
    assert!(
        took < deadline * 2 + Duration::from_millis(100),
        "too slow: {took:?}"
    );
}

#[test]
fn memory_bomb_is_oom() {
    let body = "(loop (br_if 0 (i32.ne (memory.grow (i32.const 16)) (i32.const -1)))) unreachable";
    // Declared maximum (16 MiB) within the 64 MiB local cap: growth stops at the declaration.
    let m = task(256, "", body);
    assert_eq!(fail_kind(&m, &limits()), FailKind::Oom);
}

#[test]
fn unbounded_recursion_is_stack_overflow() {
    let m = task(
        16,
        "(func $f (param i64) (result i64) (i64.add (call $f (local.get 0)) (i64.const 1)))",
        "(call $f (i64.const 0))",
    );
    assert_eq!(fail_kind(&m, &limits()), FailKind::StackOverflow);
}

#[test]
fn unreachable_is_trap() {
    let m = task(16, "", "unreachable");
    assert_eq!(fail_kind(&m, &limits()), FailKind::Trap);
}

#[test]
fn division_by_zero_is_trap() {
    let m = task(16, "", "(i64.div_u (i64.const 1) (i64.const 0))");
    assert_eq!(fail_kind(&m, &limits()), FailKind::Trap);
}

#[test]
fn output_out_of_bounds_is_invalid() {
    // Pointer far beyond the 1 MiB memory.
    let m = task(
        16,
        "",
        "(i64.or (i64.shl (i64.const 0x7fff0000) (i64.const 32)) (i64.const 16))",
    );
    assert_eq!(fail_kind(&m, &limits()), FailKind::Invalid);
    // In-bounds pointer, length running past the end.
    let m = task(
        16,
        "",
        "(i64.or (i64.shl (i64.const 1024) (i64.const 32)) (i64.const 0x10000000))",
    );
    assert_eq!(fail_kind(&m, &limits()), FailKind::Invalid);
}

#[test]
fn output_above_cap_is_invalid() {
    let m = task(
        16,
        "",
        "(i64.or (i64.shl (i64.const 0) (i64.const 32)) (i64.const 4096))",
    );
    let small = RunLimits {
        max_output: 100,
        ..limits()
    };
    assert_eq!(fail_kind(&m, &small), FailKind::Invalid);
}

#[test]
fn alloc_returning_zero_is_oom() {
    let src = r#"(module
  (memory (export "memory") 1 1)
  (func (export "nuee_alloc") (param i32) (result i32) i32.const 0)
  (func (export "nuee_run") (param i32 i32) (result i64) i64.const 0))"#;
    let m = wat::parse_str(src).expect("wat");
    assert_eq!(fail_kind(&m, &limits()), FailKind::Oom);
}

#[test]
fn import_is_invalid_at_prepare() {
    let src = r#"(module
  (import "env" "now" (func (result i64)))
  (memory (export "memory") 1 1)
  (func (export "nuee_alloc") (param i32) (result i32) i32.const 8)
  (func (export "nuee_run") (param i32 i32) (result i64) i64.const 0))"#;
    let m = wat::parse_str(src).expect("wat");
    let err = runner().prepare(&m).expect_err("imports must be refused");
    assert_eq!(err.kind, FailKind::Invalid);
}

/// A declared maximum above the worker's local memory cap is refused up front as
/// `Invalid`, before instantiation, even if the module would only use a little.
#[test]
fn declared_max_above_local_cap_is_invalid() {
    let m = task(16384, "", ECHO); // 1 GiB declared
    let err = run(&m, b"x", &limits()).expect_err("must be refused");
    assert_eq!(err.kind, FailKind::Invalid);
    // The same module runs once the local cap allows its declaration.
    let roomy = RunLimits {
        max_memory: 1024 * MIB,
        ..limits()
    };
    assert_eq!(run(&m, b"x", &roomy).expect("run"), b"x");
}

#[test]
fn relaxed_simd_is_rejected_by_validator_and_engine() {
    let m = task(
        16,
        "(func (result v128) (i32x4.relaxed_trunc_f32x4_s (v128.const i64x2 0 0)))",
        ECHO,
    );
    let r = runner();
    assert_eq!(
        r.prepare(&m).expect_err("relaxed simd").kind,
        FailKind::Invalid
    );
    assert!(nuee_proto::validate_module(&m).is_err());
    assert!(wasmtime::Module::new(r.engine(), &m).is_err());
}

#[test]
fn shared_memory_is_rejected_by_validator_and_engine() {
    let src = r#"(module
  (memory (export "memory") 1 1 shared)
  (func (export "nuee_alloc") (param i32) (result i32) i32.const 8)
  (func (export "nuee_run") (param i32 i32) (result i64) i64.const 0))"#;
    let m = wat::parse_str(src).expect("wat");
    let r = runner();
    assert_eq!(
        r.prepare(&m).expect_err("shared memory").kind,
        FailKind::Invalid
    );
    assert!(nuee_proto::validate_module(&m).is_err());
    assert!(wasmtime::Module::new(r.engine(), &m).is_err());
}

#[test]
fn allowed_features_compile_in_both() {
    // SIMD, bulk memory, sign-ext, saturating conversions and multi-value together.
    let extra = r#"(func (result i32 i32)
    (drop (i32x4.add (v128.const i32x4 1 2 3 4) (v128.const i32x4 1 2 3 4)))
    (memory.fill (i32.const 0) (i32.const 0) (i32.const 8))
    (i32.extend8_s (i32.trunc_sat_f32_s (f32.const 1.5)))
    (i32.const 2))"#;
    let m = task(16, extra, ECHO);
    let r = runner();
    assert!(nuee_proto::validate_module(&m).is_ok());
    assert!(wasmtime::Module::new(r.engine(), &m).is_ok());
    let info = r.prepare(&m).expect("prepare");
    assert_eq!(r.run(&info.hash, b"ok", &limits()).expect("run"), b"ok");
}

#[test]
fn unknown_module_is_invalid() {
    let r = runner();
    let err = r
        .run(&nuee_proto::Hash::of(b"nothing"), b"", &limits())
        .expect_err("unprepared");
    assert_eq!(err.kind, FailKind::Invalid);
}

#[test]
fn async_run_works() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let r = std::sync::Arc::new(runner());
    let m = task(16, "", ECHO);
    let info = r.prepare(&m).expect("prepare");
    let out = rt
        .block_on(r.run_async(info.hash, b"async".to_vec(), limits()))
        .expect("run");
    assert_eq!(out, b"async");
}
