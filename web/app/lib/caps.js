// @ts-check
// Feature detection. Every helper takes the global object so tests can stub it.

/** A function using i8x16.splat and i8x16.popcnt: validates only where SIMD is supported. */
export const SIMD_PROBE = Uint8Array.of(
  0, 97, 115, 109, 1, 0, 0, 0, 1, 5, 1, 96, 0, 1, 123, 3, 2, 1, 0, 10, 10, 1, 8, 0, 65, 0, 253, 15, 253, 98, 11,
);

/**
 * @typedef {{
 *   wasm: boolean,
 *   simd: boolean,
 *   webgpu: boolean,
 *   wakeLock: boolean,
 *   battery: boolean,
 *   deviceMemoryGb: number | undefined,
 *   cores: number,
 *   mobile: boolean,
 *   framed: boolean,
 * }} Caps
 */

/**
 * The globals the detection reads (window, or a stub in tests).
 * @typedef {{
 *   hardwareConcurrency?: number,
 *   deviceMemory?: number,
 *   gpu?: object | null,
 *   wakeLock?: { request?: unknown } | null,
 *   getBattery?: unknown,
 *   userAgent?: string,
 *   maxTouchPoints?: number,
 *   userAgentData?: { mobile?: unknown },
 * }} NavigatorLike
 * @typedef {{
 *   navigator?: NavigatorLike,
 *   WebAssembly?: { validate?: (b: Uint8Array) => boolean } | null,
 *   top?: unknown,
 *   self?: unknown,
 * }} GlobalLike
 */

/**
 * @param {GlobalLike} g
 * @returns {Caps}
 */
export function detect(g) {
  const nav = g.navigator ?? {};
  const validate = typeof g.WebAssembly === 'object' && g.WebAssembly !== null ? g.WebAssembly.validate : undefined;
  const wasm = typeof validate === 'function';
  let simd = false;
  if (validate) {
    try {
      simd = validate.call(g.WebAssembly, SIMD_PROBE) === true;
    } catch (e) {
      console.warn('SIMD probe failed', e);
      simd = false;
    }
  }
  const dm = typeof nav.deviceMemory === 'number' && nav.deviceMemory > 0 ? nav.deviceMemory : undefined;
  const hc = typeof nav.hardwareConcurrency === 'number' && nav.hardwareConcurrency > 0 ? Math.floor(nav.hardwareConcurrency) : 2;
  return {
    wasm,
    simd,
    webgpu: typeof nav.gpu === 'object' && nav.gpu !== null,
    wakeLock: typeof nav.wakeLock === 'object' && nav.wakeLock !== null && typeof nav.wakeLock.request === 'function',
    battery: typeof nav.getBattery === 'function',
    deviceMemoryGb: dm,
    cores: hc,
    mobile: isMobile(nav),
    framed: isFramed(g),
  };
}

/** @param {NavigatorLike} nav */
export function isMobile(nav) {
  if (nav.userAgentData && typeof nav.userAgentData.mobile === 'boolean') return nav.userAgentData.mobile;
  const ua = typeof nav.userAgent === 'string' ? nav.userAgent : '';
  if (/Mobi|Android|iPhone|iPad|iPod/.test(ua)) return true;
  // iPadOS reports itself as a Mac; touch points give it away.
  return /Macintosh/.test(ua) && typeof nav.maxTouchPoints === 'number' && nav.maxTouchPoints > 1;
}

/** @param {GlobalLike} g */
export function isFramed(g) {
  return g.top !== undefined && g.top !== g.self;
}

/**
 * The blocking problem with this browser, as an i18n key, or null.
 * @param {Caps} c
 */
export function wasmProblem(c) {
  if (!c.wasm) return 'boot.noWasm';
  if (!c.simd) return 'boot.noSimd';
  return null;
}
