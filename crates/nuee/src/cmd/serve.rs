//! `nuee serve`: serve every pinned item until Ctrl-C.
//!
//! With `--anchor`, run an always-on anchor: a persistent coordinator that accepts jobs from
//! `nuee run --anchor`, optionally its own relay, a durable store member and an HTTP gateway.
//! Nothing ever requires an anchor; it only adds persistence.
//!
//! The operator gets an **anchor link** (an `X` link: anchor id, relay, admin secret). Admin
//! requests travel over `nuee/http/0` with the admin secret as the HMAC proof:
//!
//! - `GET /status`
//! - `POST /jobs` (a [`Submission`]) ⇒ `{job, link, fingerprint, existing}`
//! - `GET /jobs`, `GET /jobs/<id>`, `GET /jobs/<id>/results` ⇒ `{link}` (an `F` link)
//! - `POST /sites {link}` ⇒ `{id, path}`, `DELETE /sites/<id>`
//! - `POST /services {id, link}` ⇒ `{id, path}`, `DELETE /services/<id>`

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use bytes::Bytes;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::protocol::{AcceptError, ProtocolHandler};
use nuee_core::{CoordinatorConfig, Event, JobSpec};
use nuee_net::driver::{MAX_HELLO, close_code};
use nuee_net::{
    Admission, AdmitConfig, AdmitError, CoordinatorDriver, CoordinatorHandle, DriverConfig,
    EndpointAddr, FrameError, HostEvent, Net, NetConfig, RelayChoice, RelayUrl, WorkerHostEvent,
    recv_console_body, recv_console_hello, recv_msg, send_msg, write_frame,
};
use nuee_proto::limits::{MAX_ASSET, MAX_INLINE_INPUT, MAX_MODULE};
use nuee_proto::{
    ALPN_HTTP, ALPN_JOB, BoundedBytes, BoundedString, BoundedVec, ByeReason, Caps, EndpointId,
    FileLink, Hash, Header, HttpRequest, HttpResponse, JobLink, Link, LocalLimits, MAX_HTTP_MSG,
    Manifest, Msg, Payload, SecretKey, ServiceLink, TaskKind,
};
use nuee_store::{FilesAcceptor, HubConfig, Offer, Registry, StoreHub};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::{Ctx, exit, fail};

/// Arguments of `nuee serve`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Run as an always-on anchor (persistent coordinator, relay, store, gateway)
    #[arg(long)]
    pub anchor: bool,
    /// File holding the admin secret (created 0600 if missing; default <data dir>/anchor/admin.secret)
    #[arg(long, value_name = "PATH", requires = "anchor")]
    pub admin_secret_file: Option<PathBuf>,
    /// Run an embedded relay with this public host name (HTTPS through Let's Encrypt)
    #[arg(
        long,
        value_name = "FQDN",
        requires_all = ["anchor", "acme_contact"],
        conflicts_with = "relay_dev"
    )]
    pub relay_hostname: Option<String>,
    /// Contact email for Let's Encrypt
    #[arg(long, value_name = "EMAIL", requires = "relay_hostname")]
    pub acme_contact: Option<String>,
    /// Run an embedded plain-HTTP relay (local use and tests)
    #[arg(long, requires = "anchor")]
    pub relay_dev: bool,
    /// Address of the embedded dev relay (port 0 picks a free port)
    #[arg(
        long,
        value_name = "ADDR",
        default_value = "127.0.0.1:3340",
        requires = "relay_dev"
    )]
    pub relay_bind: SocketAddr,
    /// File holding the bearer token clients of the embedded relay must present
    #[arg(long, value_name = "PATH", requires = "anchor")]
    pub relay_token_file: Option<PathBuf>,
    /// Most clients connected at once to the embedded relay
    #[arg(long, value_name = "N", requires = "anchor")]
    pub relay_max_conns: Option<usize>,
    /// Per-client incoming rate limit of the embedded relay, in bytes per second
    #[arg(long, value_name = "BYTES_PER_S", requires = "anchor")]
    pub relay_rate: Option<NonZeroU32>,
    /// Offer this much durable storage to the anchor's swarm, such as 50GiB
    #[arg(long, value_name = "QUOTA", requires = "anchor")]
    pub store: Option<String>,
    /// Serve pinned sites and registered services over HTTP on this address
    #[arg(long, value_name = "ADDR:PORT", requires = "anchor")]
    pub gateway: Option<SocketAddr>,
    /// Let gateway pages run scripts (still sandboxed, never same-origin): a risk you take
    #[arg(long, requires = "gateway")]
    pub gateway_scripts: bool,
    /// Only admit this peer to the anchor's swarm (repeat; enterprise mode)
    #[arg(long, value_name = "ENDPOINT_ID", requires = "anchor")]
    pub allow: Vec<String>,
    /// Only admit the peers listed in this file (one id per line, `#` comments)
    #[arg(long, value_name = "PATH", requires = "anchor")]
    pub allow_file: Option<PathBuf>,
    /// Approve each joining peer by hand
    #[arg(long, requires = "anchor")]
    pub approve: bool,
    /// Tasks the anchor runs itself at once; even with 0 it recomputes the verification
    /// samples and mismatches of the kinds it can run
    #[arg(long, value_name = "N", default_value_t = 1, requires = "anchor")]
    pub local_slots: u16,
}

/// Serves the pins, or runs the anchor.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    if ctx.global.ephemeral {
        return Err(fail(
            exit::USAGE,
            "`nuee serve` keeps state on disk; it cannot run with --ephemeral",
        ));
    }
    if args.anchor {
        return run_anchor(args, ctx).await;
    }
    let reg = Registry::load(&Registry::default_path(&ctx.data_dir()?))?;
    let net = ctx.online_net().await?;
    let hub = ctx.hub(&net).await?;
    let serve = nuee_store::serve_pins(&net, &hub, &reg).map_err(crate::store_failure)?;
    ctx.ui.emit(
        "serving",
        json!({ "id": net.id().to_string(), "items": reg.list().len() }),
    );
    ctx.ui.say(&format!(
        "serving {} pinned item(s) as {} until Ctrl-C",
        reg.list().len(),
        net.id()
    ));
    crate::ctrl_c().await;
    serve.shutdown().await.map_err(crate::store_failure)?;
    ctx.ui.emit("stopped", json!({}));
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------------------------
// Small helpers.

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    let t = text.trim().as_bytes();
    let (pairs, rest) = t.as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    pairs
        .iter()
        .map(|p| {
            let s = std::str::from_utf8(p).ok()?;
            u8::from_str_radix(s, 16).ok()
        })
        .collect()
}

fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    if tmp.exists() {
        std::fs::remove_file(&tmp).with_context(|| format!("removing {}", tmp.display()))?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    std::io::Write::write_all(&mut f, bytes)
        .with_context(|| format!("writing {}", tmp.display()))?;
    f.sync_all()
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// A 16-byte secret kept in hex in `path`: read, or created 0600 when missing.
fn load_or_create_secret(path: &Path) -> anyhow::Result<[u8; 16]> {
    if path.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path)
                .with_context(|| format!("reading {}", path.display()))?
                .permissions()
                .mode();
            if mode & 0o077 != 0 {
                return Err(fail(
                    exit::ERROR,
                    format!(
                        "{} is readable by others; run `chmod 600 {}`",
                        path.display(),
                        path.display()
                    ),
                ));
            }
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let bytes = unhex(&text).unwrap_or_default();
        return bytes.as_slice().try_into().map_err(|_| {
            fail(
                exit::ERROR,
                format!(
                    "{} does not hold a secret (expected 32 hex digits)",
                    path.display()
                ),
            )
        });
    }
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        nuee_store::ensure_private_dir(dir).map_err(crate::store_failure)?;
    }
    let secret = nuee_proto::new_secret16().context("system random source")?;
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
    std::io::Write::write_all(&mut f, format!("{}\n", hex(&secret)).as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(secret)
}

