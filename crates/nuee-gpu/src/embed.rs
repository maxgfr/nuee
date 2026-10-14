//! Sentence embeddings with all-MiniLM-L6-v2: the task format, the cosine comparator, text
//! chunking, and (feature `native`) the pinned model files and the candle runner.
//!
//! Task format (NEM1, the same as `web/app/worker/embed.js`, byte for byte), little-endian:
//!
//! ```text
//! input:  "NEM1", u32 count (1..=256), then count times: u32 byte length (<= 8 KiB) + UTF-8
//! output: count x 384 f32, one L2-normalised row per text, in input order
//! ```
//!
//! Embeddings use sentence-transformers semantics: at most 256 tokens per text (longer texts
//! are truncated), mean pooling over the attention mask, then L2 normalisation. Natively the
//! fp32 safetensors weights run on candle; browsers run the q8 ONNX export on ONNX Runtime
//! Web. The two agree within [`TAU`], which jobs put in their manifest as
//! `Comparator::Cosine { min: TAU }`.

use nuee_proto::Comparator;

use crate::GpuError;

/// Embedding width.
pub const DIM: usize = 384;
/// Texts per task.
pub const MAX_TEXTS: usize = 256;
/// Bytes per text.
pub const MAX_TEXT_BYTES: usize = 8 * 1024;
/// Tokens per text, `[CLS]` and `[SEP]` included.
pub const MAX_TOKENS: usize = 256;
/// Texts per forward pass.
pub const BATCH: usize = 64;
/// Magic of an input.
pub const MAGIC: [u8; 4] = *b"NEM1";

/// Minimum cosine between two executions of the same text, native fp32 against browser q8.
///
/// Calibrated on 235 texts (French, English, code, short and long, some past the token
/// limit): the lowest cosine measured between candle (CPU, fp32, batches of 64) and ONNX
/// Runtime Web (q8, one text per inference) was 0.97173; this is that value minus 0.005,
/// floored to 3 decimals.
pub const TAU: f32 = 0.966;

/// The comparator embedding jobs carry in their manifest.
pub const COMPARE: Comparator = Comparator::Cosine { min: TAU };

/// Encodes the texts of one task.
pub fn encode_input<S: AsRef<str>>(texts: &[S]) -> Result<Vec<u8>, GpuError> {
    if texts.is_empty() || texts.len() > MAX_TEXTS {
        return Err(GpuError::invalid(format!(
            "a task holds 1 to {MAX_TEXTS} texts, not {}",
            texts.len()
        )));
    }
    let mut out = Vec::with_capacity(8 + texts.iter().map(|t| 4 + t.as_ref().len()).sum::<usize>());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&u32::try_from(texts.len()).unwrap_or(u32::MAX).to_le_bytes());
    for t in texts {
        let t = t.as_ref().as_bytes();
        if t.len() > MAX_TEXT_BYTES {
            return Err(GpuError::invalid(format!(
                "a text is {} bytes, above {MAX_TEXT_BYTES}",
                t.len()
            )));
        }
        out.extend_from_slice(&u32::try_from(t.len()).unwrap_or(u32::MAX).to_le_bytes());
        out.extend_from_slice(t);
    }
    Ok(out)
}

fn take<'a>(input: &mut &'a [u8], n: usize, what: &str) -> Result<&'a [u8], GpuError> {
    if input.len() < n {
        return Err(GpuError::invalid(format!("NEM1 input truncated in {what}")));
    }
    let (head, rest) = input.split_at(n);
    *input = rest;
    Ok(head)
}

fn take_u32(input: &mut &[u8], what: &str) -> Result<usize, GpuError> {
    let b = take(input, 4, what)?;
    let v = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    usize::try_from(v).map_err(|_| GpuError::invalid("length out of range"))
}

/// Decodes and checks an input: magic, count, lengths, UTF-8, nothing after the last text.
pub fn decode_input(mut input: &[u8]) -> Result<Vec<String>, GpuError> {
    if take(&mut input, 4, "the magic")? != MAGIC {
        return Err(GpuError::invalid("not an NEM1 input"));
    }
    let count = take_u32(&mut input, "the count")?;
    if count == 0 || count > MAX_TEXTS {
        return Err(GpuError::invalid(format!(
            "a task holds 1 to {MAX_TEXTS} texts, not {count}"
        )));
    }
    let mut texts = Vec::with_capacity(count);
    for i in 0..count {
        let len = take_u32(&mut input, "a text length")?;
        if len > MAX_TEXT_BYTES {
            return Err(GpuError::invalid(format!(
                "text {i} is {len} bytes, above {MAX_TEXT_BYTES}"
            )));
        }
        let bytes = take(&mut input, len, "a text")?;
        let text = std::str::from_utf8(bytes)
            .map_err(|_| GpuError::invalid(format!("text {i} is not UTF-8")))?;
        texts.push(text.to_owned());
    }
    if !input.is_empty() {
        return Err(GpuError::invalid("trailing bytes after the last text"));
    }
    Ok(texts)
}

