// Native commands: a sharded JSONL labelling job run by a fake teacher on two CLI workers that
// allow its repository revision (fetched from a local mirror, as on an air-gapped machine).
// The merged output equals one unsharded run of the teacher, and a third CLI worker that
// allows nothing never receives an exec lease.
import { spawnSync } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { test, expect } from '@playwright/test';
import { Harness } from './helpers.mjs';

/** The repository URL of the job; workers map it to the local mirror. */
const REPO = 'https://example.org/nuee-e2e/fake-teacher.git';

/** Reads JSONL from $NUEE_IN, writes one record per line to $NUEE_OUT; uid = line in the input. */
const TEACHER = `import json, os

def logits(text):
    h = 0
    for c in text.encode("utf-8"):
        h = (h * 31 + c) % 1000003
    return [round(((h >> k) % 97) / 10.0 - 4.0, 3) for k in range(4)]

with open(os.environ["NUEE_IN"], encoding="utf-8") as src, open(os.environ["NUEE_OUT"], "w", encoding="utf-8") as dst:
    for uid, line in enumerate(src):
        rec = json.loads(line)
        dst.write(json.dumps({"uid": uid, "id": rec["id"], "tlogits": logits(rec["text"])}, separators=(",", ":")) + "\\n")
`;

/**
 * Runs git with a configuration of its own (no user or system settings leak into the fixture).
 * @param {string} cwd
 * @param {string[]} args
 */
function git(cwd, args) {
  const home = path.join(cwd, '..', 'git-home');
  mkdirSync(home, { recursive: true });
  const r = spawnSync('git', args, {
    cwd,
    encoding: 'utf8',
    env: {
      ...process.env,
      HOME: home,
      GIT_CONFIG_NOSYSTEM: '1',
      GIT_CONFIG_GLOBAL: path.join(home, 'gitconfig'),
      GIT_AUTHOR_NAME: 'e2e',
      GIT_AUTHOR_EMAIL: 'e2e@example.org',
      GIT_COMMITTER_NAME: 'e2e',
      GIT_COMMITTER_EMAIL: 'e2e@example.org',
      GIT_AUTHOR_DATE: '2026-01-01T00:00:00Z',
      GIT_COMMITTER_DATE: '2026-01-01T00:00:00Z',
    },
  });
  if (r.status !== 0) throw new Error(`git ${args.join(' ')} failed: ${r.stderr}`);
  return r.stdout.trim();
}

/** @type {Harness} */
let h;
test.beforeEach(() => {
  h = new Harness();
});
test.afterEach(async () => {
  await h.close();
});

