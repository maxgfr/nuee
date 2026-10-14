// @ts-check
// Train together (presenter): a DiLoCo run of the toy trainer on UCI optdigits. This page
// coordinates the rounds (nuee-train inside the iroh worker); the room joins as for any job.
/** @typedef {import('../main').ViewContext} ViewContext */
/** @typedef {import('../main').App} App */
/** @typedef {import('../api').HostSpec} HostSpec */
/** @typedef {import('../api').NueeEvent} NueeEvent */
/** @typedef {import('../api').ModuleInfo} ModuleInfo */
/** @typedef {import('../api').SwarmStatus} SwarmStatus */
/** @typedef {import('../api').TaskOutcome} TaskOutcome */
/** @typedef {import('../lib/pool').Pool} Pool */
import { h, setText, announce, alertBox, icon } from '../lib/dom.js';
import { t } from '../i18n.js';
import { errorArea, framedNotice, linkPanel, fingerprintBlock } from '../lib/ui.js';
import { fmtNumber, fmtFingerprint } from '../lib/format.js';
import { defaultCores, poolSize } from '../lib/pool.js';

/** Defaults of the demo (the same budget the exactness test calibrates). */
export const TRAIN = { rounds: 10, innerSteps: 30, workers: 4, innerLr: 0.1, outerLr: 0.7, outerMomentum: 0.9, initSeed: 7 };
const IN = 64;
const SAMPLE = IN + 1;
const SVG_NS = 'http://www.w3.org/2000/svg';
const CURVE = { w: 320, h: 140, pad: 24 };

/**
 * The asset envelope every host builds: u32 LE count, then u32 LE length + bytes per asset,
 * then the lease input.
 * @param {Uint8Array[]} assets
 * @param {Uint8Array} input
 */
export function envelope(assets, input) {
  const size = 4 + assets.reduce((a, b) => a + 4 + b.byteLength, 0) + input.byteLength;
  const out = new Uint8Array(size);
  const v = new DataView(out.buffer);
  v.setUint32(0, assets.length, true);
  let at = 4;
  for (const a of assets) {
    v.setUint32(at, a.byteLength, true);
    out.set(a, at + 4);
    at += 4 + a.byteLength;
  }
  out.set(input, at);
  return out;
}

/** `NIN1` + u64 seed: asks the trainer for its initial parameters. @param {number} seed */
export function initLease(seed) {
  const b = new Uint8Array(12);
  b.set([0x4e, 0x49, 0x4e, 0x31]);
  new DataView(b.buffer).setBigUint64(4, BigInt(seed), true);
  return b;
}

/** `NEV1` + params: evaluation on the test set. @param {Uint8Array} params */
export function evalLease(params) {
  const b = new Uint8Array(4 + params.byteLength);
  b.set([0x4e, 0x45, 0x56, 0x31]);
  b.set(params, 4);
  return b;
}

/**
 * Reads the dataset header (`OPTD`, u32 n_train, u32 n_test).
 * @param {Uint8Array} d
 * @returns {{ nTrain: number, nTest: number }}
 */
export function datasetInfo(d) {
  const v = new DataView(d.buffer, d.byteOffset, d.byteLength);
  const magic = String.fromCharCode(...d.subarray(0, 4));
  if (d.byteLength < 12 || magic !== 'OPTD') throw new Error('optdigits.bin: not an OPTD dataset');
  const nTrain = v.getUint32(4, true);
  const nTest = v.getUint32(8, true);
  if (d.byteLength !== 12 + (nTrain + nTest) * SAMPLE) throw new Error('optdigits.bin: wrong length');
  return { nTrain, nTest };
}

/**
 * Accuracy, loss and one predicted class per test sample, as returned by `NEV1`.
 * @param {Uint8Array} out
 */
export function parseEval(out) {
  const v = new DataView(out.buffer, out.byteOffset, out.byteLength);
  return { accuracy: v.getFloat32(0, true), loss: v.getFloat32(4, true), predictions: out.subarray(8) };
}

/**
 * Polyline points for a loss curve: x spreads the rounds, y goes from 0 (bottom) to the
 * largest loss (top).
 * @param {number[]} losses
 * @param {number} rounds
 * @param {{ w: number, h: number, pad: number }} box
 */
