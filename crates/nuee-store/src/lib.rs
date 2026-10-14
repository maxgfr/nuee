//! Encrypted file sharing over iroh-blobs.
//!
//! A share is a tar archive encrypted with age (X25519) and stored as one content-addressed
//! blob. Only peers that proved knowledge of the link's access secret on `nuee/job/0` may
//! talk the blobs protocol with us, and only for the hashes they were admitted for; pushes
//! are always refused.
#![forbid(unsafe_code)]

pub mod challenge;
pub mod crypt;
pub mod durable;
pub mod erasure;
pub mod gate;
pub mod pin;
pub mod share;

use std::path::PathBuf;

pub use crypt::{
    AgeKey, decrypt, decrypt_bytes, encrypt, encrypt_bytes, pack_bytes, safe_components,
    unpack_bytes,
};
#[cfg(not(target_family = "wasm"))]
pub use crypt::{Packed, pack, unpack};
pub use gate::{CONNECT_TIMEOUT, Gate, HubConfig, OutputCappedHub, StoreHub, blob_tag};
pub use pin::{PinEntry, PinProvider, Registry, pin_tag, serve_pins, with_provider};
#[cfg(not(target_family = "wasm"))]
pub use pin::{pin, unpin_local};
pub use share::{
    FilesAcceptor, Offer, ServeHandle, ShareHandle, get_bytes, self_provider, share_bytes,
};
#[cfg(not(target_family = "wasm"))]
pub use share::{GetReport, get, share_paths};

/// Largest blob a native node stores or fetches by default.
pub const MAX_BLOB_NATIVE: u64 = 16 * 1024 * 1024 * 1024;
/// Total size of the in-memory store used in browsers.
pub const MEM_STORE_CAP: u64 = 256 * 1024 * 1024;

/// What can go wrong while sharing, fetching or pinning.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Every provider refused our access proof, or the blob request.
    #[error("access refused by the provider")]
    Refused,
    /// No provider could be reached.
    #[error("provider unreachable: {0}")]
    Unreachable(String),
    /// The data does not match the link (hash, size or announced file).
    #[error("integrity check failed: {0}")]
    Integrity(String),
    /// The content cannot be decrypted with the link's key.
    #[error("cannot decrypt: {0}")]
    Decrypt(String),
    /// The blob is above the allowed size.
    #[error("{size} bytes exceed the limit of {max} bytes")]
    TooLarge {
        /// Requested size.
        size: u64,
        /// Allowed maximum.
        max: u64,
    },
    /// An archive entry is unsafe (absolute, `..`, link, device) or a name is invalid.
    #[error("unsafe path in archive: {0}")]
    UnsafePath(String),
    /// A file already exists and overwriting was not asked for.
    #[error("{} already exists (use overwrite)", .0.display())]
    Exists(PathBuf),
    /// The link has no provider.
    #[error("the link lists no provider")]
    NoProvider,
    /// The blob store failed.
    #[error("blob store: {0}")]
    Store(String),
    /// The registry file is invalid.
    #[error("pin registry: {0}")]
    Registry(String),
    /// No data directory could be determined.
    #[error("no data directory: set NUEE_DATA_DIR or HOME")]
    NoDataDir,
    /// The system random source failed.
    #[error(transparent)]
    Random(#[from] nuee_proto::RandomError),
    /// Local file system failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The data directory: `$NUEE_DATA_DIR`, else `$XDG_DATA_HOME/nuee`, else
/// `~/.local/share/nuee` (macOS included). Empty variables count as unset.
#[cfg(not(target_family = "wasm"))]
pub fn data_dir() -> Result<PathBuf, StoreError> {
    resolve_data_dir(
        std::env::var_os("NUEE_DATA_DIR"),
        std::env::var_os("XDG_DATA_HOME"),
        std::env::var_os("HOME"),
    )
    .ok_or(StoreError::NoDataDir)
}

/// The resolution rule behind [`data_dir`], on explicit values.
pub fn resolve_data_dir(
    nuee: Option<std::ffi::OsString>,
    xdg: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    let set = |v: Option<std::ffi::OsString>| v.filter(|s| !s.is_empty()).map(PathBuf::from);
    if let Some(dir) = set(nuee) {
        return Some(dir);
    }
    if let Some(dir) = set(xdg) {
        return Some(dir.join("nuee"));
    }
    set(home).map(|h| h.join(".local").join("share").join("nuee"))
}

/// Creates `dir` (and parents) and restricts it to its owner (0700 on Unix).
#[cfg(not(target_family = "wasm"))]
pub fn ensure_private_dir(dir: &std::path::Path) -> Result<(), StoreError> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(bytes)
}
