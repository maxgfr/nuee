//! WGSL compute tasks: the input format, and (feature `native`) a wgpu runner.
//!
//! # Task format (`NWG1`)
//!
//! The module is the WGSL source (UTF-8, at most [`MAX_SOURCE`] bytes; its BLAKE3 is the
//! module hash). A task input is, little-endian:
//!
//! ```text
//! b"NWG1"
//! u32 entry_len, entry point name (UTF-8, 1..=MAX_ENTRY bytes)
//! u32 wx, u32 wy, u32 wz          workgroup counts, each 1..=65535
//! u32 n                           bindings, 1..=8
//! n times:
//!   u32 binding                   binding number in @group(0), distinct
//!   u32 kind                      0 uniform, 1 storage read-only, 2 storage read_write (output)
//!   u32 size                      bytes, a non-zero multiple of 4, at most 64 MiB
//!   size bytes of data            for kinds 0 and 1 only
//! ```
//!
//! The sizes of all bindings add up to at most 128 MiB, at least one binding is an output and
//! nothing follows the last binding. The output of a task is the final contents of its kind-2
//! buffers, concatenated in the order they appear in the input. `web/app/worker/gpu.js`
//! implements the same format.

use crate::GpuError;

/// Magic bytes opening a task input.
pub const MAGIC: [u8; 4] = *b"NWG1";
/// Largest WGSL source.
pub const MAX_SOURCE: usize = 1 << 20;
/// Longest entry point name.
pub const MAX_ENTRY: usize = 256;
/// Most bindings per task.
pub const MAX_BINDINGS: usize = 8;
/// Largest workgroup count per dimension.
pub const MAX_WORKGROUPS: u32 = 65_535;
/// Largest single binding.
pub const MAX_BINDING_SIZE: u32 = 64 << 20;
/// Largest sum of all bindings.
pub const MAX_TOTAL_SIZE: u64 = 128 << 20;

/// The WGSL twin of the mandelbrot task (`tasks/mandelbrot/mandelbrot.wgsl`).
pub const MANDELBROT_WGSL: &str = include_str!("../../../tasks/mandelbrot/mandelbrot.wgsl");

/// Comparator of WGSL mandelbrot jobs: f32 escape counts are chaotic near the set's boundary,
/// so two correct GPUs (or one fusing multiply-adds and one not) disagree on some pixels. On
/// the 1024x1024 demo grid the worst 64x64 tile has 89 such pixels (2.2 %) between wgpu on
/// Metal, Chromium's WebGPU and IEEE f32 with or without FMA; 5 % leaves a margin.
pub const MANDELBROT_COMPARE: nuee_proto::Comparator = nuee_proto::Comparator::ElementTolerance {
    abs: 2.0,
    rel: 0.0,
    max_bad_permille: 50,
};

/// What a binding is used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingKind {
    /// `var<uniform>`, filled from the input.
    Uniform,
    /// `var<storage, read>`, filled from the input.
    ReadOnly,
    /// `var<storage, read_write>`, zero-filled; its final contents are part of the output.
    Output,
}

impl BindingKind {
    fn from_u32(v: u32) -> Option<BindingKind> {
        match v {
            0 => Some(BindingKind::Uniform),
            1 => Some(BindingKind::ReadOnly),
            2 => Some(BindingKind::Output),
            _ => None,
        }
    }

    fn code(self) -> u32 {
        match self {
            BindingKind::Uniform => 0,
            BindingKind::ReadOnly => 1,
            BindingKind::Output => 2,
        }
    }
}

/// One binding of a task input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding<'a> {
    /// Binding number in `@group(0)`.
    pub binding: u32,
    /// Use.
    pub kind: BindingKind,
    /// Size in bytes.
    pub size: u32,
    /// Initial contents (empty for outputs).
    pub data: &'a [u8],
}

/// A parsed task input, borrowing the input bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskInput<'a> {
    /// Entry point name.
    pub entry: &'a str,
    /// Workgroup counts.
    pub workgroups: [u32; 3],
    /// Bindings, in input order.
    pub bindings: Vec<Binding<'a>>,
}

