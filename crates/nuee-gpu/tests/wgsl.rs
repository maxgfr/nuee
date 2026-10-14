//! WGSL tasks: input format, comparators, and the wgpu runner on this machine's GPU.
//!
//! The GPU tests need an adapter (Metal on macOS, lavapipe on the Linux CI runner). Without
//! one they fail: they never skip.

use std::time::{Duration, Instant};

use nuee_core::Compare;
use nuee_gpu::GpuRunner;
use nuee_gpu::verify::{Elem, ElementTolerance, comparator};
use nuee_gpu::wgsl::{
    BindingKind, BindingSpec, MANDELBROT_COMPARE, MANDELBROT_WGSL, check_shader, encode_input,
    mandelbrot_input, parse_input,
};
use nuee_proto::{Comparator, FailKind, TaskKind};

fn runner() -> GpuRunner {
    GpuRunner::new().expect(
        "no GPU adapter found: these tests need one (Metal on macOS, lavapipe/Vulkan on Linux)",
    )
}

/// The CPU task's formula (tasks/mandelbrot), in f64, widened to u32.
fn cpu_mandelbrot(x0: f64, y0: f64, step: f64, w: u32, h: u32, max_iter: u32) -> Vec<u8> {
    let mut out = Vec::new();
    for row in 0..h {
        let ci = y0 + f64::from(row) * step;
        for col in 0..w {
            let cr = x0 + f64::from(col) * step;
            let (mut zr, mut zi) = (0.0f64, 0.0f64);
            let mut n = 0u32;
            while n < max_iter {
                if zr * zr + zi * zi > 4.0 {
                    break;
                }
                let t = zr * zr - zi * zi + cr;
                zi = 2.0 * zr * zi + ci;
                zr = t;
                n += 1;
            }
            out.extend_from_slice(&u32::from(n as u16).to_le_bytes());
        }
    }
    out
}

