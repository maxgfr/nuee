// @ts-check
// Embedding executor for `embed` leases: all-MiniLM-L6-v2 (q8 ONNX) on the self-hosted
// Transformers.js / ONNX Runtime Web in web/vendor/transformers/, in a dedicated worker.
//
// Task format (NEM1, the same as crates/nuee-gpu/src/embed.rs, byte for byte), little-endian:
//   input:  "NEM1", u32 count (1..=256), then count times: u32 byte length (<= 8 KiB) + UTF-8
//   output: count x 384 f32, one L2-normalised row per text, in input order
// Texts are truncated to 256 tokens, embedded one at a time, mean-pooled over the attention mask, then normalised
// (sentence-transformers semantics). The model files are fetched from huggingface.co at a
// pinned revision and checked against their SHA-256 before Transformers.js sees them; the
// library and the ONNX Runtime wasm only ever load from this origin.
/** @typedef {import('../api').TaskOutcome} TaskOutcome */
/** @typedef {import('../api').TaskRequest} TaskRequest */
/** @typedef {import('../api').FailKind} FailKind */

export const DIM = 384;
export const MAX_TEXTS = 256;
export const MAX_TEXT_BYTES = 8 * 1024;
export const MAX_TOKENS = 256;

/** The browser model: repository, revision, and every file used, pinned by SHA-256. */
export const WEB_MODEL = Object.freeze({
  repo: 'Xenova/all-MiniLM-L6-v2',
  rev: '751bff37182d3f1213fa05d7196b954e230abad9',
  files: Object.freeze(
    /** @type {[string, number, string][]} */ ([
      ['config.json', 650, '7135149f7cffa1a573466c6e4d8423ed73b62fd2332c575bf738a0d033f70df7'],
      ['tokenizer.json', 711661, 'da0e79933b9ed51798a3ae27893d3c5fa4a201126cef75586296df9b4d2c62a0'],
      ['tokenizer_config.json', 366, '9261e7d79b44c8195c1cada2b453e55b00aeb81e907a6664974b4d7776172ab3'],
      ['onnx/model_quantized.onnx', 22972370, 'afdb6f1a0e45b715d0bb9b11772f032c399babd23bfc31fed1c170afc848bdb1'],
    ]).map(([path, size, sha256]) => Object.freeze({ path, size, sha256 })),
  ),
});

/** Where the vendored library lives, relative to this file. */
const VENDOR = new URL('../../vendor/transformers/', import.meta.url);

