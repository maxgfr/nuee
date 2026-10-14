// @ts-check
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { detect, wasmProblem, isMobile, SIMD_PROBE } from '../app/lib/caps.js';

test('the SIMD probe is a valid SIMD module for a real engine', () => {
  assert.equal(WebAssembly.validate(SIMD_PROBE), true);
});

test('a modern desktop browser', () => {
  const self = {};
  const c = detect({
    navigator: { hardwareConcurrency: 8, deviceMemory: 8, gpu: {}, wakeLock: { request() {} }, getBattery() {}, userAgent: 'Mozilla/5.0 (X11; Linux x86_64)' },
    WebAssembly,
    top: self,
    self,
  });
  assert.deepEqual(c, { wasm: true, simd: true, webgpu: true, wakeLock: true, battery: true, deviceMemoryGb: 8, cores: 8, mobile: false, framed: false });
  assert.equal(wasmProblem(c), null);
});

test('no WebAssembly (Lockdown Mode) and no SIMD (old Safari)', () => {
  const none = detect({ navigator: {}, WebAssembly: undefined });
  assert.equal(none.wasm, false);
  assert.equal(wasmProblem(none), 'boot.noWasm');
  const old = detect({ navigator: {}, WebAssembly: { validate: () => false } });
  assert.equal(old.wasm, true);
  assert.equal(old.simd, false);
  assert.equal(wasmProblem(old), 'boot.noSimd');
  const throwing = detect({ navigator: {}, WebAssembly: { validate: () => { throw new Error('x'); } } });
  assert.equal(throwing.simd, false);
});

test('defaults, framing and mobile detection', () => {
  const top = {};
  const c = detect({ navigator: {}, WebAssembly, top, self: {} });
  assert.equal(c.cores, 2);
  assert.equal(c.deviceMemoryGb, undefined);
  assert.equal(c.webgpu, false);
  assert.equal(c.wakeLock, false);
  assert.equal(c.battery, false);
  assert.equal(c.framed, true);
  assert.equal(isMobile({ userAgent: 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)' }), true);
  assert.equal(isMobile({ userAgent: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)', maxTouchPoints: 5 }), true);
  assert.equal(isMobile({ userAgent: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)', maxTouchPoints: 0 }), false);
  assert.equal(isMobile({ userAgentData: { mobile: false }, userAgent: 'Android' }), false);
  assert.equal(isMobile({ userAgent: 'Mozilla/5.0 (Linux; Android 14) Mobile' }), true);
});
