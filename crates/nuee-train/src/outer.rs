//! The DiLoCo outer optimiser: SGD with Nesterov momentum on the pseudo-gradient.
//!
//! With `g = θ_global − agg(θ_i) = −agg(Δ_i)`:
//! `v ← μ·v + g`, then `θ ← θ − lr·(μ·v + g)`.

/// Outer optimiser settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OuterConfig {
    /// Learning rate (DiLoCo default 0.7).
    pub lr: f32,
    /// Nesterov momentum (DiLoCo default 0.9).
    pub momentum: f32,
}

impl Default for OuterConfig {
    fn default() -> Self {
        Self {
            lr: 0.7,
            momentum: 0.9,
        }
    }
}

/// Outer optimiser state: the momentum buffer.
#[derive(Debug, Clone, PartialEq)]
pub struct Outer {
    /// Momentum `v`, one value per parameter.
    pub velocity: Vec<f32>,
}

impl Outer {
    /// Zero momentum for `n` parameters.
    pub fn new(n: usize) -> Outer {
        Outer {
            velocity: vec![0.0; n],
        }
    }

    /// Applies one outer step from the aggregated delta (`agg(θ_i) − θ`). Returns the norm of the
    /// change applied to `theta`. Slices must have the same length as the momentum.
    pub fn step(&mut self, cfg: OuterConfig, theta: &mut [f32], agg_delta: &[f32]) -> f64 {
        let mut sq = 0.0f64;
        for ((t, v), d) in theta.iter_mut().zip(&mut self.velocity).zip(agg_delta) {
            let g = -*d;
            *v = cfg.momentum * *v + g;
            let change = cfg.lr * (cfg.momentum * *v + g);
            *t -= change;
            sq += f64::from(change) * f64::from(change);
        }
        sq.sqrt()
    }
}
