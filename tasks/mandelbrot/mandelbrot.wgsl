// Mandelbrot escape counts on the GPU: the WGSL twin of the CPU task (same parameters, same
// formula, f32 instead of f64). One invocation per pixel, 8x8 workgroups; the output holds
// `width * height` u32 iteration counts, row-major, `max_iter` for points that never escape.

struct Params {
    x0: f32,
    y0: f32,
    step: f32,
    width: u32,
    height: u32,
    max_iter: u32,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read_write> counts: array<u32>;

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let col = id.x;
    let row = id.y;
    if (col >= params.width || row >= params.height) {
        return;
    }
    let cr = params.x0 + f32(col) * params.step;
    let ci = params.y0 + f32(row) * params.step;
    var zr = 0.0;
    var zi = 0.0;
    var n = 0u;
    while (n < params.max_iter) {
        if (zr * zr + zi * zi > 4.0) {
            break;
        }
        let t = zr * zr - zi * zi + cr;
        zi = 2.0 * zr * zi + ci;
        zr = t;
        n = n + 1u;
    }
    counts[row * params.width + col] = n;
}
