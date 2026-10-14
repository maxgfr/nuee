//! Wire contract of the Nuée swarm: message types, links, signed manifests, limits,
//! task module validation, coordinator fingerprints and job files.
#![forbid(unsafe_code)]

pub mod bounded;
pub mod envelope;
pub mod features;
pub mod fingerprint;
pub mod id;
pub mod job_toml;
pub mod limits;
pub mod link;
pub mod manifest;
pub mod msg;
pub mod validate;

pub use bounded::{BoundError, BoundedBytes, BoundedString, BoundedVec};
pub use envelope::{build_envelope, split_envelope};
pub use features::{ALLOWED_PROPOSALS, Proposal, WASM_FEATURES};
pub use fingerprint::{fingerprint, fingerprint_string};
pub use id::{
    DOMAIN_MANIFEST, DOMAIN_PIN, DOMAIN_RESULT, EndpointId, Hash, HashParseError, PROOF_HELLO,
    PROOF_HTTP, PROOF_PIN, PROOF_TCP, PublicKey, RandomError, SecretKey, Signature, Signer,
    check_proof, new_secret16, proof, result_message, sign_domain, sign_result, verify_domain,
    verify_result,
};
pub use job_toml::{JobError, JobKind, JobToml};
pub use limits::LocalLimits;
pub use link::{FileLink, JobLink, Link, LinkError, PinLink, ServiceLink};
pub use manifest::{
    Asset, Comparator, DeltaCodec, EnvVar, ExecSpec, Manifest, ManifestError, SignedManifest,
    TrainSpec, TrainerKind, VerifyMode, VerifyPolicy,
};
pub use msg::*;
pub use validate::{ModuleInfo, ValidateError, validate_module};

/// Protocol version carried in `Hello` messages and manifests.
pub const PROTO_VERSION: u16 = 0;
/// ALPN of the swarm control protocol.
pub const ALPN_JOB: &[u8] = b"nuee/job/0";
/// ALPN of replication orders.
pub const ALPN_PIN: &[u8] = b"nuee/pin/0";
/// ALPN of the exposed TCP tunnel.
pub const ALPN_TCP: &[u8] = b"nuee/tcp/0";
/// ALPN of console HTTP requests.
pub const ALPN_HTTP: &[u8] = b"nuee/http/0";
