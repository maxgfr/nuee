// @ts-check
// File (#/F/…): fetch with progress, the verified badge, then one download per file.
/** @typedef {import('../main').ViewContext} ViewContext */
/** @typedef {import('../api').FetchedFiles} FetchedFiles */
import { h, setText, announce, download, icon } from '../lib/dom.js';
import { t } from '../i18n.js';
import { errorArea, progressBar } from '../lib/ui.js';
import { fmtBytes, fmtNumber, shortHash } from '../lib/format.js';
import { MAX_TOTAL } from './run.js';

/** @param {ViewContext} ctx */
export function render({ app, root, route }) {
  document.documentElement.dataset.area = 'file';
  const link = route.link ?? '';
  const errors = errorArea();
  const page = h('section', { class: 'page narrow', 'aria-labelledby': 'file-title' }, h('h1', { id: 'file-title', i18n: 'file.title' }), errors.el);
  root.append(page);
  let busy = false;
  let gone = false;

  app.bridge.call('parseLink', { link }).then(
    (info) => {
      if (info.kind !== 'file') {
        errors.showKey('err.link_kind');
        return;
      }
      const size = info.size ?? 0;
      const goText = h('span', { i18n: 'file.fetch' });
      const go = h('button', { type: 'button', class: 'btn primary' }, icon('download'), goText);
      const progress = progressBar('file.progress');
      progress.el.hidden = true;
      const card = h(
        'div',
        { class: 'card' },
        h('p', { i18n: 'file.about', vars: { size: fmtBytes(size), n: info.providers.length } }),
        h('p', { class: 'muted', i18n: 'file.e2e' }),
        progress.el,
        go,
      );
      page.append(card);
      if (size > MAX_TOTAL) {
        errors.showKey('err.too_large');
        go.disabled = true;
        return;
      }
      go.addEventListener('click', () => {
        go.disabled = true;
        busy = true;
        errors.clear();
        progress.el.hidden = false;
        progress.set(0, size, 'file.progressText', { got: fmtBytes(0), total: fmtBytes(size) });
        app.node()
          .then(() =>
            app.bridge.call('fetchFile', { link }, {
              onProgress: (got, total) => progress.set(got, total, 'file.progressText', { got: fmtBytes(got), total: fmtBytes(total) }),
            }),
          )
          .then(
            (got) => {
              busy = false;
              if (gone) return;
              card.replaceChildren(...result(got));
              announce(t('file.verified'));
            },
            (e) => {
              busy = false;
              errors.show(e);
              go.disabled = false;
              setText(goText, 'file.retry');
            },
          );
      });
    },
    (e) => errors.show(e),
  );

  /** @param {FetchedFiles} got */
  function result(got) {
    return [
      h(
        'p',
        { class: 'badge ok' },
        icon('check'),
        // The drawn check replaces the glyph on screen; the text keeps it for copy and search.
        h('span', { class: 'glyph', 'aria-hidden': 'true', text: '✓ ' }),
        h('span', { i18n: 'file.verified' }),
      ),
      h('p', { class: 'muted mono', i18n: 'file.hash', vars: { hash: shortHash(got.hash) } }),
      h('p', { i18n: 'file.count', vars: { n: fmtNumber(got.files.length) } }),
      h(
        'ul',
        { class: 'files' },
        ...got.files.map((f) =>
          h(
            'li',
            { class: 'row' },
            h('span', { class: 'mono', text: f.name }),
            h('span', { class: 'size', text: fmtBytes(f.bytes.byteLength) }),
            h(
              'button',
              {
                type: 'button',
                class: 'btn secondary',
                'aria-label': `${t('file.download')} ${f.name}`,
                on: { click: () => download(new Blob([/** @type {BlobPart} */ (f.bytes)]), f.name.split('/').pop() || 'file') },
              },
              icon('download'),
              h('span', { i18n: 'file.download' }),
            ),
          ),
        ),
      ),
    ];
  }

  return {
    busy: () => busy,
    cleanup: () => {
      gone = true;
    },
  };
}
