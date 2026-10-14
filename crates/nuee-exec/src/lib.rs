//! Task execution for Nuée workers: WebAssembly tasks in wasmtime under strict limits, and
//! opt-in native commands from allowlisted repository revisions.
#![forbid(unsafe_code)]

pub mod native;
pub mod wasm;

pub use native::{
    AllowEntry, ExecApproval, ExecPolicy, NativeRunner, RepoSource, Sandbox, TrainArgs,
};
pub use wasm::{ExecError, RunLimits, RunnerConfig, WasmRunner};
