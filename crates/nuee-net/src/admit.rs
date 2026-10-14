//! Admission gate shared by every ALPN handler: bounded handshakes, a hello deadline,
//! constant-time proof checks and per-peer throttling of wrong proofs.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use n0_future::time::Instant;
use nuee_proto::EndpointId;
use nuee_proto::limits::{HELLO_TIMEOUT_MS, MAX_PENDING_UNAUTH};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::framing::FrameError;

/// Most remotes tracked for throttling before old entries are dropped.
const MAX_TRACKED: usize = 4096;

/// Admission settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmitConfig {
    /// Time a new connection has to present a valid hello.
    pub hello_timeout: Duration,
    /// Most handshakes in progress at once; extra connections are closed at once.
    pub max_pending: usize,
    /// Wrong proofs within `failure_window` that trigger a refusal period.
    pub max_failures: usize,
    /// Window over which wrong proofs are counted.
    pub failure_window: Duration,
    /// How long a throttled remote is refused.
    pub refuse_for: Duration,
}

impl Default for AdmitConfig {
    fn default() -> Self {
        Self {
            hello_timeout: Duration::from_millis(HELLO_TIMEOUT_MS),
            max_pending: MAX_PENDING_UNAUTH,
            max_failures: 5,
            failure_window: Duration::from_secs(60),
            refuse_for: Duration::from_secs(60),
        }
    }
}

/// Why a connection was not admitted.
#[derive(Debug, thiserror::Error)]
pub enum AdmitError {
    /// Too many handshakes in progress.
    #[error("too many pending handshakes")]
    Busy,
    /// The remote sent too many wrong proofs recently.
    #[error("remote is temporarily refused")]
    Throttled,
    /// No hello within the deadline.
    #[error("no hello before the deadline")]
    Timeout,
    /// The proof does not match the swarm secret.
    #[error("wrong admission proof")]
    BadProof,
    /// The remote speaks another protocol version.
    #[error("unsupported protocol version {0}")]
    BadVersion(u16),
    /// The first message is not a hello.
    #[error("expected a hello message")]
    Unexpected,
    /// The hello could not be read.
    #[error(transparent)]
    Frame(#[from] FrameError),
}

#[derive(Debug, Default)]
struct Failures {
    recent: Vec<Instant>,
    refused_until: Option<Instant>,
}

/// The gate itself; cheap to share behind an `Arc`.
#[derive(Debug)]
pub struct Admission {
    cfg: AdmitConfig,
    pending: Arc<Semaphore>,
    failures: Mutex<HashMap<EndpointId, Failures>>,
}

/// A handshake slot; dropping it frees the slot.
#[derive(Debug)]
pub struct Pending {
    _permit: OwnedSemaphorePermit,
}

impl Admission {
    /// A gate with these settings.
    pub fn new(cfg: AdmitConfig) -> Self {
        Self {
            cfg,
            pending: Arc::new(Semaphore::new(cfg.max_pending)),
            failures: Mutex::new(HashMap::new()),
        }
    }

    /// Settings in use.
    pub fn config(&self) -> AdmitConfig {
        self.cfg
    }

    fn table(&self) -> MutexGuard<'_, HashMap<EndpointId, Failures>> {
        // The table holds plain data; a panic while holding the lock cannot break it.
        self.failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Takes a handshake slot for `peer`, or refuses when full or throttled.
    pub fn begin(&self, peer: &EndpointId) -> Result<Pending, AdmitError> {
        if self.is_throttled(peer) {
            return Err(AdmitError::Throttled);
        }
        let permit = self
            .pending
            .clone()
            .try_acquire_owned()
            .map_err(|_| AdmitError::Busy)?;
        Ok(Pending { _permit: permit })
    }

    /// Whether `peer` is in a refusal period.
    pub fn is_throttled(&self, peer: &EndpointId) -> bool {
        let now = Instant::now();
        let mut table = self.table();
        match table.get_mut(peer) {
            Some(f) => match f.refused_until {
                Some(until) if now < until => true,
                Some(_) => {
                    f.refused_until = None;
                    f.recent.clear();
                    false
                }
                None => false,
            },
            None => false,
        }
    }

