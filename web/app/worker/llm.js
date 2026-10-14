// @ts-check
// `llm` leases in the browser: Qwen3.5-0.8B (ONNX, q4) through the vendored Transformers.js,
// scoring the option letters of skycmd decision records the way crates/nuee-gpu/src/llm.rs
// and skycmd's scripts/teacher-score.py do.
//
// Task input: JSONL decision records (`t`, `ins`, `opts`, optional `state`), at most 512
// records and 1 MiB; the first line may be a header {"llm":{"model":"…"}}. Blank lines are
// skipped but counted. Output: JSONL {"uid":N,"tlogits":[…]} in input order, N being the
// line index after the header, scores rounded to three decimals.
//
// The weights come from Hugging Face at a pinned revision; every file is checked against its
// pinned SHA-256 before use, also when it comes back from the page's cache, and any file not
// pinned here (the vision encoder, other precisions) is never fetched.
//
// Loaded as a module Worker this file answers `probe`, `load` and `run` messages; on the main
// thread `llmExecutor` forwards leases to such a worker, one at a time.
/** @typedef {import('../api').TaskRequest} TaskRequest */
/** @typedef {import('../api').TaskOutcome} TaskOutcome */
/** @typedef {import('../api').FailKind} FailKind */
/** @typedef {import('../api').Executor} Executor */

const MiB = 2 ** 20;
export const LETTERS = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ';
export const MAX_STATE_CHARS = 2400;
export const MAX_RECORDS = 512;
export const MAX_INPUT = MiB;
export const MISSING_LOGIT = -1e4;
/** Below this `navigator.deviceMemory` (GB), when reported, the model is not offered. */
export const MIN_DEVICE_MEMORY_GB = 4;

export const MODEL = {
  id: 'onnx-community/Qwen3.5-0.8B-ONNX',
  revision: 'd1ea95774d40a29f0479bf9138cdf488a0394b6b',
  dtype: 'q4',
};

/** Every file the model needs, with its size and SHA-256 at `MODEL.revision`. */
/** @type {Record<string, { size: number, sha256: string }>} */
export const MODEL_FILES = {
  'config.json': { size: 2849, sha256: '36fed6a902ccd06ef19a452bd5a0750bd88fe347d06ab75ef515615bac5b296d' },
  'generation_config.json': { size: 248, sha256: 'dc0cbe66543f310896469b7b1448af792f403293a1080baaf04d586c57b23e48' },
  'tokenizer.json': { size: 19226111, sha256: '89da80cc6689bef4d90cc1028249436975ffb0814618f1d93c65310e05801a9b' },
  'tokenizer_config.json': { size: 10060, sha256: 'c4cd279f8fed1ddea0d23ab13e7ec255b15fcad0200ec0487fb853d880b0b40b' },
  'onnx/decoder_model_merged_q4.onnx': { size: 876359, sha256: 'cfb338c5112e11e2a123d6d03d8870cd800883ee333ec0a25289482b609a16f8' },
  'onnx/decoder_model_merged_q4.onnx_data': { size: 485425152, sha256: 'fb38200610a2206aa4c10f4c5f41c08107e2b0b44968166943b3937a1eb11f84' },
  'onnx/embed_tokens_q4.onnx': { size: 857, sha256: '8773dcf4858f855bfce13def4356ca17e0bc516a477154496b6ffb6dd6d084dc' },
  'onnx/embed_tokens_q4.onnx_data': { size: 162897920, sha256: '9210b1d26eb14136d3522584d17da14cc5f4b82b6f5607e6d80d59424a8cbe99' },
};

/** Bytes downloaded on first use (shown on the consent card). */
export const DOWNLOAD_BYTES = Object.values(MODEL_FILES).reduce((n, f) => n + f.size, 0);

/** Token ids of "A".."Z" in the pinned tokenizer (checked against the tokenizer at load). */
export const LETTER_IDS = Array.from({ length: 26 }, (_, i) => 32 + i);

const HINT = /** @type {Record<string, string>} */ ({
  choice: 'Choose the best option.',
  score: 'Choose the level that fits best.',
  noul: 'Decide whether the statement holds.',
});

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

/** Whether this device may offer the model. @param {number | undefined} deviceMemoryGb */
export function llmAllowed(deviceMemoryGb) {
  return deviceMemoryGb === undefined || deviceMemoryGb >= MIN_DEVICE_MEMORY_GB;
}

