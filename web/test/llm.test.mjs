// @ts-check
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  renderPrompt,
  parseTask,
  encodeOutput,
  llmAllowed,
  modelMatches,
  pinnedFile,
  pinnedFetch,
  pyStrip,
  DOWNLOAD_BYTES,
  MODEL_FILES,
  LETTER_IDS,
} from '../app/worker/llm.js';

const TAIL = '<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n';

// The same three records and expected prompts as crates/nuee-gpu/tests/llm.rs (teacher-score.py
// with the pinned chat template).
test('prompts match the teacher byte for byte', () => {
  assert.equal(
    renderPrompt({ t: 'choice', ins: '  Which way now?\n', opts: ['left', 'right', 'stay'], state: '\n  bird y=0.42 — gap at 0.55\n  vy=-0.1 \u001f ' }),
    '<|im_start|>user\nContext:\nbird y=0.42 — gap at 0.55\n  vy=-0.1\n\nQuestion: Which way now?\nChoose the best option.\n\nOptions:\nA. left\nB. right\nC. stay\n\nAnswer with the letter of the correct option only.' + TAIL,
  );
  assert.equal(
    renderPrompt({ t: 'score', ins: 'How risky is the next pipe?', opts: ['low', 'medium', 'high', 'extreme'], state: null }),
    '<|im_start|>user\nQuestion: How risky is the next pipe?\nChoose the level that fits best.\n\nOptions:\nA. low\nB. medium\nC. high\nD. extreme\n\nAnswer with the letter of the correct option only.' + TAIL,
  );
  assert.equal(
    renderPrompt({ t: 'noul', ins: 'The exit is reachable.', opts: ['yes', 'no'], state: 'é'.repeat(2000) + 'x'.repeat(1000) }),
    `<|im_start|>user\nContext:\n${'é'.repeat(2000)}${'x'.repeat(400)}\n\nQuestion: The exit is reachable.\nDecide whether the statement holds.\n\nOptions:\nA. yes\nB. no\n\nAnswer with the letter of the correct option only.` + TAIL,
  );
  // Python strips U+001F but not U+FEFF.
  assert.equal(pyStrip('\u001f a ﻿'), 'a ﻿');
});

test('pyStrip stays linear on long whitespace runs', () => {
  const run = ' '.repeat(1 << 20);
  const t0 = performance.now();
  assert.equal(pyStrip(`a${run}b`), `a${run}b`);
  assert.equal(pyStrip(`${run}x${run}`), 'x');
  assert.equal(pyStrip(`\u3000\u2028${run}`), '');
  const ms = performance.now() - t0;
  assert.ok(ms < 1000, `pyStrip took ${Math.round(ms)} ms on 1 MiB of spaces`);
  assert.equal(pyStrip('\t\n\x0b\x0c\r\x1c\x85\xa0 a b \u1680\u200a\u202f\u205f'), 'a b');
  assert.equal(pyStrip('\u200b a \u200b'), '\u200b a \u200b');
});

test('task inputs are parsed like the native client', () => {
  const enc = new TextEncoder();
  const text = '{"llm":{"model":"qwen3.5-0.8b"}}\n{"t":"choice","ins":"a","opts":["x","y"]}\n\n{"t":"noul","ins":"b","opts":["yes","no"],"uid":77}\n';
  const task = parseTask(enc.encode(text));
  assert.equal(task.model, 'qwen3.5-0.8b');
  assert.deepEqual(task.records.map((r) => r.uid), [0, 2]);
  assert.throws(() => parseTask(enc.encode('{"t":"choice","ins":"a","opts":["x"]}')), /options/);
  assert.throws(() => parseTask(enc.encode('{"t":1}')), /decision record/);
  assert.throws(() => parseTask(enc.encode('{"t":"choice","ins":"a","opts":["x","y"]}\n'.repeat(513))), /512/);
  assert.throws(() => parseTask(new Uint8Array(2 ** 20 + 1)), /1 MiB/);
});

test('outputs, model names and device limits', () => {
  const out = new TextDecoder().decode(encodeOutput([{ uid: 0, logits: [-3, -2.00049, -0.25] }, { uid: 1, logits: [-0.25, -2] }]));
  assert.equal(out, '{"uid":0,"tlogits":[-3,-2,-0.25]}\n{"uid":1,"tlogits":[-0.25,-2]}\n');
  assert.ok(modelMatches(null));
  assert.ok(modelMatches('Qwen/Qwen3.5-0.8B'));
  assert.ok(!modelMatches('Qwen3.5-4B'));
  assert.ok(llmAllowed(undefined));
  assert.ok(llmAllowed(4));
  assert.ok(!llmAllowed(2));
});

test('only pinned model files are fetched', () => {
  const base = 'https://huggingface.co/onnx-community/Qwen3.5-0.8B-ONNX/resolve/d1ea95774d40a29f0479bf9138cdf488a0394b6b/';
  assert.equal(pinnedFile(`${base}onnx/decoder_model_merged_q4.onnx_data`), 'onnx/decoder_model_merged_q4.onnx_data');
  assert.equal(pinnedFile(`${base}onnx/vision_encoder_q4.onnx`), null);
  assert.equal(pinnedFile(`${base}onnx/decoder_model_merged.onnx`), null);
  // Whatever revision the library names, the pinned one is served (and hash-checked).
  assert.equal(pinnedFile('https://huggingface.co/onnx-community/Qwen3.5-0.8B-ONNX/resolve/main/config.json'), 'config.json');
  assert.equal(pinnedFile('https://huggingface.co/someone/else/resolve/main/config.json'), null);
  for (const f of Object.values(MODEL_FILES)) assert.match(f.sha256, /^[0-9a-f]{64}$/);
  assert.equal(DOWNLOAD_BYTES, 668439556);
  assert.deepEqual([LETTER_IDS[0], LETTER_IDS[25]], [32, 57]);
});

test('size probes are answered from the pins without a download', async () => {
  const real = globalThis.fetch;
  let calls = 0;
  globalThis.fetch = async () => {
    calls += 1;
    return new Response(new Uint8Array([0x7b]), { status: 206 });
  };
  try {
    const url = `https://huggingface.co/onnx-community/Qwen3.5-0.8B-ONNX/resolve/main/tokenizer_config.json`;
    const probe = await pinnedFetch(url, { method: 'GET', headers: new Headers({ Range: 'bytes=0-0' }), cache: 'no-store' });
    assert.equal(probe.status, 200);
    assert.equal(probe.headers.get('content-length'), String(MODEL_FILES['tokenizer_config.json'].size));
    assert.equal(calls, 0);
    const other = await pinnedFetch('https://huggingface.co/onnx-community/Qwen3.5-0.8B-ONNX/resolve/main/onnx/vision_encoder.onnx', { headers: { range: 'bytes=0-0' } });
    assert.equal(other.status, 404);
    // A real download of a pinned file must match its hash: a 1-byte body never does.
    await assert.rejects(pinnedFetch(url), /does not match its pinned SHA-256/);
    assert.equal(calls, 1);
  } finally {
    globalThis.fetch = real;
  }
});
