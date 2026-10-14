// @ts-check
// A task worker: runs one lease at a time on a fresh WebAssembly instance (task ABI:
// `memory`, `nuee_alloc(len) -> ptr`, `nuee_run(ptr, len) -> (out_ptr << 32) | out_len`).
// Compiled modules are cached by hash for the life of this worker.
/** @typedef {import('../api').TaskRun} TaskRun */
/** @typedef {import('../api').TaskOutcome} TaskOutcome */
/** @typedef {import('../api').FailKind} FailKind */

/**
 * @type {{ postMessage(m: TaskOutcome, t: Transferable[]): void, onmessage: ((e: MessageEvent) => void) | null }}
 */
const scope = /** @type {never} */ (self);

/** @type {Map<string, WebAssembly.Module>} */
const compiled = new Map();

/** @param {FailKind} kind @param {string} detail @returns {TaskOutcome} */
function fail(kind, detail) {
  return { ok: false, kind, detail };
}

/**
 * Maps an engine error to a fail kind. Engines word these differently: V8 and JSC throw a
 * RangeError for stack exhaustion, SpiderMonkey an InternalError ("too much recursion").
 * @param {unknown} e
 * @returns {TaskOutcome}
 */
export function classify(e) {
  const name = e instanceof Error ? e.name : '';
  const msg = e instanceof Error ? e.message : String(e);
  if (/call stack|too much recursion|stack overflow/i.test(msg)) return fail('stack_overflow', msg);
  if (e instanceof WebAssembly.CompileError || e instanceof WebAssembly.LinkError) return fail('invalid', msg);
  if (name === 'RangeError' && /memory|allocat/i.test(msg)) return fail('oom', msg);
  if (e instanceof WebAssembly.RuntimeError) return fail('trap', msg);
  return fail('trap', `${name}: ${msg}`);
}

/** @param {TaskRun} t @returns {Promise<TaskOutcome>} */
async function run(t) {
  if (t.kind !== 'wasm' && t.kind !== 'trainWasm') return fail('unsupported', `kind ${t.kind} is not run by this worker`);
  if (!t.moduleHash) return fail('invalid', 'a wasm lease needs a module');
  let mod = compiled.get(t.moduleHash);
  if (!mod) {
    if (!t.module) return fail('invalid', 'module bytes missing');
    mod = await WebAssembly.compile(t.module);
    compiled.set(t.moduleHash, mod);
  }
  const instance = await WebAssembly.instantiate(mod, {});
  const { memory, nuee_alloc: alloc, nuee_run: entry } = instance.exports;
  if (!(memory instanceof WebAssembly.Memory) || typeof alloc !== 'function' || typeof entry !== 'function') {
    return fail('invalid', 'the module does not follow the task ABI');
  }
  const len = t.input.byteLength;
  const ptr = Number(alloc(len));
  if (ptr === 0 && len > 0) return fail('oom', 'nuee_alloc returned 0');
  if (memory.buffer.byteLength > t.maxMemory) return fail('oom', 'memory above the lease limit');
  new Uint8Array(memory.buffer, ptr, len).set(t.input);
  const packed = entry(ptr, len);
  if (typeof packed !== 'bigint') return fail('invalid', 'nuee_run must return an i64');
  const word = BigInt.asUintN(64, packed);
  const outPtr = Number(word >> 32n);
  const outLen = Number(word & 0xffffffffn);
  if (memory.buffer.byteLength > t.maxMemory) return fail('oom', 'memory above the lease limit');
  if (outPtr + outLen > memory.buffer.byteLength) return fail('trap', 'output outside memory');
  return { ok: true, output: new Uint8Array(memory.buffer, outPtr, outLen).slice() };
}

scope.onmessage = (e) => {
  /** @type {TaskRun} */
  const t = e.data;
  if (typeof t !== 'object' || t === null || t.op !== 'run') return;
  run(t).then(
    (out) => scope.postMessage(out, out.ok ? [out.output.buffer] : []),
    (err) => scope.postMessage(classify(err), []),
  );
};
