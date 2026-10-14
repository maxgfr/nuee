// @ts-check
// Task inputs and result merges of the catalogue jobs that mirror a CLI verb or preset:
// embeddings (`nuee embed`, crates/nuee/src/cmd/embed.rs) and skycmd DAgger records
// (`demo:skycmd-dagger`, crates/nuee/src/cmd/run.rs). Byte for byte what the CLI builds.
import { encodeInput, DIM } from '../worker/embed.js';

/** Texts per embedding task (embed.rs TEXTS_PER_TASK). */
export const TEXTS_PER_TASK = 64;
/** Minimum cosine between two executions of a text (nuee_gpu::embed::TAU). */
export const EMBED_TAU = 0.966;
/** Target chunk length and overlap, in characters (nuee_gpu::embed::CHUNK_*). */
export const CHUNK_CHARS = 1000;
export const CHUNK_OVERLAP = 200;

/** Rust's `char::is_whitespace` (Unicode White_Space). @param {string | undefined} c */
function isSpace(c) {
  if (c === undefined) return false;
  const p = /** @type {number} */ (c.codePointAt(0));
  return (
    (p >= 0x09 && p <= 0x0d) ||
    p === 0x20 ||
    p === 0x85 ||
    p === 0xa0 ||
    p === 0x1680 ||
    (p >= 0x2000 && p <= 0x200a) ||
    p === 0x2028 ||
    p === 0x2029 ||
    p === 0x202f ||
    p === 0x205f ||
    p === 0x3000
  );
}

/**
 * Port of `nuee_gpu::embed::chunk_text`: chunks of about CHUNK_CHARS characters overlapping by
 * about CHUNK_OVERLAP, as UTF-8 byte offsets.
 * @param {string} text
 * @returns {Array<{ start: number, end: number }>}
 */
export function chunkText(text) {
  const chars = Array.from(text);
  const n = chars.length;
  const enc = new TextEncoder();
  /** @type {number[]} */
  const bytes = [0];
  for (const c of chars) bytes.push(/** @type {number} */ (bytes[bytes.length - 1]) + enc.encode(c).byteLength);
  const byteAt = (/** @type {number} */ i) => /** @type {number} */ (bytes[Math.min(i, n)]);
  const ch = (/** @type {number} */ i) => (i >= 0 && i < n ? chars[i] : '\0');
  /** @type {Array<{ start: number, end: number }>} */
  const out = [];
  let start = 0;
  while (start < n) {
    while (start < n && isSpace(ch(start))) start += 1;
    if (start >= n) break;
    let end = Math.min(start + CHUNK_CHARS, n);
    if (end < n) {
      const floor = start + CHUNK_CHARS / 2;
      const cutAfter = (/** @type {(i: number) => boolean} */ pred) => {
        for (let i = end - 1; i >= floor; i -= 1) if (pred(i)) return i;
        return -1;
      };
      let i = cutAfter((k) => ch(k) === '\n' && k > 0 && ch(k - 1) === '\n');
      if (i < 0) i = cutAfter((k) => ['.', '!', '?', '\n'].includes(ch(k) ?? '') && (k + 1 >= n || isSpace(ch(k + 1))));
      if (i < 0) i = cutAfter((k) => isSpace(ch(k)));
      if (i >= 0) end = i + 1;
    }
    let last = end;
    while (last > start && isSpace(ch(last - 1))) last -= 1;
    out.push({ start: byteAt(start), end: byteAt(last) });
    if (end >= n) break;
    let next = Math.max(end - CHUNK_OVERLAP, 0, start + 1);
    const limit = Math.min(next + CHUNK_OVERLAP / 2, end);
    let j = next;
    while (j < limit && !isSpace(ch(j - 1))) j += 1;
    if (j < limit) next = j;
    start = next;
  }
  return out;
}

/**
 * @typedef {{ id: number, path: string, line?: number, chunk: number, start: number, end: number }} ChunkMeta
 * @typedef {{ meta: ChunkMeta[], texts: string[] }} EmbedJob
 */

/**
 * One text per non-empty line, the `text` field of a JSON object line, else the raw line;
 * each chunked as `EmbedJob::from_jsonl` does. Throws on an object line without a string `text`.
 * @param {string} name file name written into meta.jsonl
 * @param {string} raw file content
 * @returns {EmbedJob}
 */
