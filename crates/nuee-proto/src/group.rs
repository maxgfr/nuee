//! Permanent groups: a roster signed by a group key that belongs to no machine.
//!
//! A `J` link names one coordinator, so a swarm ends with that machine. A group is named by its
//! own key instead, and its roster says which machines may coordinate. Every keeper is trusted
//! like an anchor: the roster tolerates machines that disappear, not keepers that lie.

use serde::{Deserialize, Serialize};

use crate::PROTO_VERSION;
use crate::bounded::{BoundedBytes, BoundedString, BoundedVec};
use crate::id::{DOMAIN_ROSTER, EndpointId, Signature, Signer, verify_domain};
use crate::limits::{MAX_PEERS, MAX_ROSTER};
use crate::msg::{ProtoError, decode_with_limit, encode_with_limit};

/// One machine of a group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    /// Identity of the machine.
    pub id: EndpointId,
    /// Name shown to the group.
    pub name: BoundedString<64>,
    /// Takes tasks.
    pub worker: bool,
    /// Keeps copies of the group's files.
    pub store: bool,
    /// May coordinate the group; a lower rank is preferred.
    pub keeper_rank: Option<u16>,
}

/// Who belongs to a group at one revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roster {
    /// Protocol version.
    pub version: u16,
    /// Key of the group.
    pub group: EndpointId,
    /// Name shown to the group.
    pub name: BoundedString<64>,
    /// Grows with every change, so that an old roster cannot bring a removed member back.
    pub revision: u64,
    /// The members.
    pub members: BoundedVec<Member, MAX_PEERS>,
}

/// Encoded roster with the signature of the group key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedRoster {
    /// `postcard(Roster)`.
    pub bytes: BoundedBytes<MAX_ROSTER>,
    /// Signature over `DOMAIN_ROSTER || bytes`.
    pub sig: Signature,
}

/// A roster is invalid.
#[derive(Debug, thiserror::Error)]
pub enum RosterError {
    /// Encoding or decoding failed.
    #[error(transparent)]
    Proto(#[from] ProtoError),
    /// The signature does not match the group key.
    #[error("bad roster signature")]
    BadSignature,
    /// The roster describes another group.
    #[error("roster of another group")]
    WrongGroup,
    /// Unknown protocol version.
    #[error("unsupported roster version {0}")]
    Version(u16),
    /// A consistency rule is violated.
    #[error("invalid roster: {0}")]
    Invalid(&'static str),
}

impl Roster {
    /// Checks the consistency rules: known version, unique members and ranks, one keeper at least.
    pub fn check(&self) -> Result<(), RosterError> {
        if self.version != PROTO_VERSION {
            return Err(RosterError::Version(self.version));
        }
        let mut ids: Vec<_> = self.members.iter().map(|m| *m.id.as_bytes()).collect();
        ids.sort_unstable();
        if ids.windows(2).any(|w| w[0] == w[1]) {
            return Err(RosterError::Invalid("a member is listed twice"));
        }
        let mut ranks: Vec<_> = self.members.iter().filter_map(|m| m.keeper_rank).collect();
        if ranks.is_empty() {
            return Err(RosterError::Invalid("no keeper"));
        }
        ranks.sort_unstable();
        if ranks.windows(2).any(|w| w[0] == w[1]) {
            return Err(RosterError::Invalid("two keepers share a rank"));
        }
        Ok(())
    }

    /// Encodes and signs the roster with the group key.
    pub fn sign(&self, group_key: &dyn Signer) -> Result<SignedRoster, RosterError> {
        if group_key.id() != self.group {
            return Err(RosterError::WrongGroup);
        }
        self.check()?;
        let raw = encode_with_limit(self, MAX_ROSTER)?;
        let sig = group_key.sign(DOMAIN_ROSTER, &raw);
        let bytes =
            BoundedBytes::try_from(raw).map_err(|_| RosterError::Invalid("roster too large"))?;
        Ok(SignedRoster { bytes, sig })
    }

    /// The member with this identity.
    pub fn member(&self, id: &EndpointId) -> Option<&Member> {
        self.members.iter().find(|m| m.id == *id)
    }

    /// The keepers, preferred first.
    pub fn keepers(&self) -> Vec<&Member> {
        let mut keepers: Vec<_> = self
            .members
            .iter()
            .filter(|m| m.keeper_rank.is_some())
            .collect();
        keepers.sort_by_key(|m| m.keeper_rank);
        keepers
    }

    /// Whether this roster may replace `current`: same group and a higher revision.
    pub fn replaces(&self, current: &Roster) -> bool {
        self.group == current.group && self.revision > current.revision
    }
}

impl SignedRoster {
    /// Verifies the signature of `group`, decodes and checks the roster.
    pub fn open(&self, group: &EndpointId) -> Result<Roster, RosterError> {
        if !verify_domain(group, DOMAIN_ROSTER, &self.bytes, &self.sig) {
            return Err(RosterError::BadSignature);
        }
        let roster: Roster = decode_with_limit(&self.bytes, MAX_ROSTER)?;
        if roster.group != *group {
            return Err(RosterError::WrongGroup);
        }
        roster.check()?;
        Ok(roster)
    }
}
