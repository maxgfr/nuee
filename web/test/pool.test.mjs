// @ts-check
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { poolSize, memoryBudget, defaultCores, offeredMemMb, Pool, makeExecutor, GiB, MiB } from '../app/lib/pool.js';

test('pool size follows cores, the slider and declared memory', () => {
  const desk = { cores: 8, mobile: false };
  assert.equal(poolSize({ ...desk, wanted: 4, maxMemory: 256 * MiB }), 4);
  assert.equal(poolSize({ ...desk, wanted: 16, maxMemory: 256 * MiB }), 8, 'never above the cores');
  assert.equal(poolSize({ ...desk, wanted: 0, maxMemory: 256 * MiB }), 0, 'zero pauses');
  assert.equal(poolSize({ ...desk, wanted: 8, maxMemory: GiB }), 4, '4 GiB desktop budget');
  assert.equal(poolSize({ ...desk, wanted: 8, maxMemory: 2 * GiB }), 2);
  assert.equal(poolSize({ ...desk, wanted: 8, maxMemory: 8 * GiB }), 1, 'at least one');
  const phone = { cores: 8, mobile: true };
  assert.equal(poolSize({ ...phone, wanted: 8, maxMemory: 256 * MiB }), 4, '1 GiB mobile budget');
  assert.equal(poolSize({ ...phone, wanted: 8, maxMemory: GiB }), 1);
  assert.equal(poolSize({ cores: 8, mobile: false, deviceMemoryGb: 2, wanted: 8, maxMemory: 256 * MiB }), 4, 'half of deviceMemory');
  assert.equal(poolSize({ cores: 0, mobile: false, wanted: 3, maxMemory: 0 }), 1);
  assert.equal(poolSize({ cores: Number.NaN, mobile: false, wanted: Number.NaN, maxMemory: 1 }), 0);
});

test('budgets and defaults', () => {
  assert.equal(memoryBudget({ cores: 4, mobile: false }), 4 * GiB);
  assert.equal(memoryBudget({ cores: 4, mobile: true }), GiB);
  assert.equal(memoryBudget({ cores: 4, mobile: false, deviceMemoryGb: 0.25 }), 256 * MiB);
  assert.equal(defaultCores(8), 4);
  assert.equal(defaultCores(5), 3);
  assert.equal(defaultCores(1), 1);
  assert.equal(defaultCores(0), 1);
  assert.equal(offeredMemMb({ cores: 8, mobile: false }, 2), 1024);
  assert.equal(offeredMemMb({ cores: 8, mobile: true }, 4), 256);
});

class FakeWorker {
  /** @param {(w: FakeWorker, msg: any) => void} behave */
  constructor(behave) {
    this.behave = behave;
    /** @type {any[]} */
    this.got = [];
    this.terminated = false;
    /** @type {((e: { data: unknown }) => void) | null} */
    this.onmessage = null;
    /** @type {((e: { message?: string }) => void) | null} */
    this.onerror = null;
  }
  /** @param {any} msg */
  postMessage(msg) {
    this.got.push(msg);
    queueMicrotask(() => this.behave(this, msg));
  }
  terminate() {
    this.terminated = true;
  }
  /** @param {unknown} data */
  reply(data) {
    if (!this.terminated && this.onmessage) this.onmessage({ data });
  }
}

/** @param {Partial<import('../app/api').TaskRequest>} o @returns {import('../app/api').TaskRequest} */
function req(o) {
  return { lease: '1', job: 0, task: 0, kind: 'wasm', moduleHash: 'aa', input: new Uint8Array([1, 2]), deadlineMs: 1000, maxMemory: 64 * MiB, ...o };
}