fn parse_id(text: &str, what: &str) -> anyhow::Result<EndpointId> {
    text.trim().parse().map_err(|_| {
        fail(
            exit::USAGE,
            format!("{what}: {text:?} is not an endpoint id"),
        )
    })
}

/// The allowlist of `--allow` and `--allow-file`; `None` when neither is given.
fn allowlist(args: &Args) -> anyhow::Result<Option<HashSet<EndpointId>>> {
    if args.allow.is_empty() && args.allow_file.is_none() {
        return Ok(None);
    }
    let mut set = HashSet::new();
    for a in &args.allow {
        set.insert(parse_id(a, "--allow")?);
    }
    if let Some(path) = &args.allow_file {
        let text = std::fs::read_to_string(path)
            .map_err(|e| fail(exit::ERROR, format!("cannot read {}: {e}", path.display())))?;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or_default().trim();
            if !line.is_empty() {
                set.insert(parse_id(line, &path.display().to_string())?);
            }
        }
    }
    Ok(Some(set))
}

/// A second identity derived from the anchor's key, for the endpoint serving result bundles
/// and storage (this node's `nuee/job/0` runs the compute swarm).
fn derived_key(key: &SecretKey, label: &[u8]) -> SecretKey {
    let mut buf = label.to_vec();
    buf.extend_from_slice(&key.to_bytes());
    SecretKey::from_bytes(Hash::of(&buf).as_bytes())
}

async fn bind_online(
    ctx: &Ctx,
    secret: SecretKey,
    relay: RelayChoice,
    relay_only: bool,
) -> anyhow::Result<Net> {
    let net = Net::bind(NetConfig {
        secret,
        relay,
        relay_only,
        alpns: Vec::new(),
    })
    .await
    .map_err(|e| fail(exit::NETWORK, e.to_string()))?;
    if net.online(crate::online_timeout()).await.is_err() {
        net.close().await;
        return Err(crate::relay_unreachable());
    }
    ctx.keep(&net);
    Ok(net)
}

// ---------------------------------------------------------------------------------------------
// Admin API wire types (JSON over `nuee/http/0`).

/// A blob held by the submitter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BlobRef {
    /// BLAKE3, hex.
    pub hash: String,
    /// Size in bytes.
    pub size: u64,
}

/// A job handed to an anchor (`POST /jobs`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Submission {
    /// postcard(Manifest), hex; `job` is ignored and `coordinator` must be the anchor.
    pub manifest: String,
    /// BLAKE3 of each task input, hex, in task order.
    pub inputs: Vec<String>,
    /// Every input, each as `u32 LE length || bytes`, in task order.
    pub bundle: BlobRef,
    /// Who serves the blobs (the submitter).
    pub provider: ProviderRef,
}

/// An endpoint and its relay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ProviderRef {
    /// Endpoint id.
    pub id: String,
    /// Relay URL (may be empty).
    pub relay: String,
}

/// Most tasks in one anchored job.
/// Time an admin client that passed the hello has to send its request.
const ADMIN_BODY_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TASKS: usize = 100_000;
/// Largest inputs bundle.
const MAX_BUNDLE: u64 = 1 << 30;

/// A published folder is archived under its own name: serve its content at the site root.
fn strip_root(files: Vec<(String, Vec<u8>)>) -> Vec<(String, Vec<u8>)> {
    let root = files
        .first()
        .and_then(|(n, _)| n.split_once('/'))
        .map(|(r, _)| format!("{r}/"));
    match root {
        Some(r) if files.iter().all(|(n, _)| n.starts_with(&r)) => files
            .into_iter()
            .map(|(n, b)| (n[r.len()..].to_owned(), b))
            .collect(),
        _ => files,
    }
}

/// Splits an inputs bundle.
fn split_bundle(bytes: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let (len, tail) = rest.split_first_chunk::<4>()?;
        let len = usize::try_from(u32::from_le_bytes(*len)).ok()?;
        if len > tail.len() || out.len() >= MAX_TASKS {
            return None;
        }
        let (item, tail) = tail.split_at(len);
        out.push(item.to_vec());
        rest = tail;
    }
    Some(out)
}

