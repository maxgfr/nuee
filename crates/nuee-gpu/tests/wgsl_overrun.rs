//! A WGSL task that overruns its deadline, alone in its test binary.
//!
//! GPU work cannot be preempted, and on the macOS runner's paravirtual GPU the endless shader
//! keeps the GPU busy even after its process exits: GPU work queued after it waits past any
//! deadline. Cargo runs a package's integration tests in name order, so this binary runs after
//! `wgsl.rs`, and no later test binary uses the GPU. It needs an adapter, like `wgsl.rs`.

use std::time::{Duration, Instant};

use nuee_gpu::GpuRunner;
use nuee_gpu::wgsl::{BindingSpec, MANDELBROT_WGSL, encode_input, mandelbrot_input};
use nuee_proto::FailKind;

/// Set in the child process that runs the endless shader.
const ENDLESS_CHILD: &str = "NUEE_GPU_ENDLESS_CHILD";

/// An endless shader with a short deadline fails with Timeout (or Trap if the driver kills the
/// work first), and the runner refuses new work while it still occupies the GPU.
///
/// wgpu bounds shader loops by default, but at 2^64 iterations. On Metal the shader keeps
/// running after the deadline at least until the process exits, so it runs in a child process.
#[test]
fn endless_shader_hits_the_deadline() {
    if std::env::var_os(ENDLESS_CHILD).is_none() {
        let exe = std::env::current_exe().expect("test binary path");
        let out = std::process::Command::new(exe)
            .args(["endless_shader_hits_the_deadline", "--exact", "--nocapture"])
            .env(ENDLESS_CHILD, "1")
            .output()
            .expect("start the child test");
        let log = String::from_utf8_lossy(&out.stderr);
        eprintln!("{log}");
        assert!(out.status.success(), "child failed: {log}");
        assert!(log.contains("endless shader:"), "the child ran the shader");
        return;
    }
    let gpu = GpuRunner::new().expect(
        "no GPU adapter found: these tests need one (Metal on macOS, lavapipe/Vulkan on Linux)",
    );
    let wgsl = "@group(0) @binding(0) var<storage, read_write> o: array<u32>;\n\
                @compute @workgroup_size(64) fn main(@builtin(global_invocation_id) id: vec3<u32>) {\n\
                  var i = 0u;\n\
                  loop { i = i * 1664525u + 1013904223u; o[id.x] = i; if (o[0] == 7u && i == 0xffffffffu) { break; } }\n\
                }";
    let input = encode_input("main", [64, 1, 1], &[BindingSpec::Output(0, 64 * 64 * 4)]);
    let start = Instant::now();
    let err = gpu
        .run(wgsl, &input, Duration::from_millis(500))
        .unwrap_err();
    eprintln!("endless shader: {err} after {:?}", start.elapsed());
    assert!(
        matches!(err.kind, FailKind::Timeout | FailKind::Trap),
        "{err}"
    );
    assert!(start.elapsed() < Duration::from_secs(20));
    if err.kind == FailKind::Timeout {
        let next = gpu
            .run(
                MANDELBROT_WGSL,
                &mandelbrot_input(0.0, 0.0, 1.0, 8, 8, 10),
                Duration::from_secs(5),
            )
            .unwrap_err();
        assert_eq!(next.kind, FailKind::Unsupported, "{next}");
    }
}
