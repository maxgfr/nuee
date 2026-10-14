//! Job manifests, signed by the coordinator with domain separation.

use serde::{Deserialize, Serialize};

use crate::PROTO_VERSION;
use crate::bounded::{BoundedBytes, BoundedString, BoundedVec};
use crate::id::{DOMAIN_MANIFEST, EndpointId, Hash, Signature, Signer, verify_domain};
use crate::limits::{
    MAX_ASSET, MAX_ASSETS_PER_JOB, MAX_DEADLINE_MS, MAX_MANIFEST, MAX_MODULE, MAX_WASM_MEMORY,
};
use crate::msg::{ProtoError, TaskKind, decode_with_limit, encode_with_limit};

/// How results are checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VerifyMode {
    /// Every task runs twice on different peers.
    Full,
    /// A sample is recomputed by the coordinator, plus canaries.
    Sample,
    /// No verification.
    None,
}

/// Verification settings of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyPolicy {
    /// Mode.
    pub mode: VerifyMode,
    /// Recomputed share of results, per thousand.
    pub sample_permille: u16,
    /// Share of known-answer canary leases, per thousand.
    pub canary_permille: u16,
}

impl Default for VerifyPolicy {
    fn default() -> Self {
        Self {
            mode: VerifyMode::Sample,
            sample_permille: 100,
            canary_permille: 30,
        }
    }
}

/// How two outputs of the same task are compared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Comparator {
    /// Byte equality.
    Exact,
    /// Element-wise f32 tolerance.
    ElementTolerance {
        /// Absolute tolerance.
        abs: f32,
        /// Relative tolerance.
        rel: f32,
        /// Elements allowed outside tolerance, per thousand.
        max_bad_permille: u16,
    },
    /// Cosine similarity of f32 vectors.
    Cosine {
        /// Minimum similarity.
        min: f32,
    },
    /// Logit vectors, largest allowed gap.
    Logits {
        /// Largest allowed absolute gap.
        max_gap: f32,
    },
    /// JSON lines compared on one numeric field.
    Jsonl {
        /// Field name.
        field: BoundedString<64>,
        /// Tolerance on that field.
        tol: f32,
    },
    /// No comparison.
    None,
}

/// File attached to a job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    /// Content hash.
    pub hash: Hash,
    /// Size in bytes.
    pub size: u64,
    /// File name.
    pub name: BoundedString<128>,
}

/// One environment variable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvVar {
    /// Name.
    pub key: BoundedString<128>,
    /// Value.
    pub value: BoundedString<1024>,
}

/// Native command run from a pinned repository revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecSpec {
    /// Repository URL.
    pub repo: BoundedString<256>,
    /// Commit, 40 lowercase hex characters.
    pub rev: BoundedString<40>,
    /// Command line.
    pub argv: BoundedVec<BoundedString<1024>, 64>,
    /// Environment.
    pub env: BoundedVec<EnvVar, 32>,
    /// Time limit, in seconds.
    pub timeout_s: u32,
    /// Needs a GPU.
    pub gpu: bool,
}

/// Who runs local training steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TrainerKind {
    /// WASM trainer module.
    Wasm,
    /// Native trainer.
    Exec,
}

/// Encoding of parameter deltas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeltaCodec {
    /// int8 with error feedback.
    Int8Ef,
    /// Raw f32.
    F32,
}

/// DiLoCo training settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrainSpec {
    /// Outer rounds.
    pub rounds: u32,
    /// Local steps per round.
    pub inner_steps: u32,
    /// Workers per round.
    pub workers: u16,
    /// Outer learning rate.
    pub outer_lr: f32,
    /// Outer Nesterov momentum.
    pub outer_momentum: f32,
    /// Initial parameters.
    pub init: Hash,
    /// Trainer kind.
    pub trainer: TrainerKind,
    /// Delta encoding.
    pub codec: DeltaCodec,
    /// Trimmed-mean fraction.
    pub trim: f32,
    /// Minimum cosine between a delta and the aggregate.
    pub cos_min: f32,
    /// Largest delta norm relative to the median.
    pub norm_max_ratio: f32,
}

/// A job description, signed by its coordinator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// Protocol version.
    pub version: u16,
    /// Job number.
    pub job: u32,
    /// Display name.
    pub name: BoundedString<64>,
    /// Kind of tasks.
    pub kind: TaskKind,
    /// Module, for module-based kinds.
    pub module: Option<Hash>,
    /// Module size in bytes.
    pub module_size: u32,
    /// Largest linear memory granted, in bytes.
    pub max_memory: u64,
    /// Assets.
    pub assets: BoundedVec<Asset, MAX_ASSETS_PER_JOB>,
    /// Number of tasks.
    pub tasks: u32,
    /// Verification policy.
    pub verify: VerifyPolicy,
    /// Output comparator.
    pub compare: Comparator,
    /// Default deadline, in milliseconds.
    pub deadline_ms: u32,
    /// Native command, for exec jobs.
    pub exec: Option<ExecSpec>,
    /// Training settings, for training jobs.
    pub train: Option<TrainSpec>,
    /// Coordinator identity.
    pub coordinator: EndpointId,
}

