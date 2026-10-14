// @ts-check
// About: what Nuée is, privacy, the security model and licenses.
/** @typedef {import('../main').ViewContext} ViewContext */
import { h } from '../lib/dom.js';

/** @param {ViewContext} ctx */
export function render({ app, root }) {
  document.documentElement.dataset.area = 'about';
  /** @param {string} id @param {string[]} keys */
  const section = (id, keys) =>
    h('section', { 'aria-labelledby': `about-${id}` }, h('h2', { id: `about-${id}`, i18n: `about.${id}.title` }), ...keys.map((k) => h('p', { i18n: `about.${id}.${k}` })));
  root.append(
    h(
      'article',
      { class: 'page narrow prose', 'aria-labelledby': 'about-title' },
      h('h1', { id: 'about-title', i18n: 'about.title' }),
      section('what', ['p1', 'p2']),
      h('p', null, h('a', { href: 'docs/', i18n: 'about.docs', i18nAttr: { href: 'nav.docsHref' } })),
      section('privacy', ['p1', 'p2', 'p3']),
      section('security', ['p1', 'p2', 'p3']),
      section('licenses', ['p1', 'p2', 'p3', 'p4']),
      h('p', { class: 'muted', i18n: 'about.version', vars: { v: app.version } }),
    ),
  );
  return {};
}
