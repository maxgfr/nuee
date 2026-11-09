// @ts-check
// Group links (#/G/…): what they point to and how to take part, since a group needs the command
// line for now.
/** @typedef {import('../main').ViewContext} ViewContext */
import { h } from '../lib/dom.js';
import { errorArea, fingerprintBlock } from '../lib/ui.js';

/** @param {ViewContext} ctx */
export function render({ app, root, route }) {
  const errors = errorArea();
  const kind = 'group';
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
          fingerprintBlock(info.fingerprint, 'link.group.fingerprint'),
          h('p', { i18n: `link.${kind}.body` }),
          h('code', { class: 'mono cmd', text: 'nuee group up <link>' }),
        ),
      );
    },
    (e) => errors.show(e),
  );
  return {};
}
