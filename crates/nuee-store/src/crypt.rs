//! tar packing with strict unpacking rules, and age (X25519) encryption.
//!
//! The archive is encrypted as a whole so that file names never leave the machine in clear.
//! The 32-byte X25519 secret travels in the link (`FileLink.key`).

use std::io::{self, Read, Write};
use std::iter;

use crate::StoreError;

const BECH32_CHARSET: &[u8; 32] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";
const AGE_SECRET_HRP: &str = "age-secret-key-";
const COPY_BUF: usize = 64 * 1024;

// ---------------------------------------------------------------------------------------------
// Keys.

/// An age X25519 identity as the raw 32-byte secret carried by file links.
#[derive(Clone, PartialEq, Eq)]
pub struct AgeKey(pub [u8; 32]);

impl std::fmt::Debug for AgeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AgeKey(..)")
    }
}

impl AgeKey {
    /// A fresh key from the system random source.
    pub fn generate() -> Result<AgeKey, StoreError> {
        let a = nuee_proto::new_secret16()?;
        let b = nuee_proto::new_secret16()?;
        let mut key = [0u8; 32];
        key[..16].copy_from_slice(&a);
        key[16..].copy_from_slice(&b);
        Ok(AgeKey(key))
    }

    /// The age identity holding this secret.
    pub fn identity(&self) -> age::x25519::Identity {
        let encoded = bech32_encode(AGE_SECRET_HRP, &self.0).to_uppercase();
        encoded
            .parse()
            .unwrap_or_else(|e| unreachable!("a well-formed age secret key is rejected: {e}"))
    }

    /// The secret of an age identity.
    pub fn from_identity(identity: &age::x25519::Identity) -> Option<AgeKey> {
        use age::secrecy::ExposeSecret;
        let encoded = identity.to_string();
        let bytes = bech32_decode(AGE_SECRET_HRP, encoded.expose_secret())?;
        Some(AgeKey(bytes.try_into().ok()?))
    }
}

fn bech32_polymod(values: impl Iterator<Item = u8>) -> u32 {
    const GEN: [u32; 5] = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3];
    let mut chk: u32 = 1;
    for v in values {
        let top = chk >> 25;
        chk = ((chk & 0x1ff_ffff) << 5) ^ u32::from(v);
        for (i, g) in GEN.iter().enumerate() {
            if (top >> i) & 1 == 1 {
                chk ^= g;
            }
        }
    }
    chk
}

fn hrp_expand(hrp: &str) -> Vec<u8> {
    let b = hrp.as_bytes();
    b.iter()
        .map(|c| c >> 5)
        .chain(iter::once(0))
        .chain(b.iter().map(|c| c & 31))
        .collect()
}

fn to_5bit(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() * 8 / 5 + 1);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for &b in data {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(((acc >> bits) & 31) as u8);
        }
    }
    if bits > 0 {
        out.push(((acc << (5 - bits)) & 31) as u8);
    }
    out
}

fn bech32_encode(hrp: &str, data: &[u8]) -> String {
    let data5 = to_5bit(data);
    let mut values = hrp_expand(hrp);
    values.extend_from_slice(&data5);
    values.extend_from_slice(&[0; 6]);
    let pm = bech32_polymod(values.into_iter()) ^ 1;
    let checksum = (0..6).map(|i| ((pm >> (5 * (5 - i))) & 31) as u8);
    let mut out = String::with_capacity(hrp.len() + 1 + data5.len() + 6);
    out.push_str(hrp);
    out.push('1');
    for v in data5.into_iter().chain(checksum) {
        out.push(char::from(BECH32_CHARSET[usize::from(v)]));
    }
    out
}

