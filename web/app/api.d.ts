// The JavaScript contract of the web app. Frozen: later code fills in JavaScript against
// these types; changing a shape here is a breaking change for every page and worker.
//
// Threads
// - Main thread: the UI. It never loads the wasm package.
// - The "iroh worker" (one dedicated Web Worker): loads web/pkg/<version>/nuee_web.js, owns
//   the only NueeNode and every session. It talks to the main thread with the
//   postMessage protocol of section 2.
// - Task workers (a pool owned by the main thread): run leases, see section 3. The iroh
//   worker reaches them through the `Executor` callback, which the iroh worker implements
//   by posting `ExecCall` to the main thread.
//
// Conventions
// - 64-bit identifiers (lease ids) are decimal strings. Byte counts and memory sizes are
//   numbers (exact below 2^53).
// - Endpoint ids and hashes are 64 lowercase hex characters.
// - Bytes are Uint8Array; across postMessage their buffers are transferred, never copied.
// - Every failure is a `NueeError` ({code, message}); the UI chooses its text from `code`
//   only, `message` is English detail for logs and bug reports.
// - Nothing here is ever written to browser storage: links carry secrets.

// ---------------------------------------------------------------------------------------------
// 1. Exports of the wasm package (web/pkg/<version>/nuee_web.js, `wasm-pack --target web`).

/** Loads the module; `initSync({module: bytes})` is the synchronous variant. */
export default function init(input?: { module_or_path: RequestInfo | URL | BufferSource | WebAssembly.Module }): Promise<unknown>;
export function initSync(input: { module: BufferSource | WebAssembly.Module }): unknown;

/** Package version, the same as in web/pkg/current.json. */
export function version(): string;
/** What a pasted link points to. Accepts the full URL, the fragment, or `K/PAYLOAD`. Throws NueeError. */
export function parseLink(link: string): LinkInfo;
/** Six emoji derived from an endpoint id, to compare out of band. Throws `invalid_argument`. */
export function fingerprint(endpointId: string): string[];
/** QR code as SVG markup (starts with `<?xml` or `<svg`). Parse it with DOMParser, never innerHTML. */
export function qrSvg(text: string): string;
/** Checks a task module (zero imports, task ABI, declared max memory <= 1 GiB). Throws `invalid_module` or `too_large`. */
export function validateModule(bytes: Uint8Array): ModuleInfo;
/** A fresh random secret and endpoint id; used by tests to prove the random source works. */
export function newSecret(): { secret: string; id: string };

export class NueeNode {
  /**
   * Binds an endpoint (relay only, `Minimal` preset, the relay of `opts` or the n0 relays)
   * and waits until the relay accepted it, 10 s at most (`relay_unreachable`).
   */
  static start(opts?: NodeOptions): Promise<NueeNode>;
  /** Endpoint id of this node (used to join swarms and download). */
  readonly id: string;
  readonly fingerprint: string[];
  /** Home relay URL, empty when relays are disabled. */
  readonly relay: string;
  /** Hosts a swarm and submits its first job. The swarm gets an endpoint of its own. */
  host(spec: HostSpec, exec: Executor, onEvent: (e: NueeEvent) => void): Promise<HostSession>;
  /** Joins the swarm of a `J` link; resolves once welcomed (after `pending` when approval is manual). */
  join(link: string, opts: JoinOptions, exec: Executor, onEvent: (e: NueeEvent) => void): Promise<JoinSession>;
  /** Encrypts and serves files; the share gets an endpoint of its own. */
  share(files: NamedBytes[], onEvent: (e: NueeEvent) => void): Promise<ShareSession>;
  /** Downloads, verifies (BLAKE3) and decrypts the files of an `F` link. */
  fetchFile(link: string, onProgress?: (got: number, total: number) => void): Promise<FetchedFiles>;
  /** Same for an `S` link (static site preview). */
  fetchSite(link: string, onProgress?: (got: number, total: number) => void): Promise<FetchedFiles>;
  /** One console request to the host of an `X` link over `nuee/http/0`. */
  http(link: string, req: HttpReq): Promise<HttpResp>;
  /** Stops serving and closes the endpoint. Sessions started from this node stop working. */
  close(): Promise<void>;
}

