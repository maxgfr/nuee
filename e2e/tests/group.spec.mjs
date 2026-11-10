// A group seen from outside: the page of its link and `nuee group status` show which machines are
// on line, follow one that leaves, and say so when the group is down.
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

test('the group page and group status follow the machines of a group', async () => {
  const alice = h.dir();
  const bob = h.dir();
  const create = h.cli(['--json', 'group', 'create', 'team'], { dir: alice });
  const { link: group } = await create.event('link');
  expect(await create.exit(60_000), create.stderr).toBe(0);
  const id = h.cli(['--json', 'id'], { dir: bob });
  const { id: bobId } = await id.event('id');
  const invite = h.cli(['group', 'invite', '-', bobId, '--name', 'bob'], { dir: alice, stdin: `${group}\n` });
  expect(await invite.exit(60_000), invite.stderr).toBe(0);

  const upAlice = h.cli(['--json', 'group', 'up', '-', '--slots', '1'], { dir: alice, stdin: `${group}\n` });
  await upAlice.event('anchor');
  const upBob = h.cli(['--json', 'group', 'up', '-', '--slots', '3'], { dir: bob, stdin: `${group}\n` });

  const browser = await h.browser('chromium');
  const page = await h.page(browser, 'group');
  await page.goto(pageUrl(group));
  const summary = page.getByRole('status').filter({ hasText: 'Machines on line' });
  await expect(summary).toHaveText('Machines on line: 2 of 2 · Jobs running: 0 · Files kept: 0', { timeout: 90_000 });
  const bobRow = page.getByRole('row', { name: /bob/ });
  await expect(bobRow).toContainText('On line');
  await expect(bobRow).toContainText('3');

  const status = h.cli(['--json', 'group', 'status', '-'], { dir: alice, stdin: `${group}\n` });
  const seen = await status.event('group');
  expect(await status.exit(60_000), status.stderr).toBe(0);
  expect(seen.members.map((/** @type {{ name: string, online: boolean }} */ m) => [m.name, m.online]).sort()).toEqual([
    ['bob', true],
    ['first keeper', true],
  ]);

  // A machine that leaves shows as off line once its connection is gone.
  await upBob.stop();
  await expect(bobRow).toContainText('Off line', { timeout: 120_000 });
  await expect(summary).toContainText('Machines on line: 1 of 2');

  // With no keeper up, the page says so and a monitoring check fails.
  await upAlice.stop();
  await expect(page.getByRole('status').filter({ hasText: 'No keeper' })).toBeVisible({ timeout: 60_000 });
  const check = h.cli(['group', 'status', '-', '--check'], { dir: alice, stdin: `${group}\n` });
  expect(await check.exit(60_000), check.stderr).toBe(3);
  h.checkPages();
});
