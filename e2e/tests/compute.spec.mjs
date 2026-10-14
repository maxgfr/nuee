// Compute: the golden corpus gives the same bytes in V8, SpiderMonkey and JavaScriptCore, and a
// CLI coordinator verifies in full a Mandelbrot job run by a CLI worker and three browsers.
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { test, expect } from '@playwright/test';
import { Harness, blake3, pageUrl, services } from './helpers.mjs';

/** The golden module and corpus (crates/nuee-exec/tests/golden.rs). */
const GOLDEN_MODULE = '7085684d08d0c83832195135a40ce2583e47bb4bdb4828c849ff4746f4648057';
const GOLDEN = [
  { x0: -2.0, y0: -1.5, step: 3.0 / 256, w: 256, h: 256, iter: 500, size: 131072, hash: '4997e7fcca5edc96e1296d3f8819a9a5b7247a3bf3f953e4511f5b21b1080b59' },
  { x0: -0.743643887037151, y0: 0.13182590420533, step: 1e-7, w: 64, h: 64, iter: 2000, size: 8192, hash: '34f54682a94f9f96256bf6bcf7718ed9fd6f23d4e4712fe31db36ca724152fa4' },
  { x0: 0.0, y0: 0.0, step: 1.0, w: 1, h: 1, iter: 100, size: 2, hash: 'd9dba825ecc31a0edc64d04c8535c66a73bc3e74c4c48df84da3272949e38f01' },
];

/** @param {typeof GOLDEN[number]} g */
function goldenInput(g) {
  const b = Buffer.alloc(36);
  b.writeDoubleLE(g.x0, 0);
  b.writeDoubleLE(g.y0, 8);
  b.writeDoubleLE(g.step, 16);
  b.writeUInt32LE(g.w, 24);
  b.writeUInt32LE(g.h, 28);
  b.writeUInt32LE(g.iter, 32);
  return [...b];
}

/** @type {Harness} */
let h;
test.beforeEach(() => {
  h = new Harness();
});
test.afterEach(async () => {
  await h.close();
});

test('golden corpus: identical bytes in the task worker of every engine', async () => {
  const { web, root } = services();
  const served = readFileSync(path.join(root, 'web', 'tasks', 'mandelbrot.wasm'));
  expect(blake3(served)).toBe(GOLDEN_MODULE);
  const inputs = GOLDEN.map(goldenInput);
  const harness = new URL('e2e-golden.html', web).href;

  for (const engine of /** @type {const} */ (['chromium', 'firefox', 'webkit'])) {
    const browser = await h.browser(engine);
    const page = await h.page(browser, engine);
    // A bare same-origin page that drives the app's real task worker module.
    await page.route(harness, (route) => route.fulfill({ contentType: 'text/html', body: '<!doctype html><title>golden</title>' }));
    await page.goto(harness);
    const outputs = await page.evaluate(async (ins) => {
      const module = new Uint8Array(await (await fetch('/tasks/mandelbrot.wasm', { cache: 'no-store' })).arrayBuffer());
      const worker = new Worker('/app/worker/task-worker.js', { type: 'module' });
      const results = [];
      try {
        for (const [i, input] of ins.entries()) {
          const reply = await new Promise((resolve, reject) => {
            worker.onmessage = (e) => resolve(e.data);
            worker.onerror = (e) => reject(new Error(e.message));
            worker.postMessage({
              op: 'run',
              lease: String(i),
              kind: 'wasm',
              moduleHash: 'golden',
              module: i === 0 ? module : undefined,
              input: new Uint8Array(input),
              deadlineMs: 60_000,
              maxMemory: 1024 * 1024 * 1024,
            });
          });
          results.push(reply.ok ? { ok: true, bytes: Array.from(reply.output) } : reply);
        }
      } finally {
        worker.terminate();
      }
      return results;
    }, inputs);
    expect(outputs).toHaveLength(GOLDEN.length);
    for (const [i, g] of GOLDEN.entries()) {
      const out = outputs[i];
      expect(out.ok, `${engine} golden ${i}: ${JSON.stringify(out).slice(0, 200)}`).toBe(true);
      const bytes = Uint8Array.from(out.bytes);
      expect(bytes.length, `${engine} golden ${i} size`).toBe(g.size);
      expect(blake3(bytes), `${engine} golden ${i}`).toBe(g.hash);
    }
  }
  h.checkPages();
});

