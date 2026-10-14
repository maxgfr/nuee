//! Shareable links: `<base>#/<K>/<PAYLOAD>` where PAYLOAD is uppercase base32 (no padding) of
//! `version || postcard(body) || first 4 bytes of blake3(version || body)`.

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::bounded::{BoundedString, BoundedVec};
use crate::id::{EndpointId, Hash};
use crate::limits::MAX_LINK_PROVIDERS;
use crate::msg::{Provider, decode_with_limit, encode_with_limit};

/// Base URL used when none is configured.
pub const DEFAULT_BASE: &str = "https://maxgfr.github.io/nuee/";
/// Current link format version.
pub const LINK_VERSION: u8 = 1;
const CHECKSUM_LEN: usize = 4;
/// Largest decoded link body.
pub const MAX_LINK_BODY: usize = 4096;

/// Invite to a compute swarm (`J`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobLink {
    /// Coordinator identity.
    pub coordinator: EndpointId,
    /// Coordinator relay URL.
    pub relay: BoundedString<256>,
    /// Admission secret.
    pub secret: [u8; 16],
}

/// Shared file or site (`F`, `S`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileLink {
    /// Peers serving the ciphertext.
    pub providers: BoundedVec<Provider, MAX_LINK_PROVIDERS>,
    /// Ciphertext hash.
    pub hash: Hash,
    /// Ciphertext size.
    pub size: u64,
    /// Access token presented to providers.
    pub access: [u8; 16],
    /// age X25519 identity that decrypts the content.
    pub key: [u8; 32],
}

/// File link without the decryption key, given to pinners (`P`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinLink {
    /// Peers serving the ciphertext.
    pub providers: BoundedVec<Provider, MAX_LINK_PROVIDERS>,
    /// Ciphertext hash.
    pub hash: Hash,
    /// Ciphertext size.
    pub size: u64,
    /// Access token presented to providers.
    pub access: [u8; 16],
}

/// Exposed service (`X`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceLink {
    /// Host identity.
    pub host: EndpointId,
    /// Host relay URL.
    pub relay: BoundedString<256>,
    /// Admission secret.
    pub secret: [u8; 16],
}

/// Any link a user can paste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// Compute swarm.
    Job(JobLink),
    /// Shared file.
    File(FileLink),
    /// Exposed service.
    Service(ServiceLink),
    /// Static site.
    Site(FileLink),
}

/// A link cannot be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    /// Incomplete: too short, bad base32 or checksum mismatch.
    #[error("the link looks truncated; copy it again in full")]
    Truncated,
    /// Made by a newer or unknown version.
    #[error("link version {0} is not supported; update nuee")]
    UnsupportedVersion(u8),
    /// Checksum is fine but the content is invalid.
    #[error("the link is corrupt")]
    Corrupt,
    /// Valid link of another kind than expected.
    #[error("this is a different kind of link")]
    WrongKind,
}

impl FileLink {
    /// Same link without the decryption key.
    pub fn pin_link(&self) -> PinLink {
        PinLink {
            providers: self.providers.clone(),
            hash: self.hash,
            size: self.size,
            access: self.access,
        }
    }
}

fn encode_payload<T: Serialize>(body: &T) -> String {
    let mut bytes = vec![LINK_VERSION];
    match encode_with_limit(body, MAX_LINK_BODY) {
        Ok(b) => bytes.extend_from_slice(&b),
        Err(e) => unreachable!("link bodies are bounded well below the cap: {e}"),
    }
    let sum = blake3::hash(&bytes);
    bytes.extend_from_slice(&sum.as_bytes()[..CHECKSUM_LEN]);
    data_encoding::BASE32_NOPAD.encode(&bytes)
}

fn join(base: &str, kind: char, payload: &str) -> String {
    let base = base.split('#').next().unwrap_or(base);
    format!("{base}#/{kind}/{payload}")
}

/// Splits user input into its kind letter and payload.
fn split(input: &str) -> Result<(char, &str), LinkError> {
    let s = input.trim();
    let s = match s.rfind('#') {
        Some(i) => &s[i + 1..],
        None => s,
    };
    let s = s.trim_start_matches('/');
    let mut chars = s.chars();
    let kind = chars.next().ok_or(LinkError::Truncated)?;
    let rest = chars.as_str();
    if rest.is_empty() {
        return Err(LinkError::Truncated);
    }
    let payload = rest.strip_prefix('/').ok_or(LinkError::Corrupt)?;
    Ok((kind.to_ascii_uppercase(), payload.trim()))
}

fn decode_payload<T: DeserializeOwned>(payload: &str) -> Result<T, LinkError> {
    let upper = payload.to_ascii_uppercase();
    let bytes = data_encoding::BASE32_NOPAD
        .decode(upper.as_bytes())
        .map_err(|_| LinkError::Truncated)?;
    let (&version, _) = bytes.split_first().ok_or(LinkError::Truncated)?;
    if version != LINK_VERSION {
        return Err(LinkError::UnsupportedVersion(version));
    }
    if bytes.len() < 1 + CHECKSUM_LEN {
        return Err(LinkError::Truncated);
    }
    let (data, sum) = bytes.split_at(bytes.len() - CHECKSUM_LEN);
    if blake3::hash(data).as_bytes()[..CHECKSUM_LEN] != *sum {
        return Err(LinkError::Truncated);
    }
    decode_with_limit(&data[1..], MAX_LINK_BODY).map_err(|_| LinkError::Corrupt)
}

impl Link {
    /// Kind letter used in the URL.
    pub fn kind_letter(&self) -> char {
        match self {
            Link::Job(_) => 'J',
            Link::File(_) => 'F',
            Link::Service(_) => 'X',
            Link::Site(_) => 'S',
        }
    }

    /// Renders the link under `base` (for example [`DEFAULT_BASE`]).
    pub fn to_url(&self, base: &str) -> String {
        let payload = match self {
            Link::Job(l) => encode_payload(l),
            Link::File(l) | Link::Site(l) => encode_payload(l),
            Link::Service(l) => encode_payload(l),
        };
        join(base, self.kind_letter(), &payload)
    }

    /// Parses a full URL, a fragment or `K/PAYLOAD`.
    pub fn parse(input: &str) -> Result<Link, LinkError> {
        let (kind, payload) = split(input)?;
        match kind {
            'J' => decode_payload(payload).map(Link::Job),
            'F' => decode_payload(payload).map(Link::File),
            'X' => decode_payload(payload).map(Link::Service),
            'S' => decode_payload(payload).map(Link::Site),
            'P' => Err(LinkError::WrongKind),
            _ => Err(LinkError::Corrupt),
        }
    }
}

impl PinLink {
    /// Renders the pin link under `base`.
    pub fn to_url(&self, base: &str) -> String {
        join(base, 'P', &encode_payload(self))
    }

    /// Parses a pin link; other link kinds give [`LinkError::WrongKind`].
    pub fn parse(input: &str) -> Result<PinLink, LinkError> {
        let (kind, payload) = split(input)?;
        match kind {
            'P' => decode_payload(payload),
            'J' | 'F' | 'X' | 'S' => Err(LinkError::WrongKind),
            _ => Err(LinkError::Corrupt),
        }
    }
}
