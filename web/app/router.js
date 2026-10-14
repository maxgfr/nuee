// @ts-check
// Hash routes. Link routes carry `K/PAYLOAD` where K is the link kind letter; the payload is
// checked for its alphabet here and fully parsed (checksum, version) by the wasm package.

/**
 * @typedef {'home' | 'run' | 'train' | 'share' | 'about' | 'join' | 'file' | 'service' | 'site'} ViewName
 * @typedef {{ view: ViewName, link?: string, linkError?: 'link_truncated' | 'link_corrupt' }} Route
 */

/** Plain pages. Later views add an entry here and a loader in main.js. */
/** @type {Map<string, ViewName>} */
export const PAGES = new Map([
  ['', 'home'],
  ['run', 'run'],
  ['train', 'train'],
  ['share', 'share'],
  ['about', 'about'],
]);

/** Link kinds a page can open (the `P` pin link is for the CLI only). */
/** @type {Map<string, ViewName>} */
export const LINK_VIEWS = new Map([
  ['J', 'join'],
  ['F', 'file'],
  ['X', 'service'],
  ['S', 'site'],
]);

const BASE32 = /^[A-Z2-7]+$/;

/**
 * Maps `location.hash` to a view. Unknown paths go home; a link route with a broken payload
 * goes home with the reason, so the page can say what is wrong.
 * @param {string} hash
 * @returns {Route}
 */
export function parseHash(hash) {
  let path = hash.startsWith('#') ? hash.slice(1) : hash;
  path = path.replace(/^\/+/, '').replace(/\/+$/, '');
  const page = PAGES.get(path);
  if (page) return { view: page };
  const slash = path.indexOf('/');
  const kind = slash === -1 ? path : path.slice(0, slash);
  const view = LINK_VIEWS.get(kind);
  if (!view) return { view: 'home' };
  const payload = slash === -1 ? '' : path.slice(slash + 1).trim();
  if (payload === '') return { view: 'home', linkError: 'link_truncated' };
  if (!BASE32.test(payload)) return { view: 'home', linkError: 'link_corrupt' };
  return { view, link: `${kind}/${payload}` };
}

/**
 * Finds a Nuée link in pasted text: a full URL, a fragment, or `K/PAYLOAD`.
 * @param {string} text
 * @returns {{ kind: string, link: string } | null}
 */
export function extractLink(text) {
  const s = text.trim().replace(/\s+/g, '');
  const m = /(?:^|#\/?)([JFXSP])\/([A-Za-z2-7]*)$/.exec(s);
  if (!m || !m[1]) return null;
  return { kind: m[1], link: `${m[1]}/${(m[2] ?? '').toUpperCase()}` };
}

/** @param {string} link `K/PAYLOAD` */
export function hashOf(link) {
  return `#/${link}`;
}
