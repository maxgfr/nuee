// Bundles and minifies the app into dist/, with a note of where and from what it was built, so
// that whoever gets dist/ back from a group can see which machine and commit made it.
import { execFileSync } from 'node:child_process';
import { copyFileSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import { build } from 'esbuild';

const commit = (() => {
  try {
    return execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
  } catch {
    return 'unknown';
  }
})();

const info = {
  commit,
  builtAt: new Date().toISOString(),
  node: process.version,
  platform: `${os.platform()}-${os.arch()}`,
};

rmSync('dist', { recursive: true, force: true });
mkdirSync('dist');
await build({
  entryPoints: ['src/main.js'],
  bundle: true,
  minify: true,
  format: 'esm',
  target: 'es2022',
  outfile: 'dist/app.js',
  define: { BUILD_INFO: JSON.stringify(info) },
  logLevel: 'info',
});
copyFileSync('src/index.html', 'dist/index.html');
copyFileSync('src/style.css', 'dist/style.css');
writeFileSync('dist/build.json', `${JSON.stringify(info, null, 2)}\n`);
console.log(`built ${commit.slice(0, 12)} on ${info.platform} with node ${info.node}`);
