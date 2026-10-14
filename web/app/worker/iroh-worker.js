// @ts-check
// The iroh worker: loads the versioned wasm package, owns the only NueeNode and every
// session, and speaks the postMessage protocol of api.d.ts section 2.
/** @typedef {import('../api').Op} Op */
/** @typedef {import('../api').OpMap} OpMap */
/** @typedef {import('../api').NueeEvent} NueeEvent */
/** @typedef {import('../api').NueeError} NueeError */
/** @typedef {import('../api').TaskRequest} TaskRequest */
/** @typedef {import('../api').TaskOutcome} TaskOutcome */
/** @typedef {import('../api').FromWorker} FromWorker */
/** @typedef {import('../api').NueeNode} NueeNode */
/** @typedef {import('../api').HostSession} HostSession */
/** @typedef {import('../api').JoinSession} JoinSession */
/** @typedef {import('../api').ShareSession} ShareSession */

/**
 * The worker global, seen through the two members used here.
 * @type {{ location: { href: string }, postMessage(m: FromWorker, t: Transferable[]): void, onmessage: ((e: MessageEvent) => void) | null }}
 */
const scope = /** @type {never} */ (self);

const version = new URL(scope.location.href).searchParams.get('v') ?? '';

/** @type {Promise<typeof import('../api')>} */
const ready = (async () => {
  if (!/^[0-9A-Za-z][0-9A-Za-z.+-]{0,63}$/.test(version)) {
    throw { code: 'internal', message: `bad package version ${JSON.stringify(version)}` };
  }
  /** @type {typeof import('../api')} */
  const pkg = await import(`../../pkg/${version}/nuee_web.js`);
  await pkg.default();
  return pkg;
})();

/** @type {NueeNode | null} */
let node = null;
/** @type {Promise<NueeNode> | null} */
let starting = null;
let nextSession = 1;
/** @type {Map<number, { kind: 'host', s: HostSession } | { kind: 'join', s: JoinSession } | { kind: 'share', s: ShareSession }>} */
const sessions = new Map();
let nextExec = 1;
/** @type {Map<number, (o: TaskOutcome) => void>} */
const execs = new Map();

/** @param {Uint8Array} u */
function owned(u) {
  return u.byteOffset === 0 && u.byteLength === u.buffer.byteLength && u.buffer instanceof ArrayBuffer;
}

/** A copy that owns its buffer, so it can be transferred. @param {Uint8Array} u */
function own(u) {
  return owned(u) ? u : u.slice();
}

/** @param {FromWorker} msg @param {Transferable[]} [transfer] */
function post(msg, transfer = []) {
  scope.postMessage(msg, transfer);
}

/** @param {number | undefined} session */
function eventSink(session) {
  /** @param {NueeEvent} event */
  return (event) => {
    /** @type {Transferable[]} */
    const transfer = [];
    if (event.type === 'accepted') {
      event.output = own(event.output);
      transfer.push(event.output.buffer);
    } else if (event.type === 'lane') {
      event.frame = own(event.frame);
      transfer.push(event.frame.buffer);
    }
    post(session === undefined ? { event } : { session, event }, transfer);
  };
}

/** @param {TaskRequest} task @returns {Promise<TaskOutcome>} */
function executor(task) {
  return new Promise((resolve) => {
    const execId = nextExec;
    nextExec += 1;
    execs.set(execId, resolve);
    /** @type {Transferable[]} */
    const transfer = [];
    task.input = own(task.input);
    transfer.push(task.input.buffer);
    if (task.module) {
      task.module = own(task.module);
      transfer.push(task.module.buffer);
    }
    post({ execId, task }, transfer);
  });
}

function needNode() {
  if (!node) throw { code: 'invalid_argument', message: '`start` must succeed first' };
  return node;
}

/** @param {number} id */
function session(id) {
  const s = sessions.get(id);
  if (!s) throw { code: 'invalid_argument', message: `no session ${id}` };
  return s;
}

