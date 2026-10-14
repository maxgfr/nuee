// @ts-check
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { Bridge, BridgeError, codeOf, transferables } from '../app/lib/bridge.js';

class FakeWorker {
  constructor() {
    /** @type {Array<{ msg: any, transfer: Transferable[] }>} */
    this.sent = [];
    /** @type {Record<string, Array<(e: any) => void>>} */
    this.handlers = { message: [], error: [] };
  }
  /** @param {any} msg @param {Transferable[]} [transfer] */
  postMessage(msg, transfer = []) {
    this.sent.push({ msg, transfer });
  }
  /** @param {'message' | 'error'} type @param {(e: any) => void} fn */
  addEventListener(type, fn) {
    this.handlers[type]?.push(fn);
  }
  /** @param {unknown} data */
  emit(data) {
    for (const fn of this.handlers.message ?? []) fn({ data });
  }
}

const noExec = async () => /** @type {const} */ ({ ok: false, kind: 'unsupported', detail: '' });

test('replies are matched by id, in any order', async () => {
  const w = new FakeWorker();
  const b = new Bridge(w, noExec);
  const p1 = b.call('version', null);
  const p2 = b.call('fingerprint', { endpointId: 'ab' });
  assert.equal(w.sent.length, 2);
  const [r1, r2] = w.sent.map((s) => s.msg);
  assert.equal(r1.op, 'version');
  assert.notEqual(r1.id, r2.id);
  w.emit({ id: r2.id, ok: true, value: ['a', 'b'] });
  w.emit({ id: r1.id, ok: true, value: '1.2.3' });
  w.emit({ id: 999, ok: true, value: 'stray' });
  assert.equal(await p1, '1.2.3');
  assert.deepEqual(await p2, ['a', 'b']);
});

test('errors keep their code', async () => {
  const w = new FakeWorker();
  const b = new Bridge(w, noExec);
  const p = b.call('parseLink', { link: 'J/AAAA' });
  w.emit({ id: w.sent[0]?.msg.id, ok: false, error: { code: 'link_truncated', message: 'too short' } });
  await assert.rejects(p, (e) => e instanceof BridgeError && e.code === 'link_truncated' && e.message === 'too short');
  const q = b.call('parseLink', { link: 'x' });
  w.emit({ id: w.sent[1]?.msg.id, ok: false, error: { code: 'made_up', message: 'm' } });
  await assert.rejects(q, (e) => codeOf(e) === 'internal');
  assert.equal(codeOf({ code: 'relay_unreachable', message: '' }), 'relay_unreachable');
  assert.equal(codeOf(new Error('boom')), 'internal');
});

test('a worker that fails to load rejects pending and later calls', async () => {
  const w = new FakeWorker();
  const b = new Bridge(w, noExec);
  const p = b.call('version', null);
  for (const fn of w.handlers.error ?? []) fn({ message: 'syntax error' });
  await assert.rejects(p, (e) => codeOf(e) === 'internal');
  await assert.rejects(b.call('version', null), (e) => codeOf(e) === 'internal');
});

test('events reach subscribers with their session; progress by request id', async () => {
  const w = new FakeWorker();
  const b = new Bridge(w, noExec);
  /** @type {any[]} */
  const seen = [];
  const off = b.onEvent((e, s) => seen.push([e.type, s]));
  w.emit({ session: 3, event: { type: 'pending' } });
  w.emit({ event: { type: 'error', code: 'internal', message: 'x' } });
  off();
  w.emit({ session: 3, event: { type: 'welcome' } });
  assert.deepEqual(seen, [['pending', 3], ['error', undefined]]);

  /** @type {Array<[number, number]>} */
  const prog = [];
  const p = b.call('fetchFile', { link: 'F/AAAA' }, { onProgress: (g, t) => prog.push([g, t]) });
  const sent = w.sent.at(-1)?.msg;
  assert.equal(sent.args.progress, sent.id);
  w.emit({ progress: sent.id, got: 5, total: 10 });
  w.emit({ id: sent.id, ok: true, value: { files: [], hash: 'h', verified: true } });
  await p;
  assert.deepEqual(prog, [[5, 10]]);
});

test('executor round-trip, with rejection mapped to io', async () => {
  const w = new FakeWorker();
  /** @type {any[]} */
  const tasks = [];
  const b = new Bridge(w, async (t) => {
    tasks.push(t);
    if (t.lease === '2') throw new Error('pool gone');
    return { ok: true, output: Uint8Array.of(9, 9) };
  });
  const task = { lease: '1', job: 0, task: 4, kind: 'wasm', moduleHash: 'aa', input: Uint8Array.of(1), deadlineMs: 100, maxMemory: 1 };
  w.emit({ execId: 7, task });
  w.emit({ execId: 8, task: { ...task, lease: '2' } });
  await new Promise((r) => setTimeout(r, 10));
  assert.equal(tasks.length, 2);
  const replies = w.sent.map((s) => s.msg);
  const ok = replies.find((m) => m.execId === 7);
  assert.deepEqual(ok.outcome, { ok: true, output: Uint8Array.of(9, 9) });
  assert.equal(w.sent.find((s) => s.msg.execId === 7)?.transfer.length, 1, 'output buffer transferred');
  const bad = replies.find((m) => m.execId === 8);
  assert.equal(bad.outcome.ok, false);
  assert.equal(bad.outcome.kind, 'io');
  assert.equal(bad.outcome.detail, 'pool gone');
});

test('moving arguments transfers owned buffers only', () => {
  const owned = Uint8Array.of(1, 2, 3);
  const view = new Uint8Array(new ArrayBuffer(8), 2, 3);
  const list = transferables({ module: owned, inputs: [view, owned], name: 'x' });
  assert.deepEqual(list, [owned.buffer]);
  const w = new FakeWorker();
  const b = new Bridge(w, noExec);
  b.call('validateModule', { bytes: Uint8Array.of(1) });
  b.call('validateModule', { bytes: Uint8Array.of(1) }, { move: true });
  assert.equal(w.sent[0]?.transfer.length, 0);
  assert.equal(w.sent[1]?.transfer.length, 1);
});