fn bech32_decode(hrp: &str, s: &str) -> Option<Vec<u8>> {
    let lower = s.to_ascii_lowercase();
    let rest = lower.strip_prefix(hrp)?.strip_prefix('1')?;
    let values: Vec<u8> = rest
        .bytes()
        .map(|c| BECH32_CHARSET.iter().position(|&x| x == c).map(|p| p as u8))
        .collect::<Option<_>>()?;
    if values.len() < 6 {
        return None;
    }
    let mut all = hrp_expand(hrp);
    all.extend_from_slice(&values);
    if bech32_polymod(all.into_iter()) != 1 {
        return None;
    }
    let data5 = values.get(..values.len() - 6)?;
    let mut out = Vec::with_capacity(data5.len() * 5 / 8);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for &v in data5 {
        acc = (acc << 5) | u32::from(v);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    if bits >= 5 || (acc & ((1 << bits) - 1)) != 0 {
        return None;
    }
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// Encryption.

/// Encrypts everything read from `plaintext` into `out` with a fresh key, which is returned.
pub fn encrypt<R: Read, W: Write>(mut plaintext: R, out: W) -> Result<AgeKey, StoreError> {
    let key = AgeKey::generate()?;
    let recipient = key.identity().to_public();
    let encryptor = age::Encryptor::with_recipients(iter::once(&recipient as &dyn age::Recipient))
        .map_err(|e| StoreError::Store(format!("cannot set up encryption: {e}")))?;
    let mut writer = encryptor.wrap_output(out)?;
    io::copy(&mut plaintext, &mut writer)?;
    writer.finish()?;
    Ok(key)
}

/// Decrypts `ciphertext` with `key` into `out`. Returns the plaintext size.
///
/// A wrong key and any altered byte are both reported as [`StoreError::Decrypt`]; nothing
/// written to `out` can be trusted after an error.
pub fn decrypt<R: Read, W: Write>(
    ciphertext: R,
    key: &AgeKey,
    mut out: W,
) -> Result<u64, StoreError> {
    let identity = key.identity();
    let decryptor = age::Decryptor::new_buffered(io::BufReader::new(ciphertext))
        .map_err(|e| StoreError::Decrypt(format!("not an age file: {e}")))?;
    let mut reader = decryptor
        .decrypt(iter::once(&identity as &dyn age::Identity))
        .map_err(|e| match e {
            age::DecryptError::NoMatchingKeys => {
                StoreError::Decrypt("the link's key does not open this file (wrong key)".into())
            }
            other => StoreError::Decrypt(other.to_string()),
        })?;
    let mut buf = vec![0u8; COPY_BUF];
    let mut total = 0u64;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                return Err(StoreError::Decrypt(format!(
                    "the ciphertext is corrupted or truncated: {e}"
                )));
            }
        };
        let chunk = buf.get(..n).unwrap_or_default();
        out.write_all(chunk)?;
        total += n as u64;
    }
    out.flush()?;
    Ok(total)
}

/// Encrypts `plaintext` in memory with a fresh key.
pub fn encrypt_bytes(plaintext: &[u8]) -> Result<(Vec<u8>, AgeKey), StoreError> {
    let mut out = Vec::with_capacity(plaintext.len() + 256);
    let key = encrypt(plaintext, &mut out)?;
    Ok((out, key))
}

/// Decrypts `ciphertext` in memory.
pub fn decrypt_bytes(ciphertext: &[u8], key: &AgeKey) -> Result<Vec<u8>, StoreError> {
    let mut out = Vec::with_capacity(ciphertext.len());
    decrypt(ciphertext, key, &mut out)?;
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Archive path rules.

/// Splits an archive path into safe components: relative, no `..`, no empty name, no
/// backslash, NUL or other control character (names are printed on terminals). `.` components
/// are dropped.
pub fn safe_components(raw: &[u8]) -> Result<Vec<String>, StoreError> {
    let shown = || String::from_utf8_lossy(raw).escape_debug().to_string();
    let s = std::str::from_utf8(raw).map_err(|_| StoreError::UnsafePath(shown()))?;
    if s.starts_with('/') || s.contains('\\') || s.chars().any(char::is_control) {
        return Err(StoreError::UnsafePath(shown()));
    }
    let mut out = Vec::new();
    for part in s.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(StoreError::UnsafePath(shown())),
            p => out.push(p.to_owned()),
        }
    }
    if out.is_empty() {
        return Err(StoreError::UnsafePath(shown()));
    }
    Ok(out)
}

enum Kind {
    File,
    Dir,
}

fn entry_kind<R: Read>(entry: &tar::Entry<'_, R>, name: &str) -> Result<Kind, StoreError> {
    let t = entry.header().entry_type();
    if t.is_file() {
        Ok(Kind::File)
    } else if t.is_dir() {
        Ok(Kind::Dir)
    } else {
        Err(StoreError::UnsafePath(format!(
            "{name}: only regular files and directories are allowed"
        )))
    }
}

fn header(kind: tar::EntryType, size: u64, mode: u32) -> tar::Header {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(kind);
    h.set_size(size);
    h.set_mode(mode);
    h.set_mtime(0);
    h
}

