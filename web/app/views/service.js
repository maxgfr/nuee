// @ts-check
// Service (#/X/…): a JSON console for a service exposed with `nuee expose`, over nuee/http/0.
// Responses are shown as text only (JSON pretty-printed, other text escaped by textContent,
// binary offered as a download); HTML from the service is never interpreted.
/** @typedef {import('../main').ViewContext} ViewContext */
/** @typedef {import('../api').HttpReq} HttpReq */
/** @typedef {import('../api').HttpResp} HttpResp */
import { h, setText, announce, download } from '../lib/dom.js';
import { t } from '../i18n.js';
import { errorArea, fingerprintBlock } from '../lib/ui.js';
import { fmtBytes } from '../lib/format.js';

/** Text responses longer than this are cut for display. */
export const MAX_SHOWN = 1024 * 1024;
const HISTORY = 10;
const MAX_HEADERS = 64;
const MAX_BODY = 8 * 1024 * 1024;
const METHODS = ['GET', 'POST', 'PUT', 'PATCH', 'DELETE', 'HEAD', 'OPTIONS'];

/**
 * How to show a response body.
 * @param {[string, string][]} headers
 * @param {Uint8Array} body
 * @returns {{ kind: 'json', text: string } | { kind: 'text', text: string, truncated: boolean } | { kind: 'binary', size: number }}
 */
export function classifyBody(headers, body) {
  const ct = (headers.find(([k]) => k.toLowerCase() === 'content-type')?.[1] ?? '').toLowerCase();
  const truncated = body.length > MAX_SHOWN;
  let text;
  try {
    // `stream` keeps a character cut at the display limit from counting as invalid.
    text = new TextDecoder('utf-8', { fatal: true }).decode(body.subarray(0, MAX_SHOWN), { stream: truncated });
  } catch (e) {
    if (!(e instanceof TypeError)) throw e;
    return { kind: 'binary', size: body.length };
  }
  if (/^(?:image|audio|video|font)\/|^application\/(?:octet-stream|zip|gzip|pdf|wasm)/.test(ct) || text.includes('\u0000')) {
    return { kind: 'binary', size: body.length };
  }
  if (!truncated && (ct === '' || /json/.test(ct))) {
    try {
      return { kind: 'json', text: JSON.stringify(JSON.parse(text), null, 2) };
    } catch (e) {
      if (!(e instanceof SyntaxError)) throw e;
      // Not JSON after all: shown as plain text below.
    }
  }
  return { kind: 'text', text, truncated };
}