/** A failure with its kind. */
export class TaskFail extends Error {
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
 * Parses and checks an NEM1 input; throws a TaskFail of kind `invalid`.
 * @param {Uint8Array} input
 * @returns {string[]}
 */
export function decodeInput(input) {
  const bad = (/** @type {string} */ why) => new TaskFail('invalid', why);
  const view = new DataView(input.buffer, input.byteOffset, input.byteLength);
  let at = 0;
  const u32 = (/** @type {string} */ what) => {
    if (at + 4 > input.byteLength) throw bad(`NEM1 input truncated in ${what}`);
    const v = view.getUint32(at, true);
    at += 4;
    return v;
  };
  if (input.byteLength < 4 || input[0] !== 0x4e || input[1] !== 0x45 || input[2] !== 0x4d || input[3] !== 0x31) {
    throw bad('not an NEM1 input');
  }
  at = 4;
  const count = u32('the count');
  if (count === 0 || count > MAX_TEXTS) throw bad(`a task holds 1 to ${MAX_TEXTS} texts, not ${count}`);
  const utf8 = new TextDecoder('utf-8', { fatal: true });
  /** @type {string[]} */
  const texts = [];
  for (let i = 0; i < count; i += 1) {
    const len = u32('a text length');
    if (len > MAX_TEXT_BYTES) throw bad(`text ${i} is ${len} bytes, above ${MAX_TEXT_BYTES}`);
    if (at + len > input.byteLength) throw bad('NEM1 input truncated in a text');
    let text;
    try {
      text = utf8.decode(input.subarray(at, at + len));
    } catch {
      throw bad(`text ${i} is not UTF-8`);
    }
    texts.push(text);
    at += len;
  }
  if (at !== input.byteLength) throw bad('trailing bytes after the last text');
  return texts;
}

/**
 * Encodes the texts of one task.
 * @param {string[]} texts
 * @returns {Uint8Array}
 */
export function encodeInput(texts) {
  if (texts.length === 0 || texts.length > MAX_TEXTS) throw new TaskFail('invalid', `a task holds 1 to ${MAX_TEXTS} texts`);
  const enc = new TextEncoder();
  const parts = texts.map((t) => enc.encode(t));
  const out = new Uint8Array(8 + parts.reduce((n, p) => n + 4 + p.byteLength, 0));
  const view = new DataView(out.buffer);
  out.set([0x4e, 0x45, 0x4d, 0x31]);
  view.setUint32(4, parts.length, true);
  let at = 8;
  for (const p of parts) {
    if (p.byteLength > MAX_TEXT_BYTES) throw new TaskFail('invalid', `a text is above ${MAX_TEXT_BYTES} bytes`);
    view.setUint32(at, p.byteLength, true);
    out.set(p, at + 4);
    at += 4 + p.byteLength;
  }
  return out;
}

/** @param {Uint8Array} bytes */
async function sha256Hex(bytes) {
  const digest = new Uint8Array(await crypto.subtle.digest('SHA-256', /** @type {Uint8Array<ArrayBuffer>} */ (bytes)));
  return Array.from(digest, (b) => b.toString(16).padStart(2, '0')).join('');
}

/** @param {{ path: string }} file */
export function modelUrl(file) {
  return `https://huggingface.co/${WEB_MODEL.repo}/resolve/${WEB_MODEL.rev}/${file.path}`;
}

/**
 * Downloads every pinned model file and checks it; throws a TaskFail (`io` when the
 * download fails, `invalid` when a file does not match its pin).
 * @param {(url: string) => Promise<Response>} [get]
 * @returns {Promise<Map<string, Uint8Array>>} bytes by URL
 */
export async function fetchModel(get = (url) => fetch(url, { credentials: 'omit', cache: 'force-cache' })) {
  /** @type {Map<string, Uint8Array>} */
  const files = new Map();
  for (const file of WEB_MODEL.files) {
    const url = modelUrl(file);
    let bytes;
    try {
      const res = await get(url);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      bytes = new Uint8Array(await res.arrayBuffer());
    } catch (e) {
      throw new TaskFail('io', `cannot download ${url}: ${String(e)}`);
    }
    if (bytes.byteLength !== file.size || (await sha256Hex(bytes)) !== file.sha256) {
      throw new TaskFail('invalid', `${file.path} does not match its pinned SHA-256`);
    }
    files.set(url, bytes);
  }
  return files;
}

/**
 * @typedef {{ embed(texts: string[]): Promise<Float32Array> }} Embedder
 * @typedef {{ dims: number[], data: ArrayLike<number | bigint> }} TensorLike
 * @typedef {{
 *   env: {
 *     allowLocalModels: boolean, allowRemoteModels: boolean, useBrowserCache: boolean,
 *     useFSCache: boolean, useCustomCache: boolean, useWasmCache: boolean,
 *     fetch: (input: string | URL | Request) => Promise<Response>,
 *     backends: { onnx: { wasm: { wasmPaths: { mjs: string, wasm: string } | string, numThreads: number, proxy: boolean } } },
 *   },
 *   AutoTokenizer: { from_pretrained(repo: string, opts: object): Promise<(texts: string[], o: object) => { attention_mask: TensorLike }> },
 *   AutoModel: { from_pretrained(repo: string, opts: object): Promise<(inputs: object) => Promise<{ last_hidden_state: TensorLike }>> },
 * }} Transformers The parts of Transformers.js used here.
 */

/**
 * Loads the vendored Transformers.js and the model from already verified bytes. Every
 * request the library makes is answered from `files`; anything else gets a 404, so it can
 * neither fetch nor run anything unverified.
 * @param {Map<string, Uint8Array>} files bytes by URL, from fetchModel
 * @param {URL} [vendor] the vendored library directory
 * @returns {Promise<Embedder>}
 */
export async function loadEmbedder(files, vendor = VENDOR) {
  const tf = /** @type {Transformers} */ (await import(new URL('transformers.min.js', vendor).href));
  const env = tf.env;
  env.allowLocalModels = false;
  env.allowRemoteModels = true;
  env.useBrowserCache = false;
  env.useFSCache = false;
  env.useCustomCache = false;
  env.useWasmCache = false;
  env.fetch = async (/** @type {string | URL | Request} */ input) => {
    const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;
    // The library does not always pass the revision on: answer by file path, from the pinned
    // revision's verified bytes.
    const prefix = `https://huggingface.co/${WEB_MODEL.repo}/resolve/`;
    const rest = url.startsWith(prefix) ? url.slice(prefix.length) : '';
    const slash = rest.indexOf('/');
    const bytes = slash > 0 ? files.get(modelUrl({ path: rest.slice(slash + 1) })) : undefined;
    if (!bytes) return new Response(null, { status: 404, statusText: 'Not Found' });
    return new Response(/** @type {Uint8Array<ArrayBuffer>} */ (bytes.slice()), { status: 200, headers: { 'content-length': String(bytes.byteLength) } });
  };
  const wasm = env.backends.onnx.wasm;
  wasm.wasmPaths = {
    mjs: new URL('ort-wasm-simd-threaded.asyncify.mjs', vendor).href,
    wasm: new URL('ort-wasm-simd-threaded.asyncify.wasm', vendor).href,
  };
  wasm.numThreads = 1;
  wasm.proxy = false;
  const opts = { revision: WEB_MODEL.rev };
  const tokenizer = await tf.AutoTokenizer.from_pretrained(WEB_MODEL.repo, opts);
  const model = await tf.AutoModel.from_pretrained(WEB_MODEL.repo, { ...opts, dtype: 'q8', device: 'wasm' });
  return {
    async embed(texts) {
      const out = new Float32Array(texts.length * DIM);
      // One text per inference: the q8 model quantizes activations over the whole batch, so
      // batching would make a text's vector depend on its neighbours (and their padding).
      for (let b = 0; b < texts.length; b += 1) {
        const batch = texts.slice(b, b + 1);
        const inputs = tokenizer(batch, { padding: true, truncation: true, max_length: MAX_TOKENS });
        const { last_hidden_state: hidden } = await model(inputs);
        const [n, len, dim] = hidden.dims;
        if (dim !== DIM || n !== batch.length) throw new TaskFail('io', 'unexpected model output shape');
        const h = /** @type {Float32Array} */ (hidden.data);
        const mask = inputs.attention_mask.data;
        for (let i = 0; i < n; i += 1) {
          const row = new Float64Array(DIM);
          let count = 0;
          for (let t = 0; t < len; t += 1) {
            if (Number(mask[i * len + t]) === 0) continue;
            count += 1;
            const base = (i * len + t) * DIM;
            for (let d = 0; d < DIM; d += 1) row[d] += h[base + d];
          }
          let norm = 0;
          for (let d = 0; d < DIM; d += 1) {
            row[d] /= Math.max(count, 1e-9);
            norm += row[d] * row[d];
          }
          norm = Math.max(Math.sqrt(norm), 1e-12);
          for (let d = 0; d < DIM; d += 1) out[(b + i) * DIM + d] = row[d] / norm;
        }
      }
      return out;
    },
  };
}

/** @type {Promise<Embedder> | null} */
let loading = null;

/** The worker's embedder, loaded once (a failed load is retried on the next lease). */
function embedder() {
  if (!loading) {
    loading = fetchModel().then((files) => loadEmbedder(files));
    loading.catch(() => {
      loading = null;
    });
  }
  return loading;
}

/**
 * Runs one lease in this realm.
 * @param {{ input: Uint8Array }} req
 * @returns {Promise<TaskOutcome>}
 */
export async function runRequest(req) {
  try {
    const texts = decodeInput(req.input);
    const vectors = await (await embedder()).embed(texts);
    return { ok: true, output: new Uint8Array(vectors.buffer, vectors.byteOffset, vectors.byteLength) };
  } catch (e) {
    if (e instanceof TaskFail) return fail(e.kind, e.message);
    return fail('io', String(e));
  }
}

/**
 * The `embed` executor: leases run one at a time in a dedicated worker, which keeps the model
 * between leases and is replaced after a lease overruns its deadline.
 * @param {() => Worker} spawn
 * @returns {(req: TaskRequest) => Promise<TaskOutcome>}
 */
export function embedExecutor(spawn) {
  /** @type {Worker | null} */
  let worker = null;
  let chain = Promise.resolve();
  /** @param {TaskRequest} req @returns {Promise<TaskOutcome>} */
  const run = (req) =>
    new Promise((resolve) => {
      let w;
      try {
        w = worker ?? spawn();
      } catch (e) {
        resolve(fail('unsupported', `embedding worker unavailable: ${String(e)}`));
        return;
      }
      worker = w;
      const reset = () => {
        w.terminate();
        if (worker === w) worker = null;
      };
      const backstop = setTimeout(() => {
        reset();
        resolve(fail('timeout', `deadline of ${req.deadlineMs} ms exceeded`));
      }, req.deadlineMs);
      w.onmessage = (e) => {
        clearTimeout(backstop);
        const o = e.data;
        resolve(o && (o.ok === true || o.ok === false) ? o : fail('io', 'malformed reply from the embedding worker'));
      };
      w.onerror = (e) => {
        clearTimeout(backstop);
        reset();
        resolve(fail('io', e.message ?? 'embedding worker error'));
      };
      const input = req.input.slice();
      w.postMessage({ op: 'run', lease: req.lease, input }, [input.buffer]);
    });
  return (req) => {
    const out = chain.then(() => run(req));
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
    if (!msg || msg.op !== 'run' || !(msg.input instanceof Uint8Array)) {
      port.postMessage(fail('invalid', 'unknown request'));
      return;
    }
    runRequest(msg).then(
      (o) => port.postMessage(o, o.ok ? [o.output.buffer] : []),
      (err) => port.postMessage(fail('io', String(err))),
    );
  };
}
