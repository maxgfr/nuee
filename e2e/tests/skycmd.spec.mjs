// skycmd-arena verified in full between wasmtime (CLI), V8 (Chromium) and SpiderMonkey
// (Firefox): every task runs twice and any byte of difference would be a mismatch.
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { test, expect } from '@playwright/test';
import { Harness, pageUrl } from './helpers.mjs';

/** @type {Harness} */
let h;
test.beforeEach(() => {
  h = new Harness();
});
test.afterEach(async () => {
  await h.close();
});

test('demo:skycmd, verify full, with Chromium and Firefox tabs', async () => {
  const out = path.join(h.dir(), 'out');
  const run = h.cli(['--json', 'run', 'demo:skycmd', '--verify', 'full', '--local-slots', '1', '--wait-peers', '2', '--out', out]);
  const { link } = await run.event('link', 2 * 60_000);

  /** @type {import('@playwright/test').Page[]} */
  const tabs = [];
  for (const engine of /** @type {const} */ (['chromium', 'firefox'])) {
    const page = await h.page(await h.browser(engine), engine);
    await page.goto(pageUrl(link));
    await page.getByRole('button', { name: 'Contribute' }).click();
    tabs.push(page);
  }

  expect(await run.exit(3 * 60_000), run.stderr).toBe(0);
  const events = run.events();
  const done = events.find((e) => e.event === 'done');
  expect(done).toBeTruthy();
  expect(done.banned).toBe(0);
  expect(events.filter((e) => e.event === 'mismatch')).toEqual([]);
  expect(events.filter((e) => e.event === 'task_failed')).toEqual([]);

  const results = JSON.parse(readFileSync(path.join(out, 'results.json'), 'utf8'));
  expect(results.outputs).toHaveLength(done.tasks);
  expect(results.done).toBe(done.tasks);
  const games = new Set();
  for (const o of results.outputs) {
    const doc = JSON.parse(readFileSync(path.join(out, o.file), 'utf8'));
    expect(doc.mode).toBe('eval');
    expect(doc.metrics.episodes).toBe(doc.episodes);
    expect(typeof doc.metrics.mean_score).toBe('number');
    games.add(doc.game);
  }
  expect([...games].sort()).toEqual(['dodge', 'doom', 'flappy', 'rescue']);
  expect(events.filter((e) => e.event === 'summary').map((e) => e.game).sort()).toEqual(['dodge', 'doom', 'flappy', 'rescue']);

  for (const page of tabs) await expect(page.getByRole('heading', { name: 'Thank you!' })).toBeVisible({ timeout: 60_000 });
  h.checkPages();
});
