// @ts-check
// Pieces shared by several views.
/** @typedef {import('../main').App} App */
import { h, alertBox, copyButton, qrImage, setText, icon } from './dom.js';
import { codeOf } from './bridge.js';

/** The i18n key of an error. @param {unknown} e */
export function errorKey(e) {
  return `err.${codeOf(e)}`;
}

/**
 * A place for one error at a time.
 * @returns {{ el: HTMLElement, show: (e: unknown) => void, showKey: (key: string) => void, clear: () => void }}
 */
export function errorArea() {
  const el = h('div', { class: 'errors' });
  return {
    el,
    show(e) {
      const key = errorKey(e);
      if (key === 'err.internal' || key === 'err.invalid_argument') console.error(e);
      el.replaceChildren(alertBox(key));
    },
    showKey(key) {
      el.replaceChildren(alertBox(key));
    },
    clear() {
      el.replaceChildren();
    },
  };
}

/** Shown instead of action buttons when the page is inside a frame (anti-clickjacking). */
export function framedNotice() {
  return h(
    'div',
    { class: 'notice', role: 'note' },
    h('p', { i18n: 'framed.body' }),
    h('a', { href: location.href, target: '_blank', rel: 'noopener noreferrer', class: 'btn', i18n: 'framed.open' }),
  );
}

/**
 * The six-emoji fingerprint, large, with an accessible label.
 * @param {string[]} emoji
 * @param {string} labelKey
 */
export function fingerprintBlock(emoji, labelKey) {
  return h(
    'figure',
    { class: 'fingerprint' },
    h('figcaption', { i18n: labelKey }),
    // One span per symbol so they can sit in a row or a 3×2 block; the spaces keep the text
    // identical to fmtFingerprint().
    h('p', { class: 'emoji', role: 'img', 'aria-label': emoji.join(', ') }, ...emoji.flatMap((e, i) => (i === 0 ? [h('span', { text: e })] : [' ', h('span', { text: e })]))),
  );
}

/**
 * The link to hand out: text with a copy button, a QR code, and a presentation mode with the
 * QR full screen next to the fingerprint (and `extra`, such as a live count, under it).
 * @param {App} app
 * @param {string} link
 * @param {string[] | null} fingerprint
 * @param {HTMLElement | null} [extra]
 * @param {string | null} [hintKey] i18n key of a short hint under the link's label
 * @returns {Promise<{ el: HTMLElement, dispose: () => void }>}
 */
export async function linkPanel(app, link, fingerprint, extra = null, hintKey = null) {
  const svg = await app.bridge.call('qrSvg', { text: link });
  const qr = qrImage(svg, 'link.qrAlt');
  const big = h('img', { src: qr.img.src, class: 'qr qr-big', i18nAttr: { alt: 'link.qrAlt' } });
  const field = h('input', { type: 'text', readonly: true, class: 'mono', id: 'share-link', 'aria-describedby': hintKey ? 'share-link-hint' : undefined, props: { value: link } });
  const exit = h('button', { type: 'button', class: 'btn', i18n: 'link.exitPresent' });
  // A modal dialog keeps focus inside while the room reads the QR. The page goes full screen
  // first and the dialog opens over it (full screen inside a modal dialog leaves Chromium's
  // page unclickable once both are closed).
  const stage = h(
    'div',
    { class: 'present-stage' },
    big,
    fingerprint ? fingerprintBlock(fingerprint, 'link.fingerprint') : null,
    h('p', { class: 'present-link', text: link }),
    extra,
    exit,
  );
  const dialog = h('dialog', { class: 'present', i18nAttr: { 'aria-label': 'link.present' } }, stage);
  // Full screen ends first, then the dialog closes and focus returns to the button.
  const close = () => {
    const shut = () => {
      if (dialog.open) dialog.close();
    };
    if (document.fullscreenElement === document.documentElement) {
      document.exitFullscreen().then(shut, (e) => {
        console.warn('exit fullscreen', e);
        shut();
      });
    } else shut();
  };
  dialog.addEventListener('cancel', (e) => {
    e.preventDefault();
    close();
  });
  dialog.addEventListener('close', () => presentBtn.focus());
  const presentBtn = h(
    'button',
    {
      type: 'button',
      class: 'btn secondary',
      on: {
        click: () => {
          const open = () => {
            if (dialog.open) return;
            dialog.showModal();
            exit.focus();
          };
          const root = document.documentElement;
          if (!root.requestFullscreen) open();
          else
            root.requestFullscreen().then(open, (e) => {
              console.warn('fullscreen refused', e);
              open();
            });
        },
      },
    },
    icon('fullscreen'),
    h('span', { i18n: 'link.present' }),
  );
  exit.addEventListener('click', close);
  // Escape closes the dialog (its cancel event); leaving full screen closes it too.
  const onFs = () => {
    if (!document.fullscreenElement && dialog.open) close();
  };
  document.addEventListener('fullscreenchange', onFs);
  const el = h(
    'div',
    { class: 'link-panel' },
    qr.img,
    h(
      'div',
      { class: 'link-text' },
      h('label', { for: 'share-link', i18n: 'link.label' }),
      hintKey ? h('p', { class: 'muted hint', id: 'share-link-hint', i18n: hintKey }) : null,
      field,
      h('div', { class: 'row' }, copyButton(() => link), presentBtn),
    ),
    dialog,
  );
  field.addEventListener('focus', () => field.select());
  return {
    el,
    dispose() {
      document.removeEventListener('fullscreenchange', onFs);
      qr.dispose();
    },
  };
}

/**
 * A labelled progress bar with a text line under it.
 * @param {string} labelKey
 */
export function progressBar(labelKey) {
  const id = `p${Math.random().toString(36).slice(2, 8)}`;
  const bar = h('progress', { id, max: 1, props: { value: 0 } });
  const text = h('p', { class: 'muted' });
  const el = h('div', { class: 'progress' }, h('label', { for: id, i18n: labelKey }), bar, text);
  return {
    el,
    /** @param {number} done @param {number} total @param {string} key @param {Record<string, string | number>} vars */
    set(done, total, key, vars) {
      bar.max = Math.max(total, 1);
      bar.value = Math.min(done, bar.max);
      setText(text, key, vars);
    },
    indeterminate() {
      bar.removeAttribute('value');
    },
  };
}

/** Wake Lock while work runs; reacquired when the page becomes visible again. */
export function wakeLock() {
  /** @type {{ release(): Promise<void> } | null} */
  let sentinel = null;
  let wanted = false;
  const nav = /** @type {{ wakeLock?: { request(type: 'screen'): Promise<{ release(): Promise<void> }> } }} */ (
    /** @type {unknown} */ (navigator)
  );
  async function acquire() {
    if (!nav.wakeLock || sentinel || document.hidden) return;
    try {
      sentinel = await nav.wakeLock.request('screen');
    } catch (e) {
      console.warn('wake lock refused', e);
    }
  }
  const onVisible = () => {
    if (document.hidden) sentinel = null;
    else if (wanted) acquire();
  };
  document.addEventListener('visibilitychange', onVisible);
  return {
    /** @param {boolean} on */
    set(on) {
      wanted = on;
      if (on) acquire();
      else if (sentinel) {
        const s = sentinel;
        sentinel = null;
        s.release().catch((e) => console.warn('wake lock release', e));
      }
    },
    dispose() {
      this.set(false);
      document.removeEventListener('visibilitychange', onVisible);
    },
  };
}
