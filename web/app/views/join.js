// @ts-check
// Join (#/J/…), mobile first: a consent card, then a gauge with Pause/Stop and a thank-you.
/** @typedef {import('../main').ViewContext} ViewContext */
/** @typedef {import('../api').NueeEvent} NueeEvent */
/** @typedef {import('../api').LinkInfo} LinkInfo */
/** @typedef {import('../api').ByeReason} ByeReason */
import { h, append, setText, announce, alertBox, icon, focusOn } from '../lib/dom.js';
import { jobKey, rejectKey, endedTone } from '../lib/labels.js';
import { t } from '../i18n.js';
import { errorArea, framedNotice, fingerprintBlock, wakeLock } from '../lib/ui.js';
import { fmtNumber, fmtBytes } from '../lib/format.js';
import { defaultCores, offeredMemMb, supportedKinds } from '../lib/pool.js';
import { gpuCaps } from '../worker/gpu.js';
import { DOWNLOAD_BYTES, preloadLlm } from '../worker/llm.js';
import { joinLane } from '../lib/rtc.js';

/** @type {Record<ByeReason, string>} */
const ENDED = {
  done: 'join.ended.done',
  banned: 'join.ended.banned',
  shutdown: 'join.ended.shutdown',
  rejected: 'join.ended.rejected',
  full: 'join.ended.full',
};

