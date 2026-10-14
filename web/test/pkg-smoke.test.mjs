// @ts-check
// Loads the package built by scripts/build-web.sh in Node and exercises the exports that
// need no network. Run: scripts/build-web.sh && node --test web/test/pkg-smoke.test.mjs
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';
import path from 'node:path';

const here = path.dirname(fileURLToPath(import.meta.url));
const web = path.resolve(here, '..');
const repo = path.resolve(web, '..');

const current = JSON.parse(await readFile(path.join(web, 'pkg', 'current.json'), 'utf8'));
const pkgDir = path.join(web, current.path);
const pkg = await import(pathToFileURL(path.join(pkgDir, 'nuee_web.js')).href);
pkg.initSync({ module: await readFile(path.join(pkgDir, 'nuee_web_bg.wasm')) });

// Same values as `smoke_test_link_is_stable` in crates/nuee-web/src/lib.rs.
const LINK =
  'https://maxgfr.github.io/nuee/#/J/AHVEU3DD4KOFECV66VIHWEZOYX4ZKR3WV27L464SIIPOU2IUI3JCYGTIOR2HA4Z2F4XXEZLMMF4S4ZLYMFWXA3DFFZXXEZZPAEBAGBAFAYDQQCIKBMGA2DQPCBWAC4TW';
const COORDINATOR = 'ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c';
const MANDELBROT_HASH = '7085684d08d0c83832195135a40ce2583e47bb4bdb4828c849ff4746f4648057';

/** Calls `f`, expecting it to throw `{code, message}`; returns the code. */
function codeOf(f) {
  try {
    f();
  } catch (e) {
    assert.equal(typeof e.message, 'string');
    return e.code;
  }
  assert.fail('expected an error');
}

test('version matches current.json', () => {
  assert.equal(pkg.version(), current.version);
  assert.equal(current.path, `pkg/${current.version}/`);
});

test('fingerprint gives six emoji', () => {
  const fp = pkg.fingerprint(COORDINATOR);
  assert.equal(fp.length, 6);
  for (const e of fp) {
    assert.equal(typeof e, 'string');
    assert.ok(e.length > 0);
  }
  assert.deepEqual(pkg.fingerprint(COORDINATOR), fp);
  assert.equal(codeOf(() => pkg.fingerprint('not an id')), 'invalid_argument');
});

test('a job link parses without exposing its secret', () => {
  const info = pkg.parseLink(LINK);
  assert.equal(info.kind, 'job');
  assert.equal(info.peer, COORDINATOR);
  assert.equal(info.relay, 'https://relay.example.org/');
  assert.deepEqual(info.fingerprint, pkg.fingerprint(COORDINATOR));
  assert.equal(info.secret, undefined);
  // The fragment alone and `K/PAYLOAD` are accepted too.
  assert.equal(pkg.parseLink(LINK.slice(LINK.indexOf('#'))).peer, COORDINATOR);
  assert.equal(pkg.parseLink(LINK.slice(LINK.indexOf('J/'))).peer, COORDINATOR);
});

test('a truncated link is reported as truncated', () => {
  assert.equal(codeOf(() => pkg.parseLink(LINK.slice(0, -5))), 'link_truncated');
});

test('qrSvg returns SVG markup', () => {
  const svg = pkg.qrSvg('x');
  assert.ok(svg.startsWith('<svg') || svg.startsWith('<?xml'), svg.slice(0, 40));
  assert.ok(svg.includes('<svg'));
});

test('validateModule accepts the golden mandelbrot module', async () => {
  const bytes = await readFile(path.join(repo, 'tasks', 'golden', 'mandelbrot.wasm'));
  const info = pkg.validateModule(new Uint8Array(bytes));
  assert.equal(info.hash, MANDELBROT_HASH);
  assert.equal(info.size, bytes.length);
  assert.ok(info.maxMemory > 0 && info.maxMemory <= 1024 * 1024 * 1024);
});

test('validateModule rejects a module with an import', () => {
  const withImport = new Uint8Array([
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // magic, version
    0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type section: () -> ()
    0x02, 0x09, 0x01, 0x03, 0x65, 0x6e, 0x76, 0x01, 0x66, 0x00, 0x00, // import env.f
  ]);
  assert.equal(codeOf(() => pkg.validateModule(withImport)), 'invalid_module');
});

test('the random source works (getrandom backend present)', () => {
  const a = pkg.newSecret();
  const b = pkg.newSecret();
  assert.match(a.secret, /^[0-9a-f]{32}$/);
  assert.match(a.id, /^[0-9a-f]{64}$/);
  assert.notEqual(a.secret, b.secret);
  assert.notEqual(a.id, b.id);
  assert.equal(pkg.fingerprint(a.id).length, 6);
});
