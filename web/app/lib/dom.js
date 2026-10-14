// @ts-check
// A tiny element builder. Text only ever goes through textContent and attributes through
// setAttribute, so markup from the network can never be interpreted.
import { t } from '../i18n.js';

/**
 * @typedef {string | number | boolean | null | undefined} Prim
 * @typedef {Node | string | null | undefined | false | Child[]} Child
 * @typedef {{
 *   class?: string,
 *   text?: string,
 *   i18n?: string,
 *   vars?: Record<string, string | number>,
 *   i18nAttr?: Record<string, string>,
 *   on?: Record<string, (e: Event) => void>,
 *   props?: Record<string, Prim>,
 *   [attr: string]: unknown,
 * }} Attrs
 */

const RESERVED = new Set(['class', 'text', 'i18n', 'vars', 'i18nAttr', 'on', 'props']);
const PROPS = new Set(['value', 'checked', 'disabled', 'hidden', 'min', 'max', 'step', 'multiple', 'indeterminate']);

/**
 * Creates an element. `i18n` makes its text a translation that follows language switches;
 * `i18nAttr` does the same for attributes such as aria-label.
 * @template {keyof HTMLElementTagNameMap} K
 * @param {K} tag
 * @param {Attrs | null} [attrs]
 * @param {...Child} children
 * @returns {HTMLElementTagNameMap[K]}
 */
export function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  if (attrs) {
    if (attrs.class) el.className = attrs.class;
    if (attrs.i18n) setText(el, attrs.i18n, attrs.vars);
    else if (attrs.text !== undefined) el.textContent = attrs.text;
    if (attrs.i18nAttr) {
      el.dataset.i18nAttr = Object.entries(attrs.i18nAttr).map(([a, k]) => `${a}:${k}`).join(';');
      for (const [a, k] of Object.entries(attrs.i18nAttr)) el.setAttribute(a, t(k));
    }
    if (attrs.on) for (const [ev, fn] of Object.entries(attrs.on)) el.addEventListener(ev, fn);
    if (attrs.props) for (const [k, v] of Object.entries(attrs.props)) setProp(el, k, v);
    for (const [k, v] of Object.entries(attrs)) {
      if (RESERVED.has(k) || v === undefined || v === null || v === false) continue;
      setAttr(el, k, v === true ? '' : String(v));
    }
  }
  append(el, children);
  return el;
}

/**
 * @param {Element} el
 * @param {string} name
 * @param {string} value
 */
export function setAttr(el, name, value) {
  const n = name.toLowerCase();
  if (n.startsWith('on') || n === 'style' || n === 'srcdoc') throw new Error(`refusing attribute ${name}`);
  if ((n === 'href' || n === 'src') && /^\s*javascript:/i.test(value)) throw new Error('refusing a javascript: URL');
  el.setAttribute(name, value);
}

/**
 * @param {HTMLElement} el
 * @param {string} key
 * @param {Prim} value
 */
function setProp(el, key, value) {
  if (!PROPS.has(key)) throw new Error(`unknown property ${key}`);
  Reflect.set(el, key, value);
}

/**
 * @param {Node} parent
 * @param {Child[]} children
 */
export function append(parent, children) {
  for (const c of children) {
    if (c === null || c === undefined || c === false) continue;
    if (Array.isArray(c)) append(parent, c);
    else parent.appendChild(typeof c === 'string' ? document.createTextNode(c) : c);
  }
}

/**
 * Sets a translated text that is refreshed when the language changes.
 * @param {HTMLElement} el
 * @param {string} key
 * @param {Record<string, string | number>} [vars]
 */
export function setText(el, key, vars) {
  el.dataset.i18n = key;
  if (vars) el.dataset.i18nVars = JSON.stringify(vars);
  else delete el.dataset.i18nVars;
  el.textContent = t(key, vars);
}

/** Re-translates every marked element under `root`. @param {ParentNode} root */
export function relocalize(root) {
  for (const el of root.querySelectorAll('[data-i18n]')) {
    if (!(el instanceof HTMLElement) || !el.dataset.i18n) continue;
    const vars = el.dataset.i18nVars ? JSON.parse(el.dataset.i18nVars) : undefined;
    el.textContent = t(el.dataset.i18n, vars);
  }
  for (const el of root.querySelectorAll('[data-i18n-attr]')) {
    if (!(el instanceof HTMLElement) || !el.dataset.i18nAttr) continue;
    for (const pair of el.dataset.i18nAttr.split(';')) {
      const [a, k] = pair.split(':');
      if (a && k) el.setAttribute(a, t(k));
    }
  }
}

/** Announces a short message to screen readers through the polite live region. @param {string} msg */
export function announce(msg) {
  const region = document.getElementById('status');
  if (!region) return;
  region.textContent = '';
  // A new text node after clearing makes repeated messages announced again.
  requestAnimationFrame(() => {
    region.textContent = msg;
  });
}

