// @ts-check
// Words shown to people for values that arrive as identifiers: job names, refusal reasons,
// how a contribution ended, and what is wrong with a picked text file.

/** Job names the app and the CLI give their built-in jobs, and their i18n keys. */
const JOB_KEYS = /** @type {Record<string, string>} */ ({
  mandelbrot: 'run.job.mandelbrot',
  'mandelbrot-gpu': 'run.job.mandelbrotGpu',
  skycmd: 'run.job.skycmd',
  'skycmd-eval': 'run.job.skycmd',
  'skycmd-dagger': 'run.job.skycmdDagger',
  embed: 'run.job.embed',
});

/**
 * The i18n key naming a built-in job, or null for a name the presenter chose (shown as is).
 * @param {string} name
 * @returns {string | null}
 */
export function jobKey(name) {
  return Object.prototype.hasOwnProperty.call(JOB_KEYS, name) ? (JOB_KEYS[name] ?? null) : null;
}

/**
 * Why a job was skipped, as an i18n key. The worker reports "too many jobs" when it already
 * follows the most jobs it can; anything else is a manifest it could not open or trust.
 * @param {string} reason
 */
export function rejectKey(reason) {
  return reason === 'too many jobs' ? 'join.jobRejected.tooMany' : 'join.jobRejected.invalid';
}

/**
 * How the end of a contribution is written: an alert only when the host refused or banned
 * this device; a notice when the swarm ended or was full; plain text when the person stopped
 * or the job finished.
 * @param {string} key one of the join.ended.* keys
 * @returns {'alert' | 'notice' | 'plain'}
 */
export function endedTone(key) {
  if (key === 'join.ended.banned' || key === 'join.ended.rejected') return 'alert';
  if (key === 'join.ended.full' || key === 'join.ended.shutdown') return 'notice';
  return 'plain';
}

/**
 * Reads a picked text file. Returns its text, or the i18n key of what is wrong with it: not
 * UTF-8 text, or nothing in it.
 * @param {ArrayBuffer | Uint8Array} bytes
 * @returns {{ text: string } | { error: string }}
 */
export function readTextFile(bytes) {
  let text;
  try {
    text = new TextDecoder('utf-8', { fatal: true }).decode(bytes);
  } catch (e) {
    if (e instanceof TypeError) return { error: 'run.embed.notText' };
    throw e;
  }
  if (text.trim() === '') return { error: 'run.embed.empty' };
  return { text };
}
