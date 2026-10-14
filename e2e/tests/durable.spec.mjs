// Durable storage: a coordinator and three `join --store` members keep two copies of a share.
// When one holder dies, the coordinator places a new copy on the third member once the
// (shortened) absence delay has passed, and the file still downloads in a browser tab and with
// `nuee get`, with the sharer gone. A `share --erasure 4+2` set link opens in a tab too.
import { existsSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { test, expect } from '@playwright/test';
import { Harness, pageUrl } from './helpers.mjs';

/** Absence delay of the test coordinator (NUEE_DURABLE_ABSENT_MS); the default is 10 minutes. */
const ABSENT_MS = 3_000;

/** @type {Harness} */
let h;
test.beforeEach(() => {
  h = new Harness();
});
test.afterEach(async () => {
  await h.close();
  delete process.env.NUEE_DURABLE_ABSENT_MS;
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

/** The identity of a node directory. @param {string} dir */
async function idOf(dir) {
  const id = h.cli(['--json', 'id'], { dir });
  expect(await id.exit(30_000), id.stderr).toBe(0);
  return /** @type {string} */ ((await id.event('id')).id);
}

/** Whether the store member of `dir` lists blob `hash` among its copies. @param {string} dir @param {string} hash */
function stores(dir, hash) {
  const file = path.join(dir, 'data', 'stored.json');
  return existsSync(file) && readFileSync(file, 'utf8').includes(hash);
}

test('a lost holder is replaced; the file still downloads in a tab and with the CLI', async () => {
  // Only the coordinator reads it; the members and the sharer keep the defaults.
  process.env.NUEE_DURABLE_ABSENT_MS = String(ABSENT_MS);
  const coordinator = h.cli(['--json', 'run', 'demo:mandelbrot', '--local-slots', '0', '--wait-peers', '16']);
  delete process.env.NUEE_DURABLE_ABSENT_MS;
  const { link: swarm } = await coordinator.event('link');

  /** @type {Array<{ dir: string, id: string, proc: import('./helpers.mjs').Cli }>} */
  const members = [];
  for (let i = 0; i < 3; i += 1) {
    const dir = h.dir();
    const id = await idOf(dir);
    const proc = h.cli(['--json', 'join', '-', '--slots', '1', '--store', '64MiB'], { stdin: `${swarm}\n`, dir });
    await proc.event('welcome');
    members.push({ dir, id, proc });
  }

  const want = bytes(700_000, 11);
  const src = path.join(h.dir(), 'archive.bin');
  writeFileSync(src, want);
  const share = h.cli(['--json', 'share', src, '--replicas', '2', '--on', '-'], { stdin: `${swarm}\n` });
  const durable = await share.event('durable', 2 * 60_000);
  expect(durable.placed).toBe(true);
  expect(durable.holders).toHaveLength(2);
  const durableLink = (await waitLink(share, 'durable')).link;
  for (const id of durable.holders) expect(members.map((m) => m.id)).toContain(id);
  const holders = members.filter((m) => durable.holders.includes(m.id));
  const spare = /** @type {typeof members[number]} */ (members.find((m) => !durable.holders.includes(m.id)));
  for (const m of holders) expect(stores(m.dir, durable.hash), `holder ${m.id}`).toBe(true);
  expect(stores(spare.dir, durable.hash)).toBe(false);

  // One holder dies without a word, and the sharer leaves.
  const [lost, kept] = /** @type {[typeof members[number], typeof members[number]]} */ (holders);
  lost.proc.child.kill('SIGKILL');
  await lost.proc.exited;
  await share.stop();

  // After the absence delay the coordinator places a new copy on the spare member.
  await expect.poll(() => stores(spare.dir, durable.hash), { timeout: 90_000, intervals: [500] }).toBe(true);
  expect(kept.proc.child.exitCode).toBeNull();

  // A tab opens the durable link: the remaining holder serves it.
  const page = await h.page(await h.browser('chromium'), 'chromium');
  await page.goto(pageUrl(durableLink));
  await page.getByRole('button', { name: 'Download' }).click();
  await expect(page.locator('#main .badge.ok')).toHaveText('✓ Verified: the files match the link', { timeout: 60_000 });
  const [download] = await Promise.all([page.waitForEvent('download'), page.locator('ul.files li').first().getByRole('button').click()]);
  expect(Buffer.compare(readFileSync(await download.path()), want)).toBe(0);

  // The CLI asks the coordinator for the current holders.
  const dest = path.join(h.dir(), 'dl');
  const get = h.cli(['get', durableLink, '--on', swarm, '-o', dest]);
  expect(await get.exit(2 * 60_000), get.stderr).toBe(0);
  expect(get.lines.join('\n')).toContain('verified (BLAKE3)');
  const files = readdirSync(dest);
  expect(files).toEqual(['archive.bin']);
  expect(Buffer.compare(readFileSync(path.join(dest, 'archive.bin')), want)).toBe(0);
  h.checkPages();
});

test('a tab rebuilds an erasure-set link from the shards, with a shard holder gone', async () => {
  const coordinator = h.cli(['--json', 'run', 'demo:mandelbrot', '--local-slots', '0', '--wait-peers', '16']);
  const { link: swarm } = await coordinator.event('link');

  /** @type {Array<{ dir: string, id: string, proc: import('./helpers.mjs').Cli }>} */
  const members = [];
  // Seven members for six shards, as in nuee-store's erasure test: one spare.
  for (let i = 0; i < 7; i += 1) {
    const dir = h.dir();
    const id = await idOf(dir);
    const proc = h.cli(['--json', 'join', '-', '--slots', '1', '--store', '64MiB'], { stdin: `${swarm}\n`, dir });
    await proc.event('welcome');
    members.push({ dir, id, proc });
  }

  const want = bytes(900_000, 23);
  const src = path.join(h.dir(), 'set.bin');
  writeFileSync(src, want);
  const share = h.cli(['--json', 'share', src, '--erasure', '4+2', '--on', '-'], { stdin: `${swarm}\n` });
  const erasure = await share.event('erasure', 60_000);
  expect([erasure.k, erasure.m]).toEqual([4, 2]);
  const durable = await share.event('durable', 2 * 60_000);
  expect(durable.placed).toBe(true);
  const setLink = (await waitLink(share, 'durable')).link;

  // Six members keep one shard each; the link names four holders (header holders first). A
  // shard holder outside the link dies, and the sharer leaves: the tab still finds four valid
  // shards among the link's providers and rebuilds the ciphertext.
  const keeping = (/** @type {typeof members[number]} */ m) => existsSync(path.join(m.dir, 'data', 'stored.json'));
  await expect.poll(() => members.filter(keeping).length, { timeout: 60_000 }).toBeGreaterThanOrEqual(6);
  const outside = members.filter((m) => keeping(m) && !durable.holders.includes(m.id));
  expect(outside.length).toBeGreaterThan(0);
  const gone = /** @type {typeof members[number]} */ (outside[0]);
  gone.proc.child.kill('SIGKILL');
  await gone.proc.exited;
  await share.stop();

  const page = await h.page(await h.browser('chromium'), 'chromium');
  await page.goto(pageUrl(setLink));
  await page.getByRole('button', { name: 'Download' }).click();
  await expect(page.locator('#main .badge.ok')).toHaveText('✓ Verified: the files match the link', { timeout: 90_000 });
  const [download] = await Promise.all([page.waitForEvent('download'), page.locator('ul.files li').first().getByRole('button').click()]);
  expect(Buffer.compare(readFileSync(await download.path()), want)).toBe(0);
  h.checkPages();
});

/**
 * The `link` event of kind `kind` (a command may print several links).
 * @param {import('./helpers.mjs').Cli} cli @param {string} kind
 */
async function waitLink(cli, kind) {
  const find = () => cli.events().find((e) => e.event === 'link' && e.kind === kind);
  await expect.poll(() => find() !== undefined, { timeout: 10_000, message: `${kind} link from \`nuee ${cli.cmd}\`` }).toBe(true);
  return find();
}