    /// Runs `peer`'s hello `fut` under the hello deadline. A hello that does not arrive in
    /// time counts against `peer` like a wrong proof, so silent streams get throttled too.
    pub async fn within<T, F>(&self, peer: &EndpointId, fut: F) -> Result<T, AdmitError>
    where
        F: Future<Output = Result<T, AdmitError>>,
    {
        match n0_future::time::timeout(self.cfg.hello_timeout, fut).await {
            Ok(res) => res,
            Err(_) => {
                self.record_failure(peer);
                Err(AdmitError::Timeout)
            }
        }
    }

    /// Checks `proof` = `HMAC(secret, label || peer || own)` in constant time; a wrong
    /// proof counts against `peer`.
    pub fn verify(
        &self,
        label: &[u8],
        secret: &[u8],
        peer: &EndpointId,
        own: &EndpointId,
        proof: &[u8; 32],
    ) -> Result<(), AdmitError> {
        if nuee_proto::check_proof(label, secret, peer, own, proof) {
            return Ok(());
        }
        self.record_failure(peer);
        Err(AdmitError::BadProof)
    }

    /// Counts a failed attempt from `peer`.
    pub fn record_failure(&self, peer: &EndpointId) {
        let now = Instant::now();
        let window = self.cfg.failure_window;
        let mut table = self.table();
        if table.len() >= MAX_TRACKED && !table.contains_key(peer) {
            table.retain(|_, f| {
                f.refused_until.is_some_and(|u| now < u)
                    || f.recent.iter().any(|t| now.duration_since(*t) < window)
            });
            if table.len() >= MAX_TRACKED {
                return;
            }
        }
        let f = table.entry(*peer).or_default();
        f.recent.retain(|t| now.duration_since(*t) < window);
        f.recent.push(now);
        if f.recent.len() >= self.cfg.max_failures {
            f.refused_until = Some(now + self.cfg.refuse_for);
            f.recent.clear();
        }
    }
}

impl Default for Admission {
    fn default() -> Self {
        Self::new(AdmitConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuee_proto::{PROOF_HELLO, SecretKey};

    fn key(b: u8) -> EndpointId {
        SecretKey::from_bytes(&[b; 32]).public()
    }

    #[test]
    fn proof_and_throttle() {
        let gate = Admission::default();
        let (w, c) = (key(1), key(2));
        let secret = [7u8; 16];
        let good = nuee_proto::proof(PROOF_HELLO, &secret, &w, &c);
        assert!(gate.verify(PROOF_HELLO, &secret, &w, &c, &good).is_ok());
        for _ in 0..4 {
            assert!(gate.verify(PROOF_HELLO, &secret, &w, &c, &[0; 32]).is_err());
            assert!(gate.begin(&w).is_ok());
        }
        assert!(gate.verify(PROOF_HELLO, &secret, &w, &c, &[0; 32]).is_err());
        assert!(matches!(gate.begin(&w), Err(AdmitError::Throttled)));
        assert!(gate.begin(&key(3)).is_ok());
    }

    #[tokio::test]
    async fn silent_hellos_are_throttled() {
        let gate = Admission::new(AdmitConfig {
            hello_timeout: Duration::from_millis(5),
            ..AdmitConfig::default()
        });
        let (w, other) = (key(1), key(3));
        for _ in 0..5 {
            let pending = gate.begin(&w).expect("not throttled yet");
            let res: Result<(), _> = gate.within(&w, std::future::pending()).await;
            assert!(matches!(res, Err(AdmitError::Timeout)));
            drop(pending);
        }
        assert!(matches!(gate.begin(&w), Err(AdmitError::Throttled)));
        assert!(gate.begin(&other).is_ok());
    }

    #[test]
    fn pending_is_bounded() {
        let gate = Admission::new(AdmitConfig {
            max_pending: 2,
            ..AdmitConfig::default()
        });
        let a = gate.begin(&key(1)).unwrap();
        let _b = gate.begin(&key(2)).unwrap();
        assert!(matches!(gate.begin(&key(3)), Err(AdmitError::Busy)));
        drop(a);
        assert!(gate.begin(&key(3)).is_ok());
    }
}
