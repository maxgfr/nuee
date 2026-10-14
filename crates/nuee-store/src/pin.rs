//! Pinning: keep someone else's ciphertext and serve it to holders of the link.
//!
//! A pinner never sees the key: it stores the encrypted blob, remembers the access secret
//! in `<data dir>/pins.json`, and answers on `nuee/job/0` to anyone who proves knowledge of
//! one of the pinned items' access secret.

use nuee_net::Net;
use nuee_proto::limits::MAX_LINK_PROVIDERS;
use nuee_proto::{BoundedString, BoundedVec, Hash, PinLink, Provider};
use serde::{Deserialize, Serialize};

use crate::StoreError;
use crate::gate::StoreHub;
use crate::share::{FilesAcceptor, Offer, ServeHandle};

/// Name of the tag that keeps a pinned blob alive.
pub fn pin_tag(hash: Hash) -> String {
    format!("nuee/pin/{hash}")
}

/// Drops the tag [`pin`] put on `hash` in the local store under `data_dir`, so garbage
/// collection can reclaim the bytes. No network: only the on-disk store is opened. A data
/// directory without a store has no tag to drop.
#[cfg(not(target_family = "wasm"))]
pub async fn unpin_local(data_dir: &std::path::Path, hash: Hash) -> Result<(), StoreError> {
    let blobs = data_dir.join("blobs");
    if !blobs.is_dir() {
        return Ok(());
    }
    let fs = iroh_blobs::store::fs::FsStore::load(&blobs)
        .await
        .map_err(|e| StoreError::Store(e.to_string()))?;
    let res = fs
        .tags()
        .delete(pin_tag(hash))
        .await
        .map(|_| ())
        .map_err(|e| StoreError::Store(e.to_string()));
    let closed = fs
        .shutdown()
        .await
        .map_err(|e| StoreError::Store(e.to_string()));
    res.and(closed)
}

/// A provider as stored in the registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinProvider {
    /// Endpoint id.
    pub id: String,
    /// Relay URL.
    pub relay: Option<String>,
}

/// One pinned ciphertext.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinEntry {
    /// Ciphertext hash (hex).
    pub hash: String,
    /// Ciphertext size.
    pub size: u64,
    /// Access secret (hex).
    pub access_hex: String,
    /// Providers known when it was pinned.
    pub providers: Vec<PinProvider>,
    /// When it was pinned (ms since the Unix epoch).
    pub added_ms: u64,
}

fn bad(what: &str) -> StoreError {
    StoreError::Registry(format!("invalid {what}"))
}

impl PinEntry {
    /// The entry recording `link`.
    pub fn from_link(link: &PinLink, added_ms: u64) -> PinEntry {
        PinEntry {
            hash: link.hash.to_string(),
            size: link.size,
            access_hex: crate::hex(&link.access),
            providers: link
                .providers
                .as_slice()
                .iter()
                .map(|p| PinProvider {
                    id: p.id.to_string(),
                    relay: p.relay.as_ref().map(|r| r.as_str().to_owned()),
                })
                .collect(),
            added_ms,
        }
    }

    /// The ciphertext hash.
    pub fn hash(&self) -> Result<Hash, StoreError> {
        self.hash.parse().map_err(|_| bad("hash"))
    }

    /// The access secret.
    pub fn access(&self) -> Result<[u8; 16], StoreError> {
        let bytes = data_encoding::HEXLOWER_PERMISSIVE
            .decode(self.access_hex.as_bytes())
            .map_err(|_| bad("access secret"))?;
        bytes.try_into().map_err(|_| bad("access secret"))
    }

    /// What the pin server offers for this entry.
    pub fn offer(&self) -> Result<Offer, StoreError> {
        Ok(Offer {
            access: self.access()?,
            hash: self.hash()?,
            size: self.size,
        })
    }

    /// The pin link this entry was made from.
    pub fn link(&self) -> Result<PinLink, StoreError> {
        let mut providers = BoundedVec::new();
        for p in self.providers.iter().take(MAX_LINK_PROVIDERS) {
            let relay = match &p.relay {
                Some(r) => Some(BoundedString::try_from(r.as_str()).map_err(|_| bad("relay"))?),
                None => None,
            };
            providers
                .push(Provider {
                    id: p.id.parse().map_err(|_| bad("provider id"))?,
                    relay,
                })
                .map_err(|_| bad("provider list"))?;
        }
        Ok(PinLink {
            providers,
            hash: self.hash()?,
            size: self.size,
            access: self.access()?,
        })
    }
}

