// @ts-check
// The task worker pool. Each worker runs one lease at a time on a fresh instance; the pool
// enforces deadlines by terminating the worker and starting a fresh one.
/** @typedef {import('../api').TaskRequest} TaskRequest */
/** @typedef {import('../api').TaskOutcome} TaskOutcome */
/** @typedef {import('../api').TaskRun} TaskRun */
/** @typedef {import('../api').TaskKind} TaskKind */
/** @typedef {import('../api').Executor} Executor */
/** @typedef {import('../api').FailKind} FailKind */

export const MiB = 2 ** 20;
export const GiB = 2 ** 30;
/** Total memory all task instances may claim together. */
export const BUDGET_MOBILE = GiB;
export const BUDGET_DESKTOP = 4 * GiB;

const FAIL_KINDS = new Set(['trap', 'oom', 'timeout', 'stack_overflow', 'invalid', 'unsupported', 'cancelled', 'io']);

/**
 * @typedef {{ cores: number, mobile: boolean, deviceMemoryGb?: number }} Device
 */

/** Memory budget for all task instances together. @param {Device} d */
export function memoryBudget(d) {
  let budget = d.mobile ? BUDGET_MOBILE : BUDGET_DESKTOP;
  // navigator.deviceMemory is coarse (and capped at 8): use half of it at most.
  if (d.deviceMemoryGb) budget = Math.min(budget, (d.deviceMemoryGb * GiB) / 2);
  return Math.max(budget, 256 * MiB);
}

/**
 * How many leases may run at once.
 * @param {Device & { wanted: number, maxMemory: number }} o
 */
export function poolSize(o) {
  const cores = Math.max(1, Math.floor(o.cores) || 1);
  const wanted = Math.min(cores, Math.max(0, Math.floor(o.wanted) || 0));
  if (wanted === 0) return 0;
  const per = Math.max(o.maxMemory, 16 * MiB);
  const byMemory = Math.max(1, Math.floor(memoryBudget(o) / per));
  return Math.min(wanted, byMemory);
}

/** Half the cores, rounded up: the default of the contribution slider. @param {number} cores */
export function defaultCores(cores) {
  return Math.max(1, Math.ceil(Math.max(1, cores) / 2));
}

/**
 * Memory offered to a swarm, in MiB, for `slots` concurrent leases.
 * @param {Device} d
 * @param {number} slots
 */
export function offeredMemMb(d, slots) {
  const per = Math.floor(memoryBudget(d) / Math.max(1, slots) / MiB);
  return Math.max(64, Math.min(1024, per));
}

/**
 * The part of a Worker the pool uses (a real Worker, or a fake in tests).
 * @typedef {{
 *   postMessage(msg: TaskRun, transfer?: Transferable[]): void,
 *   terminate(): void,
 *   onmessage: ((e: { data: unknown }) => void) | null,
 *   onerror: ((e: { message?: string }) => void) | null,
 * }} TaskWorkerLike
 */

/**
 * @typedef {{ w: TaskWorkerLike, busy: boolean, modules: Set<string>, abort?: (o: TaskOutcome) => void }} Slot
 * @typedef {{ req: TaskRequest, resolve: (o: TaskOutcome) => void }} Job
 */

/** @param {FailKind} kind @param {string} detail @returns {TaskOutcome} */
export function fail(kind, detail) {
  return { ok: false, kind, detail };
}

/** @param {unknown} v @returns {TaskOutcome} */
function checkOutcome(v) {
  if (typeof v === 'object' && v !== null) {
    const o = /** @type {Record<string, unknown>} */ (v);
    if (o.ok === true && o.output instanceof Uint8Array) return { ok: true, output: o.output };
    if (o.ok === false && typeof o.kind === 'string' && FAIL_KINDS.has(o.kind)) {
      return fail(/** @type {FailKind} */ (o.kind), typeof o.detail === 'string' ? o.detail : '');
    }
  }
  return fail('io', 'malformed reply from a task worker');
}

/** Whether a view owns its whole buffer, so it can be transferred. @param {Uint8Array} u */
export function owned(u) {
  return u.byteOffset === 0 && u.byteLength === u.buffer.byteLength && u.buffer instanceof ArrayBuffer;
}

export class Pool {
  /**
   * @param {{ create: () => TaskWorkerLike, device: Device, slots: number }} o
   */
  constructor(o) {
    this.create = o.create;
    this.device = o.device;
    this.slots = o.slots;
    /** @type {Slot[]} */
    this.workers = [];
    /** @type {Job[]} */
    this.queue = [];
    /** Module bytes by hash, as first received; sent to workers that lack them. */
    /** @type {Map<string, Uint8Array>} */
    this.modules = new Map();
    this.running = 0;
    this.stopped = false;
  }

  /** @param {number} n concurrent leases (0 holds new leases in the queue) */
  setSlots(n) {
    this.slots = Math.max(0, Math.floor(n));
    this.trim();
    this.pump();
  }

