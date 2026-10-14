// @ts-check
// The static preview: hostile markup is made inert, site resources are inlined, the frame
// grants nothing.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { buildPreview, pickPage, resolvePath, mimeOf, tokenize, tarFiles, InlineError, PREVIEW_MARK } from '../app/lib/inline.js';

const enc = new TextEncoder();
/** @param {Record<string, string | Uint8Array>} m */
const site = (m) => Object.entries(m).map(([name, v]) => ({ name, bytes: typeof v === 'string' ? enc.encode(v) : v }));
/** @param {string} html @param {Record<string, string | Uint8Array>} [extra] */
const preview = (html, extra = {}) => buildPreview(site({ 'index.html': html, ...extra })).html;

const PNG = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3]);
const b64 = (/** @type {Uint8Array} */ b) => Buffer.from(b).toString('base64');

test('scripts in any casing are removed with their content', () => {
  const out = preview('<p>a</p><ScRiPt>alert(1)</sCrIpT><SCRIPT src="x.js"></SCRIPT><script/xss>alert(2)</script ><p>b</p>');
  assert.doesNotMatch(out, /<script/i);
  assert.doesNotMatch(out, /alert/);
  assert.match(out, /<p>a<\/p>/);
  assert.match(out, /<p>b<\/p>/);
});

test('event handlers are removed, including on svg and img', () => {
  const out = preview('<svg onload="alert(1)"><circle r="1" ONCLICK="x()"/></svg><img src=x onerror=alert(2)><body onpageshow="y()">');
  assert.doesNotMatch(out, /\son[a-z]+=/i);
  assert.doesNotMatch(out, /alert/);
  assert.match(out, /<svg>/);
  assert.match(out, /<img>/, 'img with an unknown source keeps no src');
});

test('javascript: URLs never survive, whatever the casing or encoding', () => {
  const out = preview(
    '<a href="JaVaScRiPt:alert(1)">x</a><a href=" &#106;avascript:alert(2)">y</a><form action="javascript:z()"><button formaction="javascript:w()">b</button></form><div data-x="javascript:v()"></div><img src="javascript:u()">',
  );
  assert.doesNotMatch(out, /\s(?:href|src|action|formaction|data-x)="[^"]*javascript/i);
  assert.doesNotMatch(out, /<form[^>]*action/i);
  assert.match(out, /<a title="JaVaScRiPt:alert\(1\)">x<\/a>/, 'link neutralized, its target kept as text');
});

test('frames, plugins, refresh, base and their content are removed', () => {
  const out = preview(
    '<head><meta http-equiv="refresh" content="0;url=https://evil.example"><META HTTP-EQUIV=Content-Security-Policy content="x"><base href="https://evil.example/"><meta charset="utf-8"></head><iframe src="https://evil.example">inner</iframe><object data="x.swf"><embed src="y.swf"></object><p>ok</p>',
  );
  for (const bad of [/<iframe/i, /<object/i, /<embed/i, /<base/i, /http-equiv/i, /evil\.example/, /refresh/]) assert.doesNotMatch(out, bad);
  assert.match(out, /<meta charset="utf-8">/);
  assert.match(out, /<p>ok<\/p>/);
});

test('style attributes are always removed', () => {
  assert.doesNotMatch(preview('<p style="color:red">a</p><div STYLE=x>b</div>'), /style=/i);
});

test('links do not navigate: href is moved into title', () => {
  const out = preview('<a href="/about.html">About</a><a href="https://example.org" title="ext">E</a><area href="m.html">');
  assert.doesNotMatch(out, /\shref=/);
  assert.match(out, /<a title="\/about.html">About<\/a>/);
  assert.match(out, /<a title="ext">E<\/a>/);
});