export class HostSession {
  /** The `J` link to hand out. It carries the admission secret: show it, never store it. */
  readonly link: string;
  /** Fingerprint of the swarm's coordinator id; workers see the same six emoji. */
  readonly fingerprint: string[];
  /** Number of the job submitted by `host`. */
  readonly job: number;
  /** Answers a `peerPending` event. */
  approve(peer: string, accept: boolean): void;
  /** Adds a job (same shape as HostSpec; the swarm settings in it are ignored). Resolves to its number. */
  submit(spec: JobSpec): Promise<number>;
  /** Drops a finished job, freeing its slot among the 64 a swarm holds; false when unknown or unfinished. */
  forget(job: number): Promise<boolean>;
  status(): Promise<SwarmStatus>;
  /** Relays WebRTC signalling between peers (S-029). */
  setWebrtc(on: boolean): void;
  /** Sends signalling from the coordinator to a connected peer. */
  sendRtc(peer: string, kind: RtcKind, payload: string): Promise<void>;
  /** A frame `peer` sent on its WebRTC lane; returns the ack to send back on that lane. */
  laneFrame(peer: string, frame: Uint8Array): Uint8Array;
  /** Says goodbye to every peer and closes the swarm's endpoint. */
  stop(): Promise<void>;
}

export class JoinSession {
  /** Concurrent leases; 0 pauses. */
  setSlots(n: number): void;
  /** Sends signalling to another peer through the coordinator. */
  sendRtc(peer: string, kind: RtcKind, payload: string): Promise<void>;
  /** The data channel to the coordinator opened: results are handed to `onFrame` from now on. */
  laneOpen(onFrame: (frame: Uint8Array) => void): void;
  /** A frame (ack) the coordinator sent on the lane. */
  laneFrame(frame: Uint8Array): void;
  /** The data channel closed: unacknowledged results are resent through iroh. */
  laneClosed(): void;
  stop(): void;
}

export class ShareSession {
  /** The `F` link. It carries the access secret and the decryption key. */
  readonly link: string;
  stop(): Promise<void>;
}

export interface NodeOptions {
  /** `'n0'` (default), `'disabled'`, or a relay URL (e2e: `http://127.0.0.1:<port>`). */
  relay?: string;
  /** Base of produced links; default `https://maxgfr.github.io/nuee/`. */
  linkBase?: string;
}

export interface LinkInfo {
  kind: 'job' | 'file' | 'service' | 'site';
  /** Coordinator (job), host (service) or first provider (file, site). */
  peer: string;
  fingerprint: string[];
  relay: string;
  /** Ciphertext hash and size (file, site). */
  hash?: string;
  size?: number;
  /** Every provider (file, site); empty otherwise. */
  providers: string[];
}

export interface ModuleInfo {
  hash: string;
  size: number;
  minMemory: number;
  maxMemory: number;
}

export type TaskKind = 'wasm' | 'wgsl' | 'embed' | 'llm' | 'trainWasm';
export type FailKind = 'trap' | 'oom' | 'timeout' | 'stack_overflow' | 'invalid' | 'unsupported' | 'cancelled' | 'io';
export type ByeReason = 'done' | 'banned' | 'shutdown' | 'rejected' | 'full';
export type RtcKind = 'offer' | 'answer' | 'ice';

export interface Compare {
  kind: 'exact' | 'tolerance' | 'cosine' | 'logits';
  abs?: number;
  rel?: number;
  /** Elements allowed outside tolerance, per thousand. */
  maxBad?: number;
  minCos?: number;
  maxGap?: number;
}

/**
 * A DiLoCo run hosted by this page (`host()` only, with `kind: 'trainWasm'`; `inputs` is
 * ignored). Every round is one job of `workers` tasks built by nuee-train; the page gets
 * `trainRound` after each round and `trainDone` with the final parameters.
 */
export interface TrainSpecJs {
  rounds: number;
  innerSteps: number;
  workers: number;
  outerLr: number;
  outerMomentum: number;
  /** Initial parameters, safetensors (F32 tensors only). */
  init: Uint8Array;
  /** Inner learning rate handed to the trainer; 0 or absent = the trainer's default. */
  innerLr?: number;
  /** Default 'f32'. */
  codec?: 'int8ef' | 'f32';
  trim?: number;
  cosMin?: number;
  normMaxRatio?: number;
  /** The next round is submitted no sooner than this after the previous one (0..=60000, default 0). */
  minRoundMs?: number;
}