/// Builds an inputs bundle.
pub(crate) fn make_bundle(inputs: &[Vec<u8>]) -> anyhow::Result<Vec<u8>> {
    let total: usize = inputs.iter().map(|i| i.len() + 4).sum();
    let mut out = Vec::with_capacity(total);
    for i in inputs {
        out.extend_from_slice(&u32::try_from(i.len())?.to_le_bytes());
        out.extend_from_slice(i);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Anchor state.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum JobState {
    Running,
    Done,
}

/// What the anchor keeps of a job, in `<data dir>/anchor/jobs/<id>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobRecord {
    id: u32,
    key: String,
    name: String,
    manifest: String,
    inputs: Vec<String>,
    bundle: String,
    bundle_size: u64,
    submitter: String,
    state: JobState,
    done: BTreeMap<u32, String>,
    failed: u32,
    results: Option<String>,
}

impl JobRecord {
    fn summary(&self) -> Value {
        json!({
            "job": self.id,
            "name": self.name,
            "tasks": self.inputs.len(),
            "done": self.done.len(),
            "failed": self.failed,
            "state": self.state,
            "results": self.results.is_some(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SiteRecord {
    id: String,
    link: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ServiceRecord {
    id: String,
    link: String,
}

#[derive(Debug, Default)]
struct Book {
    jobs: BTreeMap<u32, JobRecord>,
    /// Core job number ⇒ anchor job id, for running jobs.
    core: HashMap<u32, u32>,
    sites: Vec<SiteRecord>,
    services: Vec<ServiceRecord>,
}

struct Anchor {
    net: Net,
    hub: StoreHub,
    files_net: Net,
    files_hub: StoreHub,
    acceptor: FilesAcceptor,
    handle: CoordinatorHandle,
    dir: PathBuf,
    web_base: String,
    swarm_url: String,
    gateway: Option<nuee_host::gateway::Gateway>,
    book: Mutex<Book>,
}

type Reply = (u16, Value);

fn err(status: u16, msg: impl Into<String>) -> Reply {
    (status, json!({ "error": msg.into() }))
}

impl Anchor {
    fn jobs_dir(&self) -> PathBuf {
        self.dir.join("jobs")
    }

    fn save_job(&self, rec: &JobRecord) -> anyhow::Result<()> {
        let path = self.jobs_dir().join(format!("{}.json", rec.id));
        write_private(&path, &serde_json::to_vec_pretty(rec)?)
    }

    fn save_list<T: Serialize>(&self, name: &str, list: &[T]) -> anyhow::Result<()> {
        write_private(&self.dir.join(name), &serde_json::to_vec_pretty(list)?)
    }

    /// Builds the coordinator's spec of a stored job from the blobs held locally.
    async fn spec_of(&self, rec: &JobRecord) -> anyhow::Result<JobSpec> {
        let manifest_bytes = unhex(&rec.manifest).ok_or_else(|| anyhow::anyhow!("bad manifest"))?;
        let mut manifest: Manifest = nuee_proto::decode(&manifest_bytes)?;
        manifest.coordinator = self.net.id();
        let bundle_hash: Hash = rec.bundle.parse()?;
        let bundle = self.hub.read(bundle_hash).await?;
        let inputs = split_bundle(&bundle).ok_or_else(|| anyhow::anyhow!("bad inputs bundle"))?;
        let mut payloads = Vec::with_capacity(inputs.len());
        for input in inputs {
            if input.len() <= MAX_INLINE_INPUT {
                payloads.push(Payload::Inline(BoundedBytes::try_from(input)?));
            } else {
                let size = input.len() as u64;
                let hash = self.hub.add_bytes(Bytes::from(input)).await?;
                payloads.push(Payload::Blob { hash, size });
            }
        }
        let compare = crate::cmd::run::checker(manifest.kind, &manifest.compare);
        Ok(JobSpec {
            manifest,
            inputs: payloads,
            compare,
        })
    }

    /// Hands a stored job to the coordinator, with the tasks already done.
    async fn start_job(&self, book: &mut Book, rec: &JobRecord) -> anyhow::Result<()> {
        let spec = self.spec_of(rec).await?;
        let core = self
            .handle
            .submit(spec)
            .await
            .map_err(|e| anyhow::anyhow!("cannot submit the job: {e}"))?;
        for (task, hash) in &rec.done {
            self.handle
                .mark_done(core, *task, hash.parse()?)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        book.core.insert(core, rec.id);
        Ok(())
    }

    /// Makes what the store holds survive a crash before it is recorded as done.
    async fn sync(&self, hub: &StoreHub) -> anyhow::Result<()> {
        hub.store()
            .sync_db()
            .await
            .map_err(|e| anyhow::anyhow!("syncing the blob store: {e}"))
    }

    fn job_link(&self) -> Value {
        json!(self.swarm_url)
    }

    async fn fetch_from(&self, from: &EndpointAddr, blob: (Hash, u64)) -> Result<(), Reply> {
        self.hub
            .fetch_to_store(from.clone(), blob.0, blob.1, &mut |_, _| {})
            .await
            .map_err(|e| {
                err(
                    502,
                    format!("cannot fetch {} from the submitter: {e}", blob.0),
                )
            })
    }

    async fn submit(&self, body: &[u8]) -> Reply {
        let sub: Submission = match serde_json::from_slice(body) {
            Ok(s) => s,
            Err(e) => return err(400, format!("invalid submission: {e}")),
        };
        let Some(manifest_bytes) = unhex(&sub.manifest) else {
            return err(400, "manifest is not hex");
        };
        let mut manifest: Manifest = match nuee_proto::decode(&manifest_bytes) {
            Ok(m) => m,
            Err(e) => return err(400, format!("invalid manifest: {e}")),
        };
        if manifest.coordinator != self.net.id() {
            return err(400, "the manifest names another coordinator");
        }
        manifest.job = 0;
        if let Err(e) = manifest.check() {
            return err(400, format!("invalid manifest: {e}"));
        }
        if !matches!(
            manifest.kind,
            TaskKind::Wasm | TaskKind::Wgsl | TaskKind::Exec
        ) {
            return err(400, format!("{:?} jobs cannot be anchored", manifest.kind));
        }
        if sub.inputs.is_empty()
            || sub.inputs.len() > MAX_TASKS
            || u32::try_from(sub.inputs.len()).ok() != Some(manifest.tasks)
        {
            return err(400, "the input list does not match the task count");
        }
        let mut wanted = Vec::with_capacity(sub.inputs.len());
        for h in &sub.inputs {
            match h.parse::<Hash>() {
                Ok(h) => wanted.push(h),
                Err(_) => return err(400, "an input hash is malformed"),
            }
        }
        let Ok(bundle_hash) = sub.bundle.hash.parse::<Hash>() else {
            return err(400, "the bundle hash is malformed");
        };
        if sub.bundle.size > MAX_BUNDLE {
            return err(413, "inputs bundle too large");
        }
        let key = {
            let mut buf = manifest_bytes.clone();
            for h in &wanted {
                buf.extend_from_slice(h.as_bytes());
            }
            Hash::of(&buf).to_string()
        };
        let mut book = self.book.lock().await;
        if let Some(rec) = book.jobs.values().find(|r| r.key == key) {
            return (200, self.created(rec.id, true));
        }
        if book.core.len() >= nuee_proto::limits::MAX_JOBS {
            return err(503, "the anchor runs too many jobs; try later");
        }

        let Ok(provider) = sub.provider.id.parse::<EndpointId>() else {
            return err(400, "the provider id is malformed");
        };
        if !sub.provider.relay.is_empty()
            && let Err(e) = self.net.add_peer_relay_str(provider, &sub.provider.relay)
        {
            return err(400, format!("provider relay: {e}"));
        }
        let from = self
            .net
            .peer_addr(provider)
            .unwrap_or_else(|| EndpointAddr::new(provider));
        if let Some(module) = manifest.module {
            if manifest.module_size as usize > MAX_MODULE {
                return err(413, "module too large");
            }
            if let Err(r) = self
                .fetch_from(&from, (module, u64::from(manifest.module_size)))
                .await
            {
                return r;
            }
            if manifest.kind == TaskKind::Wasm {
                let bytes = match self.hub.read(module).await {
                    Ok(b) => b,
                    Err(e) => return err(500, e.to_string()),
                };
                if let Err(e) = nuee_proto::validate_module(&bytes) {
                    return err(400, format!("invalid module: {e}"));
                }
            }
        }
        for asset in manifest.assets.iter() {
            if asset.size > MAX_ASSET as u64 {
                return err(413, "asset too large");
            }
            if let Err(r) = self.fetch_from(&from, (asset.hash, asset.size)).await {
                return r;
            }
        }
        if let Err(r) = self.fetch_from(&from, (bundle_hash, sub.bundle.size)).await {
            return r;
        }
        let bundle = match self.hub.read(bundle_hash).await {
            Ok(b) => b,
            Err(e) => return err(500, e.to_string()),
        };
        let Some(inputs) = split_bundle(&bundle) else {
            return err(400, "malformed inputs bundle");
        };
        if inputs.len() != wanted.len()
            || inputs.iter().zip(&wanted).any(|(i, h)| Hash::of(i) != *h)
        {
            return err(400, "the inputs bundle does not match the input hashes");
        }

        let id = book.jobs.keys().next_back().map_or(1, |n| n + 1);
        let rec = JobRecord {
            id,
            key,
            name: manifest.name.as_str().to_owned(),
            manifest: hex(&manifest_bytes),
            inputs: sub.inputs.clone(),
            bundle: bundle_hash.to_string(),
            bundle_size: sub.bundle.size,
            submitter: provider.to_string(),
            state: JobState::Running,
            done: BTreeMap::new(),
            failed: 0,
            results: None,
        };
        if let Err(e) = self.sync(&self.hub).await {
            return err(500, format!("{e:#}"));
        }
        if let Err(e) = self.save_job(&rec) {
            return err(500, format!("cannot keep the job: {e:#}"));
        }
        if let Err(e) = self.start_job(&mut book, &rec).await {
            if let Err(rm) = std::fs::remove_file(self.jobs_dir().join(format!("{}.json", rec.id)))
            {
                tracing::warn!("cannot remove the record of a refused job: {rm}");
            }
            return err(503, format!("{e:#}"));
        }
        tracing::info!(job = id, name = %rec.name, "job accepted from {provider}");
        book.jobs.insert(id, rec);
        (201, self.created(id, false))
    }

    fn created(&self, id: u32, existing: bool) -> Value {
        json!({
            "job": id,
            "link": self.job_link(),
            "fingerprint": nuee_proto::fingerprint(&self.net.id()).to_vec(),
            "existing": existing,
        })
    }

    /// Records an accepted output.
    async fn accepted(
        &self,
        core: u32,
        task: u32,
        output: &[u8],
        hash: Hash,
    ) -> anyhow::Result<()> {
        self.hub
            .add_bytes(Bytes::copy_from_slice(output))
            .await
            .context("storing an output")?;
        self.sync(&self.hub).await?;
        let mut book = self.book.lock().await;
        let Some(id) = book.core.get(&core).copied() else {
            tracing::debug!(core, "output of an unknown job");
            return Ok(());
        };
        if let Some(rec) = book.jobs.get_mut(&id) {
            rec.done.insert(task, hash.to_string());
            let rec = rec.clone();
            self.save_job(&rec)?;
        }
        Ok(())
    }

    /// A job is over: its outputs become one encrypted bundle served as an `F` link.
    async fn finished(&self, core: u32, failed: u32) -> anyhow::Result<()> {
        let mut book = self.book.lock().await;
        let Some(id) = book.core.remove(&core) else {
            return Ok(());
        };
        let Some(mut rec) = book.jobs.get(&id).cloned() else {
            return Ok(());
        };
        rec.failed = failed;
        let mut files = Vec::with_capacity(rec.done.len() + 1);
        let mut outputs = Vec::with_capacity(rec.done.len());
        for (task, hash) in &rec.done {
            let bytes = self
                .hub
                .read(hash.parse()?)
                .await
                .with_context(|| format!("reading the output of task {task}"))?;
            files.push((format!("task-{task}.bin"), bytes.to_vec()));
            outputs
                .push(json!({ "task": task, "file": format!("task-{task}.bin"), "blake3": hash }));
        }
        let doc = json!({
            "job": rec.name,
            "tasks": rec.inputs.len(),
            "done": rec.done.len(),
            "failed": failed,
            "coordinator": self.net.id().to_string(),
            "outputs": outputs,
        });
        files.push(("results.json".into(), serde_json::to_vec_pretty(&doc)?));
        let tar = nuee_store::pack_bytes(&files)?;
        let (cipher, key) = nuee_store::encrypt_bytes(&tar)?;
        let size = cipher.len() as u64;
        let hash = self
            .files_hub
            .add_bytes(Bytes::from(cipher))
            .await
            .context("storing the results bundle")?;
        self.sync(&self.files_hub).await?;
        let access = nuee_proto::new_secret16()?;
        self.acceptor.add(Offer { access, hash, size });
        let mut providers = BoundedVec::new();
        providers
            .push(nuee_store::self_provider(&self.files_net))
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let link = FileLink {
            providers,
            hash,
            size,
            access,
            key: key.0,
        };
        rec.results = Some(Link::File(link).to_url(&self.web_base));
        rec.state = JobState::Done;
        self.save_job(&rec)?;
        book.jobs.insert(id, rec);
        drop(book);
        match self.handle.forget(core).await {
            Ok(true) => {}
            Ok(false) => tracing::debug!(core, "the coordinator still holds the job"),
            Err(e) => return Err(anyhow::anyhow!("{e}")),
        }
        tracing::info!(job = id, failed, "job done");
        Ok(())
    }

    async fn add_site(&self, body: &[u8], save: bool) -> Reply {
        #[derive(Deserialize)]
        struct Req {
            link: String,
        }
        let Some(gateway) = &self.gateway else {
            return err(409, "this anchor runs no gateway (see --gateway)");
        };
        let req: Req = match serde_json::from_slice(body) {
            Ok(r) => r,
            Err(e) => return err(400, format!("invalid request: {e}")),
        };
        let link = match Link::parse(&req.link) {
            Ok(Link::Site(l)) => l,
            Ok(_) => return err(400, "not a site link"),
            Err(e) => return err(400, e.to_string()),
        };
        let id = link.hash.to_string();
        let files = if self.hub.complete_size(link.hash).await.ok().flatten() == Some(link.size) {
            match self.open_site(&link).await {
                Ok(f) => f,
                Err(e) => return err(422, format!("{e:#}")),
            }
        } else {
            match nuee_store::get_bytes(&self.net, &self.hub, &link, |_, _| {}).await {
                Ok(f) => f,
                Err(e) => return err(502, format!("cannot fetch the site: {e}")),
            }
        };
        if let Err(e) = gateway.add_site(&id, strip_root(files)) {
            return err(422, e.to_string());
        }
        if save {
            let mut book = self.book.lock().await;
            book.sites.retain(|s| s.id != id);
            book.sites.push(SiteRecord {
                id: id.clone(),
                link: req.link,
            });
            if let Err(e) = self.save_list("sites.json", &book.sites) {
                return err(500, format!("{e:#}"));
            }
        }
        (201, json!({ "id": id, "path": format!("/s/{id}/") }))
    }

    async fn open_site(&self, link: &FileLink) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
        let cipher = self.hub.read(link.hash).await?;
        let tar = nuee_store::decrypt_bytes(&cipher, &nuee_store::AgeKey(link.key))?;
        Ok(nuee_store::unpack_bytes(&tar)?)
    }

    async fn add_service(&self, body: &[u8], save: bool) -> Reply {
        #[derive(Deserialize)]
        struct Req {
            id: String,
            link: String,
        }
        let Some(gateway) = &self.gateway else {
            return err(409, "this anchor runs no gateway (see --gateway)");
        };
        let req: Req = match serde_json::from_slice(body) {
            Ok(r) => r,
            Err(e) => return err(400, format!("invalid request: {e}")),
        };
        let link = match Link::parse(&req.link) {
            Ok(Link::Service(l)) => l,
            Ok(_) => return err(400, "not a service link"),
            Err(e) => return err(400, e.to_string()),
        };
        if let Err(e) = gateway.add_service(&req.id, link) {
            return err(400, e.to_string());
        }
        if save {
            let mut book = self.book.lock().await;
            book.services.retain(|s| s.id != req.id);
            book.services.push(ServiceRecord {
                id: req.id.clone(),
                link: req.link,
            });
            if let Err(e) = self.save_list("services.json", &book.services) {
                return err(500, format!("{e:#}"));
            }
        }
        (
            201,
            json!({ "id": req.id, "path": format!("/x/{}/", req.id) }),
        )
    }

    async fn remove(&self, what: &str, id: &str) -> Reply {
        let Some(gateway) = &self.gateway else {
            return err(404, "not found");
        };
        let mut book = self.book.lock().await;
        let (found, res) = if what == "sites" {
            let found = gateway.remove_site(id);
            book.sites.retain(|s| s.id != id);
            (found, self.save_list("sites.json", &book.sites))
        } else {
            let found = gateway.remove_service(id);
            book.services.retain(|s| s.id != id);
            (found, self.save_list("services.json", &book.services))
        };
        if let Err(e) = res {
            return err(500, format!("{e:#}"));
        }
        if found {
            (200, json!({ "removed": id }))
        } else {
            err(404, "not found")
        }
    }

    async fn route(&self, method: &str, path: &str, body: &[u8]) -> Reply {
        let path = path.split('?').next().unwrap_or_default();
        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
        match (method, parts.as_slice()) {
            ("GET", ["status"]) => {
                let book = self.book.lock().await;
                (
                    200,
                    json!({
                        "id": self.net.id().to_string(),
                        "swarm": self.swarm_url,
                        "jobs": book.jobs.len(),
                        "running": book.core.len(),
                        "sites": self.gateway.as_ref().map(|g| g.site_ids()),
                        "services": self.gateway.as_ref().map(|g| g.service_ids()),
                    }),
                )
            }
            ("POST", ["jobs"]) => self.submit(body).await,
            ("GET", ["jobs"]) => {
                let book = self.book.lock().await;
                let list: Vec<Value> = book.jobs.values().map(JobRecord::summary).collect();
                (200, json!({ "jobs": list }))
            }
            ("GET", ["jobs", id]) => {
                let book = self.book.lock().await;
                match id.parse::<u32>().ok().and_then(|n| book.jobs.get(&n)) {
                    Some(rec) => (200, rec.summary()),
                    None => err(404, "unknown job"),
                }
            }
            ("GET", ["jobs", id, "results"]) => {
                let book = self.book.lock().await;
                match id.parse::<u32>().ok().and_then(|n| book.jobs.get(&n)) {
                    Some(JobRecord {
                        results: Some(link),
                        ..
                    }) => (200, json!({ "link": link })),
                    Some(_) => err(409, "the job is still running"),
                    None => err(404, "unknown job"),
                }
            }
            ("POST", ["sites"]) => self.add_site(body, true).await,
            ("POST", ["services"]) => self.add_service(body, true).await,
            ("DELETE", [what @ ("sites" | "services"), id]) => self.remove(what, id).await,
            (_, ["status" | "jobs" | "sites" | "services", ..]) => err(405, "method not allowed"),
            _ => err(404, "not found"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Protocol handlers.

/// `nuee/http/0` admin API: the request's proof must come from the admin secret.
#[derive(Clone)]
struct AdminHandler {
    anchor: Arc<Anchor>,
    secret: [u8; 16],
    gate: Arc<Admission>,
}

impl std::fmt::Debug for AdminHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminHandler").finish_non_exhaustive()
    }
}

fn response(status: u16, body: &Value) -> HttpResponse {
    let mut headers = BoundedVec::new();
    if let (Ok(name), Ok(value)) = (
        BoundedString::try_from("content-type"),
        BoundedString::try_from("application/json"),
    ) && headers.push(Header { name, value }).is_err()
    {
        tracing::debug!("no room for the content type");
    }
    HttpResponse {
        status,
        headers,
        body: BoundedBytes::try_from(body.to_string().into_bytes()).unwrap_or_default(),
    }
}

impl AdminHandler {
    async fn stream(&self, peer: EndpointId, mut send: SendStream, mut recv: RecvStream) {
        let own = self.anchor.net.id();
        // The admin secret is checked on the small hello before the request is read.
        let req = match recv_console_hello(&mut recv, &self.gate, &self.secret, &peer, &own).await {
            Ok(_) => {
                match tokio::time::timeout(ADMIN_BODY_TIMEOUT, recv_console_body(&mut recv)).await {
                    Ok(read) => read.map_err(AdmitError::Frame),
                    Err(_) => Err(AdmitError::Timeout),
                }
            }
            Err(e) => Err(e),
        };
        let (status, body) = match req {
            Ok(req) => {
                let (status, body) = self
                    .anchor
                    .route(req.method.as_str(), req.path.as_str(), req.body.as_slice())
                    .await;
                tracing::info!(%peer, method = req.method.as_str(), path = req.path.as_str(), status, "admin request");
                (status, body)
            }
            Err(e) => {
                tracing::info!(%peer, "admin request refused: {e}");
                match e {
                    AdmitError::Busy => err(503, "busy"),
                    AdmitError::Throttled => err(429, "too many wrong attempts; try later"),
                    AdmitError::BadProof => err(403, "wrong admin secret"),
                    AdmitError::Timeout => err(408, "no request before the deadline"),
                    AdmitError::Frame(FrameError::TooLarge { .. }) => err(413, "request too large"),
                    _ => err(400, "malformed request"),
                }
            }
        };
        let frame = match nuee_proto::encode_with_limit(&response(status, &body), MAX_HTTP_MSG) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("cannot encode an admin response: {e}");
                return;
            }
        };
        if let Err(e) = write_frame(&mut send, &frame).await {
            tracing::debug!(%peer, "admin response not delivered: {e}");
            return;
        }
        if let Err(e) = send.finish() {
            tracing::debug!(%peer, "stream already closed: {e}");
        }
        if tokio::time::timeout(Duration::from_secs(5), send.stopped())
            .await
            .is_err()
        {
            tracing::debug!(%peer, "client did not read the response in time");
        }
    }
}

impl ProtocolHandler for AdminHandler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let peer = conn.remote_id();
        loop {
            let (send, recv) = match conn.accept_bi().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(%peer, "admin connection ended: {e}");
                    return Ok(());
                }
            };
            let this = self.clone();
            tokio::spawn(async move { this.stream(peer, send, recv).await });
        }
    }
}

/// `nuee/job/0` with the enterprise allowlist in front of the coordinator: a peer outside
/// the list gets `Bye{Rejected}` whatever its proof.
#[derive(Debug, Clone)]
struct Gatekeeper {
    driver: CoordinatorDriver,
    allow: Option<Arc<HashSet<EndpointId>>>,
}

impl Gatekeeper {
    async fn reject(conn: Connection) {
        let peer = conn.remote_id();
        let talk = async {
            let (mut send, mut recv) = conn.accept_bi().await.ok()?;
            let _hello: Msg = recv_msg(&mut recv, MAX_HELLO).await.ok()?;
            send_msg(
                &mut send,
                &Msg::Bye {
                    reason: ByeReason::Rejected,
                },
            )
            .await
            .ok()?;
            send.finish().ok()?;
            tokio::time::timeout(Duration::from_secs(2), send.stopped())
                .await
                .ok()?
                .ok()
        };
        if tokio::time::timeout(Duration::from_secs(5), talk)
            .await
            .ok()
            .flatten()
            .is_none()
        {
            tracing::debug!(%peer, "rejected peer left before the goodbye");
        }
        conn.close(close_code::REFUSED.into(), b"rejected");
    }
}

impl ProtocolHandler for Gatekeeper {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let peer = conn.remote_id();
        if self.allow.as_ref().is_some_and(|a| !a.contains(&peer)) {
            tracing::info!(%peer, "peer not on the allowlist: rejected");
            Gatekeeper::reject(conn).await;
            return Ok(());
        }
        self.driver.accept(conn).await
    }
}

// ---------------------------------------------------------------------------------------------
// The anchor itself.

fn load_list<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<Vec<T>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("reading {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(anyhow::Error::new(e).context(format!("reading {}", path.display()))),
    }
}

fn load_jobs(dir: &Path) -> anyhow::Result<BTreeMap<u32, JobRecord>> {
    let mut jobs = BTreeMap::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(jobs),
        Err(e) => return Err(anyhow::Error::new(e).context(format!("reading {}", dir.display()))),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let rec: JobRecord = serde_json::from_slice(&bytes)
            .with_context(|| format!("reading {}", path.display()))?;
        jobs.insert(rec.id, rec);
    }
    Ok(jobs)
}