test('CSS and images are inlined as data: URIs with their MIME types', () => {
  const files = {
    'css/site.css': '@import "more.css"; body { background: url("../img/bg.png") } @font-face { src: url(../fonts/f.woff2) }',
    'css/more.css': 'h1 { background-image: url(\'/img/logo.gif\') }',
    'img/bg.png': PNG,
    'img/logo.gif': new Uint8Array([0x47, 0x49, 0x46]),
    'img/photo.jpg': new Uint8Array([0xff, 0xd8, 0xff]),
    'fonts/f.woff2': new Uint8Array([0x77, 0x4f, 0x46, 0x32]),
  };
  const out = preview('<link rel="stylesheet" href="css/site.css"><img src="img/photo.jpg" alt="p"><video poster="./img/bg.png"></video>', files);
  assert.match(out, /<style>/);
  assert.ok(out.includes(`url("data:image/png;base64,${b64(PNG)}")`));
  assert.ok(out.includes('url("data:image/gif;base64,R0lG")'));
  assert.ok(out.includes('url("data:font/woff2;base64,d09GMg==")'));
  assert.ok(out.includes('src="data:image/jpeg;base64,/9j/"'));
  assert.ok(out.includes(`poster="data:image/png;base64,${b64(PNG)}"`));
  assert.doesNotMatch(out, /<link/i);
  assert.doesNotMatch(out, /@import/);
  assert.equal(mimeOf('a/b.SVG'), 'image/svg+xml');
  assert.equal(mimeOf('x.bin'), 'application/octet-stream');
});

test('external resources are dropped, never fetched', () => {
  const out = preview('<img src="https://tracker.example/p.gif"><style>body{background:url(https://tracker.example/b.png)}</style><img src="//tracker.example/q.gif">');
  assert.doesNotMatch(out, /tracker\.example/);
});

test('../ cannot reach outside the site', () => {
  assert.equal(resolvePath('index.html', '../secret.png'), null);
  assert.equal(resolvePath('a/b/page.html', '../../../x.png'), null);
  assert.equal(resolvePath('a/page.html', '../img/x.png'), 'img/x.png');
  assert.equal(resolvePath('a/page.html', '/img/x.png?v=1#f'), 'img/x.png');
  assert.equal(resolvePath('a/page.html', 'img%2Fx.png'), 'a/img/x.png');
  assert.equal(resolvePath('index.html', 'https://example.org/x.png'), null);
  assert.equal(resolvePath('index.html', '//example.org/x.png'), null);
  assert.equal(resolvePath('index.html', 'file:///etc/passwd'), null);
  const out = buildPreview(site({ 'sub/index.html': '<img src="../../outside.png"><img src="../in.png">', 'in.png': PNG })).html;
  assert.equal((out.match(/data:image\/png/g) ?? []).length, 1);
});

test('the output never contains <script, whatever the input', () => {
  const nasty = [
    '<scr<script>ipt>alert(1)</script>',
    '<<script>script>alert(1)<</script>/script>',
    '<style></style><script>alert(1)</script></style>',
    '<textarea></textarea><script>x</script></textarea>',
    '<title><script>x</script></title>',
    '<noscript><p title="</noscript><script>alert(1)</script>"></p></noscript>',
    '<svg><script xlink:href="data:,alert(1)"/></svg>',
    '<math><mtext><table><mglyph><style><img src=x onerror=alert(1)>',
    '<a title="<script>x</script>">t</a>',
    '<style>a{content:"</style><script>alert(1)</script>"}</style>',
    '<img src="x" alt="<script',
    '<!--<script>--><script>alert(1)</script>',
    '<svg><animate attributeName="href" to="javascript:alert(1)"/><set attributeName="onload" to="alert(1)"/></svg>',
  ];
  for (const html of nasty) {
    const out = preview(html);
    assert.doesNotMatch(out, /<script/i, html);
    assert.doesNotMatch(out, /<[^>]*\son[a-z]+\s*=/i, html);
    assert.ok(out.startsWith(PREVIEW_MARK));
  }
});

