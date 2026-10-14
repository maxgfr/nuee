// Shared pieces of the e2e specs: CLI processes with isolated directories, browsers whose
// pages fail the test on any console error or CSP violation, and a BLAKE3 for checking bytes.
import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { createInterface } from 'node:readline';
import { chromium, firefox, webkit, expect } from '@playwright/test';

/** The services started by setup.mjs. */
export function services() {
  const relay = process.env.NUEE_E2E_RELAY;
  const web = process.env.NUEE_E2E_WEB;
  const bin = process.env.NUEE_BIN;
  const root = process.env.NUEE_ROOT;
  if (!relay || !web || !bin || !root) throw new Error('run the suite through scripts/e2e.sh');
  return { relay, web, bin, root, tmp: process.env.NUEE_E2E_TMP ?? tmpdir() };
}

/**
 * A page URL on the local server: `hash` is `#/…`, or a full link printed by the CLI
 * (whose base is the local server). The page always uses the local relay.
 * @param {string} hashOrLink
 */
export function pageUrl(hashOrLink) {
  const { web, relay } = services();
  const hash = hashOrLink.startsWith('#') ? hashOrLink : new URL(hashOrLink).hash;
  const url = new URL(web);
  url.search = `?relay=${encodeURIComponent(relay)}`;
  url.hash = hash;
  return url.href;
}

/** The engines, by name. */
export const ENGINES = { chromium, firefox, webkit };

/** Collects the CLI processes, browsers and temp dirs of one test, and stops them all. */
export class Harness {
  constructor() {
    /** @type {Cli[]} */
    this.procs = [];
    /** @type {import('@playwright/test').Browser[]} */
    this.browsers = [];
    /** @type {string[]} */
    this.dirs = [];
    /** @type {string[]} */
    this.problems = [];
  }

  /** A fresh directory removed at the end. */
  dir() {
    const d = mkdtempSync(path.join(services().tmp, 'node-'));
    this.dirs.push(d);
    return d;
  }

  /**
   * Starts `nuee` with its own HOME, config and data directories, the local relay and web base.
   * @param {string[]} args arguments after the global ones
   * @param {{ stdin?: string, dir?: string }} [opts]
   */
  cli(args, opts = {}) {
    const { bin, relay, web } = services();
    const dir = opts.dir ?? this.dir();
    const env = {
      ...process.env,
      HOME: dir,
      NUEE_CONFIG_DIR: path.join(dir, 'config'),
      NUEE_DATA_DIR: path.join(dir, 'data'),
      NO_COLOR: '1',
    };
    delete env.XDG_CONFIG_HOME;
    delete env.XDG_DATA_HOME;
    const full = ['--relay', relay, '--web-base', web, ...args];
    const p = new Cli(spawn(bin, full, { cwd: dir, env, stdio: ['pipe', 'pipe', 'pipe'] }), full.join(' '), dir);
    if (opts.stdin !== undefined) p.child.stdin?.end(opts.stdin);
    else p.child.stdin?.end();
    this.procs.push(p);
    return p;
  }

  /**
   * Launches an engine; its pages fail the test on console errors, page errors and CSP violations.
   * @param {'chromium' | 'firefox' | 'webkit'} name
   */
  async browser(name) {
    const b = await ENGINES[name].launch();
    this.browsers.push(b);
    return b;
  }

  /**
   * A new page of `browser` (English UI, downloads allowed) watched for errors.
   * @param {import('@playwright/test').Browser} browser
   * @param {string} label
   */
  async page(browser, label) {
    const context = await browser.newContext({ locale: 'en-US', acceptDownloads: true });
    const page = await context.newPage();
    watch(page, label, this.problems);
    return page;
  }

  /** Fails when any watched page reported a problem. */
  checkPages() {
    expect(this.problems, 'console errors or CSP violations').toEqual([]);
  }

  async close() {
    await Promise.all(this.procs.map((p) => p.stop()));
    await Promise.all(this.browsers.map((b) => b.close()));
    for (const d of this.dirs) rmSync(d, { recursive: true, force: true });
  }
}

/**
 * Records console errors, uncaught errors and CSP violations of `page` (and of its workers:
 * their errors reach the page console) into `problems`.
 * @param {import('@playwright/test').Page} page
 * @param {string} label
 * @param {string[]} problems
 */
