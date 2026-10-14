// @ts-check
// The embedding executor's task format (NEM1, shared with crates/nuee-gpu/src/embed.rs) and
// the pinned model check. The model itself runs in the e2e browsers.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';

import { decodeInput, encodeInput, fetchModel, modelUrl, TaskFail, WEB_MODEL, MAX_TEXTS, MAX_TEXT_BYTES } from '../app/worker/embed.js';

/** @param {() => unknown} f */
function invalid(f) {
  assert.throws(f, (e) => e instanceof TaskFail && e.kind === 'invalid');
}

test('NEM1 inputs are encoded byte for byte like the Rust side', () => {
  const bytes = encodeInput(['a', 'é']);
  assert.deepEqual([...bytes], [0x4e, 0x45, 0x4d, 0x31, 2, 0, 0, 0, 1, 0, 0, 0, 0x61, 2, 0, 0, 0, 0xc3, 0xa9]);
  assert.deepEqual(decodeInput(bytes), ['a', 'é']);
  const big = ['x'.repeat(MAX_TEXT_BYTES), ''];
  assert.deepEqual(decodeInput(encodeInput(big)), big);
});

test('malformed NEM1 inputs are rejected', () => {
  const good = encodeInput(['hello', 'world']);
  invalid(() => decodeInput(new Uint8Array()));
  invalid(() => decodeInput(new Uint8Array([0x4e, 0x45, 0x4d, 0x32, 1, 0, 0, 0, 0, 0, 0, 0])));
  invalid(() => decodeInput(new Uint8Array([0x4e, 0x45, 0x4d, 0x31, 0, 0, 0, 0])));
  for (let cut = 1; cut < good.length; cut += 1) invalid(() => decodeInput(good.subarray(0, cut)));
  const trailing = new Uint8Array(good.length + 1);
  trailing.set(good);
  invalid(() => decodeInput(trailing));
  const notUtf8 = good.slice();
  notUtf8[12] = 0xff;
  invalid(() => decodeInput(notUtf8));
  const tooMany = new Uint8Array(8 + 4 * (MAX_TEXTS + 1));
  tooMany.set([0x4e, 0x45, 0x4d, 0x31]);
  new DataView(tooMany.buffer).setUint32(4, MAX_TEXTS + 1, true);
  invalid(() => decodeInput(tooMany));
  const huge = new Uint8Array(12);
  huge.set([0x4e, 0x45, 0x4d, 0x31, 1, 0, 0, 0]);
  new DataView(huge.buffer).setUint32(8, MAX_TEXT_BYTES + 1, true);
  invalid(() => decodeInput(huge));
  invalid(() => encodeInput([]));
  invalid(() => encodeInput(['x'.repeat(MAX_TEXT_BYTES + 1)]));
});

test('model files come from the pinned revision and are checked before use', async () => {
  for (const f of WEB_MODEL.files) {
    assert.match(f.sha256, /^[0-9a-f]{64}$/);
    assert.equal(modelUrl(f), `https://huggingface.co/Xenova/all-MiniLM-L6-v2/resolve/${WEB_MODEL.rev}/${f.path}`);
  }
  // A tampered file is refused, a failed download is an io failure.
  const fake = (/** @type {string} */ url) => Promise.resolve(new Response(new Uint8Array(WEB_MODEL.files.find((f) => modelUrl(f) === url)?.size ?? 0)));
  await assert.rejects(fetchModel(fake), (e) => e instanceof TaskFail && e.kind === 'invalid');
  await assert.rejects(
    fetchModel(() => Promise.resolve(new Response(null, { status: 404 }))),
    (e) => e instanceof TaskFail && e.kind === 'io',
  );
});

test('the vendored library is pinned by integrity and per-file hashes', async () => {
  const vendor = JSON.parse(await readFile(new URL('../vendor/transformers/VENDOR.json', import.meta.url), 'utf8'));
  assert.equal(vendor.version, '4.3.1');
  const names = vendor.packages.flatMap((/** @type {{ integrity: string, files: { to: string, sha256: string }[] }} */ p) => {
    assert.match(p.integrity, /^sha512-[A-Za-z0-9+/]+=*$/);
    return p.files.map((f) => {
      assert.match(f.sha256, /^[0-9a-f]{64}$/);
      return f.to;
    });
  });
  for (const n of ['transformers.min.js', 'ort-wasm-simd-threaded.asyncify.mjs', 'ort-wasm-simd-threaded.asyncify.wasm']) assert.ok(names.includes(n), n);
  // When the payload is present (after scripts/build-web.sh), it matches the record.
  for (const p of vendor.packages) {
    for (const f of p.files) {
      let bytes;
      try {
        bytes = await readFile(new URL(`../vendor/transformers/${f.to}`, import.meta.url));
      } catch (e) {
        if (/** @type {NodeJS.ErrnoException} */ (e).code === 'ENOENT') continue;
        throw e;
      }
      assert.equal(createHash('sha256').update(bytes).digest('hex'), f.sha256, f.to);
    }
  }
});
