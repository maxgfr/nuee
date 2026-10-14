// GPU (opt-in: NUEE_E2E_GPU=1, needs a GPU and, for the LLM test, huggingface.co).
// - Mandelbrot in WGSL, verified in full between a CLI worker (wgpu) and a Chromium tab
//   (WebGPU): the tolerance comparator accepts every pair, nobody is banned.
// - An `llm` job scored by a Chromium tab running Qwen3.5-0.8B with WebGPU: the tab's tlogits
//   have one score per option and pick the obvious answer of trivial records.
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { test, expect, chromium } from '@playwright/test';
import { Harness, pageUrl, services, watch } from './helpers.mjs';

/** Chromium with WebGPU on the real GPU (Metal on macOS) rather than the software adapter. */
const WEBGPU_ARGS = ['--enable-unsafe-webgpu', '--enable-gpu', '--use-angle=metal'];

test.describe('GPU', () => {
  test.skip(!process.env.NUEE_E2E_GPU, 'set NUEE_E2E_GPU=1 to run GPU e2e');

  /** @type {Harness} */
  let h;
  /** @type {import('@playwright/test').BrowserContext | null} */
  let persistent = null;
  test.beforeEach(() => {
    h = new Harness();
  });
  test.afterEach(async () => {
    if (persistent) await persistent.close();
    persistent = null;
    await h.close();
  });

  test('Mandelbrot WGSL: a CLI on wgpu and a Chromium tab on WebGPU agree within tolerance', async () => {
    const out = path.join(h.dir(), 'out');
    // No local slot: every tile runs on both workers, so each comparison is wgpu against WebGPU.
    const run = h.cli(['--json', 'run', 'demo:mandelbrot-gpu', '--verify', 'full', '--local-slots', '0', '--wait-peers', '2', '--out', out]);
    const { link } = await run.event('link');

    const browser = await chromium.launch({ args: WEBGPU_ARGS });
    h.browsers.push(browser);
    const tab = await h.page(browser, 'chromium');
    await tab.goto(pageUrl(link));
    // The GPU toggle exists (WebGPU adapter found) and is on by default.
    await expect(tab.locator('#gpu')).toBeChecked();
    await tab.getByRole('button', { name: 'Contribute' }).click();
    const worker = h.cli(['--json', 'join', '-', '--gpu', '--slots', '2'], { stdin: `${link}\n` });

    expect(await run.exit(5 * 60_000), run.stderr).toBe(0);
    expect(await worker.exit(60_000), worker.stderr).toBe(0);
    const events = run.events();
    const done = events.find((e) => e.event === 'done');
    expect(done).toBeTruthy();
    expect(done.tasks).toBe(256);
    expect(done.banned).toBe(0);
    expect(events.filter((e) => e.event === 'mismatch')).toEqual([]);
    expect(events.filter((e) => e.event === 'banned')).toEqual([]);
    expect(events.filter((e) => e.event === 'task_failed')).toEqual([]);

    const joined = events.filter((e) => e.event === 'peer' && e.state === 'joined');
    const tabId = joined.find((e) => String(e.agent).startsWith('nuee-web/'))?.id;
    const cliId = joined.find((e) => String(e.agent).startsWith('nuee-cli/'))?.id;
    expect(tabId).toBeTruthy();
    expect(cliId).toBeTruthy();
    // Every tile was computed twice and accepted: once by each worker.
    const verified = events.filter((e) => e.event === 'verified');
    expect(verified.length).toBeGreaterThan(0);
    const results = JSON.parse(readFileSync(path.join(out, 'results.json'), 'utf8'));
    expect(results.outputs).toHaveLength(256);
    // `by` in results.json and `verified` events names only the execution that arrived first,
    // so a steadily slower peer never appears there although its results agreed. The `done`
    // event's per-peer counters credit both executions, whatever the arrival order.
    const credit = (/** @type {string} */ id) => done.peers.find((/** @type {{ id: string }} */ p) => p.id === id);
    const tabCredit = credit(tabId);
    const cliCredit = credit(cliId);
    // `done` counts first arrivals too, so only `verified`, which credits both agreeing
    // executions, is checked here.
    expect(tabCredit?.verified, 'WebGPU results agreed with wgpu').toBeGreaterThan(0);
    expect(cliCredit?.verified, 'wgpu results agreed with WebGPU').toBeGreaterThan(0);
    expect(worker.events().filter((e) => e.event === 'task' && e.ok === true).length).toBeGreaterThan(0);
    await expect(tab.getByRole('heading', { name: 'Thank you!' })).toBeVisible({ timeout: 60_000 });
    h.checkPages();
  });

  test('llm job: a Chromium tab scores trivial records with Qwen3.5-0.8B on WebGPU', async () => {
    // First run downloads about 650 MB; the browser profile (its cache) lives in the run's temp dir.
    test.setTimeout(45 * 60_000);
    const job = h.dir();
    const records = [
      { t: 'choice', ins: 'What is 2 + 2?', opts: ['4', '9'], answer: 0 },
      { t: 'choice', ins: 'What is the opposite of hot?', opts: ['warm', 'cold'], answer: 1 },
      { t: 'choice', ins: 'What color is the sky on a clear day?', opts: ['green', 'red', 'blue'], answer: 2 },
    ];
    writeFileSync(path.join(job, 'records.jsonl'), records.map(({ t, ins, opts }) => `${JSON.stringify({ t, ins, opts })}\n`).join(''));
    writeFileSync(
      path.join(job, 'llm.toml'),
      `name = "llm-trivial"
kind = "llm"
# No --llm endpoint here to recompute with, and a single scorer: nothing to compare with.
verify = "none"
deadline = "30m"

[llm]
model = "Qwen3.5-0.8B"

[inputs]
jsonl = "records.jsonl"
`,
    );
    const out = path.join(job, 'out');
    const run = h.cli(['--json', 'run', path.join(job, 'llm.toml'), '--wait-peers', '1', '--out', out]);
    const { link } = await run.event('link');

    const profile = path.join(services().tmp, 'chromium-llm-profile');
    mkdirSync(profile, { recursive: true });
    persistent = await chromium.launchPersistentContext(profile, { args: WEBGPU_ARGS, locale: 'en-US' });
    const tab = await persistent.newPage();
    watch(tab, 'chromium llm', h.problems);
    await tab.goto(pageUrl(link));
    const webgpu = await tab.evaluate(async () => {
      const gpu = /** @type {{ gpu?: { requestAdapter(): Promise<unknown> } }} */ (/** @type {unknown} */ (navigator)).gpu;
      return gpu !== undefined && (await gpu.requestAdapter()) !== null;
    });
    expect(webgpu, 'WebGPU adapter in the tab').toBe(true);
    await tab.locator('#llm').check();
    const started = Date.now();
    await tab.getByRole('button', { name: 'Contribute' }).click();

    expect(await run.exit(40 * 60_000), run.stderr).toBe(0);
    const seconds = (Date.now() - started) / 1000;
    console.log(`llm: ${records.length} records scored in the browser in ${seconds.toFixed(1)} s (model download and load included)`);
    const events = run.events();
    const done = events.find((e) => e.event === 'done');
    expect(done).toBeTruthy();
    expect(done.tasks).toBe(1);
    expect(events.filter((e) => e.event === 'task_failed')).toEqual([]);
    expect(typeof done.merged).toBe('string');
    const rows = readFileSync(done.merged, 'utf8').trim().split('\n').map((l) => JSON.parse(l));
    expect(rows.map((r) => r.uid)).toEqual([0, 1, 2]);
    for (const [i, r] of rows.entries()) {
      const rec = /** @type {typeof records[number]} */ (records[i]);
      expect(Object.keys(r).sort()).toEqual(['tlogits', 'uid']);
      expect(r.tlogits).toHaveLength(rec.opts.length);
      for (const v of r.tlogits) expect(Number.isFinite(v)).toBe(true);
    }
    const best = rows.map((r) => r.tlogits.indexOf(Math.max(...r.tlogits)));
    expect(best, `tlogits ${JSON.stringify(rows.map((r) => r.tlogits))}`).toEqual(records.map((r) => r.answer));
    h.checkPages();
  });
});
