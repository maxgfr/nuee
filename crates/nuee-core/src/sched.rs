//! Scheduling helpers: capability routing and per-peer deadlines.

use std::collections::VecDeque;

use nuee_proto::{Caps, Manifest, TaskKind};

/// Number of recent durations kept per peer.
pub(crate) const DURATION_WINDOW: usize = 16;

/// Ring buffer of a peer's recent task durations, in milliseconds.
#[derive(Debug, Default, Clone)]
pub(crate) struct Durations(VecDeque<u64>);

impl Durations {
    pub(crate) fn push(&mut self, ms: u64) {
        if self.0.len() == DURATION_WINDOW {
            self.0.pop_front();
        }
        self.0.push_back(ms);
    }

    pub(crate) fn median(&self) -> Option<u64> {
        let mut v: Vec<u64> = self.0.iter().copied().collect();
        v.sort_unstable();
        let n = v.len();
        if n == 0 {
            return None;
        }
        let hi = *v.get(n / 2)?;
        if n % 2 == 1 {
            Some(hi)
        } else {
            let lo = *v.get(n / 2 - 1)?;
            Some(lo + (hi - lo) / 2)
        }
    }
}

/// Deadline bounds, in milliseconds.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DeadlineBounds {
    pub floor: u64,
    pub ceil: u64,
    pub exec_ceil: u64,
}

fn is_exec(kind: TaskKind) -> bool {
    matches!(kind, TaskKind::Exec | TaskKind::TrainExec)
}

/// Deadline of a lease for a peer: three times its median duration, clamped, or the
/// manifest's deadline while the peer has no history.
pub(crate) fn lease_deadline(history: &Durations, manifest: &Manifest, b: DeadlineBounds) -> u32 {
    let ceil = if is_exec(manifest.kind) {
        b.exec_ceil
    } else {
        b.ceil
    };
    let ms = match history.median() {
        None => u64::from(manifest.deadline_ms).min(ceil.max(b.floor)),
        Some(m) => m.saturating_mul(3).clamp(b.floor.min(ceil), ceil),
    };
    u32::try_from(ms).unwrap_or(u32::MAX)
}