/** @param {ViewContext} ctx */
export function render({ app, root, route }) {
  document.documentElement.dataset.area = 'service';
  const link = route.link ?? '';
  const errors = errorArea();
  const page = h('section', { class: 'page narrow', 'aria-labelledby': 'service-title' }, h('h1', { id: 'service-title', i18n: 'service.title' }), errors.el);
  root.append(page);
  let busy = false;
  /** @type {{ method: string, path: string, headers: [string, string][], body: string }[]} */
  const history = [];

  app.bridge.call('parseLink', { link }).then(
    (info) => {
      if (info.kind !== 'service') {
        errors.showKey('err.link_kind');
        return;
      }
      page.append(console_(info.fingerprint));
    },
    (e) => errors.show(e),
  );

  /** @param {string[]} fingerprint */
  function console_(fingerprint) {
    const method = h('select', { id: 'svc-method' }, ...METHODS.map((m) => h('option', { value: m, text: m })));
    const path = h('input', { type: 'text', id: 'svc-path', class: 'mono', spellcheck: 'false', props: { value: '/' } });
    const rows = h('div', { class: 'header-rows' });
    const body = h('textarea', { id: 'svc-body', class: 'mono', rows: 6, spellcheck: 'false' });
    const send = h('button', { type: 'submit', class: 'btn primary', i18n: 'service.send' });
    const out = h('div', { class: 'response', 'aria-live': 'polite' });
    const past = h('ol', { class: 'history' });

    /** @param {string} [k] @param {string} [v] */
    const addRow = (k = '', v = '') => {
      if (rows.children.length >= MAX_HEADERS) return;
      const key = h('input', { type: 'text', class: 'mono', i18nAttr: { 'aria-label': 'service.headerName', placeholder: 'service.headerName' }, props: { value: k } });
      const val = h('input', { type: 'text', class: 'mono', i18nAttr: { 'aria-label': 'service.headerValue', placeholder: 'service.headerValue' }, props: { value: v } });
      const row = h('div', { class: 'row header-row' }, key, val);
      row.append(h('button', { type: 'button', class: 'btn secondary', i18n: 'service.removeHeader', on: { click: () => row.remove() } }));
      rows.append(row);
    };

    /** @returns {[string, string][]} */
    const readHeaders = () => {
      /** @type {[string, string][]} */
      const out = [];
      for (const row of rows.children) {
        const [k, v] = row.querySelectorAll('input');
        if (k && v && k.value.trim()) out.push([k.value.trim(), v.value]);
      }
      return out;
    };

    const form = h(
      'form',
      { class: 'card console', novalidate: true },
      fingerprintBlock(fingerprint, 'link.host'),
      h('div', { class: 'row' }, h('label', { for: 'svc-method', i18n: 'service.method' }), method, h('label', { for: 'svc-path', i18n: 'service.path' }), path),
      h('fieldset', null, h('legend', { i18n: 'service.headers' }), rows, h('button', { type: 'button', class: 'btn secondary', i18n: 'service.addHeader', on: { click: () => addRow() } })),
      h('div', { class: 'field' }, h('label', { for: 'svc-body', i18n: 'service.body' }), body),
      send,
      h('p', { class: 'muted' }, h('span', { i18n: 'service.fullSites' }), ' ', h('code', { class: 'mono', text: 'nuee connect <link>' })),
    );
    form.addEventListener('submit', (e) => {
      e.preventDefault();
      if (busy) return;
      errors.clear();
      const m = method.value;
      const p = path.value.trim() || '/';
      if (!p.startsWith('/')) {
        errors.showKey('service.badPath');
        return;
      }
      const headers = readHeaders();
      const text = body.value;
      /** @type {HttpReq} */
      const req = { method: m, path: p, headers };
      if (text.trim() !== '') {
        try {
          JSON.parse(text);
        } catch (err) {
          if (!(err instanceof SyntaxError)) throw err;
          errors.showKey('service.badJson');
          return;
        }
        const bytes = new TextEncoder().encode(text);
        if (bytes.length > MAX_BODY) {
          errors.showKey('err.too_large');
          return;
        }
        req.body = bytes;
        if (!headers.some(([k]) => k.toLowerCase() === 'content-type')) headers.push(['content-type', 'application/json']);
      }
      history.unshift({ method: m, path: p, headers: readHeaders(), body: text });
      history.length = Math.min(history.length, HISTORY);
      showHistory();
      busy = true;
      send.disabled = true;
      setText(send, 'service.sending');
      const started = performance.now();
      app.node()
        .then(() => app.bridge.call('http', { link, req }))
        .then(
          (resp) => {
            out.replaceChildren(...response(resp, Math.round(performance.now() - started)));
            announce(t('service.got', { status: resp.status }));
          },
          (err) => errors.show(err),
        )
        .finally(() => {
          busy = false;
          send.disabled = false;
          setText(send, 'service.send');
        });
    });

    function showHistory() {
      past.replaceChildren(
        ...history.map((r) =>
          h(
            'li',
            null,
            h('button', {
              type: 'button',
              class: 'btn secondary mono',
              text: `${r.method} ${r.path}`,
              on: {
                click: () => {
                  method.value = r.method;
                  path.value = r.path;
                  body.value = r.body;
                  rows.replaceChildren();
                  for (const [k, v] of r.headers) addRow(k, v);
                },
              },
            }),
          ),
        ),
      );
    }

    return h(
      'div',
      null,
      form,
      h('section', { class: 'card', 'aria-labelledby': 'svc-resp' }, h('h2', { id: 'svc-resp', i18n: 'service.response' }), out),
      h('section', { class: 'card', 'aria-labelledby': 'svc-hist' }, h('h2', { id: 'svc-hist', i18n: 'service.history' }), h('p', { class: 'muted', i18n: 'service.historyNote' }), past),
    );
  }

  /** @param {HttpResp} resp @param {number} ms */
  function response(resp, ms) {
    const shown = classifyBody(resp.headers, resp.body);
    /** @type {HTMLElement} */
    let bodyEl;
    if (shown.kind === 'binary') {
      bodyEl = h(
        'p',
        null,
        h('span', { i18n: 'service.binary', vars: { size: fmtBytes(shown.size) } }),
        ' ',
        h('button', {
          type: 'button',
          class: 'btn secondary',
          i18n: 'file.download',
          on: { click: () => download(new Blob([/** @type {BlobPart} */ (resp.body)], { type: 'application/octet-stream' }), 'response.bin') },
        }),
      );
    } else {
      bodyEl = h('div', null, shown.kind === 'text' && shown.truncated ? h('p', { class: 'notice', i18n: 'service.truncated', vars: { size: fmtBytes(MAX_SHOWN) } }) : null, h('pre', { class: 'mono output-text', text: shown.text }));
    }
    return [
      h('p', { class: resp.status < 400 ? 'badge ok' : 'alert', i18n: 'service.status', vars: { status: resp.status, ms } }),
      h('details', null, h('summary', { i18n: 'service.respHeaders', vars: { n: resp.headers.length } }), h('pre', { class: 'mono output-text', text: resp.headers.map(([k, v]) => `${k}: ${v}`).join('\n') })),
      bodyEl,
    ];
  }

  return { busy: () => busy };
}
