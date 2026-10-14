// Files both ways: a CLI share opened in WebKit, and a Chromium share fetched by the CLI.
import { mkdirSync, readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs';
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

/** Deterministic test bytes. @param {number} n @param {number} seed */
function bytes(n, seed) {
  const b = Buffer.alloc(n);
  let x = seed >>> 0;
  for (let i = 0; i < n; i += 1) {
    x = (Math.imul(x, 1664525) + 1013904223) >>> 0;
    b[i] = x >>> 24;
  }
  return b;
}

/** Every file under `dir`, by path relative to it (with `/`). @param {string} dir */
function walk(dir, prefix = '') {
  /** @type {Map<string, Buffer>} */
  const out = new Map();
  for (const name of readdirSync(dir)) {
    const full = path.join(dir, name);
    const rel = prefix ? `${prefix}/${name}` : name;
    if (statSync(full).isDirectory()) for (const [k, v] of walk(full, rel)) out.set(k, v);
    else out.set(rel, readFileSync(full));
  }
  return out;
}

test('a CLI share opens in WebKit: verified and byte-identical', async () => {
  const src = path.join(h.dir(), 'dossier');
  mkdirSync(path.join(src, 'sous'), { recursive: true });
  const want = new Map([
    ['a.txt', Buffer.from('bonjour, nuée\n')],
    ['sous/b.bin', bytes(300_000, 7)],
  ]);
  for (const [rel, b] of want) writeFileSync(path.join(src, rel), b);

  const share = h.cli(['--json', 'share', src]);
  const linkEvent = await share.event('link');
  expect(linkEvent.kind).toBe('file');
  expect(linkEvent.link.startsWith(services().web)).toBe(true);

  const page = await h.page(await h.browser('webkit'), 'webkit');
  await page.goto(pageUrl(linkEvent.link));
  await page.getByRole('button', { name: 'Download' }).click();
  const badge = page.locator('#main .badge.ok');
  await expect(badge).toHaveText('✓ Verified: the files match the link', { timeout: 60_000 });

  const items = page.locator('ul.files li');
  await expect(items).toHaveCount(want.size);
  /** @type {Map<string, Buffer>} */
  const got = new Map();
  for (let i = 0; i < want.size; i += 1) {
    const item = items.nth(i);
    const name = (await item.locator('.mono').textContent()) ?? '';
    const [download] = await Promise.all([page.waitForEvent('download'), item.getByRole('button').click()]);
    const file = await download.path();
    got.set(name.replace(/^dossier\//, ''), readFileSync(file));
  }
  expect([...got.keys()].sort()).toEqual([...want.keys()].sort());
  for (const [rel, b] of want) expect(Buffer.compare(/** @type {Buffer} */ (got.get(rel)), b), rel).toBe(0);
  h.checkPages();
});

test('a Chromium share is fetched by the CLI (link on stdin): identical bytes', async () => {
  const files = [
    { name: 'notes.txt', mimeType: 'text/plain', buffer: Buffer.from('deux fichiers, un lien\n') },
    { name: 'data.bin', mimeType: 'application/octet-stream', buffer: bytes(1_500_000, 42) },
  ];
  const page = await h.page(await h.browser('chromium'), 'chromium');
  await page.goto(pageUrl('#/share'));
  await page.locator('#share-files').setInputFiles(files);
  await page.getByRole('button', { name: 'Create the link' }).click();
  const field = page.locator('#share-link');
  await expect(field).toHaveValue(/#\/F\//, { timeout: 30_000 });
  const link = await field.inputValue();

  const dest = path.join(h.dir(), 'dl');
  const get = h.cli(['get', '-', '-o', dest], { stdin: `${link}\n` });
  expect(await get.exit(2 * 60_000), get.stderr).toBe(0);
  expect(get.lines.join('\n')).toContain('verified (BLAKE3)');

  const got = walk(dest);
  expect([...got.keys()].sort()).toEqual(files.map((f) => f.name).sort());
  for (const f of files) expect(Buffer.compare(/** @type {Buffer} */ (got.get(f.name)), f.buffer), f.name).toBe(0);
  h.checkPages();
});
