// @ts-check
// Boot: feature checks, language, the versioned package, the iroh worker, then the router.
/** @typedef {import('./api').OpMap} OpMap */
/** @typedef {import('./api').Executor} Executor */
/** @typedef {import('./router').ViewName} ViewName */
/** @typedef {import('./router').Route} Route */
import { initLang, toggleLang, getLang, onLangChange, t } from './i18n.js';
import { parseHash } from './router.js';
import { detect, wasmProblem } from './lib/caps.js';
import { Bridge, BridgeError } from './lib/bridge.js';
import { Pool, makeExecutor, registerExecutor } from './lib/pool.js';
import { gpuExecutor } from './worker/gpu.js';
import { embedExecutor } from './worker/embed.js';
import { llmExecutor, llmAllowed } from './worker/llm.js';
import { h, alertBox, relocalize } from './lib/dom.js';
import { PREVIEW_MARK } from './lib/inline.js';

/**
 * What a view returns: `cleanup` runs when leaving it; `busy` makes leaving ask first.
 * @typedef {{ cleanup?: () => void, busy?: () => boolean }} Mounted
 * @typedef {{ app: App, root: HTMLElement, route: Route }} ViewContext
 * @typedef {{ render: (ctx: ViewContext) => Mounted | void }} ViewModule
 */

/** Views by name, loaded on first use. Later steps add their views here. */
/** @type {Record<ViewName, () => Promise<ViewModule>>} */
export const VIEWS = {
  home: () => import('./views/home.js'),
  run: () => import('./views/run.js'),
  train: () => import('./views/train.js'),
  join: () => import('./views/join.js'),
  file: () => import('./views/file.js'),
  share: () => import('./views/share.js'),
  about: () => import('./views/about.js'),
  service: () => import('./views/service.js'),
  site: () => import('./views/site.js'),
};

const WORKER_PATH = /\/app\/worker\/((iroh|task)-worker|gpu|embed|llm)\.js$/;

/**
 * The only Trusted Types policy: it turns our own worker script paths into script URLs, and
 * the inert documents built by lib/inline.js into HTML for the sandboxed preview frame. It
 * refuses anything else.
 * @type {{ createScriptURL(u: string): unknown, createHTML(s: string): unknown } | null}
 */
const policy = (() => {
  const tt = /** @type {{ trustedTypes?: { createPolicy(name: string, rules: { createScriptURL(u: string): string, createHTML(s: string): string }): { createScriptURL(u: string): unknown, createHTML(s: string): unknown } } }} */ (
    /** @type {unknown} */ (window)
  ).trustedTypes;
  if (!tt) return null;
  return tt.createPolicy('nuee-worker', {
    createScriptURL(u) {
      const url = new URL(u, location.href);
      if (url.origin !== location.origin || !WORKER_PATH.test(url.pathname)) throw new TypeError(`refusing worker script ${u}`);
      return url.href;
    },
    createHTML(s) {
      if (!s.startsWith(PREVIEW_MARK) || /<script/i.test(s)) throw new TypeError('refusing HTML that is not a preview document');
      return s;
    },
  });
})();

/** @param {URL} url */
function startWorker(url) {
  const src = policy ? policy.createScriptURL(url.href) : url.href;
  return new Worker(/** @type {string} */ (src), { type: 'module' });
}

/**
 * The relay this page uses: `?relay=` may name `disabled`, `n0`, an https relay, or a
 * loopback http one (local testing). Anything else is ignored.
 * @param {string} search
 * @returns {string | undefined}
 */
export function relayFromQuery(search) {
  const r = new URLSearchParams(search).get('relay');
  if (!r) return undefined;
  if (r === 'disabled' || r === 'n0') return r;
  try {
    const u = new URL(r);
    if (u.protocol === 'https:') return u.href;
    if (u.protocol === 'http:' && (u.hostname === '127.0.0.1' || u.hostname === 'localhost')) return u.href;
  } catch (e) {
    console.warn('ignoring ?relay=', e);
  }
  return undefined;
}

export class App {
  /** @param {ReturnType<typeof detect>} caps @param {string} version */
  constructor(caps, version) {
    this.caps = caps;
    this.version = version;
    this.linkBase = location.origin + location.pathname.replace(/index\.html$/, '');
    this.relay = relayFromQuery(location.search);
    /** Executors of live sessions; the iroh worker's calls go to the active one. */
    /** @type {Executor | null} */
    this.exec = null;
    const worker = startWorker(new URL(`./worker/iroh-worker.js?v=${encodeURIComponent(version)}`, import.meta.url));
    this.bridge = new Bridge(worker, (task) => {
      if (!this.exec) return Promise.resolve({ ok: false, kind: 'cancelled', detail: 'no session runs tasks' });
      return this.exec(task);
    });
    /** @type {Promise<OpMap['start']['value']> | null} */
    this.started = null;
  }

