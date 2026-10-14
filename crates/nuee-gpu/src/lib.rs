//! GPU tasks for Nuée and the tolerance verifiers their outputs need.
//!
//! Without the `native` feature (wasm32 builds) only the verifiers and the WGSL input format
//! are compiled; the `native` feature adds [`wgsl::GpuRunner`] on wgpu.

use nuee_proto::FailKind;

pub mod embed;
pub mod llm;
pub mod wgsl;

/// A GPU task did not produce an output.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind:?}: {detail}")]
pub struct GpuError {
    /// Classification reported to the coordinator.
    pub kind: FailKind,
    /// Human-readable detail.
    pub detail: String,
}

impl GpuError {
    /// An error of `kind` with `detail`.
    pub fn new(kind: FailKind, detail: impl Into<String>) -> GpuError {
        GpuError {
            kind,
            detail: detail.into(),
        }
    }

    /// An `Invalid` error.
    pub fn invalid(detail: impl Into<String>) -> GpuError {
        GpuError::new(FailKind::Invalid, detail)
    }
}

/// Comparators for outputs that may differ slightly between devices.
pub mod verify {
    use std::sync::Arc;

    use nuee_core::Compare;
    use nuee_proto::{Comparator, TaskKind};

    /// How an output's 4-byte little-endian elements are read.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Elem {
        /// IEEE-754 single precision.
        F32,
        /// Unsigned 32-bit integers.
        U32,
    }

    /// Element-wise comparison with an absolute and a relative tolerance.
    ///
    /// Two elements agree when `|a - b| <= abs + rel * |b|`; a NaN only agrees with a NaN.
    /// Outputs of different lengths, or whose length is not a multiple of 4, never agree.
    /// Two outputs agree when at most `max_bad_permille` elements per thousand disagree.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct ElementTolerance {
        /// Element type.
        pub elem: Elem,
        /// Absolute tolerance.
        pub abs: f32,
        /// Relative tolerance.
        pub rel: f32,
        /// Disagreeing elements allowed, per thousand.
        pub max_bad_permille: u16,
    }

    impl ElementTolerance {
        fn value(&self, b: [u8; 4]) -> f64 {
            match self.elem {
                Elem::F32 => f64::from(f32::from_le_bytes(b)),
                Elem::U32 => f64::from(u32::from_le_bytes(b)),
            }
        }

        fn close(&self, a: f64, b: f64) -> bool {
            if a.is_nan() || b.is_nan() {
                return a.is_nan() && b.is_nan();
            }
            if a == b {
                return true;
            }
            (a - b).abs() <= f64::from(self.abs) + f64::from(self.rel) * b.abs()
        }

        /// Number of disagreeing elements, or `None` when the outputs cannot be compared.
        pub fn bad_elements(&self, a: &[u8], b: &[u8]) -> Option<usize> {
            if a.len() != b.len() || !a.len().is_multiple_of(4) {
                return None;
            }
            let (ea, _) = a.as_chunks::<4>();
            let (eb, _) = b.as_chunks::<4>();
            Some(
                ea.iter()
                    .zip(eb)
                    .filter(|(x, y)| !self.close(self.value(**x), self.value(**y)))
                    .count(),
            )
        }
    }

    impl Compare for ElementTolerance {
        fn same(&self, a: &[u8], b: &[u8]) -> bool {
            let Some(bad) = self.bad_elements(a, b) else {
                return false;
            };
            let n = (a.len() / 4) as u128;
            (bad as u128) * 1000 <= u128::from(self.max_bad_permille) * n
        }
    }

    /// The comparator a job of `kind` needs for `compare`, when this crate provides it.
    ///
    /// The manifest's `ElementTolerance` carries no element type: WGSL outputs are read as
    /// `u32` (integer results such as iteration counts), every other kind as `f32`.
    pub fn comparator(kind: TaskKind, compare: &Comparator) -> Option<Arc<dyn Compare>> {
        match *compare {
            Comparator::ElementTolerance {
                abs,
                rel,
                max_bad_permille,
            } => Some(Arc::new(ElementTolerance {
                elem: if kind == TaskKind::Wgsl {
                    Elem::U32
                } else {
                    Elem::F32
                },
                abs,
                rel,
                max_bad_permille,
            })),
            Comparator::Logits { max_gap } => Some(Arc::new(crate::llm::LogitsCompare { max_gap })),
            Comparator::Cosine { min } => Some(Arc::new(crate::embed::Cosine { min })),
            _ => None,
        }
    }
}

#[cfg(feature = "native")]
pub use wgsl::{GpuRunner, adapter_caps, probe_adapter};
