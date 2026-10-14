// Fetches the vendored Transformers.js / ONNX Runtime files listed in VENDOR.json into this
// directory. Each npm tarball is checked against its recorded integrity (sha512), then each
// extracted file against its recorded SHA-256. Files already present with the right hash are
// kept, so a second run is offline.
// Usage: node web/vendor/transformers/fetch.mjs
import { createHash } from 'node:crypto';
import { readFile, writeFile, rename } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { gunzipSync } from 'node:zlib';

const DIR = dirname(fileURLToPath(import.meta.url));
const vendor = JSON.parse(await readFile(join(DIR, 'VENDOR.json'), 'utf8'));

const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');

async function present(file) {
  try {
    const bytes = await readFile(join(DIR, file.to));
    return bytes.length === file.size && sha256(bytes) === file.sha256;
  } catch (err) {
    if (err.code === 'ENOENT') return false;
    throw err;
  }
}

// Minimal ustar reader: returns the regular files whose names are wanted.
function untar(tar, wanted) {
  const out = new Map();
  let off = 0;
  while (off + 512 <= tar.length) {
    const header = tar.subarray(off, off + 512);
    if (header.every((b) => b === 0)) break;
    const field = (start, len) => {
      const raw = header.subarray(start, start + len);
      const end = raw.indexOf(0);
      return raw.subarray(0, end < 0 ? len : end).toString('utf8');
    };
    const prefix = field(345, 155);
    const name = prefix ? `${prefix}/${field(0, 100)}` : field(0, 100);
    const size = parseInt(field(124, 12).trim() || '0', 8);
    const type = field(156, 1) || '0';
    const start = off + 512;
    if (!Number.isFinite(size) || start + size > tar.length) throw new Error(`corrupt tar entry ${name}`);
    if (type === '0' && wanted.has(name)) out.set(name, tar.subarray(start, start + size));
    off = start + Math.ceil(size / 512) * 512;
  }
  return out;
}

for (const pkg of vendor.packages) {
  const missing = [];
  for (const file of pkg.files) if (!(await present(file))) missing.push(file);
  if (missing.length === 0) continue;

  const res = await fetch(pkg.tarball, { redirect: 'error' });
  if (!res.ok) throw new Error(`${pkg.tarball}: HTTP ${res.status}`);
  const tgz = Buffer.from(await res.arrayBuffer());
  const [algo, expected] = pkg.integrity.split('-');
  const got = createHash(algo).update(tgz).digest('base64');
  if (got !== expected) throw new Error(`${pkg.name}@${pkg.version}: integrity mismatch`);

  const files = untar(gunzipSync(tgz), new Set(missing.map((f) => f.from)));
  for (const file of missing) {
    const bytes = files.get(file.from);
    if (!bytes) throw new Error(`${pkg.name}: ${file.from} not in the tarball`);
    if (bytes.length !== file.size || sha256(bytes) !== file.sha256) {
      throw new Error(`${pkg.name}: ${file.from} does not match VENDOR.json`);
    }
    const dest = join(DIR, file.to);
    await writeFile(`${dest}.part`, bytes);
    await rename(`${dest}.part`, dest);
  }
  console.log(`vendor: ${pkg.name}@${pkg.version} (${missing.length} files)`);
}
