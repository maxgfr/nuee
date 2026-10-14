//! Durable storage: the swarm coordinator keeps a target number of copies of shared
//! ciphertexts on the store members of its swarm.
//!
//! - [`DurabilityManager`] is the pure decision logic: it knows the store members (from their
//!   `Caps::store_quota_mb`), the tracked blobs and, per blob, which member holds a copy. Its
//!   [`DurabilityManager::plan`] turns the state and the clock into [`Order`]s; the outcome of
//!   each order comes back through [`DurabilityManager::apply`].
//! - [`DurabilityService`] (native) runs the manager on the coordinator: it executes the
//!   orders over `nuee/pin/0` and storage challenges, persists the state to `durable.json`,
//!   and answers sharers that ask for replicas.
//! - [`StoreMember`] (native) is the member side: it accepts replication orders from the
//!   coordinator within its quota, fetches the ciphertext (never the key) and serves it to
//!   holders of the file link.
//! - [`request_replicas`] (native) is what `nuee share --replicas N --on <link>` sends.
//!
//! On `nuee/pin/0` the client opens one bidirectional stream and sends
//! `Hello{version, proof}` with `proof = HMAC(swarm secret, PROOF_PIN, client, server)`.
//! A sharer then sends `Replicate` once per wanted copy of the same blob (the count of
//! `Replicate` frames for one hash in a session is the replica count; the message has no
//! count field) and gets one `Accepted`/`Refused` per frame. The coordinator sends a member
//! one `Replicate` per copy, answered `Accepted` (or `Refused`) then `Done` once stored;
//! `Release` and `List` are answered with `Pins`.
//!
//! Ownership: the coordinator records which peer first asked for a blob or an erasure set.
//! Only that peer may `Release` it over `nuee/pin/0` (anyone else gets `Refused{NotMember}`)
//! and `List` answers each peer with its own entries only; the coordinator's operator keeps
//! full control locally ([`DurabilityService::forget`]). Each requester has a budget of
//! tracked blobs ([`MAX_TRACKED_PER_OWNER`]) and of bytes of copies ([`MAX_BYTES_PER_OWNER`]);
//! a blob that never got a first copy is dropped after [`UNPLACED_EXPIRY_MS`], and a non-zero
//! `ttl_s` is enforced from the moment the coordinator first planned the blob.
//!
//! Erasure sets ([`crate::erasure`]): instead of full copies, a sharer's ciphertext can be
//! kept as `k + m` shards, one copy of each, every shard on a different member
//! ([`DurabilityManager::request_erasure`]). A shard whose every copy is lost after it was
//! placed is not fetched again from the sharer: [`DurabilityManager::plan_repairs`] asks for
//! it to be rebuilt from any `k` surviving shards, and once rebuilt (reported with
//! [`DurabilityManager::rebuilt`]) it is placed again like any copy.

use nuee_proto::limits::MAX_LINK_PROVIDERS;
use nuee_proto::{BoundedVec, EndpointId, Hash, PinLink, PinRefusal, Provider};
use serde::{Deserialize, Serialize};

use crate::erasure::Header;

/// Most copies of one blob a sharer can ask for.
pub const MAX_REPLICAS: u8 = 8;
/// Most blobs one coordinator tracks.
pub const MAX_TRACKED: usize = 1024;
/// Copies kept of an erasure set's header blob (served with the set's access secret).
pub const HEADER_COPIES: u8 = 2;
/// Most store members one coordinator tracks.
pub const MAX_MEMBERS: usize = 256;
/// Most tracked blobs (shards and set headers included) one requester may have.
pub const MAX_TRACKED_PER_OWNER: usize = 128;
/// Most bytes of copies (size times wanted copies) one requester may have tracked.
pub const MAX_BYTES_PER_OWNER: u64 = 64 * 1024 * 1024 * 1024;
/// A blob that never got a first copy is dropped this long after it was first planned.
pub const UNPLACED_EXPIRY_MS: u64 = 24 * 60 * 60 * 1000;
/// Free space counted for ranking placements is capped at this: a member announcing a huge
/// quota does not win every placement (quotas are self-declared and never verified).
pub const RANK_QUOTA_CAP: u64 = 64 * 1024 * 1024 * 1024;

/// Timing of the durability manager (milliseconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurableConfig {
    /// A holder not seen for this long is considered lost and its copy is replaced.
    pub absent_after_ms: u64,
    /// Each holder is challenged this often.
    pub challenge_every_ms: u64,
    /// Each holding member is contacted (`List`) this often to see it is still there.
    pub probe_every_ms: u64,
    /// A member that refused or failed a blob is not asked again for it before this delay.
    pub retry_after_ms: u64,
}

impl Default for DurableConfig {
    fn default() -> Self {
        DurableConfig {
            absent_after_ms: 10 * 60 * 1000,
            challenge_every_ms: 60 * 60 * 1000,
            probe_every_ms: 60 * 1000,
            retry_after_ms: 10 * 60 * 1000,
        }
    }
}

/// Where a copy stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HolderState {
    /// Ordered, not confirmed yet.
    Pending,
    /// Stored, seen and challenged recently enough.
    Holding,
    /// Absent too long or failed a challenge; replaced.
    Lost,
}

/// One copy of a blob on one member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    /// Member holding the copy.
    pub member: EndpointId,
    /// State of the copy.
    pub state: HolderState,
    /// When the copy was ordered.
    pub since_ms: u64,
    /// Last successful contact.
    pub last_seen_ms: u64,
    /// Last passed challenge (or completion of the copy).
    pub last_challenge_ms: u64,
    /// An order about this copy is in flight.
    #[serde(skip)]
    pub busy: bool,
}

/// A blob kept durable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tracked {
    /// Ciphertext hash.
    pub hash: Hash,
    /// Ciphertext size.
    pub size: u64,
    /// Access secret of the file link (lets members fetch and serve the ciphertext).
    pub access: [u8; 16],
    /// Providers given by the sharer.
    pub providers: Vec<Provider>,
    /// Copies wanted; 0 once the sharer released it.
    pub target: u8,
    /// Copies ordered, held or lost.
    pub holders: Vec<Holder>,
    /// The erasure set this blob is a shard of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shard_of: Option<ShardOf>,
    /// A copy was confirmed at least once.
    #[serde(default)]
    pub placed: bool,
    /// The peer that asked for it first; `None` when the coordinator's operator did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<EndpointId>,
    /// Seconds to keep it from `since_ms` (0: until released).
    #[serde(default)]
    pub ttl_s: u32,
    /// When the manager first planned it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<u64>,
    #[serde(skip)]
    rebuilding: bool,
    #[serde(skip)]
    rebuild_after_ms: u64,
}

/// Position of a shard blob in its erasure set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardOf {
    /// Set identity (hash of the header).
    pub set: Hash,
    /// Shard index (data shards first).
    pub index: u16,
}

/// An erasure set kept durable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackedSet {
    /// Hash of the header bytes.
    pub id: Hash,
    /// The set description.
    pub header: Header,
    /// Access secret of the set; the header blob and every shard are served with it.
    pub access: [u8; 16],
    /// The peer that asked for the set; `None` when the coordinator's operator did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<EndpointId>,
}

/// Rebuild a lost shard from surviving ones, then report with [`DurabilityManager::rebuilt`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rebuild {
    /// Set identity.
    pub set: Hash,
    /// The set description.
    pub header: Header,
    /// Index of the shard to rebuild.
    pub index: u16,
    /// Hash of the shard to rebuild.
    pub hash: Hash,
    /// Access secret of the set; every shard is served with it.
    pub access: [u8; 16],
    /// Surviving shards: index, hash and a member holding it. At least `k` distinct indexes.
    pub sources: Vec<(u16, Hash, EndpointId)>,
}

/// Whether `member` keeps a copy (ordered or held) of any blob.
fn holds_any(blobs: &[Tracked], member: &EndpointId) -> bool {
    blobs.iter().any(|b| {
        b.holders
            .iter()
            .any(|h| h.member == *member && h.state != HolderState::Lost)
    })
}

/// Rendezvous rank of `member` for `blob`.
fn draw(blob: &Hash, member: &EndpointId) -> Hash {
    let mut bytes = [0u8; 64];
    bytes[..32].copy_from_slice(&blob.0);
    bytes[32..].copy_from_slice(member.as_bytes());
    Hash::of(&bytes)
}

/// Adds the providers not known yet, keeping the most recent [`MAX_LINK_PROVIDERS`].
fn add_providers(kept: &mut Vec<Provider>, new: Vec<Provider>) {
    for p in new {
        kept.retain(|q| q.id != p.id);
        kept.push(p);
    }
    let extra = kept.len().saturating_sub(MAX_LINK_PROVIDERS);
    kept.drain(..extra);
}

impl Tracked {
    fn live(&self) -> usize {
        self.holders
            .iter()
            .filter(|h| h.state != HolderState::Lost)
            .count()
    }

    /// Members holding a confirmed copy.
    pub fn holding(&self) -> Vec<EndpointId> {
        self.holders
            .iter()
            .filter(|h| h.state == HolderState::Holding)
            .map(|h| h.member)
            .collect()
    }

    fn holder_mut(&mut self, member: &EndpointId) -> Option<&mut Holder> {
        self.holders.iter_mut().find(|h| h.member == *member)
    }
}

/// A store member of the swarm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    /// Member identity.
    pub id: EndpointId,
    /// Storage it offers, in bytes.
    pub quota_bytes: u64,
    /// Relay it announced.
    pub relay: Option<String>,
    /// Currently connected to the swarm (not persisted: members rejoin after a restart).
    #[serde(skip)]
    pub present: bool,
    /// Blobs it refused or failed, and until when it is not asked again for them.
    #[serde(skip)]
    refusals: Vec<(Hash, u64)>,
    #[serde(skip)]
    probing: bool,
    #[serde(skip)]
    last_probe_ms: u64,
}

/// Something the I/O loop must do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Order {
    /// Ask `member` to store a copy, fetched from `providers`.
    Replicate {
        /// Member asked.
        member: EndpointId,
        /// Ciphertext hash.
        hash: Hash,
        /// Ciphertext size.
        size: u64,
        /// Where to fetch it (remaining holders first, then the sharer).
        providers: Vec<Provider>,
        /// Access secret of the file link.
        access: [u8; 16],
    },
    /// Ask `member` to drop its copy.
    Release {
        /// Member asked.
        member: EndpointId,
        /// Ciphertext hash.
        hash: Hash,
    },
    /// Challenge the copy of `member`.
    Challenge {
        /// Member challenged.
        member: EndpointId,
        /// Ciphertext hash.
        hash: Hash,
        /// Ciphertext size.
        size: u64,
    },
    /// Check that `member` is still there and list what it holds.
    Probe {
        /// Member contacted.
        member: EndpointId,
    },
}

/// Outcome of a replication order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replicated {
    /// The copy is stored.
    Done,
    /// The member refused.
    Refused(PinRefusal),
    /// The member could not be reached or could not fetch the data.
    Failed,
}

/// Outcome of a challenge order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Challenged {
    /// Every sampled range verified.
    Passed,
    /// The member answered but could not prove possession.
    Failed,
    /// The member could not be reached.
    Unreachable,
}

/// The outcome of an [`Order`], fed back with [`DurabilityManager::apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// A replication order finished.
    Replicate {
        /// Member asked.
        member: EndpointId,
        /// Ciphertext hash.
        hash: Hash,
        /// Outcome.
        outcome: Replicated,
    },
    /// A release order finished.
    Release {
        /// Member asked.
        member: EndpointId,
        /// Ciphertext hash.
        hash: Hash,
        /// The member confirmed.
        ok: bool,
    },
    /// A challenge finished.
    Challenge {
        /// Member challenged.
        member: EndpointId,
        /// Ciphertext hash.
        hash: Hash,
        /// Outcome.
        outcome: Challenged,
    },
    /// A probe finished: the complete blobs the member lists, `None` if unreachable.
    Probe {
        /// Member contacted.
        member: EndpointId,
        /// Complete blobs it holds.
        complete: Option<Vec<Hash>>,
    },
}

/// What is persisted to `durable.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableState {
    /// Tracked blobs.
    pub blobs: Vec<Tracked>,
    /// Known store members.
    pub members: Vec<Member>,
    /// Erasure sets.
    #[serde(default)]
    pub sets: Vec<TrackedSet>,
}

/// The coordinator's replica bookkeeping. Pure: no I/O, the clock is passed in.
#[derive(Debug, Clone, Default)]
pub struct DurabilityManager {
    cfg: DurableConfig,
    state: DurableState,
}

