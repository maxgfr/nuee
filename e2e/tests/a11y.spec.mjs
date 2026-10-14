// Accessibility, once per engine: axe-core (WCAG 2.0/2.1 A and AA, 2.2 AA) finds nothing on
// every page in light and dark schemes, and Tab reaches the primary actions with a visible focus.
import { writeFileSync } from 'node:fs';
import path from 'node:path';
import { test, expect } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';
import { Harness, pageUrl, services, watch } from './helpers.mjs';

const TAGS = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'];

/** @type {Harness} */
let h;
/** Real links from running CLI nodes. */
const links = { job: '', file: '' };

test.beforeAll(async () => {
  h = new Harness();
  // A coordinator that waits for a peer forever: its link stays valid for the whole file.
  const run = h.cli(['--json', 'run', 'demo:mandelbrot', '--local-slots', '0', '--wait-peers', '1', '--out', path.join(h.dir(), 'out')]);
  links.job = (await run.event('link')).link;
  const dir = h.dir();
  const file = path.join(dir, 'lisez-moi.txt');
  writeFileSync(file, 'Nuée\n');
  const share = h.cli(['--json', 'share', file]);
  links.file = (await share.event('link')).link;
});

test.afterAll(async () => {
  await h.close();
});

/**
 * Pages to check: where to go, what shows it is ready, and its primary action.
 * @type {Array<{ name: string, url: () => string, ready: (p: import('@playwright/test').Page) => import('@playwright/test').Locator, action: (p: import('@playwright/test').Page) => import('@playwright/test').Locator }>}
 */
const PAGES = [
  { name: 'home', url: () => pageUrl('#/'), ready: (p) => p.locator('#open-link'), action: (p) => p.getByRole('button', { name: 'Open', exact: true }) },
  { name: 'run', url: () => pageUrl('#/run'), ready: (p) => p.locator('#verify'), action: (p) => p.getByRole('button', { name: 'Start' }) },
  { name: 'join', url: () => pageUrl(links.job), ready: (p) => p.locator('.consent'), action: (p) => p.getByRole('button', { name: 'Contribute' }) },
  { name: 'file', url: () => pageUrl(links.file), ready: (p) => p.locator('#main .card'), action: (p) => p.getByRole('button', { name: 'Download' }) },
  { name: 'share', url: () => pageUrl('#/share'), ready: (p) => p.locator('#share-files'), action: (p) => p.locator('#share-files') },
  { name: 'about', url: () => pageUrl('#/about'), ready: (p) => p.locator('#main h1'), action: (p) => p.locator('.nav a[href="#/about"]') },
  // The documentation pages are static: the primary action is the first Copy button docs.js adds.
  { name: 'docs-en', url: () => new URL('docs/', services().web).href, ready: (p) => p.locator('#main h1'), action: (p) => p.locator('.copy-btn').first() },
];

for (const scheme of /** @type {const} */ (['light', 'dark'])) {
  for (const target of PAGES) {
    test(`axe: ${target.name}, ${scheme}`, async ({ page }) => {
      /** @type {string[]} */
      const problems = [];
      watch(page, target.name, problems);
      await page.emulateMedia({ colorScheme: scheme, reducedMotion: 'reduce' });
      await page.goto(target.url());
      await expect(target.ready(page)).toBeVisible({ timeout: 30_000 });
      const result = await new AxeBuilder({ page }).withTags(TAGS).analyze();
      const found = result.violations.map((v) => `${v.id}: ${v.help} (${v.nodes.map((n) => n.target.join(' ')).join(', ')})`);
      expect(found).toEqual([]);
      expect(problems).toEqual([]);
    });
  }
}