test('runs leases, sends a module once per worker, caches by hash', async () => {
  /** @type {FakeWorker[]} */
  const made = [];
  const pool = new Pool({
    create: () => {
      const w = new FakeWorker((self, msg) => self.reply({ ok: true, output: Uint8Array.of(msg.input[0] * 2) }));
      made.push(w);
      return w;
    },
    device: { cores: 4, mobile: false },
    slots: 1,
  });
  const a = await pool.run(req({ lease: '1', module: Uint8Array.of(0, 97, 115, 109), input: Uint8Array.of(3) }));
  assert.deepEqual(a, { ok: true, output: Uint8Array.of(6) });
  const b = await pool.run(req({ lease: '2', input: Uint8Array.of(5) }));
  assert.deepEqual(b, { ok: true, output: Uint8Array.of(10) });
  assert.equal(made.length, 1);
  const w = made[0];
  assert.ok(w);
  assert.ok(w.got[0].module instanceof Uint8Array, 'first lease carries the module');
  assert.equal(w.got[1].module, undefined, 'second lease only the hash');
  assert.equal(w.got[1].op, 'run');
  const missing = await pool.run(req({ moduleHash: 'bb' }));
  assert.equal(missing.ok, false);
  if (!missing.ok) assert.equal(missing.kind, 'invalid');
  pool.stop();
});

test('deadline terminates the worker and a fresh one takes over', async () => {
  /** @type {FakeWorker[]} */
  const made = [];
  let hang = true;
  const pool = new Pool({
    create: () => {
      const w = new FakeWorker((self) => {
        if (!hang) self.reply({ ok: true, output: new Uint8Array(0) });
      });
      made.push(w);
      return w;
    },
    device: { cores: 2, mobile: false },
    slots: 1,
  });
  const out = await pool.run(req({ module: Uint8Array.of(1), deadlineMs: 20 }));
  assert.deepEqual(out.ok ? null : out.kind, 'timeout');
  assert.equal(made[0]?.terminated, true);
  hang = false;
  const next = await pool.run(req({ lease: '2' }));
  assert.equal(next.ok, true);
  assert.equal(made.length, 2);
  assert.ok(made[1]?.got[0].module instanceof Uint8Array, 'the fresh worker gets the module again');
  pool.stop();
});

test('concurrency is limited by slots, malformed replies become io, stop cancels', async () => {
  let live = 0;
  let peak = 0;
  /** @type {FakeWorker[]} */
  const made = [];
  const pool = new Pool({
    create: () => {
      const w = new FakeWorker((self, msg) => {
        live += 1;
        peak = Math.max(peak, live);
        setTimeout(() => {
          live -= 1;
          self.reply(msg.lease === 'bad' ? { nope: 1 } : { ok: false, kind: 'trap', detail: 'unreachable' });
        }, 5);
      });
      made.push(w);
      return w;
    },
    device: { cores: 8, mobile: false },
    slots: 2,
  });
  const outs = await Promise.all(['1', '2', '3', '4', 'bad'].map((lease) => pool.run(req({ lease, module: Uint8Array.of(1) }))));
  assert.equal(peak, 2);
  assert.equal(made.length, 2);
  assert.deepEqual(outs.map((o) => (o.ok ? 'ok' : o.kind)), ['trap', 'trap', 'trap', 'trap', 'io']);
  pool.setSlots(0);
  const held = pool.run(req({ lease: '9' }));
  pool.stop();
  const h = await held;
  assert.equal(h.ok ? 'ok' : h.kind, 'cancelled');
});

test('executor routes kinds', async () => {
  const pool = new Pool({ create: () => new FakeWorker((self) => self.reply({ ok: true, output: new Uint8Array(0) })), device: { cores: 1, mobile: false }, slots: 1 });
  const exec = makeExecutor(pool, new Map([['embed', async () => ({ ok: true, output: Uint8Array.of(7) })]]));
  assert.deepEqual(await exec(req({ kind: 'embed' })), { ok: true, output: Uint8Array.of(7) });
  const wgsl = await exec(req({ kind: 'wgsl' }));
  assert.equal(wgsl.ok ? 'ok' : wgsl.kind, 'unsupported');
  assert.equal((await exec(req({ module: Uint8Array.of(1) }))).ok, true);
  pool.stop();
});
