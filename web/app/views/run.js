// @ts-check
// Run (presenter): pick a job, start the swarm, show the link once online, follow it live
// and download the results.
/** @typedef {import('../main').ViewContext} ViewContext */
/** @typedef {import('../main').App} App */
/** @typedef {import('../api').HostSpec} HostSpec */
/** @typedef {import('../api').NueeEvent} NueeEvent */
/** @typedef {import('../api').ModuleInfo} ModuleInfo */
/** @typedef {import('../api').SwarmStatus} SwarmStatus */
import { h, setText, announce, download, alertBox, icon, focusOn } from '../lib/dom.js';
import { readTextFile } from '../lib/labels.js';
import { embedJob, embedInputs, mergeEmbed, daggerInputs, mergeJsonl, EMBED_TAU } from '../lib/jobs.js';
import { t, onLangChange } from '../i18n.js';
import { errorArea, framedNotice, linkPanel, progressBar, fingerprintBlock } from '../lib/ui.js';
import { fmtNumber, fmtBytes, fmtFingerprint, shortHash } from '../lib/format.js';
import { defaultCores, poolSize, supportedKinds, MiB } from '../lib/pool.js';
import { hostLanes } from '../lib/rtc.js';
import { Timings, fmtDuration, reduceWaiting, waitingTotal, taskDetail, rgbOf, tilePixels } from '../lib/board.js';

/** Browser-side cap on what a page shares or hosts in total. */
export const MAX_TOTAL = 256 * MiB;

export const MANDELBROT = { size: 512, tile: 64, iter: 500, x0: -2.0, y0: -1.5, span: 3.0 };
export const SKYCMD = {
  model: 'https://maxgfr.github.io/skycmd/models/s3-pico.gguf',
  games: ['flappy', 'rescue', 'dodge', 'doom'],
  seeds: 8,
  episodes: 20,
  tau: 0.6,
};

/**
 * One 36-byte input per tile: x0, y0, step (f64 LE), then w, h, max_iter (u32 LE).
 * @param {typeof MANDELBROT} m
 * @returns {Uint8Array[]}
 */
export function mandelbrotInputs(m) {
  const step = m.span / m.size;
  const n = m.size / m.tile;
  const out = [];
  for (let ty = 0; ty < n; ty += 1) {
    for (let tx = 0; tx < n; tx += 1) {
      const b = new Uint8Array(36);
      const v = new DataView(b.buffer);
      v.setFloat64(0, m.x0 + tx * m.tile * step, true);
      v.setFloat64(8, m.y0 + ty * m.tile * step, true);
      v.setFloat64(16, step, true);
      v.setUint32(24, m.tile, true);
      v.setUint32(28, m.tile, true);
      v.setUint32(32, m.iter, true);
      out.push(b);
    }
  }
  return out;
}

/**
 * The same tiles as WGSL tasks (NWG1, see worker/gpu.js): a uniform of x0, y0, step (f32)
 * and w, h, max_iter (u32) at binding 0, u32 counts out at binding 1.
 * @param {typeof MANDELBROT} m
 * @returns {Uint8Array[]}
 */
export function mandelbrotGpuInputs(m) {
  const step = m.span / m.size;
  const n = m.size / m.tile;
  const out = [];
  for (let ty = 0; ty < n; ty += 1) {
    for (let tx = 0; tx < n; tx += 1) {
      const b = new Uint8Array(4 + 8 + 12 + 4 + 12 + 24 + 12);
      const v = new DataView(b.buffer);
      let at = 0;
      /** @param {number} x */
      const u32 = (x) => {
        v.setUint32(at, x, true);
        at += 4;
      };
      b.set([0x4e, 0x57, 0x47, 0x31], 0);
      at = 4;
      u32(4);
      b.set([0x6d, 0x61, 0x69, 0x6e], at);
      at += 4;
      u32(Math.ceil(m.tile / 8));
      u32(Math.ceil(m.tile / 8));
      u32(1);
      u32(2);
      u32(0);
      u32(0);
      u32(24);
      v.setFloat32(at, m.x0 + tx * m.tile * step, true);
      v.setFloat32(at + 4, m.y0 + ty * m.tile * step, true);
      v.setFloat32(at + 8, step, true);
      at += 12;
      u32(m.tile);
      u32(m.tile);
      u32(m.iter);
      u32(1);
      u32(2);
      u32(m.tile * m.tile * 4);
      out.push(b);
    }
  }
  return out;
}

/** @param {typeof SKYCMD} s */
export function skycmdInputs(s) {
  const enc = new TextEncoder();
  const out = [];
  for (const game of s.games) {
    for (let seed = 1; seed <= s.seeds; seed += 1) {
      out.push(enc.encode(JSON.stringify({ mode: 'eval', game, seed, episodes: s.episodes, tau: s.tau })));
    }
  }
  return out;
}

/** Tasks drawn as squares; a larger job shows its figures only. */
const MAX_GRID = 4096;
/** Device inks defined in tokens.css (--ink-0 … --ink-7). */
const INKS = 8;

/** @param {Uint8Array} bytes */
export function base64(bytes) {
  let s = '';
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(s);
}

/** @param {URL} url @param {string} what */
async function fetchBytes(url, what) {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`${what}: HTTP ${res.status}`);
  return new Uint8Array(await res.arrayBuffer());
}

/**
 * Loads a bundled task module and checks it against tasks/index.json.
 * @param {App} app
 * @param {string} name
 * @returns {Promise<{ bytes: Uint8Array, info: ModuleInfo }>}
 */
async function bundledModule(app, name) {
  const index = await fetch(new URL('../../tasks/index.json', import.meta.url), { cache: 'no-store' }).then((r) => {
    if (!r.ok) throw new Error(`tasks/index.json: HTTP ${r.status}`);
    return r.json();
  });
  const entry = Array.isArray(index) ? index.find((e) => e && e.name === name) : undefined;
  if (!entry) throw new Error(`task ${name} missing from tasks/index.json`);
  const bytes = await fetchBytes(new URL(`../../tasks/${name}.wasm`, import.meta.url), name);
  const info = await app.bridge.call('validateModule', { bytes: bytes.slice() });
  if (info.hash !== entry.hash) throw { code: 'integrity', message: `${name}.wasm does not match tasks/index.json` };
  return { bytes, info };
}