/// Encoded manifest with the coordinator's signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedManifest {
    /// `postcard(Manifest)`.
    pub bytes: BoundedBytes<MAX_MANIFEST>,
    /// Signature over `DOMAIN_MANIFEST || bytes`.
    pub sig: Signature,
}

/// A manifest is invalid.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// Encoding or decoding failed.
    #[error(transparent)]
    Proto(#[from] ProtoError),
    /// The signature does not match.
    #[error("bad manifest signature")]
    BadSignature,
    /// Signed by someone else than the expected coordinator.
    #[error("manifest names another coordinator")]
    WrongCoordinator,
    /// Unknown protocol version.
    #[error("unsupported manifest version {0}")]
    Version(u16),
    /// A limit or consistency rule is violated.
    #[error("invalid manifest: {0}")]
    Invalid(&'static str),
}

/// Whether `s` is a full lowercase hex commit id.
pub fn is_commit_rev(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn finite(v: f32) -> bool {
    v.is_finite()
}

impl Manifest {
    /// Checks limits and consistency rules.
    pub fn check(&self) -> Result<(), ManifestError> {
        use ManifestError::Invalid;
        if self.version != PROTO_VERSION {
            return Err(ManifestError::Version(self.version));
        }
        if self.module_size as usize > MAX_MODULE {
            return Err(Invalid("module too large"));
        }
        if self.max_memory > MAX_WASM_MEMORY {
            return Err(Invalid("max_memory above 1 GiB"));
        }
        if self.assets.iter().any(|a| a.size > MAX_ASSET as u64) {
            return Err(Invalid("asset too large"));
        }
        if self.verify.sample_permille > 1000 || self.verify.canary_permille > 1000 {
            return Err(Invalid("permille above 1000"));
        }
        if self.deadline_ms == 0 || self.deadline_ms > MAX_DEADLINE_MS {
            return Err(Invalid("deadline out of range"));
        }
        let comparator_ok = match &self.compare {
            Comparator::Exact | Comparator::None => true,
            Comparator::ElementTolerance {
                abs,
                rel,
                max_bad_permille,
            } => {
                finite(*abs)
                    && finite(*rel)
                    && *abs >= 0.0
                    && *rel >= 0.0
                    && *max_bad_permille <= 1000
            }
            Comparator::Cosine { min } => finite(*min),
            Comparator::Logits { max_gap } => finite(*max_gap) && *max_gap >= 0.0,
            Comparator::Jsonl { tol, .. } => finite(*tol) && *tol >= 0.0,
        };
        if !comparator_ok {
            return Err(Invalid("comparator parameters out of range"));
        }
        match self.kind {
            TaskKind::Wasm | TaskKind::Wgsl | TaskKind::TrainWasm if self.module.is_none() => {
                return Err(Invalid("module missing"));
            }
            TaskKind::Exec | TaskKind::TrainExec if self.exec.is_none() => {
                return Err(Invalid("exec spec missing"));
            }
            TaskKind::TrainWasm | TaskKind::TrainExec if self.train.is_none() => {
                return Err(Invalid("train spec missing"));
            }
            _ => {}
        }
        if let Some(exec) = &self.exec {
            if !is_commit_rev(&exec.rev) {
                return Err(Invalid("exec rev is not a 40-hex commit"));
            }
            if u64::from(exec.timeout_s) * 1000 > u64::from(MAX_DEADLINE_MS) {
                return Err(Invalid("exec timeout above 24 h"));
            }
        }
        if let Some(t) = &self.train {
            let all_finite = [
                t.outer_lr,
                t.outer_momentum,
                t.trim,
                t.cos_min,
                t.norm_max_ratio,
            ]
            .into_iter()
            .all(finite);
            if !all_finite || !(0.0..0.5).contains(&t.trim) {
                return Err(Invalid("train parameters out of range"));
            }
        }
        Ok(())
    }

    /// Encodes and signs the manifest.
    pub fn sign(&self, signer: &dyn Signer) -> Result<SignedManifest, ManifestError> {
        let raw = encode_with_limit(self, MAX_MANIFEST)?;
        let sig = signer.sign(DOMAIN_MANIFEST, &raw);
        let bytes = BoundedBytes::try_from(raw)
            .map_err(|_| ManifestError::Invalid("manifest too large"))?;
        Ok(SignedManifest { bytes, sig })
    }
}

impl SignedManifest {
    /// Verifies the signature of `coordinator`, decodes and checks every limit.
    pub fn open(&self, coordinator: &EndpointId) -> Result<Manifest, ManifestError> {
        if !verify_domain(coordinator, DOMAIN_MANIFEST, &self.bytes, &self.sig) {
            return Err(ManifestError::BadSignature);
        }
        let manifest: Manifest = decode_with_limit(&self.bytes, MAX_MANIFEST)?;
        if manifest.coordinator != *coordinator {
            return Err(ManifestError::WrongCoordinator);
        }
        manifest.check()?;
        Ok(manifest)
    }
}
