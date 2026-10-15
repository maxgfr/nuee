// @ts-check
// Small reducers behind the presenter's live board: running times per device and the
// "waiting for a checker" notice.
import { fmtNumber } from './format.js';
import { t } from '../i18n.js';

/**
 * A duration as the rest of the UI writes it: "420 ms", "1.8 s", "2.5 min".
 * @param {number} ms
 */
export function fmtDuration(ms) {
  if (ms < 1000) return `${fmtNumber(Math.round(ms))} ms`;
  if (ms < 60_000) return `${fmtNumber(ms / 1000, 1)} s`;
  return `${fmtNumber(ms / 60_000, 1)} min`;
}

/** Mean lease-to-result time of each device, from its accepted tasks only. */
export class Timings {
  constructor() {
    /** @type {Map<string, { sum: number, n: number }>} */
    this.per = new Map();
  }

  /**
   * One accepted task; `ms` is null for an output restored from an earlier run (not timed).
   * @param {string} peer
   * @param {number | null | undefined} ms
   */
  add(peer, ms) {
    if (typeof ms !== 'number' || !Number.isFinite(ms) || ms < 0) return;
    const s = this.per.get(peer) ?? { sum: 0, n: 0 };
    s.sum += ms;
    s.n += 1;
    this.per.set(peer, s);
  }

  /** @param {string} peer @returns {number | null} */
  mean(peer) {
    const s = this.per.get(peer);
    return s && s.n > 0 ? s.sum / s.n : null;
  }

  /** The mean as text, or "—" before the device has a timed accepted task. @param {string} peer */
  text(peer) {
    const m = this.mean(peer);
    return m === null ? '—' : fmtDuration(m);
  }
}

/**
 * Tasks waiting for a second device able to run their kind, per job. `waitingForChecker`
 * sets a job's count; a check of one of its tasks (verified or mismatch) or the job's end
 * means the tasks proceed again and clears it.
 * @param {Map<number, number>} state
 * @param {{ type: string, job?: number, waiting?: number }} e
 * @returns {boolean} whether the notice changed
 */
export function reduceWaiting(state, e) {
  if (typeof e.job !== 'number') return false;
  if (e.type === 'waitingForChecker') {
    const n = typeof e.waiting === 'number' && e.waiting > 0 ? e.waiting : 1;
    if (state.get(e.job) === n) return false;
    state.set(e.job, n);
    return true;
  }
  if (e.type === 'verified' || e.type === 'mismatch' || e.type === 'jobDone') return state.delete(e.job);
  return false;
}

/** Tasks waiting over every job. @param {Map<number, number>} state */
export function waitingTotal(state) {
  let n = 0;
  for (const v of state.values()) n += v;
  return n;
}

/**
 * The detail line under the task grid: "Task 63 · 🐳 🌻 🎲 · 68 ms". `who` is left out when no
 * device has answered yet; the time is "—" when the task has no measured time.
 * @param {number} task
 * @param {string | null} who
 * @param {number | null} ms
 */
export function taskDetail(task, who, ms) {
  const time = typeof ms === 'number' && Number.isFinite(ms) && ms >= 0 ? fmtDuration(ms) : '—';
  const parts = [t('run.detail.task', { n: task })];
  if (who) parts.push(who);
  parts.push(time);
  return parts.join(' · ');
}

/** `#rrggbb` to RGB; anything else gives `fallback`. @param {string} hex @param {[number, number, number]} fallback @returns {[number, number, number]} */
export function rgbOf(hex, fallback) {
  const m = /^#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/i.exec(hex.trim());
  return m ? [parseInt(m[1] ?? '0', 16), parseInt(m[2] ?? '0', 16), parseInt(m[3] ?? '0', 16)] : fallback;
}

/**
 * The RGBA pixels of one fractal tile in a device's ink on the page's paper: iteration counts
 * (u16 from CPU tiles, u32 from GPU tiles) shade from the paper to the ink, a point that never
 * escapes in full ink. Given `ranks` (sorted escape counts of the previous image), a count
 * shades by its rank among them, so a deep image, whose counts crowd together, keeps its
 * contrast. Null when the bytes are too short for the tile.
 * @param {Uint8Array} bytes
 * @param {number} tile
 * @param {number} iter
 * @param {[number, number, number]} paper
 * @param {[number, number, number]} ink
 * @param {ArrayLike<number>} [ranks]
 * @returns {Uint8ClampedArray | null}
 */
export function tilePixels(bytes, tile, iter, paper, ink, ranks) {
  const px = tile * tile;
  if (bytes.byteLength < px * 2) return null;
  const wide = bytes.byteLength >= px * 4;
  const v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const out = new Uint8ClampedArray(px * 4);
  const n = ranks ? ranks.length : 0;
  for (let i = 0; i < px; i += 1) {
    const it = wide ? v.getUint32(i * 4, true) : v.getUint16(i * 2, true);
    let f = 1;
    if (it < iter && n > 0 && ranks) {
      let a = 0;
      let b = n;
      while (a < b) {
        const mid = (a + b) >> 1;
        if (ranks[mid] < it) a = mid + 1;
        else b = mid;
      }
      f = 0.08 + 0.8 * (a / n);
    } else if (it < iter) f = 0.08 + 0.8 * Math.sqrt(it / iter);
    out[i * 4] = Math.round(paper[0] + (ink[0] - paper[0]) * f);
    out[i * 4 + 1] = Math.round(paper[1] + (ink[1] - paper[1]) * f);
    out[i * 4 + 2] = Math.round(paper[2] + (ink[2] - paper[2]) * f);
    out[i * 4 + 3] = 255;
  }
  return out;
}

/**
 * Every `every`-th count of a tile's points that escape (below `iter`), appended to `into`.
 * @param {Uint8Array} bytes
 * @param {number} tile
 * @param {number} iter
 * @param {number} every
 * @param {number[]} into
 */
export function escapeSample(bytes, tile, iter, every, into) {
  const px = tile * tile;
  if (bytes.byteLength < px * 2) return;
  const wide = bytes.byteLength >= px * 4;
  const v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  for (let i = 0; i < px; i += every) {
    const it = wide ? v.getUint32(i * 4, true) : v.getUint16(i * 2, true);
    if (it < iter) into.push(it);
  }
}
