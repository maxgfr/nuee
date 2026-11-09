//! Task execution for Nuée workers: WebAssembly tasks in wasmtime under strict limits, and
//! opt-in native commands from allowlisted repository revisions.
#![forbid(unsafe_code)]

pub mod native;
pub mod wasm;

pub use native::{
    AllowEntry, BUILD_LOG, ExecApproval, ExecPolicy, MAX_KEPT, NativeRunner, RepoRule, RepoSource,
    Sandbox, TrainArgs, build_argv, kept_path_ok, shard_argv, shard_exit_code,
};
pub use wasm::{ExecError, RunLimits, RunnerConfig, WasmRunner};
