// @ts-check
// Demos that keep going until Stop: the zoom of the fractal, the seeds of each skycmd pass and
// the shading of deep images.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { MANDELBROT, ZOOM, zoomFrame, mandelbrotInputs, skycmdInputs, SKYCMD } from '../app/views/run.js';
import { daggerInputs } from '../app/lib/jobs.js';
import { tilePixels, escapeSample } from '../app/lib/board.js';

test('the first image of the zoom is the single fractal, byte for byte', () => {
  assert.deepEqual(zoomFrame(0, false), MANDELBROT);
  assert.deepEqual(zoomFrame(0, true), MANDELBROT);
  assert.deepEqual(mandelbrotInputs(zoomFrame(0, false)), mandelbrotInputs(MANDELBROT));
});

test('each image zooms toward the same point, which stays in place, then the film starts over', () => {
  const at = (/** @type {typeof MANDELBROT} */ f) => [(ZOOM.x - f.x0) / f.span, (ZOOM.y - f.y0) / f.span];
  const [u0, v0] = at(MANDELBROT);
  let n = 1;
  for (; zoomFrame(n, false).span < zoomFrame(n - 1, false).span; n += 1) {
    const f = zoomFrame(n, false);
    assert.ok(Math.abs(f.span / zoomFrame(n - 1, false).span - ZOOM.factor) < 1e-9);
    const [u, v] = at(f);
    assert.ok(Math.abs(u - u0) < 1e-3 && Math.abs(v - v0) < 1e-3, `image ${n} moved the point`);
    assert.ok(f.iter >= zoomFrame(n - 1, false).iter && f.iter <= 65535);
    assert.ok(f.span >= ZOOM.cpu);
  }
  // As deep as f64 draws faithfully, then back to the first image.
  assert.ok(n > 100);
  assert.deepEqual(zoomFrame(n, false), MANDELBROT);
  // GPU tiles (f32) stop much higher and keep their iteration count.
  for (let k = 1; k < 5; k += 1) assert.equal(zoomFrame(k, true).iter, MANDELBROT.iter);
  let g = 1;
  while (zoomFrame(g, true).span < zoomFrame(g - 1, true).span) g += 1;
  assert.ok(zoomFrame(g - 1, true).span >= ZOOM.gpu);
  assert.ok(g < 20);
});

test('each skycmd pass plays new seeds; pass 0 is the CLI preset', () => {
  const seeds = (/** @type {Uint8Array[]} */ inputs) => inputs.map((b) => JSON.parse(new TextDecoder().decode(b)).seed);
  assert.deepEqual(seeds(skycmdInputs(SKYCMD)).slice(0, SKYCMD.seeds), [1, 2, 3, 4, 5, 6, 7, 8]);
  assert.deepEqual(seeds(skycmdInputs(SKYCMD, 2)).slice(0, SKYCMD.seeds), [17, 18, 19, 20, 21, 22, 23, 24]);
  assert.equal(skycmdInputs(SKYCMD, 2).length, skycmdInputs(SKYCMD).length);
  assert.deepEqual(daggerInputs(undefined, 0), daggerInputs());
  assert.deepEqual(seeds(daggerInputs(undefined, 1)).slice(0, 4), [5, 6, 7, 8]);
});

test('a deep image is shaded by rank among the counts of the previous one', () => {
  const paper = /** @type {[number, number, number]} */ ([250, 250, 250]);
  const ink = /** @type {[number, number, number]} */ ([0, 0, 100]);
  const counts = new Uint8Array(new Uint16Array([1000, 3000, 2000, 60000]).buffer);
  /** @type {number[]} */
  const sample = [];
  escapeSample(counts, 2, 60000, 1, sample);
  assert.deepEqual(sample, [1000, 3000, 2000]);
  const ranks = Float64Array.from([1000, 1001, 1002, 3000]).sort();
  const px = tilePixels(counts, 2, 60000, paper, ink, ranks) ?? new Uint8ClampedArray(0);
  // Counts crowded near 1000 still spread: the lowest is the lightest wash, 2000 sits above
  // three of the four ranks.
  assert.deepEqual([...px.slice(0, 4)], [230, 230, 238, 255]);
  assert.deepEqual([...px.slice(8, 12)], [80, 80, 148, 255]);
  // A point that never escapes is full ink.
  assert.deepEqual([...px.slice(12, 16)], [0, 0, 100, 255]);
});
