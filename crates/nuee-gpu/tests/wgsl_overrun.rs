//! A WGSL task that overruns its deadline, alone in its test binary.
//!
//! GPU work cannot be preempted, and on the macOS runner's paravirtual GPU a shader keeps the
//! GPU busy even after its process exits: an endless one stalled every later GPU user of the
//! job, down to the browsers of the e2e stage. So the shader here only outlasts its deadline,
//! and the test ends once the GPU takes work again. Cargo runs a package's integration tests in
//! name order, so this binary runs after `wgsl.rs`. It needs an adapter, like `wgsl.rs`.

use std::time::{Duration, Instant};

use nuee_gpu::GpuRunner;
use nuee_gpu::wgsl::{BindingSpec, MANDELBROT_WGSL, encode_input, mandelbrot_input};
use nuee_proto::FailKind;

/// Set in the child process that runs the long shader.
const OVERRUN_CHILD: &str = "NUEE_GPU_OVERRUN_CHILD";

/// A shader that outlasts a short deadline fails with Timeout (or Trap if the driver kills the
/// work first); the runner refuses new work while it still occupies the GPU, and takes work
/// again once it ends.
///
/// Each of 64 threads runs 500 million dependent multiply-adds, which takes seconds (10 on an
/// Apple GPU), well past the 500 ms deadline. On Metal the shader keeps running after the
/// deadline, so it runs in a child process, which waits for it to end.
#[test]
fn long_shader_hits_the_deadline_then_frees_the_gpu() {
    if std::env::var_os(OVERRUN_CHILD).is_none() {
        let exe = std::env::current_exe().expect("test binary path");
        let out = std::process::Command::new(exe)
            .args([
                "long_shader_hits_the_deadline_then_frees_the_gpu",
                "--exact",
                "--nocapture",
            ])
            .env(OVERRUN_CHILD, "1")
            .output()
            .expect("start the child test");
        let log = String::from_utf8_lossy(&out.stderr);
        eprintln!("{log}");
        assert!(out.status.success(), "child failed: {log}");
        assert!(log.contains("GPU free again"), "the child ran the shader");
        return;
    }
    let gpu = GpuRunner::new().expect(
        "no GPU adapter found: these tests need one (Metal on macOS, lavapipe/Vulkan on Linux)",
    );
    let wgsl = "@group(0) @binding(0) var<storage, read_write> o: array<u32>;\n\
                @compute @workgroup_size(64) fn main(@builtin(global_invocation_id) id: vec3<u32>) {\n\
                  var i = id.x;\n\
                  for (var n = 0u; n < 500000000u; n++) { i = i * 1664525u + 1013904223u; }\n\
                  o[id.x] = i;\n\
                }";
    let input = encode_input("main", [1, 1, 1], &[BindingSpec::Output(0, 64 * 4)]);
    let start = Instant::now();
    let err = gpu
        .run(wgsl, &input, Duration::from_millis(500))
        .unwrap_err();
    eprintln!("long shader: {err} after {:?}", start.elapsed());
    assert!(
        matches!(err.kind, FailKind::Timeout | FailKind::Trap),
        "{err}"
    );
    assert!(start.elapsed() < Duration::from_secs(20));
    let tile = mandelbrot_input(0.0, 0.0, 1.0, 8, 8, 10);
    if err.kind == FailKind::Timeout {
        let next = gpu
            .run(MANDELBROT_WGSL, &tile, Duration::from_secs(5))
            .unwrap_err();
        assert_eq!(next.kind, FailKind::Unsupported, "{next}");
    }
    // Nothing of this test may outlive it on the GPU.
    let limit = Instant::now() + Duration::from_secs(300);
    loop {
        match gpu.run(MANDELBROT_WGSL, &tile, Duration::from_secs(30)) {
            Ok(_) => break,
            Err(e) if e.kind == FailKind::Unsupported && Instant::now() < limit => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("the GPU never took work again: {e}"),
        }
    }
    eprintln!("GPU free again after {:?}", start.elapsed());
}