/// The list of pinned items, optionally backed by a JSON file.
#[derive(Debug, Default, Clone)]
pub struct Registry {
    path: Option<std::path::PathBuf>,
    items: Vec<PinEntry>,
}

impl Registry {
    /// An empty registry that is never written to disk.
    pub fn in_memory() -> Registry {
        Registry::default()
    }

    /// `<data dir>/pins.json`.
    pub fn default_path(data_dir: &std::path::Path) -> std::path::PathBuf {
        data_dir.join("pins.json")
    }

    /// Reads the registry at `path`; a missing file is an empty registry.
    #[cfg(not(target_family = "wasm"))]
    pub fn load(path: &std::path::Path) -> Result<Registry, StoreError> {
        let items = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| StoreError::Registry(format!("{}: {e}", path.display())))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Registry {
            path: Some(path.to_path_buf()),
            items,
        })
    }

    /// Writes the registry atomically (temporary file, then rename), readable by its owner
    /// only. Does nothing for an in-memory registry.
    #[cfg(not(target_family = "wasm"))]
    pub fn save(&self) -> Result<(), StoreError> {
        use std::io::Write;
        let Some(path) = &self.path else {
            return Ok(());
        };
        let dir = path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        std::fs::create_dir_all(dir)?;
        let json = serde_json::to_vec_pretty(&self.items)
            .map_err(|e| StoreError::Registry(e.to_string()))?;
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

    /// Adds `entry`, replacing an entry for the same hash.
    pub fn add(&mut self, entry: PinEntry) {
        self.items.retain(|e| e.hash != entry.hash);
        self.items.push(entry);
    }

    /// Removes the entry for `hash`.
    pub fn remove(&mut self, hash: &Hash) -> Option<PinEntry> {
        let key = hash.to_string();
        let pos = self.items.iter().position(|e| e.hash == key)?;
        Some(self.items.remove(pos))
    }

    /// Every entry, oldest first.
    pub fn list(&self) -> &[PinEntry] {
        &self.items
    }

    /// The backing file.
    pub fn path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
    }
}

/// `link` with `me` as a provider: the first three others are kept, then `me`.
pub fn with_provider(link: &PinLink, me: Provider) -> PinLink {
    let mut providers = BoundedVec::new();
    let me_id = me.id;
    let keep = link
        .providers
        .as_slice()
        .iter()
        .filter(|p| p.id != me_id)
        .take(MAX_LINK_PROVIDERS - 1)
        .cloned()
        .chain(std::iter::once(me));
    for p in keep {
        if providers.push(p).is_err() {
            break;
        }
    }
    PinLink {
        providers,
        hash: link.hash,
        size: link.size,
        access: link.access,
    }
}

/// Fetches the ciphertext of `link` (a full file link works too: call
/// `FileLink::pin_link` first, so the key is never handed over), keeps it under a
/// persistent tag, records it in `registry` (saved at once) and returns the link with this
/// node added as a provider.
#[cfg(not(target_family = "wasm"))]
pub async fn pin<P: FnMut(u64, u64)>(
    net: &Net,
    hub: &StoreHub,
    link: &PinLink,
    registry: &mut Registry,
    mut progress: P,
) -> Result<PinLink, StoreError> {
    crate::share::fetch_ciphertext(
        net,
        hub,
        link.providers.as_slice(),
        &link.access,
        link.hash,
        link.size,
        &mut progress,
    )
    .await?;
    hub.tag(&pin_tag(link.hash), link.hash).await?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| StoreError::Registry(format!("system clock before 1970: {e}")))?;
    let now = u64::try_from(now.as_millis()).unwrap_or(u64::MAX);
    registry.add(PinEntry::from_link(link, now));
    registry.save()?;
    Ok(with_provider(link, crate::share::self_provider(net)))
}

/// Serves every pinned item on `net`: one `nuee/job/0` acceptor that admits a peer whose
/// proof matches any pinned item's access secret, for that item's hash only.
pub fn serve_pins(
    net: &Net,
    hub: &StoreHub,
    registry: &Registry,
) -> Result<ServeHandle, StoreError> {
    let offers = registry
        .list()
        .iter()
        .map(PinEntry::offer)
        .collect::<Result<Vec<_>, _>>()?;
    let acceptor = FilesAcceptor::new(hub, "pins", offers);
    Ok(ServeHandle::spawn(net, hub, acceptor))
}
