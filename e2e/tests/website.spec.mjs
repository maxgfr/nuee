// Websites, end to end in every engine. A static site (pages in subfolders, a non-ASCII file
// name, CSS, a script, a PNG) handed to an anchor's gateway renders in a real tab: styles
// apply, the script runs, the image decodes and links navigate; the same site previews in the
// app's isolated frame. A dynamic Node app exposed with `nuee expose` and opened through
// `nuee connect` works in a normal tab: form POST, fetch() of JSON and cookies.
import { createHash } from 'node:crypto';
import { createServer } from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { test, expect } from '@playwright/test';
import { Harness, pageUrl } from './helpers.mjs';

const SITE = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'fixtures', 'site');

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
    s.closeAllConnections();
    await new Promise((resolve) => s.close(resolve));
  }
});

/** Resolves with the first line of `cli`'s stdout matching `re` (its first group). */
async function stdoutMatch(/** @type {import('./helpers.mjs').Cli} */ cli, /** @type {RegExp} */ re, ms = 90_000) {
  const deadline = Date.now() + ms;
  for (;;) {
    for (const l of cli.lines) {
      const m = re.exec(l);
      if (m) return m[1];
    }
    if (cli.child.exitCode !== null && cli.child.exitCode !== 0) throw new Error(`\`nuee ${cli.cmd}\` failed (${cli.child.exitCode})\n${cli.stderr}`);
    if (Date.now() > deadline) throw new Error(`no line matching ${re} from \`nuee ${cli.cmd}\`\n${cli.stderr}`);
    await new Promise((r) => setTimeout(r, 100));
  }
}

const APP_PAGE = `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Tiny app</title></head>
<body>
<h1>Tiny app</h1>
<form method="post" action="/submit">
  <input id="name" name="name">
  <textarea id="big" name="big"></textarea>
  <button id="send" type="submit">Send</button>
</form>
<button id="load" type="button">Load data</button>
<pre id="data"></pre>
<script>
  document.getElementById('load').addEventListener('click', async () => {
    const r = await fetch('/api/data?x=1', { headers: { 'x-from': 'page' } });
    document.getElementById('data').textContent = r.headers.get('content-type') + ' ' + JSON.stringify(await r.json());
  });
</script>
</body></html>
`;

/** A tiny dynamic app on 127.0.0.1: a page with a form, a form handler, a JSON endpoint and a cookie. */
function startApp() {
  const s = createServer((req, res) => {
    const chunks = /** @type {Buffer[]} */ ([]);
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      const body = Buffer.concat(chunks);
      const cookie = req.headers.cookie ?? '';
      if (req.method === 'GET' && req.url === '/') {
        res.writeHead(200, { 'content-type': 'text/html; charset=utf-8', 'set-cookie': 'session=s3cr3t; Path=/; SameSite=Lax' });
        res.end(APP_PAGE);
      } else if (req.method === 'POST' && req.url === '/submit') {
        const form = new URLSearchParams(body.toString('utf8'));
        const big = form.get('big') ?? '';
        const sum = createHash('sha256').update(big).digest('hex');
        res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
        res.end(
          `<!doctype html><html><body><p id="answer">Hello ${form.get('name')}</p>` +
            `<p id="big">${big.length} ${sum}</p><p id="cookie">${cookie}</p></body></html>`,
        );
      } else if (req.method === 'GET' && req.url === '/api/data?x=1') {
        res.writeHead(200, { 'content-type': 'application/json' });
        res.end(JSON.stringify({ ok: true, from: req.headers['x-from'], cookie }));
      } else {
        res.writeHead(404, { 'content-type': 'text/plain' });
        res.end('not found');
      }
    });
  });
  server = s;
  return /** @type {Promise<number>} */ (
    new Promise((resolve, reject) => {
      s.once('error', reject);
      s.listen(0, '127.0.0.1', () => {
        const addr = s.address();
        if (addr === null || typeof addr === 'string') reject(new Error('no port'));
        else resolve(addr.port);
      });
    })
  );
}

