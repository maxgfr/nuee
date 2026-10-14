// Hosting: a local JSON service exposed with `nuee expose` answers GET and POST from the page's
// console (#/X/…), and a site published with `nuee site publish` renders in the page's isolated
// preview (#/S/…): an empty sandbox, none of the site's scripts or handlers ever run, and the
// preview cannot reach the page's storage.
import { createServer } from 'node:http';
import { mkdirSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { test, expect } from '@playwright/test';
import { Harness, pageUrl } from './helpers.mjs';

/** @type {Harness} */
let h;
/** @type {import('node:http').Server | null} */
let server = null;
test.beforeEach(() => {
  h = new Harness();
});
test.afterEach(async () => {
  await h.close();
  if (server) {
    const s = server;
    server = null;
    await new Promise((resolve) => s.close(resolve));
  }
});

/** A tiny JSON service on 127.0.0.1: GET /hello, POST /echo. @returns {Promise<number>} its port */
function startService() {
  const s = createServer((req, res) => {
    const chunks = /** @type {Buffer[]} */ ([]);
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      const reply = (/** @type {number} */ status, /** @type {unknown} */ doc) => {
        res.writeHead(status, { 'content-type': 'application/json' });
        res.end(JSON.stringify(doc));
      };
      if (req.method === 'GET' && req.url === '/hello') reply(200, { hello: 'nuée', n: 42 });
      else if (req.method === 'POST' && req.url === '/echo') {
        reply(201, { method: req.method, type: req.headers['content-type'], got: JSON.parse(Buffer.concat(chunks).toString('utf8')) });
      } else reply(404, { error: 'not found' });
    });
  });
  server = s;
  return new Promise((resolve, reject) => {
    s.once('error', reject);
    s.listen(0, '127.0.0.1', () => {
      const addr = s.address();
      if (addr === null || typeof addr === 'string') reject(new Error('no port'));
      else resolve(addr.port);
    });
  });
}

/** The pretty-printed body of the console's last response. @param {import('@playwright/test').Page} page */
const responseBody = (page) => page.locator('.response > div > pre.output-text');

test('an exposed JSON service answers GET and POST from the page console', async () => {
  const port = await startService();
  const expose = h.cli(['--json', 'expose', String(port)]);
  const linkEvent = await expose.event('link');
  expect(linkEvent.kind).toBe('service');
  expect(linkEvent.link).toMatch(/#\/X\//);

  const page = await h.page(await h.browser('chromium'), 'chromium');
  await page.goto(pageUrl(linkEvent.link));
  await expect(page.locator('.console .fingerprint .emoji')).toHaveText(linkEvent.fingerprint.join(' '), { timeout: 30_000 });

  await page.locator('#svc-method').selectOption('GET');
  await page.locator('#svc-path').fill('/hello');
  await page.getByRole('button', { name: 'Send' }).click();
  await expect(page.locator('.response .badge.ok')).toHaveText(/^Status 200, \d+ ms$/, { timeout: 60_000 });
  expect(JSON.parse((await responseBody(page).textContent()) ?? '')).toEqual({ hello: 'nuée', n: 42 });

  await page.locator('#svc-method').selectOption('POST');
  await page.locator('#svc-path').fill('/echo');
  await page.locator('#svc-body').fill('{"answer": [1, 2, 3]}');
  await page.getByRole('button', { name: 'Send' }).click();
  await expect(page.locator('.response .badge.ok')).toHaveText(/^Status 201, \d+ ms$/, { timeout: 60_000 });
  expect(JSON.parse((await responseBody(page).textContent()) ?? '')).toEqual({ method: 'POST', type: 'application/json', got: { answer: [1, 2, 3] } });
  h.checkPages();
});

test('a published site renders in an isolated preview where nothing of it runs', async () => {
  const site = path.join(h.dir(), 'site');
  mkdirSync(site);
  writeFileSync(path.join(site, 'style.css'), 'h1 { color: rgb(1, 2, 3); }\n');
  writeFileSync(
    path.join(site, 'index.html'),
    `<!doctype html>
<html><head><title>A shared site</title><link rel="stylesheet" href="style.css">
<script>
  document.title = 'script ran';
  try { parent.nueeSiteRan = true; } catch (e) {}
  try { document.getElementById('marker').textContent = 'stolen: ' + localStorage.getItem('sentinel'); } catch (e) {}
</script></head>
<body>
<h1 id="title">Bonjour from a site</h1>
<p id="marker">untouched</p>
<img id="pixel" src="missing.png" onerror="document.getElementById('marker').textContent = 'handler ran'; parent.nueeSiteRan = true">
<a id="go" href="javascript:parent.nueeSiteRan=true">a link</a>
</body></html>
`,
  );
  const publish = h.cli(['--json', 'site', 'publish', site]);
  const linkEvent = await publish.event('link');
  expect(linkEvent.kind).toBe('site');
  expect(linkEvent.link).toMatch(/#\/S\//);

  const page = await h.page(await h.browser('chromium'), 'chromium');
  // A sentinel in the page origin's storage, set before the app starts (init scripts also run
  // in frames: only the top document has storage to set).
  await page.addInitScript(() => {
    if (window.top !== window) return;
    localStorage.setItem('sentinel', 'page-secret');
  });
  await page.goto(pageUrl(linkEvent.link));
  await page.getByRole('button', { name: 'Download and preview' }).click();
  await expect(page.getByText('Verified: the files match the link')).toBeVisible({ timeout: 60_000 });

  const frameEl = page.locator('iframe.preview-frame');
  await expect(frameEl).toHaveCount(1);
  expect(await frameEl.getAttribute('sandbox')).toBe('');
  const frame = await (await frameEl.elementHandle())?.contentFrame();
  expect(frame).toBeTruthy();
  if (!frame) return;
  await expect(frame.locator('#title')).toHaveText('Bonjour from a site');
  // The site's own stylesheet was inlined into the preview.
  await expect(frame.locator('#title')).toHaveCSS('color', 'rgb(1, 2, 3)');

  // Neither the script nor the handlers ran: the marker is untouched, nothing reached the page.
  await expect(frame.locator('#marker')).toHaveText('untouched');
  await frame.locator('#go').click();
  expect(await frame.locator('script').count()).toBe(0);
  expect(await frame.locator('[onerror]').count()).toBe(0);
  expect(await frame.evaluate(() => document.title)).toBe('A shared site');
  expect(await page.evaluate(() => /** @type {Record<string, unknown>} */ (/** @type {unknown} */ (window)).nueeSiteRan)).toBeUndefined();

  // The preview has an opaque origin: the page's storage is out of its reach.
  const fromPreview = await frame.evaluate(() => {
    try {
      return `read ${localStorage.getItem('sentinel')}`;
    } catch (e) {
      return e instanceof Error ? e.name : String(e);
    }
  });
  expect(fromPreview).toBe('SecurityError');
  expect(await frame.evaluate(() => origin)).toBe('null');
  expect(await page.evaluate(() => localStorage.getItem('sentinel'))).toBe('page-secret');
  await expect(frame.locator('#marker')).toHaveText('untouched');
  // No console error and no CSP violation from the page itself.
  h.checkPages();
});
