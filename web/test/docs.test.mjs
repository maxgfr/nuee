// @ts-check
// The documentation page: anchors resolve, every `nuee <verb>` shown is a real verb, and
// nothing inline that the CSP would block.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const web = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const repo = path.resolve(web, '..');
const PAGES = { en: path.join(web, 'docs', 'index.html') };

/** @param {string} file */
async function page(file) {
  return readFile(file, 'utf8');
}

/** Every id in the document. @param {string} html */
function ids(html) {
  return [...html.matchAll(/\sid="([^"]+)"/g)].map((m) => m[1] ?? '');
}

/** Ids of the headings, in order: the sections a reader can link to. @param {string} html */
function headingIds(html) {
  return [...html.matchAll(/<h[1-3] id="([^"]+)"/g)].map((m) => m[1] ?? '');
}

/** Text of every <code> element, entities decoded. @param {string} html */
function codeTexts(html) {
  return [...html.matchAll(/<code>([\s\S]*?)<\/code>/g)].map((m) =>
    (m[1] ?? '').replace(/<[^>]+>/g, '').replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"').replace(/&#39;/g, "'").replace(/&amp;/g, '&'),
  );
}

/** The verbs of the CLI, read from the `Command` enum of crates/nuee/src/main.rs. */
async function verbs() {
  const src = await readFile(path.join(repo, 'crates', 'nuee', 'src', 'main.rs'), 'utf8');
  const m = /enum Command \{([\s\S]*?)\n\}/.exec(src);
  assert.ok(m && m[1], 'enum Command not found in main.rs');
  const names = [...m[1].matchAll(/^\s*([A-Z][A-Za-z]*)\(/gm)].map((v) => (v[1] ?? '').replace(/[A-Z]/g, (c, i) => (i ? '-' : '') + c.toLowerCase()));
  assert.ok(names.length >= 10, `found ${names.length} verbs`);
  return new Set(names);
}

/** Global options that take a value, so `nuee --relay URL run` names the verb `run`. */
const VALUE_FLAGS = new Set(['--relay', '--web-base', '--config-dir', '--data-dir']);

/**
 * The verb of every `nuee …` command line in a text: the first word after `nuee` that is not
 * an option or an option's value.
 * @param {string} text
 */
function shownVerbs(text) {
  const out = [];
  for (const m of text.matchAll(/(?:^|[\s(`$])nuee((?:\s+\S+)+)/gm)) {
    const words = (m[1] ?? '').trim().split(/\s+/);
    for (let i = 0; i < words.length; i += 1) {
      const w = words[i] ?? '';
      if (w.startsWith('-')) {
        if (VALUE_FLAGS.has(w)) i += 1;
        continue;
      }
      // A verb may be followed by punctuation of the sentence around it: "(see `nuee relay`)".
      out.push(/^[a-z][a-z-]*/.exec(w)?.[0] ?? w);
      break;
    }
  }
  return out;
}

for (const [lang, file] of Object.entries(PAGES)) {
  test(`docs (${lang}): the title and introduction come before the contents`, async () => {
    const html = await page(file);
    const h1 = html.indexOf('<h1 id="doc-title">');
    const lead = html.indexOf('<p class="lead">');
    const toc = html.indexOf('<nav class="toc"');
    assert.ok(h1 >= 0 && lead >= 0 && toc >= 0, 'h1, lead or contents missing');
    assert.ok(h1 < lead && lead < toc, 'the H1 and the intro must precede the table of contents');
    const nav = html.slice(toc, html.indexOf('</nav>', toc));
    assert.match(nav, /<details class="toc-fold">\s*<summary id="toc-title">[^<]+<\/summary>/);
    assert.doesNotMatch(nav, /<details[^>]*\sopen/, 'the contents are closed by default');
  });

  test(`docs (${lang}): every internal anchor resolves and ids are unique`, async () => {
    const html = await page(file);
    const all = ids(html);
    assert.equal(new Set(all).size, all.length, `duplicate ids: ${all.filter((x, i) => all.indexOf(x) !== i).join(', ')}`);
    const known = new Set(all);
    for (const m of html.matchAll(/href="#([^"]*)"/g)) {
      assert.ok(known.has(m[1] ?? ''), `#${m[1]} does not resolve`);
    }
  });

  test(`docs (${lang}): only real nuee verbs are shown`, async () => {
    const real = await verbs();
    const html = await page(file);
    let seen = 0;
    for (const text of codeTexts(html)) {
      for (const v of shownVerbs(text)) {
        // `nuee <verb>` in prose is a placeholder, not a verb.
        if (v.startsWith('<')) continue;
        assert.ok(real.has(v), `"nuee ${v}" is not a verb of the CLI`);
        seen += 1;
      }
    }
    assert.ok(seen >= 20, `only ${seen} commands found`);
  });

  test(`docs (${lang}): no inline script, style or event handler; same CSP as the app`, async () => {
    const html = await page(file);
    for (const s of html.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script>/g)) {
      assert.match(s[1] ?? '', /\bsrc="/, 'inline script');
      assert.equal((s[2] ?? '').trim(), '');
    }
    assert.doesNotMatch(html, /<style|\sstyle=|\son[a-z]+=/i);
    const app = await page(path.join(web, 'index.html'));
    const csp = (/** @param {string} h */ (h) => /<meta http-equiv="Content-Security-Policy" content="([^"]+)">/.exec(h)?.[1]);
    assert.ok(csp(html), 'CSP meta missing');
    assert.equal(csp(html), csp(app));
    assert.match(html, new RegExp(`<html lang="${lang}"`));
    assert.match(html, /<link rel="canonical" href="https:\/\/maxgfr\.github\.io\/nuee\/docs\/">/);
    assert.doesNotMatch(html, /rel="alternate"/, 'no alternate language link: the page is English only');
  });
}
