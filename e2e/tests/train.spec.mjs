// Train together: a Chromium page runs the DiLoCo demo (toy trainer on optdigits) for a Firefox
// tab and a CLI worker. The loss falls round after round, the final test accuracy reaches the
// toy trainer's threshold, and recomputed deltas all matched.
import { test, expect } from '@playwright/test';
import { Harness, pageUrl } from './helpers.mjs';

/** Accuracy a 4-worker DiLoCo run of the demo reaches (crates/nuee-exec/tests/toy_trainer.rs). */
const MIN_ACCURACY = 0.85;

/** @type {Harness} */
let h;
test.beforeEach(() => {
  h = new Harness();
});
test.afterEach(async () => {
  await h.close();
});

test('DiLoCo demo hosted by a page, with a Firefox tab and a CLI worker', async () => {
  const host = await h.page(await h.browser('chromium'), 'chromium host');
  await host.goto(pageUrl('#/train'));
  const rounds = Number(await host.locator('#train-rounds').inputValue());
  expect(rounds).toBeGreaterThan(1);
  // Every delta a peer sends is recomputed and compared byte for byte.
  await host.locator('#train-verify').selectOption('full');
  // The page only coordinates: every delta comes from the Firefox tab or the CLI worker.
  await host.locator('#train-local').uncheck();
  await host.getByRole('button', { name: 'Start training' }).click();

  const field = host.locator('#share-link');
  await expect(field).toHaveValue(/#\/J\//, { timeout: 60_000 });
  const link = await field.inputValue();

  const worker = h.cli(['--json', 'join', '-', '--slots', '2'], { stdin: `${link}\n` });
  const tab = await h.page(await h.browser('firefox'), 'firefox');
  await tab.goto(pageUrl(link));
  await tab.getByRole('button', { name: 'Contribute' }).click();

  const accuracy = host.locator('.results .big-number');
  await expect(accuracy).toHaveText(/^Test accuracy: [\d.]+% on [\d,]+ digits/, { timeout: 5 * 60_000 });
  const pct = Number(/Test accuracy: ([\d.]+)%/.exec((await accuracy.textContent()) ?? '')?.[1]);
  expect(pct / 100).toBeGreaterThanOrEqual(MIN_ACCURACY);
  await expect(host.locator('.train-summary')).toHaveText(/^Training done: /);

  // One loss point per round; the curve's y grows downwards, so a lower loss has a larger y.
  const points = ((await host.locator('polyline.loss-line').getAttribute('points')) ?? '').trim().split(' ');
  expect(points).toHaveLength(rounds);
  const y = points.map((p) => Number(p.split(',')[1]));
  expect(y[y.length - 1], `loss curve ${points.join(' ')}`).toBeGreaterThan(/** @type {number} */ (y[0]));

  const stat = (/** @type {string} */ name) => host.locator('dl.stats dt', { hasText: new RegExp(`^${name}$`) }).locator('xpath=following-sibling::dd[1]');
  await expect(stat('Round')).toHaveText(`${rounds} / ${rounds}`);
  const verified = Number(((await stat('Verified').textContent()) ?? '0').replace(/,/g, ''));
  expect(verified, 'recomputed deltas').toBeGreaterThanOrEqual(1);
  await expect(stat('Mismatches')).toHaveText('0');
  await expect(stat('Banned')).toHaveText('0');

  // Both joiners trained: the CLI by its task events, the tab by its own counter.
  await worker.event('welcome', 1_000);
  expect(worker.events().filter((e) => e.event === 'task' && e.ok === true).length).toBeGreaterThan(0);
  const counter = (await tab.locator('.contributing .big-number').textContent()) ?? '';
  const n = Number(/^([\d,]+) tasks done/.exec(counter)?.[1]?.replace(/,/g, '') ?? '0');
  expect(n, `firefox: ${counter}`).toBeGreaterThan(0);
  h.checkPages();
});
