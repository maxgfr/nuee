//! Identities, content hashes, domain-separated signatures and admission proofs.

use core::fmt;
use core::str::FromStr;

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

pub use iroh_base::{EndpointId, PublicKey, SecretKey, Signature};

/// Domain of manifest signatures.
pub const DOMAIN_MANIFEST: &[u8] = b"nuee/manifest/0";
/// Domain of task result signatures.
pub const DOMAIN_RESULT: &[u8] = b"nuee/result/0";
/// Domain of replication order signatures.
pub const DOMAIN_PIN: &[u8] = b"nuee/pin/0";

/// Admission proof label on `nuee/job/0`.
pub const PROOF_HELLO: &[u8] = b"nuee/hello/0";
/// Admission proof label on `nuee/pin/0`.
pub const PROOF_PIN: &[u8] = b"nuee/pin-hello/0";
/// Admission proof label on `nuee/tcp/0`.
pub const PROOF_TCP: &[u8] = b"nuee/tcp-hello/0";
/// Admission proof label on `nuee/http/0`.
pub const PROOF_HTTP: &[u8] = b"nuee/http-hello/0";

/// BLAKE3 digest of some content.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    /// Hashes `bytes` with BLAKE3.
    pub fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }
    /// Borrows the digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&data_encoding::HEXLOWER.encode(&self.0))
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({self})")
    }
}

/// A string is not 64 hex characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("expected 64 hex characters")]
pub struct HashParseError;

impl FromStr for Hash {
    type Err = HashParseError;
    fn from_str(s: &str) -> Result<Self, HashParseError> {
        let bytes = data_encoding::HEXLOWER_PERMISSIVE
            .decode(s.as_bytes())
            .map_err(|_| HashParseError)?;
        let arr: [u8; 32] = bytes.try_into().map_err(|_| HashParseError)?;
        Ok(Self(arr))
    }
}

/// Something that signs on behalf of an endpoint identity.
pub trait Signer {
    /// Identity whose key produces the signatures.
    fn id(&self) -> EndpointId;
    /// Signs `domain || msg`.
    fn sign(&self, domain: &[u8], msg: &[u8]) -> Signature;
}

impl Signer for SecretKey {
    fn id(&self) -> EndpointId {
        self.public()
    }
    fn sign(&self, domain: &[u8], msg: &[u8]) -> Signature {
        let mut buf = Vec::with_capacity(domain.len() + msg.len());
        buf.extend_from_slice(domain);
        buf.extend_from_slice(msg);
        SecretKey::sign(self, &buf)
    }
}

/// Signs `domain || msg` with `signer`.
pub fn sign_domain(signer: &dyn Signer, domain: &[u8], msg: &[u8]) -> Signature {
    signer.sign(domain, msg)
}

/// Checks a signature over `domain || msg` made by `pk`.
pub fn verify_domain(pk: &PublicKey, domain: &[u8], msg: &[u8], sig: &Signature) -> bool {
    let mut buf = Vec::with_capacity(domain.len() + msg.len());
    buf.extend_from_slice(domain);
    buf.extend_from_slice(msg);
    pk.verify(&buf, sig).is_ok()
}

/// The system random source failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("random source unavailable: {0}")]
pub struct RandomError(pub getrandom04::Error);

/// Draws a fresh 16-byte secret from the system random source.
pub fn new_secret16() -> Result<[u8; 16], RandomError> {
    let mut out = [0u8; 16];
    getrandom04::fill(&mut out).map_err(RandomError)?;
    Ok(out)
}

fn proof_mac(label: &[u8], secret: &[u8], a: &EndpointId, b: &EndpointId) -> Hmac<Sha256> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret)
        .unwrap_or_else(|_| unreachable!("HMAC accepts keys of any length"));
    mac.update(label);
    mac.update(a.as_bytes());
    mac.update(b.as_bytes());
    mac
}

/// Admission proof `HMAC-SHA256(secret, label || a || b)`.
pub fn proof(label: &[u8], secret: &[u8], a: &EndpointId, b: &EndpointId) -> [u8; 32] {
    proof_mac(label, secret, a, b)
        .finalize()
        .into_bytes()
        .into()
}

/// Checks an admission proof in constant time.
pub fn check_proof(
    label: &[u8],
    secret: &[u8],
    a: &EndpointId,
    b: &EndpointId,
    candidate: &[u8; 32],
) -> bool {
    proof_mac(label, secret, a, b)
        .verify_slice(candidate)
        .is_ok()
}

/// Bytes signed by a worker for a result: `lease LE || output_hash`.
pub fn result_message(lease: u64, output_hash: &Hash) -> [u8; 40] {
    let mut out = [0u8; 40];
    out[..8].copy_from_slice(&lease.to_le_bytes());
    out[8..].copy_from_slice(&output_hash.0);
    out
}

/// Signs a task result for `lease`.
pub fn sign_result(signer: &dyn Signer, lease: u64, output_hash: &Hash) -> Signature {
    signer.sign(DOMAIN_RESULT, &result_message(lease, output_hash))
}

/// Checks a task result signature made by `worker`.
pub fn verify_result(worker: &PublicKey, lease: u64, output_hash: &Hash, sig: &Signature) -> bool {
    verify_domain(
        worker,
        DOMAIN_RESULT,
        &result_message(lease, output_hash),
        sig,
    )
}