impl DurabilityManager {
    /// An empty manager.
    pub fn new(cfg: DurableConfig) -> DurabilityManager {
        DurabilityManager {
            cfg,
            state: DurableState::default(),
        }
    }

    /// Resumes from a persisted state at `now`: copies still pending are forgotten (they are
    /// ordered again, which a member holding them already answers at once), every other copy
    /// gets a fresh absence delay, and members count as absent until they rejoin.
    pub fn restore(cfg: DurableConfig, mut state: DurableState, now: u64) -> DurabilityManager {
        for blob in &mut state.blobs {
            blob.holders.retain(|h| h.state != HolderState::Pending);
            blob.rebuilding = false;
            blob.rebuild_after_ms = 0;
            for h in &mut blob.holders {
                h.busy = false;
                if h.state == HolderState::Holding {
                    h.last_seen_ms = now;
                }
            }
        }
        // Members that hold nothing come back when they rejoin; keeping them would let
        // identities that came and went fill the member slots for good.
        let blobs = &state.blobs;
        state.members.retain(|m| holds_any(blobs, &m.id));
        for m in &mut state.members {
            m.present = false;
            m.refusals.clear();
            m.probing = false;
            m.last_probe_ms = now;
        }
        DurabilityManager { cfg, state }
    }

    /// The state to persist.
    pub fn state(&self) -> &DurableState {
        &self.state
    }

    /// Timing in use.
    pub fn config(&self) -> DurableConfig {
        self.cfg
    }

    /// The tracked blob `hash`.
    pub fn blob(&self, hash: &Hash) -> Option<&Tracked> {
        self.state.blobs.iter().find(|b| b.hash == *hash)
    }

    /// Members holding a confirmed copy of `hash`, as link providers (at most
    /// `MAX_LINK_PROVIDERS`). For an erasure set, the header holders come first, then the
    /// holders of its shards. Empty for an unknown hash.
    pub fn holders(&self, hash: &Hash) -> Vec<Provider> {
        let max = nuee_proto::limits::MAX_LINK_PROVIDERS;
        let header: Vec<EndpointId> = self.blob(hash).map(Tracked::holding).unwrap_or_default();
        let Some(set) = self.state.sets.iter().find(|s| s.id == *hash) else {
            return header
                .into_iter()
                .take(max)
                .map(|id| self.provider_of(id))
                .collect();
        };
        // One header holder first (one that also keeps a shard when possible), then one
        // holder per shard not covered yet, so a link of four providers reaches as many
        // distinct shards as it can; remaining room goes to other header holders.
        let shard_holders: Vec<Vec<EndpointId>> = set
            .header
            .shards
            .iter()
            .map(|h| self.blob(h).map(Tracked::holding).unwrap_or_default())
            .collect();
        let keeps_shard = |id: &EndpointId| shard_holders.iter().any(|hs| hs.contains(id));
        let mut ids: Vec<EndpointId> = Vec::new();
        if let Some(first) = header
            .iter()
            .find(|id| keeps_shard(id))
            .or_else(|| header.first())
        {
            ids.push(*first);
        }
        for hs in &shard_holders {
            if ids.len() >= max {
                break;
            }
            let covered = hs.iter().any(|id| ids.contains(id));
            if !covered && let Some(id) = hs.first() {
                ids.push(*id);
            }
        }
        for id in header {
            if ids.len() < max && !ids.contains(&id) {
                ids.push(id);
            }
        }
        ids.truncate(max);
        ids.into_iter().map(|id| self.provider_of(id)).collect()
    }

    /// Whether the placement asked for `hash` is reached: every wanted copy of the blob is
    /// confirmed, or for an erasure set, every header copy and one copy of every shard.
    pub fn placed(&self, hash: &Hash) -> bool {
        let full = |h: &Hash| {
            self.blob(h)
                .is_some_and(|b| b.target > 0 && b.holding().len() >= usize::from(b.target))
        };
        match self.state.sets.iter().find(|s| s.id == *hash) {
            Some(set) => full(hash) && set.header.shards.iter().all(&full),
            None => full(hash),
        }
    }

    /// A store member joined the swarm (or announced new caps).
    pub fn member_joined(&mut self, id: EndpointId, quota_bytes: u64, relay: Option<String>) {
        if let Some(m) = self.state.members.iter_mut().find(|m| m.id == id) {
            m.quota_bytes = quota_bytes;
            m.present = quota_bytes > 0;
            if relay.is_some() {
                m.relay = relay;
            }
            return;
        }
        if quota_bytes == 0 {
            return;
        }
        if self.state.members.len() >= MAX_MEMBERS {
            // Room is made by an absent member that holds nothing, if any.
            let blobs = &self.state.blobs;
            let Some(idle) = self
                .state
                .members
                .iter()
                .position(|m| !m.present && !holds_any(blobs, &m.id))
            else {
                return;
            };
            self.state.members.remove(idle);
        }
        self.state.members.push(Member {
            id,
            quota_bytes,
            relay,
            present: true,
            refusals: Vec::new(),
            probing: false,
            last_probe_ms: 0,
        });
    }

    /// A member left the swarm: no new copy goes to it. Its copies stay counted until it
    /// has been absent for `absent_after_ms`; a member holding nothing is forgotten.
    pub fn member_left(&mut self, id: &EndpointId) {
        if !holds_any(&self.state.blobs, id) {
            self.state.members.retain(|m| m.id != *id);
            return;
        }
        if let Some(m) = self.state.members.iter_mut().find(|m| m.id == *id) {
            m.present = false;
        }
    }

    /// Tracked blobs, erasure-set count and bytes of copies requested by `owner`.
    fn load_of(&self, owner: &EndpointId) -> (usize, u64) {
        self.state
            .blobs
            .iter()
            .filter(|b| b.target > 0 && b.owner.as_ref() == Some(owner))
            .fold((0usize, 0u64), |(n, bytes), b| {
                (
                    n + 1,
                    bytes.saturating_add(b.size.saturating_mul(u64::from(b.target))),
                )
            })
    }

    fn within_budget(
        &self,
        owner: Option<EndpointId>,
        blobs: usize,
        bytes: u64,
    ) -> Result<(), PinRefusal> {
        let Some(owner) = owner else {
            return Ok(());
        };
        let (n, used) = self.load_of(&owner);
        if n.saturating_add(blobs) > MAX_TRACKED_PER_OWNER
            || used.saturating_add(bytes) > MAX_BYTES_PER_OWNER
        {
            return Err(PinRefusal::Quota);
        }
        Ok(())
    }

    /// Whether `peer` asked for the blob or erasure set `hash` (and may release it).
    pub fn owned_by(&self, hash: &Hash, peer: &EndpointId) -> bool {
        match self.state.sets.iter().find(|s| s.id == *hash) {
            Some(set) => set.owner.as_ref() == Some(peer),
            None => self
                .blob(hash)
                .is_some_and(|b| b.target > 0 && b.owner.as_ref() == Some(peer)),
        }
    }

    /// The blobs `peer` asked for and still wants.
    pub fn owned(&self, peer: &EndpointId) -> impl Iterator<Item = &Tracked> {
        self.state
            .blobs
            .iter()
            .filter(move |b| b.target > 0 && b.owner.as_ref() == Some(peer))
    }

    /// [`DurabilityManager::forget`] on behalf of `peer`: only the peer that asked for
    /// `hash` may release it. Returns whether it was released.
    pub fn forget_by(&mut self, peer: &EndpointId, hash: &Hash) -> bool {
        if !self.owned_by(hash, peer) {
            return false;
        }
        self.forget(hash);
        true
    }

    /// The coordinator's operator asks for `replicas` copies of a blob. Asking again raises
    /// the target.
    pub fn request(
        &mut self,
        hash: Hash,
        size: u64,
        providers: Vec<Provider>,
        access: [u8; 16],
        replicas: u8,
    ) -> Result<(), PinRefusal> {
        let link = PinLink {
            providers: BoundedVec::new(),
            hash,
            size,
            access,
        };
        self.request_as(None, &link, providers, replicas, 0)
    }

    /// The peer `owner` asks for `replicas` copies of `link`'s blob, kept `ttl_s` seconds
    /// (0: until released). Asking again raises the target; a blob someone else asked for
    /// first is left as it is (only its owner changes it).
    pub fn request_by(
        &mut self,
        owner: EndpointId,
        link: &PinLink,
        replicas: u8,
        ttl_s: u32,
    ) -> Result<(), PinRefusal> {
        let providers = link.providers.as_slice().to_vec();
        self.request_as(Some(owner), link, providers, replicas, ttl_s)
    }

    fn request_as(
        &mut self,
        owner: Option<EndpointId>,
        link: &PinLink,
        providers: Vec<Provider>,
        replicas: u8,
        ttl_s: u32,
    ) -> Result<(), PinRefusal> {
        let (hash, size, access) = (link.hash, link.size, link.access);
        let replicas = replicas.clamp(1, MAX_REPLICAS);
        if let Some(i) = self.state.blobs.iter().position(|b| b.hash == hash) {
            let Some(b) = self.state.blobs.get(i) else {
                return Err(PinRefusal::Busy);
            };
            if b.size != size || b.access != access {
                return Err(PinRefusal::NotMember);
            }
            // A released blob still draining goes to whoever asks for it again.
            let mine = b.target == 0 || owner.is_none() || b.owner == owner;
            if !mine {
                return Ok(());
            }
            let extra = u64::from(replicas.saturating_sub(b.target));
            let take = b.target == 0 && b.owner != owner;
            self.within_budget(
                owner,
                usize::from(b.target == 0),
                size.saturating_mul(if b.target == 0 {
                    u64::from(replicas)
                } else {
                    extra
                }),
            )?;
            let Some(b) = self.state.blobs.get_mut(i) else {
                return Err(PinRefusal::Busy);
            };
            if take {
                b.owner = owner;
                b.since_ms = None;
            }
            b.target = b.target.max(replicas);
            if owner.is_some() {
                b.ttl_s = ttl_s;
            }
            add_providers(&mut b.providers, providers);
            return Ok(());
        }
        if self.state.blobs.len() >= MAX_TRACKED {
            return Err(PinRefusal::Busy);
        }
        self.within_budget(owner, 1, size.saturating_mul(u64::from(replicas)))?;
        let mut kept = Vec::new();
        add_providers(&mut kept, providers);
        self.state.blobs.push(Tracked {
            hash,
            size,
            access,
            providers: kept,
            target: replicas,
            holders: Vec::new(),
            shard_of: None,
            placed: false,
            owner,
            ttl_s,
            since_ms: None,
            rebuilding: false,
            rebuild_after_ms: 0,
        });
        Ok(())
    }

    /// A sharer asks to keep a ciphertext as an erasure set: one copy of each shard, each on
    /// a different member, and [`HEADER_COPIES`] copies of the header blob (whose hash, the
    /// set identity, is returned). `providers` serve the header and the
    /// shards until they are placed.
    pub fn request_erasure(
        &mut self,
        header: Header,
        providers: Vec<Provider>,
        access: [u8; 16],
    ) -> Result<Hash, PinRefusal> {
        self.request_erasure_as(None, header, providers, access)
    }

    /// [`DurabilityManager::request_erasure`] on behalf of the peer `owner`, within its
    /// budget; a set someone else asked for first is left as it is.
    pub fn request_erasure_by(
        &mut self,
        owner: EndpointId,
        header: Header,
        providers: Vec<Provider>,
        access: [u8; 16],
    ) -> Result<Hash, PinRefusal> {
        self.request_erasure_as(Some(owner), header, providers, access)
    }