impl TaskInput<'_> {
    /// Total size of the output.
    pub fn output_len(&self) -> u64 {
        self.bindings
            .iter()
            .filter(|b| b.kind == BindingKind::Output)
            .map(|b| u64::from(b.size))
            .sum()
    }
}

struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize, what: &str) -> Result<&'a [u8], GpuError> {
        if self.rest.len() < n {
            return Err(GpuError::invalid(format!("input truncated in {what}")));
        }
        let (head, tail) = self.rest.split_at(n);
        self.rest = tail;
        Ok(head)
    }

    fn u32(&mut self, what: &str) -> Result<u32, GpuError> {
        let b = self.take(4, what)?;
        let mut a = [0u8; 4];
        a.copy_from_slice(b);
        Ok(u32::from_le_bytes(a))
    }
}

/// Parses and checks a task input; nothing is allocated beyond the binding list.
pub fn parse_input(input: &[u8]) -> Result<TaskInput<'_>, GpuError> {
    let mut r = Reader { rest: input };
    if r.take(4, "the magic")? != MAGIC {
        return Err(GpuError::invalid("not an NWG1 input"));
    }
    let entry_len = r.u32("the entry length")? as usize;
    if entry_len == 0 || entry_len > MAX_ENTRY {
        return Err(GpuError::invalid("entry point name length out of range"));
    }
    let entry = std::str::from_utf8(r.take(entry_len, "the entry point name")?)
        .map_err(|_| GpuError::invalid("entry point name is not UTF-8"))?;
    let mut workgroups = [0u32; 3];
    for w in &mut workgroups {
        *w = r.u32("the workgroup counts")?;
        if *w == 0 || *w > MAX_WORKGROUPS {
            return Err(GpuError::invalid("workgroup count out of range"));
        }
    }
    let n = r.u32("the binding count")? as usize;
    if n == 0 || n > MAX_BINDINGS {
        return Err(GpuError::invalid("binding count out of range"));
    }
    let mut bindings: Vec<Binding<'_>> = Vec::with_capacity(n);
    let mut total = 0u64;
    for _ in 0..n {
        let binding = r.u32("a binding number")?;
        let kind = BindingKind::from_u32(r.u32("a binding kind")?)
            .ok_or_else(|| GpuError::invalid("unknown binding kind"))?;
        let size = r.u32("a binding size")?;
        if size == 0 || !size.is_multiple_of(4) || size > MAX_BINDING_SIZE {
            return Err(GpuError::invalid(format!(
                "binding {binding}: size must be a non-zero multiple of 4 up to 64 MiB"
            )));
        }
        if bindings.iter().any(|b| b.binding == binding) {
            return Err(GpuError::invalid(format!(
                "binding {binding} declared twice"
            )));
        }
        total += u64::from(size);
        if total > MAX_TOTAL_SIZE {
            return Err(GpuError::invalid("bindings above 128 MiB in total"));
        }
        let data = match kind {
            BindingKind::Output => &[][..],
            _ => r.take(size as usize, "binding data")?,
        };
        bindings.push(Binding {
            binding,
            kind,
            size,
            data,
        });
    }
    if !r.rest.is_empty() {
        return Err(GpuError::invalid("trailing bytes after the last binding"));
    }
    if !bindings.iter().any(|b| b.kind == BindingKind::Output) {
        return Err(GpuError::invalid("no output binding"));
    }
    Ok(TaskInput {
        entry,
        workgroups,
        bindings,
    })
}

