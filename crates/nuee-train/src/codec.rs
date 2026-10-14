//! Worker → coordinator parameter deltas `Δ = θ_local − θ_global`.
//!
//! Wire format (little-endian):
//!
//! ```text
//! b"NDL1" | u8 codec (0 = F32, 1 = Int8Ef) | u32 n_tensors (≤ 4096)
//! per tensor: u32 len | f32 scale | data (len × f32 for F32, len × i8 for Int8Ef)
//! ```
//!
//! `n_tensors` and every `len` must match the run's [`Layout`]. For `F32` the scale is written as
//! 1.0 and ignored. For `Int8Ef` each tensor uses `scale = max|x| / 127` (0 ⇒ all zeros) and
//! `q = round(x / scale)` clamped to [-127, 127]; the quantisation error is fed back on the
//! worker: `x = Δ + e`, `e ← x − scale·q`, so what is lost in one round is sent in the next.
//! The scale is lowered to the largest value with `127·scale` finite (only matters for `max|x|`
//! near `f32::MAX`), and a decoder rejects an `Int8Ef` tensor whose `127·scale` is not finite.
//! A non-finite delta value is sent as 0 and not carried over: its residual entry is reset to 0.

use nuee_proto::DeltaCodec;

use crate::round::META_LOSS;
use crate::{
    Layout, MAX_PARAM_BYTES, MAX_TENSORS, Params, RESERVED_PREFIX, TrainError, read_safetensors,
};

/// Magic of an encoded delta.
pub const DELTA_MAGIC: &[u8; 4] = b"NDL1";

fn codec_byte(c: DeltaCodec) -> u8 {
    match c {
        DeltaCodec::F32 => 0,
        DeltaCodec::Int8Ef => 1,
    }
}

/// Encodes `delta` (laid out as `layout`). With `Int8Ef`, `residual` is the worker's error
/// feedback state: it is added before quantising and replaced by the new error (an empty
/// residual starts at zero). With `F32` the residual is left untouched.
pub fn encode_delta(
    codec: DeltaCodec,
    layout: &Layout,
    delta: &[f32],
    residual: &mut Vec<f32>,
) -> Result<Vec<u8>, TrainError> {
    let lens = layout.lens();
    if lens.len() > MAX_TENSORS {
        return Err(TrainError::TooLarge("more than 4096 tensors"));
    }
    if delta.len() != layout.total() {
        return Err(TrainError::Layout("delta length differs from the layout"));
    }
    let per = match codec {
        DeltaCodec::F32 => 4,
        DeltaCodec::Int8Ef => 1,
    };
    let mut out = Vec::with_capacity(9 + lens.len() * 8 + delta.len() * per);
    out.extend_from_slice(DELTA_MAGIC);
    out.push(codec_byte(codec));
    out.extend_from_slice(&(lens.len() as u32).to_le_bytes());
    if codec == DeltaCodec::Int8Ef {
        if residual.is_empty() {
            residual.resize(delta.len(), 0.0);
        }
        if residual.len() != delta.len() {
            return Err(TrainError::Layout(
                "residual length differs from the layout",
            ));
        }
    }
    let mut off = 0;
    for len in lens {
        let len32 = u32::try_from(len).map_err(|_| TrainError::TooLarge("tensor too large"))?;
        out.extend_from_slice(&len32.to_le_bytes());
        let d = &delta[off..off + len];
        match codec {
            DeltaCodec::F32 => {
                out.extend_from_slice(&1.0f32.to_le_bytes());
                for x in d {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
            DeltaCodec::Int8Ef => {
                let e = &mut residual[off..off + len];
                for (ei, di) in e.iter_mut().zip(d) {
                    *ei += *di;
                }
                let max = e
                    .iter()
                    .filter(|x| x.is_finite())
                    .fold(0.0f32, |m, x| m.max(x.abs()));
                let scale = int8_scale(max);
                out.extend_from_slice(&scale.to_le_bytes());
                for x in e.iter_mut() {
                    let q = quantise(*x, scale);
                    out.push(q.to_le_bytes()[0]);
                    *x -= f32::from(q) * scale;
                    if !x.is_finite() {
                        *x = 0.0;
                    }
                }
            }
        }
        off += len;
    }
    Ok(out)
}

/// The largest scale `<= max / 127` whose dequantised range `127 * scale` is finite. For `max`
/// near `f32::MAX`, `max / 127` rounds so that `127 * scale` overflows to infinity.
fn int8_scale(max: f32) -> f32 {
    let mut scale = max / 127.0;
    while !(127.0 * scale).is_finite() {
        scale = scale.next_down();
    }
    scale
}

fn quantise(x: f32, scale: f32) -> i8 {
    if scale == 0.0 || !x.is_finite() {
        return 0;
    }
    // Clamped to the i8 range first, so the cast is exact.
    (x / scale).round().clamp(-127.0, 127.0) as i8
}

struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], TrainError> {
        let (head, rest) = self
            .buf
            .split_at_checked(n)
            .ok_or(TrainError::Delta("truncated"))?;
        self.buf = rest;
        Ok(head)
    }

    fn u32(&mut self) -> Result<u32, TrainError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
}