/** @param {ViewContext} ctx */
export function render({ app, root }) {
  document.documentElement.dataset.area = 'run';
  const errors = errorArea();
  const title = h('h1', { id: 'run-title', i18n: 'run.title' });
  const lead = h('p', { class: 'lead', i18n: 'run.lead' });
  root.append(h('section', { class: 'page', 'aria-labelledby': 'run-title' }, title, lead));
  const page = /** @type {HTMLElement} */ (root.firstElementChild);
  if (app.caps.framed) {
    page.append(framedNotice());
    return {};
  }

  // --- catalogue -------------------------------------------------------------------------
  /** @param {string} value @param {boolean} [disabled] */
  const choice = (value, disabled = false) =>
    h(
      'label',
      { class: `choice${disabled ? ' disabled' : ''}` },
      h('input', { type: 'radio', name: 'job', value, props: { checked: value === 'mandelbrot', disabled } }),
      h('span', { class: 'choice-text' }, h('strong', { i18n: `run.job.${value}` }), h('span', { class: 'muted', i18n: `run.job.${value}.body` })),
    );
  const moduleInput = h('input', { type: 'file', id: 'custom-module', accept: '.wasm,application/wasm' });
  const inputsInput = h('input', { type: 'file', id: 'custom-inputs', props: { multiple: true } });
  const moduleInfo = h('p', { class: 'muted', 'aria-live': 'polite' });
  const drop = h(
    'div',
    { class: 'drop', i18n: 'run.custom.drop' },
  );
  const custom = h(
    'fieldset',
    { class: 'custom', hidden: true },
    h('legend', { i18n: 'run.custom.legend' }),
    drop,
    h('label', { for: 'custom-module', i18n: 'run.custom.module' }),
    moduleInput,
    moduleInfo,
    h('label', { for: 'custom-inputs', i18n: 'run.custom.inputs' }),
    h('p', { class: 'muted', id: 'custom-inputs-help', i18n: 'run.custom.inputsHelp' }),
    inputsInput,
  );
  inputsInput.setAttribute('aria-describedby', 'custom-inputs-help');
  // Embeddings: one text file, a text per line (or JSONL with a `text` field).
  const textsInput = h('input', { type: 'file', id: 'embed-texts', accept: '.txt,.jsonl,.md,.csv,text/plain', 'aria-describedby': 'embed-texts-help' });
  const textsInfo = h('p', { class: 'muted', 'aria-live': 'polite' });
  // What is wrong with the picked file, tied to the field.
  const textsError = h('p', { id: 'embed-texts-error', class: 'field-error', 'aria-live': 'polite', hidden: true });
  /** @param {string | null} key */
  const textsProblem = (key) => {
    textsError.hidden = key === null;
    if (key === null) {
      textsError.textContent = '';
      textsInput.removeAttribute('aria-invalid');
      textsInput.setAttribute('aria-describedby', 'embed-texts-help');
    } else {
      setText(textsError, key);
      textsInput.setAttribute('aria-invalid', 'true');
      textsInput.setAttribute('aria-describedby', 'embed-texts-help embed-texts-error');
    }
  };
  const embedSet = h(
    'fieldset',
    { class: 'custom', hidden: true },
    h('legend', { i18n: 'run.embed.legend' }),
    h('label', { for: 'embed-texts', i18n: 'run.embed.file' }),
    h('p', { class: 'muted', id: 'embed-texts-help', i18n: 'run.embed.help' }),
    textsInput,
    textsError,
    textsInfo,
  );
  // skycmd: score the model, or collect oracle-labelled DAgger records.
  /** @param {string} value */
  const skyMode = (value) =>
    h(
      'label',
      { class: 'choice' },
      h('input', { type: 'radio', name: 'skymode', value, props: { checked: value === 'eval' } }),
      h('span', { class: 'choice-text' }, h('strong', { i18n: `run.sky.mode.${value}` }), h('span', { class: 'muted', i18n: `run.sky.mode.${value}.body` })),
    );
  const skySet = h('fieldset', { class: 'sky-mode', hidden: true }, h('legend', { i18n: 'run.sky.mode' }), skyMode('eval'), skyMode('dagger'));
  const skyModeOf = () => {
    const r = /** @type {HTMLInputElement | null} */ (skySet.querySelector('input[name=skymode]:checked'));
    return r ? r.value : 'eval';
  };
  const catalogue = h(
    'fieldset',
    { class: 'catalogue' },
    h('legend', { i18n: 'run.catalogue' }),
    choice('mandelbrot'),
    choice('mandelbrotGpu', !supportedKinds().includes('wgsl')),
    choice('skycmd'),
    choice('embed'),
    choice('train'),
    choice('custom'),
  );

  // --- settings --------------------------------------------------------------------------
  const verify = h(
    'select',
    { id: 'verify' },
    h('option', { value: 'sample', i18n: 'run.verify.sample' }),
    h('option', { value: 'full', i18n: 'run.verify.full' }),
    h('option', { value: 'none', i18n: 'run.verify.none' }),
  );
  const approve = h('input', { type: 'checkbox', id: 'approve' });
  const local = h('input', { type: 'checkbox', id: 'local', props: { checked: true } });
  const rtc = h('input', { type: 'checkbox', id: 'rtc', props: { disabled: typeof RTCPeerConnection !== 'function' }, 'aria-describedby': 'rtc-warn' });
  const settings = h(
    'fieldset',
    { class: 'settings' },
    h('legend', { i18n: 'run.settings' }),
    h('div', { class: 'field' }, h('label', { for: 'verify', i18n: 'run.verify' }), verify, h('p', { class: 'muted', i18n: 'run.verify.help' })),
    h('div', { class: 'check' }, approve, h('label', { for: 'approve', i18n: 'run.approve' })),
    h('div', { class: 'check' }, local, h('label', { for: 'local', i18n: 'run.local' })),
    h('p', { class: 'muted', i18n: 'run.local.help' }),
    h('div', { class: 'check' }, rtc, h('label', { for: 'rtc', i18n: 'run.rtc' })),
    h('p', { id: 'rtc-warn', class: 'muted', i18n: 'run.rtc.warn' }),
  );
  const startBtn = h('button', { type: 'submit', class: 'btn primary', i18n: 'run.start' });
  const form = h('form', { class: 'run-form', novalidate: true }, catalogue, skySet, embedSet, custom, settings, errors.el, h('div', { class: 'row' }, startBtn));
  page.append(form);

  const selected = () => {
    const r = /** @type {HTMLInputElement | null} */ (catalogue.querySelector('input[name=job]:checked'));
    return r ? r.value : 'mandelbrot';
  };
  catalogue.addEventListener('change', () => {
    // Training runs round after round on a page of its own, opened by Start: arrowing through
    // the choices never leaves this page.
    custom.hidden = selected() !== 'custom';
    embedSet.hidden = selected() !== 'embed';
    skySet.hidden = selected() !== 'skycmd';
  });

  /** @type {{ name: string, job: import('../lib/jobs.js').EmbedJob } | null} */
  let texts = null;
  textsInput.addEventListener('change', async () => {
    texts = null;
    textsInfo.textContent = '';
    errors.clear();
    textsProblem(null);
    const f = textsInput.files && textsInput.files[0];
    if (!f) return;
    try {
      if (f.size > MAX_TOTAL) throw { code: 'too_large', message: `${f.size} bytes` };
      // A file that is not text, or holds no text, is the person's to fix: say which.
      const read = readTextFile(await f.arrayBuffer());
      if ('error' in read) {
        textsProblem(read.error);
        return;
      }
      const job = embedJob(f.name, read.text);
      if (job.texts.length === 0) {
        textsProblem('run.embed.empty');
        return;
      }
      texts = { name: f.name, job };
      setText(textsInfo, 'run.embed.ok', { n: fmtNumber(job.texts.length), tasks: fmtNumber(embedInputs(job).length) });
    } catch (e) {
      errors.show(e);
    }
  });

  /** @type {{ bytes: Uint8Array, info: ModuleInfo } | null} */
  let customModule = null;
  /** @param {File} file */
  async function loadModule(file) {
    customModule = null;
    errors.clear();
    setText(moduleInfo, 'run.custom.checking');
    try {
      const bytes = new Uint8Array(await file.arrayBuffer());
      const info = await app.bridge.call('validateModule', { bytes: bytes.slice() });
      customModule = { bytes, info };
      setText(moduleInfo, 'run.custom.ok', { hash: shortHash(info.hash), size: fmtBytes(info.size), mem: fmtBytes(info.maxMemory) });
    } catch (e) {
      moduleInfo.textContent = '';
      errors.show(e);
    }
  }
  moduleInput.addEventListener('change', () => {
    const f = moduleInput.files && moduleInput.files[0];
    if (f) loadModule(f);
  });
  drop.addEventListener('dragover', (e) => {
    e.preventDefault();
    drop.classList.add('over');
  });
  drop.addEventListener('dragleave', () => drop.classList.remove('over'));
  drop.addEventListener('drop', (e) => {
    e.preventDefault();
    drop.classList.remove('over');
    const files = e.dataTransfer ? [...e.dataTransfer.files] : [];
    const wasm = files.find((f) => f.name.endsWith('.wasm'));
    if (wasm) loadModule(wasm);
    const rest = files.filter((f) => f !== wasm);
    if (rest.length > 0) {
      const dt = new DataTransfer();
      for (const f of rest) dt.items.add(f);
      inputsInput.files = dt.files;
    }
  });

  /** @type {(() => void) | null} */
  let stopAll = null;
  let live = false;

  form.addEventListener('submit', (e) => {
    e.preventDefault();
    if (live) return;
    if (selected() === 'train') {
      location.hash = '#/train';
      return;
    }
    errors.clear();
    startBtn.disabled = true;
    setText(startBtn, 'run.starting');
    start().catch((err) => {
      errors.show(err);
      startBtn.disabled = false;
      setText(startBtn, 'run.start');
      if (stopAll) stopAll();
    });
  });

  /** @returns {Promise<{ spec: HostSpec, kind: string, info: ModuleInfo, embed?: import('../lib/jobs.js').EmbedJob }>} */
  async function buildSpec() {
    const which = selected();
    const common = {
      verify: /** @type {'sample' | 'full' | 'none'} */ (verify.value),
      approve: approve.checked,
      webrtc: rtc.checked,
      localSlots: 0,
    };
    if (which === 'mandelbrot') {
      const m = await bundledModule(app, 'mandelbrot');
      return { kind: which, info: m.info, spec: { ...common, name: 'mandelbrot', kind: 'wasm', module: m.bytes, inputs: mandelbrotInputs(MANDELBROT), deadlineMs: 60_000 } };
    }
    if (which === 'mandelbrotGpu') {
      const module = await fetchBytes(new URL('../../tasks/mandelbrot.wgsl', import.meta.url), 'mandelbrot.wgsl');
      const info = { hash: '', size: module.byteLength, minMemory: 0, maxMemory: 256 * MiB };
      // Same tolerance as the CLI (nuee_gpu::wgsl::MANDELBROT_COMPARE).
      const compare = /** @type {const} */ ({ kind: 'tolerance', abs: 2, rel: 0, maxBad: 50 });
      return { kind: 'mandelbrot', info, spec: { ...common, name: 'mandelbrot-gpu', kind: 'wgsl', module, inputs: mandelbrotGpuInputs(MANDELBROT), compare, maxMemory: 256 * MiB, deadlineMs: 60_000 } };
    }
    if (which === 'embed') {
      if (!texts) throw { code: 'invalid_argument', message: 'no text file selected' };
      const info = { hash: '', size: 0, minMemory: 0, maxMemory: 256 * MiB };
      const compare = /** @type {const} */ ({ kind: 'cosine', minCos: EMBED_TAU });
      return { kind: 'embed', info, embed: texts.job, spec: { ...common, name: 'embed', kind: 'embed', inputs: embedInputs(texts.job), compare, deadlineMs: 5 * 60_000 } };
    }
    if (which === 'skycmd' && skyModeOf() === 'dagger') {
      const m = await bundledModule(app, 'skycmd-arena');
      const model = await fetchBytes(new URL(SKYCMD.model), 'skycmd model');
      return { kind: 'skycmdDagger', info: m.info, spec: { ...common, name: 'skycmd-dagger', kind: 'wasm', module: m.bytes, assets: [model], inputs: daggerInputs(), deadlineMs: 600_000 } };
    }
    if (which === 'skycmd') {
      const m = await bundledModule(app, 'skycmd-arena');
      const model = await fetchBytes(new URL(SKYCMD.model), 'skycmd model');
      return {
        kind: which,
        info: m.info,
        spec: { ...common, name: 'skycmd-eval', kind: 'wasm', module: m.bytes, assets: [model], inputs: skycmdInputs(SKYCMD), deadlineMs: 600_000 },
      };
    }
    if (!customModule) throw { code: 'invalid_module', message: 'no module selected' };
    const files = inputsInput.files ? [...inputsInput.files] : [];
    const total = files.reduce((a, f) => a + f.size, customModule.bytes.byteLength);
    if (total > MAX_TOTAL) throw { code: 'too_large', message: `${total} bytes` };
    const inputs = files.length === 0 ? [new Uint8Array(0)] : await Promise.all(files.map(async (f) => new Uint8Array(await f.arrayBuffer())));
    const name = (moduleInput.files && moduleInput.files[0] ? moduleInput.files[0].name : 'custom').replace(/\.wasm$/, '').slice(0, 60) || 'custom';
    return { kind: which, info: customModule.info, spec: { ...common, name, kind: 'wasm', module: customModule.bytes, inputs, deadlineMs: 60_000 } };
  }

  async function start() {
    const { spec, kind, info, embed } = await buildSpec();
    const titleKey = kind === 'skycmdDagger' ? 'run.job.skycmdDagger' : `run.job.${selected()}`;
    const total = spec.inputs.length;
    await app.node();
    const pool = app.pool(defaultCores(app.caps.cores));
    const slots = poolSize({ ...pool.device, wanted: pool.slots, maxMemory: info.maxMemory });
    spec.localSlots = local.checked ? slots : 0;
    const view = liveView(app, kind, total, spec.webrtc === true, embed ?? null);
    /** @type {number | null} */
    let session = null;
    /** @type {Array<{ event: NueeEvent, session: number | undefined }>} */
    const early = [];
    const unsubscribe = app.bridge.onEvent((event, s) => {
      if (session === null) early.push({ event, session: s });
      else if (s === session) view.onEvent(event, session);
    });
    /** @type {{ close(): void } | null} */
    let lanes = null;
    stopAll = () => {
      unsubscribe();
      if (lanes) lanes.close();
      view.dispose();
      app.releasePool(pool);
      if (session !== null) {
        const s = session;
        session = null;
        app.bridge.call('stop', { session: s }).catch((e) => console.warn('stop', e));
      }
      live = false;
    };
    const res = await app.bridge.call('host', spec, { move: true });
    session = res.session;
    if (spec.webrtc) lanes = hostLanes(app.bridge, res.session, spec.stun ?? [], () => view.laneResult());
    live = true;
    form.hidden = true;
    // While the job runs its name is the page title, above one line of what it is.
    setText(title, titleKey);
    lead.hidden = true;
    const specLine = h(
      'p',
      { class: 'spec-line' },
      h('span', { i18n: `run.spec.${spec.kind}` }),
      ' · ',
      h('span', { i18n: `run.spec.verify.${spec.verify ?? 'sample'}` }),
      ' · ',
      h('span', { i18n: 'run.spec.tasks', vars: { n: fmtNumber(total) } }),
    );
    page.append(view.el);
    view.head(title, specLine);
    focusOn(title);
    await view.started(res.link, res.fingerprint, res.session);
    for (const x of early.splice(0)) if (x.session === res.session) view.onEvent(x.event, res.session);
    view.onStop(() => {
      if (stopAll) stopAll();
      stopAll = null;
      specLine.remove();
      setText(title, 'run.title');
      lead.hidden = false;
      page.prepend(title, lead);
      form.hidden = false;
      startBtn.disabled = false;
      setText(startBtn, 'run.start');
      focusOn(title);
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
 * The live part of the presenter page: the invite and one correction note in the margin; the
 * job title, the figures, the page of task squares filling in each device's ink beside the
 * devices, then the fractal.
 * @param {App} app
 * @param {string} kind
 * @param {number} total
 * @param {boolean} webrtc
 * @param {import('../lib/jobs.js').EmbedJob | null} embed
 */
function liveView(app, kind, total, webrtc, embed) {
  const counts = { peers: 0, done: 0, verified: 0, mismatch: 0, banned: 0, failed: 0, redone: 0, rate: 0, direct: 0 };
  /** @type {Map<number, { bytes: Uint8Array, by: string, hash: string }>} */
  const results = new Map();
  /** @type {(() => void) | null} */
  let disposeLink = null;
  /** @type {(() => void) | null} */
  let stopCb = null;
  let finished = false;

  // Each device writes in its own ink, in order of first appearance.
  /** @type {Map<string, number>} */
  const inks = new Map();
  /** @param {string} peer */
  const inkOf = (peer) => {
    let k = inks.get(peer);
    if (k === undefined) {
      k = inks.size;
      inks.set(peer, k);
    }
    return k;
  };
  /** Fingerprints seen so far, to name a device in the corrections. @type {Map<string, string[]>} */
  const prints = new Map();

  const linkSlot = h('div', { class: 'link-slot' });
  const progress = progressBar('run.progress');
  const stats = h('dl', { class: 'stats' });
  const presentCount = h('p', { class: 'present-count', 'aria-live': 'off' });
  const peersBody = h('tbody');
  const pendingList = h('ul', { class: 'pending' });
  const pendingSection = h('section', { class: 'card', hidden: true, 'aria-labelledby': 'pending-title' }, h('h2', { id: 'pending-title', i18n: 'run.pending' }), pendingList);
  const image = h('canvas', { class: 'fractal', width: MANDELBROT.size, height: MANDELBROT.size, role: 'img', i18nAttr: { 'aria-label': 'run.fractalAlt' } });
  const resultsSlot = h('div', { class: 'results' });
  const downloadBtn = h('button', { type: 'button', class: 'btn', props: { disabled: true } }, icon('download'), h('span', { i18n: embed ? 'run.downloadVectors' : 'run.download' }));
  const metaBtn = embed ? h('button', { type: 'button', class: 'btn secondary', props: { disabled: true } }, icon('download'), h('span', { i18n: 'run.downloadMeta' })) : null;
  const stopBtn = h('button', { type: 'button', class: 'btn danger' }, icon('stop'), h('span', { i18n: 'run.stop' }));
  const ctx2d = image.getContext('2d');

  // The page of squares: one per task, whole squares on the 32px ruling, each with its number.
  /** @type {Array<{ el: HTMLElement, state: 'waiting' | 'done' | 'wrong' | 'corrected' | 'failed', device: number, by: string, ms: number | null }>} */
  const squares = [];
  const grid = h('ol', { class: `task-grid${total > 999 ? ' dense' : ''}`, i18nAttr: { 'aria-label': 'run.tasks' } });
  if (total <= MAX_GRID) {
    for (let i = 0; i < total; i += 1) {
      // One roving tab stop for the whole grid: arrows move between squares.
      const el = h('li', { tabindex: i === 0 ? '0' : '-1' }, h('span', { class: 'n', 'aria-hidden': 'true', text: String(i) }));
      squares.push({ el, state: 'waiting', device: 0, by: '', ms: null });
      grid.append(el);
    }
  }
  /** @param {number} task */
  const labelSquare = (task) => {
    const sq = squares[task];
    if (!sq) return;
    let label = t(`run.task.${sq.state}`, { n: task, d: sq.device + 1 });
    // The device's symbols and the measured running time, once known; never a made-up value.
    const fp = sq.by ? prints.get(sq.by) : undefined;
    if (fp && sq.state !== 'waiting' && sq.state !== 'failed') label = t('run.task.time', { label, time: fmtFingerprint(fp) });
    if (sq.ms !== null) label = t('run.task.time', { label, time: fmtDuration(sq.ms) });
    sq.el.setAttribute('title', label);
    sq.el.setAttribute('aria-label', label);
  };
  // The detail line under the grid: the hovered or focused square, else the latest accepted task.
  const detail = h('p', { class: 'task-detail', 'aria-hidden': 'true' });
  /** @type {number | null} */
  let latest = null;
  /** @type {number | null} */
  let pointed = null;
  const renderDetail = () => {
    const task = pointed ?? latest;
    const sq = task === null ? undefined : squares[task];
    if (task === null || !sq) {
      setText(detail, 'run.detail.none');
      return;
    }
    const fp = sq.by ? prints.get(sq.by) : undefined;
    const who = sq.state === 'waiting' ? null : fp ? fmtFingerprint(fp) : sq.by ? t('run.device', { d: sq.device + 1 }) : null;
    delete detail.dataset.i18n;
    delete detail.dataset.i18nVars;
    detail.textContent = taskDetail(task, who, sq.ms);
  };
  /** @param {EventTarget | null} target */
  const squareAt = (target) => {
    const li = target instanceof Element ? target.closest('li') : null;
    const i = li ? squares.findIndex((q) => q.el === li) : -1;
    return i >= 0 ? i : null;
  };
  grid.addEventListener('pointerover', (e) => {
    const i = squareAt(e.target);
    if (i !== null && i !== pointed) {
      pointed = i;
      renderDetail();
    }
  });
  grid.addEventListener('pointerleave', () => {
    pointed = grid.contains(document.activeElement) ? squareAt(document.activeElement) : null;
    renderDetail();
  });
  grid.addEventListener('focusin', (e) => {
    pointed = squareAt(e.target);
    renderDetail();
  });
  grid.addEventListener('focusout', (e) => {
    if (e.relatedTarget instanceof Node && grid.contains(e.relatedTarget)) return;
    pointed = null;
    renderDetail();
  });
  grid.addEventListener('keydown', (e) => {
    const i = squareAt(e.target);
    if (i === null) return;
    const cols = Math.max(1, getComputedStyle(grid).gridTemplateColumns.split(' ').length);
    const moves = { ArrowRight: 1, ArrowLeft: -1, ArrowDown: cols, ArrowUp: -cols, Home: -i, End: squares.length - 1 - i };
    const step = moves[/** @type {keyof typeof moves} */ (e.key)];
    if (step === undefined) return;
    e.preventDefault();
    const next = Math.min(squares.length - 1, Math.max(0, i + step));
    squares[i].el.tabIndex = -1;
    squares[next].el.tabIndex = 0;
    squares[next].el.focus();
  });
  const labelAll = () => {
    squares.forEach((_, i) => labelSquare(i));
    renderDetail();
  };
  labelAll();
  const unLang = onLangChange(labelAll);

  // Corrections: one red note in the margin with the count, the entries in a disclosure.
  // In pencil while nothing was corrected; red from the first correction.
  const corrNote = h('p', { class: 'corr-note clean' }, icon('ring'), h('span', { i18n: 'run.corr.none' }));
  const corrBanned = h('p', { class: 'corr-banned', hidden: true });
  const corrList = h('ol', { class: 'corr-list' });
  const corrDetails = h('details', { class: 'corr-details', hidden: true }, h('summary', { i18n: 'run.corr.show' }), corrList);
  /** @param {string} peer */
  const who = (peer) => {
    const fp = prints.get(peer);
    return h('span', { class: 'who' }, h('span', { class: `swatch ink-${inkOf(peer) % INKS}`, 'aria-hidden': 'true' }), fp ? h('span', { class: 'emoji-cell', text: fmtFingerprint(fp) }) : h('span', { i18n: 'run.device', vars: { d: inkOf(peer) + 1 } }));
  };
  /** @param {string} key @param {Record<string, string | number>} vars @param {string} peer */
  const correct = (key, vars, peer) => {
    corrList.prepend(h('li', null, h('span', { i18n: key, vars }), who(peer)));
    corrDetails.hidden = false;
    corrNote.classList.remove('clean');
    corrSection.classList.remove('clean');
    const corr = /** @type {HTMLElement} */ (corrNote.lastElementChild);
    setText(corr, counts.mismatch === 1 ? 'run.corr.one' : 'run.corr.many', { n: fmtNumber(counts.mismatch) });
    if (counts.banned > 0) {
      corrBanned.hidden = false;
      setText(corrBanned, 'run.corr.bannedSum', { b: fmtNumber(counts.banned), r: fmtNumber(counts.redone) });
    }
  };

  const titleSlot = h('div', { class: 'job-head' });
  // Tasks the coordinator cannot check itself, waiting for a second capable device.
  const timings = new Timings();
  /** @type {Map<number, number>} */
  const waiting = new Map();
  const waitingNote = h('p', { class: 'waiting-note', role: 'status', hidden: true });
  const renderWaiting = () => {
    const n = waitingTotal(waiting);
    waitingNote.hidden = n === 0;
    if (n > 0) setText(waitingNote, n === 1 ? 'run.waiting.one' : 'run.waiting.many', { n: fmtNumber(n) });
  };
  const trackSection = h(
    'section',
    { class: 'track', 'aria-labelledby': 'track-title' },
    h('h2', { id: 'track-title', class: 'visually-hidden', i18n: 'run.track' }),
    squares.length > 0 ? grid : null,
    squares.length > 0 ? detail : null,
    resultsSlot,
  );
  const corrSection = h('section', { class: 'corrections clean', i18nAttr: { 'aria-label': 'run.corrections' } }, corrNote, corrBanned, corrDetails);
  // The work column (its heading is the job name) comes first in reading order; on wide
  // screens the grid puts the margin column to its left.
  const el = h(
    'div',
    { class: 'live' },
    h(
      'div',
      { class: 'work-col' },
      titleSlot,
      h('div', { class: 'figures' }, progress.el, stats),
      pendingSection,
      waitingNote,
      h(
        'div',
        { class: 'work' },
        trackSection,
        h(
          'section',
          { class: 'roster', 'aria-labelledby': 'peers-title' },
          h('h2', { id: 'peers-title', i18n: 'run.peers' }),
          h(
            'table',
            { class: 'peers devices' },
            h(
              'thead',
              null,
              h(
                'tr',
                null,
                h('th', { scope: 'col', class: 'c-peer', i18n: 'run.col.peer' }),
                h('th', { scope: 'col', class: 'c-kind', i18n: 'run.col.kind' }),
                h('th', { scope: 'col', class: 'num c-num', i18n: 'run.col.done' }),
                h('th', { scope: 'col', class: 'num c-num', i18n: 'run.col.verified' }),
                h('th', { scope: 'col', class: 'num c-time', i18nAttr: { title: 'run.col.timeFull' }, i18n: 'run.col.time' }),
                h('th', { scope: 'col', class: 'c-state', i18n: 'run.col.state' }),
              ),
            ),
            peersBody,
          ),
        ),
      ),
      // The actions close the grid and devices block, bottom right, above the fractal.
      h('div', { class: 'actions' }, stopBtn, metaBtn, downloadBtn),
      kind === 'mandelbrot' ? image : null,
    ),
    h('aside', { class: 'margin-col', i18nAttr: { 'aria-label': 'run.linkTitle' } }, linkSlot, corrSection),
  );

  // The squares sit on the page's ruling: the grid's top is pushed to the next 32px line of
  // the document (the ruling scrolls with it).
  let snapFrame = 0;
  const snap = () => {
    cancelAnimationFrame(snapFrame);
    snapFrame = requestAnimationFrame(() => {
      if (!grid.isConnected) return;
      const current = parseFloat(getComputedStyle(grid).getPropertyValue('--snap')) || 0;
      const top = grid.getBoundingClientRect().top + window.scrollY - current;
      const offset = (32 - (Math.round(top) % 32)) % 32;
      if (offset !== current) grid.style.setProperty('--snap', `${offset}px`);
    });
  };
  const ro = new ResizeObserver(snap);
  ro.observe(el);

  // The figures are built once; each update only rewrites the numbers.
  /** @type {Array<[string, () => number, number]>} key, value, decimals */
  const statRows = [
    ['run.stat.peers', () => counts.peers, 0],
    ['run.stat.rate', () => counts.rate, 1],
    ['run.stat.verified', () => counts.verified, 0],
    ['run.stat.mismatch', () => counts.mismatch, 0],
    ['run.stat.banned', () => counts.banned, 0],
    ['run.stat.redone', () => counts.redone, 0],
    ['run.stat.failed', () => counts.failed, 0],
  ];
  if (webrtc) statRows.push(['run.stat.direct', () => counts.direct, 0]);
  // Figures in blue ink; corrections in red, and only when there is one.
  const red = new Set(['run.stat.mismatch', 'run.stat.banned', 'run.stat.redone']);
  const statCells = statRows.map(([k, value, digits]) => ({ k, value, digits, dd: h('dd') }));
  stats.replaceChildren(...statCells.map((c) => h('div', { class: 'stat' }, h('dt', { i18n: c.k }), c.dd)));

  function renderStats() {
    for (const c of statCells) {
      const n = c.value();
      const text = fmtNumber(n, c.digits);
      if (c.dd.textContent !== text) c.dd.textContent = text;
      c.dd.classList.toggle('bad', red.has(c.k) && n > 0);
    }
    progress.set(counts.done, total, 'run.progressText', { done: fmtNumber(counts.done), total: fmtNumber(total) });
    setText(presentCount, 'run.presentCount', { done: fmtNumber(counts.done), total: fmtNumber(total) });
  }
  renderStats();
  const unLangStats = onLangChange(renderStats);

  /** @type {ReturnType<typeof setInterval> | null} */
  let poll = null;
  let session = 0;

  /** @param {SwarmStatus} st */
  function renderPeers(st) {
    counts.peers = st.peers.length;
    const known = prints.size;
    for (const p of st.peers) prints.set(p.peer, p.fingerprint);
    if (prints.size !== known) labelAll();
    if (st.peers.length === 0) {
      peersBody.replaceChildren(h('tr', { class: 'empty-row' }, h('td', { colspan: 6, i18n: 'run.devicesNone' })));
    } else {
      peersBody.replaceChildren(
        ...st.peers.map((p) =>
          h(
            'tr',
            { class: p.banned ? 'banned' : undefined },
            h('td', { class: 'emoji-cell' }, h('span', { class: `swatch ink-${inkOf(p.peer) % INKS}`, 'aria-hidden': 'true' }), fmtFingerprint(p.fingerprint)),
            // data-label names the column where a phone stacks each device as a block.
            h('td', { class: 'c-kind', i18n: p.browser ? 'run.kind.browser' : 'run.kind.cli', i18nAttr: { 'data-label': 'run.col.kind' } }),
            h('td', { class: 'num', text: fmtNumber(p.done), i18nAttr: { 'data-label': 'run.col.done' } }),
            h('td', { class: 'num', text: fmtNumber(p.verified), i18nAttr: { 'data-label': 'run.col.verified' } }),
            h('td', { class: 'num c-time', text: timings.text(p.peer) }),
            h('td', { class: 'c-state', i18n: p.banned ? 'run.state.banned' : p.inflight > 0 ? 'run.state.busy' : 'run.state.idle' }),
          ),
        ),
      );
    }
    renderStats();
  }
  renderPeers({ peers: [], jobs: [] });

  // Each tile is drawn in its device's ink on the page's paper. The colours are read once, and
  // again when the colour scheme changes, which repaints every tile kept so far.
  const readPalette = () => {
    const style = getComputedStyle(document.documentElement);
    return {
      paper: rgbOf(style.getPropertyValue('--paper'), [251, 252, 255]),
      inks: Array.from({ length: INKS }, (_, k) => rgbOf(style.getPropertyValue(`--ink-${k}`), [29, 59, 154])),
    };
  };
  let palette = readPalette();
  /** @param {number} task @param {Uint8Array} bytes @param {string} by */
  function paintTile(task, bytes, by) {
    if (!ctx2d) return;
    const m = MANDELBROT;
    const n = m.size / m.tile;
    const pixels = tilePixels(bytes, m.tile, m.iter, palette.paper, palette.inks[inkOf(by) % INKS] ?? [29, 59, 154]);
    if (!pixels) return;
    const img = ctx2d.createImageData(m.tile, m.tile);
    img.data.set(pixels);
    ctx2d.putImageData(img, (task % n) * m.tile, Math.floor(task / n) * m.tile);
  }
  const scheme = window.matchMedia('(prefers-color-scheme: dark)');
  const onScheme = () => {
    palette = readPalette();
    if (kind === 'mandelbrot') for (const [task, r] of results) paintTile(task, r.bytes, r.by);
  };
  scheme.addEventListener('change', onScheme);

  /** @param {number} task @param {'done' | 'wrong' | 'verified' | 'failed'} state @param {string} [peer] */
  function mark(task, state, peer) {
    const sq = squares[task];
    if (!sq) return;
    const shown = sq.by;
    if (state === 'verified') {
      // A rejected result verified again: the ring stays, with a small tick in its corner.
      if (sq.state === 'wrong') {
        sq.state = 'corrected';
        sq.el.classList.remove('wrong');
        sq.el.classList.add('corrected');
        labelSquare(task);
      }
      return;
    }
    if (peer !== undefined && peer !== sq.by) {
      // Another device's result: the time measured for the earlier one is not its own.
      sq.device = inkOf(peer);
      if (state === 'wrong') sq.ms = null;
      sq.by = peer;
    } else if (peer !== undefined) sq.device = inkOf(peer);
    if (state === 'done') {
      sq.el.classList.remove('failed');
      sq.el.classList.add('done', `ink-${sq.device % INKS}`);
      if (sq.state !== 'wrong' && sq.state !== 'corrected') sq.state = 'done';
    } else if (state === 'wrong') {
      sq.state = 'wrong';
      sq.el.classList.remove('corrected');
      sq.el.classList.add('wrong');
      // The rejected result was the one on the page: the square goes back to paper inside its ring.
      if (peer === undefined || peer === shown) {
        sq.el.classList.remove('done', ...Array.from({ length: INKS }, (_, k) => `ink-${k}`));
        sq.ms = null;
      }
    } else if (!sq.el.classList.contains('done')) {
      sq.state = 'failed';
      sq.el.classList.add('failed');
    }
    labelSquare(task);
  }

  /** @param {string} peer @param {string[]} fp */
  function addPending(peer, fp) {
    pendingSection.hidden = false;
    const li = h('li', { class: 'row' }, h('span', { class: 'emoji-cell', text: fmtFingerprint(fp) }));
    /** @param {boolean} accept */
    const answer = (accept) => {
      app.bridge.call('approve', { session, peer, accept }).catch((e) => console.error('approve', e));
      // Focus moves on to the next device waiting for an answer, else to the job's title.
      const next = /** @type {HTMLElement | null} */ (li.nextElementSibling ?? li.previousElementSibling);
      li.remove();
      if (!pendingList.firstChild) pendingSection.hidden = true;
      focusOn(next ? /** @type {HTMLElement | null} */ (next.querySelector('button')) : titleSlot.querySelector('h1'));
    };
    li.append(
      h('button', { type: 'button', class: 'btn', i18n: 'run.accept', on: { click: () => answer(true) } }),
      h('button', { type: 'button', class: 'btn secondary', i18n: 'run.refuse', on: { click: () => answer(false) } }),
    );
    pendingList.append(li);
    announce(t('run.pendingAnnounce'));
  }

  function summarize() {
    if (kind !== 'skycmd') return;
    /** @type {Map<string, { n: number, score: number, crashes: number }>} */
    const per = new Map();
    const dec = new TextDecoder();
    for (const r of results.values()) {
      try {
        const o = JSON.parse(dec.decode(r.bytes));
        const g = per.get(o.game) ?? { n: 0, score: 0, crashes: 0 };
        g.n += 1;
        g.score += Number(o.metrics.mean_score) || 0;
        g.crashes += Number(o.metrics.crashes) || 0;
        per.set(o.game, g);
      } catch (e) {
        console.warn('unreadable skycmd output', e);
      }
    }
    resultsSlot.replaceChildren(
      h(
        'table',
        { class: 'peers' },
        h('caption', { i18n: 'run.sky.caption' }),
        h('thead', null, h('tr', null, h('th', { scope: 'col', i18n: 'run.sky.game' }), h('th', { scope: 'col', class: 'num', i18n: 'run.sky.score' }), h('th', { scope: 'col', class: 'num', i18n: 'run.sky.crashes' }))),
        h('tbody', null, ...[...per].map(([g, v]) => h('tr', null, h('th', { scope: 'row', text: g }), h('td', { class: 'num', text: fmtNumber(v.score / v.n, 2) }), h('td', { class: 'num', text: fmtNumber(v.crashes) })))),
      ),
    );
  }

  /** @returns {Map<number, Uint8Array>} */
  const outputs = () => new Map([...results].map(([k, r]) => [k, r.bytes]));

  downloadBtn.addEventListener('click', () => {
    if (kind === 'mandelbrot') {
      image.toBlob((blob) => {
        if (blob) download(blob, 'nuee-mandelbrot.png');
      }, 'image/png');
      return;
    }
    if (embed) {
      const { vectors } = mergeEmbed(embed, outputs());
      download(new Blob([vectors], { type: 'application/octet-stream' }), 'vectors.f32');
      return;
    }
    if (kind === 'skycmdDagger') {
      download(new Blob([mergeJsonl(outputs())], { type: 'application/x-ndjson' }), 'nuee-skycmd-dagger.jsonl');
      return;
    }
    const dec = new TextDecoder('utf-8', { fatal: true });
    const out = [...results].sort((a, b) => a[0] - b[0]).map(([task, r]) => {
      let output;
      try {
        output = JSON.parse(dec.decode(r.bytes));
      } catch {
        output = { base64: base64(r.bytes) };
      }
      return { task, by: r.by, hash: r.hash, output };
    });
    download(new Blob([JSON.stringify(out, null, 2)], { type: 'application/json' }), 'nuee-results.json');
  });
  if (metaBtn && embed) {
    metaBtn.addEventListener('click', () => {
      download(new Blob([mergeEmbed(embed, outputs()).meta], { type: 'application/x-ndjson' }), 'meta.jsonl');
    });
  }
  stopBtn.addEventListener('click', () => {
    if (stopCb) stopCb();
    el.remove();
  });

  return {
    el,
    /** Moves the page title and its spec line to the top of the work column. @param {HTMLElement} title @param {HTMLElement} specLine */
    head(title, specLine) {
      titleSlot.replaceChildren(title, specLine);
      snap();
    },
    /** @param {string} link @param {string[]} fp @param {number} s */
    async started(link, fp, s) {
      session = s;
      const panel = await linkPanel(app, link, fp, presentCount, 'run.linkHint');
      disposeLink = panel.dispose;
      linkSlot.replaceChildren(panel.el, fingerprintBlock(fp, 'run.fingerprint'));
      snap();
      announce(t('run.online'));
      poll = setInterval(() => {
        app.bridge.call('status', { session }).then(renderPeers, (e) => console.warn('status', e));
      }, 2000);
    },
    /** @param {NueeEvent} e @param {number} _s */
    onEvent(e, _s) {
      if (reduceWaiting(waiting, e)) renderWaiting();
      switch (e.type) {
        case 'accepted': {
          results.set(e.task, { bytes: e.output, by: e.by, hash: e.hash });
          timings.add(e.by, e.ms);
          const sq = squares[e.task];
          if (sq) {
            sq.by = e.by;
            sq.ms = typeof e.ms === 'number' ? e.ms : null;
          }
          mark(e.task, 'done', e.by);
          if (sq) latest = e.task;
          renderDetail();
          if (kind === 'mandelbrot') paintTile(e.task, e.output, e.by);
          break;
        }
        case 'progress':
          counts.done = e.done;
          counts.verified = e.verified;
          counts.rate = e.rate;
          renderStats();
          break;
        case 'verified':
          mark(e.task, 'verified');
          break;
        case 'mismatch':
          counts.mismatch += 1;
          mark(e.task, 'wrong', e.peer);
          renderDetail();
          correct('run.corr.mismatch', { task: e.task }, e.peer);
          renderStats();
          break;
        case 'banned':
          counts.banned += 1;
          counts.redone += e.redo;
          correct('run.corr.banned', { redo: e.redo }, e.peer);
          renderStats();
          announce(t('run.bannedAnnounce', { redo: e.redo }));
          break;
        case 'taskFailed':
          counts.failed += 1;
          mark(e.task, 'failed');
          renderStats();
          break;
        case 'peerPending':
          prints.set(e.peer, e.fingerprint);
          addPending(e.peer, e.fingerprint);
          break;
        case 'peerJoined':
          prints.set(e.peer, e.fingerprint);
          app.bridge.call('status', { session }).then(renderPeers, (err) => console.warn('status', err));
          break;
        case 'peerLeft':
          app.bridge.call('status', { session }).then(renderPeers, (err) => console.warn('status', err));
          break;
        case 'jobDone':
          finished = true;
          downloadBtn.disabled = false;
          if (metaBtn) metaBtn.disabled = false;
          summarize();
          announce(t('run.doneAnnounce'));
          resultsSlot.prepend(h('p', { class: 'ok' }, icon('check'), h('span', { i18n: e.failed > 0 ? 'run.doneFailed' : 'run.done', vars: { failed: e.failed } })));
          break;
        case 'error':
          resultsSlot.prepend(alertBox(`err.${e.code}`));
          break;
        default:
          break;
      }
    },
    /** One more result arrived over a direct lane. */
    laneResult() {
      counts.direct += 1;
      renderStats();
    },
    /** @param {() => void} fn */
    onStop(fn) {
      stopCb = fn;
    },
    get finished() {
      return finished;
    },
    dispose() {
      if (poll) clearInterval(poll);
      cancelAnimationFrame(snapFrame);
      ro.disconnect();
      unLang();
      unLangStats();
      scheme.removeEventListener('change', onScheme);
      if (disposeLink) disposeLink();
    },
  };
}
