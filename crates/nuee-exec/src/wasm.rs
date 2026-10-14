//! WebAssembly task execution in wasmtime under strict limits.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use nuee_proto::limits::MAX_WASM_MEMORY;
use nuee_proto::{FailKind, Hash, ModuleInfo, WASM_FEATURES, validate_module};
use wasmtime::{
    Config, Engine, Instance, Module, ResourceLimiter, Store, Trap, UpdateDeadline, WasmFeatures,
};

/// Most table elements a task may grow its tables to.
const MAX_TABLE_ELEMENTS: usize = 100_000;

/// Engine settings of a [`WasmRunner`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunnerConfig {
    /// Canonicalize NaNs produced by float operations. Off in production; only the
    /// golden lint turns it on to prove outputs do not depend on it.
    pub nan_canonicalization: bool,
    /// Largest stack a task may use, in bytes.
    pub max_wasm_stack: usize,
    /// Most compiled modules kept in memory.
    pub cache_modules: usize,
    /// Period of the epoch ticker, in milliseconds: how often a running task checks its
    /// wall-clock deadline.
    pub epoch_tick_ms: u64,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            nan_canonicalization: false,
            max_wasm_stack: 1024 * 1024,
            cache_modules: 32,
            epoch_tick_ms: 10,
        }
    }
}

/// Per-run limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunLimits {
    /// Wall-clock budget of the run.
    pub deadline: Duration,
    /// Largest linear memory granted, in bytes (capped at 1 GiB).
    pub max_memory: u64,
    /// Largest output accepted, in bytes.
    pub max_output: usize,
}

/// Why a run or a module failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind:?}: {detail}")]
pub struct ExecError {
    /// Classification reported to the coordinator.
    pub kind: FailKind,
    /// Human-readable detail.
    pub detail: String,
}

impl ExecError {
    fn new(kind: FailKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
}

struct Cached {
    module: Module,
    info: ModuleInfo,
}

#[derive(Default)]
struct Cache {
    map: HashMap<Hash, Cached>,
    order: VecDeque<Hash>,
}

/// Runs task modules: one engine, a bounded cache of compiled modules and an epoch ticker.
pub struct WasmRunner {
    engine: Engine,
    cfg: RunnerConfig,
    cache: Mutex<Cache>,
    stop: Arc<AtomicBool>,
}

impl std::fmt::Debug for WasmRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmRunner")
            .field("cfg", &self.cfg)
            .finish()
    }
}

fn engine_config(cfg: &RunnerConfig) -> Result<Config, ExecError> {
    let mut c = Config::new();
    // Everything off, then exactly the shared list used by module validation on, matched by
    // flag name since wasmtime pins its own wasmparser. `GC_TYPES` is left out: this build has
    // no GC runtime, so the engine is only stricter (externref modules fail to compile).
    c.wasm_features(WasmFeatures::all(), false);
    for (name, _) in WASM_FEATURES.iter_names() {
        if name == "GC_TYPES" {
            continue;
        }
        let flag = WasmFeatures::from_name(name).ok_or_else(|| {
            ExecError::new(
                FailKind::Unsupported,
                format!("engine lacks wasm feature {name}"),
            )
        })?;
        c.wasm_features(flag, true);
    }
    c.epoch_interruption(true)
        .consume_fuel(false)
        .max_wasm_stack(cfg.max_wasm_stack)
        .cranelift_nan_canonicalization(cfg.nan_canonicalization);
    Ok(c)
}