export function watch(page, label, problems) {
  page.on('console', (msg) => {
    if (msg.type() === 'error') problems.push(`${label} console: ${msg.text()}`);
  });
  page.on('pageerror', (e) => problems.push(`${label} page error: ${e.message}`));
  page.on('crash', () => problems.push(`${label} crashed`));
  page.addInitScript(() => {
    document.addEventListener('securitypolicyviolation', (e) => {
      console.error(`CSP violation: ${e.violatedDirective} blocked ${e.blockedURI}`);
    });
  });
}

/** A running `nuee` process with its stdout split in lines. */
export class Cli {
  /**
   * @param {import('node:child_process').ChildProcess} child
   * @param {string} cmd
   * @param {string} dir
   */
  constructor(child, cmd, dir) {
    this.child = child;
    this.cmd = cmd;
    this.dir = dir;
    /** @type {string[]} */
    this.lines = [];
    this.stderr = '';
    /** @type {Array<{ pred: (l: string) => boolean, resolve: (l: string) => void }>} */
    this.waiters = [];
    /** @type {Promise<number | null>} */
    this.exited = new Promise((resolve) => {
      child.once('exit', (code) => resolve(code));
    });
    if (child.stdout) {
      createInterface({ input: child.stdout }).on('line', (line) => {
        this.lines.push(line);
        this.waiters = this.waiters.filter((w) => {
          if (!w.pred(line)) return true;
          w.resolve(line);
          return false;
        });
      });
    }
    child.stderr?.on('data', (d) => {
      this.stderr += d.toString();
    });
  }

  /** NDJSON events printed so far (non-JSON lines are skipped). */
  events() {
    const out = [];
    for (const l of this.lines) {
      if (!l.startsWith('{')) continue;
      out.push(JSON.parse(l));
    }
    return out;
  }

  /**
   * The first NDJSON event named `name` (already printed or upcoming).
   * @param {string} name
   * @param {number} [ms]
   */
  event(name, ms = 60_000) {
    const pred = (/** @type {string} */ l) => l.startsWith('{') && JSON.parse(l).event === name;
    const seen = this.lines.find(pred);
    if (seen) return Promise.resolve(JSON.parse(seen));
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`no ${name} event from \`nuee ${this.cmd}\` within ${ms} ms\n${this.stderr}`)), ms);
      this.waiters.push({
        pred,
        resolve: (l) => {
          clearTimeout(timer);
          resolve(JSON.parse(l));
        },
      });
      this.exited.then((code) => {
        clearTimeout(timer);
        reject(new Error(`\`nuee ${this.cmd}\` exited (${code}) before a ${name} event\n${this.stderr}`));
      });
    });
  }

  /** Waits for the exit code, within `ms`. @param {number} ms */
  async exit(ms) {
    let timer;
    const late = new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(`\`nuee ${this.cmd}\` still running after ${ms} ms\n${this.stderr}`)), ms);
    });
    try {
      return await Promise.race([this.exited, late]);
    } finally {
      clearTimeout(timer);
    }
  }

  async stop() {
    if (this.child.exitCode !== null || this.child.signalCode !== null) return;
    const timer = setTimeout(() => this.child.kill('SIGKILL'), 5_000);
    this.child.kill('SIGTERM');
    await this.exited;
    clearTimeout(timer);
  }
}

// ---------------------------------------------------------------------------------------------
// BLAKE3 (unkeyed hash, 32-byte output), after the reference implementation.

const IV = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
const PERM = [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8];
const CHUNK_START = 1;
const CHUNK_END = 2;
const PARENT = 4;
const ROOT = 8;
const CHUNK = 1024;

/** @param {number} x @param {number} n */
const rotr = (x, n) => (x >>> n) | (x << (32 - n));

/** @param {Uint32Array} s @param {number} a @param {number} b @param {number} c @param {number} d @param {number} x @param {number} y */
function g(s, a, b, c, d, x, y) {
  s[a] = s[a] + s[b] + x;
  s[d] = rotr(s[d] ^ s[a], 16);
  s[c] = s[c] + s[d];
  s[b] = rotr(s[b] ^ s[c], 12);
  s[a] = s[a] + s[b] + y;
  s[d] = rotr(s[d] ^ s[a], 8);
  s[c] = s[c] + s[d];
  s[b] = rotr(s[b] ^ s[c], 7);
}

