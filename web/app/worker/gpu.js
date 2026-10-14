// @ts-check
// WebGPU executor for `wgsl` leases: same request and outcome shapes as the task worker.
//
// Task format (NWG1, the same as crates/nuee-gpu/src/wgsl.rs, byte for byte). The module is
// the WGSL source (UTF-8, at most 1 MiB). A task input is, little-endian:
//   "NWG1"
//   u32 entry_len, entry point name (UTF-8, 1..=256 bytes)
//   u32 wx, wy, wz        workgroup counts, each 1..=65535
//   u32 n                 bindings, 1..=8
//   n times: u32 binding (in @group(0), distinct), u32 kind (0 uniform, 1 storage read-only,
//            2 storage read_write = output), u32 size (non-zero multiple of 4, <= 64 MiB),
//            then `size` data bytes for kinds 0 and 1
// The sizes add up to at most 128 MiB, at least one binding is an output, nothing follows.
// Output: the final contents of the kind-2 buffers, concatenated in input order.
//
// Loaded as a module Worker this file answers `TaskRun` messages; on the main thread
// `gpuExecutor` runs leases in such a worker when WebGPU exists there, in this thread
// otherwise.
/** @typedef {import('../api').TaskRequest} TaskRequest */
/** @typedef {import('../api').TaskOutcome} TaskOutcome */
/** @typedef {import('../api').TaskRun} TaskRun */
/** @typedef {import('../api').FailKind} FailKind */
/** @typedef {import('../api').Executor} Executor */
/** @typedef {import('../api').JoinOptions} JoinOptions */

const MiB = 2 ** 20;
export const MAX_SOURCE = MiB;
export const MAX_ENTRY = 256;
export const MAX_BINDINGS = 8;
export const MAX_WORKGROUPS = 65535;
export const MAX_BINDING_SIZE = 64 * MiB;
export const MAX_TOTAL_SIZE = 128 * MiB;

/** WebGPU flag values (fixed by the specification). */
const USAGE = { MAP_READ: 0x1, COPY_SRC: 0x4, COPY_DST: 0x8, UNIFORM: 0x40, STORAGE: 0x80 };
const MAP_READ = 0x1;

/** A failure with its kind. */
class TaskFail extends Error {
  /** @param {FailKind} kind @param {string} detail */
  constructor(kind, detail) {
    super(detail);
    this.kind = kind;
  }
}

/** @param {FailKind} kind @param {string} detail @returns {TaskOutcome} */
function fail(kind, detail) {
  return { ok: false, kind, detail };
}

/**
 * @typedef {{ binding: number, kind: 0 | 1 | 2, size: number, data: Uint8Array | null }} Binding
 * @typedef {{ entry: string, workgroups: [number, number, number], bindings: Binding[] }} WgslTask
 */

/**
 * Parses and checks an NWG1 input; throws a TaskFail of kind `invalid`.
 * @param {Uint8Array} input
 * @returns {WgslTask}
 */
