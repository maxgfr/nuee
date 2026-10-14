// Anchor: a job handed to `nuee serve --anchor` outlives its submitter. The submitter returns
// at once; a Chromium tab joins the swarm link and the anchor sees the job through; a second
// client then retrieves every result from the anchor.
import { readFileSync, readdirSync } from 'node:fs';
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

test('a job handed to an anchor completes after its submitter left; results are retrievable', async () => {
  const anchor = h.cli(['--json', 'serve', '--anchor', '--relay-dev', '--relay-bind', '127.0.0.1:0', '--local-slots', '0']);
  const started = await anchor.event('anchor');
  expect(started.link).toMatch(/#\/X\//);
  expect(started.swarm).toMatch(/#\/J\//);

  // The submitter uploads the job and returns without waiting for it. With no local slot the
  // anchor cannot recompute samples, so the job asks for no verification.
  const submit = h.cli(['--json', 'run', 'demo:mandelbrot', '--verify', 'none', '--anchor', '-'], { stdin: `${started.link}\n` });
  expect(await submit.exit(90_000), submit.stderr).toBe(0);
  const anchored = await submit.event('anchored');
  expect(anchored.existing).toBe(false);
  const jobLink = await submit.event('link');
  expect(jobLink.kind).toBe('job');
  expect(jobLink.link).toMatch(/#\/J\//);

  const tab = await h.page(await h.browser('chromium'), 'chromium');
  await tab.goto(pageUrl(jobLink.link));
  await tab.getByRole('button', { name: 'Contribute' }).click();
  const finished = await anchor.event('job_done', 3 * 60_000);
  expect(finished.job).toBe(anchored.job);
  expect(finished.failed).toBe(0);
  expect(anchor.events().filter((e) => e.event === 'banned')).toEqual([]);
  const joined = anchor.events().filter((e) => e.event === 'peer' && e.state === 'joined');
  expect(joined.map((e) => String(e.agent).split('/')[0])).toEqual(['nuee-web']);
  // The anchor runs no task itself: the tab computed them all.
  await expect(tab.locator('.contributing .big-number')).toHaveText(/^256 tasks done/);

  // Anyone with the anchor link gets the results of the same job.
  const out = path.join(h.dir(), 'results');
  const follow = h.cli(['--json', 'run', 'demo:mandelbrot', '--verify', 'none', '--anchor', '-', '--follow', '--out', out], { stdin: `${started.link}\n` });
  expect(await follow.exit(2 * 60_000), follow.stderr).toBe(0);
  const attached = await follow.event('anchored');
  expect(attached.existing).toBe(true);
  expect(attached.job).toBe(anchored.job);
  const done = await follow.event('done');
  expect(done.tasks).toBe(256);
  const tiles = readdirSync(out).filter((f) => /^task-.*\.bin$/.test(f));
  expect(tiles).toHaveLength(256);
  const results = JSON.parse(readFileSync(path.join(out, 'results.json'), 'utf8'));
  expect(results.done).toBe(256);
  h.checkPages();
});