impl WasmRunner {
    /// Builds the engine and starts the epoch ticker.
    pub fn new(cfg: RunnerConfig) -> Result<Self, ExecError> {
        if cfg.epoch_tick_ms == 0 {
            return Err(ExecError::new(
                FailKind::Invalid,
                "epoch tick must be positive",
            ));
        }
        let engine = Engine::new(&engine_config(&cfg)?)
            .map_err(|e| ExecError::new(FailKind::Unsupported, format!("engine: {e}")))?;
        let stop = Arc::new(AtomicBool::new(false));
        let weak = engine.weak();
        let flag = Arc::clone(&stop);
        let tick = Duration::from_millis(cfg.epoch_tick_ms);
        thread::Builder::new()
            .name("nuee-epoch".into())
            .spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    thread::sleep(tick);
                    match weak.upgrade() {
                        Some(engine) => engine.increment_epoch(),
                        None => break,
                    }
                }
            })
            .map_err(|e| ExecError::new(FailKind::Io, format!("epoch thread: {e}")))?;
        Ok(Self {
            engine,
            cfg,
            cache: Mutex::new(Cache::default()),
            stop,
        })
    }

    /// The underlying engine (exposed so tests can check its feature set).
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Validates, compiles and caches a module.
    pub fn prepare(&self, module_bytes: &[u8]) -> Result<ModuleInfo, ExecError> {
        let info = validate_module(module_bytes)
            .map_err(|e| ExecError::new(FailKind::Invalid, e.to_string()))?;
        if let Some(c) = self.lock().map.get(&info.hash) {
            return Ok(c.info);
        }
        let module = Module::new(&self.engine, module_bytes)
            .map_err(|e| ExecError::new(FailKind::Invalid, format!("compile: {e}")))?;
        let mut cache = self.lock();
        if !cache.map.contains_key(&info.hash) {
            while cache.map.len() >= self.cfg.cache_modules.max(1) {
                match cache.order.pop_front() {
                    Some(old) => {
                        cache.map.remove(&old);
                    }
                    None => break,
                }
            }
            cache.order.push_back(info.hash);
            cache.map.insert(info.hash, Cached { module, info });
        }
        Ok(info)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Cache> {
        // The cache holds no invariant a panic could break halfway.
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs a prepared module on `input`, blocking the calling thread.
    pub fn run(
        &self,
        module: &Hash,
        input: &[u8],
        limits: &RunLimits,
    ) -> Result<Vec<u8>, ExecError> {
        let started = Instant::now();
        let (module, info) = {
            let cache = self.lock();
            let c = cache.map.get(module).ok_or_else(|| {
                ExecError::new(FailKind::Invalid, format!("module {module} not prepared"))
            })?;
            (c.module.clone(), c.info)
        };
        let cap = limits.max_memory.min(MAX_WASM_MEMORY);
        if info.max_memory > cap {
            return Err(ExecError::new(
                FailKind::Invalid,
                format!(
                    "module declares {} bytes of memory, above the local cap of {cap}",
                    info.max_memory
                ),
            ));
        }
        let mut store = Store::new(
            &self.engine,
            Limiter {
                max_memory: usize::try_from(cap).unwrap_or(usize::MAX),
                oom: false,
            },
        );
        store.limiter(|l| l);
        // Ticks only say when to look at the clock: counting them would let every late wakeup
        // of the ticker thread stretch the deadline. The task stops on the first tick at or
        // after its wall-clock deadline, never before.
        let deadline = limits.deadline;
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(move |_| {
            if started.elapsed() >= deadline {
                Ok(UpdateDeadline::Interrupt)
            } else {
                Ok(UpdateDeadline::Continue(1))
            }
        });

        let result = call(&mut store, &module, input, limits);
        result.map_err(|e| match e {
            Failure::Exec(e) => e,
            Failure::Wasm(e) => classify(&e, store.data().oom),
        })
    }

    /// Runs a prepared module on a blocking thread.
    pub async fn run_async(
        self: &Arc<Self>,
        module: Hash,
        input: Vec<u8>,
        limits: RunLimits,
    ) -> Result<Vec<u8>, ExecError> {
        let me = Arc::clone(self);
        tokio::task::spawn_blocking(move || me.run(&module, &input, &limits))
            .await
            .map_err(|e| ExecError::new(FailKind::Io, format!("run task: {e}")))?
    }
}

impl Drop for WasmRunner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

struct Limiter {
    max_memory: usize,
    oom: bool,
}

impl ResourceLimiter for Limiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        // Growing past the declared maximum fails too; flag it so a trap that follows is Oom.
        if desired > self.max_memory || maximum.is_some_and(|max| desired > max) {
            self.oom = true;
            return Ok(false);
        }
        Ok(true)
    }

    fn memory_grow_failed(&mut self, _error: wasmtime::Error) -> wasmtime::Result<()> {
        self.oom = true;
        Ok(())
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if desired > MAX_TABLE_ELEMENTS {
            self.oom = true;
            return Ok(false);
        }
        Ok(true)
    }
}

enum Failure {
    Exec(ExecError),
    Wasm(wasmtime::Error),
}

impl From<wasmtime::Error> for Failure {
    fn from(e: wasmtime::Error) -> Self {
        Failure::Wasm(e)
    }
}

fn invalid(detail: impl Into<String>) -> Failure {
    Failure::Exec(ExecError::new(FailKind::Invalid, detail))
}

fn call(
    store: &mut Store<Limiter>,
    module: &Module,
    input: &[u8],
    limits: &RunLimits,
) -> Result<Vec<u8>, Failure> {
    let instance = Instance::new(&mut *store, module, &[])?;
    let memory = instance
        .get_memory(&mut *store, "memory")
        .ok_or_else(|| invalid("missing memory export"))?;
    let alloc = instance.get_typed_func::<i32, i32>(&mut *store, "nuee_alloc")?;
    let run = instance.get_typed_func::<(i32, i32), i64>(&mut *store, "nuee_run")?;

    let len = i32::try_from(input.len()).map_err(|_| invalid("input above 2 GiB"))?;
    let ptr = alloc.call(&mut *store, len)?;
    if ptr == 0 {
        return Err(Failure::Exec(ExecError::new(
            FailKind::Oom,
            "nuee_alloc returned 0",
        )));
    }
    let ptr = ptr.cast_unsigned() as usize;
    memory
        .write(&mut *store, ptr, input)
        .map_err(|_| invalid("nuee_alloc returned an out-of-bounds pointer"))?;

    let packed = run.call(&mut *store, (ptr as i32, len))?.cast_unsigned();
    let out_ptr = (packed >> 32) as usize;
    let out_len = (packed & 0xffff_ffff) as usize;
    if out_len > limits.max_output {
        return Err(invalid(format!(
            "output of {out_len} bytes above the cap of {}",
            limits.max_output
        )));
    }
    let size = memory.data_size(&*store);
    let end = out_ptr
        .checked_add(out_len)
        .filter(|end| *end <= size)
        .ok_or_else(|| {
            invalid(format!(
                "output range {out_ptr}+{out_len} outside memory of {size} bytes"
            ))
        })?;
    let data = memory.data(&*store);
    data.get(out_ptr..end)
        .map(<[u8]>::to_vec)
        .ok_or_else(|| invalid("output range outside memory"))
}

fn classify(e: &wasmtime::Error, oom: bool) -> ExecError {
    match e.downcast_ref::<Trap>() {
        Some(Trap::Interrupt) => ExecError::new(FailKind::Timeout, "deadline exceeded"),
        Some(Trap::StackOverflow) => ExecError::new(FailKind::StackOverflow, e.to_string()),
        _ if oom => ExecError::new(FailKind::Oom, format!("memory limit reached: {e}")),
        Some(_) => ExecError::new(FailKind::Trap, format!("{e:#}")),
        None => ExecError::new(FailKind::Trap, format!("{e:#}")),
    }
}