test('the size cap is enforced', () => {
  const big = new Uint8Array(4096);
  assert.throws(
    () => buildPreview(site({ 'index.html': '<img src="a.png">', 'a.png': big }), { max: 2048 }),
    (/** @type {unknown} */ e) => e instanceof InlineError && e.code === 'too_large',
  );
  assert.throws(
    () => buildPreview(site({ 'index.html': 'x'.repeat(5000) }), { max: 1000 }),
    (/** @type {unknown} */ e) => e instanceof InlineError && e.code === 'too_large',
  );
  const ok = buildPreview(site({ 'index.html': '<img src="a.png">', 'a.png': big }), { max: 16384 });
  assert.ok(ok.size <= 16384 && ok.size > 4096);
});

test('without index.html the first .html page is used', () => {
  const files = site({ 'z.html': '<p>z</p>', 'docs/a.html': '<p>a</p>', 'b.htm': '<p>b</p>', 'style.css': '' });
  assert.equal(pickPage(files), 'b.htm');
  assert.equal(pickPage(site({ 'index.html': '', 'a.html': '' })), 'index.html');
  assert.equal(pickPage(site({ 'x.css': '' })), null);
  assert.match(buildPreview(files).html, /<p>b<\/p>/);
  assert.throws(() => buildPreview(site({ 'x.css': '' })), InlineError);
});

test('the tokenizer is tolerant', () => {
  assert.deepEqual(tokenize('a < b'), [{ type: 'text', text: 'a < b' }]);
  const t = tokenize('<P CLASS=x Id="y">t</p><br/>');
  assert.deepEqual(t[0], { type: 'start', name: 'p', attrs: [['class', 'x'], ['id', 'y']], selfClosing: false });
  assert.deepEqual(t.at(-1), { type: 'start', name: 'br', attrs: [], selfClosing: true });
  assert.match(preview('a < b & c <unfinished'), /a &lt; b &amp; c &lt;unfinished/);
});

test('the site archive is a valid ustar file', () => {
  const tar = tarFiles(site({ 'index.html': 'hi', 'css/a.css': 'body{}' }));
  assert.equal(tar.length, 512 * 2 + 512 * 2 + 1024);
  assert.equal(new TextDecoder().decode(tar.subarray(257, 262)), 'ustar');
  let sum = 0;
  for (let i = 0; i < 512; i++) sum += i >= 148 && i < 156 ? 0x20 : tar[i] ?? 0;
  assert.equal(parseInt(new TextDecoder().decode(tar.subarray(148, 154)), 8), sum);
  assert.equal(new TextDecoder().decode(tar.subarray(512, 514)), 'hi');
});

test('the preview frame grants nothing', async () => {
  const { previewFrameAttrs } = await import('../app/views/site.js');
  const attrs = previewFrameAttrs();
  assert.equal(attrs.sandbox, '');
  for (const [k, v] of Object.entries(attrs)) {
    assert.doesNotMatch(`${k}=${v}`, /allow-|allow=|srcdoc|src=/i);
  }
  assert.ok(!('allow' in attrs));
});

test('the service console shows text only', async () => {
  const { classifyBody, MAX_SHOWN } = await import('../app/views/service.js');
  const j = classifyBody([['Content-Type', 'application/json']], enc.encode('{"a":[1,2]}'));
  assert.deepEqual(j, { kind: 'json', text: '{\n  "a": [\n    1,\n    2\n  ]\n}' });
  const html = classifyBody([['content-type', 'text/html']], enc.encode('<script>alert(1)</script>'));
  assert.deepEqual(html, { kind: 'text', text: '<script>alert(1)</script>', truncated: false });
  assert.equal(classifyBody([], new Uint8Array([0xff, 0xfe, 0x00])).kind, 'binary');
  assert.equal(classifyBody([['content-type', 'image/png']], PNG).kind, 'binary');
  const long = classifyBody([['content-type', 'text/plain']], enc.encode('é'.repeat(MAX_SHOWN)));
  assert.equal(long.kind, 'text');
  assert.ok(long.kind === 'text' && long.truncated && long.text.length === MAX_SHOWN / 2);
});
