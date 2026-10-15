// A cheating tab (its task worker flips byte 0 of every output after its first eight) against a coordinator that takes
// no tasks itself: the page with "This page computes too" unchecked, then the CLI with
// `--local-slots 0`. Verification stays at the default `sample`; the coordinator recomputes the
// samples and the canary answers itself, bans the cheater, redoes its work, and the result is
// the honest one.
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { test, expect } from '@playwright/test';
import { Harness, pageUrl, services } from './helpers.mjs';

/** @type {Harness} */
let h;
test.beforeEach(() => {
  h = new Harness();
});
test.afterEach(async () => {
  await h.close();
});

const NEEDLE = 'return { ok: true, output: new Uint8Array(memory.buffer, outPtr, outLen).slice() };';
// Honest for its first eight outputs (accepted, mostly unchecked), then wrong every time: the ban
// must redo those accepted results too.
const HONEST_FIRST = 8;
const FLIP = `const o = new Uint8Array(memory.buffer, outPtr, outLen).slice(); globalThis.cheatCount = (globalThis.cheatCount ?? 0) + 1; if (globalThis.cheatCount > ${HONEST_FIRST} && o.length) o[0] ^= 1; return { ok: true, output: o };`;

/**
 * A Chromium tab, in its own context, whose task worker corrupts every output after the
 * first few. It is not
 * watched for console errors: being banned is what it is there for.
 * @param {string} link
 */
async function cheater(link) {
  const original = readFileSync(path.join(services().root, 'web/app/worker/task-worker.js'), 'utf8');
  expect(original.includes(NEEDLE), 'the task worker changed; update the cheat patch').toBe(true);
  const browser = await h.browser('chromium');
  const context = await browser.newContext({ locale: 'en-US' });
  await context.route('**/app/worker/task-worker.js', (route) =>
    route.fulfill({ status: 200, contentType: 'text/javascript', body: original.replace(NEEDLE, FLIP) }),
  );
  const page = await context.newPage();
  await page.goto(pageUrl(link));
  await page.getByRole('button', { name: 'Contribute' }).click();
  return page;
}

/**
 * Records the last accepted output of every task (the `accepted` events the Nuée worker posts to
 * the page), before the app loads. The canvas cannot be compared: each tile is drawn in the ink
 * of the device that computed it.
 * @param {import('@playwright/test').Page} page
 */
async function recordAccepted(page) {
  await page.addInitScript(() => {
    /** @type {Record<number, Uint8Array>} */
    const acc = {};
    Object.assign(window, { __accepted: acc });
    const Base = window.Worker;
    window.Worker = class extends Base {
      /** @param {string | URL} url @param {WorkerOptions} [opts] */
      constructor(url, opts) {
        super(url, opts);
        this.addEventListener('message', (e) => {
          const ev = e.data?.event;
          if (ev?.type === 'accepted') acc[ev.task] = ev.output.slice();
        });
      }
    };
  });
}

/** SHA-256 of every task's final output, by task. @param {import('@playwright/test').Page} page */
async function outputHashes(page) {
  return page.evaluate(async () => {
    const acc = /** @type {{ __accepted: Record<string, Uint8Array> }} */ (/** @type {unknown} */ (window)).__accepted;
    /** @type {Record<string, string>} */
    const out = {};
    for (const [task, bytes] of Object.entries(acc)) {
      const digest = await crypto.subtle.digest('SHA-256', bytes);
      out[task] = Array.from(new Uint8Array(digest), (b) => b.toString(16).padStart(2, '0')).join('');
    }
    return out;
  });
}

/**
 * Starts the CPU fractal on a page.
 * @param {import('@playwright/test').Page} page
 * @param {boolean} local whether the page computes too
 */
async function startFractal(page, local) {
  await page.goto(pageUrl('#/run'));
  await page.locator('input[name=job][value=mandelbrot]').check();
  await page.locator('#keep').uncheck();
  await expect(page.locator('#verify')).toHaveValue('sample');
  if (local) await page.locator('#local').check();
  else await page.locator('#local').uncheck();
  await page.getByRole('button', { name: 'Start' }).click();
}

