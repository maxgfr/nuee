//! wasm-bindgen exports. Everything here runs inside the dedicated "iroh worker"; task
//! execution is delegated to a JavaScript `Executor` callback that forwards leases to the task
//! workers owned by the main thread. The contract is `web/app/api.d.ts`.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use bytes::Bytes;
use iroh::protocol::Router;
use js_sys::{Array, Function, Object, Promise, Reflect, Uint8Array};
use nuee_core::{
    CoordinatorConfig, CoreError, Event, JobSpec, Outcome, RtcKind, SwarmStatus, WorkerConfig,
    WorkerEvent,
};
use nuee_net::driver::{BoxFut, SubmitError};
use nuee_net::{
    CoordinatorDriver, CoordinatorHandle, DriverConfig, HostEvent, JoinError, Net, NetConfig,
    ONLINE_TIMEOUT, RelayChoice, TaskExec, WorkerHandle, WorkerHostEvent,
};
use nuee_proto::limits::{MAX_INLINE_INPUT, MAX_WASM_MEMORY};
use nuee_proto::link::DEFAULT_BASE;
use nuee_proto::{
    ALPN_HTTP, ALPN_JOB, Asset, BoundedBytes, BoundedString, BoundedVec, Caps, Comparator,
    EndpointId, FailKind, GpuCaps, Hash, Header, HttpRequest, HttpResponse, JobLink, Lease, Link,
    MAX_HTTP_MSG, Manifest, PROOF_HTTP, PROTO_VERSION, Payload, SecretKey, TaskKind, VerifyMode,
    VerifyPolicy, decode_with_limit,
};
use nuee_store::{HubConfig, ShareHandle, StoreHub, get_bytes, share_bytes};
use tokio::sync::mpsc;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, future_to_promise, spawn_local};

use crate::{
    VERSION, WebError, browser_kind, bye_name, code, fail_from_name, fail_name, fingerprint_of,
    kind_from_name, kind_name, link_info, parse_id,
};

mod train;

/// Memory granted to non-module kinds when the spec does not say.
const DEFAULT_KIND_MEMORY: u64 = 256 * 1024 * 1024;

// ---------------------------------------------------------------------------------------------
// Conversions.

impl From<WebError> for JsValue {
    fn from(e: WebError) -> JsValue {
        Obj::new()
            .set("code", e.code)
            .set("message", e.message)
            .into()
    }
}

fn js_err(e: impl Into<WebError>) -> JsValue {
    e.into().into()
}

/// Plain JavaScript object under construction.
struct Obj(Object);

impl Obj {
    fn new() -> Obj {
        Obj(Object::new())
    }

    fn set(self, key: &str, value: impl Into<JsValue>) -> Obj {
        // Defining a data property on a fresh ordinary object cannot fail.
        if Reflect::set(&self.0, &JsValue::from_str(key), &value.into()).is_err() {
            web_sys::console::error_1(&JsValue::from_str(&format!("cannot set `{key}`")));
        }
        self
    }
}

impl From<Obj> for JsValue {
    fn from(o: Obj) -> JsValue {
        o.0.into()
    }
}

fn bytes_js(b: &[u8]) -> Uint8Array {
    Uint8Array::from(b)
}

fn strings_js<S: AsRef<str>>(items: &[S]) -> Array {
    items
        .iter()
        .map(|s| JsValue::from_str(s.as_ref()))
        .collect()
}

fn describe(v: &JsValue) -> String {
    if let Some(s) = v.as_string() {
        return s;
    }
    if let Ok(m) = Reflect::get(v, &JsValue::from_str("message"))
        && let Some(s) = m.as_string()
    {
        return s;
    }
    format!("{v:?}")
}

/// Field `key` of `obj`; `None` when absent, `undefined` or `null`.
fn field(obj: &JsValue, key: &str) -> Result<Option<JsValue>, WebError> {
    if !obj.is_object() {
        return Err(WebError::invalid(format!(
            "expected an object with `{key}`"
        )));
    }
    let v = Reflect::get(obj, &JsValue::from_str(key))
        .map_err(|e| WebError::invalid(format!("cannot read `{key}`: {}", describe(&e))))?;
    Ok(if v.is_undefined() || v.is_null() {
        None
    } else {
        Some(v)
    })
}

fn opt_str(obj: &JsValue, key: &str) -> Result<Option<String>, WebError> {
    match field(obj, key)? {
        None => Ok(None),
        Some(v) => v
            .as_string()
            .map(Some)
            .ok_or_else(|| WebError::invalid(format!("`{key}` must be a string"))),
    }
}

fn req_str(obj: &JsValue, key: &str) -> Result<String, WebError> {
    opt_str(obj, key)?.ok_or_else(|| WebError::invalid(format!("`{key}` is required")))
}

fn opt_bool(obj: &JsValue, key: &str) -> Result<Option<bool>, WebError> {
    match field(obj, key)? {
        None => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| WebError::invalid(format!("`{key}` must be a boolean"))),
    }
}

fn number_in(v: &JsValue, key: &str, max: f64) -> Result<f64, WebError> {
    match v.as_f64() {
        Some(n) if n.is_finite() && n >= 0.0 && n.fract() == 0.0 && n <= max => Ok(n),
        _ => Err(WebError::invalid(format!(
            "`{key}` must be an integer between 0 and {max}"
        ))),
    }
}

fn opt_u64(obj: &JsValue, key: &str, max: u64) -> Result<Option<u64>, WebError> {
    match field(obj, key)? {
        None => Ok(None),
        // Every value below 2^53 is exact as an f64, and the caps used here are below it.
        Some(v) => Ok(Some(number_in(&v, key, max as f64)? as u64)),
    }
}

fn opt_u32(obj: &JsValue, key: &str) -> Result<Option<u32>, WebError> {
    Ok(opt_u64(obj, key, u64::from(u32::MAX))?.and_then(|n| u32::try_from(n).ok()))
}

fn opt_u16(obj: &JsValue, key: &str) -> Result<Option<u16>, WebError> {
    Ok(opt_u64(obj, key, u64::from(u16::MAX))?.and_then(|n| u16::try_from(n).ok()))
}

fn opt_f32(obj: &JsValue, key: &str) -> Result<Option<f32>, WebError> {
    match field(obj, key)? {
        None => Ok(None),
        Some(v) => match v.as_f64() {
            Some(n) if n.is_finite() => Ok(Some(n as f32)),
            _ => Err(WebError::invalid(format!(
                "`{key}` must be a finite number"
            ))),
        },
    }
}

fn to_bytes(v: JsValue, what: &str) -> Result<Vec<u8>, WebError> {
    v.dyn_into::<Uint8Array>()
        .map(|a| a.to_vec())
        .map_err(|_| WebError::invalid(format!("{what} must be a Uint8Array")))
}

