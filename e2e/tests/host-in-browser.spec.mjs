// A Chromium page hosts a Mandelbrot job, coordinating only, for a CLI worker and a Firefox
// tab that both compute; the page sees the job through, verified in full, without a mismatch.
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

/** True when the page scrolls sideways. @param {import('@playwright/test').Page} page */
const overflows = (page) => page.evaluate(() => document.documentElement.scrollWidth > window.innerWidth);

test('a page coordinates a job for a CLI worker and a Firefox tab', async () => {
  const host = await h.page(await h.browser('chromium'), 'chromium host');
  // A phone-sized presenter: the live board must fit it without sideways scrolling.
  await host.setViewportSize({ width: 390, height: 844 });
  await host.goto(pageUrl('#/run'));
  await host.locator('input[name=job][value=mandelbrot]').check();
  await host.locator('#verify').selectOption('full');
  // The page only coordinates: every task is computed by the CLI worker and the Firefox tab.
  await host.locator('#local').uncheck();
  await host.getByRole('button', { name: 'Start' }).click();

  const field = host.locator('#share-link');
  await expect(field).toHaveValue(/#\/J\//, { timeout: 30_000 });
  // Focus moved to the job's title, which comes first in reading order.
  await expect(host.locator('#run-title')).toBeFocused();
  await expect(host.locator('#run-title')).toHaveText('Fractal (CPU)');
  await expect(host.locator('.live > :first-child')).toHaveClass(/\bwork-col\b/);
  // No correction yet: the note is in pencil, not red.
  await expect(host.locator('.corr-note')).toHaveClass(/\bclean\b/);
  const link = await field.inputValue();
  expect(link.startsWith(services().web)).toBe(true);

  const worker = h.cli(['--json', 'join', '-', '--slots', '2'], { stdin: `${link}\n` });
  const tab = await h.page(await h.browser('firefox'), 'firefox');
  await tab.goto(pageUrl(link));
  await tab.getByRole('button', { name: 'Contribute' }).click();
  // Focus follows the new state: the contribution's status line.
  await expect(tab.locator('.contributing .state')).toBeFocused({ timeout: 60_000 });

  await expect(host.getByText('64 of 64 tasks')).toBeVisible();
  await expect(host.getByText('Done. Every task has a result.')).toBeVisible();
  const progress = host.locator('.progress progress');
  expect(await progress.evaluate((p) => /** @type {HTMLProgressElement} */ (p).value / /** @type {HTMLProgressElement} */ (p).max)).toBe(1);
  const stat = (/** @type {string} */ name) => host.locator('dl.stats dt', { hasText: new RegExp(`^${name}$`) }).locator('xpath=following-sibling::dd[1]');
  await expect(stat('Mismatches')).toHaveText('0');
  await expect(stat('Banned')).toHaveText('0');
  await expect(stat('Failed attempts')).toHaveText('0');

  // Both joiners were admitted into the page's swarm.
  await worker.event('welcome', 60_000);
  await expect(tab.locator('.contributing')).toBeVisible();
  await expect(host.locator('table.peers tbody tr')).toHaveCount(2, { timeout: 30_000 });
  await expect(host.locator('table.peers tbody')).toContainText('Command line');
  await expect(host.locator('table.peers tbody')).toContainText('Browser');
  await expect(host.locator('.corr-note')).toHaveClass(/\bclean\b/);

  // The devices roster with real rows fits a phone: no sideways scroll at 390 or 320px.
  expect(await overflows(host), 'run page overflows at 390px').toBe(false);
  await host.setViewportSize({ width: 320, height: 700 });
  expect(await overflows(host), 'run page overflows at 320px').toBe(false);
  await host.setViewportSize({ width: 390, height: 844 });

  // Forced colours: a done square keeps its number visible (text and fill differ).
  await host.emulateMedia({ forcedColors: 'active' });
  const square = await host.locator('.task-grid li.done').first().evaluate((li) => {
    const s = getComputedStyle(li);
    return { color: s.color, background: s.backgroundColor };
  });
  expect(square.color).not.toBe(square.background);
  await host.emulateMedia({ forcedColors: 'none' });

  // Presentation mode is a modal dialog: focus stays inside, Escape closes it and focus returns.
  const present = host.getByRole('button', { name: 'Present full screen' });
  await present.click();
  const dialog = host.locator('dialog.present');
  await expect(dialog).toHaveAttribute('open', '');
  await expect(dialog.getByRole('button')).toBeFocused();
  // The dialog is drawn on top of the page (also when the page went full screen).
  expect(
    await dialog.getByRole('button').evaluate((b) => {
      const r = b.getBoundingClientRect();
      return document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2) === b;
    }),
  ).toBe(true);
  // Tab cycles between the dialog and the browser's own controls, never the page behind it.
  for (let i = 0; i < 4; i += 1) {
    await host.keyboard.press('Tab');
    const behind = await dialog.evaluate((d) => {
      const a = document.activeElement;
      return a !== null && a !== document.body && !d.contains(a);
    });
    expect(behind, 'focus reached the page behind the presentation').toBe(false);
  }
  await host.keyboard.press('Escape');
  await expect(dialog).not.toHaveAttribute('open', '');
  await expect(present).toBeFocused();
  expect(await host.evaluate(() => document.fullscreenElement)).toBe(null);

  // Both joiners computed: the CLI by its task events, the tab by its own counter.
  expect(worker.events().filter((e) => e.event === 'task' && e.ok === true).length).toBeGreaterThan(0);
  const counter = (await tab.locator('.contributing .big-number').textContent()) ?? '';
  const n = Number(/^([\d,]+) tasks done/.exec(counter)?.[1]?.replace(/,/g, '') ?? '0');
  expect(n, `firefox: ${counter}`).toBeGreaterThan(0);

  // Stopping the swarm brings focus back to the page title.
  await host.getByRole('button', { name: 'Stop the swarm' }).click();
  await expect(host.locator('#run-title')).toBeFocused();
  h.checkPages();
});