export function parseInput(input) {
  const view = new DataView(input.buffer, input.byteOffset, input.byteLength);
  let at = 0;
  /** @param {string} what */
  const bad = (what) => new TaskFail('invalid', what);
  /** @param {number} n @param {string} what */
  const take = (n, what) => {
    if (input.byteLength - at < n) throw bad(`input truncated in ${what}`);
    const s = input.subarray(at, at + n);
    at += n;
    return s;
  };
  /** @param {string} what */
  const u32 = (what) => {
    take(4, what);
    return view.getUint32(at - 4, true);
  };
  const magic = take(4, 'the magic');
  if (magic[0] !== 0x4e || magic[1] !== 0x57 || magic[2] !== 0x47 || magic[3] !== 0x31) throw bad('not an NWG1 input');
  const entryLen = u32('the entry length');
  if (entryLen === 0 || entryLen > MAX_ENTRY) throw bad('entry point name length out of range');
  let entry;
  try {
    entry = new TextDecoder('utf-8', { fatal: true }).decode(take(entryLen, 'the entry point name'));
  } catch (e) {
    if (e instanceof TaskFail) throw e;
    throw bad('entry point name is not UTF-8');
  }
  /** @type {[number, number, number]} */
  const workgroups = [0, 0, 0];
  for (let i = 0; i < 3; i += 1) {
    const w = u32('the workgroup counts');
    if (w === 0 || w > MAX_WORKGROUPS) throw bad('workgroup count out of range');
    workgroups[i] = w;
  }
  const n = u32('the binding count');
  if (n === 0 || n > MAX_BINDINGS) throw bad('binding count out of range');
  /** @type {Binding[]} */
  const bindings = [];
  let total = 0;
  for (let i = 0; i < n; i += 1) {
    const binding = u32('a binding number');
    const kind = u32('a binding kind');
    if (kind !== 0 && kind !== 1 && kind !== 2) throw bad('unknown binding kind');
    const size = u32('a binding size');
    if (size === 0 || size % 4 !== 0 || size > MAX_BINDING_SIZE) throw bad(`binding ${binding}: size must be a non-zero multiple of 4 up to 64 MiB`);
    if (bindings.some((b) => b.binding === binding)) throw bad(`binding ${binding} declared twice`);
    total += size;
    if (total > MAX_TOTAL_SIZE) throw bad('bindings above 128 MiB in total');
    const data = kind === 2 ? null : take(size, 'binding data');
    bindings.push({ binding, kind, size, data });
  }
  if (at !== input.byteLength) throw bad('trailing bytes after the last binding');
  if (!bindings.some((b) => b.kind === 2)) throw bad('no output binding');
  return { entry, workgroups, bindings };
}

/** The WebGPU entry point of this realm, if any. */
function navigatorGpu() {
  const nav = /** @type {{ gpu?: GPU }} */ (globalThis.navigator ?? {});
  return typeof nav.gpu === 'object' && nav.gpu !== null ? nav.gpu : null;
}

/**
 * What the join card reports about this GPU, or null without an adapter.
 * @returns {Promise<JoinOptions['gpu'] | null>}
 */
export async function gpuCaps() {
  const gpu = navigatorGpu();
  if (!gpu) return null;
  const adapter = await gpu.requestAdapter({ powerPreference: 'high-performance' });
  if (!adapter) return null;
  const info = adapter.info ?? {};
  const name = [info.vendor, info.architecture, info.device].filter((s) => typeof s === 'string' && s !== '').join(' ') || 'WebGPU';
  return {
    adapter: name.slice(0, 64),
    maxBuffer: Number(adapter.limits.maxBufferSize),
    maxStorageBinding: Number(adapter.limits.maxStorageBufferBindingSize),
    f16: adapter.features.has('shader-f16'),
  };
}

/**
 * Runs one WGSL task with WebGPU in this realm.
 * @param {string} wgsl
 * @param {Uint8Array} input
 * @param {number} deadlineMs
 * @returns {Promise<TaskOutcome>}
 */
export async function runWgsl(wgsl, input, deadlineMs) {
  let task;
  try {
    if (wgsl.length > MAX_SOURCE) throw new TaskFail('invalid', 'WGSL source above 1 MiB');
    task = parseInput(input);
  } catch (e) {
    return e instanceof TaskFail ? fail(e.kind, e.message) : fail('invalid', String(e));
  }
  const gpu = navigatorGpu();
  if (!gpu) return fail('unsupported', 'no WebGPU here');
  const adapter = await gpu.requestAdapter({ powerPreference: 'high-performance' });
  if (!adapter) return fail('unsupported', 'no WebGPU adapter');
  for (const b of task.bindings) {
    const max = b.kind === 0 ? adapter.limits.maxUniformBufferBindingSize : adapter.limits.maxStorageBufferBindingSize;
    if (b.size > max) return fail('unsupported', `binding ${b.binding} is larger than this GPU allows`);
  }
  const device = await adapter.requestDevice({
    requiredLimits: {
      maxBufferSize: Math.min(adapter.limits.maxBufferSize, MAX_TOTAL_SIZE),
      maxStorageBufferBindingSize: Math.min(adapter.limits.maxStorageBufferBindingSize, MAX_BINDING_SIZE),
      maxUniformBufferBindingSize: Math.min(adapter.limits.maxUniformBufferBindingSize, MAX_BINDING_SIZE),
    },
  });
  /** @type {ReturnType<typeof setTimeout> | undefined} */
  let timer;
  /** @type {Promise<TaskOutcome>} */
  const late = new Promise((resolve) => {
    timer = setTimeout(() => resolve(fail('timeout', `deadline of ${deadlineMs} ms exceeded on the GPU`)), deadlineMs);
  });
  /** @type {Promise<TaskOutcome>} */
  const lost = device.lost.then((/** @type {{ reason: string, message: string }} */ info) =>
    info.reason === 'destroyed' ? new Promise(() => {}) : fail('trap', `GPU device lost: ${info.message}`),
  );
  try {
    return await Promise.race([dispatch(device, wgsl, task), late, lost]);
  } catch (e) {
    return e instanceof TaskFail ? fail(e.kind, e.message) : fail('trap', String(e));
  } finally {
    clearTimeout(timer);
    device.destroy();
  }
}

