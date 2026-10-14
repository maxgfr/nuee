module.exports = {
  branches: ['main'],
  tagFormat: 'v${version}',
  plugins: [
    './scripts/release-plugin.cjs',
    ['@semantic-release/release-notes-generator', { preset: 'conventionalcommits' }],
    ['@semantic-release/git', {
      assets: ['Cargo.toml', 'Cargo.lock', 'README.md'],
      message: 'chore(release): ${nextRelease.version} [skip ci]'
    }],
    ['@semantic-release/github', {
      assets: ['dist/nuee-*', 'dist/SHA256SUMS'],
      successComment: false,
      failComment: false,
      failTitle: false,
      releasedLabels: false
    }]
  ]
};
