// The direct lane: a Chromium presenter with WebRTC on, a Firefox tab and a Chromium tab that
// accept it. Results reach the presenter over the data channels; when the Firefox tab's peer
// connection is closed mid-job its work goes on through iroh and the job still completes
// without a mismatch. With WebRTC off, no page ever creates an RTCPeerConnection.
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { test, expect } from '@playwright/test';
import { chromium, firefox } from '@playwright/test';
import { Harness, pageUrl, services } from './helpers.mjs';

/** @type {Harness} */
let h;
test.beforeEach(() => {
  h = new Harness();
});
test.afterEach(async () => {
  await h.close();
});

// Host candidates only (no STUN): the browsers must expose them as plain addresses, not mDNS names.
async function launch(/** @type {'chromium' | 'firefox'} */ name) {
  const b =
    name === 'chromium'
      ? await chromium.launch({ args: ['--disable-features=WebRtcHideLocalIpsWithMdns'] })
      : await firefox.launch({
          firefoxUserPrefs: {
            'media.peerconnection.ice.obfuscate_host_addresses': false,
            'media.peerconnection.ice.loopback': true,
          },
        });
  h.browsers.push(b);
  return b;
}

/** A watched page counting every RTCPeerConnection it constructs in `window.pcCount`. */
async function countingPage(/** @type {import('@playwright/test').Browser} */ browser, /** @type {string} */ label) {
  const page = await h.page(browser, label);
  await page.addInitScript(() => {
    const w = /** @type {Record<string, unknown>} */ (/** @type {unknown} */ (window));
    w.pcCount = 0;
    const Native = window.RTCPeerConnection;
    if (typeof Native !== 'function') return;
    window.RTCPeerConnection = class extends Native {
      /** @param {RTCConfiguration} [cfg] */
      constructor(cfg) {
        super(cfg);
        w.pcCount = Number(w.pcCount) + 1;
      }
    };
  });
  return page;
}

/** The page URL of a link, with the `?e2e=1` hook on. @param {string} link */
function e2eUrl(link) {
  const u = new URL(pageUrl(link));
  u.searchParams.set('e2e', '1');
  return u.href;
}

/** @param {import('@playwright/test').Page} page @param {string} name */
const stat = (page, name) => page.locator('dl.stats dt', { hasText: new RegExp(`^${name}$`) }).locator('xpath=following-sibling::dd[1]');

/** Tasks done shown by a joined tab. @param {import('@playwright/test').Page} tab */
async function tasksDone(tab) {
  const text = (await tab.locator('.contributing .big-number').textContent()) ?? '';
  return Number(/^([\d,]+) tasks done/.exec(text)?.[1]?.replace(/,/g, '') ?? '0');
}

/** @param {import('@playwright/test').Page} tab @param {number} cores */
async function contribute(tab, cores) {
  await tab.locator('#cores').fill(String(cores));
  await tab.locator('#direct').check();
  await tab.getByRole('button', { name: 'Contribute' }).click();
}

/**
 * 16 heavy tiles: 256×256 points inside the main cardioid, 4000 iterations each, so the job
 * lasts long enough to cut a lane in the middle. Same input layout as the Mandelbrot view.
 */
function heavyInputs() {
  const out = [];
  for (let i = 0; i < 16; i += 1) {
    const b = Buffer.alloc(36);
    b.writeDoubleLE(-0.1 + i * 1e-4, 0);
    b.writeDoubleLE(-0.0001, 8);
    b.writeDoubleLE(1e-6, 16);
    b.writeUInt32LE(256, 24);
    b.writeUInt32LE(256, 28);
    b.writeUInt32LE(4000, 32);
    out.push({ name: `tile-${String(i).padStart(2, '0')}.bin`, mimeType: 'application/octet-stream', buffer: b });
  }
  return out;
}