/**
 * @param {ArrayLike<number>} cv 8 words
 * @param {ArrayLike<number>} block 16 words
 * @param {number} counter
 * @param {number} len
 * @param {number} flags
 */
function compress(cv, block, counter, len, flags) {
  const s = new Uint32Array(16);
  for (let i = 0; i < 8; i += 1) s[i] = cv[i];
  for (let i = 0; i < 4; i += 1) s[8 + i] = IV[i];
  s[12] = counter >>> 0;
  s[13] = Math.floor(counter / 0x100000000);
  s[14] = len;
  s[15] = flags;
  let m = Uint32Array.from(block);
  for (let r = 0; r < 7; r += 1) {
    g(s, 0, 4, 8, 12, m[0], m[1]);
    g(s, 1, 5, 9, 13, m[2], m[3]);
    g(s, 2, 6, 10, 14, m[4], m[5]);
    g(s, 3, 7, 11, 15, m[6], m[7]);
    g(s, 0, 5, 10, 15, m[8], m[9]);
    g(s, 1, 6, 11, 12, m[10], m[11]);
    g(s, 2, 7, 8, 13, m[12], m[13]);
    g(s, 3, 4, 9, 14, m[14], m[15]);
    m = Uint32Array.from(PERM, (i) => m[i]);
  }
  for (let i = 0; i < 8; i += 1) {
    s[i] ^= s[i + 8];
    s[i + 8] ^= cv[i];
  }
  return s;
}

/** @param {Uint8Array} bytes up to 64 bytes */
function words(bytes) {
  const padded = new Uint8Array(64);
  padded.set(bytes);
  const v = new DataView(padded.buffer);
  return Uint32Array.from({ length: 16 }, (_, i) => v.getUint32(i * 4, true));
}

/** The pending output of a chunk: its last block, not compressed yet. @param {Uint8Array} chunk @param {number} index */
function chunkOutput(chunk, index) {
  let cv = Uint32Array.from(IV);
  const blocks = Math.max(1, Math.ceil(chunk.length / 64));
  for (let b = 0; b < blocks - 1; b += 1) {
    const flags = b === 0 ? CHUNK_START : 0;
    cv = compress(cv, words(chunk.subarray(b * 64, b * 64 + 64)), index, 64, flags).slice(0, 8);
  }
  const last = chunk.subarray((blocks - 1) * 64);
  return { cv, block: words(last), counter: index, len: last.length, flags: (blocks === 1 ? CHUNK_START : 0) | CHUNK_END };
}

/** @param {{ cv: Uint32Array, block: Uint32Array, counter: number, len: number, flags: number }} o */
const chainOf = (o) => compress(o.cv, o.block, o.counter, o.len, o.flags).slice(0, 8);

/** @param {Uint32Array} left @param {Uint32Array} right */
function parentOutput(left, right) {
  const block = new Uint32Array(16);
  block.set(left);
  block.set(right, 8);
  return { cv: Uint32Array.from(IV), block, counter: 0, len: 64, flags: PARENT };
}

/**
 * Hex BLAKE3 of `bytes`.
 * @param {Uint8Array} bytes
 */
export function blake3(bytes) {
  const chunks = Math.max(1, Math.ceil(bytes.length / CHUNK));
  /** @type {Uint32Array[]} */
  const stack = [];
  for (let i = 0; i < chunks - 1; i += 1) {
    let cv = chainOf(chunkOutput(bytes.subarray(i * CHUNK, (i + 1) * CHUNK), i));
    let total = i + 1;
    while ((total & 1) === 0) {
      const left = /** @type {Uint32Array} */ (stack.pop());
      cv = chainOf(parentOutput(left, cv));
      total /= 2;
    }
    stack.push(cv);
  }
  let out = chunkOutput(bytes.subarray((chunks - 1) * CHUNK), chunks - 1);
  while (stack.length > 0) {
    const left = /** @type {Uint32Array} */ (stack.pop());
    out = parentOutput(left, chainOf(out));
  }
  const root = compress(out.cv, out.block, out.counter, out.len, out.flags | ROOT);
  let hex = '';
  for (let i = 0; i < 8; i += 1) {
    const w = root[i];
    for (let k = 0; k < 4; k += 1) hex += ((w >>> (8 * k)) & 0xff).toString(16).padStart(2, '0');
  }
  return hex;
}