const sha256 = (/** @type {Buffer} */ b) => createHash('sha256').update(b).digest('hex');

test('a page that does not compute still bans a cheating tab and redoes its work', async () => {
  const chromium = await h.browser('chromium');

  // Reference: the page computes the whole fractal alone.
  const alone = await h.page(chromium, 'reference');
  await recordAccepted(alone);
  await startFractal(alone, true);
  await expect(alone.getByText('Done. Every task has a result.')).toBeVisible({ timeout: 120_000 });
  const honest = await outputHashes(alone);
  expect(Object.keys(honest)).toHaveLength(64);

  const host = await h.page(chromium, 'host');
  await recordAccepted(host);
  await startFractal(host, false);
  const field = host.locator('#share-link');
  await expect(field).toHaveValue(/#\/J\//, { timeout: 30_000 });
  const link = await field.inputValue();

  // The cheater is alone at first, so it gets most tasks and a wrong one is sampled almost
  // surely; by then its first, honest outputs have been accepted.
  await cheater(link);
  const stat = (/** @type {string} */ name) => host.locator('dl.stats dt', { hasText: new RegExp(`^${name}$`) }).locator('xpath=following-sibling::dd[1]');
  await expect(stat('Banned')).toHaveText('1', { timeout: 120_000 });
  const redone = Number(((await stat('Redone').textContent()) ?? '0').replace(/,/g, ''));
  expect(redone, 'the cheater had accepted results to redo').toBeGreaterThan(0);
  // The corrections note turned from pencil to red with the first correction.
  await expect(host.locator('.corr-note')).not.toHaveClass(/\bclean\b/);
  await expect(host.locator('.corrections')).not.toHaveClass(/\bclean\b/);

  // Honest contributors finish the job.
  const tab = await h.page(chromium, 'honest tab');
  await tab.goto(pageUrl(link));
  await tab.getByRole('button', { name: 'Contribute' }).click();
  h.cli(['--json', 'join', '-', '--slots', '2'], { stdin: `${link}\n` });

  await expect(host.getByText('Done. Every task has a result.')).toBeVisible({ timeout: 180_000 });
  await expect(stat('Banned')).toHaveText('1');
  expect(await outputHashes(host), 'every tile is the honest one').toEqual(honest);
  h.checkPages();
});

test('a CLI coordinator with --local-slots 0 still bans a cheating tab and redoes its work', async () => {
  // Reference: the CLI computes the whole fractal alone.
  const refOut = path.join(h.dir(), 'ref');
  const ref = h.cli(['--json', 'run', 'demo:mandelbrot', '--out', refOut]);
  expect(await ref.exit(3 * 60_000), ref.stderr).toBe(0);
  const honest = sha256(readFileSync(path.join(refOut, 'mandelbrot.ppm')));

  const out = path.join(h.dir(), 'out');
  const run = h.cli(['--json', 'run', 'demo:mandelbrot', '--local-slots', '0', '--wait-peers', '1', '--out', out]);
  const { link } = await run.event('link');
  expect(run.events().some((e) => e.event === 'guarantee'), 'wasm is checked here').toBe(false);

  await cheater(link);
  const banned = await run.event('banned', 120_000);
  expect(banned.redo, 'the cheater had accepted results to redo').toBeGreaterThan(0);

  const tab = await h.page(await h.browser('chromium'), 'honest tab');
  await tab.goto(pageUrl(link));
  await tab.getByRole('button', { name: 'Contribute' }).click();
  h.cli(['--json', 'join', '-', '--slots', '2'], { stdin: `${link}\n` });

  expect(await run.exit(3 * 60_000), run.stderr).toBe(0);
  const done = run.events().find((e) => e.event === 'done');
  expect(done.banned).toBe(1);
  expect(sha256(readFileSync(path.join(out, 'mandelbrot.ppm'))), 'the final image is the honest one').toBe(honest);
  const results = JSON.parse(readFileSync(path.join(out, 'results.json'), 'utf8'));
  expect(results.outputs.every((/** @type {{ by: string }} */ o) => o.by !== banned.peer)).toBe(true);
  h.checkPages();
});
