// @ts-check
// Turns a fetched static site into one self-contained, inert HTML document for the preview
// frame. Hosted content is hostile: the output keeps markup, text, inlined CSS and inlined
// media only. Scripts, event handlers, script URLs, frames, plugins, refreshes, <base>, form
// targets and navigation are removed. The document is rebuilt token by token from a small
// tolerant tokenizer of our own, so it behaves the same in the page and under Node, and every
// `<` that reaches the output was written by the serializer below.
/** @typedef {import('../api').NamedBytes} NamedBytes */

/** Total size of an inlined document. */
export const MAX_INLINE = 16 * 1024 * 1024;

/** First line of every preview document; the page's HTML policy only accepts such documents. */
export const PREVIEW_MARK = '<!doctype html><!-- nuee static preview -->';

/** An error the UI maps through its `code`, like the ones of the bridge. */
export class InlineError extends Error {
  /** @param {'too_large' | 'invalid_argument'} code @param {string} message */
  constructor(code, message) {
    super(message);
    this.code = code;
  }
}

const MIME = new Map([
  ['png', 'image/png'],
  ['jpg', 'image/jpeg'],
  ['jpeg', 'image/jpeg'],
  ['gif', 'image/gif'],
  ['webp', 'image/webp'],
  ['avif', 'image/avif'],
  ['svg', 'image/svg+xml'],
  ['ico', 'image/x-icon'],
  ['bmp', 'image/bmp'],
  ['woff', 'font/woff'],
  ['woff2', 'font/woff2'],
  ['ttf', 'font/ttf'],
  ['otf', 'font/otf'],
  ['css', 'text/css'],
  ['html', 'text/html'],
  ['htm', 'text/html'],
  ['mp4', 'video/mp4'],
  ['webm', 'video/webm'],
  ['mp3', 'audio/mpeg'],
  ['ogg', 'audio/ogg'],
  ['wav', 'audio/wav'],
]);

/** MIME types a preview may embed as a `data:` URI (never documents or scripts). */
const EMBEDDABLE = /^(?:image|font|audio|video)\//;

/** @param {string} name */
export function mimeOf(name) {
  const dot = name.lastIndexOf('.');
  const ext = dot === -1 ? '' : name.slice(dot + 1).toLowerCase();
  return MIME.get(ext) ?? 'application/octet-stream';
}

/**
 * The page to preview: `index.html` at the root, else the first `.html` file in name order.
 * @param {NamedBytes[]} files
 * @returns {string | null}
 */
export function pickPage(files) {
  const names = files.map((f) => f.name);
  if (names.includes('index.html')) return 'index.html';
  const pages = names.filter((n) => /\.html?$/i.test(n)).sort();
  return pages[0] ?? null;
}

/**
 * Resolves a reference found in `from` to a file of the site. URLs with a scheme, protocol
 * relative URLs and paths climbing above the site root resolve to nothing.
 * @param {string} from path of the referring file
 * @param {string} ref
 * @returns {string | null}
 */
