//! Identity key on disk: `<config dir>/secret.key`, 32 raw bytes, owner-only.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use nuee_proto::SecretKey;

use crate::{exit, fail};

/// File name of the identity key.
pub const KEY_FILE: &str = "secret.key";

/// `--config-dir` / `$NUEE_CONFIG_DIR`, else `$XDG_CONFIG_HOME/nuee`, else `~/.config/nuee`
/// (macOS included). Empty values count as unset.
pub fn config_dir(flag: Option<&Path>) -> anyhow::Result<PathBuf> {
    resolve(
        flag.map(|p| p.as_os_str().to_owned()),
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
    .ok_or_else(|| {
        fail(
            exit::ERROR,
            "no config directory: set NUEE_CONFIG_DIR or HOME, or pass --ephemeral",
        )
    })
}

fn resolve(
    explicit: Option<std::ffi::OsString>,
    xdg: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    let set = |v: Option<std::ffi::OsString>| v.filter(|s| !s.is_empty()).map(PathBuf::from);
    set(explicit)
        .or_else(|| set(xdg).map(|x| x.join("nuee")))
        .or_else(|| set(home).map(|h| h.join(".config").join("nuee")))
}

/// A new random key, never written anywhere.
pub fn fresh_key() -> anyhow::Result<SecretKey> {
    let a = nuee_proto::new_secret16().context("system random source")?;
    let b = nuee_proto::new_secret16().context("system random source")?;
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(&a);
    bytes[16..].copy_from_slice(&b);
    Ok(SecretKey::from_bytes(&bytes))
}

/// Loads the key of `dir`, creating the directory (0700) and the key (0600, exclusive
/// create) on first use. Refuses a key or directory others can read.
pub fn load_or_create(dir: &Path) -> anyhow::Result<SecretKey> {
    let path = dir.join(KEY_FILE);
    if path.exists() {
        return load(dir);
    }
    create(dir, false)
}

/// Loads an existing key, checking permissions.
pub fn load(dir: &Path) -> anyhow::Result<SecretKey> {
    check_private(dir, 0o077, "chmod 700")?;
    let path = dir.join(KEY_FILE);
    check_private(&path, 0o077, "chmod 600")?;
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let bytes: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
        fail(
            exit::ERROR,
            format!(
                "{} is not a nuee key (expected 32 bytes, found {}); move it away to create a new one",
                path.display(),
                bytes.len()
            ),
        )
    })?;
    Ok(SecretKey::from_bytes(&bytes))
}

/// Writes a new key. With `replace`, an existing key is replaced atomically.
pub fn create(dir: &Path, replace: bool) -> anyhow::Result<SecretKey> {
    make_private_dir(dir)?;
    check_private(dir, 0o077, "chmod 700")?;
    let key = fresh_key()?;
    let path = dir.join(KEY_FILE);
    if replace {
        let tmp = dir.join(format!("{KEY_FILE}.new"));
        if tmp.exists() {
            std::fs::remove_file(&tmp).with_context(|| format!("removing {}", tmp.display()))?;
        }
        write_exclusive(&tmp, &key.to_bytes())?;
        std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
    } else {
        write_exclusive(&path, &key.to_bytes())?;
    }
    Ok(key)
}

fn make_private_dir(dir: &Path) -> anyhow::Result<()> {
    if dir.exists() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting {}", dir.display()))?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(())
}

fn write_exclusive(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    f.write_all(bytes)
        .and_then(|()| f.sync_all())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn check_private(path: &Path, forbidden: u32, fix: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    let mode = meta.permissions().mode() & 0o777;
    if mode & forbidden != 0 {
        return Err(fail(
            exit::ERROR,
            format!(
                "{} is readable by other users (mode {mode:o}); run `{fix} {}`",
                path.display(),
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private(_path: &Path, _forbidden: u32, _fix: &str) -> anyhow::Result<()> {
    Ok(())
}

/// Permission report for `nuee doctor`: `Ok(None)` when no key exists yet.
pub fn check_key(dir: &Path) -> anyhow::Result<Option<()>> {
    if !dir.join(KEY_FILE).exists() {
        return Ok(None);
    }
    load(dir).map(|_| Some(()))
}