export interface JobSpec {
  name: string;
  kind: TaskKind;
  /** Required for `wasm` (validated, its declared max memory is used); shader or model id for other kinds. */
  module?: Uint8Array;
  /** Prepended to every task input as the asset envelope, in this order. */
  assets?: Uint8Array[];
  /** One input per task; inputs above 1 MiB are served as blobs. */
  inputs: Uint8Array[];
  verify?: 'full' | 'sample' | 'none';
  /** Recomputed share, per thousand (default 100). */
  sample?: number;
  /** Canary share, per thousand (default 30). */
  canary?: number;
  /** Default exact. `tolerance` reads wgsl outputs as u32 and other kinds as f32; cosine and logits are refused with `unsupported` until their steps provide them. */
  compare?: Compare;
  /** Default 60000. */
  deadlineMs?: number;
  /** Memory granted to non-wasm kinds (default 256 MiB). */
  maxMemory?: number;
  train?: TrainSpecJs;
}

export interface HostSpec extends JobSpec {
  /** Workers wait in `peerPending` until `approve`. */
  approve?: boolean;
  /** Tasks this page runs itself through `exec` at once (0: none). */
  localSlots: number;
  /**
   * Recomputations this page runs through `exec` to check peers (samples, mismatches, one
   * canary answer), even with `localSlots: 0` (default 1; 0 leaves only agreement between two
   * peers, which one holder of the link can fake).
   */
  verifySlots?: number;
  webrtc?: boolean;
  /** At most 4 STUN URLs announced to workers. */
  stun?: string[];
}

export interface JoinOptions {
  /** Concurrent leases (default 1). */
  slots?: number;
  /** Memory offered, in MiB (default 1024); also caps each lease locally. */
  memMb?: number;
  /** Kinds this page can run (default ['wasm']). */
  kinds?: TaskKind[];
  gpu?: { adapter: string; maxBuffer: number; maxStorageBinding: number; f16: boolean };
  /** Can open WebRTC data channels. */
  rtc?: boolean;
}

export interface NamedBytes {
  /** Relative path inside the share (no `..`, no leading `/`). */
  name: string;
  bytes: Uint8Array;
}

export interface FetchedFiles {
  files: NamedBytes[];
  /** BLAKE3 of the ciphertext, as in the link. */
  hash: string;
  /** Always true: unverified bytes are never returned (`integrity` instead). */
  verified: true;
}

export interface HttpReq {
  /** Default 'GET'. */
  method?: string;
  /** Path and query, default '/'. */
  path?: string;
  /** At most 64 headers. */
  headers?: [string, string][];
  /** At most 8 MiB. */
  body?: Uint8Array;
}

export interface HttpResp {
  status: number;
  headers: [string, string][];
  body: Uint8Array;
}

export interface PeerStatus {
  peer: string;
  fingerprint: string[];
  browser: boolean;
  slots: number;
  inflight: number;
  done: number;
  verified: number;
  banned: boolean;
}

export interface JobStatus {
  job: number;
  name: string;
  total: number;
  done: number;
  verified: number;
  failed: number;
  inFlight: number;
  /** Accepted tasks per second since submission. */
  rate: number;
}

export interface SwarmStatus {
  peers: PeerStatus[];
  jobs: JobStatus[];
}

/** One lease handed to the executor. */
export interface TaskRequest {
  /** u64, decimal. */
  lease: string;
  job: number;
  task: number;
  kind: TaskKind;
  /** BLAKE3 of the module, when the kind uses one. */
  moduleHash?: string;
  /** Present the first time this executor sees `moduleHash`; afterwards only the hash. */
  module?: Uint8Array;
  /** Task input, the asset envelope already applied. */
  input: Uint8Array;
  deadlineMs: number;
  maxMemory: number;
}

export type TaskOutcome =
  | { ok: true; output: Uint8Array }
  | { ok: false; kind: FailKind; detail: string };

/** Runs one lease. A rejected promise counts as `{ok: false, kind: 'io'}`. */
export type Executor = (t: TaskRequest) => Promise<TaskOutcome>;

