// @ts-check
// `node --test web/test/` hands the directory to Node as one entry point, which resolves to
// this file; it loads every *.test.mjs next to it so each test registers with node:test.
const { readdirSync } = require('node:fs');
const { pathToFileURL } = require('node:url');
const path = require('node:path');

const files = readdirSync(__dirname)
  .filter((n) => n.endsWith('.test.mjs'))
  .sort();
for (const f of files) {
  import(pathToFileURL(path.join(__dirname, f)).href).catch((e) => {
    console.error(`cannot load ${f}`, e);
    process.exitCode = 1;
  });
}
