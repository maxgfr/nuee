// @ts-check
// Publish (#/publish): this tab serves a site under a stable name, as a group's keepers do, and
// hands out the loader that serves it from any static host. Nothing of the site runs here.
/** @typedef {import('../main').ViewContext} ViewContext */
/** @typedef {import('../api').NamedBytes} NamedBytes */
import { h, setText, announce, icon, focusOn, download } from '../lib/dom.js';
import { t } from '../i18n.js';
import { errorArea, framedNotice, linkPanel } from '../lib/ui.js';
import { tarFiles } from '../lib/inline.js';
import { MAX_TOTAL } from './run.js';

const SAMPLE = ['index.html', 'about.html', 'style.css', 'app.js'];
const LOADER = ['index.html', 'nuee-loader.js', 'nuee-sw.js'];
/** The site the loader runs: this page's own address, where the Nuée package lives. */
const BASE = new URL('../../', import.meta.url).href;

/** @param {string} dir @param {string[]} names @returns {Promise<NamedBytes[]>} */
async function fetchFiles(dir, names) {
  return Promise.all(
    names.map(async (name) => {
      const res = await fetch(new URL(`../../${dir}/${name}`, import.meta.url), { cache: 'no-store' });
      if (!res.ok) throw new Error(`${dir}/${name}: HTTP ${res.status}`);
      return { name, bytes: new Uint8Array(await res.arrayBuffer()) };
    }),
  );
}

/**
 * The three files of the loader for `link`, as `nuee site loader` writes them.
 * @param {string} link
 * @returns {Promise<NamedBytes[]>}
 */
export async function loaderFiles(link) {
  const files = await fetchFiles('loader', LOADER);
  const enc = new TextEncoder();
  const dec = new TextDecoder();
  const readme = { name: 'README.txt', bytes: enc.encode(t('publish.readme')) };
  return [readme, ...files].map((f) =>
    f.name === 'nuee-loader.js'
      ? {
          name: f.name,
          bytes: enc.encode(dec.decode(f.bytes).replace('"__NUEE_LINK__"', JSON.stringify(link)).replace('"__NUEE_BASE__"', JSON.stringify(BASE))),
        }
      : f,
  );
}

/** Files of a picked folder, relative to it. @param {File[]} list */
function folderFiles(list) {
  return list.map((f) => {
    const rel = f.webkitRelativePath || f.name;
    return { file: f, name: rel.includes('/') ? rel.slice(rel.indexOf('/') + 1) : rel };
  });
}

/** @param {ViewContext} ctx */
export function render({ app, root }) {
  document.documentElement.dataset.area = 'share';
  const errors = errorArea();
  const page = h(
    'section',
    { class: 'page narrow', 'aria-labelledby': 'publish-title' },
    h('h1', { id: 'publish-title', i18n: 'publish.title' }),
    h('p', { class: 'lead', i18n: 'publish.lead' }),
  );
  root.append(page);
  if (app.caps.framed) {
    page.append(framedNotice());
    return {};
  }
  const sample = h('button', { type: 'button', class: 'btn primary', i18n: 'publish.sample' });
  const input = h('input', { type: 'file', id: 'publish-folder', props: { multiple: true } });
  // A folder picker: dom.js knows no such property, and every browser here supports it.
  Reflect.set(input, 'webkitdirectory', true);
  const form = h(
    'div',
    { class: 'card' },
    sample,
    h('p', { class: 'muted', i18n: 'publish.or' }),
    h('label', { for: 'publish-folder', i18n: 'publish.pick' }),
    input,
    errors.el,
  );
  page.append(form);

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

  /** @param {() => Promise<NamedBytes[]>} files */
  const publish = async (files) => {
    sample.disabled = true;
    input.disabled = true;
    setText(sample, 'publish.starting');
    errors.clear();
    try {
      const list = await files();
      if (!list.some((f) => f.name === 'index.html')) {
        errors.showKey('publish.noIndex');
        throw null;
      }
      const total = list.reduce((a, f) => a + f.bytes.length, 0);
      if (total > MAX_TOTAL) {
        errors.showKey('publish.tooLarge');
        throw null;
      }
      await app.node();
      const named = list.map((f) => ({ name: `site/${f.name}`, bytes: f.bytes }));
      const res = await app.bridge.call('publishSite', { files: named, name: 'site' }, { move: true });
      session = res.session;
      const panel = await linkPanel(app, res.link, null);
      disposePanel = panel.dispose;
      const keepOpen = h('p', { class: 'notice', role: 'note', i18n: 'publish.keepOpen' });
      const open = h('a', { href: res.link, target: '_blank', rel: 'noopener', class: 'btn secondary', i18n: 'publish.open' });
      const loader = h('button', { type: 'button', class: 'btn secondary' }, icon('download'), h('span', { i18n: 'publish.loader' }));
      loader.addEventListener('click', async () => {
        try {
          const tar = tarFiles(await loaderFiles(res.link), Math.floor(Date.now() / 1000));
          download(new Blob([/** @type {BlobPart} */ (tar)], { type: 'application/x-tar' }), 'nuee-loader.tar');
        } catch (e) {
          errors.show(e);
        }
      });
      const stopBtn = h('button', { type: 'button', class: 'btn danger' }, icon('stop'), h('span', { i18n: 'publish.stop' }));
      const card = h(
        'div',
        { class: 'card' },
        keepOpen,
        panel.el,
        h('div', { class: 'row' }, open, loader),
        h('p', { class: 'muted', i18n: 'publish.loaderHint' }),
        h('p', { class: 'muted' }, h('span', { i18n: 'publish.cli' }), ' ', h('a', { href: 'docs/#ex-name', i18n: 'publish.docs' })),
        errors.el,
        stopBtn,
      );
      stopBtn.addEventListener('click', () => {
        stop();
        const stopped = h('p', { i18n: 'publish.stopped' });
        card.replaceChildren(stopped, h('a', { href: '#/publish', class: 'btn secondary', i18n: 'publish.again', on: { click: () => location.reload() } }));
        focusOn(stopped);
      });
      form.replaceWith(card);
      announce(t('publish.ready'));
      focusOn(keepOpen);
    } catch (e) {
      if (e !== null) errors.show(e);
      sample.disabled = false;
      input.disabled = false;
      setText(sample, 'publish.sample');
    }
  };

  sample.addEventListener('click', () => publish(() => fetchFiles('demo-site', SAMPLE)));
  input.addEventListener('change', () => {
    const picked = folderFiles(input.files ? [...input.files] : []);
    publish(async () => Promise.all(picked.map(async ({ file, name }) => ({ name, bytes: new Uint8Array(await file.arrayBuffer()) }))));
  });

  return { busy: () => session !== null, cleanup: stop };
}
