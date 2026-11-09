// @ts-check
// Group links (#/G/…): what they point to and the commands that take part, since a group needs
// the command line for now.
/** @typedef {import('../main').ViewContext} ViewContext */
import { h, copyButton } from '../lib/dom.js';
import { errorArea, fingerprintBlock } from '../lib/ui.js';

const BUILD = 'npm ci && npm run build';

/** Each step, in the order a newcomer needs them. `-` makes the command ask for the link. */
const STEPS = [
  ['link.group.join', 'nuee id'],
  ['link.group.up', `nuee group up - --allow-repo https://github.com/you/app.git --allow-command '${BUILD}' --exec-network`],
  ['link.group.build', `nuee build --on - --keep dist -- '${BUILD}'`],
];

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
          h('div', { class: 'row' }, copyButton(() => location.href, 'link.group.copy')),
          h(
            'ol',
            { class: 'group-steps' },
            STEPS.map(([key, command]) =>
              h('li', null, h('p', { i18n: key }), h('div', { class: 'row' }, h('code', { class: 'mono cmd', text: command }), copyButton(() => command))),
            ),
          ),
          h('p', null, h('a', { href: 'docs/#ex-try', i18n: 'link.group.docs' })),
        ),
      );
    },
    (e) => errors.show(e),
  );
  return {};
}
