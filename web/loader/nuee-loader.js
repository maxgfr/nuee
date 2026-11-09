// Written by `nuee site loader`. Fetches the site from the machines of its group, checks it, and
// lets nuee-sw.js serve it from this origin. Every page of the site loads this script again, which
// looks for a newer version from time to time and keeps it for the next visit.
const LINK = "__NUEE_LINK__";
const BASE = "__NUEE_BASE__";

const META = 'nuee-meta';
/** After a check that worked, and after one that did not (the machines may be off). */
const CHECK_EVERY = 5 * 60_000;
const RETRY_EVERY = 60_000;
/** A first visit gives up after this long rather than spin. */
const GIVE_UP = 90_000;
const scope = new URL('./', import.meta.url);
const CURRENT = new URL('nuee-current', scope).href;
const LAST_CHECK = `nuee-last-check:${scope.pathname}`;
const LAST_TRY = `nuee-last-try:${scope.pathname}`;

const MIME = {
  html: 'text/html; charset=utf-8', htm: 'text/html; charset=utf-8', js: 'text/javascript; charset=utf-8',
  mjs: 'text/javascript; charset=utf-8', css: 'text/css; charset=utf-8', json: 'application/json',
  webmanifest: 'application/manifest+json', map: 'application/json', md: 'text/markdown; charset=utf-8',
  csv: 'text/csv; charset=utf-8', txt: 'text/plain; charset=utf-8', xml: 'application/xml',
  svg: 'image/svg+xml', png: 'image/png', jpg: 'image/jpeg', jpeg: 'image/jpeg', gif: 'image/gif',
  webp: 'image/webp', avif: 'image/avif', ico: 'image/x-icon', woff: 'font/woff', woff2: 'font/woff2',
  ttf: 'font/ttf', otf: 'font/otf', wasm: 'application/wasm', pdf: 'application/pdf', zip: 'application/zip',
  mp4: 'video/mp4', webm: 'video/webm', mp3: 'audio/mpeg', ogg: 'audio/ogg', wav: 'audio/wav',
};

/** What went wrong, in words a visitor can act on. */
class Stop extends Error {}
const BLOCKED = 'This browser does not let the page install what serves the site (private browsing, or a host without https). Open it in a normal window.';
const NO_PACKAGE = `The Nuée code at ${BASE} cannot be loaded. Check the connection, then try again.`;
const NO_KEEPER = 'No machine that keeps this site answered: they may all be off. Try again later.';
const BAD_SITE = 'The site arrived but does not match its link, so it was not shown.';

/** The loader's own page, as opposed to a page of the site that loads this script too. */
const status = document.getElementById('nuee-status');

/** @param {string} text */
function say(text) {
  if (status) status.textContent = text;
}

/** Tells the visitor what failed and lets them try again. @param {string} text */
function fail(text) {
  if (!status) return;
  say(text);
  if (document.getElementById('nuee-retry')) return;
  const retry = document.createElement('button');
  retry.id = 'nuee-retry';
  retry.type = 'button';
  retry.textContent = 'Try again';
  retry.addEventListener('click', () => {
    localStorage.removeItem(LAST_TRY);
    location.reload();
  });
  status.after(retry);
}

/** The iroh worker of the Nuée page, and a way to call it. */
async function connect() {
  let version;
  try {
    const res = await fetch(new URL('pkg/current.json', BASE), { cache: 'no-store' });
    version = (await res.json()).version;
  } catch {
    throw new Stop(NO_PACKAGE);
  }
  const source = new URL(`app/worker/iroh-worker.js?v=${encodeURIComponent(version)}`, BASE);
  const blob = new Blob([`import ${JSON.stringify(source.href)};`], { type: 'text/javascript' });
  const worker = new Worker(URL.createObjectURL(blob), { type: 'module' });
  let next = 1;
  let broken = null;
  const waiting = new Map();
  const breakAll = (error) => {
    broken = error;
    for (const { reject } of waiting.values()) reject(error);
    waiting.clear();
  };
  // Before its first answer an error means the code did not load; after it, a worker error is
  // one of its own tasks failing, which the call that needs it reports.
  let answered = false;
  worker.onerror = (e) => {
    if (answered) {
      console.warn('nuee loader: worker error', e.message);
      return;
    }
    e.preventDefault();
    breakAll(new Stop(NO_PACKAGE));
  };
  worker.onmessage = (e) => {
    answered = true;
    const m = e.data;
    if (typeof m.id !== 'number' || !waiting.has(m.id)) return;
    const { resolve, reject } = waiting.get(m.id);
    waiting.delete(m.id);
    if (m.ok) resolve(m.value);
    else reject(new Error(m.error?.message ?? 'failed'));
  };
  // parseLink, start, resolveName and fetchSite: loaders already deployed rely on these names.
  const call = (op, args) =>
    new Promise((resolve, reject) => {
      if (broken) return reject(broken);
      const id = next;
      next += 1;
      waiting.set(id, { resolve, reject });
      worker.postMessage({ id, op, args });
    });
  return { call, stop: () => worker.terminate() };
}

