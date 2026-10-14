// Embeddings (opt-in: NUEE_E2E_GPU=1, downloads the models from huggingface.co): a CLI
// coordinator (candle) and a Chromium tab (Transformers.js) embed a folder of texts. Every
// result of the tab is recomputed by the coordinator and passes the cosine check, and
// vectors.f32 holds N x 384 floats, one row per meta.jsonl line.
import { mkdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { test, expect } from '@playwright/test';
import { Harness, pageUrl } from './helpers.mjs';

const DIM = 384;

const SUBJECTS = ['The swarm', 'Une nuée', 'A browser tab', 'The coordinator', 'Le relais', 'A small model', 'Une ancre', 'The relay', 'Rust code', 'Un fichier partagé'];
const VERBS = ['shares work with', 'vérifie', 'waits for', 'encrypts', 'compares', 'sends a result to', 'garde', 'computes tiles for', 'ranks', 'oublie'];
const OBJECTS = ['phones and laptops.', 'les résultats d’un voisin.', 'a GPU in the next room.', 'des vecteurs normalisés.', 'every sampled task.', 'a sleepy server.'];
/** 200 short sentences in English and French: four tasks of up to 64 texts. */
const TEXTS = Array.from({ length: 200 }, (_, i) => `${SUBJECTS[i % 10]} ${VERBS[Math.floor(i / 10) % 10]} ${OBJECTS[i % 6]} (${i})`);

/** @type {Harness} */
let h;

test.describe('embeddings', () => {
  test.skip(!process.env.NUEE_E2E_GPU, 'set NUEE_E2E_GPU=1 to run GPU e2e');

  test.beforeEach(() => {
    h = new Harness();
  });
  test.afterEach(async () => {
    await h.close();
  });

  test('candle on the CLI and Transformers.js in Chromium agree on every checked text', async () => {
    // The first run downloads both models.
    test.setTimeout(20 * 60_000);
    const texts = path.join(h.dir(), 'texts');
    mkdirSync(texts);
    for (const [i, t] of TEXTS.entries()) writeFileSync(path.join(texts, `t${String(i).padStart(3, '0')}.txt`), `${t}\n`);
    const out = path.join(h.dir(), 'out');
    // Every result a peer sends is recomputed here (candle) and compared by cosine.
    const run = h.cli(['--json', 'embed', texts, '--verify', 'sample', '--sample', '1', '--local-slots', '1', '--wait-peers', '1', '--out', out]);
    const { link } = await run.event('link', 10 * 60_000);

    const tab = await h.page(await h.browser('chromium'), 'chromium');
    await tab.goto(pageUrl(link));
    await tab.getByRole('button', { name: 'Contribute' }).click();

    expect(await run.exit(15 * 60_000), run.stderr).toBe(0);
    const events = run.events();
    const done = events.find((e) => e.event === 'done');
    expect(done).toBeTruthy();
    expect(done.tasks).toBe(4);
    expect(done.banned).toBe(0);
    expect(events.filter((e) => e.event === 'mismatch')).toEqual([]);
    expect(events.filter((e) => e.event === 'banned')).toEqual([]);
    expect(events.filter((e) => e.event === 'task_failed')).toEqual([]);

    // The tab's results were recomputed with candle and passed the cosine check.
    const tabId = events.find((e) => e.event === 'peer' && e.state === 'joined' && String(e.agent).startsWith('nuee-web/'))?.id;
    expect(tabId).toBeTruthy();
    const checked = events.filter((e) => e.event === 'verified' && e.by === tabId);
    expect(checked.length, 'tab results checked against candle').toBeGreaterThan(0);

    const meta = readFileSync(path.join(out, 'meta.jsonl'), 'utf8').trim().split('\n');
    expect(meta.length).toBeGreaterThanOrEqual(TEXTS.length);
    expect(statSync(path.join(out, 'vectors.f32')).size).toBe(meta.length * DIM * 4);
    // Rows are unit vectors (sentence-transformers normalization).
    const v = new Float32Array(readFileSync(path.join(out, 'vectors.f32')).buffer.slice(0));
    for (let r = 0; r < meta.length; r += 1) {
      let n = 0;
      for (let k = 0; k < DIM; k += 1) n += (v[r * DIM + k] ?? 0) ** 2;
      expect(Math.abs(Math.sqrt(n) - 1), `row ${r} norm`).toBeLessThan(1e-3);
    }
    h.checkPages();
  });
});
