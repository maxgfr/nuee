#!/usr/bin/env bash
# The repository gate: formatting, lints, tests, supply chain, builds and end-to-end tests.
# Stops at the first failing stage and names it. Without an argument it runs everything, in
# order. CI runs the groups side by side; together they are every stage.
# Usage: scripts/check.sh [static|test|e2e-1|e2e-2|e2e-3|fuzz]
# (NUEE_E2E_GPU=1 is passed through to the e2e stage)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

usage() {
  echo "usage: scripts/check.sh [static|test|e2e-1|e2e-2|e2e-3|fuzz]" >&2
  exit 2
}

if [ "$#" -gt 1 ]; then
  usage
fi

group="${1:-all}"
# The end-to-end suite is cut in three for three machines, each still running one test at a time:
# the cross-browser tests in two halves, and the accessibility checks, many but short, apart.
e2e_args=()
case "$group" in
  all | static | test | fuzz) ;;
  e2e-1 | e2e-2)
    e2e_args=(--project=cross "--shard=${group#e2e-}/2")
    group=e2e
    ;;
  e2e-3)
    e2e_args=(--project=a11y-chromium --project=a11y-firefox --project=a11y-webkit)
    group=e2e
    ;;
  *) usage ;;
esac

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

# run "GROUPS" NAME COMMAND...: a stage, when the selected group is one of GROUPS.
run() {
  local groups="$1"
  shift
  if [ "$group" = all ] || [[ " $groups " == *" $group "* ]]; then
    stage "$@"
  fi
}

run "static" fmt cargo fmt --all --check
run "static" clippy cargo clippy --workspace --all-targets --locked -- -D warnings
run "static" clippy-wasm32 clippy_wasm
run "static test" build-tasks scripts/build-tasks.sh
run "test" test cargo test --workspace --locked
run "test" python-client python3 clients/python/test_nuee.py
run "static" deny-audit supply_chain
run "static" tasks-clippy tasks_clippy
run "static" build-web scripts/build-web.sh
run "static" node-test node --test web/test/
run "e2e" e2e scripts/e2e.sh ${e2e_args[@]+"${e2e_args[@]}"}
run "fuzz" fuzz-short fuzz_short
run "static" actionlint lint_workflows
run "static" shellcheck lint_scripts

printf '\ncheck: all stages of %s passed\n' "$group"
printf '  %s\n' "${SUMMARY[@]}"
printf '  %-16s %4ds\n' total "$SECONDS"