test('sharded JSONL labelling on two allowed CLIs equals an unsharded run; a third gets no exec lease', async () => {
  const base = h.dir();
  const mirror = path.join(base, 'mirror');
  mkdirSync(mirror);
  writeFileSync(path.join(mirror, 'fake-teacher.py'), TEACHER);
  git(mirror, ['init', '-q', '-b', 'main']);
  git(mirror, ['add', 'fake-teacher.py']);
  git(mirror, ['commit', '-q', '-m', 'fake teacher']);
  const rev = git(mirror, ['rev-parse', 'HEAD']);
  expect(rev).toMatch(/^[0-9a-f]{40}$/);

  // 11 records in shards of 3 lines: 4 tasks, the last one shorter.
  const job = path.join(base, 'job');
  mkdirSync(job);
  const records = Array.from({ length: 11 }, (_, i) => JSON.stringify({ id: `r${i}`, text: `decision ${i * 7} ${'é'.repeat(i % 3)}` }));
  writeFileSync(path.join(job, 'queue.jsonl'), `${records.join('\n')}\n`);
  writeFileSync(
    path.join(job, 'label.toml'),
    `name = "fake-label"
kind = "exec"
verify = "full"

[exec]
repo = "${REPO}"
rev = "${rev}"
argv = ["python3", "fake-teacher.py"]
timeout = "2m"

[inputs]
jsonl = "queue.jsonl"
lines_per_task = 3

[output]
merge = "jsonl"
uid_field = "uid"

[compare]
kind = "jsonl"
field = "tlogits"
tol = 0.001
`,
  );

  // The reference: the teacher once over the whole file.
  const reference = path.join(base, 'reference.jsonl');
  const ref = spawnSync('python3', [path.join(mirror, 'fake-teacher.py')], {
    encoding: 'utf8',
    env: { ...process.env, NUEE_IN: path.join(job, 'queue.jsonl'), NUEE_OUT: reference },
  });
  expect(ref.status, ref.stderr).toBe(0);
  const want = readFileSync(reference, 'utf8');
  expect(want.trim().split('\n')).toHaveLength(11);

  const out = path.join(base, 'out');
  const run = h.cli(['--json', 'run', path.join(job, 'label.toml'), '--wait-peers', '4', '--out', out]);
  const { link } = await run.event('link');

  // Pinning repo@rev is not enough: each exact command (repo, rev, argv, env) needs an approval.
  const approvals = path.join(base, 'approvals.jsonl');
  writeFileSync(approvals, `${JSON.stringify({ repo: REPO, rev, argv: ['python3', 'fake-teacher.py'], env: {} })}\n`);
  const other = path.join(base, 'other-approvals.jsonl');
  writeFileSync(other, `${JSON.stringify({ repo: REPO, rev, argv: ['python3', 'fake-teacher.py', '--other'], env: {} })}\n`);
  const pin = ['--allow-exec', `${REPO}@${rev}`, '--exec-mirror', `${REPO}=${mirror}`];
  const allow = [...pin, '--exec-approvals', approvals];
  const a = h.cli(['--json', 'join', '-', '--slots', '1', ...allow], { stdin: `${link}\n` });
  const b = h.cli(['--json', 'join', '-', '--slots', '1', ...allow], { stdin: `${link}\n` });
  // Same pin, but the approved command differs from the job's argv.
  const strictDir = h.dir();
  const strictIdCmd = h.cli(['--json', 'id'], { dir: strictDir });
  expect(await strictIdCmd.exit(30_000), strictIdCmd.stderr).toBe(0);
  const strictId = (await strictIdCmd.event('id')).id;
  const strict = h.cli(['--json', 'join', '-', '--slots', '2', ...pin, '--exec-approvals', other], { stdin: `${link}\n`, dir: strictDir });
  // The third worker's identity, to look for it among the executors.
  const plainDir = h.dir();
  const idCmd = h.cli(['--json', 'id'], { dir: plainDir });
  expect(await idCmd.exit(30_000), idCmd.stderr).toBe(0);
  const plainId = (await idCmd.event('id')).id;
  const plain = h.cli(['--json', 'join', '-', '--slots', '2'], { stdin: `${link}\n`, dir: plainDir });

  expect(await run.exit(3 * 60_000), run.stderr).toBe(0);
  for (const w of [a, b, plain, strict]) expect(await w.exit(60_000), w.stderr).toBe(0);

  const events = run.events();
  const done = events.find((e) => e.event === 'done');
  expect(done).toBeTruthy();
  expect(done.tasks).toBe(4);
  expect(done.banned).toBe(0);
  expect(events.filter((e) => e.event === 'mismatch')).toEqual([]);
  expect(events.filter((e) => e.event === 'task_failed')).toEqual([]);
  expect(typeof done.merged).toBe('string');
  expect(readFileSync(done.merged, 'utf8')).toBe(want);

  // Verified in full: each of the 4 shards ran on both allowed workers.
  const okTasks = (/** @type {import('./helpers.mjs').Cli} */ w) => w.events().filter((e) => e.event === 'task' && e.ok === true);
  expect(okTasks(a).length).toBeGreaterThan(0);
  expect(okTasks(b).length).toBeGreaterThan(0);
  expect(okTasks(a).length + okTasks(b).length).toBeGreaterThanOrEqual(8);

  // The worker without --allow-exec was admitted, saw the job and was never leased a shard.
  expect(plain.events().some((e) => e.event === 'welcome')).toBe(true);
  expect(plain.events().filter((e) => e.event === 'task')).toEqual([]);
  const joined = events.filter((e) => e.event === 'peer' && e.state === 'joined').map((e) => e.id);
  expect(joined).toContain(plainId);
  const results = JSON.parse(readFileSync(path.join(out, 'results.json'), 'utf8'));
  expect(results.outputs).toHaveLength(4);
  for (const o of results.outputs) expect(o.by).not.toBe(plainId);
  for (const e of events) if (e.event === 'verified') expect(e.by).not.toBe(plainId);

  // The worker that pinned the revision but approved another argv was admitted and ran nothing:
  // its pin still advertises the revision, so it may be leased a shard, but it refuses each one
  // before starting the command and none of its results is ever kept.
  expect(strict.events().some((e) => e.event === 'welcome')).toBe(true);
  expect(okTasks(strict)).toEqual([]);
  for (const e of strict.events()) if (e.event === 'task') expect(e.ms).toBe(0);
  // Its first refusal (Unsupported) excludes it from this job: at most the leases already in
  // flight on its two slots before the refusal reached the coordinator.
  expect(strict.events().filter((e) => e.event === 'task').length).toBeLessThanOrEqual(2);
  expect(joined).toContain(strictId);
  for (const o of results.outputs) expect(o.by).not.toBe(strictId);
  for (const e of events) if (e.event === 'verified') expect(e.by).not.toBe(strictId);
});
