#!/usr/bin/env node
// Static server for web/ (local testing and e2e): loopback only, no directory listing,
// no caching, no COOP/COEP (the app must work without them, as on GitHub Pages).
//
// Usage: node scripts/serve-web.mjs [--port N] [--root DIR]
// Prints `serving http://127.0.0.1:<port>/` once listening.

import { createServer } from 'node:http';
import { createReadStream } from 'node:fs';
import { stat } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));

/** @param {string[]} argv */
function parseArgs(argv) {
  let port = 0;
  let root = path.join(here, '..', 'web');
  for (let i = 0; i < argv.length; i += 1) {
    const a = argv[i];
    if (a === '--port') port = Number(argv[++i]);
    else if (a === '--root') root = path.resolve(argv[++i] ?? '');
    else throw new Error(`unknown argument ${a}`);
  }
  if (!Number.isInteger(port) || port < 0 || port > 65535) throw new Error('--port expects 0..65535');
  return { port, root: path.resolve(root) };
}

const TYPES = new Map([
  ['.html', 'text/html; charset=utf-8'],
  ['.js', 'text/javascript; charset=utf-8'],
  ['.mjs', 'text/javascript; charset=utf-8'],
  ['.css', 'text/css; charset=utf-8'],
  ['.json', 'application/json'],
  ['.webmanifest', 'application/manifest+json'],
  ['.wasm', 'application/wasm'],
  ['.svg', 'image/svg+xml'],
  ['.png', 'image/png'],
  ['.ico', 'image/x-icon'],
  ['.woff2', 'font/woff2'],
  ['.woff', 'font/woff'],
  ['.ttf', 'font/ttf'],
  ['.otf', 'font/otf'],
  ['.txt', 'text/plain; charset=utf-8'],
  ['.gguf', 'application/octet-stream'],
]);

/**
 * Maps a URL path to a file under `root`, or null when it escapes it.
 * @param {string} root
 * @param {string} urlPath
 */
export function resolvePath(root, urlPath) {
  let decoded;
  try {
    decoded = decodeURIComponent(urlPath);
  } catch {
    return null;
  }
  if (decoded.includes('\0')) return null;
  const rel = decoded.endsWith('/') ? `${decoded}index.html` : decoded;
  const full = path.resolve(root, `.${path.posix.normalize(`/${rel}`)}`);
  if (full !== root && !full.startsWith(root + path.sep)) return null;
  return full;
}

const { port, root } = parseArgs(process.argv.slice(2));

const server = createServer(async (req, res) => {
  const send = (/** @type {number} */ status, /** @type {string} */ text) => {
    res.writeHead(status, { 'content-type': 'text/plain; charset=utf-8', 'cache-control': 'no-store' });
    res.end(text);
  };
  if (req.method !== 'GET' && req.method !== 'HEAD') {
    send(405, 'method not allowed');
    return;
  }
  const url = new URL(req.url ?? '/', 'http://127.0.0.1');
  const file = resolvePath(root, url.pathname);
  if (!file) {
    send(404, 'not found');
    return;
  }
  let info;
  try {
    info = await stat(file);
  } catch (e) {
    if (/** @type {NodeJS.ErrnoException} */ (e).code !== 'ENOENT') console.error(e);
    send(404, 'not found');
    return;
  }
  if (!info.isFile()) {
    send(404, 'not found');
    return;
  }
  res.writeHead(200, {
    'content-type': TYPES.get(path.extname(file).toLowerCase()) ?? 'application/octet-stream',
    'content-length': info.size,
    'cache-control': 'no-store',
    'x-content-type-options': 'nosniff',
  });
  if (req.method === 'HEAD') {
    res.end();
    return;
  }
  const stream = createReadStream(file);
  stream.on('error', (e) => {
    console.error(e);
    res.destroy(e);
  });
  stream.pipe(res);
});

server.listen(port, '127.0.0.1', () => {
  const addr = server.address();
  if (addr === null || typeof addr === 'string') throw new Error('no address');
  console.log(`serving http://127.0.0.1:${addr.port}/`);
});

for (const sig of ['SIGINT', 'SIGTERM']) {
  process.on(sig, () => {
    server.close();
    server.closeAllConnections();
    process.exit(0);
  });
}