/**
 * @param {Op} op
 * @param {unknown} args checked per op below
 * @param {number} id the request id (also the progress id of downloads)
 * @returns {Promise<[unknown, Transferable[]]>}
 */
async function handle(op, args, id) {
  const pkg = await ready;
  switch (op) {
    case 'version':
      return [pkg.version(), []];
    case 'parseLink':
      return [pkg.parseLink(/** @type {OpMap['parseLink']['args']} */ (args).link), []];
    case 'fingerprint':
      return [pkg.fingerprint(/** @type {OpMap['fingerprint']['args']} */ (args).endpointId), []];
    case 'qrSvg':
      return [pkg.qrSvg(/** @type {OpMap['qrSvg']['args']} */ (args).text), []];
    case 'validateModule':
      return [pkg.validateModule(/** @type {OpMap['validateModule']['args']} */ (args).bytes), []];
    case 'start': {
      if (!starting) {
        starting = pkg.NueeNode.start(/** @type {OpMap['start']['args']} */ (args) ?? {});
        starting.then(
          (n) => {
            node = n;
          },
          () => {
            starting = null;
          },
        );
      }
      const n = await starting;
      return [{ id: n.id, fingerprint: n.fingerprint, relay: n.relay }, []];
    }
    case 'host': {
      const sid = nextSession;
      nextSession += 1;
      const s = await needNode().host(/** @type {OpMap['host']['args']} */ (args), executor, eventSink(sid));
      sessions.set(sid, { kind: 'host', s });
      return [{ session: sid, link: s.link, fingerprint: s.fingerprint, job: s.job }, []];
    }
    case 'join': {
      const sid = nextSession;
      nextSession += 1;
      const a = /** @type {OpMap['join']['args']} */ (args);
      const s = await needNode().join(a.link, a.opts ?? {}, executor, eventSink(sid));
      sessions.set(sid, { kind: 'join', s });
      return [{ session: sid }, []];
    }
    case 'share': {
      const sid = nextSession;
      nextSession += 1;
      const s = await needNode().share(/** @type {OpMap['share']['args']} */ (args).files, eventSink(sid));
      sessions.set(sid, { kind: 'share', s });
      return [{ session: sid, link: s.link }, []];
    }
    case 'approve': {
      const a = /** @type {OpMap['approve']['args']} */ (args);
      const s = session(a.session);
      if (s.kind !== 'host') throw { code: 'invalid_argument', message: 'not a host session' };
      s.s.approve(a.peer, a.accept);
      return [null, []];
    }
    case 'submit': {
      const a = /** @type {OpMap['submit']['args']} */ (args);
      const s = session(a.session);
      if (s.kind !== 'host') throw { code: 'invalid_argument', message: 'not a host session' };
      return [await s.s.submit(a.spec), []];
    }
    case 'status': {
      const a = /** @type {OpMap['status']['args']} */ (args);
      const s = session(a.session);
      if (s.kind !== 'host') throw { code: 'invalid_argument', message: 'not a host session' };
      return [await s.s.status(), []];
    }
    case 'setWebrtc': {
      const a = /** @type {OpMap['setWebrtc']['args']} */ (args);
      const s = session(a.session);
      if (s.kind !== 'host') throw { code: 'invalid_argument', message: 'not a host session' };
      s.s.setWebrtc(a.on);
      return [null, []];
    }
    case 'setSlots': {
      const a = /** @type {OpMap['setSlots']['args']} */ (args);
      const s = session(a.session);
      if (s.kind !== 'join') throw { code: 'invalid_argument', message: 'not a join session' };
      s.s.setSlots(a.n);
      return [null, []];
    }
    case 'sendRtc': {
      const a = /** @type {OpMap['sendRtc']['args']} */ (args);
      const s = session(a.session);
      if (s.kind === 'share') throw { code: 'invalid_argument', message: 'not a swarm session' };
      await s.s.sendRtc(a.peer, a.kind, a.payload);
      return [null, []];
    }
    case 'laneOpen': {
      const a = /** @type {OpMap['laneOpen']['args']} */ (args);
      const s = session(a.session);
      if (s.kind !== 'join') throw { code: 'invalid_argument', message: 'not a join session' };
      const sink = eventSink(a.session);
      s.s.laneOpen((/** @type {Uint8Array} */ frame) => sink({ type: 'lane', frame }));
      return [null, []];
    }
    case 'laneFrame': {
      const a = /** @type {OpMap['laneFrame']['args']} */ (args);
      const s = session(a.session);
      if (s.kind === 'join') {
        s.s.laneFrame(a.frame);
        return [null, []];
      }
      if (s.kind !== 'host' || typeof a.peer !== 'string') throw { code: 'invalid_argument', message: 'not a swarm session' };
      const ack = own(s.s.laneFrame(a.peer, a.frame));
      return [ack, [ack.buffer]];
    }
    case 'laneClosed': {
      const a = /** @type {OpMap['laneClosed']['args']} */ (args);
      const s = session(a.session);
      if (s.kind !== 'join') throw { code: 'invalid_argument', message: 'not a join session' };
      s.s.laneClosed();
      return [null, []];
    }
    case 'stop': {
      const a = /** @type {OpMap['stop']['args']} */ (args);
      const s = session(a.session);
      sessions.delete(a.session);
      if (s.kind === 'join') s.s.stop();
      else await s.s.stop();
      return [null, []];
    }
    case 'fetchFile':
    case 'fetchSite': {
      const a = /** @type {OpMap['fetchFile']['args']} */ (args);
      const n = needNode();
      const progress = typeof a.progress === 'number' ? a.progress : undefined;
      /** @type {((got: number, total: number) => void) | undefined} */
      const onProgress = progress === undefined ? undefined : (/** @type {number} */ got, /** @type {number} */ total) => post({ progress, got, total });
      const got = op === 'fetchFile' ? await n.fetchFile(a.link, onProgress) : await n.fetchSite(a.link, onProgress);
      const transfer = got.files.map((f) => {
        f.bytes = own(f.bytes);
        return f.bytes.buffer;
      });
      return [got, transfer];
    }
    case 'http': {
      const a = /** @type {OpMap['http']['args']} */ (args);
      const r = await needNode().http(a.link, a.req ?? {});
      r.body = own(r.body);
      return [r, [r.body.buffer]];
    }
    default:
      throw { code: 'invalid_argument', message: `unknown op ${String(op)} (request ${id})` };
  }
}

/** @param {unknown} e @returns {NueeError} */
function toError(e) {
  if (typeof e === 'object' && e !== null) {
    const o = /** @type {Record<string, unknown>} */ (e);
    const message = typeof o.message === 'string' ? o.message : String(e);
    if (typeof o.code === 'string') return /** @type {NueeError} */ ({ code: o.code, message });
    return { code: 'internal', message };
  }
  return { code: 'internal', message: String(e) };
}

scope.onmessage = (/** @type {MessageEvent} */ e) => {
  const m = e.data;
  if (typeof m !== 'object' || m === null) return;
  if (typeof m.execId === 'number' && m.outcome) {
    const resolve = execs.get(m.execId);
    if (resolve) {
      execs.delete(m.execId);
      resolve(m.outcome);
    }
    return;
  }
  if (typeof m.id !== 'number' || typeof m.op !== 'string') return;
  const id = m.id;
  handle(m.op, m.args, id).then(
    ([value, transfer]) => post(/** @type {FromWorker} */ ({ id, ok: true, value }), transfer),
    (err) => {
      const error = toError(err);
      if (error.code === 'internal') console.error(m.op, err);
      post({ id, ok: false, error });
    },
  );
};
