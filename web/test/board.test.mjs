// @ts-check
// The presenter board's running times and its "waiting for a checker" notice.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { fmtDuration, Timings, reduceWaiting, waitingTotal, taskDetail } from '../app/lib/board.js';

test('durations read like the rest of the UI', () => {
  assert.equal(fmtDuration(420), '420 ms');
  assert.equal(fmtDuration(999.6), '1,000 ms');
  assert.equal(fmtDuration(1800), '1.8 s');
  assert.equal(fmtDuration(59_940), '59.9 s');
  assert.equal(fmtDuration(150_000), '2.5 min');
});

test('average time per device counts timed accepted tasks only', () => {
  const t = new Timings();
  assert.equal(t.text('a'), '—');
  t.add('a', 1000);
  t.add('a', 2600);
  t.add('a', null);
  t.add('b', 420);
  t.add('b', Number.NaN);
  assert.equal(t.mean('a'), 1800);
  assert.equal(t.text('a'), '1.8 s');
  assert.equal(t.text('b'), '420 ms');
  assert.equal(t.text('c'), '—');
});

test('waiting for a checker: set by the event, cleared when the job proceeds', () => {
  /** @type {Map<number, number>} */
  const s = new Map();
  assert.equal(reduceWaiting(s, { type: 'waitingForChecker', job: 0, task: 3, kind: 'wasm', waiting: 12 }), true);
  assert.equal(waitingTotal(s), 12);
  assert.equal(reduceWaiting(s, { type: 'waitingForChecker', job: 1, task: 0, kind: 'embed' }), true);
  assert.equal(waitingTotal(s), 13);
  assert.equal(reduceWaiting(s, { type: 'progress', job: 0 }), false);
  assert.equal(reduceWaiting(s, { type: 'verified', job: 0 }), true);
  assert.equal(waitingTotal(s), 1);
  assert.equal(reduceWaiting(s, { type: 'jobDone', job: 1 }), true);
  assert.equal(waitingTotal(s), 0);
  assert.equal(reduceWaiting(s, { type: 'online' }), false);
});

test('the detail line names the task, its device and its measured time, or a dash', () => {
  assert.equal(taskDetail(63, '🐳 🌻 🎲', 68), 'Task 63 · 🐳 🌻 🎲 · 68\u00a0ms');
  assert.equal(taskDetail(7, '🐳 🌻 🎲', null), 'Task 7 · 🐳 🌻 🎲 · —');
  assert.equal(taskDetail(5, null, null), 'Task 5 · —');
  assert.equal(taskDetail(2, 'x', 1800), 'Task 2 · x · 1.8\u00a0s');
});