  /** Starts the node once (binding to the relay takes up to 10 s). */
  node() {
    if (!this.started) {
      this.started = this.bridge.call('start', { relay: this.relay, linkBase: this.linkBase });
      this.started.catch(() => {
        this.started = null;
      });
    }
    return this.started;
  }

  /** A task pool for `slots` concurrent leases, wired as the executor of this page. @param {number} slots */
  pool(slots) {
    const url = new URL('./worker/task-worker.js', import.meta.url);
    const pool = new Pool({
      create: () => /** @type {import('./lib/pool').TaskWorkerLike} */ (/** @type {unknown} */ (startWorker(url))),
      device: { cores: this.caps.cores, mobile: this.caps.mobile, deviceMemoryGb: this.caps.deviceMemoryGb },
      slots,
    });
    this.exec = makeExecutor(pool);
    return pool;
  }

  /** @param {import('./lib/pool').Pool} pool */
  releasePool(pool) {
    pool.stop();
    this.exec = null;
  }

  /** The srcdoc value of a preview document from lib/inline.js, through the HTML policy. @param {string} html */
  previewHtml(html) {
    return policy ? policy.createHTML(html) : html;
  }
}

/** @param {string} key */
function fatal(key) {
  const main = document.getElementById('main');
  if (!main) return;
  main.replaceChildren(h('section', { class: 'page narrow' }, h('h1', { i18n: 'boot.title' }), alertBox(key)));
}

async function boot() {
  initLang(navigator.languages);
  relocalize(document);
  const toggle = document.getElementById('lang-toggle');
  if (toggle) {
    // The button is named in the language it switches to ("FR — passer en français").
    const langOfToggle = () => toggle.setAttribute('lang', getLang() === 'fr' ? 'en' : 'fr');
    langOfToggle();
    onLangChange(langOfToggle);
    toggle.addEventListener('click', () => toggleLang());
  }

  const caps = detect(window);
  const problem = wasmProblem(caps);
  if (problem) {
    fatal(problem);
    return;
  }
  let version;
  try {
    const res = await fetch(new URL('../pkg/current.json', import.meta.url), { cache: 'no-store' });
    if (!res.ok) throw new Error(`pkg/current.json: HTTP ${res.status}`);
    const cur = await res.json();
    version = String(cur.version);
  } catch (e) {
    console.error(e);
    fatal('boot.failed');
    return;
  }
  if (caps.webgpu) registerExecutor('wgsl', gpuExecutor(() => startWorker(new URL('./worker/gpu.js', import.meta.url))));
  registerExecutor('embed', embedExecutor(() => startWorker(new URL('./worker/embed.js', import.meta.url))));
  if (llmAllowed(caps.deviceMemoryGb)) registerExecutor('llm', llmExecutor(() => startWorker(new URL('./worker/llm.js', import.meta.url))));
  const app = new App(caps, version);
  startRouter(app);
}

/** @param {App} app */
function startRouter(app) {
  const main = /** @type {HTMLElement} */ (document.getElementById('main'));
  /** @type {Mounted} */
  let current = {};
  let currentHash = location.hash;
  let token = 0;

  async function show() {
    if (location.hash !== currentHash && current.busy && current.busy()) {
      if (!window.confirm(t('leave.confirm'))) {
        history.replaceState(null, '', currentHash || '#/');
        return;
      }
    }
    currentHash = location.hash;
    const mine = (token += 1);
    const route = parseHash(location.hash);
    let mod;
    try {
      mod = await VIEWS[route.view]();
    } catch (e) {
      console.error(e);
      fatal('boot.failed');
      return;
    }
    if (mine !== token) return;
    if (current.cleanup) current.cleanup();
    const root = h('div', { class: 'view' });
    main.replaceChildren(root);
    current = mod.render({ app, root, route }) ?? {};
    for (const a of document.querySelectorAll('.nav a')) {
      if (a.getAttribute('href') === `#/${route.view}`) a.setAttribute('aria-current', 'page');
      else a.removeAttribute('aria-current');
    }
    const heading = root.querySelector('h1');
    document.title = heading && heading.textContent ? `${heading.textContent} · Nuée` : 'Nuée';
    // Move focus to the new content on navigation, not on the first load.
    if (mine > 1) main.focus();
  }

  window.addEventListener('hashchange', () => {
    show().catch((e) => {
      console.error(e);
      fatal('boot.failed');
    });
  });
  window.addEventListener('beforeunload', (e) => {
    if (current.busy && current.busy()) e.preventDefault();
  });
  app.bridge.onEvent((event) => {
    if (event.type === 'error') console.error('node error', event.code, event.message);
  });
  show().catch((e) => {
    console.error(e);
    fatal('boot.failed');
  });
}

boot().catch((e) => {
  console.error(e);
  fatal(e instanceof BridgeError ? `err.${e.code}` : 'boot.failed');
});