    fn request_erasure_as(
        &mut self,
        owner: Option<EndpointId>,
        header: Header,
        providers: Vec<Provider>,
        access: [u8; 16],
    ) -> Result<Hash, PinRefusal> {
        header.validate().map_err(|_| PinRefusal::NotMember)?;
        let id = header.id();
        if let Some(set) = self.state.sets.iter().find(|s| s.id == id) {
            if set.access != access {
                return Err(PinRefusal::NotMember);
            }
            if owner.is_some() && set.owner != owner {
                return Ok(id);
            }
            for b in &mut self.state.blobs {
                if b.shard_of.is_some_and(|s| s.set == id) {
                    b.target = b.target.max(1);
                } else if b.hash == id {
                    b.target = b.target.max(HEADER_COPIES);
                }
            }
            return Ok(id);
        }
        let mut fresh: Vec<(u16, Hash)> = Vec::new();
        for (index, hash) in (0u16..).zip(header.shards.iter().copied()) {
            if !fresh.iter().any(|(_, h)| *h == hash) {
                fresh.push((index, hash));
            }
        }
        let new = fresh
            .iter()
            .filter(|(_, h)| !self.state.blobs.iter().any(|b| b.hash == *h))
            .count();
        // The shards, plus the header blob itself.
        if self.state.blobs.len().saturating_add(new).saturating_add(1) > MAX_TRACKED {
            return Err(PinRefusal::Busy);
        }
        let size = u64::from(header.shard_size);
        let header_len = header.encode().len() as u64;
        let bytes = size
            .saturating_mul(header.shards.len() as u64)
            .saturating_add(header_len.saturating_mul(u64::from(HEADER_COPIES)));
        self.within_budget(owner, new.saturating_add(1), bytes)?;
        for (index, hash) in fresh {
            // Identical shards (degenerate data) share one blob with one copy per occurrence.
            let copies = header.shards.iter().filter(|h| **h == hash).count();
            let copies = u8::try_from(copies)
                .unwrap_or(MAX_REPLICAS)
                .min(MAX_REPLICAS);
            if let Some(b) = self.state.blobs.iter_mut().find(|b| b.hash == hash) {
                if b.size != size || b.access != access {
                    return Err(PinRefusal::NotMember);
                }
                b.target = b.target.max(copies);
                continue;
            }
            self.state.blobs.push(Tracked {
                hash,
                size,
                access,
                providers: providers.clone(),
                target: copies,
                holders: Vec::new(),
                shard_of: Some(ShardOf { set: id, index }),
                placed: false,
                owner,
                ttl_s: 0,
                since_ms: None,
                rebuilding: false,
                rebuild_after_ms: 0,
            });
        }
        let link = PinLink {
            providers: BoundedVec::new(),
            hash: id,
            size: header_len,
            access,
        };
        self.request_as(owner, &link, providers, HEADER_COPIES, 0)?;
        self.state.sets.push(TrackedSet {
            id,
            header,
            access,
            owner,
        });
        Ok(id)
    }

    /// The tracked erasure set `id`.
    pub fn set(&self, id: &Hash) -> Option<&TrackedSet> {
        self.state.sets.iter().find(|s| s.id == *id)
    }

    /// The sharer no longer wants copies: every copy is released, then the blob forgotten.
    /// `hash` may also be an erasure set identity: all its shards are released.
    pub fn forget(&mut self, hash: &Hash) {
        for b in &mut self.state.blobs {
            if b.hash == *hash || b.shard_of.is_some_and(|s| s.set == *hash) {
                b.target = 0;
            }
        }
    }

    /// Lost shards to rebuild at `now`: a shard placed once whose copies are all lost, when
    /// at least `k` other shards of its set are held. Each is returned once until
    /// [`DurabilityManager::rebuilt`] reports it.
    pub fn plan_repairs(&mut self, now: u64) -> Vec<Rebuild> {
        let mut out = Vec::new();
        for i in 0..self.state.blobs.len() {
            let Some(b) = self.state.blobs.get(i) else {
                break;
            };
            let Some(of) = b.shard_of else {
                continue;
            };
            if b.target == 0
                || !b.placed
                || b.live() > 0
                || b.rebuilding
                || now < b.rebuild_after_ms
            {
                continue;
            }
            let Some(set) = self.state.sets.iter().find(|s| s.id == of.set) else {
                continue;
            };
            let mut sources: Vec<(u16, Hash, EndpointId)> = Vec::new();
            for (index, hash) in (0u16..).zip(set.header.shards.iter().copied()) {
                if hash == b.hash {
                    continue;
                }
                let holder = self
                    .state
                    .blobs
                    .iter()
                    .find(|o| o.hash == hash)
                    .and_then(|o| o.holding().first().copied());
                if let Some(member) = holder {
                    sources.push((index, hash, member));
                }
            }
            if sources.len() < usize::from(set.header.k) {
                continue;
            }
            let rebuild = Rebuild {
                set: of.set,
                header: set.header.clone(),
                index: of.index,
                hash: b.hash,
                access: set.access,
                sources,
            };
            if let Some(b) = self.state.blobs.get_mut(i) {
                b.rebuilding = true;
            }
            tracing::info!(hash = %rebuild.hash, index = rebuild.index, "rebuilding a lost shard");
            out.push(rebuild);
        }
        out
    }

    /// Outcome of a [`Rebuild`]: `provider` now serves the rebuilt shard, or `None` if the
    /// rebuild failed (it is tried again after `retry_after_ms`).
    pub fn rebuilt(&mut self, now: u64, hash: &Hash, provider: Option<Provider>) {
        let retry = now.saturating_add(self.cfg.retry_after_ms);
        let Some(b) = self.state.blobs.iter_mut().find(|b| b.hash == *hash) else {
            return;
        };
        b.rebuilding = false;
        match provider {
            Some(p) => {
                b.providers = vec![p];
                b.placed = false;
            }
            None => b.rebuild_after_ms = retry,
        }
    }

    fn used(&self, member: &EndpointId) -> u64 {
        self.state
            .blobs
            .iter()
            .flat_map(|b| {
                b.holders
                    .iter()
                    .filter(|h| h.member == *member && h.state != HolderState::Lost)
                    .map(move |_| b.size)
            })
            .fold(0u64, u64::saturating_add)
    }

    fn provider_of(&self, id: EndpointId) -> Provider {
        let relay = self
            .state
            .members
            .iter()
            .find(|m| m.id == id)
            .and_then(|m| m.relay.as_deref())
            .and_then(|r| nuee_proto::BoundedString::try_from(r).ok());
        Provider { id, relay }
    }

    /// Decides what to do at `now`: replace absent or failed copies, place missing ones on
    /// members with room (never two copies on one member), release extra ones, challenge
    /// and probe holders when due. Orders are marked in flight until their report arrives.
    pub fn plan(&mut self, now: u64) -> Vec<Order> {
        let cfg = self.cfg;
        let mut orders = Vec::new();
        for m in &mut self.state.members {
            m.refusals.retain(|(_, until)| *until > now);
        }

        // Copies absent for too long are lost.
        for b in &mut self.state.blobs {
            for h in &mut b.holders {
                if h.state != HolderState::Lost
                    && !h.busy
                    && now.saturating_sub(h.last_seen_ms) > cfg.absent_after_ms
                {
                    tracing::info!(member = %h.member, hash = %b.hash, "copy lost: holder absent");
                    h.state = HolderState::Lost;
                }
            }
        }

        // Blobs past their time to live, and blobs that never got a first copy.
        let mut expired = Vec::new();
        for b in &mut self.state.blobs {
            let since = *b.since_ms.get_or_insert(now);
            let age = now.saturating_sub(since);
            let ttl = b.ttl_s > 0 && age >= u64::from(b.ttl_s).saturating_mul(1000);
            let unplaced = !b.placed && b.shard_of.is_none() && age >= UNPLACED_EXPIRY_MS;
            if b.target > 0 && (ttl || unplaced) {
                tracing::info!(hash = %b.hash, ttl, unplaced, "tracked blob expired");
                expired.push(b.hash);
            }
        }
        for hash in expired {
            self.forget(&hash);
        }

        // Forgotten blobs whose copies are all gone.
        self.state
            .blobs
            .retain(|b| b.target > 0 || b.holders.iter().any(|h| h.state != HolderState::Lost));
        let blobs = &self.state.blobs;
        self.state.sets.retain(|s| {
            blobs
                .iter()
                .any(|b| b.shard_of.is_some_and(|o| o.set == s.id))
        });

        let present: Vec<EndpointId> = self
            .state
            .members
            .iter()
            .filter(|m| m.present)
            .map(|m| m.id)
            .collect();

        for i in 0..self.state.blobs.len() {
            let Some(b) = self.state.blobs.get(i) else {
                break;
            };
            let (hash, size, target, live) = (b.hash, b.size, usize::from(b.target), b.live());

            // Lost copies on reachable members are dropped.
            let lost: Vec<EndpointId> = b
                .holders
                .iter()
                .filter(|h| h.state == HolderState::Lost && !h.busy && present.contains(&h.member))
                .map(|h| h.member)
                .collect();

            // A shard placed once with no copy left is rebuilt (plan_repairs), not refetched.
            let awaiting_rebuild = b.shard_of.is_some() && b.placed && live == 0;
            // Each shard of a set goes to a member holding no other shard of that set.
            let siblings: Vec<EndpointId> = match b.shard_of {
                Some(of) => self
                    .state
                    .blobs
                    .iter()
                    .filter(|o| o.hash != hash && o.shard_of.is_some_and(|s| s.set == of.set))
                    .flat_map(|o| o.holders.iter())
                    .filter(|h| h.state != HolderState::Lost)
                    .map(|h| h.member)
                    .collect(),
                None => Vec::new(),
            };

            // New copies.
            let mut placements = Vec::new();
            if live < target && !awaiting_rebuild {
                let mut candidates: Vec<(u64, EndpointId)> = self
                    .state
                    .members
                    .iter()
                    .filter(|m| m.present)
                    .filter(|m| !b.holders.iter().any(|h| h.member == m.id))
                    .filter(|m| !siblings.contains(&m.id))
                    .filter(|m| !m.refusals.iter().any(|(h, _)| *h == hash))
                    .filter_map(|m| {
                        let free = m.quota_bytes.saturating_sub(self.used(&m.id));
                        (free >= size).then_some((free.min(RANK_QUOTA_CAP), m.id))
                    })
                    .collect();
                // Roomiest first, up to the cap; ties go by a per-blob draw that an identity
                // chosen in advance cannot win for every blob.
                candidates
                    .sort_by_cached_key(|(free, id)| (std::cmp::Reverse(*free), draw(&hash, id)));
                let mut providers: Vec<Provider> = b
                    .holders
                    .iter()
                    .filter(|h| h.state == HolderState::Holding)
                    .map(|h| self.provider_of(h.member))
                    .collect();
                for p in &b.providers {
                    if !providers.iter().any(|q| q.id == p.id) {
                        providers.push(p.clone());
                    }
                }
                providers.truncate(nuee_proto::limits::MAX_LINK_PROVIDERS);
                for (_, member) in candidates.into_iter().take(target - live) {
                    placements.push(member);
                    orders.push(Order::Replicate {
                        member,
                        hash,
                        size,
                        providers: providers.clone(),
                        access: b.access,
                    });
                }
            }

            // Extra copies: the newest confirmed ones go first.
            let mut releases = Vec::new();
            if live > target {
                let mut holding: Vec<&Holder> = b
                    .holders
                    .iter()
                    .filter(|h| h.state == HolderState::Holding && !h.busy)
                    .collect();
                holding.sort_by(|x, y| y.since_ms.cmp(&x.since_ms).then(y.member.cmp(&x.member)));
                releases = holding
                    .into_iter()
                    .take(live - target)
                    .map(|h| h.member)
                    .collect();
            }

            let Some(b) = self.state.blobs.get_mut(i) else {
                break;
            };
            for member in lost.into_iter().chain(releases.iter().copied()) {
                if let Some(h) = b.holder_mut(&member) {
                    h.busy = true;
                    orders.push(Order::Release { member, hash });
                }
            }
            for member in placements {
                b.holders.push(Holder {
                    member,
                    state: HolderState::Pending,
                    since_ms: now,
                    last_seen_ms: now,
                    last_challenge_ms: now,
                    busy: true,
                });
            }

            // Challenges.
            for h in &mut b.holders {
                if h.state == HolderState::Holding
                    && !h.busy
                    && now.saturating_sub(h.last_challenge_ms) >= cfg.challenge_every_ms
                {
                    h.busy = true;
                    orders.push(Order::Challenge {
                        member: h.member,
                        hash,
                        size,
                    });
                }
            }
        }

        // Probes of every member holding something.
        let holders: Vec<EndpointId> = {
            let mut v: Vec<EndpointId> = self
                .state
                .blobs
                .iter()
                .flat_map(|b| b.holders.iter())
                .filter(|h| h.state != HolderState::Lost)
                .map(|h| h.member)
                .collect();
            v.sort();
            v.dedup();
            v
        };
        for member in holders {
            let entry = match self.state.members.iter_mut().find(|m| m.id == member) {
                Some(m) => m,
                None => {
                    self.state.members.push(Member {
                        id: member,
                        quota_bytes: 0,
                        relay: None,
                        present: false,
                        refusals: Vec::new(),
                        probing: false,
                        last_probe_ms: 0,
                    });
                    match self.state.members.last_mut() {
                        Some(m) => m,
                        None => continue,
                    }
                }
            };
            if !entry.probing && now.saturating_sub(entry.last_probe_ms) >= cfg.probe_every_ms {
                entry.probing = true;
                entry.last_probe_ms = now;
                orders.push(Order::Probe { member });
            }
        }
        orders
    }

