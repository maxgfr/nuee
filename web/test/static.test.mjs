// @ts-check
// Source-level guarantees: no HTML injection sinks, type-checked files, the CSP in place.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const web = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

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

test('no HTML injection sinks or dynamic code in web/app', async () => {
  const files = await jsFiles(path.join(web, 'app'));
  assert.ok(files.length >= 15, `found ${files.length} files`);
  const banned = /innerHTML|outerHTML|insertAdjacentHTML|document\.write|eval\(|new Function/;
  for (const f of files) {
    const src = await readFile(f, 'utf8');
    const m = banned.exec(src);
    assert.equal(m, null, `${path.relative(web, f)} uses ${m?.[0]}`);
  }
});

test('no inline styles or storage of secrets', async () => {
  for (const f of await jsFiles(path.join(web, 'app'))) {
    const src = await readFile(f, 'utf8');
    const rel = path.relative(web, f);
    assert.doesNotMatch(src, /\.style\s*=|setAttribute\(\s*'style'|cssText/, `${rel} sets inline style`);
    assert.doesNotMatch(src, /sessionStorage|indexedDB/, `${rel} uses storage`);
    if (!rel.endsWith('i18n.js')) assert.doesNotMatch(src, /localStorage/, `${rel} uses localStorage`);
  }
});

test('every script under web/app starts with // @ts-check', async () => {
  for (const f of await jsFiles(path.join(web, 'app'))) {
    const src = await readFile(f, 'utf8');
    assert.ok(src.startsWith('// @ts-check\n'), path.relative(web, f));
  }
});

test('index.html carries the CSP and no inline script or style', async () => {
  const html = await readFile(path.join(web, 'index.html'), 'utf8');
  const m = /<meta http-equiv="Content-Security-Policy" content="([^"]+)">/.exec(html);
  assert.ok(m && m[1], 'CSP meta missing');
  const csp = m[1];
  for (const d of [
    "default-src 'self'",
    "script-src 'self' 'wasm-unsafe-eval'",
    "worker-src 'self'",
    "object-src 'none'",
    "base-uri 'none'",
    "form-action 'none'",
    "style-src 'self'",
    "require-trusted-types-for 'script'",
    'trusted-types nuee-worker',
  ]) {
    assert.ok(csp.includes(d), `CSP lacks ${d}`);
  }
  assert.ok(csp.includes("style-src-attr 'none'"), 'CSP lacks style-src-attr none');
  const directives = new Map(csp.split(';').map((d) => d.trim().split(/\s+/)).map(([k, ...v]) => [k ?? '', v]));
  // Inline <style> elements only (the sandboxed site preview); nothing else may be inline.
  for (const [k, v] of directives) {
    if (k !== 'style-src-elem') assert.ok(!v.includes("'unsafe-inline'"), `${k} allows 'unsafe-inline'`);
  }
  assert.ok(!(directives.get('script-src') ?? []).includes("'unsafe-inline'"));
  assert.doesNotMatch(csp, /'unsafe-eval'|\s\*(?:\s|;|$)|cdn/i);
  for (const s of html.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script>/g)) {
    assert.match(s[1] ?? '', /\bsrc="/, 'inline script');
    assert.equal((s[2] ?? '').trim(), '');
  }
  assert.doesNotMatch(html, /<style|\sstyle="|\son[a-z]+="/i);
  assert.doesNotMatch(html, /(?:src|href)="https?:\/\/(?!github\.com\/maxgfr\/nuee")/, 'no third-party resources');
});

test('the manifest and fonts are present', async () => {
  const m = JSON.parse(await readFile(path.join(web, 'manifest.webmanifest'), 'utf8'));
  assert.equal(m.name, 'Nuée');
  assert.equal(m.display, 'standalone');
  assert.ok(m.icons.some((/** @type {{ purpose?: string }} */ i) => i.purpose === 'maskable'));
  const fonts = await readdir(path.join(web, 'fonts'));
  assert.ok(fonts.some((f) => f.endsWith('.woff2')));
  assert.ok(fonts.some((f) => f.startsWith('OFL')));
});