fn opt_bytes(obj: &JsValue, key: &str) -> Result<Option<Vec<u8>>, WebError> {
    field(obj, key)?
        .map(|v| to_bytes(v, &format!("`{key}`")))
        .transpose()
}

fn list(obj: &JsValue, key: &str) -> Result<Vec<JsValue>, WebError> {
    match field(obj, key)? {
        None => Ok(Vec::new()),
        Some(v) if Array::is_array(&v) => Ok(Array::from(&v).iter().collect()),
        Some(_) => Err(WebError::invalid(format!("`{key}` must be an array"))),
    }
}

fn bytes_list(obj: &JsValue, key: &str) -> Result<Vec<Vec<u8>>, WebError> {
    list(obj, key)?
        .into_iter()
        .map(|v| to_bytes(v, &format!("every item of `{key}`")))
        .collect()
}

fn str_list(obj: &JsValue, key: &str) -> Result<Vec<String>, WebError> {
    list(obj, key)?
        .into_iter()
        .map(|v| {
            v.as_string()
                .ok_or_else(|| WebError::invalid(format!("`{key}` must hold strings")))
        })
        .collect()
}

fn bounded<const N: usize>(s: &str, what: &str) -> Result<BoundedString<N>, WebError> {
    BoundedString::try_from(s)
        .map_err(|_| WebError::new(code::TOO_LARGE, format!("{what} is longer than {N} bytes")))
}

fn rtc_kind(name: &str) -> Result<RtcKind, WebError> {
    match name {
        "offer" => Ok(RtcKind::Offer),
        "answer" => Ok(RtcKind::Answer),
        "ice" => Ok(RtcKind::Ice),
        _ => Err(WebError::invalid(format!(
            "unknown signalling kind {name:?}"
        ))),
    }
}

fn rtc_name(kind: RtcKind) -> &'static str {
    match kind {
        RtcKind::Offer => "offer",
        RtcKind::Answer => "answer",
        RtcKind::Ice => "ice",
    }
}

fn core_err(e: &CoreError) -> WebError {
    let code = match e {
        CoreError::MissingComparator => code::UNSUPPORTED,
        CoreError::TooManyJobs(_) | CoreError::SignalTooLarge => code::TOO_LARGE,
        CoreError::TaskCount { .. } | CoreError::Manifest(_) => code::INVALID_ARGUMENT,
        CoreError::NotConnected => code::SWARM_ENDED,
    };
    WebError::new(code, e.to_string())
}

fn submit_err(e: &SubmitError) -> WebError {
    match e {
        SubmitError::Core(c) => core_err(c),
        SubmitError::Stopped(s) => WebError::new(code::SWARM_ENDED, s.to_string()),
    }
}

fn stopped(e: impl std::fmt::Display) -> JsValue {
    js_err(WebError::new(code::SWARM_ENDED, e.to_string()))
}

fn random_err(e: impl std::fmt::Display) -> WebError {
    WebError::internal(format!("random source: {e}"))
}

fn id_js(id: &EndpointId) -> JsValue {
    JsValue::from_str(&id.to_string())
}

// ---------------------------------------------------------------------------------------------
// Event sinks and the JavaScript executor.

/// A JavaScript callback receiving plain objects.
#[derive(Clone)]
struct Sink(Function);

impl Sink {
    fn emit(&self, value: JsValue) {
        if let Err(e) = self.0.call1(&JsValue::NULL, &value) {
            web_sys::console::error_2(&JsValue::from_str("event handler threw"), &e);
        }
    }

    fn event(&self, ty: &str, o: Obj) {
        self.emit(o.set("type", ty).into());
    }
}

/// Runs leases through the JavaScript `Executor` callback.
struct JsExec {
    f: Function,
    seen: RefCell<HashSet<Hash>>,
    sink: Sink,
}

impl JsExec {
    fn new(f: Function, sink: Sink) -> JsExec {
        JsExec {
            f,
            seen: RefCell::new(HashSet::new()),
            sink,
        }
    }

    async fn call(
        &self,
        lease: &Lease,
        manifest: &Manifest,
        module: Option<Bytes>,
        input: Bytes,
    ) -> Outcome {
        if !browser_kind(lease.kind) {
            return Outcome::Fail(
                FailKind::Unsupported,
                format!("{} tasks cannot run in a browser", kind_name(lease.kind)),
            );
        }
        let mut req = Obj::new()
            .set("lease", lease.lease.to_string())
            .set("job", lease.job)
            .set("task", lease.task)
            .set("kind", kind_name(lease.kind))
            .set("input", bytes_js(&input))
            .set("deadlineMs", lease.deadline_ms)
            .set("maxMemory", manifest.max_memory as f64);
        if let Some(hash) = lease.module {
            req = req.set("moduleHash", hash.to_string());
            if let Some(m) = module
                && self.seen.borrow_mut().insert(hash)
            {
                req = req.set("module", bytes_js(&m));
            }
        }
        let lease_id = lease.lease.to_string();
        self.sink
            .event("taskStarted", Obj::new().set("lease", lease_id.as_str()));
        let started = js_sys::Date::now();
        let out = match self.f.call1(&JsValue::NULL, &req.into()) {
            Ok(v) => match JsFuture::from(Promise::resolve(&v)).await {
                Ok(o) => parse_outcome(&o),
                Err(e) => Outcome::Fail(FailKind::Io, format!("executor: {}", describe(&e))),
            },
            Err(e) => Outcome::Fail(FailKind::Io, format!("executor: {}", describe(&e))),
        };
        self.sink.event(
            "taskDone",
            Obj::new()
                .set("lease", lease_id)
                .set("ms", js_sys::Date::now() - started)
                .set("ok", matches!(out, Outcome::Ok(_))),
        );
        out
    }
}

fn parse_outcome(v: &JsValue) -> Outcome {
    let parsed = (|| -> Result<Outcome, WebError> {
        if opt_bool(v, "ok")?.unwrap_or(false) {
            let out =
                opt_bytes(v, "output")?.ok_or_else(|| WebError::invalid("`output` is required"))?;
            Ok(Outcome::Ok(out))
        } else {
            let kind = req_str(v, "kind")?;
            let kind = fail_from_name(&kind)
                .ok_or_else(|| WebError::invalid(format!("unknown failure kind {kind:?}")))?;
            Ok(Outcome::Fail(
                kind,
                opt_str(v, "detail")?.unwrap_or_default(),
            ))
        }
    })();
    parsed.unwrap_or_else(|e| {
        Outcome::Fail(
            FailKind::Io,
            format!("malformed executor outcome: {}", e.message),
        )
    })
}

impl TaskExec for JsExec {
    fn run<'a>(
        &'a self,
        lease: &'a Lease,
        manifest: &'a Manifest,
        module: Option<Bytes>,
        input: Bytes,
    ) -> BoxFut<'a, Outcome> {
        Box::pin(self.call(lease, manifest, module, input))
    }
}