/**
 * @param {GPUDevice} device
 * @param {string} wgsl
 * @param {WgslTask} task
 * @returns {Promise<TaskOutcome>}
 */
async function dispatch(device, wgsl, task) {
  device.pushErrorScope('out-of-memory');
  device.pushErrorScope('validation');
  const module = device.createShaderModule({ code: wgsl });
  const info = await module.getCompilationInfo();
  const errors = info.messages.filter((/** @type {{ type: string }} */ m) => m.type === 'error');
  if (errors.length > 0) {
    throw new TaskFail('invalid', `WGSL: ${errors.map((/** @type {{ lineNum: number, message: string }} */ m) => `${m.lineNum}: ${m.message}`).join('; ')}`);
  }
  let pipeline;
  try {
    pipeline = await device.createComputePipelineAsync({ layout: 'auto', compute: { module, entryPoint: task.entry } });
  } catch (e) {
    throw new TaskFail('invalid', `pipeline: ${e instanceof Error ? e.message : String(e)}`);
  }
  const buffers = [];
  const outputs = [];
  for (const b of task.bindings) {
    if (b.kind === 2) {
      const buf = device.createBuffer({ size: b.size, usage: USAGE.STORAGE | USAGE.COPY_SRC });
      outputs.push({ src: buf, stage: device.createBuffer({ size: b.size, usage: USAGE.MAP_READ | USAGE.COPY_DST }), size: b.size });
      buffers.push(buf);
    } else {
      const buf = device.createBuffer({ size: b.size, usage: (b.kind === 0 ? USAGE.UNIFORM : USAGE.STORAGE) | USAGE.COPY_DST });
      if (b.data) device.queue.writeBuffer(buf, 0, b.data);
      buffers.push(buf);
    }
  }
  const group = device.createBindGroup({
    layout: pipeline.getBindGroupLayout(0),
    entries: task.bindings.map((b, i) => ({ binding: b.binding, resource: { buffer: buffers[i] } })),
  });
  const encoder = device.createCommandEncoder();
  const pass = encoder.beginComputePass();
  pass.setPipeline(pipeline);
  pass.setBindGroup(0, group);
  pass.dispatchWorkgroups(...task.workgroups);
  pass.end();
  for (const o of outputs) encoder.copyBufferToBuffer(o.src, 0, o.stage, 0, o.size);
  device.queue.submit([encoder.finish()]);
  const invalid = await device.popErrorScope();
  if (invalid) throw new TaskFail('invalid', `GPU validation: ${invalid.message}`);
  const oom = await device.popErrorScope();
  if (oom) throw new TaskFail('oom', `GPU out of memory: ${oom.message}`);
  await Promise.all(outputs.map((o) => o.stage.mapAsync(MAP_READ)));
  const out = new Uint8Array(outputs.reduce((a, o) => a + o.size, 0));
  let at = 0;
  for (const o of outputs) {
    out.set(new Uint8Array(o.stage.getMappedRange()), at);
    at += o.size;
    o.stage.unmap();
  }
  return { ok: true, output: out };
}

/** Shader sources by module hash, as first received. */
const sources = new Map();

/**
 * The WGSL source of a request, remembering it by hash.
 * @param {{ moduleHash?: string, module?: Uint8Array }} req
 * @returns {string | TaskOutcome}
 */
