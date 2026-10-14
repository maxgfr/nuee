// Global setup: a local `nuee relay --dev` and the static web server, both on 127.0.0.1.
// Their URLs reach the tests through the environment; the returned function stops them.
import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = process.env.NUEE_ROOT ?? path.join(here, '..');
const bin = process.env.NUEE_BIN ?? path.join(repo, 'target', 'debug', 'nuee');

/**
 * Starts `cmd` and resolves with the first stdout line matching `re` (its first group).
 * @param {string} name
 * @param {string} cmd
 * @param {string[]} args
 * @param {NodeJS.ProcessEnv} env
 * @param {RegExp} re
 */
function startService(name, cmd, args, env, re) {
  const child = spawn(cmd, args, { env, stdio: ['ignore', 'pipe', 'inherit'] });
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      child.kill('SIGKILL');
      reject(new Error(`${name} did not start within 30 s`));
    }, 30_000);
    child.once('error', (e) => {
      clearTimeout(timer);
      reject(e);
    });
    child.once('exit', (code, signal) => {
      clearTimeout(timer);
      reject(new Error(`${name} exited early (${code ?? signal})`));
    });
    const lines = createInterface({ input: child.stdout });
    lines.on('line', (line) => {
      const m = re.exec(line);
      if (m) {
        clearTimeout(timer);
        child.removeAllListeners('exit');
        resolve({ child, url: m[1] });
      }
    });
  });
}

/** @param {import('node:child_process').ChildProcess} child */
function stop(child) {
  return new Promise((resolve) => {
    if (child.exitCode !== null || child.signalCode !== null) {
      resolve(undefined);
      return;
    }
    const timer = setTimeout(() => child.kill('SIGKILL'), 5_000);
    child.once('exit', () => {
      clearTimeout(timer);
      resolve(undefined);
    });
    child.kill('SIGTERM');
  });
}

export default async function setup() {
  const base = process.env.NUEE_E2E_TMP ?? tmpdir();
  const dir = mkdtempSync(path.join(base, 'services-'));
  const env = {
    ...process.env,
    HOME: dir,
    NUEE_CONFIG_DIR: path.join(dir, 'config'),
    NUEE_DATA_DIR: path.join(dir, 'data'),
    NO_COLOR: '1',
  };
  /** @type {import('node:child_process').ChildProcess[]} */
  const children = [];
  const killAll = () => {
    for (const c of children) if (c.exitCode === null && c.signalCode === null) c.kill('SIGKILL');
  };
  process.once('exit', killAll);
  try {
    const relay = await startService('relay', bin, ['relay', '--dev', '--bind', '127.0.0.1:0'], env, /^relay ready: (\S+)/);
    children.push(relay.child);
    const web = await startService('web server', process.execPath, [path.join(repo, 'scripts', 'serve-web.mjs')], env, /^serving (\S+)/);
    children.push(web.child);
    process.env.NUEE_E2E_RELAY = relay.url;
    process.env.NUEE_E2E_WEB = web.url;
    process.env.NUEE_BIN = bin;
    process.env.NUEE_ROOT = repo;
    process.env.NUEE_E2E_TMP = base;
  } catch (e) {
    killAll();
    throw e;
  }
  return async () => {
    await Promise.all(children.map(stop));
    process.removeListener('exit', killAll);
    rmSync(dir, { recursive: true, force: true });
  };
}