/// Decodes a delta for `layout`. Every length is checked against the layout and the input size
/// before anything is allocated; trailing bytes are an error. Never panics.
pub fn decode_delta(bytes: &[u8], layout: &Layout) -> Result<Vec<f32>, TrainError> {
    if bytes.len() > MAX_PARAM_BYTES + 9 + 8 * MAX_TENSORS {
        return Err(TrainError::Delta("larger than 1 GiB"));
    }
    let mut r = Reader { buf: bytes };
    if r.take(4)? != DELTA_MAGIC {
        return Err(TrainError::Delta("does not start with NDL1"));
    }
    let per = match r.take(1)? {
        [0] => 4usize,
        [1] => 1,
        _ => return Err(TrainError::Delta("unknown codec")),
    };
    let n = r.u32()? as usize;
    if n > MAX_TENSORS {
        return Err(TrainError::Delta("more than 4096 tensors"));
    }
    if n != layout.shapes.len() {
        return Err(TrainError::Delta("tensor count differs from the layout"));
    }
    let lens = layout.lens();
    let total = layout.total();
    if total
        .checked_mul(per)
        .is_none_or(|need| need + 8 * n + 9 != bytes.len())
    {
        return Err(TrainError::Delta("size does not match the layout"));
    }
    let mut out = Vec::with_capacity(total);
    for want in lens {
        let len = r.u32()? as usize;
        if len != want {
            return Err(TrainError::Delta("tensor length differs from the layout"));
        }
        let s = r.take(4)?;
        let scale = f32::from_le_bytes([s[0], s[1], s[2], s[3]]);
        let data = r.take(len * per)?;
        if per == 4 {
            out.extend(
                data.as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| f32::from_le_bytes(*c)),
            );
        } else {
            if !scale.is_finite() || scale < 0.0 || !(127.0 * scale).is_finite() {
                return Err(TrainError::Delta("bad int8 scale"));
            }
            out.extend(
                data.iter()
                    .map(|b| f32::from(i8::from_le_bytes([*b])) * scale),
            );
        }
    }
    if !r.buf.is_empty() {
        return Err(TrainError::Delta("trailing bytes"));
    }
    Ok(out)
}

/// Splits a WASM trainer output into the encoded delta and the mean inner loss trailer.
pub fn split_output(output: &[u8]) -> Result<(&[u8], f32), TrainError> {
    let at = output
        .len()
        .checked_sub(4)
        .ok_or(TrainError::Task("output shorter than its loss trailer"))?;
    let (delta, loss) = output.split_at(at);
    Ok((
        delta,
        f32::from_le_bytes([loss[0], loss[1], loss[2], loss[3]]),
    ))
}

/// Appends the loss trailer to an encoded delta (what a trainer returns).
pub fn with_loss(mut delta: Vec<u8>, loss: f32) -> Vec<u8> {
    delta.extend_from_slice(&loss.to_le_bytes());
    delta
}

/// Worker-side encoder for exec trainers, which write new parameters as safetensors: it turns
/// them into the same output as a WASM trainer (`NDL1` delta `θ_out − θ_in`, then the `f32`
/// loss from the `nuee.loss` metadata, NaN when absent). It keeps one error-feedback residual,
/// reset whenever the job or the parameter layout changes.
#[derive(Debug, Default)]
pub struct ExecEncoder {
    key: Option<(u32, [u8; 32])>,
    residual: Vec<f32>,
}

impl ExecEncoder {
    /// A fresh encoder (zero residual).
    pub fn new() -> ExecEncoder {
        ExecEncoder::default()
    }

    /// Encodes the trainer `output` of job `job` against the parameters it was `given` (both
    /// safetensors), with `codec`. A malformed output or one with another layout is an error and
    /// leaves the residual untouched.
    pub fn encode(
        &mut self,
        job: u32,
        codec: DeltaCodec,
        given: &[u8],
        output: &[u8],
    ) -> Result<Vec<u8>, TrainError> {
        let given = Params::from_safetensors(given)?;
        let raw = read_safetensors(output)?;
        let loss = raw
            .metadata
            .get(META_LOSS)
            .and_then(|s| s.parse::<f32>().ok())
            .unwrap_or(f32::NAN);
        let new = Params::new(
            raw.tensors
                .into_iter()
                .filter(|(n, _, _)| !n.starts_with(RESERVED_PREFIX))
                .collect(),
        )?;
        let layout = given.layout_hash();
        if new.layout_hash() != layout {
            return Err(TrainError::Layout("trainer output has another layout"));
        }
        let delta: Vec<f32> = new
            .data
            .iter()
            .zip(&given.data)
            .map(|(a, b)| a - b)
            .collect();
        if self.key != Some((job, layout)) {
            self.key = Some((job, layout));
            self.residual.clear();
        }
        let enc = encode_delta(codec, &given.layout(), &delta, &mut self.residual)?;
        Ok(with_loss(enc, loss))
    }

    /// The current error-feedback residual (empty before the first int8 encoding).
    pub fn residual(&self) -> &[f32] {
        &self.residual
    }
}
