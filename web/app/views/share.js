// @ts-check
// Share (#/share): pick files, get a link and QR; this tab serves them while it stays open.
/** @typedef {import('../main').ViewContext} ViewContext */
/** @typedef {import('../api').NamedBytes} NamedBytes */
import { h, setText, announce, icon, focusOn } from '../lib/dom.js';
import { t } from '../i18n.js';
import { errorArea, framedNotice, linkPanel } from '../lib/ui.js';
import { fmtBytes, fmtNumber } from '../lib/format.js';
import { MAX_TOTAL } from './run.js';

/**
 * Names inside a share: base names only, made unique.
 * @param {string[]} names
 */
export function uniqueNames(names) {
  const seen = new Set();
  return names.map((raw) => {
    const base = (raw.split(/[\\/]/).pop() || 'file').replace(/^\.+/, '_') || 'file';
    let name = base;
    for (let i = 2; seen.has(name); i += 1) {
      const dot = base.lastIndexOf('.');
      name = dot > 0 ? `${base.slice(0, dot)} (${i})${base.slice(dot)}` : `${base} (${i})`;
    }
    seen.add(name);
    return name;
  });
}

/** @param {ViewContext} ctx */
export function render({ app, root }) {
  document.documentElement.dataset.area = 'share';
  const errors = errorArea();
  const page = h('section', { class: 'page narrow', 'aria-labelledby': 'share-title' }, h('h1', { id: 'share-title', i18n: 'share.title' }), h('p', { class: 'lead', i18n: 'share.lead' }));
  root.append(page);
  if (app.caps.framed) {
    page.append(framedNotice());
    return {};
  }
  const input = h('input', { type: 'file', id: 'share-files', props: { multiple: true } });
  const summary = h('p', { class: 'muted', 'aria-live': 'polite' });
  const go = h('button', { type: 'button', class: 'btn primary', i18n: 'share.start', props: { disabled: true } });
  const drop = h('div', { class: 'drop', i18n: 'share.drop' });
  const form = h('div', { class: 'card' }, drop, h('label', { for: 'share-files', i18n: 'share.pick' }), input, h('p', { class: 'muted', i18n: 'share.limit' }), summary, errors.el, go);
  page.append(form);

  /** @type {File[]} */
  let files = [];
  /** @param {File[]} list */
  const pick = (list) => {
    files = list;
    errors.clear();
    const total = files.reduce((a, f) => a + f.size, 0);
    setText(summary, 'share.summary', { n: fmtNumber(files.length), size: fmtBytes(total) });
    if (total > MAX_TOTAL) {
      errors.showKey('share.tooLarge');
      go.disabled = true;
    } else go.disabled = files.length === 0;
  };
  input.addEventListener('change', () => pick(input.files ? [...input.files] : []));
  drop.addEventListener('dragover', (e) => {
    e.preventDefault();
    drop.classList.add('over');
  });
  drop.addEventListener('dragleave', () => drop.classList.remove('over'));
  drop.addEventListener('drop', (e) => {
    e.preventDefault();
    drop.classList.remove('over');
    pick(e.dataTransfer ? [...e.dataTransfer.files] : []);
  });

  /** @type {number | null} */
  let session = null;
  /** @type {(() => void) | null} */
  let disposePanel = null;
  const stop = () => {
    if (disposePanel) disposePanel();
    disposePanel = null;
    if (session !== null) {
      const s = session;
      session = null;
      app.bridge.call('stop', { session: s }).catch((e) => console.warn('stop', e));
    }
  };

  go.addEventListener('click', async () => {
    go.disabled = true;
    setText(go, 'share.starting');
    errors.clear();
    try {
      const names = uniqueNames(files.map((f) => f.name));
      /** @type {NamedBytes[]} */
      const named = await Promise.all(files.map(async (f, i) => ({ name: names[i] ?? 'file', bytes: new Uint8Array(await f.arrayBuffer()) })));
      await app.node();
      const res = await app.bridge.call('share', { files: named }, { move: true });
      session = res.session;
      const panel = await linkPanel(app, res.link, null);
      disposePanel = panel.dispose;
      const stopBtn = h('button', { type: 'button', class: 'btn danger' }, icon('stop'), h('span', { i18n: 'share.stop' }));
      const keepOpen = h('p', { class: 'notice', role: 'note', i18n: 'share.keepOpen' });
      const card = h('div', { class: 'card' }, keepOpen, panel.el, stopBtn);
      stopBtn.addEventListener('click', () => {
        stop();
        const stopped = h('p', { i18n: 'share.stopped' });
        card.replaceChildren(stopped, h('a', { href: '#/share', class: 'btn secondary', i18n: 'share.again', on: { click: () => location.reload() } }));
        focusOn(stopped);
      });
      form.replaceWith(card);
      announce(t('share.ready'));
      focusOn(keepOpen);
    } catch (e) {
      errors.show(e);
      go.disabled = false;
      setText(go, 'share.start');
    }
  });

  return { busy: () => session !== null, cleanup: stop };
}
