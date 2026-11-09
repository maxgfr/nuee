// A site named on a group, delivered from any static host: the loader on one origin, the Nuée
// package on another, the content from the group's machines.
import { createServer } from 'node:http';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { test, expect } from '@playwright/test';
import { Harness } from './helpers.mjs';

/** @type {Harness} */
let h;
test.beforeEach(() => {
  h = new Harness();
});
/** @type {import('node:http').Server | undefined} */
let host;
test.afterEach(async () => {
  host?.closeAllConnections();
  host?.close();
  await h.close();
});

/** The site's files for a version. @param {string} dir @param {string} version */
function writeSite(dir, version) {
  mkdirSync(dir, { recursive: true });
  writeFileSync(path.join(dir, 'index.html'), `<!doctype html><title>${version}</title><h1>${version}</h1><p id="js">no script</p><a href="page2.html">next</a><script src="app.js"></script>`);
  writeFileSync(path.join(dir, 'page2.html'), '<!doctype html><title>second</title><h1>second page</h1>');
  writeFileSync(path.join(dir, 'app.js'), 'document.getElementById("js").textContent = "ran on " + location.origin;');
}

/** A static host for `dir`, on its own origin. @param {string} dir @returns {Promise<string>} */
function serve(dir) {
  const types = { '.html': 'text/html', '.js': 'text/javascript' };
  const s = createServer((req, res) => {
    const name = new URL(req.url ?? '/', 'http://x').pathname.replace(/\/$/, '/index.html');
    try {
      const body = readFileSync(path.join(dir, path.basename(name)));
      res.writeHead(200, { 'content-type': types[/** @type {'.html'} */ (path.extname(name))] ?? 'text/plain' });
      res.end(body);
    } catch {
      res.writeHead(name === '/favicon.ico' ? 204 : 404);
      res.end();
    }
  });
  host = s;
  return new Promise((resolve, reject) => {
    s.once('error', reject);
    s.listen(0, '127.0.0.1', () => {
      const addr = s.address();
      if (addr === null || typeof addr === 'string') reject(new Error('no port'));
      else resolve(`http://127.0.0.1:${addr.port}/`);
    });
  });
}

for (const engine of /** @type {const} */ (['chromium', 'firefox', 'webkit'])) {
  test(`${engine}: a named site runs from a static host and follows its name`, async () => {
    const dir = h.dir();
    const create = h.cli(['--json', 'group', 'create', 'site'], { dir });
    const { link: group } = await create.event('link');
    expect(await create.exit(60_000), create.stderr).toBe(0);
    const up = h.cli(['--json', 'group', 'up', '-', '--slots', '1'], { dir, stdin: `${group}\n` });
    await up.event('anchor');

    const site = path.join(dir, 'public');
    const publish = async (/** @type {string} */ version) => {
      writeSite(site, version);
      const p = h.cli(['--json', 'site', 'publish', site, '--on', '-', '--name', 'demo'], { dir, stdin: `${group}\n` });
      const { link } = await p.event('link');
      expect(await p.exit(60_000), p.stderr).toBe(0);
      return link;
    };

    const name = await publish('version one');
    const files = path.join(dir, 'loader');
    const loader = h.cli(['--json', 'site', 'loader', '-', '-o', files], { dir, stdin: `${name}\n` });
    expect(await loader.exit(60_000), loader.stderr).toBe(0);
    const origin = await serve(files);

    const browser = await h.browser(engine);
    const page = await h.page(browser, engine);
    await page.goto(origin);
    await expect(page.locator('h1')).toHaveText('version one', { timeout: 90_000 });
    // The site's own script ran, on the static host's origin.
    await expect(page.locator('#js')).toHaveText(`ran on ${origin.replace(/\/$/, '')}`);
    await page.locator('a').click();
    await expect(page.locator('h1')).toHaveText('second page');

    // A new version under the same name: the link does not change, the next visit shows it.
    expect(await publish('version two')).toBe(name);
    await page.goto(origin);
    await page.evaluate(() => {
      localStorage.clear();
    });
    await page.goto(`${origin}page2.html`);
    const stored = () =>
      page.evaluate(async () => {
        const names = (await caches.keys()).filter((k) => k.startsWith('nuee-site-'));
        const pages = await Promise.all(names.map(async (n) => (await caches.open(n)).match(new URL('index.html', location.href))));
        return (await Promise.all(pages.map((r) => r?.text()))).join();
      });
    await expect.poll(stored, { timeout: 60_000 }).toContain('version two');
    await page.goto(origin);
    await expect(page.locator('h1')).toHaveText('version two');

    // Every machine gone, the visitor still has the site.
    await up.stop();
    await page.reload();
    await expect(page.locator('h1')).toHaveText('version two');
    h.checkPages();
  });
}