/**
 * Moves focus to `el` once it is in the document, after a view replaced its content: the
 * new state's heading or status, or the next row to answer. It becomes focusable from script
 * only (tabindex -1) when it is not focusable already.
 * @param {HTMLElement | null | undefined} el
 */
export function focusOn(el) {
  if (!el || !el.isConnected) return;
  if (el.tabIndex < 0 && !el.hasAttribute('tabindex')) el.setAttribute('tabindex', '-1');
  el.focus();
}

/**
 * An alert box for an error code; `role="alert"` makes screen readers read it at once.
 * @param {string} key i18n key
 * @param {Record<string, string | number>} [vars]
 */
export function alertBox(key, vars) {
  return h('div', { class: 'alert', role: 'alert', i18n: key, vars });
}

/**
 * A button that copies `text()` to the clipboard and says so.
 * @param {() => string} text
 * @param {string} [label] i18n key
 */
export function copyButton(text, label = 'copy') {
  const words = h('span', { class: 'copy-words', i18n: label });
  // Both icons are drawn once; the `copied` state shows the check instead of the copy icon.
  const btn = h('button', { type: 'button', class: 'btn secondary copy-btn' }, icon('copy'), icon('check'), words);
  btn.addEventListener('click', () => {
    navigator.clipboard.writeText(text()).then(
      () => {
        setText(words, 'copied');
        btn.classList.add('copied');
        announce(t('copied'));
        setTimeout(() => {
          setText(words, label);
          btn.classList.remove('copied');
        }, 2000);
      },
      (e) => {
        console.warn('clipboard write failed', e);
        setText(words, 'copyFailed');
        announce(t('copyFailed'));
        setTimeout(() => setText(words, label), 6000);
      },
    );
  });
  return btn;
}

const SVG_NS = 'http://www.w3.org/2000/svg';

/** The drawn icon set: 24-unit paths, stroked in the current text color. */
const ICONS = {
  copy: ['M8.5 8.5h10v11h-10z', 'M5.5 15.5v-11h10'],
  fullscreen: ['M4 9V4h5', 'M20 9V4h-5', 'M4 15v5h5', 'M20 15v5h-5'],
  check: ['M5 12.5l4.5 4.5L19 7.5'],
  cross: ['M6.5 6.5l11 11', 'M17.5 6.5l-11 11'],
  ring: ['M19.6 9.2c1.2 3.6-1.6 7.9-6.3 9.4-4.7 1.5-9.1-.3-10.3-3.9-1.2-3.6 1.6-7.9 6.3-9.4 3.4-1.1 6.7-.6 8.7 1.1'],
  download: ['M12 4v11', 'M7 10.5l5 5 5-5', 'M5 20h14'],
  stop: ['M6.5 6.5h11v11h-11z'],
};

/**
 * A small inline SVG icon, hidden from assistive technology (the text next to it names the
 * action).
 * @param {keyof typeof ICONS} name
 * @returns {SVGSVGElement}
 */
export function icon(name) {
  const svg = /** @type {SVGSVGElement} */ (document.createElementNS(SVG_NS, 'svg'));
  for (const [k, v] of [['viewBox', '0 0 24 24'], ['class', `icon icon-${name}`], ['aria-hidden', 'true'], ['focusable', 'false']]) svg.setAttribute(k, v);
  for (const d of ICONS[name]) {
    const path = document.createElementNS(SVG_NS, 'path');
    path.setAttribute('d', d);
    svg.appendChild(path);
  }
  return svg;
}

/**
 * Offers bytes as a download; the object URL is revoked once the click was handled.
 * @param {Blob} blob
 * @param {string} name
 */
export function download(blob, name) {
  const url = URL.createObjectURL(blob);
  const a = h('a', { href: url, download: name, class: 'visually-hidden' });
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 10_000);
}

/**
 * Shows QR SVG markup as an image. The markup is loaded through a blob: URL into <img>, where
 * SVG can never run scripts, so nothing has to be parsed into the document.
 * @param {string} svg
 * @param {string} altKey i18n key
 * @returns {{ img: HTMLImageElement, dispose: () => void }}
 */
export function qrImage(svg, altKey) {
  const trimmed = svg.trimStart();
  if (!trimmed.startsWith('<?xml') && !trimmed.startsWith('<svg')) throw new Error('not SVG markup');
  const url = URL.createObjectURL(new Blob([svg], { type: 'image/svg+xml' }));
  const img = h('img', { src: url, class: 'qr', i18nAttr: { alt: altKey }, width: 320, height: 320 });
  return { img, dispose: () => URL.revokeObjectURL(url) };
}