/** Everything pushed to `onEvent`, discriminated by `type`. */
export type NueeEvent =
  // host and share: the session's own endpoint reached its relay
  | { type: 'online'; relay: string }
  // host
  | { type: 'peerJoined'; peer: string; fingerprint: string[]; browser: boolean; slots: number }
  | { type: 'peerPending'; peer: string; fingerprint: string[]; browser: boolean }
  | { type: 'peerLeft'; peer: string }
  // `ms`: lease to result, measured by this page (round trip included, so a worker cannot
  // shorten it); null for an output restored from an earlier run.
  | { type: 'accepted'; job: number; task: number; by: string; hash: string; ms: number | null; output: Uint8Array }
  | { type: 'verified'; job: number; task: number; by: string }
  | { type: 'mismatch'; job: number; task: number; peer: string }
  | { type: 'banned'; peer: string; redo: number }
  | { type: 'taskFailed'; job: number; task: number; kind: FailKind }
  // this page cannot check `kind` and no other admitted peer able to run it is left for the
  // second execution these tasks need (`task` is the first of `waiting`). Sent once per job
  // until the peers able to run the kind change; the tasks resume when such a peer joins.
  | { type: 'waitingForChecker'; job: number; task: number; waiting: number; kind: TaskKind }
  | { type: 'progress'; job: number; done: number; total: number; verified: number; rate: number }
  | { type: 'jobDone'; job: number; failed: number }
  // host of a training run: `round` rounds of `rounds` are done; `loss` is the mean inner loss
  | { type: 'trainRound'; round: number; rounds: number; loss: number | null; accepted: number; rejected: number; updateNorm: number }
  | { type: 'trainDone'; rounds: number; params: Uint8Array }
  // join
  | { type: 'pending' }
  | { type: 'welcome'; swarm: { name: string; coordinator: string; fingerprint: string[]; webrtc: boolean; stun: string[] } }
  | { type: 'jobAdded'; job: number; name: string }
  | { type: 'jobRejected'; reason: string }
  | { type: 'leaseRefused'; lease: string; kind: FailKind }
  // host (local slots) and join
  | { type: 'taskStarted'; lease: string }
  | { type: 'taskDone'; lease: string; ms: number; ok: boolean }
  | { type: 'rtc'; peer: string; kind: RtcKind; payload: string }
  // join, after `laneOpen`: a frame to write on the data channel
  | { type: 'lane'; frame: Uint8Array }
  | { type: 'ended'; reason: ByeReason }
  | { type: 'error'; code: ErrorCode; message: string };

// ---------------------------------------------------------------------------------------------
// 2. postMessage protocol, main thread <-> iroh worker.
//
// Requests carry a fresh `id`; exactly one reply with the same id follows. Callbacks of the
// Rust API become messages: `onEvent` -> `EventPush` (with the session id when the event
// belongs to a session), `onProgress` -> `ProgressPush`, `Executor` -> `ExecCall`/`ExecReply`.
// `start` must succeed before any other op except the pure ones (parseLink, fingerprint,
// qrSvg, validateModule, version).

export type Op =
  | 'start' | 'host' | 'join' | 'approve' | 'submit' | 'forget' | 'setSlots' | 'setWebrtc' | 'sendRtc'
  | 'stop' | 'share' | 'fetchFile' | 'fetchSite' | 'http' | 'status' | 'parseLink'
  | 'fingerprint' | 'qrSvg' | 'validateModule' | 'version' | 'laneOpen' | 'laneFrame' | 'laneClosed';

/** Arguments and resolved value of each op. `session` is the id returned by host/join/share. */
export interface OpMap {
  start: { args: NodeOptions; value: { id: string; fingerprint: string[]; relay: string } };
  host: { args: HostSpec; value: { session: number; link: string; fingerprint: string[]; job: number } };
  join: { args: { link: string; opts: JoinOptions }; value: { session: number } };
  approve: { args: { session: number; peer: string; accept: boolean }; value: null };
  submit: { args: { session: number; spec: JobSpec }; value: number };
  forget: { args: { session: number; job: number }; value: boolean };
  setSlots: { args: { session: number; n: number }; value: null };
  setWebrtc: { args: { session: number; on: boolean }; value: null };
  sendRtc: { args: { session: number; peer: string; kind: RtcKind; payload: string }; value: null };
  stop: { args: { session: number }; value: null };
  /** Join session: its data channel opened; frames to send arrive as `lane` events. */
  laneOpen: { args: { session: number }; value: null };
  /** A frame received on a data channel: host sessions name the `peer` and get the ack to send back. */
  laneFrame: { args: { session: number; peer?: string; frame: Uint8Array }; value: Uint8Array | null };
  /** Join session: its data channel closed. */
  laneClosed: { args: { session: number }; value: null };
  share: { args: { files: NamedBytes[] }; value: { session: number; link: string } };
  fetchFile: { args: { link: string; progress?: number }; value: FetchedFiles };
  fetchSite: { args: { link: string; progress?: number }; value: FetchedFiles };
  http: { args: { link: string; req: HttpReq }; value: HttpResp };
  status: { args: { session: number }; value: SwarmStatus };
  parseLink: { args: { link: string }; value: LinkInfo };
  fingerprint: { args: { endpointId: string }; value: string[] };
  qrSvg: { args: { text: string }; value: string };
  validateModule: { args: { bytes: Uint8Array }; value: ModuleInfo };
  version: { args: null; value: string };
}