export function curvePoints(losses, rounds, box) {
  const max = Math.max(...losses, 1e-6);
  const span = Math.max(1, rounds - 1);
  return losses
    .map((l, i) => {
      const x = box.pad + ((box.w - 2 * box.pad) * i) / span;
      const y = box.h - box.pad - ((box.h - 2 * box.pad) * Math.max(0, l)) / max;
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(' ');
}

/** @param {string} tag @param {Record<string, string | number>} attrs */
function svg(tag, attrs) {
  const el = document.createElementNS(SVG_NS, tag);
  for (const [k, v] of Object.entries(attrs)) el.setAttribute(k, String(v));
  return el;
}

/** @param {URL} url @param {string} what */
async function fetchBytes(url, what) {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`${what}: HTTP ${res.status}`);
  return new Uint8Array(await res.arrayBuffer());
}

/**
 * The bundled trainer, checked against tasks/index.json, and its dataset.
 * @param {App} app
 */
async function loadTrainer(app) {
  const index = await fetch(new URL('../../tasks/index.json', import.meta.url), { cache: 'no-store' }).then((r) => {
    if (!r.ok) throw new Error(`tasks/index.json: HTTP ${r.status}`);
    return r.json();
  });
  const entry = Array.isArray(index) ? index.find((e) => e && e.name === 'toy-trainer') : undefined;
  if (!entry) throw new Error('task toy-trainer missing from tasks/index.json');
  const module = await fetchBytes(new URL('../../tasks/toy-trainer.wasm', import.meta.url), 'toy-trainer');
  const info = await app.bridge.call('validateModule', { bytes: module.slice() });
  if (info.hash !== entry.hash) throw { code: 'integrity', message: 'toy-trainer.wasm does not match tasks/index.json' };
  const dataset = await fetchBytes(new URL('../../tasks/optdigits.bin', import.meta.url), 'optdigits.bin');
  return { module, info, dataset, ...datasetInfo(dataset) };
}

/**
 * Runs one lease of the trainer on this page's pool.
 * @param {Pool} pool
 * @param {{ module: Uint8Array, info: ModuleInfo, dataset: Uint8Array }} tr
 * @param {Uint8Array} lease
 */
async function runLocal(pool, tr, lease) {
  /** @type {TaskOutcome} */
  const out = await pool.run({
    lease: '0',
    job: 0,
    task: 0,
    kind: 'trainWasm',
    moduleHash: tr.info.hash,
    module: tr.module.slice(),
    input: envelope([tr.dataset], lease),
    deadlineMs: 60_000,
    maxMemory: tr.info.maxMemory,
  });
  if (!out.ok) throw { code: 'internal', message: `toy-trainer: ${out.kind} ${out.detail}` };
  return out.output;
}

/** @param {ViewContext} ctx */
export function render({ app, root }) {
  document.documentElement.dataset.area = 'train';
  const errors = errorArea();
  root.append(h('section', { class: 'page', 'aria-labelledby': 'train-title' }, h('h1', { id: 'train-title', i18n: 'train.title' }), h('p', { class: 'lead', i18n: 'train.lead' })));
  const page = /** @type {HTMLElement} */ (root.firstElementChild);
  if (app.caps.framed) {
    page.append(framedNotice());
    return {};
  }

  /** @param {string} id @param {string} key @param {number} value @param {number} max */
  const numberField = (id, key, value, max) => {
    const input = h('input', { type: 'number', id, inputmode: 'numeric', props: { value, min: 1, max, step: 1 } });
    return { input, el: h('div', { class: 'field' }, h('label', { for: id, i18n: key }), input) };
  };
  const rounds = numberField('train-rounds', 'train.rounds', TRAIN.rounds, 200);
  const inner = numberField('train-inner', 'train.innerSteps', TRAIN.innerSteps, 2000);
  const workers = numberField('train-workers', 'train.workers', TRAIN.workers, 64);
  const verify = h(
    'select',
    { id: 'train-verify' },
    h('option', { value: 'sample', i18n: 'run.verify.sample' }),
    h('option', { value: 'full', i18n: 'run.verify.full' }),
    h('option', { value: 'none', i18n: 'run.verify.none' }),
  );
  const approve = h('input', { type: 'checkbox', id: 'train-approve' });
  const local = h('input', { type: 'checkbox', id: 'train-local', props: { checked: true } });
  const startBtn = h('button', { type: 'submit', class: 'btn primary', i18n: 'train.start' });
  const form = h(
    'form',
    { class: 'run-form', novalidate: true },
    h(
      'fieldset',
      { class: 'settings' },
      h('legend', { i18n: 'run.settings' }),
      rounds.el,
      inner.el,
      workers.el,
      h('p', { class: 'muted', i18n: 'train.budgetHelp' }),
      h('div', { class: 'field' }, h('label', { for: 'train-verify', i18n: 'run.verify' }), verify, h('p', { class: 'muted', i18n: 'train.verifyHelp' })),
      h('div', { class: 'check' }, approve, h('label', { for: 'train-approve', i18n: 'run.approve' })),
      h('div', { class: 'check' }, local, h('label', { for: 'train-local', i18n: 'run.local' })),
    ),
    errors.el,
    h('div', { class: 'row' }, startBtn),
  );
  page.append(form, h('p', { class: 'muted', i18n: 'train.attribution' }));

  /** @type {(() => void) | null} */
  let stopAll = null;
  let live = false;

  /** @param {HTMLInputElement} input @param {number} max */
  const intOf = (input, max) => {
    const n = Math.floor(Number(input.value));
    if (!Number.isFinite(n) || n < 1 || n > max) throw { code: 'invalid_argument', message: `${input.id} out of range` };
    return n;
  };

  form.addEventListener('submit', (e) => {
    e.preventDefault();
    if (live) return;
    errors.clear();
    startBtn.disabled = true;
    setText(startBtn, 'train.loading');
    start().catch((err) => {
      errors.show(err);
      startBtn.disabled = false;
      setText(startBtn, 'train.start');
      if (stopAll) stopAll();
    });
  });

  async function start() {
    const plan = { rounds: intOf(rounds.input, 200), innerSteps: intOf(inner.input, 2000), workers: intOf(workers.input, 64) };
    const tr = await loadTrainer(app);
    await app.node();
    const pool = app.pool(defaultCores(app.caps.cores));
    const slots = poolSize({ ...pool.device, wanted: pool.slots, maxMemory: tr.info.maxMemory });
    const init = await runLocal(pool, tr, initLease(TRAIN.initSeed));
    /** @type {HostSpec} */
    const spec = {
      name: 'train-optdigits',
      kind: 'trainWasm',
      module: tr.module.slice(),
      assets: [tr.dataset.slice()],
      inputs: [],
      verify: /** @type {'sample' | 'full' | 'none'} */ (verify.value),
      approve: approve.checked,
      localSlots: local.checked ? slots : 0,
      deadlineMs: 60_000,
      train: { ...plan, outerLr: TRAIN.outerLr, outerMomentum: TRAIN.outerMomentum, innerLr: TRAIN.innerLr, codec: 'f32', init },
    };
    const view = liveView(app, plan.rounds, async (params) => parseEval(await runLocal(pool, tr, evalLease(params))), tr);
    /** @type {number | null} */
    let session = null;
    /** @type {Array<{ event: NueeEvent, session: number | undefined }>} */
    const early = [];
    const unsubscribe = app.bridge.onEvent((event, s) => {
      if (session === null) early.push({ event, session: s });
      else if (s === session) view.onEvent(event);
    });
    stopAll = () => {
      unsubscribe();
      view.dispose();
      app.releasePool(pool);
      if (session !== null) {
        const s = session;
        session = null;
        app.bridge.call('stop', { session: s }).catch((e) => console.warn('stop', e));
      }
      live = false;
    };
    const res = await app.bridge.call('host', spec);
    session = res.session;
    live = true;
    form.hidden = true;
    page.append(view.el);
    await view.started(res.link, res.fingerprint, res.session);
    for (const x of early.splice(0)) if (x.session === res.session) view.onEvent(x.event);
    view.onStop(() => {
      if (stopAll) stopAll();
      stopAll = null;
      form.hidden = false;
      startBtn.disabled = false;
      setText(startBtn, 'train.start');
    });
  }

  return {
    busy: () => live,
    cleanup: () => {
      if (stopAll) stopAll();
    },
  };
}

/**
 * The live part: link, loss curve, round summary, final accuracy and sample digits.
 * @param {App} app
 * @param {number} rounds
 * @param {(params: Uint8Array) => Promise<{ accuracy: number, loss: number, predictions: Uint8Array }>} evaluate
 * @param {{ dataset: Uint8Array, nTrain: number, nTest: number }} data
 */
function liveView(app, rounds, evaluate, data) {
  const counts = { peers: 0, round: 0, verified: 0, mismatch: 0, banned: 0 };
  /** @type {number[]} */
  const losses = [];
  /** @type {(() => void) | null} */
  let disposeLink = null;
  /** @type {(() => void) | null} */
  let stopCb = null;
  /** @type {ReturnType<typeof setInterval> | null} */
  let poll = null;
  let session = 0;

  const linkSlot = h('div', { class: 'link-slot' });
  const pendingList = h('ul', { class: 'pending' });
  const pendingSection = h('section', { class: 'card', hidden: true, 'aria-labelledby': 'train-pending-title' }, h('h2', { id: 'train-pending-title', i18n: 'run.pending' }), pendingList);
  const stats = h('dl', { class: 'stats' });
  const summary = h('p', { class: 'train-summary', 'aria-live': 'polite', i18n: 'train.waiting' });
  const resultSlot = h('div', { class: 'results' });
  const stopBtn = h('button', { type: 'button', class: 'btn danger' }, icon('stop'), h('span', { i18n: 'run.stop' }));

  const chart = svg('svg', { class: 'loss-curve', viewBox: `0 0 ${CURVE.w} ${CURVE.h}`, role: 'img', 'aria-labelledby': 'train-curve-label' });
  const chartLabel = svg('title', { id: 'train-curve-label' });
  chartLabel.textContent = t('train.curveAlt');
  const bottom = CURVE.h - CURVE.pad;
  chart.append(
    chartLabel,
    svg('line', { class: 'axis', x1: CURVE.pad, y1: bottom, x2: CURVE.w - CURVE.pad, y2: bottom }),
    svg('line', { class: 'axis', x1: CURVE.pad, y1: CURVE.pad, x2: CURVE.pad, y2: bottom }),
  );
  const line = svg('polyline', { class: 'loss-line', points: '', fill: 'none' });
  chart.append(line);

  const el = h(
    'div',
    { class: 'live' },
    h('section', { class: 'card', 'aria-labelledby': 'train-link-title' }, h('h2', { id: 'train-link-title', i18n: 'run.linkTitle' }), h('p', { class: 'muted', i18n: 'run.linkHelp' }), linkSlot),
    pendingSection,
    h(
      'section',
      { class: 'card', 'aria-labelledby': 'train-track-title' },
      h('h2', { id: 'train-track-title', i18n: 'train.curve' }),
      chart,
      summary,
      stats,
      resultSlot,
      h('div', { class: 'actions' }, stopBtn),
    ),
  );

  function renderStats() {
    /** @type {Array<[string, string]>} */
    const rows = [
      ['run.stat.peers', fmtNumber(counts.peers)],
      ['train.stat.round', `${fmtNumber(counts.round)} / ${fmtNumber(rounds)}`],
      ['train.stat.loss', losses.length > 0 ? fmtNumber(losses[losses.length - 1] ?? 0, 3) : '–'],
      ['run.stat.verified', fmtNumber(counts.verified)],
      ['run.stat.mismatch', fmtNumber(counts.mismatch)],
      ['run.stat.banned', fmtNumber(counts.banned)],
    ];
    stats.replaceChildren(...rows.map(([k, v]) => h('div', { class: 'stat' }, h('dt', { i18n: k }), h('dd', { text: v }))));
  }
  renderStats();

  /** @param {SwarmStatus} st */
  function renderPeers(st) {
    counts.peers = st.peers.length;
    renderStats();
  }

  /** @param {string} peer @param {string[]} fp */
  function addPending(peer, fp) {
    pendingSection.hidden = false;
    const li = h('li', { class: 'row' }, h('span', { class: 'emoji-cell', text: fmtFingerprint(fp) }));
    /** @param {boolean} accept */
    const answer = (accept) => {
      app.bridge.call('approve', { session, peer, accept }).catch((e) => console.error('approve', e));
      li.remove();
      if (!pendingList.firstChild) pendingSection.hidden = true;
    };
    li.append(
      h('button', { type: 'button', class: 'btn', i18n: 'run.accept', on: { click: () => answer(true) } }),
      h('button', { type: 'button', class: 'btn secondary', i18n: 'run.refuse', on: { click: () => answer(false) } }),
    );
    pendingList.append(li);
    announce(t('run.pendingAnnounce'));
  }

  /** One test sample per class, drawn 8×8 with the model's prediction. @param {Uint8Array} predictions */
  function samples(predictions) {
    const test = data.dataset.subarray(12 + data.nTrain * SAMPLE);
    /** @type {number[]} */
    const picks = [];
    for (let digit = 0; digit < 10; digit += 1) {
      for (let i = 0; i < data.nTest; i += 1) {
        if (test[i * SAMPLE + IN] === digit) {
          picks.push(i);
          break;
        }
      }
    }
    const items = picks.map((i) => {
      const label = test[i * SAMPLE + IN] ?? 0;
      const guess = predictions[i] ?? 0;
      const canvas = h('canvas', { class: 'digit', width: 8, height: 8, 'aria-hidden': 'true' });
      const c2d = canvas.getContext('2d');
      if (c2d) {
        const img = c2d.createImageData(8, 8);
        for (let p = 0; p < IN; p += 1) {
          const v = 255 - Math.round(((test[i * SAMPLE + p] ?? 0) * 255) / 16);
          img.data.set([v, v, v, 255], p * 4);
        }
        c2d.putImageData(img, 0, 0);
      }
      return h(
        'li',
        { class: guess === label ? 'digit-ok' : 'digit-wrong' },
        canvas,
        h('span', { i18n: guess === label ? 'train.sampleOk' : 'train.sampleWrong', vars: { guess, label } }),
      );
    });
    return h('figure', { class: 'digits' }, h('figcaption', { i18n: 'train.samples' }), h('ul', { class: 'digit-list' }, ...items));
  }

  /** @param {Uint8Array} params */
  async function finish(params) {
    setText(summary, 'train.evaluating');
    try {
      const ev = await evaluate(params);
      const pct = fmtNumber(ev.accuracy * 100, 1);
      resultSlot.replaceChildren(h('p', { class: 'ok big-number', i18n: 'train.accuracy', vars: { pct, n: fmtNumber(data.nTest) } }), samples(ev.predictions));
      // The summary is a live region: it announces the result itself.
      setText(summary, 'train.done', { pct });
    } catch (e) {
      console.error('evaluation', e);
      resultSlot.replaceChildren(alertBox('err.internal'));
    }
  }

  stopBtn.addEventListener('click', () => {
    if (stopCb) stopCb();
    el.remove();
  });

  return {
    el,
    /** @param {string} link @param {string[]} fp @param {number} s */
    async started(link, fp, s) {
      session = s;
      const panel = await linkPanel(app, link, fp);
      disposeLink = panel.dispose;
      linkSlot.replaceChildren(panel.el, fingerprintBlock(fp, 'run.fingerprint'));
      announce(t('run.online'));
      poll = setInterval(() => {
        app.bridge.call('status', { session }).then(renderPeers, (e) => console.warn('status', e));
      }, 2000);
    },
    /** @param {NueeEvent} e */
    onEvent(e) {
      switch (e.type) {
        case 'trainRound': {
          counts.round = e.round;
          if (e.loss !== null) losses.push(e.loss);
          line.setAttribute('points', curvePoints(losses, rounds, CURVE));
          const loss = e.loss === null ? '–' : fmtNumber(e.loss, 3);
          setText(summary, 'train.roundStatus', { round: e.round, rounds: e.rounds, loss });
          renderStats();
          break;
        }
        case 'trainDone':
          finish(e.params);
          break;
        case 'verified':
          counts.verified += 1;
          renderStats();
          break;
        case 'mismatch':
          counts.mismatch += 1;
          renderStats();
          break;
        case 'banned':
          counts.banned += 1;
          renderStats();
          announce(t('run.bannedAnnounce', { redo: e.redo }));
          break;
        case 'peerPending':
          addPending(e.peer, e.fingerprint);
          break;
        case 'peerJoined':
        case 'peerLeft':
          app.bridge.call('status', { session }).then(renderPeers, (err) => console.warn('status', err));
          break;
        case 'error':
          resultSlot.prepend(alertBox(`err.${e.code}`));
          break;
        default:
          break;
      }
    },
    /** @param {() => void} fn */
    onStop(fn) {
      stopCb = fn;
    },
    dispose() {
      if (poll) clearInterval(poll);
      if (disposeLink) disposeLink();
    },
  };
}
