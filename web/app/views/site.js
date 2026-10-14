// @ts-check
// Site (#/S/…): fetch and verify the files, then show a static, inert preview. Nothing from
// the site runs in this origin: the document is rebuilt by lib/inline.js (no scripts, no
// handlers, no navigation) and shown in an iframe whose sandbox grants nothing, through
// srcdoc, so it also inherits this page's CSP.
/** @typedef {import('../main').ViewContext} ViewContext */
/** @typedef {import('../api').FetchedFiles} FetchedFiles */
import { h, setText, announce, download, icon } from '../lib/dom.js';
import { t } from '../i18n.js';
import { errorArea, fingerprintBlock, progressBar } from '../lib/ui.js';
import { fmtBytes, fmtNumber, shortHash } from '../lib/format.js';
import { buildPreview, tarFiles } from '../lib/inline.js';

/** Download cap of a site, as for files. */
const MAX_SITE = 1024 * 1024 * 1024;

/**
 * Attributes of the preview frame. The sandbox is empty: no scripts, no same origin, no
 * forms, no popups, no top navigation.
 * @returns {Record<string, string>}
 */
export function previewFrameAttrs() {
  return {
    sandbox: '',
    class: 'preview-frame',
    referrerpolicy: 'no-referrer',
    loading: 'lazy',
  };
}

/** The preview iframe. `srcdoc` is the HTML policy's output for an inline.js document. @param {unknown} srcdoc */
export function previewFrame(srcdoc) {
  const frame = h('iframe', { ...previewFrameAttrs(), i18nAttr: { title: 'site.frameTitle' } });
  // dom.js refuses srcdoc as an attribute on purpose; this is the one place that sets it,
  // with a value that went through the page's Trusted Types policy.
  Reflect.set(frame, 'srcdoc', srcdoc);
  return frame;
}

/** @param {ViewContext} ctx */
export function render({ app, root, route }) {
  document.documentElement.dataset.area = 'site';
  const link = route.link ?? '';
  const errors = errorArea();
  const page = h('section', { class: 'page', 'aria-labelledby': 'site-title' }, h('h1', { id: 'site-title', i18n: 'site.title' }), errors.el);
  root.append(page);
  let busy = false;
  let gone = false;

  app.bridge.call('parseLink', { link }).then(
    (info) => {
      if (info.kind !== 'site') {
        errors.showKey('err.link_kind');
        return;
      }
      const size = info.size ?? 0;
      const go = h('button', { type: 'button', class: 'btn primary', i18n: 'site.fetch' });
      const progress = progressBar('file.progress');
      progress.el.hidden = true;
      const card = h(
        'div',
        { class: 'card narrow' },
        fingerprintBlock(info.fingerprint, 'link.host'),
        h('p', { i18n: 'site.about', vars: { size: fmtBytes(size) } }),
        progress.el,
        go,
        dynamicHint(),
      );
      page.append(card);
      if (size > MAX_SITE) {
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
            app.bridge.call('fetchSite', { link }, {
              onProgress: (got, total) => progress.set(got, total, 'file.progressText', { got: fmtBytes(got), total: fmtBytes(total) }),
            }),
          )
          .then(
            (got) => {
              busy = false;
              if (gone) return;
              card.replaceChildren(...summary(got));
              showPreview(got);
            },
            (e) => {
              busy = false;
              errors.show(e);
              go.disabled = false;
              setText(go, 'file.retry');
            },
          );
      });
    },
    (e) => errors.show(e),
  );

  /** @param {FetchedFiles} got */
  function summary(got) {
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
      h('button', {
        type: 'button',
        class: 'btn secondary',
        i18n: 'site.downloadTar',
        on: { click: () => download(new Blob([/** @type {BlobPart} */ (tarFiles(got.files, Math.floor(Date.now() / 1000)))], { type: 'application/x-tar' }), 'site.tar') },
      }),
      dynamicHint(),
    ];
  }

  /** @param {FetchedFiles} got */
  function showPreview(got) {
    let doc;
    try {
      doc = buildPreview(got.files);
    } catch (e) {
      errors.show(e);
      return;
    }
    page.append(
      h(
        'figure',
        { class: 'preview' },
        h('figcaption', { class: 'notice', role: 'note', i18n: 'site.banner' }),
        previewFrame(app.previewHtml(doc.html)),
        h('p', { class: 'muted mono', i18n: 'site.page', vars: { page: doc.page } }),
      ),
    );
    announce(t('site.ready'));
  }

  return {
    busy: () => busy,
    cleanup: () => {
      gone = true;
    },
  };
}

function dynamicHint() {
  return h('p', { class: 'muted' }, h('span', { i18n: 'site.dynamic' }), ' ', h('code', { class: 'mono', text: 'nuee connect <link>' }));
}
