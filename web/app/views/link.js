// @ts-check
// Service (#/X/…) and site (#/S/…) links: what they point to and how to open them today.
// The service console and the site preview replace this view in their own steps.
/** @typedef {import('../main').ViewContext} ViewContext */
import { h } from '../lib/dom.js';
import { errorArea, fingerprintBlock } from '../lib/ui.js';

/** @param {ViewContext} ctx */
export function render({ app, root, route }) {
  const errors = errorArea();
  const kind = route.view === 'site' ? 'site' : 'service';
  document.documentElement.dataset.area = kind;
  const page = h('section', { class: 'page narrow', 'aria-labelledby': 'link-title' }, h('h1', { id: 'link-title', i18n: `link.${kind}.title` }), errors.el);
  root.append(page);
  app.bridge.call('parseLink', { link: route.link ?? '' }).then(
    (info) => {
      if (info.kind !== kind) {
        errors.showKey('err.link_kind');
        return;
      }
      page.append(
        h(
          'div',
          { class: 'card' },
          fingerprintBlock(info.fingerprint, 'link.host'),
          h('p', { i18n: `link.${kind}.body` }),
          kind === 'service' ? h('code', { class: 'mono cmd', text: 'nuee connect <link>' }) : null,
        ),
      );
    },
    (e) => errors.show(e),
  );
  return {};
}
