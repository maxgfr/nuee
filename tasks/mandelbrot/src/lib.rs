//! Mandelbrot escape counts: a deterministic reference task.
//!
//! Input (36 bytes, little-endian): `x0: f64, y0: f64, step: f64, width: u32, height: u32,
//! max_iter: u32`. Output: `width * height` little-endian `u16` escape iteration counts, row-major,
//! `max_iter` for points that never escape. Invalid input panics, which traps the instance.

const INPUT_LEN: usize = 36;
const MAX_PIXELS: u64 = 1 << 20;
const MAX_ITER: u32 = 65535;

fn f64_at(input: &[u8], at: usize) -> f64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&input[at..at + 8]);
    f64::from_le_bytes(b)
}

fn u32_at(input: &[u8], at: usize) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&input[at..at + 4]);
    u32::from_le_bytes(b)
}

fn escape(cr: f64, ci: f64, max_iter: u32) -> u16 {
    let (mut zr, mut zi) = (0.0f64, 0.0f64);
    let mut n = 0;
    while n < max_iter {
        if zr * zr + zi * zi > 4.0 {
            break;
        }
        let t = zr * zr - zi * zi + cr;
        zi = 2.0 * zr * zi + ci;
        zr = t;
        n += 1;
    }
    n as u16
}

fn run(input: &[u8]) -> Vec<u8> {
    assert_eq!(input.len(), INPUT_LEN, "mandelbrot: input must be 36 bytes");
    let x0 = f64_at(input, 0);
    let y0 = f64_at(input, 8);
    let step = f64_at(input, 16);
    let width = u32_at(input, 24);
    let height = u32_at(input, 28);
    let max_iter = u32_at(input, 32);
    assert!(x0.is_finite() && y0.is_finite() && step.is_finite(), "mandelbrot: non-finite coordinate");
    assert!(width > 0 && height > 0, "mandelbrot: empty image");
    assert!(u64::from(width) * u64::from(height) <= MAX_PIXELS, "mandelbrot: image too large");
    assert!(max_iter <= MAX_ITER, "mandelbrot: max_iter above 65535");

    let mut out = Vec::with_capacity(width as usize * height as usize * 2);
    for row in 0..height {
        let ci = y0 + f64::from(row) * step;
        for col in 0..width {
            let cr = x0 + f64::from(col) * step;
            out.extend_from_slice(&escape(cr, ci, max_iter).to_le_bytes());
        }
    }
    out
}

nuee_task::export!(run);