/** Published files sit in one top-level folder; the site starts inside it. */
function rooted(files) {
  const tops = new Set(files.map((f) => f.name.split('/')[0]));
  const nested = tops.size === 1 && files.every((f) => f.name.includes('/'));
  return files.map((f) => ({ name: nested ? f.name.slice(f.name.indexOf('/') + 1) : f.name, bytes: f.bytes }));
}

/** The pages load this script too, so that they can find a newer version. */
function withLoader(bytes) {
  const tag = new TextEncoder().encode(`<script type="module" src="${new URL('nuee-loader.js', scope).pathname}"></script>`);
  const out = new Uint8Array(bytes.length + tag.length);
  out.set(bytes);
  out.set(tag, bytes.length);
  return out;
}

async function store(got) {
  const name = `nuee-site-${got.hash}`;
  const site = await caches.open(name);
  for (const file of rooted(got.files)) {
    const ext = file.name.includes('.') ? file.name.split('.').pop().toLowerCase() : '';
    const html = ext === 'html' || ext === 'htm';
    // A known type is never sniffed; an unknown one is left to the browser.
    const headers = MIME[ext] ? { 'content-type': MIME[ext], 'x-content-type-options': 'nosniff' } : {};
    await site.put(new URL(file.name, scope).href, new Response(html ? withLoader(file.bytes) : file.bytes, { headers }));
  }
  const before = await currentCache();
  await (await caches.open(META)).put(CURRENT, new Response(JSON.stringify({ cache: name, hash: got.hash })));
  // The version just replaced stays until the next update: a tab may still be reading it.
  for (const old of await caches.keys()) {
    if (old.startsWith('nuee-site-') && old !== name && old !== before) await caches.delete(old);
  }
}

async function currentCache() {
  const pointer = await (await caches.open(META)).match(CURRENT);
  return pointer ? (await pointer.json()).cache : null;
}

/** Where to go when the stored site has no page at this address. */
async function storedPages(name) {
  const keys = await (await caches.open(name)).keys();
  return keys.map((r) => r.url).filter((u) => /\.html?$/.test(u));
}

async function check(before) {
  say('Starting…');
  const nuee = await connect();
  try {
    const info = await nuee.call('parseLink', { link: LINK }).catch(() => {
      throw new Stop('This loader holds a damaged link: write it again with `nuee site loader`.');
    });
    await nuee.call('start', info.relay ? { relay: info.relay } : {}).catch(() => {
      throw new Stop(NO_KEEPER);
    });
    say('Asking the machines that keep this site…');
    const { link } = await nuee.call('resolveName', { link: LINK }).catch(() => {
      throw new Stop(NO_KEEPER);
    });
    const site = await nuee.call('parseLink', { link });
    if (before === `nuee-site-${site.hash}`) return false;
    say(`Fetching the site (${Math.ceil((site.size ?? 0) / 1024)} kB) and checking it…`);
    const got = await nuee.call('fetchSite', { link }).catch((e) => {
      throw new Stop(/match|integrity|decrypt/i.test(String(e?.message)) ? BAD_SITE : NO_KEEPER);
    });
    await store(got);
    return true;
  } finally {
    nuee.stop();
  }
}

async function main() {
  if (LINK.startsWith('__NUEE')) {
    say('This is the template of the site loader: `nuee site loader` writes a working copy.');
    return;
  }
  if (!('serviceWorker' in navigator) || !('caches' in window)) throw new Stop(BLOCKED);
  try {
    await navigator.serviceWorker.register(new URL('nuee-sw.js', scope), { scope: scope.pathname });
    await navigator.serviceWorker.ready;
  } catch {
    throw new Stop(BLOCKED);
  }
  const before = await currentCache();
  if (before && status && navigator.serviceWorker.controller) {
    // The site is stored, yet this address shows the loader: the site has no page here.
    const pages = await storedPages(before);
    if (pages.length > 0) {
      say('The site has no index.html at this address. Its pages:');
      const list = document.createElement('ul');
      for (const url of pages) {
        const a = document.createElement('a');
        a.href = url;
        a.textContent = new URL(url).pathname;
        const li = document.createElement('li');
        li.append(a);
        list.append(li);
      }
      status.after(list);
      return;
    }
  }
  const now = Date.now();
  const since = (key) => now - Number(localStorage.getItem(key) ?? 0);
  if (before && (since(LAST_CHECK) < CHECK_EVERY || since(LAST_TRY) < RETRY_EVERY)) return;
  const timeout = new Promise((_, reject) => setTimeout(() => reject(new Stop(NO_KEEPER)), GIVE_UP));
  // Only a check that failed delays the next one: leaving the page in the middle of one does not.
  const changed = await Promise.race([check(before), timeout]).catch((e) => {
    localStorage.setItem(LAST_TRY, String(Date.now()));
    throw e;
  });
  localStorage.setItem(LAST_CHECK, String(Date.now()));
  if (changed && !before) location.reload();
}

main().catch((e) => {
  console.warn('nuee loader:', e);
  // A page of the site already shows the stored version: only the loader's own page says why.
  fail(e instanceof Stop ? e.message : NO_KEEPER);
});
