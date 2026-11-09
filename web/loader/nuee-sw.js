// Serves the site that nuee-loader.js stored. Whatever it does not hold comes from the network,
// starting with the loader page on a first visit.
const META = 'nuee-meta';
const CURRENT = new URL('nuee-current', self.registration.scope).href;
const OWN = ['nuee-sw.js', 'nuee-loader.js'].map((f) => new URL(f, self.registration.scope).href);

self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (e) => e.waitUntil(self.clients.claim()));

/** @param {Request} request */
async function stored(request) {
  const url = new URL(request.url);
  url.search = '';
  url.hash = '';
  const pointer = await (await caches.open(META)).match(CURRENT);
  if (!pointer) return undefined;
  const site = await caches.open((await pointer.json()).cache);
  const candidates = url.pathname.endsWith('/') ? [`${url.href}index.html`] : [url.href, `${url.href}/index.html`, `${url.href}.html`];
  for (const candidate of candidates) {
    const hit = await site.match(candidate);
    if (hit) return hit;
  }
  return undefined;
}

self.addEventListener('fetch', (e) => {
  const { request } = e;
  if (request.method !== 'GET' || new URL(request.url).origin !== self.location.origin) return;
  if (OWN.includes(new URL(request.url).href.split('?')[0])) return;
  // A request the page dropped, or the network gone: answer as the browser would, not with an error.
  const failed = () => (request.mode === 'navigate' ? Response.error() : new Response(null, { status: 504 }));
  e.respondWith(stored(request).then((hit) => hit ?? fetch(request).catch(failed)));
});