export function embedJob(name, raw) {
  /** @type {EmbedJob} */
  const job = { meta: [], texts: [] };
  const enc = new TextEncoder();
  const dec = new TextDecoder();
  // Rust's str::lines: split on \n, drop one trailing \r, no last empty line.
  const lines = raw.split('\n');
  if (lines[lines.length - 1] === '') lines.pop();
  lines.forEach((l, n) => {
    const line = l.endsWith('\r') ? l.slice(0, -1) : l;
    if (line.trim() === '') return;
    let text = line;
    /** @type {unknown} */
    let v;
    try {
      v = JSON.parse(line);
    } catch {
      v = undefined;
    }
    if (v !== null && typeof v === 'object' && !Array.isArray(v)) {
      const t = /** @type {Record<string, unknown>} */ (v).text;
      if (typeof t !== 'string') throw { code: 'invalid_argument', message: `${name}:${n + 1}: a JSON object needs a string \`text\` field` };
      text = t;
    }
    const b = enc.encode(text);
    chunkText(text).forEach((c, chunk) => {
      job.meta.push({ id: job.meta.length, path: name, line: n, chunk, start: c.start, end: c.end });
      job.texts.push(dec.decode(b.subarray(c.start, c.end)));
    });
  });
  return job;
}

/** NEM1 inputs, TEXTS_PER_TASK texts each. @param {EmbedJob} job @returns {Uint8Array[]} */
export function embedInputs(job) {
  const out = [];
  for (let i = 0; i < job.texts.length; i += TEXTS_PER_TASK) out.push(encodeInput(job.texts.slice(i, i + TEXTS_PER_TASK)));
  return out;
}

/**
 * `vectors.f32` (rows in meta order) and `meta.jsonl`, as `EmbedJob::write_outputs` writes them.
 * @param {EmbedJob} job
 * @param {Map<number, Uint8Array>} outputs task output bytes by task index
 * @returns {{ vectors: Uint8Array, meta: string }}
 */
export function mergeEmbed(job, outputs) {
  const tasks = Math.ceil(job.texts.length / TEXTS_PER_TASK);
  const vectors = new Uint8Array(job.texts.length * DIM * 4);
  for (let task = 0; task < tasks; task += 1) {
    const count = Math.min(TEXTS_PER_TASK, job.texts.length - task * TEXTS_PER_TASK);
    const bytes = outputs.get(task);
    if (!bytes) throw new Error(`task ${task} has no output`);
    if (bytes.byteLength !== count * DIM * 4) throw new Error(`task ${task} output has ${bytes.byteLength} bytes, expected ${count * DIM * 4}`);
    vectors.set(bytes, task * TEXTS_PER_TASK * DIM * 4);
  }
  const meta = job.meta
    .map((m) => JSON.stringify(m.line === undefined ? { id: m.id, path: m.path, chunk: m.chunk, start: m.start, end: m.end } : { id: m.id, path: m.path, line: m.line, chunk: m.chunk, start: m.start, end: m.end }))
    .map((l) => `${l}\n`)
    .join('');
  return { vectors, meta };
}

/** The `demo:skycmd-dagger` preset (run.rs SKY_*). */
// SKY_TAU is an f32 there: serde_json writes it widened to f64 (0.6000000238418579), so the
// same value is rounded to f32 here before JSON.stringify writes it.
export const DAGGER = Object.freeze({ games: ['flappy', 'rescue', 'dodge', 'doom'], seeds: 4, n: 4000, tau: Math.fround(0.6), maxPipes: 200 });

/**
 * One input per game and seed, with the keys in serde_json's (sorted) order.
 * @param {typeof DAGGER} d
 * @returns {Uint8Array[]}
 */
export function daggerInputs(d = DAGGER) {
  const enc = new TextEncoder();
  const out = [];
  for (const game of d.games) {
    for (let seed = 1; seed <= d.seeds; seed += 1) {
      out.push(enc.encode(JSON.stringify({ game, max_pipes: d.maxPipes, mode: 'dagger', n: d.n, seed, tau: d.tau })));
    }
  }
  return out;
}

/**
 * The records of every task, in task order, as one JSONL text.
 * @param {Map<number, Uint8Array>} outputs
 * @returns {string}
 */
export function mergeJsonl(outputs) {
  const dec = new TextDecoder('utf-8', { fatal: true });
  return [...outputs]
    .sort((a, b) => a[0] - b[0])
    .map(([, bytes]) => dec.decode(bytes))
    .filter((s) => s.length > 0)
    .map((s) => (s.endsWith('\n') ? s : `${s}\n`))
    .join('');
}
