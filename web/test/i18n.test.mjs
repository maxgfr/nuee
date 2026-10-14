// @ts-check
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { STRINGS, t } from '../app/i18n.js';

const app = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../app');

/** @param {string} dir @returns {Promise<string[]>} */
async function jsFiles(dir) {
  const out = [];
  for (const e of await readdir(dir, { withFileTypes: true })) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) out.push(...(await jsFiles(p)));
    else if (e.name.endsWith('.js')) out.push(p);
  }
  return out;
}

test('no string is empty', () => {
  for (const [k, v] of Object.entries(STRINGS)) assert.ok(v.trim().length > 0, `${k} is empty`);
});

test('every error code has a message', () => {
  const codes = ['relay_unreachable', 'swarm_ended', 'link_truncated', 'link_version', 'link_corrupt', 'link_kind', 'admission_refused', 'integrity', 'decrypt', 'too_large', 'unsupported', 'invalid_module', 'invalid_argument', 'internal'];
  for (const c of codes) assert.ok(STRINGS[`err.${c}`], c);
});

test('every literal key used by the app exists', async () => {
  const used = new Set();
  const pattern = /(?:i18n|showKey\(|setText\([\w.]+,|\bt\(|alertBox\(|progressBar\(|fingerprintBlock\([^,()]+,|qrImage\([^,()]+,|labelKey\s*=|:\s*)\s*'([a-z][\w]*(?:\.[\w]+)+)'/g;
  for (const f of await jsFiles(app)) {
    const src = await readFile(f, 'utf8');
    for (const m of src.matchAll(pattern)) if (m[1]) used.add(m[1]);
  }
  const html = await readFile(path.join(app, '..', 'index.html'), 'utf8');
  for (const m of html.matchAll(/data-i18n="([^"]+)"/g)) if (m[1]) used.add(m[1]);
  for (const m of html.matchAll(/data-i18n-attr="[\w-]+:([^"]+)"/g)) if (m[1]) used.add(m[1]);
  assert.ok(used.size > 50, `only ${used.size} keys found`);
  const missing = [...used].filter((k) => !(k in STRINGS) && !/^(api|lib|app|views|worker|pkg|tasks)\./.test(k));
  assert.deepEqual(missing, []);
});

test('interpolation', () => {
  assert.equal(t('join.coresValue', { n: 2, total: 8 }), '2 of 8');
});