/// A binding to encode with [`encode_input`].
#[derive(Debug, Clone, Copy)]
pub enum BindingSpec<'a> {
    /// A uniform buffer with these contents.
    Uniform(u32, &'a [u8]),
    /// A read-only storage buffer with these contents.
    ReadOnly(u32, &'a [u8]),
    /// A zero-filled read_write storage buffer of this size.
    Output(u32, u32),
}

/// Encodes a task input. Checks nothing: [`parse_input`] does on the other side.
pub fn encode_input(entry: &str, workgroups: [u32; 3], bindings: &[BindingSpec<'_>]) -> Vec<u8> {
    let mut v = Vec::new();
    let put = |v: &mut Vec<u8>, x: u32| v.extend_from_slice(&x.to_le_bytes());
    v.extend_from_slice(&MAGIC);
    put(&mut v, entry.len() as u32);
    v.extend_from_slice(entry.as_bytes());
    for w in workgroups {
        put(&mut v, w);
    }
    put(&mut v, bindings.len() as u32);
    for b in bindings {
        match *b {
            BindingSpec::Uniform(n, data) | BindingSpec::ReadOnly(n, data) => {
                let kind = if matches!(b, BindingSpec::Uniform(..)) {
                    BindingKind::Uniform
                } else {
                    BindingKind::ReadOnly
                };
                put(&mut v, n);
                put(&mut v, kind.code());
                put(&mut v, data.len() as u32);
                v.extend_from_slice(data);
            }
            BindingSpec::Output(n, size) => {
                put(&mut v, n);
                put(&mut v, BindingKind::Output.code());
                put(&mut v, size);
            }
        }
    }
    v
}

/// Input of a [`MANDELBROT_WGSL`] task: a `width` x `height` tile from (`x0`, `y0`), `step`
/// apart, at most `max_iter` iterations.
pub fn mandelbrot_input(
    x0: f32,
    y0: f32,
    step: f32,
    width: u32,
    height: u32,
    max_iter: u32,
) -> Vec<u8> {
    let mut params = Vec::with_capacity(24);
    for f in [x0, y0, step] {
        params.extend_from_slice(&f.to_le_bytes());
    }
    for u in [width, height, max_iter] {
        params.extend_from_slice(&u.to_le_bytes());
    }
    encode_input(
        "main",
        [width.div_ceil(8), height.div_ceil(8), 1],
        &[
            BindingSpec::Uniform(0, &params),
            BindingSpec::Output(1, width.saturating_mul(height).saturating_mul(4)),
        ],
    )
}

#[cfg(feature = "native")]
pub use native::{GpuRunner, adapter_caps, check_shader, probe_adapter};

#[cfg(feature = "native")]
mod native {
    use std::future::Future;
    use std::pin::pin;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Waker};
    use std::time::{Duration, Instant};

    use naga::valid::{Capabilities, ValidationFlags, Validator};
    use naga::{AddressSpace, ShaderStage, StorageAccess};
    use nuee_proto::{BoundedString, FailKind, GpuCaps};
    use wgpu::util::DeviceExt as _;

    use super::{
        BindingKind, MAX_BINDING_SIZE, MAX_SOURCE, MAX_TOTAL_SIZE, TaskInput, parse_input,
    };
    use crate::GpuError;

    /// Drives a future that wgpu resolves synchronously on native backends.
    fn block_on<F: Future>(fut: F, deadline: Instant) -> Option<F::Output> {
        let mut fut = pin!(fut);
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return Some(v);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::yield_now();
        }
    }

    /// Outcome of mapping one staging buffer, once known.
    type MapState = Option<Result<(), wgpu::BufferAsyncError>>;

    const SETUP: Duration = Duration::from_secs(10);

    fn instance() -> wgpu::Instance {
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle())
    }

    fn request_adapter(instance: &wgpu::Instance) -> Option<wgpu::Adapter> {
        let opts = wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        };
        block_on(instance.request_adapter(&opts), Instant::now() + SETUP)?.ok()
    }

    /// Capabilities of this machine's GPU, `None` without a usable adapter.
    pub fn adapter_caps() -> Option<GpuCaps> {
        let adapter = request_adapter(&instance())?;
        Some(caps_of(&adapter))
    }

    /// Capabilities and backend name (Metal, Vulkan, Dx12, Gl…) of the adapter `join --gpu` uses,
    /// `None` without a usable adapter.
    pub fn probe_adapter() -> Option<(GpuCaps, String)> {
        let adapter = request_adapter(&instance())?;
        let backend = format!("{:?}", adapter.get_info().backend);
        Some((caps_of(&adapter), backend))
    }

    fn caps_of(adapter: &wgpu::Adapter) -> GpuCaps {
        let info = adapter.get_info();
        let limits = adapter.limits();
        let mut name = info.name;
        let mut cut = name.len().min(64);
        while !name.is_char_boundary(cut) {
            cut -= 1;
        }
        name.truncate(cut);
        GpuCaps {
            adapter: BoundedString::try_from(name.as_str()).unwrap_or_default(),
            max_buffer: limits.max_buffer_size,
            max_storage_binding: limits.max_storage_buffer_binding_size,
            f16: adapter.features().contains(wgpu::Features::SHADER_F16),
        }
    }

    /// Checks a WGSL source against a task input without a device: naga parsing and
    /// validation, a compute entry point named as in the input, and every binding the entry
    /// point uses matching a declared one of a compatible kind (and the other way round).
    pub fn check_shader(wgsl: &str, task: &TaskInput<'_>) -> Result<(), GpuError> {
        if wgsl.len() > MAX_SOURCE {
            return Err(GpuError::invalid("WGSL source above 1 MiB"));
        }
        let module = naga::front::wgsl::parse_str(wgsl)
            .map_err(|e| GpuError::invalid(format!("WGSL: {}", e.emit_to_string(wgsl))))?;
        let info = Validator::new(ValidationFlags::default(), Capabilities::default())
            .validate(&module)
            .map_err(|e| GpuError::invalid(format!("WGSL validation: {}", e.into_inner())))?;
        let (index, _) = module
            .entry_points
            .iter()
            .enumerate()
            .find(|(_, ep)| ep.name == task.entry && ep.stage == ShaderStage::Compute)
            .ok_or_else(|| {
                GpuError::invalid(format!("no compute entry point named {:?}", task.entry))
            })?;
        let uses = info.get_entry_point(index);
        let mut seen = Vec::with_capacity(task.bindings.len());
        for (handle, var) in module.global_variables.iter() {
            let Some(rb) = &var.binding else { continue };
            if uses[handle].is_empty() {
                continue;
            }
            if rb.group != 0 {
                return Err(GpuError::invalid(format!(
                    "the entry point uses @group({}); only @group(0) is bound",
                    rb.group
                )));
            }
            let declared = task
                .bindings
                .iter()
                .find(|b| b.binding == rb.binding)
                .ok_or_else(|| {
                    GpuError::invalid(format!("binding {} is not in the input", rb.binding))
                })?;
            let ok = match (var.space, declared.kind) {
                (AddressSpace::Uniform, BindingKind::Uniform) => true,
                (AddressSpace::Storage { access }, BindingKind::ReadOnly) => {
                    !access.contains(StorageAccess::STORE)
                }
                (AddressSpace::Storage { access }, BindingKind::Output) => {
                    access.contains(StorageAccess::STORE)
                }
                _ => false,
            };
            if !ok {
                return Err(GpuError::invalid(format!(
                    "binding {} is declared {:?} in the input but is {:?} in the shader",
                    rb.binding, declared.kind, var.space
                )));
            }
            seen.push(rb.binding);
        }
        if let Some(b) = task.bindings.iter().find(|b| !seen.contains(&b.binding)) {
            return Err(GpuError::invalid(format!(
                "binding {} is not used by the entry point",
                b.binding
            )));
        }
        Ok(())
    }

    /// Runs WGSL tasks on this machine's GPU, one fresh device per task.
    pub struct GpuRunner {
        adapter: wgpu::Adapter,
        caps: GpuCaps,
        /// Devices whose overrunning work has not drained yet.
        overrun: Arc<AtomicUsize>,
    }

    impl std::fmt::Debug for GpuRunner {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("GpuRunner")
                .field("caps", &self.caps)
                .finish()
        }
    }

    impl GpuRunner {
        /// The high-performance adapter of the default backends, `None` when there is none.
        pub fn new() -> Option<GpuRunner> {
            let adapter = request_adapter(&instance())?;
            let caps = caps_of(&adapter);
            Some(GpuRunner {
                adapter,
                caps,
                overrun: Arc::new(AtomicUsize::new(0)),
            })
        }

        /// The adapter's capabilities.
        pub fn caps(&self) -> &GpuCaps {
            &self.caps
        }

        fn device(&self) -> Result<(wgpu::Device, wgpu::Queue), GpuError> {
            let mut limits = self.adapter.limits();
            limits.max_buffer_size = limits.max_buffer_size.min(MAX_TOTAL_SIZE);
            limits.max_storage_buffer_binding_size = limits
                .max_storage_buffer_binding_size
                .min(u64::from(MAX_BINDING_SIZE));
            limits.max_uniform_buffer_binding_size = limits
                .max_uniform_buffer_binding_size
                .min(u64::from(MAX_BINDING_SIZE));
            let desc = wgpu::DeviceDescriptor {
                label: Some("nuee-task"),
                required_limits: limits,
                ..Default::default()
            };
            block_on(self.adapter.request_device(&desc), Instant::now() + SETUP)
                .ok_or_else(|| GpuError::new(FailKind::Io, "the GPU device did not open in time"))?
                .map_err(|e| {
                    GpuError::new(FailKind::Io, format!("cannot open the GPU device: {e}"))
                })
        }

        /// Runs `wgsl` on `input` (the NWG1 format) within `deadline`.
        ///
        /// Invalid sources or inputs fail with `Invalid` before any device is opened. Past the
        /// deadline the device is destroyed and the task fails with `Timeout`; a device lost
        /// during the run fails with `Trap`.
        pub fn run(
            &self,
            wgsl: &str,
            input: &[u8],
            deadline: Duration,
        ) -> Result<Vec<u8>, GpuError> {
            let start = Instant::now();
            if self.overrun.load(Ordering::SeqCst) > 0 {
                // GPU work cannot be preempted: until the overrunning shader ends (or the
                // driver kills it) new work would only queue behind it.
                return Err(GpuError::new(
                    FailKind::Unsupported,
                    "the GPU is still busy with a task that overran its deadline",
                ));
            }
            let task = parse_input(input)?;
            check_shader(wgsl, &task)?;
            for b in &task.bindings {
                let max = match b.kind {
                    BindingKind::Uniform => self.adapter.limits().max_uniform_buffer_binding_size,
                    _ => self.adapter.limits().max_storage_buffer_binding_size,
                };
                if u64::from(b.size) > max {
                    return Err(GpuError::new(
                        FailKind::Unsupported,
                        format!("binding {} is larger than this GPU allows", b.binding),
                    ));
                }
            }
            let (device, queue) = self.device()?;
            let lost = Arc::new(AtomicBool::new(false));
            let flag = lost.clone();
            device.set_device_lost_callback(move |reason, _msg| {
                if reason != wgpu::DeviceLostReason::Destroyed {
                    flag.store(true, Ordering::SeqCst);
                }
            });
            let result = dispatch(&device, &queue, wgsl, &task, start + deadline, &lost);
            device.destroy();
            if matches!(&result, Err(e) if e.kind == FailKind::Timeout) {
                // Dropping the queue waits for the GPU to go idle (Metal blocks in
                // `waitUntilCompleted`), which an overrunning shader may never do: let a
                // detached thread own that wait so the caller gets its Timeout now.
                self.overrun.fetch_add(1, Ordering::SeqCst);
                let overrun = self.overrun.clone();
                let spawned = std::thread::Builder::new()
                    .name("nuee-gpu-release".into())
                    .spawn(move || {
                        drop((queue, device));
                        overrun.fetch_sub(1, Ordering::SeqCst);
                    });
                if let Err(e) = spawned {
                    self.overrun.fetch_sub(1, Ordering::SeqCst);
                    return Err(GpuError::new(
                        FailKind::Io,
                        format!("cannot release the GPU device: {e}"),
                    ));
                }
            }
            result
        }
    }

    fn scope_error(e: wgpu::Error) -> GpuError {
        match e {
            wgpu::Error::OutOfMemory { .. } => GpuError::new(FailKind::Oom, "GPU out of memory"),
            wgpu::Error::Validation { description, .. } => {
                GpuError::invalid(format!("GPU validation: {description}"))
            }
            wgpu::Error::Internal { description, .. } => {
                GpuError::new(FailKind::Trap, format!("GPU internal error: {description}"))
            }
        }
    }

    fn dispatch(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        wgsl: &str,
        task: &TaskInput<'_>,
        deadline: Instant,
        lost: &AtomicBool,
    ) -> Result<Vec<u8>, GpuError> {
        let oom = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let internal = device.push_error_scope(wgpu::ErrorFilter::Internal);
        let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("task"),
            source: wgpu::ShaderSource::Wgsl(wgsl.into()),
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("task"),
            layout: None,
            module: &shader,
            entry_point: Some(task.entry),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });
        let mut buffers = Vec::with_capacity(task.bindings.len());
        let mut staging = Vec::new();
        for b in &task.bindings {
            let buf = match b.kind {
                BindingKind::Uniform => {
                    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("uniform"),
                        contents: b.data,
                        usage: wgpu::BufferUsages::UNIFORM,
                    })
                }
                BindingKind::ReadOnly => {
                    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("read"),
                        contents: b.data,
                        usage: wgpu::BufferUsages::STORAGE,
                    })
                }
                BindingKind::Output => {
                    let out = device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("output"),
                        size: u64::from(b.size),
                        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                        mapped_at_creation: false,
                    });
                    staging.push((
                        buffers.len(),
                        device.create_buffer(&wgpu::BufferDescriptor {
                            label: Some("staging"),
                            size: u64::from(b.size),
                            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                            mapped_at_creation: false,
                        }),
                    ));
                    out
                }
            };
            buffers.push(buf);
        }
        let entries: Vec<wgpu::BindGroupEntry<'_>> = task
            .bindings
            .iter()
            .zip(&buffers)
            .map(|(b, buf)| wgpu::BindGroupEntry {
                binding: b.binding,
                resource: buf.as_entire_binding(),
            })
            .collect();
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("task"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &entries,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("task"),
        });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &group, &[]);
            let [x, y, z] = task.workgroups;
            pass.dispatch_workgroups(x, y, z);
        }
        for (i, stage) in &staging {
            encoder.copy_buffer_to_buffer(&buffers[*i], 0, stage, 0, stage.size());
        }
        let submission = queue.submit([encoder.finish()]);

        let pop = |guard: wgpu::ErrorScopeGuard| -> Result<(), GpuError> {
            match block_on(guard.pop(), Instant::now() + SETUP) {
                None => Err(GpuError::new(
                    FailKind::Io,
                    "GPU error scope did not resolve",
                )),
                Some(Some(e)) => Err(scope_error(e)),
                Some(None) => Ok(()),
            }
        };
        let checks = [pop(validation), pop(internal), pop(oom)];
        for c in checks {
            c?;
        }

        let mapped: Arc<Mutex<Vec<MapState>>> = Arc::new(Mutex::new(vec![None; staging.len()]));
        for (k, (_, stage)) in staging.iter().enumerate() {
            let slot = mapped.clone();
            stage.map_async(wgpu::MapMode::Read, .., move |r| {
                if let Ok(mut v) = slot.lock()
                    && let Some(s) = v.get_mut(k)
                {
                    *s = Some(r);
                }
            });
        }
        let timeout = deadline.saturating_duration_since(Instant::now());
        let polled = device.poll(wgpu::PollType::Wait {
            submission_index: Some(submission),
            timeout: Some(timeout),
        });
        if lost.load(Ordering::SeqCst) {
            return Err(GpuError::new(FailKind::Trap, "the GPU device was lost"));
        }
        match polled {
            Ok(_) => {}
            Err(wgpu::PollError::Timeout) => {
                return Err(GpuError::new(
                    FailKind::Timeout,
                    "deadline exceeded on the GPU",
                ));
            }
            Err(e) => return Err(GpuError::new(FailKind::Trap, format!("GPU: {e}"))),
        }
        let states = mapped
            .lock()
            .map_err(|_| GpuError::new(FailKind::Io, "mapping state poisoned"))?
            .clone();
        let mut out = Vec::with_capacity(usize::try_from(task.output_len()).unwrap_or(0));
        for ((_, stage), state) in staging.iter().zip(states) {
            match state {
                Some(Ok(())) => {}
                Some(Err(e)) => {
                    return Err(GpuError::new(
                        FailKind::Trap,
                        format!("reading the output: {e}"),
                    ));
                }
                None => return Err(GpuError::new(FailKind::Io, "the output was not mapped")),
            }
            let view = stage
                .get_mapped_range(..)
                .map_err(|e| GpuError::new(FailKind::Io, format!("reading the output: {e}")))?;
            out.extend_from_slice(&view);
        }
        Ok(out)
    }
}