/// Joins the anchor's own swarm as a storage-only member from the files endpoint.
async fn store_member(
    files_net: &Net,
    files_hub: &StoreHub,
    link: &JobLink,
    quota: u64,
) -> anyhow::Result<nuee_net::WorkerHandle> {
    let relay = Ctx::relay_string(files_net);
    let exec = Arc::new(crate::cmd::run::WasmExec::new(None)?);
    let caps = Caps {
        agent: BoundedString::try_from(format!("nuee-anchor/{}", env!("CARGO_PKG_VERSION")))
            .unwrap_or_default(),
        browser: false,
        cpu_slots: 0,
        gpu: None,
        mem_mb: 0,
        kinds: 0,
        cached: BoundedVec::new(),
        exec_allow: BoundedVec::new(),
        store_quota_mb: (quota / (1024 * 1024)).max(1),
        relay: BoundedString::try_from(relay.as_str())
            .ok()
            .filter(|r| !r.is_empty()),
        rtc: false,
    };
    let cfg = nuee_core::WorkerConfig {
        caps,
        limits: LocalLimits::default(),
    };
    nuee_net::join(files_net, link, cfg, exec, Arc::new(files_hub.clone()))
        .await
        .map_err(|e| anyhow::anyhow!("the store member cannot join the anchor's swarm: {e}"))
}

