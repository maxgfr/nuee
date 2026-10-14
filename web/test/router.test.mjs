// @ts-check
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parseHash, extractLink, hashOf } from '../app/router.js';

const P = 'AHVEU3DD4KOFECV66VIHWEZOYX4ZKR3W';

test('plain pages', () => {
  assert.deepEqual(parseHash(''), { view: 'home' });
  assert.deepEqual(parseHash('#'), { view: 'home' });
  assert.deepEqual(parseHash('#/'), { view: 'home' });
  assert.deepEqual(parseHash('#/run'), { view: 'run' });
  assert.deepEqual(parseHash('#/run/'), { view: 'run' });
  assert.deepEqual(parseHash('#/share'), { view: 'share' });
  assert.deepEqual(parseHash('#/about'), { view: 'about' });
});

test('link routes', () => {
  assert.deepEqual(parseHash(`#/J/${P}`), { view: 'join', link: `J/${P}` });
  assert.deepEqual(parseHash(`#/F/${P}`), { view: 'file', link: `F/${P}` });
  assert.deepEqual(parseHash(`#/X/${P}`), { view: 'service', link: `X/${P}` });
  assert.deepEqual(parseHash(`#/S/${P}`), { view: 'site', link: `S/${P}` });
});

test('unknown and malformed hashes', () => {
  for (const h of ['#/nope', '#/RUN', '#/P/' + P, '#/j/' + P, '#//', '#/J/x/y/../z', '#/%00', '#/constructor', '#/__proto__']) {
    assert.equal(parseHash(h).view, 'home', h);
  }
  assert.deepEqual(parseHash('#/J'), { view: 'home', linkError: 'link_truncated' });
  assert.deepEqual(parseHash('#/J/'), { view: 'home', linkError: 'link_truncated' });
  assert.deepEqual(parseHash('#/F/abc!'), { view: 'home', linkError: 'link_corrupt' });
  assert.deepEqual(parseHash(`#/J/${P}1`), { view: 'home', linkError: 'link_corrupt' });
  assert.deepEqual(parseHash('#/J/<script>'), { view: 'home', linkError: 'link_corrupt' });
});

test('pasted links', () => {
  assert.deepEqual(extractLink(`https://maxgfr.github.io/nuee/#/J/${P}`), { kind: 'J', link: `J/${P}` });
  assert.deepEqual(extractLink(`  #/F/${P}\n`), { kind: 'F', link: `F/${P}` });
  assert.deepEqual(extractLink(`S/${P}`), { kind: 'S', link: `S/${P}` });
  assert.deepEqual(extractLink(`X/${P.toLowerCase()}`), { kind: 'X', link: `X/${P}` });
  assert.deepEqual(extractLink(`P/${P}`), { kind: 'P', link: `P/${P}` });
  // Line breaks inside a copied link are dropped.
  assert.deepEqual(extractLink(`#/J/${P.slice(0, 10)}\n${P.slice(10)}`), { kind: 'J', link: `J/${P}` });
  assert.equal(extractLink('hello world'), null);
  assert.equal(extractLink('https://example.org/'), null);
  assert.equal(extractLink(''), null);
  assert.equal(hashOf(`J/${P}`), `#/J/${P}`);
});
