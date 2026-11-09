// Written by `nuee site loader`. Fetches the site from the machines of its group, checks it, and
// lets nuee-sw.js serve it from this origin. Every page of the site loads this script again, which
// looks for a newer version from time to time and keeps it for the next visit.
const LINK = "__NUEE_LINK__";
const BASE = "__NUEE_BASE__";

const META = 'nuee-meta';
const CHECK_EVERY = 5 * 60_000;
const scope = new URL('./', import.meta.url);
const CURRENT = new URL('nuee-current', scope).href;
const LAST_CHECK = `nuee-last-check:${scope.pathname}`;

const MIME = {
  html: 'text/html; charset=utf-8', htm: 'text/html; charset=utf-8', js: 'text/javascript; charset=utf-8',
  mjs: 'text/javascript; charset=utf-8', css: 'text/css; charset=utf-8', json: 'application/json',
  svg: 'image/svg+xml', png: 'image/png', jpg: 'image/jpeg', jpeg: 'image/jpeg', gif: 'image/gif',
  webp: 'image/webp', avif: 'image/avif', ico: 'image/x-icon', woff: 'font/woff', woff2: 'font/woff2',
  ttf: 'font/ttf', otf: 'font/otf', txt: 'text/plain; charset=utf-8', xml: 'application/xml',
  wasm: 'application/wasm', pdf: 'application/pdf', mp4: 'video/mp4', webm: 'video/webm', mp3: 'audio/mpeg',
};

/** @param {string} text */
function say(text) {
  const el = document.getElementById('nuee-status');
  if (el) el.textContent = text;
}

/** The iroh worker of the Nuée page, and a way to call it. */
async function connect() {
  const current = await (await fetch(new URL('pkg/current.json', BASE), { cache: 'no-store' })).json();
  const source = new URL(`app/worker/iroh-worker.js?v=${encodeURIComponent(current.version)}`, BASE);
  const blob = new Blob([`import ${JSON.stringify(source.href)};`], { type: 'text/javascript' });
  const worker = new Worker(URL.createObjectURL(blob), { type: 'module' });
  let next = 1;
  const waiting = new Map();
  worker.onmessage = (e) => {
    const m = e.data;
    if (typeof m.id !== 'number' || !waiting.has(m.id)) return;
    const { resolve, reject } = waiting.get(m.id);
    waiting.delete(m.id);
    if (m.ok) resolve(m.value);
    else reject(new Error(m.error?.message ?? 'failed'));
  };
  const call = (op, args) =>
    new Promise((resolve, reject) => {
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
  const tag = `<script type="module" src="${new URL('nuee-loader.js', scope).pathname}"></script>`;
  const out = new Uint8Array(bytes.length + tag.length);
  out.set(bytes);
  out.set(new TextEncoder().encode(tag), bytes.length);
  return out;
}

async function store(got) {
  const name = `nuee-site-${got.hash}`;
  const site = await caches.open(name);
  for (const file of rooted(got.files)) {
    const ext = file.name.split('.').pop().toLowerCase();
    const html = ext === 'html' || ext === 'htm';
    const headers = { 'content-type': MIME[ext] ?? 'application/octet-stream', 'x-content-type-options': 'nosniff' };
    await site.put(new URL(file.name, scope).href, new Response(html ? withLoader(file.bytes) : file.bytes, { headers }));
  }
  const meta = await caches.open(META);
  const before = await currentCache();
  await meta.put(CURRENT, new Response(JSON.stringify({ cache: name, hash: got.hash })));
  if (before && before !== name) await caches.delete(before);
}

async function currentCache() {
  const pointer = await (await caches.open(META)).match(CURRENT);
  return pointer ? (await pointer.json()).cache : null;
}

async function main() {
  if (!('serviceWorker' in navigator) || !('caches' in window)) {
    say('This site needs a browser with service workers, over https.');
    return;
  }
  await navigator.serviceWorker.register(new URL('nuee-sw.js', scope), { scope: scope.pathname });
  await navigator.serviceWorker.ready;
  const before = await currentCache();
  if (before && Date.now() - Number(localStorage.getItem(LAST_CHECK) ?? 0) < CHECK_EVERY) return;
  const nuee = await connect();
  try {
    const info = await nuee.call('parseLink', { link: LINK });
    await nuee.call('start', info.relay ? { relay: info.relay } : {});
    const { link } = await nuee.call('resolveName', { link: LINK });
    const site = await nuee.call('parseLink', { link });
    if (before === `nuee-site-${site.hash}`) {
      localStorage.setItem(LAST_CHECK, String(Date.now()));
      return;
    }
    await store(await nuee.call('fetchSite', { link }));
    localStorage.setItem(LAST_CHECK, String(Date.now()));
    if (!before) location.reload();
  } finally {
    nuee.stop();
  }
}

main().catch((e) => {
  console.warn('nuee loader:', e);
  say('No machine of this site’s group answered. Try again later.');
});