test('results travel over direct lanes and fall back to iroh when one closes', async () => {
  const host = await countingPage(await launch('chromium'), 'chromium host');
  await host.goto(e2eUrl('#/run'));
  await host.locator('input[name=job][value=custom]').check();
  const wasm = readFileSync(path.join(services().root, 'tasks/golden/mandelbrot.wasm'));
  await host.locator('#custom-module').setInputFiles({ name: 'mandelbrot.wasm', mimeType: 'application/wasm', buffer: wasm });
  await expect(host.getByText(/^Valid module /)).toBeVisible();
  await host.locator('#custom-inputs').setInputFiles(heavyInputs());
  await host.locator('#verify').selectOption('full');
  await host.locator('#local').uncheck();
  await host.locator('#rtc').check();
  await host.getByRole('button', { name: 'Start' }).click();

  const field = host.locator('#share-link');
  await expect(field).toHaveValue(/#\/J\//, { timeout: 30_000 });
  const link = await field.inputValue();

  const fox = await countingPage(await launch('firefox'), 'firefox');
  const chrome = await countingPage(await launch('chromium'), 'chromium tab');
  await fox.goto(e2eUrl(link));
  await chrome.goto(e2eUrl(link));
  await contribute(fox, 1);
  await contribute(chrome, 1);

  // Both lanes open and carry results.
  const opened = (/** @type {import('@playwright/test').Page} */ p) =>
    p.waitForFunction(() => {
      const r = /** @type {{ nueeRtc?: { stats: { opened: number, framesOut: number } } }} */ (/** @type {unknown} */ (window)).nueeRtc;
      return r !== undefined && r.stats.opened >= 1 && r.stats.framesOut >= 1;
    }, null, { timeout: 60_000 });
  await opened(fox);
  await opened(chrome);
  await expect(fox.getByText('Direct connection to the presenter open.')).toBeVisible();

  // Cut the Firefox lane mid-job: its work must go on through iroh.
  const before = await tasksDone(fox);
  const doneBefore = Number((await host.locator('.progress').textContent())?.match(/(\d+) of 16/)?.[1] ?? '16');
  expect(doneBefore, 'the job ended before the lane could be cut').toBeLessThan(16);
  await fox.evaluate(() => {
    const r = /** @type {{ nueeRtc: { closeAll(): void } }} */ (/** @type {unknown} */ (window)).nueeRtc;
    r.closeAll();
  });
  const foxOutAtClose = await fox.evaluate(() => /** @type {{ nueeRtc: { stats: { framesOut: number } } }} */ (/** @type {unknown} */ (window)).nueeRtc.stats.framesOut);

  await expect(host.getByText('16 of 16 tasks')).toBeVisible();
  await expect(host.getByText('Done. Every task has a result.')).toBeVisible();
  await expect(stat(host, 'Mismatches')).toHaveText('0');
  await expect(stat(host, 'Banned')).toHaveText('0');
  await expect(stat(host, 'Failed attempts')).toHaveText('0');
  const direct = Number((await stat(host, 'Results over direct connections').textContent())?.replace(/,/g, '') ?? '0');
  expect(direct, 'results received over the lanes').toBeGreaterThan(0);
  const hostStats = await host.evaluate(() => /** @type {{ nueeRtc: { stats: { created: number, opened: number, framesIn: number } } }} */ (/** @type {unknown} */ (window)).nueeRtc.stats);
  expect(hostStats.opened).toBeGreaterThanOrEqual(2);
  expect(hostStats.framesIn).toBe(direct);

  // After the cut, Firefox kept computing, and sent nothing more on the closed lane.
  const after = await tasksDone(fox);
  expect(after, 'firefox computed after its lane closed').toBeGreaterThan(before);
  const foxStats = await fox.evaluate(() => /** @type {{ nueeRtc: { stats: { framesOut: number, open: number } } }} */ (/** @type {unknown} */ (window)).nueeRtc.stats);
  expect(foxStats.framesOut).toBe(foxOutAtClose);
  expect(foxStats.open).toBe(0);
  for (const p of [host, fox, chrome]) {
    expect(await p.evaluate(() => /** @type {Record<string, number>} */ (/** @type {unknown} */ (window)).pcCount)).toBeGreaterThan(0);
  }
  h.checkPages();
});

test('with WebRTC off no RTCPeerConnection is ever created', async () => {
  const host = await countingPage(await launch('chromium'), 'chromium host');
  await host.goto(e2eUrl('#/run'));
  await host.locator('input[name=job][value=mandelbrot]').check();
  await host.locator('#keep').uncheck();
  await expect(host.locator('#rtc')).not.toBeChecked();
  await host.getByRole('button', { name: 'Start' }).click();
  const field = host.locator('#share-link');
  await expect(field).toHaveValue(/#\/J\//, { timeout: 30_000 });
  const link = await field.inputValue();

  // The joiner even accepts the direct lane: the presenter did not offer it, so none opens.
  const tab = await countingPage(await launch('chromium'), 'chromium tab');
  await tab.goto(e2eUrl(link));
  await contribute(tab, 2);

  await expect(host.getByText('64 of 64 tasks')).toBeVisible();
  await expect(host.getByText('Done. Every task has a result.')).toBeVisible();
  await expect(stat(host, 'Mismatches')).toHaveText('0');
  await expect(host.locator('dl.stats dt', { hasText: 'Results over direct connections' })).toHaveCount(0);
  for (const p of [host, tab]) {
    expect(await p.evaluate(() => /** @type {Record<string, number>} */ (/** @type {unknown} */ (window)).pcCount)).toBe(0);
  }
  h.checkPages();
});
