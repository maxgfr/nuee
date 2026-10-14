#!/usr/bin/env bash
# End-to-end tests: builds the tasks, the web package and the CLI, then runs Playwright on
# Chromium, Firefox and WebKit against a local relay and a local static server (no Internet).
# Extra arguments go to `playwright test` (e.g. `scripts/e2e.sh tests/files.spec.mjs`).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

scripts/build-tasks.sh
scripts/build-web.sh
cargo build -p nuee

npm ci --prefix e2e --ignore-scripts --no-audit --no-fund

export NUEE_BIN="$root/target/debug/nuee"
export NUEE_ROOT="$root"
NUEE_E2E_TMP="$(mktemp -d "${TMPDIR:-/tmp}/nuee-e2e.XXXXXX")"
export NUEE_E2E_TMP
# Never reach the Internet: no n0 relay, no telemetry, browsers from the local cache only.
export PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1

cleanup() {
  local status=$?
  # Playwright stops what it started; this catches anything left after a crash.
  if pgrep -P $$ >/dev/null; then
    pkill -TERM -P $$
  fi
  rm -rf "$NUEE_E2E_TMP"
  exit "$status"
}
trap cleanup EXIT INT TERM

cd e2e
npx --no-install playwright test "$@"