fn u32s(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn f32s(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

// --- GPU ---------------------------------------------------------------------------------------

#[test]
fn mandelbrot_tile_agrees_with_the_cpu_task() {
    let gpu = runner();
    eprintln!("adapter: {:?}", gpu.caps());
    let tiles = [
        (-2.0f64, -1.5f64, 3.0 / 64.0, 500u32),
        (-0.75, 0.05, 0.1 / 64.0, 1000),
    ];
    let cmp = comparator(TaskKind::Wgsl, &MANDELBROT_COMPARE).expect("tolerance comparator");
    let exact = ElementTolerance {
        elem: Elem::U32,
        abs: 2.0,
        rel: 0.0,
        max_bad_permille: 20,
    };
    for (x0, y0, step, iter) in tiles {
        let input = mandelbrot_input(x0 as f32, y0 as f32, step as f32, 64, 64, iter);
        let out = gpu
            .run(MANDELBROT_WGSL, &input, Duration::from_secs(30))
            .expect("the shader runs");
        let cpu = cpu_mandelbrot(x0, y0, step, 64, 64, iter);
        assert_eq!(out.len(), 64 * 64 * 4);
        let bad = exact.bad_elements(&out, &cpu).expect("same length");
        eprintln!("tile ({x0}, {y0}): {bad} of 4096 pixels outside ±2");
        assert!(
            cmp.same(&out, &cpu),
            "{bad} pixels disagree (at most 204 allowed)"
        );
        assert!(out != vec![0u8; out.len()], "the output is not all zeros");
    }
}

#[test]
fn invalid_wgsl_is_invalid() {
    let input = mandelbrot_input(0.0, 0.0, 1.0, 8, 8, 10);
    let task = parse_input(&input).unwrap();
    // Checked without any device.
    let err = check_shader("fn main( {", &task).unwrap_err();
    assert_eq!(err.kind, FailKind::Invalid);
    let typed = "@group(0) @binding(1) var<storage, read_write> o: array<u32>;\n\
                 @compute @workgroup_size(1) fn main() { o[0] = 1.5; }";
    assert_eq!(
        check_shader(typed, &task).unwrap_err().kind,
        FailKind::Invalid
    );
    let err = runner()
        .run("fn main( {", &input, Duration::from_secs(5))
        .unwrap_err();
    assert_eq!(err.kind, FailKind::Invalid, "{err}");
}

#[test]
fn binding_mismatches_are_invalid() {
    let gpu = runner();
    let params = [0u8; 24];
    let cases = [
        // The output declared read-only.
        encode_input(
            "main",
            [1, 1, 1],
            &[
                BindingSpec::Uniform(0, &params),
                BindingSpec::ReadOnly(1, &[0; 256]),
            ],
        ),
        // Binding 1 missing from the input.
        encode_input(
            "main",
            [1, 1, 1],
            &[
                BindingSpec::Uniform(0, &params),
                BindingSpec::Output(2, 256),
            ],
        ),
        // The uniform declared as storage.
        encode_input(
            "main",
            [1, 1, 1],
            &[
                BindingSpec::ReadOnly(0, &params),
                BindingSpec::Output(1, 256),
            ],
        ),
        // A binding the shader does not use.
        encode_input(
            "main",
            [1, 1, 1],
            &[
                BindingSpec::Uniform(0, &params),
                BindingSpec::Output(1, 256),
                BindingSpec::Output(3, 4),
            ],
        ),
        // No such entry point.
        encode_input(
            "nope",
            [1, 1, 1],
            &[
                BindingSpec::Uniform(0, &params),
                BindingSpec::Output(1, 256),
            ],
        ),
    ];
    for (i, input) in cases.iter().enumerate() {
        let err = gpu
            .run(MANDELBROT_WGSL, input, Duration::from_secs(5))
            .unwrap_err();
        assert_eq!(err.kind, FailKind::Invalid, "case {i}: {err}");
    }
    // A uniform buffer smaller than the shader's struct is caught by wgpu's validation.
    let small = encode_input(
        "main",
        [1, 1, 1],
        &[
            BindingSpec::Uniform(0, &[0; 8]),
            BindingSpec::Output(1, 256),
        ],
    );
    let err = gpu
        .run(MANDELBROT_WGSL, &small, Duration::from_secs(5))
        .unwrap_err();
    assert_eq!(err.kind, FailKind::Invalid, "{err}");
}

/// Set in the child process that runs the endless shader.
const ENDLESS_CHILD: &str = "NUEE_GPU_ENDLESS_CHILD";

/// An endless shader with a short deadline fails with Timeout (or Trap if the driver kills the
/// work first), and the runner refuses new work while it still occupies the GPU.
///
/// wgpu bounds shader loops by default, but at 2^64 iterations. GPU work cannot be preempted:
/// on Metal it keeps running after the deadline until the process exits, so the shader runs in
/// a child process to leave the GPU free for the other tests.
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
    let gpu = runner();
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

// --- Input format --------------------------------------------------------------------------

#[test]
fn input_round_trips() {
    let input = mandelbrot_input(-2.0, -1.5, 0.25, 64, 32, 100);
    let t = parse_input(&input).unwrap();
    assert_eq!(t.entry, "main");
    assert_eq!(t.workgroups, [8, 4, 1]);
    assert_eq!(t.bindings.len(), 2);
    assert_eq!(t.bindings[0].kind, BindingKind::Uniform);
    assert_eq!(t.bindings[0].data.len(), 24);
    assert_eq!(t.bindings[1].kind, BindingKind::Output);
    assert_eq!(t.output_len(), 64 * 32 * 4);
}

#[test]
fn malformed_inputs_are_invalid() {
    let good = mandelbrot_input(0.0, 0.0, 1.0, 8, 8, 10);
    for cut in 0..good.len() {
        assert!(parse_input(&good[..cut]).is_err(), "truncated at {cut}");
    }
    let mut trailing = good.clone();
    trailing.push(0);
    assert!(parse_input(&trailing).is_err());
    let mut magic = good.clone();
    magic[3] = b'2';
    assert!(parse_input(&magic).is_err());
    let cases = [
        encode_input("main", [0, 1, 1], &[BindingSpec::Output(0, 4)]),
        encode_input("main", [65_536, 1, 1], &[BindingSpec::Output(0, 4)]),
        encode_input("", [1, 1, 1], &[BindingSpec::Output(0, 4)]),
        encode_input("main", [1, 1, 1], &[]),
        encode_input("main", [1, 1, 1], &[BindingSpec::ReadOnly(0, &[0; 4])]),
        encode_input("main", [1, 1, 1], &[BindingSpec::Output(0, 6)]),
        encode_input("main", [1, 1, 1], &[BindingSpec::Output(0, 0)]),
        encode_input("main", [1, 1, 1], &[BindingSpec::Output(0, (64 << 20) + 4)]),
        encode_input(
            "main",
            [1, 1, 1],
            &[BindingSpec::Output(0, 4), BindingSpec::Output(0, 4)],
        ),
        encode_input(
            "main",
            [1, 1, 1],
            &[
                BindingSpec::Output(0, 64 << 20),
                BindingSpec::Output(1, 64 << 20),
                BindingSpec::Output(2, 4),
            ],
        ),
        encode_input("main", [1, 1, 1], &[BindingSpec::Output(0, 4); 9]),
    ];
    for (i, c) in cases.iter().enumerate() {
        let err = parse_input(c).unwrap_err();
        assert_eq!(err.kind, FailKind::Invalid, "case {i}");
    }
    // A huge declared data size is refused without allocating it.
    let mut lying = encode_input(
        "main",
        [1, 1, 1],
        &[BindingSpec::ReadOnly(0, &[0; 4]), BindingSpec::Output(1, 4)],
    );
    // magic, entry length, "main", 3 workgroup counts, count, binding, kind: then the size.
    lying[36..40].copy_from_slice(&(64u32 << 20).to_le_bytes());
    assert_eq!(parse_input(&lying).unwrap_err().kind, FailKind::Invalid);
}

// --- Comparators ---------------------------------------------------------------------------

fn tol(elem: Elem, abs: f32, rel: f32, permille: u16) -> ElementTolerance {
    ElementTolerance {
        elem,
        abs,
        rel,
        max_bad_permille: permille,
    }
}

#[test]
fn nan_only_equals_nan() {
    let t = tol(Elem::F32, 1e9, 1.0, 0);
    assert!(t.same(&f32s(&[f32::NAN]), &f32s(&[f32::NAN])));
    assert!(!t.same(&f32s(&[f32::NAN]), &f32s(&[1.0])));
    assert!(!t.same(&f32s(&[1.0]), &f32s(&[f32::NAN])));
    assert!(t.same(&f32s(&[f32::INFINITY]), &f32s(&[f32::INFINITY])));
}

#[test]
fn lengths_must_match() {
    let t = tol(Elem::U32, 100.0, 0.0, 1000);
    assert!(!t.same(&u32s(&[1, 2]), &u32s(&[1])));
    assert!(!t.same(&[0, 0, 0], &[0, 0, 0]));
    assert!(t.same(&[], &[]));
}

#[test]
fn absolute_and_relative_tolerances() {
    let a = tol(Elem::U32, 2.0, 0.0, 0);
    assert!(a.same(&u32s(&[10, 500]), &u32s(&[12, 498])));
    assert!(!a.same(&u32s(&[10]), &u32s(&[13])));
    assert!(a.same(&u32s(&[0]), &u32s(&[2])));
    assert!(!a.same(&u32s(&[u32::MAX]), &u32s(&[0])));
    let r = tol(Elem::F32, 0.0, 0.01, 0);
    assert!(r.same(&f32s(&[100.9]), &f32s(&[100.0])));
    assert!(!r.same(&f32s(&[101.5]), &f32s(&[100.0])));
    assert!(r.same(&f32s(&[-0.0]), &f32s(&[0.0])));
}

#[test]
fn bad_element_threshold() {
    let t = tol(Elem::U32, 0.0, 0.0, 20);
    let base = vec![7u32; 1000];
    let mut v = base.clone();
    for x in v.iter_mut().take(20) {
        *x = 0;
    }
    assert!(t.same(&u32s(&v), &u32s(&base)), "20 of 1000 is allowed");
    v[20] = 0;
    assert!(!t.same(&u32s(&v), &u32s(&base)), "21 of 1000 is not");
    let strict = tol(Elem::U32, 0.0, 0.0, 0);
    assert!(!strict.same(&u32s(&[1, 2]), &u32s(&[1, 3])));
}

#[test]
fn comparator_follows_the_manifest() {
    let c = comparator(TaskKind::Wgsl, &MANDELBROT_COMPARE).unwrap();
    assert!(c.same(&u32s(&[100, 3]), &u32s(&[98, 5])));
    assert!(!c.same(&u32s(&[100]), &u32s(&[97])));
    assert!(comparator(TaskKind::Wgsl, &Comparator::Exact).is_none());
    let f = comparator(
        TaskKind::Embed,
        &Comparator::ElementTolerance {
            abs: 0.5,
            rel: 0.0,
            max_bad_permille: 0,
        },
    )
    .unwrap();
    assert!(f.same(&f32s(&[1.0]), &f32s(&[1.4])));
}

/// Escape counts of a tile in f32, the shader's formula, with or without fused multiply-adds.
fn f32_mandelbrot(x0: f32, y0: f32, step: f32, size: u32, max_iter: u32, fma: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for row in 0..size {
        for col in 0..size {
            let (cr, ci) = (x0 + col as f32 * step, y0 + row as f32 * step);
            let (mut zr, mut zi, mut n) = (0f32, 0f32, 0u32);
            while n < max_iter && zr * zr + zi * zi <= 4.0 {
                let t = if fma {
                    zr.mul_add(zr, -(zi * zi)) + cr
                } else {
                    zr * zr - zi * zi + cr
                };
                zi = if fma {
                    (2.0 * zr).mul_add(zi, ci)
                } else {
                    2.0 * zr * zi + ci
                };
                zr = t;
                n += 1;
            }
            out.extend_from_slice(&n.to_le_bytes());
        }
    }
    out
}

#[test]
fn demo_tiles_from_two_correct_f32_evaluations_are_accepted() {
    // The tiles of `nuee run demo:mandelbrot-gpu`: 1024x1024 over x in [-2, 0.5], from
    // y = -1.25, in 64-pixel tiles, 500 iterations.
    let cmp = comparator(TaskKind::Wgsl, &MANDELBROT_COMPARE).expect("tolerance comparator");
    let step = 2.5f64 / 1024.0;
    let tile = |task: u32, fma: bool| {
        let (tx, ty) = (task % 16, task / 16);
        let x0 = (-2.0 + f64::from(tx * 64) * step) as f32;
        let y0 = (-1.25 + f64::from(ty * 64) * step) as f32;
        f32_mandelbrot(x0, y0, step as f32, 64, 500, fma)
    };
    for task in 0..256 {
        assert!(
            cmp.same(&tile(task, false), &tile(task, true)),
            "tile {task}"
        );
    }
    // Still sharp enough to tell tiles apart: a neighbour's output is refused.
    assert!(!cmp.same(&tile(73, false), &tile(74, false)));
}