/// Rows as little-endian bytes.
pub fn encode_output(rows: &[f32]) -> Vec<u8> {
    rows.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Reads an output of `count` rows; `None` when its length does not match.
pub fn decode_output(bytes: &[u8], count: usize) -> Option<Vec<f32>> {
    if count.checked_mul(DIM * 4) != Some(bytes.len()) {
        return None;
    }
    let (chunks, _) = bytes.as_chunks::<4>();
    Some(chunks.iter().map(|c| f32::from_le_bytes(*c)).collect())
}

/// Cosine similarity, or `None` when a vector is zero, not finite, or the lengths differ.
pub fn cosine(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() {
        return None;
    }
    let (mut dot, mut na, mut nb) = (0f64, 0f64, 0f64);
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let c = dot / (na.sqrt() * nb.sqrt());
    c.is_finite().then_some(c as f32)
}

/// Largest distance of a row's L2 norm from 1 that still counts as normalised.
pub const NORM_TOL: f64 = 0.01;

/// Whether every row of an output has an L2 norm of 1 within [`NORM_TOL`].
pub fn rows_normalised(bytes: &[u8]) -> bool {
    let row = DIM * 4;
    if bytes.is_empty() || !bytes.len().is_multiple_of(row) {
        return false;
    }
    let Some(v) = decode_output(bytes, bytes.len() / row) else {
        return false;
    };
    v.as_chunks::<DIM>().0.iter().all(|r| {
        let n = r
            .iter()
            .map(|x| f64::from(*x) * f64::from(*x))
            .sum::<f64>()
            .sqrt();
        (n - 1.0).abs() <= NORM_TOL
    })
}

/// Agreement of two embedding outputs: same shape, every row L2-normalised (within
/// [`NORM_TOL`]: cosine alone ignores magnitude), and every pair of rows has a cosine of at
/// least `min`. Empty outputs, ragged lengths and rows that are zero or not finite never
/// agree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cosine {
    /// Minimum cosine per row.
    pub min: f32,
}

impl Cosine {
    /// The lowest cosine between rows of `a` and `b`, or `None` when they cannot be compared.
    pub fn lowest(a: &[u8], b: &[u8]) -> Option<f32> {
        let row = DIM * 4;
        if a.is_empty() || a.len() != b.len() || !a.len().is_multiple_of(row) {
            return None;
        }
        let count = a.len() / row;
        let (a, b) = (decode_output(a, count)?, decode_output(b, count)?);
        let mut low = f32::INFINITY;
        for (x, y) in a.as_chunks::<DIM>().0.iter().zip(b.as_chunks::<DIM>().0) {
            low = low.min(cosine(x, y)?);
        }
        Some(low)
    }
}

impl nuee_core::Compare for Cosine {
    fn same(&self, a: &[u8], b: &[u8]) -> bool {
        rows_normalised(a)
            && rows_normalised(b)
            && Cosine::lowest(a, b).is_some_and(|c| c >= self.min)
    }
}

// ---------------------------------------------------------------------------------------------
// Chunking.

/// Target chunk length, in characters.
pub const CHUNK_CHARS: usize = 1000;
/// Characters shared by consecutive chunks.
pub const CHUNK_OVERLAP: usize = 200;

/// A chunk of a text, as byte offsets (on character boundaries).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    /// First byte.
    pub start: usize,
    /// One past the last byte.
    pub end: usize,
}