/// Whether a peer with `caps` may run tasks of `manifest`.
pub(crate) fn routable(caps: &Caps, manifest: &Manifest) -> bool {
    let kind = manifest.kind;
    if !caps.supports(kind) {
        return false;
    }
    if kind == TaskKind::Wgsl && caps.gpu.is_none() {
        return false;
    }
    if let Some(exec) = &manifest.exec {
        if is_exec(kind) {
            let wanted = format!("{}@{}", exec.repo.as_str(), exec.rev.as_str());
            if !caps.exec_allow.iter().any(|a| a.as_str() == wanted) {
                return false;
            }
        }
        if exec.gpu && caps.gpu.is_none() {
            return false;
        }
    } else if is_exec(kind) {
        return false;
    }
    let offered = u64::from(caps.mem_mb).saturating_mul(1024 * 1024);
    manifest.max_memory <= offered
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuee_proto::{
        BoundedString, BoundedVec, Comparator, ExecSpec, GpuCaps, Hash, SecretKey, VerifyPolicy,
    };

    fn manifest(kind: TaskKind) -> Manifest {
        Manifest {
            version: nuee_proto::PROTO_VERSION,
            job: 0,
            name: BoundedString::new(),
            kind,
            module: Some(Hash::of(b"m")),
            module_size: 10,
            max_memory: 64 * 1024 * 1024,
            assets: BoundedVec::new(),
            tasks: 1,
            verify: VerifyPolicy::default(),
            compare: Comparator::Exact,
            deadline_ms: 60_000,
            exec: None,
            train: None,
            coordinator: SecretKey::from_bytes(&[1; 32]).public(),
        }
    }

    fn caps(kinds: &[TaskKind]) -> Caps {
        Caps {
            agent: BoundedString::new(),
            browser: false,
            cpu_slots: 2,
            gpu: None,
            mem_mb: 1024,
            kinds: kinds.iter().fold(0, |acc, k| acc | k.bit()),
            cached: BoundedVec::new(),
            exec_allow: BoundedVec::new(),
            store_quota_mb: 0,
            relay: None,
            rtc: false,
        }
    }

    const B: DeadlineBounds = DeadlineBounds {
        floor: 5_000,
        ceil: 600_000,
        exec_ceil: 86_400_000,
    };

    #[test]
    fn median_odd_even_and_window() {
        let mut d = Durations::default();
        assert_eq!(d.median(), None);
        for v in [30, 10, 20] {
            d.push(v);
        }
        assert_eq!(d.median(), Some(20));
        d.push(40);
        assert_eq!(d.median(), Some(25));
        for _ in 0..DURATION_WINDOW {
            d.push(1000);
        }
        assert_eq!(d.0.len(), DURATION_WINDOW);
        assert_eq!(d.median(), Some(1000));
    }

    #[test]
    fn deadline_uses_manifest_then_clamped_median() {
        let m = manifest(TaskKind::Wasm);
        let mut d = Durations::default();
        assert_eq!(lease_deadline(&d, &m, B), 60_000);
        d.push(100);
        assert_eq!(lease_deadline(&d, &m, B), 5_000);
        d.push(10_000);
        d.push(10_000);
        assert_eq!(lease_deadline(&d, &m, B), 30_000);
        for _ in 0..DURATION_WINDOW {
            d.push(1_000_000);
        }
        assert_eq!(lease_deadline(&d, &m, B), 600_000);
    }

    #[test]
    fn exec_deadline_uses_exec_ceiling() {
        let mut m = manifest(TaskKind::Exec);
        m.deadline_ms = 3_600_000;
        let mut d = Durations::default();
        assert_eq!(lease_deadline(&d, &m, B), 3_600_000);
        d.push(3_000_000);
        assert_eq!(lease_deadline(&d, &m, B), 9_000_000);
    }

    #[test]
    fn routing_by_kind_memory_gpu_and_exec_allow() {
        let wasm = manifest(TaskKind::Wasm);
        assert!(routable(&caps(&[TaskKind::Wasm]), &wasm));
        assert!(!routable(&caps(&[TaskKind::Wgsl]), &wasm));

        let mut small = caps(&[TaskKind::Wasm]);
        small.mem_mb = 32;
        assert!(!routable(&small, &wasm));

        let wgsl = manifest(TaskKind::Wgsl);
        let mut c = caps(&[TaskKind::Wgsl]);
        assert!(!routable(&c, &wgsl));
        c.gpu = Some(GpuCaps {
            adapter: BoundedString::new(),
            max_buffer: 1 << 30,
            max_storage_binding: 1 << 27,
            f16: false,
        });
        assert!(routable(&c, &wgsl));

        let embed = manifest(TaskKind::Embed);
        assert!(routable(&caps(&[TaskKind::Embed]), &embed));

        let mut exec = manifest(TaskKind::Exec);
        exec.exec = Some(ExecSpec {
            repo: BoundedString::try_from("https://example.org/r").unwrap_or_default(),
            rev: BoundedString::try_from("0123456789abcdef0123456789abcdef01234567")
                .unwrap_or_default(),
            argv: BoundedVec::new(),
            env: BoundedVec::new(),
            timeout_s: 60,
            gpu: false,
        });
        let mut c = caps(&[TaskKind::Exec]);
        assert!(!routable(&c, &exec));
        c.exec_allow = BoundedVec::try_from(vec![
            BoundedString::try_from(
                "https://example.org/r@0123456789abcdef0123456789abcdef01234567",
            )
            .unwrap_or_default(),
        ])
        .unwrap_or_default();
        assert!(routable(&c, &exec));
    }
}