function sourceOf(req) {
  if (!req.moduleHash) return fail('invalid', 'lease without a shader');
  if (req.module && !sources.has(req.moduleHash)) {
    if (req.module.byteLength > MAX_SOURCE) return fail('invalid', 'WGSL source above 1 MiB');
    try {
      sources.set(req.moduleHash, new TextDecoder('utf-8', { fatal: true }).decode(req.module));
    } catch (e) {
      return fail('invalid', `the shader is not UTF-8: ${String(e)}`);
    }
  }
  const src = sources.get(req.moduleHash);
  return typeof src === 'string' ? src : fail('invalid', `shader ${req.moduleHash} was never received`);
}

/** @param {TaskRequest | TaskRun} req @returns {Promise<TaskOutcome>} */
async function runRequest(req) {
  const src = sourceOf(req);
  if (typeof src !== 'string') return src;
  return runWgsl(src, req.input, req.deadlineMs);
}

/**
 * The executor of `wgsl` leases. `spawn` starts this file as a module Worker; leases run
 * there when the worker has WebGPU, on this thread otherwise. The worker is replaced after a
 * lease overruns its deadline.
 * @param {() => Worker} spawn
 * @returns {Executor}
 */
export function gpuExecutor(spawn) {
  /** @type {Promise<Worker | null> | null} */
  let ready = null;
  const start = () => {
    ready = new Promise((resolve) => {
      let w;
      try {
        w = spawn();
      } catch (e) {
        console.warn('GPU worker unavailable, using the main thread', e);
        resolve(null);
        return;
      }
      const worker = w;
      worker.onmessage = (e) => {
        const ok = e.data && e.data.gpu === true;
        if (!ok) worker.terminate();
        resolve(ok ? worker : null);
      };
      worker.onerror = (e) => {
        console.warn('GPU worker failed, using the main thread', e.message);
        worker.terminate();
        resolve(null);
      };
      worker.postMessage({ op: 'probe' });
    });
    return ready;
  };
  /** Hashes whose source the worker already holds. */
  const sent = new Set();
  let chain = Promise.resolve();
  return (req) => {
    const run = async () => {
      const worker = await (ready ?? start());
      if (!worker) return runRequest(req);
      const src = sourceOf(req);
      if (typeof src !== 'string') return src;
      const fresh = !sent.has(req.moduleHash);
      return new Promise((resolve) => {
        const backstop = setTimeout(() => {
          worker.terminate();
          ready = null;
          sent.clear();
          resolve(fail('timeout', `deadline of ${req.deadlineMs} ms exceeded`));
        }, req.deadlineMs + 2000);
        worker.onmessage = (e) => {
          clearTimeout(backstop);
          if (fresh) sent.add(req.moduleHash);
          const o = e.data;
          resolve(o && (o.ok === true || o.ok === false) ? o : fail('io', 'malformed reply from the GPU worker'));
        };
        worker.onerror = (e) => {
          clearTimeout(backstop);
          worker.terminate();
          ready = null;
          sent.clear();
          resolve(fail('io', e.message ?? 'GPU worker error'));
        };
        /** @type {TaskRun} */
        const msg = {
          op: 'run',
          lease: req.lease,
          kind: req.kind,
          moduleHash: req.moduleHash,
          module: fresh ? new TextEncoder().encode(src) : undefined,
          input: req.input,
          deadlineMs: req.deadlineMs,
          maxMemory: req.maxMemory,
        };
        worker.postMessage(msg);
      });
    };
    // One lease at a time on the GPU.
    const out = chain.then(run, run);
    chain = out.then(
      () => undefined,
      () => undefined,
    );
    return out;
  };
}

// Worker side.
const scope = /** @type {{ WorkerGlobalScope?: Function }} */ (/** @type {unknown} */ (globalThis));
if (typeof scope.WorkerGlobalScope === 'function' && globalThis instanceof scope.WorkerGlobalScope) {
  const port = /** @type {{ onmessage: ((e: MessageEvent) => void) | null, postMessage(m: unknown, t?: Transferable[]): void }} */ (/** @type {unknown} */ (globalThis));
  port.onmessage = (e) => {
    const msg = e.data;
    if (msg && msg.op === 'probe') {
      port.postMessage({ gpu: navigatorGpu() !== null });
      return;
    }
    if (!msg || msg.op !== 'run') {
      port.postMessage(fail('invalid', 'unknown request'));
      return;
    }
    runRequest(msg).then(
      (o) => port.postMessage(o, o.ok ? [o.output.buffer] : []),
      (err) => port.postMessage(fail('io', String(err))),
    );
  };
}
