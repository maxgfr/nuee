// @ts-check
// Main-thread client of the iroh worker (api.d.ts section 2): one promise per request id,
// session events to subscribers, executor calls forwarded to the task pool.
/** @typedef {import('../api').Op} Op */
/** @typedef {import('../api').OpMap} OpMap */
/** @typedef {import('../api').NueeEvent} NueeEvent */
/** @typedef {import('../api').NueeError} NueeError */
/** @typedef {import('../api').ErrorCode} ErrorCode */
/** @typedef {import('../api').Executor} Executor */
/** @typedef {import('../api').TaskOutcome} TaskOutcome */
/** @typedef {import('../api').ToWorker} ToWorker */
import { owned } from './pool.js';

const CODES = new Set([
  'relay_unreachable', 'swarm_ended', 'link_truncated', 'link_version', 'link_corrupt', 'link_kind',
  'admission_refused', 'integrity', 'decrypt', 'too_large', 'unsupported', 'invalid_module',
  'invalid_argument', 'internal',
]);

/** A failed request; `code` picks the text shown, `message` is detail for logs. */
export class BridgeError extends Error {
  /** @param {ErrorCode} code @param {string} message */
  constructor(code, message) {
    super(message);
    this.name = 'BridgeError';
    /** @type {ErrorCode} */
    this.code = code;
  }
}

/** Normalizes anything thrown into a NueeError shape. @param {unknown} e @returns {NueeError} */
export function toNueeError(e) {
  if (typeof e === 'object' && e !== null) {
    const o = /** @type {Record<string, unknown>} */ (e);
    if (typeof o.code === 'string' && CODES.has(o.code)) {
      return { code: /** @type {ErrorCode} */ (o.code), message: typeof o.message === 'string' ? o.message : '' };
    }
    if (typeof o.message === 'string') return { code: 'internal', message: o.message };
  }
  return { code: 'internal', message: String(e) };
}

/** The error code of anything thrown. @param {unknown} e @returns {ErrorCode} */
export function codeOf(e) {
  return e instanceof BridgeError ? e.code : toNueeError(e).code;
}

/**
 * The part of a Worker the bridge uses.
 * @typedef {{
 *   postMessage(msg: ToWorker, transfer?: Transferable[]): void,
 *   addEventListener(type: 'message', fn: (e: { data: unknown }) => void): void,
 *   addEventListener(type: 'error', fn: (e: { message?: string }) => void): void,
 *   terminate?(): void,
 * }} WorkerLike
 */

/** @typedef {(event: NueeEvent, session: number | undefined) => void} EventListener */

/** Buffers of every Uint8Array found (shallowly, in arrays and objects) that can be moved. @param {unknown} v */
export function transferables(v) {
  /** @type {Set<ArrayBuffer>} */
  const out = new Set();
  /** @param {unknown} x @param {number} depth */
  const walk = (x, depth) => {
    if (depth > 4 || typeof x !== 'object' || x === null) return;
    if (x instanceof Uint8Array) {
      if (owned(x)) out.add(/** @type {ArrayBuffer} */ (x.buffer));
      return;
    }
    for (const y of Array.isArray(x) ? x : Object.values(x)) walk(y, depth + 1);
  };
  walk(v, 0);
  return [...out];
}

export class Bridge {
  /**
   * @param {WorkerLike} worker
   * @param {Executor} executor runs leases asked by the worker
   */
  constructor(worker, executor) {
    this.worker = worker;
    this.executor = executor;
    this.nextId = 1;
    /** @type {Map<number, { resolve: (v: never) => void, reject: (e: BridgeError) => void }>} */
    this.pending = new Map();
    /** @type {Set<EventListener>} */
    this.listeners = new Set();
    /** @type {Map<number, (got: number, total: number) => void>} */
    this.progress = new Map();
    /** @type {BridgeError | null} */
    this.dead = null;
    worker.addEventListener('message', (e) => this.receive(e.data));
    worker.addEventListener('error', (e) => this.fail(new BridgeError('internal', e.message ?? 'the network worker failed to load')));
  }

  /**
   * Sends one request. Byte arrays in `args` are transferred when `move` is true (the caller
   * gives them up); otherwise they are copied.
   * @template {Op} K
   * @param {K} op
   * @param {OpMap[K]['args']} args
   * @param {{ move?: boolean, onProgress?: (got: number, total: number) => void }} [opts]
   * @returns {Promise<OpMap[K]['value']>}
   */
  call(op, args, opts = {}) {
    if (this.dead) return Promise.reject(this.dead);
    const id = this.nextId;
    this.nextId += 1;
    let sent = args;
    if (opts.onProgress && (op === 'fetchFile' || op === 'fetchSite')) {
      this.progress.set(id, opts.onProgress);
      sent = /** @type {OpMap[K]['args']} */ ({ .../** @type {object} */ (args), progress: id });
    }
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      /** @type {import('../api').Request<K>} */
      const req = { id, op, args: sent };
      this.worker.postMessage(/** @type {ToWorker} */ (req), opts.move ? transferables(args) : []);
    });
  }

  /** Subscribes to events; returns the unsubscribe function. @param {EventListener} fn */
  onEvent(fn) {
    this.listeners.add(fn);
    return () => {
      this.listeners.delete(fn);
    };
  }

  /** @param {BridgeError} err */
  fail(err) {
    this.dead = err;
    for (const p of this.pending.values()) p.reject(err);
    this.pending.clear();
    this.progress.clear();
  }

  /** @param {unknown} data */
  receive(data) {
    if (typeof data !== 'object' || data === null) return;
    const m = /** @type {Record<string, unknown>} */ (data);
    if (typeof m.execId === 'number' && typeof m.task === 'object' && m.task !== null) {
      this.exec(m.execId, /** @type {import('../api').TaskRequest} */ (m.task));
    } else if (typeof m.id === 'number' && typeof m.ok === 'boolean') {
      const p = this.pending.get(m.id);
      if (!p) return;
      this.pending.delete(m.id);
      this.progress.delete(m.id);
      if (m.ok) p.resolve(/** @type {never} */ (m.value));
      else {
        const err = toNueeError(m.error);
        p.reject(new BridgeError(err.code, err.message));
      }
    } else if (typeof m.progress === 'number' && typeof m.got === 'number') {
      const fn = this.progress.get(m.progress);
      if (fn) fn(m.got, typeof m.total === 'number' ? m.total : 0);
    } else if (typeof m.event === 'object' && m.event !== null && 'type' in m.event) {
      const event = /** @type {NueeEvent} */ (m.event);
      const session = typeof m.session === 'number' ? m.session : undefined;
      for (const fn of [...this.listeners]) fn(event, session);
    }
  }

  /**
   * @param {number} execId
   * @param {import('../api').TaskRequest} task
   */
  exec(execId, task) {
    /** @param {TaskOutcome} outcome */
    const reply = (outcome) => {
      const transfer = outcome.ok && owned(outcome.output) ? [outcome.output.buffer] : [];
      this.worker.postMessage({ execId, outcome }, transfer);
    };
    let promise;
    try {
      promise = this.executor(task);
    } catch (e) {
      promise = Promise.reject(e);
    }
    promise.then(reply, (e) => {
      console.error('executor failed', e);
      reply({ ok: false, kind: 'io', detail: toNueeError(e).message });
    });
  }
}
