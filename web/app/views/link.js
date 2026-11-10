// @ts-check
// Group links (#/G/…): what the group looks like now, from the keeper that coordinates it, and
// the commands that take part, since a machine joins a group from the command line.
/** @typedef {import('../main').ViewContext} ViewContext */
/** @typedef {import('../api').GroupOverview} GroupOverview */
import { h, copyButton, setText } from '../lib/dom.js';
import { t } from '../i18n.js';
import { errorArea, fingerprintBlock } from '../lib/ui.js';
import { fmtNumber } from '../lib/format.js';

const BUILD = 'npm ci && npm run build';

/** Each step, in the order a newcomer needs them. `-` makes the command ask for the link. */
const STEPS = [
  ['link.group.join', 'nuee id'],
  ['link.group.up', `nuee group up - --allow-repo https://github.com/you/app.git --allow-command '${BUILD}' --exec-network`],
  ['link.group.build', `nuee build --on - --keep dist -- '${BUILD}'`],
];

/** How often the page asks again, while it is open. */
const REFRESH_MS = 10_000;

/** Jobs shown, newest first. */
const SHOWN_JOBS = 10;

/** @param {ViewContext} ctx */
export function render({ app, root, route }) {
  const errors = errorArea();
  const kind = 'group';
  const link = route.link ?? '';
  document.documentElement.dataset.area = kind;
  const page = h('section', { class: 'page narrow', 'aria-labelledby': 'link-title' }, h('h1', { id: 'link-title', i18n: `link.${kind}.title` }), errors.el);
  root.append(page);
  /** @type {ReturnType<typeof setTimeout> | undefined} */
  let timer;
  let gone = false;

  app.bridge.call('parseLink', { link }).then(
    (info) => {
      if (info.kind !== kind) {
        errors.showKey('err.link_kind');
        return;
      }
      const live = liveStatus();
      page.append(
        live.el,
        h(
          'div',
          { class: 'card' },
          fingerprintBlock(info.fingerprint, 'link.group.fingerprint'),
          h('p', { i18n: `link.${kind}.body` }),
          h('div', { class: 'row' }, copyButton(() => location.href, 'link.group.copy')),
          h('h2', { i18n: 'link.group.take' }),
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
      const ask = () => {
        app
          .node()
          .then(() => app.bridge.call('groupOverview', { link }))
          .then(
            (o) => live.show(o),
            () => live.unreachable(),
          )
          .finally(() => {
            if (!gone) timer = setTimeout(ask, REFRESH_MS);
          });
      };
      ask();
    },
    (e) => errors.show(e),
  );
  return {
    cleanup: () => {
      gone = true;
      clearTimeout(timer);
    },
  };
}

/** The section that shows the group now, filled by each answer. */
function liveStatus() {
  const summary = h('p', { class: 'muted', role: 'status', i18n: 'link.group.asking' });
  const body = h('div');
  const updated = h('p', { class: 'muted' });
  const el = h('section', { class: 'card group-live', 'aria-labelledby': 'group-live-title' }, h('h2', { id: 'group-live-title', i18n: 'link.group.now' }), summary, body, updated);

  /** @param {GroupOverview} o */
  const show = (o) => {
    const workers = o.members.filter((m) => m.online !== null);
    const online = workers.filter((m) => m.online).length;
    setText(summary, 'link.group.summary', {
      online: fmtNumber(online),
      total: fmtNumber(workers.length),
      running: fmtNumber(o.running),
      files: fmtNumber(o.files),
    });
    const parts = [machines(o)];
    if (o.jobs.length > 0) parts.push(jobs(o));
    const named = o.sites.map((s) => s.name).filter((n) => typeof n === 'string');
    if (named.length > 0) parts.push(h('p', null, h('span', { i18n: 'link.group.sites' }), ' ', h('span', { class: 'mono', text: named.join(', ') })));
    body.replaceChildren(...parts);
    setText(updated, 'link.group.updated', { time: new Date().toLocaleTimeString() });
  };

  const unreachable = () => {
    setText(summary, 'link.group.unreachable');
    body.replaceChildren();
    setText(updated, 'link.group.updated', { time: new Date().toLocaleTimeString() });
  };

  return { el, show, unreachable };
}

/** @param {GroupOverview} o */
function machines(o) {
  return h(
    'table',
    { class: 'peers group-machines' },
    h('caption', { i18n: 'link.group.machines' }),
    h(
      'thead',
      null,
      h(
        'tr',
        null,
        h('th', { scope: 'col', i18n: 'link.group.col.name' }),
        h('th', { scope: 'col', i18n: 'link.group.col.role' }),
        h('th', { scope: 'col', i18n: 'link.group.col.state' }),
        h('th', { scope: 'col', class: 'num', i18n: 'link.group.col.slots' }),
        h('th', { scope: 'col', class: 'num', i18n: 'link.group.col.working' }),
      ),
    ),
    h(
      'tbody',
      null,
      ...o.members.map((m) =>
        h(
          'tr',
          { class: m.online === false ? 'off' : '' },
          h('th', { scope: 'row', text: m.name }),
          h('td', { i18n: `link.group.role.${m.role}` }),
          h('td', { i18n: m.online === null ? 'link.group.state.none' : m.online ? 'link.group.state.on' : 'link.group.state.off' }),
          h('td', { class: 'num', text: m.slots === null ? '' : fmtNumber(m.slots) }),
          h('td', { class: 'num', text: m.working === null ? '' : fmtNumber(m.working) }),
        ),
      ),
    ),
  );
}

/** @param {GroupOverview} o */
function jobs(o) {
  return h(
    'table',
    { class: 'peers group-jobs' },
    h('caption', { i18n: 'link.group.jobs' }),
    h(
      'thead',
      null,
      h(
        'tr',
        null,
        h('th', { scope: 'col', class: 'num', i18n: 'link.group.col.job' }),
        h('th', { scope: 'col', i18n: 'link.group.col.name' }),
        h('th', { scope: 'col', i18n: 'link.group.col.state' }),
        h('th', { scope: 'col', class: 'num', i18n: 'link.group.col.tasks' }),
      ),
    ),
    h(
      'tbody',
      null,
      ...o.jobs.slice(0, SHOWN_JOBS).map((j) =>
        h(
          'tr',
          null,
          h('td', { class: 'num', text: `#${j.job}` }),
          h('th', { scope: 'row', class: 'mono', text: j.name }),
          h('td', { text: j.state === 'running' ? t('link.group.job.running') : j.failed > 0 ? t('link.group.job.failed', { n: fmtNumber(j.failed) }) : t('link.group.job.done') }),
          h('td', { class: 'num', text: `${fmtNumber(j.done)}/${fmtNumber(j.tasks)}` }),
        ),
      ),
    ),
  );
}
