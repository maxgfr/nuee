//! Robust aggregation of worker deltas.
//!
//! 1. Deltas with a non-finite value or the wrong length are dropped.
//! 2. With three or more left: the coordinate-wise median `m` is computed, then every delta with
//!    `cos(Δ, m) < cos_min` or `‖Δ‖ > norm_max_ratio·‖m‖` is dropped (both checks are skipped
//!    when `m` is zero), and the survivors are combined with a coordinate-wise trimmed mean that
//!    drops `floor(trim·k)` values at each end.
//! 3. With one or two deltas left there is no meaningful majority: their plain mean is used.

use crate::{TrainError, norm};

/// Aggregation settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AggConfig {
    /// Fraction trimmed at each end of the coordinate-wise mean, in [0, 0.5).
    pub trim: f32,
    /// Smallest accepted cosine between a delta and the median.
    pub cos_min: f32,
    /// Largest accepted norm relative to the median's norm.
    pub norm_max_ratio: f32,
}

impl Default for AggConfig {
    fn default() -> Self {
        Self {
            trim: 0.2,
            cos_min: 0.0,
            norm_max_ratio: 3.0,
        }
    }
}

/// Why a delta was left out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reject {
    /// Wrong number of values.
    Length,
    /// NaN or infinity.
    NonFinite,
    /// Points away from the median.
    Cosine(f32),
    /// Too long compared with the median; the ratio.
    Norm(f32),
}

/// Result of an aggregation.
#[derive(Debug, Clone, PartialEq)]
pub struct Aggregate {
    /// The combined delta.
    pub delta: Vec<f32>,
    /// Indices of the deltas used.
    pub accepted: Vec<usize>,
    /// Indices left out, with the reason.
    pub rejected: Vec<(usize, Reject)>,
}

fn median(vals: &mut [f32]) -> f32 {
    vals.sort_unstable_by(f32::total_cmp);
    let n = vals.len();
    if n % 2 == 1 {
        vals[n / 2]
    } else {
        (vals[n / 2 - 1] + vals[n / 2]) / 2.0
    }
}

fn coordinatewise(deltas: &[&[f32]], dim: usize, mut f: impl FnMut(&mut [f32]) -> f32) -> Vec<f32> {
    let mut col = vec![0.0f32; deltas.len()];
    (0..dim)
        .map(|j| {
            for (c, d) in col.iter_mut().zip(deltas) {
                *c = d[j];
            }
            f(&mut col)
        })
        .collect()
}

fn mean(deltas: &[&[f32]], dim: usize) -> Vec<f32> {
    let n = deltas.len() as f64;
    (0..dim)
        .map(|j| (deltas.iter().map(|d| f64::from(d[j])).sum::<f64>() / n) as f32)
        .collect()
}

/// Aggregates the deltas, all expected to hold `dim` values. Fails with [`TrainError::NoDelta`]
/// when nothing survives.
pub fn aggregate(deltas: &[Vec<f32>], dim: usize, cfg: AggConfig) -> Result<Aggregate, TrainError> {
    let mut rejected = Vec::new();
    let mut ok: Vec<usize> = Vec::new();
    for (i, d) in deltas.iter().enumerate() {
        if d.len() != dim {
            rejected.push((i, Reject::Length));
        } else if !d.iter().all(|x| x.is_finite()) {
            rejected.push((i, Reject::NonFinite));
        } else {
            ok.push(i);
        }
    }
    if ok.is_empty() {
        return Err(TrainError::NoDelta);
    }
    if ok.len() <= 2 {
        let sel: Vec<&[f32]> = ok.iter().map(|&i| deltas[i].as_slice()).collect();
        return Ok(Aggregate {
            delta: mean(&sel, dim),
            accepted: ok,
            rejected,
        });
    }
    let sel: Vec<&[f32]> = ok.iter().map(|&i| deltas[i].as_slice()).collect();
    let m = coordinatewise(&sel, dim, median);
    let m_norm = norm(&m);
    let mut accepted = Vec::new();
    for &i in &ok {
        let d = &deltas[i];
        if m_norm > 0.0 {
            let d_norm = norm(d);
            let dot: f64 = d
                .iter()
                .zip(&m)
                .map(|(a, b)| f64::from(*a) * f64::from(*b))
                .sum();
            let cos = if d_norm > 0.0 {
                dot / (d_norm * m_norm)
            } else {
                0.0
            };
            if cos < f64::from(cfg.cos_min) {
                rejected.push((i, Reject::Cosine(cos as f32)));
                continue;
            }
            let ratio = d_norm / m_norm;
            if ratio > f64::from(cfg.norm_max_ratio) {
                rejected.push((i, Reject::Norm(ratio as f32)));
                continue;
            }
        }
        accepted.push(i);
    }
    rejected.sort_by_key(|r| r.0);
    if accepted.is_empty() {
        return Err(TrainError::NoDelta);
    }
    let sel: Vec<&[f32]> = accepted.iter().map(|&i| deltas[i].as_slice()).collect();
    let k = sel.len();
    let mut cut = (f64::from(cfg.trim.clamp(0.0, 0.5)) * k as f64).floor() as usize;
    if 2 * cut >= k {
        cut = (k - 1) / 2;
    }
    let delta = coordinatewise(&sel, dim, |col| {
        col.sort_unstable_by(f32::total_cmp);
        let kept = &col[cut..k - cut];
        (kept.iter().map(|x| f64::from(*x)).sum::<f64>() / kept.len() as f64) as f32
    });
    Ok(Aggregate {
        delta,
        accepted,
        rejected,
    })
}