/// The driver shares its executor through an `Arc`; on wasm32 everything runs on the one
/// worker thread, so `TaskExec` asks for neither `Send` nor `Sync` there.
fn exec_of(executor: JsValue, sink: &Sink) -> Result<Arc<dyn TaskExec>, WebError> {
    let f = executor
        .dyn_into::<Function>()
        .map_err(|_| WebError::invalid("the executor must be a function"))?;
    let exec: Box<dyn TaskExec> = Box::new(JsExec::new(f, sink.clone()));
    Ok(Arc::from(exec))
}

fn sink_of(f: JsValue, what: &str) -> Result<Sink, WebError> {
    f.dyn_into::<Function>()
        .map(Sink)
        .map_err(|_| WebError::invalid(format!("{what} must be a function")))
}

// ---------------------------------------------------------------------------------------------
// Events.

fn status_js(s: &SwarmStatus) -> JsValue {
    let peers: Array = s
        .peers
        .iter()
        .map(|p| -> JsValue {
            Obj::new()
                .set("peer", id_js(&p.id))
                .set("fingerprint", strings_js(&fingerprint_of(&p.id)))
                .set("browser", p.browser)
                .set("slots", p.slots)
                .set("inflight", p.inflight)
                .set("done", p.done)
                .set("verified", p.verified)
                .set("banned", p.banned)
                .into()
        })
        .collect();
    let jobs: Array = s
        .jobs
        .iter()
        .map(|j| -> JsValue {
            Obj::new()
                .set("job", j.job)
                .set("name", j.name.as_str())
                .set("total", j.total)
                .set("done", j.done)
                .set("verified", j.verified)
                .set("failed", j.failed)
                .set("inFlight", j.in_flight)
                .set("rate", j.rate_per_s)
                .into()
        })
        .collect();
    Obj::new().set("peers", peers).set("jobs", jobs).into()
}

fn core_event(sink: &Sink, ev: Event) {
    match ev {
        Event::PeerJoined { peer, caps } => sink.event(
            "peerJoined",
            Obj::new()
                .set("peer", id_js(&peer))
                .set("fingerprint", strings_js(&fingerprint_of(&peer)))
                .set("browser", caps.browser)
                .set("slots", caps.cpu_slots),
        ),
        Event::PeerPending { peer, caps } => sink.event(
            "peerPending",
            Obj::new()
                .set("peer", id_js(&peer))
                .set("fingerprint", strings_js(&fingerprint_of(&peer)))
                .set("browser", caps.browser),
        ),
        Event::PeerLeft { peer } => sink.event("peerLeft", Obj::new().set("peer", id_js(&peer))),
        // `accepted` carries the output; this one only duplicates it.
        Event::TaskAccepted { .. } => {}
        Event::Verified { job, task, by } => sink.event(
            "verified",
            Obj::new()
                .set("job", job)
                .set("task", task)
                .set("by", id_js(&by)),
        ),
        Event::Mismatch { job, task, peer } => sink.event(
            "mismatch",
            Obj::new()
                .set("job", job)
                .set("task", task)
                .set("peer", id_js(&peer)),
        ),
        Event::Banned { peer, redo } => sink.event(
            "banned",
            Obj::new().set("peer", id_js(&peer)).set("redo", redo),
        ),
        Event::WaitingForChecker {
            job,
            task,
            waiting,
            kind,
        } => sink.event(
            "waitingForChecker",
            Obj::new()
                .set("job", job)
                .set("task", task)
                .set("waiting", waiting)
                .set("kind", crate::kind_name(kind)),
        ),
        Event::TaskFailed { job, task, kind } => sink.event(
            "taskFailed",
            Obj::new()
                .set("job", job)
                .set("task", task)
                .set("kind", fail_name(kind)),
        ),
        Event::Progress {
            job,
            done,
            total,
            verified,
            rate_per_s,
        } => sink.event(
            "progress",
            Obj::new()
                .set("job", job)
                .set("done", done)
                .set("total", total)
                .set("verified", verified)
                .set("rate", rate_per_s),
        ),
        Event::JobDone { job, failed } => {
            sink.event("jobDone", Obj::new().set("job", job).set("failed", failed))
        }
        Event::Rtc {
            from,
            kind,
            payload,
        } => rtc_event(sink, &from, kind, payload),
    }
}

/// Hands a signalling message to the page, unless it fails the checks of `nuee_net::rtc`.
fn rtc_event(sink: &Sink, from: &EndpointId, kind: RtcKind, payload: String) {
    if let Err(e) = nuee_net::rtc::check_signal(kind, &payload) {
        web_sys::console::debug_1(&JsValue::from_str(&format!(
            "signalling from {from} dropped: {e}"
        )));
        return;
    }
    sink.event(
        "rtc",
        Obj::new()
            .set("peer", id_js(from))
            .set("kind", rtc_name(kind))
            .set("payload", payload),
    );
}

fn host_event(sink: &Sink, ev: HostEvent) {
    match ev {
        HostEvent::Accepted {
            job,
            task,
            output,
            output_hash,
            by,
            ms,
        } => {
            let ev = crate::AcceptedEvent::new(job, task, &by, &output_hash, ms);
            sink.event(
                "accepted",
                Obj::new()
                    .set("job", ev.job)
                    .set("task", ev.task)
                    .set("by", ev.by)
                    .set("hash", ev.hash)
                    .set("ms", ev.ms)
                    .set("output", bytes_js(&output)),
            )
        }
        HostEvent::Event(e) => core_event(sink, e),
        // `peerLeft` (core) already reports the departure.
        HostEvent::Closed { .. } => {}
    }
}

async fn pump_host(mut rx: mpsc::UnboundedReceiver<HostEvent>, sink: Sink) {
    while let Some(ev) = rx.recv().await {
        host_event(&sink, ev);
    }
    sink.event("ended", Obj::new().set("reason", "shutdown"));
}

fn worker_event(sink: &Sink, ev: WorkerEvent) {
    match ev {
        WorkerEvent::Pending => sink.event("pending", Obj::new()),
        WorkerEvent::Welcomed {
            swarm,
            webrtc,
            stun,
        } => sink.event(
            "welcome",
            Obj::new().set(
                "swarm",
                Obj::new()
                    .set("name", swarm.name.as_str())
                    .set("coordinator", id_js(&swarm.coordinator))
                    .set(
                        "fingerprint",
                        strings_js(&fingerprint_of(&swarm.coordinator)),
                    )
                    .set("webrtc", webrtc)
                    .set("stun", strings_js(&stun)),
            ),
        ),
        WorkerEvent::JobAdded { job, name, .. } => {
            sink.event("jobAdded", Obj::new().set("job", job).set("name", name));
        }
        WorkerEvent::JobRejected { reason } => {
            sink.event("jobRejected", Obj::new().set("reason", reason));
        }
        WorkerEvent::LeaseRefused { lease, kind } => sink.event(
            "leaseRefused",
            Obj::new()
                .set("lease", lease.to_string())
                .set("kind", fail_name(kind)),
        ),
        WorkerEvent::Rtc {
            from,
            kind,
            payload,
        } => rtc_event(sink, &from, kind, payload),
    }
}

