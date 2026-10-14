const fs = require('node:fs');
const { execFileSync } = require('node:child_process');

const HEADER = /^([a-z][a-z0-9-]*)(?:\([^()\r\n]+\))?(!)?: \S.*$/;
const BREAKING = /^BREAKING[ -]CHANGE: /m;
const RANK = { patch: 1, minor: 2, major: 3 };

// Classify one commit message; anything that is not a Conventional Commit is ignored.
function bumpFor(message) {
  if (typeof message !== 'string') return null;
  const header = message.split(/\r?\n/, 1)[0];
  const match = HEADER.exec(header);
  if (!match) return null;
  if (match[2] === '!' || BREAKING.test(message)) return 'major';
  if (match[1] === 'feat') return 'minor';
  if (match[1] === 'fix' || match[1] === 'perf') return 'patch';
  return null;
}

exports.bumpFor = bumpFor;

exports.analyzeCommits = async (config, context) => {
  let release = null;
  for (const commit of context.commits || []) {
    const bump = bumpFor(commit && commit.message);
    if (bump && (!release || RANK[bump] > RANK[release])) release = bump;
  }
  // Before 1.0.0 a breaking change bumps the minor version.
  const last = context.lastRelease && context.lastRelease.version;
  if (release === 'major' && (!last || /^0\./.test(last))) release = 'minor';
  return release;
};

exports.verifyRelease = async (config, { nextRelease }) => {
  const version = nextRelease.version;
  if (!/^\d+\.\d+\.\d+$/.test(version)) throw new Error('Expected a stable semantic version');
  const pinned = process.env.NUEE_RELEASE_VERSION;
  if (pinned && pinned !== version) throw new Error('Release version changed after the matrix build');
  if (process.env.GITHUB_OUTPUT) fs.appendFileSync(process.env.GITHUB_OUTPUT, `version=${version}\n`);
};

exports.prepare = async (config, { nextRelease }) => {
  const env = { ...process.env, NUEE_RELEASE_VERSION: nextRelease.version };
  execFileSync('python3', ['scripts/prepare_release.py', nextRelease.version], { stdio: 'inherit', env });
};