async fn run_anchor(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    if args.approve && !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return Err(fail(
            exit::USAGE,
            "--approve needs an interactive terminal to answer y/N",
        ));
    }
    let allow = allowlist(&args)?.map(Arc::new);
    let quota = args
        .store
        .as_deref()
        .map(crate::cmd::join::parse_quota)
        .transpose()?;
    let data = ctx.data_dir()?;
    let dir = data.join("anchor");
    nuee_store::ensure_private_dir(&dir).map_err(crate::store_failure)?;
    nuee_store::ensure_private_dir(&dir.join("jobs")).map_err(crate::store_failure)?;
    let admin_path = args
        .admin_secret_file
        .clone()
        .unwrap_or_else(|| dir.join("admin.secret"));
    let admin_secret = load_or_create_secret(&admin_path)?;
    let swarm_secret = load_or_create_secret(&dir.join("swarm.secret"))?;
    let key = ctx.secret()?;
    let files_key = derived_key(&key, b"nuee/anchor-files/0");

    // The embedded relay, when asked for; the anchor's endpoints then use it.
    let relay_server = if args.relay_dev || args.relay_hostname.is_some() {
        let setup = crate::cmd::relay::RelaySetup {
            dev: args.relay_dev,
            bind: args.relay_bind,
            hostname: args.relay_hostname.clone(),
            acme_contact: args.acme_contact.clone(),
            acme_cache: None,
            token_file: args.relay_token_file.clone(),
            max_conns: args.relay_max_conns,
            rate: args.relay_rate,
            exempt: vec![key.public(), files_key.public()],
        };
        let (server, url) = crate::cmd::relay::start(setup, ctx).await?;
        Some((server, url))
    } else {
        if args.relay_token_file.is_some()
            || args.relay_max_conns.is_some()
            || args.relay_rate.is_some()
        {
            return Err(fail(
                exit::USAGE,
                "--relay-token-file, --relay-max-conns and --relay-rate need --relay-dev or --relay-hostname",
            ));
        }
        None
    };
    let relay = match &relay_server {
        Some((_, url)) => {
            let parsed: RelayUrl = url
                .parse()
                .map_err(|_| anyhow::anyhow!("the embedded relay has a bad URL {url:?}"))?;
            RelayChoice::Custom(vec![parsed])
        }
        None => ctx.relay_choice()?,
    };
    let net = bind_online(ctx, key, relay.clone(), ctx.global.relay_only).await?;
    let files_net = bind_online(ctx, files_key, relay, ctx.global.relay_only).await?;
    let hub = StoreHub::open(&net, &data, HubConfig::default())
        .await
        .with_context(|| format!("opening the store in {}", data.display()))?;
    let files_dir = dir.join("files");
    let files_hub = StoreHub::open(&files_net, &files_dir, HubConfig::default())
        .await
        .with_context(|| format!("opening the store in {}", files_dir.display()))?;

    let relay_str = Ctx::relay_string(&net);
    let job_link = JobLink {
        coordinator: net.id(),
        relay: BoundedString::try_from(relay_str.as_str())
            .map_err(|_| anyhow::anyhow!("relay URL too long for a link"))?,
        secret: swarm_secret,
    };
    let swarm_url = Link::Job(job_link.clone()).to_url(&ctx.global.web_base);
    let anchor_link = Link::Service(ServiceLink {
        host: net.id(),
        relay: job_link.relay.clone(),
        secret: admin_secret,
    });
    let anchor_url = anchor_link.to_url(&ctx.global.web_base);

    // Files endpoint: result bundles, and the durable store member when --store is set.
    let (acceptor, files_router, member) = match quota {
        Some(q) => {
            let registry_path = nuee_store::durable::StoreMember::default_registry(&files_dir);
            let registry = Registry::load(&registry_path).map_err(crate::store_failure)?;
            let member = nuee_store::durable::StoreMember::new(&files_hub, &job_link, q, registry)
                .map_err(crate::store_failure)?;
            ctx.ui.note(&format!(
                "offering {} MiB of durable storage, {} MiB in use",
                q / (1024 * 1024),
                member.used() / (1024 * 1024)
            ));
            (member.acceptor(), member.spawn(), Some(q))
        }
        None => {
            let acceptor = FilesAcceptor::new(&files_hub, "anchor", Vec::new());
            let serve = nuee_store::ServeHandle::spawn(&files_net, &files_hub, acceptor.clone());
            (acceptor, serve.router, None)
        }
    };

    let seed_bytes = nuee_proto::new_secret16().context("system random source")?;
    let mut seed = [0u8; 8];
    seed.copy_from_slice(&seed_bytes[..8]);
    let core_cfg = CoordinatorConfig {
        name: BoundedString::try_from("anchor").unwrap_or_default(),
        local_slots: args.local_slots,
        // Kinds this anchor cannot run answer Unsupported and fall back to peer agreement.
        verify_slots: 1,
        approve: args.approve,
        seed: u64::from_le_bytes(seed),
        ..CoordinatorConfig::default()
    };
    let (driver, mut handle) = CoordinatorDriver::spawn(
        net.clone(),
        swarm_secret,
        core_cfg,
        DriverConfig {
            local_slots: args.local_slots,
            ..DriverConfig::default()
        },
        Arc::new(crate::cmd::run::WasmExec::new(None)?),
        Arc::new(hub.clone()),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut events =
        std::mem::replace(&mut handle.events, tokio::sync::mpsc::unbounded_channel().1);
    let durability = nuee_store::durable::DurabilityService::new(
        &net,
        swarm_secret,
        crate::durable_config(),
        Some(nuee_store::durable::DurabilityService::default_path(&dir)),
    )
    .map_err(crate::store_failure)?;
    durability.set_rebuilder(files_hub.clone(), acceptor.clone());
    // nuee/pin/0 follows the job driver's decisions: approvals and bans.
    durability.require_approval(args.approve);
    let _durability_task = durability.start();

    let gateway = args.gateway.map(|_| {
        nuee_host::gateway::Gateway::new(
            nuee_host::gateway::GatewayConfig {
                scripts: args.gateway_scripts,
            },
            Some(net.clone()),
        )
    });
    let anchor = Arc::new(Anchor {
        net: net.clone(),
        hub: hub.clone(),
        files_net: files_net.clone(),
        files_hub: files_hub.clone(),
        acceptor: acceptor.clone(),
        handle,
        dir: dir.clone(),
        web_base: ctx.global.web_base.clone(),
        swarm_url: swarm_url.clone(),
        gateway: gateway.clone(),
        book: Mutex::new(Book::default()),
    });

    // Resume what was there before the restart.
    {
        let mut book = anchor.book.lock().await;
        let jobs = load_jobs(&anchor.jobs_dir())?;
        for rec in jobs.values() {
            match rec.state {
                JobState::Running => {
                    if let Err(e) = anchor.start_job(&mut book, rec).await {
                        ctx.ui.note(&format!("job {} cannot resume: {e:#}", rec.id));
                    }
                }
                JobState::Done => {
                    if let Some(Ok(Link::File(l))) = rec.results.as_deref().map(Link::parse) {
                        acceptor.add(Offer {
                            access: l.access,
                            hash: l.hash,
                            size: l.size,
                        });
                    }
                }
            }
        }
        let resumed = book.core.len();
        book.jobs = jobs;
        book.sites = load_list(&dir.join("sites.json"))?;
        book.services = load_list(&dir.join("services.json"))?;
        let (sites, services) = (book.sites.clone(), book.services.clone());
        drop(book);
        for s in sites {
            let (status, body) = anchor
                .add_site(json!({ "link": s.link }).to_string().as_bytes(), false)
                .await;
            if status >= 300 {
                ctx.ui.note(&format!("site {} not restored: {body}", s.id));
            }
        }
        for s in services {
            let (status, body) = anchor
                .add_service(
                    json!({ "id": s.id, "link": s.link }).to_string().as_bytes(),
                    false,
                )
                .await;
            if status >= 300 {
                ctx.ui
                    .note(&format!("service {} not restored: {body}", s.id));
            }
        }
        if resumed > 0 {
            ctx.ui.emit("resumed", json!({ "jobs": resumed }));
            ctx.ui.note(&format!("resumed {resumed} running job(s)"));
        }
    }

    let admin = AdminHandler {
        anchor: anchor.clone(),
        secret: admin_secret,
        gate: Arc::new(Admission::new(AdmitConfig::default())),
    };
    let mut allow = allow;
    if let Some(a) = &mut allow {
        Arc::make_mut(a).insert(files_net.id());
    }
    let router = net
        .router()
        .accept(
            ALPN_JOB,
            Gatekeeper {
                driver,
                allow: allow.clone(),
            },
        )
        .accept(iroh_blobs::ALPN, hub.protocol())
        .accept(nuee_proto::ALPN_PIN, durability.clone())
        .accept(ALPN_HTTP, admin)
        .spawn();

    let gateway_addr = match (&gateway, args.gateway) {
        (Some(g), Some(addr)) => {
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .map_err(|e| fail(exit::ERROR, format!("cannot listen on {addr}: {e}")))?;
            let local = listener.local_addr()?;
            let g = g.clone();
            tokio::spawn(async move {
                if let Err(e) = g.serve(listener).await {
                    tracing::warn!("gateway stopped: {e}");
                }
            });
            Some(local)
        }
        _ => None,
    };

    let mut member_handle = match member {
        Some(q) => Some(store_member(&files_net, &files_hub, &job_link, q).await?),
        None => None,
    };

    ctx.ui.emit(
        "anchor",
        json!({
            "id": net.id().to_string(),
            "link": anchor_url,
            "swarm": swarm_url,
            "relay": relay_str,
            "gateway": gateway_addr.map(|a| format!("http://{a}")),
            "fingerprint": nuee_proto::fingerprint(&net.id()).to_vec(),
            "allowlist": allow.as_ref().map(|a| a.len()),
            "store": quota,
        }),
    );
    if !ctx.ui.is_json() {
        ctx.ui
            .say("anchor link (keep it secret: it carries the admin secret):");
        ctx.ui.say(&anchor_url);
        ctx.ui.say(&format!("swarm link: {swarm_url}"));
        ctx.ui.say(&format!(
            "fingerprint {}",
            nuee_proto::fingerprint_string(&net.id())
        ));
        if let Some(a) = gateway_addr {
            ctx.ui.say(&format!("gateway: http://{a}"));
        }
        if let Some((_, url)) = &relay_server {
            ctx.ui.say(&format!("relay: {url}"));
        }
    }

    let stop = stop_signal();
    tokio::pin!(stop);
    loop {
        let member_ev = async {
            match &mut member_handle {
                Some(h) => h.events.recv().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            () = &mut stop => break,
            ev = member_ev => match ev {
                Some(WorkerHostEvent::Ended(reason)) => {
                    tracing::warn!("the store member left the swarm ({reason:?})");
                    member_handle = None;
                }
                Some(WorkerHostEvent::Lost(why)) => {
                    tracing::warn!("the store member lost the swarm: {why}");
                    member_handle = None;
                }
                Some(_) => {}
                None => member_handle = None,
            },
            ev = events.recv() => {
                let Some(ev) = ev else {
                    return Err(anyhow::anyhow!("the coordinator stopped unexpectedly"));
                };
                // One failed event (a result bundle that cannot be written) must not stop
                // the anchor; it is reported and the next events are handled.
                if let Err(e) = anchor_event(&anchor, &durability, ctx, ev).await {
                    tracing::error!("{e:#}");
                    ctx.ui.note(&format!("error: {e:#}"));
                }
            }
        }
    }

    ctx.ui.emit("stopped", json!({}));
    if anchor.handle.shutdown().await.is_err() {
        tracing::debug!("the coordinator had already stopped");
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    if let Err(e) = router.shutdown().await {
        tracing::warn!("closing the endpoint: {e}");
    }
    if let Err(e) = files_router.shutdown().await {
        tracing::warn!("closing the files endpoint: {e}");
    }
    if let Some((server, _)) = relay_server
        && let Err(e) = server.shutdown().await
    {
        tracing::warn!("relay shutdown: {e}");
    }
    Ok(ExitCode::SUCCESS)
}

/// Ctrl-C, or SIGTERM from a service manager (systemd, Docker).
async fn stop_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    () = crate::ctrl_c() => {}
                    _ = term.recv() => {}
                }
                return;
            }
            Err(e) => tracing::warn!("cannot listen for SIGTERM: {e}"),
        }
    }
    crate::ctrl_c().await;
}