for (const engine of /** @type {const} */ (['chromium', 'firefox', 'webkit'])) {
  test(`${engine}: a static site renders through the gateway and in the preview`, async () => {
    const anchor = h.cli(['--json', 'serve', '--anchor', '--relay-dev', '--relay-bind', '127.0.0.1:0', '--local-slots', '0', '--gateway', '127.0.0.1:0', '--gateway-scripts']);
    const started = await anchor.event('anchor');
    expect(started.gateway).toMatch(/^http:\/\/127\.0\.0\.1:\d+$/);

    const handed = h.cli(['site', 'publish', SITE, '--on', '-'], { stdin: `${started.link}\n` });
    const sitePath = await stdoutMatch(handed, /served by the anchor's gateway at (\/s\/[a-z0-9-]+\/)/);
    expect(await handed.exit(60_000), handed.stderr).toBe(0);
    const base = `${started.gateway}${sitePath}`;

    const browser = await h.browser(engine);
    const page = await h.page(browser, engine);
    const first = await page.goto(base);
    expect(first?.status()).toBe(200);
    const headers = (await first?.allHeaders()) ?? {};
    expect(headers['content-type']).toBe('text/html; charset=utf-8');
    expect(headers['content-security-policy']).toBe('sandbox allow-scripts allow-forms allow-popups');
    expect(headers['x-content-type-options']).toBe('nosniff');

    await expect(page.locator('#title')).toHaveText('A site served by Nuée');
    await expect(page.locator('#title')).toHaveCSS('color', 'rgb(12, 34, 56)');
    await expect(page.locator('#js')).toHaveText('script ran');
    // Sandboxed without allow-same-origin: the page runs in an opaque origin.
    expect(await page.evaluate(() => origin)).toBe('null');
    await expect.poll(() => page.locator('#pixel').evaluate((/** @type {HTMLImageElement} */ img) => img.complete && img.naturalWidth)).toBe(4);
    expect(await page.locator('#pixel').evaluate((/** @type {HTMLImageElement} */ img) => img.naturalHeight)).toBe(3);

    await page.locator('#next').click();
    await expect(page.locator('#title')).toHaveText('Une page d’été');
    expect(decodeURIComponent(new URL(page.url()).pathname)).toBe(`${sitePath}pages/été.html`);
    await expect(page.locator('body')).toHaveCSS('background-color', 'rgb(250, 240, 230)');
    await page.locator('#home').click();
    await expect(page.locator('#js')).toHaveText('script ran');

    // The same folder published on its own previews in the app, inside the isolated frame.
    const publish = h.cli(['--json', 'site', 'publish', SITE]);
    const linkEvent = await publish.event('link');
    expect(linkEvent.kind).toBe('site');
    const app = await h.page(browser, `${engine} app`);
    await app.goto(pageUrl(linkEvent.link));
    await app.getByRole('button', { name: 'Download and preview' }).click();
    await expect(app.getByText('Verified: the files match the link')).toBeVisible({ timeout: 60_000 });
    const frameEl = app.locator('iframe.preview-frame');
    await expect(frameEl).toHaveCount(1);
    expect(await frameEl.getAttribute('sandbox')).toBe('');
    const frame = await (await frameEl.elementHandle())?.contentFrame();
    if (!frame) throw new Error('no preview frame');
    await expect(frame.locator('#title')).toHaveText('A site served by Nuée');
    await expect(frame.locator('#title')).toHaveCSS('color', 'rgb(12, 34, 56)');
    // No script runs in the preview.
    await expect(frame.locator('#js')).toHaveText('static');
    h.checkPages();
  });

  test(`${engine}: a dynamic app works through expose and connect`, async () => {
    const port = await startApp();
    const expose = h.cli(['--json', 'expose', String(port)]);
    const linkEvent = await expose.event('link');
    expect(linkEvent.kind).toBe('service');
    const connect = h.cli(['--json', 'connect', '-'], { stdin: `${linkEvent.link}\n` });
    const listening = await connect.event('listening');
    const local = `http://localhost:${listening.port}`;

    const page = await h.page(await h.browser(engine), engine);
    const first = await page.goto(`${local}/`);
    expect(first?.status()).toBe(200);
    // connect is a raw TCP tunnel: the service's Set-Cookie reaches the browser untouched.
    expect((await first?.allHeaders())?.['set-cookie']).toBe('session=s3cr3t; Path=/; SameSite=Lax');
    await expect(page.locator('h1')).toHaveText('Tiny app');

    const [data] = await Promise.all([page.waitForResponse(`${local}/api/data?x=1`), page.locator('#load').click()]);
    expect(data.status()).toBe(200);
    await expect(page.locator('#data')).toHaveText('application/json {"ok":true,"from":"page","cookie":"session=s3cr3t"}');

    // A form POST with a body of several hundred kilobytes arrives whole.
    const big = 'nuée-'.repeat(60_000);
    const bigSum = createHash('sha256').update(big).digest('hex');
    await page.locator('#name').fill('Zoé');
    await page.locator('#big').fill(big);
    await page.locator('#send').click();
    await expect(page.locator('#answer')).toHaveText('Hello Zoé', { timeout: 60_000 });
    await expect(page.locator('#big')).toHaveText(`${big.length} ${bigSum}`);
    await expect(page.locator('#cookie')).toHaveText('session=s3cr3t');
    h.checkPages();
  });
}