test('keyboard: Tab reaches the primary actions with a visible focus outline', async ({ page, browserName }) => {
  /** @type {string[]} */
  const problems = [];
  watch(page, 'keyboard', problems);
  // Safari moves between links with Option-Tab; buttons and fields take plain Tab everywhere.
  const tab = browserName === 'webkit' ? 'Alt+Tab' : 'Tab';
  for (const target of PAGES) {
    // A fresh load each time, so focus starts from the top of the document.
    await page.goto('about:blank');
    await page.goto(target.url());
    await expect(target.ready(page)).toBeVisible({ timeout: 30_000 });
    const action = target.action(page);
    await expect(action).toBeVisible();
    let reached = false;
    for (let i = 0; i < 60 && !reached; i += 1) {
      await page.keyboard.press(tab);
      reached = await action.evaluate((el) => el === document.activeElement);
    }
    expect(reached, `${target.name}: Tab never reached the primary action`).toBe(true);
    const outline = await action.evaluate((el) => {
      const s = getComputedStyle(el);
      return { style: s.outlineStyle, width: parseFloat(s.outlineWidth), color: s.outlineColor, visible: el.matches(':focus-visible') };
    });
    expect(outline.visible, `${target.name}: focus is not visible`).toBe(true);
    expect(outline.style, `${target.name}: no focus outline`).not.toBe('none');
    expect(outline.width, `${target.name}: focus outline too thin`).toBeGreaterThanOrEqual(2);
  }
  expect(problems).toEqual([]);
});

test('keyboard: arrowing through the job choices never leaves the page', async ({ page }) => {
  /** @type {string[]} */
  const problems = [];
  watch(page, 'choices', problems);
  await page.goto(pageUrl('#/run'));
  await expect(page.locator('#verify')).toBeVisible({ timeout: 30_000 });
  await page.locator('input[name=job][value=mandelbrot]').focus();
  // Down through every choice (Train together among them) and back up.
  for (const key of ['ArrowDown', 'ArrowDown', 'ArrowDown', 'ArrowDown', 'ArrowDown', 'ArrowDown', 'ArrowUp', 'ArrowUp']) {
    await page.keyboard.press(key);
    expect(new URL(page.url()).hash).toBe('#/run');
  }
  // Training opens its own page from Start.
  await page.locator('input[name=job][value=train]').check();
  expect(new URL(page.url()).hash).toBe('#/run');
  await page.getByRole('button', { name: 'Start' }).click();
  await expect(page).toHaveURL(/#\/train$/);
  expect(problems).toEqual([]);
});

test('home: the murmuration settles and stops within about five seconds', async ({ page }) => {
  /** @type {string[]} */
  const problems = [];
  watch(page, 'murmuration', problems);
  await page.emulateMedia({ reducedMotion: 'no-preference' });
  await page.goto(pageUrl('#/'));
  await expect(page.locator('#open-link')).toBeVisible({ timeout: 30_000 });
  const flock = page.locator('canvas.flock');
  await page.waitForTimeout(6_500);
  const a = await flock.evaluate((c) => /** @type {HTMLCanvasElement} */ (c).toDataURL());
  await page.waitForTimeout(1_000);
  const b = await flock.evaluate((c) => /** @type {HTMLCanvasElement} */ (c).toDataURL());
  expect(a === b, 'the flock still moves after six seconds').toBe(true);
  expect(problems).toEqual([]);
});

test('copy: a failed copy is announced and the label comes back', async ({ page }) => {
  /** @type {string[]} */
  const problems = [];
  watch(page, 'copy', problems);
  await page.addInitScript(() => {
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText: () => Promise.reject(new Error('denied')) } });
  });
  await page.goto(pageUrl('#/'));
  await expect(page.locator('#open-link')).toBeVisible({ timeout: 30_000 });
  const copy = page.locator('.cli .copy-btn');
  await copy.click();
  await expect(copy).toContainText('Copy failed');
  await expect(page.locator('#status')).toContainText('Copy failed');
  await expect(copy).toHaveText('Copy', { timeout: 10_000 });
  expect(problems).toEqual([]);
});

test('join on a 320px phone: nothing scrolls sideways', async ({ page }) => {
  /** @type {string[]} */
  const problems = [];
  watch(page, 'join-320', problems);
  await page.setViewportSize({ width: 320, height: 700 });
  await page.goto(pageUrl(links.job));
  await expect(page.locator('.consent .emoji')).toBeVisible({ timeout: 30_000 });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  expect(problems).toEqual([]);
});