async fn anchor_event(
    anchor: &Anchor,
    durability: &nuee_store::durable::DurabilityService,
    ctx: &Ctx,
    ev: HostEvent,
) -> anyhow::Result<()> {
    match ev {
        HostEvent::Accepted {
            job,
            task,
            output,
            output_hash,
            ..
        } => anchor.accepted(job, task, &output, output_hash).await?,
        HostEvent::Event(e) => match e {
            Event::PeerJoined { peer, caps } => {
                durability.member_joined(peer, &caps);
                ctx.ui.emit(
                    "peer",
                    json!({ "id": peer.to_string(), "state": "joined", "agent": caps.agent.as_str() }),
                );
            }
            Event::PeerLeft { peer } => {
                durability.member_left(&peer);
                ctx.ui
                    .emit("peer", json!({ "id": peer.to_string(), "state": "left" }));
            }
            Event::PeerPending { peer, caps } => {
                let question = super::run::admit_question(&peer, &caps);
                let yes = tokio::task::block_in_place(|| ctx.ui.confirm(&question))?;
                anchor
                    .handle
                    .approve(peer, yes)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            }
            Event::Progress {
                job,
                done,
                total,
                verified,
                ..
            } => {
                let id = anchor.book.lock().await.core.get(&job).copied();
                ctx.ui.emit(
                    "progress",
                    json!({ "job": id, "done": done, "total": total, "verified": verified }),
                );
            }
            Event::Banned { peer, redo } => {
                durability.peer_banned(peer);
                ctx.ui
                    .emit("banned", json!({ "peer": peer.to_string(), "redo": redo }));
            }
            Event::WaitingForChecker {
                job, waiting, kind, ..
            } => {
                let id = anchor.book.lock().await.core.get(&job).copied();
                ctx.ui.emit(
                    "waiting_checker",
                    json!({ "job": id, "waiting": waiting, "kind": format!("{kind:?}") }),
                );
                ctx.ui.note(&super::run::waiting_checker_line(
                    waiting,
                    kind,
                    "submit it again with run --anchor --verify none",
                ));
            }
            Event::JobDone { job, failed } => {
                let id = anchor.book.lock().await.core.get(&job).copied();
                anchor.finished(job, failed).await?;
                ctx.ui
                    .emit("job_done", json!({ "job": id, "failed": failed }));
            }
            _ => {}
        },
        HostEvent::Closed { .. } => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Client side: `nuee run --anchor`, `nuee site publish --on`.

/// Reads an anchor link (`-` = stdin).
pub(crate) fn anchor_link(arg: &str) -> anyhow::Result<ServiceLink> {
    let text = crate::link_arg(arg)?;
    match Link::parse(&text).map_err(crate::link_failure)? {
        Link::Service(l) => Ok(l),
        _ => Err(fail(
            exit::USAGE,
            "not an anchor link (expected the link printed by `nuee serve --anchor`)",
        )),
    }
}

/// An endpoint for talking to an anchor: `--relay` when given, else the anchor's relay.
pub(crate) async fn client_net(ctx: &Ctx, link: &ServiceLink) -> anyhow::Result<Net> {
    if ctx.global.relay.is_some() || link.relay.as_str().is_empty() {
        return ctx.online_net().await;
    }
    let url: RelayUrl = link
        .relay
        .as_str()
        .parse()
        .map_err(|_| fail(exit::USAGE, "the anchor link names an invalid relay"))?;
    bind_online(
        ctx,
        ctx.secret()?,
        RelayChoice::Custom(vec![url]),
        ctx.global.relay_only,
    )
    .await
}

/// One admin request; returns the status and the JSON body.
pub(crate) async fn admin(
    net: &Net,
    link: &ServiceLink,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> anyhow::Result<(u16, Value)> {
    let mut headers = BoundedVec::new();
    if body.is_some() {
        headers
            .push(Header {
                name: BoundedString::try_from("content-type")?,
                value: BoundedString::try_from("application/json")?,
            })
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    let req = HttpRequest {
        proof: [0; 32],
        method: BoundedString::try_from(method)?,
        path: BoundedString::try_from(path)?,
        headers,
        body: BoundedBytes::try_from(body.map(Value::to_string).unwrap_or_default().into_bytes())
            .map_err(|_| fail(exit::USAGE, "request too large for the anchor"))?,
    };
    let resp = nuee_host::http_request(net, link, req)
        .await
        .map_err(|e| fail(exit::NETWORK, format!("the anchor did not answer: {e}")))?;
    let value: Value = serde_json::from_slice(resp.body.as_slice()).unwrap_or_else(
        |_| json!({ "error": String::from_utf8_lossy(resp.body.as_slice()).into_owned() }),
    );
    if resp.status == 403 || resp.status == 429 {
        return Err(fail(
            exit::REFUSED,
            format!(
                "the anchor refused the request: {}",
                value["error"].as_str().unwrap_or("wrong admin secret")
            ),
        ));
    }
    Ok((resp.status, value))
}

fn admin_error(status: u16, value: &Value) -> anyhow::Error {
    let msg = value["error"].as_str().unwrap_or("unexpected answer");
    let code = if status >= 500 {
        exit::NETWORK
    } else {
        exit::ERROR
    };
    fail(code, format!("the anchor answered {status}: {msg}"))
}

/// What `nuee run --anchor` hands over.
pub(crate) struct AnchorJob {
    /// Manifest with the anchor as coordinator.
    pub manifest: Manifest,
    /// Module bytes (empty for exec jobs).
    pub module: Vec<u8>,
    /// Asset bytes, in manifest order.
    pub assets: Vec<Vec<u8>>,
    /// One input per task.
    pub inputs: Vec<Vec<u8>>,
}

/// Uploads a job to an anchor; with `follow`, waits for it and downloads the results.
pub(crate) async fn submit_to_anchor(
    ctx: &Ctx,
    link: &ServiceLink,
    net: &Net,
    mut job: AnchorJob,
    follow: Option<PathBuf>,
) -> anyhow::Result<ExitCode> {
    job.manifest.coordinator = link.host;
    let manifest_bytes = nuee_proto::encode(&job.manifest)?;
    let bundle = make_bundle(&job.inputs)?;
    let hub = StoreHub::memory(net, HubConfig::default());
    if !job.module.is_empty() {
        hub.add_bytes(Bytes::from(std::mem::take(&mut job.module)))
            .await
            .map_err(crate::store_failure)?;
    }
    for a in std::mem::take(&mut job.assets) {
        hub.add_bytes(Bytes::from(a))
            .await
            .map_err(crate::store_failure)?;
    }
    let bundle_size = bundle.len() as u64;
    let bundle_hash = hub
        .add_bytes(Bytes::from(bundle))
        .await
        .map_err(crate::store_failure)?;
    hub.gate().admit(link.host);
    let router = net
        .router()
        .accept(iroh_blobs::ALPN, hub.protocol())
        .spawn();
    let sub = Submission {
        manifest: hex(&manifest_bytes),
        inputs: job.inputs.iter().map(|i| Hash::of(i).to_string()).collect(),
        bundle: BlobRef {
            hash: bundle_hash.to_string(),
            size: bundle_size,
        },
        provider: ProviderRef {
            id: net.id().to_string(),
            relay: Ctx::relay_string(net),
        },
    };
    let res = admin(
        net,
        link,
        "POST",
        "/jobs",
        Some(&serde_json::to_value(&sub)?),
    )
    .await;
    // The anchor has fetched every blob once it answers.
    hub.gate().revoke(link.host);
    let res = match res {
        Ok(r) => after_submit(ctx, link, net, r, follow).await,
        Err(e) => Err(e),
    };
    // Shutting the router down closes the endpoint too.
    if let Err(e) = router.shutdown().await {
        tracing::debug!("closing the endpoint: {e}");
    }
    res
}

async fn after_submit(
    ctx: &Ctx,
    link: &ServiceLink,
    net: &Net,
    (status, value): (u16, Value),
    follow: Option<PathBuf>,
) -> anyhow::Result<ExitCode> {
    if status != 200 && status != 201 {
        return Err(admin_error(status, &value));
    }
    let id = value["job"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("the anchor gave no job number"))?;
    let url = value["link"].as_str().unwrap_or_default().to_owned();
    ctx.ui.emit(
        "anchored",
        json!({ "job": id, "anchor": link.host.to_string(), "existing": value["existing"] }),
    );
    ctx.ui.show_link("job", &url, Some(&link.host), None)?;
    ctx.ui.note(&format!(
        "job {id} runs on the anchor {}; it keeps running without this process",
        link.host
    ));
    match follow {
        Some(out) => follow_job(ctx, link, net, id, &out).await,
        None => Ok(ExitCode::SUCCESS),
    }
}

/// Polls a job on the anchor until it is done, then downloads its results bundle.
async fn follow_job(
    ctx: &Ctx,
    link: &ServiceLink,
    net: &Net,
    id: u64,
    out: &Path,
) -> anyhow::Result<ExitCode> {
    let bar = ctx.ui.bar(0, "tasks");
    let stop = crate::ctrl_c();
    tokio::pin!(stop);
    let summary = loop {
        let (status, v) = admin(net, link, "GET", &format!("/jobs/{id}"), None).await?;
        if status != 200 {
            return Err(admin_error(status, &v));
        }
        let (done, total) = (
            v["done"].as_u64().unwrap_or(0),
            v["tasks"].as_u64().unwrap_or(0),
        );
        bar.set_length(total);
        bar.set_position(done);
        ctx.ui.emit(
            "progress",
            json!({ "job": id, "done": done, "total": total }),
        );
        if v["state"] == "done" {
            break v;
        }
        tokio::select! {
            () = &mut stop => {
                bar.finish_and_clear();
                return Err(fail(exit::CANCELLED, format!(
                    "stopped following; job {id} keeps running on the anchor")));
            }
            () = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    };
    bar.finish_and_clear();
    let (status, v) = admin(net, link, "GET", &format!("/jobs/{id}/results"), None).await?;
    if status != 200 {
        return Err(admin_error(status, &v));
    }
    let results = match Link::parse(v["link"].as_str().unwrap_or_default()) {
        Ok(Link::File(l)) => l,
        _ => return Err(fail(exit::ERROR, "the anchor gave no results link")),
    };
    std::fs::create_dir_all(out)
        .map_err(|e| fail(exit::ERROR, format!("cannot create {}: {e}", out.display())))?;
    let hub = StoreHub::memory(net, HubConfig::default());
    let report = nuee_store::get(net, &hub, &results, out, true, |_, _| {})
        .await
        .map_err(crate::store_failure)?;
    let failed = summary["failed"].as_u64().unwrap_or(0);
    ctx.ui.emit(
        "done",
        json!({
            "job": id,
            "tasks": summary["tasks"],
            "out": out.display().to_string(),
            "files": report.files.len(),
            "failed": failed,
        }),
    );
    ctx.ui.say(&format!(
        "done: {} files in {}",
        report.files.len(),
        out.display()
    ));
    if failed > 0 {
        return Err(fail(
            exit::ERROR,
            format!(
                "{failed} tasks failed for good; the others are in {}",
                out.display()
            ),
        ));
    }
    Ok(ExitCode::SUCCESS)
}

/// Hands a published site to an anchor's gateway. The anchor gets the full link, key
/// included, so it sees the site's plaintext.
pub(crate) async fn publish_on_anchor(
    ctx: &Ctx,
    net: &Net,
    anchor: &ServiceLink,
    site_url: &str,
) -> anyhow::Result<String> {
    let (status, v) = admin(
        net,
        anchor,
        "POST",
        "/sites",
        Some(&json!({ "link": site_url })),
    )
    .await?;
    if status != 200 && status != 201 {
        return Err(admin_error(status, &v));
    }
    let path = v["path"].as_str().unwrap_or_default().to_owned();
    ctx.ui
        .emit("anchored", json!({ "id": v["id"], "path": path }));
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundles_round_trip() {
        let inputs = vec![b"a".to_vec(), Vec::new(), vec![7u8; 300]];
        let b = make_bundle(&inputs).expect("bundle");
        assert_eq!(split_bundle(&b), Some(inputs));
        assert_eq!(split_bundle(&[1, 0, 0]), None);
        assert_eq!(split_bundle(&[5, 0, 0, 0, 1]), None);
    }

    #[test]
    fn a_single_folder_is_served_at_the_root() {
        let files = vec![
            ("site/index.html".to_owned(), Vec::new()),
            ("site/css/a.css".to_owned(), Vec::new()),
        ];
        let names: Vec<String> = strip_root(files).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["index.html", "css/a.css"]);
        let mixed = vec![("a/x".to_owned(), Vec::new()), ("b".to_owned(), Vec::new())];
        assert_eq!(strip_root(mixed.clone()), mixed);
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(unhex(&hex(&[0, 255, 16])), Some(vec![0, 255, 16]));
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
    }
}
