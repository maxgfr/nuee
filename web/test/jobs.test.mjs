// @ts-check
// Inputs and merges of the embedding and skycmd DAgger catalogue jobs: the same bytes as
// `nuee embed` (embed.rs) and `demo:skycmd-dagger` (run.rs) build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { chunkText, embedJob, embedInputs, mergeEmbed, daggerInputs, mergeJsonl, TEXTS_PER_TASK } from '../app/lib/jobs.js';

/** NEM1 by hand: magic, u32 count, then u32 length + UTF-8 per text. @param {string[]} texts */
function nem1(texts) {
  const parts = texts.map((t) => Buffer.from(t, 'utf8'));
  const head = Buffer.alloc(8);
  head.write('NEM1', 0, 'latin1');
  head.writeUInt32LE(texts.length, 4);
  return new Uint8Array(Buffer.concat([head, ...parts.flatMap((p) => { const l = Buffer.alloc(4); l.writeUInt32LE(p.length); return [l, p]; })]));
}

test('embedding job: one text per non-empty line, JSONL text fields, CLI meta lines', () => {
  const job = embedJob('notes.jsonl', 'Bonjour le monde\n\n  \n{"text":"Été à Paris"}\r\n42\n');
  assert.deepEqual(job.texts, ['Bonjour le monde', 'Été à Paris', '42']);
  assert.deepEqual(job.meta[1], { id: 1, path: 'notes.jsonl', line: 3, chunk: 0, start: 0, end: 14 });
  assert.throws(() => embedJob('x.jsonl', '{"title":"no text"}\n'));
  const inputs = embedInputs(job);
  assert.equal(inputs.length, 1);
  assert.deepEqual(inputs[0], nem1(job.texts));
});

test('embedding job: 64 texts per task, long lines chunked like chunk_text', () => {
  const lines = Array.from({ length: 130 }, (_, i) => `ligne ${i}`).join('\n');
  const job = embedJob('a.txt', lines);
  const inputs = embedInputs(job);
  assert.equal(inputs.length, 3);
  assert.deepEqual(inputs[2], nem1(job.texts.slice(2 * TEXTS_PER_TASK)));
  // 1500 characters without a break: one cut at 1000, the next chunk backs off by 200.
  const long = 'a'.repeat(1500);
  assert.deepEqual(chunkText(long), [{ start: 0, end: 1000 }, { start: 800, end: 1500 }]);
  // A sentence end in the second half of the window is preferred.
  const s = `${'b'.repeat(700)}. ${'c'.repeat(600)}`;
  assert.equal(chunkText(s)[0]?.end, 701);
  assert.deepEqual(chunkText('  \n '), []);
});

test('embedding merge: rows in task order, meta.jsonl as the CLI writes it', () => {
  const job = embedJob('t.txt', Array.from({ length: 65 }, (_, i) => `t${i}`).join('\n'));
  const row = 384 * 4;
  const a = new Uint8Array(64 * row).fill(1);
  const b = new Uint8Array(row).fill(2);
  const { vectors, meta } = mergeEmbed(job, new Map([[1, b], [0, a]]));
  assert.equal(vectors.byteLength, 65 * row);
  assert.equal(vectors[64 * row - 1], 1);
  assert.equal(vectors[64 * row], 2);
  const lines = meta.trimEnd().split('\n');
  assert.equal(lines.length, 65);
  assert.equal(lines[0], '{"id":0,"path":"t.txt","line":0,"chunk":0,"start":0,"end":2}');
  assert.throws(() => mergeEmbed(job, new Map([[0, a]])));
  assert.throws(() => mergeEmbed(job, new Map([[0, a], [1, a]])));
});

test('skycmd DAgger: the CLI preset inputs and a merge in task order', () => {
  const inputs = daggerInputs();
  assert.equal(inputs.length, 16);
  assert.equal(new TextDecoder().decode(inputs[0]), '{"game":"flappy","max_pipes":200,"mode":"dagger","n":4000,"seed":1,"tau":0.6000000238418579}');
  assert.equal(new TextDecoder().decode(inputs[15]), '{"game":"doom","max_pipes":200,"mode":"dagger","n":4000,"seed":4,"tau":0.6000000238418579}');
  const enc = new TextEncoder();
  const merged = mergeJsonl(new Map([[1, enc.encode('{"b":1}\n{"b":2}\n')], [0, enc.encode('{"a":1}')], [2, new Uint8Array(0)]]));
  assert.equal(merged, '{"a":1}\n{"b":1}\n{"b":2}\n');
});