    /// Records the outcome of an order at `now`.
    pub fn apply(&mut self, now: u64, report: Report) {
        let retry = now.saturating_add(self.cfg.retry_after_ms);
        match report {
            Report::Replicate {
                member,
                hash,
                outcome,
            } => {
                let Some(b) = self.state.blobs.iter_mut().find(|b| b.hash == hash) else {
                    return;
                };
                match outcome {
                    Replicated::Done => {
                        b.placed = true;
                        if let Some(h) = b.holder_mut(&member) {
                            h.state = HolderState::Holding;
                            h.busy = false;
                            h.last_seen_ms = now;
                            h.last_challenge_ms = now;
                        }
                    }
                    Replicated::Refused(_) | Replicated::Failed => {
                        b.holders.retain(|h| h.member != member);
                        if let Some(m) = self.state.members.iter_mut().find(|m| m.id == member) {
                            m.refusals.push((hash, retry));
                        }
                    }
                }
            }
            Report::Release { member, hash, ok } => {
                let Some(b) = self.state.blobs.iter_mut().find(|b| b.hash == hash) else {
                    return;
                };
                if ok {
                    b.holders.retain(|h| h.member != member);
                } else if let Some(h) = b.holder_mut(&member) {
                    h.busy = false;
                }
            }
            Report::Challenge {
                member,
                hash,
                outcome,
            } => {
                let Some(h) = self
                    .state
                    .blobs
                    .iter_mut()
                    .find(|b| b.hash == hash)
                    .and_then(|b| b.holder_mut(&member))
                else {
                    return;
                };
                h.busy = false;
                match outcome {
                    Challenged::Passed => {
                        h.last_seen_ms = now;
                        h.last_challenge_ms = now;
                    }
                    Challenged::Failed => {
                        tracing::info!(%member, %hash, "copy lost: challenge failed");
                        h.last_seen_ms = now;
                        h.state = HolderState::Lost;
                    }
                    Challenged::Unreachable => {}
                }
            }
            Report::Probe { member, complete } => {
                if let Some(m) = self.state.members.iter_mut().find(|m| m.id == member) {
                    m.probing = false;
                }
                let Some(complete) = complete else {
                    return;
                };
                for b in &mut self.state.blobs {
                    let hash = b.hash;
                    if let Some(h) = b.holder_mut(&member) {
                        if h.state == HolderState::Lost {
                            continue;
                        }
                        h.last_seen_ms = now;
                        if h.state == HolderState::Holding && !h.busy && !complete.contains(&hash) {
                            tracing::info!(%member, %hash, "copy lost: no longer listed");
                            h.state = HolderState::Lost;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(not(target_family = "wasm"))]
pub use io::{
    DurabilityService, Keep, Placement, ReplicaAnswer, ServiceConfig, ServiceTask, StoreMember,
    durable_link, get_durable, keep_durably, list_pins, refresh_link, release_pin,
    request_erasure_set, request_replicas, where_is,
};

#[cfg(not(target_family = "wasm"))]
mod io {
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::time::Duration;

    use iroh::endpoint::{Connection, RecvStream, SendStream, VarInt};
    use iroh::protocol::{AcceptError, ProtocolHandler, Router};
    use n0_future::task::AbortOnDropHandle;
    use nuee_net::driver::{MAX_HELLO, close_code};
    use nuee_net::{Admission, AdmitConfig, AdmitError, FrameError, Net, recv_msg, send_msg};
    use nuee_proto::limits::{MAX_LINK_PROVIDERS, MAX_MSG};
    use nuee_proto::{
        ALPN_JOB, ALPN_PIN, BoundedVec, Caps, EndpointId, FileLink, Hash, JobLink, PROOF_PIN,
        PROTO_VERSION, PinInfo, PinLink, PinMsg, PinRefusal, Provider,
    };
    use tokio::sync::mpsc;

    use super::{
        Challenged, DurabilityManager, DurableConfig, DurableState, Order, Rebuild, Replicated,
        Report,
    };
    use crate::StoreError;
    use crate::erasure::Header;
    use crate::gate::{CONNECT_TIMEOUT, StoreHub};
    use crate::pin::{PinEntry, Registry};
    use crate::share::{FilesAcceptor, Offer};

    /// Time allowed for an answer on `nuee/pin/0`.
    const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);
    /// Time a member has to fetch and store a copy.
    const REPLICATE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
    /// Copies a member fetches at once.
    const MAX_PARALLEL_FETCHES: usize = 4;

    /// Name of the tag that keeps a copy held for a storage swarm alive. Separate from the
    /// tags of `pin` and `share`, so releasing a copy never drops the node's own data.
    pub fn stored_tag(hash: Hash) -> String {
        format!("nuee/stored/{hash}")
    }

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        // Plain data behind the lock; a panic elsewhere cannot leave it half-updated.
        m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }

    fn close(conn: &Connection, code: u32, reason: &[u8]) {
        conn.close(VarInt::from_u32(code), reason);
    }

    async fn answer(recv: &mut RecvStream) -> Result<PinMsg, String> {
        n0_future::time::timeout(ANSWER_TIMEOUT, recv_msg::<PinMsg>(recv, MAX_MSG))
            .await
            .map_err(|_| "no answer in time".to_owned())?
            .map_err(|e| e.to_string())
    }

    /// Opens a `nuee/pin/0` session with `peer` and sends the hello.
    async fn open_pin(
        net: &Net,
        secret: &[u8; 16],
        peer: EndpointId,
    ) -> Result<(Connection, SendStream, RecvStream), StoreError> {
        let conn = n0_future::time::timeout(CONNECT_TIMEOUT, net.connect(peer, ALPN_PIN))
            .await
            .map_err(|_| StoreError::Unreachable("connection timed out".to_owned()))?
            .map_err(|e| StoreError::Unreachable(e.to_string()))?;
        let (mut send, recv) = match conn.open_bi().await {
            Ok(streams) => streams,
            Err(e) => return Err(refused_or_lost(&conn, e.to_string()).await),
        };
        let hello = PinMsg::Hello {
            version: PROTO_VERSION,
            proof: nuee_proto::proof(PROOF_PIN, secret, &net.id(), &peer),
        };
        send_or_refused(&conn, &mut send, &hello).await?;
        Ok((conn, send, recv))
    }

    /// Writes one frame to the coordinator. A peer that is not let in closes the session
    /// with REFUSED, which can land while the frame is still being written: the write then
    /// fails, and the close code tells a refusal from a lost connection.
    async fn send_or_refused(
        conn: &Connection,
        send: &mut SendStream,
        msg: &PinMsg,
    ) -> Result<(), StoreError> {
        match send_msg(send, msg).await {
            Ok(()) => Ok(()),
            Err(e) => Err(refused_or_lost(conn, e.to_string()).await),
        }
    }

    /// Reads and checks the hello of an incoming `nuee/pin/0` session.
    async fn accept_hello(
        admission: &Admission,
        conn: &Connection,
        secret: &[u8; 16],
        own: EndpointId,
    ) -> Option<(SendStream, RecvStream)> {
        let peer = conn.remote_id();
        let pending = match admission.begin(&peer) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(%peer, "pin connection refused: {e}");
                close(conn, close_code::BUSY, b"busy");
                return None;
            }
        };
        let hello = admission
            .within(&peer, async {
                let (send, mut recv) = conn.accept_bi().await.map_err(|e| {
                    AdmitError::Frame(FrameError::Read(iroh::endpoint::ReadError::from(e)))
                })?;
                match recv_msg::<PinMsg>(&mut recv, MAX_HELLO).await? {
                    PinMsg::Hello { version, proof } => {
                        if version != PROTO_VERSION {
                            return Err(AdmitError::BadVersion(version));
                        }
                        if nuee_proto::check_proof(PROOF_PIN, secret, &peer, &own, &proof) {
                            Ok((send, recv))
                        } else {
                            admission.record_failure(&peer);
                            Err(AdmitError::BadProof)
                        }
                    }
                    _ => Err(AdmitError::Unexpected),
                }
            })
            .await;
        drop(pending);
        match hello {
            Ok(streams) => Some(streams),
            Err(e) => {
                tracing::debug!(%peer, "pin admission failed: {e}");
                let code = match e {
                    AdmitError::Timeout => close_code::TIMEOUT,
                    AdmitError::Frame(_) | AdmitError::Unexpected => close_code::PROTOCOL,
                    AdmitError::Busy | AdmitError::Throttled => close_code::BUSY,
                    AdmitError::BadProof | AdmitError::BadVersion(_) => close_code::REFUSED,
                };
                close(conn, code, b"refused");
                None
            }
        }
    }

    async fn finish(conn: Connection, mut send: SendStream) {
        if let Err(e) = send.finish() {
            tracing::debug!("pin stream already closed: {e}");
        }
        if n0_future::time::timeout(Duration::from_secs(2), send.stopped())
            .await
            .is_err()
        {
            tracing::debug!("pin peer did not close in time");
        }
        close(&conn, close_code::DONE, b"done");
    }

    // -----------------------------------------------------------------------------------------
    // Coordinator side.

    /// Settings of the [`DurabilityService`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ServiceConfig {
        /// Timing of the manager.
        pub durable: DurableConfig,
        /// How often the manager plans.
        pub tick: Duration,
        /// Chunks sampled per challenge.
        pub samples: usize,
        /// Largest blob accepted for replication.
        pub max_blob: u64,
    }

    impl Default for ServiceConfig {
        fn default() -> Self {
            ServiceConfig {
                durable: DurableConfig::default(),
                tick: Duration::from_secs(5),
                samples: crate::challenge::DEFAULT_SAMPLES,
                max_blob: crate::MAX_BLOB_NATIVE,
            }
        }
    }

    /// How often a sharer's session checks whether its placement is reached.
    const PLACED_POLL: Duration = Duration::from_millis(200);

    enum Next {
        Msg(PinMsg),
        Placed(Hash),
        End,
    }

    enum Wake {
        Tick,
        Report(Report),
        Rebuilt(Hash, Option<Provider>),
    }

    #[derive(Debug)]
    struct ServiceInner {
        net: Net,
        secret: [u8; 16],
        cfg: ServiceConfig,
        manager: Mutex<DurabilityManager>,
        path: Option<PathBuf>,
        admission: Admission,
        wake: mpsc::UnboundedSender<Wake>,
        rx: Mutex<Option<mpsc::UnboundedReceiver<Wake>>>,
        rebuilder: Mutex<Option<(StoreHub, FilesAcceptor)>>,
        access: Mutex<PeerAccess>,
    }

    /// Which swarm peers may use `nuee/pin/0`, mirroring the job driver's decisions.
    #[derive(Debug, Default)]
    struct PeerAccess {
        /// The operator approves peers by hand (`--approve`).
        approval: bool,
        /// Peers the job driver let in (after the operator's approval when it is required).
        joined: HashSet<EndpointId>,
        /// Peers banned by the job driver.
        banned: HashSet<EndpointId>,
    }

    impl PeerAccess {
        fn allows(&self, peer: &EndpointId) -> bool {
            !self.banned.contains(peer) && (!self.approval || self.joined.contains(peer))
        }
    }

    impl std::fmt::Debug for Wake {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Wake::Tick => f.write_str("Tick"),
                Wake::Report(r) => write!(f, "Report({r:?})"),
                Wake::Rebuilt(h, p) => write!(f, "Rebuilt({h}, {p:?})"),
            }
        }
    }

    /// The durability manager of a swarm coordinator, with its I/O. Register
    /// [`DurabilityService`] itself as the `nuee/pin/0` handler of the coordinator's router
    /// (sharers ask for replicas there), feed it the store members with
    /// [`DurabilityService::member_joined`], and keep the task of
    /// [`DurabilityService::start`] alive.
    #[derive(Debug, Clone)]
    pub struct DurabilityService {
        inner: Arc<ServiceInner>,
    }

    /// The running loop of a [`DurabilityService`]; dropping it stops the loop.
    #[derive(Debug)]
    pub struct ServiceTask {
        _run: AbortOnDropHandle<()>,
        _tick: AbortOnDropHandle<()>,
    }

    impl DurabilityService {
        /// A service for the swarm whose link secret is `secret`. With `path` (normally
        /// `<data dir>/durable.json`) the state is loaded from and saved to that file.
        pub fn new(
            net: &Net,
            secret: [u8; 16],
            cfg: ServiceConfig,
            path: Option<PathBuf>,
        ) -> Result<DurabilityService, StoreError> {
            let manager = match &path {
                Some(p) => match std::fs::read(p) {
                    Ok(bytes) => {
                        let state: DurableState = serde_json::from_slice(&bytes)
                            .map_err(|e| StoreError::Registry(format!("{}: {e}", p.display())))?;
                        DurabilityManager::restore(cfg.durable, state, now_ms())
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        DurabilityManager::new(cfg.durable)
                    }
                    Err(e) => return Err(e.into()),
                },
                None => DurabilityManager::new(cfg.durable),
            };
            for m in &manager.state().members {
                if let Some(relay) = &m.relay
                    && let Err(e) = net.add_announced_relay_str(m.id, relay)
                {
                    tracing::debug!(member = %m.id, "ignoring a stored relay: {e}");
                }
            }
            let (wake, rx) = mpsc::unbounded_channel();
            Ok(DurabilityService {
                inner: Arc::new(ServiceInner {
                    net: net.clone(),
                    secret,
                    cfg,
                    manager: Mutex::new(manager),
                    path,
                    admission: Admission::new(AdmitConfig::default()),
                    wake,
                    rx: Mutex::new(Some(rx)),
                    rebuilder: Mutex::new(None),
                    access: Mutex::new(PeerAccess::default()),
                }),
            })
        }

        /// The swarm's operator approves peers by hand (`--approve`): `nuee/pin/0` then only
        /// serves peers the job driver let in ([`DurabilityService::member_joined`]).
        pub fn require_approval(&self, on: bool) {
            lock(&self.inner.access).approval = on;
        }

        /// The job driver banned `peer`: it may no longer use `nuee/pin/0` nor receive copies.
        pub fn peer_banned(&self, peer: EndpointId) {
            {
                let mut access = lock(&self.inner.access);
                access.joined.remove(&peer);
                access.banned.insert(peer);
            }
            lock(&self.inner.manager).member_left(&peer);
        }

        /// Whether `peer` may use `nuee/pin/0` now.
        pub fn allows(&self, peer: &EndpointId) -> bool {
            *peer == self.inner.net.id() || lock(&self.inner.access).allows(peer)
        }

        /// The operator releases `hash` (a blob or an erasure set), whoever asked for it.
        pub fn forget(&self, hash: &Hash) {
            lock(&self.inner.manager).forget(hash);
            self.poke();
        }

        /// `<data dir>/durable.json`.
        pub fn default_path(data_dir: &std::path::Path) -> PathBuf {
            data_dir.join("durable.json")
        }

        /// Starts the planning loop. Only the first call starts anything.
        pub fn start(&self) -> Option<ServiceTask> {
            let rx = lock(&self.inner.rx).take()?;
            let run = n0_future::task::spawn(self.clone().run(rx));
            let tick = self.inner.cfg.tick;
            let wake = self.inner.wake.clone();
            let ticker = n0_future::task::spawn(async move {
                loop {
                    n0_future::time::sleep(tick).await;
                    if wake.send(Wake::Tick).is_err() {
                        break;
                    }
                }
            });
            Some(ServiceTask {
                _run: AbortOnDropHandle::new(run),
                _tick: AbortOnDropHandle::new(ticker),
            })
        }

        /// A copy of the manager's state.
        pub fn state(&self) -> DurableState {
            lock(&self.inner.manager).state().clone()
        }

        /// A swarm peer joined (the job driver let it in, after the operator's approval when
        /// it is required); it becomes a store member if its caps offer storage.
        pub fn member_joined(&self, peer: EndpointId, caps: &Caps) {
            {
                let mut access = lock(&self.inner.access);
                if access.banned.contains(&peer) {
                    return;
                }
                access.joined.insert(peer);
            }
            let relay = caps
                .relay
                .as_ref()
                .map(|r| r.as_str().to_owned())
                .filter(|r| !r.is_empty());
            if let Some(r) = &relay
                && let Err(e) = self.inner.net.add_announced_relay_str(peer, r)
            {
                tracing::debug!(%peer, "ignoring the member relay: {e}");
            }
            let quota = caps.store_quota_mb.saturating_mul(1024 * 1024);
            lock(&self.inner.manager).member_joined(peer, quota, relay);
            self.poke();
        }

        /// A swarm peer left.
        pub fn member_left(&self, peer: &EndpointId) {
            lock(&self.inner.manager).member_left(peer);
        }

        /// The operator asks for `replicas` copies of a blob.
        pub fn request(&self, link: &PinLink, replicas: u8) -> Result<(), PinRefusal> {
            if link.size > self.inner.cfg.max_blob {
                return Err(PinRefusal::TooLarge);
            }
            lock(&self.inner.manager).request(
                link.hash,
                link.size,
                link.providers.as_slice().to_vec(),
                link.access,
                replicas,
            )?;
            self.poke();
            Ok(())
        }

        /// What a sharer's `Replicate` does: `peer` asks for `replicas` copies of a blob.
        fn request_by(
            &self,
            peer: EndpointId,
            link: &PinLink,
            replicas: u8,
            ttl_s: u32,
        ) -> Result<(), PinRefusal> {
            if link.size > self.inner.cfg.max_blob {
                return Err(PinRefusal::TooLarge);
            }
            lock(&self.inner.manager).request_by(peer, link, replicas, ttl_s)?;
            self.poke();
            Ok(())
        }

        /// Lets the service rebuild lost erasure shards: survivors are fetched into `hub`, the
        /// rebuilt shard is stored there and offered through `acceptor` so members can fetch
        /// it. `acceptor` must be served on `nuee/job/0` of the endpoint of `hub`; when this
        /// node's `nuee/job/0` already runs a compute swarm, use a dedicated endpoint (see
        /// [`crate::share::ServeHandle::spawn`]). Without it, lost shards are not repaired.
        pub fn set_rebuilder(&self, hub: StoreHub, acceptor: FilesAcceptor) {
            *lock(&self.inner.rebuilder) = Some((hub, acceptor));
        }

        /// Asks to keep a ciphertext as the erasure set `header`, its header and shards
        /// served by `providers` until placed. Returns the set identity.
        pub fn request_erasure(
            &self,
            header: Header,
            providers: Vec<Provider>,
            access: [u8; 16],
        ) -> Result<Hash, PinRefusal> {
            if u64::from(header.shard_size) > self.inner.cfg.max_blob {
                return Err(PinRefusal::TooLarge);
            }
            let id = lock(&self.inner.manager).request_erasure(header, providers, access)?;
            self.poke();
            Ok(id)
        }

        fn request_erasure_by(
            &self,
            peer: EndpointId,
            header: Header,
            providers: Vec<Provider>,
            access: [u8; 16],
        ) -> Result<Hash, PinRefusal> {
            if u64::from(header.shard_size) > self.inner.cfg.max_blob {
                return Err(PinRefusal::TooLarge);
            }
            let id =
                lock(&self.inner.manager).request_erasure_by(peer, header, providers, access)?;
            self.poke();
            Ok(id)
        }

        fn poke(&self) {
            if self.inner.wake.send(Wake::Tick).is_err() {
                tracing::debug!("durability loop not running");
            }
        }

        fn save(&self, manager: &DurabilityManager) {
            let Some(path) = &self.inner.path else {
                return;
            };
            if let Err(e) = save_json(path, manager.state()) {
                tracing::warn!("cannot save {}: {e}", path.display());
            }
        }

        async fn run(self, mut rx: mpsc::UnboundedReceiver<Wake>) {
            while let Some(wake) = rx.recv().await {
                let now = now_ms();
                let (orders, repairs) = {
                    let mut m = lock(&self.inner.manager);
                    match wake {
                        Wake::Report(report) => m.apply(now, report),
                        Wake::Rebuilt(hash, provider) => m.rebuilt(now, &hash, provider),
                        Wake::Tick => {}
                    }
                    let orders = m.plan(now);
                    let repairs = if lock(&self.inner.rebuilder).is_some() {
                        m.plan_repairs(now)
                    } else {
                        Vec::new()
                    };
                    self.save(&m);
                    (orders, repairs)
                };
                for repair in repairs {
                    let svc = self.clone();
                    n0_future::task::spawn(async move {
                        let hash = repair.hash;
                        let provider = match svc.rebuild(repair).await {
                            Ok(p) => Some(p),
                            Err(why) => {
                                tracing::info!(%hash, "shard rebuild failed: {why}");
                                None
                            }
                        };
                        if svc.inner.wake.send(Wake::Rebuilt(hash, provider)).is_err() {
                            tracing::debug!("durability loop stopped");
                        }
                    });
                }
                for order in orders {
                    let svc = self.clone();
                    n0_future::task::spawn(async move {
                        let report = svc.execute(order).await;
                        if svc.inner.wake.send(Wake::Report(report)).is_err() {
                            tracing::debug!("durability loop stopped");
                        }
                    });
                }
            }
        }

        /// Fetches survivors until `k` verify, rebuilds the shard, stores and offers it.
        async fn rebuild(&self, job: Rebuild) -> Result<Provider, String> {
            let (hub, acceptor) = lock(&self.inner.rebuilder)
                .clone()
                .ok_or_else(|| "no rebuilder".to_owned())?;
            // The rebuilder's endpoint fetches and serves the shards.
            let net = hub.net().clone();
            let k = usize::from(job.header.k);
            let size = u64::from(job.header.shard_size);
            let mut have: Vec<(u16, Vec<u8>)> = Vec::new();
            for (index, hash, member) in job.sources {
                if have.len() >= k {
                    break;
                }
                if net.id() != self.inner.net.id()
                    && let Some(addr) = self.inner.net.peer_addr(member)
                {
                    let relay = addr.relay_urls().next().cloned();
                    net.add_peer(member, relay, addr.ip_addrs().copied().collect());
                }
                let access = job.access;
                let provider = Provider {
                    id: member,
                    relay: None,
                };
                let fetched = crate::share::fetch_ciphertext(
                    &net,
                    &hub,
                    std::slice::from_ref(&provider),
                    &access,
                    hash,
                    size,
                    &mut |_, _| {},
                )
                .await;
                if let Err(e) = fetched {
                    tracing::debug!(%member, %hash, "survivor shard not fetched: {e}");
                    continue;
                }
                match hub.read(hash).await {
                    Ok(bytes) if job.header.check_shard(index, &bytes) => {
                        have.push((index, bytes.to_vec()));
                    }
                    Ok(_) => tracing::info!(%member, %hash, "survivor shard does not verify"),
                    Err(e) => tracing::debug!(%hash, "survivor shard unreadable: {e}"),
                }
            }
            let shard = crate::erasure::rebuild_shard(&job.header, have, job.index)
                .map_err(|e| e.to_string())?;
            let stored = hub
                .add_bytes(bytes::Bytes::from(shard))
                .await
                .map_err(|e| e.to_string())?;
            if stored != job.hash {
                return Err(format!("stored {stored}, expected {}", job.hash));
            }
            acceptor.add(Offer {
                access: job.access,
                hash: job.hash,
                size,
            });
            Ok(crate::share::self_provider(&net))
        }

        async fn execute(&self, order: Order) -> Report {
            let net = &self.inner.net;
            let secret = &self.inner.secret;
            match order {
                Order::Replicate {
                    member,
                    hash,
                    size,
                    providers,
                    access,
                } => {
                    let outcome = replicate(net, secret, member, hash, size, providers, access)
                        .await
                        .unwrap_or_else(|why| {
                            tracing::debug!(%member, %hash, "replication failed: {why}");
                            Replicated::Failed
                        });
                    Report::Replicate {
                        member,
                        hash,
                        outcome,
                    }
                }
                Order::Release { member, hash } => {
                    let ok = match exchange(net, secret, member, PinMsg::Release { hash }).await {
                        Ok(PinMsg::Pins { .. }) => true,
                        Ok(other) => {
                            tracing::debug!(%member, "unexpected answer to a release: {other:?}");
                            false
                        }
                        Err(why) => {
                            tracing::debug!(%member, "release failed: {why}");
                            false
                        }
                    };
                    Report::Release { member, hash, ok }
                }
                Order::Probe { member } => Report::Probe {
                    member,
                    complete: list(net, secret, member).await,
                },
                Order::Challenge { member, hash, size } => {
                    // The pin session re-admits us to the member's blobs for its copies.
                    let outcome = match list(net, secret, member).await {
                        None => Challenged::Unreachable,
                        Some(_) => match nonce() {
                            Ok(nonce) => {
                                let verdict = crate::challenge::challenge(
                                    net,
                                    member,
                                    hash,
                                    size,
                                    nonce,
                                    self.inner.cfg.samples,
                                )
                                .await;
                                match verdict {
                                    crate::challenge::Verdict::Passed { .. } => Challenged::Passed,
                                    crate::challenge::Verdict::Failed(why) => {
                                        tracing::info!(%member, %hash, "challenge failed: {why}");
                                        Challenged::Failed
                                    }
                                    crate::challenge::Verdict::Unreachable(why) => {
                                        tracing::debug!(%member, "challenge unreachable: {why}");
                                        Challenged::Unreachable
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!("no randomness for a challenge: {e}");
                                Challenged::Unreachable
                            }
                        },
                    };
                    Report::Challenge {
                        member,
                        hash,
                        outcome,
                    }
                }
            }
        }

        async fn handle(&self, conn: Connection) {
            let inner = &self.inner;
            let peer = conn.remote_id();
            let Some((mut send, mut recv)) =
                accept_hello(&inner.admission, &conn, &inner.secret, inner.net.id()).await
            else {
                return;
            };
            if !self.allows(&peer) {
                tracing::debug!(%peer, "pin session refused: peer banned or not approved");
                close(&conn, close_code::REFUSED, b"refused");
                return;
            }
            // Frames are read by their own task so that waiting for a placement never drops
            // a half-read frame.
            let (frames, mut incoming) = mpsc::channel::<PinMsg>(4);
            let _reader = AbortOnDropHandle::new(n0_future::task::spawn(async move {
                loop {
                    match recv_msg::<PinMsg>(&mut recv, MAX_MSG).await {
                        Ok(m) => {
                            if frames.send(m).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::debug!(%peer, "pin session ended: {e}");
                            break;
                        }
                    }
                }
            }));
            let mut asked: HashMap<Hash, u8> = HashMap::new();
            // The placement this sharer waits for; `Holders` is pushed once it is reached.
            let mut watch: Option<Hash> = None;
            loop {
                let next = match watch {
                    Some(h) => {
                        n0_future::future::or(
                            async { incoming.recv().await.map_or(Next::End, Next::Msg) },
                            async {
                                self.wait_placed(h).await;
                                Next::Placed(h)
                            },
                        )
                        .await
                    }
                    None => incoming.recv().await.map_or(Next::End, Next::Msg),
                };
                let msg = match next {
                    Next::End => break,
                    Next::Placed(hash) => {
                        watch = None;
                        PinMsg::Where { hash }
                    }
                    Next::Msg(m) => m,
                };
                // A ban or a refusal can come while the session is open.
                if !self.allows(&peer) {
                    close(&conn, close_code::REFUSED, b"refused");
                    return;
                }
                let reply = match msg {
                    PinMsg::Where { hash } => PinMsg::Holders {
                        hash,
                        providers: self.where_is(&hash),
                    },
                    PinMsg::Replicate {
                        hash,
                        size,
                        mut providers,
                        access,
                        ttl_s,
                    } => {
                        crate::share::drop_refused_relays(
                            &self.inner.net,
                            providers.as_mut_slice(),
                        );
                        let n = asked.entry(hash).or_insert(0);
                        *n = n.saturating_add(1);
                        let link = PinLink {
                            providers,
                            hash,
                            size,
                            access,
                        };
                        match self.request_by(peer, &link, *n, ttl_s) {
                            Ok(()) => {
                                watch = Some(hash);
                                PinMsg::Accepted { hash }
                            }
                            Err(reason) => PinMsg::Refused { hash, reason },
                        }
                    }
                    PinMsg::ReplicateSet {
                        header,
                        mut providers,
                        access,
                    } => {
                        crate::share::drop_refused_relays(
                            &self.inner.net,
                            providers.as_mut_slice(),
                        );
                        let id = Hash::of(header.as_slice());
                        match crate::erasure::decode_header(header.as_slice()) {
                            Ok(h) => {
                                match self.request_erasure_by(
                                    peer,
                                    h,
                                    providers.as_slice().to_vec(),
                                    access,
                                ) {
                                    Ok(id) => {
                                        watch = Some(id);
                                        PinMsg::Accepted { hash: id }
                                    }
                                    Err(reason) => PinMsg::Refused { hash: id, reason },
                                }
                            }
                            Err(e) => {
                                tracing::debug!(%peer, "invalid erasure header: {e}");
                                PinMsg::Refused {
                                    hash: id,
                                    reason: PinRefusal::NotMember,
                                }
                            }
                        }
                    }
                    PinMsg::Release { hash } => {
                        // Only the peer that asked for it may release it.
                        let released = lock(&inner.manager).forget_by(&peer, &hash);
                        if released {
                            self.poke();
                            PinMsg::Pins {
                                items: self.tracked(&peer),
                            }
                        } else {
                            tracing::info!(%peer, %hash, "release refused: not the requester");
                            PinMsg::Refused {
                                hash,
                                reason: PinRefusal::NotMember,
                            }
                        }
                    }
                    PinMsg::List => PinMsg::Pins {
                        items: self.tracked(&peer),
                    },
                    other => {
                        tracing::debug!(%peer, "unexpected pin message: {other:?}");
                        break;
                    }
                };
                if let Err(e) = send_msg(&mut send, &reply).await {
                    tracing::debug!(%peer, "pin answer not delivered: {e}");
                    break;
                }
            }
            close(&conn, close_code::DONE, b"done");
        }

        /// Current holders of a blob or an erasure set (see [`DurabilityManager::holders`]).
        pub fn where_is(&self, hash: &Hash) -> BoundedVec<Provider, MAX_LINK_PROVIDERS> {
            let providers = lock(&self.inner.manager).holders(hash);
            BoundedVec::try_from(providers).unwrap_or_default()
        }

        /// Whether the placement asked for `hash` is reached.
        pub fn placed(&self, hash: &Hash) -> bool {
            lock(&self.inner.manager).placed(hash)
        }

        async fn wait_placed(&self, hash: Hash) {
            while !self.placed(&hash) {
                n0_future::time::sleep(PLACED_POLL).await;
            }
        }

        /// The blobs `peer` asked for.
        fn tracked(&self, peer: &EndpointId) -> BoundedVec<PinInfo, 256> {
            let m = lock(&self.inner.manager);
            let items: Vec<PinInfo> = m
                .owned(peer)
                .take(256)
                .map(|b| PinInfo {
                    hash: b.hash,
                    size: b.size,
                    complete: b.target > 0 && b.holding().len() >= usize::from(b.target),
                })
                .collect();
            BoundedVec::try_from(items).unwrap_or_default()
        }
    }

    impl ProtocolHandler for DurabilityService {
        async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
            self.handle(connection).await;
            Ok(())
        }
    }

    fn nonce() -> Result<[u8; 32], StoreError> {
        let a = nuee_proto::new_secret16()?;
        let b = nuee_proto::new_secret16()?;
        let mut out = [0u8; 32];
        out[..16].copy_from_slice(&a);
        out[16..].copy_from_slice(&b);
        Ok(out)
    }

    async fn exchange(
        net: &Net,
        secret: &[u8; 16],
        member: EndpointId,
        msg: PinMsg,
    ) -> Result<PinMsg, String> {
        let (conn, mut send, mut recv) = open_pin(net, secret, member)
            .await
            .map_err(|e| e.to_string())?;
        let res = async {
            send_msg(&mut send, &msg).await.map_err(|e| e.to_string())?;
            answer(&mut recv).await
        }
        .await;
        finish(conn, send).await;
        res
    }

    async fn list(net: &Net, secret: &[u8; 16], member: EndpointId) -> Option<Vec<Hash>> {
        match exchange(net, secret, member, PinMsg::List).await {
            Ok(PinMsg::Pins { items }) => Some(
                items
                    .as_slice()
                    .iter()
                    .filter(|i| i.complete)
                    .map(|i| i.hash)
                    .collect(),
            ),
            Ok(other) => {
                tracing::debug!(%member, "unexpected answer to a list: {other:?}");
                None
            }
            Err(why) => {
                tracing::debug!(%member, "probe failed: {why}");
                None
            }
        }
    }

    async fn replicate(
        net: &Net,
        secret: &[u8; 16],
        member: EndpointId,
        hash: Hash,
        size: u64,
        providers: Vec<Provider>,
        access: [u8; 16],
    ) -> Result<Replicated, String> {
        let providers = BoundedVec::try_from(providers).map_err(|e| e.to_string())?;
        let (conn, mut send, mut recv) = open_pin(net, secret, member)
            .await
            .map_err(|e| e.to_string())?;
        let res = async {
            let order = PinMsg::Replicate {
                hash,
                size,
                providers,
                access,
                ttl_s: 0,
            };
            send_msg(&mut send, &order)
                .await
                .map_err(|e| e.to_string())?;
            match answer(&mut recv).await? {
                PinMsg::Accepted { hash: h } if h == hash => {}
                PinMsg::Done { hash: h } if h == hash => return Ok(Replicated::Done),
                PinMsg::Refused { hash: h, reason } if h == hash => {
                    return Ok(Replicated::Refused(reason));
                }
                other => return Err(format!("unexpected answer {other:?}")),
            }
            let done =
                n0_future::time::timeout(REPLICATE_TIMEOUT, recv_msg::<PinMsg>(&mut recv, MAX_MSG))
                    .await
                    .map_err(|_| "copy not done in time".to_owned())?
                    .map_err(|e| e.to_string())?;
            match done {
                PinMsg::Done { hash: h } if h == hash => Ok(Replicated::Done),
                PinMsg::Refused { hash: h, reason } if h == hash => Ok(Replicated::Refused(reason)),
                other => Err(format!("unexpected answer {other:?}")),
            }
        }
        .await;
        finish(conn, send).await;
        res
    }

    fn save_json(path: &std::path::Path, state: &DurableState) -> Result<(), StoreError> {
        use std::io::Write;
        let dir = path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        std::fs::create_dir_all(dir)?;
        let json =
            serde_json::to_vec_pretty(state).map_err(|e| StoreError::Registry(e.to_string()))?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tmp.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        tmp.write_all(&json)?;
        tmp.as_file().sync_all()?;
        tmp.persist(path).map_err(|e| StoreError::Io(e.error))?;
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Sharer side.

    /// The coordinator's answer to a replica request.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ReplicaAnswer {
        /// The coordinator tracks the blob with this many copies wanted.
        Accepted(u8),
        /// The coordinator refused.
        Refused(PinRefusal),
    }

    /// Asks the coordinator of `job` to keep `replicas` copies of `link`'s ciphertext on its
    /// store members. The key never leaves the sharer: `link` is a pin link.
    pub async fn request_replicas(
        net: &Net,
        job: &JobLink,
        link: &PinLink,
        replicas: u8,
    ) -> Result<ReplicaAnswer, StoreError> {
        let coordinator = job.coordinator;
        if !job.relay.as_str().is_empty()
            && let Err(e) = net.add_peer_relay_str(coordinator, job.relay.as_str())
        {
            tracing::debug!("ignoring the swarm relay: {e}");
        }
        let (conn, mut send, mut recv) = open_pin(net, &job.secret, coordinator).await?;
        let res = async {
            let mut accepted = 0u8;
            for _ in 0..replicas.clamp(1, super::MAX_REPLICAS) {
                let msg = PinMsg::Replicate {
                    hash: link.hash,
                    size: link.size,
                    providers: link.providers.clone(),
                    access: link.access,
                    ttl_s: 0,
                };
                send_or_refused(&conn, &mut send, &msg).await?;
                match answer(&mut recv).await {
                    Ok(PinMsg::Accepted { hash }) if hash == link.hash => accepted += 1,
                    Ok(PinMsg::Refused { reason, .. }) => {
                        return Ok(ReplicaAnswer::Refused(reason));
                    }
                    Ok(other) => {
                        return Err(StoreError::Unreachable(format!(
                            "unexpected answer {other:?}"
                        )));
                    }
                    Err(why) => {
                        // A wrong proof closes the connection with REFUSED.
                        let code = n0_future::time::timeout(Duration::from_secs(1), conn.closed())
                            .await
                            .ok()
                            .and_then(|_| nuee_net::driver::close_code_of(&conn));
                        return Err(match code {
                            Some(close_code::REFUSED | close_code::BUSY) => StoreError::Refused,
                            _ => StoreError::Unreachable(why),
                        });
                    }
                }
            }
            Ok(ReplicaAnswer::Accepted(accepted))
        }
        .await;
        finish(conn, send).await;
        res
    }

    /// Asks the coordinator of `job` to keep an erasure set: `header` is the encoded header,
    /// whose blob and shards `providers` serve (see [`crate::erasure::offer_set`]).
    /// `Accepted(1)` means the coordinator tracks the set.
    pub async fn request_erasure_set(
        net: &Net,
        job: &JobLink,
        header: &[u8],
        providers: &[Provider],
        access: [u8; 16],
    ) -> Result<ReplicaAnswer, StoreError> {
        let id = Hash::of(header);
        let msg = PinMsg::ReplicateSet {
            header: nuee_proto::BoundedBytes::try_from(header.to_vec())
                .map_err(|e| StoreError::Integrity(format!("erasure header: {e}")))?,
            providers: BoundedVec::try_from(providers.to_vec())
                .map_err(|e| StoreError::Integrity(format!("providers: {e}")))?,
            access,
        };
        let coordinator = job.coordinator;
        if !job.relay.as_str().is_empty()
            && let Err(e) = net.add_peer_relay_str(coordinator, job.relay.as_str())
        {
            tracing::debug!("ignoring the swarm relay: {e}");
        }
        let (conn, mut send, mut recv) = open_pin(net, &job.secret, coordinator).await?;
        let res = async {
            send_or_refused(&conn, &mut send, &msg).await?;
            match answer(&mut recv).await {
                Ok(PinMsg::Accepted { hash }) if hash == id => Ok(ReplicaAnswer::Accepted(1)),
                Ok(PinMsg::Refused { reason, .. }) => Ok(ReplicaAnswer::Refused(reason)),
                Ok(other) => Err(StoreError::Unreachable(format!(
                    "unexpected answer {other:?}"
                ))),
                Err(why) => {
                    let code = n0_future::time::timeout(Duration::from_secs(1), conn.closed())
                        .await
                        .ok()
                        .and_then(|_| nuee_net::driver::close_code_of(&conn));
                    Err(match code {
                        Some(close_code::REFUSED | close_code::BUSY) => StoreError::Refused,
                        _ => StoreError::Unreachable(why),
                    })
                }
            }
        }
        .await;
        finish(conn, send).await;
        res
    }

    /// What a sharer asks the coordinator to keep.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Keep {
        /// This many copies of the link's ciphertext.
        Replicas(u8),
        /// The link points to an erasure set header: these are its encoded bytes. The header
        /// and every shard are served by the link's providers with the link's access secret.
        Set(Vec<u8>),
    }

    /// Where a durable placement stands once the sharer stopped waiting.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Placement {
        /// The wanted placement is reached; these are the holders.
        Placed(Vec<Provider>),
        /// Not reached before the deadline; these are the holders so far.
        Pending(Vec<Provider>),
        /// The coordinator refused.
        Refused(PinRefusal),
    }

    impl Placement {
        /// The holders known, empty after a refusal.
        pub fn holders(&self) -> &[Provider] {
            match self {
                Placement::Placed(p) | Placement::Pending(p) => p,
                Placement::Refused(_) => &[],
            }
        }
    }

    /// The link to hand out: the holders first, then `sharer` (if it still serves) when
    /// there is room left. At most `MAX_LINK_PROVIDERS` providers; with no holder at all,
    /// the link is left as it is.
    pub fn durable_link(
        link: &FileLink,
        holders: &[Provider],
        sharer: Option<Provider>,
    ) -> FileLink {
        let mut providers: Vec<Provider> = Vec::new();
        for p in holders {
            if providers.len() < MAX_LINK_PROVIDERS && !providers.iter().any(|q| q.id == p.id) {
                providers.push(p.clone());
            }
        }
        if providers.is_empty() {
            return link.clone();
        }
        if let Some(me) = sharer
            && providers.len() < MAX_LINK_PROVIDERS
            && !providers.iter().any(|q| q.id == me.id)
        {
            providers.push(me);
        }
        let mut out = link.clone();
        out.providers = BoundedVec::try_from(providers).unwrap_or_else(|_| link.providers.clone());
        out
    }

    fn connect_coordinator(net: &Net, job: &JobLink) {
        if !job.relay.as_str().is_empty()
            && let Err(e) = net.add_peer_relay_str(job.coordinator, job.relay.as_str())
        {
            tracing::debug!("ignoring the swarm relay: {e}");
        }
    }

    async fn refused_or_lost(conn: &Connection, why: String) -> StoreError {
        // A wrong proof closes the connection with REFUSED.
        let code = n0_future::time::timeout(Duration::from_secs(1), conn.closed())
            .await
            .ok()
            .and_then(|_| nuee_net::driver::close_code_of(conn));
        match code {
            Some(close_code::REFUSED | close_code::BUSY) => StoreError::Refused,
            _ => StoreError::Unreachable(why),
        }
    }

    /// Asks the coordinator of `job` to keep `link` (copies or an erasure set, see [`Keep`]),
    /// then waits up to `wait` for the placement, calling `progress(elapsed)` about once a
    /// second. Returns the placement and the link to hand out (see [`durable_link`]), with
    /// `sharer` last when given. Only the pin part of `link` is sent: the key stays here.
    pub async fn keep_durably<P: FnMut(Duration)>(
        net: &Net,
        job: &JobLink,
        link: &FileLink,
        keep: Keep,
        wait: Duration,
        sharer: Option<Provider>,
        mut progress: P,
    ) -> Result<(Placement, FileLink), StoreError> {
        let (id, requests) = match &keep {
            Keep::Replicas(n) => {
                let msg = PinMsg::Replicate {
                    hash: link.hash,
                    size: link.size,
                    providers: link.providers.clone(),
                    access: link.access,
                    ttl_s: 0,
                };
                (
                    link.hash,
                    vec![msg; usize::from((*n).clamp(1, super::MAX_REPLICAS))],
                )
            }
            Keep::Set(header) => {
                let msg = PinMsg::ReplicateSet {
                    header: nuee_proto::BoundedBytes::try_from(header.clone())
                        .map_err(|e| StoreError::Integrity(format!("erasure header: {e}")))?,
                    providers: link.providers.clone(),
                    access: link.access,
                };
                (Hash::of(header), vec![msg])
            }
        };
        connect_coordinator(net, job);
        let (conn, mut send, mut recv) = open_pin(net, &job.secret, job.coordinator).await?;
        // Frames are read by their own task so that a progress tick never cuts one in half.
        let (frames, mut incoming) = mpsc::channel::<Result<PinMsg, String>>(4);
        let _reader = AbortOnDropHandle::new(n0_future::task::spawn(async move {
            loop {
                let msg = recv_msg::<PinMsg>(&mut recv, MAX_MSG)
                    .await
                    .map_err(|e| e.to_string());
                let end = msg.is_err();
                if frames.send(msg).await.is_err() || end {
                    break;
                }
            }
        }));
        let res = async {
            for msg in &requests {
                send_or_refused(&conn, &mut send, msg).await?;
                let reply = n0_future::time::timeout(ANSWER_TIMEOUT, incoming.recv())
                    .await
                    .map_err(|_| StoreError::Unreachable("no answer in time".into()))?
                    .unwrap_or_else(|| Err("session closed".into()));
                match reply {
                    Ok(PinMsg::Accepted { hash }) if hash == id => {}
                    Ok(PinMsg::Refused { reason, .. }) => return Ok(Placement::Refused(reason)),
                    Ok(other) => {
                        return Err(StoreError::Unreachable(format!(
                            "unexpected answer {other:?}"
                        )));
                    }
                    Err(why) => return Err(refused_or_lost(&conn, why).await),
                }
            }
            let start = n0_future::time::Instant::now();
            let mut asked = false;
            loop {
                let elapsed = start.elapsed();
                if elapsed >= wait && !asked {
                    send_or_refused(&conn, &mut send, &PinMsg::Where { hash: id }).await?;
                    asked = true;
                }
                let tick = if asked {
                    ANSWER_TIMEOUT
                } else {
                    (wait - elapsed).min(Duration::from_secs(1))
                };
                match n0_future::time::timeout(tick, incoming.recv()).await {
                    Err(_) if asked => {
                        return Err(StoreError::Unreachable("no answer in time".into()));
                    }
                    Err(_) => progress(start.elapsed()),
                    Ok(None) => return Err(StoreError::Unreachable("session closed".into())),
                    Ok(Some(Err(why))) => return Err(StoreError::Unreachable(why)),
                    Ok(Some(Ok(PinMsg::Holders { hash, providers }))) if hash == id => {
                        let providers = providers.into_inner();
                        // Before our `Where`, a `Holders` is the push of a reached placement.
                        return Ok(if asked {
                            Placement::Pending(providers)
                        } else {
                            Placement::Placed(providers)
                        });
                    }
                    Ok(Some(Ok(other))) => {
                        return Err(StoreError::Unreachable(format!(
                            "unexpected message {other:?}"
                        )));
                    }
                }
            }
        }
        .await;
        finish(conn, send).await;
        let placement = res?;
        let out = durable_link(link, placement.holders(), sharer);
        Ok((placement, out))
    }

    /// Asks the coordinator of `job` who holds `hash` (a blob or an erasure set) now.
    pub async fn where_is(
        net: &Net,
        job: &JobLink,
        hash: Hash,
    ) -> Result<Vec<Provider>, StoreError> {
        connect_coordinator(net, job);
        let (conn, mut send, mut recv) = open_pin(net, &job.secret, job.coordinator).await?;
        let res = async {
            send_or_refused(&conn, &mut send, &PinMsg::Where { hash }).await?;
            match answer(&mut recv).await {
                Ok(PinMsg::Holders { hash: h, providers }) if h == hash => {
                    Ok(providers.into_inner())
                }
                Ok(other) => Err(StoreError::Unreachable(format!(
                    "unexpected answer {other:?}"
                ))),
                Err(why) => Err(refused_or_lost(&conn, why).await),
            }
        }
        .await;
        finish(conn, send).await;
        res
    }

    async fn pin_exchange(net: &Net, job: &JobLink, msg: PinMsg) -> Result<PinMsg, StoreError> {
        connect_coordinator(net, job);
        let (conn, mut send, mut recv) = open_pin(net, &job.secret, job.coordinator).await?;
        let res = async {
            send_or_refused(&conn, &mut send, &msg).await?;
            match answer(&mut recv).await {
                Ok(reply) => Ok(reply),
                Err(why) => Err(refused_or_lost(&conn, why).await),
            }
        }
        .await;
        finish(conn, send).await;
        res
    }

    /// The blobs this node asked the coordinator of `job` to keep (`List`).
    pub async fn list_pins(net: &Net, job: &JobLink) -> Result<Vec<PinInfo>, StoreError> {
        match pin_exchange(net, job, PinMsg::List).await? {
            PinMsg::Pins { items } => Ok(items.into_inner()),
            other => Err(StoreError::Unreachable(format!(
                "unexpected answer {other:?}"
            ))),
        }
    }

    /// Asks the coordinator of `job` to stop keeping `hash` (a blob or an erasure set this
    /// node asked for). `Ok(false)`: the coordinator refused (someone else asked for it, or
    /// it is not tracked).
    pub async fn release_pin(net: &Net, job: &JobLink, hash: Hash) -> Result<bool, StoreError> {
        match pin_exchange(net, job, PinMsg::Release { hash }).await? {
            PinMsg::Pins { .. } => Ok(true),
            PinMsg::Refused { hash: h, .. } if h == hash => Ok(false),
            other => Err(StoreError::Unreachable(format!(
                "unexpected answer {other:?}"
            ))),
        }
    }

    /// `link` with its providers replaced by the current holders known to the coordinator
    /// of `job`; `Err(NoProvider)` if it knows none.
    pub async fn refresh_link(
        net: &Net,
        job: &JobLink,
        link: &FileLink,
    ) -> Result<FileLink, StoreError> {
        let holders = where_is(net, job, link.hash).await?;
        if holders.is_empty() {
            return Err(StoreError::NoProvider);
        }
        Ok(durable_link(link, &holders, None))
    }

    /// What `nuee get <link> [--on <swarm>]` does: [`crate::get`] with the link's providers,
    /// and when none of them delivers and `swarm` is given, once more with the holders its
    /// coordinator knows now. Returns the report and whether the coordinator was asked.
    pub async fn get_durable<P: FnMut(u64, u64)>(
        net: &Net,
        hub: &StoreHub,
        link: &FileLink,
        swarm: Option<&JobLink>,
        dest: &std::path::Path,
        overwrite: bool,
        mut progress: P,
    ) -> Result<(crate::GetReport, bool), StoreError> {
        let first = crate::get(net, hub, link, dest, overwrite, &mut progress).await;
        let Some(job) = swarm else {
            return first.map(|r| (r, false));
        };
        match first {
            Ok(r) => Ok((r, false)),
            // `Integrity` too: a provider of a set that holds only shards names another blob
            // of the secret and cannot serve the header. Nothing unverified was kept, and
            // the coordinator's current holders are checked the same way.
            Err(
                StoreError::Unreachable(_)
                | StoreError::NoProvider
                | StoreError::Refused
                | StoreError::Integrity(_),
            ) => {
                let fresh = refresh_link(net, job, link).await?;
                let report = crate::get(net, hub, &fresh, dest, overwrite, &mut progress).await?;
                Ok((report, true))
            }
            Err(e) => Err(e),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Member side.

    #[derive(Debug)]
    struct MemberInner {
        net: Net,
        hub: StoreHub,
        coordinator: EndpointId,
        secret: [u8; 16],
        quota: u64,
        registry: Mutex<Registry>,
        acceptor: FilesAcceptor,
        admission: Admission,
        inflight: Mutex<HashMap<Hash, u64>>,
        /// Copies a release took out of the registry and has not counted as reclaiming yet:
        /// they still count as used, so no order is admitted in between.
        releasing: Mutex<HashMap<Hash, u64>>,
        /// Released or failed copies still on disk until garbage collection removes them;
        /// they count against the quota until then.
        reclaiming: Mutex<HashMap<Hash, u64>>,
    }

    /// A store member: accepts replication orders from the swarm coordinator over
    /// `nuee/pin/0`, within `quota` bytes, and serves the copies it holds to holders of the
    /// file link on `nuee/job/0`.
    #[derive(Debug, Clone)]
    pub struct StoreMember {
        inner: Arc<MemberInner>,
    }

    impl StoreMember {
        /// A member of the swarm of `job` offering `quota` bytes, with the copies recorded in
        /// `registry` (normally `<data dir>/stored.json`).
        pub fn new(
            hub: &StoreHub,
            job: &JobLink,
            quota: u64,
            registry: Registry,
        ) -> Result<StoreMember, StoreError> {
            let offers = registry
                .list()
                .iter()
                .map(PinEntry::offer)
                .collect::<Result<Vec<_>, _>>()?;
            for o in &offers {
                hub.gate().admit_for(job.coordinator, o.hash);
            }
            // Copies recorded before `stored_tag` existed were kept by the pin and share
            // tags; give them their own tag so a later release leaves those tags alone. The
            // old tags stay: they cannot be told apart from the node's own pins and shares.
            let legacy: Vec<Hash> = offers.iter().map(|o| o.hash).collect();
            if !legacy.is_empty() {
                let tags = hub.clone();
                n0_future::task::spawn(async move {
                    for hash in legacy {
                        if let Err(e) = tags.tag(&stored_tag(hash), hash).await {
                            tracing::warn!(%hash, "cannot tag a stored copy: {e}");
                        }
                    }
                });
            }
            let acceptor = FilesAcceptor::new(hub, "store", offers);
            Ok(StoreMember {
                inner: Arc::new(MemberInner {
                    net: hub.net().clone(),
                    hub: hub.clone(),
                    coordinator: job.coordinator,
                    secret: job.secret,
                    quota,
                    registry: Mutex::new(registry),
                    acceptor,
                    admission: Admission::new(AdmitConfig::default()),
                    inflight: Mutex::new(HashMap::new()),
                    releasing: Mutex::new(HashMap::new()),
                    reclaiming: Mutex::new(HashMap::new()),
                }),
            })
        }

        /// `<data dir>/stored.json`.
        pub fn default_registry(data_dir: &std::path::Path) -> PathBuf {
            data_dir.join("stored.json")
        }

        /// The `nuee/job/0` handler serving the copies.
        pub fn acceptor(&self) -> FilesAcceptor {
            self.inner.acceptor.clone()
        }

        /// Serves `nuee/pin/0`, `nuee/job/0` (copies) and the blobs protocol on the hub's
        /// endpoint.
        pub fn spawn(&self) -> Router {
            self.inner
                .net
                .router()
                .accept(ALPN_PIN, self.clone())
                .accept(ALPN_JOB, self.acceptor())
                .accept(iroh_blobs::ALPN, self.inner.hub.protocol())
                .spawn()
        }

        /// Bytes used by the copies held, and by those a release is taking out.
        pub fn used(&self) -> u64 {
            // The registry first: a release adds to `releasing` before it lets go of the
            // registry lock, so a copy on its way out is always seen in one or the other.
            let held = lock(&self.inner.registry)
                .list()
                .iter()
                .map(|e| e.size)
                .fold(0u64, u64::saturating_add);
            let releasing = lock(&self.inner.releasing)
                .values()
                .fold(0u64, |a, b| a.saturating_add(*b));
            held.saturating_add(releasing)
        }

        /// Hashes of the copies held.
        pub fn held(&self) -> Vec<Hash> {
            lock(&self.inner.registry)
                .list()
                .iter()
                .filter_map(|e| e.hash().ok())
                .collect()
        }

        fn holds(&self, hash: &Hash) -> bool {
            self.held().contains(hash)
        }

        async fn pins(&self) -> BoundedVec<PinInfo, 256> {
            let entries: Vec<(Hash, u64)> = lock(&self.inner.registry)
                .list()
                .iter()
                .filter_map(|e| e.hash().ok().map(|h| (h, e.size)))
                .take(256)
                .collect();
            let mut items = Vec::with_capacity(entries.len());
            for (hash, size) in entries {
                let complete = match self.inner.hub.complete_size(hash).await {
                    Ok(Some(s)) => s == size,
                    Ok(None) => false,
                    Err(e) => {
                        tracing::warn!(%hash, "cannot read the store: {e}");
                        false
                    }
                };
                items.push(PinInfo {
                    hash,
                    size,
                    complete,
                });
            }
            BoundedVec::try_from(items).unwrap_or_default()
        }

        /// Bytes of released or failed copies not reclaimed yet. A copy the node still
        /// keeps under another tag (its own pin or share) is never collected, so it is not
        /// counted.
        pub async fn reclaiming(&self) -> Result<u64, StoreError> {
            let pending: Vec<Hash> = lock(&self.inner.reclaiming).keys().copied().collect();
            for hash in pending {
                let hub = &self.inner.hub;
                if !hub.present(hash).await? || hub.tagged(hash).await? {
                    lock(&self.inner.reclaiming).remove(&hash);
                }
            }
            Ok(lock(&self.inner.reclaiming)
                .values()
                .fold(0u64, |a, b| a.saturating_add(*b)))
        }

        /// A copy is dropped or failed: its bytes count until the store no longer has them.
        fn reclaim(&self, hash: Hash, size: u64) {
            lock(&self.inner.reclaiming).insert(hash, size);
        }

        /// Checks an order against the limits and reserves room for it: copies held,
        /// copies being fetched and copies not reclaimed yet all count against the quota.
        async fn admit_order(&self, hash: Hash, size: u64) -> Result<(), PinRefusal> {
            if self.inner.hub.check_size(size).is_err() {
                return Err(PinRefusal::TooLarge);
            }
            // Used first, then reclaiming: a release moves a copy from the one to the other
            // in that order, so it is counted at least once, never zero times.
            let held = self.used();
            let reclaiming = match self.reclaiming().await {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(%hash, "cannot read the store: {e}");
                    return Err(PinRefusal::Busy);
                }
            };
            let used = held.saturating_add(reclaiming);
            let mut inflight = lock(&self.inner.inflight);
            if inflight.contains_key(&hash) || inflight.len() >= MAX_PARALLEL_FETCHES {
                return Err(PinRefusal::Busy);
            }
            let reserved = inflight.values().fold(0u64, |a, b| a.saturating_add(*b));
            if used.saturating_add(reserved).saturating_add(size) > self.inner.quota {
                return Err(PinRefusal::Quota);
            }
            inflight.insert(hash, size);
            Ok(())
        }

        async fn store(&self, link: &PinLink) -> Result<(), StoreError> {
            let inner = &self.inner;
            // The copy is fetched under the swarm's own tag only: the node's shares and
            // pins of the same blob, even ones made while the fetch runs, are left alone.
            crate::share::fetch_ciphertext_as(
                &inner.net,
                &inner.hub,
                link.providers.as_slice(),
                &link.access,
                nuee_proto::FileInfo {
                    hash: link.hash,
                    size: link.size,
                },
                &stored_tag(link.hash),
                &mut |_, _| {},
            )
            .await?;
            {
                let mut reg = lock(&inner.registry);
                reg.add(PinEntry::from_link(link, now_ms()));
                reg.save()?;
            }
            inner.acceptor.add(Offer {
                access: link.access,
                hash: link.hash,
                size: link.size,
            });
            inner.hub.gate().admit_for(inner.coordinator, link.hash);
            Ok(())
        }

        async fn release(&self, hash: Hash) -> Result<(), StoreError> {
            let inner = &self.inner;
            let removed = {
                let mut reg = lock(&inner.registry);
                let removed = reg.remove(&hash);
                if let Some(entry) = &removed {
                    reg.save()?;
                    // Before the registry lock goes: the copy keeps counting as used.
                    lock(&inner.releasing).insert(hash, entry.size);
                }
                removed
            };
            if let Some(entry) = removed {
                inner.acceptor.remove(&hash);
                let collectable = async {
                    inner.hub.untag(&stored_tag(hash)).await?;
                    Ok::<_, StoreError>(!inner.hub.tagged(hash).await?)
                }
                .await;
                // Only what garbage collection can free counts as reclaiming; after an error
                // the copy counts there too, until reclaiming() finds it gone.
                if !matches!(collectable, Ok(false)) {
                    self.reclaim(hash, entry.size);
                }
                lock(&inner.releasing).remove(&hash);
                collectable?;
            }
            Ok(())
        }

        async fn handle(&self, conn: Connection) {
            let inner = &self.inner;
            let peer = conn.remote_id();
            let Some((mut send, mut recv)) =
                accept_hello(&inner.admission, &conn, &inner.secret, inner.net.id()).await
            else {
                return;
            };
            let from_coordinator = peer == inner.coordinator;
            if from_coordinator {
                for h in self.held() {
                    inner.hub.gate().admit_for(peer, h);
                }
            }
            loop {
                let msg = match recv_msg::<PinMsg>(&mut recv, MAX_MSG).await {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::debug!(%peer, "pin session ended: {e}");
                        break;
                    }
                };
                let reply = match msg {
                    PinMsg::Replicate { hash, .. } | PinMsg::Release { hash }
                        if !from_coordinator =>
                    {
                        PinMsg::Refused {
                            hash,
                            reason: PinRefusal::NotMember,
                        }
                    }
                    // What a member holds is the coordinator's business only.
                    PinMsg::List if !from_coordinator => PinMsg::Pins {
                        items: BoundedVec::new(),
                    },
                    PinMsg::Replicate {
                        hash,
                        size,
                        mut providers,
                        access,
                        ..
                    } => {
                        crate::share::drop_refused_relays(&inner.net, providers.as_mut_slice());
                        if self.holds(&hash) {
                            PinMsg::Done { hash }
                        } else if let Err(reason) = self.admit_order(hash, size).await {
                            PinMsg::Refused { hash, reason }
                        } else {
                            let link = PinLink {
                                providers,
                                hash,
                                size,
                                access,
                            };
                            let accepted = send_msg(&mut send, &PinMsg::Accepted { hash }).await;
                            let stored = match accepted {
                                Ok(()) => self.store(&link).await,
                                Err(e) => Err(StoreError::Unreachable(e.to_string())),
                            };
                            if stored.is_err() {
                                // Whatever arrived stays on disk until it is reclaimed.
                                self.reclaim(hash, size);
                            }
                            lock(&inner.inflight).remove(&hash);
                            match stored {
                                Ok(()) => PinMsg::Done { hash },
                                Err(e) => {
                                    tracing::warn!(%hash, "copy failed: {e}");
                                    break;
                                }
                            }
                        }
                    }
                    PinMsg::Release { hash } => {
                        if let Err(e) = self.release(hash).await {
                            tracing::warn!(%hash, "release failed: {e}");
                            break;
                        }
                        PinMsg::Pins {
                            items: self.pins().await,
                        }
                    }
                    PinMsg::List => PinMsg::Pins {
                        items: self.pins().await,
                    },
                    other => {
                        tracing::debug!(%peer, "unexpected pin message: {other:?}");
                        break;
                    }
                };
                if let Err(e) = send_msg(&mut send, &reply).await {
                    tracing::debug!(%peer, "pin answer not delivered: {e}");
                    break;
                }
            }
            // An answer sent just before the session ends (the Accepted of a copy that then
            // failed) still reaches the coordinator.
            finish(conn, send).await;
        }
    }

    impl ProtocolHandler for StoreMember {
        async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
            self.handle(connection).await;
            Ok(())
        }
    }
}
