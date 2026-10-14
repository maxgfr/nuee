#!/usr/bin/env bash
# The repository gate: formatting, lints, tests, supply chain, builds and end-to-end tests.
# Stops at the first failing stage and names it. CI runs this script as is.
# Usage: scripts/check.sh   (NUEE_E2E_GPU=1 is passed through to the e2e stage)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [ "$#" -ne 0 ]; then
  echo "usage: scripts/check.sh" >&2
  exit 2
fi

# Crates that also build for the browser (wasm32-unknown-unknown).
WASM_CRATES=(nuee-proto nuee-core nuee-net nuee-store nuee-train nuee-web)

SUMMARY=()

stage() {
  local name="$1"
  shift
  printf '\n==> %s\n' "$name"
  local start=$SECONDS
  if ! "$@"; then
    printf '\ncheck: FAILED at stage "%s"\n' "$name" >&2
    exit 1
  fi
  SUMMARY+=("$(printf '%-16s %4ds' "$name" $((SECONDS - start)))")
}

clippy_wasm() {
  local args=()
  for c in "${WASM_CRATES[@]}"; do
    args+=(-p "$c")
  done
  cargo clippy "${args[@]}" --target wasm32-unknown-unknown --locked -- -D warnings
}

supply_chain() {
  cargo deny check || return 1
  # Fails on vulnerabilities; unmaintained or yanked crates are printed as warnings.
  cargo audit
}

tasks_clippy() {
  (cd tasks && cargo clippy --locked -- -D warnings)
}

FUZZ_TARGETS=(decode_msg parse_link validate_module parse_job_toml decode_frames delta_codec
  erasure_header http_frame)

fuzz_short() {
  if ! cargo +nightly fuzz --version >/dev/null 2>&1; then
    echo "check: \`cargo +nightly fuzz\` is unavailable; install it with" >&2
    echo "  rustup toolchain install nightly && cargo install cargo-fuzz" >&2
    return 1
  fi
  # cargo-fuzz defaults to the triple it was built for, which is musl for prebuilt Linux
  # binaries (no ASan there): build for this machine's host instead.
  local t host
  host="$(rustc +nightly -vV | sed -n 's/^host: //p')"
  for t in "${FUZZ_TARGETS[@]}"; do
    printf -- '--- fuzz %s\n' "$t"
    (cd fuzz && cargo +nightly fuzz run --target "$host" "$t" -- -runs=20000 -max_total_time=60) ||
      return 1
  done
}

need_tool() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "check: $1 is not installed; $2" >&2
    return 1
  fi
}

lint_workflows() {
  need_tool actionlint "install it with \`brew install actionlint\` or \`go install github.com/rhysd/actionlint/cmd/actionlint@latest\`" || return 1
  actionlint
}

lint_scripts() {
  need_tool shellcheck "install it with \`brew install shellcheck\` or \`apt-get install shellcheck\`" || return 1
  shellcheck scripts/*.sh
}

stage fmt cargo fmt --all --check
stage clippy cargo clippy --workspace --all-targets --locked -- -D warnings
stage clippy-wasm32 clippy_wasm
stage build-tasks scripts/build-tasks.sh
stage test cargo test --workspace --locked
stage deny-audit supply_chain
stage tasks-clippy tasks_clippy
stage build-web scripts/build-web.sh
stage node-test node --test web/test/
stage e2e scripts/e2e.sh
stage fuzz-short fuzz_short
stage actionlint lint_workflows
stage shellcheck lint_scripts

printf '\ncheck: all stages passed\n'
printf '  %s\n' "${SUMMARY[@]}"
printf '  %-16s %4ds\n' total "$SECONDS"