async fn pump_worker(mut rx: mpsc::UnboundedReceiver<WorkerHostEvent>, sink: Sink) {
    while let Some(ev) = rx.recv().await {
        match ev {
            // Details follow in `WorkerEvent::Welcomed`.
            WorkerHostEvent::Welcomed => {}
            WorkerHostEvent::Event(e) => worker_event(&sink, e),
            WorkerHostEvent::Ended(reason) => {
                sink.event("ended", Obj::new().set("reason", bye_name(reason)));
                return;
            }
            WorkerHostEvent::Lost(why) => {
                sink.event(
                    "error",
                    Obj::new()
                        .set("code", code::SWARM_ENDED)
                        .set("message", why),
                );
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Job specs.

struct JobInput {
    name: String,
    kind: TaskKind,
    module: Option<Vec<u8>>,
    max_memory: u64,
    assets: Vec<Vec<u8>>,
    inputs: Vec<Vec<u8>>,
    verify: VerifyPolicy,
    compare: Comparator,
    deadline_ms: u32,
    /// A DiLoCo run (`trainWasm` with `train`), driven by [`train`].
    train: Option<train::Setup>,
}

fn parse_job(spec: &JsValue) -> Result<JobInput, WebError> {
    let name = req_str(spec, "name")?;
    let kind_s = req_str(spec, "kind")?;
    let kind = kind_from_name(&kind_s)
        .ok_or_else(|| WebError::invalid(format!("unknown task kind {kind_s:?}")))?;
    if !browser_kind(kind) {
        return Err(WebError::new(
            code::UNSUPPORTED,
            format!("{kind_s} jobs cannot be hosted from a browser"),
        ));
    }
    let train = train::parse(spec, kind)?;
    let module = opt_bytes(spec, "module")?;
    let max_memory = match (kind, &module) {
        (TaskKind::Wasm | TaskKind::TrainWasm, Some(m)) => crate::validate(m)?.max_memory,
        (TaskKind::Wasm | TaskKind::TrainWasm, None) => {
            return Err(WebError::invalid("wasm jobs need a `module`"));
        }
        _ => opt_u64(spec, "maxMemory", MAX_WASM_MEMORY)?.unwrap_or(DEFAULT_KIND_MEMORY),
    };
    let mut verify = VerifyPolicy::default();
    if let Some(mode) = opt_str(spec, "verify")? {
        verify.mode = match mode.as_str() {
            "full" => VerifyMode::Full,
            "sample" => VerifyMode::Sample,
            "none" => VerifyMode::None,
            _ => return Err(WebError::invalid(format!("unknown verify mode {mode:?}"))),
        };
    }
    if let Some(p) = opt_u16(spec, "sample")? {
        verify.sample_permille = p.min(1000);
    }
    if let Some(p) = opt_u16(spec, "canary")? {
        verify.canary_permille = p.min(1000);
    }
    let compare = match field(spec, "compare")? {
        None => Comparator::Exact,
        Some(c) => match req_str(&c, "kind")?.as_str() {
            "exact" => Comparator::Exact,
            "tolerance" => Comparator::ElementTolerance {
                abs: opt_f32(&c, "abs")?.unwrap_or(0.0),
                rel: opt_f32(&c, "rel")?.unwrap_or(0.0),
                max_bad_permille: opt_u16(&c, "maxBad")?.unwrap_or(0),
            },
            "cosine" => Comparator::Cosine {
                min: opt_f32(&c, "minCos")?.unwrap_or(0.999),
            },
            "logits" => Comparator::Logits {
                max_gap: opt_f32(&c, "maxGap")?.unwrap_or(0.0),
            },
            other => return Err(WebError::invalid(format!("unknown comparator {other:?}"))),
        },
    };
    Ok(JobInput {
        name,
        kind,
        module,
        max_memory,
        assets: bytes_list(spec, "assets")?,
        inputs: bytes_list(spec, "inputs")?,
        verify,
        compare,
        deadline_ms: opt_u32(spec, "deadlineMs")?
            .unwrap_or(nuee_proto::limits::DEFAULT_DEADLINE_MS),
        train,
    })
}

async fn publish(hub: &StoreHub, bytes: Vec<u8>) -> Result<Hash, WebError> {
    hub.add_bytes(Bytes::from(bytes))
        .await
        .map_err(|e| WebError::from(&e))
}

async fn submit_job(host: &Host, job: JobInput) -> Result<u32, WebError> {
    if job.inputs.is_empty() {
        return Err(WebError::invalid("a job needs at least one input"));
    }
    let (module, module_size) = match job.module {
        Some(m) => {
            let size = u32::try_from(m.len())
                .map_err(|_| WebError::new(code::TOO_LARGE, "module too large"))?;
            (Some(publish(&host.hub, m).await?), size)
        }
        None => (None, 0),
    };
    let mut assets = BoundedVec::new();
    for (i, a) in job.assets.into_iter().enumerate() {
        let size = a.len() as u64;
        let hash = publish(&host.hub, a).await?;
        assets
            .push(Asset {
                hash,
                size,
                name: bounded(&format!("asset-{i}"), "asset name")?,
            })
            .map_err(|_| WebError::new(code::TOO_LARGE, "too many assets"))?;
    }
    let tasks = u32::try_from(job.inputs.len())
        .map_err(|_| WebError::new(code::TOO_LARGE, "too many inputs"))?;
    let mut inputs = Vec::with_capacity(job.inputs.len());
    for input in job.inputs {
        let payload = if input.len() <= MAX_INLINE_INPUT {
            let inline = BoundedBytes::try_from(input.as_slice())
                .map_err(|_| WebError::internal("inline input above its cap"))?;
            Payload::Inline(inline)
        } else {
            let size = input.len() as u64;
            Payload::Blob {
                hash: publish(&host.hub, input).await?,
                size,
            }
        };
        inputs.push(payload);
    }
    let manifest = Manifest {
        version: PROTO_VERSION,
        job: 0,
        name: bounded(&job.name, "the job name")?,
        kind: job.kind,
        module,
        module_size,
        max_memory: job.max_memory,
        assets,
        tasks,
        verify: job.verify,
        compare: job.compare,
        deadline_ms: job.deadline_ms,
        exec: None,
        train: job.train.map(|t| t.spec),
        coordinator: host.net.id(),
    };
    // Element tolerances come from nuee-gpu; comparators nobody provides yet make the core
    // refuse the job with `MissingComparator` (code `unsupported`).
    let compare = nuee_gpu::verify::comparator(manifest.kind, &manifest.compare);
    host.handle
        .submit(JobSpec {
            manifest,
            inputs,
            compare,
        })
        .await
        .map_err(|e| submit_err(&e))
}

// ---------------------------------------------------------------------------------------------
// Free functions.

/// Installs the panic hook once the module is instantiated.
#[wasm_bindgen(start)]
fn install_panic_hook() {
    console_error_panic_hook::set_once();
}

/// Version of the package (`Cargo.toml`).
#[wasm_bindgen]
pub fn version() -> String {
    VERSION.to_owned()
}

/// What a link points to (no secret is returned).
#[wasm_bindgen(js_name = parseLink)]
pub fn parse_link(link: &str) -> Result<JsValue, JsValue> {
    let info = link_info(link).map_err(js_err)?;
    let mut o = Obj::new()
        .set("kind", info.kind)
        .set("peer", id_js(&info.peer))
        .set("fingerprint", strings_js(&fingerprint_of(&info.peer)))
        .set("relay", info.relay);
    if let Some(h) = info.hash {
        o = o.set("hash", h);
    }
    if let Some(s) = info.size {
        o = o.set("size", s as f64);
    }
    let providers: Vec<String> = info.providers.iter().map(|p| p.to_string()).collect();
    o = o.set("providers", strings_js(&providers));
    Ok(o.into())
}

/// The six emoji of an endpoint id.
#[wasm_bindgen]
pub fn fingerprint(endpoint_id: &str) -> Result<Vec<String>, JsValue> {
    let id = parse_id(endpoint_id).map_err(js_err)?;
    Ok(fingerprint_of(&id))
}

/// SVG markup of a QR code; parse it with `DOMParser`, never `innerHTML`.
#[wasm_bindgen(js_name = qrSvg)]
pub fn qr_svg(text: &str) -> Result<String, JsValue> {
    crate::qr_svg(text).map_err(js_err)
}

/// Checks a task module against the task ABI.
#[wasm_bindgen(js_name = validateModule)]
pub fn validate_module(bytes: &[u8]) -> Result<JsValue, JsValue> {
    let info = crate::validate(bytes).map_err(js_err)?;
    Ok(Obj::new()
        .set("hash", info.hash.to_string())
        .set("size", info.size as f64)
        .set("minMemory", info.min_memory as f64)
        .set("maxMemory", info.max_memory as f64)
        .into())
}

/// A fresh 16-byte secret (hex) and a fresh endpoint id; proves the random source works.
#[wasm_bindgen(js_name = newSecret)]
pub fn new_secret() -> Result<JsValue, JsValue> {
    let secret = nuee_proto::new_secret16().map_err(|e| js_err(random_err(e)))?;
    let hex: String = secret.iter().map(|b| format!("{b:02x}")).collect();
    let id = SecretKey::generate().public();
    Ok(Obj::new().set("secret", hex).set("id", id_js(&id)).into())
}

// ---------------------------------------------------------------------------------------------
// Node.

struct NodeCfg {
    relay: RelayChoice,
    base: String,
}

impl NodeCfg {
    fn from_js(opts: &JsValue) -> Result<NodeCfg, WebError> {
        let opts = if opts.is_undefined() || opts.is_null() {
            JsValue::from(Object::new())
        } else {
            opts.clone()
        };
        let relay = match opt_str(&opts, "relay")?.as_deref() {
            None | Some("n0") => RelayChoice::N0,
            Some("disabled") => RelayChoice::Disabled,
            Some(url) => {
                RelayChoice::Custom(vec![url.parse().map_err(|e| {
                    WebError::invalid(format!("invalid relay URL {url:?}: {e}"))
                })?])
            }
        };
        let base = opt_str(&opts, "linkBase")?.unwrap_or_else(|| DEFAULT_BASE.to_owned());
        Ok(NodeCfg { relay, base })
    }
}

async fn bind(relay: &RelayChoice) -> Result<Net, WebError> {
    let mut cfg = NetConfig::new(SecretKey::generate());
    cfg.relay = relay.clone();
    cfg.relay_only = true;
    let net = Net::bind(cfg).await.map_err(|e| WebError::from(&e))?;
    net.online(ONLINE_TIMEOUT)
        .await
        .map_err(|e| WebError::from(&e))?;
    Ok(net)
}

fn relay_of(net: &Net) -> String {
    net.home_relay().map(|u| u.to_string()).unwrap_or_default()
}

struct Node {
    net: Net,
    hub: StoreHub,
    router: Router,
    relay: RelayChoice,
    base: String,
}

/// One browser node: joins swarms and downloads; `host` and `share` get endpoints of their own.
#[wasm_bindgen]
pub struct NueeNode {
    inner: Rc<Node>,
}

#[wasm_bindgen]
impl NueeNode {
    /// Binds an endpoint (relay only) and waits until the relay accepted it (10 s at most).
    pub async fn start(opts: JsValue) -> Result<NueeNode, JsValue> {
        let cfg = NodeCfg::from_js(&opts).map_err(js_err)?;
        let net = bind(&cfg.relay).await.map_err(js_err)?;
        let hub = StoreHub::memory(&net, HubConfig::default());
        // Workers publish large outputs; the coordinator fetches them from us.
        let router = net
            .router()
            .accept(iroh_blobs::ALPN, hub.protocol())
            .spawn();
        Ok(NueeNode {
            inner: Rc::new(Node {
                net,
                hub,
                router,
                relay: cfg.relay,
                base: cfg.base,
            }),
        })
    }

    /// Endpoint id of this node.
    #[wasm_bindgen(getter)]
    pub fn id(&self) -> String {
        self.inner.net.id().to_string()
    }

    /// Six emoji of this node's id.
    #[wasm_bindgen(getter)]
    pub fn fingerprint(&self) -> Vec<String> {
        fingerprint_of(&self.inner.net.id())
    }

    /// Home relay URL, empty when relays are disabled.
    #[wasm_bindgen(getter)]
    pub fn relay(&self) -> String {
        relay_of(&self.inner.net)
    }

    /// Hosts a swarm from this browser and submits its first job.
    pub fn host(&self, spec: JsValue, exec: JsValue, on_event: JsValue) -> Promise {
        let node = self.inner.clone();
        future_to_promise(async move { host(node, spec, exec, on_event).await.map_err(js_err) })
    }

    /// Joins the swarm of a `J` link and runs leases through `exec`.
    pub fn join(&self, link: String, opts: JsValue, exec: JsValue, on_event: JsValue) -> Promise {
        let node = self.inner.clone();
        future_to_promise(
            async move { join(node, link, opts, exec, on_event).await.map_err(js_err) },
        )
    }

    /// Shares in-memory files (`{name, bytes}[]`) and returns the `F` link.
    pub fn share(&self, files: JsValue, on_event: JsValue) -> Promise {
        let node = self.inner.clone();
        future_to_promise(async move { share(node, files, on_event).await.map_err(js_err) })
    }

    /// Downloads the files of an `F` link.
    #[wasm_bindgen(js_name = fetchFile)]
    pub fn fetch_file(&self, link: String, on_progress: JsValue) -> Promise {
        let node = self.inner.clone();
        future_to_promise(
            async move { fetch(node, link, on_progress, false).await.map_err(js_err) },
        )
    }

    /// Downloads the files of an `S` link (static preview).
    #[wasm_bindgen(js_name = fetchSite)]
    pub fn fetch_site(&self, link: String, on_progress: JsValue) -> Promise {
        let node = self.inner.clone();
        future_to_promise(async move { fetch(node, link, on_progress, true).await.map_err(js_err) })
    }

    /// Sends one console request over `nuee/http/0` to the host of an `X` link.
    pub fn http(&self, link: String, req: JsValue) -> Promise {
        let node = self.inner.clone();
        future_to_promise(async move { http(node, link, req).await.map_err(js_err) })
    }

    /// Stops serving and closes the endpoint.
    pub fn close(&self) -> Promise {
        let node = self.inner.clone();
        future_to_promise(async move {
            node.router
                .shutdown()
                .await
                .map_err(|e| js_err(WebError::internal(e)))?;
            node.net.close().await;
            Ok(JsValue::UNDEFINED)
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Host.

struct Host {
    net: Net,
    hub: StoreHub,
    router: Router,
    handle: CoordinatorHandle,
    link: String,
}

/// A swarm hosted by this browser.
#[wasm_bindgen]
pub struct HostSession {
    inner: Rc<Host>,
    job: u32,
}

async fn host(
    node: Rc<Node>,
    spec: JsValue,
    exec: JsValue,
    on_event: JsValue,
) -> Result<JsValue, WebError> {
    let sink = sink_of(on_event, "onEvent")?;
    let exec = exec_of(exec, &sink)?;
    let job = parse_job(&spec)?;
    let approve = opt_bool(&spec, "approve")?.unwrap_or(false);
    let local_slots = opt_u16(&spec, "localSlots")?.unwrap_or(1);
    // The page checks results itself even when it takes no tasks (`localSlots: 0`).
    let verify_slots = opt_u16(&spec, "verifySlots")?.unwrap_or(1);
    let webrtc = opt_bool(&spec, "webrtc")?.unwrap_or(false);
    let mut stun = BoundedVec::new();
    for s in str_list(&spec, "stun")? {
        stun.push(bounded(&s, "a STUN URL")?)
            .map_err(|_| WebError::new(code::TOO_LARGE, "at most 4 STUN servers"))?;
    }

    let net = bind(&node.relay).await?;
    sink.event("online", Obj::new().set("relay", relay_of(&net)));
    let hub = StoreHub::memory(&net, HubConfig::default());
    let secret = nuee_proto::new_secret16().map_err(random_err)?;
    let seed_bytes = nuee_proto::new_secret16().map_err(random_err)?;
    let mut seed = [0u8; 8];
    seed.copy_from_slice(seed_bytes.get(..8).unwrap_or(&[0; 8]));
    let core_cfg = CoordinatorConfig {
        name: bounded(&job.name, "the job name")?,
        local_slots,
        verify_slots,
        approve,
        seed: u64::from_le_bytes(seed),
        webrtc,
        stun,
        ..CoordinatorConfig::default()
    };
    let driver_cfg = DriverConfig {
        local_slots,
        ..DriverConfig::default()
    };
    let (driver, mut handle) = CoordinatorDriver::spawn(
        net.clone(),
        secret,
        core_cfg,
        driver_cfg,
        exec,
        // Peers' blob outputs above what a worker may produce are refused before any byte
        // is fetched, and fetched outputs are not kept in the page's store.
        Arc::new(nuee_store::OutputCappedHub::new(
            hub.clone(),
            nuee_proto::limits::LocalLimits::default().max_output as u64,
        )),
    )
    .map_err(WebError::internal)?;
    let router = net
        .router()
        .accept(ALPN_JOB, driver)
        .accept(iroh_blobs::ALPN, hub.protocol())
        .spawn();
    let events = std::mem::replace(&mut handle.events, mpsc::unbounded_channel().1);
    let relay = relay_of(&net);
    let link = Link::Job(JobLink {
        coordinator: net.id(),
        relay: bounded(&relay, "the relay URL")?,
        secret,
    })
    .to_url(&node.base);
    let host = Rc::new(Host {
        net,
        hub,
        router,
        handle,
        link,
    });
    let first = if job.train.is_some() {
        train::start(host.clone(), job, events, sink).await?
    } else {
        spawn_local(pump_host(events, sink));
        submit_job(&host, job).await?
    };
    Ok(HostSession {
        inner: host,
        job: first,
    }
    .into())
}

#[wasm_bindgen]
impl HostSession {
    /// The `J` link to hand out (it carries the admission secret).
    #[wasm_bindgen(getter)]
    pub fn link(&self) -> String {
        self.inner.link.clone()
    }

    /// Six emoji of the coordinator id, to compare with what workers see.
    #[wasm_bindgen(getter)]
    pub fn fingerprint(&self) -> Vec<String> {
        fingerprint_of(&self.inner.net.id())
    }

    /// Number of the first job.
    #[wasm_bindgen(getter)]
    pub fn job(&self) -> u32 {
        self.job
    }

    /// Answers a pending approval.
    pub fn approve(&self, peer: &str, accept: bool) -> Result<(), JsValue> {
        let id = parse_id(peer).map_err(js_err)?;
        self.inner.handle.approve(id, accept).map_err(stopped)
    }

    /// Submits one more job to the swarm; resolves to its number.
    pub fn submit(&self, spec: JsValue) -> Promise {
        let host = self.inner.clone();
        future_to_promise(async move {
            let job = parse_job(&spec).map_err(js_err)?;
            if job.train.is_some() {
                return Err(js_err(WebError::new(
                    code::UNSUPPORTED,
                    "a training run is started with host(), one per swarm",
                )));
            }
            let n = submit_job(&host, job).await.map_err(js_err)?;
            Ok(JsValue::from(n))
        })
    }

    /// Snapshot of peers and jobs.
    pub fn status(&self) -> Promise {
        let host = self.inner.clone();
        future_to_promise(async move {
            let s = host.handle.status().await.map_err(stopped)?;
            Ok(status_js(&s))
        })
    }

    /// Turns WebRTC signalling relaying on or off.
    #[wasm_bindgen(js_name = setWebrtc)]
    pub fn set_webrtc(&self, on: bool) -> Result<(), JsValue> {
        self.inner.handle.set_webrtc(on).map_err(stopped)
    }

    /// Sends signalling from the coordinator to a peer.
    #[wasm_bindgen(js_name = sendRtc)]
    pub fn send_rtc(&self, peer: String, kind: String, payload: String) -> Promise {
        let host = self.inner.clone();
        future_to_promise(async move {
            let id = parse_id(&peer).map_err(js_err)?;
            let kind = rtc_kind(&kind).map_err(js_err)?;
            host.handle
                .send_rtc(id, kind, payload)
                .await
                .map_err(|e| js_err(submit_err(&e)))?;
            Ok(JsValue::UNDEFINED)
        })
    }

    /// A frame `peer` sent on its WebRTC lane; returns the ack to send back on that lane.
    #[wasm_bindgen(js_name = laneFrame)]
    pub fn lane_frame(&self, peer: &str, frame: &[u8]) -> Result<Vec<u8>, JsValue> {
        let id = parse_id(peer).map_err(js_err)?;
        match self.inner.handle.lane_frame(id, frame) {
            Ok(ack) => Ok(ack.to_vec()),
            Err(nuee_net::LaneInError::Stopped(e)) => Err(stopped(e)),
            Err(nuee_net::LaneInError::Frame(e)) => Err(js_err(WebError::invalid(e.to_string()))),
        }
    }

    /// Says goodbye to every peer and closes the swarm's endpoint.
    pub fn stop(&self) -> Promise {
        let host = self.inner.clone();
        future_to_promise(async move {
            if let Err(e) = host.handle.shutdown().await {
                // Already stopped: nothing left to say goodbye to.
                web_sys::console::debug_1(&JsValue::from_str(&e.to_string()));
            }
            host.router
                .shutdown()
                .await
                .map_err(|e| js_err(WebError::internal(e)))?;
            host.net.close().await;
            Ok(JsValue::UNDEFINED)
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Join.

/// This browser's membership in a swarm.
#[wasm_bindgen]
pub struct JoinSession {
    handle: Rc<WorkerHandle>,
}

fn join_caps(net: &Net, opts: &JsValue, slots: u16) -> Result<Caps, WebError> {
    let mut kinds = 0u32;
    let names = str_list(opts, "kinds")?;
    if names.is_empty() {
        kinds = TaskKind::Wasm.bit();
    }
    for n in names {
        let k = kind_from_name(&n)
            .filter(|k| browser_kind(*k))
            .ok_or_else(|| WebError::invalid(format!("unsupported task kind {n:?}")))?;
        kinds |= k.bit();
    }
    let gpu = match field(opts, "gpu")? {
        None => None,
        Some(g) => Some(GpuCaps {
            adapter: bounded(&opt_str(&g, "adapter")?.unwrap_or_default(), "adapter")?,
            max_buffer: opt_u64(&g, "maxBuffer", 1 << 53)?.unwrap_or(0),
            max_storage_binding: opt_u64(&g, "maxStorageBinding", 1 << 53)?.unwrap_or(0),
            f16: opt_bool(&g, "f16")?.unwrap_or(false),
        }),
    };
    let relay = relay_of(net);
    Ok(Caps {
        agent: bounded(&format!("nuee-web/{VERSION}"), "agent")?,
        browser: true,
        cpu_slots: slots,
        gpu,
        mem_mb: opt_u32(opts, "memMb")?.unwrap_or(1024),
        kinds,
        cached: BoundedVec::new(),
        exec_allow: BoundedVec::new(),
        store_quota_mb: 0,
        relay: if relay.is_empty() {
            None
        } else {
            Some(bounded(&relay, "the relay URL")?)
        },
        rtc: opt_bool(opts, "rtc")?.unwrap_or(false),
    })
}

async fn join(
    node: Rc<Node>,
    link: String,
    opts: JsValue,
    exec: JsValue,
    on_event: JsValue,
) -> Result<JsValue, WebError> {
    let sink = sink_of(on_event, "onEvent")?;
    let exec = exec_of(exec, &sink)?;
    let job_link = match Link::parse(&link)? {
        Link::Job(l) => l,
        _ => return Err(WebError::new(code::LINK_KIND, "not a swarm (J) link")),
    };
    let opts = if opts.is_undefined() || opts.is_null() {
        JsValue::from(Object::new())
    } else {
        opts
    };
    let slots = opt_u16(&opts, "slots")?.unwrap_or(1);
    let caps = join_caps(&node.net, &opts, slots)?;
    let mut limits = nuee_proto::LocalLimits::default();
    if let Some(mb) = opt_u32(&opts, "memMb")? {
        limits.max_memory = limits.max_memory.min(u64::from(mb) * 1024 * 1024);
    }
    let cfg = WorkerConfig { caps, limits };
    // The page shows the wait for approval while it lasts, not once it is over.
    let pending = || sink.event("pending", Obj::new());
    let blobs = Arc::new(node.hub.clone());
    let mut handle = nuee_net::join_with(&node.net, &job_link, cfg, exec, blobs, pending)
        .await
        .map_err(|e| match e {
            JoinError::Refused(_) => WebError::new(code::ADMISSION_REFUSED, e.to_string()),
            JoinError::Net(n) => WebError::from(&n),
            JoinError::Handshake(h) => WebError::new(code::SWARM_ENDED, h),
        })?;
    let events = std::mem::replace(&mut handle.events, mpsc::unbounded_channel().1);
    spawn_local(pump_worker(events, sink));
    Ok(JoinSession {
        handle: Rc::new(handle),
    }
    .into())
}

#[wasm_bindgen]
impl JoinSession {
    /// Number of concurrent leases (0 pauses).
    #[wasm_bindgen(js_name = setSlots)]
    pub fn set_slots(&self, n: u16) -> Result<(), JsValue> {
        self.handle.set_slots(n).map_err(stopped)
    }

    /// Sends signalling to another peer through the coordinator.
    #[wasm_bindgen(js_name = sendRtc)]
    pub fn send_rtc(&self, peer: String, kind: String, payload: String) -> Promise {
        let handle = self.handle.clone();
        future_to_promise(async move {
            let id = parse_id(&peer).map_err(js_err)?;
            let kind = rtc_kind(&kind).map_err(js_err)?;
            handle
                .send_rtc(id, kind, payload)
                .await
                .map_err(|e| js_err(submit_err(&e)))?;
            Ok(JsValue::UNDEFINED)
        })
    }

    /// The WebRTC lane to the coordinator opened: results are now handed to `on_frame`
    /// (one `Uint8Array` per frame, to write on the data channel).
    #[wasm_bindgen(js_name = laneOpen)]
    pub fn lane_open(&self, on_frame: JsValue) -> Result<(), JsValue> {
        let f: Function = on_frame
            .dyn_into()
            .map_err(|_| js_err(WebError::invalid("onFrame must be a function")))?;
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        self.handle.lane_open(tx).map_err(stopped)?;
        spawn_local(async move {
            while let Some(frame) = rx.recv().await {
                if let Err(e) = f.call1(&JsValue::NULL, &bytes_js(&frame)) {
                    web_sys::console::error_2(&JsValue::from_str("lane handler threw"), &e);
                }
            }
        });
        Ok(())
    }

    /// A frame the coordinator sent on the lane.
    #[wasm_bindgen(js_name = laneFrame)]
    pub fn lane_frame(&self, frame: Vec<u8>) -> Result<(), JsValue> {
        self.handle.lane_frame(frame).map_err(stopped)
    }

    /// The lane closed: what it did not deliver is resent through iroh.
    #[wasm_bindgen(js_name = laneClosed)]
    pub fn lane_closed(&self) -> Result<(), JsValue> {
        self.handle.lane_closed().map_err(stopped)
    }

    /// Leaves the swarm.
    pub fn stop(&self) -> Result<(), JsValue> {
        self.handle.stop().map_err(stopped)
    }
}

// ---------------------------------------------------------------------------------------------
// Files.

/// A share served by this browser while the session lives.
#[wasm_bindgen]
pub struct ShareSession {
    inner: Rc<(Net, ShareHandle, String)>,
}

async fn share(node: Rc<Node>, files: JsValue, on_event: JsValue) -> Result<JsValue, WebError> {
    let sink = sink_of(on_event, "onEvent")?;
    if !Array::is_array(&files) {
        return Err(WebError::invalid("files must be an array of {name, bytes}"));
    }
    let mut list = Vec::new();
    for f in Array::from(&files).iter() {
        let name = req_str(&f, "name")?;
        let bytes =
            opt_bytes(&f, "bytes")?.ok_or_else(|| WebError::invalid("every file needs `bytes`"))?;
        list.push((name, bytes));
    }
    if list.is_empty() {
        return Err(WebError::invalid("nothing to share"));
    }
    let net = bind(&node.relay).await?;
    sink.event("online", Obj::new().set("relay", relay_of(&net)));
    let hub = StoreHub::memory(&net, HubConfig::default());
    let handle = share_bytes(&net, &hub, &list)
        .await
        .map_err(|e| WebError::from(&e))?;
    let link = Link::File(handle.link.clone()).to_url(&node.base);
    Ok(ShareSession {
        inner: Rc::new((net, handle, link)),
    }
    .into())
}

#[wasm_bindgen]
impl ShareSession {
    /// The `F` link to hand out (it carries the access secret and the decryption key).
    #[wasm_bindgen(getter)]
    pub fn link(&self) -> String {
        self.inner.2.clone()
    }

    /// Stops serving.
    pub fn stop(&self) -> Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            inner
                .1
                .shutdown()
                .await
                .map_err(|e| js_err(WebError::from(&e)))?;
            inner.0.close().await;
            Ok(JsValue::UNDEFINED)
        })
    }
}

async fn fetch(
    node: Rc<Node>,
    link: String,
    on_progress: JsValue,
    site: bool,
) -> Result<JsValue, WebError> {
    let progress = if on_progress.is_undefined() || on_progress.is_null() {
        None
    } else {
        Some(sink_of(on_progress, "onProgress")?)
    };
    let file = match (Link::parse(&link)?, site) {
        (Link::File(l), false) | (Link::Site(l), true) => l,
        (_, false) => return Err(WebError::new(code::LINK_KIND, "not a file (F) link")),
        (_, true) => return Err(WebError::new(code::LINK_KIND, "not a site (S) link")),
    };
    let files = get_bytes(&node.net, &node.hub, &file, |got, total| {
        if let Some(p) = &progress
            && let Err(e) =
                p.0.call2(&JsValue::NULL, &(got as f64).into(), &(total as f64).into())
        {
            web_sys::console::error_2(&JsValue::from_str("progress handler threw"), &e);
        }
    })
    .await
    .map_err(|e| WebError::from(&e))?;
    let arr: Array = files
        .iter()
        .map(|(name, bytes)| -> JsValue {
            Obj::new()
                .set("name", name.as_str())
                .set("bytes", bytes_js(bytes))
                .into()
        })
        .collect();
    Ok(Obj::new()
        .set("files", arr)
        .set("hash", file.hash.to_string())
        .set("verified", true)
        .into())
}

// ---------------------------------------------------------------------------------------------
// Console requests.

fn headers_of(
    v: &JsValue,
) -> Result<BoundedVec<Header, { nuee_proto::limits::MAX_HTTP_HEADERS }>, WebError> {
    let mut out = BoundedVec::new();
    for pair in list(v, "headers")? {
        let pair = Array::from(&pair);
        let (Some(name), Some(value)) = (pair.get(0).as_string(), pair.get(1).as_string()) else {
            return Err(WebError::invalid(
                "headers must be [name, value] string pairs",
            ));
        };
        out.push(Header {
            name: bounded(&name, "a header name")?,
            value: bounded(&value, "a header value")?,
        })
        .map_err(|_| WebError::new(code::TOO_LARGE, "too many headers"))?;
    }
    Ok(out)
}

async fn http(node: Rc<Node>, link: String, req: JsValue) -> Result<JsValue, WebError> {
    let service = match Link::parse(&link)? {
        Link::Service(l) => l,
        _ => return Err(WebError::new(code::LINK_KIND, "not a service (X) link")),
    };
    let method = opt_str(&req, "method")?.unwrap_or_else(|| "GET".to_owned());
    let path = opt_str(&req, "path")?.unwrap_or_else(|| "/".to_owned());
    let body = opt_bytes(&req, "body")?.unwrap_or_default();
    let request = HttpRequest {
        proof: nuee_proto::proof(PROOF_HTTP, &service.secret, &node.net.id(), &service.host),
        method: bounded(&method, "the method")?,
        path: bounded(&path, "the path")?,
        headers: headers_of(&req)?,
        body: BoundedBytes::try_from(body.as_slice())
            .map_err(|_| WebError::new(code::TOO_LARGE, "request body above 8 MiB"))?,
    };
    if !service.relay.as_str().is_empty() {
        node.net
            .add_peer_relay_str(service.host, service.relay.as_str())
            .map_err(|e| WebError::from(&e))?;
    }
    let conn = node
        .net
        .connect(service.host, ALPN_HTTP)
        .await
        .map_err(|e| WebError::from(&e))?;
    let io = |e: &dyn std::fmt::Display| WebError::new(code::SWARM_ENDED, e.to_string());
    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| io(&e))?;
    let answer = nuee_net::console_exchange(&mut send, &mut recv, &request)
        .await
        .map_err(|e| io(&e))?;
    let resp: HttpResponse = decode_with_limit(&answer, MAX_HTTP_MSG)
        .map_err(|e| WebError::new(code::INTEGRITY, format!("malformed response: {e}")))?;
    conn.close(0u32.into(), b"done");
    let headers: Array = resp
        .headers
        .as_slice()
        .iter()
        .map(|h| -> JsValue { strings_js(&[h.name.as_str(), h.value.as_str()]).into() })
        .collect();
    Ok(Obj::new()
        .set("status", resp.status)
        .set("headers", headers)
        .set("body", bytes_js(resp.body.as_slice()))
        .into())
}