  /** @param {number} maxMemory */
  limit(maxMemory) {
    return poolSize({ ...this.device, wanted: this.slots, maxMemory });
  }

  /** @param {TaskRequest} req @returns {Promise<TaskOutcome>} */
  run(req) {
    if (this.stopped) return Promise.resolve(fail('cancelled', 'the pool is stopped'));
    if (req.moduleHash && req.module && !this.modules.has(req.moduleHash)) this.modules.set(req.moduleHash, req.module);
    return new Promise((resolve) => {
      this.queue.push({ req, resolve });
      this.pump();
    });
  }

  pump() {
    while (this.queue.length > 0 && !this.stopped) {
      const head = this.queue[0];
      if (!head || this.running >= this.limit(head.req.maxMemory)) return;
      this.queue.shift();
      this.dispatch(head);
    }
  }

  /** Drops idle workers beyond the slot count. */
  trim() {
    const keep = Math.max(this.slots, this.running);
    for (let i = this.workers.length - 1; i >= 0 && this.workers.length > keep; i -= 1) {
      const s = this.workers[i];
      if (s && !s.busy) {
        s.w.terminate();
        this.workers.splice(i, 1);
      }
    }
  }

  /** @returns {Slot} */
  idle() {
    const free = this.workers.find((s) => !s.busy);
    if (free) return free;
    const s = { w: this.create(), busy: false, modules: new Set() };
    this.workers.push(s);
    return s;
  }

  /** @param {Slot} s */
  drop(s) {
    s.w.onmessage = null;
    s.w.onerror = null;
    s.w.terminate();
    const i = this.workers.indexOf(s);
    if (i !== -1) this.workers.splice(i, 1);
  }

  /** @param {Job} job */
  dispatch(job) {
    const { req } = job;
    let module;
    if (req.moduleHash) {
      const known = this.modules.get(req.moduleHash);
      if (!known) {
        job.resolve(fail('invalid', `module ${req.moduleHash} was never received`));
        return;
      }
      module = known;
    }
    const s = this.idle();
    s.busy = true;
    this.running += 1;
    let settled = false;
    /** @param {TaskOutcome} out @param {boolean} replace */
    const finish = (out, replace) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      this.running -= 1;
      if (replace) this.drop(s);
      else s.busy = false;
      job.resolve(out);
      this.trim();
      this.pump();
    };
    const timer = setTimeout(() => finish(fail('timeout', `deadline of ${req.deadlineMs} ms exceeded`), true), req.deadlineMs);
    s.w.onmessage = (e) => {
      if (req.moduleHash) s.modules.add(req.moduleHash);
      finish(checkOutcome(e.data), false);
    };
    s.w.onerror = (e) => finish(fail('io', e.message ?? 'task worker error'), true);
    s.abort = (o) => finish(o, true);
    /** @type {TaskRun} */
    const msg = {
      op: 'run',
      lease: req.lease,
      kind: req.kind,
      moduleHash: req.moduleHash,
      module: module && req.moduleHash && !s.modules.has(req.moduleHash) ? module.slice() : undefined,
      input: req.input,
      deadlineMs: req.deadlineMs,
      maxMemory: req.maxMemory,
    };
    /** @type {Transferable[]} */
    const transfer = [];
    if (owned(req.input)) transfer.push(req.input.buffer);
    if (msg.module) transfer.push(msg.module.buffer);
    s.w.postMessage(msg, transfer);
  }

  stop() {
    this.stopped = true;
    for (const job of this.queue.splice(0)) job.resolve(fail('cancelled', 'stopped'));
    for (const s of [...this.workers]) {
      if (s.busy && s.abort) s.abort(fail('cancelled', 'stopped'));
      else this.drop(s);
    }
  }
}

/** Executors for kinds other than plain wasm (S-018 wgsl, S-019 embed, S-020 llm register here). */
/** @type {Map<TaskKind, Executor>} */
export const kindExecutors = new Map();

/** @param {TaskKind} kind @param {Executor} ex */
export function registerExecutor(kind, ex) {
  kindExecutors.set(kind, ex);
}

/** Kinds this page can run. @returns {TaskKind[]} */
export function supportedKinds() {
  return ['wasm', 'trainWasm', ...[...kindExecutors.keys()].filter((k) => k !== 'wasm' && k !== 'trainWasm')];
}

/**
 * The executor handed to the iroh worker: wasm and trainWasm leases (same task ABI) go to the
 * pool, other kinds to their registered executor.
 * @param {Pool} pool
 * @param {Map<TaskKind, Executor>} [extra]
 * @returns {Executor}
 */
export function makeExecutor(pool, extra = kindExecutors) {
  return (task) => {
    const ex = extra.get(task.kind);
    if (ex) return ex(task);
    if (task.kind === 'wasm' || task.kind === 'trainWasm') return pool.run(task);
    return Promise.resolve(fail('unsupported', `no executor for ${task.kind} in this page`));
  };
}