// Python's str.isspace, which str.strip() and Jinja's `trim` use. Every one is a single UTF-16
// code unit. A linear scan from both ends: a regex like /\s+$/ backtracks quadratically on a long
// run of spaces followed by another character.
const PY_SPACE = new Set([
  0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x1c, 0x1d, 0x1e, 0x1f, 0x20, 0x85, 0xa0, 0x1680,
  0x2000, 0x2001, 0x2002, 0x2003, 0x2004, 0x2005, 0x2006, 0x2007, 0x2008, 0x2009, 0x200a,
  0x2028, 0x2029, 0x202f, 0x205f, 0x3000,
]);

/** @param {string} s */
export function pyStrip(s) {
  let start = 0;
  let end = s.length;
  while (start < end && PY_SPACE.has(s.charCodeAt(start))) start++;
  while (end > start && PY_SPACE.has(s.charCodeAt(end - 1))) end--;
  return s.slice(start, end);
}

/** @typedef {{ t: string, ins: string, opts: string[], state?: string | null }} DecisionRecord */

/** The user message of a record, as teacher-score.py builds it. @param {DecisionRecord} r */
export function userContent(r) {
  const opts = r.opts.map((o, i) => `${LETTERS[i]}. ${o}`).join('\n');
  const state = Array.from(pyStrip(r.state ?? '')).slice(0, MAX_STATE_CHARS).join('');
  const parts = [];
  if (state) parts.push(`Context:\n${state}`);
  parts.push(`Question: ${pyStrip(r.ins)}\n${HINT[r.t] ?? ''}`);
  parts.push(`Options:\n${opts}`);
  parts.push('Answer with the letter of the correct option only.');
  return parts.join('\n\n');
}

/** Qwen3.5's chat template, one user message, non-thinking generation prompt. @param {DecisionRecord} r */
export function renderPrompt(r) {
  return `<|im_start|>user\n${pyStrip(userContent(r))}<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n`;
}

/**
 * Decodes and checks a task input; throws a TaskFail of kind `invalid`.
 * @param {Uint8Array} input
 * @returns {{ model: string | null, records: Array<{ uid: number, rec: DecisionRecord }> }}
 */
export function parseTask(input) {
  if (input.byteLength > MAX_INPUT) throw new TaskFail('invalid', 'LLM input above 1 MiB');
  let text;
  try {
    text = new TextDecoder('utf-8', { fatal: true }).decode(input);
  } catch {
    throw new TaskFail('invalid', 'LLM input is not UTF-8');
  }
  const lines = text.split('\n');
  if (lines.length && lines[lines.length - 1] === '') lines.pop();
  /** @type {string | null} */
  let model = null;
  if (lines.length) {
    const head = tryJson(lines[0].replace(/\r$/, ''));
    if (head && typeof head === 'object' && 'llm' in head && Object.keys(head).length === 1) {
      const m = head.llm && typeof head.llm === 'object' ? head.llm.model : undefined;
      model = typeof m === 'string' ? m : null;
      lines.shift();
    }
  }
  const records = [];
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i].replace(/\r$/, '');
    if (!pyStrip(line)) continue;
    const r = tryJson(line);
    const ok =
      r && typeof r === 'object' && !Array.isArray(r) &&
      typeof r.t === 'string' && typeof r.ins === 'string' &&
      Array.isArray(r.opts) && r.opts.every((/** @type {unknown} */ o) => typeof o === 'string') &&
      (r.state === undefined || r.state === null || typeof r.state === 'string');
    if (!ok) throw new TaskFail('invalid', `record ${i}: not a decision record`);
    if (r.opts.length < 2 || r.opts.length > LETTERS.length) {
      throw new TaskFail('invalid', `record ${i}: ${r.opts.length} options (2 to 26 allowed)`);
    }
    if (records.length === MAX_RECORDS) throw new TaskFail('invalid', 'more than 512 records in one task');
    records.push({ uid: i, rec: /** @type {DecisionRecord} */ (r) });
  }
  return { model, records };
}

/** @param {string} s @returns {any} */
function tryJson(s) {
  try {
    return JSON.parse(s);
  } catch {
    return undefined;
  }
}

/** @param {Array<{ uid: number, logits: number[] }>} rows @returns {Uint8Array} */
export function encodeOutput(rows) {
  const text = rows
    .map(({ uid, logits }) => `{"uid":${uid},"tlogits":${JSON.stringify(logits.map((v) => Math.round(v * 1000) / 1000))}}\n`)
    .join('');
  return new TextEncoder().encode(text);
}