/// Builds a tar archive in memory from `(relative name, bytes)` pairs.
pub fn pack_bytes(files: &[(String, Vec<u8>)]) -> Result<Vec<u8>, StoreError> {
    let mut seen = std::collections::HashSet::new();
    let mut builder = tar::Builder::new(Vec::new());
    for (name, bytes) in files {
        let name = safe_components(name.as_bytes())?.join("/");
        if !seen.insert(name.clone()) {
            return Err(StoreError::Exists(name.into()));
        }
        let mut h = header(tar::EntryType::Regular, bytes.len() as u64, 0o644);
        builder.append_data(&mut h, &name, bytes.as_slice())?;
    }
    Ok(builder.into_inner()?)
}

/// Reads every regular file of a tar archive held in memory, with the same path rules as
/// [`unpack`]. Directories carry no data and are skipped.
pub fn unpack_bytes(tar_bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, StoreError> {
    let mut archive = tar::Archive::new(tar_bytes);
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let raw = entry.path_bytes().into_owned();
        let name = safe_components(&raw)?.join("/");
        match entry_kind(&entry, &name)? {
            Kind::Dir => {}
            Kind::File => {
                if !seen.insert(name.clone()) {
                    return Err(StoreError::Exists(name.into()));
                }
                let mut data = Vec::new();
                entry.read_to_end(&mut data)?;
                out.push((name, data));
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Native file system side.

#[cfg(not(target_family = "wasm"))]
pub use native::{Packed, pack, unpack};

#[cfg(not(target_family = "wasm"))]
mod native {
    use std::fs::{self, File, OpenOptions};
    use std::io::{self, Read, Seek, SeekFrom};
    use std::path::{Path, PathBuf};

    use super::{Kind, entry_kind, header, safe_components};
    use crate::StoreError;

    /// A tar archive written to an anonymous temporary file.
    #[derive(Debug)]
    pub struct Packed {
        /// The archive, rewound to its start.
        pub file: File,
        /// Archive size in bytes.
        pub size: u64,
        /// Regular files packed.
        pub files: u64,
        /// Symbolic links and special files that were left out.
        pub skipped: Vec<PathBuf>,
    }

    fn utf8_name(path: &Path) -> Result<String, StoreError> {
        path.to_str()
            .map(str::to_owned)
            .ok_or_else(|| StoreError::UnsafePath(format!("{} is not UTF-8", path.display())))
    }

    /// Packs files and directories into a temporary tar file. Each argument is stored under
    /// its own name; directories keep their relative layout. Symbolic links are never
    /// followed: they are skipped and listed in [`Packed::skipped`].
    pub fn pack(paths: &[PathBuf]) -> Result<Packed, StoreError> {
        let file = tempfile::tempfile()?;
        let mut builder = tar::Builder::new(file);
        let mut skipped = Vec::new();
        let mut files = 0u64;
        let mut names = std::collections::HashSet::new();
        for path in paths {
            let meta = fs::symlink_metadata(path)?;
            if meta.file_type().is_symlink() {
                skipped.push(path.clone());
                continue;
            }
            let base = match path.file_name() {
                Some(n) => PathBuf::from(n),
                None => fs::canonicalize(path)?
                    .file_name()
                    .map(PathBuf::from)
                    .ok_or_else(|| StoreError::UnsafePath(path.display().to_string()))?,
            };
            let name = utf8_name(&base)?;
            if !names.insert(name.clone()) {
                return Err(StoreError::Exists(base));
            }
            add(&mut builder, path, &name, &meta, &mut files, &mut skipped)?;
        }
        let mut file = builder.into_inner()?;
        let size = file.seek(SeekFrom::End(0))?;
        file.seek(SeekFrom::Start(0))?;
        Ok(Packed {
            file,
            size,
            files,
            skipped,
        })
    }

    fn add(
        builder: &mut tar::Builder<File>,
        path: &Path,
        name: &str,
        meta: &fs::Metadata,
        files: &mut u64,
        skipped: &mut Vec<PathBuf>,
    ) -> Result<(), StoreError> {
        let ft = meta.file_type();
        if ft.is_symlink() {
            skipped.push(path.to_path_buf());
        } else if ft.is_file() {
            let f = File::open(path)?;
            let len = f.metadata()?.len();
            let mut h = header(tar::EntryType::Regular, len, 0o644);
            builder.append_data(&mut h, name, f.take(len))?;
            *files += 1;
        } else if ft.is_dir() {
            let mut h = header(tar::EntryType::Directory, 0, 0o755);
            builder.append_data(&mut h, format!("{name}/"), io::empty())?;
            let mut children = fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
            children.sort_by_key(|e| e.file_name());
            for child in children {
                let child_name = utf8_name(Path::new(&child.file_name()))?;
                let child_meta = fs::symlink_metadata(child.path())?;
                add(
                    builder,
                    &child.path(),
                    &format!("{name}/{child_name}"),
                    &child_meta,
                    files,
                    skipped,
                )?;
            }
        } else {
            skipped.push(path.to_path_buf());
        }
        Ok(())
    }

    /// Ensures `path` is a real directory (not a link), creating it when missing.
    fn ensure_dir(path: &Path) -> Result<(), StoreError> {
        match fs::symlink_metadata(path) {
            Ok(m) if m.file_type().is_symlink() => Err(StoreError::UnsafePath(format!(
                "{} is a symbolic link",
                path.display()
            ))),
            Ok(m) if m.is_dir() => Ok(()),
            Ok(_) => Err(StoreError::Exists(path.to_path_buf())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(path)?;
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Extracts a tar stream under `dest` (created if needed). Only regular files and
    /// directories are accepted; absolute paths, `..`, links and devices are refused, and
    /// no path may go through an existing symbolic link. Existing files are kept unless
    /// `overwrite`. Returns the files written.
    pub fn unpack<R: Read>(
        tar_stream: R,
        dest: &Path,
        overwrite: bool,
    ) -> Result<Vec<PathBuf>, StoreError> {
        fs::create_dir_all(dest)?;
        let mut archive = tar::Archive::new(tar_stream);
        let mut written = Vec::new();
        for entry in archive.entries()? {
            let mut entry = entry?;
            let raw = entry.path_bytes().into_owned();
            let parts = safe_components(&raw)?;
            let shown = parts.join("/");
            let kind = entry_kind(&entry, &shown)?;
            let (last, parents) = parts
                .split_last()
                .ok_or_else(|| StoreError::UnsafePath(shown.clone()))?;
            let mut target = dest.to_path_buf();
            for p in parents {
                target.push(p);
                ensure_dir(&target)?;
            }
            target.push(last);
            match kind {
                Kind::Dir => ensure_dir(&target)?,
                Kind::File => {
                    let mut opts = OpenOptions::new();
                    opts.write(true);
                    match fs::symlink_metadata(&target) {
                        Ok(m) if m.file_type().is_symlink() => {
                            return Err(StoreError::UnsafePath(format!(
                                "{} is a symbolic link",
                                target.display()
                            )));
                        }
                        Ok(m) if m.is_file() && overwrite => {
                            opts.truncate(true);
                        }
                        Ok(_) => return Err(StoreError::Exists(target)),
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {
                            opts.create_new(true);
                        }
                        Err(e) => return Err(e.into()),
                    }
                    let mut out = opts.open(&target)?;
                    io::copy(&mut entry, &mut out)?;
                    written.push(target);
                }
            }
        }
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_round_trips_through_age() {
        let key = AgeKey([7; 32]);
        let id = key.identity();
        assert_eq!(AgeKey::from_identity(&id), Some(key));
        let fresh = age::x25519::Identity::generate();
        let k = AgeKey::from_identity(&fresh).expect("decode");
        use age::secrecy::ExposeSecret;
        assert_eq!(
            k.identity().to_string().expose_secret(),
            fresh.to_string().expose_secret()
        );
    }

    #[test]
    fn components_refuse_control_characters() {
        for raw in [
            &b"a\x1b[2Jb.txt"[..],
            b"dir/\x07bell",
            b"line\nbreak",
            b"tab\there",
            b"del\x7f",
            "c1\u{9b}31m".as_bytes(),
        ] {
            let e = safe_components(raw).expect_err("control character accepted");
            let StoreError::UnsafePath(shown) = e else {
                panic!("wrong error {e:?}");
            };
            assert!(!shown.chars().any(char::is_control), "{shown:?}");
        }
        assert!(safe_components("caf\u{e9}/\u{1f600}.txt".as_bytes()).is_ok());
    }

    #[test]
    fn components_rules() {
        assert!(safe_components(b"a/b/c.txt").is_ok());
        assert_eq!(
            safe_components(b"./a//b").expect("ok"),
            vec!["a".to_owned(), "b".to_owned()]
        );
        for bad in [
            &b"../evil"[..],
            b"/etc/passwd",
            b"a/../../b",
            b"",
            b".",
            b"a\\b",
        ] {
            assert!(safe_components(bad).is_err(), "{:?}", bad);
        }
    }
}