test('CLI coordinator, verify full: a CLI worker and Chromium, Firefox and WebKit tabs', async () => {
  const out = path.join(h.dir(), 'out');
  const run = h.cli(['--json', 'run', 'demo:mandelbrot', '--verify', 'full', '--local-slots', '1', '--wait-peers', '4', '--out', out]);
  const linkEvent = await run.event('link');
  expect(linkEvent.kind).toBe('job');
  expect(linkEvent.fingerprint).toHaveLength(6);
  const link = linkEvent.link;
  expect(link.startsWith(services().web)).toBe(true);

  /** @type {Array<{ engine: string, page: import('@playwright/test').Page }>} */
  const tabs = [];
  for (const engine of /** @type {const} */ (['chromium', 'firefox', 'webkit'])) {
    const page = await h.page(await h.browser(engine), engine);
    await page.goto(pageUrl(link));
    // The consent card shows the same fingerprint as the CLI.
    await expect(page.locator('.consent .fingerprint .emoji')).toHaveText(linkEvent.fingerprint.join(' '), { timeout: 30_000 });
    await page.getByRole('button', { name: 'Contribute' }).click();
    tabs.push({ engine, page });
  }
  const worker = h.cli(['--json', 'join', '-', '--slots', '2'], { stdin: `${link}\n` });

  expect(await run.exit(3 * 60_000), run.stderr).toBe(0);
  expect(await worker.exit(60_000), worker.stderr).toBe(0);

  const events = run.events();
  const done = events.find((e) => e.event === 'done');
  expect(done).toBeTruthy();
  expect(done.tasks).toBe(256);
  expect(done.banned).toBe(0);
  expect(events.filter((e) => e.event === 'mismatch')).toEqual([]);
  expect(events.filter((e) => e.event === 'banned')).toEqual([]);
  expect(events.filter((e) => e.event === 'task_failed')).toEqual([]);

  const results = JSON.parse(readFileSync(path.join(out, 'results.json'), 'utf8'));
  expect(results.outputs).toHaveLength(256);
  const executors = new Set(results.outputs.map((o) => o.by));
  for (const e of events) if (e.event === 'verified') executors.add(e.by);

  const joined = events.filter((e) => e.event === 'peer' && e.state === 'joined');
  const browsers = joined.filter((e) => String(e.agent).startsWith('nuee-web/')).map((e) => e.id);
  const clis = joined.filter((e) => String(e.agent).startsWith('nuee-cli/')).map((e) => e.id);
  expect(new Set(browsers).size).toBe(3);
  expect(new Set(clis).size).toBe(1);
  // Every accepted or verified result comes from a member of the swarm, browsers included.
  // (A result names one executor; the second execution of a task is only seen by its peer.)
  const members = new Set([...browsers, ...clis, results.coordinator]);
  for (const id of executors) expect(members.has(id), `result by unknown peer ${id}`).toBe(true);
  expect(browsers.some((id) => executors.has(id)), 'no browser result was accepted').toBe(true);
  expect(clis.some((id) => executors.has(id)) || executors.has(results.coordinator)).toBe(true);

  const cliTasks = worker.events().filter((e) => e.event === 'task' && e.ok === true);
  expect(cliTasks.length).toBeGreaterThan(0);

  // Each tab saw the end of the job and counted its own tasks.
  for (const { engine, page } of tabs) {
    await expect(page.getByRole('heading', { name: 'Thank you!' }), engine).toBeVisible({ timeout: 60_000 });
    const summary = await page.locator('.contributing p', { hasText: 'This device did' }).textContent();
    const n = Number(/did ([\d,]+) tasks/.exec(summary ?? '')?.[1]?.replace(/,/g, '') ?? '0');
    expect(n, `${engine}: ${summary}`).toBeGreaterThan(0);
  }
  h.checkPages();
});
