// @ts-check
// Number, size and identifier formatting, in English.

const UNITS = ['B', 'KB', 'MB', 'GB', 'TB'];

/** @param {number} n */
export function fmtNumber(n, digits = 0) {
  return new Intl.NumberFormat('en', { maximumFractionDigits: digits }).format(n);
}

/** Decimal byte sizes (1 MB = 10^6 bytes). @param {number} n */
export function fmtBytes(n) {
  let i = 0;
  let v = n;
  while (v >= 1000 && i < UNITS.length - 1) {
    v /= 1000;
    i += 1;
  }
  return `${fmtNumber(v, i === 0 ? 0 : 1)} ${UNITS[i]}`;
}

/** First 12 hex characters, enough to compare by eye. @param {string} hex */
export function shortHash(hex) {
  return hex.slice(0, 12);
}

/** @param {string[]} emoji */
export function fmtFingerprint(emoji) {
  return emoji.join(' ');
}

/** A stable hue (0..359) for a peer id, used to color its birds and tiles. @param {string} id */
export function hueOf(id) {
  let x = 0;
  for (let i = 0; i < id.length; i += 1) x = (x * 31 + id.charCodeAt(i)) % 360;
  return x;
}