/** @param {ViewContext} ctx */
export function render({ app, root, route }) {
  document.documentElement.dataset.area = 'join';
  const link = route.link ?? '';
  const errors = errorArea();
  const page = h('section', { class: 'page narrow', 'aria-labelledby': 'join-title' }, h('h1', { id: 'join-title', i18n: 'join.title' }), errors.el);
  root.append(page);

  /** @type {null | (() => void)} */
  let teardown = null;
  let active = false;
  // Scoring with the language model downloads it first: opt-in on the consent card.
  let useLlm = false;

  app.bridge.call('parseLink', { link }).then(
    (info) => {
      if (info.kind !== 'job') {
        errors.showKey('err.link_kind');
        return;
      }
      consent(info);
    },
    (e) => errors.show(e),
  );

  /** @param {LinkInfo} info */
  function consent(info) {
    const cores = app.caps.cores;
    const slider = h('input', { type: 'range', id: 'cores', min: 1, max: cores, step: 1, props: { value: defaultCores(cores) }, 'aria-describedby': 'cores-value' });
    const coresValue = h('output', { id: 'cores-value', for: 'cores' });
    const showCores = () => setText(coresValue, 'join.coresValue', { n: slider.value, total: cores });
    slider.addEventListener('input', showCores);
    showCores();
    const ac = app.caps.battery ? h('input', { type: 'checkbox', id: 'ac' }) : null;
    // GPU tasks need a wgsl executor; the toggle appears once one is registered.
    const gpuReady = app.caps.webgpu && supportedKinds().includes('wgsl');
    const gpu = gpuReady ? h('input', { type: 'checkbox', id: 'gpu', props: { checked: true } }) : null;
    const llm = supportedKinds().includes('llm') ? h('input', { type: 'checkbox', id: 'llm' }) : null;
    // Direct lane to the presenter: opt-in, only used when the presenter turned WebRTC on.
    const direct = typeof RTCPeerConnection === 'function' ? h('input', { type: 'checkbox', id: 'direct', 'aria-describedby': 'direct-warn' }) : null;
    const go = h('button', { type: 'button', class: 'btn primary big', i18n: 'join.contribute' });
    const card = h(
      'div',
      { class: 'card consent' },
      h('h2', { i18n: 'join.who' }),
      fingerprintBlock(info.fingerprint, 'join.fingerprint'),
      h('p', { class: 'muted', i18n: 'join.compare' }),
      h('h2', { i18n: 'join.what' }),
      h('p', { i18n: 'join.whatBody' }),
      h(
        'ul',
        { class: 'facts' },
        h('li', { class: 'can' }, icon('check'), h('span', { i18n: 'join.can' })),
        h('li', { class: 'cannot' }, icon('cross'), h('span', { i18n: 'join.cannot' })),
        h('li', { class: 'can' }, icon('check'), h('span', { i18n: 'join.stopAny' })),
      ),
      h('h2', { i18n: 'join.howMuch' }),
      h('div', { class: 'field' }, h('label', { for: 'cores', i18n: 'join.cores' }), slider, coresValue),
      gpu ? h('div', { class: 'check' }, gpu, h('label', { for: 'gpu', i18n: 'join.gpu' })) : null,
      llm ? h('div', { class: 'check' }, llm, h('label', { for: 'llm', i18n: 'join.llm', vars: { size: fmtBytes(DOWNLOAD_BYTES) } })) : null,
      ac ? h('div', { class: 'check' }, ac, h('label', { for: 'ac', i18n: 'join.ac' })) : null,
      direct ? h('div', { class: 'check' }, direct, h('label', { for: 'direct', i18n: 'join.rtc' })) : null,
      direct ? h('p', { id: 'direct-warn', class: 'muted', i18n: 'join.rtc.warn' }) : null,
      app.caps.framed ? framedNotice() : go,
    );
    page.append(card);
    go.addEventListener('click', () => {
      go.disabled = true;
      setText(go, 'join.connecting');
      errors.clear();
      useLlm = llm ? llm.checked : false;
      if (useLlm) preloadLlm().catch((e) => console.warn('llm', e));
      contribute(Number(slider.value), ac ? ac.checked : false, gpu ? gpu.checked : false, direct ? direct.checked : false).then(
        (status) => {
          card.remove();
          focusOn(status);
        },
        (e) => {
          errors.show(e);
          go.disabled = false;
          setText(go, 'join.contribute');
          if (teardown) teardown();
        },
      );
    });
  }

  /**
   * @param {number} slots
   * @param {boolean} acOnly
   * @param {boolean} useGpu
   * @param {boolean} wantLane
   * @returns {Promise<HTMLElement>} the status line, where focus goes once the work started
   */
  async function contribute(slots, acOnly, useGpu, wantLane) {
    await app.node();
    const pool = app.pool(slots);
    const lock = wakeLock();
    const device = pool.device;
    const stats = { tasks: 0, ms: 0, running: 0, refused: 0 };
    let paused = false;
    let batteryPaused = false;
    // With the direct lane accepted, work waits until the lane is settled (open or not).
    let laneWait = wantLane;
    /** @type {{ close(): void } | null} */
    let lane = null;
    /** @type {number | null} */
    let session = null;
    /** @type {Array<{ event: NueeEvent, s: number | undefined }>} */
    const early = [];
    /** @type {(() => void) | null} */
    let unBattery = null;

    const status = h('p', { class: 'state', i18n: 'join.state.connecting' });
    const gauge = h('meter', { id: 'gauge', min: 0, max: slots, value: 0 });
    const gaugeLabel = h('label', { for: 'gauge', i18n: 'join.gauge' });
    const done = h('p', { class: 'big-number' });
    // This device's own squares: outlined while a task runs, inked in when it is done.
    const mine = h('ol', { class: 'task-grid mine', 'aria-hidden': 'true' });
    /** @type {Map<string, HTMLElement>} */
    const squares = new Map();
    const MAX_SQUARES = 240;
    let started = 0;
    const jobs = h('ul', { class: 'facts' });
    const pauseBtn = h('button', { type: 'button', class: 'btn secondary', i18n: 'join.pause' });
    const stopBtn = h('button', { type: 'button', class: 'btn danger' }, icon('stop'), h('span', { i18n: 'join.stop' }));
    const panel = h(
      'div',
      { class: 'card contributing' },
      status,
      gaugeLabel,
      gauge,
      done,
      h('h2', { i18n: 'join.myTasks' }),
      mine,
      h('h2', { i18n: 'join.jobs' }),
      jobs,
      h('div', { class: 'row' }, pauseBtn, stopBtn),
    );
    const laneNote = h('p', { class: 'muted', 'aria-live': 'polite', hidden: true });
    const refresh = () => {
      // The bar shows the tasks in flight, and only while there is one.
      gauge.value = Math.min(stats.running, slots);
      gauge.hidden = stats.running === 0;
      gaugeLabel.hidden = stats.running === 0;
      setText(done, 'join.done', { n: fmtNumber(stats.tasks), s: fmtNumber(stats.ms / 1000, 1) });
    };
    refresh();

    /** @param {number} n */
    const setSlots = (n) => {
      pool.setSlots(n);
      if (session !== null) app.bridge.call('setSlots', { session, n }).catch((e) => console.warn('setSlots', e));
      lock.set(n > 0);
    };
    const applyPause = () => {
      const off = paused || batteryPaused;
      setSlots(off || laneWait ? 0 : slots);
      setText(status, off ? (batteryPaused && !paused ? 'join.state.battery' : 'join.state.paused') : 'join.state.working');
      setText(pauseBtn, paused ? 'join.resume' : 'join.pause');
    };

    /** @param {string} key @param {boolean} [thanks] */
    const finish = (key, thanks = false) => {
      if (!active) return;
      teardown?.();
      // The squares stay in place (never detached), so finished ones do not ink in again.
      mine.classList.add('settled');
      for (const c of [...panel.children]) if (c !== mine) c.remove();
      if (stats.tasks === 0) mine.remove();
      // Only a refusal or a ban is an alert; a stop, a full or ended swarm is written plainly.
      const tone = endedTone(key);
      const ended = key === 'join.ended.done' ? null : tone === 'alert' ? alertBox(key) : h('p', { class: tone === 'notice' ? 'notice' : undefined, i18n: key });
      const head = [
        thanks ? h('h2', { i18n: 'join.thanks' }) : null,
        ended,
        h('p', { i18n: 'join.summary', vars: { n: fmtNumber(stats.tasks), s: fmtNumber(stats.ms / 1000, 1) } }),
      ];
      if (mine.parentNode === panel) for (const n of head) if (n) panel.insertBefore(n, mine);
      if (mine.parentNode !== panel) append(panel, head);
      panel.append(h('a', { href: '#/', class: 'btn secondary', i18n: 'join.home' }));
      announce(t(thanks ? 'join.thanks' : key));
      focusOn(/** @type {HTMLElement} */ (head.find((n) => n !== null)));
    };

    /** @param {NueeEvent} e */
    const onEvent = (e) => {
      switch (e.type) {
        case 'pending':
          setText(status, 'join.state.pending');
          announce(t('join.state.pending'));
          break;
        case 'welcome':
          setText(status, 'join.state.working');
          announce(t('join.state.working'));
          if (wantLane && e.swarm.webrtc && session !== null) {
            lane = joinLane(app.bridge, session, e.swarm.coordinator, e.swarm.stun, (open) => {
              laneNote.hidden = false;
              setText(laneNote, open ? 'join.rtc.on' : 'join.rtc.off');
              laneWait = false;
              applyPause();
            });
          } else if (laneWait) {
            laneWait = false;
            applyPause();
          }
          break;
        case 'jobAdded': {
          // Built-in jobs by their name in the page's language; a presenter's own name as is.
          const k = jobKey(e.name);
          jobs.append(k ? h('li', { i18n: k }) : h('li', { text: e.name }));
          break;
        }
        case 'jobRejected':
          jobs.append(h('li', { class: 'muted', i18n: rejectKey(e.reason) }));
          break;
        case 'leaseRefused':
          stats.refused += 1;
          break;
        case 'taskStarted': {
          stats.running += 1;
          started += 1;
      const sq = h('li', null, h('span', { class: 'n', text: String(started) }));
          squares.set(e.lease, sq);
          mine.append(sq);
          while (mine.children.length > MAX_SQUARES && mine.firstElementChild) mine.firstElementChild.remove();
          refresh();
          break;
        }
        case 'taskDone': {
          stats.running = Math.max(0, stats.running - 1);
          if (e.ok) stats.tasks += 1;
          stats.ms += e.ms;
          const sq = squares.get(e.lease);
          squares.delete(e.lease);
          if (sq) sq.classList.add(...(e.ok ? ['done', 'ink-0'] : ['failed']));
          refresh();
          break;
        }
        case 'ended':
          finish(ENDED[e.reason] ?? 'join.ended.shutdown', e.reason === 'done');
          break;
        case 'error':
          panel.prepend(alertBox(`err.${e.code}`));
          break;
        default:
          break;
      }
    };

    const unsubscribe = app.bridge.onEvent((event, s) => {
      if (session === null) early.push({ event, s });
      else if (s === session) onEvent(event);
    });
    active = true;
    teardown = () => {
      active = false;
      teardown = null;
      unsubscribe();
      if (lane) lane.close();
      if (unBattery) unBattery();
      lock.dispose();
      app.releasePool(pool);
      if (session !== null) {
        const s = session;
        session = null;
        app.bridge.call('stop', { session: s }).catch((e) => console.warn('stop', e));
      }
    };
    pauseBtn.addEventListener('click', () => {
      paused = !paused;
      applyPause();
    });
    stopBtn.addEventListener('click', () => finish('join.ended.stopped', true));

    panel.insertBefore(laneNote, gauge.previousSibling);
    page.append(panel);
    const gpu = useGpu ? await gpuCaps() : null;
    const opts = {
      gpu: gpu ?? undefined,
      slots: wantLane ? 0 : slots,
      rtc: wantLane,
      memMb: offeredMemMb(device, slots),
      kinds: supportedKinds().filter((k) => (k !== 'wgsl' || gpu !== null) && (k !== 'llm' || useLlm)),
    };
    const res = await app.bridge.call('join', { link, opts });
    session = res.session;
    for (const x of early.splice(0)) if (x.s === session) onEvent(x.event);
    if (status.dataset.i18n === 'join.state.connecting') setText(status, 'join.state.working');
    lock.set(true);

    if (acOnly) {
      const nav = /** @type {{ getBattery?: () => Promise<EventTarget & { charging: boolean }> }} */ (/** @type {unknown} */ (navigator));
      if (nav.getBattery) {
        const battery = await nav.getBattery();
        const onCharge = () => {
          batteryPaused = !battery.charging;
          applyPause();
        };
        battery.addEventListener('chargingchange', onCharge);
        unBattery = () => battery.removeEventListener('chargingchange', onCharge);
        onCharge();
      }
    }
    return status;
  }

  return {
    busy: () => active,
    cleanup: () => {
      if (teardown) teardown();
    },
  };
}