export function resolvePath(from, ref) {
  let r = ref.trim().replace(/[#?].*$/s, '');
  if (r === '' || /^[a-z][a-z0-9+.-]*:/i.test(r) || r.startsWith('//') || r.includes('\\')) return null;
  try {
    r = decodeURIComponent(r);
  } catch {
    return null;
  }
  const parts = r.startsWith('/') ? [] : from.split('/').slice(0, -1);
  for (const seg of r.split('/')) {
    if (seg === '' || seg === '.') continue;
    if (seg === '..') {
      if (parts.length === 0) return null;
      parts.pop();
    } else parts.push(seg);
  }
  return parts.length ? parts.join('/') : null;
}

/** @param {Uint8Array} bytes */
function base64(bytes) {
  let s = '';
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(s);
}

const ENTITIES = new Map([
  ['amp', '&'],
  ['lt', '<'],
  ['gt', '>'],
  ['quot', '"'],
  ['apos', "'"],
  ['nbsp', ' '],
  ['colon', ':'],
  ['tab', '\t'],
  ['newline', '\n'],
]);

/** Decodes the character references that matter for URLs and attribute checks. @param {string} s */
export function decodeEntities(s) {
  return s.replace(/&(#x[0-9a-f]+|#[0-9]+|[a-z]+);?/gi, (m, body) => {
    const b = String(body);
    if (b[0] === '#') {
      const cp = b[1] === 'x' || b[1] === 'X' ? parseInt(b.slice(2), 16) : parseInt(b.slice(1), 10);
      return cp > 0 && cp <= 0x10ffff ? String.fromCodePoint(cp) : '�';
    }
    return ENTITIES.get(b.toLowerCase()) ?? m;
  });
}

/** @param {string} s */
function escapeText(s) {
  return s.replace(/&(?!(?:#x[0-9a-f]+|#[0-9]+|[a-z][a-z0-9]*);)/gi, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

/** @param {string} s */
function escapeAttr(s) {
  return s.replace(/&/g, '&amp;').replace(/"/g, '&quot;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

// ---------------------------------------------------------------------------------------------
// Tokenizer

/**
 * @typedef {{ type: 'text', text: string }
 *   | { type: 'start', name: string, attrs: [string, string][], selfClosing: boolean }
 *   | { type: 'end', name: string }
 *   | { type: 'raw', name: string, text: string }} Token
 */

/** Elements whose content is raw text up to their end tag. `noscript` is not one: scripting is off in the preview. */
const RAW = new Set(['script', 'style', 'textarea', 'title', 'xmp', 'iframe', 'noembed', 'noframes']);

/**
 * Splits HTML into tokens. Tolerant: whatever is not markup is text, and an unfinished tag
 * at the end is text too (it is escaped on output).
 * @param {string} src
 * @returns {Token[]}
 */
export function tokenize(src) {
  /** @type {Token[]} */
  const out = [];
  let i = 0;
  let text = '';
  const flush = () => {
    if (text) out.push({ type: 'text', text });
    text = '';
  };
  while (i < src.length) {
    const lt = src.indexOf('<', i);
    if (lt === -1) {
      text += src.slice(i);
      break;
    }
    text += src.slice(i, lt);
    i = lt;
    const next = src[i + 1] ?? '';
    if (src.startsWith('<!--', i)) {
      const end = src.indexOf('-->', i + 4);
      i = end === -1 ? src.length : end + 3;
      continue;
    }
    if (next === '!' || next === '?') {
      const end = src.indexOf('>', i);
      i = end === -1 ? src.length : end + 1;
      continue;
    }
    if (next === '/' && /[a-z]/i.test(src[i + 2] ?? '')) {
      const m = /^<\/([a-z][^\s/>]*)[^>]*>?/i.exec(src.slice(i, i + 4096));
      if (m) {
        flush();
        out.push({ type: 'end', name: (m[1] ?? '').toLowerCase() });
        i += m[0].length;
        continue;
      }
    }
    if (/[a-z]/i.test(next)) {
      const tag = readTag(src, i);
      if (tag) {
        flush();
        out.push({ type: 'start', name: tag.name, attrs: tag.attrs, selfClosing: tag.selfClosing });
        i = tag.end;
        if (RAW.has(tag.name)) {
          const close = new RegExp(`</${tag.name}(?=[\\s/>]|$)`, 'i');
          const rest = src.slice(i);
          const m = close.exec(rest);
          const body = m ? rest.slice(0, m.index) : rest;
          out.push({ type: 'raw', name: tag.name, text: body });
          i += body.length;
          if (m) {
            const gt = src.indexOf('>', i);
            i = gt === -1 ? src.length : gt + 1;
            out.push({ type: 'end', name: tag.name });
          }
        }
        continue;
      }
    }
    text += '<';
    i += 1;
  }
  flush();
  return out;
}

/**
 * @param {string} src
 * @param {number} start index of `<`
 * @returns {{ name: string, attrs: [string, string][], selfClosing: boolean, end: number } | null}
 */
function readTag(src, start) {
  let i = start + 1;
  let name = '';
  while (i < src.length && !/[\s/>]/.test(src[i] ?? '')) name += src[i++];
  /** @type {[string, string][]} */
  const attrs = [];
  let selfClosing = false;
  for (;;) {
    while (i < src.length && /[\s/]/.test(src[i] ?? '')) {
      selfClosing = src[i] === '/';
      i++;
    }
    if (i >= src.length) return null;
    if (src[i] === '>') {
      i++;
      break;
    }
    selfClosing = false;
    let an = '';
    // An attribute name may start with `=` per the HTML tokenizer, never with whitespace, `/` or `>`.
    do an += src[i++];
    while (i < src.length && !/[\s/>=]/.test(src[i] ?? ''));
    while (i < src.length && /\s/.test(src[i] ?? '')) i++;
    let value = '';
    if (src[i] === '=') {
      i++;
      while (i < src.length && /\s/.test(src[i] ?? '')) i++;
      const q = src[i];
      if (q === '"' || q === "'") {
        const end = src.indexOf(q, i + 1);
        if (end === -1) return null;
        value = src.slice(i + 1, end);
        i = end + 1;
      } else {
        while (i < src.length && !/[\s>]/.test(src[i] ?? '')) value += src[i++];
      }
    }
    attrs.push([an.toLowerCase(), value]);
  }
  return { name: name.toLowerCase(), attrs, selfClosing, end: i };
}

// ---------------------------------------------------------------------------------------------
// Sanitizing serializer

/** Elements dropped with everything inside them. */
const DROP_WITH_CONTENT = new Set(['script', 'iframe', 'noembed', 'noframes', 'frameset', 'template', 'xmp']);
/** Elements dropped, their content kept (and sanitized like the rest). */
const DROP_TAG = new Set([
  'base', 'object', 'embed', 'applet', 'frame', 'portal', 'fencedframe', 'noscript', 'plaintext',
  'link', 'meta', 'param', 'set', 'animate', 'animatemotion', 'animatetransform', 'discard',
  'handler', 'listener', 'foreignobject', 'html', 'head', 'body',
]);
/** Attributes that point at a resource to inline. */
const RESOURCE_ATTRS = new Set(['src', 'poster', 'background']);
/** Attributes that navigate, submit, load or run something: removed. */
const DROP_ATTRS = new Set([
  'action', 'formaction', 'srcdoc', 'srcset', 'imagesrcset', 'ping', 'target', 'data', 'codebase',
  'manifest', 'http-equiv', 'xmlns:xlink', 'is', 'popovertarget', 'dynsrc', 'lowsrc', 'longdesc',
  'cite', 'archive', 'classid', 'attributename', 'from', 'to', 'values', 'by', 'begin', 'href',
  'xlink:href', 'nonce', 'integrity', 'autofocus',
]);
const SAFE_NAME = /^[a-z][a-z0-9-]*$/;
const SAFE_ATTR = /^[a-z_][a-z0-9_.:-]*$/;
const SCRIPT_URL = /^(?:javascript|vbscript|livescript|data:text\/html|data:application|data:image\/svg)/i;

/** @param {string} v */
function looksLikeScriptUrl(v) {
  // Browsers ignore ASCII whitespace and control characters inside the scheme.
  return SCRIPT_URL.test(decodeEntities(v).replace(/[\u0000- \u007f-\u009f]/g, ''));
}

/**
 * Builds the preview document of a site.
 * @param {NamedBytes[]} files
 * @param {{ max?: number }} [opts]
 * @returns {{ html: string, page: string, size: number }}
 */
export function buildPreview(files, opts = {}) {
  const max = opts.max ?? MAX_INLINE;
  const page = pickPage(files);
  if (!page) throw new InlineError('invalid_argument', 'the site has no .html page');
  /** @type {Map<string, Uint8Array>} */
  const byName = new Map(files.map((f) => [f.name, f.bytes]));
  const utf8 = new TextDecoder('utf-8');
  let budget = max;
  /** @param {number} n */
  const spend = (n) => {
    budget -= n;
    if (budget < 0) throw new InlineError('too_large', `the preview would exceed ${max} bytes`);
  };
  /** @type {Map<string, string>} */
  const dataUris = new Map();

  /** A data: URI for an embeddable file of the site, or null. @param {string} from @param {string} ref */
  const dataUri = (from, ref) => {
    const path = resolvePath(from, decodeEntities(ref));
    if (!path) return null;
    const bytes = byName.get(path);
    if (!bytes) return null;
    const mime = mimeOf(path);
    if (!EMBEDDABLE.test(mime)) return null;
    const cached = dataUris.get(path);
    if (cached) return cached;
    if (Math.ceil(bytes.length / 3) * 4 > budget) throw new InlineError('too_large', `the preview would exceed ${max} bytes`);
    const uri = `data:${mime};base64,${base64(bytes)}`;
    dataUris.set(path, uri);
    return uri;
  };

  /**
   * Rewrites a stylesheet: local url(...) become data: URIs, local @import are inlined, the
   * rest is removed. `<` is escaped so the text can never close its <style>.
   * @param {string} src @param {string} from @param {Set<string>} seen
   * @returns {string}
   */
  const rewriteCss = (src, from, seen) => {
    const css = src.replace(/\u0000/g, '');
    /** @type {string[]} */
    const imported = [];
    let out = css.replace(/@import\s+(?:url\(\s*)?(["']?)([^"')\s;]+)\1\s*\)?[^;]*;?/gi, (_m, _q, ref) => {
      const path = resolvePath(from, String(ref));
      if (!path || seen.has(path) || seen.size > 16) return '';
      const bytes = byName.get(path);
      if (!bytes || mimeOf(path) !== 'text/css') return '';
      imported.push(rewriteCss(utf8.decode(bytes), path, new Set([...seen, path])));
      return `\u0000${imported.length - 1}\u0000`;
    });
    out = out.replace(/url\(\s*(["']?)(.*?)\1\s*\)/gis, (_m, _q, ref) => {
      const uri = dataUri(from, String(ref));
      return uri ? `url("${uri}")` : 'none';
    });
    out = out.replace(/\u0000(\d+)\u0000/g, (_m, n) => imported[Number(n)] ?? '');
    out = out.replace(/expression\s*\(/gi, '(').replace(/-moz-binding/gi, '-x-binding').replace(/behavior\s*:/gi, 'x-behavior:');
    return out.replace(/</g, '\\3c ');
  };

  const tokens = tokenize(utf8.decode(byName.get(page)));
  /** @type {string[]} */
  const parts = [PREVIEW_MARK];
  spend(PREVIEW_MARK.length);
  /** @param {string} s */
  const emit = (s) => {
    spend(s.length);
    parts.push(s);
  };
  /** @type {string | null} element being skipped with its content */
  let skipping = null;
  let depth = 0;
  for (const tok of tokens) {
    if (skipping) {
      if (tok.type === 'start' && tok.name === skipping && !tok.selfClosing) depth++;
      if (tok.type === 'end' && tok.name === skipping && --depth === 0) skipping = null;
      continue;
    }
    if (tok.type === 'text') {
      emit(escapeText(tok.text));
      continue;
    }
    if (tok.type === 'raw') {
      if (tok.name === 'style') emit(rewriteCss(tok.text, page, new Set([page])));
      else if (tok.name === 'textarea' || tok.name === 'title') emit(escapeText(tok.text));
      continue;
    }
    if (!SAFE_NAME.test(tok.name)) continue;
    if (tok.type === 'end') {
      if (DROP_TAG.has(tok.name) || DROP_WITH_CONTENT.has(tok.name)) continue;
      emit(`</${tok.name}>`);
      continue;
    }
    if (DROP_WITH_CONTENT.has(tok.name)) {
      if (!tok.selfClosing && !RAW.has(tok.name)) {
        skipping = tok.name;
        depth = 1;
      }
      continue;
    }
    if (tok.name === 'link') {
      const rel = (tok.attrs.find(([k]) => k === 'rel')?.[1] ?? '').toLowerCase().split(/\s+/);
      const href = tok.attrs.find(([k]) => k === 'href')?.[1];
      if (rel.includes('stylesheet') && href) {
        const path = resolvePath(page, decodeEntities(href));
        const bytes = path ? byName.get(path) : undefined;
        if (path && bytes && mimeOf(path) === 'text/css') emit(`<style>${rewriteCss(utf8.decode(bytes), path, new Set([path]))}</style>`);
      }
      continue;
    }
    if (tok.name === 'meta') {
      const keep = tok.attrs.filter(([k]) => k === 'charset' || k === 'name' || k === 'content');
      if (!tok.attrs.some(([k]) => k === 'http-equiv') && keep.length) emit(`<meta${keep.map(([k, v]) => ` ${k}="${escapeAttr(decodeEntities(v))}"`).join('')}>`);
      continue;
    }
    if (DROP_TAG.has(tok.name)) continue;
    let attrs = '';
    for (const [k, raw] of tok.attrs) {
      if (!SAFE_ATTR.test(k) || k.startsWith('on') || k === 'style') continue;
      if (k === 'href' || k === 'xlink:href') {
        if (tok.name === 'image') {
          const uri = dataUri(page, raw);
          if (uri) attrs += ` href="${escapeAttr(uri)}"`;
        } else if ((tok.name === 'a' || tok.name === 'area') && !tok.attrs.some(([n]) => n === 'title')) {
          attrs += ` title="${escapeAttr(decodeEntities(raw).trim())}"`;
        }
        continue;
      }
      if (RESOURCE_ATTRS.has(k)) {
        const uri = dataUri(page, raw);
        if (uri) attrs += ` ${k}="${escapeAttr(uri)}"`;
        continue;
      }
      if (DROP_ATTRS.has(k) || looksLikeScriptUrl(raw)) continue;
      attrs += raw === '' ? ` ${k}` : ` ${k}="${escapeAttr(decodeEntities(raw))}"`;
    }
    emit(`<${tok.name}${attrs}${tok.selfClosing ? ' /' : ''}>`);
  }
  const html = parts.join('');
  return { html, page, size: max - budget };
}

// ---------------------------------------------------------------------------------------------
// Tar archive of the site (ustar), for "download the site".

/**
 * @param {NamedBytes[]} files
 * @param {number} [mtime] seconds since the epoch
 * @returns {Uint8Array}
 */
export function tarFiles(files, mtime = 0) {
  const enc = new TextEncoder();
  let total = 1024;
  for (const f of files) total += 512 + Math.ceil(f.bytes.length / 512) * 512;
  const out = new Uint8Array(total);
  let off = 0;
  /** @param {number} at @param {string} s @param {number} len */
  const put = (at, s, len) => {
    const b = enc.encode(s);
    if (b.length > len) throw new InlineError('invalid_argument', `tar field too long: ${s}`);
    out.set(b, at);
  };
  /** @param {number} n @param {number} len */
  const octal = (n, len) => n.toString(8).padStart(len - 1, '0');
  for (const f of files) {
    let name = f.name;
    let prefix = '';
    if (enc.encode(name).length > 100) {
      const cut = name.lastIndexOf('/', name.length - 1);
      prefix = name.slice(0, cut);
      name = name.slice(cut + 1);
    }
    const h = off;
    put(h, name, 100);
    put(h + 100, octal(0o644, 8), 8);
    put(h + 108, octal(0, 8), 8);
    put(h + 116, octal(0, 8), 8);
    put(h + 124, octal(f.bytes.length, 12), 12);
    put(h + 136, octal(mtime, 12), 12);
    out.fill(0x20, h + 148, h + 156);
    put(h + 156, '0', 1);
    put(h + 257, 'ustar', 6);
    put(h + 263, '00', 2);
    put(h + 345, prefix, 155);
    let sum = 0;
    for (let i = h; i < h + 512; i++) sum += out[i] ?? 0;
    put(h + 148, `${octal(sum, 7)}\0`, 8);
    out.set(f.bytes, h + 512);
    off = h + 512 + Math.ceil(f.bytes.length / 512) * 512;
  }
  return out;
}
