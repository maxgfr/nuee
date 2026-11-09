#!/usr/bin/env bash
# The GitHub Action (action.yml): fetches a checked Nuée binary, then submits this commit to the
# group. The work itself runs on the group's machines; this runner only waits for the result.
set -euo pipefail

if [ "${GITHUB_EVENT_NAME:-}" = pull_request_target ]; then
  echo "::error::Nuée refuses pull_request_target: it would give the group's key to a fork's code" >&2
  exit 1
fi

case "${RUNNER_OS:-}-${RUNNER_ARCH:-}" in
  Linux-X64) target=x86_64-unknown-linux-gnu ;;
  Linux-ARM64) target=aarch64-unknown-linux-gnu ;;
  macOS-ARM64) target=aarch64-apple-darwin ;;
  macOS-X64) target=x86_64-apple-darwin ;;
  *) echo "::error::no Nuée binary for ${RUNNER_OS:-?}-${RUNNER_ARCH:-?}" >&2; exit 1 ;;
esac

state=$(mktemp -d "${RUNNER_TEMP:-/tmp}/nuee.XXXXXX")
export NUEE_CONFIG_DIR="$state/config" NUEE_DATA_DIR="$state/data"

nuee=${NUEE_BIN:-}
if [ -z "$nuee" ]; then
  case "${NUEE_VERSION:-}" in
    v[0-9]*) base="https://github.com/maxgfr/nuee/releases/download/$NUEE_VERSION" ;;
    *) base="https://github.com/maxgfr/nuee/releases/latest/download" ;;
  esac
  nuee="$state/nuee-$target"
  curl -fsSL --retry 3 -o "$nuee" "$base/nuee-$target"
  curl -fsSL --retry 3 -o "$state/SHA256SUMS" "$base/SHA256SUMS"
  (cd "$state" && grep " \*\?nuee-$target\$" SHA256SUMS | shasum -a 256 -c -)
  chmod +x "$nuee"
fi

repo="${GITHUB_SERVER_URL:?}/${GITHUB_REPOSITORY:?}.git"
args=(--repo "$repo" --rev "${GITHUB_SHA:?}" --timeout "${NUEE_TIMEOUT:-3600}" --on -)
# A re-run is asked for because the last result is not trusted: do not get it back.
if [ "${GITHUB_RUN_ATTEMPT:-1}" != 1 ]; then
  args+=(--fresh)
fi
out=${NUEE_ACTION_OUT:-nuee-out}
mkdir -p "$out"
if [ -n "${GITHUB_OUTPUT:-}" ]; then
  echo "out=$out" >>"$GITHUB_OUTPUT"
fi

status=0
if [ -n "${NUEE_SHARDS:-}" ]; then
  printf '%s' "$NUEE_GROUP" | "$nuee" shard "$NUEE_SHARDS" "${args[@]}" --out "$out" -- "$NUEE_COMMAND" || status=$?
  logs=$(find "$out" -name task-0.bin)
else
  while IFS= read -r path; do
    if [ -n "$path" ]; then args+=(--keep "$path"); fi
  done <<<"${NUEE_KEEP:-}"
  printf '%s' "$NUEE_GROUP" | "$nuee" build "${args[@]}" --out "$out" -- "$NUEE_COMMAND" || status=$?
  logs="$out/nuee-build.log"
fi

# The logs of what failed, where the person reading the run will look.
if [ "$status" != 0 ]; then
  for log in $logs; do
    if [ -f "$log" ] && ! tail -n 1 "$log" | grep -qx 'nuee-exit-code 0'; then
      echo "::group::$log"
      tail -n 200 "$log"
      echo "::endgroup::"
    fi
  done
fi
exit "$status"