/** Whether the job's model is the one this page runs. @param {string | null} model */
export function modelMatches(model) {
  return model === null || /qwen3\.5-0\.8b/i.test(model);
}

// ---------------------------------------------------------------------------------------------
// Pinned downloads.

const PREFIX = `https://huggingface.co/${MODEL.id}/resolve/${MODEL.revision}/`;

/**
 * The pinned file a URL or cache key names, if any. The library does not always pass the
 * revision on, so any revision segment is accepted: the bytes always come from the pinned
 * revision and are checked against its hashes.
 * @param {string} key
 */
export function pinnedFile(key) {
  const marker = `huggingface.co/${MODEL.id}/resolve/`;
  const at = key.indexOf(marker);
  if (at < 0) return null;
  const rest = key.slice(at + marker.length).split(/[?#]/)[0];
  const slash = rest.indexOf('/');
  const name = slash > 0 ? rest.slice(slash + 1) : '';
  return Object.hasOwn(MODEL_FILES, name) ? name : null;
}

/** @param {ArrayBuffer} buf */
async function sha256Hex(buf) {
  const d = new Uint8Array(await crypto.subtle.digest('SHA-256', buf));
  return Array.from(d, (b) => b.toString(16).padStart(2, '0')).join('');
}

/** Checks `buf` against the pin of `name`. @param {string} name @param {ArrayBuffer} buf */
async function matchesPin(name, buf) {
  const pin = MODEL_FILES[name];
  return buf.byteLength === pin.size && (await sha256Hex(buf)) === pin.sha256;
}

/** @param {ArrayBuffer} buf */
function verifiedResponse(buf) {
  return new Response(buf, { status: 200, headers: { 'content-length': String(buf.byteLength) } });
}

const CACHE = `nuee-llm-${MODEL.revision}`;

/** A cache for Transformers.js that hands back only intact pinned files. */
const pinnedCache = {
  /** @param {string} key */
  async match(key) {
    const name = pinnedFile(String(key));
    if (!name || typeof caches === 'undefined') return undefined;
    const c = await caches.open(CACHE);
    const hit = await c.match(PREFIX + name);
    if (!hit) return undefined;
    const buf = await hit.arrayBuffer();
    if (await matchesPin(name, buf)) return verifiedResponse(buf);
    await c.delete(PREFIX + name);
    return undefined;
  },
  /** @param {string} key @param {Response} resp */
  async put(key, resp) {
    const name = pinnedFile(String(key));
    if (!name || typeof caches === 'undefined') return;
    const c = await caches.open(CACHE);
    await c.put(PREFIX + name, resp);
  },
};

/**
 * fetch() for Transformers.js: only pinned files, each checked before use. Anything else
 * answers 404, which Transformers.js treats as an absent optional file. The library first
 * probes each file's size with `Range: bytes=0-0`; such a probe is answered from the pin,
 * without a download (a partial body could never match the pinned hash).
 * @param {RequestInfo | URL} input @param {RequestInit} [init]
 */
export async function pinnedFetch(input, init) {
  const url = String(input instanceof Request ? input.url : input);
  const name = url.startsWith(`https://huggingface.co/${MODEL.id}/resolve/`) ? pinnedFile(url) : null;
  if (!name) return new Response(null, { status: 404, statusText: 'Not Found' });
  const headers = new Headers(init?.headers ?? (input instanceof Request ? input.headers : undefined));
  if (headers.has('range')) {
    return new Response(null, { status: 200, headers: { 'content-length': String(MODEL_FILES[name].size) } });
  }
  const res = await fetch(PREFIX + name, { ...init, credentials: 'omit', redirect: 'follow' });
  if (!res.ok) throw new Error(`${name}: HTTP ${res.status}`);
  const buf = await res.arrayBuffer();
  if (!(await matchesPin(name, buf))) throw new Error(`${name} does not match its pinned SHA-256`);
  return verifiedResponse(buf);
}

// ---------------------------------------------------------------------------------------------
// The model.

/**
 * @typedef {{
 *   tokenizer: { encode(text: string, o?: { add_special_tokens?: boolean }): number[] },
 *   model: ((inputs: Record<string, unknown>) => Promise<{ logits: { dims: number[], data: ArrayLike<number> } }>) & { sessions?: Record<string, { inputNames?: string[] }> },
 *   Tensor: new (type: string, data: unknown, dims: number[]) => unknown,
 *   keepLast: boolean,
 * }} Loaded
 */

/** @type {Promise<Loaded> | null} */
let loading = null;

async function hasWebGpu() {
  const gpu = /** @type {{ gpu?: { requestAdapter(): Promise<unknown> } }} */ (/** @type {unknown} */ (navigator)).gpu;
  if (!gpu) return false;
  try {
    return (await gpu.requestAdapter()) !== null;
  } catch {
    return false;
  }
}

/** @returns {Promise<Loaded>} */
function load() {
  if (loading) return loading;
  loading = (async () => {
    const vendor = new URL('../../vendor/transformers/', import.meta.url);
    const tf = await import(new URL('transformers.min.js', vendor).href);
    const { env, AutoTokenizer, Qwen3_5ForCausalLM, Tensor } = tf;
    env.allowLocalModels = false;
    env.allowRemoteModels = true;
    env.useBrowserCache = false;
    env.useCustomCache = true;
    env.customCache = pinnedCache;
    env.fetch = pinnedFetch;
    env.useFSCache = false;
    env.useWasmCache = false;
    const wasm = env.backends.onnx.wasm;
    wasm.wasmPaths = {
      mjs: new URL('ort-wasm-simd-threaded.asyncify.mjs', vendor).href,
      wasm: new URL('ort-wasm-simd-threaded.asyncify.wasm', vendor).href,
    };
    wasm.proxy = false;
    const device = (await hasWebGpu()) ? 'webgpu' : 'wasm';
    const tokenizer = await AutoTokenizer.from_pretrained(MODEL.id, { revision: MODEL.revision });
    for (let i = 0; i < LETTERS.length; i++) {
      const ids = tokenizer.encode(LETTERS[i], { add_special_tokens: false });
      if (ids.length !== 1 || ids[0] !== LETTER_IDS[i]) throw new Error(`letter ${LETTERS[i]} does not map to token ${LETTER_IDS[i]}`);
    }
    const model = await Qwen3_5ForCausalLM.from_pretrained(MODEL.id, { revision: MODEL.revision, dtype: MODEL.dtype, device });
    const decoder = model.sessions?.decoder_model_merged;
    const keepLast = Array.isArray(decoder?.inputNames) && decoder.inputNames.includes('num_logits_to_keep');
    return { tokenizer, model, Tensor, keepLast };
  })();
  loading.catch(() => {
    loading = null;
  });
  return loading;
}

/**
 * Scores a task. Records of equal token length share one forward pass (no padding, so the
 * result is exact), up to 8 at a time when the decoder keeps only the last position's logits.
 * @param {Uint8Array} input @param {number} deadlineMs
 * @returns {Promise<TaskOutcome>}
 */
export async function runLlm(input, deadlineMs) {
  const stop = performance.now() + deadlineMs;
  try {
    const task = parseTask(input);
    if (!modelMatches(task.model)) throw new TaskFail('unsupported', `this page runs Qwen3.5-0.8B, not ${task.model}`);
    const m = await load();
    const encoded = task.records.map(({ uid, rec }) => ({ uid, n: rec.opts.length, ids: m.tokenizer.encode(renderPrompt(rec), { add_special_tokens: false }) }));
    /** @type {Map<number, typeof encoded>} */
    const byLen = new Map();
    for (const e of encoded) {
      const g = byLen.get(e.ids.length) ?? [];
      g.push(e);
      byLen.set(e.ids.length, g);
    }
    /** @type {Map<number, number[]>} */
    const scores = new Map();
    const maxBatch = m.keepLast ? 8 : 1;
    for (const [len, group] of byLen) {
      for (let i = 0; i < group.length; i += maxBatch) {
        if (performance.now() > stop) throw new TaskFail('timeout', 'LLM task deadline passed');
        const batch = group.slice(i, i + maxBatch);
        const flat = BigInt64Array.from(batch.flatMap((e) => e.ids.map((x) => BigInt(x))));
        /** @type {Record<string, unknown>} */
        const inputs = {
          input_ids: new m.Tensor('int64', flat, [batch.length, len]),
          attention_mask: new m.Tensor('int64', new BigInt64Array(flat.length).fill(1n), [batch.length, len]),
        };
        if (m.keepLast) inputs.num_logits_to_keep = new m.Tensor('int64', BigInt64Array.from([1n]), []);
        const { logits } = await m.model(inputs);
        const [, t, v] = logits.dims;
        batch.forEach((e, b) => {
          const base = (b * t + (t - 1)) * v;
          scores.set(e.uid, LETTER_IDS.slice(0, e.n).map((id) => Number(logits.data[base + id])));
        });
      }
    }
    const rows = task.records.map(({ uid }) => ({ uid, logits: scores.get(uid) ?? [] }));
    return { ok: true, output: encodeOutput(rows) };
  } catch (e) {
    if (e instanceof TaskFail) return fail(e.kind, e.message);
    return fail('io', e instanceof Error ? e.message : String(e));
  }
}

// ---------------------------------------------------------------------------------------------
// Main thread.

/** @type {null | (() => Promise<boolean>)} */
let preloader = null;

/** Starts the download (after consent) so the first lease does not wait for it. */
export function preloadLlm() {
  return preloader ? preloader() : Promise.resolve(false);
}

/**
 * The executor of `llm` leases: a dedicated module Worker started by `spawn`, one lease at a
 * time. Loading the model has no deadline; each lease then has its own.
 * @param {() => Worker} spawn
 * @returns {Executor}
 */
export function llmExecutor(spawn) {
  /** @type {Promise<Worker | null> | null} */
  let ready = null;
  let chain = Promise.resolve();
  /** @param {Worker} worker @param {unknown} msg @param {number} limitMs @returns {Promise<TaskOutcome>} */
  const ask = (worker, msg, limitMs) =>
    new Promise((resolve) => {
      const backstop = setTimeout(() => {
        worker.terminate();
        ready = null;
        resolve(fail('timeout', `no answer from the LLM worker in ${limitMs} ms`));
      }, limitMs);
      worker.onmessage = (e) => {
        clearTimeout(backstop);
        const o = e.data;
        resolve(o && (o.ok === true || o.ok === false) ? o : fail('io', 'malformed reply from the LLM worker'));
      };
      worker.onerror = (e) => {
        clearTimeout(backstop);
        worker.terminate();
        ready = null;
        resolve(fail('io', e.message ?? 'LLM worker error'));
      };
      worker.postMessage(msg);
    });
  const start = () => {
    ready = (async () => {
      let worker;
      try {
        worker = spawn();
      } catch (e) {
        console.warn('LLM worker unavailable', e);
        return null;
      }
      // The first load downloads the weights: allow it 30 minutes.
      const o = await ask(worker, { op: 'load' }, 30 * 60 * 1000);
      if (!o.ok) {
        console.warn('LLM model did not load:', o.detail);
        worker.terminate();
        return null;
      }
      return worker;
    })();
    return ready;
  };
  /** @template T @param {() => Promise<T>} fn @returns {Promise<T>} */
  const serial = (fn) => {
    const out = chain.then(fn, fn);
    chain = out.then(
      () => undefined,
      () => undefined,
    );
    return out;
  };
  preloader = () => serial(async () => (await (ready ?? start())) !== null);
  return (req) =>
    serial(async () => {
      const worker = await (ready ?? start());
      if (!worker) return fail('unsupported', 'the language model could not be loaded in this browser');
      return ask(worker, { op: 'run', input: req.input, deadlineMs: req.deadlineMs }, req.deadlineMs + 2000);
    });
}

// Worker side.
const scope = /** @type {{ WorkerGlobalScope?: Function }} */ (/** @type {unknown} */ (globalThis));
if (typeof scope.WorkerGlobalScope === 'function' && globalThis instanceof scope.WorkerGlobalScope) {
  const port = /** @type {{ onmessage: ((e: MessageEvent) => void) | null, postMessage(m: unknown, t?: Transferable[]): void }} */ (/** @type {unknown} */ (globalThis));
  port.onmessage = (e) => {
    const msg = e.data;
    if (msg && msg.op === 'load') {
      load().then(
        () => port.postMessage({ ok: true, output: new Uint8Array(0) }),
        (err) => port.postMessage(fail('unsupported', err instanceof Error ? err.message : String(err))),
      );
      return;
    }
    if (!msg || msg.op !== 'run' || !(msg.input instanceof Uint8Array) || typeof msg.deadlineMs !== 'number') {
      port.postMessage(fail('invalid', 'unknown request'));
      return;
    }
    runLlm(msg.input, msg.deadlineMs).then(
      (o) => port.postMessage(o, o.ok ? [o.output.buffer] : []),
      (err) => port.postMessage(fail('io', String(err))),
    );
  };
}