/// Splits `text` into chunks of about [`CHUNK_CHARS`] characters overlapping by about
/// [`CHUNK_OVERLAP`], cutting after a paragraph break, else a sentence end, else a space, when
/// one lies in the second half of the window. Blank texts give no chunk.
pub fn chunk_text(text: &str) -> Vec<Chunk> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let n = chars.len();
    let byte_at = |i: usize| chars.get(i).map_or(text.len(), |(b, _)| *b);
    let ch = |i: usize| chars.get(i).map_or('\0', |(_, c)| *c);
    let mut out = Vec::new();
    let mut start = 0;
    while start < n {
        while start < n && ch(start).is_whitespace() {
            start += 1;
        }
        if start >= n {
            break;
        }
        let mut end = (start + CHUNK_CHARS).min(n);
        if end < n {
            let floor = start + CHUNK_CHARS / 2;
            let cut_after = |pred: &dyn Fn(usize) -> bool| (floor..end).rev().find(|&i| pred(i));
            let para = cut_after(&|i| ch(i) == '\n' && i > 0 && ch(i - 1) == '\n');
            let sentence = || {
                cut_after(&|i| {
                    matches!(ch(i), '.' | '!' | '?' | '\n')
                        && (i + 1 >= n || ch(i + 1).is_whitespace())
                })
            };
            let space = || cut_after(&|i| ch(i).is_whitespace());
            if let Some(i) = para.or_else(sentence).or_else(space) {
                end = i + 1;
            }
        }
        let mut last = end;
        while last > start && ch(last - 1).is_whitespace() {
            last -= 1;
        }
        out.push(Chunk {
            start: byte_at(start),
            end: byte_at(last),
        });
        if end >= n {
            break;
        }
        // Back off by the overlap, then forward to the start of a word.
        let mut next = end.saturating_sub(CHUNK_OVERLAP).max(start + 1);
        let limit = (next + CHUNK_OVERLAP / 2).min(end);
        let mut j = next;
        while j < limit && !ch(j - 1).is_whitespace() {
            j += 1;
        }
        if j < limit {
            next = j;
        }
        start = next;
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Native: pinned model files and the candle runner.

#[cfg(feature = "native")]
pub use native::*;

#[cfg(feature = "native")]
mod native {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use candle_core::{DType, Device, Tensor};
    use candle_nn::VarBuilder;
    use candle_transformers::models::bert::{BertModel, Config};
    use nuee_proto::FailKind;
    use sha2::{Digest, Sha256};
    use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

    use super::{BATCH, DIM, MAX_TOKENS, decode_input, encode_output};
    use crate::GpuError;

    /// A pinned file of a model repository.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PinnedFile {
        /// Path in the repository.
        pub path: &'static str,
        /// Size in bytes.
        pub size: u64,
        /// SHA-256, lowercase hex.
        pub sha256: &'static str,
    }

    /// A model repository at a fixed revision, with the files used from it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PinnedModel {
        /// Hugging Face repository.
        pub repo: &'static str,
        /// Commit.
        pub rev: &'static str,
        /// Files, each checked by size and SHA-256 before use.
        pub files: &'static [PinnedFile],
    }

    /// all-MiniLM-L6-v2 as run natively (fp32 safetensors).
    pub const MINILM: PinnedModel = PinnedModel {
        repo: "sentence-transformers/all-MiniLM-L6-v2",
        rev: "1110a243fdf4706b3f48f1d95db1a4f5529b4d41",
        files: &[
            PinnedFile {
                path: "config.json",
                size: 612,
                sha256: "953f9c0d463486b10a6871cc2fd59f223b2c70184f49815e7efbcab5d8908b41",
            },
            PinnedFile {
                path: "tokenizer.json",
                size: 466_247,
                sha256: "be50c3628f2bf5bb5e3a7f17b1f74611b2561a3a27eeab05e5aa30f411572037",
            },
            PinnedFile {
                path: "model.safetensors",
                size: 90_868_376,
                sha256: "53aa51172d142c89d9012cce15ae4d6cc0ca6895895114379cacb4fab128d9db",
            },
        ],
    };

    /// Why model files could not be made available.
    #[derive(Debug, thiserror::Error)]
    pub enum ModelError {
        /// The download failed.
        #[error("cannot download {url}: {detail}")]
        Network {
            /// What was fetched.
            url: String,
            /// Why it failed.
            detail: String,
        },
        /// A file does not match its pinned size or SHA-256.
        #[error("{path} does not match its pinned SHA-256")]
        Integrity {
            /// The file.
            path: String,
        },
        /// Reading or writing the cache failed.
        #[error("{path}: {detail}")]
        Io {
            /// The file.
            path: String,
            /// Why it failed.
            detail: String,
        },
    }

    impl From<ModelError> for GpuError {
        fn from(e: ModelError) -> GpuError {
            let kind = match e {
                ModelError::Integrity { .. } => FailKind::Invalid,
                _ => FailKind::Io,
            };
            GpuError::new(kind, e.to_string())
        }
    }

    fn io(path: &Path, e: impl std::fmt::Display) -> ModelError {
        ModelError::Io {
            path: path.display().to_string(),
            detail: e.to_string(),
        }
    }

    /// SHA-256 as lowercase hex.
    pub fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// Hosts model files may come from, redirects included.
    pub fn allowed_host(host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        host == "huggingface.co" || host.ends_with(".huggingface.co") || host.ends_with(".hf.co")
    }

    impl PinnedModel {
        /// Cache directory of this model under `root`: `<root>/<repo>@<rev>/`.
        pub fn dir(&self, root: &Path) -> PathBuf {
            root.join(format!("{}@{}", self.repo, self.rev))
        }

        /// Download URL of `file`.
        pub fn url(&self, file: &PinnedFile) -> String {
            format!(
                "https://huggingface.co/{}/resolve/{}/{}",
                self.repo, self.rev, file.path
            )
        }

        /// Reads `file` from the cache under `root` and checks it; `Ok(None)` when absent.
        pub fn read_cached(
            &self,
            root: &Path,
            file: &PinnedFile,
        ) -> Result<Option<Vec<u8>>, ModelError> {
            let path = self.dir(root).join(file.path);
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(io(&path, e)),
            };
            if bytes.len() as u64 != file.size || sha256_hex(&bytes) != file.sha256 {
                return Err(ModelError::Integrity {
                    path: path.display().to_string(),
                });
            }
            Ok(Some(bytes))
        }

        /// Makes every file available under `root`, downloading what is missing or corrupt.
        /// A cached file that fails its check is deleted and downloaded again; a download that
        /// fails its check is never written.
        pub async fn fetch(&self, root: &Path) -> Result<PathBuf, ModelError> {
            let dir = self.dir(root);
            let mut client = None;
            for file in self.files {
                match self.read_cached(root, file) {
                    Ok(Some(_)) => continue,
                    Ok(None) => {}
                    Err(ModelError::Integrity { path }) => {
                        tracing::warn!("{path} is corrupt; downloading it again");
                        match std::fs::remove_file(&path) {
                            Ok(()) => {}
                            // A concurrent fetcher removed or replaced it first.
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                            Err(e) => return Err(io(Path::new(&path), e)),
                        }
                    }
                    Err(e) => return Err(e),
                }
                let c = match &client {
                    Some(c) => c,
                    None => client.insert(http_client()?),
                };
                let bytes = download(c, &self.url(file), file.size).await?;
                let path = dir.join(file.path);
                if bytes.len() as u64 != file.size || sha256_hex(&bytes) != file.sha256 {
                    return Err(ModelError::Integrity {
                        path: self.url(file),
                    });
                }
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
                }
                write_atomically(&path, &bytes)?;
            }
            Ok(dir)
        }
    }

    /// Writes `bytes` to `path` through a temporary file unique to this writer, then renames it
    /// into place. Other threads and processes fetching the same model never share the
    /// temporary file, and a reader sees either no file or the whole file. When two writers
    /// race, both hold the same checked bytes and the last rename wins.
    fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), ModelError> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let part = path.with_file_name(format!(
            ".{name}.{}.{}.part",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let written = std::fs::write(&part, bytes)
            .map_err(|e| io(&part, e))
            .and_then(|()| std::fs::rename(&part, path).map_err(|e| io(path, e)));
        if written.is_err()
            && let Err(e) = std::fs::remove_file(&part)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("cannot remove {}: {e}", part.display());
        }
        written
    }

    fn http_client() -> Result<reqwest::Client, ModelError> {
        if rustls::crypto::CryptoProvider::get_default().is_none()
            && rustls::crypto::ring::default_provider()
                .install_default()
                .is_err()
        {
            // Another thread installed a provider in between, which is all that is needed.
            tracing::debug!("a TLS crypto provider was installed concurrently");
        }
        let policy = reqwest::redirect::Policy::custom(|attempt| {
            let url = attempt.url();
            if attempt.previous().len() >= 10 {
                attempt.error("too many redirects")
            } else if url.scheme() != "https" || !url.host_str().is_some_and(allowed_host) {
                let to = url.host_str().unwrap_or("?").to_owned();
                attempt.error(format!("redirect to a host that is not allowed: {to}"))
            } else {
                attempt.follow()
            }
        });
        reqwest::Client::builder()
            .https_only(true)
            .redirect(policy)
            .connect_timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| ModelError::Network {
                url: "https://huggingface.co".into(),
                detail: e.to_string(),
            })
    }

    async fn download(
        client: &reqwest::Client,
        url: &str,
        size: u64,
    ) -> Result<Vec<u8>, ModelError> {
        let net = |e: &dyn std::fmt::Display| ModelError::Network {
            url: url.to_owned(),
            detail: e.to_string(),
        };
        let mut resp = client
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| net(&e))?;
        if resp.content_length().is_some_and(|n| n != size) {
            return Err(ModelError::Integrity {
                path: url.to_owned(),
            });
        }
        let mut out = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
        while let Some(chunk) = resp.chunk().await.map_err(|e| net(&e))? {
            if out.len() as u64 + chunk.len() as u64 > size {
                return Err(ModelError::Integrity {
                    path: url.to_owned(),
                });
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    /// Where an [`Embedder`] runs.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum EmbedDevice {
        /// Metal on macOS when it can run the model, else the CPU.
        Auto,
        /// The CPU (deterministic).
        Cpu,
    }

    fn model_err(e: impl std::fmt::Display) -> GpuError {
        GpuError::new(FailKind::Io, format!("embedding model: {e}"))
    }

    /// all-MiniLM-L6-v2 loaded on a device.
    pub struct Embedder {
        model: BertModel,
        tokenizer: Tokenizer,
        device: Device,
    }

    impl std::fmt::Debug for Embedder {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Embedder")
                .field("device", &self.device_name())
                .finish_non_exhaustive()
        }
    }

    impl Embedder {
        /// Downloads (when needed) and loads [`MINILM`] from the cache under `root`.
        pub async fn fetch_and_load(
            root: &Path,
            device: EmbedDevice,
        ) -> Result<Embedder, GpuError> {
            MINILM.fetch(root).await?;
            let root = root.to_owned();
            tokio::task::spawn_blocking(move || Embedder::load(&root, device))
                .await
                .map_err(|e| GpuError::new(FailKind::Io, format!("loader thread: {e}")))?
        }

        /// Loads [`MINILM`] from the cache under `root`; every file is checked against its
        /// pinned SHA-256 as it is read.
        pub fn load(root: &Path, device: EmbedDevice) -> Result<Embedder, GpuError> {
            let mut files = Vec::with_capacity(MINILM.files.len());
            for file in MINILM.files {
                let bytes = MINILM.read_cached(root, file)?.ok_or_else(|| {
                    GpuError::new(FailKind::Io, format!("{} is not downloaded", file.path))
                })?;
                files.push(bytes);
            }
            let [config, tokenizer, weights] =
                <[Vec<u8>; 3]>::try_from(files).map_err(|_| model_err("unexpected file list"))?;
            let config: Config = serde_json::from_slice(&config).map_err(model_err)?;
            let mut tokenizer = Tokenizer::from_bytes(&tokenizer).map_err(model_err)?;
            tokenizer
                .with_truncation(Some(TruncationParams {
                    max_length: MAX_TOKENS,
                    ..TruncationParams::default()
                }))
                .map_err(model_err)?;
            tokenizer.with_padding(Some(PaddingParams {
                strategy: PaddingStrategy::BatchLongest,
                ..PaddingParams::default()
            }));
            if device == EmbedDevice::Auto
                && let Some(gpu) = gpu_device()
            {
                // Not every operation has a kernel on every backend: keep the GPU only when a
                // probe embedding runs on it.
                match Embedder::build(&config, tokenizer.clone(), weights.clone(), gpu) {
                    Ok(e) if e.embed(&["probe"]).is_ok() => return Ok(e),
                    Ok(_) | Err(_) => {
                        tracing::debug!("embedding on the CPU: the GPU lacks a kernel")
                    }
                }
            }
            Embedder::build(&config, tokenizer, weights, Device::Cpu)
        }

        fn build(
            config: &Config,
            tokenizer: Tokenizer,
            weights: Vec<u8>,
            device: Device,
        ) -> Result<Embedder, GpuError> {
            let vb = VarBuilder::from_buffered_safetensors(weights, DType::F32, &device)
                .map_err(model_err)?;
            let model = BertModel::load(vb, config).map_err(model_err)?;
            Ok(Embedder {
                model,
                tokenizer,
                device,
            })
        }

        /// `"cpu"` or `"metal"`.
        pub fn device_name(&self) -> &'static str {
            if self.device.is_metal() {
                "metal"
            } else {
                "cpu"
            }
        }

        /// Embeddings of `texts`: `texts.len() x DIM` values, rows L2-normalised.
        pub fn embed<S: AsRef<str>>(&self, texts: &[S]) -> Result<Vec<f32>, GpuError> {
            let mut out = Vec::with_capacity(texts.len() * DIM);
            for batch in texts.chunks(BATCH) {
                let batch: Vec<&str> = batch.iter().map(AsRef::as_ref).collect();
                out.extend(self.embed_batch(&batch).map_err(model_err)?);
            }
            Ok(out)
        }

        fn embed_batch(&self, texts: &[&str]) -> candle_core::Result<Vec<f32>> {
            let enc = self
                .tokenizer
                .encode_batch(texts.to_vec(), true)
                .map_err(candle_core::Error::msg)?;
            let ids: Vec<Tensor> = enc
                .iter()
                .map(|e| Tensor::new(e.get_ids(), &self.device))
                .collect::<candle_core::Result<_>>()?;
            let mask: Vec<Tensor> = enc
                .iter()
                .map(|e| Tensor::new(e.get_attention_mask(), &self.device))
                .collect::<candle_core::Result<_>>()?;
            let ids = Tensor::stack(&ids, 0)?;
            let mask = Tensor::stack(&mask, 0)?;
            let types = ids.zeros_like()?;
            let hidden = self.model.forward(&ids, &types, Some(&mask))?;
            let mask = mask.to_dtype(DType::F32)?.unsqueeze(2)?;
            let summed = hidden.broadcast_mul(&mask)?.sum(1)?;
            let counts = mask.sum(1)?.clamp(1e-9f32, f32::MAX)?;
            let mean = summed.broadcast_div(&counts)?;
            let norm = mean
                .sqr()?
                .sum_keepdim(1)?
                .sqrt()?
                .clamp(1e-12f32, f32::MAX)?;
            let rows = mean.broadcast_div(&norm)?.to_device(&Device::Cpu)?;
            rows.flatten_all()?.to_vec1::<f32>()
        }

        /// Runs one NEM1 task.
        pub fn run(&self, input: &[u8]) -> Result<Vec<u8>, GpuError> {
            let texts = decode_input(input)?;
            Ok(encode_output(&self.embed(&texts)?))
        }
    }

    fn gpu_device() -> Option<Device> {
        #[cfg(target_os = "macos")]
        {
            if let Err(missing) =
                metal::supported(|class| objc2::runtime::AnyClass::get(class).is_some())
            {
                tracing::info!(
                    "embedding on the CPU: Metal has no {} class (it needs macOS 15 or later)",
                    missing.to_string_lossy()
                );
                return None;
            }
            if let Ok(d) = Device::new_metal(0) {
                return Some(d);
            }
        }
        None
    }

    #[cfg(target_os = "macos")]
    mod metal {
        use std::ffi::CStr;

        /// Objective-C classes candle's Metal device instantiates when it is created. Without
        /// one, creating the device panics instead of returning an error, so each is checked
        /// first. `MTLResidencySet` appeared in macOS 15.
        pub(super) const REQUIRED_CLASSES: &[&CStr] = &[c"MTLResidencySetDescriptor"];

        /// `Ok` when every required class is present, else the first missing one.
        pub(super) fn supported(has_class: impl Fn(&CStr) -> bool) -> Result<(), &'static CStr> {
            match REQUIRED_CLASSES.iter().find(|c| !has_class(c)) {
                Some(missing) => Err(missing),
                None => Ok(()),
            }
        }

        #[cfg(test)]
        mod tests {
            use super::*;

            #[test]
            fn metal_is_refused_when_residency_sets_are_missing() {
                let sonoma = |c: &CStr| c != c"MTLResidencySetDescriptor";
                assert_eq!(supported(sonoma), Err(c"MTLResidencySetDescriptor"));
            }

            #[test]
            fn metal_is_kept_when_every_class_is_present() {
                assert_eq!(supported(|_| true), Ok(()));
            }

            #[test]
            fn the_check_asks_for_the_class_candle_creates() {
                let asked = std::cell::RefCell::new(Vec::new());
                let missing = supported(|c| {
                    asked.borrow_mut().push(c.to_owned());
                    false
                });
                assert_eq!(missing, Err(c"MTLResidencySetDescriptor"));
                assert_eq!(
                    asked.into_inner(),
                    [c"MTLResidencySetDescriptor".to_owned()]
                );
            }
        }
    }
}
