// @ts-check
// Identifiers shown as words, how a contribution's end is written, picked text files, the
// fractal tile shading and the murmuration's flight time.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { jobKey, rejectKey, endedTone, readTextFile } from '../app/lib/labels.js';
import { tilePixels, rgbOf } from '../app/lib/board.js';
import { flightClock, flightPhase, FLIGHT_MS, SETTLE_MS } from '../app/lib/murmuration.js';
import { STRINGS } from '../app/i18n.js';

test('built-in job names are shown by their translated label, other names as they are', () => {
  assert.equal(jobKey('mandelbrot'), 'run.job.mandelbrot');
  assert.equal(jobKey('mandelbrot-gpu'), 'run.job.mandelbrotGpu');
  assert.equal(jobKey('skycmd-eval'), 'run.job.skycmd');
  assert.equal(jobKey('skycmd'), 'run.job.skycmd');
  assert.equal(jobKey('skycmd-dagger'), 'run.job.skycmdDagger');
  assert.equal(jobKey('embed'), 'run.job.embed');
  assert.equal(jobKey('my-module'), null);
  assert.equal(jobKey('toString'), null);
  for (const n of ['mandelbrot', 'mandelbrot-gpu', 'skycmd-eval', 'skycmd-dagger', 'embed']) {
    const k = /** @type {string} */ (jobKey(n));
    assert.ok(STRINGS[k], k);
  }
});

test('a refusal reason is a sentence, never the raw code', () => {
  assert.equal(rejectKey('too many jobs'), 'join.jobRejected.tooMany');
  assert.equal(rejectKey('bad signature'), 'join.jobRejected.invalid');
  for (const k of ['join.jobRejected.tooMany', 'join.jobRejected.invalid']) assert.ok(STRINGS[k], k);
});

test('only a ban or a refusal ends a contribution with an alert', () => {
  assert.equal(endedTone('join.ended.banned'), 'alert');
  assert.equal(endedTone('join.ended.rejected'), 'alert');
  assert.equal(endedTone('join.ended.full'), 'notice');
  assert.equal(endedTone('join.ended.shutdown'), 'notice');
  assert.equal(endedTone('join.ended.stopped'), 'plain');
  assert.equal(endedTone('join.ended.done'), 'plain');
});

test('a picked text file says what is wrong with it', () => {
  const enc = new TextEncoder();
  assert.deepEqual(readTextFile(enc.encode('un\ndeux\n')), { text: 'un\ndeux\n' });
  assert.deepEqual(readTextFile(new Uint8Array([0xff, 0xfe, 0x00, 0x41])), { error: 'run.embed.notText' });
  assert.deepEqual(readTextFile(enc.encode(' \n\n\t')), { error: 'run.embed.empty' });
  assert.deepEqual(readTextFile(new Uint8Array(0)), { error: 'run.embed.empty' });
  for (const k of ['run.embed.notText', 'run.embed.empty']) assert.ok(STRINGS[k], k);
});

test('a fractal tile shades from the paper to the device ink', () => {
  const paper = /** @type {[number, number, number]} */ ([250, 250, 250]);
  const ink = /** @type {[number, number, number]} */ ([0, 0, 100]);
  // A 1×1 tile of u16 counts: inside the set (iter) is full ink.
  const inside = new Uint8Array(new Uint16Array([500]).buffer);
  assert.deepEqual([...(tilePixels(inside, 1, 500, paper, ink) ?? [])], [0, 0, 100, 255]);
  // Zero iterations is a light wash (8%) of the ink; u32 counts (GPU) read the same way.
  const outside = new Uint8Array(new Uint32Array([0]).buffer);
  assert.deepEqual([...(tilePixels(outside, 1, 500, paper, ink) ?? [])], [230, 230, 238, 255]);
  assert.equal(tilePixels(new Uint8Array(1), 1, 500, paper, ink), null);
  assert.deepEqual(rgbOf(' #1d3b9a ', [0, 0, 0]), [29, 59, 154]);
  assert.deepEqual(rgbOf('rgb(1 2 3)', [9, 9, 9]), [9, 9, 9]);
});

test('the murmuration flies about five seconds, then settles and stops', () => {
  assert.ok(FLIGHT_MS <= 5000);
  assert.deepEqual(flightPhase(0), { pull: 0, done: false });
  assert.deepEqual(flightPhase(FLIGHT_MS - SETTLE_MS), { pull: 0, done: false });
  const mid = flightPhase(FLIGHT_MS - SETTLE_MS / 2);
  assert.ok(mid.pull > 0 && mid.pull < 1 && !mid.done);
  assert.ok(flightPhase(FLIGHT_MS - 10).pull > mid.pull);
  assert.deepEqual(flightPhase(FLIGHT_MS), { pull: 1, done: true });
  assert.deepEqual(flightPhase(60_000), { pull: 1, done: true });
});

test('the flight runs on the wall clock, except a gap of more than a second', () => {
  // A slow frame (a busy machine) counts in full, so the flight never outlasts five seconds.
  assert.equal(flightClock(1000, 5000, 5200), 1200);
  // The first frame, and a frame more than a second after the last one (a background tab),
  // count as one frame.
  assert.equal(flightClock(0, 0, 123_456), 16);
  assert.equal(flightClock(1000, 5000, 7000), 1016);
});
