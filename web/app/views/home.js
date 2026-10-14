// @ts-check
// Home: the promise, four doors, the CLI install line and the murmuration demo.
/** @typedef {import('../main').ViewContext} ViewContext */
import { h, append, copyButton } from '../lib/dom.js';
import { extractLink, hashOf } from '../router.js';
import { murmuration, demoBirds } from '../lib/murmuration.js';
import { errorArea } from '../lib/ui.js';

const BREW = 'brew install maxgfr/tap/nuee';

/** @param {ViewContext} ctx */
export function render({ app, root, route }) {
  document.documentElement.dataset.area = 'home';
  const errors = errorArea();
  const input = h('input', {
    type: 'text',
    id: 'open-link',
    class: 'mono',
    autocomplete: 'off',
    autocapitalize: 'off',
    spellcheck: 'false',
    'aria-describedby': 'open-help',
  });
  const form = h(
    'form',
    { class: 'open-form', novalidate: true },
    h('p', { id: 'open-help', class: 'muted', i18n: 'open.help' }),
    h('div', { class: 'row' }, input, h('button', { type: 'submit', class: 'btn', i18n: 'open.submit' })),
    errors.el,
  );
  form.addEventListener('submit', (e) => {
    e.preventDefault();
    errors.clear();
    const found = extractLink(input.value);
    if (!found) {
      errors.showKey('open.notNuee');
      return;
    }
    if (found.kind === 'P') {
      errors.showKey('open.pinLink');
      return;
    }
    app.bridge.call('parseLink', { link: found.link }).then(
      () => {
        location.hash = hashOf(found.link);
      },
      (err) => errors.show(err),
    );
  });

  const canvas = h('canvas', { class: 'flock', 'aria-hidden': 'true' });
  // append() skips the missing alert; Element.append would print "null".
  append(root, [
    h(
      'section',
      { class: 'hero', 'aria-labelledby': 'home-title' },
      h('h1', { id: 'home-title', i18n: 'home.title' }),
      h('p', { class: 'lead', i18n: 'home.promise' }),
      canvas,
    ),
    route.linkError ? h('div', { class: 'page' }, h('div', { class: 'alert', role: 'alert', i18n: `err.${route.linkError}` })) : null,
    h(
      'section',
      { class: 'doors', 'aria-labelledby': 'doors-title' },
      h('h2', { id: 'doors-title', class: 'visually-hidden', i18n: 'home.doors' }),
      h(
        'ul',
        { class: 'door-list' },
        h('li', { class: 'door door-run' }, h('h3', null, h('a', { href: '#/run', i18n: 'door.run.title' })), h('p', { i18n: 'door.run.body' })),
        h(
          'li',
          { class: 'door door-join' },
          h('h3', null, h('a', { href: '#open-link', i18n: 'door.join.title', on: { click: (e) => { e.preventDefault(); input.focus(); } } })),
          h('p', { i18n: 'door.join.body' }),
        ),
        h('li', { class: 'door door-share' }, h('h3', null, h('a', { href: '#/share', i18n: 'door.share.title' })), h('p', { i18n: 'door.share.body' })),
        h('li', { class: 'door door-open' }, h('h3', { class: 'door-label' }, h('label', { for: 'open-link', i18n: 'door.open.title' })), form),
      ),
    ),
    h(
      'section',
      { class: 'cli', 'aria-labelledby': 'cli-title' },
      h('h2', { id: 'cli-title', i18n: 'home.cli.title' }),
      h('p', { i18n: 'home.cli.body' }),
      h('div', { class: 'row' }, h('code', { class: 'mono cmd', text: BREW }), copyButton(() => BREW)),
      h('p', { class: 'cli-docs' }, h('a', { href: 'docs/', i18n: 'home.cli.docs', i18nAttr: { href: 'nav.docsHref' } })),
    ),
  ]);
  const flock = murmuration(canvas, { birds: demoBirds(160) });
  return { cleanup: () => flock.stop() };
}