export interface Request<K extends Op = Op> {
  id: number;
  op: K;
  args: OpMap[K]['args'];
}

export type Reply<K extends Op = Op> =
  | { id: number; ok: true; value: OpMap[K]['value'] }
  | { id: number; ok: false; error: NueeError };

/** Worker -> main: an event of a session (absent for node-level events). */
export interface EventPush {
  session?: number;
  event: NueeEvent;
}

/** Worker -> main: download progress of request `progress` (the id given in fetchFile/fetchSite args). */
export interface ProgressPush {
  progress: number;
  got: number;
  total: number;
}

/** Worker -> main: run a lease (from a host's local slots or a joined swarm). */
export interface ExecCall {
  execId: number;
  task: TaskRequest;
}

/** Main -> worker: the outcome of `ExecCall` `execId`. */
export interface ExecReply {
  execId: number;
  outcome: TaskOutcome;
}

export type ToWorker = Request | ExecReply;
export type FromWorker = Reply | EventPush | ProgressPush | ExecCall;

// ---------------------------------------------------------------------------------------------
// 3. Task worker protocol, main thread <-> task worker (web/app/worker/task.js).
//
// One fresh WebAssembly instance per lease; compiled modules are cached by `moduleHash`
// (the main thread keeps the bytes it received first and sends them to any task worker that
// lacks them). The pool enforces `deadlineMs` with `worker.terminate()` and answers
// `{ok: false, kind: 'timeout'}`, then replaces the worker. Pool size follows
// hardwareConcurrency and the module's declared max memory. Kind-specific executors
// (`gpu.js` for wgsl, `embed.js`, `llm.js`) take the same request and answer the same
// outcome.

export interface TaskRun {
  op: 'run';
  lease: string;
  kind: TaskKind;
  moduleHash?: string;
  module?: Uint8Array;
  input: Uint8Array;
  deadlineMs: number;
  maxMemory: number;
}

/** Task worker -> main: the answer to the only `TaskRun` in flight on that worker. */
export type TaskRunReply = TaskOutcome;

// ---------------------------------------------------------------------------------------------
// 4. Errors.

export interface NueeError {
  code: ErrorCode;
  message: string;
}

/**
 * What the UI says for each code (one line, translated in the UI):
 * - relay_unreachable: "Cannot reach the relay. Check your connection and try again."
 * - swarm_ended: "This swarm or share is gone (or its host is offline)."
 * - link_truncated: "This link is incomplete. Copy it again in full."
 * - link_version: "This link comes from a newer Nuée. Reload the page."
 * - link_corrupt: "This link is damaged."
 * - link_kind: "This link is not meant for this page."
 * - admission_refused: "The host refused this device."
 * - integrity: "The received data does not match the link; nothing was kept."
 * - decrypt: "The data cannot be decrypted with this link."
 * - too_large: "Too large for this browser."
 * - unsupported: "This browser cannot do this yet."
 * - invalid_module: "This file is not a valid Nuée task module."
 * - invalid_argument: "Something went wrong on this page." (a bug: log `message`)
 * - internal: "Unexpected error." (log `message`)
 */
export type ErrorCode =
  | 'relay_unreachable'
  | 'swarm_ended'
  | 'link_truncated'
  | 'link_version'
  | 'link_corrupt'
  | 'link_kind'
  | 'admission_refused'
  | 'integrity'
  | 'decrypt'
  | 'too_large'
  | 'unsupported'
  | 'invalid_module'
  | 'invalid_argument'
  | 'internal';
